use super::error::{Error, Result};
use super::{media_info, subtitles, thumbnails};
use super::AppState;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
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
        .route("/api/media/{id}/fanart", get(media_fanart))
        .route("/api/shows/{id}", get(show))
        .route("/api/shows/{id}/poster", get(show_poster))
        .route("/api/shows/{id}/fanart", get(show_fanart))
        .route("/api/shows/{id}/seasons/{n}/poster", get(season_poster))
        .route("/api/rooms", get(list_rooms).post(create_room))
        .route("/api/scan", axum::routing::post(start_scan))
        .route("/api/scan/status", get(scan_status))
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
    for (meta, room_state, viewers) in rooms {
        let (current_media_id, current_media_title) = match room_state {
            Some(s) => {
                let title: Option<(String,)> = sqlx::query_as(
                    "SELECT title FROM media WHERE id = ?",
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
}

#[derive(Debug, Serialize)]
pub struct LibraryResponse {
    pub movies: Vec<MovieSummary>,
    pub shows: Vec<ShowSummary>,
}

async fn library(State(state): State<AppState>) -> Result<Json<LibraryResponse>> {
    let movies = sqlx::query_as::<_, MovieSummary>(
        "SELECT id, title, year FROM media WHERE kind = 'movie' ORDER BY title",
    )
    .fetch_all(&state.pool)
    .await?;

    let shows = sqlx::query_as::<_, ShowSummary>(
        "SELECT s.id, s.title, s.year,
                (SELECT COUNT(*) FROM media m WHERE m.show_id = s.id) AS episode_count
         FROM shows s
         ORDER BY s.title",
    )
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(LibraryResponse { movies, shows }))
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
         FROM media WHERE id = ?",
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
}

#[derive(Debug, Serialize, FromRow)]
pub struct EpisodeSummary {
    pub id: String,
    pub season_number: i64,
    pub episode_number: i64,
    pub title: String,
    pub plot: Option<String>,
    pub runtime_minutes: Option<i64>,
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

async fn show(State(state): State<AppState>, Path(id): Path<String>) -> Result<Json<ShowResponse>> {
    let show = sqlx::query_as::<_, Show>(
        "SELECT id, title, original_title, year, plot, imdb_id, tmdb_id, tvdb_id
         FROM shows WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(Error::NotFound)?;

    let eps = sqlx::query_as::<_, EpisodeSummary>(
        "SELECT id,
                COALESCE(season_number, 0)  AS season_number,
                COALESCE(episode_number, 0) AS episode_number,
                title, plot, runtime_minutes
         FROM media
         WHERE show_id = ? AND kind = 'episode'
         ORDER BY season_number, episode_number",
    )
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
    let path = lookup(&state, "SELECT path FROM media WHERE id = ?", &id).await?;
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
    let sidecar: Option<(Option<String>,)> =
        sqlx::query_as("SELECT image_path FROM media WHERE id = ?")
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

async fn media_fanart(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req: Request,
) -> Result<axum::response::Response> {
    let path = lookup(&state, "SELECT fanart_path FROM media WHERE id = ?", &id).await?;
    serve(path, req).await
}

async fn show_poster(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req: Request,
) -> Result<axum::response::Response> {
    let path = lookup(&state, "SELECT poster_path FROM shows WHERE id = ?", &id).await?;
    serve(path, req).await
}

async fn show_fanart(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req: Request,
) -> Result<axum::response::Response> {
    let path = lookup(&state, "SELECT fanart_path FROM shows WHERE id = ?", &id).await?;
    serve(path, req).await
}

/// Derived at request time: look for `seasonNN-poster.ext` in the show folder,
/// or `season-specials-poster.ext` for season 0.
async fn season_poster(
    State(state): State<AppState>,
    Path((id, n)): Path<(String, i64)>,
    req: Request,
) -> Result<axum::response::Response> {
    let show_path: (String,) = sqlx::query_as("SELECT path FROM shows WHERE id = ?")
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
