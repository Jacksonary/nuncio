-- Ownership of the confirmed destination identity persisted on a projection
-- row (migration 0023).
--
-- A row can make several trips into the same folder over its lifetime. The
-- destination id recorded by an earlier trip must not overwrite, or stand in
-- for, the identity of a newer one: an `@removed` carrying the old copy would
-- otherwise delete the new projection. Recording the operation that wrote the
-- identity — and its queue sequence, which is the explicit trip order — lets
-- the writer and every reader decide whether a receipt still owns the row.
--
-- `move_target_op_id` is the `pending_op.id` that claimed the identity;
-- `move_target_seq` is that operation's `seq` (NULL for claims made before
-- this migration). Both are cleared together with
-- `move_target_remote_id`/`move_target_folder_id` whenever the row is bound or
-- changes folder.
ALTER TABLE message ADD COLUMN move_target_op_id TEXT;
ALTER TABLE message ADD COLUMN move_target_seq INTEGER;
