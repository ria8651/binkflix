//! Seek-aware ffmpeg producer.
//!
//! One `Producer` per active media. ffmpeg launches with a fast input
//! `-ss` near the user's target so it lands close to the right point
//! without scanning the file linearly (MKV cluster index lookup,
//! sub-millisecond regardless of file length).
//!
//! ## Pipeline
//!
//! ```text
//!   client GET seg-N.m4s
//!        │
//!        ▼
//!   ensure_segment ──► slot mutex ──► (cache hit? → serve)
//!        │                          (out of range? → kill old, launch new)
//!        │                          (in range? → bump high_water + wait)
//!        ▼
//!   producer ffmpeg writes seg-{i:05}.m4s into a per-run scratch dir
//!        │
//!        ▼
//!   watcher renames into canonical plan_dir, first-write-wins
//!        │
//!        ▼
//!   wait_for_file(canonical) → serve
//! ```
//!
//! ## Why filename-based scratch→canonical mapping (not content-based)
//!
//! ffmpeg's HLS-fmp4 muxer cuts at `first kf with pts - start_pts ≥
//! N×hls_time`. On a seek-restart, `start_pts` = the cluster-landing
//! PTS (whatever keyframe ≤ -ss target was indexed in the source), so
//! cuts land at `start_pts + 6, start_pts + 12, …` — **not** at the
//! plan's absolute "first kf ≥ N×6" boundaries. Earlier versions tried
//! to classify scratch segments by sidx.earliest_presentation_time and
//! match them to plan boundaries within a 0.5s tolerance; this rejected
//! ~all of them on seek-restart and produced a sparse-island cache.
//!
//! Instead we trust the scratch FILENAME. ffmpeg writes
//! `seg-{i:05}.m4s` with `i` starting at `-start_number = start_idx`,
//! so scratch indices already are plan indices. With
//! `-hls_segment_options movflags=+frag_discont` (the Jellyfin trick),
//! each segment's `tfdt` is the source-absolute sample DTS, so the
//! player aligns playback by media time. The playlist's EXTINF can
//! drift from the actual segment span by a fraction of a second at
//! seek-restart boundaries, which both hls.js and Safari tolerate.
//!
//! ## Pre-roll
//!
//! The first output segment of any run carries ~1s of audio-encoder
//! priming (or, for `-c:a copy`, just the `-ss` cluster-landing
//! offset). We pre-roll input `-ss` by one plan-segment so that
//! priming lands inside a throwaway segment the watcher discards.
//!
//! ## Lifecycle
//!
//! * **Start**: any segment request to a media without a running
//!   producer spawns one targeted at that segment.
//! * **In range**: requests within `[start_idx, head + LOOKAHEAD]` just
//!   bump high_water and wait for ffmpeg to catch up.
//! * **Out of range** (seek backward, or far seek forward): kill the
//!   current run and relaunch at the new target.
//! * **Backpressure (pull-driven)**: ffmpeg is SIGSTOP'd whenever
//!   `head ≥ target_head`. `target_head` only advances when a request
//!   arrives — each request bumps it to `max(target_head, idx +
//!   LOOKAHEAD_BUFFER)`. So if the client stops fetching (e.g. its
//!   buffer is full, or MSE got detached by a strict CSP), the
//!   producer stalls within ≤LOOKAHEAD_BUFFER segments instead of
//!   racing to EOF.
//! * **Idle**: 30s of no requests reaps the producer entirely.

use super::cache;
use super::hwenc::HwEncoder;
use super::plan::{AudioPlan, Mode, StreamPlan};
use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;

/// How many segments past the most recently requested one ffmpeg is
/// allowed to read ahead before SIGSTOP. Small enough that a stalled
/// client (broken MSE, paused playback, etc.) doesn't waste CPU; large
/// enough that sequential hls.js fetches don't ping-pong the producer
/// stop/start on every segment.
const LOOKAHEAD_BUFFER: u32 = 3;

/// How many segments past `head` a request can target before we abandon
/// the current run and relaunch at the new target. A normal hls.js
/// playback never asks more than `LOOKAHEAD_BUFFER` ahead, so this only
/// trips on real seeks across a wide gap — at which point a fresh fast
/// input seek beats sequential decode.
const LOOKAHEAD_WINDOW: u32 = 8;

/// Pre-roll: how many plan-segments before the user's target ffmpeg
/// actually starts at. Absorbs first-segment priming/cluster-landing
/// offset; the watcher discards anything below the user's target.
const PREROLL_SEGMENTS: u32 = 1;

const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const SEGMENT_WAIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Registry key: `(media_id, audio_idx, mode_tag)`. Different audio tracks
/// AND different encode modes/bitrates run as independent producers because
/// each writes a different segment cache dir; killing one to start another
/// would destroy any other client's playback on the same file.
type ProducerKey = (String, u32, String);

#[derive(Default)]
pub struct ProducerRegistry {
    by_media: DashMap<ProducerKey, Arc<Mutex<Option<ProducerHandle>>>>,
}

impl ProducerRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn slot(&self, media_id: &str, audio_idx: u32, mode_tag: &str) -> Arc<Mutex<Option<ProducerHandle>>> {
        self.by_media
            .entry((media_id.to_string(), audio_idx, mode_tag.to_string()))
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    }

    pub async fn snapshot(&self, media_id: &str, audio_idx: u32, mode_tag: &str) -> Option<crate::types::HlsProducerState> {
        let slot = self.by_media.get(&(media_id.to_string(), audio_idx, mode_tag.to_string())).map(|e| e.clone())?;
        let guard = slot.lock().await;
        let h = guard.as_ref()?;
        let start_idx = h.start_idx;
        let head = h.head.load(Ordering::Acquire);
        let target_head = h.target_head.load(Ordering::Acquire);
        let paused = h.paused.load(Ordering::Acquire);
        let idle_for_secs = h.last_request_at.read().await.elapsed().as_secs_f64();
        Some(crate::types::HlsProducerState {
            start_idx,
            head,
            target_head,
            paused,
            idle_for_secs,
            lookahead_buffer: LOOKAHEAD_BUFFER,
            lookahead_window: LOOKAHEAD_WINDOW,
        })
    }
}

