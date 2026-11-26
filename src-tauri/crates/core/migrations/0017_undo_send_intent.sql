-- Distinguish "not sent yet" from "outcome unknown" for queued operations.
--
-- `send_intent` is persisted *before* a wire command is issued. A crash (or a
-- retryable error) can therefore no longer make an operation look untouched:
-- recovery keeps `outcome_unknown` set, and undo refuses to cancel/restore it.
ALTER TABLE pending_op ADD COLUMN send_intent INTEGER NOT NULL DEFAULT 0;
ALTER TABLE pending_op ADD COLUMN outcome_unknown INTEGER NOT NULL DEFAULT 0;

-- An undo entry whose original operation rows could not be resolved. It must
-- never be interpreted as "this message never had a server operation".
ALTER TABLE undo_entry ADD COLUMN evidence_missing INTEGER NOT NULL DEFAULT 0;
