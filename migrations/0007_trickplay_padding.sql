-- Inter-tile padding (in pixels) baked into the sprite at generation
-- time. Stored so the client can compute background-position with the
-- exact stride that ffmpeg used. Default 0 keeps existing pre-padding
-- rows rendering correctly until they're regenerated.
ALTER TABLE media_trickplay ADD COLUMN padding INTEGER NOT NULL DEFAULT 0;