pub struct ProducerHandle {
    /// First plan idx this run *promotes* to canonical (= the user's
    /// seek target). ffmpeg's `-start_number` is set a bit earlier
    /// (`-PREROLL_SEGMENTS`) so the first encoded segment, which carries
    /// the cluster-landing/audio-priming offset, can be discarded; that
    /// pre-roll segment never lands in canonical, so `head` is tracked
    /// relative to `start_idx` (not seg 1) — otherwise the unfilled
    /// pre-roll gap pins `head` at `start_idx-1` forever and
    /// backpressure never engages on seek-from-cold-cache.
    pub start_idx: u32,
    /// Highest segment such that all of `[start_idx ..= head]` exist
    /// in canonical. Advanced by the watcher.
    pub head: Arc<AtomicU32>,
    /// Highest segment ffmpeg is allowed to advance to in this pull.
    /// Bumped by `ensure_segment` on each request to `idx +
    /// LOOKAHEAD_BUFFER`; ffmpeg is SIGSTOP'd whenever
    /// `head ≥ target_head`. Server defends itself instead of trusting
    /// the client to push back.
    pub target_head: Arc<AtomicU32>,
    pub paused: Arc<AtomicBool>,
    pub last_request_at: Arc<RwLock<Instant>>,
    pub child: Child,
    /// Per-run scratch dir. ffmpeg writes here; watcher promotes to
    /// canonical (only for segments at-or-after target_idx, only if
    /// canonical is empty — first-write-wins). Held only for its
    /// `Drop` side-effect, which removes the directory on producer
    /// shutdown (or panic). Co-located under `plan_dir` because the
    /// watcher promotes via `tokio::fs::hard_link`, which can't cross
    /// filesystems.
    _run_dir: tempfile::TempDir,
    tasks: Vec<JoinHandle<()>>,
}

impl ProducerHandle {
    async fn shutdown(mut self) {
        for t in &self.tasks {
            t.abort();
        }
        // SIGCONT first in case the child is currently SIGSTOP'd by
        // backpressure — a stopped process can still receive SIGKILL,
        // but resuming it first lets the kernel clean up cleanly.
        if let Some(pid) = self.child.id() {
            let _ = signal_resume(pid);
        }
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        // `self._run_dir` (TempDir) is dropped at end-of-scope, which
        // removes the scratch dir synchronously. A handful of small
        // segments — fine to do inline rather than via spawn_blocking.
    }
}

#[derive(Clone)]
pub struct ProducerCtx {
    pub media_id: String,
    pub source: PathBuf,
    pub plan: Arc<StreamPlan>,
    pub plan_dir: PathBuf,
    pub audio_idx: u32,
    /// Cache key for the registry — the same `(media_id, audio_idx)`
    /// running with different `mode_tag`s are independent producers so a
    /// remux client and a transcode client can coexist without one
    /// killing the other's ffmpeg.
    pub mode_tag: String,
    /// `None` when the requested audio index doesn't exist on the source.
    /// ffmpeg gets `-map 0:a:N?` either way; the optional flag just means
    /// "no audio in the output" if the stream is missing.
    pub audio: Option<AudioPlan>,
    /// H.264 hardware encoder picked at server startup. Note that the
    /// *effective* encoder may degrade to `None` mid-process if the
    /// sticky fallback flag has been set by a prior failed launch — see
    /// [`effective_hw`].
    pub hw: HwEncoder,
}

/// Process-wide sticky flag: once a hwenc producer fails to start, every
/// subsequent producer (this one and future) uses libx264. Cheaper to
/// reason about than per-media retry state, and a hwenc that fails once
/// will keep failing for the same reason (missing driver, busted device,
/// codec rejection by the kernel module).
static HW_DISABLED: AtomicBool = AtomicBool::new(false);

fn effective_hw(ctx_hw: HwEncoder) -> HwEncoder {
    if HW_DISABLED.load(Ordering::Acquire) {
        HwEncoder::None
    } else {
        ctx_hw
    }
}

/// What the m3u8 endpoint should advertise as `X-Stream-Encoder` for a
/// given context — accounts for the sticky fallback flag.
pub fn current_encoder_name(ctx_hw: HwEncoder) -> &'static str {
    effective_hw(ctx_hw).ffmpeg_name()
}

