-- Intermediate builds wrote `state = 'quarantined'` for draft deletes that must
-- never run. An older binary maps unknown states to Pending and would send
-- them, so no such row may survive: normalize them to `cancelled` (the audit
-- intent stays in the payload's `quarantined` flag).
UPDATE pending_op SET state = 'cancelled' WHERE state = 'quarantined';
