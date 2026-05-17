//! Server-side runtime: DB, filesystem scanner, REST + WebSocket routes.
//! Everything in this tree is `#[cfg(feature = "server")]` — never compiled for the WASM client.

pub mod analytics;
pub mod api;
pub mod auth;
pub mod cli;
pub mod db;
pub mod error;
pub mod filename;
pub mod hls;
pub mod media_info;
pub mod nfo;
pub mod preferences;
pub mod remux;
pub mod scanner;
pub mod subtitles;
pub mod syncplay;
pub mod thumbnails;
pub mod tmp;
pub mod trickplay;
pub mod user_settings;
pub mod users;
pub mod watch;

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
use tower_http::set_header::SetResponseHeaderLayer;
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
    pub hls_producers: Arc<hls::ProducerRegistry>,
    /// H.264 hardware encoder picked at startup. The producer reads this
    /// (combined with the process-wide sticky-fallback flag inside
    /// `producer.rs`) when building each ffmpeg invocation.
    pub hwenc: hls::HwEncoder,
    /// `None` disables bastion auth entirely — set `BASTION_ORIGIN` to enable.
    pub auth: Option<auth::AuthState>,
}

/// Permissive CSP that explicitly allows the resources this app uses.
///
/// `media-src 'self' blob:` is load-bearing — hls.js attaches MSE
/// by setting `video.src = URL.createObjectURL(mediaSource)`, which is
/// a `blob:` URL. Without `blob:` listed, Firefox blocks the attach
/// (visible as `Content at .../play may not load data from blob:...`)
/// and playback never starts even though segments arrive. Multiple
/// CSP headers combine restrictively, so a strict downstream proxy
/// will need this same relaxation.
///
/// `script-src 'unsafe-inline'`: Dioxus 0.7 fullstack injects two inline
/// `<script>` blocks into the served HTML (the wasm-bindgen bootloader
/// and the hydration kickoff). They have no nonce/hash hook, so the
/// pragmatic options are `'unsafe-inline'` or patching Dioxus. Modern
/// browsers ignore `'unsafe-inline'` when a hash/nonce is present, so
/// adding hashes later would tighten this without another CSP edit.
///
/// `script-src 'unsafe-eval'`: Dioxus' `document::eval()` (used in the
/// theme switcher, search focus, and outside-click bridge) compiles
/// each JS snippet through `new Function(...)`, which CSP treats as
/// eval. `'wasm-unsafe-eval'` only covers wasm compilation, not the
/// Function constructor. Without this, those `eval()` calls throw
/// silently and cascade into a `DOMException` from the wasm-bindgen
/// glue once it tries to use the half-initialised callback.
///
/// Fonts (Open Sans, Chivo) are vendored under `assets/static/fonts/` and
/// referenced from `style.css` with absolute `/static/fonts/...` URLs, so no
/// Google Fonts origins appear here.
const CSP_POLICY: &str = "default-src 'self'; \
    script-src 'self' 'unsafe-inline' 'unsafe-eval' 'wasm-unsafe-eval' https://cdn.jsdelivr.net; \
    style-src 'self' 'unsafe-inline'; \
    img-src 'self' data: blob: https:; \
    media-src 'self' blob:; \
    worker-src 'self' blob:; \
    connect-src 'self' ws: wss: https://cdn.jsdelivr.net; \
    font-src 'self' data:";

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
            active: Vec::new(),
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
    p.active.clear();
    p.message = err;
    p.last_finished_at = Some(now);
    p.last_summary = Some(summary);
    p.last_elapsed_ms = Some(started.elapsed().as_millis() as u64);
}

pub fn run() {
    let _ = dotenvy::dotenv();

    let _log_guard = init_logging();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    if let Err(e) = rt.block_on(run_async()) {
        tracing::error!(%e, "server exited with error");
        std::process::exit(1);
    }
}

fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::Layer;

    let filter = || {
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info,binkflix=debug"))
    };

    let stdout_layer = tracing_subscriber::fmt::layer().with_filter(filter());

    let log_dir = PathBuf::from(env_or("BINKFLIX_LOG_DIR", "./data/logs"));
    let file_setup = std::fs::create_dir_all(&log_dir).and_then(|_| {
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let path = log_dir.join(format!("binkflix-{ts}.log"));
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map(|f| (path, f))
    });

    match file_setup {
        Ok((path, file)) => {
            let (writer, guard) = tracing_appender::non_blocking(file);
            let file_layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer)
                .with_filter(filter());

            tracing_subscriber::registry()
                .with(stdout_layer)
                .with(file_layer)
                .init();

            tracing::info!(path = %path.display(), "file logging enabled");
            Some(guard)
        }
        Err(e) => {
            tracing_subscriber::registry().with(stdout_layer).init();
            tracing::warn!(
                %e,
                dir = %log_dir.display(),
                "could not open log file; continuing with stdout only",
            );
            None
        }
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

    // If a previous run was killed abruptly (preview_stop, OOM, kill -9
    // on the parent), any ffmpeg children we'd spawned can survive as
    // orphans — possibly still SIGSTOP'd from backpressure, holding
    // file descriptors open and burning a process slot. Sweep them
    // before we start fresh so they don't compete with the new
    // producers for the same plan dirs.
    hls::sweep_orphan_ffmpegs().await;

    // Same idea for the unified scratch dir: ensure it exists and clear
    // any leftovers from a prior crashed run.
    tmp::init_and_sweep().await;

    // Probe ffmpeg once for hw H.264 encoders; pinned for the process.
    let hwenc = hls::detect_hwenc().await;

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

    // Soft-delete library rows no longer in BINKFLIX_LIBRARY. Watch history
    // and other related rows are preserved; if a library path comes back
    // (e.g. a misconfiguration is corrected), `ensure_library` resurrects
    // the rows. `binkflix cleanup --apply` purges them for good.
    let active_ids: Vec<i64> = scan_jobs.iter().map(|(id, _)| *id).collect();
    let removed = scanner::prune_libraries(&pool, &active_ids).await?;
    if removed > 0 {
        info!(removed, "soft-deleted libraries no longer configured");
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

    let auth_state = auth::AuthConfig::from_env().map(auth::AuthState::new);
    match &auth_state {
        Some(a) => info!(cfg = ?a.cfg, "bastion auth enabled"),
        None => info!("bastion auth disabled (set BASTION_ORIGIN to enable); using dev identity"),
    }

    let state = AppState {
        pool,
        hub: syncplay::Hub::new(),
        scan_progress,
        scan_lock,
        libraries,
        hls_producers: hls::ProducerRegistry::new(),
        hwenc,
        auth: auth_state.clone(),
    };

    // Build the router: our routes first (they take priority), then the Dioxus
    // application mounted as the fallback so `/` and client-side routes work.
    let mut my_routes: Router<AppState> =
        Router::new().merge(api::router()).merge(syncplay::router()).merge(hls::router());
    if auth_state.is_some() {
        my_routes = my_routes.merge(auth::router());
    }
    let my_routes = my_routes.with_state(state.clone());

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
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_session,
        ))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::CONTENT_SECURITY_POLICY,
            axum::http::HeaderValue::from_static(CSP_POLICY),
        ))
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
