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

use super::analytics::{self, PlaybackSessionStart};
use super::auth::Session;
use super::error::{Error, Result};
use super::AppState;
use crate::types::{BrowserCompat, MediaTechInfo};
use axum::body::Body;
use bytes::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::stream;
use serde::Deserialize;
use sqlx::SqlitePool;
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

    // Snapshot enough source detail to make later analysis self-contained
    // (codecs may change if the user replaces a file). Probe-on-miss so
    // freshly-added files still record real codecs rather than NULL.
    let info = load_or_probe_info(&state, &id, &path).await;
    // Pull request-scoped data out into owned values *before* the next
    // `.await` so the handler future stays `Send` — borrowing through
    // `&req` across an await trips axum's Handler trait inference.
    let user_sub = req.extensions().get::<Session>().map(|s| s.user_sub.clone());
    let browser = req
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(analytics::parse_ua);
    let session_id = open_playback_session(
        &state.pool,
        &id,
        mode,
        q.mode.is_some(),
        &info,
        user_sub,
        browser,
    )
    .await;

    match mode {
        BrowserCompat::Direct => direct_stream(&path, req).await,
        BrowserCompat::Remux => remux_stream(&state, &id, &path, session_id).await,
        // Transcode is delivered via the HLS pipeline (`-c:v libx264`
        // segmented into fMP4). The client picks that URL directly when
        // the compat verdict is Transcode; this branch only fires for
        // the `?mode=transcode` debug override against `/stream`, where
        // we redirect to the equivalent HLS URL.
        BrowserCompat::Transcode => {
            let target = format!("/api/media/{id}/hls/index.m3u8?a=0&mode=transcode");
            Ok((
                StatusCode::TEMPORARY_REDIRECT,
                [(header::LOCATION, target)],
            )
                .into_response())
        }
    }
}

async fn load_or_probe_info(state: &AppState, id: &str, path: &str) -> Option<MediaTechInfo> {
    if let Ok(Some(info)) = super::media_info::load(&state.pool, id).await {
        return Some(info);
    }
    super::media_info::probe(std::path::Path::new(path)).await.ok()
}

fn delivery_mode_str(m: BrowserCompat) -> &'static str {
    match m {
        BrowserCompat::Direct => "direct",
        BrowserCompat::Remux => "remux",
        BrowserCompat::Transcode => "transcode",
    }
}

