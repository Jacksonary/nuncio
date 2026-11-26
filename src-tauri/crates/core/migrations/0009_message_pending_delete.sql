-- A row hidden by an optimistic permanent delete that the server has not
-- confirmed yet. Unlike the old behaviour, the full row (attachments and FTS
-- documents included) stays in place so a refused command can undo the hide
-- instead of rebuilding the message from a snapshot or a window-limited sync.
--
-- Folder re-syncs must leave these rows alone: the remote delete has not been
-- acknowledged, so neither a UIDVALIDITY reset nor the expunge/prune pass may
-- drop them. The flag is cleared when the operation succeeds (the row is then
-- deleted for real) or when it fails (the row is unhidden).
ALTER TABLE message ADD COLUMN pending_delete INTEGER NOT NULL DEFAULT 0;
