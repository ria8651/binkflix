use super::error::{Error, Result};
use super::AppState;
use axum::extract::{Path, Request, State};
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
        .route("/api/media/{id}/stream", get(media_stream))
        .route("/api/media/{id}/image", get(media_image))
        .route("/api/media/{id}/fanart", get(media_fanart))
        .route("/api/shows/{id}", get(show))
        .route("/api/shows/{id}/poster", get(show_poster))
        .route("/api/shows/{id}/fanart", get(show_fanart))
        .route("/api/shows/{id}/seasons/{n}/poster", get(season_poster))
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

async fn media_stream(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req: Request,
) -> Result<axum::response::Response> {
    let path = lookup(&state, "SELECT path FROM media WHERE id = ?", &id).await?;
    serve(path, req).await
}

async fn media_image(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req: Request,
) -> Result<axum::response::Response> {
    let path = lookup(&state, "SELECT image_path FROM media WHERE id = ?", &id).await?;
    serve(path, req).await
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
