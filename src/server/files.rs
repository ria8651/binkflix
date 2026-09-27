//! Item ↔ file resolution.
//!
//! A `media` row is the thing you watch — an episode or a movie — and keeps
//! its id however the bytes behind it change. The bytes live in `media_files`
//! (see migration 0028): a quality upgrade or rename adds a file row and
//! retires the old one without touching the item, so watch history, prefs
//! and URLs stay put.
//!
//! Everything client-facing addresses the item; everything derived from the
//! bytes (probe, stream plan, subtitles, thumbnails, trickplay, markers, HLS
//! cache) addresses the file. Handlers cross that line exactly once, through
//! [`primary`].

use sqlx::SqlitePool;

pub struct FileRef {
    pub id: String,
    pub path: String,
}

/// Correlated subquery for the id of the file item `item_col` plays from:
/// its largest live file. Largest because two live files for one item are
/// almost always two qualities of the same thing, and the id tie-break keeps
/// the pick stable. `item_col` is spliced in verbatim — pass a column
/// reference like `"m.id"` or a `?` placeholder, never user input.
pub fn primary_file_id(item_col: &str) -> String {
    format!(
        "(SELECT pf.id FROM media_files pf
           WHERE pf.media_id = {item_col} AND pf.deleted_at IS NULL
           ORDER BY pf.file_size DESC, pf.id
           LIMIT 1)"
    )
}

/// The file `media_id` currently plays from, or `None` when the item is
/// unknown or has no live file.
pub async fn primary(pool: &SqlitePool, media_id: &str) -> sqlx::Result<Option<FileRef>> {
    let sql = format!(
        "SELECT id, path FROM media_files WHERE id = {}",
        primary_file_id("?")
    );
    let row: Option<(String, String)> = sqlx::query_as(&sql)
        .bind(media_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(id, path)| FileRef { id, path }))
}
