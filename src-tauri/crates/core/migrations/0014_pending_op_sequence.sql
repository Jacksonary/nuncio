-- Explicit insertion order for the operation queue.
--
-- Ordering used to fall back to SQLite's implicit `rowid`. That is stable in
-- practice but is an implementation detail: a table rebuild (or a VACUUM that
-- renumbers pages, depending on the rebuild strategy) may not preserve it, and
-- APPEND/DELETE pairs as well as "newest attribution" scans depend on the exact
-- enqueue order. Backfill from `rowid` so existing queues keep their order.
ALTER TABLE pending_op ADD COLUMN seq INTEGER;

UPDATE pending_op SET seq = rowid WHERE seq IS NULL;

CREATE INDEX IF NOT EXISTS idx_pending_op_seq ON pending_op(account_id, seq);
CREATE INDEX IF NOT EXISTS idx_pending_op_due_seq
    ON pending_op(state, next_retry_at, seq);
