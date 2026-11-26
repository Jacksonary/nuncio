-- Durable, per-message undo state machine.
--
-- `undo_log` remains the queue of reversible user actions (one row per
-- message_act, consumed by the toast/keyboard undo path). Once an undo is
-- started it is copied into `undo_request` / `undo_entry`, so each message is
-- tracked independently and a deferred request survives a restart.

CREATE TABLE IF NOT EXISTS undo_request (
    id           TEXT PRIMARY KEY,
    account_id   TEXT NOT NULL,
    kind         TEXT NOT NULL,
    value        INTEGER,
    state        TEXT NOT NULL DEFAULT 'pending',
    attempt      INTEGER NOT NULL DEFAULT 0,
    op_ids_json  TEXT NOT NULL DEFAULT '[]',
    last_error   TEXT,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_undo_request_state ON undo_request(state, updated_at);

CREATE TABLE IF NOT EXISTS undo_entry (
    id                 TEXT PRIMARY KEY,
    request_id         TEXT NOT NULL REFERENCES undo_request(id) ON DELETE CASCADE,
    account_id         TEXT NOT NULL,
    message_id         TEXT NOT NULL,
    original_folder_id TEXT NOT NULL,
    target_folder_id   TEXT,
    target_uid         INTEGER,
    target_uidvalidity INTEGER,
    target_graph_id    TEXT,
    permanent          INTEGER NOT NULL DEFAULT 0,
    result             TEXT NOT NULL DEFAULT 'pending',
    inverse_op_id      TEXT,
    op_ids_json        TEXT NOT NULL DEFAULT '[]',
    payload_json       TEXT NOT NULL DEFAULT '{}',
    ordinal            INTEGER NOT NULL DEFAULT 0,
    created_at         TEXT NOT NULL,
    updated_at         TEXT NOT NULL,
    UNIQUE(request_id, message_id)
);
CREATE INDEX IF NOT EXISTS idx_undo_entry_request ON undo_entry(request_id, ordinal);
CREATE INDEX IF NOT EXISTS idx_undo_entry_inverse ON undo_entry(inverse_op_id);