pub async fn ensure_segment(
    registry: &ProducerRegistry,
    ctx: &ProducerCtx,
    idx: u32,
) -> anyhow::Result<PathBuf> {
    if idx == 0 || idx as usize > ctx.plan.segments.len() {
        anyhow::bail!("segment index {idx} out of range");
    }
    let total = ctx.plan.segments.len() as u32;
    let seg_path = ctx.plan_dir.join(cache::segment_filename(idx));

    // Fast path: cached on disk. Still bump target_head so a running
    // producer keeps its read-ahead window aligned with where the
    // client actually is.
    if tokio::fs::try_exists(&seg_path).await.unwrap_or(false) {
        bump_target_head(registry, &ctx.media_id, ctx.audio_idx, &ctx.mode_tag, idx, total).await;
        return Ok(seg_path);
    }

    // Slot mutex serialises start/restart so concurrent seek-back-to-back
    // can't fire two ffmpegs on the same (media, audio_idx, mode).
    let slot = registry.slot(&ctx.media_id, ctx.audio_idx, &ctx.mode_tag);
    {
        let mut guard = slot.lock().await;
        let needs_restart = match guard.as_ref() {
            Some(h) => {
                let head = h.head.load(Ordering::Acquire);
                idx < h.start_idx || idx > head.saturating_add(LOOKAHEAD_WINDOW)
            }
            None => true,
        };
        if needs_restart {
            if let Some(old) = guard.take() {
                old.shutdown().await;
            }
            *guard = Some(launch_producer(ctx.clone(), idx, slot.clone()).await?);
            // Drop the slot lock before sweeping siblings — they take
            // their own slot locks, and holding two at once invites
            // deadlock if a concurrent request races on a sibling.
            drop(guard);
            shutdown_siblings(registry, &ctx.media_id, ctx.audio_idx, &ctx.mode_tag).await;
        } else {
            let h = guard.as_mut().expect("checked above");
            let new_target = idx.saturating_add(LOOKAHEAD_BUFFER).min(total);
            h.target_head.fetch_max(new_target, Ordering::AcqRel);
            *h.last_request_at.write().await = Instant::now();
            if h.paused.load(Ordering::Acquire) {
                if let Some(pid) = h.child.id() {
                    let _ = signal_resume(pid);
                    h.paused.store(false, Ordering::Release);
                }
            }
        }
    }

    wait_for_file(&seg_path, SEGMENT_WAIT_TIMEOUT).await?;
    Ok(seg_path)
}

/// Kill any other producer for the same `media_id` whose `(audio_idx,
/// mode_tag)` differs from the active one. Called when a fresh
/// producer launches: switching mode (Remux ↔ Transcode), bitrate, or
/// audio track from the player UI is the same user replacing their
/// previous pull, so the old ffmpeg shouldn't keep burning CPU until
/// its 30s idle reaper trips. A second user simultaneously playing
/// the same file in a different mode is rare on a self-hosted setup,
/// and they'd recover automatically — `ensure_segment` just relaunches
/// their producer on their next segment fetch.
async fn shutdown_siblings(
    registry: &ProducerRegistry,
    media_id: &str,
    keep_audio_idx: u32,
    keep_mode_tag: &str,
) {
    let siblings: Vec<(ProducerKey, Arc<Mutex<Option<ProducerHandle>>>)> = registry
        .by_media
        .iter()
        .filter_map(|entry| {
            let (mid, aidx, tag) = entry.key();
            if mid != media_id {
                return None;
            }
            if *aidx == keep_audio_idx && tag == keep_mode_tag {
                return None;
            }
            Some((entry.key().clone(), entry.value().clone()))
        })
        .collect();
    for (key, slot) in siblings {
        let mut guard = slot.lock().await;
        if let Some(old) = guard.take() {
            tracing::info!(media = %media_id, audio = key.1, mode = %key.2, "shutting down sibling producer");
            old.shutdown().await;
        }
    }
}

async fn bump_target_head(registry: &ProducerRegistry, media_id: &str, audio_idx: u32, mode_tag: &str, idx: u32, total: u32) {
    if let Some(slot) = registry.by_media.get(&(media_id.to_string(), audio_idx, mode_tag.to_string())).map(|e| e.clone()) {
        let guard = slot.lock().await;
        if let Some(h) = guard.as_ref() {
            let new_target = idx.saturating_add(LOOKAHEAD_BUFFER).min(total);
            h.target_head.fetch_max(new_target, Ordering::AcqRel);
            *h.last_request_at.write().await = Instant::now();
        }
    }
}

async fn wait_for_file(path: &Path, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if tokio::fs::try_exists(path).await.unwrap_or(false) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for {}", path.display());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn launch_producer(
    ctx: ProducerCtx,
    target_idx: u32,
    slot: Arc<Mutex<Option<ProducerHandle>>>,
) -> anyhow::Result<ProducerHandle> {
    // Retry loop for the hw-encoder runtime fallback. We try with the
    // resolved hw encoder; if the ffmpeg child dies within 750ms (the
    // signature of a driver/kernel-module rejection — by the time
    // surfaces are negotiated the encoder has either claimed the device
    // or thrown), we set the process-wide `HW_DISABLED` flag, log, and
    // retry once with libx264. Software won't trip the same path so the
    // loop is at most two iterations.
    loop {
        let active_hw = effective_hw(ctx.hw);
        let mut handle = launch_once(ctx.clone(), target_idx, slot.clone()).await?;
        if active_hw == HwEncoder::None {
            return Ok(handle);
        }
        match wait_for_early_exit(&mut handle.child, Duration::from_millis(750)).await {
            EarlyExit::Alive => return Ok(handle),
            EarlyExit::Exited(status) => {
                tracing::warn!(
                    media = %ctx.media_id,
                    encoder = active_hw.ffmpeg_name(),
                    ?status,
                    "hwenc producer exited during startup; falling back to libx264 process-wide"
                );
                HW_DISABLED.store(true, Ordering::Release);
                handle.shutdown().await;
                // Loop body re-resolves `effective_hw(ctx.hw)`, which
                // now returns `None` because of the flag we just set.
            }
        }
    }
}

enum EarlyExit {
    Alive,
    Exited(std::process::ExitStatus),
}

