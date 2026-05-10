//! Subtitle discovery, extraction, and persistence.
//!
//! Subtitles are extracted once at scan time and cached inline in SQLite
//! (see `scan_for_media`). The API reads from the DB at request time —
//! no ffmpeg spawn, no source-drive access at play-time.
//!
//! Two sources are probed during a scan:
//!   * sidecar files next to the video (`Video.en.ass`, `Video.srt`, …)
//!   * text tracks embedded in the container, probed via `ffprobe` and
//!     extracted with `ffmpeg` (PGS/DVD bitmap subs are skipped; they need
//!     OCR).

use crate::server::media_info::EmbeddedSubtitleStream;
use crate::types::SubtitleTrack;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Extensions we recognise as text-subtitle sidecars.
const SIDECAR_EXTS: &[&str] = &["ass", "ssa", "srt", "vtt"];

/// Subtitle codecs ffmpeg can copy/transcode into a text format in one shot.
fn is_text_codec(codec: &str) -> bool {
    matches!(
        codec,
        "ass" | "ssa" | "subrip" | "srt" | "webvtt" | "mov_text" | "text"
    )
}

// ---- public: API-facing DB queries ----

/// List the subtitle tracks previously extracted for a media row.
pub async fn list_from_db(pool: &SqlitePool, media_id: &str) -> anyhow::Result<Vec<SubtitleTrack>> {
    let rows = sqlx::query_as::<_, (String, String, String, String, i64, i64)>(
        "SELECT track_id, format, language, label, is_default, is_forced
         FROM subtitles WHERE media_id = ?
         ORDER BY is_default DESC, track_id",
    )
    .bind(media_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(id, format, language, label, is_default, is_forced)| SubtitleTrack {
            id,
            format,
            language,
            label,
            default: is_default != 0,
            forced: is_forced != 0,
        })
        .collect())
}

/// Fetch a single track's content + content-type from the DB.
pub async fn get_from_db(
    pool: &SqlitePool,
    media_id: &str,
    track_id: &str,
) -> anyhow::Result<Option<(Vec<u8>, &'static str)>> {
    let row: Option<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT format, content FROM subtitles WHERE media_id = ? AND track_id = ?",
    )
    .bind(media_id)
    .bind(track_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(format, content)| {
        let ct = match format.as_str() {
            "ass" => "text/x-ssa; charset=utf-8",
            _ => "text/vtt; charset=utf-8",
        };
        (content, ct)
    }))
}

// ---- public: scan-time population ----

