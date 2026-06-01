-- Build provenance for playback analytics + stale-client detection.
--
-- `BUILD_ID` is baked into the binary/bundle at compile time (the Docker build
-- derives it from `git rev-parse`; unset builds report an empty id). The server
-- stamps its own build on each session; the client reports the build its
-- frontend bundle was compiled with on each sample. A session whose samples
-- carry a `client_build_id` that differs from its `server_build_id` is a viewer
-- running a stale, cached frontend.
--
-- Motivating incident (2026-05-31): a viewer on a weeks-old cached player.js
-- saw only a loading spinner and posted zero telemetry samples, while peers on
-- the current build were fine. With these columns that's a one-query diagnosis.
ALTER TABLE playback_sessions ADD COLUMN server_build_id TEXT;
ALTER TABLE playback_samples  ADD COLUMN client_build_id TEXT;
