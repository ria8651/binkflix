//! Plan-driven HLS pipeline.
//!
//! Two layers:
//! * **Plan** (`plan.rs`): pre-computed segment timeline persisted on the
//!   `media` row. Built once via a ffprobe keyframe pass; renders straight
//!   to a VOD m3u8 with `#EXT-X-ENDLIST` so the player sees the full
//!   timeline immediately and can seek anywhere.
//! * **Producer** (`producer.rs`): one ffmpeg child per active media, run
//!   sequentially from a `start_idx`, killed+restarted on seeks far from
//!   `head`, paused/resumed for backpressure, reaped on idle.
//!
//! HTTP surface (unchanged from the prior `/hls/{id}/...` endpoints):
//! * `GET /api/media/{id}/hls/index.m3u8` → render from plan, instant.
//! * `GET /api/media/{id}/hls/init.mp4` → ensure producer running, wait for
//!   the canonical init.mp4 to land, serve.
//! * `GET /api/media/{id}/hls/seg-NNNNN.m4s` → cache hit serves immediately;
//!   cache miss triggers the producer (start/resume/restart) and waits.

mod cache;
mod hwenc;
mod plan;
mod playlist;
mod producer;

pub use cache::{cache_root, id_is_safe, is_allowed_name, mime_for};
pub use hwenc::{detect as detect_hwenc, HwEncoder};
pub use plan::PLAN_VERSION;
pub use producer::{sweep_orphan_ffmpegs, ProducerRegistry};
// SessionRegistry / spawn_session_sweeper are defined in this module.

use super::error::{Error, Result};
use super::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Router};
use crate::types::MediaTechInfo;
use dashmap::DashMap;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Query string carried on every HLS endpoint:
/// * `?a=N` — source audio stream index (defaults to 0).
/// * `?mode=remux|transcode` — encode strategy (defaults to the cached
///   compat verdict — `Direct`-classed files can still be requested via
///   HLS by passing an explicit mode).
/// * `?bitrate=K` — target video bitrate in kbps for `mode=transcode`.
///   Ignored for remux. None means "auto" (derived from source bitrate).
/// * `?t=<sec>` — desired playback start position in seconds. Surfaced
///   into the m3u8 as `#EXT-X-START:TIME-OFFSET=<t>` so hls.js / Safari
///   align their first segment fetch to that offset (instead of seg 1).
///   The client computes this from the current playhead on mid-session
///   src changes (mode/bitrate/audio switch) or from saved-progress on
///   cold loads. Stays in the URL so the m3u8 remains a pure function
///   of its query — safe to cache by URL behind a reverse proxy.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct HlsParams {
    #[serde(default)]
    pub a: Option<u32>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub bitrate: Option<u32>,
    #[serde(default)]
    pub t: Option<f64>,
}

impl HlsParams {
    fn idx(&self) -> u32 {
        self.a.unwrap_or(0)
    }
}

/// Server-driven playback sessions for HLS. The producer pool is
/// intentionally NOT keyed by user (viewers share producers), so per-viewer
/// telemetry is tracked separately here: one session row per
/// `(user, media, audio, mode)`, opened on the first request `serve()`
/// observes and closed by the idle sweeper after [`SESSION_IDLE`] of no
/// requests. No client `start`/`end` calls — the lifecycle is entirely
/// request-driven.
type SessionKey = (String, String, u32, String);

struct ActiveSession {
    id: String,
    started: Instant,
    last_seen: Instant,
}

#[derive(Default)]
pub struct SessionRegistry {
    by_key: DashMap<SessionKey, ActiveSession>,
}

/// Idle window before a session is considered ended. A viewer's session
/// lingers this long after their last segment fetch before `ended_at` is
/// written — fine for analytics.
const SESSION_IDLE: Duration = Duration::from_secs(45);

