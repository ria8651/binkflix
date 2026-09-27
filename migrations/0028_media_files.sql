-- Split the playable *item* (an episode or a movie) from the *file* that
-- currently backs it.
--
-- WHY: `media` used to be both at once, keyed by file path. Replacing a file
-- under a new name (a Sonarr/Radarr quality upgrade, a rename) minted a brand
-- new `media` row and soft-deleted the old one, so everything hung off the old
-- id — watch progress, rewatch/Continue Watching scope state, playback prefs,
-- analytics — was stranded on a row nobody could reach any more.
--
-- NOW:
--   media        the item. Identity is what it *is* — (show, season, episode)
--                for an episode, tmdb/imdb id or folder for a movie — and the
--                scanner re-attaches new files to an existing item by that
--                identity. Everything about the item as something you watch
--                keys on `media.id`, which is also what URLs carry.
--   media_files  one row per file on disk, keyed by path. Everything derived
--                from the bytes (probe, stream plan, subtitles, thumbnails,
--                trickplay, markers, fingerprints, scan timings, HLS cache
--                dirs) keys on `media_files.id`. An item plays from its
--                primary file (see server/files.rs).
--
-- Every existing file keeps its old media id as its file id, so derived rows,
-- on-disk HLS caches and item ids all carry over without rewriting a single
-- key. Duplicate items left behind by earlier replacements are merged by the
-- scanner's reconcile pass (scanner::reconcile_duplicates), not here, so the
-- merge rules live in exactly one place.
--
-- Several tables are rebuilt to move their foreign keys. That relies on
-- migrations running with foreign keys OFF (see `migrate` in server/db.rs):
-- with them on, `DROP TABLE media` would cascade-delete every child row,
-- watch history included.

-- ---- files ----------------------------------------------------------------

CREATE TABLE media_files (
    id                    TEXT PRIMARY KEY,
    media_id              TEXT NOT NULL REFERENCES media(id) ON DELETE CASCADE,
    library_id            INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    path                  TEXT NOT NULL UNIQUE,
    file_size             INTEGER NOT NULL,
    probe_json            TEXT,
    stream_plan_json      TEXT,
    source_mtime          INTEGER,
    source_size           INTEGER,
    content_mtime         INTEGER,
    content_size          INTEGER,
    subtitles_version     INTEGER NOT NULL DEFAULT 1,
    thumbnails_version    INTEGER NOT NULL DEFAULT 1,
    trickplay_version     INTEGER NOT NULL DEFAULT 1,
    markers_version       INTEGER NOT NULL DEFAULT 1,
    audio_markers_version INTEGER NOT NULL DEFAULT 1,
    -- File mtime when first indexed. The item's own `added_at` is when the
    -- *item* first appeared, which an upgrade deliberately doesn't bump.
    added_at              TEXT,
    scanned_at            TEXT NOT NULL DEFAULT (datetime('now')),
    deleted_at            TEXT
);

INSERT INTO media_files (
    id, media_id, library_id, path, file_size,
    probe_json, stream_plan_json, source_mtime, source_size,
    content_mtime, content_size,
    subtitles_version, thumbnails_version, trickplay_version,
    markers_version, audio_markers_version,
    added_at, scanned_at, deleted_at
)
SELECT id, id, library_id, path, file_size,
       probe_json, stream_plan_json, source_mtime, source_size,
       content_mtime, content_size,
       subtitles_version, thumbnails_version, trickplay_version,
       markers_version, audio_markers_version,
       added_at, scanned_at, deleted_at
  FROM media;

CREATE INDEX idx_media_files_media   ON media_files(media_id);
CREATE INDEX idx_media_files_library ON media_files(library_id);

-- ---- items ----------------------------------------------------------------

