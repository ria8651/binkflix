//! Generated-thumbnail extraction + DB caching.
//!
//! If a media row has no sidecar image, we grab a single frame from the
//! video at scan time and cache it as JPEG in `media_thumbnails`. The
//! API's image endpoint prefers the sidecar path and falls back to this
//! cache — so after the initial scan we never hit the source drive for
//! grid/thumbnail images.

use sqlx::SqlitePool;
use std::path::Path;
use tokio::process::Command;

/// Best-effort seek offset for the grab. A minute in is usually past the
/// intro/logo/black frames; ffmpeg just gives us what it's got if the
/// video is shorter.
const SEEK_SECONDS: u32 = 60;

/// Target width in pixels; aspect ratio preserved by ffmpeg's `-1`.
const THUMB_WIDTH: u32 = 480;

/// Fetch the cached thumbnail blob for a media id, if any.
pub async fn get_from_db(
    pool: &SqlitePool,
    media_id: &str,
) -> anyhow::Result<Option<(Vec<u8>, String)>> {
    let row: Option<(Vec<u8>, String)> = sqlx::query_as(
        "SELECT content, mime FROM media_thumbnails WHERE media_id = ?",
    )
    .bind(media_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Generate (or regenerate) the cached thumbnail for `media_id` from `video`.
/// Idempotent — UPSERT. Logs and swallows failures so a missing ffmpeg or
/// a weird container can't fail a library scan.
pub async fn scan_for_media(pool: &SqlitePool, media_id: &str, video: &Path) {
    match extract_frame(video).await {
        Ok(bytes) => {
            if let Err(e) = sqlx::query(
                "INSERT INTO media_thumbnails (media_id, content, mime)
                 VALUES (?, ?, 'image/jpeg')
                 ON CONFLICT(media_id) DO UPDATE SET
                    content = excluded.content,
                    mime = excluded.mime,
                    created_at = datetime('now')",
            )
            .bind(media_id)
            .bind(&bytes)
            .execute(pool)
            .await
            {
                tracing::warn!(media_id, %e, "failed to persist thumbnail");
            }
        }
        Err(e) => {
            tracing::debug!(media_id, video = %video.display(), %e, "thumbnail extract failed");
        }
    }
}

async fn extract_frame(video: &Path) -> anyhow::Result<Vec<u8>> {
    let started = std::time::Instant::now();
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y"])
        // `-ss` before `-i` is fast (imprecise) seek — we don't need
        // frame accuracy for a grid thumb.
        .args(["-ss", &SEEK_SECONDS.to_string()])
        // Probe caps match the subtitle extractor: source drives can be slow.
        .args(["-analyzeduration", "1000000"])
        .args(["-probesize", "1000000"])
        .args(["-fflags", "+nobuffer"])
        .arg("-i")
        .arg(video)
        .args(["-frames:v", "1"])
        .args(["-vf", &format!("scale={THUMB_WIDTH}:-1")])
        .args(["-f", "image2", "-c:v", "mjpeg", "-q:v", "4"])
        .arg("pipe:1")
        .output()
        .await?;

    if !output.status.success() {
        anyhow::bail!(
            "ffmpeg thumbnail failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    tracing::debug!(
        video = %video.display(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        bytes = output.stdout.len(),
        "extracted thumbnail"
    );
    Ok(output.stdout)
}
