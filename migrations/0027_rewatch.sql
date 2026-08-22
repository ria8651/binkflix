-- Rewatch passes, and per-scope "Continue Watching" state.
--
-- DESIGN: a rewatch is *declared*, not inferred. Pressing "Rewatch" stamps
-- `started_at`; every per-episode question is then a timestamp comparison
-- against it, which is deliberately cheaper than inferring passes from playback
-- behaviour. Replaying the credits of an old episode, scrubbing back past the
-- completion threshold, or watching out of order can't fabricate a pass,
-- because only the button writes `started_at`. The granular per-play record
-- already lives in `playback_sessions` for anyone who wants it.
--
--   done this pass    = watch_progress.last_completed_at >= started_at
--   touched this pass = watch_progress.updated_at        >= started_at
--
-- `started_at = 0` means "not rewatching", and both comparisons then degrade to
-- exactly the pre-rewatch behaviour — which is why ending a pass zeroes the
-- column instead of deleting the row, and why nothing has to special-case the
-- absence of a pass.
--
-- One row per *scope*: `show:<id>` for a series (a rewatch spans every episode)
-- and `media:<id>` for a movie, mirroring `media_preferences`. No FK, since the
-- target table depends on the prefix.
--
-- `hidden_at` replaces the old per-episode `watch_progress.dismissed`. That
-- flag was already a whole-show hide in practice — Continue Watching shows one
-- tile per show, and only the most recently touched row was ever consulted —
-- but storing it on a row that can stop *being* that row meant it leaked:
-- marking a different episode watched made the show reappear, a rename dropped
-- the flag with the row, and a hidden rewatch pointing at an episode with no
-- row yet had nothing to flag at all. Per scope, hiding means what it says.
--
-- It's a timestamp rather than a flag so that clearing needs no write: a hide
-- holds only until something newer happens in the scope, so a progress report,
-- a mark-watched, or starting a rewatch all bring the tile back on their own
-- (see `scope_hidden` in server/watch.rs).
CREATE TABLE watch_scope_state (
    user_sub   TEXT    NOT NULL,
    scope_key  TEXT    NOT NULL,
    -- 2 = first rewatch. Not rendered anywhere yet; kept so "3rd time through"
    -- is available later without a schema change.
    pass_no    INTEGER NOT NULL DEFAULT 1,
    started_at INTEGER NOT NULL DEFAULT 0,
    hidden_at  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_sub, scope_key)
);

-- When the user last *finished* this item, ever. Distinct from `updated_at`
-- (any heartbeat) and from `completed`, which as of this migration is sticky: a
-- progress report only ever sets it, never clears it. Without that, ten seconds
-- into a rewatch the "you've seen this" bit was gone for good.
ALTER TABLE watch_progress ADD COLUMN last_completed_at INTEGER;

-- Best available backfill: for already-complete rows the last heartbeat is when
-- they were finished (± the length of the tail the user watched).
UPDATE watch_progress SET last_completed_at = updated_at WHERE completed = 1;

-- Carry existing dismissals over to scope state. `dismiss` used to stamp
-- `updated_at = now` on the row it hid (so it stayed the row Continue Watching
-- consulted), which makes that timestamp the right `hidden_at`: it's exactly
-- the point after which nothing had happened in the scope.
INSERT INTO watch_scope_state (user_sub, scope_key, hidden_at)
SELECT wp.user_sub,
       CASE WHEN m.kind = 'episode' AND m.show_id IS NOT NULL
            THEN 'show:' || m.show_id
            ELSE 'media:' || m.id END,
       MAX(wp.updated_at)
  FROM watch_progress wp
  JOIN media m ON m.id = wp.media_id
 WHERE wp.dismissed = 1
 GROUP BY 1, 2;

ALTER TABLE watch_progress DROP COLUMN dismissed;
