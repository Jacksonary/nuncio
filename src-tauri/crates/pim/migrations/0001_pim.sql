-- nuncio-pim owned tables.
--
-- The core schema already stores calendars and events.  The PIM layer only
-- needs two pieces of bookkeeping that do not belong in the shared model:
--
-- * a mapping between a local calendar id and the provider's calendar
--   identifier (Google calendar id, Graph calendar id or the absolute CalDAV
--   collection URL), because provider ids are not globally unique;
-- * a per-occurrence "notification already delivered" marker so the reminder
--   loop does not fire the same reminder again after a restart.

CREATE TABLE IF NOT EXISTS pim_calendar_map (
    calendar_id TEXT PRIMARY KEY REFERENCES calendar(id) ON DELETE CASCADE,
    account_id  TEXT NOT NULL,
    provider    TEXT NOT NULL,
    remote_id   TEXT NOT NULL,
    href        TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    UNIQUE (account_id, provider, remote_id)
);

CREATE INDEX IF NOT EXISTS idx_pim_calendar_map_account
    ON pim_calendar_map(account_id, provider);

CREATE TABLE IF NOT EXISTS pim_reminder_fired (
    event_id       TEXT NOT NULL,
    instance_start TEXT NOT NULL,
    fired_at       TEXT NOT NULL,
    PRIMARY KEY (event_id, instance_start)
);

CREATE INDEX IF NOT EXISTS idx_pim_reminder_fired_time
    ON pim_reminder_fired(fired_at);
