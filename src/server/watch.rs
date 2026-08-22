//! Per-user watch progress, rewatch passes, and the "Continue Watching" row.
//!
//! `watch_progress (user_sub, media_id)` holds two things that answer different
//! questions, and keeping them apart is the whole design:
//!
//!   * **lifetime history** — `completed` / `last_completed_at`. Monotonic: a
//!     progress report only ever sets these, never clears them. Otherwise ten
//!     seconds into a rewatch the "you have seen this" bit is gone for good.
//!   * **the current position** — `position_secs` / `updated_at`. Overwritten
//!     freely; meaningful only relative to the pass it was written in.
//!
//! A rewatch is a *declared* pass (`watch_scope_state`, migration 0027) rather
//! than something inferred from playback, so every per-item question reduces to
//! a timestamp comparison against `started_at`:
//!
//!   done this pass    = completed && last_completed_at >= started_at
//!   touched this pass = updated_at >= started_at
//!
//! `started_at = 0` means "no pass running", and both comparisons then degrade
//! to exactly the pre-pass behaviour — which is why ending a rewatch zeroes the
//! column rather than deleting the row, and why nothing here special-cases the
//! absence of a pass.
//!
//! Hiding a tile from Continue Watching is scope state too (`hidden_at`), not a
//! flag on whichever episode row happened to be newest. Same trick: the hide
//! holds only while nothing newer has happened in the scope, so it clears
//! itself without a write.

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

/// Did this row's last completion land inside the current pass? With
/// `pass_start = 0` this is just "have they ever finished it".
fn done_in_pass(completed: i64, last_completed_at: Option<i64>, pass_start: i64) -> bool {
    completed != 0 && last_completed_at.unwrap_or(0) >= pass_start
}

/// Is a hide still in force? `latest_activity` is the most recent thing that
/// happened in the scope — the newest progress row, or the pass start.
///
/// Making this a comparison rather than a flag is what keeps the hide honest
/// with no code to clear it: reporting progress, marking an episode watched,
/// and starting a rewatch all stamp a later timestamp, so each brings the tile
/// back on its own. `mark_watched` un-hiding a show is therefore defined
/// behaviour — activity in the scope un-hides it — rather than a side effect of
/// which row Continue Watching happened to read.
fn scope_hidden(hidden_at: i64, latest_activity: i64) -> bool {
    hidden_at > 0 && hidden_at >= latest_activity
}

/// Pass scope for a media item: `show:<id>` for an episode (a rewatch spans the
/// whole series), `media:<id>` for a movie. Mirrors `media_preferences`.
async fn scope_key(pool: &sqlx::SqlitePool, media_id: &str) -> Result<String> {
    let row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT kind, show_id FROM media WHERE id = ?")
            .bind(media_id)
            .fetch_optional(pool)
            .await?;
    Ok(match row {
        Some((kind, Some(show_id))) if kind == "episode" => format!("show:{show_id}"),
        _ => format!("media:{media_id}"),
    })
}

