-- The file content (mtime unix-secs + size bytes) that this row's scanner-
-- derived data (probe_json + subtitles, plus the cosmetic thumbnails and
-- trickplay) was built from. (probe_json is renamed from tech_json in
-- 0022 — see that migration's preamble for the why.)
-- Compared bidirectionally against the live file by both the scanner and by
-- read-time consumers, so an in-place swap with a preserved/backdated mtime is
-- detected even when byte size is unchanged. NULL = never derived / pre-fix.
ALTER TABLE media ADD COLUMN content_mtime INTEGER;
ALTER TABLE media ADD COLUMN content_size  INTEGER;

-- Distinguish a full library scan from an access-triggered single-file refresh
-- in the timing history.
ALTER TABLE scan_timings ADD COLUMN trigger TEXT NOT NULL DEFAULT 'scan';
