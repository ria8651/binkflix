//! On-demand video/audio technical metadata via `ffprobe`.
//!
//! Unlike subtitles (extracted once at scan time and cached in SQLite),
//! tech info is only relevant when someone opens the debug menu, so we
//! probe at request time and don't persist. Results are small — a single
//! ffprobe call takes well under a second on local storage.

use crate::types::{AudioTrackInfo, BrowserCompat, MediaTechInfo, VideoTrackInfo};
use serde::Deserialize;
use sqlx::SqlitePool;
use std::path::Path;
use tokio::process::Command;

/// Read the cached probe for `media_id`, if the scanner has populated it.
pub async fn load(pool: &SqlitePool, media_id: &str) -> anyhow::Result<Option<MediaTechInfo>> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT tech_json FROM media WHERE id = ?")
            .bind(media_id)
            .fetch_optional(pool)
            .await?;
    match row.and_then(|(s,)| s) {
        None => Ok(None),
        // Corrupt/incompatible cached JSON (e.g. after a struct change) —
        // treat as absent so the caller falls back to a live probe rather
        // than 500ing.
        Some(s) => Ok(serde_json::from_str(&s).ok()),
    }
}

/// Persist probe results on the `media` row. Best-effort: errors are logged
/// by the caller rather than propagated, since a missing cache is recoverable.
pub async fn store(pool: &SqlitePool, media_id: &str, info: &MediaTechInfo) -> anyhow::Result<()> {
    let json = serde_json::to_string(info)?;
    sqlx::query("UPDATE media SET tech_json = ? WHERE id = ?")
        .bind(json)
        .bind(media_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Raw subtitle stream entry as reported by ffprobe. Filtering / target-format
/// decisions happen in [`crate::server::subtitles`]; this struct just carries
/// what's needed to make those decisions without re-probing.
pub struct EmbeddedSubtitleStream {
    pub index: u32,
    pub codec: String,
    pub tags: std::collections::BTreeMap<String, String>,
    pub disposition: std::collections::BTreeMap<String, i64>,
}

pub async fn probe(video: &Path) -> anyhow::Result<MediaTechInfo> {
    Ok(probe_full(video).await?.0)
}

/// Like [`probe`] but also returns subtitle stream metadata, so the scanner
/// can avoid a second ffprobe just to enumerate subtitle tracks.
pub async fn probe_full(
    video: &Path,
) -> anyhow::Result<(MediaTechInfo, Vec<EmbeddedSubtitleStream>)> {
    // `-protocol_whitelist file`: harden in case `video` is ever a DB-sourced
    // path that's a URL ("http://…", "concat:…", "subfile:…"). Without it,
    // ffprobe would happily open network or composite inputs derived from
    // attacker-controlled rows.
    let output = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-protocol_whitelist", "file",
            "-print_format", "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(video)
        .output()
        .await?;

    if !output.status.success() {
        anyhow::bail!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let parsed: ProbeOutput = serde_json::from_slice(&output.stdout)?;

    let mut video_track: Option<VideoTrackInfo> = None;
    let mut audio_tracks: Vec<AudioTrackInfo> = Vec::new();
    let mut subtitle_streams: Vec<EmbeddedSubtitleStream> = Vec::new();

    for s in &parsed.streams {
        match s.codec_type.as_str() {
            "video" => {
                // Skip cover art / attached pics, which ffprobe also reports
                // as "video" streams. Rare on TV rips but common on music.
                if s.disposition.get("attached_pic").copied().unwrap_or(0) != 0 {
                    continue;
                }
                if video_track.is_none() {
                    video_track = Some(VideoTrackInfo {
                        codec: s.codec_name.clone(),
                        profile: s.profile.clone().filter(|s| !s.is_empty()),
                        width: s.width,
                        height: s.height,
                        fps: parse_fps(s.r_frame_rate.as_deref()),
                        bitrate_kbps: parse_bitrate_kbps(s.bit_rate.as_deref()),
                        pix_fmt: s.pix_fmt.clone().filter(|s| !s.is_empty()),
                    });
                }
            }
            "audio" => {
                audio_tracks.push(AudioTrackInfo {
                    codec: s.codec_name.clone(),
                    channels: s.channels,
                    channel_layout: s.channel_layout.clone().filter(|s| !s.is_empty()),
                    sample_rate_hz: s.sample_rate.as_deref().and_then(|s| s.parse().ok()),
                    bitrate_kbps: parse_bitrate_kbps(s.bit_rate.as_deref()),
                    language: s.tags.get("language").cloned().filter(|s| !s.is_empty()),
                    title: s.tags.get("title").cloned().filter(|s| !s.is_empty()),
                    default: s.disposition.get("default").copied().unwrap_or(0) != 0,
                });
            }
            "subtitle" => {
                subtitle_streams.push(EmbeddedSubtitleStream {
                    index: s.index,
                    codec: s.codec_name.clone(),
                    tags: s.tags.clone(),
                    disposition: s.disposition.clone(),
                });
            }
            _ => {}
        }
    }

    let container = parsed.format.format_name.clone().filter(|s| !s.is_empty());
    let (browser_compat, compat_reason) =
        compute_compat(video_track.as_ref(), &audio_tracks, container.as_deref());

    let info = MediaTechInfo {
        container,
        duration_seconds: parsed.format.duration.as_deref().and_then(|s| s.parse().ok()),
        bitrate_kbps: parse_bitrate_kbps(parsed.format.bit_rate.as_deref()),
        file_size: parsed.format.size.as_deref().and_then(|s| s.parse().ok()),
        video: video_track,
        audio: audio_tracks,
        browser_compat,
        compat_reason,
    };
    Ok((info, subtitle_streams))
}

/// Decide whether a file can be served as-is, remuxed cheaply, or needs
/// a full transcode. The remux pipeline picks its output container
/// (fMP4 vs WebM) based on the input video codec, so we accept both
/// H.264 (→ MP4 path) and VP9/AV1 (→ WebM path) as copyable.
///
/// - `Direct`: container + codecs already match what a browser plays
///             natively over HTTP — MP4/MOV with H.264+AAC/MP3, or
///             WebM with VP9/VP8/AV1+Opus/Vorbis.
/// - `Remux`:  video codec is copyable but container and/or audio need
///             repackaging. Audio gets copied where browsers accept it
///             in the chosen output container, else transcoded to a
///             container-native codec (AAC in MP4, Opus in WebM).
/// - `Transcode`: video codec isn't browser-playable (HEVC here for
///                cross-browser safety, MPEG-2, VC-1, …). Not yet wired
///                up — the stream endpoint falls back to attempting
///                remux as a last-ditch effort.
fn compute_compat(
    video: Option<&VideoTrackInfo>,
    audio: &[AudioTrackInfo],
    container: Option<&str>,
) -> (BrowserCompat, Option<String>) {
    let Some(v) = video else { return (BrowserCompat::Direct, None) };

    let video_in_mp4 = matches!(v.codec.as_str(), "h264");
    let video_in_webm = matches!(v.codec.as_str(), "vp9" | "vp8" | "av1");
    if !video_in_mp4 && !video_in_webm {
        return (
            BrowserCompat::Transcode,
            Some(format!(
                "video codec {} isn't supported by browsers — needs full transcode",
                v.codec
            )),
        );
    }

    // Pick the default audio track if present, else the first. Zero-audio
    // files can still be direct/remuxed.
    let a = audio.iter().find(|a| a.default).or_else(|| audio.first());

    let container_formats: Vec<&str> = container
        .map(|c| c.split(',').map(str::trim).collect())
        .unwrap_or_default();
    let container_is_mp4 = container_formats
        .iter()
        .any(|p| matches!(*p, "mp4" | "mov" | "m4a" | "m4v"));
    let container_is_webm = container_formats.iter().any(|p| *p == "webm");

    if video_in_mp4 && container_is_mp4
        && a.map_or(true, |a| matches!(a.codec.as_str(), "aac" | "mp3"))
    {
        return (BrowserCompat::Direct, None);
    }
    if video_in_webm && container_is_webm
        && a.map_or(true, |a| matches!(a.codec.as_str(), "opus" | "vorbis"))
    {
        return (BrowserCompat::Direct, None);
    }

    // Remux verdict — pick the most informative reason. Container mismatch
    // dominates audio mismatch since fixing the container is what forces
    // ffmpeg into the loop in the first place.
    let target_family = if video_in_mp4 { "MP4" } else { "WebM" };
    let target_audio = if video_in_mp4 { "AAC/MP3" } else { "Opus/Vorbis" };
    let container_label = container.unwrap_or("unknown");
    let reason = if video_in_mp4 && !container_is_mp4 {
        format!("container {container_label} needs repackaging to {target_family}")
    } else if video_in_webm && !container_is_webm {
        format!("container {container_label} needs repackaging to {target_family}")
    } else if let Some(a) = a {
        format!(
            "audio codec {} isn't supported in {target_family} — needs {target_audio}",
            a.codec
        )
    } else {
        format!("source needs repackaging to {target_family}")
    };
    (BrowserCompat::Remux, Some(reason))
}

fn parse_bitrate_kbps(s: Option<&str>) -> Option<u64> {
    s.and_then(|s| s.parse::<u64>().ok()).map(|bps| bps / 1000)
}

fn parse_fps(s: Option<&str>) -> Option<f64> {
    // ffprobe reports `num/den`, e.g. `24000/1001`.
    let s = s?;
    let (num, den) = s.split_once('/')?;
    let n: f64 = num.parse().ok()?;
    let d: f64 = den.parse().ok()?;
    if d == 0.0 { return None; }
    Some(n / d)
}

#[derive(Debug, Deserialize)]
struct ProbeOutput {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    #[serde(default)]
    format: ProbeFormat,
}

#[derive(Debug, Default, Deserialize)]
struct ProbeFormat {
    format_name: Option<String>,
    duration: Option<String>,
    size: Option<String>,
    bit_rate: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProbeStream {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    codec_type: String,
    #[serde(default)]
    codec_name: String,
    profile: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    r_frame_rate: Option<String>,
    pix_fmt: Option<String>,
    channels: Option<u32>,
    channel_layout: Option<String>,
    sample_rate: Option<String>,
    bit_rate: Option<String>,
    #[serde(default)]
    tags: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    disposition: std::collections::BTreeMap<String, i64>,
}
