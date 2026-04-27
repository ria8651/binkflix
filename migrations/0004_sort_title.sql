-- Sort-friendly title (lowercased, leading article stripped) so library
-- listings can ORDER BY a column the scanner controls — the raw `title`
-- is too dirty when no NFO is present (`The.Matrix.1999.1080p...mkv`).
-- Backfilled from the current `title`; the next scan overwrites it with
-- the cleaned version produced by `filename::sort_title`.

ALTER TABLE shows ADD COLUMN sort_title TEXT NOT NULL DEFAULT '';
ALTER TABLE media ADD COLUMN sort_title TEXT NOT NULL DEFAULT '';

UPDATE shows SET sort_title = lower(title);
UPDATE media SET sort_title = lower(title);

DROP INDEX IF EXISTS idx_shows_title;
DROP INDEX IF EXISTS idx_media_title;

CREATE INDEX idx_shows_sort_title ON shows(sort_title);
CREATE INDEX idx_media_sort_title ON media(sort_title);
