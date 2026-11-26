-- Persist the identity source of a stored VTODO/task row and the lifecycle of
-- its delete tombstone.
--
-- A CalDAV VTODO without a UID is identified by a placeholder derived from its
-- href (`A.ics` -> `A`). That placeholder is *not* a real UID: another resource
-- may legitimately carry the real UID `A`. Without this column the delta upsert
-- and `find_task` compared only the string values and merged the two resources
-- into one row.
--
-- Default 0 keeps every pre-existing row a real UID (the overwhelmingly common
-- case); derived identities are written explicitly by the sync paths from now
-- on. A historical row whose identity really was href-derived stays "real"
-- until the next provider delivery overwrites the flag; treating it as real can
-- retain a stale row but never deletes a live one.
ALTER TABLE task ADD COLUMN uid_is_derived INTEGER NOT NULL DEFAULT 0;

-- Tombstone identity source, compared before the value exactly like the task
-- row: a real UID tombstone must not suppress a derived identity that happens
-- to spell the same string, and vice versa.
ALTER TABLE pim_task_delete_tombstone
    ADD COLUMN uid_is_derived INTEGER NOT NULL DEFAULT 0;

-- A tombstone now has two states:
--   * `pending`  - the queued DELETE has not reached the server yet. It
--                  protects the object indefinitely;
--   * `settled`  - the remote DELETE succeeded. `suppress_until` (set at
--                  settlement time) bounds how long it keeps suppressing a
--                  delta that was already in flight.
--
-- Pre-existing rows were written with a wall-clock TTL at delete time, so they
-- default to `settled` and keep their original expiry semantics.
ALTER TABLE pim_task_delete_tombstone
    ADD COLUMN state TEXT NOT NULL DEFAULT 'settled';

CREATE INDEX IF NOT EXISTS idx_pim_task_delete_tombstone_state
    ON pim_task_delete_tombstone(task_list_id, state, suppress_until);
