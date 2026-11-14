-- Nuncio core schema (namespace `core`, version 1).
-- Owns every table used by the mail, calendar, contacts and tasks modules.
-- Domain crates add their own `schema_migrations` namespaces; they never edit
-- this file.

/* ───────────────────────────── Accounts ───────────────────────────── */

CREATE TABLE IF NOT EXISTS account (
    id                    TEXT PRIMARY KEY,
    provider              TEXT NOT NULL,
    email                 TEXT NOT NULL,
    display_name          TEXT NOT NULL DEFAULT '',
    color                 TEXT NOT NULL DEFAULT '#0078D4',
    auth_kind             TEXT NOT NULL,
    oauth_provider        TEXT,
    imap_json             TEXT,
    smtp_json             TEXT,
    pop3_json             TEXT,
    caldav_url            TEXT,
    carddav_url           TEXT,
    sync_window_days      INTEGER NOT NULL DEFAULT 90,
    mail_enabled          INTEGER NOT NULL DEFAULT 1,
    calendar_enabled      INTEGER NOT NULL DEFAULT 1,
    contacts_enabled      INTEGER NOT NULL DEFAULT 1,
    tasks_enabled         INTEGER NOT NULL DEFAULT 1,
    notifications_enabled INTEGER NOT NULL DEFAULT 1,
    include_in_unified    INTEGER NOT NULL DEFAULT 1,
    sort_order            INTEGER NOT NULL DEFAULT 0,
    state                 TEXT NOT NULL DEFAULT 'ok',
    state_message         TEXT,
    capabilities_json     TEXT NOT NULL DEFAULT '{}',
    created_at            TEXT NOT NULL,
    updated_at            TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_account_sort ON account(sort_order, created_at);

CREATE TABLE IF NOT EXISTS identity (
    id           TEXT PRIMARY KEY,
    account_id   TEXT NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    name         TEXT NOT NULL DEFAULT '',
    email        TEXT NOT NULL,
    reply_to     TEXT,
    signature_id TEXT,
    is_default   INTEGER NOT NULL DEFAULT 0,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_identity_account ON identity(account_id);

CREATE TABLE IF NOT EXISTS signature (
    id            TEXT PRIMARY KEY,
    account_id    TEXT REFERENCES account(id) ON DELETE CASCADE,
    name          TEXT NOT NULL DEFAULT '',
    html          TEXT NOT NULL DEFAULT '',
    use_for_new   INTEGER NOT NULL DEFAULT 0,
    use_for_reply INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_signature_account ON signature(account_id);

/* ─────────────────────────────── Mail ─────────────────────────────── */

CREATE TABLE IF NOT EXISTS folder (
    id              TEXT PRIMARY KEY,
    account_id      TEXT NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    parent_id       TEXT,
    remote_path     TEXT NOT NULL,
    name            TEXT NOT NULL,
    role            TEXT NOT NULL DEFAULT 'other',
    unread_count    INTEGER NOT NULL DEFAULT 0,
    total_count     INTEGER NOT NULL DEFAULT 0,
    is_favorite     INTEGER NOT NULL DEFAULT 0,
    sync_enabled    INTEGER NOT NULL DEFAULT 1,
    is_selectable   INTEGER NOT NULL DEFAULT 1,
    sort_order      INTEGER NOT NULL DEFAULT 0,
    last_sync_at    TEXT,
    uidvalidity     INTEGER,
    uidnext         INTEGER,
    highest_modseq  INTEGER,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    UNIQUE (account_id, remote_path)
);
CREATE INDEX IF NOT EXISTS idx_folder_account ON folder(account_id, sort_order, name);
CREATE INDEX IF NOT EXISTS idx_folder_role ON folder(account_id, role);

CREATE TABLE IF NOT EXISTS message (
    id                TEXT PRIMARY KEY,
    account_id        TEXT NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    folder_id         TEXT NOT NULL REFERENCES folder(id) ON DELETE CASCADE,
    uid               INTEGER,
    uidvalidity       INTEGER,
    gm_msgid          TEXT,
    gm_thrid          TEXT,
    message_id_header TEXT,
    in_reply_to       TEXT,
    references_json   TEXT NOT NULL DEFAULT '[]',
    thread_id         TEXT NOT NULL DEFAULT '',
    subject           TEXT NOT NULL DEFAULT '',
    from_json         TEXT,
    to_json           TEXT NOT NULL DEFAULT '[]',
    cc_json           TEXT NOT NULL DEFAULT '[]',
    bcc_json          TEXT NOT NULL DEFAULT '[]',
    reply_to_json     TEXT NOT NULL DEFAULT '[]',
    date              TEXT NOT NULL,
    internal_date     TEXT,
    preview           TEXT NOT NULL DEFAULT '',
    seen              INTEGER NOT NULL DEFAULT 0,
    flagged           INTEGER NOT NULL DEFAULT 0,
    answered          INTEGER NOT NULL DEFAULT 0,
    forwarded         INTEGER NOT NULL DEFAULT 0,
    is_draft          INTEGER NOT NULL DEFAULT 0,
    has_attachments   INTEGER NOT NULL DEFAULT 0,
    importance        TEXT NOT NULL DEFAULT 'normal',
    size              INTEGER NOT NULL DEFAULT 0,
    body_path         TEXT,
    body_cached_at    TEXT,
    labels_json       TEXT NOT NULL DEFAULT '[]',
    is_deleted_local  INTEGER NOT NULL DEFAULT 0,
    raw_headers       TEXT,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_message_folder_date ON message(folder_id, date DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_message_account_date ON message(account_id, date DESC);
CREATE INDEX IF NOT EXISTS idx_message_thread ON message(account_id, thread_id);
CREATE INDEX IF NOT EXISTS idx_message_account_uid ON message(account_id, folder_id, uid);
CREATE UNIQUE INDEX IF NOT EXISTS idx_message_unique_uid
    ON message(folder_id, uid) WHERE uid IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_message_message_id ON message(message_id_header);
CREATE INDEX IF NOT EXISTS idx_message_body_cached ON message(body_cached_at) WHERE body_path IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_message_flagged ON message(account_id, flagged, date);

CREATE VIRTUAL TABLE IF NOT EXISTS fts_mail USING fts5(
    message_id UNINDEXED,
    subject,
    addresses,
    preview,
    body,
    bigram,
    tokenize = "unicode61 remove_diacritics 2"
);

CREATE TABLE IF NOT EXISTS attachment (
    id            TEXT PRIMARY KEY,
    message_id    TEXT NOT NULL REFERENCES message(id) ON DELETE CASCADE,
    part_id       TEXT,
    section       TEXT,
    filename      TEXT NOT NULL DEFAULT '',
    mime_type     TEXT NOT NULL DEFAULT 'application/octet-stream',
    size          INTEGER NOT NULL DEFAULT 0,
    content_id    TEXT,
    is_inline     INTEGER NOT NULL DEFAULT 0,
    is_downloaded INTEGER NOT NULL DEFAULT 0,
    path          TEXT,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_attachment_message ON attachment(message_id);
CREATE INDEX IF NOT EXISTS idx_attachment_content_id ON attachment(message_id, content_id);

CREATE TABLE IF NOT EXISTS draft (
    id                        TEXT PRIMARY KEY,
    account_id                TEXT NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    identity_id               TEXT,
    from_json                 TEXT,
    to_json                   TEXT NOT NULL DEFAULT '[]',
    cc_json                   TEXT NOT NULL DEFAULT '[]',
    bcc_json                  TEXT NOT NULL DEFAULT '[]',
    subject                   TEXT NOT NULL DEFAULT '',
    html                      TEXT NOT NULL DEFAULT '',
    attachments_json          TEXT NOT NULL DEFAULT '[]',
    reply_mode                TEXT,
    source_message_id         TEXT,
    in_reply_to               TEXT,
    references_json           TEXT NOT NULL DEFAULT '[]',
    importance                TEXT NOT NULL DEFAULT 'normal',
    request_read_receipt      INTEGER NOT NULL DEFAULT 0,
    remote_draft_message_id   TEXT,
    created_at                TEXT NOT NULL,
    updated_at                TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_draft_account ON draft(account_id, updated_at DESC);

CREATE TABLE IF NOT EXISTS outbox (
    id            TEXT PRIMARY KEY,
    account_id    TEXT NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    draft_id      TEXT,
    recipients_json TEXT NOT NULL DEFAULT '[]',
    subject       TEXT NOT NULL DEFAULT '',
    rfc822_path   TEXT NOT NULL,
    state         TEXT NOT NULL DEFAULT 'pending',
    attempts      INTEGER NOT NULL DEFAULT 0,
    next_retry_at TEXT,
    last_error    TEXT,
    created_at    TEXT NOT NULL,
    sent_at       TEXT
);
CREATE INDEX IF NOT EXISTS idx_outbox_due ON outbox(state, next_retry_at, created_at);
CREATE INDEX IF NOT EXISTS idx_outbox_account ON outbox(account_id);

CREATE TABLE IF NOT EXISTS pending_op (
    id            TEXT PRIMARY KEY,
    account_id    TEXT NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    kind          TEXT NOT NULL,
    payload_json  TEXT NOT NULL DEFAULT '{}',
    state         TEXT NOT NULL DEFAULT 'pending',
    attempts      INTEGER NOT NULL DEFAULT 0,
    next_retry_at TEXT,
    description   TEXT NOT NULL DEFAULT '',
    last_error    TEXT,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_pending_op_due ON pending_op(state, next_retry_at, created_at);
CREATE INDEX IF NOT EXISTS idx_pending_op_account ON pending_op(account_id, state);

CREATE TABLE IF NOT EXISTS undo_log (
    id           TEXT PRIMARY KEY,
    account_id   TEXT NOT NULL,
    kind         TEXT NOT NULL,
    description  TEXT NOT NULL DEFAULT '',
    payload_json TEXT NOT NULL DEFAULT '{}',
    created_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_undo_log_created ON undo_log(created_at DESC);

CREATE TABLE IF NOT EXISTS mail_rule (
    id              TEXT PRIMARY KEY,
    account_id      TEXT REFERENCES account(id) ON DELETE CASCADE,
    name            TEXT NOT NULL DEFAULT '',
    enabled         INTEGER NOT NULL DEFAULT 1,
    match_all       INTEGER NOT NULL DEFAULT 1,
    conditions_json TEXT NOT NULL DEFAULT '[]',
    actions_json    TEXT NOT NULL DEFAULT '[]',
    stop_processing INTEGER NOT NULL DEFAULT 0,
    sort_order      INTEGER NOT NULL DEFAULT 0,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_mail_rule_account ON mail_rule(account_id, sort_order);

CREATE TABLE IF NOT EXISTS trusted_sender (
    id         TEXT PRIMARY KEY,
    value      TEXT NOT NULL COLLATE NOCASE,
    account_id TEXT REFERENCES account(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL,
    UNIQUE (value)
);
CREATE INDEX IF NOT EXISTS idx_trusted_sender_value ON trusted_sender(value);

CREATE TABLE IF NOT EXISTS recipient_history (
    email        TEXT PRIMARY KEY COLLATE NOCASE,
    name         TEXT,
    use_count    INTEGER NOT NULL DEFAULT 1,
    last_used_at TEXT NOT NULL
);

/* ───────────────────────────── Calendar ───────────────────────────── */

CREATE TABLE IF NOT EXISTS calendar (
    id               TEXT PRIMARY KEY,
    account_id       TEXT NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    name             TEXT NOT NULL,
    color            TEXT NOT NULL DEFAULT '#0078D4',
    is_read_only     INTEGER NOT NULL DEFAULT 0,
    is_visible       INTEGER NOT NULL DEFAULT 1,
    is_default       INTEGER NOT NULL DEFAULT 0,
    subscription_url TEXT,
    ctag             TEXT,
    sync_token       TEXT,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_calendar_account ON calendar(account_id);

CREATE TABLE IF NOT EXISTS event (
    id                 TEXT PRIMARY KEY,
    calendar_id        TEXT NOT NULL REFERENCES calendar(id) ON DELETE CASCADE,
    account_id         TEXT NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    uid                TEXT NOT NULL,
    title              TEXT NOT NULL DEFAULT '',
    location           TEXT NOT NULL DEFAULT '',
    description        TEXT NOT NULL DEFAULT '',
    start_at           TEXT NOT NULL,
    end_at             TEXT NOT NULL,
    -- Canonical UTC instants used for range queries (event.start/end keep their
    -- original offset or YYYY-MM-DD form for display).
    start_utc          TEXT NOT NULL DEFAULT '',
    end_utc            TEXT NOT NULL DEFAULT '',
    all_day            INTEGER NOT NULL DEFAULT 0,
    time_zone          TEXT NOT NULL DEFAULT 'UTC',
    recurrence_json    TEXT,
    rrule              TEXT,
    exdates_json       TEXT NOT NULL DEFAULT '[]',
    recurrence_id      TEXT,
    series_id          TEXT,
    status             TEXT NOT NULL DEFAULT 'confirmed',
    show_as            TEXT NOT NULL DEFAULT 'busy',
    is_private         INTEGER NOT NULL DEFAULT 0,
    organizer_json     TEXT,
    attendees_json     TEXT NOT NULL DEFAULT '[]',
    reminders_json     TEXT NOT NULL DEFAULT '[]',
    is_organizer       INTEGER NOT NULL DEFAULT 0,
    my_response        TEXT,
    online_meeting_url TEXT,
    color              TEXT,
    raw_ics            TEXT,
    etag               TEXT,
    remote_id          TEXT,
    href               TEXT,
    created_at         TEXT NOT NULL,
    updated_at         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_event_calendar_start ON event(calendar_id, start_utc);
CREATE INDEX IF NOT EXISTS idx_event_calendar_uid ON event(calendar_id, uid);
CREATE INDEX IF NOT EXISTS idx_event_series ON event(series_id);
CREATE INDEX IF NOT EXISTS idx_event_remote ON event(calendar_id, remote_id);
CREATE INDEX IF NOT EXISTS idx_event_href ON event(calendar_id, href);

CREATE TABLE IF NOT EXISTS event_reminder_state (
    event_id       TEXT NOT NULL REFERENCES event(id) ON DELETE CASCADE,
    instance_start TEXT NOT NULL,
    dismissed_at   TEXT,
    snoozed_until  TEXT,
    updated_at     TEXT NOT NULL,
    PRIMARY KEY (event_id, instance_start)
);

/* ───────────────────────────── Contacts ───────────────────────────── */

CREATE TABLE IF NOT EXISTS address_book (
    id           TEXT PRIMARY KEY,
    account_id   TEXT NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    is_read_only INTEGER NOT NULL DEFAULT 0,
    is_default   INTEGER NOT NULL DEFAULT 0,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_address_book_account ON address_book(account_id);

CREATE TABLE IF NOT EXISTS contact (
    id             TEXT PRIMARY KEY,
    address_book_id TEXT NOT NULL REFERENCES address_book(id) ON DELETE CASCADE,
    account_id     TEXT NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    display_name   TEXT NOT NULL DEFAULT '',
    given_name     TEXT NOT NULL DEFAULT '',
    family_name    TEXT NOT NULL DEFAULT '',
    middle_name    TEXT NOT NULL DEFAULT '',
    nickname       TEXT NOT NULL DEFAULT '',
    company        TEXT NOT NULL DEFAULT '',
    job_title      TEXT NOT NULL DEFAULT '',
    department     TEXT NOT NULL DEFAULT '',
    emails_json    TEXT NOT NULL DEFAULT '[]',
    phones_json    TEXT NOT NULL DEFAULT '[]',
    addresses_json TEXT NOT NULL DEFAULT '[]',
    websites_json  TEXT NOT NULL DEFAULT '[]',
    birthday       TEXT,
    notes          TEXT NOT NULL DEFAULT '',
    photo          TEXT,
    is_favorite    INTEGER NOT NULL DEFAULT 0,
    raw_vcard      TEXT,
    etag           TEXT,
    remote_id      TEXT,
    href           TEXT,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_contact_book_name ON contact(address_book_id, display_name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_contact_account ON contact(account_id);
CREATE INDEX IF NOT EXISTS idx_contact_display ON contact(display_name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_contact_remote ON contact(address_book_id, remote_id);
CREATE INDEX IF NOT EXISTS idx_contact_favorite ON contact(account_id, is_favorite);

CREATE TABLE IF NOT EXISTS contact_group (
    id              TEXT PRIMARY KEY,
    address_book_id TEXT NOT NULL REFERENCES address_book(id) ON DELETE CASCADE,
    account_id      TEXT NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    remote_id       TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_contact_group_book ON contact_group(address_book_id);

CREATE TABLE IF NOT EXISTS contact_group_member (
    group_id   TEXT NOT NULL REFERENCES contact_group(id) ON DELETE CASCADE,
    contact_id TEXT NOT NULL REFERENCES contact(id) ON DELETE CASCADE,
    added_at   TEXT NOT NULL,
    PRIMARY KEY (group_id, contact_id)
);
CREATE INDEX IF NOT EXISTS idx_group_member_contact ON contact_group_member(contact_id);

/* ─────────────────────────────── To Do ────────────────────────────── */

CREATE TABLE IF NOT EXISTS task_list (
    id           TEXT PRIMARY KEY,
    account_id   TEXT NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    color        TEXT NOT NULL DEFAULT '#0078D4',
    is_default   INTEGER NOT NULL DEFAULT 0,
    is_read_only INTEGER NOT NULL DEFAULT 0,
    sort_order   INTEGER NOT NULL DEFAULT 0,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_task_list_account ON task_list(account_id, sort_order);

CREATE TABLE IF NOT EXISTS task (
    id                TEXT PRIMARY KEY,
    list_id           TEXT NOT NULL REFERENCES task_list(id) ON DELETE CASCADE,
    account_id        TEXT NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    title             TEXT NOT NULL DEFAULT '',
    notes             TEXT NOT NULL DEFAULT '',
    status            TEXT NOT NULL DEFAULT 'notStarted',
    importance        TEXT NOT NULL DEFAULT 'normal',
    due_date          TEXT,
    reminder_at       TEXT,
    recurrence_json   TEXT,
    my_day_date       TEXT,
    completed_at      TEXT,
    steps_json        TEXT NOT NULL DEFAULT '[]',
    sort_order        INTEGER NOT NULL DEFAULT 0,
    linked_message_id TEXT,
    etag              TEXT,
    remote_id         TEXT,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_task_list_sort ON task(list_id, sort_order, created_at);
CREATE INDEX IF NOT EXISTS idx_task_due ON task(account_id, due_date);
CREATE INDEX IF NOT EXISTS idx_task_my_day ON task(account_id, my_day_date);
CREATE INDEX IF NOT EXISTS idx_task_completed ON task(account_id, completed_at);
CREATE INDEX IF NOT EXISTS idx_task_remote ON task(list_id, remote_id);

/* ─────────────────────────────── Common ───────────────────────────── */

CREATE TABLE IF NOT EXISTS sync_state (
    account_id TEXT NOT NULL,
    module     TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (account_id, module, key)
);
CREATE INDEX IF NOT EXISTS idx_sync_state_module ON sync_state(account_id, module);

CREATE TABLE IF NOT EXISTS settings (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    json       TEXT NOT NULL DEFAULT '{}',
    updated_at TEXT NOT NULL
);

/* ───────────────────────────── Seed data ──────────────────────────── */

-- Built-in local account (calendar / contacts / tasks only).
INSERT OR IGNORE INTO account (
    id, provider, email, display_name, color, auth_kind, oauth_provider,
    imap_json, smtp_json, pop3_json, caldav_url, carddav_url,
    sync_window_days, mail_enabled, calendar_enabled, contacts_enabled, tasks_enabled,
    notifications_enabled, include_in_unified, sort_order, state, state_message,
    capabilities_json, created_at, updated_at
) VALUES (
    'local', 'local', '', '本地', '#5B5FC7', 'password', NULL,
    NULL, NULL, NULL, NULL, NULL,
    0, 0, 1, 1, 1,
    1, 0, -1, 'ok', NULL,
    '{"idle":false,"condstore":false,"qresync":false,"move":false,"uidplus":false,"gmailExtensions":false,"specialUse":false,"serverSearch":false,"autoSavesSent":false,"calendar":true,"contacts":true,"tasks":true,"taskImportance":true,"taskReminders":true,"taskSteps":true,"taskRecurrence":true}',
    '1970-01-01T00:00:00Z', '1970-01-01T00:00:00Z'
);

INSERT OR IGNORE INTO calendar (
    id, account_id, name, color, is_read_only, is_visible, is_default, subscription_url,
    ctag, sync_token, created_at, updated_at
) VALUES (
    'local-calendar', 'local', '日历', '#0078D4', 0, 1, 1, NULL,
    NULL, NULL, '1970-01-01T00:00:00Z', '1970-01-01T00:00:00Z'
);

INSERT OR IGNORE INTO address_book (
    id, account_id, name, is_read_only, is_default, created_at, updated_at
) VALUES (
    'local-contacts', 'local', '联系人', 0, 1,
    '1970-01-01T00:00:00Z', '1970-01-01T00:00:00Z'
);

INSERT OR IGNORE INTO task_list (
    id, account_id, name, color, is_default, is_read_only, sort_order, created_at, updated_at
) VALUES (
    'local-tasks', 'local', '任务', '#0078D4', 1, 0, 0,
    '1970-01-01T00:00:00Z', '1970-01-01T00:00:00Z'
);

INSERT OR IGNORE INTO settings (id, json, updated_at)
VALUES (1, '{}', '1970-01-01T00:00:00Z');
