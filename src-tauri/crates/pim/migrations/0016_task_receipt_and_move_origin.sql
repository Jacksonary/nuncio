-- Persist the identity the provider last acknowledged for a task, separately
-- from the content/delta writer.
--
-- `pim_task_content_state.row_op_id` alone cannot answer "has another
-- operation already confirmed a *newer* identity?". A delta upsert attributes
-- its row write to the external writer (the empty-string owner) and overwrites
-- `row_op_id`; a save receipt that lands afterwards then had no evidence left
-- that its acknowledgement is the newest one, so it was dropped and the real
-- UID/ETag never reached the row.
--
-- `receipt_op_id`/`receipt_op_sequence` remember which operation confirmed
-- which identity (and its queue order), so a late receipt can be distinguished
-- from a genuinely newer confirmation even after a delta rewrote the owner.
ALTER TABLE pim_task_content_state ADD COLUMN receipt_op_id TEXT;
ALTER TABLE pim_task_content_state ADD COLUMN receipt_op_sequence INTEGER;
ALTER TABLE pim_task_content_state ADD COLUMN receipt_remote_id TEXT;
ALTER TABLE pim_task_content_state ADD COLUMN receipt_href TEXT;
ALTER TABLE pim_task_content_state ADD COLUMN receipt_uid_is_derived INTEGER;

CREATE INDEX IF NOT EXISTS idx_pim_task_content_state_receipt_owner
    ON pim_task_content_state(receipt_op_id);

-- Distinguish a move's "left this list" tombstone from a user delete.
--
-- A successful move A -> B records a settled tombstone for A. When the user
-- moves the task back B -> A, that old A tombstone still matches the same
-- resource (Google relinks the object and keeps its provider id), which hid
-- the returned task from deltas and let a full listing prune it. Recording the
-- origin lets a successful return remove only the stale "left here" markers;
-- pending DELETE tombstones (still owed to the provider) are never touched.
ALTER TABLE pim_task_delete_tombstone
    ADD COLUMN origin TEXT NOT NULL DEFAULT 'delete';

CREATE INDEX IF NOT EXISTS idx_pim_task_delete_tombstone_origin
    ON pim_task_delete_tombstone(task_list_id, origin, state);
