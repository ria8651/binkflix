//! Scrub-bar preview sprite sheets ("trickplay").
//!
//! At scan time we sample one frame every `INTERVAL_S` seconds across the
//! full duration and pack them into a single JPEG sprite sheet via
//! ffmpeg's `tile` filter. The player loads the sprite + a small JSON
//! manifest once and draws the right tile via `background-position` as
//! the user hovers the seek bar — no per-hover ffmpeg cost, no DB hit per
//! frame.

use serde::Serialize;
use sqlx::SqlitePool;
use std::path::Path;
use tokio::process::Command;

/// Seconds between sampled frames.
pub const INTERVAL_S: u32 = 10;
/// Tile width in pixels — 16:9.
pub const TILE_W: u32 = 240;
/// Tile height in pixels — 16:9.
pub const TILE_H: u32 = 135;
/// Pixels of blank space between adjacent tiles in the sprite. Without
/// this, browser sampling at the displayed size can leak a sliver of the
/// neighbouring frame into the rendered tile.
pub const TILE_PADDING: u32 = 4;
/// Don't bother for very short clips — nobody scrubs a 20-second video.
const MIN_DURATION_S: f64 = 30.0;
/// Hard cap on tile count to keep sprite size sane on multi-hour content.
/// 720 tiles = 2 hours at 10s intervals; over that we stretch the interval.
const MAX_TILES: u32 = 720;

#[derive(Debug, Serialize)]
pub struct TrickplayManifest {
    pub interval: u32,
    pub tile_w: u32,
    pub tile_h: u32,
    pub padding: u32,
    pub cols: u32,
    pub rows: u32,
    pub count: u32,
}

pub async fn get_manifest(
    pool: &SqlitePool,
    media_id: &str,
) -> anyhow::Result<Option<TrickplayManifest>> {
    let row: Option<(i64, i64, i64, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT interval_s, tile_w, tile_h, padding, cols, rows, count
         FROM media_trickplay WHERE media_id = ?",
    )
    .bind(media_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(interval, tw, th, p, c, r, n)| TrickplayManifest {
        interval: interval as u32,
        tile_w: tw as u32,
        tile_h: th as u32,
        padding: p as u32,
        cols: c as u32,
        rows: r as u32,
        count: n as u32,
    }))
}

pub async fn get_sprite(
    pool: &SqlitePool,
    media_id: &str,
) -> anyhow::Result<Option<(Vec<u8>, String)>> {
    let row: Option<(Vec<u8>, String)> =
        sqlx::query_as("SELECT content, mime FROM media_trickplay WHERE media_id = ?")
            .bind(media_id)
            .fetch_optional(pool)
            .await?;
    Ok(row)
}

