-- Track when a send actually started so an abandoned `sending` row can be
-- distinguished from one that is still transmitting. Without this the recovery
-- pass compared against `created_at`, which may be hours old for an item that
-- waited behind an offline queue.
ALTER TABLE outbox ADD COLUMN sending_started_at TEXT;
