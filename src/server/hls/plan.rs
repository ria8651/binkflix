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
///   v12:   `Mode::Transcode { bitrate_kbps, max_height }` added. Mode is
///          a per-request param (cache dir keyed by `mode_tag`), so the
///          persisted plan still always describes the remux variant —
///          transcode plans use uniform 6s segments and are built on the
///          fly without DB caching.
///   v13:   Rate control moved to quality-targeted modes where the encoder
///          has one: capped CRF on libx264, `-q:v` on VideoToolbox under
///          Auto (its quality mode is silently voided by `-maxrate`, so an
///          explicit pick still gets ABR — see `Mode::Transcode::hard_cap`).
///          VAAPI/QSV keep ABR. `max_height` is now resolved by the caller
///          too, so Auto derives it from the source resolution instead of
///          from the bitrate. All of these change segment bytes under tags
///          that would otherwise collide with v12's.
///   v14:   HEVC remux tagged `hvc1` instead of ffmpeg's default `hev1` —
///          Apple's HLS stack (AVPlayer, Safari) only accepts `hvc1` in
///          fMP4. Changes init.mp4 bytes for every HEVC remux.
///   v15:   Audio demuxed out of the video segments into its own renditions
///          (see `AudioPlan`), served behind a master playlist. Video
///          segments are video-only and shared across audio tracks; the
///          plan carries the variant's `CODECS` string.
pub const PLAN_VERSION: u32 = 15;

/// Target segment length. ffmpeg only cuts at source keyframes under
/// `-c:v copy`, so real segments will land near this value but vary with
/// the source GOP layout.
const TARGET_SEGMENT_SECS: f64 = 6.0;

/// Hard ceiling on a single segment. Above this we still accept the segment
/// (we can't make ffmpeg cut where there isn't a keyframe), but log it: a
/// 30s segment is a sign of a long-GOP source the user might want to
/// transcode for better seeking.
const MAX_SEGMENT_WARN_SECS: f64 = 30.0;

/// Top-level plan for the video rendition, persisted as JSON in
/// `media_files.stream_plan_json` (remux only; transcode plans are cheap
/// to rebuild).
///
/// Audio is intentionally absent: each audio track is its own rendition
/// with its own timeline (see `AudioPlan`), so one persisted plan serves
/// every audio track on the file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamPlan {
    pub version: u32,
    pub mode: Mode,
    pub duration: f64,
    pub video_codec: String,
    /// RFC 6381 codec string of the output video (`avc1.640029`,
    /// `hvc1.2.4.L120.B0`) for the master playlist's `CODECS`, which is how
    /// players pick between the stereo and surround variants. `None` when
    /// the source's parameters couldn't be read.
    #[serde(default)]
    pub codecs: Option<String>,
    /// `PQ` / `HLG` for HDR sources, `None` for SDR. Without it AVPlayer
    /// treats an HDR stream as SDR.
    #[serde(default)]
    pub video_range: Option<String>,
    pub segments: Vec<Segment>,
}

/// Two encode strategies share the same producer/playlist/cache machinery:
///
/// * `Remux` — `-c:v copy` into fMP4. Cheap, but only viable when the
///   source video codec is one fMP4 + browser stack can decode (h264 today,
///   plus best-effort hevc on systems with hardware decoders). Segments
///   are cut at source keyframes, so timing is determined by an ffprobe
///   pass over the source's video packets.
/// * `Transcode` — `-c:v libx264` at `bitrate_kbps` with the source
///   downscaled to at most `max_height`. Real CPU cost, but always plays.
///   Segments are uniform 6s blocks (no source-keyframe constraint —
///   ffmpeg is told to force keyframes at boundaries), so the plan can
///   be built from duration alone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Mode {
    Remux,
    Transcode {
        bitrate_kbps: u32,
        max_height: u32,
        /// `true` when `bitrate_kbps` came from an explicit menu pick — the
        /// user asking for a hard ceiling, usually to fit a slow link. That
        /// promise outranks picture quality, so encoders whose quality mode
        /// can't be capped must fall back to rate control to honour it.
        ///
        /// `false` under Auto, where the number is only a budget. An encoder
        /// with a real quality mode is then free to ignore it and target
        /// quality instead (see `apply_video_args`).
        hard_cap: bool,
    },
}

