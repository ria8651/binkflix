//! `binkflix cleanup [--apply]` — list soft-deleted media/shows/libraries
//! and optionally purge them. Hard deletion here is the only path that
//! actually drops the rows; the scanner only sets `deleted_at`.

use std::path::PathBuf;

use sqlx::SqlitePool;

pub async fn run(apply: bool) -> anyhow::Result<()> {
    let db_path = PathBuf::from(
        std::env::var("BINKFLIX_DB").unwrap_or_else(|_| "./data/binkflix.db".into()),
    );
    let pool = super::super::db::connect(&db_path).await?;

    let libraries: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT id, path, deleted_at FROM libraries WHERE deleted_at IS NOT NULL ORDER BY deleted_at",
    )
    .fetch_all(&pool)
    .await?;
    let shows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id, title, deleted_at FROM shows WHERE deleted_at IS NOT NULL ORDER BY deleted_at",
    )
    .fetch_all(&pool)
    .await?;
    let media: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT id, kind, path, deleted_at FROM media WHERE deleted_at IS NOT NULL ORDER BY deleted_at",
    )
    .fetch_all(&pool)
    .await?;

    let total = libraries.len() + shows.len() + media.len();
    if total == 0 {
        println!("No soft-deleted rows. Nothing to clean up.");
        return Ok(());
    }

    println!("Soft-deleted rows ({} total):\n", total);
    if !libraries.is_empty() {
        println!("Libraries ({}):", libraries.len());
        for (id, path, deleted_at) in &libraries {
            println!("  [{deleted_at}] id={id}  {path}");
        }
        println!();
    }
    if !shows.is_empty() {
        println!("Shows ({}):", shows.len());
        for (id, title, deleted_at) in &shows {
            println!("  [{deleted_at}] {id}  {title}");
        }
        println!();
    }
    if !media.is_empty() {
        println!("Media ({}):", media.len());
        for (id, kind, path, deleted_at) in &media {
            println!("  [{deleted_at}] {kind} {id}  {path}");
        }
        println!();
    }

    if !apply {
        println!("Dry run — re-run with `--apply` to hard-delete these rows.");
        println!("Note: cascading FKs will also remove watch_progress, subtitles, thumbnails, etc. for purged media.");
        return Ok(());
    }

    let removed = purge(&pool).await?;
    println!("Purged {removed} rows.");
    Ok(())
}

async fn purge(pool: &SqlitePool) -> anyhow::Result<u64> {
    // Order matters only loosely (cascades make it irrelevant), but explicit
    // top-down ordering keeps the trace easy to follow.
    let mut total: u64 = 0;
    let res = sqlx::query("DELETE FROM media WHERE deleted_at IS NOT NULL")
        .execute(pool)
        .await?;
    total += res.rows_affected();
    let res = sqlx::query("DELETE FROM shows WHERE deleted_at IS NOT NULL")
        .execute(pool)
        .await?;
    total += res.rows_affected();
    let res = sqlx::query("DELETE FROM libraries WHERE deleted_at IS NOT NULL")
        .execute(pool)
        .await?;
    total += res.rows_affected();
    Ok(total)
}
