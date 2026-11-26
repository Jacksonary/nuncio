-- Persist which operation last confirmed a contact's remote identity.
--
-- A create/update receipt carries the provider's identity (UID/href/ETag) and
-- used to be written to the row unconditionally. When a newer save had already
-- been acknowledged, a late receipt from an older save could overwrite the
-- confirmed `remote_id` and the identity kind (`uid_is_derived`) with stale
-- values.
--
-- The PIM layer owns this state (the `contact` table belongs to core):
--   * a local create/update claims the row by writing its operation id here in
--     the same transaction as the row and the queued operation;
--   * a receipt may write identity only while it still owns the row. A row with
--     no state (written before this migration, or by a delta upsert) is
--     unowned; with no usable evidence the receipt must not overwrite an
--     already confirmed `remote_id`.
--
-- The state survives `prune_finished` because it is not part of the queue, so
-- the evidence remains after the newer operation's `done` row is pruned.
-- `ON DELETE CASCADE` drops it with its contact.
CREATE TABLE IF NOT EXISTS pim_contact_identity_state (
    contact_id     TEXT PRIMARY KEY REFERENCES contact(id) ON DELETE CASCADE,
    identity_rev   INTEGER NOT NULL DEFAULT 0,
    identity_op_id TEXT,
    updated_at     TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pim_contact_identity_state_owner
    ON pim_contact_identity_state(identity_op_id);
