-- Which kind of reconciliation a row is waiting for. The previous build used a
-- single `awaiting_reconcile` flag for three different situations, so a sync
-- could infer "the server permanently deleted this message" from a state that
-- only meant "a move was rolled back". The reason and the owning operation are
-- stored explicitly:
--
-- * `pendingDelete`  - a permanent delete may have reached the server even
--                      though the command looked refused;
-- * `rebind`        - the folder-local identity is unknown and must be bound
--                      again from the server;
-- * `moveRollback`  - a failed move left the optimistic projection in its
--                      target folder; the op still owns it.
ALTER TABLE message ADD COLUMN reconcile_reason TEXT;

-- Id of the pending_op that created the state above (NULL for legacy rows).
ALTER TABLE message ADD COLUMN reconcile_op_id TEXT;

-- The pre-operation folder-local identity, kept separately from `uid` (which
-- is cleared so no UID-addressed action can hit the wrong message). A
-- reconciliation may only conclude anything when the live generation still
-- matches this generation and a `UID SEARCH` answers definitively.
ALTER TABLE message ADD COLUMN reconcile_uid INTEGER;
ALTER TABLE message ADD COLUMN reconcile_uidvalidity INTEGER;

-- Rows written by the previous build used one flag for every situation. The
-- safest reading is "the identity is unknown, rebind it": it never authorizes
-- an erasure and still shows the user that the row is waiting.
UPDATE message SET reconcile_reason = 'rebind'
 WHERE awaiting_reconcile = 1 AND reconcile_reason IS NULL;
