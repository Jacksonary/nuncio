//! PIM-owned persistence: provider calendar mapping and reminder bookkeeping.
//!
//! These tables live in the `pim` migration namespace (see
//! `migrations/0001_pim.sql`) and never duplicate core data.

use chrono::Utc;
use nuncio_core::repo::{dt_string, now_string, parse_dt_opt};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::Result;

pub const PIM_NAMESPACE: &str = "pim";
pub const PIM_MIGRATIONS: &[(u32, &str)] = &[
    (1, include_str!("../migrations/0001_pim.sql")),
    (2, include_str!("../migrations/0002_contacts_tasks.sql")),
    (3, include_str!("../migrations/0003_meta.sql")),
];

/// Provider calendar identity for one local [`nuncio_core::models::Calendar`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarRemote {
    pub calendar_id: String,
    pub account_id: String,
    pub provider: String,
    pub remote_id: String,
    pub href: Option<String>,
}

impl CalendarRemote {
    pub fn new(
        calendar_id: impl Into<String>,
        account_id: impl Into<String>,
        provider: impl Into<String>,
        remote_id: impl Into<String>,
        href: Option<String>,
    ) -> Self {
        Self {
            calendar_id: calendar_id.into(),
            account_id: account_id.into(),
            provider: provider.into(),
            remote_id: remote_id.into(),
            href,
        }
    }
}

