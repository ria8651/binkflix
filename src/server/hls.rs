//! HLS VOD playback.
//!
//! Unifies the old `/stream` (direct + remux) path behind a single pipeline
//! that gives the browser real random access: ffmpeg segments each source
//! into keyframe-aligned fMP4 chunks on disk, then we serve the playlist +
//! segments as plain static files.
//!
//! Why HLS and not byte-range served whole files:
//!   * The in-memory remux path (fMP4 over a non-seekable pipe) allowed the
//!     scrubber to look right after the moov patch, but any real seek past
//!     the buffered region caused the browser decoder to get fed junk
//!     (samples without their moof header → VideoToolbox `BadDataErr`).
//!   * tower-http's `ServeFile` byte-range worked for most MP4s but was
//!     flaky for some MKV containers (moov-at-end, VFR streams, etc.),
//!     which is what the user was seeing in "direct mode".
//!
//! First-play latency: ffmpeg runs in the background; we return an
//! EVENT-type playlist right away, so hls.js can start playback as soon
//! as the first segment hits disk. The playlist grows on each refresh
//! until ffmpeg writes `#EXT-X-ENDLIST`; from that point on the file is
//! pure VOD and every subsequent play of the same media is instant
//! static-file serving.
//!
//! Single-flight: a DashMap of `Arc<Notify>` prevents two concurrent
//! requests for the same id from spawning two ffmpegs.

