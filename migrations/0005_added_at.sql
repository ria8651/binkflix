-- Track when a show / media row was first added so the home page can show a
-- "Recently Added" row. NULL on existing rows; the scanner backfills from file
-- mtime on next start.
ALTER TABLE shows ADD COLUMN added_at TEXT;
ALTER TABLE media ADD COLUMN added_at TEXT;

CREATE INDEX idx_shows_added_at ON shows(added_at DESC);
CREATE INDEX idx_media_added_at ON media(added_at DESC);
