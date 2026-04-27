CREATE TABLE watch_progress (
    user_sub      TEXT NOT NULL,
    media_id      TEXT NOT NULL REFERENCES media(id) ON DELETE CASCADE,
    position_secs REAL NOT NULL,
    duration_secs REAL NOT NULL,
    completed     INTEGER NOT NULL DEFAULT 0,
    updated_at    INTEGER NOT NULL,
    PRIMARY KEY (user_sub, media_id)
);

CREATE INDEX idx_watch_progress_user_recent
    ON watch_progress(user_sub, updated_at DESC);
