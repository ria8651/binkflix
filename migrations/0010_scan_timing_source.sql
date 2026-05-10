-- Source-side encoding info on each scan_timings row, so per-stage timings
-- (especially trickplay_ms) can be correlated with codec / resolution /
-- bitrate / GOP without re-probing the file later. Populated from the
-- ffprobe results already gathered in pass 1, plus a keyframe count
-- captured as a side-effect of the existing trickplay ffmpeg call.

ALTER TABLE scan_timings ADD COLUMN video_codec    TEXT;
ALTER TABLE scan_timings ADD COLUMN audio_codec    TEXT;
ALTER TABLE scan_timings ADD COLUMN container      TEXT;
ALTER TABLE scan_timings ADD COLUMN width          INTEGER;
ALTER TABLE scan_timings ADD COLUMN height         INTEGER;
ALTER TABLE scan_timings ADD COLUMN duration_ms    INTEGER;
ALTER TABLE scan_timings ADD COLUMN bitrate_kbps   INTEGER;
ALTER TABLE scan_timings ADD COLUMN pixel_format   TEXT;
-- NULL when the trickplay job didn't run (too-short clip) or its ffmpeg
-- failed before producing a parseable frame log.
ALTER TABLE scan_timings ADD COLUMN keyframe_count INTEGER;
