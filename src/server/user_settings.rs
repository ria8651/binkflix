//! Per-user app settings (e.g. selected theme). Keyed by `user_sub` so a
//! user's picks follow them across devices. See `media_preferences` for the
//! per-media equivalent.

use super::auth::Session;
use super::error::Result;
use super::AppState;
use axum::{
    extract::{Extension, State},
    http::StatusCode,
    Json,
};
use sqlx::FromRow;

use crate::types::UserSettings;

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(FromRow)]
struct SettingsRow {
    theme: Option<String>,
}

pub async fn get_user_settings(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
) -> Result<Json<UserSettings>> {
    let row: Option<SettingsRow> = sqlx::query_as(
        "SELECT theme FROM user_settings WHERE user_sub = ?",
    )
    .bind(&session.user_sub)
    .fetch_optional(&state.pool)
    .await?;
    Ok(Json(match row {
        Some(r) => UserSettings { theme: r.theme },
        None => UserSettings::default(),
    }))
}

pub async fn set_user_settings(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Json(body): Json<UserSettings>,
) -> Result<StatusCode> {
    // Normalize: empty/whitespace means "no preference"; store NULL so a
    // single `is_none()` check on read covers both shapes.
    let theme = body
        .theme
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    sqlx::query(
        "INSERT INTO user_settings (user_sub, theme, updated_at)
         VALUES (?, ?, ?)
         ON CONFLICT(user_sub) DO UPDATE SET
             theme      = excluded.theme,
             updated_at = excluded.updated_at",
    )
    .bind(&session.user_sub)
    .bind(&theme)
    .bind(now_secs())
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
