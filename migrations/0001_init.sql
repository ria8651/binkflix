PRAGMA foreign_keys = ON;

CREATE TABLE libraries (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT NOT NULL,
    path       TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- A TV show — a folder with episodes inside. Movies do NOT live here; only
-- show-level metadata that episodes roll up to.
CREATE TABLE shows (
    id             TEXT PRIMARY KEY,
    library_id     INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    path           TEXT NOT NULL UNIQUE,
    title          TEXT NOT NULL,
    original_title TEXT,
    year           INTEGER,
    plot           TEXT,
    imdb_id        TEXT,
    tmdb_id        TEXT,
    tvdb_id        TEXT,
    poster_path    TEXT,
    fanart_path    TEXT,
    scanned_at     TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_shows_library ON shows(library_id);
CREATE INDEX idx_shows_title   ON shows(title);

-- Every playable video file, movie or episode.
-- Episode-specific columns (show_id, season_number, episode_number) are NULL
-- for movies. A reclassification (movie ↔ episode for the same path) is a
-- single INSERT ... ON CONFLICT(path) DO UPDATE — no cross-table dance.
CREATE TABLE media (
    id              TEXT PRIMARY KEY,
    library_id      INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    kind            TEXT NOT NULL CHECK(kind IN ('movie', 'episode')),
    path            TEXT NOT NULL UNIQUE,
    file_size       INTEGER NOT NULL,

    title           TEXT NOT NULL,
    original_title  TEXT,
    year            INTEGER,
    plot            TEXT,
    runtime_minutes INTEGER,
    imdb_id         TEXT,
    tmdb_id         TEXT,

    -- Movies: portrait poster. Episodes: 16:9 thumb (still from the video).
    image_path      TEXT,
    fanart_path     TEXT,

    -- Episode linkage. NULL for movies.
    show_id         TEXT REFERENCES shows(id) ON DELETE CASCADE,
    season_number   INTEGER,
    episode_number  INTEGER,

    scanned_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_media_library  ON media(library_id);
CREATE INDEX idx_media_kind     ON media(kind);
CREATE INDEX idx_media_title    ON media(title);
CREATE INDEX idx_media_show_ep  ON media(show_id, season_number, episode_number)
    WHERE show_id IS NOT NULL;

-- Genres are denormalized (no genre table) — you rarely query "all media with
-- genre X" right now, and strings are cheap. Easy to normalize later.
CREATE TABLE media_genres (
    media_id TEXT NOT NULL REFERENCES media(id) ON DELETE CASCADE,
    genre    TEXT NOT NULL,
    PRIMARY KEY (media_id, genre)
);

CREATE TABLE show_genres (
    show_id TEXT NOT NULL REFERENCES shows(id) ON DELETE CASCADE,
    genre   TEXT NOT NULL,
    PRIMARY KEY (show_id, genre)
);
