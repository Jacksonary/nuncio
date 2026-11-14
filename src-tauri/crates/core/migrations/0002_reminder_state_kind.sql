-- Generalise per-occurrence reminder dismiss/snooze state so task reminders can
-- use the same persistence as calendar events.
--
-- The old table keyed rows by the `event` foreign key, which cannot represent a
-- task.  Rows are copied into the new `reminder_state` table (kind = 'event')
-- and the old table is removed.  Triggers keep cleanup working even though the
-- new table is polymorphic and therefore cannot carry a foreign key.

CREATE TABLE IF NOT EXISTS reminder_state (
    kind           TEXT NOT NULL,
    entity_id      TEXT NOT NULL,
    instance_start TEXT NOT NULL,
    dismissed_at   TEXT,
    snoozed_until  TEXT,
    updated_at     TEXT NOT NULL,
    PRIMARY KEY (kind, entity_id, instance_start)
);

INSERT OR IGNORE INTO reminder_state
    (kind, entity_id, instance_start, dismissed_at, snoozed_until, updated_at)
SELECT 'event', event_id, instance_start, dismissed_at, snoozed_until, updated_at
FROM event_reminder_state;

DROP TABLE IF EXISTS event_reminder_state;

CREATE INDEX IF NOT EXISTS idx_reminder_state_snooze
    ON reminder_state(kind, snoozed_until);

CREATE TRIGGER IF NOT EXISTS trg_event_reminder_state_cleanup
AFTER DELETE ON event
BEGIN
    DELETE FROM reminder_state WHERE kind = 'event' AND entity_id = OLD.id;
END;

CREATE TRIGGER IF NOT EXISTS trg_task_reminder_state_cleanup
AFTER DELETE ON task
BEGIN
    DELETE FROM reminder_state WHERE kind = 'task' AND entity_id = OLD.id;
END;
