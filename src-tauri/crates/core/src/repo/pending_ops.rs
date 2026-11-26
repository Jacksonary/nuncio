//! Persistent operation queue (`pending_op`) used by every sync engine.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};

use crate::error::AppError;
use crate::models::{new_id, NewPendingOp, PendingOp, PendingOpState};
use crate::repo::{get_opt_dt, get_opt_string, get_string, json_or_default, json_text};
use crate::Result;

const COLUMNS: &str = "\
    id, account_id, kind, payload_json, state, attempts, next_retry_at, description,\
    last_error, created_at, seq, send_intent, outcome_unknown";

fn row_to_op(row: &Row<'_>) -> Result<PendingOp> {
    let payload: Option<String> = row.get("payload_json")?;
    let state: String = get_string(row, "state")?;
    Ok(PendingOp {
        id: get_string(row, "id")?,
        account_id: get_string(row, "account_id")?,
        kind: get_string(row, "kind")?,
        payload: json_or_default(payload.as_deref(), "payload_json"),
        state: state.parse().unwrap_or(PendingOpState::Pending),
        sequence: row.get::<_, Option<i64>>("seq")?.unwrap_or_default(),
        attempts: row.get::<_, i64>("attempts")?.max(0) as u32,
        next_retry_at: get_opt_dt(row, "next_retry_at")?,
        created_at: crate::repo::get_dt(row, "created_at")?,
        description: get_string(row, "description")?,
        last_error: get_opt_string(row, "last_error")?,
        send_intent: row.get::<_, i64>("send_intent")? != 0,
        outcome_unknown: row.get::<_, i64>("outcome_unknown")? != 0,
    })
}

/// Intermediate builds wrote `state = 'quarantined'`, which older binaries
/// map to Pending and would execute. Normalize such rows to `cancelled` (the
/// audit intent lives in the payload flag). Idempotent; also covered by
/// migration 0015.
pub fn normalize_quarantined_state(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE pending_op SET state = 'cancelled' WHERE state = 'quarantined'",
        [],
    )?)
}

pub fn enqueue(conn: &Connection, op: NewPendingOp) -> Result<PendingOp> {
    enqueue_with_id(conn, &new_id(), op)
}

/// Insert an operation with a caller-chosen id.
///
/// Used when the id must be recorded against another row in the same
/// transaction (undo compensation claims its entry with the inverse operation
/// id before queueing it).
pub fn enqueue_with_id(conn: &Connection, id: &str, op: NewPendingOp) -> Result<PendingOp> {
    let created_at = crate::repo::now_string();
    conn.execute(
        "INSERT INTO pending_op (id, account_id, kind, payload_json, state, attempts, next_retry_at,
                                 description, last_error, created_at, updated_at, seq)
         VALUES (?1, ?2, ?3, ?4, 'pending', 0, NULL, ?5, NULL, ?6, ?6,
                 (SELECT COALESCE(MAX(seq), 0) + 1 FROM pending_op))",
        params![
            id,
            op.account_id,
            op.kind,
            json_text(&op.payload)?,
            op.description,
            created_at
        ],
    )?;
    get_required(conn, id)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<PendingOp>> {
    let sql = format!("SELECT {COLUMNS} FROM pending_op WHERE id = ?1");
    crate::repo::query_one(conn, &sql, params![id], row_to_op)
}

pub fn get_required(conn: &Connection, id: &str) -> Result<PendingOp> {
    get(conn, id)?.ok_or_else(|| AppError::NotFound(format!("pending operation {id} not found")))
}

