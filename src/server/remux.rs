//! Streaming endpoint for `/api/media/{id}/stream`.
//!
//! The server decides the delivery mode — the client just hits one URL
//! and gets whatever the source needs. The verdict comes from the
//! `browser_compat` field cached on the `media` row by the scanner (or
//! probed inline on cache miss), so we avoid the round-trip of "client
//! probes tech → picks URL → hits stream" which used to race the video
//! element's initial src set and flash a spurious "codec not supported"
//! error overlay.
//!
//! Delivery modes:
//! * `Direct`: source file served verbatim with byte-range support (fast
//!   seek). Used when container + codecs are already browser-friendly.
//! * `Remux`: source piped through ffmpeg with `-c:v copy -c:a aac` into
//!   fragmented MP4. Video-copy is ~free; audio re-encode is cheap.
//!   No byte-range support (stdin is non-seekable) — the browser can
//!   still scrub within what it has buffered.
//! * `Transcode`: not implemented yet — returns 501 until we add a real
//!   `-c:v libx264` path.
//!
//! Debug override: `?mode={direct,remux,transcode}` forces a specific
//! path regardless of the cached verdict. Useful when A/B-testing codec
//! support or the remux pipeline.
//!
//! ffmpeg is killed when the HTTP response body drops (client disconnect,
//! tab close, new src) via `Child::kill_on_drop`.

use super::error::{Error, Result};
use super::AppState;
use crate::types::BrowserCompat;
use axum::body::Body;
use bytes::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::stream;
use serde::Deserialize;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdout, Command};

#[derive(Debug, Deserialize, Default)]
pub struct StreamQuery {
    pub mode: Option<String>,
}

pub async fn media_stream(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<StreamQuery>,
    req: axum::extract::Request,
) -> Result<Response> {
    let path: (String,) = sqlx::query_as("SELECT path FROM media WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(Error::NotFound)?;
    let path = path.0;

    // Explicit override wins — useful for debugging or forcing remux on a
    // file that would otherwise be served direct.
    let explicit = match q.mode.as_deref() {
        Some("direct") => Some(BrowserCompat::Direct),
        Some("remux") => Some(BrowserCompat::Remux),
        Some("transcode") => Some(BrowserCompat::Transcode),
        Some(other) => return Err(Error::BadRequest(format!("unknown stream mode: {other}"))),
        None => None,
    };

    // No override: read the verdict from the cache the scanner populates.
    // On cache miss (file added after the last scan, probe previously
    // failed) probe inline so we still pick the right mode. Cheap —
    // subsequent plays hit the cache.
    let mode = match explicit {
        Some(m) => m,
        None => verdict_for(&state, &id, &path).await,
    };

    match mode {
        BrowserCompat::Direct => direct_stream(&path, req).await,
        BrowserCompat::Remux => remux_stream(&state, &id, &path).await,
        // Refuse silently-remuxing a file classified as transcode — the
        // remux pipeline is `-c:v copy`, which either fails the mux or
        // produces a stream the browser can't decode. Surface 501 so the
        // client can prompt the user to explicitly try `?mode=remux` or
        // `?mode=direct` with informed consent.
        BrowserCompat::Transcode => Err(Error::NotImplemented(
            "transcoding isn't implemented; use ?mode=remux or ?mode=direct to attempt a best-effort play".into(),
        )),
    }
}

async fn verdict_for(state: &AppState, id: &str, path: &str) -> BrowserCompat {
    if let Ok(Some(info)) = super::media_info::load(&state.pool, id).await {
        return info.browser_compat;
    }
    match super::media_info::probe(std::path::Path::new(path)).await {
        Ok(info) => {
            let verdict = info.browser_compat;
            let _ = super::media_info::store(&state.pool, id, &info).await;
            verdict
        }
        // If we can't even probe, direct is the least-bad default — the
        // browser will surface its own error if it really can't play it.
        Err(_) => BrowserCompat::Direct,
    }
}

async fn direct_stream(path: &str, req: axum::extract::Request) -> Result<Response> {
    use tower::ServiceExt;
    use tower_http::services::ServeFile;
    let resp = ServeFile::new(path)
        .oneshot(req)
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!(e)))?;
    let mut resp = resp.into_response();
    // Tell the client exactly how we chose to serve this — the debug
    // panel reads these rather than inferring from Accept-Ranges (which
    // can't distinguish remux from a future transcode path).
    resp.headers_mut().insert("x-stream-mode", HeaderValue::from_static("direct"));
    Ok(resp)
}

/// Output container family chosen based on the source video codec.
///
/// `Mp4` is used for H.264 (the dominant case) — fragmented MP4 with
/// `delay_moov` so the mvhd has a real duration on first byte.
/// `WebM` is used for VP9/VP8/AV1 sources, since those codecs don't go
/// into MP4 cleanly and every modern browser accepts them in WebM.
#[derive(Debug, Clone, Copy)]
enum OutputFamily {
    Mp4,
    WebM,
}