async fn wait_for_early_exit(child: &mut Child, total: Duration) -> EarlyExit {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return EarlyExit::Exited(status),
            Ok(None) => tokio::time::sleep(Duration::from_millis(50)).await,
            // `try_wait` errors are basically "the OS lost the child" —
            // treat like alive so we don't trigger a fallback over an
            // unrelated kernel hiccup.
            Err(_) => return EarlyExit::Alive,
        }
    }
    EarlyExit::Alive
}

async fn launch_once(
    ctx: ProducerCtx,
    target_idx: u32,
    slot: Arc<Mutex<Option<ProducerHandle>>>,
) -> anyhow::Result<ProducerHandle> {
    tokio::fs::create_dir_all(&ctx.plan_dir).await?;

    // ffmpeg starts a bit before target_idx so the first encoded
    // segment (priming gap) can be discarded; canonical output begins
    // at target_idx.
    let ff_start_idx = target_idx.saturating_sub(PREROLL_SEGMENTS).max(1);

    // Per-run scratch dir. Each run gets its own folder so concurrent
    // promotions can hard-link into canonical without colliding. The
    // tempfile-generated random suffix makes back-to-back launches
    // collision-free; Drop removes the dir on producer shutdown (or
    // crash). Parent stays `plan_dir` because the watcher hard-links
    // segments into the canonical paths and hard-links can't cross
    // filesystems.
    let run_dir = tempfile::Builder::new()
        .prefix("_run-")
        .tempdir_in(&ctx.plan_dir)?;

    let seg = ctx
        .plan
        .segments
        .get((ff_start_idx as usize).saturating_sub(1))
        .ok_or_else(|| anyhow::anyhow!("ff_start_idx {ff_start_idx} out of plan range"))?;
    let start_t = seg.t;

    tracing::info!(
        media = %ctx.media_id,
        target_idx, ff_start_idx, start_t,
        "launching producer ffmpeg"
    );
    let (mut child, argv) = spawn_ffmpeg(&ctx, ff_start_idx, start_t, run_dir.path())?;

    // Persist the exact ffmpeg invocation per plan so a future failure
    // can be diagnosed without scraping container logs — "ask the user
    // to send me <plan_dir>/ffmpeg.cmd and ffmpeg.log".
    let cmd_path = ctx.plan_dir.join("ffmpeg.cmd");
    if let Err(e) = tokio::fs::write(&cmd_path, format_argv(&argv)).await {
        tracing::warn!(error = %e, path = %cmd_path.display(), "failed to write ffmpeg.cmd");
    }

    if let Some(stderr) = child.stderr.take() {
        let id = ctx.media_id.clone();
        let log_path = ctx.plan_dir.join("ffmpeg.log");
        tokio::spawn(async move {
            // Truncate per run — last run wins. The watcher already
            // delivers the previous run's segments to canonical before
            // a new producer launches, so the only consumer of
            // ffmpeg.log is the *current* run's diagnostics.
            let mut log = match tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&log_path)
                .await
            {
                Ok(f) => Some(BufWriter::new(f)),
                Err(e) => {
                    tracing::warn!(error = %e, path = %log_path.display(), "failed to open ffmpeg.log");
                    None
                }
            };
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "binkflix::hls::ffmpeg", media = %id, "{line}");
                if let Some(w) = log.as_mut() {
                    if w.write_all(line.as_bytes()).await.is_err()
                        || w.write_all(b"\n").await.is_err()
                    {
                        log = None;
                    }
                }
            }
            if let Some(mut w) = log {
                let _ = w.flush().await;
            }
        });
    }

    let total_segments = ctx.plan.segments.len() as u32;
    let head = Arc::new(AtomicU32::new(target_idx.saturating_sub(1)));
    // Initial pull window: serve the requested segment plus a small
    // read-ahead. ffmpeg will produce up to here and then SIGSTOP until
    // another request bumps target_head further.
    let initial_target = target_idx
        .saturating_add(LOOKAHEAD_BUFFER)
        .min(total_segments);
    let target_head = Arc::new(AtomicU32::new(initial_target));
    let paused = Arc::new(AtomicBool::new(false));
    let last_request_at = Arc::new(RwLock::new(Instant::now()));

    let watcher = spawn_watcher(
        ctx.plan_dir.clone(),
        run_dir.path().to_path_buf(),
        head.clone(),
        total_segments,
        target_idx,
    );
    let reaper = spawn_reaper(
        ctx.media_id.clone(),
        slot,
        head.clone(),
        target_head.clone(),
        paused.clone(),
        last_request_at.clone(),
    );

    Ok(ProducerHandle {
        start_idx: target_idx,
        head,
        target_head,
        paused,
        last_request_at,
        child,
        _run_dir: run_dir,
        tasks: vec![watcher, reaper],
    })
}

