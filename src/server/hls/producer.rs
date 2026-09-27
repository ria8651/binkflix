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
//!   ensure_segment ──► pool mutex ──► (cache hit? → serve)
//!        │                          (covered by a pooled producer? → follow + wait)
//!        │                          (uncovered region? → spawn another producer)
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
//! ## Lifecycle (region pool)
//!
//! Each `(media, audio, mode)` key owns a *pool* of producers. Nothing
//! is ever killed because another viewer moved — that's what lets a
//! watch party share a file without members thrashing each other.
//!
//! * **Follow**: a request whose segment is covered by an existing
//!   producer (`[start_idx, head + LOOKAHEAD_WINDOW]`) bumps that
//!   producer's read-ahead and waits on the shared cache. Synced viewers
//!   thus coalesce onto one ffmpeg.
//! * **Spawn**: a request for an uncovered region (seek into cold cache)
//!   spawns an *additional* producer in the pool, targeted at it. The old
//!   one is left alone.
//! * **Backpressure (pull-driven)**: ffmpeg is SIGSTOP'd whenever
//!   `head ≥ target_head`. `target_head` only advances when a request
//!   arrives — each request bumps it to `max(target_head, idx +
//!   LOOKAHEAD_BUFFER)`. So if the client stops fetching (e.g. its
//!   buffer is full, or MSE got detached by a strict CSP), the
//!   producer stalls within ≤LOOKAHEAD_BUFFER segments instead of
//!   racing to EOF.
//! * **Idle**: 30s of no requests reaps the producer entirely.

use super::cache;
use super::hwenc::{HwEncConfig, HwEncoder};
use super::plan::{AudioPlan, Mode, StreamPlan};
use dashmap::DashMap;
use sqlx::SqlitePool;
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

/// Capped-CRF quality target for libx264. 21 at `veryfast` is visually
/// transparent on most content while landing well under the bitrate
/// ceiling on anything that isn't grain- or motion-heavy. Mirrored in
/// `src/video_player.rs` for the debug panel's label.
const TRANSCODE_CRF: u32 = 21;

/// VideoToolbox constant-quality target, chosen to match `TRANSCODE_CRF`
/// so a viewer sees the same picture whichever encoder the server has.
/// Measured on a 1080p WEB-DL sample: CRF 21 scored VMAF 94.2, `-q:v 70`
/// scored 94.5 (`-q:v 65` was 92.8, `-q:v 75` was 95.4).
///
/// VideoToolbox needs roughly 2.4x the bitrate of libx264 for that same
/// score. That's the standing trade for using the hardware path, not
/// something this constant can tune away.
const VIDEOTOOLBOX_QUALITY: u32 = 70;

/// VAAPI QVBR quality target (`-global_quality`), QP-like: lower is better.
///
/// Measured on the deployment target — Intel Gen9.5 / iHD 25.2.3, i5-8400T
/// — against a 1080p WEB-DL sample, scored with VMAF:
///
/// | mode                | bitrate  | VMAF  |
/// |---------------------|----------|-------|
/// | ABR 8000k (was)     | 6591 kbps| 93.09 |
/// | QVBR `-gq 18`       | 2577 kbps| 92.08 |
/// | QVBR `-gq 20`       | 2456 kbps| 91.98 |
/// | QVBR `-gq 22`       | 2052 kbps| 91.74 |
/// | QVBR `-gq 26`       | 1177 kbps| 90.56 |
///
/// The curve is flat above ~20 — `gq18` buys 0.1 VMAF over `gq20` for
/// another 120 kbps — so 20 sits at the top of the useful range without
/// paying into the saturated part. Against the old ABR it gives up ~1.1
/// VMAF for a 63% bitrate cut.
const VAAPI_QUALITY: u32 = 20;

/// How often a follower re-checks its leader while waiting on the shared
/// cache, and how long the leader's `head` may stall short of the target
/// before the follower gives up and spawns its own producer.
const FOLLOWER_POLL: Duration = Duration::from_millis(200);
const FOLLOWER_STALL_GRACE: Duration = Duration::from_millis(1500);

/// Registry key: `(file_id, audio_idx, mode_tag)`. Each key maps to a
/// *pool* of producers, not a single one. A segment request follows a
/// pooled producer already covering its region (so synced viewers
/// coalesce onto one ffmpeg); a request for a genuinely different region
/// spawns an additional producer. A producer is never killed because
/// another viewer moved — abandoned ones fill their lookahead, SIGSTOP
/// under backpressure (zero CPU), and idle-reap. The on-disk canonical
/// cache is shared across the whole pool (it's keyed only by this tuple),
/// so two producers promoting the same segment is a harmless
/// first-writer-wins.
type ProducerKey = (String, u32, String);

#[derive(Default)]
pub struct ProducerRegistry {
    by_media: DashMap<ProducerKey, Arc<Mutex<Vec<ProducerHandle>>>>,
}