/// Pending operations whose retry time is due, oldest first.
///
/// `created_at` has second precision, so several operations enqueued in the
/// same transaction share one timestamp. `seq` is the explicit insertion order
/// (assigned by [`enqueue`], backfilled from `rowid` by migration 0014) and
/// breaks that tie deterministically. It must not be the implicit `rowid`: a
/// table rebuild may renumber it, which would let a replacement's DELETE run
/// after its APPEND or make the "newest attribution" scan pick an older APPEND.
pub fn next_due(conn: &Connection, now: DateTime<Utc>, limit: u32) -> Result<Vec<PendingOp>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM pending_op
         WHERE state = 'pending' AND (next_retry_at IS NULL OR next_retry_at <= ?1)
         ORDER BY seq ASC LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![crate::repo::dt_string(now), limit.max(1) as i64])?;
    let mut ops = Vec::new();
    while let Some(row) = rows.next()? {
        ops.push(row_to_op(row)?);
    }
    Ok(ops)
}

pub fn list(conn: &Connection, account_id: Option<&str>) -> Result<Vec<PendingOp>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM pending_op WHERE (?1 IS NULL OR account_id = ?1)
         ORDER BY seq ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![account_id])?;
    let mut ops = Vec::new();
    while let Some(row) = rows.next()? {
        ops.push(row_to_op(row)?);
    }
    Ok(ops)
}

/// Operations whose row was created or last transitioned at/after `since`.
///
/// Sync engines use this to protect an object from a delta that was fetched
/// before a local write was uploaded: a `done`/`failed`/`cancelled` row is not
/// [`is_outstanding`], but its `updated_at` still proves the object changed
/// while the delta request was in flight. Timestamps have second precision
/// (`dt_string`), so the comparison is deliberately inclusive.
pub fn list_touched_since(
    conn: &Connection,
    account_id: Option<&str>,
    since: DateTime<Utc>,
) -> Result<Vec<PendingOp>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM pending_op
         WHERE (?1 IS NULL OR account_id = ?1)
           AND (created_at >= ?2 OR updated_at >= ?2)
         ORDER BY seq ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![account_id, crate::repo::dt_string(since)])?;
    let mut ops = Vec::new();
    while let Some(row) = rows.next()? {
        ops.push(row_to_op(row)?);
    }
    Ok(ops)
}

/// `true` while an operation may still own the local object it describes.
///
/// A queued (`pending`) or executing (`inProgress`) operation has not been
/// confirmed against the provider yet: a delta response taken before it lands
/// describes an older server state and must not replace the optimistic local
/// row. Ambiguous attempts stay in one of these states (`mark_retry` keeps the
/// row `pending` and only sets `outcome_unknown`), so they are covered too.
///
/// Settled rows (`done`/`failed`/`cancelled`) no longer own the object: the
/// write either landed or its local effect was rolled back, so a later sync may
/// apply the server value.
pub fn is_outstanding(op: &PendingOp) -> bool {
    matches!(
        op.state,
        PendingOpState::Pending | PendingOpState::InProgress
    )
}

pub fn mark_in_progress(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE pending_op SET state = 'inProgress', updated_at = ?2 WHERE id = ?1",
        params![id, crate::repo::now_string()],
    )?;
    Ok(())
}

/// Claim an operation before executing it.
///
/// Like [`claim`], but an `inProgress` row is reclaimable as well: the previous
/// pass either crashed or failed to record completion. A settled row
/// (`cancelled`/`failed`/`done`) is never resurrected, which is what stops a
/// worker holding an older snapshot from re-running an operation that a
/// concurrent delete cancelled. Returns the number of rows changed (`0` means
/// the operation was settled by another actor).
pub fn claim_for_execution(conn: &Connection, id: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE pending_op SET state = 'inProgress', updated_at = ?2
          WHERE id = ?1 AND state IN ('pending', 'inProgress')",
        params![id, crate::repo::now_string()],
    )?)
}

/// Claim a `pending` operation for execution.
///
/// Unlike [`mark_in_progress`], the transition is conditional: an operation
/// that a discard/undo cancelled while it was still queued must not be
/// resurrected by a worker that read it a moment earlier. Returns the number
/// of rows changed (`0` means another actor already settled/cancelled it).
pub fn claim(conn: &Connection, id: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE pending_op SET state = 'inProgress', updated_at = ?2
          WHERE id = ?1 AND state = 'pending'",
        params![id, crate::repo::now_string()],
    )?)
}

