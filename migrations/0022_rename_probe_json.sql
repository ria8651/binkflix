-- Rename `media.tech_json` → `probe_json` to better describe the content:
-- the column caches the JSON output of an ffprobe pass over the source
-- file (the `MediaTechInfo` struct in code). The original name dates from
-- when it was treated as an ephemeral debug cache; it has since become
-- load-bearing for playback (`derive_audio_plan` indexes the track list
-- here to pick which audio stream ffmpeg maps into the HLS output, so a
-- stale value drops audio after an in-place file swap). The role-side of
-- the change is captured by the new `content_mtime`/`content_size`
-- signature in 0021 + the validate-on-read path in `media_info::load_fresh`.
ALTER TABLE media RENAME COLUMN tech_json TO probe_json;