/// The two renditions an audio track can be served as. Each lives in its
/// own `EXT-X-MEDIA` group referenced by its own variant, so a player
/// picks by what it can decode (`CODECS`) rather than us guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioFlavor {
    /// Stereo AAC. Decodes everywhere, so every track gets one.
    Stereo,
    /// The source's channels, passed through where Apple can play the
    /// codec and otherwise E-AC-3 5.1. Only for tracks with more than two
    /// channels.
    Surround,
}

impl AudioFlavor {
    pub const ALL: [AudioFlavor; 2] = [AudioFlavor::Stereo, AudioFlavor::Surround];

    pub fn as_str(self) -> &'static str {
        match self {
            AudioFlavor::Stereo => "stereo",
            AudioFlavor::Surround => "surround",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == s)
    }
}

/// One audio rendition: which source track, how ffmpeg turns it into the
/// output, and its timeline. Derived per request from the probe (cheap),
/// never persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioPlan {
    pub idx: u32,
    pub flavor: AudioFlavor,
    pub src_codec: String,
    /// What the producer's ffmpeg will emit: `copy`, `aac` or `eac3`.
    pub out_codec: String,
    pub channels: u32,
    /// Output bitrate, or the source's for `copy` (an estimate when the
    /// probe has none). Only feeds the master playlist's `BANDWIDTH`.
    pub bitrate_kbps: u32,
    /// RFC 6381 codec string for `CODECS`.
    pub codecs: String,
    /// `CHANNELS` attribute: the channel count, or `16/JOC` for Atmos.
    pub channels_attr: String,
    /// Uniform 6s grid over the whole file. Audio cuts anywhere, so unlike
    /// a remux video plan this needs no probe pass.
    pub segments: Vec<Segment>,
}

impl AudioPlan {
    /// Cache and producer key, e.g. `a0-stereo`.
    pub fn tag(&self) -> String {
        format!("a{}-{}", self.idx, self.flavor.as_str())
    }
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

/// Map a target video bitrate to the maximum output height. The ladder is
/// conservative — picking a height that the bitrate can sustain at a
/// reasonable per-pixel rate avoids the "blocky 1080p at 1 Mbps" failure
/// mode. Source resolution is the upper bound; we never upscale.
///
/// This is for *explicit* quality picks only. The menu labels a preset
/// "2 Mbps · ~480p", so choosing it has to actually downscale — dropping a
/// tier on a slow connection is the whole reason that menu exists. Auto
/// goes the other way round; see `bitrate_for_height`.
pub fn height_for_bitrate(bitrate_kbps: u32) -> u32 {
    match bitrate_kbps {
        b if b >= 6000 => 1080,
        b if b >= 3000 => 720,
        b if b >= 1500 => 480,
        _ => 360,
    }
}

/// Bitrate ceiling for an output height — the inverse ladder, used by Auto.
///
/// The direction matters. A source's bitrate is a property of its *codec's
/// efficiency*, not of its resolution, so deriving height from bitrate
/// punishes well-encoded files: a 1080p HEVC at 2 Mbps used to land on the
/// 480p rung purely for being efficiently encoded. Auto now takes the height
/// from the source and lets the budget follow from the height.
///
/// These are ceilings, not targets. The libx264 path runs capped CRF (see
/// `apply_video_args`) and typically settles well under them.
pub fn bitrate_for_height(height: u32) -> u32 {
    match height {
        h if h > 720 => 8000,
        h if h > 480 => 4000,
        h if h > 360 => 2000,
        _ => 1000,
    }
}

/// Build a transcode plan: uniform 6s segments derived from duration. No
/// keyframe ffprobe pass — the producer's ffmpeg forces keyframes at
/// segment boundaries via `-force_key_frames`.
pub fn build_transcode_plan(
    info: &MediaTechInfo,
    bitrate_kbps: u32,
    max_height: u32,
    hard_cap: bool,
) -> anyhow::Result<StreamPlan> {
    let v = info.video.as_ref().ok_or_else(|| anyhow::anyhow!("no video stream"))?;
    let duration = info
        .duration_seconds
        .filter(|d| d.is_finite() && *d > 0.0)
        .ok_or_else(|| anyhow::anyhow!("missing or invalid duration"))?;
    if duration < 0.5 {
        anyhow::bail!("duration too short to segment");
    }
    let segments = uniform_segments(duration, TARGET_SEGMENT_SECS);
    Ok(StreamPlan {
        version: PLAN_VERSION,
        mode: Mode::Transcode {
            bitrate_kbps,
            max_height,
            hard_cap,
        },
        duration,
        video_codec: v.codec.clone(),
        // Every backend is pinned to High@4.1 (see `apply_video_args`).
        codecs: Some(TRANSCODE_CODECS.into()),
        // Encoded as 8-bit SDR whatever the source was.
        video_range: None,
        segments,
    })
}

/// `CODECS` for transcoded video: H.264 High, level 4.1.
const TRANSCODE_CODECS: &str = "avc1.640029";

/// A uniform grid's last segment shorter than this is folded into the one
/// before. Durations are approximate (the container's is its longest
/// stream's; frame sizes overhang), and a sliver segment the stream doesn't
/// actually reach is one ffmpeg never writes — a request for it would wait
/// out the full timeout. Losing up to this much at the very end is the
/// cheaper failure.
const MIN_TAIL_SECS: f64 = 1.0;

fn uniform_segments(duration: f64, target_secs: f64) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut t = 0.0_f64;
    let mut idx = 1u32;
    while t + target_secs < duration {
        out.push(Segment { i: idx, t, d: target_secs });
        idx += 1;
        t += target_secs;
    }
    let tail = (duration - t).max(0.0);
    match out.last_mut() {
        Some(last) if tail < MIN_TAIL_SECS => last.d += tail,
        _ if tail > 0.0 => out.push(Segment { i: idx, t, d: tail }),
        _ => {}
    }
    out
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
    let (codecs, video_range) = probe_video_params(src, &v.codec).await;