pub fn mark_done(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE pending_op SET state = 'done', last_error = NULL, updated_at = ?2 WHERE id = ?1",
        params![id, crate::repo::now_string()],
    )?;
    Ok(())
}

/// Settle a claimed operation.
///
/// The `inProgress` precondition is what stops a worker that was still running
/// when the user discarded/sent the draft from turning `cancelled` back into
/// `done` (and making a remote side effect look settled). Returns the number
/// of rows changed.
pub fn finish(conn: &Connection, id: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE pending_op SET state = 'done', last_error = NULL, updated_at = ?2
          WHERE id = ?1 AND state = 'inProgress'",
        params![id, crate::repo::now_string()],
    )?)
}

/// Keep an operation queued but move its next attempt, without consuming a
/// retry (used by APPEND verification passes that cannot conclude yet).
///
/// A cancelled/failed/completed row is never resurrected.
pub fn defer(conn: &Connection, id: &str, at: DateTime<Utc>) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE pending_op SET state = 'pending', next_retry_at = ?2, updated_at = ?3
          WHERE id = ?1 AND state IN ('pending', 'inProgress')",
        params![id, crate::repo::dt_string(at), crate::repo::now_string()],
    )?)
}

/// Persist the intent to issue a wire command for this operation.
///
/// This must be written *before* the command leaves the client. A crash or an
/// ambiguous transport error afterwards therefore leaves durable evidence that
/// the operation may have reached the server; recovery and retries keep
/// `outcome_unknown` set, and undo refuses to cancel/restore it as "not sent".
pub fn mark_send_intent(conn: &Connection, id: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE pending_op SET send_intent = 1, updated_at = ?2
          WHERE id = ?1 AND state IN ('pending', 'inProgress')",
        params![id, crate::repo::now_string()],
    )?)
}

/// Increment attempts and schedule the next retry with exponential backoff.
///
/// An operation with a persisted send intent is *not* downgraded to an
/// untouched `pending` row: `outcome_unknown` is kept (and set) so undo and
/// rollback never mistake an ambiguous attempt for "nothing was sent".
pub fn mark_retry(
    conn: &Connection,
    id: &str,
    error: &str,
    now: DateTime<Utc>,
) -> Result<PendingOp> {
    let op = get_required(conn, id)?;
    let attempts = op.attempts.saturating_add(1);
    let next_retry = now + PendingOp::retry_delay(attempts);
    conn.execute(
        "UPDATE pending_op SET state = 'pending',
                outcome_unknown = CASE WHEN send_intent = 1 OR outcome_unknown = 1 THEN 1 ELSE 0 END,
                attempts = ?2, next_retry_at = ?3, last_error = ?4, updated_at = ?5
         WHERE id = ?1 AND state IN ('pending', 'inProgress')",
        params![
            id,
            attempts as i64,
            crate::repo::dt_string(next_retry),
            error,
            crate::repo::now_string()
        ],
    )?;
    get_required(conn, id)
}

/// Record that the outcome of an attempted command cannot be determined and
/// keep the operation retryable.
pub fn mark_outcome_unknown(conn: &Connection, id: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE pending_op SET outcome_unknown = 1, updated_at = ?2 WHERE id = ?1",
        params![id, crate::repo::now_string()],
    )?)
}

/// Mark as permanently failed (retries exhausted / non-retryable protocol error).
pub fn mark_failed(conn: &Connection, id: &str, error: &str) -> Result<()> {
    conn.execute(
        "UPDATE pending_op SET state = 'failed', last_error = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, error, crate::repo::now_string()],
    )?;
    Ok(())
}

pub fn cancel(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE pending_op SET state = 'cancelled', updated_at = ?2 WHERE id = ?1",
        params![id, crate::repo::now_string()],
    )?;
    Ok(())
}