async fn remux_stream(state: &AppState, id: &str, path: &str) -> Result<Response> {
    // Cached probe carries both the duration (for mvhd) and the video
    // codec (which decides MP4 vs WebM output). Cache miss = live probe
    // so freshly-added files still work.
    let info = match super::media_info::load(&state.pool, id).await.ok().flatten() {
        Some(info) => Some(info),
        None => {
            let probed = super::media_info::probe(std::path::Path::new(path)).await.ok();
            if let Some(ref info) = probed {
                let _ = super::media_info::store(&state.pool, id, info).await;
            }
            probed
        }
    };

    let duration = info
        .as_ref()
        .and_then(|i| i.duration_seconds)
        .filter(|d| d.is_finite() && *d > 0.0);

    // Default to MP4 when we couldn't probe — matches the dominant case.
    let family = info
        .as_ref()
        .and_then(|i| i.video.as_ref())
        .map(|v| match v.codec.as_str() {
            "vp9" | "vp8" | "av1" => OutputFamily::WebM,
            _ => OutputFamily::Mp4,
        })
        .unwrap_or(OutputFamily::Mp4);

    let (content_type, format_flag, movflags_flag): (&str, &str, Option<&str>) = match family {
        // Fragmented MP4 + delay_moov: browser can start playback before
        // ffmpeg writes the full moov, and -t above populates mvhd with
        // the real duration so the scrubber shows correct length up-front.
        OutputFamily::Mp4 => (
            "video/mp4",
            "mp4",
            Some("frag_keyframe+delay_moov+default_base_moof"),
        ),
        // WebM is natively streamable (Matroska's cues are optional) —
        // no special flags needed. Live mode prevents ffmpeg from
        // buffering too aggressively before emitting the first cluster.
        OutputFamily::WebM => ("video/webm", "webm", None),
    };

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-hide_banner")
        .arg("-loglevel").arg("warning")
        .arg("-nostdin")
        .arg("-i").arg(path);
    if let Some(d) = duration {
        cmd.arg("-t").arg(format!("{d:.3}"));
    }
    cmd.arg("-c:v").arg("copy");

    // Audio: copy when the source codec is already native to the target
    // container; otherwise transcode to the container's preferred codec
    // (AAC for MP4, Opus for WebM). Stereo downmix keeps CPU low.
    let src_audio_codec = info
        .as_ref()
        .and_then(|i| i.audio.iter().find(|a| a.default).or_else(|| i.audio.first()))
        .map(|a| a.codec.as_str());
    // Returned alongside the chosen audio args so we can surface the
    // actual action ("copy" vs "aac"/"opus") in a response header.
    let audio_action: &str = match (family, src_audio_codec) {
        (OutputFamily::Mp4, Some("aac" | "mp3")) => {
            cmd.arg("-c:a").arg("copy");
            "copy"
        }
        (OutputFamily::Mp4, _) => {
            cmd.arg("-c:a").arg("aac").arg("-ac").arg("2").arg("-b:a").arg("192k");
            "aac"
        }
        (OutputFamily::WebM, Some("opus" | "vorbis")) => {
            cmd.arg("-c:a").arg("copy");
            "copy"
        }
        (OutputFamily::WebM, _) => {
            cmd.arg("-c:a").arg("libopus").arg("-ac").arg("2").arg("-b:a").arg("160k");
            "opus"
        }
    };

    cmd
        // Strip subtitles / data / attachments — they don't belong in the
        // streamed container. Clients get subs via /subtitles.
        .arg("-sn")
        .arg("-dn")
        .arg("-map").arg("0:v:0")
        .arg("-map").arg("0:a:0?");
    if let Some(flags) = movflags_flag {
        cmd.arg("-movflags").arg(flags);
    }
    let mut child = cmd
        .arg("-f").arg(format_flag)
        .arg("pipe:1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Error::Other(anyhow::anyhow!("failed to spawn ffmpeg: {e}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Other(anyhow::anyhow!("ffmpeg stdout missing")))?;

    // Hold the Child inside the stream's state so kill_on_drop fires the
    // instant the response body is dropped (client disconnects, switches
    // source, etc.). If we let `child` drop here, ffmpeg would be killed
    // before the first byte is read.
    let stream = stream::unfold(ReadState { stdout, _child: child }, |mut st| async move {
        let mut buf = vec![0u8; 64 * 1024];
        match st.stdout.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(Bytes::from(buf)), st))
            }
            Err(e) => Some((Err(e), st)),
        }
    });

    let body = Body::from_stream(stream);
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    // Can't honor Range on a pipe. Declaring `none` tells the browser not
    // to bother sending Range requests (and not to expose scrubbing
    // beyond the buffered range as a real seek).
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("none"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // Explicit mode + per-stream actions for the debug panel. Accept-
    // Ranges alone would conflate remux with a future transcode path
    // (both are non-seekable pipes).
    headers.insert("x-stream-mode", HeaderValue::from_static("remux"));
    headers.insert("x-stream-video", HeaderValue::from_static("copy"));
    headers.insert(
        "x-stream-audio",
        HeaderValue::from_str(audio_action).unwrap_or(HeaderValue::from_static("?")),
    );

    Ok((StatusCode::OK, headers, body).into_response())
}

struct ReadState {
    stdout: ChildStdout,
    _child: Child,
}
