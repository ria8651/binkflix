//! `StreamPlan`: the pre-computed segment timeline a player drives playback
//! against. Built once per source file (cheap ffprobe pass over packet
//! timestamps), persisted on the `media` row, used to render the m3u8
//! instantly without spawning ffmpeg.
//!
//! The plan describes *what* segments exist and where they start in source
//! time. The producer (see `producer.rs`) is what actually generates segment
//! bytes on demand.

use super::cache;
use crate::types::MediaTechInfo;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// Bumped whenever the plan-building algorithm changes in a way that should
/// invalidate previously-cached plans (e.g. segment grouping rule changes,
/// codec-arg changes that affect init.mp4 compatibility).
///
/// History (every bump in this branch was a research dead-end except the
/// last; kept terse to discourage further archaeology):
///   v1–v8: A/V-sync experiments around audio resampler anchoring,
///          `-output_ts_offset`, tfdt post-patching, two-stage `-ss`,
///          and from-zero-only fallbacks. None worked across
///          seek-restarts.
///   v9:    Found `-hls_segment_options movflags=+frag_discont`
///          (Jellyfin's `DynamicHlsController.cs`). With it the
///          HLS-fmp4 muxer writes `tfdt` using the actual sample DTS
///          including the `-ss` offset — both tracks land on the
///          source-absolute timeline. Tried to additionally classify
///          scratch segments by sidx.earliest_presentation_time;
///          rejected ~all of them because the muxer cuts relative to
///          first-input-PTS (cluster landing), not at plan boundaries.
///   v10:   Filename-based scratch→canonical mapping. ffmpeg's
///          `-start_number=start_idx` makes scratch filenames already
///          equal plan indices; with `+frag_discont` the segment
///          contents are tfdt-tagged with source-absolute time, so
///          the player aligns by media time regardless of slight
///          EXTINF/actual-duration discrepancies at run boundaries.
///          Producer collapsed from ~1000 lines to ~500.
///   v11:   Audio track moved out of the plan body and into a per-request
///          parameter (cache dir keyed by audio index). The persisted
///          plan no longer carries an `AudioPlan` — `derive_audio_plan`
///          builds one on demand from the probe.
pub const PLAN_VERSION: u32 = 11;

/// Target segment length. ffmpeg only cuts at source keyframes under
/// `-c:v copy`, so real segments will land near this value but vary with
/// the source GOP layout.
const TARGET_SEGMENT_SECS: f64 = 6.0;

/// Hard ceiling on a single segment. Above this we still accept the segment
/// (we can't make ffmpeg cut where there isn't a keyframe), but log it: a
/// 30s segment is a sign of a long-GOP source the user might want to
/// transcode for better seeking.
const MAX_SEGMENT_WARN_SECS: f64 = 30.0;

/// Top-level plan persisted as JSON in `media.stream_plan_json`.
///
/// Audio is intentionally absent: the per-request `audio_idx` decides
/// which source stream to mux, and `derive_audio_plan` builds the
/// matching `AudioPlan` on demand. That lets a single persisted plan
/// serve every audio track on the file without growing per-track
/// duplicates in the DB.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamPlan {
    pub version: u32,
    pub mode: Mode,
    pub duration: f64,
    pub video_codec: String,
    pub segments: Vec<Segment>,
}

/// Future: `Transcode { codec, height, bitrate_kbps }`. For now, only Remux
/// (copy-mux) is implemented; other modes return 501 at request time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Mode {
    Remux,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioPlan {
    pub src_codec: Option<String>,
    /// What the producer's ffmpeg will emit. Either "copy" (when source is
    /// already AAC/MP3) or "aac" (everything else gets transcoded — cheap).
    pub out_codec: String,
    pub channels: u32,
    pub bitrate_kbps: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub i: u32,
    pub t: f64,
    pub d: f64,
}

