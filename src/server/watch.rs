//! Per-user watch progress + "Continue Watching" row.
//!
//! Stored in `watch_progress` (user_sub, media_id) → position/duration/completed.
//! Heartbeat from the player upserts; the home page asks for the row.

use super::auth::Session;
use super::error::{Error, Result};
use super::AppState;
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use sqlx::FromRow;

use crate::types::{ContinueItem, ProgressReport, WatchProgress};

const COMPLETION_RATIO: f64 = 0.9;
const ROW_TTL_SECS: i64 = 31 * 86_400;
const ROW_LIMIT: usize = 20;

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub async fn report_progress(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
    Json(body): Json<ProgressReport>,
) -> Result<StatusCode> {
    if !body.position_secs.is_finite() || body.position_secs < 0.0 {
        return Err(Error::BadRequest("position_secs invalid".into()));
    }
    if !body.duration_secs.is_finite() || body.duration_secs < 0.0 {
        return Err(Error::BadRequest("duration_secs invalid".into()));
    }
    let completed = if body.duration_secs > 0.0 {
        body.position_secs / body.duration_secs > COMPLETION_RATIO
    } else {
        false
    };
    sqlx::query(
        "INSERT INTO watch_progress (user_sub, media_id, position_secs, duration_secs, completed, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(user_sub, media_id) DO UPDATE SET
             position_secs = excluded.position_secs,
             duration_secs = excluded.duration_secs,
             completed     = excluded.completed,
             updated_at    = excluded.updated_at",
    )
    .bind(&session.user_sub)
    .bind(&id)
    .bind(body.position_secs)
    .bind(body.duration_secs)
    .bind(completed as i64)
    .bind(now_secs())
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Force-mark a media item as watched so it drops off "Continue Watching"
/// (or, for shows, rolls forward to the next episode). Idempotent.
pub async fn mark_watched(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    let existing: Option<(f64,)> = sqlx::query_as(
        "SELECT duration_secs FROM watch_progress WHERE user_sub = ? AND media_id = ?",
    )
    .bind(&session.user_sub)
    .bind(&id)
    .fetch_optional(&state.pool)
    .await?;
    // Use the real duration when we know it (so the bar fills); otherwise a
    // 1.0 placeholder is enough for `completed = position/duration > 0.9`
    // to read true on subsequent reads.
    let duration = existing.map(|(d,)| d).filter(|d| *d > 0.0).unwrap_or(1.0);
    sqlx::query(
        "INSERT INTO watch_progress (user_sub, media_id, position_secs, duration_secs, completed, updated_at)
         VALUES (?, ?, ?, ?, 1, ?)
         ON CONFLICT(user_sub, media_id) DO UPDATE SET
             position_secs = excluded.duration_secs,
             completed     = 1,
             updated_at    = excluded.updated_at",
    )
    .bind(&session.user_sub)
    .bind(&id)
    .bind(duration)
    .bind(duration)
    .bind(now_secs())
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Clear the watch_progress row for a media item — i.e. mark unwatched. The
/// row vanishes from "Continue Watching" without leaving a "completed"
/// crumb that would surface the next episode.
pub async fn mark_unwatched(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    sqlx::query("DELETE FROM watch_progress WHERE user_sub = ? AND media_id = ?")
        .bind(&session.user_sub)
        .bind(&id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(FromRow)]
struct ProgressRow {
    position_secs: f64,
    duration_secs: f64,
    completed: i64,
    updated_at: i64,
}

pub async fn get_progress(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> Result<Json<Option<WatchProgress>>> {
    let row: Option<ProgressRow> = sqlx::query_as(
        "SELECT position_secs, duration_secs, completed, updated_at
         FROM watch_progress WHERE user_sub = ? AND media_id = ?",
    )
    .bind(&session.user_sub)
    .bind(&id)
    .fetch_optional(&state.pool)
    .await?;
    Ok(Json(row.map(|r| WatchProgress {
        media_id: id,
        position_secs: r.position_secs,
        duration_secs: r.duration_secs,
        completed: r.completed != 0,
        updated_at: r.updated_at,
    })))
}

#[derive(FromRow)]
struct CwRow {
    media_id: String,
    kind: String,
    title: String,
    show_id: Option<String>,
    show_title: Option<String>,
    season_number: Option<i64>,
    episode_number: Option<i64>,
    position_secs: f64,
    duration_secs: f64,
    completed: i64,
}

#[derive(FromRow)]
struct NextEp {
    id: String,
    title: String,
    season_number: Option<i64>,
    episode_number: Option<i64>,
}

pub async fn continue_watching(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
) -> Result<Json<Vec<ContinueItem>>> {
    let cutoff = now_secs() - ROW_TTL_SECS;

    let rows: Vec<CwRow> = sqlx::query_as(
        "SELECT m.id            AS media_id,
                m.kind          AS kind,
                m.title         AS title,
                m.show_id       AS show_id,
                s.title         AS show_title,
                m.season_number AS season_number,
                m.episode_number AS episode_number,
                wp.position_secs AS position_secs,
                wp.duration_secs AS duration_secs,
                wp.completed    AS completed
         FROM watch_progress wp
         JOIN media m ON m.id = wp.media_id
         LEFT JOIN shows s ON s.id = m.show_id
         WHERE wp.user_sub = ? AND wp.updated_at > ?
         ORDER BY wp.updated_at DESC",
    )
    .bind(&session.user_sub)
    .bind(cutoff)
    .fetch_all(&state.pool)
    .await?;

    let mut out: Vec<ContinueItem> = Vec::with_capacity(ROW_LIMIT);
    let mut seen_shows: std::collections::HashSet<String> = std::collections::HashSet::new();

    for r in rows {
        if out.len() >= ROW_LIMIT {
            break;
        }
        match r.kind.as_str() {
            "movie" => {
                if r.completed != 0 {
                    continue;
                }
                out.push(ContinueItem {
                    media_id: r.media_id,
                    kind: "movie".into(),
                    title: r.title,
                    show_id: None,
                    position_secs: r.position_secs,
                    duration_secs: r.duration_secs,
                });
            }
            "episode" => {
                let Some(show_id) = r.show_id.clone() else { continue };
                if !seen_shows.insert(show_id.clone()) {
                    continue;
                }
                let show_title = r.show_title.clone().unwrap_or_default();
                if r.completed == 0 {
                    out.push(ContinueItem {
                        media_id: r.media_id,
                        kind: "episode".into(),
                        title: format_episode_title(
                            &show_title,
                            r.season_number,
                            r.episode_number,
                            &r.title,
                        ),
                        show_id: Some(show_id),
                        position_secs: r.position_secs,
                        duration_secs: r.duration_secs,
                    });
                } else if let Some(next) = next_episode(
                    &state.pool,
                    &show_id,
                    r.season_number.unwrap_or(0),
                    r.episode_number.unwrap_or(0),
                )
                .await?
                {
                    if user_completed(&state.pool, &session.user_sub, &next.id).await? {
                        continue;
                    }
                    out.push(ContinueItem {
                        media_id: next.id,
                        kind: "episode".into(),
                        title: format_episode_title(
                            &show_title,
                            next.season_number,
                            next.episode_number,
                            &next.title,
                        ),
                        show_id: Some(show_id),
                        position_secs: 0.0,
                        duration_secs: 0.0,
                    });
                }
            }
            _ => {}
        }
    }

    Ok(Json(out))
}

fn format_episode_title(
    show: &str,
    season: Option<i64>,
    episode: Option<i64>,
    ep_title: &str,
) -> String {
    match (season, episode) {
        (Some(s), Some(e)) => format!("{show} — S{s}E{e:02} — {ep_title}"),
        _ => format!("{show} — {ep_title}"),
    }
}

async fn next_episode(
    pool: &sqlx::SqlitePool,
    show_id: &str,
    after_season: i64,
    after_episode: i64,
) -> Result<Option<NextEp>> {
    let row: Option<NextEp> = sqlx::query_as(
        "SELECT id, title, season_number, episode_number
         FROM media
         WHERE kind = 'episode' AND show_id = ?
           AND ( (season_number = ? AND episode_number > ?)
              OR (season_number > ?) )
         ORDER BY season_number ASC, episode_number ASC
         LIMIT 1",
    )
    .bind(show_id)
    .bind(after_season)
    .bind(after_episode)
    .bind(after_season)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

async fn user_completed(pool: &sqlx::SqlitePool, user_sub: &str, media_id: &str) -> Result<bool> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT completed FROM watch_progress WHERE user_sub = ? AND media_id = ?",
    )
    .bind(user_sub)
    .bind(media_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(c,)| c != 0).unwrap_or(false))
}