/// `(pass_no, started_at)` of the running pass for `scope`, or `None`.
///
/// Deliberately blind to `hidden_at`: hiding only suppresses the Continue
/// Watching tile. The show page still shows the rewatch, resume still lands
/// inside it, and progress still counts towards it.
async fn active_pass(
    pool: &sqlx::SqlitePool,
    user_sub: &str,
    scope: &str,
) -> Result<Option<(i64, i64)>> {
    Ok(sqlx::query_as(
        "SELECT pass_no, started_at FROM watch_scope_state
         WHERE user_sub = ? AND scope_key = ? AND started_at > 0",
    )
    .bind(user_sub)
    .bind(scope)
    .fetch_optional(pool)
    .await?)
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
    let crossed =
        body.duration_secs > 0.0 && body.position_secs / body.duration_secs > COMPLETION_RATIO;
    let now = now_secs();
    let scope = scope_key(&state.pool, &id).await?;
    let pass_start = active_pass(&state.pool, &session.user_sub, &scope)
        .await?
        .map(|(_, started_at)| started_at)
        .unwrap_or(0);

    // `completed` is set, never cleared. `last_completed_at` is stamped only
    // when this pass hasn't recorded a completion yet, so replaying the tail of
    // an episode doesn't keep moving the date the user finished it — and with
    // no pass running (`pass_start = 0`) it can never be re-stamped at all,
    // which is what keeps "went back for the credits of episode 6" from
    // looking like progress in a rewatch that hasn't been started yet.
    sqlx::query(
        "INSERT INTO watch_progress
             (user_sub, media_id, position_secs, duration_secs, completed,
              last_completed_at, updated_at)
         VALUES (?, ?, ?, ?, ?, CASE WHEN ? THEN ? END, ?)
         ON CONFLICT(user_sub, media_id) DO UPDATE SET
             position_secs = excluded.position_secs,
             duration_secs = excluded.duration_secs,
             completed     = MAX(watch_progress.completed, excluded.completed),
             last_completed_at = CASE
                 WHEN excluded.completed = 1
                  AND (watch_progress.last_completed_at IS NULL
                       OR watch_progress.last_completed_at < ?)
                 THEN excluded.updated_at
                 ELSE watch_progress.last_completed_at END,
             updated_at    = excluded.updated_at",
    )
    .bind(&session.user_sub)
    .bind(&id)
    .bind(body.position_secs)
    .bind(body.duration_secs)
    .bind(crossed as i64)
    .bind(crossed)
    .bind(now)
    .bind(now)
    .bind(pass_start)
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
    // Unlike a heartbeat this always stamps `last_completed_at` — it's an
    // explicit "I'm done with this", so it counts for the current pass however
    // long ago the item was last finished.
    sqlx::query(
        "INSERT INTO watch_progress
             (user_sub, media_id, position_secs, duration_secs, completed,
              last_completed_at, updated_at)
         VALUES (?, ?, ?, ?, 1, ?, ?)
         ON CONFLICT(user_sub, media_id) DO UPDATE SET
             position_secs     = excluded.duration_secs,
             completed         = 1,
             last_completed_at = excluded.last_completed_at,
             updated_at        = excluded.updated_at",
    )
    .bind(&session.user_sub)
    .bind(&id)
    .bind(duration)
    .bind(duration)
    .bind(now_secs())
    .bind(now_secs())
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Hide a Continue Watching tile.
///
/// Scope-level, so it doesn't matter whether the episode the tile points at has
/// a `watch_progress` row yet — an "up next" or rewatch tile routinely points at
/// one that doesn't. Continue Watching only ever shows one tile per show, so
/// hiding the show is what the button has always meant.
///
/// Nothing needs to undo this: the hide lapses as soon as anything newer happens
/// in the scope (see `scope_hidden`).
pub async fn dismiss_cw(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    let scope = scope_key(&state.pool, &id).await?;
    sqlx::query(
        "INSERT INTO watch_scope_state (user_sub, scope_key, hidden_at)
         VALUES (?, ?, ?)
         ON CONFLICT(user_sub, scope_key) DO UPDATE SET hidden_at = excluded.hidden_at",
    )
    .bind(&session.user_sub)
    .bind(&scope)
    .bind(now_secs())
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Clear the watch_progress row for a media item — i.e. mark unwatched. The
/// row vanishes from "Continue Watching" without leaving a "completed"
/// crumb that would surface the next episode. Forgets the item's history
/// too: there's nothing left to say the user ever finished it.
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

// ---- Rewatch passes ----

/// Is there anything left to watch in a pass anchored at `pass_start`? Decides
/// whether pressing Rewatch resumes an existing pass or stamps a new one.
async fn pass_has_work(
    pool: &sqlx::SqlitePool,
    user_sub: &str,
    scope: &str,
    pass_start: i64,
) -> Result<bool> {
    if let Some(show_id) = scope.strip_prefix("show:") {
        let unfinished: Option<(i64,)> = sqlx::query_as(
            "SELECT 1 FROM media m
             LEFT JOIN watch_progress wp ON wp.media_id = m.id AND wp.user_sub = ?
             WHERE m.kind = 'episode' AND m.show_id = ? AND m.deleted_at IS NULL
               AND NOT (COALESCE(wp.completed, 0) = 1
                        AND COALESCE(wp.last_completed_at, 0) >= ?)
             LIMIT 1",
        )
        .bind(user_sub)
        .bind(show_id)
        .bind(pass_start)
        .fetch_optional(pool)
        .await?;
        return Ok(unfinished.is_some());
    }
    let media_id = scope.strip_prefix("media:").unwrap_or(scope);
    let row: Option<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT completed, last_completed_at FROM watch_progress
         WHERE user_sub = ? AND media_id = ?",
    )
    .bind(user_sub)
    .bind(media_id)
    .fetch_optional(pool)
    .await?;
    Ok(match row {
        Some((completed, last)) => !done_in_pass(completed, last, pass_start),
        None => true,
    })
}

/// Begin a rewatch. A pass that still has something left in it is resumed
/// rather than restarted — dismissing the Continue Watching tile, or just
/// leaving it alone for a while, must not cost the user their place. Anything
/// else (no pass, or one that's been finished or explicitly ended) stamps a
/// fresh `started_at`.
///
/// Both branches clear `hidden_at`, so pressing Rewatch on a hidden show brings
/// its tile back.
async fn begin_pass(pool: &sqlx::SqlitePool, user_sub: &str, scope: &str) -> Result<()> {
    let existing: Option<(i64,)> = sqlx::query_as(
        "SELECT started_at FROM watch_scope_state
         WHERE user_sub = ? AND scope_key = ? AND started_at > 0",
    )
    .bind(user_sub)
    .bind(scope)
    .fetch_optional(pool)
    .await?;
    if let Some((started_at,)) = existing {
        if pass_has_work(pool, user_sub, scope, started_at).await? {
            sqlx::query(
                "UPDATE watch_scope_state SET hidden_at = 0
                 WHERE user_sub = ? AND scope_key = ?",
            )
            .bind(user_sub)
            .bind(scope)
            .execute(pool)
            .await?;
            return Ok(());
        }
    }
    sqlx::query(
        "INSERT INTO watch_scope_state (user_sub, scope_key, pass_no, started_at, hidden_at)
         VALUES (?, ?, 2, ?, 0)
         ON CONFLICT(user_sub, scope_key) DO UPDATE SET
             pass_no    = watch_scope_state.pass_no + 1,
             started_at = excluded.started_at,
             hidden_at  = 0",
    )
    .bind(user_sub)
    .bind(scope)
    .bind(now_secs())
    .execute(pool)
    .await?;
    Ok(())
}

/// End a rewatch outright — the deliberate show-page action, as opposed to
/// hiding its tile. Zeroing `started_at` discards the anchor, so the next
/// rewatch starts from the beginning; that's the point of the distinction.
/// Zero *is* "no pass" for every comparison in this module, so the row can stay
/// and keep its `pass_no`.
async fn end_pass(pool: &sqlx::SqlitePool, user_sub: &str, scope: &str) -> Result<()> {
    sqlx::query(
        "UPDATE watch_scope_state SET started_at = 0, hidden_at = 0
         WHERE user_sub = ? AND scope_key = ?",
    )
    .bind(user_sub)
    .bind(scope)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn start_show_rewatch(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    begin_pass(&state.pool, &session.user_sub, &format!("show:{id}")).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn end_show_rewatch(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    end_pass(&state.pool, &session.user_sub, &format!("show:{id}")).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn start_media_rewatch(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    begin_pass(&state.pool, &session.user_sub, &format!("media:{id}")).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn end_media_rewatch(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    end_pass(&state.pool, &session.user_sub, &format!("media:{id}")).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Reads ----

#[derive(FromRow)]
struct ProgressRow {
    position_secs: f64,
    duration_secs: f64,
    completed: i64,
    last_completed_at: Option<i64>,
    updated_at: i64,
}

pub async fn get_progress(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> Result<Json<Option<WatchProgress>>> {
    let row: Option<ProgressRow> = sqlx::query_as(
        "SELECT position_secs, duration_secs, completed, last_completed_at, updated_at
         FROM watch_progress WHERE user_sub = ? AND media_id = ?",
    )
    .bind(&session.user_sub)
    .bind(&id)
    .fetch_optional(&state.pool)
    .await?;
    let scope = scope_key(&state.pool, &id).await?;
    let pass = active_pass(&state.pool, &session.user_sub, &scope).await?;
    let pass_start = pass.map(|(_, s)| s).unwrap_or(0);
    Ok(Json(row.map(|r| {
        // Resume only a position that belongs to the current pass and isn't
        // already spent: a leftover from a previous pass (or from having
        // finished the thing) starts over at zero.
        let done = done_in_pass(r.completed, r.last_completed_at, pass_start);
        let resume_secs = if !done && r.updated_at >= pass_start {
            r.position_secs
        } else {
            0.0
        };
        WatchProgress {
            media_id: id,
            position_secs: r.position_secs,
            duration_secs: r.duration_secs,
            completed: r.completed != 0,
            updated_at: r.updated_at,
            resume_secs,
            // Reported only while this item still has something left in the
            // pass, so a finished rewatch reads as plain "watched" again and
            // the button offers a fresh one.
            rewatch_pass: if done { None } else { pass.map(|(n, _)| n) },
        }
    })))
}

#[derive(FromRow)]
struct CwRow {
    media_id: String,
    kind: String,
    title: String,
    year: Option<i64>,
    show_id: Option<String>,
    show_title: Option<String>,
    season_number: Option<i64>,
    episode_number: Option<i64>,
    position_secs: f64,
    duration_secs: f64,
    completed: i64,
    last_completed_at: Option<i64>,
    updated_at: i64,
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

    // Per-scope state: `(started_at, hidden_at)`, both zero when absent. Tiny —
    // one row per thing being rewatched or hidden. This is the only read in the
    // module that consults `hidden_at`; a hidden scope is still mid-rewatch
    // everywhere else.
    let scope_state: std::collections::HashMap<String, (i64, i64)> = sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT scope_key, started_at, hidden_at FROM watch_scope_state WHERE user_sub = ?",
    )
    .bind(&session.user_sub)
    .fetch_all(&state.pool)
    .await?
    .into_iter()
    .map(|(scope, started_at, hidden_at)| (scope, (started_at, hidden_at)))
    .collect();

    let rows: Vec<CwRow> = sqlx::query_as(
        "SELECT m.id            AS media_id,
                m.kind          AS kind,
                m.title         AS title,
                m.year          AS year,
                m.show_id       AS show_id,
                s.title         AS show_title,
                m.season_number AS season_number,
                m.episode_number AS episode_number,
                wp.position_secs AS position_secs,
                wp.duration_secs AS duration_secs,
                wp.completed    AS completed,
                wp.last_completed_at AS last_completed_at,
                wp.updated_at   AS updated_at
         FROM watch_progress wp
         JOIN media m ON m.id = wp.media_id AND m.deleted_at IS NULL
         LEFT JOIN shows s ON s.id = m.show_id AND s.deleted_at IS NULL
         WHERE wp.user_sub = ? AND wp.updated_at > ?
         ORDER BY wp.updated_at DESC",
    )
    .bind(&session.user_sub)
    .bind(cutoff)
    .fetch_all(&state.pool)
    .await?;

    // Tiles are collected with the timestamp they should sort on, because a
    // pass that was just started outranks the stale rows it supersedes.
    let mut out: Vec<(i64, ContinueItem)> = Vec::new();
    let mut seen_shows: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_movies: std::collections::HashSet<String> = std::collections::HashSet::new();

    for r in rows {
        // Each row can cost a lookup or two below, so stop once no further
        // candidate could survive the truncation: a running pass is the only
        // thing that can outrank an already-collected tile, and there are at
        // most `passes.len()` of those.
        if out.len() >= ROW_LIMIT + scope_state.len() {
            break;
        }
        match r.kind.as_str() {
            "movie" => {
                seen_movies.insert(r.media_id.clone());
                let (pass_start, hidden_at) = scope_state
                    .get(&format!("media:{}", r.media_id))
                    .copied()
                    .unwrap_or((0, 0));
                if scope_hidden(hidden_at, r.updated_at.max(pass_start)) {
                    continue;
                }
                if done_in_pass(r.completed, r.last_completed_at, pass_start) {
                    continue;
                }
                // A position written before the pass started belongs to the
                // previous viewing; the tile shows a fresh start.
                let fresh = r.updated_at >= pass_start;
                out.push((
                    r.updated_at.max(pass_start),
                    ContinueItem {
                        media_id: r.media_id,
                        kind: "movie".into(),
                        title: r.title,
                        show_id: None,
                        show_title: None,
                        season_number: None,
                        episode_number: None,
                        year: r.year,
                        position_secs: if fresh { r.position_secs } else { 0.0 },
                        duration_secs: if fresh { r.duration_secs } else { 0.0 },
                    },
                ));
            }
            "episode" => {
                let Some(show_id) = r.show_id.clone() else { continue };
                if !seen_shows.insert(show_id.clone()) {
                    continue;
                }
                let (pass_start, hidden_at) = scope_state
                    .get(&format!("show:{show_id}"))
                    .copied()
                    .unwrap_or((0, 0));
                // Hiding the scope suppresses the show outright, rather than
                // letting it fall through to the anchor path below. That path
                // reads the sticky `completed` on the episode being rewatched as
                // "finished" and rolls forward, so a show hidden mid-rewatch
                // would otherwise come straight back wearing the next episode.
                if scope_hidden(hidden_at, r.updated_at.max(pass_start)) {
                    continue;
                }
                if pass_start > 0 {
                    // Under a declared pass the frontier is absolute — the first
                    // episode of the show not yet done for this pass — rather
                    // than "the next one after whatever row was touched last".
                    // Replaying an old episode mid-rewatch therefore can't drag
                    // the tile forward past episodes still to come.
                    if let Some(item) =
                        pass_tile(&state.pool, &session.user_sub, &show_id, pass_start).await?
                    {
                        out.push((r.updated_at.max(pass_start), item));
                    }
                    continue;
                }
                if r.completed == 0 {
                    out.push((
                        r.updated_at,
                        ContinueItem {
                                media_id: r.media_id,
                            kind: "episode".into(),
                            title: r.title,
                            show_id: Some(show_id),
                            show_title: r.show_title,
                            season_number: r.season_number,
                            episode_number: r.episode_number,
                            year: None,
                            position_secs: r.position_secs,
                            duration_secs: r.duration_secs,
                        },
                    ));
                } else if let Some(next) = next_unwatched_episode(
                    &state.pool,
                    &session.user_sub,
                    &show_id,
                    r.season_number.unwrap_or(0),
                    r.episode_number.unwrap_or(0),
                )
                .await?
                {
                    out.push((
                        r.updated_at,
                        ContinueItem {
                            media_id: next.id,
                            kind: "episode".into(),
                            title: next.title,
                            show_id: Some(show_id),
                            show_title: r.show_title,
                            season_number: next.season_number,
                            episode_number: next.episode_number,
                            year: None,
                            position_secs: 0.0,
                            duration_secs: 0.0,
                        },
                    ));
                }
            }
            _ => {}
        }
    }

    // Anything with a running pass but no recent row of its own — a show last
    // touched years ago, say. Pressing Rewatch has to surface it, and the
    // 31-day window above would never have found it.
    for (scope, (pass_start, hidden_at)) in &scope_state {
        if *pass_start == 0 || scope_hidden(*hidden_at, *pass_start) {
            continue;
        }
        if let Some(show_id) = scope.strip_prefix("show:") {
            if seen_shows.contains(show_id) {
                continue;
            }
            if let Some(item) =
                pass_tile(&state.pool, &session.user_sub, show_id, *pass_start).await?
            {
                out.push((*pass_start, item));
            }
        } else if let Some(media_id) = scope.strip_prefix("media:") {
            if seen_movies.contains(media_id) {
                continue;
            }
            if let Some(item) =
                movie_pass_tile(&state.pool, &session.user_sub, media_id, *pass_start).await?
            {
                out.push((*pass_start, item));
            }
        }
    }

    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.truncate(ROW_LIMIT);
    Ok(Json(out.into_iter().map(|(_, item)| item).collect()))
}

#[derive(FromRow)]
struct FrontierRow {
    id: String,
    title: String,
    season_number: Option<i64>,
    episode_number: Option<i64>,
    position_secs: f64,
    duration_secs: f64,
    updated_at: i64,
}

/// The Continue Watching tile for a show with a running rewatch: its earliest
/// episode not yet finished in this pass. `None` once the pass is complete.
async fn pass_tile(
    pool: &sqlx::SqlitePool,
    user_sub: &str,
    show_id: &str,
    pass_start: i64,
) -> Result<Option<ContinueItem>> {
    let show: Option<(String,)> =
        sqlx::query_as("SELECT title FROM shows WHERE id = ? AND deleted_at IS NULL")
            .bind(show_id)
            .fetch_optional(pool)
            .await?;
    let Some((show_title,)) = show else {
        return Ok(None);
    };
    let row: Option<FrontierRow> = sqlx::query_as(
        "SELECT m.id, m.title, m.season_number, m.episode_number,
                COALESCE(wp.position_secs, 0.0) AS position_secs,
                COALESCE(wp.duration_secs, 0.0) AS duration_secs,
                COALESCE(wp.updated_at, 0)      AS updated_at
         FROM media m
         LEFT JOIN watch_progress wp ON wp.media_id = m.id AND wp.user_sub = ?
         WHERE m.kind = 'episode' AND m.show_id = ? AND m.deleted_at IS NULL
           AND NOT (COALESCE(wp.completed, 0) = 1
                    AND COALESCE(wp.last_completed_at, 0) >= ?)
         ORDER BY m.season_number ASC, m.episode_number ASC
         LIMIT 1",
    )
    .bind(user_sub)
    .bind(show_id)
    .bind(pass_start)
    .fetch_optional(pool)
    .await?;
    let Some(r) = row else { return Ok(None) };
    let fresh = r.updated_at >= pass_start;
    Ok(Some(ContinueItem {
        media_id: r.id,
        kind: "episode".into(),
        title: r.title,
        show_id: Some(show_id.to_string()),
        show_title: Some(show_title),
        season_number: r.season_number,
        episode_number: r.episode_number,
        year: None,
        position_secs: if fresh { r.position_secs } else { 0.0 },
        duration_secs: if fresh { r.duration_secs } else { 0.0 },
    }))
}

#[derive(FromRow)]
struct MoviePassRow {
    title: String,
    year: Option<i64>,
    position_secs: f64,
    duration_secs: f64,
    completed: i64,
    last_completed_at: Option<i64>,
    updated_at: i64,
}

/// Same idea as `pass_tile` for a movie being rewatched.
async fn movie_pass_tile(
    pool: &sqlx::SqlitePool,
    user_sub: &str,
    media_id: &str,
    pass_start: i64,
) -> Result<Option<ContinueItem>> {
    let row: Option<MoviePassRow> = sqlx::query_as(
        "SELECT m.title, m.year,
                COALESCE(wp.position_secs, 0.0) AS position_secs,
                COALESCE(wp.duration_secs, 0.0) AS duration_secs,
                COALESCE(wp.completed, 0)       AS completed,
                wp.last_completed_at            AS last_completed_at,
                COALESCE(wp.updated_at, 0)      AS updated_at
         FROM media m
         LEFT JOIN watch_progress wp ON wp.media_id = m.id AND wp.user_sub = ?
         WHERE m.id = ? AND m.kind = 'movie' AND m.deleted_at IS NULL",
    )
    .bind(user_sub)
    .bind(media_id)
    .fetch_optional(pool)
    .await?;
    let Some(r) = row else { return Ok(None) };
    if done_in_pass(r.completed, r.last_completed_at, pass_start) {
        return Ok(None);
    }
    let fresh = r.updated_at >= pass_start;
    Ok(Some(ContinueItem {
        media_id: media_id.to_string(),
        kind: "movie".into(),
        title: r.title,
        show_id: None,
        show_title: None,
        season_number: None,
        episode_number: None,
        year: r.year,
        position_secs: if fresh { r.position_secs } else { 0.0 },
        duration_secs: if fresh { r.duration_secs } else { 0.0 },
    }))
}

/// The show's frontier after `(after_season, after_episode)`: the first episode
/// in order the user has never finished.
///
/// Skipping over episodes they *have* finished, rather than giving up at the
/// first one, is what stops a poke at an old episode from wiping the show out of
/// Continue Watching — replay the credits of episode 3 of a show you've seen to
/// episode 5, and the frontier is still episode 6, not nothing. This leans on
/// `completed` being sticky, so a rewatch can't blur where that frontier is.
///
/// Note "never finished", not "not finished since some timestamp": an
/// undeclared rewatch is deliberately not inferred here. Declaring one via
/// `watch_scope_state` is what changes the answer, and that path doesn't come
/// through this function at all.
async fn next_unwatched_episode(
    pool: &sqlx::SqlitePool,
    user_sub: &str,
    show_id: &str,
    after_season: i64,
    after_episode: i64,
) -> Result<Option<NextEp>> {
    let row: Option<NextEp> = sqlx::query_as(
        "SELECT m.id, m.title, m.season_number, m.episode_number
         FROM media m
         LEFT JOIN watch_progress wp ON wp.media_id = m.id AND wp.user_sub = ?
         WHERE m.kind = 'episode' AND m.show_id = ? AND m.deleted_at IS NULL
           AND ( (m.season_number = ? AND m.episode_number > ?)
              OR (m.season_number > ?) )
           AND COALESCE(wp.completed, 0) = 0
         ORDER BY m.season_number ASC, m.episode_number ASC
         LIMIT 1",
    )
    .bind(user_sub)
    .bind(show_id)
    .bind(after_season)
    .bind(after_episode)
    .bind(after_season)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}
