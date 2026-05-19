//! Self-register binkflix with bastion on boot.
//!
//! On first boot we generate an RSA keypair, persist it in a singleton row,
//! and POST `{ slug, name, public_jwk, permissions }` to
//! `<bastion>/api/services/register`. Bastion files this as a pending
//! registration; an admin approves it from bastion's admin UI (including
//! configuring the return URL on bastion's side). Once approved, every boot
//! re-asserts the perm catalog (idempotent: bastion soft-deletes keys we
//! drop, restores keys we re-add).
//!
//! If bastion is unreachable or returns an unexpected status, we log and
//! continue starting up — binkflix shouldn't fail to boot just because
//! bastion is down. The auth middleware will redirect users to bastion
//! whenever it's back.

use anyhow::{anyhow, Context, Result};
use data_encoding::BASE32_NOPAD;
use josekit::jws::RS256;
use rand::RngCore;
use serde::Serialize;
use serde_json::Value;
use sqlx::SqlitePool;
use std::time::Duration;
use tracing::{info, warn};

const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Permission catalog binkflix declares to bastion. Each tuple is
/// `(key, description, default_allow)`. Permissions with `default_allow=true`
/// are auto-granted to every user with a grant on this service — admins can
/// revoke them per-user afterwards.
///
/// Bastion stores this as the canonical list of permissions admins can grant
/// per-user; anything removed gets soft-deleted on the next sync (existing
/// `user_perms` rows survive, just stop appearing in fresh tokens).
pub const BINKFLIX_PERMS: &[(&str, &str, bool)] = &[
    ("library:write", "Trigger scans, edit library metadata", false),
    ("playback:write", "Submit playback progress and history", true),
];

#[derive(Clone)]
pub struct Keypair {
    pub public_jwk_json: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationStatus {
    Pending,
    Approved,
    Denied,
}

#[derive(Serialize)]
struct PermEntryOut<'a> {
    key: &'a str,
    description: &'a str,
    default_allow: bool,
}

#[derive(Serialize)]
struct RegisterBody<'a> {
    slug: &'a str,
    name: &'a str,
    /// Best guess at our public callback URL. Bastion uses this only as the
    /// initial prefill for a new service's pending registration — admins set
    /// the authoritative value in the bastion GUI before approving.
    #[serde(skip_serializing_if = "Option::is_none")]
    return_url: Option<&'a str>,
    public_jwk: Value,
    permissions: Vec<PermEntryOut<'a>>,
}

/// Load the singleton keypair from `service_keypair`, or generate one and
/// persist it. KID is a random 12-byte base32 string, matching bastion's
/// signing-key convention.
///
/// The private key is persisted but not loaded into memory by this caller —
/// future signed-call paths (`PUT /api/services/<slug>/permissions`) can
/// read it from `service_keypair.private_jwk` when needed.
pub async fn ensure_keypair(pool: &SqlitePool) -> Result<Keypair> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT public_jwk FROM service_keypair WHERE id = 1")
            .fetch_optional(pool)
            .await?;
    if let Some((pub_str,)) = row {
        return Ok(Keypair {
            public_jwk_json: pub_str,
        });
    }

    // RSA-2048 keygen is CPU-bound and takes 100–500ms; keep it off the
    // tokio worker thread so we don't stall other connections during the
    // (one-time) first boot.
    let pair = tokio::task::spawn_blocking(|| RS256.generate_key_pair(2048))
        .await
        .map_err(|e| anyhow!("rsa keygen join: {}", e))?
        .map_err(|e| anyhow!("rsa keygen: {}", e))?;
    let mut private_jwk = pair.to_jwk_private_key();
    let mut public_jwk = pair.to_jwk_public_key();

    let kid = new_kid();
    private_jwk.set_key_id(&kid);
    private_jwk.set_algorithm("RS256");
    public_jwk.set_key_id(&kid);
    public_jwk.set_algorithm("RS256");
    public_jwk.set_key_use("sig");

    let priv_str = serde_json::to_string(private_jwk.as_ref())?;
    let pub_str = serde_json::to_string(public_jwk.as_ref())?;

    sqlx::query(
        "INSERT INTO service_keypair (id, kid, private_jwk, public_jwk) VALUES (1, ?, ?, ?)",
    )
    .bind(&kid)
    .bind(&priv_str)
    .bind(&pub_str)
    .execute(pool)
    .await?;

    info!(kid = %kid, "generated fresh bastion service keypair");
    Ok(Keypair {
        public_jwk_json: pub_str,
    })
}

