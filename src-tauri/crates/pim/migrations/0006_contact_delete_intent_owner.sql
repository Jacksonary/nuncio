-- Bind a delete intent to the operation that may have produced the remote
-- object, and give an unaddressable (`unknown`) intent a bounded lifetime.
--
-- `owner_op_id`/`owner_attempt` identify the queued `pim.contact.save` that was
-- in flight when the delete raced it. Cleanup must be performed by that same
-- operation: a different operation whose local row vanished must never delete
-- another operation's intent, or a create that is still uploading loses its
-- only record and its server object is re-imported.
--
-- `unknown_windows` counts how many sync windows have passed since the intent
-- became unaddressable (no remote id/href) while its owner is no longer
-- outstanding. When it reaches the release threshold the row becomes
-- `released`, which clears the book-wide import block. `released_at` records
-- when that happened so the release remains auditable.
ALTER TABLE pim_contact_delete_intent ADD COLUMN owner_op_id TEXT;

ALTER TABLE pim_contact_delete_intent ADD COLUMN owner_attempt INTEGER;

ALTER TABLE pim_contact_delete_intent
    ADD COLUMN unknown_windows INTEGER NOT NULL DEFAULT 0;

ALTER TABLE pim_contact_delete_intent ADD COLUMN released_at TEXT;
