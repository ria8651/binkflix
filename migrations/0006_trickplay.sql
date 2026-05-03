-- Trickplay sprite sheets: a grid of frames sampled at fixed intervals
-- across the full duration, packed into one JPEG. The player uses
-- background-position to draw the right tile when the user hovers the
-- scrub bar. Stored alongside `media_thumbnails` so re-scans are
-- self-contained — no extra files on disk.
CREATE TABLE media_trickplay (
    media_id   TEXT PRIMARY KEY REFERENCES media(id) ON DELETE CASCADE,
    content    BLOB NOT NULL,
    mime       TEXT NOT NULL DEFAULT 'image/jpeg',
    interval_s INTEGER NOT NULL,
    tile_w     INTEGER NOT NULL,
    tile_h     INTEGER NOT NULL,
    cols       INTEGER NOT NULL,
    rows       INTEGER NOT NULL,
    count      INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
