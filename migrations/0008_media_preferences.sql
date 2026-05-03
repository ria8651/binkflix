-- Sticky per-user playback prefs (audio track, subtitle track, transcode
-- mode, bitrate). Keyed by an opaque `scope_key` string that the client
-- builds: `show:<id>` for episodes (so prefs carry across episodes of
-- one series), `media:<id>` for movies. No FK because the target table
-- depends on the prefix; cleanup on delete is acceptable as orphaned rows
-- (a small text table; user-driven sweeps are fine if needed later).
CREATE TABLE media_preferences (
    user_sub        TEXT NOT NULL,
    scope_key       TEXT NOT NULL,
    subtitle_id     TEXT,
    subtitle_lang   TEXT,
    audio_idx       INTEGER,
    audio_lang      TEXT,
    audio_codec     TEXT,
    transcode_mode  TEXT,
    bitrate_kbps    INTEGER,
    updated_at      INTEGER NOT NULL,
    PRIMARY KEY (user_sub, scope_key)
);
