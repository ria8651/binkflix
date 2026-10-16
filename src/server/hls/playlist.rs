//! Render a VOD m3u8 playlist directly from a `StreamPlan`. No ffmpeg
//! involvement — the playlist is available the moment the plan is built,
//! letting the player see the full timeline before a single segment exists.

use super::plan::StreamPlan;

/// Render the VOD m3u8. `audio_idx`, `mode`, and `bitrate` are appended to
/// the init.mp4 + segment URIs so hls.js's segment fetches inherit the
/// full request shape — relative-URI resolution dropping the parent's
/// query is a known footgun.
///
/// `cache_tag` is the plan-dir name (e.g. `v12-m…-s…-a0-tx…`) — the server's
/// content-identity key. Threaded into the segment/init URIs as `&v=` so the
/// `immutable` cache entries those URLs earn are correctly busted whenever the
/// plan regenerates (source mtime/size change, plan-version bump, encode
/// change). The m3u8 itself is served `no-store`, so a new generation yields a
/// fresh playlist with fresh URLs and old pinned entries are never re-requested.
/// The `serve` segment handler ignores `v=` (it resolves the plan dir from live
/// source state), so the token is purely a client cache key.
pub fn render_m3u8(
    plan: &StreamPlan,
    audio_idx: u32,
    mode: Option<&str>,
    bitrate_kbps: Option<u32>,
    time_offset: Option<f64>,
    cache_tag: &str,
) -> String {
    let target = plan
        .segments
        .iter()
        .map(|s| s.d.ceil() as u64)
        .max()
        .unwrap_or(6);

    let mut query = format!("a={audio_idx}");
    if let Some(m) = mode {
        if !m.is_empty() {
            query.push_str(&format!("&mode={m}"));
        }
    }
    if let Some(b) = bitrate_kbps {
        query.push_str(&format!("&bitrate={b}"));
    }
    if !cache_tag.is_empty() {
        query.push_str(&format!("&v={cache_tag}"));
    }

    let mut out = String::with_capacity(64 + plan.segments.len() * 48);
    out.push_str("#EXTM3U\n");
    out.push_str("#EXT-X-VERSION:7\n");
    // Tell the player where the user wants to start. Both hls.js and
    // Safari respect this and align their first segment fetch to the
    // offset, which is what stops the server from spawning ffmpeg at
    // seg 1 when the user is resuming at e.g. 27:54. PRECISE=YES means
    // the player must not slide forward to the next IDR — segments are
    // already keyframe-aligned at our 6s boundaries (HLS-fmp4 +
    // independent_segments + force_key_frames at the producer), so
    // there's no IDR for it to slide to anyway.
    if let Some(t) = time_offset {
        if t.is_finite() && t > 0.0 {
            out.push_str(&format!("#EXT-X-START:TIME-OFFSET={t:.3},PRECISE=YES\n"));
        }
    }
    out.push_str(&format!("#EXT-X-TARGETDURATION:{target}\n"));
    out.push_str("#EXT-X-MEDIA-SEQUENCE:1\n");
    out.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
    out.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
    out.push_str(&format!("#EXT-X-MAP:URI=\"init.mp4?{query}\"\n"));
    for s in &plan.segments {
        out.push_str(&format!("#EXTINF:{:.3},\n", s.d));
        out.push_str(&format!("seg-{:05}.m4s?{query}\n", s.i));
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

/// A subtitle track as the master playlist advertises it.
pub struct SubtitleRendition<'a> {
    pub track_id: &'a str,
    pub name: &'a str,
    pub language: &'a str,
    pub default: bool,
    pub forced: bool,
}

/// Render the master playlist: one variant (the muxed media playlist, since
/// the user picks quality rather than ABR) plus a WebVTT rendition per
/// subtitle track, which is what gives AVPlayer its native subtitle menu.
/// `query` goes on every child URI so they resolve the same plan the master
/// did. `start_query` (`&t=`) only goes on the variant, where `EXT-X-START`
/// is rendered.
///
/// `BANDWIDTH` is required but only steers variant choice, so an estimate
/// is fine with one variant. `CODECS` is left off until audio is demuxed:
/// AVPlayer reads them from init.mp4, and a wrong string is worse than none.
pub fn render_master(
    query: &str,
    start_query: &str,
    bandwidth_bps: u64,
    resolution: Option<(u32, u32)>,
    subs: &[SubtitleRendition],
) -> String {
    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    let mut names: Vec<String> = Vec::with_capacity(subs.len());
    let mut has_default = false;
    for s in subs {
        // NAME must be unique within the group, and quoted strings can't
        // carry quotes or newlines.
        let base = quoted_safe(if s.name.is_empty() { s.track_id } else { s.name });
        let mut name = base.clone();
        let mut n = 2;
        while names.contains(&name) {
            name = format!("{base} ({n})");
            n += 1;
        }
        // At most one DEFAULT=YES per group.
        let default = s.default && !has_default;
        has_default |= default;
        let yn = |b: bool| if b { "YES" } else { "NO" };
        out.push_str(&format!("#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"{name}\""));
        if is_language_tag(s.language) {
            out.push_str(&format!(",LANGUAGE=\"{}\"", s.language));
        }
        // FORCED=YES requires AUTOSELECT=YES.
        out.push_str(&format!(
            ",DEFAULT={},AUTOSELECT=YES,FORCED={},URI=\"subs-{}.m3u8?{query}\"\n",
            yn(default),
            yn(s.forced),
            s.track_id,
        ));
        names.push(name);
    }
    out.push_str(&format!("#EXT-X-STREAM-INF:BANDWIDTH={bandwidth_bps}"));
    if let Some((w, h)) = resolution {
        out.push_str(&format!(",RESOLUTION={w}x{h}"));
    }
    if !subs.is_empty() {
        out.push_str(",SUBTITLES=\"subs\"");
    }
    out.push_str(&format!("\nindex.m3u8?{query}{start_query}\n"));
    out
}

/// Render a subtitle rendition: the whole track as one WebVTT segment
/// spanning the media. The segment is the track flattened to VTT; it has a
/// `.vtt` name because ffmpeg-based players refuse other segment URLs.
pub fn render_subtitle_playlist(duration: f64, track_id: &str) -> String {
    let d = duration.max(0.001);
    format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:1\n\
         #EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:{d:.3},\nsubs-{track_id}.vtt\n#EXT-X-ENDLIST\n",
        d.ceil() as u64,
    )
}