/// POST to `<bastion>/api/services/register`. Idempotent — safe to call on
/// every boot. Bastion responds 202 if pending review, 200 if already
/// approved (catalog re-synced), 409 if a different key claims this slug.
pub async fn register(
    pool: &SqlitePool,
    bastion_origin: &str,
    slug: &str,
    name: &str,
    return_url_suggestion: Option<&str>,
    perms: &[(&str, &str, bool)],
) -> Result<RegistrationStatus> {
    let keypair = ensure_keypair(pool).await?;
    let public_jwk: Value = serde_json::from_str(&keypair.public_jwk_json)
        .context("re-parse stored public_jwk")?;

    let body = RegisterBody {
        slug,
        name,
        return_url: return_url_suggestion,
        public_jwk,
        permissions: perms
            .iter()
            .map(|(k, d, da)| PermEntryOut {
                key: k,
                description: d,
                default_allow: *da,
            })
            .collect(),
    };

    let url = format!("{}/api/services/register", bastion_origin.trim_end_matches('/'));
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()?;
    let resp = http.post(&url).json(&body).send().await?;
    let status_code = resp.status();
    let body_text = resp.text().await.unwrap_or_default();

    if !status_code.is_success() {
        warn!(
            status = %status_code,
            body = %body_text,
            "bastion register call rejected — see admin UI"
        );
        return Err(anyhow!(
            "bastion /api/services/register returned {}: {}",
            status_code,
            body_text
        ));
    }

    let parsed: Value = serde_json::from_str(&body_text).unwrap_or(Value::Null);
    let status_str = parsed
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let status = match status_str {
        "approved" => RegistrationStatus::Approved,
        "denied" => RegistrationStatus::Denied,
        _ => RegistrationStatus::Pending,
    };

    if status == RegistrationStatus::Approved {
        sqlx::query("UPDATE service_keypair SET approved_at = unixepoch() WHERE id = 1")
            .execute(pool)
            .await?;
        info!(slug = %slug, "bastion: service approved");
    } else {
        info!(
            slug = %slug,
            status = %status_str,
            "bastion: registration submitted, waiting for admin approval"
        );
    }
    Ok(status)
}

/// Background task: poll bastion until our slug flips to approved or denied.
/// Exits silently on either terminal state; keeps trying through transient
/// errors so the operator can approve at their leisure without restarting
/// binkflix.
pub async fn poll_until_approved(pool: SqlitePool, bastion_origin: String, slug: String) {
    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "bastion poller: failed to build http client");
            return;
        }
    };
    let url = format!(
        "{}/api/services/{}/status",
        bastion_origin.trim_end_matches('/'),
        slug
    );
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        match http.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body: Value = resp.json().await.unwrap_or(Value::Null);
                match body.get("status").and_then(|v| v.as_str()) {
                    Some("approved") => {
                        let _ = sqlx::query(
                            "UPDATE service_keypair SET approved_at = unixepoch() WHERE id = 1",
                        )
                        .execute(&pool)
                        .await;
                        info!(slug = %slug, "bastion: service approved");
                        return;
                    }
                    Some("denied") => {
                        warn!(slug = %slug, "bastion: service denied — admin must intervene");
                        return;
                    }
                    _ => {}
                }
            }
            Ok(resp) => {
                warn!(status = %resp.status(), "bastion poll: non-success response");
            }
            Err(e) => {
                warn!(error = %e, "bastion poll: request failed");
            }
        }
    }
}

fn new_kid() -> String {
    let mut bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut bytes);
    BASE32_NOPAD.encode(&bytes).to_lowercase()
}