impl SessionRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Open-or-refresh the session for `key`. Returns `(session_id,
    /// is_new)`; the caller inserts the DB row when `is_new`. No `.await`
    /// is held across the DashMap entry lock.
    fn touch(&self, key: SessionKey) -> (String, bool) {
        use dashmap::mapref::entry::Entry;
        let now = Instant::now();
        match self.by_key.entry(key) {
            Entry::Occupied(mut e) => {
                e.get_mut().last_seen = now;
                (e.get().id.clone(), false)
            }
            Entry::Vacant(e) => {
                let id = uuid::Uuid::new_v4().to_string();
                e.insert(ActiveSession {
                    id: id.clone(),
                    started: now,
                    last_seen: now,
                });
                (id, true)
            }
        }
    }

    /// Close + drop sessions idle for [`SESSION_IDLE`]. Run on a loop from
    /// startup (see [`spawn_session_sweeper`]).
    pub async fn sweep(&self, pool: &sqlx::SqlitePool) {
        let now = Instant::now();
        let mut expired: Vec<(String, u64)> = Vec::new();
        self.by_key.retain(|_, v| {
            if now.duration_since(v.last_seen) >= SESSION_IDLE {
                let watched_ms = v.last_seen.duration_since(v.started).as_millis() as u64;
                expired.push((v.id.clone(), watched_ms));
                false
            } else {
                true
            }
        });
        for (id, watched_ms) in expired {
            super::analytics::close_playback_session(pool, &id, Some(watched_ms)).await;
        }
    }
}

/// Spawn the 15s loop that closes idle HLS playback sessions.
pub fn spawn_session_sweeper(sessions: Arc<SessionRegistry>, pool: sqlx::SqlitePool) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            sessions.sweep(&pool).await;
        }
    });
}

/// Cap on the audio index a request can ask for. Sanity bound; real
/// files have at most a handful of audio streams.
const MAX_AUDIO_IDX: u32 = 64;

/// Allowed transcode bitrate range. Below 200 kbps libx264 produces
/// unwatchable mush; above 20 Mbps the cost outpaces what any
/// reasonable display profits from.
const MIN_BITRATE_KBPS: u32 = 200;
const MAX_BITRATE_KBPS: u32 = 20_000;
/// Height Auto assumes when the probe reported no video height at all.
/// Comfortable middle rung — better to guess 720p than to guess wrong in
/// either extreme.
const AUTO_FALLBACK_HEIGHT: u32 = 720;
/// Auto never transcodes above 1080p, even from a 4K source. Encoding
/// 2160p in real time is out of reach for the software path, and the
/// explicit presets top out here too.
const AUTO_MAX_HEIGHT: u32 = 1080;
/// How far over the source's own bitrate Auto is allowed to spend. A soft
/// 1080p rip doesn't get sharper for being handed 8 Mbps, so the source
/// bitrate caps the budget — but h264 needs real headroom to re-encode an
/// HEVC/AV1 source without visibly losing to it, hence 2x rather than 1x.
const AUTO_SOURCE_HEADROOM: u32 = 2;

pub fn router() -> Router<AppState> {
    Router::new()
        // `state` must be matched before the catch-all `{file}` route or
        // axum routes it through `serve()` and 400s on the unknown name.
        .route("/api/media/{id}/hls/state", get(state))
        .route("/api/media/{id}/hls/{file}", get(serve))
}

/// Debug snapshot consumed by the player's debug panel. Cheap enough to
/// poll once a second: scans the plan dir for cached segments (sub-ms on
/// 1k-entry dirs), reads three atomics + one async lock for producer
/// state. Not authenticated separately — same `require_session` middleware
/// covers it as the rest of `/api/media/...`.
async fn state(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HlsParams>,
) -> Result<axum::Json<crate::types::HlsState>> {
    let audio_idx = validate_audio_idx(params.idx())?;
    let resolved = resolve_plan(&state, &id, audio_idx, &params).await?;
    let cached_segments = scan_cached_segments(&resolved.plan_dir, resolved.plan.segments.len() as u32).await;
    let producer = state
        .hls_producers
        .snapshot(&id, audio_idx, &resolved.mode_tag)
        .await;
    Ok(axum::Json(crate::types::HlsState {
        duration: resolved.plan.duration,
        total_segments: resolved.plan.segments.len() as u32,
        segment_durations: resolved.plan.segments.iter().map(|s| s.d).collect(),
        cached_segments,
        producer,
    }))
}

