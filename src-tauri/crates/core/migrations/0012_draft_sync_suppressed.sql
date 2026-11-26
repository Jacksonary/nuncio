-- The user explicitly chose to keep a draft local after its remote copy could
-- not be confirmed. Saves stop queueing uploads; no remote cleanup is retried
-- and nothing is deleted automatically.
ALTER TABLE draft ADD COLUMN sync_suppressed INTEGER NOT NULL DEFAULT 0;
