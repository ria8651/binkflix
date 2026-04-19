//! Server-side runtime: DB, filesystem scanner, REST + WebSocket routes.
//! Everything in this tree is `#[cfg(feature = "server")]` — never compiled for the WASM client.

pub mod api;
pub mod db;
pub mod error;
pub mod media_info;
pub mod nfo;
pub mod scanner;
pub mod subtitles;
pub mod syncplay;
pub mod thumbnails;

use crate::app::App;
use crate::types::ScanProgress;
use axum::Router;
use dioxus::prelude::{DioxusRouterExt, ServeConfig};
use sqlx::SqlitePool;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub hub: Arc<syncplay::Hub>,
    pub scan_progress: scanner::ProgressHandle,
    pub scan_lock: Arc<Mutex<()>>,
    pub libraries: Arc<Vec<(i64, PathBuf)>>,
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Runs each library scan serially, updating the shared progress handle so
/// the UI's rescan button has live feedback. Clears `running` when done.
pub async fn run_scans(
    pool: &SqlitePool,
    jobs: &[(i64, PathBuf)],
    progress: scanner::ProgressHandle,
) {
    let started = std::time::Instant::now();
    // Preserve last_* across runs so the UI can keep showing the previous
    // summary until this scan replaces it.
    let (prev_finished, prev_summary, prev_elapsed) = {
        let p = progress.read().await;
        (p.last_finished_at, p.last_summary.clone(), p.last_elapsed_ms)
    };
    {
        let mut p = progress.write().await;
        *p = ScanProgress {
            running: true,
            phase: "starting".into(),
            done: 0,
            total: 0,
            current: None,
            message: None,
            last_finished_at: prev_finished,
            last_summary: prev_summary,
            last_elapsed_ms: prev_elapsed,
        };
    }
    let mut agg = scanner::ScanStats::default();
    let mut err: Option<String> = None;
    for (id, root) in jobs {
        match scanner::scan_library_with_progress(pool, *id, root, Some(progress.clone())).await {
            Ok(s) => {
                agg.movies_indexed += s.movies_indexed;
                agg.movies_skipped += s.movies_skipped;
                agg.episodes_indexed += s.episodes_indexed;
                agg.episodes_skipped += s.episodes_skipped;
                agg.shows_indexed += s.shows_indexed;
                agg.shows_skipped += s.shows_skipped;
            }
            Err(e) => {
                tracing::error!(%e, root = %root.display(), "scan failed");
                err = Some(format!("scan failed: {e}"));
            }
        }
    }
    let indexed = agg.movies_indexed + agg.episodes_indexed + agg.shows_indexed;
    let skipped = agg.movies_skipped + agg.episodes_skipped + agg.shows_skipped;
    let summary = if indexed == 0 && skipped == 0 {
        "nothing to scan".to_string()
    } else if indexed == 0 {
        format!("everything up to date ({skipped} unchanged)")
    } else {
        format!("{indexed} indexed · {skipped} unchanged")
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut p = progress.write().await;
    p.running = false;
    p.phase = "idle".into();
    p.current = None;
    p.message = err;
    p.last_finished_at = Some(now);
    p.last_summary = Some(summary);
    p.last_elapsed_ms = Some(started.elapsed().as_millis() as u64);
}

pub fn run() {
    let _ = dotenvy::dotenv();

    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,binkflix=debug")),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    if let Err(e) = rt.block_on(run_async()) {
        tracing::error!(%e, "server exited with error");
        std::process::exit(1);
    }
}

async fn run_async() -> anyhow::Result<()> {
    let library_paths: Vec<PathBuf> = env_or("BINKFLIX_LIBRARY", "./library")
        .split(':')
        .filter(|s| !s.is_empty())
        .map(|s| PathBuf::from(s.trim()))
        .collect();
    let db_path = PathBuf::from(env_or("BINKFLIX_DB", "./data/binkflix.db"));

    // Explicit override wins; otherwise fall back to the address dx passes in
    // during `dx serve --fullstack` (or localhost:8080 standalone).
    let bind: SocketAddr = match std::env::var("BINKFLIX_BIND") {
        Ok(v) => v.parse()?,
        Err(_) => dioxus::cli_config::fullstack_address_or_localhost(),
    };

    if library_paths.is_empty() {
        anyhow::bail!("BINKFLIX_LIBRARY is empty; set at least one path");
    }
    for p in &library_paths {
        if !p.exists() {
            anyhow::bail!("library path does not exist: {}", p.display());
        }
    }

    let pool = db::connect(&db_path).await?;

    // Register each path as its own row in `libraries` and kick off scans in
    // the background so startup isn't blocked on large trees.
    let mut scan_jobs: Vec<(i64, PathBuf)> = Vec::with_capacity(library_paths.len());
    for path in &library_paths {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("library")
            .to_string();
        let id = scanner::ensure_library(&pool, &name, path).await?;
        scan_jobs.push((id, path.clone()));
    }

    // Drop any library rows no longer in BINKFLIX_LIBRARY — changing the
    // env var shouldn't leave orphan shows/media in the DB. Cascading FKs
    // clean up the child rows (shows → media → subtitles/thumbnails).
    let active_ids: Vec<i64> = scan_jobs.iter().map(|(id, _)| *id).collect();
    let removed = scanner::prune_libraries(&pool, &active_ids).await?;
    if removed > 0 {
        info!(removed, "pruned libraries no longer configured");
    }

    let scan_progress: scanner::ProgressHandle = Arc::new(RwLock::new(ScanProgress::default()));
    let scan_lock = Arc::new(Mutex::new(()));
    let libraries = Arc::new(scan_jobs.clone());

    {
        let scan_pool = pool.clone();
        let progress = scan_progress.clone();
        let lock = scan_lock.clone();
        let jobs = scan_jobs;
        tokio::spawn(async move {
            let _guard = lock.lock().await;
            run_scans(&scan_pool, &jobs, progress).await;
        });
    }

    let state = AppState {
        pool,
        hub: syncplay::Hub::new(),
        scan_progress,
        scan_lock,
        libraries,
    };

    // Build the router: our routes first (they take priority), then the Dioxus
    // application mounted as the fallback so `/` and client-side routes work.
    let my_routes: Router = Router::new()
        .merge(api::router())
        .merge(syncplay::router())
        .with_state(state);

    // Vendored static files that need stable, unhashed URLs — JASSUB's worker
    // and wasm can't go through Dioxus's asset pipeline because the JS module
    // references them by literal path at runtime. Served directly from
    // `assets/jassub/` so the URLs stay same-origin (CORS-safe for Worker).
    // Vendored static files that need stable, unhashed URLs — JASSUB's worker
    // and wasm can't go through Dioxus's asset pipeline because the JS module
    // references them by literal path at runtime. Same for our own player.js:
    // the asset pipeline was stripping Content-Type, which browsers reject
    // for `<script type="module">`. ServeDir sets the right MIME.
    let router = axum::Router::new()
        .serve_dioxus_application(ServeConfig::new(), App)
        .nest_service("/jassub", ServeDir::new("assets/jassub"))
        .nest_service("/static", ServeDir::new("assets/static"))
        .merge(my_routes)
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());

    info!(
        %bind,
        libraries = ?library_paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "binkflix listening"
    );
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, router).await?;
    Ok(())
}
