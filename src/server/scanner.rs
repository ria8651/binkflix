use super::nfo::{self, EpisodeNfo, MovieNfo};
use super::{subtitles, thumbnails};
use crate::types::ScanProgress;
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

async fn set_progress(handle: &ProgressHandle, f: impl FnOnce(&mut ScanProgress)) {
    let mut p = handle.write().await;
    f(&mut p);
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
        if nfo::parse_sxxeyy(stem).is_some() {
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

    if let Some((id,)) = sqlx::query_as::<_, (i64,)>("SELECT id FROM libraries WHERE path = ?")
        .bind(&path_str)
        .fetch_optional(pool)
        .await?
    {
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

pub async fn scan_library(
    pool: &SqlitePool,
    library_id: i64,
    root: &Path,
) -> anyhow::Result<ScanStats> {
    scan_library_with_progress(pool, library_id, root, None).await
}

pub async fn scan_library_with_progress(
    pool: &SqlitePool,
    library_id: i64,
    root: &Path,
    progress: Option<ProgressHandle>,
) -> anyhow::Result<ScanStats> {
    let started = std::time::Instant::now();
    info!(path = %root.display(), "scanning library");
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
                match upsert_episode(pool, library_id, &show_id, &abs, file_size).await {
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

    // --- Phase 2: extract subtitles + thumbnails in parallel. Bounded
    // concurrency so we don't thrash spinning disks; each job is independent
    // and writes back to the pool directly.
    let total = asset_jobs.len();
    if let Some(p) = &progress {
        set_progress(p, |s| {
            s.phase = "assets".into();
            s.done = 0;
            s.total = total;
            s.current = None;
        })
        .await;
    }
    if total > 0 {
        let assets_started = std::time::Instant::now();
        let pool = pool.clone();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let progress = progress.clone();
        stream::iter(asset_jobs.into_iter())
            .for_each_concurrent(concurrency, |job| {
                let pool = pool.clone();
                let done = done.clone();
                let progress = progress.clone();
                async move {
                    let job_started = std::time::Instant::now();
                    let sub_count = match subtitles::scan_for_media(&pool, &job.media_id, &job.video).await {
                        Ok(n) => n,
                        Err(e) => {
                            warn!(media_id = %job.media_id, %e, "subtitle scan failed");
                            0
                        }
                    };
                    if !job.has_sidecar_image {
                        thumbnails::scan_for_media(&pool, &job.media_id, &job.video).await;
                    }
                    let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if let Some(p) = &progress {
                        let title = job.title.clone();
                        set_progress(p, |s| {
                            s.done = n;
                            s.current = Some(title);
                        })
                        .await;
                    }
                    info!(
                        progress = format!("{n}/{total}"),
                        title = %job.title,
                        subs = sub_count,
                        thumb = !job.has_sidecar_image,
                        elapsed_ms = job_started.elapsed().as_millis() as u64,
                        "assets extracted",
                    );
                }
            })
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

/// Delete `libraries` rows (and via FK cascade their shows + media +
/// subtitles + thumbnails) whose id isn't in `active_ids`. Called at
/// startup after registering the currently-configured library paths.
pub async fn prune_libraries(pool: &SqlitePool, active_ids: &[i64]) -> anyhow::Result<u64> {
    // Refuse to wipe everything if the env var is empty — callers already
    // bail before reaching here, but belt-and-braces.
    if active_ids.is_empty() {
        return Ok(0);
    }
    // Fetch + filter in Rust rather than building a dynamic `NOT IN (?, ?, …)`
    // binding list. N is tiny (one per configured library path).
    let all: Vec<(i64,)> = sqlx::query_as("SELECT id FROM libraries")
        .fetch_all(pool)
        .await?;
    let mut removed: u64 = 0;
    for (id,) in all {
        if active_ids.contains(&id) {
            continue;
        }
        let res = sqlx::query("DELETE FROM libraries WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
        removed += res.rows_affected();
        debug!(library_id = id, "pruned library");
    }
    Ok(removed)
}


/// Delete media rows in this library whose path wasn't seen during the walk,
/// then drop any show whose entire episode set has vanished.
///
/// Returns the total number of rows removed (media + shows). FK cascades
/// clean up subtitles, thumbnails, and media_genres automatically.
async fn prune_missing(
    pool: &SqlitePool,
    library_id: i64,
    seen: &HashSet<String>,
) -> anyhow::Result<u64> {
    let existing: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, path FROM media WHERE library_id = ?",
    )
    .bind(library_id)
    .fetch_all(pool)
    .await?;

    let to_delete: Vec<String> = existing
        .into_iter()
        .filter(|(_, p)| !seen.contains(p))
        .map(|(id, _)| id)
        .collect();

    let mut removed: u64 = 0;
    for id in &to_delete {
        let res = sqlx::query("DELETE FROM media WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
        removed += res.rows_affected();
        debug!(media_id = %id, "pruned media");
    }

    // Shows whose every episode is gone. We don't track seen show dirs
    // separately because a show can legitimately have zero rows if its
    // episodes were all just deleted — that's the case we want to act on.
    let orphan_shows: Vec<(String,)> = sqlx::query_as(
        "SELECT id FROM shows
         WHERE library_id = ?
           AND NOT EXISTS (SELECT 1 FROM media WHERE media.show_id = shows.id)",
    )
    .bind(library_id)
    .fetch_all(pool)
    .await?;

    for (id,) in &orphan_shows {
        let res = sqlx::query("DELETE FROM shows WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
        removed += res.rows_affected();
        debug!(show_id = %id, "pruned empty show");
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

    let existing: Option<(String, String)> =
        sqlx::query_as("SELECT id, scanned_at FROM shows WHERE path = ?")
            .bind(&path_str)
            .fetch_optional(pool)
            .await?;

    if let Some((id, scanned_at)) = &existing {
        if !any_newer_than(&[&nfo_path], scanned_at) {
            return Ok((id.clone(), false));
        }
    }

    let id = existing
        .map(|(id, _)| id)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let nfo = nfo::parse_tvshow_nfo(&nfo_path).unwrap_or_default();
    let title = nfo.title.clone().unwrap_or_else(|| {
        show_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Untitled Show")
            .to_string()
    });
    let poster = find_show_poster(show_dir).map(|p| p.to_string_lossy().into_owned());
    let fanart = find_show_fanart(show_dir).map(|p| p.to_string_lossy().into_owned());
    let tvdb_id = nfo
        .uniqueid
        .iter()
        .find(|u| u.kind.eq_ignore_ascii_case("tvdb"))
        .map(|u| u.value.clone());

    sqlx::query(
        r#"
        INSERT INTO shows (
            id, library_id, path, title, original_title, year, plot,
            imdb_id, tmdb_id, tvdb_id, poster_path, fanart_path
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(path) DO UPDATE SET
            title = excluded.title,
            original_title = excluded.original_title,
            year = excluded.year,
            plot = excluded.plot,
            imdb_id = excluded.imdb_id,
            tmdb_id = excluded.tmdb_id,
            tvdb_id = excluded.tvdb_id,
            poster_path = excluded.poster_path,
            fanart_path = excluded.fanart_path,
            scanned_at = datetime('now')
        "#,
    )
    .bind(&id)
    .bind(library_id)
    .bind(&path_str)
    .bind(&title)
    .bind(&nfo.original_title)
    .bind(nfo.year_or_premiered())
    .bind(&nfo.plot)
    .bind(nfo.imdb_id())
    .bind(nfo.tmdb_id())
    .bind(&tvdb_id)
    .bind(&poster)
    .bind(&fanart)
    .execute(pool)
    .await?;

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

async fn upsert_episode(
    pool: &SqlitePool,
    library_id: i64,
    show_id: &str,
    video: &Path,
    file_size: i64,
) -> anyhow::Result<Option<UpsertOutcome>> {
    let path_str = video.to_string_lossy().into_owned();
    let base = video.file_stem().and_then(|s| s.to_str()).unwrap_or("episode");
    let nfo_path = video.with_extension("nfo");
    let nfo_opt = nfo_path.is_file().then_some(nfo_path);

    // Skip if unchanged.
    let existing: Option<(String, String, i64)> =
        sqlx::query_as("SELECT id, scanned_at, file_size FROM media WHERE path = ?")
            .bind(&path_str)
            .fetch_optional(pool)
            .await?;

    if let Some((id, scanned_at, existing_size)) = &existing {
        let mut sources: Vec<&Path> = vec![video];
        if let Some(n) = nfo_opt.as_ref() {
            sources.push(n);
        }
        if *existing_size == file_size && !any_newer_than(&sources, scanned_at) {
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
        _ => match nfo::parse_sxxeyy(base) {
            Some((s, e)) => (s, e),
            None => {
                warn!(file = base, "cannot determine season/episode; skipping");
                return Ok(None);
            }
        },
    };

    let title = nfo.title.clone().unwrap_or_else(|| base.to_string());
    let thumb = find_episode_thumb(video).map(|p| p.to_string_lossy().into_owned());

    let id = existing
        .map(|(id, _, _)| id)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    sqlx::query(
        r#"
        INSERT INTO media (
            id, library_id, kind, path, file_size,
            title, plot, runtime_minutes, image_path,
            show_id, season_number, episode_number
        )
        VALUES (?, ?, 'episode', ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(path) DO UPDATE SET
            kind            = 'episode',
            title           = excluded.title,
            plot            = excluded.plot,
            runtime_minutes = excluded.runtime_minutes,
            image_path      = excluded.image_path,
            show_id         = excluded.show_id,
            season_number   = excluded.season_number,
            episode_number  = excluded.episode_number,
            file_size       = excluded.file_size,
            -- clear movie-only fields in case this row was previously a movie
            original_title  = NULL,
            year            = NULL,
            imdb_id         = NULL,
            tmdb_id         = NULL,
            fanart_path     = NULL,
            scanned_at      = datetime('now')
        "#,
    )
    .bind(&id)
    .bind(library_id)
    .bind(&path_str)
    .bind(file_size)
    .bind(&title)
    .bind(&nfo.plot)
    .bind(nfo.runtime)
    .bind(&thumb)
    .bind(show_id)
    .bind(season)
    .bind(episode)
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

    let existing: Option<(String, String, i64)> =
        sqlx::query_as("SELECT id, scanned_at, file_size FROM media WHERE path = ?")
            .bind(&path_str)
            .fetch_optional(pool)
            .await?;

    if let Some((id, scanned_at, existing_size)) = &existing {
        let mut sources: Vec<&Path> = vec![video];
        if let Some(n) = nfo_path.as_ref() {
            sources.push(n);
        }
        if *existing_size == file_size && !any_newer_than(&sources, scanned_at) {
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

    let title = nfo.title.clone().unwrap_or_else(|| base.to_string());
    let image = find_movie_image(video).map(|p| p.to_string_lossy().into_owned());
    let fanart = find_movie_fanart(video).map(|p| p.to_string_lossy().into_owned());

    let id = existing
        .map(|(id, _, _)| id)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    sqlx::query(
        r#"
        INSERT INTO media (
            id, library_id, kind, path, file_size,
            title, original_title, year, plot, runtime_minutes,
            imdb_id, tmdb_id, image_path, fanart_path
        )
        VALUES (?, ?, 'movie', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(path) DO UPDATE SET
            kind            = 'movie',
            title           = excluded.title,
            original_title  = excluded.original_title,
            year            = excluded.year,
            plot            = excluded.plot,
            runtime_minutes = excluded.runtime_minutes,
            imdb_id         = excluded.imdb_id,
            tmdb_id         = excluded.tmdb_id,
            image_path      = excluded.image_path,
            fanart_path     = excluded.fanart_path,
            file_size       = excluded.file_size,
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
    .bind(&nfo.original_title)
    .bind(nfo.year)
    .bind(&nfo.plot)
    .bind(nfo.runtime)
    .bind(nfo.imdb_id())
    .bind(nfo.tmdb_id())
    .bind(&image)
    .bind(&fanart)
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
