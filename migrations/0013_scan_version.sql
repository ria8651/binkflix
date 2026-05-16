-- Per-row scan version. The scanner's early-return ("nothing newer than
-- scanned_at, skip") doesn't fire when the *code* changed what it persists —
-- e.g. when a new column starts being written, or when an existing column
-- starts being filled from a previously-ignored NFO field. Bumping a
-- compile-time constant invalidates every row whose stored version is lower,
-- forcing a re-upsert at the next scan.
--
-- Default of 0 means every existing row is treated as stale on the first
-- run after this migration; the scanner writes the current version on
-- successful upsert.
ALTER TABLE shows ADD COLUMN scan_version INTEGER NOT NULL DEFAULT 0;
ALTER TABLE media ADD COLUMN scan_version INTEGER NOT NULL DEFAULT 0;
