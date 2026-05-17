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
use std::time::UNIX_EPOCH;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;
use walkdir::WalkDir;

pub type ProgressHandle = Arc<RwLock<ScanProgress>>;

/// Bump these whenever the corresponding `upsert_*` function changes what it
/// persists — e.g. starts writing a new column, populating a join table, or
/// reading a previously-ignored NFO field. Existing rows have an older
/// `scan_version` and the early-return guard treats them as stale, forcing a
/// re-upsert at the next scan. The constants live next to the upserts they
/// gate so the reason for a bump is visible in the diff.
const SHOW_SCAN_VERSION: i64 = 3;
const MEDIA_SCAN_VERSION: i64 = 3;

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

/// Work item carried from the index pass into the asset pass.
struct AssetJob {
    media_id: String,
    video: PathBuf,
    title: String,
    has_sidecar_image: bool,
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
    for entry in WalkDir::new(&root).follow_links(true).into_iter().flatten() {
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
                        if out.needs_assets { stats.episodes_indexed += 1; } else { stats.episodes_skipped += 1; }
                        Some(out)
                    }
                    Ok(None) => { stats.episodes_skipped += 1; None }
                    Err(e) => { warn!(path = %abs.display(), %e, "failed to index episode"); None }
                }
            }
            Classification::Movie => {
                match upsert_movie(pool, library_id, &abs, file_size).await {
                    Ok(Some(out)) => {
                        if out.needs_assets { stats.movies_indexed += 1; } else { stats.movies_skipped += 1; }
                        Some(out)
                    }
                    Ok(None) => { stats.movies_skipped += 1; None }
                    Err(e) => { warn!(path = %abs.display(), %e, "failed to index movie"); None }
                }
            }
        };

        if let Some(out) = outcome {
            if out.needs_assets {
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

        // Per-file accumulator carried through the four passes. We capture
        // each stage's elapsed_ms here and INSERT one `scan_timings` row at
        // the end so the row is whole rather than written piecemeal.
        // Per-file accumulator threaded through the four passes. `save_ms`
        // is computed inline in pass 4 (no need to store).
        struct PerFile {
            job: AssetJob,
            tech_info: Option<crate::types::MediaTechInfo>,
            sub_tracks: u32,
            probe_ms: u64,
            subtitles_ms: u64,
            thumbnail_ms: u64,
            trickplay_ms: u64,
            keyframe_count: Option<u32>,
            started: std::time::Instant,
        }

        // -- Pass 1: probe + subtitles -----------------------------------
        if let Some(p) = &progress {
            set_progress(p, |s| {
                s.phase = "subtitles".into();
                s.done = 0;
                s.total = total;
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
                async move {
                    let started = std::time::Instant::now();
                    if let Some(p) = &progress {
                        add_active(p, &job.media_id, &job.title, "subtitles").await;
                    }
                    let t = std::time::Instant::now();
                    let (tech_info, embedded_subs) = match super::media_info::probe_full(&job.video).await {
                        Ok(pair) => (Some(pair.0), pair.1),
                        Err(e) => {
                            warn!(media_id = %job.media_id, %e, "ffprobe failed");
                            (None, Vec::new())
                        }
                    };
                    let probe_ms = t.elapsed().as_millis() as u64;

                    let t = std::time::Instant::now();
                    let sub_count = match subtitles::scan_for_media(&pool, &job.media_id, &job.video, &embedded_subs).await {
                        Ok(n) => n,
                        Err(e) => {
                            warn!(media_id = %job.media_id, %e, "subtitle scan failed");
                            0
                        }
                    };
                    let subtitles_ms = t.elapsed().as_millis() as u64;

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
                        progress = format!("{n}/{total}"),
                        title = %job.title,
                        subs = sub_count,
                        elapsed_ms = subtitles_ms,
                        "subtitles done",
                    );
                    PerFile {
                        job,
                        tech_info,
                        sub_tracks: embedded_subs.len() as u32,
                        probe_ms,
                        subtitles_ms,
                        thumbnail_ms: 0,
                        trickplay_ms: 0,
                        keyframe_count: None,
                        started,
                    }
                }
            })
            .buffer_unordered(concurrency)
            .collect()
            .await;
        info!(
            total = pass1.len(),
            elapsed_ms = assets_started.elapsed().as_millis() as u64,
            "subtitles pass complete",
        );

        // -- Pass 2: thumbnails ------------------------------------------
        if let Some(p) = &progress {
            set_progress(p, |s| {
                s.phase = "thumbnails".into();
                s.done = 0;
                s.total = pass1.len();
                s.active.clear();
            })
            .await;
        }
        let done_p2 = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pass1_total = pass1.len();
        let pass2: Vec<PerFile> = stream::iter(pass1.into_iter())
            .map(|mut f| {
                let pool = pool.clone();
                let progress = progress.clone();
                let done = done_p2.clone();
                async move {
                    if !f.job.has_sidecar_image {
                        if let Some(p) = &progress {
                            add_active(p, &f.job.media_id, &f.job.title, "thumbnail").await;
                        }
                        let t = std::time::Instant::now();
                        thumbnails::scan_for_media(&pool, &f.job.media_id, &f.job.video).await;
                        f.thumbnail_ms = t.elapsed().as_millis() as u64;
                    }
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
            total = pass1_total,
            elapsed_ms = assets_started.elapsed().as_millis() as u64,
            "thumbnails pass complete",
        );

        // -- Pass 3: trickplay -------------------------------------------
        if let Some(p) = &progress {
            set_progress(p, |s| {
                s.phase = "trickplay".into();
                s.done = 0;
                s.total = pass2.len();
                s.active.clear();
            })
            .await;
        }
        let done_p3 = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pass3: Vec<PerFile> = stream::iter(pass2.into_iter())
            .map(|mut f| {
                let pool = pool.clone();
                let progress = progress.clone();
                let done = done_p3.clone();
                async move {
                    if let Some(p) = &progress {
                        add_active(p, &f.job.media_id, &f.job.title, "trickplay").await;
                    }
                    let t = std::time::Instant::now();
                    f.keyframe_count = trickplay::scan_for_media(
                        &pool,
                        &f.job.media_id,
                        &f.job.video,
                        f.tech_info.as_ref().and_then(|i| i.duration_seconds),
                    )
                    .await;
                    f.trickplay_ms = t.elapsed().as_millis() as u64;
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

        // -- Pass 4: save tech info + record scan_timings -----------------
        if let Some(p) = &progress {
            set_progress(p, |s| {
                s.phase = "saving".into();
                s.done = 0;
                s.total = pass3.len();
                s.active.clear();
            })
            .await;
        }
        let done_p4 = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        stream::iter(pass3.into_iter())
            .map(|mut f| {
                let pool = pool.clone();
                let progress = progress.clone();
                let done = done_p4.clone();
                async move {
                    if let Some(p) = &progress {
                        add_active(p, &f.job.media_id, &f.job.title, "saving").await;
                    }
                    // Pull source-side fields out before the `take()` below
                    // hands ownership of `tech_info` to media_info::store.
                    // Default audio = the track flagged `default`, else the
                    // first one, mirroring how compute_compat picks it.
                    let (
                        video_codec, audio_codec, container,
                        width, height, duration_ms, bitrate_kbps, pixel_format,
                    ) = match f.tech_info.as_ref() {
                        Some(info) => {
                            let v = info.video.as_ref();
                            let a = info.audio.iter().find(|a| a.default).or_else(|| info.audio.first());
                            (
                                v.map(|v| v.codec.clone()),
                                a.map(|a| a.codec.clone()),
                                info.container.clone(),
                                v.and_then(|v| v.width),
                                v.and_then(|v| v.height),
                                info.duration_seconds.map(|s| (s * 1000.0) as u64),
                                info.bitrate_kbps,
                                v.and_then(|v| v.pix_fmt.clone()),
                            )
                        }
                        None => (None, None, None, None, None, None, None, None),
                    };

                    let t = std::time::Instant::now();
                    if let Some(info) = f.tech_info.take() {
                        if let Err(e) = super::media_info::store(&pool, &f.job.media_id, &info).await {
                            warn!(media_id = %f.job.media_id, %e, "failed to cache tech info");
                        }
                    }
                    let save_ms = t.elapsed().as_millis() as u64;

                    let total_ms = f.started.elapsed().as_millis() as u64;
                    analytics::record_scan_timing(
                        &pool,
                        &f.job.media_id,
                        ScanTiming {
                            probe_ms: f.probe_ms,
                            subtitles_ms: f.subtitles_ms,
                            subtitle_tracks: f.sub_tracks,
                            thumbnail_ms: f.thumbnail_ms,
                            trickplay_ms: f.trickplay_ms,
                            save_ms,
                            total_ms,
                            video_codec,
                            audio_codec,
                            container,
                            width,
                            height,
                            duration_ms,
                            bitrate_kbps,
                            pixel_format,
                            keyframe_count: f.keyframe_count,
                        },
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
                        progress = format!("{n}/{total}"),
                        title = %f.job.title,
                        elapsed_ms = total_ms,
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

    info!(
        ?stats,
        total_elapsed_ms = started.elapsed().as_millis() as u64,
        "scan complete"
    );
    Ok(stats)
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
/// `needs_assets = true` means we re-indexed the row (new or changed), so
/// the caller should (re)extract subtitles + thumbnails for it.
/// `has_sidecar_image` lets the caller skip thumbnail generation when the
/// library already supplies one.
pub struct UpsertOutcome {
    pub id: String,
    pub needs_assets: bool,
    pub has_sidecar_image: bool,
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

    // Skip if unchanged.
    let existing: Option<(String, String, i64, i64, Option<String>)> = sqlx::query_as(
        "SELECT id, scanned_at, file_size, scan_version, deleted_at FROM media WHERE path = ?",
    )
    .bind(&path_str)
    .fetch_optional(pool)
    .await?;

    if let Some((id, scanned_at, existing_size, scan_version, deleted_at)) = &existing {
        let mut sources: Vec<&Path> = vec![video];
        if let Some(n) = nfo_opt.as_ref() {
            sources.push(n);
        }
        // Parent dir mtime catches sidecar thumb add/remove.
        if let Some(parent) = video.parent() {
            sources.push(parent);
        }
        // A soft-deleted row always re-upserts so `deleted_at` gets cleared.
        if deleted_at.is_none()
            && *scan_version == MEDIA_SCAN_VERSION
            && *existing_size == file_size
            && !any_newer_than(&sources, scanned_at)
        {
            // Unchanged — still report the id so the caller can decide.
            return Ok(Some(UpsertOutcome {
                id: id.clone(),
                needs_assets: false,
                has_sidecar_image: find_episode_thumb(video).is_some(),
            }));
        }
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
        .map(|(id, _, _, _, _)| id)
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
        needs_assets: true,
        has_sidecar_image: thumb.is_some(),
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

    let existing: Option<(String, String, i64, i64, Option<String>)> = sqlx::query_as(
        "SELECT id, scanned_at, file_size, scan_version, deleted_at FROM media WHERE path = ?",
    )
    .bind(&path_str)
    .fetch_optional(pool)
    .await?;

    if let Some((id, scanned_at, existing_size, scan_version, deleted_at)) = &existing {
        // The video, the NFO, and any sidecar (poster / fanart / thumb) all
        // affect what we'd write to the DB. Tracking the parent directory's
        // mtime in addition to the video covers sidecar add/remove/replace —
        // most filesystems bump the dir's mtime when a child is added or
        // deleted, so we don't have to enumerate every possible sidecar
        // candidate path.
        let mut sources: Vec<&Path> = vec![video];
        if let Some(n) = nfo_path.as_ref() {
            sources.push(n);
        }
        if let Some(parent) = video.parent() {
            sources.push(parent);
        }
        // A soft-deleted row always re-upserts so `deleted_at` gets cleared.
        if deleted_at.is_none()
            && *scan_version == MEDIA_SCAN_VERSION
            && *existing_size == file_size
            && !any_newer_than(&sources, scanned_at)
        {
            return Ok(Some(UpsertOutcome {
                id: id.clone(),
                needs_assets: false,
                has_sidecar_image: find_movie_image(video).is_some(),
            }));
        }
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
        .map(|(id, _, _, _, _)| id)
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
        needs_assets: true,
        has_sidecar_image: image.is_some(),
    }))
}