/// Extract every usable subtitle track for `video` (sidecars + embedded) and
/// (re)populate the `subtitles` rows for `media_id`.
///
/// Called from the scanner when a video is first indexed or when its
/// mtime/size change. Idempotent — wipes+re-inserts under a single tx.
///
/// `embedded` is the subtitle stream list from a prior `media_info::probe_full`
/// call, so we don't re-spawn ffprobe just to enumerate text tracks.
pub async fn scan_for_media(
    pool: &SqlitePool,
    media_id: &str,
    video: &Path,
    embedded: &[EmbeddedSubtitleStream],
) -> anyhow::Result<usize> {
    let mut tracks = Vec::new();

    // Sidecars: cheap, never fail the whole scan if one is unreadable.
    for (idx, path) in find_sidecars(video).into_iter().enumerate() {
        match extract_sidecar(&path).await {
            Ok(data) => tracks.push(Extracted {
                track_id: format!("file-{idx}"),
                format: data.format,
                language: sidecar_language(&path).unwrap_or_default(),
                label: path.file_name().and_then(|n| n.to_str()).unwrap_or("subtitle").to_string(),
                default: false,
                forced: false,
                content: data.content,
            }),
            Err(e) => tracing::warn!(path = %path.display(), %e, "sidecar read failed"),
        }
    }

    // Embedded: one ffmpeg invocation extracts every text track in one read
    // pass. On failure (e.g. one corrupt stream poisoning the batch) we fall
    // back to per-track invocations so the rest of the tracks still land.
    let classified = classify_embedded(embedded);
    let by_index: HashMap<u32, Vec<u8>> = match extract_all_embedded(video, &classified).await {
        Ok(map) => map,
        Err(e) => {
            tracing::warn!(
                video = %video.display(),
                %e,
                "batched subtitle extract failed; falling back to per-track"
            );
            extract_per_track_fallback(video, &classified).await
        }
    };
    for e in classified {
        if let Some(content) = by_index.get(&e.stream_index).cloned() {
            tracks.push(Extracted {
                track_id: format!("embed-{}", e.stream_index),
                format: e.target_format,
                language: e.language,
                label: e.label,
                default: e.default,
                forced: e.forced,
                content,
            });
        }
    }

    let count = tracks.len();
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM subtitles WHERE media_id = ?")
        .bind(media_id)
        .execute(&mut *tx)
        .await?;
    for t in tracks {
        sqlx::query(
            "INSERT INTO subtitles
                (media_id, track_id, format, language, label, is_default, is_forced, content)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(media_id)
        .bind(&t.track_id)
        .bind(t.format)
        .bind(&t.language)
        .bind(&t.label)
        .bind(t.default as i64)
        .bind(t.forced as i64)
        .bind(&t.content)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(count)
}

struct Extracted {
    track_id: String,
    format: &'static str,
    language: String,
    label: String,
    default: bool,
    forced: bool,
    content: Vec<u8>,
}

// ---- sidecar discovery + extraction ----

fn find_sidecars(video: &Path) -> Vec<PathBuf> {
    let Some(dir) = video.parent() else { return Vec::new() };
    let Some(stem) = video.file_stem().and_then(|s| s.to_str()) else { return Vec::new() };

    let stem_lower = stem.to_lowercase();
    let mut found = Vec::new();
    let Ok(read) = std::fs::read_dir(dir) else { return found };
    for entry in read.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else { continue };
        if !SIDECAR_EXTS.contains(&ext.to_ascii_lowercase().as_str()) {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        let name_lower = name.to_lowercase();
        if name_lower == stem_lower || name_lower.starts_with(&format!("{stem_lower}.")) {
            found.push(path);
        }
    }
    found.sort();
    found
}

fn sidecar_language(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let tail = stem.rsplit('.').next()?;
    let ok = (tail.len() == 2 || tail.len() == 3) && tail.chars().all(|c| c.is_ascii_alphabetic());
    if ok { Some(tail.to_ascii_lowercase()) } else { None }
}

struct SidecarData {
    format: &'static str,
    content: Vec<u8>,
}

async fn extract_sidecar(path: &Path) -> anyhow::Result<SidecarData> {
    let bytes = tokio::fs::read(path).await?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    Ok(match ext.as_str() {
        "ass" | "ssa" => SidecarData { format: "ass", content: bytes },
        "vtt"         => SidecarData { format: "vtt", content: bytes },
        "srt"         => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            SidecarData { format: "vtt", content: srt_to_vtt(&text).into_bytes() }
        }
        _ => SidecarData { format: "vtt", content: bytes },
    })
}

// ---- ffmpeg for embedded streams ----

struct EmbeddedMeta {
    stream_index: u32,
    codec: String,
    target_format: &'static str,
    language: String,
    label: String,
    default: bool,
    forced: bool,
}

/// Pick text-codec subtitles out of the pre-probed stream list and decide
/// each track's target format / display label.
fn classify_embedded(streams: &[EmbeddedSubtitleStream]) -> Vec<EmbeddedMeta> {
    let mut out = Vec::new();
    for s in streams {
        if !is_text_codec(&s.codec) {
            continue;
        }
        let target_format: &'static str = match s.codec.as_str() {
            "ass" | "ssa" => "ass",
            _ => "vtt",
        };
        let title = s.tags.get("title").cloned().unwrap_or_default();
        let lang = s.tags.get("language").cloned().unwrap_or_default();
        let label = if !title.is_empty() {
            title
        } else if !lang.is_empty() {
            format!("Track {} ({lang})", s.index)
        } else {
            format!("Track {}", s.index)
        };
        out.push(EmbeddedMeta {
            stream_index: s.index,
            codec: s.codec.clone(),
            target_format,
            language: lang,
            label,
            default: s.disposition.get("default").copied().unwrap_or(0) != 0,
            forced: s.disposition.get("forced").copied().unwrap_or(0) != 0,
        });
    }
    out
}

