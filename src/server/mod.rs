//! Server-side runtime: DB, filesystem scanner, REST + WebSocket routes.
//! Everything in this tree is `#[cfg(feature = "server")]` — never compiled for the WASM client.

pub mod api;
pub mod db;
pub mod error;
pub mod nfo;
pub mod scanner;
pub mod syncplay;

use crate::app::App;
use axum::Router;
use dioxus::prelude::{DioxusRouterExt, ServeConfig};
use sqlx::SqlitePool;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub hub: Arc<syncplay::Hub>,
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
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

    let scan_pool = pool.clone();
    tokio::spawn(async move {
        for (id, root) in scan_jobs {
            if let Err(e) = scanner::scan_library(&scan_pool, id, &root).await {
                tracing::error!(%e, root = %root.display(), "scan failed");
            }
        }
    });

    let state = AppState { pool, hub: syncplay::Hub::new() };

    // Build the router: our routes first (they take priority), then the Dioxus
    // application mounted as the fallback so `/` and client-side routes work.
    let my_routes: Router = Router::new()
        .merge(api::router())
        .merge(syncplay::router())
        .with_state(state);

    let router = axum::Router::new()
        .serve_dioxus_application(ServeConfig::new(), App)
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
