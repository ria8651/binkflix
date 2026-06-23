-- Unified playback markers (intro/recap/outro/credits/chapter) and the
-- per-file Chromaprint cache that backs cross-episode audio detection.
--
-- DESIGN: one marker store, many producers. Embedded chapters (cheap, per-
-- file), audio-fingerprint matching (per-season), an optional silence/black
-- refinement pass, and manual edits all write into `media_markers`; the
-- player is the single consumer (scrub-bar ticks + skip button). Markers are
-- per-media — a shared intro that only some episodes carry produces rows
-- only on the episodes that actually contain it.

PRAGMA foreign_keys = ON;

CREATE TABLE media_markers (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    media_id    TEXT NOT NULL REFERENCES media(id) ON DELETE CASCADE,
    -- Position-classified kind. 'chapter' is the generic recurring/mid-runtime
    -- segment (e.g. a character-info interstitial with a signature jingle).
    kind        TEXT NOT NULL CHECK(kind IN
                    ('intro','recap','outro','credits','chapter')),
    start_secs  REAL NOT NULL,
    end_secs    REAL NOT NULL,
    title       TEXT,
    -- Producer that wrote this row. Lets a re-run delete + replace only its
    -- own markers without clobbering a manual edit or a chapter-derived one.
    source      TEXT NOT NULL CHECK(source IN
                    ('chapter','audio','silence','manual')),
    confidence  REAL NOT NULL DEFAULT 1.0,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    -- Dedup guard: one marker per (media, source, kind, start).
    UNIQUE (media_id, source, kind, start_secs)
);
CREATE INDEX idx_media_markers_media ON media_markers(media_id);

-- Per-file Chromaprint cache so a season re-analysis (or adding one new
-- episode) re-fingerprints only the files that actually changed. Keyed by the
-- SAME content signature the rest of the scanner uses (media.content_mtime/
-- content_size, added in 0021); a file swap invalidates it exactly like
-- probe_json does.
--
-- `raw` is a flat little-endian u32 array — the whole-episode Chromaprint
-- "raw" sub-fingerprints (`fpcalc -raw`). Sub-fingerprint i covers media time
-- ≈ i * FP_ITEM_SECS (see markers.rs); Chromaprint truncates a couple of
-- seconds before the true end. Whole-file (not windowed) so a recurring
-- mid-episode segment is detectable, not just head/tail intros. `fpcalc`'s
-- `-chunk` mode is unused — it errors on current builds.
CREATE TABLE media_fingerprints (
    media_id        TEXT PRIMARY KEY REFERENCES media(id) ON DELETE CASCADE,
    content_mtime   INTEGER,
    content_size    INTEGER,
    -- Bump (FP_ALGO_VERSION in markers.rs) to force a re-fingerprint when the
    -- fpcalc invocation / item-period assumption changes.
    fp_algo_version INTEGER NOT NULL,
    raw             BLOB NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Re-analysis gating, mirroring the per-asset *_version columns (0017).
-- `markers_version` gates the per-file CHAPTER pass; `audio_markers_version`
-- gates the per-season audio-matching pass. DEFAULT matches the initial
-- constant in scanner.rs so existing rows are treated up-to-date (the 0017
-- convention; contrast 0013 which defaulted 0 to invalidate every row).
ALTER TABLE media ADD COLUMN markers_version       INTEGER NOT NULL DEFAULT 1;
ALTER TABLE media ADD COLUMN audio_markers_version INTEGER NOT NULL DEFAULT 1;