use super::error::{Error, Result};
use super::AppState;
use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use dashmap::DashMap;
use std::path::{Path as StdPath, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

/// Uuid (with dashes) and "word"-safe ids only. `cache_dir_for` joins `id` into
/// the cache root, so anything that escapes (`..`, slashes, absolute paths)
/// would let callers point ffmpeg/remove_dir_all at arbitrary places.
fn id_is_safe(id: &str) -> bool {
    !id.is_empty()
        && id.len() < 128
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Root of the HLS cache on disk. Each media id gets its own subdir.
fn cache_root() -> PathBuf {
    std::env::var("BINKFLIX_HLS_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./data/hls"))
}

fn cache_dir_for(id: &str) -> PathBuf {
    cache_root().join(id)
}

#[derive(Default)]
pub struct HlsCache {
    /// Per-id notify: waiters block on this, the generator fires
    /// `notify_waiters()` once playlist generation has *started* (first
    /// segment is on disk) so the initial playlist request doesn't 404.
    pending: DashMap<String, Arc<PendingState>>,
}

struct PendingState {
    /// Latched "the first segment (or a terminal error) is ready". Waiters
    /// check this before arming the Notify below so a Notify firing in
    /// between the map lookup and the await can't leave them stuck. A
    /// plain `Notify::notify_waiters` only wakes tasks that are currently
    /// parked — the atomic bridges the gap.
    ready: AtomicBool,
    /// Fires when `ready` flips to true OR when ffmpeg finishes (either
    /// outcome — error or success — unblocks waiters).
    notify: Notify,
    /// Set to true once ffmpeg exited; watcher uses this to stop polling.
    ffmpeg_done: AtomicBool,
    /// Error message if ffmpeg failed. Checked by every waiter after
    /// `ready` flips — without this, the second request into a failed
    /// generation would return a cache dir that has no playlist and the
    /// subsequent file read would surface as a misleading 404.
    error: tokio::sync::Mutex<Option<String>>,
}

impl PendingState {
    fn new() -> Self {
        Self {
            ready: AtomicBool::new(false),
            notify: Notify::new(),
            ffmpeg_done: AtomicBool::new(false),
            error: tokio::sync::Mutex::new(None),
        }
    }

    fn mark_ready(&self) {
        self.ready.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    async fn wait_ready(&self) -> std::result::Result<(), String> {
        if !self.ready.load(Ordering::Acquire) {
            // `Notified::enable` synchronously registers interest so a
            // `notify_waiters` that fires between this line and `.await`
            // still wakes us. Without it the check-then-park pattern has
            // a hole.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.ready.load(Ordering::Acquire) {
                notified.await;
            }
        }
        if let Some(msg) = self.error.lock().await.clone() {
            return Err(msg);
        }
        Ok(())
    }
}

impl HlsCache {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

/// Ensure an ffmpeg job is running (or has finished) for this media. Returns
/// the cache dir once there's at least a playlist + first segment on disk.
/// Concurrent callers all block on the same Notify and share the one job.
async fn ensure_started(state: &AppState, id: &str) -> Result<PathBuf> {
    // Validate *before* doing anything with the id on the filesystem — the
    // two disk operations below (`remove_dir_all` the cache dir, later
    // `cache_dir_for` for ffmpeg output) would otherwise honour any `..`
    // segments a caller managed to sneak in.
    if !id_is_safe(id) {
        return Err(Error::BadRequest("invalid media id".into()));
    }

    let cache = cache_dir_for(id);
    let done = cache.join(".done");
    if done.exists() {
        return Ok(cache);
    }

    // If someone else is already generating, wait on the shared state and
    // surface any error they hit. Without the error check, a second
    // request into a failed generation would silently return the empty
    // cache dir.
    if let Some(existing) = state.hls_cache.pending.get(id).map(|e| e.clone()) {
        existing
            .wait_ready()
            .await
            .map_err(|msg| Error::Other(anyhow::anyhow!(msg)))?;
        return Ok(cache);
    }

    // We're the leader. Resolve the source path and probe *before* we
    // touch the cache dir — wiping and re-creating an unknown id's cache
    // on every NotFound would be silly.
    let row: (String,) = sqlx::query_as("SELECT path FROM media WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(Error::NotFound)?;
    let src = row.0;

    // Register the PendingState before spawning so any racer that checks
    // the map in between sees us.
    let pending = Arc::new(PendingState::new());
    state
        .hls_cache
        .pending
        .insert(id.to_string(), pending.clone());

    // Wipe any half-written state from an earlier aborted run. If ffmpeg
    // left old segments around the playlist would reference stale bytes.
    let _ = tokio::fs::remove_dir_all(&cache).await;
    if let Err(e) = tokio::fs::create_dir_all(&cache).await {
        // Don't leave a zombie PendingState if we can't even create the
        // cache — later callers would wait on a `ready` that never flips.
        state.hls_cache.pending.remove(id);
        return Err(Error::Other(anyhow::anyhow!("create hls cache: {e}")));
    }

    // Decide audio handling from the cached probe. No probe = transcode
    // to AAC, which always works. With a probe, copy AAC through and
    // re-encode everything else.
    let info = super::media_info::load(&state.pool, id).await.ok().flatten();
    let src_audio_codec = info
        .as_ref()
        .and_then(|i| i.audio.iter().find(|a| a.default).or_else(|| i.audio.first()))
        .map(|a| a.codec.clone());

    // Video handling: copy when the codec is already a valid HLS/fMP4
    // payload. Everything else would need a real transcode, which isn't
    // implemented yet — surface 501 up-front so users get a clear error.
    let video_codec = info
        .as_ref()
        .and_then(|i| i.video.as_ref())
        .map(|v| v.codec.clone());
    if matches!(video_codec.as_deref(), Some("vp9" | "vp8" | "av1")) {
        // These codecs don't fit in fMP4 — they need a WebM/DASH
        // pipeline instead, which isn't implemented. Fail fast so the
        // caller falls back to the byte-range /stream path.
        state.hls_cache.pending.remove(id);
        return Err(Error::NotImplemented(format!(
            "{} isn't supported in HLS (needs WebM/DASH)",
            video_codec.as_deref().unwrap_or("?")
        )));
    }

    // Kick off ffmpeg in the background. The pending notifier is shared
    // with the watcher and the ffmpeg task.
    let watcher_pending = pending.clone();
    let watcher_cache = cache.clone();
    tokio::spawn(async move {
        // Poll for the first segment on disk. We stop as soon as the
        // ffmpeg task flips `ffmpeg_done` (it's already marked ready for
        // us in that case, so nothing left to watch for) — without that
        // check this task would keep stat-ing a wiped cache dir for up
        // to the full ceiling after an early ffmpeg failure.
        for _ in 0..6000 {
            if watcher_pending.ffmpeg_done.load(Ordering::Acquire) {
                return;
            }
            if let Ok(mut rd) = tokio::fs::read_dir(&watcher_cache).await {
                while let Ok(Some(entry)) = rd.next_entry().await {
                    let name = entry.file_name();
                    if let Some(s) = name.to_str() {
                        if s.starts_with("seg-") && s.ends_with(".m4s") {
                            watcher_pending.mark_ready();
                            return;
                        }
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    });

    let ffmpeg_pending = pending.clone();
    let ffmpeg_cache = cache.clone();
    let id_for_task = id.to_string();
    let hls_cache = state.hls_cache.clone();
    tokio::spawn(async move {
        let result =
            run_ffmpeg(&src, &ffmpeg_cache, src_audio_codec.as_deref()).await;
        match &result {
            Ok(()) => {
                // Success: write the sentinel so future requests skip
                // straight to static serving.
                let _ = tokio::fs::write(ffmpeg_cache.join(".done"), "").await;
                tracing::info!(%id_for_task, "hls generation complete");
            }
            Err(e) => {
                tracing::warn!(%id_for_task, error = %e, "hls generation failed");
                let mut err = ffmpeg_pending.error.lock().await;
                *err = Some(e.to_string());
            }
        }
        // Tell the watcher it can stop polling and release waiters.
        // `mark_ready` is idempotent — the watcher may already have
        // fired it once the first segment landed.
        ffmpeg_pending.ffmpeg_done.store(true, Ordering::Release);
        ffmpeg_pending.mark_ready();
        // Leave the entry in the pending map for a moment so late joiners
        // still see the Arc and can read any error — then remove it.
        // Without the linger, a waiter that lost the race to our Arc
        // clone above would fall through and spawn a second ffmpeg for
        // the same id, wiping our output from under us.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        hls_cache.pending.remove(&id_for_task);
    });

    pending
        .wait_ready()
        .await
        .map_err(|msg| Error::Other(anyhow::anyhow!(msg)))?;
    Ok(cache)
}

async fn run_ffmpeg(
    src: &str,
    cache: &StdPath,
    src_audio_codec: Option<&str>,
) -> anyhow::Result<()> {
    let audio_args: &[&str] = match src_audio_codec {
        Some("aac") => &["-c:a", "copy"],
        // Everything else down-mixes to stereo AAC. ac3/eac3/dts/truehd
        // have zero browser support; re-encoding is cheap compared to
        // video.
        _ => &["-c:a", "aac", "-ac", "2", "-b:a", "192k"],
    };

    let playlist = cache.join("index.m3u8");
    let seg_pattern = cache.join("seg-%05d.m4s");

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-hide_banner")
        .arg("-loglevel").arg("warning")
        .arg("-nostdin")
        .arg("-i").arg(src)
        // First video + (optional) first audio track. Subtitles and
        // data streams don't belong in the HLS payload — subtitles are
        // served separately via /api/media/{id}/subtitle/*.
        .arg("-map").arg("0:v:0")
        .arg("-map").arg("0:a:0?")
        .arg("-c:v").arg("copy")
        .args(audio_args)
        .arg("-sn").arg("-dn")
        // Segment parameters. `hls_time` is a hint; with `-c:v copy`
        // ffmpeg still only cuts on the source's existing keyframes, so
        // real segment durations can be shorter or longer depending on
        // GOP layout.
        .arg("-f").arg("hls")
        .arg("-hls_time").arg("6")
        .arg("-hls_playlist_type").arg("event")
        .arg("-hls_segment_type").arg("fmp4")
        .arg("-hls_flags").arg("independent_segments+program_date_time")
        .arg("-hls_fmp4_init_filename").arg("init.mp4")
        .arg("-hls_segment_filename").arg(&seg_pattern)
        .arg(&playlist)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn ffmpeg: {e}"))?;

    // Forward stderr to tracing so the caller can see what ffmpeg is
    // doing without us eating the output.
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "binkflix::hls::ffmpeg", "{line}");
            }
        });
    }

    let status = child
        .wait()
        .await
        .map_err(|e| anyhow::anyhow!("ffmpeg wait: {e}"))?;
    if !status.success() {
        anyhow::bail!("ffmpeg exited with status {status}");
    }
    Ok(())
}

/// Serve any file under a media's HLS cache dir. Filename is validated
/// against the limited set of names ffmpeg actually produces so a caller
/// can't escape the cache root with `..` or pull random files.
pub async fn serve(
    State(state): State<AppState>,
    Path((id, file)): Path<(String, String)>,
) -> Result<Response> {
    if !is_allowed_name(&file) {
        return Err(Error::BadRequest(format!("invalid hls file: {file}")));
    }

    let cache = ensure_started(&state, &id).await?;
    let path = cache.join(&file);

    // Playlist is read + served rather than streamed because hls.js
    // re-fetches it every few seconds; the event-type playlist grows
    // each time and we want whatever's on disk right now, not a stale
    // cached reader.
    let bytes = tokio::fs::read(&path).await.map_err(|_| Error::NotFound)?;
    let mime = mime_for(&file);
    let mut resp = (StatusCode::OK, bytes).into_response();
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    // Playlists mutate while ffmpeg is running — hls.js must not cache.
    // Segments and init, once written, are immutable, so let the browser
    // cache aggressively.
    let cache_header = if file.ends_with(".m3u8") {
        "no-store"
    } else {
        "public, max-age=31536000, immutable"
    };
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache_header));
    Ok(resp)
}

fn is_allowed_name(name: &str) -> bool {
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

fn mime_for(name: &str) -> &'static str {
    if name.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else if name.ends_with(".m4s") || name.ends_with(".mp4") {
        "video/mp4"
    } else {
        "application/octet-stream"
    }
}

pub fn router() -> Router<AppState> {
    Router::new().route("/api/media/{id}/hls/{file}", get(serve))
}
