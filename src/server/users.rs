//! Persisted identity table — bastion JWT `sub` → human login.
//!
//! Auth upserts here on each authenticated request so we have a durable
//! `user_sub → login` mapping even though the rest of the schema only stores
//! the opaque sub. See migrations/0012_users.sql for the schema and the why.

use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use tracing::warn;

// Per-sub debounce so a logged-in user browsing rapidly doesn't generate one
// SQL write per request. last_seen accuracy of ~1h is plenty for the recovery
// use case ("seen this week" vs "haven't seen in a year").
const TOUCH_DEBOUNCE: Duration = Duration::from_secs(3600);

static LAST_TOUCH: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Record (or refresh) the login known for a given user_sub. Best-effort —
/// callers must not propagate errors, since this is a side-effect of auth,
/// not part of its critical path. Debounced in-process so request-rate
/// upserts don't pile up on the SQLite write lock.
pub async fn touch(pool: &SqlitePool, user_sub: &str, login: &str) {
    {
        let mut g = LAST_TOUCH.lock().expect("LAST_TOUCH poisoned");
        if let Some(prev) = g.get(user_sub) {
            if prev.elapsed() < TOUCH_DEBOUNCE {
                return;
            }
        }
        g.insert(user_sub.to_string(), Instant::now());
    }

    let now = now_secs();
    let res = sqlx::query(
        "INSERT INTO users (user_sub, login, first_seen, last_seen)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(user_sub) DO UPDATE SET
             login     = excluded.login,
             last_seen = excluded.last_seen",
    )
    .bind(user_sub)
    .bind(login)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await;
    if let Err(e) = res {
        warn!(%user_sub, %login, %e, "failed to upsert users row");
    }
}