fn quoted_safe(s: &str) -> String {
    s.chars()
        .map(|c| if c == '"' || c.is_control() { '\'' } else { c })
        .collect()
}

/// Loose RFC 5646 check. The scanner stores whatever the container said:
/// usually ISO 639-2 (`eng`), sometimes `und` or nothing.
fn is_language_tag(s: &str) -> bool {
    let mut parts = s.split('-');
    let primary = parts.next().unwrap_or_default();
    s != "und"
        && (2..=3).contains(&primary.len())
        && primary.chars().all(|c| c.is_ascii_alphabetic())
        && parts.all(|p| (1..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

#[cfg(test)]
mod tests {
    use super::super::plan::{Mode, Segment, StreamPlan, PLAN_VERSION};
    use super::*;

    fn sample_plan() -> StreamPlan {
        StreamPlan {
            version: PLAN_VERSION,
            mode: Mode::Remux,
            duration: 12.0,
            video_codec: "h264".into(),
            segments: vec![
                Segment { i: 1, t: 0.0, d: 6.0 },
                Segment { i: 2, t: 6.0, d: 6.0 },
            ],
        }
    }

    #[test]
    fn renders_complete_vod_playlist() {
        let s = render_m3u8(&sample_plan(), 0, None, None, None, "");
        assert!(s.starts_with("#EXTM3U\n"));
        assert!(s.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(s.contains("#EXT-X-MAP:URI=\"init.mp4?a=0\""));
        assert!(s.contains("seg-00001.m4s?a=0"));
        assert!(s.contains("seg-00002.m4s?a=0"));
        assert!(s.ends_with("#EXT-X-ENDLIST\n"));
    }

    #[test]
    fn target_duration_covers_longest_segment() {
        let mut plan = sample_plan();
        plan.segments.push(Segment { i: 3, t: 12.0, d: 9.7 });
        let s = render_m3u8(&plan, 0, None, None, None, "");
        assert!(s.contains("#EXT-X-TARGETDURATION:10"));
    }

    #[test]
    fn audio_idx_threads_into_segment_uris() {
        let s = render_m3u8(&sample_plan(), 2, None, None, None, "");
        assert!(s.contains("#EXT-X-MAP:URI=\"init.mp4?a=2\""));
        assert!(s.contains("seg-00001.m4s?a=2"));
        assert!(s.contains("seg-00002.m4s?a=2"));
    }

    #[test]
    fn mode_and_bitrate_thread_into_segment_uris() {
        let s = render_m3u8(&sample_plan(), 0, Some("transcode"), Some(4000), None, "");
        assert!(s.contains("#EXT-X-MAP:URI=\"init.mp4?a=0&mode=transcode&bitrate=4000\""));
        assert!(s.contains("seg-00001.m4s?a=0&mode=transcode&bitrate=4000"));
    }

    #[test]
    fn cache_tag_threads_into_segment_uris() {
        let tag = "v12-m1709251234-s5242880-a0-tx4000h720";
        let s = render_m3u8(&sample_plan(), 0, Some("transcode"), Some(4000), None, tag);
        assert!(s.contains(&format!(
            "#EXT-X-MAP:URI=\"init.mp4?a=0&mode=transcode&bitrate=4000&v={tag}\""
        )));
        assert!(s.contains(&format!("seg-00001.m4s?a=0&mode=transcode&bitrate=4000&v={tag}")));
    }

    #[test]
    fn empty_cache_tag_omits_v_param() {
        let s = render_m3u8(&sample_plan(), 0, None, None, None, "");
        assert!(!s.contains("&v="));
    }

    fn sub<'a>(track_id: &'a str, name: &'a str, language: &'a str, default: bool) -> SubtitleRendition<'a> {
        SubtitleRendition { track_id, name, language, default, forced: false }
    }

    #[test]
    fn master_points_at_media_playlist_and_subtitle_renditions() {
        let subs = [sub("embed-3", "English", "eng", true), sub("file-0", "Signs", "eng", false)];
        let s = render_master("a=1&mode=remux", "&t=12", 5_000_000, Some((1920, 1080)), &subs);
        assert!(s.starts_with("#EXTM3U\n"));
        assert!(s.contains(
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",LANGUAGE=\"eng\",\
             DEFAULT=YES,AUTOSELECT=YES,FORCED=NO,URI=\"subs-embed-3.m3u8?a=1&mode=remux\"\n"
        ));
        assert!(s.contains("NAME=\"Signs\",LANGUAGE=\"eng\",DEFAULT=NO"));
        assert!(s.contains(
            "#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,SUBTITLES=\"subs\"\n\
             index.m3u8?a=1&mode=remux&t=12\n"
        ));
    }

    #[test]
    fn master_without_subtitles_omits_the_group() {
        let s = render_master("a=0", "", 1000, None, &[]);
        assert!(!s.contains("SUBTITLES"));
        assert!(s.ends_with("#EXT-X-STREAM-INF:BANDWIDTH=1000\nindex.m3u8?a=0\n"));
    }

    #[test]
    fn master_dedupes_names_and_defaults() {
        let subs = [
            sub("a", "Eng \"full\"", "", true),
            sub("b", "Eng \"full\"", "und", true),
            sub("c", "", "en-US", false),
        ];
        let s = render_master("a=0", "", 1000, None, &subs);
        assert!(s.contains("NAME=\"Eng 'full'\",DEFAULT=YES"));
        assert!(s.contains("NAME=\"Eng 'full' (2)\",DEFAULT=NO"));
        assert!(s.contains("NAME=\"c\",LANGUAGE=\"en-US\""));
    }

    #[test]
    fn subtitle_playlist_is_one_segment_spanning_the_media() {
        let s = render_subtitle_playlist(1425.4, "embed-3");
        assert!(s.contains("#EXT-X-TARGETDURATION:1426\n"));
        assert!(s.contains("#EXTINF:1425.400,\nsubs-embed-3.vtt\n"));
        assert!(s.ends_with("#EXT-X-ENDLIST\n"));
    }

    #[test]
    fn time_offset_emits_ext_x_start() {
        let s = render_m3u8(&sample_plan(), 0, None, None, Some(7.5), "");
        assert!(s.contains("#EXT-X-START:TIME-OFFSET=7.500,PRECISE=YES"));
    }

    #[test]
    fn zero_or_negative_time_offset_is_omitted() {
        let s_zero = render_m3u8(&sample_plan(), 0, None, None, Some(0.0), "");
        let s_neg = render_m3u8(&sample_plan(), 0, None, None, Some(-1.0), "");
        assert!(!s_zero.contains("EXT-X-START"));
        assert!(!s_neg.contains("EXT-X-START"));
    }
}
