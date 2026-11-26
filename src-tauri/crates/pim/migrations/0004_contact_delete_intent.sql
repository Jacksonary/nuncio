-- A contact delete that raced an in-flight create.
--
-- The local row is gone, but the provider object may still be created by the
-- queued `pim.contact.save`. This intent survives the delete transaction so
-- the create receipt (or a retry of the create op) can remove the remote
-- object it produced and then drop the intent. `remote_id`/`href`/`etag` are
-- backfilled from the receipt when it arrives.
CREATE TABLE IF NOT EXISTS pim_contact_delete_intent (
    contact_id      TEXT PRIMARY KEY,
    account_id      TEXT NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    address_book_id TEXT NOT NULL REFERENCES address_book(id) ON DELETE CASCADE,
    remote_id       TEXT,
    href            TEXT,
    etag            TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pim_contact_delete_intent_account
    ON pim_contact_delete_intent(account_id);
