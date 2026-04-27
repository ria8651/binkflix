//! Render a VOD m3u8 playlist directly from a `StreamPlan`. No ffmpeg
//! involvement — the playlist is available the moment the plan is built,
//! letting the player see the full timeline before a single segment exists.

use super::plan::StreamPlan;

pub fn render_m3u8(plan: &StreamPlan) -> String {
    // EXT-X-TARGETDURATION must be >= the longest segment, rounded up. A
    // value too small confuses some clients (they retry the playlist
    // expecting it to grow).
    let target = plan
        .segments
        .iter()
        .map(|s| s.d.ceil() as u64)
        .max()
        .unwrap_or(6);

    let mut out = String::with_capacity(64 + plan.segments.len() * 48);
    out.push_str("#EXTM3U\n");
    out.push_str("#EXT-X-VERSION:7\n");
    out.push_str(&format!("#EXT-X-TARGETDURATION:{target}\n"));
    out.push_str("#EXT-X-MEDIA-SEQUENCE:1\n");
    out.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
    out.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
    out.push_str("#EXT-X-MAP:URI=\"init.mp4\"\n");
    for s in &plan.segments {
        out.push_str(&format!("#EXTINF:{:.3},\n", s.d));
        out.push_str(&format!("seg-{:05}.m4s\n", s.i));
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

#[cfg(test)]
mod tests {
    use super::super::plan::{AudioPlan, Mode, Segment, StreamPlan, PLAN_VERSION};
    use super::*;

    fn sample_plan() -> StreamPlan {
        StreamPlan {
            version: PLAN_VERSION,
            mode: Mode::Remux,
            duration: 12.0,
            video_codec: "h264".into(),
            audio: AudioPlan {
                src_codec: Some("aac".into()),
                out_codec: "copy".into(),
                channels: 2,
                bitrate_kbps: 192,
            },
            segments: vec![
                Segment { i: 1, t: 0.0, d: 6.0 },
                Segment { i: 2, t: 6.0, d: 6.0 },
            ],
        }
    }

    #[test]
    fn renders_complete_vod_playlist() {
        let s = render_m3u8(&sample_plan());
        assert!(s.starts_with("#EXTM3U\n"));
        assert!(s.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(s.contains("#EXT-X-MAP:URI=\"init.mp4\""));
        assert!(s.contains("seg-00001.m4s"));
        assert!(s.contains("seg-00002.m4s"));
        assert!(s.ends_with("#EXT-X-ENDLIST\n"));
    }

    #[test]
    fn target_duration_covers_longest_segment() {
        let mut plan = sample_plan();
        plan.segments.push(Segment { i: 3, t: 12.0, d: 9.7 });
        let s = render_m3u8(&plan);
        assert!(s.contains("#EXT-X-TARGETDURATION:10"));
    }
}