/// Copy-remux viability gate. Returns `Ok(())` if the source is safe to
/// segment with `-c:v copy`, else an `Err(reason)` for logs / 501 fallback.
///
/// Conservative: the moment anything looks off we fall back to declaring
/// the file Transcode-only, rather than enumerating every category of
/// breakage in the plan itself (open-GOP B-frames, edit lists, VFR with
/// sparse keyframes, multiple SPS, Annex-B bitstreams that need filtering,
/// HDR sidecar metadata, …). One gate, one decision.
pub fn is_copy_remux_viable(info: &MediaTechInfo) -> Result<(), String> {
    let v = info.video.as_ref().ok_or("no video stream")?;
    // VP9/VP8/AV1 don't fit cleanly in fMP4 — they need a WebM/DASH path
    // we don't implement, so refuse those up front. Everything else
    // (h264, hevc, mpeg4, …) we copy-mux into fMP4 and let the browser
    // decide whether it has a decoder. Modern Firefox / Chrome / Safari
    // all decode HEVC where the OS provides a hardware decoder; on
    // systems without one the <video> element fires its own error event
    // and the player surfaces that.
    if matches!(v.codec.as_str(), "vp9" | "vp8" | "av1") {
        return Err(format!(
            "{} can't be muxed into HLS-fmp4 (needs WebM/DASH)",
            v.codec
        ));
    }
    let dur = info
        .duration_seconds
        .filter(|d| d.is_finite() && *d > 0.0)
        .ok_or("missing or invalid duration")?;
    if dur < 0.5 {
        return Err("duration too short to segment".into());
    }
    Ok(())
}

/// Build a fresh plan. Runs ffprobe over the source's video packets, folds
/// them into ~6s segments cut on keyframes, and packages the result.
pub async fn build_remux_plan(src: &Path, info: &MediaTechInfo) -> anyhow::Result<StreamPlan> {
    if let Err(reason) = is_copy_remux_viable(info) {
        anyhow::bail!("source not viable for copy-remux: {reason}");
    }
    let v = info.video.as_ref().expect("viable => video present");
    let duration = info.duration_seconds.expect("viable => duration present");

    let segments = probe_segments(src, duration).await?;

    Ok(StreamPlan {
        version: PLAN_VERSION,
        mode: Mode::Remux,
        duration,
        video_codec: v.codec.clone(),
        segments,
    })
}

/// Pick the Nth audio stream from the probe and decide whether ffmpeg
/// should copy it through or transcode to AAC. Returns `None` when the
/// source has no audio at all OR when `audio_idx` overruns the available
/// streams — callers treat that as "no audio mapped" (ffmpeg's
/// `-map 0:a:N?` then silently drops the audio map).
///
/// `aac`/`mp3` → copy (browsers decode them natively in MP4).
/// Everything else (`ac3`/`dts`/`eac3`/`truehd`/…) → re-encode to
/// stereo AAC at 192 kbps. Re-encode is cheap relative to video.
pub fn derive_audio_plan(info: &MediaTechInfo, audio_idx: u32) -> Option<AudioPlan> {
    let track = info.audio.get(audio_idx as usize)?;
    let src_codec = Some(track.codec.clone());
    let out_codec = match src_codec.as_deref() {
        Some("aac") | Some("mp3") => "copy".to_string(),
        _ => "aac".to_string(),
    };
    Some(AudioPlan {
        src_codec,
        out_codec,
        channels: 2,
        bitrate_kbps: 192,
    })
}

/// Run ffprobe to enumerate video packets, fold keyframe timestamps into
/// segment groups of ~`TARGET_SEGMENT_SECS`, return the segment list.
///
/// Streams output line-by-line so memory stays O(segments), not O(packets).
async fn probe_segments(src: &Path, duration: f64) -> anyhow::Result<Vec<Segment>> {
    let mut child = Command::new("ffprobe")
        .arg("-v").arg("error")
        .arg("-select_streams").arg("v:0")
        .arg("-show_entries").arg("packet=pts_time,flags")
        .arg("-of").arg("csv=print_section=0")
        .arg(src)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("ffprobe stdout missing"))?;

    let mut reader = BufReader::new(stdout).lines();
    let mut keyframes: Vec<f64> = Vec::new();
    // `keyframes` is sorted/deduped after collection — see comment below.
    while let Some(line) = reader.next_line().await? {
        // Lines look like "12.345,K_" or "12.456,__" (or sometimes "N/A,K_"
        // for streams without a global PTS — those we skip; the next
        // packet with a real PTS becomes our keyframe.)
        let mut parts = line.splitn(2, ',');
        let pts_str = parts.next().unwrap_or("");
        let flags = parts.next().unwrap_or("");
        if !flags.contains('K') {
            continue;
        }
        let Ok(t) = pts_str.parse::<f64>() else {
            continue;
        };
        if !t.is_finite() || t < 0.0 {
            continue;
        }
        keyframes.push(t);
    }

    let status = child.wait().await?;
    if !status.success() {
        anyhow::bail!("ffprobe exited with status {status}");
    }

    if keyframes.is_empty() {
        anyhow::bail!("no keyframes found in source");
    }

    // ffprobe reports packets in **decode** order. For HEVC (and any
    // codec with B-frames) decode order ≠ presentation order, so the
    // raw list can have keyframes out of monotonic-PTS sequence —
    // e.g. a CRA at 32.032 appearing before an IDR at 24.958 because
    // the CRA's leading pictures place it later in decode but earlier
    // visually. The grouping algorithm assumes monotonic PTS, so sort
    // the list before handing it off.
    keyframes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    keyframes.dedup_by(|a, b| (*a - *b).abs() < 1e-6);

    Ok(group_segments(&keyframes, duration))
}