async fn scan_cached_segments(plan_dir: &std::path::Path, total: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let Ok(mut rd) = tokio::fs::read_dir(plan_dir).await else {
        return found;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(idx) = cache::segment_index(name) {
            if idx > 0 && idx <= total {
                found.push(idx);
            }
        }
    }
    found.sort_unstable();
    found
}

fn validate_audio_idx(idx: u32) -> Result<u32> {
    if idx > MAX_AUDIO_IDX {
        return Err(Error::BadRequest(format!(
            "audio index {idx} out of range (max {MAX_AUDIO_IDX})"
        )));
    }
    Ok(idx)
}

struct ResolvedPlan {
    plan: Arc<plan::StreamPlan>,
    plan_dir: PathBuf,
    src: PathBuf,
    info: MediaTechInfo,
    /// Cache-key tag derived from the chosen mode + bitrate. Same shape
    /// as `mode_tag` in `cache::plan_dir_name`.
    mode_tag: String,
}

/// Resolve the plan + on-disk plan dir + tech info for a media. Builds +
/// persists the remux plan on cache miss; transcode plans are built on
/// demand without DB caching (cheap — duration only). Sweeps stale
/// sibling dirs after a rebuild. The remux plan timeline is shared
/// across audio tracks; `audio_idx` and `mode_tag` only affect the
/// returned `plan_dir` so each (track, mode, bitrate) caches separately.
async fn resolve_plan(
    state: &AppState,
    id: &str,
    audio_idx: u32,
    params: &HlsParams,
) -> Result<ResolvedPlan> {
    if !id_is_safe(id) {
        return Err(Error::BadRequest("invalid media id".into()));
    }
    let row: (String,) = sqlx::query_as(
        "SELECT path FROM media WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(Error::NotFound)?;
    let src = PathBuf::from(row.0);

    // Validate-on-read: if the file changed since we last derived its
    // probe_json, run the essential pass first so `derive_audio_plan`
    // indexes the current track list (a stale 1-track list would map a
    // chosen higher index to nothing → audio dropped).
    let info = match super::media_info::load_fresh(state, id)
        .await
        .ok()
        .and_then(|f| f.info)
    {
        Some(i) => i,
        None => {
            let probed = super::media_info::probe(&src)
                .await
                .map_err(|e| Error::Other(anyhow::anyhow!(e)))?;
            let _ = super::media_info::store(&state.pool, id, &probed).await;
            probed
        }
    };

    let chosen_mode = match params.mode.as_deref() {
        Some("remux") => RequestedMode::Remux,
        Some("transcode") => RequestedMode::Transcode,
        Some(other) => {
            return Err(Error::BadRequest(format!("unknown hls mode: {other}")));
        }
        None => match info.browser_compat {
            crate::types::BrowserCompat::Transcode => RequestedMode::Transcode,
            _ => RequestedMode::Remux,
        },
    };

    let (mtime, size) = cache::stat_source(&src).await.map_err(Error::Io)?;

    match chosen_mode {
        RequestedMode::Remux => {
            let plan = match plan::load_if_fresh(&state.pool, id, &src)
                .await
                .map_err(|e| Error::Other(anyhow::anyhow!(e)))?
            {
                Some(loaded) => loaded,
                None => {
                    // ffprobe error messages can include the full source path;
                    // log details server-side and return a generic public message.
                    let p = plan::build_remux_plan(&src, &info)
                        .await
                        .map_err(|e| {
                            tracing::error!(media_id = %id, %e, "failed to build HLS plan");
                            Error::NotImplemented("hls plan unavailable".into())
                        })?;
                    plan::store(&state.pool, id, &p, mtime, size)
                        .await
                        .map_err(|e| Error::Other(anyhow::anyhow!(e)))?;
                    let keep_prefix = cache::plan_dir_prefix(p.version, mtime, size);
                    cache::sweep_stale_plan_dirs(id, &keep_prefix).await;
                    p
                }
            };
            let mode_tag = "remux".to_string();
            let plan_dir = cache::plan_dir(id, plan.version, mtime, size, audio_idx, &mode_tag);
            Ok(ResolvedPlan {
                plan: Arc::new(plan),
                plan_dir,
                src,
                info,
                mode_tag,
            })
        }
        RequestedMode::Transcode => {
            let (bitrate, max_height) = resolve_encode_target(params.bitrate, &info);
            let hard_cap = params.bitrate.is_some();
            let plan = plan::build_transcode_plan(&info, bitrate, max_height, hard_cap)
                .map_err(|e| Error::Other(anyhow::anyhow!(e)))?;
            // The `-auto` suffix keeps Auto's cache separate from an explicit
            // pick that happens to resolve to the same numbers: on a
            // quality-mode encoder the two produce different bytes. It's a
            // *suffix* so `server_transcode_telemetry`'s `tx{bitrate}h`
            // prefix lookup still matches either.
            let mode_tag = if hard_cap {
                format!("tx{bitrate}h{max_height}")
            } else {
                format!("tx{bitrate}h{max_height}-auto")
            };
            let plan_dir = cache::plan_dir(id, plan.version, mtime, size, audio_idx, &mode_tag);
            Ok(ResolvedPlan {
                plan: Arc::new(plan),
                plan_dir,
                src,
                info,
                mode_tag,
            })
        }
    }
}

#[derive(Copy, Clone)]
enum RequestedMode {
    Remux,
    Transcode,
}

/// Resolve a transcode request to its `(bitrate_kbps, max_height)` pair.
///
/// The two branches deliberately run the dependency in opposite
/// directions:
///
/// * **Explicit** — the user picked a rung off the menu, and that menu
///   promises a resolution ("2 Mbps · ~480p"). Bitrate leads, height
///   follows, so dropping a tier genuinely downscales.
/// * **Auto** — height comes from the source and the budget follows from
///   the height. Deriving height from the source *bitrate* (as this used
///   to) means the more efficiently a file is encoded the worse the
///   picture we hand back, which is exactly backwards.
///
/// Keep in sync with `encode_target` in `src/video_player.rs`.
fn resolve_encode_target(explicit: Option<u32>, info: &MediaTechInfo) -> (u32, u32) {
    if let Some(b) = explicit {
        let bitrate = b.clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS);
        return (bitrate, plan::height_for_bitrate(bitrate));
    }
    let height = info
        .video
        .as_ref()
        .and_then(|v| v.height)
        .unwrap_or(AUTO_FALLBACK_HEIGHT)
        .min(AUTO_MAX_HEIGHT);
    let budget = plan::bitrate_for_height(height);
    let source_cap = info
        .bitrate_kbps
        .and_then(|b| u32::try_from(b).ok())
        .and_then(|b| b.checked_mul(AUTO_SOURCE_HEADROOM))
        .unwrap_or(u32::MAX);
    (
        budget.min(source_cap).max(MIN_BITRATE_KBPS),
        height.max(1),
    )
}