pub fn get_by_calendar(conn: &Connection, calendar_id: &str) -> Result<Option<CalendarRemote>> {
    let row: Option<(String, String, String, String, Option<String>)> = conn
        .query_row(
            "SELECT calendar_id, account_id, provider, remote_id, href
             FROM pim_calendar_map WHERE calendar_id = ?1",
            params![calendar_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(
        |(calendar_id, account_id, provider, remote_id, href)| CalendarRemote {
            calendar_id,
            account_id,
            provider,
            remote_id,
            href,
        },
    ))
}

pub fn get_by_remote(
    conn: &Connection,
    account_id: &str,
    provider: &str,
    remote_id: &str,
) -> Result<Option<CalendarRemote>> {
    let row: Option<(String, String, String, String, Option<String>)> = conn
        .query_row(
            "SELECT calendar_id, account_id, provider, remote_id, href
             FROM pim_calendar_map
             WHERE account_id = ?1 AND provider = ?2 AND remote_id = ?3",
            params![account_id, provider, remote_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(
        |(calendar_id, account_id, provider, remote_id, href)| CalendarRemote {
            calendar_id,
            account_id,
            provider,
            remote_id,
            href,
        },
    ))
}

pub fn set(conn: &Connection, mapping: &CalendarRemote) -> Result<()> {
    conn.execute(
        "INSERT INTO pim_calendar_map
             (calendar_id, account_id, provider, remote_id, href, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
         ON CONFLICT(calendar_id) DO UPDATE SET
             account_id = excluded.account_id,
             provider = excluded.provider,
             remote_id = excluded.remote_id,
             href = excluded.href,
             updated_at = excluded.updated_at",
        params![
            mapping.calendar_id,
            mapping.account_id,
            mapping.provider,
            mapping.remote_id,
            mapping.href,
            now_string()
        ],
    )?;
    Ok(())
}

pub fn delete(conn: &Connection, calendar_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM pim_calendar_map WHERE calendar_id = ?1",
        params![calendar_id],
    )?;
    Ok(())
}

pub fn list_for_account(conn: &Connection, account_id: &str) -> Result<Vec<CalendarRemote>> {
    let mut stmt = conn.prepare(
        "SELECT calendar_id, account_id, provider, remote_id, href
         FROM pim_calendar_map WHERE account_id = ?1 ORDER BY created_at ASC",
    )?;
    let mut rows = stmt.query(params![account_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(CalendarRemote {
            calendar_id: row.get(0)?,
            account_id: row.get(1)?,
            provider: row.get(2)?,
            remote_id: row.get(3)?,
            href: row.get(4)?,
        });
    }
    Ok(out)
}

/* ───────────────── address book / task list mapping ─────────────────── */

/// Provider identity for one local [`nuncio_core::models::AddressBook`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressBookRemote {
    pub address_book_id: String,
    pub account_id: String,
    pub provider: String,
    pub remote_id: String,
    pub href: Option<String>,
}

impl AddressBookRemote {
    pub fn new(
        address_book_id: impl Into<String>,
        account_id: impl Into<String>,
        provider: impl Into<String>,
        remote_id: impl Into<String>,
        href: Option<String>,
    ) -> Self {
        Self {
            address_book_id: address_book_id.into(),
            account_id: account_id.into(),
            provider: provider.into(),
            remote_id: remote_id.into(),
            href,
        }
    }
}

pub fn get_book_by_local(
    conn: &Connection,
    address_book_id: &str,
) -> Result<Option<AddressBookRemote>> {
    let row: Option<(String, String, String, String, Option<String>)> = conn
        .query_row(
            "SELECT address_book_id, account_id, provider, remote_id, href
             FROM pim_address_book_map WHERE address_book_id = ?1",
            params![address_book_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(
        |(address_book_id, account_id, provider, remote_id, href)| AddressBookRemote {
            address_book_id,
            account_id,
            provider,
            remote_id,
            href,
        },
    ))
}

pub fn get_book_by_remote(
    conn: &Connection,
    account_id: &str,
    provider: &str,
    remote_id: &str,
) -> Result<Option<AddressBookRemote>> {
    let row: Option<(String, String, String, String, Option<String>)> = conn
        .query_row(
            "SELECT address_book_id, account_id, provider, remote_id, href
             FROM pim_address_book_map
             WHERE account_id = ?1 AND provider = ?2 AND remote_id = ?3",
            params![account_id, provider, remote_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(
        |(address_book_id, account_id, provider, remote_id, href)| AddressBookRemote {
            address_book_id,
            account_id,
            provider,
            remote_id,
            href,
        },
    ))
}

pub fn set_book(conn: &Connection, mapping: &AddressBookRemote) -> Result<()> {
    conn.execute(
        "INSERT INTO pim_address_book_map
             (address_book_id, account_id, provider, remote_id, href, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
         ON CONFLICT(address_book_id) DO UPDATE SET
             account_id = excluded.account_id,
             provider = excluded.provider,
             remote_id = excluded.remote_id,
             href = excluded.href,
             updated_at = excluded.updated_at",
        params![
            mapping.address_book_id,
            mapping.account_id,
            mapping.provider,
            mapping.remote_id,
            mapping.href,
            now_string()
        ],
    )?;
    Ok(())
}

pub fn delete_book(conn: &Connection, address_book_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM pim_address_book_map WHERE address_book_id = ?1",
        params![address_book_id],
    )?;
    Ok(())
}

pub fn list_books_for_account(
    conn: &Connection,
    account_id: &str,
) -> Result<Vec<AddressBookRemote>> {
    let mut stmt = conn.prepare(
        "SELECT address_book_id, account_id, provider, remote_id, href
         FROM pim_address_book_map WHERE account_id = ?1 ORDER BY created_at ASC",
    )?;
    let mut rows = stmt.query(params![account_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(AddressBookRemote {
            address_book_id: row.get(0)?,
            account_id: row.get(1)?,
            provider: row.get(2)?,
            remote_id: row.get(3)?,
            href: row.get(4)?,
        });
    }
    Ok(out)
}

/// Provider identity for one local [`nuncio_core::models::TaskList`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskListRemote {
    pub task_list_id: String,
    pub account_id: String,
    pub provider: String,
    pub remote_id: String,
    pub href: Option<String>,
}

impl TaskListRemote {
    pub fn new(
        task_list_id: impl Into<String>,
        account_id: impl Into<String>,
        provider: impl Into<String>,
        remote_id: impl Into<String>,
        href: Option<String>,
    ) -> Self {
        Self {
            task_list_id: task_list_id.into(),
            account_id: account_id.into(),
            provider: provider.into(),
            remote_id: remote_id.into(),
            href,
        }
    }
}

pub fn get_task_list_by_local(
    conn: &Connection,
    task_list_id: &str,
) -> Result<Option<TaskListRemote>> {
    let row: Option<(String, String, String, String, Option<String>)> = conn
        .query_row(
            "SELECT task_list_id, account_id, provider, remote_id, href
             FROM pim_task_list_map WHERE task_list_id = ?1",
            params![task_list_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(
        |(task_list_id, account_id, provider, remote_id, href)| TaskListRemote {
            task_list_id,
            account_id,
            provider,
            remote_id,
            href,
        },
    ))
}

pub fn get_task_list_by_remote(
    conn: &Connection,
    account_id: &str,
    provider: &str,
    remote_id: &str,
) -> Result<Option<TaskListRemote>> {
    let row: Option<(String, String, String, String, Option<String>)> = conn
        .query_row(
            "SELECT task_list_id, account_id, provider, remote_id, href
             FROM pim_task_list_map
             WHERE account_id = ?1 AND provider = ?2 AND remote_id = ?3",
            params![account_id, provider, remote_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(
        |(task_list_id, account_id, provider, remote_id, href)| TaskListRemote {
            task_list_id,
            account_id,
            provider,
            remote_id,
            href,
        },
    ))
}

pub fn set_task_list(conn: &Connection, mapping: &TaskListRemote) -> Result<()> {
    conn.execute(
        "INSERT INTO pim_task_list_map
             (task_list_id, account_id, provider, remote_id, href, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
         ON CONFLICT(task_list_id) DO UPDATE SET
             account_id = excluded.account_id,
             provider = excluded.provider,
             remote_id = excluded.remote_id,
             href = excluded.href,
             updated_at = excluded.updated_at",
        params![
            mapping.task_list_id,
            mapping.account_id,
            mapping.provider,
            mapping.remote_id,
            mapping.href,
            now_string()
        ],
    )?;
    Ok(())
}

pub fn delete_task_list(conn: &Connection, task_list_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM pim_task_list_map WHERE task_list_id = ?1",
        params![task_list_id],
    )?;
    Ok(())
}

pub fn list_task_lists_for_account(
    conn: &Connection,
    account_id: &str,
) -> Result<Vec<TaskListRemote>> {
    let mut stmt = conn.prepare(
        "SELECT task_list_id, account_id, provider, remote_id, href
         FROM pim_task_list_map WHERE account_id = ?1 ORDER BY created_at ASC",
    )?;
    let mut rows = stmt.query(params![account_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(TaskListRemote {
            task_list_id: row.get(0)?,
            account_id: row.get(1)?,
            provider: row.get(2)?,
            remote_id: row.get(3)?,
            href: row.get(4)?,
        });
    }
    Ok(out)
}

/* ─────────────────────── task step remote mapping ──────────────────── */

pub fn step_local_id(conn: &Connection, task_id: &str, remote_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT step_id FROM pim_task_step_map WHERE task_id = ?1 AND remote_id = ?2 LIMIT 1",
            params![task_id, remote_id],
            |row| row.get(0),
        )
        .optional()?)
}

pub fn set_step_mapping(
    conn: &Connection,
    step_id: &str,
    task_id: &str,
    remote_id: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO pim_task_step_map (step_id, task_id, remote_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(step_id) DO UPDATE SET
             task_id = excluded.task_id,
             remote_id = excluded.remote_id,
             updated_at = excluded.updated_at",
        params![step_id, task_id, remote_id, now_string()],
    )?;
    Ok(())
}

pub fn step_remote_id(conn: &Connection, step_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT remote_id FROM pim_task_step_map WHERE step_id = ?1",
            params![step_id],
            |row| row.get(0),
        )
        .optional()?)
}

pub fn delete_step_mapping(conn: &Connection, step_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM pim_task_step_map WHERE step_id = ?1",
        params![step_id],
    )?;
    Ok(())
}

pub fn step_mappings_for_task(conn: &Connection, task_id: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT step_id, remote_id FROM pim_task_step_map WHERE task_id = ?1 ORDER BY created_at ASC",
    )?;
    let mut rows = stmt.query(params![task_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((row.get(0)?, row.get(1)?));
    }
    Ok(out)
}

pub fn clear_step_mappings(conn: &Connection, task_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM pim_task_step_map WHERE task_id = ?1",
        params![task_id],
    )?;
    Ok(())
}

/* ───────────────────────── task resource hrefs ─────────────────────── */

pub fn set_task_href(conn: &Connection, task_id: &str, href: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO pim_task_resource (task_id, href, updated_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(task_id) DO UPDATE SET href = excluded.href, updated_at = excluded.updated_at",
        params![task_id, href, now_string()],
    )?;
    Ok(())
}

pub fn task_href(conn: &Connection, task_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT href FROM pim_task_resource WHERE task_id = ?1",
            params![task_id],
            |row| row.get(0),
        )
        .optional()?)
}

