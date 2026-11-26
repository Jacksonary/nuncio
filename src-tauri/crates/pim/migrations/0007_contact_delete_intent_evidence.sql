-- Bound two failure modes that previously held a book's cursor forever.
--
-- `evidence_cursor` is the provider cursor observed when an unaddressable
-- (`unknown`) intent last counted a sync window. A window only counts when the
-- cursor actually advanced: replaying the same blocked batch must never be
-- mistaken for new evidence that the object is gone.
--
-- `ambiguous_windows` counts sync windows in which an intent that still has an
-- identity kept matching an incoming object ambiguously (same UID, different
-- href) while its owning operation was already terminal. After the bound the
-- intent is released so the renamed object can be imported and the cursor can
-- move again.
ALTER TABLE pim_contact_delete_intent ADD COLUMN evidence_cursor TEXT;

ALTER TABLE pim_contact_delete_intent
    ADD COLUMN ambiguous_windows INTEGER NOT NULL DEFAULT 0;

-- Per-operation create receipts that raced a local delete.
--
-- The intent row records which operation owns the cleanup, but every operation
-- that issued a POST can create a server object independently (a retry after a
-- lost receipt plus a later Save). Each successful receipt is kept here so a
-- later Save's accurate remote identity is not discarded merely because the
-- intent is owned by an earlier operation.
CREATE TABLE IF NOT EXISTS pim_contact_delete_intent_receipt (
    contact_id      TEXT NOT NULL
                    REFERENCES pim_contact_delete_intent(contact_id) ON DELETE CASCADE,
    op_id           TEXT NOT NULL,
    remote_id       TEXT,
    href            TEXT,
    etag            TEXT,
    state           TEXT NOT NULL DEFAULT 'pending',
    suppress_until  TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    PRIMARY KEY (contact_id, op_id)
);

CREATE INDEX IF NOT EXISTS idx_pim_contact_delete_intent_receipt_contact
    ON pim_contact_delete_intent_receipt(contact_id);
