//! Append-only analytics writes.
//!
//! Four tables (see `migrations/0009_analytics.sql`):
//!   * `scan_timings`       — per-asset-job stage timings
//!   * `playback_sessions`  — one row per playback (open/close)
//!   * `playback_samples`   — rolling client-side telemetry
//!   * `events`             — JSON catch-all (watch-party + future signals)
//!
//! All writes are best-effort: a failed analytics insert MUST NOT break a
//! user-facing flow. Callers either `let _ = ...await` or log via the
//! helpers' built-in `warn!` on error.

use serde_json::Value;
use sqlx::SqlitePool;

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---- scan_timings --------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ScanTiming {
    pub probe_ms: u64,
    pub subtitles_ms: u64,
    pub subtitle_tracks: u32,
    pub thumbnail_ms: u64,
    pub trickplay_ms: u64,
    pub save_ms: u64,
    pub total_ms: u64,
    // Source-side encoding info — populated from the pass-1 ffprobe
    // results so a slow `trickplay_ms` can be correlated with the file's
    // characteristics (codec, resolution, bitrate, GOP) without re-probing.
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub container: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_ms: Option<u64>,
    pub bitrate_kbps: Option<u64>,
    pub pixel_format: Option<String>,
    /// Number of keyframes ffmpeg saw while building the trickplay sprite.
    /// Combined with `duration_ms` this gives an effective average GOP.
    pub keyframe_count: Option<u32>,
}

pub async fn record_scan_timing(pool: &SqlitePool, media_id: &str, t: ScanTiming) {
    let res = sqlx::query(
        "INSERT INTO scan_timings
            (media_id, scanned_at, probe_ms, subtitles_ms, subtitle_tracks,
             thumbnail_ms, trickplay_ms, save_ms, total_ms,
             video_codec, audio_codec, container, width, height,
             duration_ms, bitrate_kbps, pixel_format, keyframe_count)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(media_id)
    .bind(now_secs())
    .bind(t.probe_ms as i64)
    .bind(t.subtitles_ms as i64)
    .bind(t.subtitle_tracks as i64)
    .bind(t.thumbnail_ms as i64)
    .bind(t.trickplay_ms as i64)
    .bind(t.save_ms as i64)
    .bind(t.total_ms as i64)
    .bind(t.video_codec)
    .bind(t.audio_codec)
    .bind(t.container)
    .bind(t.width.map(|n| n as i64))
    .bind(t.height.map(|n| n as i64))
    .bind(t.duration_ms.map(|n| n as i64))
    .bind(t.bitrate_kbps.map(|n| n as i64))
    .bind(t.pixel_format)
    .bind(t.keyframe_count.map(|n| n as i64))
    .execute(pool)
    .await;
    if let Err(e) = res {
        tracing::warn!(%media_id, %e, "failed to record scan_timings row");
    }
}

// ---- playback_sessions ---------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct PlaybackSessionStart<'a> {
    pub id: &'a str,
    pub user_sub: Option<&'a str>,
    pub media_id: &'a str,
    pub delivery_mode: &'a str,
    pub chosen_reason: Option<&'a str>,
    pub src_video_codec: Option<&'a str>,
    pub src_audio_codec: Option<&'a str>,
    pub src_container: Option<&'a str>,
    pub out_video_codec: Option<&'a str>,
    pub out_audio_codec: Option<&'a str>,
    pub out_container: Option<&'a str>,
    pub target_bitrate_kbps: Option<u32>,
    pub browser: Option<&'a str>,
    pub room_id: Option<&'a str>,
    pub forced_via_query: bool,
}

pub async fn open_playback_session(pool: &SqlitePool, s: PlaybackSessionStart<'_>) {
    let res = sqlx::query(
        "INSERT INTO playback_sessions
            (id, user_sub, media_id, started_at, delivery_mode, chosen_reason,
             src_video_codec, src_audio_codec, src_container,
             out_video_codec, out_audio_codec, out_container,
             target_bitrate_kbps, browser, room_id, forced_via_query)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(s.id)
    .bind(s.user_sub)
    .bind(s.media_id)
    .bind(now_secs())
    .bind(s.delivery_mode)
    .bind(s.chosen_reason)
    .bind(s.src_video_codec)
    .bind(s.src_audio_codec)
    .bind(s.src_container)
    .bind(s.out_video_codec)
    .bind(s.out_audio_codec)
    .bind(s.out_container)
    .bind(s.target_bitrate_kbps.map(|n| n as i64))
    .bind(s.browser)
    .bind(s.room_id)
    .bind(s.forced_via_query as i64)
    .execute(pool)
    .await;
    if let Err(e) = res {
        tracing::warn!(session_id = %s.id, %e, "failed to open playback_sessions row");
    }
}

/// Fill in the output codec/container/bitrate columns once the per-mode
/// path has decided them. Used by the remux pipeline; transcode/direct can
/// call this too if they want their `out_*` columns populated.
pub async fn set_playback_outputs(
    pool: &SqlitePool,
    session_id: &str,
    out_video_codec: Option<&str>,
    out_audio_codec: Option<&str>,
    out_container: Option<&str>,
    target_bitrate_kbps: Option<u32>,
) {
    let res = sqlx::query(
        "UPDATE playback_sessions
         SET out_video_codec = ?, out_audio_codec = ?, out_container = ?,
             target_bitrate_kbps = ?
         WHERE id = ?",
    )
    .bind(out_video_codec)
    .bind(out_audio_codec)
    .bind(out_container)
    .bind(target_bitrate_kbps.map(|n| n as i64))
    .bind(session_id)
    .execute(pool)
    .await;
    if let Err(e) = res {
        tracing::warn!(%session_id, %e, "failed to set playback outputs");
    }
}

pub async fn close_playback_session(
    pool: &SqlitePool,
    session_id: &str,
    duration_played_ms: Option<u64>,
) {
    let res = sqlx::query(
        "UPDATE playback_sessions
         SET ended_at = ?, duration_played_ms = COALESCE(?, duration_played_ms)
         WHERE id = ? AND ended_at IS NULL",
    )
    .bind(now_secs())
    .bind(duration_played_ms.map(|n| n as i64))
    .bind(session_id)
    .execute(pool)
    .await;
    if let Err(e) = res {
        tracing::warn!(%session_id, %e, "failed to close playback_sessions row");
    }
}

// ---- playback_samples ----------------------------------------------------

#[derive(Debug, Clone)]
pub struct PlaybackSample<'a> {
    pub session_id: &'a str,
    pub position_ms: i64,
    pub buffered_ahead_ms: Option<i64>,
    pub transcode_position_ms: Option<i64>,
    pub transcode_rate_x100: Option<i64>,
    pub observed_kbps: Option<i64>,
    pub network_state: Option<&'a str>,
}