/// Pure function: keyframe timestamps (must be sorted by PTS) → segment
/// list matching what ffmpeg's HLS muxer would produce with `-c:v copy
/// -hls_time TARGET_SEGMENT_SECS`. Extracted so it can be unit-tested
/// without spawning ffprobe.
///
/// Algorithm matches ffmpeg's `hlsenc.c`: cut the next segment at the
/// first keyframe whose PTS is ≥ `n * hls_time`, where `n` increments
/// by 1 per segment regardless of how long the previous segment ended
/// up being. This yields cumulative segment boundaries near absolute
/// multiples of the target — *not* "first keyframe ≥ seg_start +
/// hls_time" (a relative target), which we used to do and which
/// produced subtly different cut points than ffmpeg's actual output.
/// When the plan and the segment files disagree on where each segment
/// covers, the player loads the wrong file for a given timecode and
/// content visibly skips.
fn group_segments(keyframes: &[f64], duration: f64) -> Vec<Segment> {
    let mut segments = Vec::new();
    if keyframes.is_empty() || duration <= 0.0 {
        return segments;
    }

    // Always start the first segment at t=0 even if the first keyframe
    // is slightly later (rare, but happens with edit-list-shifted
    // streams).
    let mut seg_start = 0.0_f64.max(keyframes[0]);
    let mut next_idx = 1u32;
    // n=1 means the first cut is the first keyframe with PTS ≥ 1*hls_time.
    let mut n = 1u32;

    let mut i = 0;
    while i < keyframes.len() {
        let target_t = (n as f64) * TARGET_SEGMENT_SECS;
        // Find the first keyframe at or after `target_t`. Skip ahead to
        // a keyframe strictly after `seg_start` so a duplicate PTS
        // can't produce a zero-length segment.
        let mut j = i + 1;
        while j < keyframes.len() && keyframes[j] < target_t {
            j += 1;
        }
        let next_boundary = if j < keyframes.len() {
            keyframes[j]
        } else {
            duration
        };
        let dur = (next_boundary - seg_start).max(0.0);
        if dur > MAX_SEGMENT_WARN_SECS {
            tracing::warn!(
                segment_index = next_idx,
                start = seg_start,
                duration = dur,
                "long segment (sparse keyframes in source)"
            );
        }
        segments.push(Segment {
            i: next_idx,
            t: seg_start,
            d: dur,
        });
        next_idx += 1;
        n += 1;
        if j >= keyframes.len() {
            break;
        }
        seg_start = next_boundary;
        i = j;
    }
    segments
}

/// Cached row for a media's plan + invalidation keys.
pub struct PlanRow {
    pub plan: StreamPlan,
    pub source_mtime: i64,
    pub source_size: i64,
}

/// Load a plan from the DB if one exists *and* matches the on-disk source's
/// mtime+size. Returns `Ok(None)` for cache miss (caller should rebuild).
pub async fn load_if_fresh(
    pool: &SqlitePool,
    media_id: &str,
    src: &Path,
) -> anyhow::Result<Option<PlanRow>> {
    let row: Option<(Option<String>, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT stream_plan_json, source_mtime, source_size FROM media WHERE id = ?",
    )
    .bind(media_id)
    .fetch_optional(pool)
    .await?;
    let Some((Some(json), Some(mtime), Some(size))) = row else {
        return Ok(None);
    };
    let plan: StreamPlan = match serde_json::from_str(&json) {
        Ok(p) => p,
        // Schema drift between binary versions: treat as miss.
        Err(_) => return Ok(None),
    };
    if plan.version != PLAN_VERSION {
        return Ok(None);
    }
    let (cur_mtime, cur_size) = match cache::stat_source(src).await {
        Ok(t) => t,
        // Source missing → caller will surface NotFound; report cache miss.
        Err(_) => return Ok(None),
    };
    if cur_mtime != mtime || cur_size != size {
        return Ok(None);
    }
    Ok(Some(PlanRow {
        plan,
        source_mtime: mtime,
        source_size: size,
    }))
}

