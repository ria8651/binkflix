//! Render the HLS playlists straight from the plans. No ffmpeg involvement —
//! a playlist is available the moment its plan is, so the player sees the
//! full timeline before a single segment exists.
//!
//! The tree a player walks:
//!
//! ```text
//! master.m3u8                      variants + renditions
//! ├── index.m3u8?mode=…            video (one plan, per mode/bitrate)
//! ├── audio/{idx}-{flavor}/index.m3u8   one per audio rendition
//! └── subs-{track}.m3u8            one WebVTT segment per subtitle track
//! ```

use super::plan::{AudioFlavor, Segment};

/// Render a VOD media playlist over `segments`. `query` is appended to the
/// init.mp4 and segment URIs so their fetches carry the full request shape
/// (relative-URI resolution drops the playlist's own query, a known
/// footgun) — for video that's `mode`/`bitrate`, which pick the plan.
///
/// `query` should end with the plan-dir name as `v=` (e.g.
/// `v=v15-m…-s…-v-remux`) — the server's content-identity key. It busts the
/// `immutable` cache entries those URLs earn whenever the plan regenerates
/// (source mtime/size change, plan-version bump, encode change). Playlists
/// are served `no-store`, so a new generation yields fresh URLs and old
/// pinned entries are never re-requested. Segment handlers ignore `v=`
/// (they resolve the plan dir from live source state), so the token is
/// purely a client cache key.
pub fn render_m3u8(segments: &[Segment], query: &str, time_offset: Option<f64>) -> String {
    let target = segments.iter().map(|s| s.d.ceil() as u64).max().unwrap_or(6);

    let mut out = String::with_capacity(64 + segments.len() * 48);
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
    for s in segments {
        out.push_str(&format!("#EXTINF:{:.3},\n", s.d));
        out.push_str(&format!("seg-{:05}.m4s?{query}\n", s.i));
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

/// What the master playlist says about the video rendition.
pub struct VideoVariant<'a> {
    /// Query for `index.m3u8`: what picks the plan (`mode`, `bitrate`),
    /// plus `a`/`t` to carry the client's audio pick and start position.
    pub query: &'a str,
    pub bandwidth_bps: u64,
    pub resolution: Option<(u32, u32)>,
    pub codecs: Option<&'a str>,
    pub video_range: Option<&'a str>,
}

/// One audio rendition: source track `idx` as `flavor`.
pub struct AudioRendition<'a> {
    pub idx: u32,
    pub flavor: AudioFlavor,
    /// Shared by a track's stereo and surround renditions, so players show
    /// the two as one choice and pick the flavor through the variant.
    pub name: &'a str,
    pub language: Option<&'a str>,
    pub default: bool,
    pub codecs: &'a str,
    pub channels: &'a str,
    pub bitrate_kbps: u32,
}

/// A subtitle track as the master playlist advertises it.
pub struct SubtitleRendition<'a> {
    pub track_id: &'a str,
    pub name: &'a str,
    pub language: &'a str,
    pub default: bool,
    pub forced: bool,
}

