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

use super::error::{Error, Result};
use super::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use crate::types::MediaTechInfo;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

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

/// Cap on the audio index a request can ask for. Sanity bound; real
/// files have at most a handful of audio streams.
const MAX_AUDIO_IDX: u32 = 64;

/// Allowed transcode bitrate range. Below 200 kbps libx264 produces
/// unwatchable mush; above 20 Mbps the cost outpaces what any
/// reasonable display profits from.
const MIN_BITRATE_KBPS: u32 = 200;
const MAX_BITRATE_KBPS: u32 = 20_000;
/// Fallback when the source has no probed bitrate and the user picked
/// "Auto". Comfortable 720p territory; the height ladder downscales
/// accordingly.
const AUTO_BITRATE_FALLBACK_KBPS: u32 = 4000;
/// Auto bitrate ceiling — never spend more than this even if the source
/// is a 30 Mbps Blu-ray rip. Users wanting more detail can pick an
/// explicit preset.
const AUTO_BITRATE_CEILING_KBPS: u32 = 6000;

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
    let row: (String,) = sqlx::query_as("SELECT path FROM media WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(Error::NotFound)?;
    let src = PathBuf::from(row.0);

    let info = match super::media_info::load(&state.pool, id)
        .await
        .ok()
        .flatten()
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
                    let p = plan::build_remux_plan(&src, &info)
                        .await
                        .map_err(|e| Error::NotImplemented(format!("hls plan: {e}")))?;
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
            let bitrate = resolve_bitrate(params.bitrate, info.bitrate_kbps);
            let plan = plan::build_transcode_plan(&info, bitrate)
                .map_err(|e| Error::Other(anyhow::anyhow!(e)))?;
            let max_height = match plan.mode {
                plan::Mode::Transcode { max_height, .. } => max_height,
                plan::Mode::Remux => unreachable!("build_transcode_plan returns Transcode"),
            };
            let mode_tag = format!("tx{bitrate}h{max_height}");
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

fn resolve_bitrate(explicit: Option<u32>, source_kbps: Option<u64>) -> u32 {
    if let Some(b) = explicit {
        return b.clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS);
    }
    let auto = source_kbps
        .and_then(|b| u32::try_from(b).ok())
        .unwrap_or(AUTO_BITRATE_FALLBACK_KBPS);
    auto.clamp(MIN_BITRATE_KBPS, AUTO_BITRATE_CEILING_KBPS)
}

async fn serve(
    State(state): State<AppState>,
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
        let body = playlist::render_m3u8(
            &plan,
            audio_idx,
            params.mode.as_deref(),
            params.bitrate,
            time_offset,
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
        // For transcode, advertise the H.264 encoder that's effectively
        // active. `current_encoder_name` honours the runtime-fallback
        // sticky flag, so a second playback after a hw-startup failure
        // already reads "libx264" here.
        if matches!(plan.mode, plan::Mode::Transcode { .. }) {
            let enc = producer::current_encoder_name(state.hwenc);
            if let Ok(v) = HeaderValue::from_str(enc) {
                h.insert("X-Stream-Encoder", v);
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