    Ok(StreamPlan {
        version: PLAN_VERSION,
        mode: Mode::Remux,
        duration,
        video_codec: v.codec.clone(),
        codecs,
        video_range,
        segments,
    })
}

/// Read the source video's `CODECS` string and `VIDEO-RANGE`. The codec
/// string comes from the decoder configuration record (`avcC`/`hvcC`),
/// which `-c:v copy` carries into init.mp4 unchanged, so it's exact. Sources
/// that store Annex-B parameter sets instead (MPEG-TS) fall back to
/// ffprobe's profile and level. Best-effort: a failed probe only costs the
/// master playlist its `CODECS`.
async fn probe_video_params(src: &Path, codec: &str) -> (Option<String>, Option<String>) {
    #[derive(Deserialize)]
    struct Out {
        #[serde(default)]
        streams: Vec<Stream>,
    }
    #[derive(Deserialize)]
    struct Stream {
        profile: Option<String>,
        level: Option<i64>,
        color_transfer: Option<String>,
        extradata: Option<String>,
    }
    let output = Command::new("ffprobe")
        .arg("-v").arg("error")
        .arg("-protocol_whitelist").arg("file")
        .arg("-select_streams").arg("v:0")
        .arg("-show_entries").arg("stream=profile,level,color_transfer,extradata")
        .arg("-show_data")
        .arg("-of").arg("json")
        .arg(src)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await;
    let Some(s) = output
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| serde_json::from_slice::<Out>(&o.stdout).ok())
        .and_then(|o| o.streams.into_iter().next())
    else {
        return (None, None);
    };
    let record = s.extradata.as_deref().map(parse_hexdump).unwrap_or_default();
    let codecs = codecs_from_record(codec, &record)
        .or_else(|| codecs_from_profile(codec, s.profile.as_deref(), s.level));
    let range = match s.color_transfer.as_deref() {
        Some("smpte2084") => Some("PQ".to_string()),
        Some("arib-std-b67") => Some("HLG".to_string()),
        _ => None,
    };
    (codecs, range)
}

/// Decode ffprobe's `-show_data` hexdump
/// (`00000000: 0164 000d ffe1 ...  .d......`) back into bytes.
fn parse_hexdump(dump: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for line in dump.lines() {
        let Some((_, rest)) = line.split_once(": ") else { continue };
        // The ASCII column starts after a run of two spaces.
        let hex = rest.split("  ").next().unwrap_or_default();
        let digits: Vec<u8> = hex.bytes().filter(|b| b.is_ascii_hexdigit()).collect();
        for pair in digits.chunks_exact(2) {
            if let Ok(b) = u8::from_str_radix(std::str::from_utf8(pair).unwrap_or_default(), 16) {
                out.push(b);
            }
        }
    }
    out
}

