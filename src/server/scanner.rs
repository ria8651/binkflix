use super::analytics::{self, ScanTiming};
use super::filename;
use super::nfo::{self, EpisodeNfo, MovieNfo};
use super::{subtitles, thumbnails, trickplay};
use crate::types::{ActiveJob, ScanProgress};
use chrono::NaiveDateTime;
use futures::stream::{self, StreamExt};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;
use walkdir::WalkDir;

pub type ProgressHandle = Arc<RwLock<ScanProgress>>;

/// Cooperative cancellation handle for a library scan. Snapshots the
/// shared generation counter at construction; `is_cancelled` returns true
/// once the counter has been bumped (i.e. someone started a new scan).
///
/// The scan loop checks this between files and between asset jobs — far
/// enough apart that the overhead is negligible, tight enough that a
/// restart takes effect within one file (a fraction of a second on disk,
/// the duration of the slowest essential-pass probe at the outer bound).
#[derive(Clone)]
pub struct CancelToken {
    counter: Arc<AtomicU64>,
    my_gen: u64,
}

impl CancelToken {
    pub fn new(counter: Arc<AtomicU64>) -> Self {
        let my_gen = counter.load(Ordering::Acquire);
        Self { counter, my_gen }
    }
    pub fn is_cancelled(&self) -> bool {
        self.counter.load(Ordering::Acquire) != self.my_gen
    }
}

/// Bump `SHOW_SCAN_VERSION` / `MEDIA_SCAN_VERSION` whenever the corresponding
/// `upsert_*` function changes what it persists — e.g. starts writing a new
/// column, populating a join table, or reading a previously-ignored NFO
/// field. Existing rows have an older `scan_version` and the early-return
/// guard treats them as stale, forcing a re-upsert at the next scan.
///
/// The `*_VERSION` constants below are independent: each gates exactly one
/// extractor pass. Bumping `MEDIA_SCAN_VERSION` only re-runs the metadata
/// upsert — assets are untouched. Bumping `SUBTITLES_VERSION` only re-runs
/// the subtitle pass. This split exists because a metadata-column addition
/// shouldn't trigger an hour-long re-extract of trickplay sprites. See
/// migration 0017 for the per-asset columns.
const SHOW_SCAN_VERSION: i64 = 3;
const MEDIA_SCAN_VERSION: i64 = 3;
const SUBTITLES_VERSION: i64 = 1;
const THUMBNAILS_VERSION: i64 = 1;
const TRICKPLAY_VERSION: i64 = 1;
/// Embedded-chapter markers are a byproduct of the essential pass's probe, so
/// they're gated alongside subtitles via `needs_essential`. Bump this when the
/// chapter→marker classification changes to re-derive on unchanged files
/// without forcing a subtitle re-extract. (Audio-detected markers are gated
/// separately by `AUDIO_MARKERS_VERSION` since they're season-scoped.)
const MARKERS_VERSION: i64 = 1;
/// Gates the per-season audio-fingerprint pass. Bump to force re-analysis of
/// every season after a detection-algorithm change. Day-to-day re-analysis is
/// driven by fingerprint freshness (a new/changed episode), not this constant.
const AUDIO_MARKERS_VERSION: i64 = 1;

async fn set_progress(handle: &ProgressHandle, f: impl FnOnce(&mut ScanProgress)) {
    let mut p = handle.write().await;
    f(&mut p);
}

async fn add_active(handle: &ProgressHandle, media_id: &str, title: &str, stage: &str) {
    handle.write().await.active.push(ActiveJob {
        media_id: media_id.into(),
        title: title.into(),
        stage: stage.into(),
    });
}

const VIDEO_EXTENSIONS: &[&str] =
    &["mkv", "mp4", "m4v", "avi", "mov", "webm", "ts", "m2ts", "wmv", "flv"];

const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp"];

// --- filesystem helpers ---

fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| VIDEO_EXTENSIONS.iter().any(|v| v.eq_ignore_ascii_case(ext)))
        .unwrap_or(false)
}

fn sqlite_ts_to_secs(s: &str) -> Option<i64> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|dt| dt.and_utc().timestamp())
}