fn spawn_ffmpeg(
    ctx: &ProducerCtx,
    start_idx: u32,
    start_t: f64,
    run_dir: &Path,
) -> anyhow::Result<(Child, Vec<String>)> {
    let hw = effective_hw(ctx.hw);
    // Common preamble + HLS muxer flags. Codec args (video copy vs
    // libx264) come from `apply_video_args` based on plan mode. Key
    // shared pieces:
    //
    //  * `-ss <start_t>` before `-i`: fast demuxer-index seek, lands at
    //    nearest cluster ≤ start_t, sub-millisecond.
    //  * `-copyts -avoid_negative_ts disabled`: preserve source PTS
    //    through the pipeline, don't shift either track.
    //  * `-hls_segment_options movflags=+frag_discont`: THE flag (from
    //    Jellyfin's `DynamicHlsController.cs`). Without it the
    //    HLS-fmp4 muxer normalises each fragment's tfdt to zero, which
    //    breaks A/V sync on seek-restart.
    //  * `-hls_segment_filename seg-%05d.m4s -start_number start_idx`:
    //    scratch filenames already encode plan indices; the watcher
    //    renames straight across.
    let mut cmd = Command::new("ffmpeg");
    cmd.current_dir(run_dir)
        .arg("-hide_banner")
        .arg("-loglevel").arg("warning")
        .arg("-nostdin");
    // VAAPI/QSV need a `-init_hw_device` + `-filter_hw_device` pair before
    // `-i` so the encoder and the `hwupload` filter share a device. Pure
    // VideoToolbox doesn't need any device init since the encoder owns
    // its own session; we just keep the software input + sw scale and
    // let h264_videotoolbox handle the upload internally.
    if matches!(ctx.plan.mode, Mode::Transcode { .. }) {
        match hw {
            HwEncoder::Vaapi => {
                cmd.arg("-init_hw_device")
                    .arg("vaapi=va:/dev/dri/renderD128")
                    .arg("-filter_hw_device").arg("va");
            }
            HwEncoder::Qsv => {
                cmd.arg("-init_hw_device")
                    .arg("qsv=qsv:hw_any")
                    .arg("-filter_hw_device").arg("qsv");
            }
            _ => {}
        }
    }
    cmd
        // Restrict to local file inputs only — see media_info.rs.
        .arg("-protocol_whitelist").arg("file")
        // Generous probe defaults: matroska sources with many streams
        // (multi-audio, fonts, attachments) can need >5MB to resolve all
        // codec parameters. ffmpeg's default warning ("Consider
        // increasing analyzeduration / probesize") shows up routinely;
        // bump both so the input demuxer has stable codec params before
        // the output muxer starts writing init.mp4.
        .arg("-analyzeduration").arg("10M")
        .arg("-probesize").arg("50M")
        .arg("-ss").arg(format!("{start_t:.6}"))
        .arg("-copyts")
        .arg("-i").arg(&ctx.source)
        .arg("-map").arg("0:v:0")
        .arg("-map").arg(format!("0:a:{}?", ctx.audio_idx))
        .arg("-avoid_negative_ts").arg("disabled");
    apply_video_args(&mut cmd, &ctx.plan.mode, hw);
    apply_audio_args(&mut cmd, ctx.audio.as_ref(), &ctx.plan.mode);
    cmd.arg("-sn").arg("-dn")
        .arg("-f").arg("hls")
        .arg("-hls_time").arg("6")
        .arg("-hls_playlist_type").arg("vod")
        .arg("-hls_segment_type").arg("fmp4")
        .arg("-hls_flags").arg("independent_segments+program_date_time")
        .arg("-hls_segment_options").arg("movflags=+frag_discont")
        .arg("-hls_fmp4_init_filename").arg("init.mp4")
        .arg("-hls_segment_filename").arg("seg-%05d.m4s")
        .arg("-start_number").arg(start_idx.to_string())
        .arg("-hls_list_size").arg("0")
        .arg("_run.m3u8")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    #[cfg(unix)]
    setup_pdeath_unix(&mut cmd);

    let argv = collect_argv(&cmd);
    let child = cmd.spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn ffmpeg: {e}"))?;
    Ok((child, argv))
}

/// Snapshot the program + args of a built `Command` as plain strings
/// (lossy on non-UTF-8) so we can persist them next to the run for
/// post-mortem inspection.
fn collect_argv(cmd: &Command) -> Vec<String> {
    let std_cmd = cmd.as_std();
    let mut argv = Vec::with_capacity(1 + std_cmd.get_args().len());
    argv.push(std_cmd.get_program().to_string_lossy().into_owned());
    for a in std_cmd.get_args() {
        argv.push(a.to_string_lossy().into_owned());
    }
    argv
}

/// Render argv as a single shell-friendly line. Args containing spaces
/// or shell metacharacters get single-quoted; embedded single quotes
/// become `'\''`. Output is meant for human-readable diagnosis (paste
/// into a terminal), not for re-execution by another tool.
fn format_argv(argv: &[String]) -> String {
    let mut out = String::new();
    for (i, a) in argv.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if a.is_empty() || a.chars().any(|c| matches!(c, ' ' | '\t' | '\n' | '\'' | '"' | '\\' | '$' | '`' | '*' | '?' | '[' | ']' | '(' | ')' | '<' | '>' | '|' | '&' | ';' | '#' | '!')) {
            out.push('\'');
            for ch in a.chars() {
                if ch == '\'' {
                    out.push_str("'\\''");
                } else {
                    out.push(ch);
                }
            }
            out.push('\'');
        } else {
            out.push_str(a);
        }
    }
    out.push('\n');
    out
}

