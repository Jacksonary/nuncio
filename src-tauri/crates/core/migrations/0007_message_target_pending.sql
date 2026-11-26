-- A row whose optimistic move has not been confirmed by the server yet: its
-- `uid`/`uidvalidity` were cleared because UIDs are folder-local, and the row
-- is waiting for the target folder's next sync to give it the new identity.
--
-- This is an explicit state instead of "uid IS NULL", because a NULL UID is
-- also normal for Graph accounts (no IMAP UIDs) and for drafts that were never
-- uploaded. Only rows marked here may be refused by actions that need a
-- server address.
ALTER TABLE message ADD COLUMN target_pending INTEGER NOT NULL DEFAULT 0;
