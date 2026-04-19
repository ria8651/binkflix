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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubtitleTrack {
    pub id: String,
    /// "ass" (render with JASSUB) or "vtt" (native <track>).
    pub format: String,
    pub language: String,
    pub label: String,
    pub default: bool,
    pub forced: bool,
}

// URL builders — relative paths work through dx proxy and same-origin alike.
pub fn media_image_url(id: &str) -> String { format!("/api/media/{id}/image") }
pub fn media_fanart_url(id: &str) -> String { format!("/api/media/{id}/fanart") }
pub fn media_stream_url(id: &str) -> String { format!("/api/media/{id}/stream") }
pub fn media_subtitles_url(id: &str) -> String { format!("/api/media/{id}/subtitles") }
pub fn media_subtitle_url(id: &str, track: &str) -> String {
    format!("/api/media/{id}/subtitle/{track}")
}

// ---- Syncplay (watch-party) ----

/// Server's authoritative snapshot of what a room is currently watching.
/// Sent to new joiners in `Welcome.current` and updated on every Play/Pause/Seek/SetMedia.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoomState {
    pub media_id: String,
    pub position_ms: i64,
    pub playing: bool,
    pub updated_at: i64,
}

/// Dropdown list item. `current_media_*` is None when the room exists but nobody
/// has started anything yet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoomListItem {
    pub id: String,
    pub viewers: usize,
    pub current_media_id: Option<String>,
    pub current_media_title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CreateRoomResp {
    pub id: String,
}

/// Messages a client sends to the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Client-authoritative intent: play at media-time `position_ms`.
    Play { position_ms: i64 },
    /// Pause at `position_ms`.
    Pause { position_ms: i64 },
    /// Seek to `position_ms` (stays in current play/pause state).
    Seek { position_ms: i64 },
    /// Switch the room to a new media. Fans out as SetMedia broadcast.
    SetMedia { media_id: String },
    /// Periodic heartbeat for latency estimation. Server echoes back.
    Ping { client_ts: i64 },
    /// Report current playback position for drift detection.
    Heartbeat { position_ms: i64, playing: bool },
}

/// Messages the server fans out to all clients in a room.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Broadcast {
    Welcome { client_id: String, server_ts: i64, current: Option<RoomState> },
    Peer { client_id: String, joined: bool, viewers: usize },
    Play { position_ms: i64, server_ts: i64, from: String },
    Pause { position_ms: i64, server_ts: i64, from: String },
    Seek { position_ms: i64, server_ts: i64, from: String },
    SetMedia { media_id: String, server_ts: i64, from: String },
    Pong { client_ts: i64, server_ts: i64 },
    Drift { client_id: String, position_ms: i64, playing: bool, server_ts: i64 },
}

pub fn show_poster_url(id: &str) -> String { format!("/api/shows/{id}/poster") }
pub fn show_fanart_url(id: &str) -> String { format!("/api/shows/{id}/fanart") }
pub fn season_poster_url(show_id: &str, season: i64) -> String {
    format!("/api/shows/{show_id}/seasons/{season}/poster")
}
