//! Device pairing for clients that can't do bastion's browser redirect (the
//! Apple TV app). Shaped like the OAuth device flow (RFC 8628):
//!
//!   device  → POST /auth/device/code  → `{ device_code, user_code, … }`
//!   device shows `user_code` and the `/link?code=…` URL (or a QR of it)
//!   browser → POST /api/devices/link `{ user_code }` on its cookie session
//!   device  → POST /auth/device/token `{ device_code }`, polling, until it
//!             gets `{ token }`
//!
//! The token is a long-lived bearer credential (`Authorization: Bearer …`)
//! carrying the approving user's identity and perms. Only its hash is
//! persisted (migration 0029), so tokens survive restarts — unlike cookie
//! sessions — and a leaked DB doesn't leak them. Pending pairings are
//! in-memory: a restart mid-pairing just means asking for a new code.
//!
//! Only mounted when bastion auth is on; with auth off every request is
//! already served as the dev identity and there is nothing to pair.

use super::auth::Session;
use super::error::{Error, Result};
use super::AppState;
use crate::types::{DeviceInfo, LinkedDevice};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use rand::{Rng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// How long a device has to get its code approved.
const PAIRING_TTL: Duration = Duration::from_secs(10 * 60);
/// Suggested poll interval for `/auth/device/token`, in seconds.
const POLL_INTERVAL_SECS: u64 = 2;
/// `/auth/device/code` is unauthenticated; cap what it can make us hold.
/// Entries are tiny, so the cap is high enough that filling it to block
/// pairing takes real effort.
const MAX_PENDING: usize = 1024;
/// How long a validated token is trusted before it's re-read from the DB.
/// Bounds how often `last_used_at` is written; revocation through
/// `DELETE /api/devices/{id}` evicts immediately regardless.
const TOKEN_CACHE_TTL: Duration = Duration::from_secs(60);
/// No ambiguous letters or vowels (no accidental words), as RFC 8628 §6.1
/// suggests. 20^8 ≈ 2.6e10 codes, and guessing one needs a signed-in session.
const USER_CODE_ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";

struct Pending {
    user_code: String,
    name: String,
    expires: Instant,
    /// Set when a browser starts approving, so a second approval of the
    /// same code (double submit, two people) can't mint a second token.
    claimed: bool,
    /// Set once a browser approves; handed over on the next poll.
    token: Option<(String, String)>,
}

#[derive(Clone, Default)]
pub struct Devices {
    /// Keyed by device code.
    pending: Arc<Mutex<HashMap<String, Pending>>>,
    /// Token hash → session it resolves to, and when that was read.
    cache: Arc<Mutex<HashMap<String, (Session, Instant)>>>,
    /// Bumped on every revoke. A lookup that raced a revoke doesn't cache
    /// what it read, or the revoked token would work until the cache TTL.
    revocations: Arc<std::sync::atomic::AtomicU64>,
}

impl Devices {
    /// Resolve a bearer token to the session it was issued for.
    pub async fn session_for(&self, pool: &SqlitePool, token: &str) -> Option<Session> {
        let hash = hash_token(token);
        if let Some((session, at)) = self.cache.lock().expect("device cache poisoned").get(&hash) {
            if at.elapsed() < TOKEN_CACHE_TTL {
                return Some(session.clone());
            }
        }

        let revocations = self.revocations.load(std::sync::atomic::Ordering::SeqCst);
        let row: Option<(String, String, String)> = sqlx::query_as(
            "UPDATE device_tokens SET last_used_at = ? WHERE token_hash = ?
             RETURNING user_sub, login, perms_json",
        )
        .bind(now_secs())
        .bind(&hash)
        .fetch_optional(pool)
        .await
        .map_err(|e| warn!(%e, "device token lookup failed"))
        .ok()
        .flatten();

        let mut cache = self.cache.lock().expect("device cache poisoned");
        let Some((user_sub, login, perms_json)) = row else {
            cache.remove(&hash);
            return None;
        };
        let session = Session {
            user_sub,
            login,
            created_at: Instant::now(),
            perms: serde_json::from_str(&perms_json).unwrap_or_default(),
        };
        if self.revocations.load(std::sync::atomic::Ordering::SeqCst) == revocations {
            cache.insert(hash, (session.clone(), Instant::now()));
        }
        Some(session)
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/device/code", post(start))
        .route("/auth/device/token", post(poll))
        .route("/api/devices", get(list))
        .route("/api/devices/link", post(link))
        .route("/api/devices/{id}", delete(revoke))
}

fn devices(state: &AppState) -> &Devices {
    // `router()` is only mounted when auth is on.
    &state.auth.as_ref().expect("device routes mounted without auth").devices
}

#[derive(Deserialize, Default)]
struct StartBody {
    /// Shown in the device list, e.g. "Living Room Apple TV".
    name: Option<String>,
}

#[derive(Serialize)]
struct StartResp {
    device_code: String,
    user_code: String,
    /// Relative to the server origin the device is already talking to.
    verification_uri: &'static str,
    verification_uri_complete: String,
    expires_in: u64,
    interval: u64,
}

async fn start(
    State(state): State<AppState>,
    body: Option<Json<StartBody>>,
) -> Result<Json<StartResp>> {
    let name = body
        .and_then(|Json(b)| b.name)
        .map(|n| n.trim().chars().take(64).collect::<String>())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Unnamed device".into());

    let mut pending = devices(&state).pending.lock().expect("pairings poisoned");
    pending.retain(|_, p| p.expires > Instant::now());
    if pending.len() >= MAX_PENDING {
        return Err(Error::BadRequest("too many pending pairings, try again later".into()));
    }

    let device_code = random_hex(32);
    let user_code = new_user_code();
    pending.insert(
        device_code.clone(),
        Pending {
            user_code: user_code.clone(),
            name,
            expires: Instant::now() + PAIRING_TTL,
            claimed: false,
            token: None,
        },
    );
    Ok(Json(StartResp {
        device_code,
        verification_uri_complete: format!("/link?code={user_code}"),
        user_code,
        verification_uri: "/link",
        expires_in: PAIRING_TTL.as_secs(),
        interval: POLL_INTERVAL_SECS,
    }))
}

#[derive(Deserialize)]
struct PollBody {
    device_code: String,
}

#[derive(Serialize)]
struct PollResp {
    token: String,
    login: String,
}

/// RFC 8628 §3.5 error shape: 400 with `{"error": "authorization_pending"}`
/// while waiting, `expired_token` once the code is gone.
async fn poll(State(state): State<AppState>, Json(body): Json<PollBody>) -> Response {
    let mut pending = devices(&state).pending.lock().expect("pairings poisoned");
    let error = |code: &str| {
        (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": code }))).into_response()
    };
    let status = pending
        .get(&body.device_code)
        .map(|p| (p.expires <= Instant::now(), p.token.is_some()));
    match status {
        None => error("expired_token"),
        Some((true, _)) => {
            pending.remove(&body.device_code);
            error("expired_token")
        }
        Some((false, false)) => error("authorization_pending"),
        Some((false, true)) => {
            let (token, login) = pending
                .remove(&body.device_code)
                .and_then(|p| p.token)
                .expect("approved pairing holds its token");
            Json(PollResp { token, login }).into_response()
        }
    }
}

#[derive(Deserialize)]
struct LinkBody {
    user_code: String,
}

async fn link(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    headers: HeaderMap,
    Json(body): Json<LinkBody>,
) -> Result<Json<LinkedDevice>> {
    browser_only(&headers)?;
    let wanted = normalize_user_code(&body.user_code);
    let devices = devices(&state);

    // Claim the pairing under the lock, then do the DB write without it.
    let (device_code, name) = {
        let mut pending = devices.pending.lock().expect("pairings poisoned");
        let (dc, p) = pending
            .iter_mut()
            .find(|(_, p)| {
                !p.claimed && p.expires > Instant::now() && normalize_user_code(&p.user_code) == wanted
            })
            .ok_or(Error::NotFound)?;
        p.claimed = true;
        (dc.clone(), p.name.clone())
    };

    let token = random_hex(32);
    let now = now_secs();
    sqlx::query(
        "INSERT INTO device_tokens
             (id, token_hash, user_sub, login, perms_json, name, created_at, last_used_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(uuid::Uuid::new_v4().simple().to_string())
    .bind(hash_token(&token))
    .bind(&session.user_sub)
    .bind(&session.login)
    .bind(serde_json::to_string(&session.perms).unwrap_or_else(|_| "[]".into()))
    .bind(&name)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .inspect_err(|_| {
        if let Some(p) = devices.pending.lock().expect("pairings poisoned").get_mut(&device_code) {
            p.claimed = false;
        }
    })?;

    if let Some(p) = devices.pending.lock().expect("pairings poisoned").get_mut(&device_code) {
        p.token = Some((token, session.login.clone()));
    }
    info!(login = %session.login, device = %name, "device linked");
    Ok(Json(LinkedDevice { name }))
}

async fn list(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    headers: HeaderMap,
) -> Result<Json<Vec<DeviceInfo>>> {
    browser_only(&headers)?;
    let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(
        "SELECT id, name, created_at, last_used_at FROM device_tokens
         WHERE user_sub = ? ORDER BY last_used_at DESC",
    )
    .bind(&session.user_sub)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, name, created_at, last_used_at)| DeviceInfo { id, name, created_at, last_used_at })
            .collect(),
    ))
}

async fn revoke(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    browser_only(&headers)?;
    let hash: Option<String> = sqlx::query_scalar(
        "DELETE FROM device_tokens WHERE id = ? AND user_sub = ? RETURNING token_hash",
    )
    .bind(&id)
    .bind(&session.user_sub)
    .fetch_optional(&state.pool)
    .await?;
    let hash = hash.ok_or(Error::NotFound)?;
    let devices = devices(&state);
    devices.revocations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    devices.cache.lock().expect("device cache poisoned").remove(&hash);
    Ok(StatusCode::NO_CONTENT)
}

/// Device management is for signed-in browsers. A paired device's token
/// must not mint more tokens (which would outlive revoking it) or manage
/// the user's other devices.
fn browser_only(headers: &HeaderMap) -> Result<()> {
    if super::auth::bearer_token(headers).is_some() {
        return Err(Error::Forbidden);
    }
    Ok(())
}

fn hash_token(token: &str) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(token.as_bytes()))
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    data_encoding::HEXLOWER.encode(&buf)
}

/// `XXXX-XXXX` from [`USER_CODE_ALPHABET`].
fn new_user_code() -> String {
    let mut rng = rand::thread_rng();
    let mut code: String = (0..8)
        .map(|_| USER_CODE_ALPHABET[rng.gen_range(0..USER_CODE_ALPHABET.len())] as char)
        .collect();
    code.insert(4, '-');
    code
}

/// Users type codes however they like: case and separators don't matter.
fn normalize_user_code(code: &str) -> String {
    code.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_codes_are_formatted_and_normalize_back() {
        let code = new_user_code();
        assert_eq!(code.len(), 9);
        assert_eq!(code.as_bytes()[4], b'-');
        assert!(code.bytes().filter(|b| *b != b'-').all(|b| USER_CODE_ALPHABET.contains(&b)));
        assert_eq!(normalize_user_code(&code.to_lowercase().replace('-', " ")), code.replace('-', ""));
    }
}
