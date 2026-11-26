-- Independent bounded release for per-operation create receipts and durable
-- quarantine of objects dropped by book-level delete isolation.
--
-- `ambiguous_windows` counts how many sync windows a pending receipt kept
-- matching an incoming object ambiguously while its operation was terminal.
-- A receipt is released on its own (`released_at`) even when its parent intent
-- was already released: the parent's state must not decide whether the child
-- can stop holding the cursor. A released receipt stops blocking ambiguity but
-- its exact remote identity keeps suppressing the object for one tombstone TTL.
ALTER TABLE pim_contact_delete_intent_receipt
    ADD COLUMN ambiguous_windows INTEGER NOT NULL DEFAULT 0;

ALTER TABLE pim_contact_delete_intent_receipt ADD COLUMN released_at TEXT;

-- Objects hidden by an identity-less delete intent (book-level isolation) are
-- committed batches: their cursor advances, so the provider will never deliver
-- them again. Persist them here so the release of the intent can replay them
-- instead of losing unrelated contacts forever.
--
-- `identity` is a stable key inside the book (`remote id` + `href`) so the same
-- object re-delivered by several blocked batches updates one row instead of
-- accumulating duplicates. `payload` is the serialized RemoteContact.
CREATE TABLE IF NOT EXISTS pim_contact_quarantine (
    id              TEXT PRIMARY KEY,
    account_id      TEXT NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    address_book_id TEXT NOT NULL REFERENCES address_book(id) ON DELETE CASCADE,
    identity        TEXT NOT NULL,
    remote_id       TEXT,
    href            TEXT,
    payload         TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    UNIQUE (address_book_id, identity)
);

CREATE INDEX IF NOT EXISTS idx_pim_contact_quarantine_book
    ON pim_contact_quarantine(account_id, address_book_id);
