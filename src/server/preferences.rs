//! Per-user sticky playback preferences (audio/subtitle/quality picks).
//!
//! Stored in `media_preferences` keyed by `(user_sub, scope_key)`. The
//! scope is opaque to the server — clients build `show:<id>` for episodes
//! and `media:<id>` for movies, and the player loads/saves under that key
//! so a choice carries across episodes of one show without polluting a
//! different show's prefs.

use super::auth::Session;
use super::error::Result;
use super::AppState;
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use sqlx::FromRow;

use crate::types::MediaPreferences;

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(FromRow)]
struct PrefsRow {
    subtitle_id: Option<String>,
    subtitle_lang: Option<String>,
    audio_idx: Option<i64>,
    audio_lang: Option<String>,
    audio_codec: Option<String>,
    transcode_mode: Option<String>,
    bitrate_kbps: Option<i64>,
}

pub async fn get_preferences(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(scope): Path<String>,
) -> Result<Json<Option<MediaPreferences>>> {
    let row: Option<PrefsRow> = sqlx::query_as(
        "SELECT subtitle_id, subtitle_lang, audio_idx, audio_lang, audio_codec,
                transcode_mode, bitrate_kbps
         FROM media_preferences WHERE user_sub = ? AND scope_key = ?",
    )
    .bind(&session.user_sub)
    .bind(&scope)
    .fetch_optional(&state.pool)
    .await?;
    Ok(Json(row.map(|r| MediaPreferences {
        subtitle_id: r.subtitle_id,
        subtitle_lang: r.subtitle_lang,
        audio_idx: r.audio_idx.map(|n| n as u32),
        audio_lang: r.audio_lang,
        audio_codec: r.audio_codec,
        transcode_mode: r.transcode_mode,
        bitrate_kbps: r.bitrate_kbps.map(|n| n as u32),
    })))
}

pub async fn set_preferences(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(scope): Path<String>,
    Json(body): Json<MediaPreferences>,
) -> Result<StatusCode> {
    sqlx::query(
        "INSERT INTO media_preferences
            (user_sub, scope_key, subtitle_id, subtitle_lang,
             audio_idx, audio_lang, audio_codec,
             transcode_mode, bitrate_kbps, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(user_sub, scope_key) DO UPDATE SET
             subtitle_id    = excluded.subtitle_id,
             subtitle_lang  = excluded.subtitle_lang,
             audio_idx      = excluded.audio_idx,
             audio_lang     = excluded.audio_lang,
             audio_codec    = excluded.audio_codec,
             transcode_mode = excluded.transcode_mode,
             bitrate_kbps   = excluded.bitrate_kbps,
             updated_at     = excluded.updated_at",
    )
    .bind(&session.user_sub)
    .bind(&scope)
    .bind(&body.subtitle_id)
    .bind(&body.subtitle_lang)
    .bind(body.audio_idx.map(|n| n as i64))
    .bind(&body.audio_lang)
    .bind(&body.audio_codec)
    .bind(&body.transcode_mode)
    .bind(body.bitrate_kbps.map(|n| n as i64))
    .bind(now_secs())
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