fn mtime_secs(p: &Path) -> i64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// File mtime as a SQLite-compatible UTC timestamp ("YYYY-MM-DD HH:MM:SS"),
/// falling back to "now" when the path can't be stat'd. Used as `added_at`
/// at INSERT time so the "Recently Added" row reflects when files actually
/// landed on disk, not when the scanner first noticed them.
fn file_added_at(path: &Path) -> String {
    let system_time = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok();
    let dt: chrono::DateTime<chrono::Utc> = match system_time {
        Some(t) => t.into(),
        None => chrono::Utc::now(),
    };
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// True if any file's mtime is newer than `last_scan`. Unparseable timestamps
/// force a re-index (safer than skipping something stale).
fn any_newer_than(files: &[&Path], last_scan: &str) -> bool {
    let Some(last) = sqlite_ts_to_secs(last_scan) else {
        return true;
    };
    files.iter().any(|p| mtime_secs(p) > last)
}

fn first_existing(dir: &Path, stems: &[&str]) -> Option<PathBuf> {
    for stem in stems {
        for ext in IMAGE_EXTS {
            let p = dir.join(format!("{stem}.{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn find_show_poster(show_dir: &Path) -> Option<PathBuf> {
    first_existing(show_dir, &["poster", "folder", "cover"])
}

fn find_show_fanart(show_dir: &Path) -> Option<PathBuf> {
    first_existing(show_dir, &["fanart", "backdrop"])
}

fn find_show_clearlogo(show_dir: &Path) -> Option<PathBuf> {
    first_existing(show_dir, &["clearlogo", "logo"])
}

fn find_show_banner(show_dir: &Path) -> Option<PathBuf> {
    first_existing(show_dir, &["banner"])
}

fn find_movie_image(video: &Path) -> Option<PathBuf> {
    let dir = video.parent()?;
    let base = video.file_stem()?.to_str()?;
    let owned = [
        format!("{base}-poster"),
        base.to_string(),
        "poster".to_string(),
        "folder".to_string(),
        "cover".to_string(),
    ];
    first_existing(dir, &owned.iter().map(String::as_str).collect::<Vec<_>>())
}

fn find_movie_fanart(video: &Path) -> Option<PathBuf> {
    let dir = video.parent()?;
    let base = video.file_stem()?.to_str()?;
    let owned = [
        format!("{base}-fanart"),
        "fanart".to_string(),
        "backdrop".to_string(),
    ];
    first_existing(dir, &owned.iter().map(String::as_str).collect::<Vec<_>>())
}

fn find_episode_thumb(video: &Path) -> Option<PathBuf> {
    let dir = video.parent()?;
    let base = video.file_stem()?.to_str()?;
    let owned = [format!("{base}-thumb")];
    first_existing(dir, &owned.iter().map(String::as_str).collect::<Vec<_>>())
}

fn matching_nfo(video: &Path) -> Option<PathBuf> {
    let candidate = video.with_extension("nfo");
    if candidate.is_file() {
        return Some(candidate);
    }
    let parent = video.parent()?;
    let movie = parent.join("movie.nfo");
    if movie.is_file() { Some(movie) } else { None }
}

/// Nearest ancestor containing `tvshow.nfo`, up to `library_root`.
fn find_show_folder(video: &Path, library_root: &Path) -> Option<PathBuf> {
    let mut cur = video.parent();
    while let Some(dir) = cur {
        if dir.join("tvshow.nfo").is_file() {
            return Some(dir.to_path_buf());
        }
        if dir == library_root {
            break;
        }
        cur = dir.parent();
    }
    None
}

// --- classification ---

enum Classification {
    Episode(PathBuf),
    Movie,
}

fn classify(video: &Path, library_root: &Path) -> Classification {
    let nfo_sibling = video.with_extension("nfo");
    let nfo_kind = nfo_sibling
        .is_file()
        .then(|| nfo::detect_nfo_kind(&nfo_sibling))
        .flatten();

    if let Some(nfo::NfoKind::Movie) = nfo_kind {
        return Classification::Movie;
    }

    if let Some(nfo::NfoKind::Episode) = nfo_kind {
        let show_dir = find_show_folder(video, library_root)
            .or_else(|| video.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| library_root.to_path_buf());
        return Classification::Episode(show_dir);
    }

    if let Some(dir) = find_show_folder(video, library_root) {
        return Classification::Episode(dir);
    }

    if let Some(stem) = video.file_stem().and_then(|s| s.to_str()) {
        if filename::parse_episode(stem).is_some() {
            if let Some(parent) = video.parent() {
                if parent != library_root {
                    return Classification::Episode(parent.to_path_buf());
                }
            }
        }
    }

    Classification::Movie
}

// --- public entry ---

#[derive(Default, Debug)]
pub struct ScanStats {
    pub movies_indexed: usize,
    pub movies_skipped: usize,
    pub episodes_indexed: usize,
    pub episodes_skipped: usize,
    pub shows_indexed: usize,
    pub shows_skipped: usize,
}

pub async fn ensure_library(pool: &SqlitePool, name: &str, path: &Path) -> anyhow::Result<i64> {
    let path_str = path.canonicalize()?.to_string_lossy().into_owned();

    if let Some((id, deleted_at)) =
        sqlx::query_as::<_, (i64, Option<String>)>(
            "SELECT id, deleted_at FROM libraries WHERE path = ?",
        )
        .bind(&path_str)
        .fetch_optional(pool)
        .await?
    {
        // Resurrect a previously-soft-deleted library and any of its
        // shows/media that were soft-deleted by the same library-prune.
        // Per-file prunes are re-applied later in prune_missing.
        if deleted_at.is_some() {
            sqlx::query("UPDATE libraries SET deleted_at = NULL WHERE id = ?")
                .bind(id)
                .execute(pool)
                .await?;
            sqlx::query("UPDATE shows SET deleted_at = NULL WHERE library_id = ?")
                .bind(id)
                .execute(pool)
                .await?;
            sqlx::query("UPDATE media SET deleted_at = NULL WHERE library_id = ?")
                .bind(id)
                .execute(pool)
                .await?;
            info!(library_id = id, %path_str, "restored soft-deleted library");
        }
        return Ok(id);
    }

    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO libraries (name, path) VALUES (?, ?) RETURNING id",
    )
    .bind(name)
    .bind(&path_str)
    .fetch_one(pool)
    .await?;

    Ok(id)
}

/// Work item carried from the index pass into the asset pass. Each
/// `needs_*` flag gates its own pass — a job can need just one asset
/// re-extracted, not all three. `needs_essential` covers probe +
/// probe_json + subtitles + content-signature stamp.
struct AssetJob {
    media_id: String,
    video: PathBuf,
    title: String,
    has_sidecar_image: bool,
    needs_essential: bool,
    needs_thumbnails: bool,
    needs_trickplay: bool,
}

/// Populate `added_at` on rows that pre-date the column (NULL). Uses the
/// file/folder mtime — the closest proxy we have for "when the user copied
/// this in" — and falls back to `scanned_at` when the path is gone. No-op
/// once every row has been backfilled.
async fn backfill_added_at(pool: &SqlitePool) -> anyhow::Result<()> {
    let shows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT id, path, scanned_at FROM shows WHERE added_at IS NULL")
            .fetch_all(pool)
            .await?;
    for (id, path, scanned_at) in shows {
        let added = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .map(|t| {
                let dt: chrono::DateTime<chrono::Utc> = t.into();
                dt.format("%Y-%m-%d %H:%M:%S").to_string()
            })
            .unwrap_or(scanned_at);
        sqlx::query("UPDATE shows SET added_at = ? WHERE id = ?")
            .bind(added)
            .bind(id)
            .execute(pool)
            .await?;
    }

    let media: Vec<(String, String, String)> =
        sqlx::query_as("SELECT id, path, scanned_at FROM media WHERE added_at IS NULL")
            .fetch_all(pool)
            .await?;
    for (id, path, scanned_at) in media {
        let added = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .map(|t| {
                let dt: chrono::DateTime<chrono::Utc> = t.into();
                dt.format("%Y-%m-%d %H:%M:%S").to_string()
            })
            .unwrap_or(scanned_at);
        sqlx::query("UPDATE media SET added_at = ? WHERE id = ?")
            .bind(added)
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

pub async fn scan_library_with_progress(
    pool: &SqlitePool,
    library_id: i64,
    root: &Path,
    progress: Option<ProgressHandle>,
    cancel: Option<CancelToken>,
) -> anyhow::Result<ScanStats> {
    let started = std::time::Instant::now();
    info!(path = %root.display(), "scanning library");
    backfill_added_at(pool).await?;
    let root_display = root.display().to_string();
    if let Some(p) = &progress {
        set_progress(p, |s| {
            s.phase = "indexing".into();
            s.done = 0;
            s.total = 0;
            s.current = Some(root_display.clone());
        })
        .await;
    }
    let root = root.canonicalize()?;

    let mut show_ids: HashMap<PathBuf, String> = HashMap::new();
    let mut stats = ScanStats::default();

    // Dev escape hatch: cap the number of videos processed per scan so a huge
    // library doesn't slow down iteration. Unset/0 means no cap.
    let max_videos: Option<usize> = std::env::var("BINKFLIX_MAX_SCAN")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0);

    // Parallelism for the asset (subs + thumb) pass. Default is conservative
    // because many users have their media on slow-random-access storage
    // (NAS, USB disk) where aggressive concurrency thrashes the drive.
    let concurrency: usize = std::env::var("BINKFLIX_SCAN_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(4);

    let mut videos_seen: usize = 0;
    let mut asset_jobs: Vec<AssetJob> = Vec::new();
    // Paths we saw during this walk — used after phase 1 to prune rows for
    // files/shows that no longer exist on disk. Only safe to act on this
    // when the walk completed naturally (no MAX_SCAN short-circuit).
    let mut seen_media_paths: HashSet<String> = HashSet::new();
    let mut walk_completed = true;

    // --- Phase 1: walk the library and upsert every media/show row. Fast;
    // finishes before the user has loaded the home page. Asset extraction
    // is deferred to phase 2 so the library becomes browseable immediately.
    //
    // `follow_links(false)`: a symlink inside the library could otherwise
    // point at anywhere on disk (`movie.mkv → /etc/passwd`), and the
    // canonical resolution would land verbatim in `media.path` and be
    // served back via `/api/media/{id}/stream`. Don't follow.
    for entry in WalkDir::new(&root).follow_links(false).into_iter().flatten() {
        if let Some(c) = &cancel {
            if c.is_cancelled() {
                info!("scan cancelled mid-walk");
                walk_completed = false;
                break;
            }
        }
        let path = entry.path();
        if !entry.file_type().is_file() || !is_video(path) {
            continue;
        }
        if let Some(max) = max_videos {
            if videos_seen >= max {
                info!(max, "BINKFLIX_MAX_SCAN reached; stopping scan early");
                walk_completed = false;
                break;
            }
        }
        videos_seen += 1;
        if let Some(p) = &progress {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            set_progress(p, |s| {
                s.done = videos_seen;
                s.current = Some(name);
            })
            .await;
        }

        let abs = match path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                warn!(?path, %e, "skipping unreadable path");
                continue;
            }
        };
        let file_size = entry.metadata().map(|m| m.len() as i64).unwrap_or(0);
        seen_media_paths.insert(abs.to_string_lossy().into_owned());

        let outcome = match classify(&abs, &root) {
            Classification::Episode(show_dir) => {
                let show_id = match show_ids.get(&show_dir) {
                    Some(id) => id.clone(),
                    None => {
                        let (id, indexed) = upsert_show(pool, library_id, &show_dir).await?;
                        if indexed { stats.shows_indexed += 1; } else { stats.shows_skipped += 1; }
                        show_ids.insert(show_dir.clone(), id.clone());
                        id
                    }
                };
                match upsert_episode(pool, library_id, &show_id, &show_dir, &abs, file_size).await {
                    Ok(Some(out)) => {
                        if out.re_indexed { stats.episodes_indexed += 1; } else { stats.episodes_skipped += 1; }
                        Some(out)
                    }
                    Ok(None) => { stats.episodes_skipped += 1; None }
                    Err(e) => { warn!(path = %abs.display(), %e, "failed to index episode"); None }
                }
            }
            Classification::Movie => {
                match upsert_movie(pool, library_id, &abs, file_size).await {
                    Ok(Some(out)) => {
                        if out.re_indexed { stats.movies_indexed += 1; } else { stats.movies_skipped += 1; }
                        Some(out)
                    }
                    Ok(None) => { stats.movies_skipped += 1; None }
                    Err(e) => { warn!(path = %abs.display(), %e, "failed to index movie"); None }
                }
            }
        };

        if let Some(out) = outcome {
            if out.needs_essential || out.needs_thumbnails || out.needs_trickplay {
                let title = abs
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                asset_jobs.push(AssetJob {
                    media_id: out.id,
                    video: abs,
                    title,
                    has_sidecar_image: out.has_sidecar_image,
                    needs_essential: out.needs_essential,
                    needs_thumbnails: out.needs_thumbnails,
                    needs_trickplay: out.needs_trickplay,
                });
            }
        }
    }

    // --- Prune: rows for files (or whole shows) that no longer exist on
    // disk. Skipped when MAX_SCAN cut the walk short — we can't distinguish
    // "deleted from disk" from "beyond the dev cap". Cascading FKs handle
    // subtitles/thumbnails/genres; shows go via a separate pass after media
    // so we never orphan a referenced show.
    if walk_completed {
        let removed = prune_missing(pool, library_id, &seen_media_paths).await?;
        if removed > 0 {
            info!(removed, "pruned rows for deleted files");
        }
    }

    let index_elapsed_ms = started.elapsed().as_millis() as u64;
    info!(
        movies_indexed = stats.movies_indexed,
        episodes_indexed = stats.episodes_indexed,
        pending_assets = asset_jobs.len(),
        index_elapsed_ms,
        "library indexed — extracting assets",
    );

    // --- Phase 2: stage-by-stage passes prioritised by user value.
    //
    // Subtitles are the only asset that unlocks playability, so we finish
    // them for *every* file before any thumbnail or trickplay sprite is
    // touched. Likewise thumbnails (browse-page eye candy) before trickplay
    // (scrub-bar polish). Within each pass we still run up to `concurrency`
    // files in parallel, just at the same stage.
    //
    // Trade-off: a file that would have been "fully done" 30 seconds in
    // (under the old per-file pipeline) now waits until pass 3 to get its
    // trickplay. The win is the global ordering — playable library faster.
    let total = asset_jobs.len();
    if total > 0 {
        let assets_started = std::time::Instant::now();
        let pool = pool.clone();
        let progress = progress.clone();

        // Per-file accumulator threaded between the three asset passes.
        // `tech_info` is captured in pass 1 (essential) so pass 2/3 don't
        // need to re-probe — they reuse codec/resolution/duration for
        // analytics and for `trickplay::scan_for_media`'s duration hint.
        // Each pass writes its own `scan_timings` row inline (tagged by
        // `trigger`) so a mid-scan restart only loses the actively-running
        // pass for in-flight files instead of every per-file row that
        // hadn't yet reached a final "save" pass.
        struct PerFile {
            job: AssetJob,
            tech_info: Option<crate::types::MediaTechInfo>,
        }

        // -- Pass 1: probe + subtitles + content signature ----------------
        // Calls the shared `run_essential` helper so the validate-on-read
        // refresh path and the library scan stay aligned. The signature
        // gets stamped inside that helper; pass-2/3 still consult the
        // per-job needs flags.
        let essential_total = asset_jobs.iter().filter(|j| j.needs_essential).count();
        if let Some(p) = &progress {
            set_progress(p, |s| {
                s.phase = "subtitles".into();
                s.done = 0;
                s.total = essential_total;
                s.current = None;
                s.active.clear();
            })
            .await;
        }
        let done_p1 = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pass1: Vec<PerFile> = stream::iter(asset_jobs.into_iter())
            .map(|job| {
                let pool = pool.clone();
                let progress = progress.clone();
                let done = done_p1.clone();
                let cancel = cancel.clone();
                async move {
                    let skip = !job.needs_essential
                        || cancel.as_ref().map(|c| c.is_cancelled()).unwrap_or(false);
                    if skip {
                        return PerFile { job, tech_info: None };
                    }
                    if let Some(p) = &progress {
                        add_active(p, &job.media_id, &job.title, "subtitles").await;
                    }
                    let started = std::time::Instant::now();
                    let outcome = run_essential(&pool, &job.media_id, &job.video).await;
                    let total_ms = started.elapsed().as_millis() as u64;

                    record_essential_timing(&pool, &job.media_id, &outcome, total_ms).await;

                    let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if let Some(p) = &progress {
                        let title = job.title.clone();
                        let media_id = job.media_id.clone();
                        set_progress(p, |s| {
                            s.done = n;
                            s.current = Some(title);
                            s.active.retain(|j| j.media_id != media_id);
                        })
                        .await;
                    }
                    debug!(
                        progress = format!("{n}/{essential_total}"),
                        title = %job.title,
                        subs = outcome.sub_tracks,
                        elapsed_ms = outcome.subtitles_ms,
                        "subtitles done",
                    );
                    PerFile { job, tech_info: outcome.tech_info }
                }
            })
            .buffer_unordered(concurrency)
            .collect()
            .await;
        info!(
            total = essential_total,
            elapsed_ms = assets_started.elapsed().as_millis() as u64,
            "subtitles pass complete",
        );

        // -- Pass 2: thumbnails ------------------------------------------
        let thumbnails_total = pass1.iter().filter(|f| f.job.needs_thumbnails).count();
        if let Some(p) = &progress {
            set_progress(p, |s| {
                s.phase = "thumbnails".into();
                s.done = 0;
                s.total = thumbnails_total;
                s.active.clear();
            })
            .await;
        }
        let done_p2 = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pass2: Vec<PerFile> = stream::iter(pass1.into_iter())
            .map(|f| {
                let pool = pool.clone();
                let progress = progress.clone();
                let done = done_p2.clone();
                let cancel = cancel.clone();
                async move {
                    if !f.job.needs_thumbnails
                        || cancel.as_ref().map(|c| c.is_cancelled()).unwrap_or(false)
                    {
                        return f;
                    }
                    // Sidecar rows skip ffmpeg but still bump the version, so
                    // a future THUMBNAILS_VERSION bump doesn't permanently
                    // re-trip them. API endpoints prefer image_path anyway.
                    let thumbnail_ms = if f.job.has_sidecar_image {
                        0
                    } else {
                        if let Some(p) = &progress {
                            add_active(p, &f.job.media_id, &f.job.title, "thumbnail").await;
                        }
                        let t = std::time::Instant::now();
                        thumbnails::scan_for_media(&pool, &f.job.media_id, &f.job.video).await;
                        t.elapsed().as_millis() as u64
                    };
                    if let Err(e) = sqlx::query(
                        "UPDATE media SET thumbnails_version = ? WHERE id = ?",
                    )
                    .bind(THUMBNAILS_VERSION)
                    .bind(&f.job.media_id)
                    .execute(&pool)
                    .await
                    {
                        warn!(media_id = %f.job.media_id, %e, "failed to update thumbnails_version");
                    }
                    record_thumbnail_timing(
                        &pool,
                        &f.job.media_id,
                        f.tech_info.as_ref(),
                        thumbnail_ms,
                    )
                    .await;
                    let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if let Some(p) = &progress {
                        let title = f.job.title.clone();
                        let media_id = f.job.media_id.clone();
                        set_progress(p, |s| {
                            s.done = n;
                            s.current = Some(title);
                            s.active.retain(|j| j.media_id != media_id);
                        })
                        .await;
                    }
                    f
                }
            })
            .buffer_unordered(concurrency)
            .collect()
            .await;
        info!(
            total = thumbnails_total,
            elapsed_ms = assets_started.elapsed().as_millis() as u64,
            "thumbnails pass complete",
        );

        // -- Pass 3: trickplay -------------------------------------------
        let trickplay_total = pass2.iter().filter(|f| f.job.needs_trickplay).count();
        if let Some(p) = &progress {
            set_progress(p, |s| {
                s.phase = "trickplay".into();
                s.done = 0;
                s.total = trickplay_total;
                s.active.clear();
            })
            .await;
        }
        let done_p3 = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        stream::iter(pass2.into_iter())
            .map(|f| {
                let pool = pool.clone();
                let progress = progress.clone();
                let done = done_p3.clone();
                let cancel = cancel.clone();
                async move {
                    if !f.job.needs_trickplay
                        || cancel.as_ref().map(|c| c.is_cancelled()).unwrap_or(false)
                    {
                        return;
                    }
                    if let Some(p) = &progress {
                        add_active(p, &f.job.media_id, &f.job.title, "trickplay").await;
                    }
                    // Duration normally comes from pass-1's probe. When only
                    // trickplay is stale (pass 1 was skipped), fall back to
                    // the cached probe data — the file is unchanged by
                    // definition, so the stored value is authoritative.
                    let duration = match f.tech_info.as_ref().and_then(|i| i.duration_seconds) {
                        Some(d) => Some(d),
                        None => match super::media_info::load(&pool, &f.job.media_id).await {
                            Ok(Some(info)) => info.duration_seconds,
                            _ => None,
                        },
                    };
                    let t = std::time::Instant::now();
                    let keyframe_count = trickplay::scan_for_media(
                        &pool,
                        &f.job.media_id,
                        &f.job.video,
                        duration,
                    )
                    .await;
                    let trickplay_ms = t.elapsed().as_millis() as u64;
                    if let Err(e) = sqlx::query(
                        "UPDATE media SET trickplay_version = ? WHERE id = ?",
                    )
                    .bind(TRICKPLAY_VERSION)
                    .bind(&f.job.media_id)
                    .execute(&pool)
                    .await
                    {
                        warn!(media_id = %f.job.media_id, %e, "failed to update trickplay_version");
                    }
                    record_trickplay_timing(
                        &pool,
                        &f.job.media_id,
                        f.tech_info.as_ref(),
                        trickplay_ms,
                        keyframe_count,
                    )
                    .await;
                    let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if let Some(p) = &progress {
                        let title = f.job.title.clone();
                        let media_id = f.job.media_id.clone();
                        set_progress(p, |s| {
                            s.done = n;
                            s.current = Some(title);
                            s.active.retain(|j| j.media_id != media_id);
                        })
                        .await;
                    }
                    info!(
                        progress = format!("{n}/{trickplay_total}"),
                        title = %f.job.title,
                        elapsed_ms = trickplay_ms,
                        "assets extracted",
                    );
                }
            })
            .buffer_unordered(concurrency)
            .for_each(|_| async {})
            .await;

        info!(
            total,
            assets_elapsed_ms = assets_started.elapsed().as_millis() as u64,
            "asset extraction complete",
        );
    }

    // -- Phase 4: audio-fingerprint intro/outro detection (season-scoped) ----
    // Runs independently of the per-file asset passes: it correlates whole
    // seasons, so it can't live inside a per-file loop. No-op when `fpcalc`
    // is unavailable, and gated per season on fingerprint freshness so a
    // stable library re-scans for cheap.
    run_audio_match_pass(pool, library_id, concurrency, &cancel, &progress).await;

    info!(
        ?stats,
        total_elapsed_ms = started.elapsed().as_millis() as u64,
        "scan complete"
    );
    Ok(stats)
}

/// Phase 4 orchestration: find this library's multi-episode seasons and run
/// audio-fingerprint detection on the ones that need it. Best-effort — a
/// failure on one season is logged and the rest continue.
async fn run_audio_match_pass(
    pool: &SqlitePool,
    library_id: i64,
    concurrency: usize,
    cancel: &Option<CancelToken>,
    progress: &Option<ProgressHandle>,
) {
    use super::markers::{self, FpcalcStatus};
    if markers::fpcalc_status().await != FpcalcStatus::Available {
        return;
    }

    // Seasons with ≥2 file-backed episodes — the quorum the detector needs.
    // Show title comes along so the progress UI can name the season it's on.
    let seasons: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT m.show_id, m.season_number, COALESCE(s.title, 'Show')
         FROM media m
         JOIN shows s ON s.id = m.show_id
         WHERE m.library_id = ? AND m.kind = 'episode' AND m.deleted_at IS NULL
               AND m.show_id IS NOT NULL AND m.season_number IS NOT NULL
         GROUP BY m.show_id, m.season_number
         HAVING COUNT(*) >= 2",
    )
    .bind(library_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    if seasons.is_empty() {
        return;
    }

    let total = seasons.len();
    if let Some(p) = progress {
        set_progress(p, |s| {
            s.phase = "audio-match".into();
            s.done = 0;
            s.total = total;
            s.current = None;
            s.active.clear();
        })
        .await;
    }

    // Seasons are independent — each fingerprints + correlates its own
    // episodes — so analyse up to `concurrency` at once, mirroring the
    // per-file asset passes. The active list names every season in flight;
    // `done` ticks up as each finishes. `media_id` carries the show id so
    // the row can be retired by id (a show can have >1 season in flight).
    let done = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    stream::iter(seasons.into_iter())
        .map(|(show_id, season, title)| {
            let pool = pool.clone();
            let progress = progress.clone();
            let cancel = cancel.clone();
            let done = done.clone();
            async move {
                if cancel.as_ref().map(|c| c.is_cancelled()).unwrap_or(false) {
                    return;
                }
                let label = format!("{title} — Season {season}");
                if let Some(p) = &progress {
                    add_active(p, &show_id, &label, "analysing").await;
                }
                if let Err(e) = analyze_one_season(&pool, &show_id, season, &cancel).await {
                    warn!(%show_id, season, %e, "audio-match: season analysis failed");
                }
                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if let Some(p) = &progress {
                    let sid = show_id.clone();
                    set_progress(p, |s| {
                        s.done = n;
                        s.active.retain(|j| j.media_id != sid);
                    })
                    .await;
                }
            }
        })
        .buffer_unordered(concurrency)
        .for_each(|_| async {})
        .await;
}

/// Fingerprint + correlate one season, storing `audio`-source markers. Skips
/// when nothing changed since the last analysis (all members have a current
/// fingerprint and an up-to-date `audio_markers_version`).
async fn analyze_one_season(
    pool: &SqlitePool,
    show_id: &str,
    season: i64,
    cancel: &Option<CancelToken>,
) -> anyhow::Result<()> {
    type Row = (
        String,      // media id
        String,      // path
        Option<i64>, // media.content_mtime
        Option<i64>, // media.content_size
        i64,         // media.audio_markers_version
        Option<i64>, // fingerprint.content_mtime
        Option<i64>, // fingerprint.content_size
        Option<i64>, // fingerprint.fp_algo_version
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT m.id, m.path, m.content_mtime, m.content_size, m.audio_markers_version,
                f.content_mtime, f.content_size, f.fp_algo_version
         FROM media m
         LEFT JOIN media_fingerprints f ON f.media_id = m.id
         WHERE m.show_id = ? AND m.season_number = ? AND m.kind = 'episode'
               AND m.deleted_at IS NULL
         ORDER BY m.episode_number",
    )
    .bind(show_id)
    .bind(season)
    .fetch_all(pool)
    .await?;
    if rows.len() < 2 {
        return Ok(());
    }
    if rows.len() > super::markers::MAX_SEASON_EPISODES {
        warn!(
            %show_id, season, count = rows.len(), cap = super::markers::MAX_SEASON_EPISODES,
            "audio-match: season exceeds size cap; skipping"
        );
        return Ok(());
    }

    // Re-analyse only if a member is new/changed (no current fingerprint) or
    // the algorithm version moved.
    let needs = rows.iter().any(|(_, _, mm, ms, amv, fm, fs, fv)| {
        let fp_current =
            mm.is_some() && mm == fm && ms == fs && *fv == Some(super::markers::FP_ALGO_VERSION);
        !fp_current || *amv < AUDIO_MARKERS_VERSION
    });
    if !needs {
        return Ok(());
    }

    let mut eps: Vec<super::markers::SeasonEpisode> = Vec::with_capacity(rows.len());
    for (id, path, mm, ms, _, _, _, _) in &rows {
        if cancel.as_ref().map(|c| c.is_cancelled()).unwrap_or(false) {
            return Ok(());
        }
        let video = Path::new(path);
        let sig = match (mm, ms) {
            (Some(m), Some(s)) => (*m, *s),
            _ => stat_signature(video),
        };
        let fp = match super::markers::ensure_fingerprint(pool, id, video, sig).await {
            Ok(fp) if !fp.is_empty() => fp,
            Ok(_) => {
                warn!(%id, "audio-match: empty fingerprint; skipping episode");
                continue;
            }
            Err(e) => {
                warn!(%id, %e, "audio-match: fingerprint failed; skipping episode");
                continue;
            }
        };
        let duration = match super::media_info::load(pool, id).await {
            Ok(Some(info)) => info.duration_seconds.unwrap_or(0.0),
            _ => 0.0,
        };
        eps.push(super::markers::SeasonEpisode { media_id: id.clone(), duration, fp });
    }
    if eps.len() < 2 {
        return Ok(());
    }

    let analyzed = super::markers::analyze_season(&eps);
    let analyzed_ids: std::collections::HashSet<&str> =
        analyzed.iter().map(|(id, _)| id.as_str()).collect();
    let mut total_markers = 0usize;
    for (media_id, markers) in &analyzed {
        total_markers += markers.len();
        if let Err(e) = super::markers::store_markers(pool, media_id, "audio", markers).await {
            warn!(%media_id, %e, "audio-match: failed to store markers");
        }
    }

    // Clear stale `audio` markers for any season member we couldn't fingerprint
    // this run (e.g. its file was replaced with one fpcalc can't process). Left
    // alone they'd keep pointing the skip button at old content and never
    // self-heal, since the failing fingerprint also blocks re-analysis.
    for (id, ..) in &rows {
        if !analyzed_ids.contains(id.as_str()) {
            if let Err(e) = super::markers::store_markers(pool, id, "audio", &[]).await {
                warn!(%id, %e, "audio-match: failed to clear stale markers");
            }
        }
    }

    // Stamp every member so an unchanged season skips next scan. Done even
    // for members that failed to fingerprint — they'll re-trip via the
    // fingerprint-freshness check (no row) on the next run anyway.
    for (id, ..) in &rows {
        let _ = sqlx::query("UPDATE media SET audio_markers_version = ? WHERE id = ?")
            .bind(AUDIO_MARKERS_VERSION)
            .bind(id.as_str())
            .execute(pool)
            .await;
    }
    info!(%show_id, season, episodes = eps.len(), markers = total_markers, "audio-match: season analysed");
    Ok(())
}

// --- prune ---

/// Soft-delete `libraries` rows (and propagate to their shows + media)
/// whose id isn't in `active_ids`. Called at startup after registering
/// the currently-configured library paths. Rows are kept on disk so
/// watch history survives an accidental config change; `binkflix cleanup
/// --apply` can purge them later.
pub async fn prune_libraries(pool: &SqlitePool, active_ids: &[i64]) -> anyhow::Result<u64> {
    // Refuse to wipe everything if the env var is empty — callers already
    // bail before reaching here, but belt-and-braces.
    if active_ids.is_empty() {
        return Ok(0);
    }
    // Fetch + filter in Rust rather than building a dynamic `NOT IN (?, ?, …)`
    // binding list. N is tiny (one per configured library path).
    let all: Vec<(i64,)> =
        sqlx::query_as("SELECT id FROM libraries WHERE deleted_at IS NULL")
            .fetch_all(pool)
            .await?;
    let mut removed: u64 = 0;
    for (id,) in all {
        if active_ids.contains(&id) {
            continue;
        }
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let res = sqlx::query(
            "UPDATE libraries SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL",
        )
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?;
        removed += res.rows_affected();
        // Propagate so a single per-table `deleted_at IS NULL` filter on
        // reads covers everything — no joins back to libraries needed.
        sqlx::query(
            "UPDATE shows SET deleted_at = ? WHERE library_id = ? AND deleted_at IS NULL",
        )
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?;
        sqlx::query(
            "UPDATE media SET deleted_at = ? WHERE library_id = ? AND deleted_at IS NULL",
        )
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?;
        debug!(library_id = id, "soft-deleted library");
    }
    Ok(removed)
}


/// Soft-delete media rows in this library whose path wasn't seen during the
/// walk, then soft-delete any show whose entire episode set has vanished.
///
/// Returns the total number of rows soft-deleted. Watch history and other
/// related rows are preserved; rows can be resurrected by the upsert path
/// if the file reappears, and purged for real via `binkflix cleanup --apply`.
async fn prune_missing(
    pool: &SqlitePool,
    library_id: i64,
    seen: &HashSet<String>,
) -> anyhow::Result<u64> {
    let existing: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, path FROM media WHERE library_id = ? AND deleted_at IS NULL",
    )
    .bind(library_id)
    .fetch_all(pool)
    .await?;

    let to_delete: Vec<String> = existing
        .into_iter()
        .filter(|(_, p)| !seen.contains(p))
        .map(|(id, _)| id)
        .collect();

    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mut removed: u64 = 0;
    for id in &to_delete {
        let res = sqlx::query(
            "UPDATE media SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL",
        )
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?;
        removed += res.rows_affected();
        debug!(media_id = %id, "soft-deleted media");
    }

    // Shows whose every non-soft-deleted episode is gone. A show with zero
    // live episodes is the case we want to act on; previously-soft-deleted
    // episodes don't count toward "still alive".
    let orphan_shows: Vec<(String,)> = sqlx::query_as(
        "SELECT id FROM shows
         WHERE library_id = ?
           AND deleted_at IS NULL
           AND NOT EXISTS (
               SELECT 1 FROM media
                WHERE media.show_id = shows.id AND media.deleted_at IS NULL
           )",
    )
    .bind(library_id)
    .fetch_all(pool)
    .await?;

    for (id,) in &orphan_shows {
        let res = sqlx::query(
            "UPDATE shows SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL",
        )
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?;
        removed += res.rows_affected();
        debug!(show_id = %id, "soft-deleted empty show");
    }

    Ok(removed)
}

// --- single-file essential + cosmetic refresh (shared by scan + validate-on-read) ---

/// Result of a single-file essential-pass derivation — i.e. the things we
/// need on disk *before* playback (probe_json, subtitles) plus the content
/// signature. The library scan keeps these around to feed pass 4's
/// analytics row; the single-file refresh logs them inline and drops them.
pub struct EssentialOutcome {
    pub tech_info: Option<crate::types::MediaTechInfo>,
    pub sub_tracks: u32,
    pub probe_ms: u64,
    pub subtitles_ms: u64,
    /// `(mtime, size)` captured at the start of the essential pass — what
    /// we stamp on the row at the end, so any change *during* the probe
    /// forces another refresh on the next read.
    pub signature: (i64, i64),
}

/// Run the essential pass for one file: probe, persist `probe_json`,
/// (re)extract subtitles, bump `subtitles_version`, stamp the content
/// signature. The signature is captured *before* the probe so a file swap
/// during the probe naturally re-triggers on the next read. Logs but
/// doesn't return individual extractor errors — failure to extract
/// subtitles still produces an outcome (the audio button will still work).
async fn run_essential(pool: &SqlitePool, media_id: &str, video: &Path) -> EssentialOutcome {
    let signature = stat_signature(video);

    let t = std::time::Instant::now();
    let (tech_info, embedded_subs, chapters) = match super::media_info::probe_full(video).await {
        Ok((info, subs, chapters)) => (Some(info), subs, chapters),
        Err(e) => {
            warn!(%media_id, %e, "ffprobe failed");
            (None, Vec::new(), Vec::new())
        }
    };
    let probe_ms = t.elapsed().as_millis() as u64;

    if let Some(info) = tech_info.as_ref() {
        if let Err(e) = super::media_info::store(pool, media_id, info).await {
            warn!(%media_id, %e, "failed to cache tech info");
        }
    }

    // Embedded-chapter markers ride along with the probe (free). Replace only
    // the `chapter`-source rows so audio-detected markers (a separate, season-
    // scoped producer) survive an essential re-run.
    let duration = tech_info.as_ref().and_then(|t| t.duration_seconds).unwrap_or(0.0);
    let chapter_markers = super::markers::chapters_to_markers(&chapters, duration);
    if let Err(e) = super::markers::store_markers(pool, media_id, "chapter", &chapter_markers).await
    {
        warn!(%media_id, %e, "failed to store chapter markers");
    }

    let t = std::time::Instant::now();
    if let Err(e) = subtitles::scan_for_media(pool, media_id, video, &embedded_subs).await {
        warn!(%media_id, %e, "subtitle scan failed");
    }
    let subtitles_ms = t.elapsed().as_millis() as u64;

    // Bump the per-row version unconditionally — matches today's policy where
    // version was written at upsert time regardless of whether asset
    // extraction succeeded. Failures don't auto-retry; the user re-triggers
    // with a version bump.
    if let Err(e) = sqlx::query(
        "UPDATE media SET subtitles_version = ?,
                          markers_version  = ?,
                          content_mtime    = ?,
                          content_size     = ?
         WHERE id = ?",
    )
    .bind(SUBTITLES_VERSION)
    .bind(MARKERS_VERSION)
    .bind(signature.0)
    .bind(signature.1)
    .bind(media_id)
    .execute(pool)
    .await
    {
        warn!(%media_id, %e, "failed to stamp content signature / subtitles_version");
    }

    EssentialOutcome {
        tech_info,
        sub_tracks: embedded_subs.len() as u32,
        probe_ms,
        subtitles_ms,
        signature,
    }
}

/// (mtime_secs, file_size) for `video`. Both zero if the file isn't
/// readable — the caller still stamps that signature so a later read
/// sees the mismatch and re-triggers.
fn stat_signature(video: &Path) -> (i64, i64) {
    match std::fs::metadata(video) {
        Ok(m) => {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            (mtime, m.len() as i64)
        }
        Err(_) => (0, 0),
    }
}

/// Pulled out so every `scan_timings` insert (essential, thumbnail,
/// trickplay, stale_read) carries the same source-side columns and a
/// later analyst can correlate per-stage timings against codec /
/// resolution / bitrate without re-probing.
struct SourceFields {
    video_codec: Option<String>,
    audio_codec: Option<String>,
    container: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    duration_ms: Option<u64>,
    bitrate_kbps: Option<u64>,
    pixel_format: Option<String>,
}

fn source_fields(info: Option<&crate::types::MediaTechInfo>) -> SourceFields {
    let Some(info) = info else {
        return SourceFields {
            video_codec: None,
            audio_codec: None,
            container: None,
            width: None,
            height: None,
            duration_ms: None,
            bitrate_kbps: None,
            pixel_format: None,
        };
    };
    let v = info.video.as_ref();
    // Default audio = the track flagged `default`, else the first,
    // matching how `compute_compat` picks one for the verdict.
    let a = info
        .audio
        .iter()
        .find(|a| a.default)
        .or_else(|| info.audio.first());
    SourceFields {
        video_codec: v.map(|v| v.codec.clone()),
        audio_codec: a.map(|a| a.codec.clone()),
        container: info.container.clone(),
        width: v.and_then(|v| v.width),
        height: v.and_then(|v| v.height),
        duration_ms: info.duration_seconds.map(|s| (s * 1000.0) as u64),
        bitrate_kbps: info.bitrate_kbps,
        pixel_format: v.and_then(|v| v.pix_fmt.clone()),
    }
}

/// Per-pass `scan_timings` write — one row per pass per file, so a
/// mid-scan restart only loses the actively-running pass for in-flight
/// files instead of the whole per-file accumulator (the old pass-4 design
/// dropped everything not yet "saved"). The `trigger` tag identifies the
/// pass; non-applicable timing columns are 0.
async fn record_essential_timing(
    pool: &SqlitePool,
    media_id: &str,
    outcome: &EssentialOutcome,
    total_ms: u64,
) {
    let s = source_fields(outcome.tech_info.as_ref());
    analytics::record_scan_timing(
        pool,
        media_id,
        ScanTiming {
            probe_ms: outcome.probe_ms,
            subtitles_ms: outcome.subtitles_ms,
            subtitle_tracks: outcome.sub_tracks,
            thumbnail_ms: 0,
            trickplay_ms: 0,
            save_ms: 0,
            total_ms,
            video_codec: s.video_codec,
            audio_codec: s.audio_codec,
            container: s.container,
            width: s.width,
            height: s.height,
            duration_ms: s.duration_ms,
            bitrate_kbps: s.bitrate_kbps,
            pixel_format: s.pixel_format,
            keyframe_count: None,
            trigger: "scan_essential",
        },
    )
    .await;
}

async fn record_thumbnail_timing(
    pool: &SqlitePool,
    media_id: &str,
    info: Option<&crate::types::MediaTechInfo>,
    thumbnail_ms: u64,
) {
    let s = source_fields(info);
    analytics::record_scan_timing(
        pool,
        media_id,
        ScanTiming {
            probe_ms: 0,
            subtitles_ms: 0,
            subtitle_tracks: 0,
            thumbnail_ms,
            trickplay_ms: 0,
            save_ms: 0,
            total_ms: thumbnail_ms,
            video_codec: s.video_codec,
            audio_codec: s.audio_codec,
            container: s.container,
            width: s.width,
            height: s.height,
            duration_ms: s.duration_ms,
            bitrate_kbps: s.bitrate_kbps,
            pixel_format: s.pixel_format,
            keyframe_count: None,
            trigger: "scan_thumbnail",
        },
    )
    .await;
}

async fn record_trickplay_timing(
    pool: &SqlitePool,
    media_id: &str,
    info: Option<&crate::types::MediaTechInfo>,
    trickplay_ms: u64,
    keyframe_count: Option<u32>,
) {
    let s = source_fields(info);
    analytics::record_scan_timing(
        pool,
        media_id,
        ScanTiming {
            probe_ms: 0,
            subtitles_ms: 0,
            subtitle_tracks: 0,
            thumbnail_ms: 0,
            trickplay_ms,
            save_ms: 0,
            total_ms: trickplay_ms,
            video_codec: s.video_codec,
            audio_codec: s.audio_codec,
            container: s.container,
            width: s.width,
            height: s.height,
            duration_ms: s.duration_ms,
            bitrate_kbps: s.bitrate_kbps,
            pixel_format: s.pixel_format,
            keyframe_count,
            trigger: "scan_trickplay",
        },
    )
    .await;
}

/// Access-triggered single-file refresh: re-runs the essential pass on
/// `media_id`'s current path, stamps the content signature, and records
/// a `scan_timings` row with `trigger='stale_read'`. Updates the global
/// scan-status channel briefly so the UI surfaces the refresh as a
/// mini-scan.
///
/// Caller is responsible for de-duplicating concurrent refreshes for the
/// same `media_id` (see `AppState::refresh_locks`). Returns `Ok(false)` if
/// the media row is missing or soft-deleted.
pub async fn refresh_media_file(
    pool: &SqlitePool,
    progress: Option<&ProgressHandle>,
    media_id: &str,
) -> anyhow::Result<bool> {
    let row: Option<(String, String, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT path, title, content_mtime, content_size
         FROM media WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(media_id)
    .fetch_optional(pool)
    .await?;
    let Some((path, title, old_m, old_s)) = row else {
        return Ok(false);
    };
    let video = PathBuf::from(&path);

    if let Some(p) = progress {
        add_active(p, media_id, &title, "refreshing").await;
    }

    let started = std::time::Instant::now();
    let outcome = run_essential(pool, media_id, &video).await;
    let total_ms = started.elapsed().as_millis() as u64;

    info!(
        %media_id,
        title,
        old_mtime = ?old_m,
        old_size = ?old_s,
        new_mtime = outcome.signature.0,
        new_size = outcome.signature.1,
        trigger = "stale_read",
        "single-file refresh complete",
    );

    let s = source_fields(outcome.tech_info.as_ref());
    analytics::record_scan_timing(
        pool,
        media_id,
        ScanTiming {
            probe_ms: outcome.probe_ms,
            subtitles_ms: outcome.subtitles_ms,
            subtitle_tracks: outcome.sub_tracks,
            thumbnail_ms: 0,
            trickplay_ms: 0,
            save_ms: 0,
            total_ms,
            video_codec: s.video_codec,
            audio_codec: s.audio_codec,
            container: s.container,
            width: s.width,
            height: s.height,
            duration_ms: s.duration_ms,
            bitrate_kbps: s.bitrate_kbps,
            pixel_format: s.pixel_format,
            keyframe_count: None,
            trigger: "stale_read",
        },
    )
    .await;

    if let Some(p) = progress {
        let mid = media_id.to_string();
        set_progress(p, |s| {
            s.active.retain(|j| j.media_id != mid);
        })
        .await;
    }

    Ok(true)
}

/// Background companion to [`refresh_media_file`]: regenerate the
/// cosmetic assets (thumbnail + trickplay sprite) for a single media row.
/// Run after a stale-read refresh has updated the essential data so the
/// in-flight read could return immediately. Best-effort: failures are
/// logged and swallowed.
pub async fn refresh_media_assets(pool: &SqlitePool, media_id: &str) {
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT path, image_path FROM media WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(media_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let Some((path, image_path)) = row else {
        return;
    };
    let video = PathBuf::from(&path);

    if image_path.is_none() {
        thumbnails::scan_for_media(pool, media_id, &video).await;
    }
    let _ = sqlx::query("UPDATE media SET thumbnails_version = ? WHERE id = ?")
        .bind(THUMBNAILS_VERSION)
        .bind(media_id)
        .execute(pool)
        .await;

    let duration = match super::media_info::load(pool, media_id).await {
        Ok(Some(info)) => info.duration_seconds,
        _ => None,
    };
    trickplay::scan_for_media(pool, media_id, &video, duration).await;
    let _ = sqlx::query("UPDATE media SET trickplay_version = ? WHERE id = ?")
        .bind(TRICKPLAY_VERSION)
        .bind(media_id)
        .execute(pool)
        .await;
}

// --- upserts ---

async fn upsert_show(
    pool: &SqlitePool,
    library_id: i64,
    show_dir: &Path,
) -> anyhow::Result<(String, bool)> {
    let path_str = show_dir.to_string_lossy().into_owned();
    let nfo_path = show_dir.join("tvshow.nfo");

    let existing: Option<(String, String, i64, Option<String>)> = sqlx::query_as(
        "SELECT id, scanned_at, scan_version, deleted_at FROM shows WHERE path = ?",
    )
    .bind(&path_str)
    .fetch_optional(pool)
    .await?;

    if let Some((id, scanned_at, scan_version, deleted_at)) = &existing {
        // Track the show dir's mtime alongside the NFO so adding/removing
        // poster.jpg or fanart.jpg also triggers a re-upsert. The
        // scan_version check forces a re-upsert when the scanner code has
        // started persisting new fields since this row was last written.
        // A soft-deleted row always re-upserts so `deleted_at` gets cleared.
        if deleted_at.is_none()
            && *scan_version == SHOW_SCAN_VERSION
            && !any_newer_than(&[&nfo_path, show_dir], scanned_at)
        {
            return Ok((id.clone(), false));
        }
    }

    let id = existing
        .map(|(id, _, _, _)| id)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let nfo = nfo::parse_tvshow_nfo(&nfo_path).unwrap_or_default();
    let title = nfo.title.clone().unwrap_or_else(|| {
        let folder = show_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Untitled Show");
        let cleaned = filename::clean_title(folder);
        if cleaned.is_empty() { folder.to_string() } else { cleaned }
    });
    let sort_title = filename::sort_title(&title);
    let poster = find_show_poster(show_dir).map(|p| p.to_string_lossy().into_owned());
    let fanart = find_show_fanart(show_dir).map(|p| p.to_string_lossy().into_owned());
    let clearlogo = find_show_clearlogo(show_dir).map(|p| p.to_string_lossy().into_owned());
    let banner = find_show_banner(show_dir).map(|p| p.to_string_lossy().into_owned());
    let tvdb_id = nfo
        .uniqueid
        .iter()
        .find(|u| u.kind.eq_ignore_ascii_case("tvdb"))
        .map(|u| u.value.clone());

    let (rating, rating_votes, rating_source) = match nfo.primary_rating() {
        Some((v, votes, src)) => (Some(v), votes, Some(src)),
        None => (None, None, None),
    };
    let studio = if nfo.studio.is_empty() { None } else { Some(nfo.studio.join(", ")) };

    let added_at = file_added_at(show_dir);
    sqlx::query(
        r#"
        INSERT INTO shows (
            id, library_id, path, title, sort_title, original_title, year, plot,
            imdb_id, tmdb_id, tvdb_id, poster_path, fanart_path, clearlogo_path,
            banner_path, added_at, scan_version,
            rating, rating_votes, rating_source, mpaa, studio,
            premiered_date, end_date, status
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(path) DO UPDATE SET
            title = excluded.title,
            sort_title = excluded.sort_title,
            original_title = excluded.original_title,
            year = excluded.year,
            plot = excluded.plot,
            imdb_id = excluded.imdb_id,
            tmdb_id = excluded.tmdb_id,
            tvdb_id = excluded.tvdb_id,
            poster_path = excluded.poster_path,
            fanart_path = excluded.fanart_path,
            clearlogo_path = excluded.clearlogo_path,
            banner_path = excluded.banner_path,
            scan_version = excluded.scan_version,
            rating         = excluded.rating,
            rating_votes   = excluded.rating_votes,
            rating_source  = excluded.rating_source,
            mpaa           = excluded.mpaa,
            studio         = excluded.studio,
            premiered_date = excluded.premiered_date,
            end_date       = excluded.end_date,
            status         = excluded.status,
            deleted_at = NULL,
            scanned_at = datetime('now')
        "#,
    )
    .bind(&id)
    .bind(library_id)
    .bind(&path_str)
    .bind(&title)
    .bind(&sort_title)
    .bind(&nfo.original_title)
    .bind(nfo.year_or_premiered())
    .bind(&nfo.plot)
    .bind(nfo.imdb_id())
    .bind(nfo.tmdb_id())
    .bind(&tvdb_id)
    .bind(&poster)
    .bind(&fanart)
    .bind(&clearlogo)
    .bind(&banner)
    .bind(&added_at)
    .bind(SHOW_SCAN_VERSION)
    .bind(rating)
    .bind(rating_votes)
    .bind(&rating_source)
    .bind(&nfo.mpaa)
    .bind(&studio)
    .bind(&nfo.premiered)
    .bind(&nfo.enddate)
    .bind(&nfo.status)
    .execute(pool)
    .await?;

    sqlx::query("DELETE FROM show_genres WHERE show_id = ?")
        .bind(&id)
        .execute(pool)
        .await?;
    for g in &nfo.genre {
        sqlx::query("INSERT OR IGNORE INTO show_genres (show_id, genre) VALUES (?, ?)")
            .bind(&id)
            .bind(g)
            .execute(pool)
            .await?;
    }

    debug!(title, "indexed show");
    Ok((id, true))
}

/// Returned by `upsert_episode` / `upsert_movie`.
///
/// `re_indexed` reflects whether the metadata row was re-upserted (drives
/// the indexed/skipped stats). Each `needs_*` flag is independent and gates
/// exactly one asset pass: `needs_essential` covers probe+probe_json+subtitles
/// (and stamps the content signature); the other two cover their named
/// passes. A flag fires when the file content changed (signature mismatch
/// or unknown) OR the matching `*_VERSION` constant is newer than the
/// row's stored version — except thumbnails/trickplay don't fire on a
/// content-signature *unknown* row (forward-only repair: pre-fix rows
/// essential-refresh but don't trigger a library-wide trickplay storm).
///
/// `has_sidecar_image` lets pass 2 skip thumbnail generation when the
/// library already supplies one (but the version is still bumped, so the
/// row doesn't permanently re-trip).
pub struct UpsertOutcome {
    pub id: String,
    pub has_sidecar_image: bool,
    pub re_indexed: bool,
    pub needs_essential: bool,
    pub needs_thumbnails: bool,
    pub needs_trickplay: bool,
}

/// First contiguous run of ASCII digits parsed as i64. Used by the
/// folder/filename fallback when no SxxEyy tag or nfo is present.
fn first_int(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            return s[start..i].parse().ok();
        }
        i += 1;
    }
    None
}

/// Fallback when SxxEyy / nfo don't give us S+E: season comes from the
/// immediate parent folder name (first integer), episode from the first
/// integer in the filename. Files directly in the show folder get season 1.
/// If no integer is present in the filename, derive a stable pseudo-number
/// from its byte hash so episodes still sort deterministically.
fn infer_season_episode(video: &Path, show_dir: &Path) -> (i64, i64) {
    let parent = video.parent().unwrap_or(show_dir);
    let season = if parent == show_dir {
        1
    } else {
        parent
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(first_int)
            .unwrap_or(1)
    };
    let stem = video.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let episode = first_int(stem).unwrap_or_else(|| {
        let mut h: u64 = 1469598103934665603;
        for byte in stem.bytes() {
            h ^= byte as u64;
            h = h.wrapping_mul(1099511628211);
        }
        1000 + (h % 9000) as i64
    });
    (season, episode)
}

async fn upsert_episode(
    pool: &SqlitePool,
    library_id: i64,
    show_id: &str,
    show_dir: &Path,
    video: &Path,
    file_size: i64,
) -> anyhow::Result<Option<UpsertOutcome>> {
    let path_str = video.to_string_lossy().into_owned();
    let base = video.file_stem().and_then(|s| s.to_str()).unwrap_or("episode");
    let nfo_path = video.with_extension("nfo");
    let nfo_opt = nfo_path.is_file().then_some(nfo_path);

    type ExistingRow = (
        String,
        String,
        i64,
        i64,
        Option<String>,
        i64,
        i64,
        i64,
        i64,
        Option<i64>,
        Option<i64>,
    );
    let existing: Option<ExistingRow> = sqlx::query_as(
        "SELECT id, scanned_at, file_size, scan_version, deleted_at,
                subtitles_version, markers_version, thumbnails_version, trickplay_version,
                content_mtime, content_size
         FROM media WHERE path = ?",
    )
    .bind(&path_str)
    .fetch_optional(pool)
    .await?;

    // Compute staleness via the content signature (mtime,size) — bidirectional
    // so an in-place file swap with a preserved/backdated mtime is detected.
    // `content_unknown` covers pre-fix rows: the essential pass runs to backfill the
    // signature, but the heavier cosmetic passes don't fire (forward-only
    // repair — avoids a library-wide trickplay storm after the migration).
    // Sidecar-only sources (NFO + parent dir mtime) still force a metadata
    // re-upsert through the existing `any_newer_than` path; the video itself
    // is covered by the signature.
    let (file_changed, metadata_stale, needs_essential, needs_thumbnails, needs_trickplay) =
        if let Some((
            _,
            scanned_at,
            _existing_size,
            scan_version,
            deleted_at,
            sv,
            mv,
            tv,
            pv,
            cm,
            cs,
        )) = &existing
        {
            let mut sidecar_sources: Vec<&Path> = Vec::new();
            if let Some(n) = nfo_opt.as_ref() {
                sidecar_sources.push(n);
            }
            if let Some(parent) = video.parent() {
                sidecar_sources.push(parent);
            }
            let stored_sig: Option<(i64, i64)> = match (cm, cs) {
                (Some(m), Some(s)) => Some((*m, *s)),
                _ => None,
            };
            let cur_sig = (mtime_secs(video), file_size);
            let content_changed =
                stored_sig.is_some() && stored_sig != Some(cur_sig);
            let content_unknown = stored_sig.is_none();
            let sidecars_changed = any_newer_than(&sidecar_sources, scanned_at);
            let file_changed =
                deleted_at.is_some() || content_changed || sidecars_changed;
            let metadata_stale = *scan_version != MEDIA_SCAN_VERSION;
            (
                file_changed,
                metadata_stale,
                deleted_at.is_some()
                    || content_changed
                    || content_unknown
                    || *sv < SUBTITLES_VERSION
                    || *mv < MARKERS_VERSION,
                deleted_at.is_some() || content_changed || *tv < THUMBNAILS_VERSION,
                deleted_at.is_some() || content_changed || *pv < TRICKPLAY_VERSION,
            )
        } else {
            // New row — treat as fully stale so every pass runs.
            (true, true, true, true, true)
        };

    if !file_changed && !metadata_stale && !needs_essential && !needs_thumbnails && !needs_trickplay {
        let id = existing.as_ref().map(|r| r.0.clone()).unwrap_or_default();
        return Ok(Some(UpsertOutcome {
            id,
            has_sidecar_image: find_episode_thumb(video).is_some(),
            re_indexed: false,
            needs_essential: false,
            needs_thumbnails: false,
            needs_trickplay: false,
        }));
    }

    let nfo: EpisodeNfo = nfo_opt
        .as_deref()
        .and_then(|p| nfo::parse_episode_nfo(p).ok())
        .unwrap_or_default();

    let (season, episode) = match (nfo.season, nfo.episode) {
        (Some(s), Some(e)) => (s, e),
        _ => filename::parse_episode(base).unwrap_or_else(|| {
            let inferred = infer_season_episode(video, show_dir);
            debug!(
                file = base,
                season = inferred.0,
                episode = inferred.1,
                "no episode tag/nfo — inferred from folder + filename"
            );
            inferred
        }),
    };

    let title = nfo
        .title
        .clone()
        .unwrap_or_else(|| filename::clean_episode_title(base, episode));
    let sort_title = filename::sort_title(&title);
    let thumb = find_episode_thumb(video).map(|p| p.to_string_lossy().into_owned());

    let id = existing
        .map(|r| r.0)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let added_at = file_added_at(video);
    sqlx::query(
        r#"
        INSERT INTO media (
            id, library_id, kind, path, file_size,
            title, sort_title, plot, runtime_minutes, image_path,
            show_id, season_number, episode_number, added_at, scan_version,
            release_date
        )
        VALUES (?, ?, 'episode', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(path) DO UPDATE SET
            kind            = 'episode',
            title           = excluded.title,
            sort_title      = excluded.sort_title,
            plot            = excluded.plot,
            runtime_minutes = excluded.runtime_minutes,
            image_path      = excluded.image_path,
            show_id         = excluded.show_id,
            season_number   = excluded.season_number,
            episode_number  = excluded.episode_number,
            file_size       = excluded.file_size,
            scan_version    = excluded.scan_version,
            release_date    = excluded.release_date,
            deleted_at      = NULL,
            -- clear movie-only fields in case this row was previously a movie
            original_title  = NULL,
            year            = NULL,
            imdb_id         = NULL,
            tmdb_id         = NULL,
            fanart_path     = NULL,
            rating          = NULL,
            rating_votes    = NULL,
            rating_source   = NULL,
            mpaa            = NULL,
            studio          = NULL,
            tagline         = NULL,
            director        = NULL,
            writers         = NULL,
            scanned_at      = datetime('now')
        "#,
    )
    .bind(&id)
    .bind(library_id)
    .bind(&path_str)
    .bind(file_size)
    .bind(&title)
    .bind(&sort_title)
    .bind(&nfo.plot)
    .bind(nfo.runtime)
    .bind(&thumb)
    .bind(show_id)
    .bind(season)
    .bind(episode)
    .bind(&added_at)
    .bind(MEDIA_SCAN_VERSION)
    .bind(&nfo.aired)
    .execute(pool)
    .await?;

    // Episodes inherit genres from their show; no per-episode genre table needed.
    sqlx::query("DELETE FROM media_genres WHERE media_id = ?")
        .bind(&id)
        .execute(pool)
        .await?;

    debug!(title, season, episode, "indexed episode");
    Ok(Some(UpsertOutcome {
        id,
        has_sidecar_image: thumb.is_some(),
        re_indexed: true,
        needs_essential,
        needs_thumbnails,
        needs_trickplay,
    }))
}

async fn upsert_movie(
    pool: &SqlitePool,
    library_id: i64,
    video: &Path,
    file_size: i64,
) -> anyhow::Result<Option<UpsertOutcome>> {
    let path_str = video.to_string_lossy().into_owned();
    let base = video.file_stem().and_then(|s| s.to_str()).unwrap_or("Untitled");
    let nfo_path = matching_nfo(video);

    type ExistingRow = (
        String,
        String,
        i64,
        i64,
        Option<String>,
        i64,
        i64,
        i64,
        i64,
        Option<i64>,
        Option<i64>,
    );
    let existing: Option<ExistingRow> = sqlx::query_as(
        "SELECT id, scanned_at, file_size, scan_version, deleted_at,
                subtitles_version, markers_version, thumbnails_version, trickplay_version,
                content_mtime, content_size
         FROM media WHERE path = ?",
    )
    .bind(&path_str)
    .fetch_optional(pool)
    .await?;

    // Same logic as upsert_episode: bidirectional (mtime,size) signature for
    // the video, sidecar mtime check for NFO / poster / fanart / thumb (the
    // parent dir's mtime bumps when any sidecar is added or removed on most
    // filesystems).
    let (file_changed, metadata_stale, needs_essential, needs_thumbnails, needs_trickplay) =
        if let Some((
            _,
            scanned_at,
            _existing_size,
            scan_version,
            deleted_at,
            sv,
            mv,
            tv,
            pv,
            cm,
            cs,
        )) = &existing
        {
            let mut sidecar_sources: Vec<&Path> = Vec::new();
            if let Some(n) = nfo_path.as_ref() {
                sidecar_sources.push(n);
            }
            if let Some(parent) = video.parent() {
                sidecar_sources.push(parent);
            }
            let stored_sig: Option<(i64, i64)> = match (cm, cs) {
                (Some(m), Some(s)) => Some((*m, *s)),
                _ => None,
            };
            let cur_sig = (mtime_secs(video), file_size);
            let content_changed =
                stored_sig.is_some() && stored_sig != Some(cur_sig);
            let content_unknown = stored_sig.is_none();
            let sidecars_changed = any_newer_than(&sidecar_sources, scanned_at);
            let file_changed =
                deleted_at.is_some() || content_changed || sidecars_changed;
            let metadata_stale = *scan_version != MEDIA_SCAN_VERSION;
            (
                file_changed,
                metadata_stale,
                deleted_at.is_some()
                    || content_changed
                    || content_unknown
                    || *sv < SUBTITLES_VERSION
                    || *mv < MARKERS_VERSION,
                deleted_at.is_some() || content_changed || *tv < THUMBNAILS_VERSION,
                deleted_at.is_some() || content_changed || *pv < TRICKPLAY_VERSION,
            )
        } else {
            (true, true, true, true, true)
        };

    if !file_changed && !metadata_stale && !needs_essential && !needs_thumbnails && !needs_trickplay {
        let id = existing.as_ref().map(|r| r.0.clone()).unwrap_or_default();
        return Ok(Some(UpsertOutcome {
            id,
            has_sidecar_image: find_movie_image(video).is_some(),
            re_indexed: false,
            needs_essential: false,
            needs_thumbnails: false,
            needs_trickplay: false,
        }));
    }

    let nfo: MovieNfo = nfo_path
        .as_deref()
        .and_then(|p| nfo::parse_movie_nfo(p).ok())
        .unwrap_or_default();

    let parsed = filename::parse_movie(base);
    let title = nfo.title.clone().unwrap_or_else(|| {
        if parsed.title.is_empty() { base.to_string() } else { parsed.title.clone() }
    });
    let year = nfo.year.or(parsed.year);
    let sort_title = filename::sort_title(&title);
    let image = find_movie_image(video).map(|p| p.to_string_lossy().into_owned());
    let fanart = find_movie_fanart(video).map(|p| p.to_string_lossy().into_owned());

    let id = existing
        .map(|r| r.0)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let (rating, rating_votes, rating_source) = match nfo.primary_rating() {
        Some((v, votes, src)) => (Some(v), votes, Some(src)),
        None => (None, None, None),
    };
    let studio = if nfo.studio.is_empty() { None } else { Some(nfo.studio.join(", ")) };
    let director = if nfo.director.is_empty() { None } else { Some(nfo.director.join(", ")) };
    let writers = if nfo.credits.is_empty() { None } else { Some(nfo.credits.join(", ")) };

    let added_at = file_added_at(video);
    sqlx::query(
        r#"
        INSERT INTO media (
            id, library_id, kind, path, file_size,
            title, sort_title, original_title, year, plot, runtime_minutes,
            imdb_id, tmdb_id, image_path, fanart_path, added_at, scan_version,
            rating, rating_votes, rating_source, mpaa, studio,
            tagline, release_date, director, writers
        )
        VALUES (?, ?, 'movie', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(path) DO UPDATE SET
            kind            = 'movie',
            title           = excluded.title,
            sort_title      = excluded.sort_title,
            original_title  = excluded.original_title,
            year            = excluded.year,
            plot            = excluded.plot,
            runtime_minutes = excluded.runtime_minutes,
            imdb_id         = excluded.imdb_id,
            tmdb_id         = excluded.tmdb_id,
            image_path      = excluded.image_path,
            fanart_path     = excluded.fanart_path,
            file_size       = excluded.file_size,
            scan_version    = excluded.scan_version,
            rating          = excluded.rating,
            rating_votes    = excluded.rating_votes,
            rating_source   = excluded.rating_source,
            mpaa            = excluded.mpaa,
            studio          = excluded.studio,
            tagline         = excluded.tagline,
            release_date    = excluded.release_date,
            director        = excluded.director,
            writers         = excluded.writers,
            deleted_at      = NULL,
            -- clear episode-only fields in case this was previously an episode
            show_id         = NULL,
            season_number   = NULL,
            episode_number  = NULL,
            scanned_at      = datetime('now')
        "#,
    )
    .bind(&id)
    .bind(library_id)
    .bind(&path_str)
    .bind(file_size)
    .bind(&title)
    .bind(&sort_title)
    .bind(&nfo.original_title)
    .bind(year)
    .bind(&nfo.plot)
    .bind(nfo.runtime)
    .bind(nfo.imdb_id())
    .bind(nfo.tmdb_id())
    .bind(&image)
    .bind(&fanart)
    .bind(&added_at)
    .bind(MEDIA_SCAN_VERSION)
    .bind(rating)
    .bind(rating_votes)
    .bind(&rating_source)
    .bind(&nfo.mpaa)
    .bind(&studio)
    .bind(&nfo.tagline)
    .bind(&nfo.premiered)
    .bind(&director)
    .bind(&writers)
    .execute(pool)
    .await?;

    sqlx::query("DELETE FROM media_genres WHERE media_id = ?")
        .bind(&id)
        .execute(pool)
        .await?;
    for g in &nfo.genre {
        sqlx::query("INSERT OR IGNORE INTO media_genres (media_id, genre) VALUES (?, ?)")
            .bind(&id)
            .bind(g)
            .execute(pool)
            .await?;
    }

    debug!(title, "indexed movie");
    Ok(Some(UpsertOutcome {
        id,
        has_sidecar_image: image.is_some(),
        re_indexed: true,
        needs_essential,
        needs_thumbnails,
        needs_trickplay,
    }))
}
