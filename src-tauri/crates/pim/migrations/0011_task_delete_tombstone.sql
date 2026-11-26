-- Suppress re-import of a task the user just deleted.
--
-- `task_delete` removes the local row before the queued `pim.task.delete`
-- reaches the server. A delta fetched in that window (or before the DELETE
-- completes) still names the object, so applying it would import the task
-- again. A row here blocks that import.
--
-- The identity is the same pair the sync paths use: the provider id, plus the
-- resource href for CardDAV/CalDAV-style providers (normalized with
-- `dav::href_key` when matched, so `%41.ics` and `A.ics` compare equal).
--
-- `suppress_until` bounds the block. The row is refreshed when the remote
-- DELETE succeeds, giving a stale batch one more window to land; it is removed
-- when the operation rolls back (the local task is restored) and pruned once
-- the window elapses, so a task that still exists server-side (for example
-- after a crash before the operation ran) is eventually imported again rather
-- than hidden forever.
CREATE TABLE IF NOT EXISTS pim_task_delete_tombstone (
    id             TEXT PRIMARY KEY,
    account_id     TEXT NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    task_list_id   TEXT NOT NULL REFERENCES task_list(id) ON DELETE CASCADE,
    remote_id      TEXT,
    href           TEXT,
    suppress_until TEXT NOT NULL,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pim_task_delete_tombstone_list
    ON pim_task_delete_tombstone(task_list_id, suppress_until);