/// Cancel an operation only if it has not started.
///
/// Unlike [`cancel`], this is conditional and returns the number of rows
/// changed. Undo uses it so an operation a worker already claimed (`inProgress`,
/// or whose outcome is unknown) is never turned into a `cancelled` row: the
/// wire command may be out and cancelling would misreport it.
pub fn cancel_if_pending(conn: &Connection, id: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE pending_op SET state = 'cancelled', updated_at = ?2
          WHERE id = ?1 AND state = 'pending'",
        params![id, crate::repo::now_string()],
    )?)
}

/// Isolate a `pending` operation without executing or completing it.
///
/// The row is stored as `cancelled`, a state every released build understands;
/// isolation is recorded on the payload instead (`quarantined` +
/// `quarantine_reason`). That distinction matters for downgrades: an older
/// binary parses an unknown state string as `pending` and would send the
/// delete, whereas `cancelled` is never selected by [`next_due`] or any
/// executor pass. The payload marker makes [`prune_finished`] retain the row as
/// an auditable record. The transition is conditional so a worker that claimed
/// the row first is never overwritten; returns the number of rows changed (`0`
/// means it was claimed or already settled).
pub fn quarantine(conn: &Connection, id: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE pending_op SET state = 'cancelled', updated_at = ?2
          WHERE id = ?1 AND state = 'pending'",
        params![id, crate::repo::now_string()],
    )?)
}

pub fn cancel_for_account(conn: &Connection, account_id: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE pending_op SET state = 'cancelled', updated_at = ?2
         WHERE account_id = ?1 AND state IN ('pending', 'inProgress')",
        params![account_id, crate::repo::now_string()],
    )?)
}

pub fn pending_count(conn: &Connection, account_id: Option<&str>) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM pending_op WHERE state = 'pending' AND (?1 IS NULL OR account_id = ?1)",
        params![account_id],
        |row| row.get(0),
    )?)
}

