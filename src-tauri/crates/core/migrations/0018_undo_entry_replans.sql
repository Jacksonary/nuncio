-- Bound automatic inverse-operation re-planning.
--
-- When a queued inverse is settled as provably "not executed", undo releases
-- its claim and re-plans the entry. A target that keeps failing before the
-- command is sent (for example because its folder disappeared) would otherwise
-- create a fresh operation with a zero retry count on every pass forever. Each
-- re-plan is counted here; the entry fails visibly once the budget is spent.
ALTER TABLE undo_entry ADD COLUMN replans INTEGER NOT NULL DEFAULT 0;