/// Build (or rebuild) the sprite for `media_id`. Idempotent UPSERT.
/// Logs and swallows failures so a missing ffmpeg or weird container
/// can't fail a library scan. On success, returns the number of source
/// keyframes the ffmpeg call processed (useful for analytics correlation
/// — average GOP = duration / keyframe_count). `None` means trickplay
/// didn't run or its frame count couldn't be parsed; caller should treat
/// that as "no signal," not zero.
pub async fn scan_for_media(
    pool: &SqlitePool,
    media_id: &str,
    video: &Path,
    duration_secs: Option<f64>,
) -> Option<u32> {
    let Some(duration) = duration_secs else {
        tracing::debug!(media_id, "trickplay skipped: no duration");
        return None;
    };
    if duration < MIN_DURATION_S {
        return None;
    }

    // Tile count: cover the full duration; clamp by MAX_TILES by stretching
    // the interval rather than truncating, so a 4-hour film still gets
    // previews across the whole timeline.
    let raw_count = (duration / INTERVAL_S as f64).ceil() as u32;
    let (interval, count) = if raw_count > MAX_TILES {
        let stretched = (duration / MAX_TILES as f64).ceil() as u32;
        let n = (duration / stretched as f64).ceil() as u32;
        (stretched, n.max(1))
    } else {
        (INTERVAL_S, raw_count.max(1))
    };
    let cols = (count as f64).sqrt().ceil() as u32;
    let rows = (count + cols - 1) / cols;

    match build_sprite(video, interval, cols, rows).await {
        Ok((bytes, keyframe_count)) => {
            if let Err(e) = sqlx::query(
                "INSERT INTO media_trickplay
                    (media_id, content, mime, interval_s, tile_w, tile_h, padding, cols, rows, count)
                 VALUES (?, ?, 'image/jpeg', ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(media_id) DO UPDATE SET
                    content    = excluded.content,
                    mime       = excluded.mime,
                    interval_s = excluded.interval_s,
                    tile_w     = excluded.tile_w,
                    tile_h     = excluded.tile_h,
                    padding    = excluded.padding,
                    cols       = excluded.cols,
                    rows       = excluded.rows,
                    count      = excluded.count,
                    created_at = datetime('now')",
            )
            .bind(media_id)
            .bind(&bytes)
            .bind(interval as i64)
            .bind(TILE_W as i64)
            .bind(TILE_H as i64)
            .bind(TILE_PADDING as i64)
            .bind(cols as i64)
            .bind(rows as i64)
            .bind(count as i64)
            .execute(pool)
            .await
            {
                tracing::warn!(media_id, %e, "failed to persist trickplay sprite");
                None
            } else {
                tracing::debug!(
                    media_id,
                    interval,
                    cols,
                    rows,
                    count,
                    bytes = bytes.len(),
                    keyframe_count,
                    "built trickplay"
                );
                keyframe_count
            }
        }
        Err(e) => {
            tracing::debug!(media_id, video = %video.display(), %e, "trickplay extract failed");
            None
        }
    }
}

async fn build_sprite(
    video: &Path,
    interval: u32,
    cols: u32,
    rows: u32,
) -> anyhow::Result<(Vec<u8>, Option<u32>)> {
    let started = std::time::Instant::now();
    // `-skip_frame nokey` makes the decoder discard P/B frames, so we only
    // pay the cost of decoding I-frames — typically a 5–50× speedup
    // depending on GOP size. The `fps=1/interval` filter then picks the
    // keyframe nearest each interval slot, so a tile labelled e.g. "1m20s"
    // may actually show the keyframe at 1m18s or 1m22s. Invisible at
    // hover-preview fidelity; the data model (fixed interval → tile index)
    // is preserved.
    //
    // `showinfo=checksum=0` is inserted *before* `fps` so it sees every
    // input (keyframe-only) frame the decoder emits, not the
    // post-decimation output. Each frame produces one stderr line of the
    // form `[Parsed_showinfo_0 @ 0x…] n:N pts:…`; we count those after the
    // run to populate `scan_timings.keyframe_count`.
    //
    // `checksum=0` is **load-bearing**: by default showinfo hashes every
    // frame's pixel data (checksum, plane_checksum, mean, stdev), which
    // for intra-only VP9 (~50 keyframes/sec at 1080p) bottlenecks the
    // whole pipeline on memory bandwidth and is much slower than the
    // decode itself. With it off we just pay for a stringified frame
    // index per keyframe — negligible.
    //
    // We scale to exact tile dims (no pad/letterbox dance) — `tile`
    // requires uniform input sizes, and pad+scale together stumble on
    // pixel-format rounding for many sources ("Padded dimensions cannot be
    // smaller than input"). For 16:9 sources (the overwhelming majority)
    // 240×135 is exact; on odd aspects the tile is mildly squished, which
    // is fine for hover previews. `tile` then packs frames into the
    // `cols x rows` grid.
    let vf = format!(
        "showinfo=checksum=0,fps=1/{interval},scale={tw}:{th},setsar=1,tile={cols}x{rows}:padding={pad}:color=black",
        interval = interval,
        tw = TILE_W,
        th = TILE_H,
        cols = cols,
        rows = rows,
        pad = TILE_PADDING,
    );
    // `-v info` is needed for `showinfo`'s per-frame log lines; on success
    // we discard stderr (after counting), on failure the extra context is
    // useful for diagnosing.
    let output = Command::new("ffmpeg")
        .args(["-v", "info", "-nostdin", "-y"])
        .args(["-analyzeduration", "1000000"])
        .args(["-probesize", "1000000"])
        .args(["-skip_frame", "nokey"])
        .arg("-i")
        .arg(video)
        .args(["-vf", &vf])
        .args(["-frames:v", "1"])
        .args(["-f", "image2", "-c:v", "mjpeg", "-q:v", "5"])
        .arg("pipe:1")
        .output()
        .await?;

    if !output.status.success() {
        anyhow::bail!(
            "ffmpeg trickplay failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let keyframe_count = parse_showinfo_count(&output.stderr);
    tracing::debug!(
        video = %video.display(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        bytes = output.stdout.len(),
        keyframe_count,
        "extracted trickplay sprite"
    );
    Ok((output.stdout, keyframe_count))
}

/// Count `Parsed_showinfo_0` lines in ffmpeg's stderr — one per source
/// frame the decoder fed into the filter graph. With `-skip_frame nokey`
/// upstream, that equals the number of keyframes in the file. Returns
/// `None` if the marker is absent (older/altered ffmpeg log format) so
/// the caller can distinguish "no signal" from "zero keyframes."
fn parse_showinfo_count(stderr: &[u8]) -> Option<u32> {
    let s = std::str::from_utf8(stderr).ok()?;
    let n = s.matches("Parsed_showinfo_0").count();
    if n == 0 { None } else { Some(n as u32) }
}