/// Render the master playlist.
///
/// Audio is split into two groups, each referenced by its own variant over
/// the same video: `stereo` (AAC, plays anywhere) and `surround`
/// (passthrough or E-AC-3). Every group carries every track, so the
/// surround group falls back to a track's stereo rendition when it has no
/// surround one; otherwise a player on that variant couldn't pick the
/// track at all. The surround group only exists if some track has a
/// surround rendition. The variants' `CODECS` are what let
/// a player take the one it can decode — Apple TV the surround, a browser
/// the stereo — without the client having to know. Surround is listed
/// first: players start on the first variant they can play, and starting
/// on stereo would invite an ABR "upgrade" to surround mid-playback, which
/// swaps the audio under the viewer.
///
/// The surround variant is only offered when the video's `CODECS` is
/// known, since a variant's `CODECS` has to name every format or none,
/// and without it nothing tells a browser to skip the AC-3 variant.
///
/// Quality isn't a variant: the user picks it, so there is never more than
/// one video plan here.
pub fn render_master(
    video: &VideoVariant,
    audio: &[AudioRendition],
    subs: &[SubtitleRendition],
) -> String {
    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    let yn = |b: bool| if b { "YES" } else { "NO" };

    // Groups in variant order: each track's rendition of the group's
    // flavor, else its stereo one.
    let mut groups: Vec<(AudioFlavor, Vec<&AudioRendition>)> = Vec::new();
    let has_surround = audio.iter().any(|a| a.flavor == AudioFlavor::Surround);
    for flavor in [AudioFlavor::Surround, AudioFlavor::Stereo] {
        if flavor == AudioFlavor::Surround && (!has_surround || video.codecs.is_none()) {
            continue;
        }
        let mut members: Vec<&AudioRendition> = Vec::new();
        for a in audio {
            if members.iter().any(|m| m.idx == a.idx) {
                continue;
            }
            let own = audio.iter().find(|r| r.idx == a.idx && r.flavor == flavor);
            let stereo = audio.iter().find(|r| r.idx == a.idx && r.flavor == AudioFlavor::Stereo);
            if let Some(r) = own.or(stereo) {
                members.push(r);
            }
        }
        if !members.is_empty() {
            groups.push((flavor, members));
        }
    }

    // Names are per track, deduped across tracks, and reused by both
    // groups.
    let mut names: Vec<(u32, String)> = Vec::new();
    for a in audio {
        if !names.iter().any(|(i, _)| *i == a.idx) {
            let name = unique_name(a.name, &format!("Track {}", a.idx + 1), names.iter().map(|(_, n)| n));
            names.push((a.idx, name));
        }
    }
    for (flavor, members) in &groups {
        let mut has_default = false;
        for a in members {
            let name = &names.iter().find(|(i, _)| *i == a.idx).expect("named above").1;
            let default = a.default && !has_default;
            has_default |= default;
            out.push_str(&format!(
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"{}\",NAME=\"{name}\"",
                flavor.as_str()
            ));
            if let Some(lang) = a.language.filter(|l| is_language_tag(l)) {
                out.push_str(&format!(",LANGUAGE=\"{lang}\""));
            }
            out.push_str(&format!(
                ",DEFAULT={},AUTOSELECT=YES,CHANNELS=\"{}\",URI=\"audio/{}-{}/index.m3u8\"\n",
                yn(default),
                a.channels,
                a.idx,
                a.flavor.as_str(),
            ));
        }
    }

    let mut sub_names: Vec<String> = Vec::with_capacity(subs.len());
    let mut has_default = false;
    for s in subs {
        let name = unique_name(s.name, s.track_id, sub_names.iter());
        // At most one DEFAULT=YES per group.
        let default = s.default && !has_default;
        has_default |= default;
        out.push_str(&format!("#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"{name}\""));
        if is_language_tag(s.language) {
            out.push_str(&format!(",LANGUAGE=\"{}\"", s.language));
        }
        // FORCED=YES requires AUTOSELECT=YES.
        out.push_str(&format!(
            ",DEFAULT={},AUTOSELECT=YES,FORCED={},URI=\"subs-{}.m3u8\"\n",
            yn(default),
            yn(s.forced),
            s.track_id,
        ));
        sub_names.push(name);
    }

    let mut variant = |audio: Option<(AudioFlavor, &[&AudioRendition])>| {
        let audio_kbps = audio
            .and_then(|(_, m)| m.iter().map(|a| a.bitrate_kbps).max())
            .unwrap_or(0);
        out.push_str(&format!(
            "#EXT-X-STREAM-INF:BANDWIDTH={}",
            video.bandwidth_bps + u64::from(audio_kbps) * 1000
        ));
        if let Some(v) = video.codecs {
            let mut codecs = vec![v];
            for a in audio.map(|(_, m)| m).unwrap_or_default() {
                if !codecs.contains(&a.codecs) {
                    codecs.push(a.codecs);
                }
            }
            out.push_str(&format!(",CODECS=\"{}\"", codecs.join(",")));
        }
        if let Some((w, h)) = video.resolution {
            out.push_str(&format!(",RESOLUTION={w}x{h}"));
        }
        if let Some(range) = video.video_range {
            out.push_str(&format!(",VIDEO-RANGE={range}"));
        }
        if let Some((flavor, _)) = audio {
            out.push_str(&format!(",AUDIO=\"{}\"", flavor.as_str()));
        }
        if !subs.is_empty() {
            out.push_str(",SUBTITLES=\"subs\"");
        }
        // Without this a player may assume CEA-608 captions in the video
        // and offer a phantom "CC" option (AVPlayer does).
        out.push_str(",CLOSED-CAPTIONS=NONE");
        out.push_str(&format!("\nindex.m3u8?{}\n", video.query));
    };
    if groups.is_empty() {
        variant(None);
    }
    for (flavor, members) in &groups {
        variant(Some((*flavor, members)));
    }
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

/// `name` (or `fallback` when empty) made safe for a quoted string and
/// unique against `taken`. NAME must be unique within a group, and quoted
/// strings can't carry quotes or newlines.
fn unique_name<'a>(name: &str, fallback: &str, taken: impl Iterator<Item = &'a String> + Clone) -> String {
    let base = quoted_safe(if name.is_empty() { fallback } else { name });
    let mut candidate = base.clone();
    let mut n = 2;
    while taken.clone().any(|t| *t == candidate) {
        candidate = format!("{base} ({n})");
        n += 1;
    }
    candidate
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
    use super::*;

    fn segments() -> Vec<Segment> {
        vec![Segment { i: 1, t: 0.0, d: 6.0 }, Segment { i: 2, t: 6.0, d: 6.0 }]
    }

    #[test]
    fn renders_complete_vod_playlist() {
        let s = render_m3u8(&segments(), "mode=remux&v=tag", None);
        assert!(s.starts_with("#EXTM3U\n"));
        assert!(s.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(s.contains("#EXT-X-MAP:URI=\"init.mp4?mode=remux&v=tag\""));
        assert!(s.contains("seg-00001.m4s?mode=remux&v=tag"));
        assert!(s.contains("seg-00002.m4s?mode=remux&v=tag"));
        assert!(s.ends_with("#EXT-X-ENDLIST\n"));
    }

    #[test]
    fn target_duration_covers_longest_segment() {
        let mut segs = segments();
        segs.push(Segment { i: 3, t: 12.0, d: 9.7 });
        let s = render_m3u8(&segs, "v=tag", None);
        assert!(s.contains("#EXT-X-TARGETDURATION:10"));
    }

    #[test]
    fn time_offset_emits_ext_x_start() {
        let s = render_m3u8(&segments(), "v=tag", Some(7.5));
        assert!(s.contains("#EXT-X-START:TIME-OFFSET=7.500,PRECISE=YES"));
    }

    #[test]
    fn zero_or_negative_time_offset_is_omitted() {
        assert!(!render_m3u8(&segments(), "v=tag", Some(0.0)).contains("EXT-X-START"));
        assert!(!render_m3u8(&segments(), "v=tag", Some(-1.0)).contains("EXT-X-START"));
    }

    fn video<'a>(codecs: Option<&'a str>) -> VideoVariant<'a> {
        VideoVariant {
            query: "mode=remux&a=0",
            bandwidth_bps: 5_000_000,
            resolution: Some((1920, 1080)),
            codecs,
            video_range: None,
        }
    }

    fn audio<'a>(idx: u32, flavor: AudioFlavor, name: &'a str, codecs: &'a str) -> AudioRendition<'a> {
        let (channels, bitrate_kbps) = match flavor {
            AudioFlavor::Stereo => ("2", 192),
            AudioFlavor::Surround => ("6", 640),
        };
        AudioRendition {
            idx,
            flavor,
            name,
            language: Some("eng"),
            default: idx == 0,
            codecs,
            channels,
            bitrate_kbps,
        }
    }

    fn sub<'a>(track_id: &'a str, name: &'a str, language: &'a str, default: bool) -> SubtitleRendition<'a> {
        SubtitleRendition { track_id, name, language, default, forced: false }
    }

    #[test]
    fn master_offers_a_surround_and_a_stereo_variant() {
        let renditions = [
            audio(0, AudioFlavor::Surround, "English", "ac-3"),
            audio(0, AudioFlavor::Stereo, "English", "mp4a.40.2"),
            audio(1, AudioFlavor::Stereo, "Japanese", "mp4a.40.2"),
        ];
        let s = render_master(&video(Some("hvc1.2.4.L120.B0")), &renditions, &[]);
        assert!(s.contains(
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"surround\",NAME=\"English\",LANGUAGE=\"eng\",\
             DEFAULT=YES,AUTOSELECT=YES,CHANNELS=\"6\",URI=\"audio/0-surround/index.m3u8\"\n"
        ));
        assert!(s.contains("GROUP-ID=\"stereo\",NAME=\"English\""));
        assert!(s.contains(
            "GROUP-ID=\"stereo\",NAME=\"Japanese\",LANGUAGE=\"eng\",DEFAULT=NO,AUTOSELECT=YES,\
             CHANNELS=\"2\",URI=\"audio/1-stereo/index.m3u8\""
        ));
        // A track without surround still has to be pickable on the
        // surround variant, so that group falls back to its stereo.
        assert!(s.contains(
            "GROUP-ID=\"surround\",NAME=\"Japanese\",LANGUAGE=\"eng\",DEFAULT=NO,AUTOSELECT=YES,\
             CHANNELS=\"2\",URI=\"audio/1-stereo/index.m3u8\""
        ));
        // Surround first, so a player that can take it starts there.
        let surround = s.find("AUDIO=\"surround\"").expect("surround variant");
        let stereo = s.find("AUDIO=\"stereo\"").expect("stereo variant");
        assert!(surround < stereo);
        assert!(s.contains(
            "#EXT-X-STREAM-INF:BANDWIDTH=5640000,CODECS=\"hvc1.2.4.L120.B0,ac-3,mp4a.40.2\",\
             RESOLUTION=1920x1080,AUDIO=\"surround\",CLOSED-CAPTIONS=NONE\nindex.m3u8?mode=remux&a=0\n"
        ));
        assert!(s.contains(
            "#EXT-X-STREAM-INF:BANDWIDTH=5192000,CODECS=\"hvc1.2.4.L120.B0,mp4a.40.2\",\
             RESOLUTION=1920x1080,AUDIO=\"stereo\",CLOSED-CAPTIONS=NONE\nindex.m3u8?mode=remux&a=0\n"
        ));
    }

    #[test]
    fn master_without_any_surround_has_one_variant() {
        let renditions = [audio(0, AudioFlavor::Stereo, "English", "mp4a.40.2")];
        let s = render_master(&video(Some("avc1.640029")), &renditions, &[]);
        assert!(!s.contains("surround"));
        assert_eq!(s.matches("#EXT-X-STREAM-INF").count(), 1);
    }

    #[test]
    fn master_without_video_codecs_offers_only_stereo() {
        let renditions = [
            audio(0, AudioFlavor::Surround, "English", "ac-3"),
            audio(0, AudioFlavor::Stereo, "English", "mp4a.40.2"),
        ];
        let s = render_master(&video(None), &renditions, &[]);
        assert!(!s.contains("surround"));
        assert!(!s.contains("CODECS"));
        assert!(s.contains("AUDIO=\"stereo\""));
    }

    #[test]
    fn master_without_audio_has_one_plain_variant() {
        let s = render_master(&video(Some("avc1.640029")), &[], &[]);
        assert!(!s.contains("AUDIO"));
        assert!(s.ends_with(
            "#EXT-X-STREAM-INF:BANDWIDTH=5000000,CODECS=\"avc1.640029\",RESOLUTION=1920x1080,\
             CLOSED-CAPTIONS=NONE\n\
             index.m3u8?mode=remux&a=0\n"
        ));
    }

    #[test]
    fn master_names_tracks_once_and_reuses_them_across_groups() {
        let renditions = [
            audio(0, AudioFlavor::Surround, "", "ec-3"),
            audio(0, AudioFlavor::Stereo, "", "mp4a.40.2"),
            audio(1, AudioFlavor::Stereo, "Track 1", "mp4a.40.2"),
        ];
        let s = render_master(&video(Some("avc1.640029")), &renditions, &[]);
        assert_eq!(s.matches("NAME=\"Track 1\"").count(), 2);
        assert!(s.contains("NAME=\"Track 1 (2)\""));
    }

    #[test]
    fn master_lists_subtitle_renditions() {
        let subs = [sub("embed-3", "English", "eng", true), sub("file-0", "Signs", "eng", false)];
        let s = render_master(&video(None), &[], &subs);
        assert!(s.contains(
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",LANGUAGE=\"eng\",\
             DEFAULT=YES,AUTOSELECT=YES,FORCED=NO,URI=\"subs-embed-3.m3u8\"\n"
        ));
        assert!(s.contains("NAME=\"Signs\",LANGUAGE=\"eng\",DEFAULT=NO"));
        assert!(s.contains("RESOLUTION=1920x1080,SUBTITLES=\"subs\",CLOSED-CAPTIONS=NONE\n"));
    }

    #[test]
    fn master_dedupes_subtitle_names_and_defaults() {
        let subs = [
            sub("a", "Eng \"full\"", "", true),
            sub("b", "Eng \"full\"", "und", true),
            sub("c", "", "en-US", false),
        ];
        let s = render_master(&video(None), &[], &subs);
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
}
