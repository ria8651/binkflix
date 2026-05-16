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
    #[serde(default)]
    pub has_banner: bool,
}

/// One newly-added playable item — an episode or a movie. Episodes carry
/// `show_id` + `show_title` so the home page card can show "Show — S1E2".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecentItem {
    pub media_id: String,
    /// "movie" | "episode"
    pub kind: String,
    pub title: String,
    pub show_id: Option<String>,
    pub show_title: Option<String>,
    pub season_number: Option<i64>,
    pub episode_number: Option<i64>,
    /// Release year for movies; used as the second-line subtitle so movie
    /// tiles in the Recently Added row don't sit empty under the title.
    pub year: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchResponse {
    pub movies: Vec<MovieSummary>,
    pub shows: Vec<ShowSummary>,
    pub total_movies: i64,
    pub total_shows: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Library {
    pub movies: Vec<MovieSummary>,
    pub shows: Vec<ShowSummary>,
    pub recently_added: Vec<RecentItem>,
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
    #[serde(default)]
    pub has_clearlogo: bool,
    #[serde(default)]
    pub has_fanart: bool,
    #[serde(default)]
    pub has_banner: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EpisodeSummary {
    pub id: String,
    pub season_number: i64,
    pub episode_number: i64,
    pub title: String,
    pub plot: Option<String>,
    pub runtime_minutes: Option<i64>,
    #[serde(default)]
    pub position_secs: f64,
    #[serde(default)]
    pub duration_secs: f64,
    #[serde(default)]
    pub completed: i64,
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

/// Video/audio technical metadata probed on demand with ffprobe.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MediaTechInfo {
    pub container: Option<String>,
    pub duration_seconds: Option<f64>,
    pub bitrate_kbps: Option<u64>,
    pub file_size: Option<u64>,
    pub video: Option<VideoTrackInfo>,
    pub audio: Vec<AudioTrackInfo>,
    /// What the browser can do with this file: play it as-is, remux into
    /// fragmented MP4, or full transcode.
    pub browser_compat: BrowserCompat,
    /// Human-readable explanation for why the verdict isn't `Direct`. `None`
    /// for direct files. Surfaced in the playback info panel so an operator
    /// can see *why* the server picked Remux/Transcode without re-deriving
    /// the rule.
    #[serde(default)]
    pub compat_reason: Option<String>,
}

/// How we expect to deliver this media to a modern browser.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserCompat {
    /// Source is already browser-friendly — serve the file directly.
    Direct,
    /// Container and/or audio codec needs repackaging, but video stream
    /// can be copied (cheap). Served via ffmpeg `-c:v copy` into fMP4.
    Remux,
    /// Video codec isn't browser-playable — needs real transcode. Not
    /// implemented yet; served endpoint returns 501.
    Transcode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VideoTrackInfo {
    pub codec: String,
    pub profile: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub bitrate_kbps: Option<u64>,
    pub pix_fmt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AudioTrackInfo {
    pub codec: String,
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
    pub sample_rate_hz: Option<u32>,
    pub bitrate_kbps: Option<u64>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
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

/// Debug-panel snapshot of the HLS pipeline for one media. Exposed by
/// `/api/media/{id}/hls/state` and rendered as a YouTube-style timeline
/// bar so an operator can see at a glance where ffmpeg is running, what
/// segments are cached on disk, and what the client has buffered.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HlsState {
    pub duration: f64,
    pub total_segments: u32,
    /// Each segment's duration in source-time order. Index in this array
    /// + 1 is the segment number used in URLs.
    pub segment_durations: Vec<f64>,
    /// Sorted segment indices currently on disk under the plan dir.
    pub cached_segments: Vec<u32>,
    pub producer: Option<HlsProducerState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HlsProducerState {
    pub start_idx: u32,
    /// Highest segment ffmpeg has finished writing to canonical.
    pub head: u32,
    /// Highest segment ffmpeg is *allowed* to produce in this pull.
    /// Only advances when a request arrives — pull-driven backpressure.
    pub target_head: u32,
    pub paused: bool,
    pub idle_for_secs: f64,
    /// How many segments past the most recently requested one ffmpeg
    /// is permitted to read ahead.
    pub lookahead_buffer: u32,
    /// Far-ahead-request relaunch threshold; not part of the
    /// backpressure loop, just exposed for the debug panel.
    pub lookahead_window: u32,
}

// URL builders — relative paths work through dx proxy and same-origin alike.
pub fn media_image_url(id: &str) -> String { format!("/api/media/{id}/image") }
pub fn media_stream_url(id: &str) -> String { format!("/api/media/{id}/stream") }
/// HLS playlist URL with explicit mode + optional bitrate. `mode` is
/// `"remux"` or `"transcode"`; bitrate (kbps) is honored for transcode
/// only. Empty mode falls back to the server's compat verdict.
pub fn media_hls_url(id: &str, audio_idx: u32, mode: &str, bitrate_kbps: Option<u32>) -> String {
    let mut url = format!("/api/media/{id}/hls/index.m3u8?a={audio_idx}");
    if !mode.is_empty() {
        url.push_str(&format!("&mode={mode}"));
    }
    if let Some(b) = bitrate_kbps {
        url.push_str(&format!("&bitrate={b}"));
    }
    url
}
pub fn media_subtitle_url(id: &str, track: &str) -> String {
    format!("/api/media/{id}/subtitle/{track}")
}

// ---- Syncplay (watch-party) ----

/// Server's authoritative snapshot of what a room is currently watching.
/// `version` is bumped on every mutation; clients use it to ignore stale or
/// already-applied broadcasts (no string compare on media_id, no clock games).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoomState {
    pub media_id: String,
    pub position_ms: i64,
    pub playing: bool,
    pub updated_at: i64,
    pub version: u64,
}

/// One participant in a room. `client_id` distinguishes multiple tabs by the
/// same user; `username` is the display name (Bastion `login`, or "dev" when
/// auth is off).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Member {
    pub client_id: String,
    pub user_sub: String,
    pub username: String,
}

/// Dropdown list item. `current_media_*` is None when the room exists but nobody
/// has started anything yet. `members` carries usernames for the rooms list UI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoomListItem {
    pub id: String,
    pub viewers: usize,
    pub current_media_id: Option<String>,
    pub current_media_title: Option<String>,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[allow(dead_code)]
pub struct CreateRoomResp {
    pub id: String,
}

/// One asset-extraction job currently in flight during phase 2. Phase 2 runs
/// up to `BINKFLIX_SCAN_CONCURRENCY` of these in parallel, and `current`
/// can only hold one filename, so we list them out separately for the UI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActiveJob {
    pub media_id: String,
    pub title: String,
    /// "probing" | "subtitles" | "thumbnail" | "trickplay" | "saving"
    pub stage: String,
}

/// Live status of a library scan. Polled by the UI to drive the rescan button
/// and progress bar. `total` is 0 until phase 1 finishes (we don't know the
/// asset-job count yet).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ScanProgress {
    pub running: bool,
    pub phase: String,
    pub done: usize,
    pub total: usize,
    pub current: Option<String>,
    pub message: Option<String>,
    /// Unix seconds when the last scan finished. None before the first run.
    pub last_finished_at: Option<i64>,
    /// Human summary of the last completed scan (e.g. "12 indexed · 4 skipped").
    pub last_summary: Option<String>,
    /// Elapsed time of the last completed scan.
    pub last_elapsed_ms: Option<u64>,
    /// Phase-2 jobs currently running, with their stage. Phase 1 leaves this
    /// empty and uses `current` instead.
    #[serde(default)]
    pub active: Vec<ActiveJob>,
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
///
/// `version` on Play/Pause/Seek/SetMedia/Resync mirrors `RoomState.version` after
/// the mutation — clients use it to gate idempotent application. `from` carries
/// the full `Member` so UIs can render "Alice paused" without a side lookup.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum Broadcast {
    Welcome {
        you: Member,
        server_ts: i64,
        current: Option<RoomState>,
        members: Vec<Member>,
    },
    /// Roster snapshot. Sent on every join/leave so clients always have a
    /// current member list (replaces the old per-event Peer message).
    Members {
        members: Vec<Member>,
        joined: Option<Member>,
        left: Option<Member>,
    },
    Play { position_ms: i64, server_ts: i64, from: Member, version: u64 },
    Pause { position_ms: i64, server_ts: i64, from: Member, version: u64 },
    Seek { position_ms: i64, server_ts: i64, from: Member, version: u64 },
    SetMedia { media_id: String, server_ts: i64, from: Member, version: u64 },
    /// Periodic snapshot for drift correction. `live_position_ms` is the
    /// server's projection at `server_ts` (state.position_ms + elapsed if
    /// playing). Clients reconcile only if their local position is far off.
    Resync { state: RoomState, live_position_ms: i64, server_ts: i64 },
    Pong { client_ts: i64, server_ts: i64 },
}