CREATE TABLE media_new (
    id              TEXT PRIMARY KEY,
    library_id      INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    kind            TEXT NOT NULL CHECK(kind IN ('movie', 'episode')),

    title           TEXT NOT NULL,
    sort_title      TEXT NOT NULL DEFAULT '',
    original_title  TEXT,
    year            INTEGER,
    plot            TEXT,
    runtime_minutes INTEGER,
    imdb_id         TEXT,
    tmdb_id         TEXT,

    -- Movies: portrait poster. Episodes: 16:9 thumb sidecar. Written from
    -- whichever file last re-indexed; the generated fallback thumbnail lives
    -- on the file instead (media_thumbnails).
    image_path      TEXT,
    fanart_path     TEXT,

    -- Episode identity. NULL for movies.
    show_id         TEXT REFERENCES shows(id) ON DELETE CASCADE,
    season_number   INTEGER,
    episode_number  INTEGER,

    rating          REAL,
    rating_votes    INTEGER,
    rating_source   TEXT,
    mpaa            TEXT,
    studio          TEXT,
    tagline         TEXT,
    release_date    TEXT,
    director        TEXT,
    writers         TEXT,

    scan_version    INTEGER NOT NULL DEFAULT 0,
    added_at        TEXT,
    scanned_at      TEXT NOT NULL DEFAULT (datetime('now')),
    -- Set once the item has no live file left.
    deleted_at      TEXT
);

INSERT INTO media_new (
    id, library_id, kind, title, sort_title, original_title, year, plot,
    runtime_minutes, imdb_id, tmdb_id, image_path, fanart_path,
    show_id, season_number, episode_number,
    rating, rating_votes, rating_source, mpaa, studio, tagline,
    release_date, director, writers,
    scan_version, added_at, scanned_at, deleted_at
)
SELECT id, library_id, kind, title, sort_title, original_title, year, plot,
       runtime_minutes, imdb_id, tmdb_id, image_path, fanart_path,
       show_id, season_number, episode_number,
       rating, rating_votes, rating_source, mpaa, studio, tagline,
       release_date, director, writers,
       scan_version, added_at, scanned_at, deleted_at
  FROM media;

-- Children that stay on the item (watch_progress, media_genres,
-- playback_sessions) reference `media` by name, so they re-bind to the new
-- table untouched.
DROP TABLE media;
ALTER TABLE media_new RENAME TO media;

CREATE INDEX idx_media_library    ON media(library_id);
CREATE INDEX idx_media_kind       ON media(kind);
CREATE INDEX idx_media_sort_title ON media(sort_title);
CREATE INDEX idx_media_added_at   ON media(added_at DESC);
CREATE INDEX idx_media_show_ep    ON media(show_id, season_number, episode_number)
    WHERE show_id IS NOT NULL;

-- ---- file-derived tables: media_id → file_id ------------------------------

CREATE TABLE subtitles_new (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id     TEXT NOT NULL REFERENCES media_files(id) ON DELETE CASCADE,
    track_id    TEXT NOT NULL,
    format      TEXT NOT NULL CHECK(format IN ('ass', 'vtt')),
    language    TEXT NOT NULL DEFAULT '',
    label       TEXT NOT NULL DEFAULT '',
    is_default  INTEGER NOT NULL DEFAULT 0,
    is_forced   INTEGER NOT NULL DEFAULT 0,
    content     BLOB NOT NULL,
    UNIQUE (file_id, track_id)
);
INSERT INTO subtitles_new (id, file_id, track_id, format, language, label, is_default, is_forced, content)
SELECT id, media_id, track_id, format, language, label, is_default, is_forced, content
  FROM subtitles;
DROP TABLE subtitles;
ALTER TABLE subtitles_new RENAME TO subtitles;
CREATE INDEX idx_subtitles_file ON subtitles(file_id);

