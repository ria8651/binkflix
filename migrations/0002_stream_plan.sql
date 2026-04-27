-- Plan-driven HLS pipeline: cached segment timeline + invalidation keys.
--
-- `stream_plan_json` holds a `StreamPlan` (segment boundaries, mode, codecs)
-- built from a ffprobe keyframe pass. Kept separate from `tech_json` because
-- (a) it's an internal streaming artifact the debug UI never sees, and
-- (b) it can be invalidated independently when the plan-builder algorithm
--     bumps its version.
--
-- `source_mtime` (unix seconds) + `source_size` (bytes) are the staleness
-- check: if either changes vs the source file on disk, the cached plan and
-- on-disk segment cache are wiped and rebuilt on the next playlist request.
ALTER TABLE media ADD COLUMN stream_plan_json TEXT;
ALTER TABLE media ADD COLUMN source_mtime    INTEGER;
ALTER TABLE media ADD COLUMN source_size     INTEGER;