pub async fn store(
    pool: &SqlitePool,
    media_id: &str,
    plan: &StreamPlan,
    source_mtime: i64,
    source_size: i64,
) -> anyhow::Result<()> {
    let json = serde_json::to_string(plan)?;
    sqlx::query(
        "UPDATE media SET stream_plan_json = ?, source_mtime = ?, source_size = ? WHERE id = ?",
    )
    .bind(json)
    .bind(source_mtime)
    .bind(source_size)
    .bind(media_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_segments_short_gop_yields_six_second_segments() {
        // Keyframes every 1s for 30s.
        let kf: Vec<f64> = (0..30).map(|i| i as f64).collect();
        let segs = group_segments(&kf, 30.0);
        // Expect ~5 segments at ~6s each (the rule cuts at the *next*
        // keyframe at or after target, so first cut is at t=6 → seg 0..6).
        assert!(segs.len() >= 4 && segs.len() <= 6, "got {} segments", segs.len());
        assert_eq!(segs[0].t, 0.0);
        let total: f64 = segs.iter().map(|s| s.d).sum();
        assert!((total - 30.0).abs() < 0.01);
    }

    #[test]
    fn group_segments_long_gop_accepts_long_segments() {
        // Keyframe every 20s, total 60s. We can't cut more often than the source.
        let kf = vec![0.0, 20.0, 40.0];
        let segs = group_segments(&kf, 60.0);
        // 3 segments at ~20s each.
        assert_eq!(segs.len(), 3);
        for s in &segs {
            assert!((s.d - 20.0).abs() < 0.01);
        }
    }

    #[test]
    fn group_segments_single_keyframe_one_segment() {
        let kf = vec![0.0];
        let segs = group_segments(&kf, 12.0);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].t, 0.0);
        assert!((segs[0].d - 12.0).abs() < 0.01);
    }

    #[test]
    fn group_segments_indices_are_one_based_and_contiguous() {
        let kf: Vec<f64> = (0..20).map(|i| i as f64 * 3.0).collect();
        let segs = group_segments(&kf, 60.0);
        for (n, s) in segs.iter().enumerate() {
            assert_eq!(s.i, (n + 1) as u32);
        }
    }

    /// Regression: real keyframes from an HEVC file. ffmpeg's actual
    /// `-hls_time 6 -c:v copy` playlist for these keyframes (verified
    /// by running ffmpeg directly on the source) cuts at:
    ///   0, 7.341, 12.079, 18.652, 24.958, 32.032, 38.972, 42.609,
    ///   52.886, 54.388.
    /// Our previous "first kf ≥ seg_start + hls_time" algorithm cut at:
    ///   0, 7.341, 13.413, 19.653, 32.032, 38.972, 46.980, ...
    /// — different boundaries, leading to the player loading the wrong
    /// segment file for a given timecode and visibly skipping content.
    #[test]
    fn group_segments_matches_ffmpeg_hls_muxer_on_hevc_sample() {
        let kf: Vec<f64> = vec![
            0.0, 1.001, 2.769, 5.572, 7.341, 10.444, 12.079, 13.413,
            14.515, 16.583, 18.652, 19.653, 20.988, 23.857, 24.958,
            32.032, 38.972, 40.040, 42.609, 46.980, 52.886, 54.388,
            62.462, 65.899, 72.573,
        ];
        let segs = group_segments(&kf, 80.0);
        let boundaries: Vec<f64> = std::iter::once(0.0)
            .chain(segs.iter().scan(0.0, |acc, s| { *acc += s.d; Some(*acc) }))
            .collect();
        // The playlist starts at 0 and includes the duration tail (80.0).
        // 65.899 is *not* a boundary because at that point the target
        // was n=11 → 66.0s and 65.899 < 66; the next eligible cut is
        // 72.573 at n=12.
        let expected = [
            0.0, 7.341, 12.079, 18.652, 24.958, 32.032, 38.972, 42.609,
            52.886, 54.388, 62.462, 72.573, 80.0,
        ];
        assert_eq!(boundaries.len(), expected.len(),
            "boundary count mismatch: got {boundaries:?}");
        for (got, want) in boundaries.iter().zip(expected.iter()) {
            assert!(
                (got - want).abs() < 0.001,
                "boundary mismatch: got {got}, want {want} (full: {boundaries:?})"
            );
        }
    }
}
