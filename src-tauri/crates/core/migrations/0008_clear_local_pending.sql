-- An earlier version marked every cross-folder move with `target_pending`,
-- including Local/POP3 accounts. Those accounts never queue a server move and
-- never re-associate a target UID, so the marker left their messages stuck:
-- actions refused to touch a row that no sync would ever release.
--
-- New moves only mark rows whose server operation was actually queued. Clear
-- the stale markers written by the old code.
UPDATE message SET target_pending = 0
 WHERE account_id IN (SELECT id FROM account WHERE provider IN ('local', 'pop3'));
