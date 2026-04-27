//! On-disk cache layout for HLS artifacts.
//!
//! Layout:
//! ```
//! ./data/hls/{media_id}/{plan_dir}/
//!     init.mp4
//!     seg-00001.m4s
//!     seg-00002.m4s
//!     ...
//! ```
//! `plan_dir` encodes the plan's invalidation keys (algorithm version, source
//! mtime, source size). When any of those change, a different `plan_dir` is
//! used so the new run never overlaps with stale artifacts. A startup sweep
//! deletes any sibling directories whose name doesn't match the current plan.

use std::path::{Path, PathBuf};

/// Uuid-style or word-safe ids only. The id is joined into filesystem paths,
/// so anything that escapes (`..`, slashes, absolute paths) would let callers
/// point ffmpeg/remove_dir_all at arbitrary places.
pub fn id_is_safe(id: &str) -> bool {
    !id.is_empty()
        && id.len() < 128
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Root of the HLS cache. One subdirectory per media id, then one per plan
/// invalidation key (mtime/size/version).
pub fn cache_root() -> PathBuf {
    std::env::var("BINKFLIX_HLS_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./data/hls"))
}

pub fn media_dir(id: &str) -> PathBuf {
    cache_root().join(id)
}

/// Subdirectory within `media_dir` whose name encodes the plan's invalidation
/// keys. Changes if the source file changes (mtime/size), the plan algorithm
/// is bumped (`PLAN_VERSION`), or the user picks a different audio track.
/// Audio index is part of the key because each track's segments contain a
/// different muxed audio stream — the video timeline is identical, but the
/// fragment bytes differ, so they need separate cache entries.
pub fn plan_dir_name(
    plan_version: u32,
    source_mtime: i64,
    source_size: i64,
    audio_idx: u32,
) -> String {
    format!("v{plan_version}-m{source_mtime}-s{source_size}-a{audio_idx}")
}

pub fn plan_dir(
    id: &str,
    plan_version: u32,
    source_mtime: i64,
    source_size: i64,
    audio_idx: u32,
) -> PathBuf {
    media_dir(id).join(plan_dir_name(plan_version, source_mtime, source_size, audio_idx))
}

/// Remove every subdirectory of `media_dir(id)` whose name doesn't share
/// the given `keep_prefix`. Per-audio-index variants of the *current*
/// (version,mtime,size) are kept so switching tracks doesn't repeatedly
/// blow away each other's caches; only genuinely stale dirs (old plan
/// version, outdated source mtime/size) are removed. Best-effort — IO
/// errors are logged and swallowed because a stale dir is harmless
/// beyond wasted disk.
pub async fn sweep_stale_plan_dirs(id: &str, keep_prefix: &str) {
    let dir = media_dir(id);
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(_) => return,
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else { continue };
        if name_str.starts_with(keep_prefix) {
            continue;
        }
        let p = entry.path();
        if let Err(e) = tokio::fs::remove_dir_all(&p).await {
            tracing::warn!(path = %p.display(), error = %e, "failed to remove stale hls dir");
        }
    }
}

/// Prefix used by `sweep_stale_plan_dirs` to keep all per-audio-index
/// variants of the current (version, mtime, size) tuple.
pub fn plan_dir_prefix(plan_version: u32, source_mtime: i64, source_size: i64) -> String {
    format!("v{plan_version}-m{source_mtime}-s{source_size}-")
}

/// Filenames the HTTP layer is allowed to serve out of a plan directory.
/// Anything else is rejected with 400 to avoid path-traversal via the catch-all
/// `{file}` route parameter.
pub fn is_allowed_name(name: &str) -> bool {
    if name == "index.m3u8" || name == "init.mp4" {
        return true;
    }
    if let Some(rest) = name.strip_prefix("seg-") {
        if let Some(num) = rest.strip_suffix(".m4s") {
            return !num.is_empty() && num.chars().all(|c| c.is_ascii_digit());
        }
    }
    false
}

/// Parse `seg-NNNNN.m4s` → segment index (1-based as ffmpeg numbers them).
/// Returns `None` for non-segment filenames.
pub fn segment_index(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("seg-")?;
    let num = rest.strip_suffix(".m4s")?;
    num.parse().ok()
}

pub fn segment_filename(idx: u32) -> String {
    format!("seg-{idx:05}.m4s")
}

pub fn mime_for(name: &str) -> &'static str {
    if name.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else if name.ends_with(".m4s") || name.ends_with(".mp4") {
        "video/mp4"
    } else {
        "application/octet-stream"
    }
}

/// stat the source file for invalidation. Returns (mtime_secs, size_bytes).
pub async fn stat_source(src: &Path) -> std::io::Result<(i64, i64)> {
    let meta = tokio::fs::metadata(src).await?;
    let mtime = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let size = meta.len() as i64;
    Ok((mtime, size))
}