pub async fn record_playback_sample(pool: &SqlitePool, s: PlaybackSample<'_>) {
    let res = sqlx::query(
        "INSERT INTO playback_samples
            (session_id, ts, position_ms, buffered_ahead_ms,
             transcode_position_ms, transcode_rate_x100,
             observed_kbps, network_state)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(s.session_id)
    .bind(now_ms())
    .bind(s.position_ms)
    .bind(s.buffered_ahead_ms)
    .bind(s.transcode_position_ms)
    .bind(s.transcode_rate_x100)
    .bind(s.observed_kbps)
    .bind(s.network_state)
    .execute(pool)
    .await;
    if let Err(e) = res {
        tracing::warn!(session_id = %s.session_id, %e, "failed to record playback_samples row");
    }
}

// ---- events --------------------------------------------------------------

pub async fn record_event(
    pool: &SqlitePool,
    kind: &str,
    user_sub: Option<&str>,
    media_id: Option<&str>,
    room_id: Option<&str>,
    data: &Value,
) {
    let data_str = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string());
    let res = sqlx::query(
        "INSERT INTO events (ts, kind, user_sub, media_id, room_id, data)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(now_secs())
    .bind(kind)
    .bind(user_sub)
    .bind(media_id)
    .bind(room_id)
    .bind(&data_str)
    .execute(pool)
    .await;
    if let Err(e) = res {
        tracing::warn!(kind, %e, "failed to record events row");
    }
}

// ---- User-Agent → "Browser X / OS" ---------------------------------------

/// Parse a User-Agent header into something readable like
/// `"Chrome 120 / macOS"`. Hand-rolled rather than pulling another crate;
/// covers the browsers we actually see, falls back to `Other`.
pub fn parse_ua(ua: &str) -> String {
    // iOS first: iPhone/iPad UAs include "like Mac OS X", so the macOS
    // check would otherwise win. Same for Android, which includes "Linux".
    let os = if ua.contains("iPhone") || ua.contains("iPad") || ua.contains("iPod") {
        "iOS"
    } else if ua.contains("Android") {
        "Android"
    } else if ua.contains("Mac OS X") || ua.contains("Macintosh") {
        "macOS"
    } else if ua.contains("Windows") {
        "Windows"
    } else if ua.contains("Linux") {
        "Linux"
    } else {
        "Other"
    };

    // Order matters: Edge/OPR/Chromium-derivatives advertise Chrome too,
    // so check the more specific tokens first.
    let browser = if let Some(v) = capture_after(ua, "Edg/") {
        format!("Edge {v}")
    } else if let Some(v) = capture_after(ua, "OPR/") {
        format!("Opera {v}")
    } else if let Some(v) = capture_after(ua, "Firefox/") {
        format!("Firefox {v}")
    } else if let Some(v) = capture_after(ua, "Chrome/") {
        format!("Chrome {v}")
    } else if ua.contains("Safari/") && ua.contains("Version/") {
        let v = capture_after(ua, "Version/").unwrap_or_else(|| "?".into());
        format!("Safari {v}")
    } else {
        "Other".into()
    };

    format!("{browser} / {os}")
}

fn capture_after(s: &str, marker: &str) -> Option<String> {
    let i = s.find(marker)? + marker.len();
    let rest = &s[i..];
    // Token = digits and dots until first non-numeric byte.
    let end = rest
        .bytes()
        .position(|b| !(b.is_ascii_digit() || b == b'.'))
        .unwrap_or(rest.len());
    if end == 0 { None } else { Some(rest[..end].trim_end_matches('.').to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ua_chrome_macos() {
        let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
        assert_eq!(parse_ua(ua), "Chrome 120.0.0.0 / macOS");
    }

    #[test]
    fn parse_ua_firefox_linux() {
        let ua = "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0";
        assert_eq!(parse_ua(ua), "Firefox 121.0 / Linux");
    }

    #[test]
    fn parse_ua_safari_ios() {
        let ua = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) \
                  AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";
        assert_eq!(parse_ua(ua), "Safari 17.0 / iOS");
    }

    #[test]
    fn parse_ua_edge_wins_over_chrome() {
        let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Edg/120.0.0.0";
        assert_eq!(parse_ua(ua), "Edge 120.0.0.0 / Windows");
    }
}