impl ProducerRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn pool(&self, file_id: &str, audio_idx: u32, mode_tag: &str) -> Arc<Mutex<Vec<ProducerHandle>>> {
        self.by_media
            .entry((file_id.to_string(), audio_idx, mode_tag.to_string()))
            .or_insert_with(|| Arc::new(Mutex::new(Vec::new())))
            .clone()
    }

    /// Snapshot the lead producer whose mode tag *starts with* `prefix`.
    ///
    /// The analytics row records the target bitrate but not the height, and
    /// since Auto now derives height from the source resolution the two are
    /// no longer a fixed pair — `tx{bitrate}h` is the most specific key the
    /// telemetry endpoint can rebuild from what it stored. `remux` has no
    /// suffix, so it round-trips through the same call unchanged.
    pub async fn snapshot_by_prefix(
        &self,
        file_id: &str,
        audio_idx: u32,
        prefix: &str,
    ) -> Option<crate::types::HlsProducerState> {
        // Resolve to an owned tag first: holding a DashMap guard across the
        // `.await` inside `snapshot` would risk deadlocking the shard.
        let tag = self
            .by_media
            .iter()
            .find(|e| {
                let (m, a, tag) = e.key();
                m == file_id && *a == audio_idx && tag.starts_with(prefix)
            })
            .map(|e| e.key().2.clone())?;
        self.snapshot(file_id, audio_idx, &tag).await
    }

    pub async fn snapshot(&self, file_id: &str, audio_idx: u32, mode_tag: &str) -> Option<crate::types::HlsProducerState> {
        let pool = self.by_media.get(&(file_id.to_string(), audio_idx, mode_tag.to_string())).map(|e| e.clone())?;
        let guard = pool.lock().await;
        // Report the lead producer (furthest along). The debug panel
        // shows one row; the pool is usually size 1 anyway.
        let h = guard.iter().max_by_key(|h| h.head.load(Ordering::Acquire))?;
        let start_idx = h.start_idx;
        let head = h.head.load(Ordering::Acquire);
        let target_head = h.target_head.load(Ordering::Acquire);
        let paused = h.paused.load(Ordering::Acquire);
        let encode_rate_x100 = h.rate_x100.load(Ordering::Acquire);
        let idle_for_secs = h.last_request_at.read().await.elapsed().as_secs_f64();
        Some(crate::types::HlsProducerState {
            start_idx,
            head,
            target_head,
            paused,
            idle_for_secs,
            encode_rate_x100,
            lookahead_buffer: LOOKAHEAD_BUFFER,
            lookahead_window: LOOKAHEAD_WINDOW,
        })
    }
}

/// Index of the best producer in `pool` whose window covers `idx`
/// (`start_idx ≤ idx ≤ head + LOOKAHEAD_WINDOW`), excluding any whose
/// `head` Arc is in `skip` (producers a caller already found stalled).
/// Prefers the producer furthest along (highest `head`) so the follower
/// waits the least.
fn pick_covering(pool: &[ProducerHandle], idx: u32, skip: &[Arc<AtomicU32>]) -> Option<usize> {
    pool.iter()
        .enumerate()
        .filter(|(_, h)| {
            covers(h.start_idx, h.head.load(Ordering::Acquire), idx)
                && !skip.iter().any(|s| Arc::ptr_eq(s, &h.head))
        })
        .max_by_key(|(_, h)| h.head.load(Ordering::Acquire))
        .map(|(i, _)| i)
}

/// Whether a producer at `start_idx` whose canonical `head` has reached
/// `head` can serve segment `idx` without a far-seek relaunch: `idx` must
/// be at or after the run's start and no more than `LOOKAHEAD_WINDOW` past
/// `head` (a fresh fast input-seek beats sequential decode beyond that, so
/// such a request spawns its own producer instead of following).
fn covers(start_idx: u32, head: u32, idx: u32) -> bool {
    idx >= start_idx && idx <= head.saturating_add(LOOKAHEAD_WINDOW)
}

/// Bump a producer's read-ahead target to cover `idx`, refresh its idle
/// timer, and resume it if backpressure had it SIGSTOP'd.
async fn nudge(h: &ProducerHandle, idx: u32, total: u32) {
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
    /// Recent encode throughput, `realtime × 100`. Maintained by the
    /// watcher from a sliding window of `head` advances; surfaced via
    /// `snapshot` for `transcode_rate_x100` telemetry. Reads ~0 while
    /// SIGSTOP'd, since `head` doesn't move then.
    pub rate_x100: Arc<AtomicU32>,
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
    /// The item being played — what telemetry events and logs name.
    pub media_id: String,
    /// The file `source` belongs to — what the registry and on-disk cache
    /// are keyed by, since segments are only interchangeable within a file.
    pub file_id: String,
    pub source: PathBuf,
    pub plan: Arc<StreamPlan>,
    pub plan_dir: PathBuf,
    pub audio_idx: u32,
    /// Cache key for the registry — the same `(file_id, audio_idx)`
    /// running with different `mode_tag`s are independent producers so a
    /// remux client and a transcode client can coexist without one
    /// killing the other's ffmpeg.
    pub mode_tag: String,
    /// `None` when the requested audio index doesn't exist on the source.
    /// ffmpeg gets `-map 0:a:N?` either way; the optional flag just means
    /// "no audio in the output" if the stream is missing.
    pub audio: Option<AudioPlan>,
    /// Hardware-encoder configuration resolved at server startup. The
    /// encoder a given launch actually ends up on may differ: a hwenc
    /// ffmpeg that dies during startup falls back to libx264 for *that
    /// launch only* (lenient mode) or fails the request (strict) — see
    /// [`launch_producer`].
    pub hw: HwEncConfig,
    /// DB handle for best-effort `transcode.*` lifecycle telemetry.
    pub pool: SqlitePool,
}

/// Just the bits a spawned watcher/reaper/stderr-reader needs to emit a
/// `transcode.*` event — so we don't drag a whole `ProducerCtx` (with its
/// `Arc<StreamPlan>` etc.) into every background task.
#[derive(Clone)]
struct EventMeta {
    pool: SqlitePool,
    media_id: String,
    audio_idx: u32,
    mode_tag: String,
}

