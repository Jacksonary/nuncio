-- Durable write sequence for PIM task operations.
--
-- `pending_op.seq` is allocated as `MAX(seq) + 1`. Housekeeping
-- (`prune_finished`) removes settled rows, so a later operation can receive a
-- number that an older operation already used. Save receipts persist the
-- sequence of the operation that confirmed an identity and compare it with the
-- current operation's sequence; after a prune, a reused number made a *past*
-- confirmation look like a *future* one. The receipt was then dropped, the
-- real UID/ETag never reached the task row, and a later delete suppressed only
-- the derived placeholder while the real object resurrected the task.
--
-- `pim_write_seq` is a single-row high-water mark owned by PIM, outside the
-- pending queue. Task operations take their `pending_op.seq` from it, so the
-- value used for receipt ordering never rewinds when the queue is pruned.
CREATE TABLE IF NOT EXISTS pim_write_seq (
    id    INTEGER PRIMARY KEY CHECK (id = 1),
    value INTEGER NOT NULL
);

INSERT OR IGNORE INTO pim_write_seq (id, value) VALUES (1, 0);

-- Continue above anything already durable: a queued operation's sequence, and
-- any receipt persisted by a previous build (whose operation row may already
-- have been pruned). Starting below a stored receipt would preserve the very
-- reuse this migration removes.
UPDATE pim_write_seq
   SET value = MAX(
        value,
        (SELECT COALESCE(MAX(seq), 0) FROM pending_op),
        (SELECT COALESCE(MAX(receipt_op_sequence), 0) FROM pim_task_content_state)
   );