/// RFC 6381 string from an `avcC` / `hvcC` decoder configuration record
/// (ISO/IEC 14496-15). `None` for anything else, including Annex-B
/// extradata, which starts with a start code rather than version 1.
fn codecs_from_record(codec: &str, r: &[u8]) -> Option<String> {
    match codec {
        "h264" if r.len() >= 4 && r[0] == 1 => {
            Some(format!("avc1.{:02x}{:02x}{:02x}", r[1], r[2], r[3]))
        }
        "hevc" if r.len() >= 13 && r[0] == 1 => {
            let space = ["", "A", "B", "C"][(r[1] >> 6) as usize];
            let tier = if r[1] & 0x20 != 0 { 'H' } else { 'L' };
            let profile = r[1] & 0x1f;
            // The compatibility flags are written bit-reversed.
            let compat = u32::from_be_bytes([r[2], r[3], r[4], r[5]]).reverse_bits();
            // Constraint bytes, trailing zero bytes dropped.
            let mut constraints: Vec<String> = r[6..12].iter().map(|b| format!("{b:X}")).collect();
            while constraints.len() > 1 && constraints.last().is_some_and(|c| c == "0") {
                constraints.pop();
            }
            Some(format!(
                "hvc1.{space}{profile}.{compat:X}.{tier}{}.{}",
                r[12],
                constraints.join(".")
            ))
        }
        _ => None,
    }
}

/// Approximate `CODECS` from ffprobe's profile name and level, for sources
/// without a configuration record. The constraint fields are guesses (the
/// common values), which players tolerate.
fn codecs_from_profile(codec: &str, profile: Option<&str>, level: Option<i64>) -> Option<String> {
    let level = level.filter(|l| *l > 0)?;
    match codec {
        "h264" => {
            let (idc, constraints) = match profile? {
                "Constrained Baseline" => (0x42, 0x40),
                "Baseline" => (0x42, 0x00),
                "Main" => (0x4d, 0x00),
                "High" => (0x64, 0x00),
                "High 10" => (0x6e, 0x00),
                "High 4:2:2" => (0x7a, 0x00),
                "High 4:4:4 Predictive" => (0xf4, 0x00),
                _ => return None,
            };
            Some(format!("avc1.{idc:02x}{constraints:02x}{level:02x}"))
        }
        "hevc" => {
            let (idc, compat) = match profile? {
                "Main" => (1, 6),
                "Main 10" => (2, 4),
                "Rext" => (4, 10),
                _ => return None,
            };
            Some(format!("hvc1.{idc}.{compat:X}.L{level}.B0"))
        }
        _ => None,
    }
}

/// Bitrate assumed for a copied track whose probe reported none.
const COPY_FALLBACK_KBPS: u32 = 640;
const STEREO_AAC_KBPS: u32 = 192;
const SURROUND_EAC3_KBPS: u32 = 640;

/// Plan source audio track `idx` as `flavor`. `None` when the track
/// doesn't exist, or for `Surround` when it has two channels or fewer
/// (the stereo rendition already carries everything).
///
/// * **Stereo**: AAC that's already stereo is copied. Anything else is
///   downmixed to 192k AAC, which every browser and Apple device decodes.
/// * **Surround**: AAC, AC-3 and E-AC-3 are copied, since Apple plays all
///   three (and passes AC-3/E-AC-3 through to a receiver). Codecs it can't
///   play (DTS, TrueHD, FLAC, Opus, PCM) become E-AC-3 5.1 at 640k.
pub fn derive_audio_plan(info: &MediaTechInfo, idx: u32, flavor: AudioFlavor) -> Option<AudioPlan> {
    let track = info.audio.get(idx as usize)?;
    let src_channels = track.channels.unwrap_or(2);
    let src_kbps = track.bitrate_kbps.and_then(|b| u32::try_from(b).ok());
    let codec = track.codec.as_str();
    let (out_codec, channels, bitrate_kbps, codecs) = match flavor {
        AudioFlavor::Stereo if codec == "aac" && src_channels <= 2 => {
            ("copy", src_channels, src_kbps.unwrap_or(STEREO_AAC_KBPS), "mp4a.40.2")
        }
        AudioFlavor::Stereo => ("aac", 2, STEREO_AAC_KBPS, "mp4a.40.2"),
        AudioFlavor::Surround if src_channels <= 2 => return None,
        AudioFlavor::Surround => match codec {
            "aac" => ("copy", src_channels, src_kbps.unwrap_or(COPY_FALLBACK_KBPS), "mp4a.40.2"),
            "ac3" => ("copy", src_channels, src_kbps.unwrap_or(COPY_FALLBACK_KBPS), "ac-3"),
            "eac3" => ("copy", src_channels, src_kbps.unwrap_or(COPY_FALLBACK_KBPS), "ec-3"),
            _ => ("eac3", src_channels.min(6), SURROUND_EAC3_KBPS, "ec-3"),
        },
    };
    // Atmos in E-AC-3 is signalled as joint object coding, not a count.
    let atmos = out_codec == "copy"
        && codec == "eac3"
        && track.profile.as_deref().is_some_and(|p| p.contains("Atmos"));
    let channels_attr = if atmos { "16/JOC".to_string() } else { channels.to_string() };
    // The track's own length where the probe has it: the container's is
    // the longest stream's, and grid slots past this track's end never get
    // written.
    let duration = track
        .duration_seconds
        .or(info.duration_seconds)
        .filter(|d| d.is_finite() && *d > 0.0)?;
    Some(AudioPlan {
        idx,
        flavor,
        src_codec: codec.to_string(),
        out_codec: out_codec.to_string(),
        channels,
        bitrate_kbps,
        codecs: codecs.to_string(),
        channels_attr,
        segments: uniform_segments(duration, TARGET_SEGMENT_SECS),
    })
}

