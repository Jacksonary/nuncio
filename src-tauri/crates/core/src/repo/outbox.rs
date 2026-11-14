//! Outbox repository for messages waiting to be sent over SMTP.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};

use crate::error::AppError;
use crate::models::{Address, OutboxItem, OutboxState};
use crate::repo::{get_opt_dt, get_opt_string, get_string, json_or_default, json_text, now_string};
use crate::Result;

const COLUMNS: &str = "\
    id, account_id, draft_id, recipients_json, subject, rfc822_path, state, attempts,\
    next_retry_at, last_error, created_at, sent_at, sending_started_at";

fn row_to_item(row: &Row<'_>) -> Result<OutboxItem> {
    let recipients: Option<String> = row.get("recipients_json")?;
    let state: String = get_string(row, "state")?;
    Ok(OutboxItem {
        id: get_string(row, "id")?,
        account_id: get_string(row, "account_id")?,
        draft_id: get_opt_string(row, "draft_id")?,
        recipients: json_or_default(recipients.as_deref(), "recipients_json"),
        subject: get_string(row, "subject")?,
        rfc822_path: get_string(row, "rfc822_path")?,
        state: state.parse().unwrap_or(OutboxState::Pending),
        attempts: row.get::<_, i64>("attempts")?.max(0) as u32,
        next_retry_at: get_opt_dt(row, "next_retry_at")?,
        last_error: get_opt_string(row, "last_error")?,
        created_at: crate::repo::get_dt(row, "created_at")?,
        sent_at: get_opt_dt(row, "sent_at")?,
        sending_started_at: get_opt_dt(row, "sending_started_at")?,
    })
}

pub fn enqueue(conn: &Connection, item: &OutboxItem) -> Result<OutboxItem> {
    conn.execute(
        "INSERT INTO outbox (id, account_id, draft_id, recipients_json, subject, rfc822_path, state,
                             attempts, next_retry_at, last_error, created_at, sent_at,
                             sending_started_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL)
         ON CONFLICT(id) DO UPDATE SET
             recipients_json = excluded.recipients_json,
             subject = excluded.subject,
             rfc822_path = excluded.rfc822_path,
             state = excluded.state,
             attempts = excluded.attempts,
             next_retry_at = excluded.next_retry_at,
             last_error = excluded.last_error,
             sent_at = excluded.sent_at",
        params![
            item.id,
            item.account_id,
            item.draft_id,
            json_text(&item.recipients)?,
            item.subject,
            item.rfc822_path,
            item.state.as_str(),
            item.attempts as i64,
            item.next_retry_at.map(crate::repo::dt_string),
            item.last_error,
            crate::repo::dt_string(item.created_at),
            item.sent_at.map(crate::repo::dt_string)
        ],
    )?;
    get_required(conn, &item.id)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<OutboxItem>> {
    let sql = format!("SELECT {COLUMNS} FROM outbox WHERE id = ?1");
    crate::repo::query_one(conn, &sql, params![id], row_to_item)
}

pub fn get_required(conn: &Connection, id: &str) -> Result<OutboxItem> {
    get(conn, id)?.ok_or_else(|| AppError::NotFound(format!("outbox item {id} not found")))
}

pub fn list(conn: &Connection, account_id: Option<&str>) -> Result<Vec<OutboxItem>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM outbox WHERE (?1 IS NULL OR account_id = ?1) AND state <> 'sent'
         ORDER BY created_at ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![account_id])?;
    let mut items = Vec::new();
    while let Some(row) = rows.next()? {
        items.push(row_to_item(row)?);
    }
    Ok(items)
}

/// Pending items ready to be sent (backoff honoured).
pub fn next_due(conn: &Connection, now: DateTime<Utc>, limit: u32) -> Result<Vec<OutboxItem>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM outbox
         WHERE state = 'pending' AND (next_retry_at IS NULL OR next_retry_at <= ?1)
         ORDER BY created_at ASC LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![crate::repo::dt_string(now), limit.max(1) as i64])?;
    let mut items = Vec::new();
    while let Some(row) = rows.next()? {
        items.push(row_to_item(row)?);
    }
    Ok(items)
}

pub fn mark_sending(conn: &Connection, id: &str) -> Result<()> {
    let changed = conn.execute(
        "UPDATE outbox SET state = 'sending' WHERE id = ?1",
        params![id],
    )?;
    if changed == 0 {
        return Err(AppError::NotFound(format!("outbox item {id} not found")));
    }
    Ok(())
}

pub fn mark_sent(conn: &Connection, id: &str) -> Result<()> {
    let now = now_string();
    conn.execute(
        "UPDATE outbox SET state = 'sent', sent_at = ?2, last_error = NULL,
                           sending_started_at = NULL
         WHERE id = ?1",
        params![id, now],
    )?;
    Ok(())
}

/// Record a failed attempt and schedule a retry. Attempts beyond `max_attempts`
/// mark the item as permanently failed.
pub fn mark_failed(
    conn: &Connection,
    id: &str,
    error: &str,
    now: DateTime<Utc>,
    max_attempts: u32,
) -> Result<OutboxState> {
    let item = get_required(conn, id)?;
    let attempts = item.attempts.saturating_add(1);
    let (state, next_retry) = if attempts >= max_attempts {
        (OutboxState::Failed, None)
    } else {
        let delay = crate::models::PendingOp::retry_delay(attempts);
        (OutboxState::Pending, Some(now + delay))
    };
    conn.execute(
        "UPDATE outbox SET state = ?2, attempts = ?3, next_retry_at = ?4, last_error = ?5,
                           sending_started_at = NULL
         WHERE id = ?1",
        params![
            id,
            state.as_str(),
            attempts as i64,
            next_retry.map(crate::repo::dt_string),
            error
        ],
    )?;
    Ok(state)
}

pub fn retry_now(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE outbox SET state = 'pending', next_retry_at = NULL, sending_started_at = NULL
         WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

pub fn remove(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM outbox WHERE id = ?1", params![id])?;
    Ok(())
}

/// Remove an item only when it is not currently transmitting.
///
/// Returns the number of rows deleted; `0` means the item is `sending` or gone,
/// so the caller must report a conflict instead of claiming success.
pub fn remove_when_not_sending(conn: &Connection, id: &str) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM outbox WHERE id = ?1 AND state <> 'sending'",
        params![id],
    )?)
}

pub fn pending_count(conn: &Connection, account_id: Option<&str>) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM outbox WHERE state <> 'sent' AND (?1 IS NULL OR account_id = ?1)",
        params![account_id],
        |row| row.get(0),
    )?)
}

/// Prune sent items older than `keep` (cleanup task).
pub fn prune_sent(conn: &Connection, older_than: DateTime<Utc>) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM outbox WHERE state = 'sent' AND sent_at IS NOT NULL AND sent_at < ?1",
        params![crate::repo::dt_string(older_than)],
    )?)
}

/// Helper used by the mail service to decode an outbox row's recipients.
pub fn recipients(item: &OutboxItem) -> &[Address] {
    &item.recipients
}
