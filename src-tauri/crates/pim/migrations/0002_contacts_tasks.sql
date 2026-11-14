-- nuncio-pim stage 2: contacts and tasks bookkeeping.
--
-- Core already stores address books, contacts, groups, task lists and tasks.
-- The PIM layer only adds:
--   * provider identity for address books and task lists (provider ids are not
--     globally unique and must never leak into the shared model);
--   * a mapping from a local task-step id to the provider's checklist/subtask
--     id, so step edits can be pushed individually;
--   * a per-reminder marker so the reminder loop does not repeat a task
--     notification after a restart.

CREATE TABLE IF NOT EXISTS pim_address_book_map (
    address_book_id TEXT PRIMARY KEY REFERENCES address_book(id) ON DELETE CASCADE,
    account_id      TEXT NOT NULL,
    provider        TEXT NOT NULL,
    remote_id       TEXT NOT NULL,
    href            TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    UNIQUE (account_id, provider, remote_id)
);

CREATE INDEX IF NOT EXISTS idx_pim_address_book_map_account
    ON pim_address_book_map(account_id, provider);

CREATE TABLE IF NOT EXISTS pim_task_list_map (
    task_list_id TEXT PRIMARY KEY REFERENCES task_list(id) ON DELETE CASCADE,
    account_id   TEXT NOT NULL,
    provider     TEXT NOT NULL,
    remote_id    TEXT NOT NULL,
    href         TEXT,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    UNIQUE (account_id, provider, remote_id)
);

CREATE INDEX IF NOT EXISTS idx_pim_task_list_map_account
    ON pim_task_list_map(account_id, provider);

CREATE TABLE IF NOT EXISTS pim_task_step_map (
    step_id    TEXT PRIMARY KEY,
    task_id    TEXT NOT NULL REFERENCES task(id) ON DELETE CASCADE,
    remote_id  TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pim_task_step_map_task
    ON pim_task_step_map(task_id, remote_id);

-- CardDAV-style hrefs for tasks; the core task table stores the provider id
-- and ETag but has no column for the resource URL.
CREATE TABLE IF NOT EXISTS pim_task_resource (
    task_id    TEXT PRIMARY KEY REFERENCES task(id) ON DELETE CASCADE,
    href       TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS pim_task_reminder_fired (
    task_id      TEXT NOT NULL,
    reminder_key TEXT NOT NULL,
    fired_at     TEXT NOT NULL,
    PRIMARY KEY (task_id, reminder_key)
);

CREATE INDEX IF NOT EXISTS idx_pim_task_reminder_time
    ON pim_task_reminder_fired(fired_at);
