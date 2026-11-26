-- Make quarantine replay order and tombstone identity source explicit.
--
-- Quarantine snapshots were ordered by `updated_at` and broke ties with
-- `rowid`. Both are unstable: two writes inside the same second share a
-- timestamp, and `rowid` follows *first* insertion, so a resource delivered as
-- UID X -> Y -> X (equivalent href) replayed X(new), Y(old) and the stale Y
-- overwrote the newest X. `seq` is assigned monotonically on every write
-- (including the `ON CONFLICT` refresh) and replay applies the largest value
-- last, independent of wall-clock granularity and row order.
ALTER TABLE pim_contact_quarantine ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS idx_pim_contact_quarantine_seq
    ON pim_contact_quarantine(address_book_id, seq);

-- A CardDAV vCard without a UID is identified by a placeholder derived from
-- its href (`A.vcf`). That placeholder is *not* a real UID: a new card may
-- legitimately carry the real UID `A.vcf` at the same href. Persist where the
-- remote identity came from so a tombstone only matches its own kind:
--   * real UID  -> only another real UID;
--   * derived   -> only a UID-less card at an equivalent href.
-- Default 0 keeps every pre-existing row a real UID (the overwhelmingly common
-- case); derived identities are written explicitly.
ALTER TABLE pim_contact_delete_intent
    ADD COLUMN uid_is_derived INTEGER NOT NULL DEFAULT 0;

ALTER TABLE pim_contact_delete_intent_receipt
    ADD COLUMN uid_is_derived INTEGER NOT NULL DEFAULT 0;
