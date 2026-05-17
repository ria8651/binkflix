PRAGMA foreign_keys = ON;

ALTER TABLE shows ADD COLUMN rating         REAL;
ALTER TABLE shows ADD COLUMN rating_votes   INTEGER;
ALTER TABLE shows ADD COLUMN rating_source  TEXT;
ALTER TABLE shows ADD COLUMN mpaa           TEXT;
ALTER TABLE shows ADD COLUMN studio         TEXT;
ALTER TABLE shows ADD COLUMN premiered_date TEXT;
ALTER TABLE shows ADD COLUMN end_date       TEXT;
ALTER TABLE shows ADD COLUMN status         TEXT;

ALTER TABLE media ADD COLUMN rating         REAL;
ALTER TABLE media ADD COLUMN rating_votes   INTEGER;
ALTER TABLE media ADD COLUMN rating_source  TEXT;
ALTER TABLE media ADD COLUMN mpaa           TEXT;
ALTER TABLE media ADD COLUMN studio         TEXT;
ALTER TABLE media ADD COLUMN tagline        TEXT;
ALTER TABLE media ADD COLUMN release_date   TEXT;
ALTER TABLE media ADD COLUMN director       TEXT;
ALTER TABLE media ADD COLUMN writers        TEXT;
