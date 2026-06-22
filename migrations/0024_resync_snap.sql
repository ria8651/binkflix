-- Watch-party drift telemetry. The periodic server Resync (every 5s) can hard-
-- snap a viewer's <video> position when local drift exceeds the snap threshold
-- (`apply_remote` in syncplay_client.rs). That correction is applied locally and
-- deliberately *not* re-broadcast, so today it leaves no server-side trace — the
-- "one viewer jumps back-then-forward while everyone else stays smooth" glitch
-- can only be inferred from gaps between the 5s position samples.
--
-- These columns let the client report the snap directly, on the next sample:
--   resync_snaps   — count of Resync snaps applied since the previous sample
--                    (>1 in a single 5s window is the flapping/threshold-edge
--                    signature).
--   resync_snap_ms — signed delta of the last snap, in ms: positive = snapped
--                    forward (catch up), negative = snapped back. NULL when no
--                    snap occurred in the window.
-- Both nullable for back-compat (older clients never set them).
ALTER TABLE playback_samples ADD COLUMN resync_snaps   INTEGER;
ALTER TABLE playback_samples ADD COLUMN resync_snap_ms INTEGER;
