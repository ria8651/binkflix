use super::analytics::{self, PlaybackSample};
use super::error::{Error, Result};
use super::{media_info, subtitles, thumbnails, trickplay};
use super::AppState;
use axum::extract::{Path, Request, State};
use axum_extra::extract::Query;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::path::PathBuf;
use tower::ServiceExt;
use tower_http::services::ServeFile;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/library", get(library))
        .route("/api/media/{id}", get(media))
        .route("/api/media/{id}/stream", get(super::remux::media_stream))
        .route("/api/media/{id}/subtitles", get(media_subtitles))
        .route("/api/media/{id}/subtitle/{track}", get(media_subtitle))
        .route("/api/media/{id}/tech", get(media_tech))
        .route("/api/media/{id}/image", get(media_image))
        .route("/api/media/{id}/trickplay.json", get(media_trickplay_manifest))
        .route("/api/media/{id}/trickplay.jpg", get(media_trickplay_sprite))
        .route("/api/media/{id}/fanart", get(media_fanart))
        .route("/api/shows/{id}", get(show))
        .route("/api/shows/{id}/poster", get(show_poster))
        .route("/api/shows/{id}/fanart", get(show_fanart))
        .route("/api/shows/{id}/clearlogo", get(show_clearlogo))
        .route("/api/shows/{id}/banner", get(show_banner))
        .route("/api/shows/{id}/seasons/{n}/poster", get(season_poster))
        .route("/api/rooms", get(list_rooms).post(create_room))
        .route("/api/scan", post(start_scan))
        .route("/api/scan/status", get(scan_status))
        .route(
            "/api/media/{id}/progress",
            get(super::watch::get_progress).post(super::watch::report_progress),
        )
        .route("/api/continue-watching", get(super::watch::continue_watching))
        .route("/api/continue-watching/dismiss/{id}", post(super::watch::dismiss_cw))
        .route("/api/media/{id}/watched", post(super::watch::mark_watched).delete(super::watch::mark_unwatched))
        .route(
            "/api/preferences/{scope}",
            get(super::preferences::get_preferences).post(super::preferences::set_preferences),
        )
        .route("/api/playback/sample", post(playback_sample))
        .route("/api/search", get(search))
        .route("/api/genres", get(list_genres))
}

#[derive(Deserialize)]
struct PlaybackSampleBody {
    session_id: String,
    position_ms: i64,
    buffered_ahead_ms: Option<i64>,
    transcode_position_ms: Option<i64>,
    /// Transcode rate as `realtime × 100` (e.g. 125 == 1.25× realtime).
    /// Integer rather than float so SQL aggregation (avg, min, max) is
    /// straightforward and we don't pay for `REAL` storage on every row.
    transcode_rate_x100: Option<i64>,
    observed_kbps: Option<i64>,
    /// `idle` | `loading` | `stalled` — matches the HTMLMediaElement
    /// `networkState` enum surface the player can observe.
    network_state: Option<String>,
}

async fn playback_sample(
    State(state): State<AppState>,
    Json(body): Json<PlaybackSampleBody>,
) -> StatusCode {
    analytics::record_playback_sample(
        &state.pool,
        PlaybackSample {
            session_id: &body.session_id,
            position_ms: body.position_ms,
            buffered_ahead_ms: body.buffered_ahead_ms,
            transcode_position_ms: body.transcode_position_ms,
            transcode_rate_x100: body.transcode_rate_x100,
            observed_kbps: body.observed_kbps,
            network_state: body.network_state.as_deref(),
        },
    )
    .await;
    StatusCode::NO_CONTENT
}

async fn scan_status(State(state): State<AppState>) -> Json<crate::types::ScanProgress> {
    Json(state.scan_progress.read().await.clone())
}