// ---- Watch progress / continue watching ----

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WatchProgress {
    pub media_id: String,
    pub position_secs: f64,
    pub duration_secs: f64,
    pub completed: bool,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[allow(dead_code)]
pub struct ProgressReport {
    pub position_secs: f64,
    pub duration_secs: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContinueItem {
    pub media_id: String,
    pub kind: String,
    /// For episodes, the episode's own title; for movies, the movie title.
    /// The client composes the second-line subtitle ("Show · S1E2" / year)
    /// from the structured fields below.
    pub title: String,
    /// `Some` for episodes — use the show poster, not the episode thumb.
    pub show_id: Option<String>,
    pub show_title: Option<String>,
    pub season_number: Option<i64>,
    pub episode_number: Option<i64>,
    pub year: Option<i64>,
    pub position_secs: f64,
    pub duration_secs: f64,
}

// ---- Per-user playback preferences (sticky audio/subtitle/quality picks) ----
//
// Keyed at the *show* level for episodes (so the choice carries across
// episodes of one series) and at the media level for movies. The key is
// computed client-side and sent as `scope` in the URL.
//
// All fields are optional: a `None` means "no preference, fall back to
// the player's auto behaviour". Track-identifying fields are stored
// alongside the index/id so the client can fall back to language matching
// when stream order differs between episodes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MediaPreferences {
    /// `Some("")` = explicit Off; `Some(id)` = pick by id; `None` = no pref.
    #[serde(default)]
    pub subtitle_id: Option<String>,
    #[serde(default)]
    pub subtitle_lang: Option<String>,
    #[serde(default)]
    pub audio_idx: Option<u32>,
    #[serde(default)]
    pub audio_lang: Option<String>,
    #[serde(default)]
    pub audio_codec: Option<String>,
    /// "direct" | "remux" | "transcode" | None (= auto)
    #[serde(default)]
    pub transcode_mode: Option<String>,
    #[serde(default)]
    pub bitrate_kbps: Option<u32>,
}

pub fn show_poster_url(id: &str) -> String { format!("/api/shows/{id}/poster") }
pub fn show_fanart_url(id: &str) -> String { format!("/api/shows/{id}/fanart") }
pub fn show_clearlogo_url(id: &str) -> String { format!("/api/shows/{id}/clearlogo") }
pub fn show_banner_url(id: &str) -> String { format!("/api/shows/{id}/banner") }
pub fn media_fanart_url(id: &str) -> String { format!("/api/media/{id}/fanart") }
pub fn season_poster_url(show_id: &str, season: i64) -> String {
    format!("/api/shows/{show_id}/seasons/{season}/poster")
}