fn apply_video_args(cmd: &mut Command, mode: &Mode, hw: HwEncoder) {
    match mode {
        Mode::Remux => {
            cmd.arg("-c:v").arg("copy");
        }
        Mode::Transcode { bitrate_kbps, max_height } => {
            // `scale=-2:'min(H,ih)'` keeps source aspect, never
            // upscales, and the `-2` rounds width to the nearest even
            // multiple (libx264 + yuv420p require even dimensions).
            // For libx264 / videotoolbox we keep `format=yuv420p` so
            // the 10-bit→8-bit conversion runs *inside* the filter
            // graph rather than relying on `-pix_fmt`'s implicit
            // auto-insertion. For VAAPI/QSV the HW encoder needs nv12
            // and the surface uploaded to the device, hence
            // `format=nv12,hwupload` instead.
            let vf = match hw {
                HwEncoder::Vaapi => format!(
                    "scale=-2:'min({max_height},ih)':flags=lanczos,format=nv12,hwupload"
                ),
                HwEncoder::Qsv => format!(
                    "scale=-2:'min({max_height},ih)':flags=lanczos,format=nv12,hwupload=extra_hw_frames=64"
                ),
                _ => format!(
                    "scale=-2:'min({max_height},ih)':flags=lanczos,format=yuv420p"
                ),
            };
            let maxrate = bitrate_kbps.saturating_mul(15) / 10; // 1.5×
            let bufsize = bitrate_kbps.saturating_mul(2);

            cmd.arg("-vf").arg(vf).arg("-c:v").arg(hw.ffmpeg_name());

            // Per-encoder knobs. VAAPI/QSV reject `-pix_fmt`/`-preset`
            // (they get pixfmt from the input HW frame) and use a
            // numeric `-level 41` form. VideoToolbox needs `-allow_sw 1`
            // so it gracefully handles formats the GPU can't take and
            // `-realtime 1` to keep latency in the segment-budget
            // ballpark.
            match hw {
                HwEncoder::None => {
                    cmd.arg("-preset").arg("veryfast")
                        .arg("-profile:v").arg("high")
                        .arg("-level").arg("4.1")
                        .arg("-pix_fmt").arg("yuv420p");
                }
                HwEncoder::VideoToolbox => {
                    cmd.arg("-profile:v").arg("high")
                        .arg("-level").arg("4.1")
                        .arg("-allow_sw").arg("1")
                        .arg("-realtime").arg("1");
                }
                HwEncoder::Vaapi => {
                    cmd.arg("-profile:v").arg("high")
                        .arg("-level").arg("41");
                }
                HwEncoder::Qsv => {
                    cmd.arg("-preset").arg("veryfast")
                        .arg("-profile:v").arg("high")
                        .arg("-level").arg("41");
                }
            }

            // Bitrate ladder + IDR-on-segment-boundary work for every
            // backend. `force_key_frames "expr:gte(t,n_forced*6)"`
            // makes ffmpeg insert IDRs exactly at our 6s segment
            // boundaries so each produced segment is independently
            // decodable — which is what `independent_segments`
            // advertises in the playlist.
            cmd.arg("-b:v").arg(format!("{bitrate_kbps}k"))
                .arg("-maxrate").arg(format!("{maxrate}k"))
                .arg("-bufsize").arg(format!("{bufsize}k"))
                .arg("-force_key_frames").arg("expr:gte(t,n_forced*6)");
        }
    }
}

fn apply_audio_args(cmd: &mut Command, audio: Option<&AudioPlan>, mode: &Mode) {
    // No audio plan = no source stream at this index. ffmpeg's
    // `-map 0:a:N?` already silently drops the output, so we just don't
    // pass any `-c:a`/`-b:a` flags and let ffmpeg produce a video-only
    // output. (This also covers files with no audio at all.)
    let Some(audio) = audio else { return };
    // For Transcode we always re-encode audio to stereo AAC: the rest of
    // the pipeline is already CPU-bound on libx264, and a uniform output
    // codec sidesteps the "source AAC has weird channel layout that
    // browsers won't decode" footgun on a path users only hit when
    // remux already wasn't viable.
    let force_aac = matches!(mode, Mode::Transcode { .. });
    if !force_aac && audio.out_codec == "copy" {
        cmd.arg("-c:a").arg("copy");
    } else {
        cmd.arg("-c:a").arg("aac")
            .arg("-ac").arg(audio.channels.to_string())
            .arg("-b:a").arg(format!("{}k", audio.bitrate_kbps));
    }
}

#[cfg(unix)]
fn setup_pdeath_unix(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: closure runs in the child between fork and exec. Only
    // async-signal-safe libc calls. setpgid puts ffmpeg in its own
    // group; on Linux PR_SET_PDEATHSIG kills it when the parent dies
    // (no equivalent on macOS, but the startup sweep + explicit kill
    // on shutdown cover most orphan cases).
    unsafe {
        cmd.pre_exec(|| {
            let _ = libc::setpgid(0, 0);
            #[cfg(target_os = "linux")]
            {
                libc::prctl(1 /* PR_SET_PDEATHSIG */, libc::SIGKILL, 0, 0, 0);
            }
            Ok(())
        });
    }
}