/// Remove completed rows whose *last transition* is older than `older_than`
/// (housekeeping).
///
/// Cleanup is based on `updated_at`, not `created_at`: an operation may sit in
/// the queue for longer than the retention window before it finally succeeds.
/// Deleting it immediately after settlement would erase the evidence that the
/// object changed to the uploaded value, letting a delta fetched before the
/// upload overwrite it with the older server copy.
///
/// A settled APPEND that only ever found a *candidate* copy (`candidateOnly`)
/// is retained: it records that the draft's remote copy cannot be attributed
/// to this client, and a later discard/send must keep that protection. The
/// same applies to an APPEND phase still marked `unknown` without a receipt:
/// its create may already have committed, and dropping the row would let a
/// later autosave upload a second copy. A
/// cancelled DELETE that may already have reached the server (`outcomeUnknown`)
/// is retained for the same reason: it is the durable evidence that the remote
/// side may differ from the local projection, which the next sync reconciles.
/// Both markers are rare, so retaining them indefinitely stays cheap.
///
/// A quarantined row is `cancelled` (a state older builds also understand)
/// plus a `quarantined` payload marker, so a legacy intent that could not be
/// attributed survives as an auditable record instead of being pruned like an
/// ordinary settled row.
///
/// A confirmed move-style operation is retained while it is still the only
/// evidence for an unbound projection: the `message` row it names is still
/// `target_pending` in the destination folder, the row's identity slot is
/// unowned or owned by this operation, **and** the row does not carry the same
/// confirmed destination in `move_target_remote_id`. New confirmations write
/// that identity and its owner onto the row (which is why the operation may
/// then be pruned); this guard covers receipts recorded before those columns
/// existed. A newer trip's claim makes this receipt historical, so it is not
/// retained and cannot later back an alias for the new projection. Protecting
/// the projection this way is deliberately tied to its binding state instead
/// of extending the retention window.
pub fn prune_finished(conn: &Connection, older_than: DateTime<Utc>) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM pending_op
          WHERE state IN ('done', 'cancelled', 'failed') AND updated_at < ?1
            AND NOT (
                (kind = 'appendDraft'
                 AND (COALESCE(json_extract(payload_json, '$.candidateOnly'), 0) = 1
                      OR COALESCE(json_extract(payload_json, '$.outcomeUnknown'), 0) = 1
                      -- A create whose phase is Unknown and returned no receipt
                      -- may already exist on the server. Keeping the row is
                      -- what stops a later autosave from POSTing a duplicate,
                      -- even after the operation exhausted its retries.
                      OR (json_extract(payload_json, '$.appendState') = 'unknown'
                          AND json_extract(payload_json, '$.appendedUid') IS NULL
                          AND COALESCE(json_extract(payload_json, '$.appendedRemoteId'), '') = '')))
                OR (kind = 'deleteRemoteDraft'
                    AND (COALESCE(json_extract(payload_json, '$.outcomeUnknown'), 0) = 1
                         OR COALESCE(json_extract(payload_json, '$.quarantined'), 0) = 1))
                -- A confirmed move destination that has not been bound yet
                -- must not lose its only durable evidence. The receipt names
                -- both the local row and the confirmed Graph id; the operation
                -- is retained until the row carries that id itself (or the
                -- projection is gone, rebound, or moved elsewhere).
                OR (kind IN ('move', 'archive', 'markJunk', 'delete')
                    AND EXISTS (
                        SELECT 1
                          FROM json_each(
                                   CASE WHEN json_valid(payload_json)
                                        THEN COALESCE(json_extract(payload_json, '$.outcomes'), '[]')
                                        ELSE '[]' END) AS outcome
                         WHERE json_extract(outcome.value, '$.state') = 'confirmed'
                           AND COALESCE(json_extract(outcome.value, '$.targetRemoteId'), '') <> ''
                           AND EXISTS (
                               SELECT 1 FROM message m
                                WHERE m.id = json_extract(outcome.value, '$.messageId')
                                  AND m.account_id = pending_op.account_id
                                  AND m.folder_id = json_extract(outcome.value, '$.targetFolderId')
                                  AND m.uid IS NULL
                                  AND m.target_pending = 1
                                  -- The receipt only protects a projection whose
                                  -- identity this operation owns (or that has no
                                  -- owner yet, for rows written before the
                                  -- ownership column). A newer trip's claim makes
                                  -- this receipt historical evidence that must not
                                  -- back an alias for the new projection.
                                  -- A blank token was written by legacy rollback
                                  -- code and means no owner; it must not stop
                                  -- this receipt from protecting the row.
                                  AND (NULLIF(NULLIF(TRIM(m.move_target_op_id), ''), '0') IS NULL
                                       OR m.move_target_op_id = pending_op.id
                                       OR COALESCE(m.move_target_seq, 0) < pending_op.seq)
                                  AND (COALESCE(m.move_target_remote_id, '')
                                           <> json_extract(outcome.value, '$.targetRemoteId')
                                       OR COALESCE(m.move_target_folder_id, '') <> m.folder_id)
                           )
                    ))
            )",
        params![crate::repo::dt_string(older_than)],
    )?)
}

/// Replace a queued operation's payload.
///
/// Used for operations that must persist mid-flight progress (an APPEND whose
/// literal was sent but whose completion is still unknown) so a retry can
/// resume from that phase instead of repeating the server command.
pub fn update_payload(conn: &Connection, id: &str, payload: &serde_json::Value) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE pending_op SET payload_json = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, json_text(payload)?, crate::repo::now_string()],
    )?)
}

/// Payload deserialized into a concrete type.
pub fn payload<T: for<'de> serde::Deserialize<'de>>(op: &PendingOp) -> Result<T> {
    serde_json::from_value(op.payload.clone())
        .map_err(|error| AppError::internal(format!("invalid pending op payload: {error}")))
}
