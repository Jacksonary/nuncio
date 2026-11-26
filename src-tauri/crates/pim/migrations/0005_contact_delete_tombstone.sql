-- Track the lifecycle of a contact delete intent.
--
-- `state` is one of:
--   * `pending`  – the remote object still has to be removed;
--   * `unknown`  – a create may have committed server-side but no receipt
--                  (remote id/href) ever arrived, so the object cannot be
--                  addressed yet; the intent must stay for reconciliation;
--   * `settled`  – the remote DELETE succeeded. The row is kept as a tombstone
--                  so a delta batch captured before the delete still suppresses
--                  the remote id instead of re-importing the contact.
--
-- `suppress_until` bounds how long a settled tombstone blocks a delta. Expired
-- tombstones are pruned when the next delta is applied.
ALTER TABLE pim_contact_delete_intent
    ADD COLUMN state TEXT NOT NULL DEFAULT 'pending';

ALTER TABLE pim_contact_delete_intent
    ADD COLUMN suppress_until TEXT;

CREATE INDEX IF NOT EXISTS idx_pim_contact_delete_intent_account_remote
    ON pim_contact_delete_intent(account_id, remote_id);
