-- Per-user app settings. One row per user. Theme is the only column today,
-- but future user-level prefs (default audio language, autoplay toggle, …)
-- should land here as additional NULL-able columns rather than a new table.
-- No FK to `users` — matches the convention in 0008_media_preferences.sql.
CREATE TABLE user_settings (
    user_sub   TEXT PRIMARY KEY,
    theme      TEXT,
    updated_at INTEGER NOT NULL
);