async fn start_scan(State(state): State<AppState>) -> Json<crate::types::ScanProgress> {
    // Already running? Just return current status.
    if state.scan_progress.read().await.running {
        return Json(state.scan_progress.read().await.clone());
    }
    let pool = state.pool.clone();
    let progress = state.scan_progress.clone();
    let lock = state.scan_lock.clone();
    let libs = state.libraries.clone();
    // Mark running immediately so the client sees `running: true` on return.
    {
        let mut p = progress.write().await;
        p.running = true;
        p.phase = "starting".into();
        p.done = 0;
        p.total = 0;
        p.current = None;
        p.message = None;
    }
    tokio::spawn(async move {
        let _guard = lock.lock().await;
        super::run_scans(&pool, &libs, progress).await;
    });
    Json(state.scan_progress.read().await.clone())
}

// ---- Syncplay rooms ----

async fn list_rooms(
    State(state): State<AppState>,
) -> Result<Json<Vec<crate::types::RoomListItem>>> {
    let rooms = state.hub.list_rooms();
    let mut out = Vec::with_capacity(rooms.len());
    for (meta, room_state, viewers, members) in rooms {
        let (current_media_id, current_media_title) = match room_state {
            Some(s) => {
                let title: Option<(String,)> = sqlx::query_as(
                    "SELECT title FROM media WHERE id = ? AND deleted_at IS NULL",
                )
                .bind(&s.media_id)
                .fetch_optional(&state.pool)
                .await?;
                (Some(s.media_id), title.map(|(t,)| t))
            }
            None => (None, None),
        };
        out.push(crate::types::RoomListItem {
            id: meta.id,
            viewers,
            current_media_id,
            current_media_title,
            members,
        });
    }
    Ok(Json(out))
}

async fn create_room(
    State(state): State<AppState>,
) -> Result<Json<crate::types::CreateRoomResp>> {
    let meta = state.hub.create_room();
    Ok(Json(crate::types::CreateRoomResp { id: meta.id }))
}

async fn health() -> &'static str {
    "ok"
}

// ---- Library overview ----

#[derive(Debug, Serialize, FromRow)]
pub struct MovieSummary {
    pub id: String,
    pub title: String,
    pub year: Option<i64>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct ShowSummary {
    pub id: String,
    pub title: String,
    pub year: Option<i64>,
    pub episode_count: i64,
    pub has_banner: bool,
}

#[derive(Debug, Serialize, FromRow)]
pub struct RecentItem {
    pub media_id: String,
    pub kind: String,
    pub title: String,
    pub show_id: Option<String>,
    pub show_title: Option<String>,
    pub season_number: Option<i64>,
    pub episode_number: Option<i64>,
    pub year: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct LibraryResponse {
    pub movies: Vec<MovieSummary>,
    pub shows: Vec<ShowSummary>,
    pub recently_added: Vec<RecentItem>,
}

const RECENTLY_ADDED_LIMIT: i64 = 10;
/// Items older than this drop off the "Recently Added" row entirely, even if
/// the row isn't full — a show added six months ago shouldn't keep masquerading
/// as fresh just because the library is small.
const RECENTLY_ADDED_MAX_AGE_DAYS: i64 = 30;

async fn library(State(state): State<AppState>) -> Result<Json<LibraryResponse>> {
    let movies = sqlx::query_as::<_, MovieSummary>(
        "SELECT id, title, year FROM media \
         WHERE kind = 'movie' AND deleted_at IS NULL \
         ORDER BY sort_title",
    )
    .fetch_all(&state.pool)
    .await?;

    let shows = sqlx::query_as::<_, ShowSummary>(
        "SELECT s.id, s.title, s.year,
                (SELECT COUNT(*) FROM media m
                   WHERE m.show_id = s.id AND m.deleted_at IS NULL) AS episode_count,
                (s.banner_path IS NOT NULL) AS has_banner
         FROM shows s
         WHERE s.deleted_at IS NULL
         ORDER BY s.sort_title",
    )
    .fetch_all(&state.pool)
    .await?;

    // Episode-level "Recently Added" — newly-added playable files sorted by
    // file mtime captured at scan time. Episodes collapse per show (only the
    // freshest one per series); movies stay one-per-row. ROW_NUMBER partitions
    // episodes by `show_id` and movies by their own id (so they all rank 1).
    let recently_added = sqlx::query_as::<_, RecentItem>(
        "WITH ranked AS (
             SELECT m.id, m.kind, m.title, m.show_id,
                    m.season_number, m.episode_number, m.year,
                    COALESCE(m.added_at, m.scanned_at) AS effective_at,
                    ROW_NUMBER() OVER (
                        PARTITION BY CASE WHEN m.kind = 'episode' THEN m.show_id ELSE m.id END
                        ORDER BY COALESCE(m.added_at, m.scanned_at) DESC, m.id DESC
                    ) AS rn
             FROM media m
             WHERE m.deleted_at IS NULL
         )
         SELECT r.id  AS media_id,
                r.kind,
                r.title,
                r.show_id,
                s.title AS show_title,
                r.season_number,
                r.episode_number,
                r.year
         FROM ranked r
         LEFT JOIN shows s ON s.id = r.show_id
         WHERE r.rn = 1
           AND r.effective_at >= datetime('now', ?)
         ORDER BY r.effective_at DESC
         LIMIT ?",
    )
    .bind(format!("-{RECENTLY_ADDED_MAX_AGE_DAYS} days"))
    .bind(RECENTLY_ADDED_LIMIT)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(LibraryResponse { movies, shows, recently_added }))
}

