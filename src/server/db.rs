use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection, SqlitePoolOptions};
use sqlx::{ConnectOptions, Connection};
use sqlx::SqlitePool;
use std::path::Path;
use std::str::FromStr;

pub async fn connect(db_path: &Path) -> anyhow::Result<SqlitePool> {
    if let Some(parent) = db_path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
    }

    let url = format!("sqlite://{}", db_path.display());
    let opts = SqliteConnectOptions::from_str(&url)?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .foreign_keys(true);

    migrate(&opts).await?;

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;

    Ok(pool)
}

/// Run migrations on a dedicated connection with foreign keys *off*.
///
/// A migration that rebuilds a parent table (0028 rebuilds `media`) must
/// `DROP` the old one, and with enforcement on that drop cascade-deletes
/// every child row — watch history included. SQLite only lets enforcement
/// change outside a transaction, and sqlx runs each SQLite migration inside
/// one (it ignores `-- no-transaction` for this backend), so the pragma has
/// to be set on the connection before sqlx starts. `foreign_key_check`
/// afterwards stands in for the enforcement that was skipped.
async fn migrate(opts: &SqliteConnectOptions) -> anyhow::Result<()> {
    let mut conn = opts.clone().foreign_keys(false).connect().await?;
    sqlx::migrate!("./migrations").run(&mut conn).await?;

    let violations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
        .fetch_one(&mut conn)
        .await?;
    if violations > 0 {
        tracing::warn!(violations, "foreign key violations after migrating; run `PRAGMA foreign_key_check`");
    }

    vacuum_if_bloated(&mut conn).await;
    conn.close().await?;
    Ok(())
}

/// Reclaim space when over a quarter of the file is free pages.
///
/// SQLite keeps a dropped table's pages as free space rather than shrinking
/// the file, so a migration that rebuilds a table leaves the old copy's
/// footprint behind (0028 left ~900 MB on prod, mostly subtitle and
/// trickplay blobs). Day to day those pages are reused and this never fires;
/// it's for the boot after a rebuild. Best-effort: VACUUM needs about the
/// DB's size in spare disk, and failing it only means the file stays big.
async fn vacuum_if_bloated(conn: &mut SqliteConnection) {
    let sizes: Result<(i64, i64), _> = sqlx::query_as(
        "SELECT (SELECT freelist_count FROM pragma_freelist_count),
                (SELECT page_count FROM pragma_page_count)",
    )
    .fetch_one(&mut *conn)
    .await;
    let Ok((free, total)) = sizes else { return };
    if free * 4 <= total {
        return;
    }
    tracing::info!(free_pages = free, total_pages = total, "vacuuming database");
    let started = std::time::Instant::now();
    match sqlx::query("VACUUM").execute(&mut *conn).await {
        Ok(_) => tracing::info!(elapsed_ms = started.elapsed().as_millis() as u64, "vacuum complete"),
        Err(e) => tracing::warn!(%e, "vacuum failed; database keeps its free pages"),
    }
}
