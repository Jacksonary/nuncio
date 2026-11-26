-- A task delete that raced an in-flight save.
--
-- `task_delete` removes the local row before the queued DELETE reaches the
-- server. A `pim.task.save` that is already on the wire can still commit a
-- remote object (a create) or upgrade a derived identity to a real UID (a
-- rewrite). Its receipt must not be written to the deleted row (the foreign
-- key to `task` would fail) and must not be dropped either, or the object
-- stays on the server and a later delta re-imports the task the user deleted.
--
-- The intent survives the delete transaction and is addressed by the save's
-- receipt: `remote_id`/`href`/`etag` are backfilled when the provider
-- acknowledges the write, and a DELETE op is then pointed at that identity.
-- `owner_op_id`/`owner_attempt` bind the cleanup to the operation that may
-- have produced the object; an operation that provably never sent (no
-- `send_intent`/`outcome_unknown`) cannot have created anything and removes an
-- identity-less intent instead.
CREATE TABLE IF NOT EXISTS pim_task_delete_intent (
    task_id         TEXT PRIMARY KEY,
    account_id      TEXT NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    task_list_id    TEXT NOT NULL REFERENCES task_list(id) ON DELETE CASCADE,
    remote_id       TEXT,
    href            TEXT,
    etag            TEXT,
    uid_is_derived  INTEGER NOT NULL DEFAULT 0,
    owner_op_id     TEXT,
    owner_attempt   INTEGER,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pim_task_delete_intent_account
    ON pim_task_delete_intent(account_id);

CREATE INDEX IF NOT EXISTS idx_pim_task_delete_intent_list
    ON pim_task_delete_intent(task_list_id);