/// INSERT a `playback_sessions` row and return its id. Best-effort: if the
/// insert fails the session id is still useful as a correlation key for
/// downstream samples, even though they'll lack the parent row.
async fn open_playback_session(
    pool: &SqlitePool,
    media_id: &str,
    mode: BrowserCompat,
    forced_via_query: bool,
    info: &Option<MediaTechInfo>,
    user_sub: Option<String>,
    browser: Option<String>,
) -> String {
    let session_id = uuid::Uuid::new_v4().simple().to_string();

    let src_video_codec = info.as_ref().and_then(|i| i.video.as_ref()).map(|v| v.codec.clone());
    let src_audio_codec = info
        .as_ref()
        .and_then(|i| i.audio.iter().find(|a| a.default).or_else(|| i.audio.first()))
        .map(|a| a.codec.clone());
    let src_container = info.as_ref().and_then(|i| i.container.clone());

    // The verdict's free-text rationale (e.g. "container matroska needs
    // repackaging to mp4") is the most useful "why" we have. For ?mode=
    // overrides, prefix to make the override obvious in queries.
    let chosen_reason = if forced_via_query {
        Some(format!("forced_via_query:{}", delivery_mode_str(mode)))
    } else {
        info.as_ref().and_then(|i| i.compat_reason.clone())
    };

    analytics::open_playback_session(
        pool,
        PlaybackSessionStart {
            id: &session_id,
            user_sub: user_sub.as_deref(),
            media_id,
            delivery_mode: delivery_mode_str(mode),
            chosen_reason: chosen_reason.as_deref(),
            src_video_codec: src_video_codec.as_deref(),
            src_audio_codec: src_audio_codec.as_deref(),
            src_container: src_container.as_deref(),
            // `out_*` and `target_bitrate_kbps` are filled in by the per-mode
            // path once it knows them (currently only remux_stream).
            out_video_codec: None,
            out_audio_codec: None,
            out_container: None,
            target_bitrate_kbps: None,
            browser: browser.as_deref(),
            room_id: None,
            forced_via_query,
        },
    )
    .await;

    session_id
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

async fn remux_stream(
    state: &AppState,
    id: &str,
    path: &str,
    session_id: String,
) -> Result<Response> {
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
        // Fragmented MP4 with empty_moov: moov is written up-front (before
        // any fragments), so we can buffer + patch it inline below. We used
        // to use delay_moov hoping ffmpeg would write the full duration
        // there, but under `-c:v copy` the duration written is just the
        // first fragment's accumulated track_duration — often a single
        // multi-minute GOP — so the scrubber reported e.g. 4 minutes for a
        // 2-hour movie. With empty_moov the moov has duration 0, which we
        // then overwrite with the probed total in `patch_mp4_moov_durations`.
        OutputFamily::Mp4 => (
            "video/mp4",
            "mp4",
            Some("frag_keyframe+empty_moov+default_base_moof"),
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

    // Now that we know the actual output codecs/container, fill them into
    // the session row. `audio_action` is "copy" when we keep the source
    // codec — record the source codec in that case rather than the literal
    // string "copy", so queries against `out_audio_codec` see real codecs.
    let out_audio_codec_label: String = match audio_action {
        "copy" => src_audio_codec.unwrap_or("copy").to_string(),
        other => other.to_string(),
    };
    let out_container_label = match family {
        OutputFamily::Mp4 => "mp4",
        OutputFamily::WebM => "webm",
    };
    analytics::set_playback_outputs(
        &state.pool,
        &session_id,
        // `-c:v copy` always: report the actual source codec, not "copy".
        info.as_ref().and_then(|i| i.video.as_ref()).map(|v| v.codec.as_str()),
        Some(&out_audio_codec_label),
        Some(out_container_label),
        None,
    )
    .await;

    // Hold the Child inside the stream's state so kill_on_drop fires the
    // instant the response body is dropped (client disconnects, switches
    // source, etc.). If we let `child` drop here, ffmpeg would be killed
    // before the first byte is read.
    //
    // For MP4 output we also buffer until we've seen the whole `moov` box,
    // patch its duration fields with the probed total, then switch to raw
    // pass-through for the fragments that follow. ffmpeg under `-c:v copy`
    // writes mvhd/tkhd/mdhd/mehd with wrong or zero durations (see the
    // movflags comment above); without this patch the browser's scrubber
    // shows the wrong length and refuses to seek past the initial fragment.
    // WebM output doesn't need this — ffmpeg writes the Matroska Duration
    // element from its own input probe.
    let patch_moov = matches!(family, OutputFamily::Mp4);
    let stream = stream::unfold(
        StreamState {
            stdout,
            _child: child,
            _session: SessionEndGuard {
                pool: state.pool.clone(),
                session_id,
            },
            phase: if patch_moov {
                Phase::Buffering { buf: Vec::with_capacity(64 * 1024), total_secs: duration }
            } else {
                Phase::PassThrough
            },
        },
        |mut st| async move {
            loop {
                match st.phase {
                    Phase::Buffering { mut buf, total_secs } => {
                        let mut chunk = vec![0u8; 64 * 1024];
                        match st.stdout.read(&mut chunk).await {
                            Ok(0) => {
                                // ffmpeg exited before we finished the moov. Flush
                                // whatever we buffered so the client sees the error
                                // the same way it would for a raw stream.
                                st.phase = Phase::PassThrough;
                                if buf.is_empty() {
                                    return None;
                                }
                                return Some((Ok::<_, std::io::Error>(Bytes::from(buf)), st));
                            }
                            Ok(n) => {
                                buf.extend_from_slice(&chunk[..n]);
                                if let Some(range) = find_top_level_box(&buf, *b"moov") {
                                    if let Some(secs) = total_secs {
                                        patch_mp4_moov_durations(&mut buf[range], secs);
                                    }
                                    st.phase = Phase::PassThrough;
                                    return Some((Ok(Bytes::from(buf)), st));
                                }
                                // Safety valve: if ffmpeg somehow writes >4MB of
                                // pre-moov data, stop buffering and stream as-is
                                // rather than holding memory forever. The client
                                // will just see the uncorrected duration.
                                if buf.len() > 4 * 1024 * 1024 {
                                    st.phase = Phase::PassThrough;
                                    return Some((Ok(Bytes::from(buf)), st));
                                }
                                st.phase = Phase::Buffering { buf, total_secs };
                                continue;
                            }
                            Err(e) => {
                                st.phase = Phase::Buffering { buf, total_secs };
                                return Some((Err(e), st));
                            }
                        }
                    }
                    Phase::PassThrough => {
                        let mut chunk = vec![0u8; 64 * 1024];
                        match st.stdout.read(&mut chunk).await {
                            Ok(0) => return None,
                            Ok(n) => {
                                chunk.truncate(n);
                                st.phase = Phase::PassThrough;
                                return Some((Ok(Bytes::from(chunk)), st));
                            }
                            Err(e) => {
                                st.phase = Phase::PassThrough;
                                return Some((Err(e), st));
                            }
                        }
                    }
                }
            }
        },
    );

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

struct StreamState {
    stdout: ChildStdout,
    _child: Child,
    _session: SessionEndGuard,
    phase: Phase,
}

/// Closes the `playback_sessions` row when the response body terminates —
/// either a clean stream end (unfold returns None and drops the state) or a
/// client disconnect (axum drops the body, which drops the state). Spawned
/// async so Drop stays sync; the close is idempotent at the SQL level
/// (`WHERE ended_at IS NULL`).
struct SessionEndGuard {
    pool: SqlitePool,
    session_id: String,
}

impl Drop for SessionEndGuard {
    fn drop(&mut self) {
        if self.session_id.is_empty() {
            return;
        }
        let pool = self.pool.clone();
        let id = std::mem::take(&mut self.session_id);
        tokio::spawn(async move {
            analytics::close_playback_session(&pool, &id, None).await;
        });
    }
}

enum Phase {
    /// Accumulating bytes from ffmpeg until we've seen the complete top-level
    /// `moov` box, which we then patch in-place before emitting.
    Buffering { buf: Vec<u8>, total_secs: Option<f64> },
    /// Moov already emitted (or we gave up) — raw pass-through.
    PassThrough,
}

/// Scan a byte buffer as a sequence of ISO-BMFF boxes and return the byte
/// range of the first top-level box whose type matches `fourcc`, if the
/// buffer contains the complete box. Returns `None` when the target box
/// hasn't appeared yet or is still being received.
fn find_top_level_box(buf: &[u8], fourcc: [u8; 4]) -> Option<std::ops::Range<usize>> {
    let mut p = 0;
    while p + 8 <= buf.len() {
        let size = u32::from_be_bytes(buf[p..p + 4].try_into().ok()?) as usize;
        let name: [u8; 4] = buf[p + 4..p + 8].try_into().ok()?;
        let (header_len, box_len) = if size == 1 {
            if p + 16 > buf.len() {
                return None;
            }
            let large = u64::from_be_bytes(buf[p + 8..p + 16].try_into().ok()?) as usize;
            (16, large)
        } else if size == 0 {
            // Size-0 means "to end of file" — not useful for in-stream seeking.
            return None;
        } else {
            (8, size)
        };
        if box_len < header_len {
            return None;
        }
        let end = p.checked_add(box_len)?;
        if end > buf.len() {
            return None;
        }
        if name == fourcc {
            return Some(p..end);
        }
        p = end;
    }
    None
}

/// Walk every box inside the moov payload (recursing into the known container
/// boxes) and call `visit` with each leaf box's fourcc and payload slice.
///
/// Collected-offset-then-mutate style: walking with a `&mut` closure while
/// handing out `&mut` slices to the same buffer fights the borrow checker.
/// Instead we collect (name, payload_range) pairs first, then mutate.
fn collect_leaf_boxes(
    buf: &[u8],
    range: std::ops::Range<usize>,
    out: &mut Vec<([u8; 4], std::ops::Range<usize>)>,
) {
    // These are the ISO-BMFF container boxes we need to descend into to reach
    // the duration-bearing leaves (mvhd, tkhd, mdhd, mehd).
    const CONTAINERS: &[&[u8; 4]] = &[b"moov", b"trak", b"mdia", b"mvex", b"edts", b"minf", b"stbl"];
    let mut p = range.start;
    while p + 8 <= range.end {
        let Ok(size_bytes) = buf[p..p + 4].try_into() else { return };
        let size = u32::from_be_bytes(size_bytes) as usize;
        let Ok(name_bytes) = buf[p + 4..p + 8].try_into() else { return };
        let name: [u8; 4] = name_bytes;
        let (header_len, box_len) = if size == 1 {
            if p + 16 > range.end {
                return;
            }
            let Ok(lb) = buf[p + 8..p + 16].try_into() else { return };
            (16, u64::from_be_bytes(lb) as usize)
        } else if size == 0 {
            (8, range.end - p)
        } else {
            (8, size)
        };
        if box_len < header_len || p + box_len > range.end {
            return;
        }
        let payload = (p + header_len)..(p + box_len);
        if CONTAINERS.iter().any(|c| **c == name) {
            collect_leaf_boxes(buf, payload, out);
        } else {
            out.push((name, payload));
        }
        p += box_len;
    }
}

/// Rewrite mvhd/tkhd/mdhd/mehd duration fields inside a buffered `moov` box
/// (including its 8-byte header) so they reflect `total_secs`. ffmpeg writes
/// these as 0 under `empty_moov` (or wrong under `delay_moov` + `-c:v copy`),
/// but all the boxes are fixed-width: we can patch durations in-place without
/// disturbing sizes or sibling offsets. Unknown/malformed shapes are skipped
/// silently — the stream still plays, just with the original (wrong) values.
fn patch_mp4_moov_durations(moov: &mut [u8], total_secs: f64) {
    if moov.len() < 8 || &moov[4..8] != b"moov" {
        return;
    }
    let mut leaves = Vec::new();
    collect_leaf_boxes(moov, 8..moov.len(), &mut leaves);

    // mvhd holds the movie timescale that mvhd/tkhd/mehd durations are
    // expressed in. Default to ffmpeg's MOV_TIMESCALE (1000) if somehow absent.
    let movie_timescale = leaves
        .iter()
        .find(|(n, _)| n == b"mvhd")
        .and_then(|(_, r)| read_fullbox_timescale(&moov[r.clone()], FullboxKind::Mvhd))
        .unwrap_or(1000);
    let movie_dur = (total_secs * movie_timescale as f64).round().max(0.0) as u64;

    for (name, range) in &leaves {
        let slice = &mut moov[range.clone()];
        match name {
            b"mvhd" => write_fullbox_duration(slice, FullboxKind::Mvhd, movie_dur),
            b"tkhd" => write_fullbox_duration(slice, FullboxKind::Tkhd, movie_dur),
            b"mehd" => write_fullbox_duration(slice, FullboxKind::Mehd, movie_dur),
            b"mdhd" => {
                // mdhd uses its own per-track timescale (not the movie's).
                if let Some(ts) = read_fullbox_timescale(slice, FullboxKind::Mdhd) {
                    let d = (total_secs * ts as f64).round().max(0.0) as u64;
                    write_fullbox_duration(slice, FullboxKind::Mdhd, d);
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
enum FullboxKind { Mvhd, Tkhd, Mdhd, Mehd }

/// Offsets within a fullbox payload (after the 4-byte version+flags prefix),
/// for both v0 (32-bit time fields) and v1 (64-bit time fields), to reach the
/// timescale and duration fields. `None` means the field doesn't exist for
/// that kind.
fn fullbox_layout(kind: FullboxKind, version: u8) -> (Option<usize>, Option<(usize, bool)>) {
    // Returns (timescale_offset, Some((duration_offset, is_64bit))).
    // All offsets are from the start of the fullbox payload (after version+flags).
    match (kind, version) {
        // mvhd v0: creation(4) mod(4) timescale(4) duration(4) ...
        (FullboxKind::Mvhd, 0) => (Some(4 + 4 + 4), Some((4 + 4 + 4 + 4, false))),
        // mvhd v1: creation(8) mod(8) timescale(4) duration(8) ...
        (FullboxKind::Mvhd, _) => (Some(8 + 8), Some((8 + 8 + 4, true))),
        // tkhd v0: creation(4) mod(4) track_id(4) reserved(4) duration(4) ...
        (FullboxKind::Tkhd, 0) => (None, Some((4 + 4 + 4 + 4, false))),
        // tkhd v1: creation(8) mod(8) track_id(4) reserved(4) duration(8) ...
        (FullboxKind::Tkhd, _) => (None, Some((8 + 8 + 4 + 4, true))),
        // mdhd v0: creation(4) mod(4) timescale(4) duration(4) ...
        (FullboxKind::Mdhd, 0) => (Some(4 + 4), Some((4 + 4 + 4, false))),
        // mdhd v1: creation(8) mod(8) timescale(4) duration(8) ...
        (FullboxKind::Mdhd, _) => (Some(8 + 8), Some((8 + 8 + 4, true))),
        // mehd v0: fragment_duration(4) ; v1: fragment_duration(8)
        (FullboxKind::Mehd, 0) => (None, Some((0, false))),
        (FullboxKind::Mehd, _) => (None, Some((0, true))),
    }
}

fn read_fullbox_timescale(payload: &[u8], kind: FullboxKind) -> Option<u32> {
    if payload.len() < 4 {
        return None;
    }
    let version = payload[0];
    let (ts, _) = fullbox_layout(kind, version);
    let off = 4 + ts?;
    let slice = payload.get(off..off + 4)?;
    Some(u32::from_be_bytes(slice.try_into().ok()?))
}

fn write_fullbox_duration(payload: &mut [u8], kind: FullboxKind, duration: u64) {
    if payload.len() < 4 {
        return;
    }
    let version = payload[0];
    let (_, dur) = fullbox_layout(kind, version);
    let Some((dur_off, is_64)) = dur else { return };
    let off = 4 + dur_off;
    if is_64 {
        if let Some(slot) = payload.get_mut(off..off + 8) {
            slot.copy_from_slice(&duration.to_be_bytes());
        }
    } else {
        let d32: u32 = duration.try_into().unwrap_or(u32::MAX);
        if let Some(slot) = payload.get_mut(off..off + 4) {
            slot.copy_from_slice(&d32.to_be_bytes());
        }
    }
}