async fn serve(
    State(state): State<AppState>,
    Extension(session): Extension<super::auth::Session>,
    headers: HeaderMap,
    Path((id, file)): Path<(String, String)>,
    Query(params): Query<HlsParams>,
) -> Result<Response> {
    if !is_allowed_name(&file) {
        return Err(Error::BadRequest(format!("invalid hls file: {file}")));
    }

    let audio_idx = validate_audio_idx(params.idx())?;
    let resolved = resolve_plan(&state, &id, audio_idx, &params).await?;
    let ResolvedPlan { plan, plan_dir, src, info, mode_tag } = resolved;
    let path = plan_dir.join(&file);

    // Server-driven playback session: open-or-refresh keyed by
    // (user, media, audio, mode). Opened on the first request; refreshed
    // (cheap, in-memory) on every later one; closed by the idle sweeper.
    // `room_id` is derived from the syncplay hub so a watch party is
    // attributed without the client passing anything.
    let (session_id, is_new) = state.active_sessions.touch((
        session.user_sub.clone(),
        id.clone(),
        audio_idx,
        mode_tag.clone(),
    ));
    if is_new {
        let delivery_mode = match &plan.mode {
            plan::Mode::Remux => "remux",
            plan::Mode::Transcode { .. } => "transcode",
        };
        let src_video = info.video.as_ref().map(|v| v.codec.as_str());
        let src_audio = info.audio.get(audio_idx as usize).map(|a| a.codec.as_str());
        let (out_video, out_audio, bitrate): (Option<&str>, Option<&str>, Option<u32>) =
            match &plan.mode {
                plan::Mode::Remux => (src_video, src_audio, None),
                plan::Mode::Transcode { bitrate_kbps, .. } => {
                    (Some("h264"), Some("aac"), Some(*bitrate_kbps))
                }
            };
        let room_id = state.hub.room_for_user(&session.user_sub);
        let browser = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(super::analytics::parse_ua);
        super::analytics::open_playback_session(
            &state.pool,
            super::analytics::PlaybackSessionStart {
                id: &session_id,
                user_sub: Some(&session.user_sub),
                media_id: &id,
                delivery_mode,
                chosen_reason: None,
                src_video_codec: src_video,
                src_audio_codec: src_audio,
                src_container: info.container.as_deref(),
                out_video_codec: out_video,
                out_audio_codec: out_audio,
                out_container: Some("mp4"),
                target_bitrate_kbps: bitrate,
                browser: browser.as_deref(),
                room_id: room_id.as_deref(),
                audio_idx: Some(audio_idx),
                forced_via_query: params.mode.is_some(),
            },
        )
        .await;
    }

    if file == "index.m3u8" {
        // Clamp the requested start to inside the plan's covered range.
        // RFC 8216 says players SHOULD clamp, but some older Safari
        // versions silently drop an out-of-range TIME-OFFSET and start
        // at 0 — exactly the behavior we're trying to prevent. The
        // 0.5s tail leaves room for the last segment without landing
        // past EOF.
        let time_offset = params.t.and_then(|t| {
            if t.is_finite() && t > 0.0 {
                Some(t.min((plan.duration - 0.5).max(0.0)))
            } else {
                None
            }
        });
        // The plan-dir name is the server's content-identity key; thread it
        // into the segment/init URIs (as `&v=`) so the `immutable` cache those
        // URLs earn is busted whenever the plan regenerates. See `render_m3u8`.
        let cache_tag = plan_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let body = playlist::render_m3u8(
            &plan,
            audio_idx,
            params.mode.as_deref(),
            params.bitrate,
            time_offset,
            cache_tag,
        );
        let mut resp = text_response(body, "application/vnd.apple.mpegurl", false);
        let h = resp.headers_mut();
        // Mirror the `/stream` endpoint's mode advertisement so the
        // player's debug-panel `ObservedStream` parser engages — without
        // `X-Stream-Mode` it would fall back to the cached compat
        // verdict and miss the encoder/container we're actually serving.
        let mode_str = match plan.mode {
            plan::Mode::Remux => "remux",
            plan::Mode::Transcode { .. } => "transcode",
        };
        if let Ok(v) = HeaderValue::from_str(mode_str) {
            h.insert("X-Stream-Mode", v);
        }
        // Hand the client its server-derived session id so the optional
        // metrics stream can tag samples without a separate start call.
        if let Ok(v) = HeaderValue::from_str(&session_id) {
            h.insert("X-Playback-Session", v);
        }
        // For transcode, advertise the H.264 encoder that's effectively
        // active. `current_encoder_name` honours the runtime-fallback
        // sticky flag, so a second playback after a hw-startup failure
        // already reads "libx264" here.
        if let plan::Mode::Transcode { hard_cap, .. } = plan.mode {
            let enc = producer::current_encoder_name(state.hwenc);
            if let Ok(v) = HeaderValue::from_str(enc) {
                h.insert("X-Stream-Encoder", v);
            }
            // Which rate control that encoder ended up on. Without this the
            // panel can't tell capped CRF from ABR from VideoToolbox's
            // uncapped quality mode, and would report a bitrate ceiling
            // that two of the three don't actually honour.
            let rc = producer::current_rate_control_name(state.hwenc, hard_cap);
            if let Ok(v) = HeaderValue::from_str(&rc) {
                h.insert("X-Stream-Ratecontrol", v);
            }
        }
        return Ok(resp);
    }

    let ctx = producer::ProducerCtx {
        media_id: id.clone(),
        source: src,
        plan: plan.clone(),
        plan_dir: plan_dir.clone(),
        audio_idx,
        mode_tag,
        audio: plan::derive_audio_plan(&info, audio_idx),
        hw: state.hwenc,
        pool: state.pool.clone(),
    };

    if file == "init.mp4" {
        ensure_init(&state, &ctx).await?;
    } else if let Some(idx) = cache::segment_index(&file) {
        producer::ensure_segment(&state.hls_producers, &ctx, idx)
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!(e)))?;
    }

    let bytes = tokio::fs::read(&path).await.map_err(|_| Error::NotFound)?;
    Ok(file_response(bytes, mime_for(&file), true))
}

