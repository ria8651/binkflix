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
mod plan;
mod playlist;
mod producer;

pub use cache::{cache_root, id_is_safe, is_allowed_name, mime_for};
pub use plan::PLAN_VERSION;
pub use producer::{sweep_orphan_ffmpegs, ProducerRegistry};

use super::error::{Error, Result};
use super::AppState;
use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::path::PathBuf;
use std::sync::Arc;

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
) -> Result<axum::Json<crate::types::HlsState>> {
    let (plan, plan_dir, _src) = resolve_plan(&state, &id).await?;
    let cached_segments = scan_cached_segments(&plan_dir, plan.segments.len() as u32).await;
    let producer = state.hls_producers.snapshot(&id).await;
    Ok(axum::Json(crate::types::HlsState {
        duration: plan.duration,
        total_segments: plan.segments.len() as u32,
        segment_durations: plan.segments.iter().map(|s| s.d).collect(),
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

/// Resolve the plan + on-disk plan dir for a media. Builds + persists the
/// plan on cache miss; sweeps stale sibling dirs after a rebuild.
async fn resolve_plan(state: &AppState, id: &str) -> Result<(Arc<plan::StreamPlan>, PathBuf, PathBuf)> {
    if !id_is_safe(id) {
        return Err(Error::BadRequest("invalid media id".into()));
    }
    let row: (String,) = sqlx::query_as("SELECT path FROM media WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(Error::NotFound)?;
    let src = PathBuf::from(row.0);

    if let Some(loaded) = plan::load_if_fresh(&state.pool, id, &src)
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!(e)))?
    {
        let plan_dir = cache::plan_dir(
            id,
            loaded.plan.version,
            loaded.source_mtime,
            loaded.source_size,
        );
        return Ok((Arc::new(loaded.plan), plan_dir, src));
    }

    // Cache miss → build a fresh plan. The probe (also cached on the media
    // row as `tech_json`) provides codec + duration. If it's missing we
    // probe inline and persist; viability is decided by `build_remux_plan`
    // and surfaces as 501 if the source can't be copy-muxed.
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

    let plan = plan::build_remux_plan(&src, &info)
        .await
        .map_err(|e| Error::NotImplemented(format!("hls plan: {e}")))?;
    let (mtime, size) = cache::stat_source(&src)
        .await
        .map_err(Error::Io)?;
    plan::store(&state.pool, id, &plan, mtime, size)
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!(e)))?;

    let dir_name = cache::plan_dir_name(plan.version, mtime, size);
    let plan_dir = cache::media_dir(id).join(&dir_name);
    cache::sweep_stale_plan_dirs(id, &dir_name).await;

    Ok((Arc::new(plan), plan_dir, src))
}

async fn serve(
    State(state): State<AppState>,
    Path((id, file)): Path<(String, String)>,
) -> Result<Response> {
    if !is_allowed_name(&file) {
        return Err(Error::BadRequest(format!("invalid hls file: {file}")));
    }

    let (plan, plan_dir, src) = resolve_plan(&state, &id).await?;
    let path = plan_dir.join(&file);

    if file == "index.m3u8" {
        let body = playlist::render_m3u8(&plan);
        return Ok(text_response(body, "application/vnd.apple.mpegurl", false));
    }

    let ctx = producer::ProducerCtx {
        media_id: id.clone(),
        source: src,
        plan: plan.clone(),
        plan_dir: plan_dir.clone(),
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

/// Ensure init.mp4 exists. ffmpeg writes it to the canonical path as a
/// side effect of producing the first segment, so we just kick the
/// producer at segment 1 and wait for the file to land.
async fn ensure_init(state: &AppState, ctx: &producer::ProducerCtx) -> Result<()> {
    let canonical = ctx.plan_dir.join("init.mp4");
    if tokio::fs::try_exists(&canonical).await.unwrap_or(false) {
        return Ok(());
    }
    let _ = producer::ensure_segment(&state.hls_producers, ctx, 1)
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!(e)))?;
    if tokio::fs::try_exists(&canonical).await.unwrap_or(false) {
        return Ok(());
    }
    Err(Error::Other(anyhow::anyhow!(
        "init.mp4 not produced after producer started"
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
