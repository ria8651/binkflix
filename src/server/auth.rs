//! Bastion SSO integration. In-memory opaque session cookies; JWTs from
//! bastion are verified once at `/auth/bastion` and discarded. Session dies on
//! restart — fine for a single-user self-hosted setup.
//!
//! Flow:
//!   unauthed → middleware redirects to `<bastion>/auth/login?service=<slug>&return=<self>/auth/bastion`
//!   bastion  → redirects to `<self>/auth/bastion?bastion_token=<jwt>`
//!   we verify, mint opaque session id, set cookie, redirect `/`

use axum::{
    extract::{Query, Request, State},
    http::{header, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{info, warn};

const COOKIE_NAME: &str = "binkflix_session";
const JWKS_TTL: Duration = Duration::from_secs(5 * 60);
const SESSION_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 30);

#[derive(Clone, Debug)]
pub struct AuthConfig {
    pub bastion_origin: String,
    pub service_slug: String,
}

impl AuthConfig {
    /// Auth is opt-in: returns `None` unless `BASTION_ORIGIN` is set. When off,
    /// binkflix skips the middleware entirely and runs fully open (fine for
    /// local dev when you just want to hack on features).
    pub fn from_env() -> Option<Self> {
        let bastion_origin = std::env::var("BASTION_ORIGIN").ok()?;
        Some(Self {
            bastion_origin,
            service_slug: std::env::var("BASTION_SERVICE_SLUG")
                .unwrap_or_else(|_| "binkflix".into()),
        })
    }
}

#[derive(Clone)]
#[allow(dead_code)] // fields are read by downstream handlers via request extensions
pub struct Session {
    /// Stable identity hash from bastion (JWT `sub`). Safe to persist — survives
    /// bastion DB wipes as long as the user signs back in with the same first
    /// auth provider.
    pub user_sub: String,
    pub login: String,
    pub created_at: Instant,
}

#[derive(Default)]
struct JwksCache {
    set: Option<JwkSet>,
    fetched_at: Option<Instant>,
}

#[derive(Clone)]
pub struct AuthState {
    pub cfg: AuthConfig,
    sessions: Arc<RwLock<HashMap<String, Session>>>,
    jwks: Arc<RwLock<JwksCache>>,
    http: reqwest::Client,
}

impl AuthState {
    pub fn new(cfg: AuthConfig) -> Self {
        Self {
            cfg,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            jwks: Arc::new(RwLock::new(JwksCache::default())),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("reqwest client"),
        }
    }

    async fn jwks(&self) -> anyhow::Result<JwkSet> {
        {
            let g = self.jwks.read().await;
            if let (Some(set), Some(at)) = (&g.set, g.fetched_at) {
                if at.elapsed() < JWKS_TTL {
                    return Ok(set.clone());
                }
            }
        }
        let url = format!("{}/.well-known/jwks.json", self.cfg.bastion_origin);
        let set: JwkSet = self.http.get(&url).send().await?.error_for_status()?.json().await?;
        let mut g = self.jwks.write().await;
        g.set = Some(set.clone());
        g.fetched_at = Some(Instant::now());
        Ok(set)
    }

    async fn session_of(&self, jar: &CookieJar) -> Option<Session> {
        let sid = jar.get(COOKIE_NAME)?.value().to_string();
        let sessions = self.sessions.read().await;
        let s = sessions.get(&sid)?;
        if s.created_at.elapsed() > SESSION_TTL {
            return None;
        }
        Some(s.clone())
    }
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct Claims {
    /// Stable identity hash. Bastion-owned, portable across DB wipes.
    sub: String,
    iss: String,
    aud: String,
    exp: i64,
    /// Bastion-owned display name. Seeded from the auth provider's login at
    /// signup but may diverge later (user can rename themselves on bastion).
    username: Option<String>,
    svc: String,
}

pub fn router() -> Router<crate::server::AppState> {
    Router::new()
        .route("/auth/login", get(login))
        .route("/auth/bastion", get(bastion_callback))
        .route("/auth/logout", get(logout))
}

/// Kicks the user over to bastion to authenticate. No return URL is sent —
/// bastion already knows where to redirect (the service's registered URL).
async fn login(State(state): State<crate::server::AppState>) -> Response {
    let Some(auth) = state.auth.as_ref() else {
        return (StatusCode::NOT_FOUND, "auth disabled").into_response();
    };
    let url = format!(
        "{}/auth/login?service={}",
        auth.cfg.bastion_origin,
        urlencode(&auth.cfg.service_slug),
    );
    Redirect::to(&url).into_response()
}

#[derive(Deserialize)]
struct BastionCallbackParams {
    bastion_token: Option<String>,
}

async fn bastion_callback(
    State(state): State<crate::server::AppState>,
    jar: CookieJar,
    Query(params): Query<BastionCallbackParams>,
) -> Response {
    let Some(auth) = state.auth.as_ref() else {
        return (StatusCode::NOT_FOUND, "auth disabled").into_response();
    };
    let token = match params.bastion_token {
        Some(t) => t,
        None => return (StatusCode::BAD_REQUEST, "missing bastion_token").into_response(),
    };

    let claims = match verify_token(auth, &token).await {
        Ok(c) => c,
        Err(e) => {
            warn!(%e, "bastion token verification failed");
            return (StatusCode::UNAUTHORIZED, format!("invalid token: {e}")).into_response();
        }
    };

    let user_sub = claims.sub.clone();
    let login_name = claims
        .username
        .unwrap_or_else(|| format!("user:{}", &user_sub[..8.min(user_sub.len())]));
    let sid = new_sid();

    {
        let mut sessions = auth.sessions.write().await;
        sessions.insert(
            sid.clone(),
            Session {
                user_sub: user_sub.clone(),
                login: login_name.clone(),
                created_at: Instant::now(),
            },
        );
    }

    super::users::touch(&state.pool, &user_sub, &login_name).await;

    info!(%login_name, %user_sub, "bastion auth success");

    // Secure cookies require HTTPS. In release builds (Docker / production)
    // we assume a TLS-terminating reverse proxy in front of us; in debug
    // builds developers commonly run plain `dx serve` on http://localhost
    // and a Secure cookie would silently disappear.
    let cookie = Cookie::build((COOKIE_NAME, sid))
        .path("/")
        .http_only(true)
        .secure(!cfg!(debug_assertions))
        .same_site(SameSite::Lax)
        .build();
    let jar = jar.add(cookie);

    (jar, Redirect::to("/")).into_response()
}

async fn logout(
    State(state): State<crate::server::AppState>,
    jar: CookieJar,
) -> impl IntoResponse {
    if let (Some(c), Some(auth)) = (jar.get(COOKIE_NAME), state.auth.as_ref()) {
        let sid = c.value().to_string();
        auth.sessions.write().await.remove(&sid);
    }
    let jar = jar.remove(Cookie::build(COOKIE_NAME).path("/").build());
    (jar, Redirect::to("/"))
}

async fn verify_token(state: &AuthState, token: &str) -> anyhow::Result<Claims> {
    let header = decode_header(token)?;
    let kid = header.kid.ok_or_else(|| anyhow::anyhow!("token missing kid"))?;

    let jwks = state.jwks().await?;
    let jwk = jwks
        .find(&kid)
        .ok_or_else(|| anyhow::anyhow!("unknown kid {kid}"))?;
    let key = DecodingKey::from_jwk(jwk)?;

    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[state.cfg.bastion_origin.as_str()]);
    validation.set_audience(&[state.cfg.service_slug.as_str()]);

    let data = decode::<Claims>(token, &key, &validation)?;
    if data.claims.svc != state.cfg.service_slug {
        anyhow::bail!("token svc claim mismatch");
    }
    Ok(data.claims)
}

/// Hardcoded dev identity injected when bastion auth is disabled. Handlers
/// always see a `Session`, so they can read user_sub/login without branching.
const DEV_USER_SUB: &str = "dev";
const DEV_LOGIN: &str = "dev";

/// Middleware: when bastion is configured, require a session cookie and
/// redirect/401 on miss. When bastion is off, inject a default dev `Session`
/// so downstream handlers can always rely on having one in request extensions.
pub async fn require_session(
    State(state): State<crate::server::AppState>,
    jar: CookieJar,
    req: Request,
    next: Next,
) -> Response {
    if is_public_path(req.uri()) {
        return next.run(req).await;
    }

    let Some(auth) = state.auth.as_ref() else {
        super::users::touch(&state.pool, DEV_USER_SUB, DEV_LOGIN).await;
        let mut req = req;
        req.extensions_mut().insert(Session {
            user_sub: DEV_USER_SUB.into(),
            login: DEV_LOGIN.into(),
            created_at: Instant::now(),
        });
        return next.run(req).await;
    };

    if let Some(session) = auth.session_of(&jar).await {
        super::users::touch(&state.pool, &session.user_sub, &session.login).await;
        let mut req = req;
        req.extensions_mut().insert(session);
        return next.run(req).await;
    }

    // API calls get a 401; navigational requests get redirected.
    let accept = req
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if req.uri().path().starts_with("/api/") || !accept.contains("text/html") {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Redirect::to("/auth/login").into_response()
}

fn is_public_path(uri: &Uri) -> bool {
    let p = uri.path();
    p.starts_with("/auth/")
        || p.starts_with("/jassub/")
        || p.starts_with("/static/")
        || p == "/favicon.ico"
}

fn new_sid() -> String {
    // uuid v4 uses getrandom → cryptographic. Hex form, no hyphens.
    uuid::Uuid::new_v4().simple().to_string()
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}