/// Run ffprobe to enumerate video packets, fold keyframe timestamps into
/// segment groups of ~`TARGET_SEGMENT_SECS`, return the segment list.
///
/// Streams output line-by-line so memory stays O(segments), not O(packets).
async fn probe_segments(src: &Path, duration: f64) -> anyhow::Result<Vec<Segment>> {
    let mut child = Command::new("ffprobe")
        .arg("-v").arg("error")
        // Restrict to local file inputs only.
        .arg("-protocol_whitelist").arg("file")
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

/// Load a plan from the DB if one exists *and* matches the on-disk source's
/// mtime+size. Returns `Ok(None)` for cache miss (caller should rebuild).
pub async fn load_if_fresh(
    pool: &SqlitePool,
    file_id: &str,
    src: &Path,
) -> anyhow::Result<Option<StreamPlan>> {
    let row: Option<(Option<String>, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT stream_plan_json, source_mtime, source_size FROM media_files WHERE id = ?",
    )
    .bind(file_id)
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
    Ok(Some(plan))
}

pub async fn store(
    pool: &SqlitePool,
    file_id: &str,
    plan: &StreamPlan,
    source_mtime: i64,
    source_size: i64,
) -> anyhow::Result<()> {
    let json = serde_json::to_string(plan)?;
    sqlx::query(
        "UPDATE media_files SET stream_plan_json = ?, source_mtime = ?, source_size = ? WHERE id = ?",
    )
    .bind(json)
    .bind(source_mtime)
    .bind(source_size)
    .bind(file_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AudioTrackInfo, BrowserCompat};

    #[test]
    fn hexdump_parses_back_to_bytes() {
        let dump = "\n00000000: 0164 000d ffe1 0019 6764 000d acd9 4141  .d......gd....AA\n\
                    00000010: fb01 10                                  ...\n";
        assert_eq!(
            parse_hexdump(dump),
            vec![0x01, 0x64, 0x00, 0x0d, 0xff, 0xe1, 0x00, 0x19, 0x67, 0x64, 0x00, 0x0d, 0xac, 0xd9, 0x41, 0x41, 0xfb, 0x01, 0x10]
        );
    }

    #[test]
    fn codecs_from_avcc() {
        // x264 High@1.3, as ffprobe dumped it.
        let r = [0x01, 0x64, 0x00, 0x0d, 0xff, 0xe1];
        assert_eq!(codecs_from_record("h264", &r).as_deref(), Some("avc1.64000d"));
    }

    #[test]
    fn codecs_from_hvcc() {
        // x265 Main@3.1: profile 1, compat flags 0x60000000 (bit-reversed
        // to 6), constraint byte 0x90, level_idc 93.
        let main = [0x01, 0x01, 0x60, 0x00, 0x00, 0x00, 0x90, 0, 0, 0, 0, 0, 0x5d];
        assert_eq!(codecs_from_record("hevc", &main).as_deref(), Some("hvc1.1.6.L93.90"));
        // Main 10, high tier, level 5.1 (153), typical UHD Blu-ray header.
        let main10 = [0x01, 0x22, 0x20, 0x00, 0x00, 0x00, 0xb0, 0, 0, 0, 0, 0, 0x99];
        assert_eq!(codecs_from_record("hevc", &main10).as_deref(), Some("hvc1.2.4.H153.B0"));
    }

    #[test]
    fn annex_b_extradata_falls_back_to_profile_and_level() {
        let annex_b = [0x00, 0x00, 0x00, 0x01, 0x67, 0x64];
        assert_eq!(codecs_from_record("h264", &annex_b), None);
        assert_eq!(codecs_from_profile("h264", Some("High"), Some(41)).as_deref(), Some("avc1.640029"));
        assert_eq!(codecs_from_profile("hevc", Some("Main 10"), Some(120)).as_deref(), Some("hvc1.2.4.L120.B0"));
        assert_eq!(codecs_from_profile("hevc", Some("Main"), None), None);
        assert_eq!(codecs_from_profile("vp9", Some("Profile 0"), Some(41)), None);
    }

    fn track(codec: &str, channels: u32) -> AudioTrackInfo {
        AudioTrackInfo {
            codec: codec.into(),
            profile: None,
            duration_seconds: None,
            channels: Some(channels),
            channel_layout: None,
            sample_rate_hz: Some(48_000),
            bitrate_kbps: None,
            language: None,
            title: None,
            default: false,
        }
    }

    fn info(audio: Vec<AudioTrackInfo>) -> MediaTechInfo {
        MediaTechInfo {
            container: Some("matroska".into()),
            duration_seconds: Some(20.0),
            bitrate_kbps: None,
            file_size: None,
            video: None,
            audio,
            browser_compat: BrowserCompat::Remux,
            compat_reason: None,
        }
    }

    fn flavors(t: AudioTrackInfo) -> (Option<String>, Option<String>) {
        let i = info(vec![t]);
        let out = |f| derive_audio_plan(&i, 0, f).map(|a| format!("{} {} {}", a.out_codec, a.codecs, a.channels_attr));
        (out(AudioFlavor::Stereo), out(AudioFlavor::Surround))
    }

    #[test]
    fn stereo_sources_get_only_a_stereo_rendition() {
        assert_eq!(flavors(track("aac", 2)), (Some("copy mp4a.40.2 2".into()), None));
        assert_eq!(flavors(track("mp3", 2)), (Some("aac mp4a.40.2 2".into()), None));
        assert_eq!(flavors(track("ac3", 2)), (Some("aac mp4a.40.2 2".into()), None));
    }

    #[test]
    fn surround_passes_apple_codecs_through_and_converts_the_rest() {
        assert_eq!(flavors(track("ac3", 6)), (Some("aac mp4a.40.2 2".into()), Some("copy ac-3 6".into())));
        assert_eq!(flavors(track("eac3", 6)).1, Some("copy ec-3 6".into()));
        assert_eq!(flavors(track("aac", 6)).1, Some("copy mp4a.40.2 6".into()));
        // TrueHD 7.1 has no Apple decoder: E-AC-3, capped at 5.1.
        assert_eq!(flavors(track("truehd", 8)).1, Some("eac3 ec-3 6".into()));
        assert_eq!(flavors(track("dts", 6)).1, Some("eac3 ec-3 6".into()));
    }

    #[test]
    fn atmos_is_signalled_as_joint_object_coding() {
        let mut t = track("eac3", 6);
        t.profile = Some("Dolby Digital Plus + Dolby Atmos".into());
        assert_eq!(flavors(t).1, Some("copy ec-3 16/JOC".into()));
    }

    #[test]
    fn audio_timeline_is_a_uniform_grid() {
        let a = derive_audio_plan(&info(vec![track("aac", 2)]), 0, AudioFlavor::Stereo).unwrap();
        let starts: Vec<f64> = a.segments.iter().map(|s| s.t).collect();
        assert_eq!(starts, vec![0.0, 6.0, 12.0, 18.0]);
        assert!((a.segments[3].d - 2.0).abs() < 1e-9);
        assert_eq!(a.tag(), "a0-stereo");
        assert!(derive_audio_plan(&info(vec![]), 0, AudioFlavor::Stereo).is_none());
    }

    #[test]
    fn audio_timeline_follows_the_track_and_folds_a_sliver_tail() {
        // Container says 20s (its longest stream); this track is 18.021s.
        // The 0.021s past 18 is folded into the last segment rather than
        // becoming a slot ffmpeg would never write.
        let mut t = track("ac3", 6);
        t.duration_seconds = Some(18.021);
        let a = derive_audio_plan(&info(vec![t]), 0, AudioFlavor::Surround).unwrap();
        assert_eq!(a.segments.len(), 3);
        assert!((a.segments[2].d - 6.021).abs() < 1e-9);
    }

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