pub fn task_by_href(conn: &Connection, list_id: &str, href: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT t.id FROM task t JOIN pim_task_resource r ON r.task_id = t.id
             WHERE t.list_id = ?1 AND r.href = ?2 LIMIT 1",
            params![list_id, href],
            |row| row.get(0),
        )
        .optional()?)
}

/// `(href, etag)` pairs for a CalDAV task collection.
pub fn task_etags(conn: &Connection, list_id: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT r.href, t.etag FROM task t JOIN pim_task_resource r ON r.task_id = t.id
         WHERE t.list_id = ?1 AND t.etag IS NOT NULL",
    )?;
    let mut rows = stmt.query(params![list_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((row.get(0)?, row.get(1)?));
    }
    Ok(out)
}

/// Map a provider child-task id back to its local step and parent task.
pub fn step_lookup_by_remote(
    conn: &Connection,
    remote_id: &str,
) -> Result<Option<(String, String, String)>> {
    Ok(conn
        .query_row(
            "SELECT step_id, task_id, remote_id FROM pim_task_step_map WHERE remote_id = ?1 LIMIT 1",
            params![remote_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?)
}

/* ─────────────────────── task reminder bookkeeping ─────────────────── */

pub fn task_reminder_fired(conn: &Connection, task_id: &str, key: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pim_task_reminder_fired WHERE task_id = ?1 AND reminder_key = ?2",
        params![task_id, key],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

pub fn mark_task_reminder_fired(conn: &Connection, task_id: &str, key: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO pim_task_reminder_fired (task_id, reminder_key, fired_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(task_id, reminder_key) DO UPDATE SET fired_at = excluded.fired_at",
        params![task_id, key, now_string()],
    )?;
    Ok(())
}

/// Allow a task reminder to fire again (used after snoozing).
pub fn clear_task_reminder_fired(conn: &Connection, task_id: &str, key: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM pim_task_reminder_fired WHERE task_id = ?1 AND reminder_key = ?2",
        params![task_id, key],
    )?;
    Ok(())
}

/// True when a reminder for this occurrence was already delivered.
pub fn reminder_fired(conn: &Connection, event_id: &str, instance_start: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pim_reminder_fired
         WHERE event_id = ?1 AND instance_start = ?2",
        params![event_id, instance_start],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Record a delivered reminder, keeping one row per occurrence.
pub fn mark_reminder_fired(conn: &Connection, event_id: &str, instance_start: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO pim_reminder_fired (event_id, instance_start, fired_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(event_id, instance_start) DO UPDATE SET fired_at = excluded.fired_at",
        params![event_id, instance_start, now_string()],
    )?;
    Ok(())
}

/// Allow an occurrence to fire again (used after snoozing).
pub fn clear_reminder_fired(conn: &Connection, event_id: &str, instance_start: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM pim_reminder_fired WHERE event_id = ?1 AND instance_start = ?2",
        params![event_id, instance_start],
    )?;
    Ok(())
}

/// Remove fired markers for events that no longer exist.
pub fn prune_reminder_fired(conn: &Connection) -> Result<usize> {
    let cutoff = dt_string(Utc::now() - chrono::Duration::days(35));
    let removed = conn.execute(
        "DELETE FROM pim_reminder_fired
         WHERE event_id NOT IN (SELECT id FROM event) OR fired_at < ?1",
        params![cutoff],
    )?;
    let removed_tasks = conn.execute(
        "DELETE FROM pim_task_reminder_fired
         WHERE task_id NOT IN (SELECT id FROM task) OR fired_at < ?1",
        params![cutoff],
    )?;
    Ok(removed + removed_tasks)
}

/// Read the last webcal refresh timestamp from a calendar sync marker.
pub fn webcal_last_refresh(
    conn: &Connection,
    calendar_id: &str,
) -> Result<Option<chrono::DateTime<Utc>>> {
    let value: Option<String> = conn
        .query_row(
            "SELECT sync_token FROM calendar WHERE id = ?1",
            params![calendar_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    match value {
        Some(value) => Ok(parse_dt_opt(Some(value))?),
        None => Ok(None),
    }
}

/// Store the webcal refresh timestamp in `calendar.sync_token`.
pub fn set_webcal_last_refresh(
    conn: &Connection,
    calendar_id: &str,
    at: chrono::DateTime<Utc>,
) -> Result<()> {
    conn.execute(
        "UPDATE calendar SET sync_token = ?2, updated_at = ?2 WHERE id = ?1",
        params![calendar_id, dt_string(at)],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::{Db, CORE_MIGRATIONS, CORE_NAMESPACE};

    fn db() -> Db {
        let db = Db::open_in_memory().expect("in-memory db");
        db.migrate(CORE_NAMESPACE, CORE_MIGRATIONS)
            .expect("core schema");
        db.migrate(PIM_NAMESPACE, PIM_MIGRATIONS)
            .expect("pim schema");
        db
    }

    #[test]
    fn calendar_mapping_round_trip() {
        let db = db();
        db.write_sync(|conn| {
            let calendar = nuncio_core::repo::calendars::create(conn, "local", "Test", "#0078D4")?;
            let mapping = CalendarRemote::new(
                calendar.id.clone(),
                "local",
                "caldav",
                "https://dav/cal/",
                Some("https://dav/cal/".into()),
            );
            set(conn, &mapping)?;
            assert_eq!(get_by_calendar(conn, &calendar.id)?, Some(mapping.clone()));
            assert_eq!(
                get_by_remote(conn, "local", "caldav", "https://dav/cal/")?,
                Some(mapping)
            );
            Ok(())
        })
        .expect("round trip");
    }

    #[test]
    fn reminder_markers_are_idempotent() {
        let db = db();
        db.write_sync(|conn| {
            assert!(!reminder_fired(conn, "e1", "2026-11-12")?);
            mark_reminder_fired(conn, "e1", "2026-11-12")?;
            mark_reminder_fired(conn, "e1", "2026-11-12")?;
            assert!(reminder_fired(conn, "e1", "2026-11-12")?);
            clear_reminder_fired(conn, "e1", "2026-11-12")?;
            assert!(!reminder_fired(conn, "e1", "2026-11-12")?);
            Ok(())
        })
        .expect("markers");
    }
}