/// Watcher: scan the run dir, promote each `seg-NNNNN.m4s` (idx ≥
/// `target_idx`) into canonical when we can prove it's complete on
/// disk. Indices below `target_idx` are pre-roll throwaways and get
/// deleted. Also promotes init.mp4 the first time it appears.
///
/// **Why the completeness gate.** ffmpeg writes each segment as
/// `fopen → fwrite × N → fclose`. SIGSTOP from backpressure can
/// freeze the process mid-write, leaving the file on disk
/// truncated. A naive `try_exists`-and-link path would then promote
/// the partial bytes into canonical, and hls.js's MSE append would
/// throw InvalidStateError — surfacing as `bufferAppendError`.
///
/// We accept a segment when either:
///
///  1. **Next-exists**: ffmpeg has already moved on to a
///     higher-indexed scratch file, which can only happen if it
///     closed the current one. Fast path; covers mid-stream
///     producer running normally.
///  2. **Stable + structurally complete**: file size hasn't changed
///     since the previous tick *and* its top-level mp4 box layout
///     walks cleanly to EOF. Covers the last segment of a run
///     (natural EOF, no successor will ever appear) and the
///     between-segments pause case (ffmpeg stopped between cuts —
///     file is fully written, just the next one hasn't started).
fn spawn_watcher(
    plan_dir: PathBuf,
    run_dir: PathBuf,
    head: Arc<AtomicU32>,
    total_segments: u32,
    target_idx: u32,
) -> JoinHandle<()> {
    use std::collections::{BTreeMap, HashMap};
    let canonical_init = plan_dir.join("init.mp4");
    let scratch_init = run_dir.join("init.mp4");
    let mut prev_sizes: HashMap<u32, u64> = HashMap::new();
    tokio::spawn(async move {
        loop {
            // Snapshot the scratch dir: idx → (path, size).
            let mut scratch: BTreeMap<u32, (PathBuf, u64)> = BTreeMap::new();
            if let Ok(mut rd) = tokio::fs::read_dir(&run_dir).await {
                while let Ok(Some(entry)) = rd.next_entry().await {
                    let name = entry.file_name();
                    let Some(name_str) = name.to_str() else { continue };
                    let Some(idx) = cache::segment_index(name_str) else { continue };
                    let Ok(meta) = entry.metadata().await else { continue };
                    scratch.insert(idx, (entry.path(), meta.len()));
                }
            }

            // init.mp4 is byte-identical across runs for a given plan,
            // so first-write-wins is fine. ffmpeg's HLS-fmp4 muxer
            // writes init.mp4 in order: open, write moov, close, then
            // start the first segment. So the existence of *any* scratch
            // `seg-*.m4s` proves init.mp4 has been closed and is safe
            // to promote — without this gate the watcher could copy a
            // mid-write file (DEMUXER_ERROR_COULD_NOT_PARSE on the
            // client side).
            if !tokio::fs::try_exists(&canonical_init).await.unwrap_or(false)
                && tokio::fs::try_exists(&scratch_init).await.unwrap_or(false)
                && !scratch.is_empty()
            {
                if let Err(e) = atomic_link_or_copy(&scratch_init, &canonical_init).await {
                    tracing::warn!(error = %e, "failed to promote init.mp4");
                }
            }

            for (&idx, (path, size)) in &scratch {
                if idx == 0 || idx > total_segments || idx < target_idx {
                    let _ = tokio::fs::remove_file(path).await;
                    continue;
                }
                let canonical = plan_dir.join(cache::segment_filename(idx));
                if tokio::fs::try_exists(&canonical).await.unwrap_or(false) {
                    let _ = tokio::fs::remove_file(path).await;
                    continue;
                }

                let safe = if scratch.contains_key(&(idx + 1)) {
                    true
                } else if prev_sizes.get(&idx) == Some(size) {
                    segment_is_complete(path).await
                } else {
                    false
                };
                if !safe {
                    continue;
                }

                if let Err(e) = atomic_link_or_copy(path, &canonical).await {
                    tracing::warn!(
                        scratch = %path.display(),
                        target = %canonical.display(),
                        error = %e,
                        "failed to promote segment"
                    );
                }
            }

            // Refresh stability tracking for the next tick. Drop
            // entries for files that no longer exist (already
            // promoted or pre-roll-discarded).
            prev_sizes = scratch.iter().map(|(i, (_, s))| (*i, *s)).collect();

            // Recompute head as the highest segment such that
            // [target_idx ..= head] are all in canonical. Anchored
            // at the run's first canonical output (not seg 1) so the
            // pre-roll's intentional gap below `target_idx` doesn't
            // pin head and starve backpressure.
            let cur = head.load(Ordering::Acquire);
            let mut next = cur.max(target_idx.saturating_sub(1));
            while tokio::fs::try_exists(&plan_dir.join(cache::segment_filename(next + 1)))
                .await
                .unwrap_or(false)
            {
                next += 1;
            }
            if next != cur {
                head.store(next, Ordering::Release);
            }
            if next >= total_segments {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
}

fn spawn_reaper(
    media_id: String,
    slot: Arc<Mutex<Option<ProducerHandle>>>,
    head: Arc<AtomicU32>,
    target_head: Arc<AtomicU32>,
    paused: Arc<AtomicBool>,
    last_request_at: Arc<RwLock<Instant>>,
) -> JoinHandle<()> {
    // Pull-driven backpressure: ffmpeg may advance only as far as
    // `target_head`. `target_head` only moves when a request arrives,
    // so a stalled client (broken MSE, paused playback) leaves ffmpeg
    // SIGSTOP'd within ≤LOOKAHEAD_BUFFER segments instead of racing
    // to EOF.
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;

            let last = *last_request_at.read().await;
            if last.elapsed() >= IDLE_TIMEOUT {
                let mut guard = slot.lock().await;
                if let Some(h) = guard.as_ref() {
                    if Arc::ptr_eq(&h.head, &head) {
                        let old = guard.take().expect("guard had Some");
                        drop(guard);
                        tracing::info!(media = %media_id, "reaping idle hls producer");
                        old.shutdown().await;
                    }
                }
                return;
            }

            let h = head.load(Ordering::Acquire);
            let target = target_head.load(Ordering::Acquire);
            let is_paused = paused.load(Ordering::Acquire);
            if !is_paused && h >= target {
                if let Some(pid) = current_pid(&slot, &head).await {
                    match signal_pause(pid) {
                        Ok(()) => {
                            paused.store(true, Ordering::Release);
                            tracing::debug!(media = %media_id, head = h, target, "paused producer");
                        }
                        Err(e) => tracing::warn!(media = %media_id, pid, error = %e,
                            "failed to SIGSTOP producer; backpressure not engaging"),
                    }
                }
            } else if is_paused && target > h {
                if let Some(pid) = current_pid(&slot, &head).await {
                    match signal_resume(pid) {
                        Ok(()) => {
                            paused.store(false, Ordering::Release);
                            tracing::debug!(media = %media_id, head = h, target, "resumed producer");
                        }
                        Err(e) => tracing::warn!(media = %media_id, pid, error = %e,
                            "failed to SIGCONT producer"),
                    }
                }
            }
        }
    })
}

async fn current_pid(
    slot: &Arc<Mutex<Option<ProducerHandle>>>,
    head_marker: &Arc<AtomicU32>,
) -> Option<u32> {
    let guard = slot.lock().await;
    let h = guard.as_ref()?;
    if !Arc::ptr_eq(&h.head, head_marker) {
        return None;
    }
    h.child.id()
}

/// Walk top-level mp4 boxes by header size and confirm they tile the
/// whole file with no gap or overhang. ffmpeg's HLS-fmp4 segment
/// layout is `(styp)? sidx* moof mdat` (sometimes with `prft`
/// between); regardless of which boxes are present, every one's
/// 32-bit `size` field declares its own length, so a segment whose
/// box sizes sum to the file length is structurally complete. A
/// truncated mid-write file fails this check because the last box's
/// declared size extends past EOF.
async fn segment_is_complete(path: &Path) -> bool {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
    let Ok(mut f) = tokio::fs::File::open(path).await else {
        return false;
    };
    let Ok(meta) = f.metadata().await else { return false };
    let total = meta.len();
    let mut pos = 0u64;
    let mut header = [0u8; 8];
    while pos < total {
        if total - pos < 8 {
            return false;
        }
        if f.seek(SeekFrom::Start(pos)).await.is_err() {
            return false;
        }
        if f.read_exact(&mut header).await.is_err() {
            return false;
        }
        let size32 = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        // We don't expect 64-bit large boxes (`size32 == 1`) or
        // "to end of file" (`size32 == 0`) in fmp4 segments, but
        // both would make completeness ambiguous via this header
        // alone — refuse to declare them complete from header walk.
        if size32 < 8 {
            return false;
        }
        let size = size32 as u64;
        if pos + size > total {
            return false;
        }
        pos += size;
    }
    pos == total
}

async fn atomic_link_or_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    match tokio::fs::hard_link(src, dst).await {
        Ok(()) => {
            let _ = tokio::fs::remove_file(src).await;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = tokio::fs::remove_file(src).await;
            Ok(())
        }
        Err(_) => {
            tokio::fs::copy(src, dst).await?;
            let _ = tokio::fs::remove_file(src).await;
            Ok(())
        }
    }
}

