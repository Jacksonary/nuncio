//! Per-occurrence reminder dismiss/snooze state.
//!
//! The table is polymorphic (`kind` = `event` | `task`) so the reminder engine
//! uses one persistence path for calendar occurrences and task reminders.  The
//! `event_reminder_state` helpers are kept as thin wrappers so existing callers
//! do not need to know about the kind column.

use rusqlite::{params, Connection, OptionalExtension};

use crate::repo::{now_string, parse_dt_opt};
use crate::Result;

/// Reminder kind for a calendar occurrence.
pub const KIND_EVENT: &str = "event";
/// Reminder kind for a task.
pub const KIND_TASK: &str = "task";

/// Current reminder state for an occurrence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReminderState {
    pub dismissed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub snoozed_until: Option<chrono::DateTime<chrono::Utc>>,
}

pub fn get_for(
    conn: &Connection,
    kind: &str,
    entity_id: &str,
    instance_start: &str,
) -> Result<Option<ReminderState>> {
    let row: Option<(Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT dismissed_at, snoozed_until FROM reminder_state
             WHERE kind = ?1 AND entity_id = ?2 AND instance_start = ?3",
            params![kind, entity_id, instance_start],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match row {
        Some((dismissed, snoozed)) => Ok(Some(ReminderState {
            dismissed_at: parse_dt_opt(dismissed)?,
            snoozed_until: parse_dt_opt(snoozed)?,
        })),
        None => Ok(None),
    }
}

pub fn dismiss_for(
    conn: &Connection,
    kind: &str,
    entity_id: &str,
    instance_start: &str,
) -> Result<()> {
    let now = now_string();
    conn.execute(
        "INSERT INTO reminder_state
             (kind, entity_id, instance_start, dismissed_at, snoozed_until, updated_at)
         VALUES (?1, ?2, ?3, ?4, NULL, ?4)
         ON CONFLICT(kind, entity_id, instance_start) DO UPDATE SET
             dismissed_at = excluded.dismissed_at,
             snoozed_until = NULL,
             updated_at = excluded.updated_at",
        params![kind, entity_id, instance_start, now],
    )?;
    Ok(())
}

pub fn snooze_for(
    conn: &Connection,
    kind: &str,
    entity_id: &str,
    instance_start: &str,
    minutes: u32,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<chrono::DateTime<chrono::Utc>> {
    let until = now + chrono::Duration::minutes(minutes.max(1) as i64);
    conn.execute(
        "INSERT INTO reminder_state
             (kind, entity_id, instance_start, dismissed_at, snoozed_until, updated_at)
         VALUES (?1, ?2, ?3, NULL, ?4, ?5)
         ON CONFLICT(kind, entity_id, instance_start) DO UPDATE SET
             dismissed_at = NULL,
             snoozed_until = excluded.snoozed_until,
             updated_at = excluded.updated_at",
        params![
            kind,
            entity_id,
            instance_start,
            crate::repo::dt_string(until),
            crate::repo::dt_string(now)
        ],
    )?;
    Ok(until)
}

/// True when a reminder should not fire for this occurrence at `now`.
pub fn is_suppressed_for(
    conn: &Connection,
    kind: &str,
    entity_id: &str,
    instance_start: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<bool> {
    match get_for(conn, kind, entity_id, instance_start)? {
        Some(state) => {
            if state.dismissed_at.is_some() {
                return Ok(true);
            }
            if let Some(until) = state.snoozed_until {
                return Ok(until > now);
            }
            Ok(false)
        }
        None => Ok(false),
    }
}

pub fn clear_for_entity(conn: &Connection, kind: &str, entity_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM reminder_state WHERE kind = ?1 AND entity_id = ?2",
        params![kind, entity_id],
    )?;
    Ok(())
}

pub fn clear_for_occurrence_for(
    conn: &Connection,
    kind: &str,
    entity_id: &str,
    instance_start: &str,
) -> Result<()> {
    conn.execute(
        "DELETE FROM reminder_state
         WHERE kind = ?1 AND entity_id = ?2 AND instance_start = ?3",
        params![kind, entity_id, instance_start],
    )?;
    Ok(())
}

/// Task reminders whose snooze expired and must be evaluated again, as
/// `(task_id, reminder_key)` ordered by the snooze deadline.
pub fn due_task_snoozes(
    conn: &Connection,
    now: chrono::DateTime<chrono::Utc>,
    limit: usize,
) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT entity_id, instance_start FROM reminder_state
         WHERE kind = 'task' AND dismissed_at IS NULL AND snoozed_until IS NOT NULL
           AND snoozed_until <= ?1
         ORDER BY snoozed_until ASC LIMIT ?2",
    )?;
    let mut rows = stmt.query(params![
        crate::repo::dt_string(now),
        limit.min(i64::MAX as usize) as i64
    ])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((row.get(0)?, row.get(1)?));
    }
    Ok(out)
}

/// Remove task reminder state that no longer corresponds to an existing task.
pub fn prune_task_state(conn: &Connection) -> Result<usize> {
    let removed = conn.execute(
        "DELETE FROM reminder_state
         WHERE kind = 'task' AND entity_id NOT IN (SELECT id FROM task)",
        [],
    )?;
    Ok(removed)
}

/* ───────────────────────── event compatibility API ───────────────────── */

pub fn get(
    conn: &Connection,
    event_id: &str,
    instance_start: &str,
) -> Result<Option<ReminderState>> {
    get_for(conn, KIND_EVENT, event_id, instance_start)
}

pub fn dismiss(conn: &Connection, event_id: &str, instance_start: &str) -> Result<()> {
    dismiss_for(conn, KIND_EVENT, event_id, instance_start)
}

pub fn snooze(
    conn: &Connection,
    event_id: &str,
    instance_start: &str,
    minutes: u32,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<chrono::DateTime<chrono::Utc>> {
    snooze_for(conn, KIND_EVENT, event_id, instance_start, minutes, now)
}

pub fn is_suppressed(
    conn: &Connection,
    event_id: &str,
    instance_start: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<bool> {
    is_suppressed_for(conn, KIND_EVENT, event_id, instance_start, now)
}

pub fn clear_for_event(conn: &Connection, event_id: &str) -> Result<()> {
    clear_for_entity(conn, KIND_EVENT, event_id)
}

pub fn clear_for_occurrence(conn: &Connection, event_id: &str, instance_start: &str) -> Result<()> {
    clear_for_occurrence_for(conn, KIND_EVENT, event_id, instance_start)
}
