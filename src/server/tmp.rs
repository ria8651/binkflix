//! Project-local scratch space.
//!
//! All on-disk temp files go under `BINKFLIX_TMP` (default `./data/tmp`)
//! so `/tmp` isn't polluted and crash leftovers stay alongside the rest
//! of the dev data dir. Sites that need a same-filesystem invariant
//! (e.g. HLS producer scratch, which hard-links into the canonical
//! plan_dir) build their `TempDir` directly with `tempdir_in(parent)`
//! against their own anchor instead of using this helper.

use std::path::PathBuf;

pub fn tmp_root() -> PathBuf {
    std::env::var("BINKFLIX_TMP")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./data/tmp"))
}

/// Create the tmp root if missing and best-effort sweep stale entries
/// from prior runs. Called once at server startup.
pub async fn init_and_sweep() {
    let root = tmp_root();
    if let Err(e) = tokio::fs::create_dir_all(&root).await {
        tracing::warn!(path = %root.display(), error = %e, "failed to create tmp root");
        return;
    }
    let Ok(mut rd) = tokio::fs::read_dir(&root).await else {
        return;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let p = entry.path();
        if let Err(e) = tokio::fs::remove_dir_all(&p).await {
            tracing::warn!(path = %p.display(), error = %e, "failed to sweep tmp entry");
        }
    }
}

/// Build a `tempfile::TempDir` rooted under `tmp_root()`. Drop cleans up
/// automatically.
pub fn tempdir(prefix: &str) -> std::io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(tmp_root())
}
