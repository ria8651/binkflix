ALTER TABLE libraries ADD COLUMN deleted_at TEXT;
ALTER TABLE shows     ADD COLUMN deleted_at TEXT;
ALTER TABLE media     ADD COLUMN deleted_at TEXT;
