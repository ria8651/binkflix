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
use super::plan::{AudioPlan, StreamPlan};
use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
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

#[derive(Default)]
pub struct ProducerRegistry {
    by_media: DashMap<String, Arc<Mutex<Option<ProducerHandle>>>>,
}

impl ProducerRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn slot(&self, media_id: &str) -> Arc<Mutex<Option<ProducerHandle>>> {
        self.by_media
            .entry(media_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    }

    pub async fn snapshot(&self, media_id: &str) -> Option<crate::types::HlsProducerState> {
        let slot = self.by_media.get(media_id).map(|e| e.clone())?;
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
    /// canonical is empty — first-write-wins).
    pub run_dir: PathBuf,
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
            let _ = signal_resume(pid).await;
        }
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        let _ = tokio::fs::remove_dir_all(&self.run_dir).await;
    }
}

#[derive(Clone)]
pub struct ProducerCtx {
    pub media_id: String,
    pub source: PathBuf,
    pub plan: Arc<StreamPlan>,
    pub plan_dir: PathBuf,
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
        bump_target_head(registry, &ctx.media_id, idx, total).await;
        return Ok(seg_path);
    }

    // Slot mutex serialises start/restart so concurrent seek-back-to-back
    // can't fire two ffmpegs on the same media.
    let slot = registry.slot(&ctx.media_id);
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
        } else {
            let h = guard.as_mut().expect("checked above");
            let new_target = idx.saturating_add(LOOKAHEAD_BUFFER).min(total);
            h.target_head.fetch_max(new_target, Ordering::AcqRel);
            *h.last_request_at.write().await = Instant::now();
            if h.paused.load(Ordering::Acquire) {
                if let Some(pid) = h.child.id() {
                    let _ = signal_resume(pid).await;
                    h.paused.store(false, Ordering::Release);
                }
            }
        }
    }

    wait_for_file(&seg_path, SEGMENT_WAIT_TIMEOUT).await?;
    Ok(seg_path)
}

async fn bump_target_head(registry: &ProducerRegistry, media_id: &str, idx: u32, total: u32) {
    if let Some(slot) = registry.by_media.get(media_id).map(|e| e.clone()) {
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
    tokio::fs::create_dir_all(&ctx.plan_dir).await?;

    // ffmpeg starts a bit before target_idx so the first encoded
    // segment (priming gap) can be discarded; canonical output begins
    // at target_idx.
    let ff_start_idx = target_idx.saturating_sub(PREROLL_SEGMENTS).max(1);

    // Per-run scratch dir. Each run gets its own folder so concurrent
    // promotions can hard-link into canonical without colliding. Run id
    // mixes pid with a monotonic counter so back-to-back launches in
    // the same process can't share a directory name.
    static RUN_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let run_id = ((std::process::id() as u64) << 32)
        | RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let run_dir = ctx.plan_dir.join(format!("_run-{run_id:016x}"));
    let _ = tokio::fs::remove_dir_all(&run_dir).await;
    tokio::fs::create_dir_all(&run_dir).await?;

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
    let mut child = spawn_ffmpeg(&ctx, ff_start_idx, start_t, &run_dir)?;

    if let Some(stderr) = child.stderr.take() {
        let id = ctx.media_id.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "binkflix::hls::ffmpeg", media = %id, "{line}");
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
        run_dir.clone(),
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
        run_dir,
        tasks: vec![watcher, reaper],
    })
}

fn spawn_ffmpeg(
    ctx: &ProducerCtx,
    start_idx: u32,
    start_t: f64,
    run_dir: &Path,
) -> anyhow::Result<Child> {
    // The flag set that took a long time to find. Key pieces:
    //
    //  * `-ss <start_t>` before `-i`: fast demuxer-index seek, lands at
    //    nearest cluster ≤ start_t, sub-millisecond.
    //  * `-copyts -avoid_negative_ts disabled`: preserve source PTS
    //    through the pipeline, don't shift either track.
    //  * `-hls_segment_options movflags=+frag_discont`: THE flag (from
    //    Jellyfin's `DynamicHlsController.cs`). Without it the
    //    HLS-fmp4 muxer normalises each fragment's tfdt to zero, which
    //    breaks A/V sync on seek-restart because the trun
    //    composition_offsets are still source-absolute. With it, tfdt
    //    is the actual sample DTS (including the seek offset) and both
    //    tracks land on the source-absolute timeline.
    //  * `-hls_segment_filename seg-%05d.m4s -start_number start_idx`:
    //    scratch filenames already encode plan indices; the watcher
    //    renames straight across.
    let mut cmd = Command::new("ffmpeg");
    cmd.current_dir(run_dir)
        .arg("-hide_banner")
        .arg("-loglevel").arg("warning")
        .arg("-nostdin")
        .arg("-ss").arg(format!("{start_t:.6}"))
        .arg("-copyts")
        .arg("-i").arg(&ctx.source)
        .arg("-map").arg("0:v:0")
        .arg("-map").arg("0:a:0?")
        .arg("-avoid_negative_ts").arg("disabled")
        .arg("-c:v").arg("copy");
    apply_audio_args(&mut cmd, &ctx.plan.audio);
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

    cmd.spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn ffmpeg: {e}"))
}

fn apply_audio_args(cmd: &mut Command, audio: &AudioPlan) {
    if audio.out_codec == "copy" {
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
            // init.mp4 is byte-identical across runs for a given plan,
            // so first-write-wins is fine. ffmpeg writes init.mp4 once
            // before the first segment so by the time any scratch
            // seg-*.m4s exists, init.mp4 is closed.
            if !tokio::fs::try_exists(&canonical_init).await.unwrap_or(false)
                && tokio::fs::try_exists(&scratch_init).await.unwrap_or(false)
            {
                if let Err(e) = atomic_link_or_copy(&scratch_init, &canonical_init).await {
                    tracing::warn!(error = %e, "failed to promote init.mp4");
                }
            }

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
                    if signal_pause(pid).await.is_ok() {
                        paused.store(true, Ordering::Release);
                        tracing::debug!(media = %media_id, head = h, target, "paused producer");
                    }
                }
            } else if is_paused && target > h {
                if let Some(pid) = current_pid(&slot, &head).await {
                    if signal_resume(pid).await.is_ok() {
                        paused.store(false, Ordering::Release);
                        tracing::debug!(media = %media_id, head = h, target, "resumed producer");
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
async fn signal_pause(pid: u32) -> std::io::Result<()> {
    run_kill(pid, "-STOP").await
}

#[cfg(unix)]
async fn signal_resume(pid: u32) -> std::io::Result<()> {
    run_kill(pid, "-CONT").await
}

#[cfg(unix)]
async fn run_kill(pid: u32, sig: &str) -> std::io::Result<()> {
    let status = tokio::process::Command::new("kill")
        .arg(sig)
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .status()
        .await?;
    if !status.success() {
        return Err(std::io::Error::other(format!("kill {sig} {pid} failed")));
    }
    Ok(())
}

#[cfg(not(unix))]
async fn signal_pause(_pid: u32) -> std::io::Result<()> {
    Err(std::io::Error::other("pause not supported on this platform"))
}

#[cfg(not(unix))]
async fn signal_resume(_pid: u32) -> std::io::Result<()> {
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
        let _ = run_kill(pid, "-CONT").await;
        if run_kill(pid, "-KILL").await.is_ok() {
            killed += 1;
        }
    }
    if killed > 0 {
        tracing::info!(killed, "swept orphan ffmpeg processes from previous run");
    }
}

#[cfg(not(unix))]
pub async fn sweep_orphan_ffmpegs() {}