impl EventMeta {
    fn from_ctx(ctx: &ProducerCtx) -> Self {
        Self {
            pool: ctx.pool.clone(),
            media_id: ctx.media_id.clone(),
            audio_idx: ctx.audio_idx,
            mode_tag: ctx.mode_tag.clone(),
        }
    }

    /// Best-effort fire-and-forget event. `extra` fields are merged onto
    /// the standard `{audio_idx, mode_tag}` envelope.
    fn emit(&self, kind: &'static str, extra: serde_json::Value) {
        let pool = self.pool.clone();
        let media = self.media_id.clone();
        let aidx = self.audio_idx;
        let mode = self.mode_tag.clone();
        tokio::spawn(async move {
            let mut data = serde_json::json!({ "audio_idx": aidx, "mode_tag": mode });
            if let (Some(obj), Some(ex)) = (data.as_object_mut(), extra.as_object()) {
                for (k, v) in ex {
                    obj.insert(k.clone(), v.clone());
                }
            }
            crate::server::analytics::record_event(&pool, kind, None, Some(&media), None, &data)
                .await;
        });
    }
}

/// What the m3u8 endpoint advertises as `X-Stream-Encoder`: the encoder
/// producers are *asked* to use. Runtime fallback is per-launch and lives
/// entirely inside `launch_producer`, so it isn't knowable here — this
/// response is written before any producer for the stream exists.
pub fn requested_encoder_name(cfg: &HwEncConfig) -> &'static str {
    cfg.requested_name()
}

/// What the m3u8 endpoint should advertise as `X-Stream-Ratecontrol`.
///
/// Resolved server-side on purpose: the client can't see the Apple-Silicon
/// gate inside `select_rate_control`, so any client-side guess would
/// misreport it. Like the encoder name, this describes the *requested*
/// backend — a launch that falls back to libx264 also falls back to its
/// rate control.
pub fn requested_rate_control_name(cfg: &HwEncConfig, hard_cap: bool) -> String {
    match select_rate_control(cfg.encoder, hard_cap) {
        RateControl::CappedCrf => format!("crf {TRANSCODE_CRF}"),
        RateControl::VideotoolboxQuality => format!("q:v {VIDEOTOOLBOX_QUALITY}"),
        RateControl::VaapiQvbr => format!("qvbr {VAAPI_QUALITY}"),
        RateControl::Abr => "abr".to_string(),
    }
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

    // Fast path: cached on disk. Still nudge a covering producer so it
    // keeps its read-ahead window aligned with where the client is.
    if tokio::fs::try_exists(&seg_path).await.unwrap_or(false) {
        bump_covering(registry, &ctx.file_id, ctx.audio_idx, &ctx.mode_tag, idx, total).await;
        return Ok(seg_path);
    }

    let pool = registry.pool(&ctx.file_id, ctx.audio_idx, &ctx.mode_tag);
    let overall_deadline = Instant::now() + SEGMENT_WAIT_TIMEOUT;
    // Producers we tried to follow but found stalled — don't re-follow
    // them on the next loop, spawn our own instead.
    let mut stalled: Vec<Arc<AtomicU32>> = Vec::new();

    loop {
        // Decide under the pool lock: follow a covering producer, or
        // spawn a new one. The lock serialises this decision so a synced
        // burst of requests resolves to one spawn + N follows, not N
        // spawns (thundering-herd guard within the key).
        let leader_head = {
            let mut guard = pool.lock().await;
            if tokio::fs::try_exists(&seg_path).await.unwrap_or(false) {
                if let Some(i) = pick_covering(&guard, idx, &[]) {
                    nudge(&guard[i], idx, total).await;
                }
                return Ok(seg_path);
            }
            if let Some(i) = pick_covering(&guard, idx, &stalled) {
                nudge(&guard[i], idx, total).await;
                guard[i].head.clone()
            } else {
                // No live producer covers this region — spawn one. Holding
                // the pool lock across the launch is what makes concurrent
                // requests for the same region coalesce.
                let handle = launch_producer(ctx.clone(), idx, pool.clone()).await?;
                guard.push(handle);
                drop(guard);
                wait_for_file(&seg_path, overall_deadline.saturating_duration_since(Instant::now())).await?;
                return Ok(seg_path);
            }
        };

        match follow_wait(&seg_path, &pool, idx, &leader_head, total, overall_deadline).await {
            FollowOutcome::Ready => return Ok(seg_path),
            FollowOutcome::Respawn => stalled.push(leader_head),
            FollowOutcome::Timeout => {
                anyhow::bail!("timed out waiting for {}", seg_path.display())
            }
        }
    }
}

enum FollowOutcome {
    /// The followed producer delivered the segment to canonical.
    Ready,
    /// The leader stalled / disappeared — caller should re-decide (spawn).
    Respawn,
    /// Overall deadline elapsed.
    Timeout,
}

