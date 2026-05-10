-- Analytics tables. Append-only history; queried directly with sqlite3 for now.

-- ---- scan_timings: per-asset-job stage breakdown -------------------------
-- Phase-2 of library_scan does ffprobe + subtitles + thumbnail + trickplay
-- + tech-info save per file. We record one row per job at completion. With
-- concurrency 4, scanned_at is when the *job* finished — rows from one scan
-- run share a range but are not strictly ordered.
CREATE TABLE scan_timings (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    media_id        TEXT    NOT NULL REFERENCES media(id) ON DELETE CASCADE,
    scanned_at      INTEGER NOT NULL,
    probe_ms        INTEGER NOT NULL,
    subtitles_ms    INTEGER NOT NULL,
    subtitle_tracks INTEGER NOT NULL,
    thumbnail_ms    INTEGER NOT NULL,
    trickplay_ms    INTEGER NOT NULL,
    save_ms         INTEGER NOT NULL,
    total_ms        INTEGER NOT NULL
);
CREATE INDEX idx_scan_timings_media       ON scan_timings(media_id);
CREATE INDEX idx_scan_timings_scanned_at  ON scan_timings(scanned_at);

-- ---- playback_sessions: one row per play attempt -------------------------
-- INSERTed when a stream request lands; ended_at is set when the streaming
-- task finishes. Snapshots codecs/container at session start so later
-- library churn doesn't invalidate the historical record.
--
-- delivery_mode mirrors BrowserCompat:
--   'direct'    — file served byte-range
--   'remux'     — ffmpeg `-c:v copy -c:a aac` to fMP4
--   'transcode' — full re-encode via the HLS pipeline
CREATE TABLE playback_sessions (
    id                   TEXT    PRIMARY KEY,
    user_sub             TEXT,
    media_id             TEXT    NOT NULL REFERENCES media(id) ON DELETE CASCADE,
    started_at           INTEGER NOT NULL,
    ended_at             INTEGER,
    duration_played_ms   INTEGER,
    delivery_mode        TEXT    NOT NULL,
    chosen_reason        TEXT,
    src_video_codec      TEXT,
    src_audio_codec      TEXT,
    src_container        TEXT,
    out_video_codec      TEXT,
    out_audio_codec      TEXT,
    out_container        TEXT,
    target_bitrate_kbps  INTEGER,
    browser              TEXT,
    room_id              TEXT,
    forced_via_query     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_pbs_user       ON playback_sessions(user_sub, started_at);
CREATE INDEX idx_pbs_media      ON playback_sessions(media_id, started_at);
CREATE INDEX idx_pbs_started_at ON playback_sessions(started_at);

-- ---- playback_samples: rolling client-side telemetry ---------------------
-- Posted by the player every ~10s. No FK on session_id — keep raw
-- observations even if the session row is pruned later.
CREATE TABLE playback_samples (
    id                       INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id               TEXT    NOT NULL,
    ts                       INTEGER NOT NULL,
    position_ms              INTEGER NOT NULL,
    buffered_ahead_ms        INTEGER,
    transcode_position_ms    INTEGER,
    transcode_rate_x100      INTEGER,
    observed_kbps            INTEGER,
    network_state            TEXT
);
CREATE INDEX idx_pbs_samples_session_ts ON playback_samples(session_id, ts);

-- ---- events: catch-all for low-volume signals (incl. watch parties) ------
-- `data` is JSON; query with `json_extract`. media_id is intentionally not a
-- FK so events outlive deletion of the underlying media.
CREATE TABLE events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          INTEGER NOT NULL,
    kind        TEXT    NOT NULL,
    user_sub    TEXT,
    media_id    TEXT,
    room_id     TEXT,
    data        TEXT    NOT NULL DEFAULT '{}'
);
CREATE INDEX idx_events_kind_ts  ON events(kind, ts);
CREATE INDEX idx_events_room_ts  ON events(room_id, ts);
CREATE INDEX idx_events_user_ts  ON events(user_sub, ts);
CREATE INDEX idx_events_media_ts ON events(media_id, ts);
