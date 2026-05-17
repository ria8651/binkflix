-- Per-asset scan versions. The single `scan_version` on `media` (added in
-- 0013) was overloaded: bumping it to re-upsert metadata (a new column or
-- NFO field) also forced re-extraction of every subtitle, thumbnail, and
-- trickplay sprite — an hour-long job for a modest library. Split it: keep
-- `scan_version` for metadata, and gate each asset pass on its own column.
--
-- DEFAULT matches the initial constant value in scanner.rs so existing rows
-- are treated as up-to-date on this migration. (Contrast 0013_scan_version
-- which defaulted to 0 to deliberately invalidate every row.) When a future
-- change needs to re-run one extractor, bump the matching constant; rows
-- with stored < constant become stale for just that pass.
ALTER TABLE media ADD COLUMN subtitles_version  INTEGER NOT NULL DEFAULT 1;
ALTER TABLE media ADD COLUMN thumbnails_version INTEGER NOT NULL DEFAULT 1;
ALTER TABLE media ADD COLUMN trickplay_version  INTEGER NOT NULL DEFAULT 1;
