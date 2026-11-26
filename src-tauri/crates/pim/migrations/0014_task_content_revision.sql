-- Persist who last wrote a task's content, so a permanently failed operation
-- can decide whether its snapshot still owns the row.
--
-- The old guard lived in the pending-queue only: it scanned for a newer
-- *queued* operation and compared second-precision `updated_at` values. Once a
-- newer save succeeded and its `done` row was pruned, all evidence of that
-- write disappeared; a same-second timestamp then let an older failed save
-- restore its snapshot over the confirmed content.
--
-- The PIM layer owns this state (the `task` table itself belongs to core, and
-- core-only builds must keep working without the PIM migrations):
--   * `content_rev` / `content_op_id` are bumped only by writes that change
--     the fields a save rollback restores (title, notes, dates, status, steps,
--     ...). A save may restore its snapshot only while `content_op_id` is
--     still its own operation id.
--   * `row_rev` / `row_op_id` are bumped by every operation row write,
--     including a move and a save receipt (identity/ETag, not content), and
--     are used to keep a failed move from undoing a newer write.
--
-- The state survives `prune_finished` because it is not part of the queue. A
-- row with no state (written before this migration) has an unknown writer, so
-- an operation must not claim it. An empty-string owner marks a writer that is
-- not a pending operation: a delta upsert, or a local edit for a local-only
-- account. `ON DELETE CASCADE` drops the evidence with its task.
CREATE TABLE IF NOT EXISTS pim_task_content_state (
    task_id       TEXT PRIMARY KEY REFERENCES task(id) ON DELETE CASCADE,
    content_rev   INTEGER NOT NULL DEFAULT 0,
    content_op_id TEXT,
    row_rev       INTEGER NOT NULL DEFAULT 0,
    row_op_id     TEXT,
    updated_at    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pim_task_content_state_content_owner
    ON pim_task_content_state(content_op_id);

CREATE INDEX IF NOT EXISTS idx_pim_task_content_state_row_owner
    ON pim_task_content_state(row_op_id);