// ---- Media (movie or episode) ----

#[derive(Debug, Serialize, FromRow)]
pub struct Media {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub original_title: Option<String>,
    pub year: Option<i64>,
    pub plot: Option<String>,
    pub runtime_minutes: Option<i64>,
    pub imdb_id: Option<String>,
    pub tmdb_id: Option<String>,
    pub file_size: i64,
    pub show_id: Option<String>,
    pub season_number: Option<i64>,
    pub episode_number: Option<i64>,
}

async fn media(State(state): State<AppState>, Path(id): Path<String>) -> Result<Json<Media>> {
    let row = sqlx::query_as::<_, Media>(
        "SELECT id, kind, title, original_title, year, plot, runtime_minutes,
                imdb_id, tmdb_id, file_size, show_id, season_number, episode_number
         FROM media WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(&id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(Error::NotFound)?;
    Ok(Json(row))
}

// ---- Show + seasons ----

#[derive(Debug, Serialize, FromRow)]
pub struct Show {
    pub id: String,
    pub title: String,
    pub original_title: Option<String>,
    pub year: Option<i64>,
    pub plot: Option<String>,
    pub imdb_id: Option<String>,
    pub tmdb_id: Option<String>,
    pub tvdb_id: Option<String>,
    pub has_clearlogo: bool,
    pub has_fanart: bool,
    pub has_banner: bool,
}

#[derive(Debug, Serialize, FromRow)]
pub struct EpisodeSummary {
    pub id: String,
    pub season_number: i64,
    pub episode_number: i64,
    pub title: String,
    pub plot: Option<String>,
    pub runtime_minutes: Option<i64>,
    pub position_secs: f64,
    pub duration_secs: f64,
    pub completed: i64,
}

#[derive(Debug, Serialize)]
pub struct Season {
    pub number: i64,
    pub episodes: Vec<EpisodeSummary>,
}

#[derive(Debug, Serialize)]
pub struct ShowResponse {
    pub show: Show,
    pub seasons: Vec<Season>,
}

async fn show(
    State(state): State<AppState>,
    Extension(session): Extension<super::auth::Session>,
    Path(id): Path<String>,
) -> Result<Json<ShowResponse>> {
    let show = sqlx::query_as::<_, Show>(
        "SELECT id, title, original_title, year, plot, imdb_id, tmdb_id, tvdb_id,
                (clearlogo_path IS NOT NULL) AS has_clearlogo,
                (fanart_path    IS NOT NULL) AS has_fanart,
                (banner_path    IS NOT NULL) AS has_banner
         FROM shows WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(&id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(Error::NotFound)?;

    let eps = sqlx::query_as::<_, EpisodeSummary>(
        "SELECT m.id,
                COALESCE(m.season_number, 0)  AS season_number,
                COALESCE(m.episode_number, 0) AS episode_number,
                m.title, m.plot, m.runtime_minutes,
                COALESCE(wp.position_secs, 0.0) AS position_secs,
                COALESCE(wp.duration_secs, 0.0) AS duration_secs,
                COALESCE(wp.completed, 0)       AS completed
         FROM media m
         LEFT JOIN watch_progress wp
           ON wp.media_id = m.id AND wp.user_sub = ?
         WHERE m.show_id = ? AND m.kind = 'episode' AND m.deleted_at IS NULL
         ORDER BY m.season_number, m.episode_number",
    )
    .bind(&session.user_sub)
    .bind(&id)
    .fetch_all(&state.pool)
    .await?;

    let mut seasons: Vec<Season> = Vec::new();
    for ep in eps {
        match seasons.last_mut() {
            Some(s) if s.number == ep.season_number => s.episodes.push(ep),
            _ => seasons.push(Season {
                number: ep.season_number,
                episodes: vec![ep],
            }),
        }
    }

    Ok(Json(ShowResponse { show, seasons }))
}

// ---- File serving ----

async fn serve(path: String, req: Request) -> Result<axum::response::Response> {
    let resp = ServeFile::new(path)
        .oneshot(req)
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!(e)))?;
    Ok(resp.into_response())
}

async fn lookup(state: &AppState, sql: &str, id: &str) -> Result<String> {
    let row: Option<(Option<String>,)> = sqlx::query_as(sql)
        .bind(id)
        .fetch_optional(&state.pool)
        .await?;
    row.and_then(|(p,)| p).ok_or(Error::NotFound)
}

async fn media_subtitles(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<crate::types::SubtitleTrack>>> {
    let tracks = subtitles::list_from_db(&state.pool, &id).await.map_err(Error::Other)?;
    Ok(Json(tracks))
}

async fn media_subtitle(
    State(state): State<AppState>,
    Path((id, track_id)): Path<(String, String)>,
) -> Result<axum::response::Response> {
    let (body, content_type) = subtitles::get_from_db(&state.pool, &id, &track_id)
        .await
        .map_err(Error::Other)?
        .ok_or(Error::NotFound)?;

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    // Subtitle content is immutable for a given media row + track_id;
    // safe to let browsers cache for a while.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=3600"),
    );
    Ok((StatusCode::OK, headers, body).into_response())
}

async fn media_tech(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<crate::types::MediaTechInfo>> {
    // Hit the cached probe first (written at scan time). Fall back to a live
    // ffprobe if the cache is empty — e.g. scan hasn't reached this row yet,
    // or the probe failed the first time and we want to retry.
    if let Some(info) = media_info::load(&state.pool, &id).await.map_err(Error::Other)? {
        return Ok(Json(info));
    }
    let path = lookup(
        &state,
        "SELECT path FROM media WHERE id = ? AND deleted_at IS NULL",
        &id,
    )
    .await?;
    let info = media_info::probe(std::path::Path::new(&path))
        .await
        .map_err(Error::Other)?;
    let _ = media_info::store(&state.pool, &id, &info).await;
    Ok(Json(info))
}

async fn media_image(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req: Request,
) -> Result<axum::response::Response> {
    // Prefer the sidecar image the library ships — it's authoritative
    // (posters, episode thumbnails). Fall back to the DB-cached generated
    // thumbnail so we don't hit the source drive on every grid render.
    let sidecar: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT image_path FROM media WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(&id)
    .fetch_optional(&state.pool)
    .await?;
    if let Some((Some(path),)) = sidecar {
        return serve(path, req).await;
    }

    if let Some((bytes, mime)) = thumbnails::get_from_db(&state.pool, &id)
        .await
        .map_err(Error::Other)?
    {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("image/jpeg")),
        );
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, max-age=3600"),
        );
        return Ok((StatusCode::OK, headers, bytes).into_response());
    }

    Err(Error::NotFound)
}