/// Ensure init.mp4 exists. ffmpeg's HLS-fmp4 muxer writes init.mp4
/// *before* any segment data — the moov box is flushed as soon as codec
/// params are settled. We kick the producer at segment 1 (just to get
/// the muxer running) but **return as soon as init.mp4 lands**, not
/// after seg 1's full encode.
///
/// Why it matters: hls.js loads init.mp4 strictly before its first
/// media-segment fetch. If the player has a saved-position resume
/// pending (e.g. user left off at 25:00), the resume `seekTo` only
/// fires after `attach` completes — i.e. after init.mp4 returns. If we
/// blocked here on seg 1's full encode, ffmpeg would burn through 6+
/// segments at the start of the file before hls.js ever gets a chance
/// to ask for seg 250. Returning early lets the seek-driven segment
/// fetch arrive immediately, which `ensure_segment` handles via the
/// existing far-seek restart path (kills the seg-1 producer, relaunches
/// at target_idx=250).
async fn ensure_init(state: &AppState, ctx: &producer::ProducerCtx) -> Result<()> {
    let canonical = ctx.plan_dir.join("init.mp4");
    if tokio::fs::try_exists(&canonical).await.unwrap_or(false) {
        return Ok(());
    }
    // Drive the producer in the background at seg 1. The seg-1 spawn
    // looks like a footgun, but with `#EXT-X-START:TIME-OFFSET=N` in
    // the m3u8 the player's first segment fetch is at the user's
    // intended position, not seg 1 — so `ensure_segment(N)` arrives
    // alongside (or just after) this spawn and the existing far-seek
    // restart in `ensure_segment` kills this transient seg-1 producer
    // and relaunches at N. Wasted work is bounded to whatever ffmpeg
    // managed to encode in the few hundred ms before the real segment
    // request arrived.
    //
    // Cancellation here doesn't kill the producer — that's the
    // registry's job — but we don't *await* the segment, only init.mp4
    // itself.
    let registry = state.hls_producers.clone();
    let ctx_bg = ctx.clone();
    tokio::spawn(async move {
        let _ = producer::ensure_segment(&registry, &ctx_bg, 1).await;
    });
    // Poll for init.mp4. The watcher promotes it the first tick (≤100ms)
    // after ffmpeg writes it; ffmpeg writes it shortly after `-i` is
    // probed. 60s ceiling matches the segment wait timeout.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        if tokio::fs::try_exists(&canonical).await.unwrap_or(false) {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Err(Error::Other(anyhow::anyhow!(
        "init.mp4 not produced within timeout"
    )))
}

