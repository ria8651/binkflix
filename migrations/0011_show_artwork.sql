-- Extra sidecar artwork for shows. `clearlogo` is a transparent-PNG title-card
-- (Kodi convention) that themes can render in place of the show <h1>; `banner`
-- is a wide/short marketing image. NULL on existing rows until the scanner
-- next runs over the show folder.
ALTER TABLE shows ADD COLUMN clearlogo_path TEXT;
ALTER TABLE shows ADD COLUMN banner_path TEXT;
