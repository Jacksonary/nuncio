-- Confirmed destination identity of an optimistic move, persisted on the
-- projection row itself.
--
-- The executor records a confirmed Graph move's destination id in the
-- `pending_op` payload. That row is housekeeping-pruned after seven days, and
-- a destination that has not appeared in a delta yet would then lose its only
-- identity: a stale update/removal carrying the projection's old id could bind
-- or delete it, and another copy sharing the Message-ID could consume it.
--
-- Writing the receipt onto the row makes the protection survive operation
-- pruning. `move_target_folder_id` is the local folder the confirmed id
-- belongs to; it is compared with `folder_id` so an identity left over from a
-- previous trip is never applied to a different projection. Both columns are
-- cleared as soon as the row is bound (`uid`/`gm_msgid` set,
-- `target_pending = 0`), so they only describe a waiting projection.
ALTER TABLE message ADD COLUMN move_target_remote_id TEXT;
ALTER TABLE message ADD COLUMN move_target_folder_id TEXT;