/// Wait on the shared cache for a producer (`leader_head`) we're
/// following to deliver `seg_path`, keeping it alive and advancing toward
/// `idx`. Bounded by `overall_deadline` (correctness backstop — never an
/// indefinite wait); returns `Respawn` early if the leader leaves the
/// pool or its `head` stalls short of `idx` past `FOLLOWER_STALL_GRACE`.
async fn follow_wait(
    seg_path: &Path,
    pool: &Arc<Mutex<Vec<ProducerHandle>>>,
    idx: u32,
    leader_head: &Arc<AtomicU32>,
    total: u32,
    overall_deadline: Instant,
) -> FollowOutcome {
    let mut last_head = leader_head.load(Ordering::Acquire);
    let mut last_progress = Instant::now();
    loop {
        if tokio::fs::try_exists(seg_path).await.unwrap_or(false) {
            return FollowOutcome::Ready;
        }
        if Instant::now() >= overall_deadline {
            return FollowOutcome::Timeout;
        }
        tokio::time::sleep(FOLLOWER_POLL).await;

        let now_head = leader_head.load(Ordering::Acquire);
        if now_head > last_head {
            last_head = now_head;
            last_progress = Instant::now();
        }

        // Keep the leader alive (refresh idle timer) and advancing toward
        // our target. If it's no longer in the pool, it was reaped/removed.
        let still_following = {
            let guard = pool.lock().await;
            if tokio::fs::try_exists(seg_path).await.unwrap_or(false) {
                return FollowOutcome::Ready;
            }
            match guard.iter().find(|p| Arc::ptr_eq(&p.head, leader_head)) {
                Some(h) => {
                    nudge(h, idx, total).await;
                    true
                }
                None => false,
            }
        };
        if !still_following {
            return FollowOutcome::Respawn;
        }
        if last_progress.elapsed() >= FOLLOWER_STALL_GRACE
            && leader_head.load(Ordering::Acquire) < idx
        {
            return FollowOutcome::Respawn;
        }
    }
}

