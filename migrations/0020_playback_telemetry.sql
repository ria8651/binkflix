-- Watch-party / HLS telemetry. HLS playback previously created no
-- playback_sessions rows at all (only the legacy /stream path did), and
-- room_id was never populated. These columns let server-driven HLS
-- sessions record the audio track and (derived from the syncplay hub)
-- which room a viewer was in, and let the optional client metrics stream
-- carry per-viewer decode/error signals. All nullable for back-compat.

ALTER TABLE playback_sessions ADD COLUMN audio_idx INTEGER;

CREATE INDEX idx_pbs_room ON playback_sessions(room_id, started_at);

ALTER TABLE playback_samples ADD COLUMN audio_idx      INTEGER;
ALTER TABLE playback_samples ADD COLUMN dropped_frames INTEGER;
ALTER TABLE playback_samples ADD COLUMN decoded_frames INTEGER;
ALTER TABLE playback_samples ADD COLUMN player_error   TEXT;