fn text_response(body: String, mime: &'static str, immutable: bool) -> Response {
    let mut resp = (StatusCode::OK, body).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if immutable {
            "public, max-age=31536000, immutable"
        } else {
            "no-store"
        }),
    );
    resp
}

fn file_response(bytes: Vec<u8>, mime: &'static str, immutable: bool) -> Response {
    let mut resp = (StatusCode::OK, bytes).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if immutable {
            "public, max-age=31536000, immutable"
        } else {
            "no-store"
        }),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{BrowserCompat, VideoTrackInfo};

    fn info(height: Option<u32>, container_kbps: Option<u64>) -> MediaTechInfo {
        MediaTechInfo {
            container: Some("matroska".into()),
            duration_seconds: Some(1200.0),
            bitrate_kbps: container_kbps,
            file_size: None,
            video: height.map(|h| VideoTrackInfo {
                codec: "hevc".into(),
                profile: None,
                width: Some(h * 16 / 9),
                height: Some(h),
                fps: Some(23.976),
                bitrate_kbps: None,
                pix_fmt: Some("yuv420p10le".into()),
            }),
            audio: vec![],
            browser_compat: BrowserCompat::Transcode,
            compat_reason: None,
        }
    }

    #[test]
    fn auto_keeps_source_height_on_efficient_encodes() {
        // The regression this whole change exists for: a 1080p HEVC rip at
        // 2 Mbps used to be read as "low bitrate → 480p". Height must now
        // come from the source, and the budget from the height.
        let (bitrate, height) = resolve_encode_target(None, &info(Some(1080), Some(2000)));
        assert_eq!(height, 1080);
        // 8000 budget for 1080p, capped by 2x the 2 Mbps source.
        assert_eq!(bitrate, 4000);
    }

    #[test]
    fn auto_budget_follows_height() {
        assert_eq!(resolve_encode_target(None, &info(Some(1080), None)), (8000, 1080));
        assert_eq!(resolve_encode_target(None, &info(Some(720), None)), (4000, 720));
        assert_eq!(resolve_encode_target(None, &info(Some(480), None)), (2000, 480));
        assert_eq!(resolve_encode_target(None, &info(Some(360), None)), (1000, 360));
    }

    #[test]
    fn auto_never_upscales_and_caps_at_1080p() {
        // 4K source is pulled down to the 1080p ceiling, not encoded at 2160p.
        assert_eq!(resolve_encode_target(None, &info(Some(2160), None)), (8000, 1080));
        // A genuinely small source stays small rather than being inflated.
        assert_eq!(resolve_encode_target(None, &info(Some(240), None)).1, 240);
    }

    #[test]
    fn auto_falls_back_when_probe_has_no_height() {
        assert_eq!(resolve_encode_target(None, &info(None, None)), (4000, 720));
    }

    #[test]
    fn auto_never_drops_below_the_floor() {
        // A pathologically low source bitrate must not drive the budget to
        // zero via the 2x headroom cap.
        let (bitrate, _) = resolve_encode_target(None, &info(Some(1080), Some(10)));
        assert_eq!(bitrate, MIN_BITRATE_KBPS);
    }

    #[test]
    fn explicit_picks_still_downscale() {
        // The menu labels promise a resolution, so an explicit low tier has
        // to actually drop the height even on a 1080p source.
        assert_eq!(resolve_encode_target(Some(2000), &info(Some(1080), Some(9000))), (2000, 480));
        assert_eq!(resolve_encode_target(Some(1000), &info(Some(1080), Some(9000))), (1000, 360));
        assert_eq!(resolve_encode_target(Some(8000), &info(Some(1080), Some(9000))), (8000, 1080));
    }

    #[test]
    fn explicit_picks_ignore_the_source_bitrate_cap() {
        // Auto's "don't outspend the source" guard must not silently
        // undercut a bitrate the user asked for by name.
        assert_eq!(resolve_encode_target(Some(8000), &info(Some(1080), Some(1500))), (8000, 1080));
    }

    #[test]
    fn explicit_picks_are_clamped_to_the_allowed_range() {
        assert_eq!(resolve_encode_target(Some(0), &info(Some(1080), None)).0, MIN_BITRATE_KBPS);
        assert_eq!(resolve_encode_target(Some(999_999), &info(Some(1080), None)).0, MAX_BITRATE_KBPS);
    }
}
