use super::nfo::{self, EpisodeNfo, MovieNfo};
use chrono::NaiveDateTime;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use tracing::{debug, info, warn};
use uuid::Uuid;
use walkdir::WalkDir;

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

pub async fn scan_library(
    pool: &SqlitePool,
    library_id: i64,
    root: &Path,
) -> anyhow::Result<ScanStats> {
    let started = std::time::Instant::now();
    info!(path = %root.display(), "scanning library");
    let root = root.canonicalize()?;

    let mut show_ids: HashMap<PathBuf, String> = HashMap::new();
    let mut stats = ScanStats::default();

    for entry in WalkDir::new(&root).follow_links(true).into_iter().flatten() {
        let path = entry.path();
        if !entry.file_type().is_file() || !is_video(path) {
            continue;
        }

        let abs = match path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                warn!(?path, %e, "skipping unreadable path");
                continue;
            }
        };
        let file_size = entry.metadata().map(|m| m.len() as i64).unwrap_or(0);

        match classify(&abs, &root) {
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
                    Ok(true) => stats.episodes_indexed += 1,
                    Ok(false) => stats.episodes_skipped += 1,
                    Err(e) => warn!(path = %abs.display(), %e, "failed to index episode"),
                }
            }
            Classification::Movie => {
                match upsert_movie(pool, library_id, &abs, file_size).await {
                    Ok(true) => stats.movies_indexed += 1,
                    Ok(false) => stats.movies_skipped += 1,
                    Err(e) => warn!(path = %abs.display(), %e, "failed to index movie"),
                }
            }
        }
    }

    info!(
        ?stats,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "scan complete"
    );
    Ok(stats)
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

async fn upsert_episode(
    pool: &SqlitePool,
    library_id: i64,
    show_id: &str,
    video: &Path,
    file_size: i64,
) -> anyhow::Result<bool> {
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

    if let Some((_id, scanned_at, existing_size)) = &existing {
        let mut sources: Vec<&Path> = vec![video];
        if let Some(n) = nfo_opt.as_ref() {
            sources.push(n);
        }
        if *existing_size == file_size && !any_newer_than(&sources, scanned_at) {
            return Ok(false);
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
                return Ok(false);
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
    Ok(true)
}

async fn upsert_movie(
    pool: &SqlitePool,
    library_id: i64,
    video: &Path,
    file_size: i64,
) -> anyhow::Result<bool> {
    let path_str = video.to_string_lossy().into_owned();
    let base = video.file_stem().and_then(|s| s.to_str()).unwrap_or("Untitled");
    let nfo_path = matching_nfo(video);

    let existing: Option<(String, String, i64)> =
        sqlx::query_as("SELECT id, scanned_at, file_size FROM media WHERE path = ?")
            .bind(&path_str)
            .fetch_optional(pool)
            .await?;

    if let Some((_id, scanned_at, existing_size)) = &existing {
        let mut sources: Vec<&Path> = vec![video];
        if let Some(n) = nfo_path.as_ref() {
            sources.push(n);
        }
        if *existing_size == file_size && !any_newer_than(&sources, scanned_at) {
            return Ok(false);
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
    Ok(true)
}