async fn media_trickplay_manifest(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<axum::response::Response> {
    match trickplay::get_manifest(&state.pool, &id)
        .await
        .map_err(Error::Other)?
    {
        Some(m) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=86400, immutable"),
            );
            Ok((StatusCode::OK, headers, Json(m)).into_response())
        }
        None => Err(Error::NotFound),
    }
}

async fn media_trickplay_sprite(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<axum::response::Response> {
    match trickplay::get_sprite(&state.pool, &id)
        .await
        .map_err(Error::Other)?
    {
        Some((bytes, mime)) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("image/jpeg")),
            );
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=86400, immutable"),
            );
            Ok((StatusCode::OK, headers, bytes).into_response())
        }
        None => Err(Error::NotFound),
    }
}

async fn media_fanart(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req: Request,
) -> Result<axum::response::Response> {
    // For episodes, prefer the parent show's fanart over the per-episode still
    // so home-page tiles read as "the show" rather than a random frame. Falls
    // back to the regular media image (movie poster / episode thumb) when no
    // fanart exists at either level.
    match lookup(
        &state,
        "SELECT fanart_path FROM media WHERE id = ? AND deleted_at IS NULL",
        &id,
    )
    .await
    {
        Ok(path) => serve(path, req).await,
        Err(Error::NotFound) => {
            let show_fanart = lookup(
                &state,
                "SELECT s.fanart_path FROM media m \
                 JOIN shows s ON s.id = m.show_id \
                 WHERE m.id = ? AND m.kind = 'episode' \
                   AND m.deleted_at IS NULL AND s.deleted_at IS NULL",
                &id,
            )
            .await;
            match show_fanart {
                Ok(path) => serve(path, req).await,
                Err(Error::NotFound) => media_image(State(state), Path(id), req).await,
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

async fn show_poster(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req: Request,
) -> Result<axum::response::Response> {
    let path = lookup(
        &state,
        "SELECT poster_path FROM shows WHERE id = ? AND deleted_at IS NULL",
        &id,
    )
    .await?;
    serve(path, req).await
}

async fn show_fanart(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req: Request,
) -> Result<axum::response::Response> {
    let path = lookup(
        &state,
        "SELECT fanart_path FROM shows WHERE id = ? AND deleted_at IS NULL",
        &id,
    )
    .await?;
    serve(path, req).await
}

async fn show_clearlogo(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req: Request,
) -> Result<axum::response::Response> {
    let path = lookup(
        &state,
        "SELECT clearlogo_path FROM shows WHERE id = ? AND deleted_at IS NULL",
        &id,
    )
    .await?;
    serve(path, req).await
}

async fn show_banner(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req: Request,
) -> Result<axum::response::Response> {
    let path = lookup(
        &state,
        "SELECT banner_path FROM shows WHERE id = ? AND deleted_at IS NULL",
        &id,
    )
    .await?;
    serve(path, req).await
}

/// Derived at request time: look for `seasonNN-poster.ext` in the show folder,
/// or `season-specials-poster.ext` for season 0.
async fn season_poster(
    State(state): State<AppState>,
    Path((id, n)): Path<(String, i64)>,
    req: Request,
) -> Result<axum::response::Response> {
    let show_path: (String,) = sqlx::query_as(
        "SELECT path FROM shows WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(&id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(Error::NotFound)?;

    let dir = PathBuf::from(show_path.0);
    let stems: Vec<String> = if n == 0 {
        vec!["season-specials-poster".to_string()]
    } else {
        vec![
            format!("season{:02}-poster", n),
            format!("season{}-poster", n),
        ]
    };

    for stem in &stems {
        for ext in &["jpg", "jpeg", "png", "webp"] {
            let p = dir.join(format!("{stem}.{ext}"));
            if p.is_file() {
                return serve(p.to_string_lossy().into_owned(), req).await;
            }
        }
    }
    Err(Error::NotFound)
}

// ---- Search ----

/// Query string for `/api/search`. Repeated `genres=` params arrive as a Vec.
/// `kind`/`watched`/`sort` are strings rather than enums so unknown values
/// degrade gracefully (treated as "any" / default).
#[derive(Debug, Deserialize)]
struct SearchParams {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    year_min: Option<i64>,
    #[serde(default)]
    year_max: Option<i64>,
    #[serde(default)]
    genres: Vec<String>,
    #[serde(default)]
    watched: Option<String>,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    offset: Option<i64>,
}

/// Cap a single page of results so a runaway request can't scan the whole
/// library into memory. The UI requests fewer than this in practice.
const SEARCH_MAX_LIMIT: i64 = 500;

async fn search(
    State(state): State<AppState>,
    Extension(session): Extension<super::auth::Session>,
    Query(p): Query<SearchParams>,
) -> Result<Json<crate::types::SearchResponse>> {
    let q = p.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let like = q.map(|s| format!("%{}%", escape_like(s)));
    let want_movies = p.kind.as_deref() != Some("show");
    let want_shows = p.kind.as_deref() != Some("movie");
    let limit = p.limit.unwrap_or(60).clamp(1, SEARCH_MAX_LIMIT);
    let offset = p.offset.unwrap_or(0).max(0);
    let watched = p.watched.as_deref().unwrap_or("any");
    let sort = p.sort.as_deref().unwrap_or(if q.is_some() { "relevance" } else { "title" });

    let (movies, total_movies) = if want_movies {
        let (sql_rows, sql_count) = build_movie_search_sql(
            like.as_deref(),
            p.year_min,
            p.year_max,
            &p.genres,
            watched,
            sort,
        );
        let rows = bind_movie_search(
            sqlx::query_as::<_, MovieSummary>(&sql_rows),
            like.as_deref(),
            p.year_min,
            p.year_max,
            &p.genres,
            watched,
            &session.user_sub,
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.pool)
        .await?;
        let total: (i64,) = bind_movie_search(
            sqlx::query_as::<_, (i64,)>(&sql_count),
            like.as_deref(),
            p.year_min,
            p.year_max,
            &p.genres,
            watched,
            &session.user_sub,
        )
        .fetch_one(&state.pool)
        .await?;
        (rows, total.0)
    } else {
        (Vec::new(), 0)
    };

    let (shows, total_shows) = if want_shows {
        let (sql_rows, sql_count) = build_show_search_sql(
            like.as_deref(),
            p.year_min,
            p.year_max,
            &p.genres,
            watched,
            sort,
        );
        let rows = bind_show_search(
            sqlx::query_as::<_, ShowSummary>(&sql_rows),
            like.as_deref(),
            p.year_min,
            p.year_max,
            &p.genres,
            watched,
            &session.user_sub,
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.pool)
        .await?;
        let total: (i64,) = bind_show_search(
            sqlx::query_as::<_, (i64,)>(&sql_count),
            like.as_deref(),
            p.year_min,
            p.year_max,
            &p.genres,
            watched,
            &session.user_sub,
        )
        .fetch_one(&state.pool)
        .await?;
        (rows, total.0)
    } else {
        (Vec::new(), 0)
    };

    let movies_out = movies
        .into_iter()
        .map(|m| crate::types::MovieSummary { id: m.id, title: m.title, year: m.year })
        .collect();
    let shows_out = shows
        .into_iter()
        .map(|s| crate::types::ShowSummary {
            id: s.id,
            title: s.title,
            year: s.year,
            episode_count: s.episode_count,
            has_banner: s.has_banner,
        })
        .collect();

    Ok(Json(crate::types::SearchResponse {
        movies: movies_out,
        shows: shows_out,
        total_movies,
        total_shows,
    }))
}

/// Escape SQL LIKE wildcards in user input so a query like "10%" doesn't
/// silently match everything. Paired with `ESCAPE '\\'` in the SQL.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '%' || c == '_' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn build_movie_search_sql(
    like: Option<&str>,
    year_min: Option<i64>,
    year_max: Option<i64>,
    genres: &[String],
    watched: &str,
    sort: &str,
) -> (String, String) {
    let mut wheres: Vec<String> = vec!["m.kind = 'movie'".into(), "m.deleted_at IS NULL".into()];
    if like.is_some() {
        wheres.push("(m.sort_title LIKE ? ESCAPE '\\' OR m.title LIKE ? ESCAPE '\\')".into());
    }
    if year_min.is_some() {
        wheres.push("m.year >= ?".into());
    }
    if year_max.is_some() {
        wheres.push("m.year <= ?".into());
    }
    if !genres.is_empty() {
        let placeholders = vec!["?"; genres.len()].join(", ");
        wheres.push(format!(
            "(SELECT COUNT(DISTINCT genre) FROM media_genres \
              WHERE media_id = m.id AND genre IN ({placeholders})) = ?",
        ));
    }
    match watched {
        "watched" => wheres.push(
            "EXISTS (SELECT 1 FROM watch_progress wp \
              WHERE wp.media_id = m.id AND wp.user_sub = ? AND wp.completed = 1)"
                .into(),
        ),
        "unwatched" => wheres.push(
            "NOT EXISTS (SELECT 1 FROM watch_progress wp \
              WHERE wp.media_id = m.id AND wp.user_sub = ? \
                AND (wp.completed = 1 OR wp.position_secs > 0))"
                .into(),
        ),
        "in_progress" => wheres.push(
            "EXISTS (SELECT 1 FROM watch_progress wp \
              WHERE wp.media_id = m.id AND wp.user_sub = ? \
                AND wp.completed = 0 AND wp.position_secs > 0)"
                .into(),
        ),
        _ => {}
    }
    let where_sql = wheres.join(" AND ");
    let order_sql = match sort {
        "title" => "m.sort_title ASC".to_string(),
        "year_desc" => "m.year IS NULL, m.year DESC, m.sort_title".to_string(),
        "year_asc" => "m.year IS NULL, m.year ASC, m.sort_title".to_string(),
        "recently_added" => "COALESCE(m.added_at, m.scanned_at) DESC, m.id DESC".to_string(),
        _ => "m.sort_title ASC".to_string(),
    };
    let rows = format!(
        "SELECT m.id, m.title, m.year FROM media m \
         WHERE {where_sql} ORDER BY {order_sql} LIMIT ? OFFSET ?"
    );
    let count = format!("SELECT COUNT(*) FROM media m WHERE {where_sql}");
    (rows, count)
}

fn bind_movie_search<'q, O>(
    mut q: sqlx::query::QueryAs<'q, sqlx::Sqlite, O, sqlx::sqlite::SqliteArguments<'q>>,
    like: Option<&'q str>,
    year_min: Option<i64>,
    year_max: Option<i64>,
    genres: &'q [String],
    watched: &str,
    user_sub: &'q str,
) -> sqlx::query::QueryAs<'q, sqlx::Sqlite, O, sqlx::sqlite::SqliteArguments<'q>>
where
    O: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> + Send + Unpin,
{
    if let Some(l) = like {
        q = q.bind(l).bind(l);
    }
    if let Some(y) = year_min {
        q = q.bind(y);
    }
    if let Some(y) = year_max {
        q = q.bind(y);
    }
    for g in genres {
        q = q.bind(g);
    }
    if !genres.is_empty() {
        q = q.bind(genres.len() as i64);
    }
    match watched {
        "watched" | "unwatched" | "in_progress" => q = q.bind(user_sub),
        _ => {}
    }
    q
}

fn build_show_search_sql(
    like: Option<&str>,
    year_min: Option<i64>,
    year_max: Option<i64>,
    genres: &[String],
    watched: &str,
    sort: &str,
) -> (String, String) {
    let mut wheres: Vec<String> = vec!["s.deleted_at IS NULL".into()];
    if like.is_some() {
        wheres.push("(s.sort_title LIKE ? ESCAPE '\\' OR s.title LIKE ? ESCAPE '\\')".into());
    }
    if year_min.is_some() {
        wheres.push("s.year >= ?".into());
    }
    if year_max.is_some() {
        wheres.push("s.year <= ?".into());
    }
    if !genres.is_empty() {
        let placeholders = vec!["?"; genres.len()].join(", ");
        wheres.push(format!(
            "(SELECT COUNT(DISTINCT genre) FROM show_genres \
              WHERE show_id = s.id AND genre IN ({placeholders})) = ?",
        ));
    }
    // Show-level watched semantics:
    // - watched     = at least one episode and every episode is completed
    // - in_progress = some progress exists but not fully watched
    // - unwatched   = no progress at all
    match watched {
        "watched" => wheres.push(
            "EXISTS (SELECT 1 FROM media m WHERE m.show_id = s.id AND m.deleted_at IS NULL) \
             AND NOT EXISTS ( \
               SELECT 1 FROM media m \
               LEFT JOIN watch_progress wp ON wp.media_id = m.id AND wp.user_sub = ? \
               WHERE m.show_id = s.id AND m.kind = 'episode' AND m.deleted_at IS NULL \
                 AND COALESCE(wp.completed, 0) = 0)"
                .into(),
        ),
        "unwatched" => wheres.push(
            "NOT EXISTS ( \
               SELECT 1 FROM media m \
               JOIN watch_progress wp ON wp.media_id = m.id \
               WHERE m.show_id = s.id AND wp.user_sub = ? AND m.deleted_at IS NULL \
                 AND (wp.completed = 1 OR wp.position_secs > 0))"
                .into(),
        ),
        "in_progress" => wheres.push(
            "EXISTS ( \
               SELECT 1 FROM media m \
               JOIN watch_progress wp ON wp.media_id = m.id \
               WHERE m.show_id = s.id AND wp.user_sub = ? AND m.deleted_at IS NULL \
                 AND (wp.completed = 1 OR wp.position_secs > 0)) \
             AND EXISTS ( \
               SELECT 1 FROM media m \
               LEFT JOIN watch_progress wp ON wp.media_id = m.id AND wp.user_sub = ? \
               WHERE m.show_id = s.id AND m.kind = 'episode' AND m.deleted_at IS NULL \
                 AND COALESCE(wp.completed, 0) = 0)"
                .into(),
        ),
        _ => {}
    }
    let where_clause = if wheres.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", wheres.join(" AND "))
    };
    let order_sql = match sort {
        "title" => "s.sort_title ASC".to_string(),
        "year_desc" => "s.year IS NULL, s.year DESC, s.sort_title".to_string(),
        "year_asc" => "s.year IS NULL, s.year ASC, s.sort_title".to_string(),
        "recently_added" => "COALESCE(s.added_at, s.scanned_at) DESC, s.id DESC".to_string(),
        _ => "s.sort_title ASC".to_string(),
    };
    let rows = format!(
        "SELECT s.id, s.title, s.year, \
                (SELECT COUNT(*) FROM media m \
                   WHERE m.show_id = s.id AND m.deleted_at IS NULL) AS episode_count, \
                (s.banner_path IS NOT NULL) AS has_banner \
         FROM shows s {where_clause} ORDER BY {order_sql} LIMIT ? OFFSET ?"
    );
    let count = format!("SELECT COUNT(*) FROM shows s {where_clause}");
    (rows, count)
}

fn bind_show_search<'q, O>(
    mut q: sqlx::query::QueryAs<'q, sqlx::Sqlite, O, sqlx::sqlite::SqliteArguments<'q>>,
    like: Option<&'q str>,
    year_min: Option<i64>,
    year_max: Option<i64>,
    genres: &'q [String],
    watched: &str,
    user_sub: &'q str,
) -> sqlx::query::QueryAs<'q, sqlx::Sqlite, O, sqlx::sqlite::SqliteArguments<'q>>
where
    O: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> + Send + Unpin,
{
    if let Some(l) = like {
        q = q.bind(l).bind(l);
    }
    if let Some(y) = year_min {
        q = q.bind(y);
    }
    if let Some(y) = year_max {
        q = q.bind(y);
    }
    for g in genres {
        q = q.bind(g);
    }
    if !genres.is_empty() {
        q = q.bind(genres.len() as i64);
    }
    match watched {
        "watched" => q = q.bind(user_sub),
        "unwatched" => q = q.bind(user_sub),
        "in_progress" => q = q.bind(user_sub).bind(user_sub),
        _ => {}
    }
    q
}

async fn list_genres(State(state): State<AppState>) -> Result<Json<Vec<String>>> {
    // Only media_genres is populated today (scanner writes per-movie and
    // per-episode entries). show_genres is reserved for a future per-show
    // override; UNION with it so adding rows there doesn't need a code change.
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT genre FROM media_genres \
         UNION \
         SELECT genre FROM show_genres \
         ORDER BY 1",
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(rows.into_iter().map(|(g,)| g).collect()))
}
