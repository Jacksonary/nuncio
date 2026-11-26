-- Durable identity of the server draft a local draft supersedes, pinned when
-- the draft is opened or an APPEND receipt binds it. Later saves must not
-- re-read a UID from the rebindable Drafts projection.
ALTER TABLE draft ADD COLUMN remote_draft_json TEXT;

-- Persistent cleanup decision for the source draft's remote copy, captured
-- when the message is queued. SMTP post-send processing consumes this decision
-- instead of enqueueing a delete from the draft's projection id.
ALTER TABLE outbox ADD COLUMN draft_cleanup TEXT NOT NULL DEFAULT 'none';
