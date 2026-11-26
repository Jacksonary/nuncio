-- Last writer per message flag field.
--
-- An undo or a failed operation may only restore a flag while it still owns
-- that field. Comparing the current value is not enough: "read -> unread ->
-- read" leaves the value equal to the first action's write, and a later action
-- that has not reached the server yet is still the user's newest intent.
--
-- Every local flag change (optimistic action, inverse compensation, mark-read
-- on open) records its writer here, one row per (message, field). The executor
-- re-checks ownership inside the send claim and the rollback transaction, and
-- local/POP3 undo uses the same rows without a server operation.
CREATE TABLE IF NOT EXISTS message_flag_writer (
    message_id TEXT NOT NULL,
    field TEXT NOT NULL,
    -- `pending_op.id` for a queued server command, or the undo action token
    -- for a local/POP3 change that has no remote operation.
    owner_id TEXT NOT NULL,
    -- Latest value this owner wrote.
    value INTEGER,
    -- Monotonic write order across every message. Ties are not expected: all
    -- writes run under the database write lock.
    seq INTEGER NOT NULL,
    action TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (message_id, field)
);

CREATE INDEX IF NOT EXISTS idx_message_flag_writer_owner
    ON message_flag_writer(owner_id);