/// Fast-path helper: nudge whichever pooled producer already covers `idx`
/// (no-op if none does — the segment is already on disk).
async fn bump_covering(
    registry: &ProducerRegistry,
    file_id: &str,
    audio_idx: u32,
    mode_tag: &str,
    idx: u32,
    total: u32,
) {
    if let Some(pool) = registry
        .by_media
        .get(&(file_id.to_string(), audio_idx, mode_tag.to_string()))
        .map(|e| e.clone())
    {
        let guard = pool.lock().await;
        if let Some(i) = pick_covering(&guard, idx, &[]) {
            nudge(&guard[i], idx, total).await;
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
    pool: Arc<Mutex<Vec<ProducerHandle>>>,
) -> anyhow::Result<ProducerHandle> {
    // Retry loop for the hw-encoder runtime fallback, scoped to this launch
    // and nothing else. We try with the configured hw encoder; if the ffmpeg
    // child dies *unsuccessfully* within 750ms (the signature of a
    // driver/kernel-module rejection — by then the encoder has either
    // claimed the device or thrown), we log, emit telemetry, and retry once
    // with libx264. libx264 won't trip the same path so the loop is at most
    // two iterations.
    //
    // Three things this deliberately does NOT do, each of which was a bug:
    //
    //  * Probe non-transcode plans. A remux is `-c:v copy` with no encoder
    //    in the pipeline, so it cannot fail *as* a hwenc — and because a
    //    stream copy of a page-cached file can reach EOF before the reaper's
    //    first backpressure tick, it routinely exits inside the window.
    //  * Treat a successful exit as a failure. Status 0 means ffmpeg did the
    //    job; a short tail-of-file transcode can legitimately finish in
    //    <750ms.
    //  * Remember anything process-wide. A per-launch decision stays
    //    per-launch, so one bad read (or one genuinely busy GPU) can't
    //    strand every later stream on software until the container restarts.
    let mut hw = ctx.hw.encoder;
    loop {
        let mut handle = launch_once(ctx.clone(), hw, target_idx, pool.clone()).await?;
        // Skipping the wait entirely (rather than waiting and ignoring the
        // result) keeps 750ms off every remux launch.
        if !hwenc_at_risk(hw, &ctx.plan.mode) {
            return Ok(handle);
        }
        let exit = wait_for_early_exit(&mut handle.child, Duration::from_millis(750)).await;
        match classify_startup(hw, &ctx.plan.mode, &exit) {
            StartupVerdict::Ok => return Ok(handle),
            StartupVerdict::HwencFailed(status) => {
                handle.shutdown().await;
                // One event kind for both outcomes so "how often does the GPU
                // fail to start" is a single query; `fell_back` says what was
                // done about it.
                let meta = EventMeta::from_ctx(&ctx);
                meta.emit(
                    "transcode.hwenc_failure",
                    serde_json::json!({
                        "encoder": hw.ffmpeg_name(),
                        "exit_status": format!("{status:?}"),
                        "fell_back": !ctx.hw.strict,
                        "target_idx": target_idx,
                    }),
                );
                if ctx.hw.strict {
                    tracing::error!(
                        media = %ctx.media_id,
                        encoder = hw.ffmpeg_name(),
                        ?status,
                        "hwenc producer exited during startup; failing transcode (strict mode)"
                    );
                    anyhow::bail!(
                        "hardware encoder {} failed to start and software fallback is disabled \
                         (BINKFLIX_HWACCEL={})",
                        hw.ffmpeg_name(),
                        hw.env_value(),
                    );
                }
                tracing::warn!(
                    media = %ctx.media_id,
                    encoder = hw.ffmpeg_name(),
                    ?status,
                    "hwenc producer exited during startup; falling back to libx264 for this launch"
                );
                hw = HwEncoder::None;
            }
        }
    }
}

enum EarlyExit {
    Alive,
    Exited(std::process::ExitStatus),
}

/// Could this launch's ffmpeg fail *as* a hardware encoder? Only if one is
/// actually in the pipeline: `Mode::Remux` is `-c:v copy` and doesn't even
/// get the `-init_hw_device` preamble, so nothing it does says anything
/// about the GPU.
fn hwenc_at_risk(hw: HwEncoder, mode: &Mode) -> bool {
    hw != HwEncoder::None && matches!(mode, Mode::Transcode { .. })
}

enum StartupVerdict {
    Ok,
    HwencFailed(std::process::ExitStatus),
}

/// Read a launch's first 750ms. Only an *unsuccessful* exit on a plan that
/// actually uses the hw encoder is evidence against the hardware; a status-0
/// exit means ffmpeg finished the job, which a stream copy or a short
/// tail-of-file transcode can genuinely do inside the window.
fn classify_startup(hw: HwEncoder, mode: &Mode, exit: &EarlyExit) -> StartupVerdict {
    match exit {
        EarlyExit::Alive => StartupVerdict::Ok,
        EarlyExit::Exited(status) => {
            if hwenc_at_risk(hw, mode) && !status.success() {
                StartupVerdict::HwencFailed(*status)
            } else {
                StartupVerdict::Ok
            }
        }
    }
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

/// `hw` is the encoder for *this* attempt — `launch_producer` lowers it to
/// `HwEncoder::None` when retrying after a hwenc startup failure.
async fn launch_once(
    ctx: ProducerCtx,
    hw: HwEncoder,
    target_idx: u32,
    pool: Arc<Mutex<Vec<ProducerHandle>>>,
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
    let meta = EventMeta::from_ctx(&ctx);

    tracing::info!(
        media = %ctx.media_id,
        target_idx, ff_start_idx, start_t,
        "launching producer ffmpeg"
    );
    let (mut child, argv) = spawn_ffmpeg(&ctx, hw, ff_start_idx, start_t, run_dir.path())?;
    meta.emit(
        "transcode.spawn",
        serde_json::json!({ "target_idx": target_idx, "start_t": start_t }),
    );

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
        let warn_meta = meta.clone();
        tokio::spawn(async move {
            // Timestamp/corruption warnings worth surfacing as telemetry —
            // these are the fingerprint of A/V-sync-hostile source files.
            // Emit at most one event per category per run (deduped) to
            // keep the events table quiet.
            const WARN_NEEDLES: [&str; 4] =
                ["Packet duration", "out of range", "Non-monotonous DTS", "corrupt"];
            let mut warned: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
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
                for needle in WARN_NEEDLES {
                    if line.contains(needle) && warned.insert(needle) {
                        let sample: String = line.chars().take(200).collect();
                        warn_meta.emit(
                            "transcode.ffmpeg_warning",
                            serde_json::json!({ "needle": needle, "sample": sample }),
                        );
                    }
                }
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
    let rate_x100 = Arc::new(AtomicU32::new(0));
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
        rate_x100.clone(),
        total_segments,
        target_idx,
        meta.clone(),
    );
    let reaper = spawn_reaper(
        ctx.media_id.clone(),
        pool,
        head.clone(),
        target_head.clone(),
        paused.clone(),
        last_request_at.clone(),
        meta,
    );

    Ok(ProducerHandle {
        start_idx: target_idx,
        head,
        rate_x100,
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
    hw: HwEncoder,
    start_idx: u32,
    start_t: f64,
    run_dir: &Path,
) -> anyhow::Result<(Child, Vec<String>)> {
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

/// How a given encoder should be driven for one transcode request.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum RateControl {
    /// Constant quality bounded by `maxrate`/`bufsize`. Best of both, but
    /// only libx264 can actually do it.
    CappedCrf,
    /// VideoToolbox `-q:v`, with no bitrate ceiling at all.
    VideotoolboxQuality,
    /// VAAPI QVBR: a quality target *and* a real ceiling, the hardware
    /// equivalent of capped CRF. Unlike VideoToolbox, `-global_quality`
    /// keeps working with `-maxrate` present.
    VaapiQvbr,
    /// Single-pass average bitrate. The fallback whenever a quality mode
    /// either doesn't exist or can't respect a ceiling we owe the user.
    Abr,
}

/// Pick the rate-control mode for `hw`.
///
/// `hard_cap` is the deciding input for VideoToolbox. Its constant-quality
/// mode is genuinely good — on a 1080p sample it beat its own ABR by a wide
/// margin, VMAF 94.5 at 3.7 Mbps against 93.4 at 5.1 — but passing
/// `-maxrate` alongside `-q:v` makes it *silently* ignore the quality
/// setting and revert to rate control. Verified by measurement: `-q:v` 40,
/// 60 and 80 all produced byte-identical bitrates once `-maxrate` was
/// present. So the two are mutually exclusive, and when the user has asked
/// for a specific ceiling that ceiling wins.
///
/// The mode is also Apple-Silicon-only, hence the `target_arch` gate — on an
/// Intel Mac `-q:v` would be ignored and the encode would run with no
/// target of any kind.
///
/// VAAPI needs no such compromise: QVBR honours `-global_quality` and
/// `-maxrate` together, verified on the deployment target (Intel Gen9.5,
/// iHD 25.2.3) — quality still moved the output with the cap in place
/// (`gq` 18/20/22/24/26 gave 2577/2456/2052/1558/1177 kbps), and a 1500k
/// cap held. So VAAPI gets quality mode for explicit picks too; the
/// ceiling is still enforced, which is all an explicit pick asks for.
///
/// QSV is left on ABR. `h264_qsv` is present on the target but the auto
/// detection prefers VAAPI whenever `/dev/dri/renderD*` exists, so nothing
/// reaches it there and it has never been measured.
fn select_rate_control(hw: HwEncoder, hard_cap: bool) -> RateControl {
    match hw {
        HwEncoder::None => RateControl::CappedCrf,
        HwEncoder::Vaapi => RateControl::VaapiQvbr,
        HwEncoder::VideoToolbox
            if !hard_cap && cfg!(all(target_os = "macos", target_arch = "aarch64")) =>
        {
            RateControl::VideotoolboxQuality
        }
        _ => RateControl::Abr,
    }
}

fn apply_video_args(cmd: &mut Command, mode: &Mode, hw: HwEncoder) {
    match mode {
        Mode::Remux => {
            cmd.arg("-c:v").arg("copy");
        }
        Mode::Transcode { bitrate_kbps, max_height, hard_cap } => {
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
            // Which rate-control mode this encoder gets. Quality-targeted
            // beats bitrate-targeted whenever it's available *and* able to
            // respect the ceiling we owe the user.
            let rc = select_rate_control(hw, *hard_cap);

            // ABR treats `bitrate_kbps` as an average and needs headroom
            // above it for peaks, hence 1.5x. Capped CRF has no average to
            // aim at — whatever `maxrate` allows simply becomes the rate on
            // hard content — so there the budget *is* the ceiling. Leaving
            // it at 1.5x would let a grainy 1080p source sit at 12 Mbps
            // under a preset the menu calls 8.
            let maxrate = if matches!(rc, RateControl::CappedCrf | RateControl::VaapiQvbr) {
                *bitrate_kbps
            } else {
                bitrate_kbps.saturating_mul(15) / 10
            };
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

            // Rate control. libx264 runs *capped CRF*: constant quality
            // with `maxrate`/`bufsize` as a hard ceiling, which is what
            // modern VOD ladders use. Single-pass ABR has to chase an
            // average with no view of the whole file, so it pads easy
            // scenes and still starves hard ones; CRF spends what each
            // scene needs and simply undershoots the budget on quiet
            // content. Same peak bandwidth, better picture, smaller
            // segments — which matters here because we're stuck on
            // `veryfast` and a forced IDR every 6s, and both of those
            // already cost efficiency we can't buy back.
            //
            // The hardware encoders have no real CRF equivalent
            // (VAAPI/QSV/VideoToolbox quality modes are driver-dependent
            // and routinely lose to their own ABR), so they keep the
            // single-pass ABR they've always used.
            //
            // `force_key_frames "expr:gte(t,n_forced*6)"` applies to
            // every backend: it puts IDRs exactly on our 6s segment
            // boundaries so each segment is independently decodable —
            // what `independent_segments` advertises in the playlist.
            match rc {
                RateControl::CappedCrf => {
                    cmd.arg("-crf").arg(TRANSCODE_CRF.to_string())
                        .arg("-maxrate").arg(format!("{maxrate}k"))
                        .arg("-bufsize").arg(format!("{bufsize}k"));
                }
                RateControl::VideotoolboxQuality => {
                    // Deliberately no `-b:v`/`-maxrate`/`-bufsize`: passing
                    // any of them makes VideoToolbox silently discard the
                    // quality setting and fall back to rate control, which
                    // is measurably worse per bit than either mode alone.
                    cmd.arg("-q:v").arg(VIDEOTOOLBOX_QUALITY.to_string());
                }
                RateControl::VaapiQvbr => {
                    // `-rc_mode QVBR` must be explicit: with only a bitrate
                    // present ffmpeg's `auto` picks VBR, which ignores
                    // `-global_quality` entirely.
                    cmd.arg("-rc_mode").arg("QVBR")
                        .arg("-global_quality").arg(VAAPI_QUALITY.to_string())
                        .arg("-b:v").arg(format!("{bitrate_kbps}k"))
                        .arg("-maxrate").arg(format!("{maxrate}k"))
                        .arg("-bufsize").arg(format!("{bufsize}k"));
                }
                RateControl::Abr => {
                    cmd.arg("-b:v").arg(format!("{bitrate_kbps}k"))
                        .arg("-maxrate").arg(format!("{maxrate}k"))
                        .arg("-bufsize").arg(format!("{bufsize}k"));
                }
            }
            cmd.arg("-force_key_frames").arg("expr:gte(t,n_forced*6)");
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
    rate_x100: Arc<AtomicU32>,
    total_segments: u32,
    target_idx: u32,
    meta: EventMeta,
) -> JoinHandle<()> {
    use std::collections::{BTreeMap, HashMap, VecDeque};
    let canonical_init = plan_dir.join("init.mp4");
    let scratch_init = run_dir.join("init.mp4");
    let mut prev_sizes: HashMap<u32, u64> = HashMap::new();
    // Throttle promote-failure telemetry — the watcher ticks every 100ms,
    // so a persistent failure would otherwise flood the events table.
    let mut promote_failures: u64 = 0;
    // Sliding window of (sampled_at, head) for the encode-rate estimate.
    // One entry per tick; pruned to RATE_WINDOW. Rate = media-seconds
    // produced (head delta × nominal 6s segment) per wall-second over the
    // window, so a SIGSTOP'd producer (head frozen) decays to 0.
    let mut rate_window: VecDeque<(Instant, u32)> = VecDeque::new();
    const RATE_WINDOW: Duration = Duration::from_secs(6);
    const NOMINAL_SEG_SECS: f64 = 6.0;
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
                    // First failure, then every 50th, so a stuck producer
                    // leaves a breadcrumb without flooding the table.
                    if promote_failures % 50 == 0 {
                        meta.emit(
                            "transcode.promote_failure",
                            serde_json::json!({ "idx": idx, "error": e.to_string() }),
                        );
                    }
                    promote_failures += 1;
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

            // Update the encode-rate estimate from the head trajectory over
            // the trailing RATE_WINDOW. Oldest-vs-newest within the window
            // keeps it a true throughput average (and ~0 while paused).
            let now = Instant::now();
            rate_window.push_back((now, next));
            while rate_window.front().is_some_and(|(t, _)| now.duration_since(*t) > RATE_WINDOW) {
                rate_window.pop_front();
            }
            if let (Some((t0, h0)), Some((t1, h1))) =
                (rate_window.front().copied(), rate_window.back().copied())
            {
                let dt = t1.duration_since(t0).as_secs_f64();
                let media_secs = h1.saturating_sub(h0) as f64 * NOMINAL_SEG_SECS;
                let rate = if dt > 0.0 { (media_secs / dt * 100.0).round() } else { 0.0 };
                rate_x100.store(rate.clamp(0.0, u32::MAX as f64) as u32, Ordering::Release);
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
    pool: Arc<Mutex<Vec<ProducerHandle>>>,
    head: Arc<AtomicU32>,
    target_head: Arc<AtomicU32>,
    paused: Arc<AtomicBool>,
    last_request_at: Arc<RwLock<Instant>>,
    meta: EventMeta,
) -> JoinHandle<()> {
    // Pull-driven backpressure: ffmpeg may advance only as far as
    // `target_head`. `target_head` only moves when a request arrives,
    // so a stalled client (broken MSE, paused playback) leaves ffmpeg
    // SIGSTOP'd within ≤LOOKAHEAD_BUFFER segments instead of racing
    // to EOF. The reaper identifies *its own* producer within the pool
    // by `Arc::ptr_eq` on the `head` Arc it was handed at launch.
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;

            let last = *last_request_at.read().await;
            if last.elapsed() >= IDLE_TIMEOUT {
                let mut guard = pool.lock().await;
                if let Some(pos) = guard.iter().position(|h| Arc::ptr_eq(&h.head, &head)) {
                    let old = guard.swap_remove(pos);
                    drop(guard);
                    tracing::info!(media = %media_id, "reaping idle hls producer");
                    meta.emit(
                        "transcode.reap",
                        serde_json::json!({ "head": head.load(Ordering::Acquire) }),
                    );
                    old.shutdown().await;
                }
                return;
            }

            let h = head.load(Ordering::Acquire);
            let target = target_head.load(Ordering::Acquire);
            let is_paused = paused.load(Ordering::Acquire);
            if !is_paused && h >= target {
                if let Some(pid) = current_pid(&pool, &head).await {
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
                if let Some(pid) = current_pid(&pool, &head).await {
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
    pool: &Arc<Mutex<Vec<ProducerHandle>>>,
    head_marker: &Arc<AtomicU32>,
) -> Option<u32> {
    let guard = pool.lock().await;
    let h = guard.iter().find(|h| Arc::ptr_eq(&h.head, head_marker))?;
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

    /// Both statuses this pair of tests needs, as ffmpeg would deliver them.
    fn exited(code: i32) -> EarlyExit {
        use std::os::unix::process::ExitStatusExt;
        EarlyExit::Exited(std::process::ExitStatus::from_raw(code << 8))
    }

    fn transcode() -> Mode {
        Mode::Transcode { bitrate_kbps: 8000, max_height: 1080, hard_cap: false }
    }

    #[test]
    fn clean_exit_is_never_a_hwenc_failure() {
        // The regression that stranded prod on libx264 for 17 days: ffmpeg
        // finishing its work inside the probe window read as a dead GPU.
        assert!(matches!(
            classify_startup(HwEncoder::Vaapi, &transcode(), &exited(0)),
            StartupVerdict::Ok
        ));
        assert!(matches!(
            classify_startup(HwEncoder::Vaapi, &transcode(), &exited(1)),
            StartupVerdict::HwencFailed(_)
        ));
    }

    #[test]
    fn remux_cannot_implicate_the_hw_encoder() {
        // `-c:v copy` has no encoder in the pipeline, so however it dies it
        // says nothing about the hardware — and a stream copy reaching EOF
        // before the reaper's first tick made this the common case.
        assert!(!hwenc_at_risk(HwEncoder::Vaapi, &Mode::Remux));
        assert!(matches!(
            classify_startup(HwEncoder::Vaapi, &Mode::Remux, &exited(1)),
            StartupVerdict::Ok
        ));
        // Software transcodes have nothing to fall back *to*.
        assert!(!hwenc_at_risk(HwEncoder::None, &transcode()));
        // A hw transcode is the one case worth probing.
        assert!(hwenc_at_risk(HwEncoder::Vaapi, &transcode()));
    }

    #[test]
    fn software_always_gets_capped_crf() {
        // libx264 is the one encoder that can honour a ceiling *and* target
        // quality, so it uses CRF in both directions.
        assert_eq!(select_rate_control(HwEncoder::None, true), RateControl::CappedCrf);
        assert_eq!(select_rate_control(HwEncoder::None, false), RateControl::CappedCrf);
    }

    #[test]
    fn explicit_pick_never_loses_its_ceiling() {
        // VideoToolbox's quality mode can't be capped, so an explicit pick
        // must fall back to ABR there even though quality mode looks better.
        assert_eq!(select_rate_control(HwEncoder::VideoToolbox, true), RateControl::Abr);
        assert_eq!(select_rate_control(HwEncoder::Qsv, true), RateControl::Abr);
        // VAAPI keeps QVBR: it honours the ceiling, which is the only thing
        // an explicit pick actually asks for.
        assert_eq!(select_rate_control(HwEncoder::Vaapi, true), RateControl::VaapiQvbr);
    }

    #[test]
    fn vaapi_uses_qvbr_in_both_directions() {
        assert_eq!(select_rate_control(HwEncoder::Vaapi, false), RateControl::VaapiQvbr);
        assert_eq!(select_rate_control(HwEncoder::Vaapi, true), RateControl::VaapiQvbr);
    }

    #[test]
    fn qsv_stays_on_abr_until_measured() {
        // Present on the target but never selected (auto-detect prefers
        // VAAPI when a render node exists), so its quality modes are
        // unverified.
        assert_eq!(select_rate_control(HwEncoder::Qsv, false), RateControl::Abr);
    }

    #[test]
    fn vaapi_qvbr_sets_the_mode_explicitly_and_caps_at_the_budget() {
        let mode = Mode::Transcode { bitrate_kbps: 8000, max_height: 1080, hard_cap: false };
        let argv = video_argv(&mode, HwEncoder::Vaapi);
        let val = |f: &str| argv.iter().position(|a| a == f).map(|i| argv[i + 1].clone());
        // Without an explicit `-rc_mode`, ffmpeg's `auto` picks VBR and
        // silently ignores `-global_quality`.
        assert_eq!(val("-rc_mode").as_deref(), Some("QVBR"));
        assert_eq!(val("-global_quality").as_deref(), Some("20"));
        // Quality-targeted, so the budget is the ceiling — not 1.5x it.
        assert_eq!(val("-maxrate").as_deref(), Some("8000k"));
    }

    #[test]
    fn videotoolbox_auto_uses_quality_mode_on_apple_silicon() {
        let rc = select_rate_control(HwEncoder::VideoToolbox, false);
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            assert_eq!(rc, RateControl::VideotoolboxQuality);
        } else {
            // `-q:v` is Apple-Silicon-only; anywhere else it would be
            // ignored and leave the encode with no target at all.
            assert_eq!(rc, RateControl::Abr);
        }
    }

    fn video_argv(mode: &Mode, hw: HwEncoder) -> Vec<String> {
        let mut cmd = Command::new("ffmpeg");
        apply_video_args(&mut cmd, mode, hw);
        cmd.as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn videotoolbox_quality_mode_emits_no_bitrate_flags() {
        // Measured footgun: passing any of `-b:v`/`-maxrate`/`-bufsize`
        // alongside `-q:v` makes VideoToolbox silently discard the quality
        // setting and revert to rate control (`-q:v` 40/60/80 all produced
        // identical bitrates once `-maxrate` was present). Guard it.
        if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            return;
        }
        let mode = Mode::Transcode { bitrate_kbps: 8000, max_height: 1080, hard_cap: false };
        let argv = video_argv(&mode, HwEncoder::VideoToolbox);
        assert!(argv.iter().any(|a| a == "-q:v"), "expected quality mode: {argv:?}");
        for flag in ["-b:v", "-maxrate", "-bufsize", "-crf"] {
            assert!(!argv.iter().any(|a| a == flag), "{flag} must not accompany -q:v: {argv:?}");
        }
    }

    #[test]
    fn capped_crf_keeps_the_budget_as_a_hard_ceiling() {
        let mode = Mode::Transcode { bitrate_kbps: 4000, max_height: 720, hard_cap: false };
        let argv = video_argv(&mode, HwEncoder::None);
        let val = |f: &str| {
            argv.iter().position(|a| a == f).map(|i| argv[i + 1].clone())
        };
        assert_eq!(val("-crf").as_deref(), Some("21"));
        // Not 1.5x: under CRF whatever maxrate allows becomes the rate on
        // hard content, so the ceiling has to be the budget itself.
        assert_eq!(val("-maxrate").as_deref(), Some("4000k"));
        assert!(!argv.iter().any(|a| a == "-b:v"));
    }

    #[test]
    fn explicit_pick_on_hardware_keeps_its_bitrate_target() {
        let mode = Mode::Transcode { bitrate_kbps: 2000, max_height: 480, hard_cap: true };
        let argv = video_argv(&mode, HwEncoder::VideoToolbox);
        assert!(argv.iter().any(|a| a == "-b:v"), "{argv:?}");
        assert!(!argv.iter().any(|a| a == "-q:v"), "{argv:?}");
    }

    #[test]
    fn covers_window() {
        // A producer started at seg 10 that has reached head 20.
        // In-range: from its start through head + LOOKAHEAD_WINDOW.
        assert!(covers(10, 20, 10)); // exactly at start
        assert!(covers(10, 20, 20)); // exactly at head
        assert!(covers(10, 20, 20 + LOOKAHEAD_WINDOW)); // edge of window
        // Out of range: before start (seek backward) → spawn own.
        assert!(!covers(10, 20, 9));
        // Out of range: far seek forward past the window → spawn own.
        assert!(!covers(10, 20, 20 + LOOKAHEAD_WINDOW + 1));
    }
}
