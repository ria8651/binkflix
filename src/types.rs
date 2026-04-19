//! DTOs shared between client (WASM) and server. No side-specific imports here.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MovieSummary {
    pub id: String,
    pub title: String,
    pub year: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShowSummary {
    pub id: String,
    pub title: String,
    pub year: Option<i64>,
    pub episode_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Library {
    pub movies: Vec<MovieSummary>,
    pub shows: Vec<ShowSummary>,
}

/// A playable video — movie or episode. Episode-only fields are None for movies.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Media {
    pub id: String,
    pub kind: String,                // "movie" | "episode"
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EpisodeSummary {
    pub id: String,
    pub season_number: i64,
    pub episode_number: i64,
    pub title: String,
    pub plot: Option<String>,
    pub runtime_minutes: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Season {
    pub number: i64,
    pub episodes: Vec<EpisodeSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShowDetail {
    pub show: Show,
    pub seasons: Vec<Season>,
}

// URL builders — relative paths work through dx proxy and same-origin alike.
pub fn media_image_url(id: &str) -> String { format!("/api/media/{id}/image") }
pub fn media_fanart_url(id: &str) -> String { format!("/api/media/{id}/fanart") }
pub fn media_stream_url(id: &str) -> String { format!("/api/media/{id}/stream") }

pub fn show_poster_url(id: &str) -> String { format!("/api/shows/{id}/poster") }
pub fn show_fanart_url(id: &str) -> String { format!("/api/shows/{id}/fanart") }
pub fn season_poster_url(show_id: &str, season: i64) -> String {
    format!("/api/shows/{show_id}/seasons/{season}/poster")
}