#[cfg(unix)]
fn signal_pause(pid: u32) -> std::io::Result<()> {
    send_signal(pid, libc::SIGSTOP)
}

#[cfg(unix)]
fn signal_resume(pid: u32) -> std::io::Result<()> {
    send_signal(pid, libc::SIGCONT)
}

// Direct syscall instead of shelling out to /bin/kill: minimal container
// images (distroless, scratch + ffmpeg static, alpine without procps) often
// lack the `kill` binary even when signals work fine, and a silent
// Command::new("kill") failure leaves backpressure disabled with no obvious
// cause.
#[cfg(unix)]
fn send_signal(pid: u32, sig: i32) -> std::io::Result<()> {
    // SAFETY: libc::kill is async-signal-safe and just takes pid_t + signum.
    // pid was obtained from Child::id() while the child was live; if the
    // child has since exited the kernel returns ESRCH, surfaced as io::Error.
    let rc = unsafe { libc::kill(pid as libc::pid_t, sig) };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

#[cfg(not(unix))]
fn signal_pause(_pid: u32) -> std::io::Result<()> {
    Err(std::io::Error::other("pause not supported on this platform"))
}

#[cfg(not(unix))]
fn signal_resume(_pid: u32) -> std::io::Result<()> {
    Ok(())
}

/// One-shot startup sweep: kill any ffmpeg processes whose command line
/// references our HLS cache root, mopping up orphans from a previous
/// abruptly-terminated parent. Best-effort, Unix-only.
#[cfg(unix)]
pub async fn sweep_orphan_ffmpegs() {
    let cache_root = super::cache::cache_root();
    let needle = match cache_root.to_str() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return,
    };
    let out = match tokio::process::Command::new("pgrep")
        .arg("-f")
        .arg(format!("ffmpeg.*{needle}"))
        .output()
        .await
    {
        Ok(o) => o,
        Err(_) => return,
    };
    if !out.status.success() {
        return;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut killed = 0;
    for line in stdout.lines() {
        let Ok(pid) = line.trim().parse::<u32>() else { continue };
        // SIGCONT first in case the orphan inherited a SIGSTOP from us;
        // a stopped process can technically receive SIGKILL but resuming
        // it first lets the kernel clean up cleanly.
        let _ = send_signal(pid, libc::SIGCONT);
        if send_signal(pid, libc::SIGKILL).is_ok() {
            killed += 1;
        }
    }
    if killed > 0 {
        tracing::info!(killed, "swept orphan ffmpeg processes from previous run");
    }
}

#[cfg(not(unix))]
pub async fn sweep_orphan_ffmpegs() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_argv_quotes_paths_with_spaces() {
        let argv = vec![
            "ffmpeg".to_string(),
            "-i".to_string(),
            "/srv/My Movies/it's a film.mkv".to_string(),
            "-c:v".to_string(),
            "libx264".to_string(),
        ];
        let out = format_argv(&argv);
        // Path with spaces and a single quote gets single-quoted with
        // the embedded `'` rendered as `'\''`.
        assert!(out.contains("'/srv/My Movies/it'\\''s a film.mkv'"));
        // Plain args stay unquoted.
        assert!(out.starts_with("ffmpeg -i "));
        assert!(out.contains(" -c:v libx264"));
        assert!(out.ends_with('\n'));
    }
}
