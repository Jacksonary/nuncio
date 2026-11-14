//! Persistent operation queue (`pending_op`) used by every sync engine.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};

use crate::error::AppError;
use crate::models::{new_id, NewPendingOp, PendingOp, PendingOpState};
use crate::repo::{get_opt_dt, get_opt_string, get_string, json_or_default, json_text};
use crate::Result;

const COLUMNS: &str = "\
    id, account_id, kind, payload_json, state, attempts, next_retry_at, description,\
    last_error, created_at";

fn row_to_op(row: &Row<'_>) -> Result<PendingOp> {
    let payload: Option<String> = row.get("payload_json")?;
    let state: String = get_string(row, "state")?;
    Ok(PendingOp {
        id: get_string(row, "id")?,
        account_id: get_string(row, "account_id")?,
        kind: get_string(row, "kind")?,
        payload: json_or_default(payload.as_deref(), "payload_json"),
        state: state.parse().unwrap_or(PendingOpState::Pending),
        attempts: row.get::<_, i64>("attempts")?.max(0) as u32,
        next_retry_at: get_opt_dt(row, "next_retry_at")?,
        created_at: crate::repo::get_dt(row, "created_at")?,
        description: get_string(row, "description")?,
        last_error: get_opt_string(row, "last_error")?,
    })
}

pub fn enqueue(conn: &Connection, op: NewPendingOp) -> Result<PendingOp> {
    let id = new_id();
    let created_at = crate::repo::now_string();
    conn.execute(
        "INSERT INTO pending_op (id, account_id, kind, payload_json, state, attempts, next_retry_at,
                                 description, last_error, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'pending', 0, NULL, ?5, NULL, ?6, ?6)",
        params![
            id,
            op.account_id,
            op.kind,
            json_text(&op.payload)?,
            op.description,
            created_at
        ],
    )?;
    get_required(conn, &id)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<PendingOp>> {
    let sql = format!("SELECT {COLUMNS} FROM pending_op WHERE id = ?1");
    crate::repo::query_one(conn, &sql, params![id], row_to_op)
}

pub fn get_required(conn: &Connection, id: &str) -> Result<PendingOp> {
    get(conn, id)?.ok_or_else(|| AppError::NotFound(format!("pending operation {id} not found")))
}

/// Pending operations whose retry time is due, oldest first.
pub fn next_due(conn: &Connection, now: DateTime<Utc>, limit: u32) -> Result<Vec<PendingOp>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM pending_op
         WHERE state = 'pending' AND (next_retry_at IS NULL OR next_retry_at <= ?1)
         ORDER BY created_at ASC LIMIT ?2"
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
         ORDER BY created_at ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![account_id])?;
    let mut ops = Vec::new();
    while let Some(row) = rows.next()? {
        ops.push(row_to_op(row)?);
    }
    Ok(ops)
}

pub fn mark_in_progress(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE pending_op SET state = 'inProgress', updated_at = ?2 WHERE id = ?1",
        params![id, crate::repo::now_string()],
    )?;
    Ok(())
}

pub fn mark_done(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE pending_op SET state = 'done', last_error = NULL, updated_at = ?2 WHERE id = ?1",
        params![id, crate::repo::now_string()],
    )?;
    Ok(())
}

/// Increment attempts and schedule the next retry with exponential backoff.
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
        "UPDATE pending_op SET state = 'pending', attempts = ?2, next_retry_at = ?3, last_error = ?4, updated_at = ?5
         WHERE id = ?1",
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

/// Remove completed rows older than `older_than` (housekeeping).
pub fn prune_finished(conn: &Connection, older_than: DateTime<Utc>) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM pending_op WHERE state IN ('done', 'cancelled', 'failed') AND created_at < ?1",
        params![crate::repo::dt_string(older_than)],
    )?)
}

/// Payload deserialized into a concrete type.
pub fn payload<T: for<'de> serde::Deserialize<'de>>(op: &PendingOp) -> Result<T> {
    serde_json::from_value(op.payload.clone())
        .map_err(|error| AppError::internal(format!("invalid pending op payload: {error}")))
}
