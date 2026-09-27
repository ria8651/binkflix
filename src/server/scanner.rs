use super::analytics::{self, ScanTiming};
use super::filename;
use super::nfo::{self, EpisodeNfo, MovieNfo};
use super::{subtitles, thumbnails, trickplay};
use crate::types::{ActiveJob, Phase, ScanProgress, Stage};
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
const AUDIO_MARKERS_VERSION: i64 = 2;

async fn set_progress(handle: &ProgressHandle, f: impl FnOnce(&mut ScanProgress)) {
    let mut p = handle.write().await;
    f(&mut p);
}

/// `key` identifies the job for later removal: a file id for per-file
/// passes, a show id for season analysis.
async fn add_active(handle: &ProgressHandle, key: &str, title: &str, stage: Stage) {
    handle.write().await.active.push(ActiveJob {
        media_id: key.into(),
        title: title.into(),
        stage,
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
        // shows/items/files that were soft-deleted by the same library-prune.
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
            sqlx::query("UPDATE media_files SET deleted_at = NULL WHERE library_id = ?")
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
/// probe_json + subtitles + content-signature stamp. Assets belong to the
/// file, so a job is addressed by file id.
struct AssetJob {
    file_id: String,
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

    // Items carry no path of their own; any of their files will do.
    let media: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT m.id, MIN(f.path), m.scanned_at
         FROM media m JOIN media_files f ON f.media_id = m.id
         WHERE m.added_at IS NULL
         GROUP BY m.id",
    )
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
            s.phase = Phase::Indexing;
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
                match upsert_movie(pool, library_id, &root, &abs, file_size).await {
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
                    file_id: out.file_id,
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
                s.phase = Phase::Subtitles;
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
                        add_active(p, &job.file_id, &job.title, Stage::Subtitles).await;
                    }
                    let started = std::time::Instant::now();
                    let outcome = run_essential(&pool, &job.file_id, &job.video).await;
                    let total_ms = started.elapsed().as_millis() as u64;

                    record_essential_timing(&pool, &job.file_id, &outcome, total_ms).await;

                    let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if let Some(p) = &progress {
                        let title = job.title.clone();
                        let file_id = job.file_id.clone();
                        set_progress(p, |s| {
                            s.done = n;
                            s.current = Some(title);
                            s.active.retain(|j| j.media_id != file_id);
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
                s.phase = Phase::Thumbnails;
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
                            add_active(p, &f.job.file_id, &f.job.title, Stage::Thumbnail).await;
                        }
                        let t = std::time::Instant::now();
                        thumbnails::scan_for_media(&pool, &f.job.file_id, &f.job.video).await;
                        t.elapsed().as_millis() as u64
                    };
                    if let Err(e) = sqlx::query(
                        "UPDATE media_files SET thumbnails_version = ? WHERE id = ?",
                    )
                    .bind(THUMBNAILS_VERSION)
                    .bind(&f.job.file_id)
                    .execute(&pool)
                    .await
                    {
                        warn!(file_id = %f.job.file_id, %e, "failed to update thumbnails_version");
                    }
                    record_thumbnail_timing(
                        &pool,
                        &f.job.file_id,
                        f.tech_info.as_ref(),
                        thumbnail_ms,
                    )
                    .await;
                    let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if let Some(p) = &progress {
                        let title = f.job.title.clone();
                        let file_id = f.job.file_id.clone();
                        set_progress(p, |s| {
                            s.done = n;
                            s.current = Some(title);
                            s.active.retain(|j| j.media_id != file_id);
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
                s.phase = Phase::Trickplay;
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
                        add_active(p, &f.job.file_id, &f.job.title, Stage::Trickplay).await;
                    }
                    // Duration normally comes from pass-1's probe. When only
                    // trickplay is stale (pass 1 was skipped), fall back to
                    // the cached probe data — the file is unchanged by
                    // definition, so the stored value is authoritative.
                    let duration = match f.tech_info.as_ref().and_then(|i| i.duration_seconds) {
                        Some(d) => Some(d),
                        None => match super::media_info::load(&pool, &f.job.file_id).await {
                            Ok(Some(info)) => info.duration_seconds,
                            _ => None,
                        },
                    };
                    let t = std::time::Instant::now();
                    let keyframe_count = trickplay::scan_for_media(
                        &pool,
                        &f.job.file_id,
                        &f.job.video,
                        duration,
                    )
                    .await;
                    let trickplay_ms = t.elapsed().as_millis() as u64;
                    if let Err(e) = sqlx::query(
                        "UPDATE media_files SET trickplay_version = ? WHERE id = ?",
                    )
                    .bind(TRICKPLAY_VERSION)
                    .bind(&f.job.file_id)
                    .execute(&pool)
                    .await
                    {
                        warn!(file_id = %f.job.file_id, %e, "failed to update trickplay_version");
                    }
                    record_trickplay_timing(
                        &pool,
                        &f.job.file_id,
                        f.tech_info.as_ref(),
                        trickplay_ms,
                        keyframe_count,
                    )
                    .await;
                    let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if let Some(p) = &progress {
                        let title = f.job.title.clone();
                        let file_id = f.job.file_id.clone();
                        set_progress(p, |s| {
                            s.done = n;
                            s.current = Some(title);
                            s.active.retain(|j| j.media_id != file_id);
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
            s.phase = Phase::AudioMatch;
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
                    add_active(p, &show_id, &label, Stage::Analysing).await;
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
        String,      // file id
        String,      // path
        Option<i64>, // media_files.content_mtime
        Option<i64>, // media_files.content_size
        i64,         // media_files.audio_markers_version
        Option<i64>, // fingerprint.content_mtime
        Option<i64>, // fingerprint.content_size
        Option<i64>, // fingerprint.fp_algo_version
    );
    // One file per episode — its primary. A second copy of the same episode
    // would "share" its entire runtime with the first and swamp detection.
    let sql = format!(
        "SELECT f.id, f.path, f.content_mtime, f.content_size, f.audio_markers_version,
                fp.content_mtime, fp.content_size, fp.fp_algo_version
         FROM media m
         JOIN media_files f ON f.id = {}
         LEFT JOIN media_fingerprints fp ON fp.file_id = f.id
         WHERE m.show_id = ? AND m.season_number = ? AND m.kind = 'episode'
               AND m.deleted_at IS NULL
         ORDER BY m.episode_number",
        super::files::primary_file_id("m.id"),
    );
    let rows: Vec<Row> = sqlx::query_as(&sql)
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
        eps.push(super::markers::SeasonEpisode { file_id: id.clone(), duration, fp });
    }
    if eps.len() < 2 {
        return Ok(());
    }

    let analyzed = super::markers::analyze_season(&eps);
    let analyzed_ids: std::collections::HashSet<&str> =
        analyzed.iter().map(|(id, _)| id.as_str()).collect();
    let mut total_markers = 0usize;
    for (file_id, markers) in &analyzed {
        total_markers += markers.len();
        if let Err(e) = super::markers::store_markers(pool, file_id, "audio", markers).await {
            warn!(%file_id, %e, "audio-match: failed to store markers");
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
        let _ = sqlx::query("UPDATE media_files SET audio_markers_version = ? WHERE id = ?")
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
        sqlx::query(
            "UPDATE media_files SET deleted_at = ? WHERE library_id = ? AND deleted_at IS NULL",
        )
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?;
        debug!(library_id = id, "soft-deleted library");
    }
    Ok(removed)
}


/// Soft-delete files in this library whose path wasn't seen during the walk,
/// merge items that turn out to be the same episode or movie, then
/// soft-delete items left without a live file and shows left without a live
/// item.
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
        "SELECT id, path FROM media_files WHERE library_id = ? AND deleted_at IS NULL",
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
            "UPDATE media_files SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL",
        )
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?;
        removed += res.rows_affected();
        debug!(file_id = %id, "soft-deleted file");
    }

    reconcile_duplicates(pool, library_id).await?;

    // Items with no live file: every file was deleted above, or moved to
    // another item during the walk (an NFO now naming a different episode).
    let res = sqlx::query(
        "UPDATE media SET deleted_at = ?
         WHERE library_id = ? AND deleted_at IS NULL
           AND NOT EXISTS (
               SELECT 1 FROM media_files f
                WHERE f.media_id = media.id AND f.deleted_at IS NULL
           )",
    )
    .bind(&now)
    .bind(library_id)
    .execute(pool)
    .await?;
    removed += res.rows_affected();

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

/// Merge items in this library that are the same episode or movie.
///
/// The upserts already attach a new file to the existing item it belongs to,
/// so this only has work where that couldn't happen: duplicates minted before
/// items and files were split (migration 0028) — each replaced file used to
/// strand its watch history on a soft-deleted row — and movies whose tmdb/imdb
/// id only appeared after both copies were indexed. Cheap when there's
/// nothing to do: three GROUP BYs over the library.
async fn reconcile_duplicates(pool: &SqlitePool, library_id: i64) -> anyhow::Result<()> {
    // The survivor is the oldest item: the one existing links, bookmarks and
    // history most likely point at. Same order `episode_item` / `movie_item`
    // pick by, so a scan and a reconcile never disagree about which it is.
    let episodes: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT show_id, season_number, episode_number FROM media
         WHERE library_id = ? AND kind = 'episode' AND show_id IS NOT NULL
           AND season_number IS NOT NULL AND episode_number IS NOT NULL
         GROUP BY show_id, season_number, episode_number
         HAVING COUNT(*) > 1",
    )
    .bind(library_id)
    .fetch_all(pool)
    .await?;
    for (show_id, season, episode) in episodes {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM media
             WHERE kind = 'episode' AND show_id = ? AND season_number = ? AND episode_number = ?
             ORDER BY added_at, id",
        )
        .bind(&show_id)
        .bind(season)
        .bind(episode)
        .fetch_all(pool)
        .await?;
        merge_items(pool, &ids).await?;
    }

    for col in ["tmdb_id", "imdb_id"] {
        let keys: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT {col} FROM media
             WHERE library_id = ? AND kind = 'movie' AND COALESCE({col}, '') != ''
             GROUP BY {col}
             HAVING COUNT(*) > 1"
        ))
        .bind(library_id)
        .fetch_all(pool)
        .await?;
        for key in keys {
            let ids: Vec<String> = sqlx::query_scalar(&format!(
                "SELECT id FROM media
                 WHERE library_id = ? AND kind = 'movie' AND {col} = ?
                 ORDER BY added_at, id"
            ))
            .bind(library_id)
            .bind(&key)
            .fetch_all(pool)
            .await?;
            merge_items(pool, &ids).await?;
        }
    }
    Ok(())
}

/// Fold `ids[1..]` into `ids[0]`: files, watch history, per-scope state and
/// analytics move over, then the extras are deleted.
///
/// Metadata comes from the most recently scanned member that still has a
/// live file, since the survivor's may describe a file that's gone (an
/// episode `-thumb.jpg` deleted along with it, say).
async fn merge_items(pool: &SqlitePool, ids: &[String]) -> anyhow::Result<()> {
    let Some((survivor, victims)) = ids.split_first() else {
        return Ok(());
    };
    if victims.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;

    let mut donor: Option<(&str, String)> = None;
    for id in ids {
        let row: Option<(String, bool)> = sqlx::query_as(
            "SELECT scanned_at,
                    EXISTS (SELECT 1 FROM media_files f
                             WHERE f.media_id = media.id AND f.deleted_at IS NULL)
             FROM media WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((scanned_at, true)) = row {
            if donor.as_ref().map_or(true, |(_, best)| scanned_at > *best) {
                donor = Some((id.as_str(), scanned_at));
            }
        }
    }
    if let Some((donor, _)) = donor.filter(|(d, _)| *d != survivor.as_str()) {
        sqlx::query(
            "UPDATE media SET
                (title, sort_title, original_title, year, plot, runtime_minutes,
                 imdb_id, tmdb_id, image_path, fanart_path,
                 rating, rating_votes, rating_source, mpaa, studio, tagline,
                 release_date, director, writers, scan_version, scanned_at)
              = (SELECT title, sort_title, original_title, year, plot, runtime_minutes,
                        imdb_id, tmdb_id, image_path, fanart_path,
                        rating, rating_votes, rating_source, mpaa, studio, tagline,
                        release_date, director, writers, scan_version, scanned_at
                   FROM media WHERE id = ?)
             WHERE id = ?",
        )
        .bind(donor)
        .bind(survivor)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM media_genres WHERE media_id = ?")
            .bind(survivor)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE media_genres SET media_id = ? WHERE media_id = ?")
            .bind(survivor)
            .bind(donor)
            .execute(&mut *tx)
            .await?;
    }

    let survivor_scope = format!("media:{survivor}");
    for victim in victims {
        // Per user, keep the newer position and the union of "finished":
        // `completed` and `last_completed_at` are monotonic by design (see
        // watch.rs), so a merge must never lose one. SET expressions read the
        // pre-update row, so every CASE compares against the old timestamp.
        sqlx::query(
            "INSERT INTO watch_progress
                (user_sub, media_id, position_secs, duration_secs, completed,
                 updated_at, last_completed_at)
             SELECT user_sub, ?, position_secs, duration_secs, completed,
                    updated_at, last_completed_at
               FROM watch_progress WHERE media_id = ?
             ON CONFLICT(user_sub, media_id) DO UPDATE SET
                position_secs = CASE WHEN excluded.updated_at > watch_progress.updated_at
                                     THEN excluded.position_secs
                                     ELSE watch_progress.position_secs END,
                duration_secs = CASE WHEN excluded.updated_at > watch_progress.updated_at
                                     THEN excluded.duration_secs
                                     ELSE watch_progress.duration_secs END,
                updated_at    = MAX(excluded.updated_at, watch_progress.updated_at),
                completed     = MAX(excluded.completed, watch_progress.completed),
                last_completed_at = NULLIF(MAX(COALESCE(excluded.last_completed_at, 0),
                                               COALESCE(watch_progress.last_completed_at, 0)), 0)",
        )
        .bind(survivor)
        .bind(victim)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM watch_progress WHERE media_id = ?")
            .bind(victim)
            .execute(&mut *tx)
            .await?;

        // Movie-scoped prefs and rewatch/hide state (`media:<id>`). Episodes
        // scope by show, which a merge within one show doesn't touch. Where
        // both have a row, the survivor's stands.
        let victim_scope = format!("media:{victim}");
        for table in ["media_preferences", "watch_scope_state"] {
            sqlx::query(&format!(
                "UPDATE OR IGNORE {table} SET scope_key = ? WHERE scope_key = ?"
            ))
            .bind(&survivor_scope)
            .bind(&victim_scope)
            .execute(&mut *tx)
            .await?;
            sqlx::query(&format!("DELETE FROM {table} WHERE scope_key = ?"))
                .bind(&victim_scope)
                .execute(&mut *tx)
                .await?;
        }

        for sql in [
            "UPDATE media_files       SET media_id = ? WHERE media_id = ?",
            "UPDATE playback_sessions SET media_id = ? WHERE media_id = ?",
            "UPDATE events            SET media_id = ? WHERE media_id = ?",
        ] {
            sqlx::query(sql)
                .bind(survivor)
                .bind(victim)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM media WHERE id = ?")
            .bind(victim)
            .execute(&mut *tx)
            .await?;
    }

    sqlx::query(
        "UPDATE media SET deleted_at = NULL
         WHERE id = ? AND EXISTS (SELECT 1 FROM media_files f
                                   WHERE f.media_id = media.id AND f.deleted_at IS NULL)",
    )
    .bind(survivor)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    info!(%survivor, merged = ?victims, "merged duplicate items");
    Ok(())
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
async fn run_essential(pool: &SqlitePool, file_id: &str, video: &Path) -> EssentialOutcome {
    let signature = stat_signature(video);

    let t = std::time::Instant::now();
    let (tech_info, embedded_subs, chapters) = match super::media_info::probe_full(video).await {
        Ok((info, subs, chapters)) => (Some(info), subs, chapters),
        Err(e) => {
            warn!(%file_id, %e, "ffprobe failed");
            (None, Vec::new(), Vec::new())
        }
    };
    let probe_ms = t.elapsed().as_millis() as u64;

    if let Some(info) = tech_info.as_ref() {
        if let Err(e) = super::media_info::store(pool, file_id, info).await {
            warn!(%file_id, %e, "failed to cache tech info");
        }
    }

    // Embedded-chapter markers ride along with the probe (free). Replace only
    // the `chapter`-source rows so audio-detected markers (a separate, season-
    // scoped producer) survive an essential re-run.
    let duration = tech_info.as_ref().and_then(|t| t.duration_seconds).unwrap_or(0.0);
    let chapter_markers = super::markers::chapters_to_markers(&chapters, duration);
    if let Err(e) = super::markers::store_markers(pool, file_id, "chapter", &chapter_markers).await
    {
        warn!(%file_id, %e, "failed to store chapter markers");
    }

    let t = std::time::Instant::now();
    if let Err(e) = subtitles::scan_for_media(pool, file_id, video, &embedded_subs).await {
        warn!(%file_id, %e, "subtitle scan failed");
    }
    let subtitles_ms = t.elapsed().as_millis() as u64;

    // Bump the per-row version unconditionally — matches today's policy where
    // version was written at upsert time regardless of whether asset
    // extraction succeeded. Failures don't auto-retry; the user re-triggers
    // with a version bump.
    if let Err(e) = sqlx::query(
        "UPDATE media_files SET subtitles_version = ?,
                          markers_version  = ?,
                          content_mtime    = ?,
                          content_size     = ?
         WHERE id = ?",
    )
    .bind(SUBTITLES_VERSION)
    .bind(MARKERS_VERSION)
    .bind(signature.0)
    .bind(signature.1)
    .bind(file_id)
    .execute(pool)
    .await
    {
        warn!(%file_id, %e, "failed to stamp content signature / subtitles_version");
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
    file_id: &str,
    outcome: &EssentialOutcome,
    total_ms: u64,
) {
    let s = source_fields(outcome.tech_info.as_ref());
    analytics::record_scan_timing(
        pool,
        file_id,
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
    file_id: &str,
    info: Option<&crate::types::MediaTechInfo>,
    thumbnail_ms: u64,
) {
    let s = source_fields(info);
    analytics::record_scan_timing(
        pool,
        file_id,
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
    file_id: &str,
    info: Option<&crate::types::MediaTechInfo>,
    trickplay_ms: u64,
    keyframe_count: Option<u32>,
) {
    let s = source_fields(info);
    analytics::record_scan_timing(
        pool,
        file_id,
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
/// file `file_id`, stamps the content signature, and records
/// a `scan_timings` row with `trigger='stale_read'`. Updates the global
/// scan-status channel briefly so the UI surfaces the refresh as a
/// mini-scan.
///
/// Caller is responsible for de-duplicating concurrent refreshes for the
/// same `file_id` (see `AppState::refresh_locks`). Returns `Ok(false)` if
/// the file row is missing or soft-deleted.
pub async fn refresh_media_file(
    pool: &SqlitePool,
    progress: Option<&ProgressHandle>,
    file_id: &str,
) -> anyhow::Result<bool> {
    let row: Option<(String, String, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT f.path, m.title, f.content_mtime, f.content_size
         FROM media_files f JOIN media m ON m.id = f.file_id
         WHERE f.id = ? AND f.deleted_at IS NULL",
    )
    .bind(file_id)
    .fetch_optional(pool)
    .await?;
    let Some((path, title, old_m, old_s)) = row else {
        return Ok(false);
    };
    let video = PathBuf::from(&path);

    if let Some(p) = progress {
        add_active(p, file_id, &title, Stage::Refreshing).await;
    }

    let started = std::time::Instant::now();
    let outcome = run_essential(pool, file_id, &video).await;
    let total_ms = started.elapsed().as_millis() as u64;

    info!(
        %file_id,
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
        file_id,
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
        let mid = file_id.to_string();
        set_progress(p, |s| {
            s.active.retain(|j| j.media_id != mid);
        })
        .await;
    }

    Ok(true)
}

/// Background companion to [`refresh_media_file`]: regenerate the
/// cosmetic assets (thumbnail + trickplay sprite) for a single file.
/// Run after a stale-read refresh has updated the essential data so the
/// in-flight read could return immediately. Best-effort: failures are
/// logged and swallowed.
pub async fn refresh_media_assets(pool: &SqlitePool, file_id: &str) {
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT f.path, m.image_path
         FROM media_files f JOIN media m ON m.id = f.file_id
         WHERE f.id = ? AND f.deleted_at IS NULL",
    )
    .bind(file_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let Some((path, image_path)) = row else {
        return;
    };
    let video = PathBuf::from(&path);

    if image_path.is_none() {
        thumbnails::scan_for_media(pool, file_id, &video).await;
    }
    let _ = sqlx::query("UPDATE media_files SET thumbnails_version = ? WHERE id = ?")
        .bind(THUMBNAILS_VERSION)
        .bind(file_id)
        .execute(pool)
        .await;

    let duration = match super::media_info::load(pool, file_id).await {
        Ok(Some(info)) => info.duration_seconds,
        _ => None,
    };
    trickplay::scan_for_media(pool, file_id, &video, duration).await;
    let _ = sqlx::query("UPDATE media_files SET trickplay_version = ? WHERE id = ?")
        .bind(TRICKPLAY_VERSION)
        .bind(file_id)
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
    pub file_id: String,
    pub has_sidecar_image: bool,
    pub re_indexed: bool,
    pub needs_essential: bool,
    pub needs_thumbnails: bool,
    pub needs_trickplay: bool,
}

impl UpsertOutcome {
    fn unchanged(file_id: String, has_sidecar_image: bool) -> Self {
        Self {
            file_id,
            has_sidecar_image,
            re_indexed: false,
            needs_essential: false,
            needs_thumbnails: false,
            needs_trickplay: false,
        }
    }
}

/// The file row at a path, plus what staleness and re-linking need to know
/// about the item it currently belongs to.
#[derive(sqlx::FromRow)]
struct ExistingFile {
    id: String,
    media_id: String,
    scanned_at: String,
    deleted_at: Option<String>,
    subtitles_version: i64,
    markers_version: i64,
    thumbnails_version: i64,
    trickplay_version: i64,
    content_mtime: Option<i64>,
    content_size: Option<i64>,
    item_kind: String,
    item_scan_version: i64,
    item_deleted_at: Option<String>,
}

async fn existing_file(pool: &SqlitePool, path: &str) -> anyhow::Result<Option<ExistingFile>> {
    Ok(sqlx::query_as(
        "SELECT f.id, f.media_id, f.scanned_at, f.deleted_at,
                f.subtitles_version, f.markers_version, f.thumbnails_version, f.trickplay_version,
                f.content_mtime, f.content_size,
                m.kind AS item_kind, m.scan_version AS item_scan_version,
                m.deleted_at AS item_deleted_at
         FROM media_files f
         JOIN media m ON m.id = f.media_id
         WHERE f.path = ?",
    )
    .bind(path)
    .fetch_optional(pool)
    .await?)
}

/// Which parts of an upsert a file needs. See [`UpsertOutcome`] for what
/// each asset flag gates.
struct Staleness {
    reindex: bool,
    needs_essential: bool,
    needs_thumbnails: bool,
    needs_trickplay: bool,
}

impl Staleness {
    /// A path never seen before: every pass runs.
    const NEW_FILE: Self = Self {
        reindex: true,
        needs_essential: true,
        needs_thumbnails: true,
        needs_trickplay: true,
    };

    fn any(&self) -> bool {
        self.reindex || self.needs_essential || self.needs_thumbnails || self.needs_trickplay
    }
}

/// Staleness via the content signature (mtime,size) — bidirectional so an
/// in-place file swap with a preserved/backdated mtime is detected.
/// `content_unknown` covers pre-fix rows: the essential pass runs to backfill
/// the signature, but the heavier cosmetic passes don't fire (forward-only
/// repair — avoids a library-wide trickplay storm after the migration).
/// Sidecar-only sources (`nfo` + parent dir mtime) still force a metadata
/// re-upsert through `any_newer_than`; the video itself is covered by the
/// signature. A soft-deleted file or item coming back re-runs everything.
fn staleness(f: &ExistingFile, video: &Path, file_size: i64, nfo: Option<&Path>) -> Staleness {
    let mut sidecar_sources: Vec<&Path> = Vec::new();
    if let Some(n) = nfo {
        sidecar_sources.push(n);
    }
    if let Some(parent) = video.parent() {
        sidecar_sources.push(parent);
    }
    let stored_sig = match (f.content_mtime, f.content_size) {
        (Some(m), Some(s)) => Some((m, s)),
        _ => None,
    };
    let cur_sig = (mtime_secs(video), file_size);
    let content_changed = stored_sig.is_some() && stored_sig != Some(cur_sig);
    let content_unknown = stored_sig.is_none();
    let returned = f.deleted_at.is_some() || f.item_deleted_at.is_some();
    let sidecars_changed = any_newer_than(&sidecar_sources, &f.scanned_at);
    Staleness {
        reindex: returned
            || content_changed
            || sidecars_changed
            || f.item_scan_version != MEDIA_SCAN_VERSION,
        needs_essential: returned
            || content_changed
            || content_unknown
            || f.subtitles_version < SUBTITLES_VERSION
            || f.markers_version < MARKERS_VERSION,
        needs_thumbnails: returned || content_changed || f.thumbnails_version < THUMBNAILS_VERSION,
        needs_trickplay: returned || content_changed || f.trickplay_version < TRICKPLAY_VERSION,
    }
}

/// Point the file row at `path` at item `media_id`, creating it on first
/// sight. Keeps the file's id when it moves between items, so everything
/// derived from its bytes (subtitles, trickplay, markers, HLS cache) moves
/// with it rather than being rebuilt. Returns the file id.
async fn upsert_file(
    pool: &SqlitePool,
    existing: Option<&ExistingFile>,
    media_id: &str,
    library_id: i64,
    path: &str,
    file_size: i64,
    added_at: &str,
) -> anyhow::Result<String> {
    let id = existing
        .map(|f| f.id.clone())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    sqlx::query(
        "INSERT INTO media_files (id, media_id, library_id, path, file_size, added_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(path) DO UPDATE SET
             media_id   = excluded.media_id,
             file_size  = excluded.file_size,
             deleted_at = NULL,
             scanned_at = datetime('now')",
    )
    .bind(&id)
    .bind(media_id)
    .bind(library_id)
    .bind(path)
    .bind(file_size)
    .bind(added_at)
    .execute(pool)
    .await?;
    if let Some(f) = existing.filter(|f| f.media_id != media_id) {
        info!(path, from = %f.media_id, to = %media_id, "file moved to another item");
    }
    Ok(id)
}

/// The item for episode `(show_id, season, episode)`. This is the lookup
/// that makes a replaced file keep its history: the new path finds the
/// episode's existing item — soft-deleted or not — instead of minting one.
/// `current` (the file's item, if it has one) wins while it still *is* this
/// episode; otherwise the oldest match, the survivor `reconcile_duplicates`
/// would pick. A new id only for an episode never seen before.
async fn episode_item(
    pool: &SqlitePool,
    current: Option<&str>,
    show_id: &str,
    season: i64,
    episode: i64,
) -> anyhow::Result<String> {
    let found: Option<String> = sqlx::query_scalar(
        "SELECT id FROM media
         WHERE kind = 'episode' AND show_id = ? AND season_number = ? AND episode_number = ?
         ORDER BY (id = ?) DESC, added_at, id
         LIMIT 1",
    )
    .bind(show_id)
    .bind(season)
    .bind(episode)
    .bind(current.unwrap_or(""))
    .fetch_optional(pool)
    .await?;
    Ok(found.unwrap_or_else(|| Uuid::new_v4().to_string()))
}

/// The item for a movie file. Movies have no key as intrinsic as an
/// episode's, so in order: the item the file already belongs to; an item
/// with the same tmdb/imdb id; an item in the same folder whose every file
/// has vanished from disk (Radarr's upgrade-in-place, NFO or not); otherwise
/// a new one. The folder rule is skipped at the library root, where
/// unrelated movies sit side by side.
async fn movie_item(
    pool: &SqlitePool,
    library_id: i64,
    library_root: &Path,
    current: Option<&ExistingFile>,
    tmdb_id: Option<&str>,
    imdb_id: Option<&str>,
    video: &Path,
) -> anyhow::Result<String> {
    if let Some(f) = current.filter(|f| f.item_kind == "movie") {
        return Ok(f.media_id.clone());
    }
    let tmdb_id = tmdb_id.filter(|s| !s.is_empty());
    let imdb_id = imdb_id.filter(|s| !s.is_empty());
    if tmdb_id.is_some() || imdb_id.is_some() {
        let found: Option<String> = sqlx::query_scalar(
            "SELECT id FROM media
             WHERE kind = 'movie' AND library_id = ?
               AND (tmdb_id = ? OR imdb_id = ?)
             ORDER BY added_at, id
             LIMIT 1",
        )
        .bind(library_id)
        .bind(tmdb_id)
        .bind(imdb_id)
        .fetch_optional(pool)
        .await?;
        if let Some(id) = found {
            return Ok(id);
        }
    }
    if let Some(dir) = video.parent().filter(|d| *d != library_root) {
        if let Some(id) = orphaned_movie_in(pool, library_id, dir).await? {
            return Ok(id);
        }
    }
    Ok(Uuid::new_v4().to_string())
}

/// A movie item with a file directly in `dir` and no file left on disk
/// anywhere — the old half of a replacement the walk hasn't pruned yet.
async fn orphaned_movie_in(
    pool: &SqlitePool,
    library_id: i64,
    dir: &Path,
) -> anyhow::Result<Option<String>> {
    let escaped = dir
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let rows: Vec<(String, String)> = sqlx::query_as(
        r"SELECT f.media_id, f.path FROM media_files f
          JOIN media m ON m.id = f.media_id
          WHERE m.kind = 'movie' AND m.library_id = ? AND f.path LIKE ? ESCAPE '\'
          ORDER BY m.added_at, m.id",
    )
    .bind(library_id)
    .bind(format!("{escaped}/%"))
    .fetch_all(pool)
    .await?;

    let mut checked: HashSet<String> = HashSet::new();
    for (media_id, path) in rows {
        if Path::new(&path).parent() != Some(dir) || !checked.insert(media_id.clone()) {
            continue;
        }
        let paths: Vec<String> =
            sqlx::query_scalar("SELECT path FROM media_files WHERE media_id = ?")
                .bind(&media_id)
                .fetch_all(pool)
                .await?;
        if !paths.iter().any(|p| Path::new(p).is_file()) {
            return Ok(Some(media_id));
        }
    }
    Ok(None)
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

    let existing = existing_file(pool, &path_str).await?;
    let stale = match &existing {
        Some(f) => staleness(f, video, file_size, nfo_opt.as_deref()),
        None => Staleness::NEW_FILE,
    };
    if !stale.any() {
        let id = existing.map(|f| f.id).unwrap_or_default();
        return Ok(Some(UpsertOutcome::unchanged(id, find_episode_thumb(video).is_some())));
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

    let media_id = episode_item(
        pool,
        existing.as_ref().map(|f| f.media_id.as_str()),
        show_id,
        season,
        episode,
    )
    .await?;

    // `added_at` is only written on insert: an item's "added" is when the
    // episode first appeared, which a replacement file shouldn't reset.
    let added_at = file_added_at(video);
    sqlx::query(
        r#"
        INSERT INTO media (
            id, library_id, kind,
            title, sort_title, plot, runtime_minutes, image_path,
            show_id, season_number, episode_number, added_at, scan_version,
            release_date
        )
        VALUES (?, ?, 'episode', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            title           = excluded.title,
            sort_title      = excluded.sort_title,
            plot            = excluded.plot,
            runtime_minutes = excluded.runtime_minutes,
            image_path      = excluded.image_path,
            scan_version    = excluded.scan_version,
            release_date    = excluded.release_date,
            deleted_at      = NULL,
            scanned_at      = datetime('now')
        "#,
    )
    .bind(&media_id)
    .bind(library_id)
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

    let file_id = upsert_file(
        pool,
        existing.as_ref(),
        &media_id,
        library_id,
        &path_str,
        file_size,
        &added_at,
    )
    .await?;

    // Episodes inherit genres from their show; no per-episode genre table needed.
    sqlx::query("DELETE FROM media_genres WHERE media_id = ?")
        .bind(&media_id)
        .execute(pool)
        .await?;

    debug!(title, season, episode, "indexed episode");
    Ok(Some(UpsertOutcome {
        file_id,
        has_sidecar_image: thumb.is_some(),
        re_indexed: true,
        needs_essential: stale.needs_essential,
        needs_thumbnails: stale.needs_thumbnails,
        needs_trickplay: stale.needs_trickplay,
    }))
}

async fn upsert_movie(
    pool: &SqlitePool,
    library_id: i64,
    library_root: &Path,
    video: &Path,
    file_size: i64,
) -> anyhow::Result<Option<UpsertOutcome>> {
    let path_str = video.to_string_lossy().into_owned();
    let base = video.file_stem().and_then(|s| s.to_str()).unwrap_or("Untitled");
    let nfo_path = matching_nfo(video);

    // Same staleness as episodes: the parent dir's mtime bumps when any
    // sidecar (poster / fanart / thumb) is added or removed on most
    // filesystems.
    let existing = existing_file(pool, &path_str).await?;
    let stale = match &existing {
        Some(f) => staleness(f, video, file_size, nfo_path.as_deref()),
        None => Staleness::NEW_FILE,
    };
    if !stale.any() {
        let id = existing.map(|f| f.id).unwrap_or_default();
        return Ok(Some(UpsertOutcome::unchanged(id, find_movie_image(video).is_some())));
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

    let media_id = movie_item(
        pool,
        library_id,
        library_root,
        existing.as_ref(),
        nfo.tmdb_id(),
        nfo.imdb_id(),
        video,
    )
    .await?;

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
            id, library_id, kind,
            title, sort_title, original_title, year, plot, runtime_minutes,
            imdb_id, tmdb_id, image_path, fanart_path, added_at, scan_version,
            rating, rating_votes, rating_source, mpaa, studio,
            tagline, release_date, director, writers
        )
        VALUES (?, ?, 'movie', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
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
            scanned_at      = datetime('now')
        "#,
    )
    .bind(&media_id)
    .bind(library_id)
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

    let file_id = upsert_file(
        pool,
        existing.as_ref(),
        &media_id,
        library_id,
        &path_str,
        file_size,
        &added_at,
    )
    .await?;

    sqlx::query("DELETE FROM media_genres WHERE media_id = ?")
        .bind(&media_id)
        .execute(pool)
        .await?;
    for g in &nfo.genre {
        sqlx::query("INSERT OR IGNORE INTO media_genres (media_id, genre) VALUES (?, ?)")
            .bind(&media_id)
            .bind(g)
            .execute(pool)
            .await?;
    }

    debug!(title, "indexed movie");
    Ok(Some(UpsertOutcome {
        file_id,
        has_sidecar_image: image.is_some(),
        re_indexed: true,
        needs_essential: stale.needs_essential,
        needs_thumbnails: stale.needs_thumbnails,
        needs_trickplay: stale.needs_trickplay,
    }))
}

#[cfg(test)]
mod tests {
    //! Item identity across file churn, end to end: a real migrated DB, real
    //! files on disk, full scans. The files are junk bytes, so the probe and
    //! asset passes fail softly — only indexing and pruning matter here.
    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        pool: SqlitePool,
        library_id: i64,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).unwrap();
        let pool = super::super::db::connect(&dir.path().join("test.db")).await.unwrap();
        let library_id = ensure_library(&pool, "test", &root).await.unwrap();
        Fixture { root: root.canonicalize().unwrap(), _dir: dir, pool, library_id }
    }

    impl Fixture {
        fn write(&self, rel: &str, body: &[u8]) -> PathBuf {
            let p = self.root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, body).unwrap();
            p
        }

        async fn scan(&self) {
            scan_library_with_progress(&self.pool, self.library_id, &self.root, None, None)
                .await
                .unwrap();
        }

        /// Live item ids for an episode.
        async fn episode_ids(&self, season: i64, episode: i64) -> Vec<String> {
            sqlx::query_scalar(
                "SELECT id FROM media WHERE kind = 'episode' AND deleted_at IS NULL
                   AND season_number = ? AND episode_number = ?",
            )
            .bind(season)
            .bind(episode)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        async fn live_movie_ids(&self) -> Vec<String> {
            sqlx::query_scalar("SELECT id FROM media WHERE kind = 'movie' AND deleted_at IS NULL")
                .fetch_all(&self.pool)
                .await
                .unwrap()
        }

        async fn set_progress(&self, media_id: &str, position: f64, updated_at: i64) {
            sqlx::query(
                "INSERT INTO watch_progress (user_sub, media_id, position_secs, duration_secs, completed, updated_at)
                 VALUES ('u', ?, ?, 1400.0, 0, ?)",
            )
            .bind(media_id)
            .bind(position)
            .bind(updated_at)
            .execute(&self.pool)
            .await
            .unwrap();
        }

        async fn progress(&self, media_id: &str) -> Option<f64> {
            sqlx::query_scalar(
                "SELECT position_secs FROM watch_progress WHERE user_sub = 'u' AND media_id = ?",
            )
            .bind(media_id)
            .fetch_optional(&self.pool)
            .await
            .unwrap()
        }

        async fn primary_path(&self, media_id: &str) -> Option<String> {
            super::super::files::primary(&self.pool, media_id)
                .await
                .unwrap()
                .map(|f| f.path)
        }
    }

    const TVSHOW_NFO: &[u8] = b"<tvshow><title>Show</title></tvshow>";

    #[tokio::test]
    async fn replacing_an_episode_file_keeps_its_item() {
        let fx = fixture().await;
        fx.write("Show/tvshow.nfo", TVSHOW_NFO);
        let old = fx.write("Show/Season 01/Show - S01E02 - WEBDL-720p.mkv", b"old");
        fx.scan().await;
        let ids = fx.episode_ids(1, 2).await;
        assert_eq!(ids.len(), 1);
        let item = ids[0].clone();
        fx.set_progress(&item, 600.0, 100).await;

        // Sonarr-style upgrade: new name, new bytes, old file gone.
        std::fs::remove_file(&old).unwrap();
        let new = fx.write("Show/Season 01/Show - S01E02 - Bluray-1080p.mkv", b"new and bigger");
        fx.scan().await;

        assert_eq!(fx.episode_ids(1, 2).await, vec![item.clone()]);
        assert_eq!(fx.progress(&item).await, Some(600.0));
        assert_eq!(
            fx.primary_path(&item).await.as_deref(),
            Some(new.canonicalize().unwrap().to_str().unwrap())
        );
        let retired: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM media_files WHERE media_id = ? AND deleted_at IS NOT NULL",
        )
        .bind(&item)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
        assert_eq!(retired, 1, "the old file is retired, not re-parented elsewhere");
    }

    #[tokio::test]
    async fn two_copies_of_an_episode_are_one_item_playing_the_larger() {
        let fx = fixture().await;
        fx.write("Show/tvshow.nfo", TVSHOW_NFO);
        fx.write("Show/Season 01/Show - S01E03 - 720p.mkv", b"small");
        let big = fx.write("Show/Season 01/Show - S01E03 - 1080p.mkv", b"considerably larger");
        fx.scan().await;

        let ids = fx.episode_ids(1, 3).await;
        assert_eq!(ids.len(), 1);
        assert_eq!(
            fx.primary_path(&ids[0]).await.as_deref(),
            Some(big.canonicalize().unwrap().to_str().unwrap())
        );
    }

    #[tokio::test]
    async fn reconcile_folds_a_stranded_pre_split_duplicate_into_the_oldest() {
        let fx = fixture().await;
        fx.write("Show/tvshow.nfo", TVSHOW_NFO);
        fx.write("Show/Season 01/Show - S01E04.mkv", b"current");
        fx.scan().await;
        let live = fx.episode_ids(1, 4).await.remove(0);
        fx.set_progress(&live, 30.0, 200).await;

        // What the old path-keyed schema left behind after a rename: an older,
        // soft-deleted item for the same episode holding the real history.
        let show_id: String = sqlx::query_scalar("SELECT show_id FROM media WHERE id = ?")
            .bind(&live)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO media (id, library_id, kind, title, show_id, season_number, episode_number,
                                added_at, deleted_at)
             VALUES ('stranded', ?, 'episode', 'Old', ?, 1, 4, '2020-01-01 00:00:00', '2021-01-01 00:00:00')",
        )
        .bind(fx.library_id)
        .bind(&show_id)
        .execute(&fx.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO media_files (id, media_id, library_id, path, file_size, deleted_at)
             VALUES ('stranded-file', 'stranded', ?, '/gone/Show - S01E04 - old.mkv', 1, '2021-01-01 00:00:00')",
        )
        .bind(fx.library_id)
        .execute(&fx.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO watch_progress (user_sub, media_id, position_secs, duration_secs, completed,
                                         updated_at, last_completed_at)
             VALUES ('u', 'stranded', 1350.0, 1400.0, 1, 100, 100)",
        )
        .execute(&fx.pool)
        .await
        .unwrap();

        fx.scan().await;

        // The oldest item survives, now live and backed by the current file…
        assert_eq!(fx.episode_ids(1, 4).await, vec!["stranded".to_string()]);
        assert!(fx.primary_path("stranded").await.unwrap().ends_with("Show - S01E04.mkv"));
        // …with the newer position, and "finished" kept even though the
        // newer row wasn't.
        let (position, completed, last): (f64, i64, Option<i64>) = sqlx::query_as(
            "SELECT position_secs, completed, last_completed_at FROM watch_progress
             WHERE user_sub = 'u' AND media_id = 'stranded'",
        )
        .fetch_one(&fx.pool)
        .await
        .unwrap();
        assert_eq!((position, completed, last), (30.0, 1, Some(100)));
        // …and metadata from the member that actually has the file.
        let title: String = sqlx::query_scalar("SELECT title FROM media WHERE id = 'stranded'")
            .fetch_one(&fx.pool)
            .await
            .unwrap();
        assert_ne!(title, "Old");
        assert_eq!(fx.progress(&live).await, None);
    }

    #[tokio::test]
    async fn upgrading_a_movie_in_its_folder_keeps_its_item() {
        let fx = fixture().await;
        let old = fx.write("Film (2020)/Film.2020.720p.mkv", b"old");
        fx.scan().await;
        let ids = fx.live_movie_ids().await;
        assert_eq!(ids.len(), 1);
        fx.set_progress(&ids[0], 1200.0, 100).await;

        std::fs::remove_file(&old).unwrap();
        fx.write("Film (2020)/Film.2020.1080p.mkv", b"new");
        fx.scan().await;

        assert_eq!(fx.live_movie_ids().await, ids);
        assert_eq!(fx.progress(&ids[0]).await, Some(1200.0));
    }

    #[tokio::test]
    async fn movies_side_by_side_at_the_root_stay_separate() {
        let fx = fixture().await;
        let a = fx.write("Alpha.2001.mkv", b"a");
        fx.scan().await;
        std::fs::remove_file(&a).unwrap();
        fx.write("Beta.2002.mkv", b"b");
        fx.scan().await;

        // No folder of its own, no shared id: Beta is a different movie.
        let ids = fx.live_movie_ids().await;
        assert_eq!(ids.len(), 1);
        let title: String = sqlx::query_scalar("SELECT title FROM media WHERE id = ?")
            .bind(&ids[0])
            .fetch_one(&fx.pool)
            .await
            .unwrap();
        assert_eq!(title, "Beta");
        let alpha_live: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM media WHERE title = 'Alpha' AND deleted_at IS NULL")
                .fetch_one(&fx.pool)
                .await
                .unwrap();
        assert_eq!(alpha_live, 0);
    }
}