/// Extract every embedded text track in `embeds` with a single ffmpeg
/// invocation (one input read pass, N outputs to a tempdir). Returns a map
/// keyed by `stream_index` so the caller can pair each blob with its
/// `EmbeddedMeta`.
///
/// On slow storage the saving here is large: ffmpeg opens the input once,
/// runs the demuxer once, and writes all subtitle outputs from the same
/// packet stream — versus N opens + N demuxer inits with the per-track path.
async fn extract_all_embedded(
    video: &Path,
    embeds: &[EmbeddedMeta],
) -> anyhow::Result<HashMap<u32, Vec<u8>>> {
    if embeds.is_empty() {
        return Ok(HashMap::new());
    }

    let dir = tempfile::Builder::new().prefix("binkflix-subs-").tempdir()?;
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error", "-nostdin", "-y"])
        // See `extract_per_track_fallback` for why these caps matter.
        .args(["-analyzeduration", "1000000"])
        .args(["-probesize", "1000000"])
        .args(["-fflags", "+nobuffer"])
        .arg("-i")
        .arg(video);

    let mut outputs: Vec<(u32, PathBuf)> = Vec::with_capacity(embeds.len());
    for e in embeds {
        let copy_ok = (e.target_format == "ass" && (e.codec == "ass" || e.codec == "ssa"))
            || (e.target_format == "vtt" && e.codec == "webvtt");
        let fmt = if e.target_format == "ass" { "ass" } else { "webvtt" };
        let ext = if e.target_format == "ass" { "ass" } else { "vtt" };
        let path = dir.path().join(format!("{}.{ext}", e.stream_index));
        // Per-output options bind to the next output file in argv — so
        // mixing copy (for already-text codecs) and transcode (for
        // mov_text → webvtt etc.) within one invocation is fine.
        cmd.args(["-map", &format!("0:{}", e.stream_index)]);
        if copy_ok {
            cmd.args(["-c:s", "copy"]);
        }
        cmd.args(["-f", fmt]).arg(&path);
        outputs.push((e.stream_index, path));
    }

    let started = std::time::Instant::now();
    let out = cmd.output().await?;
    if !out.status.success() {
        anyhow::bail!(
            "ffmpeg extract failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let mut map = HashMap::with_capacity(outputs.len());
    for (idx, p) in &outputs {
        match tokio::fs::read(p).await {
            Ok(b) => {
                map.insert(*idx, b);
            }
            Err(e) => tracing::warn!(stream = idx, %e, "subtitle output missing"),
        }
    }
    tracing::debug!(
        video = %video.display(),
        tracks = map.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "extracted embedded subtitles (batched)",
    );
    Ok(map)
}

/// Per-track fallback when the batched path fails (e.g. one corrupt stream).
/// Spawns one ffmpeg per track and discards individual failures so a single
/// bad track can't take down the whole file's subtitles.
async fn extract_per_track_fallback(
    video: &Path,
    embeds: &[EmbeddedMeta],
) -> HashMap<u32, Vec<u8>> {
    let mut out = HashMap::new();
    for e in embeds {
        match extract_embedded(video, e.stream_index, &e.codec, e.target_format).await {
            Ok(content) => {
                out.insert(e.stream_index, content);
            }
            Err(err) => tracing::warn!(
                video = %video.display(),
                stream = e.stream_index,
                %err,
                "embedded subtitle extract failed (per-track fallback)"
            ),
        }
    }
    out
}

async fn extract_embedded(
    video: &Path,
    stream_index: u32,
    codec: &str,
    target_format: &str,
) -> anyhow::Result<Vec<u8>> {
    // Copy when source codec already matches target.
    let codec_args: &[&str] = if (target_format == "ass" && (codec == "ass" || codec == "ssa"))
        || (target_format == "vtt" && codec == "webvtt")
    {
        &["-c:s", "copy"]
    } else {
        &[]
    };
    let fmt_arg = if target_format == "ass" { "ass" } else { "webvtt" };

    let started = std::time::Instant::now();
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y"])
        // Cap probe scanning — defaults are 5MB/5s, which can be dozens of
        // seconds on slow random-access storage. We already know from ffprobe
        // which stream we want; libavformat just needs enough to recognise it.
        .args(["-analyzeduration", "1000000"])
        .args(["-probesize", "1000000"])
        .args(["-fflags", "+nobuffer"])
        .arg("-i")
        .arg(video)
        .args(["-map", &format!("0:{stream_index}")])
        .args(codec_args)
        .args(["-f", fmt_arg, "pipe:1"])
        .output()
        .await?;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    if !output.status.success() {
        anyhow::bail!(
            "ffmpeg extract failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    tracing::debug!(
        video = %video.display(),
        stream_index,
        format = target_format,
        elapsed_ms,
        bytes = output.stdout.len(),
        "extracted embedded subtitle",
    );

    Ok(output.stdout)
}

// ---- SRT → WebVTT (plain-text cues only) ----

fn srt_to_vtt(srt: &str) -> String {
    let mut out = String::with_capacity(srt.len() + 16);
    out.push_str("WEBVTT\n\n");
    for line in srt.lines() {
        // SRT uses ',' for millisecond separator; VTT uses '.'.
        if line.contains("-->") {
            out.push_str(&line.replace(',', "."));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}
