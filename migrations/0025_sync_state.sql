-- Watch-party sync state per sample, so drift and play-state problems are
-- queryable after the fact (companion to 0024's resync_snaps / resync_snap_ms,
-- which only capture the hard-seek corrections).
--
--   sync_drift_ms       — signed local−room drift at the last Resync, ms
--                         (+ ahead of the room, − behind). NULL when not in a
--                         party / before the first Resync.
--   playback_rate_x100  — <video>.playbackRate ×100 (100 = normal). Values off
--                         100 mean the client is gliding to correct drift.
--   room_playing        — the room's intended play state at sample time (1/0),
--                         NULL when not in a party.
--   paused              — the <video> element's own paused state (1/0).
--
-- A row with room_playing=1 & paused=1 (or vice-versa) is a play-state
-- desync — e.g. the "joined a paused room but my player started playing" case.
-- All nullable for back-compat.
ALTER TABLE playback_samples ADD COLUMN sync_drift_ms      INTEGER;
ALTER TABLE playback_samples ADD COLUMN playback_rate_x100 INTEGER;
ALTER TABLE playback_samples ADD COLUMN room_playing       INTEGER;
ALTER TABLE playback_samples ADD COLUMN paused             INTEGER;