CREATE TABLE media_thumbnails_new (
    file_id    TEXT PRIMARY KEY REFERENCES media_files(id) ON DELETE CASCADE,
    content    BLOB NOT NULL,
    mime       TEXT NOT NULL DEFAULT 'image/jpeg',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
INSERT INTO media_thumbnails_new (file_id, content, mime, created_at)
SELECT media_id, content, mime, created_at FROM media_thumbnails;
DROP TABLE media_thumbnails;
ALTER TABLE media_thumbnails_new RENAME TO media_thumbnails;

CREATE TABLE media_trickplay_new (
    file_id    TEXT PRIMARY KEY REFERENCES media_files(id) ON DELETE CASCADE,
    content    BLOB NOT NULL,
    mime       TEXT NOT NULL DEFAULT 'image/jpeg',
    interval_s INTEGER NOT NULL,
    tile_w     INTEGER NOT NULL,
    tile_h     INTEGER NOT NULL,
    cols       INTEGER NOT NULL,
    rows       INTEGER NOT NULL,
    count      INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    padding    INTEGER NOT NULL DEFAULT 0
);
INSERT INTO media_trickplay_new
    (file_id, content, mime, interval_s, tile_w, tile_h, cols, rows, count, created_at, padding)
SELECT media_id, content, mime, interval_s, tile_w, tile_h, cols, rows, count, created_at, padding
  FROM media_trickplay;
DROP TABLE media_trickplay;
ALTER TABLE media_trickplay_new RENAME TO media_trickplay;

CREATE TABLE media_markers_new (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id     TEXT NOT NULL REFERENCES media_files(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL CHECK(kind IN
                    ('intro','recap','outro','credits','chapter')),
    start_secs  REAL NOT NULL,
    end_secs    REAL NOT NULL,
    title       TEXT,
    source      TEXT NOT NULL CHECK(source IN
                    ('chapter','audio','silence','manual')),
    confidence  REAL NOT NULL DEFAULT 1.0,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (file_id, source, kind, start_secs)
);
INSERT INTO media_markers_new
    (id, file_id, kind, start_secs, end_secs, title, source, confidence, created_at)
SELECT id, media_id, kind, start_secs, end_secs, title, source, confidence, created_at
  FROM media_markers;
DROP TABLE media_markers;
ALTER TABLE media_markers_new RENAME TO media_markers;
CREATE INDEX idx_media_markers_file ON media_markers(file_id);

CREATE TABLE media_fingerprints_new (
    file_id         TEXT PRIMARY KEY REFERENCES media_files(id) ON DELETE CASCADE,
    content_mtime   INTEGER,
    content_size    INTEGER,
    fp_algo_version INTEGER NOT NULL,
    raw             BLOB NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (datetime('now'))
);
INSERT INTO media_fingerprints_new
    (file_id, content_mtime, content_size, fp_algo_version, raw, created_at)
SELECT media_id, content_mtime, content_size, fp_algo_version, raw, created_at
  FROM media_fingerprints;
DROP TABLE media_fingerprints;
ALTER TABLE media_fingerprints_new RENAME TO media_fingerprints;

CREATE TABLE scan_timings_new (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id         TEXT    NOT NULL REFERENCES media_files(id) ON DELETE CASCADE,
    scanned_at      INTEGER NOT NULL,
    probe_ms        INTEGER NOT NULL,
    subtitles_ms    INTEGER NOT NULL,
    subtitle_tracks INTEGER NOT NULL,
    thumbnail_ms    INTEGER NOT NULL,
    trickplay_ms    INTEGER NOT NULL,
    save_ms         INTEGER NOT NULL,
    total_ms        INTEGER NOT NULL,
    video_codec     TEXT,
    audio_codec     TEXT,
    container       TEXT,
    width           INTEGER,
    height          INTEGER,
    duration_ms     INTEGER,
    bitrate_kbps    INTEGER,
    pixel_format    TEXT,
    keyframe_count  INTEGER,
    trigger         TEXT NOT NULL DEFAULT 'scan'
);
INSERT INTO scan_timings_new (
    id, file_id, scanned_at, probe_ms, subtitles_ms, subtitle_tracks,
    thumbnail_ms, trickplay_ms, save_ms, total_ms,
    video_codec, audio_codec, container, width, height, duration_ms,
    bitrate_kbps, pixel_format, keyframe_count, trigger
)
SELECT id, media_id, scanned_at, probe_ms, subtitles_ms, subtitle_tracks,
       thumbnail_ms, trickplay_ms, save_ms, total_ms,
       video_codec, audio_codec, container, width, height, duration_ms,
       bitrate_kbps, pixel_format, keyframe_count, trigger
  FROM scan_timings;
DROP TABLE scan_timings;
ALTER TABLE scan_timings_new RENAME TO scan_timings;
CREATE INDEX idx_scan_timings_file       ON scan_timings(file_id);
CREATE INDEX idx_scan_timings_scanned_at ON scan_timings(scanned_at);

-- ---- playback sessions: keep the item, also record the file ---------------

-- `media_id` stays the item, so a session is still "what did they watch".
-- `file_id` says which bytes were served; SET NULL rather than CASCADE so
-- purging an old file doesn't erase the history of having played it.
ALTER TABLE playback_sessions
    ADD COLUMN file_id TEXT REFERENCES media_files(id) ON DELETE SET NULL;
UPDATE playback_sessions SET file_id = media_id;
