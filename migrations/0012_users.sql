-- Per-user identity table. The auth layer upserts into this on every
-- authenticated request so we have a durable mapping from the bastion JWT
-- `sub` claim back to a human-readable login. user_sub appears denormalised
-- across watch_progress, media_preferences, playback_sessions, and events;
-- if bastion ever rebuilds with a different sub derivation, this table is
-- what an admin uses to figure out who-was-who and re-key those rows.
CREATE TABLE users (
    user_sub   TEXT PRIMARY KEY,
    login      TEXT NOT NULL,
    first_seen INTEGER NOT NULL,
    last_seen  INTEGER NOT NULL
);
