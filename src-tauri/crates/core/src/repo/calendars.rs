//! Calendar repository.

use rusqlite::{params, Connection, Row};

use crate::error::AppError;
use crate::models::{new_id, Calendar};
use crate::repo::{get_bool, get_opt_string, get_string, now_string};
use crate::Result;

const COLUMNS: &str =
    "id, account_id, name, color, is_read_only, is_visible, is_default, subscription_url";

fn row_to_calendar(row: &Row<'_>) -> Result<Calendar> {
    Ok(Calendar {
        id: get_string(row, "id")?,
        account_id: get_string(row, "account_id")?,
        name: get_string(row, "name")?,
        color: get_string(row, "color")?,
        is_read_only: get_bool(row, "is_read_only")?,
        is_visible: get_bool(row, "is_visible")?,
        is_default: get_bool(row, "is_default")?,
        subscription_url: get_opt_string(row, "subscription_url")?,
    })
}

pub fn list(conn: &Connection) -> Result<Vec<Calendar>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM calendar ORDER BY account_id ASC, is_default DESC, name COLLATE NOCASE ASC"
    ))?;
    let mut rows = stmt.query([])?;
    let mut calendars = Vec::new();
    while let Some(row) = rows.next()? {
        calendars.push(row_to_calendar(row)?);
    }
    Ok(calendars)
}

pub fn list_for_account(conn: &Connection, account_id: &str) -> Result<Vec<Calendar>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM calendar WHERE account_id = ?1 ORDER BY is_default DESC, name COLLATE NOCASE ASC"
    ))?;
    let mut rows = stmt.query(params![account_id])?;
    let mut calendars = Vec::new();
    while let Some(row) = rows.next()? {
        calendars.push(row_to_calendar(row)?);
    }
    Ok(calendars)
}

pub fn list_visible(conn: &Connection) -> Result<Vec<Calendar>> {
    Ok(list(conn)?
        .into_iter()
        .filter(|calendar| calendar.is_visible)
        .collect())
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Calendar>> {
    let sql = format!("SELECT {COLUMNS} FROM calendar WHERE id = ?1");
    crate::repo::query_one(conn, &sql, params![id], row_to_calendar)
}

pub fn get_required(conn: &Connection, id: &str) -> Result<Calendar> {
    get(conn, id)?.ok_or_else(|| AppError::NotFound(format!("calendar {id} not found")))
}

pub fn default_for_account(conn: &Connection, account_id: &str) -> Result<Option<Calendar>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM calendar WHERE account_id = ?1 ORDER BY is_default DESC, name ASC LIMIT 1"
    );
    crate::repo::query_one(conn, &sql, params![account_id], row_to_calendar)
}

/// The default calendar to write new events to (first writable, else any).
pub fn writable_default(conn: &Connection) -> Result<Option<Calendar>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM calendar WHERE is_read_only = 0
         ORDER BY is_default DESC, (account_id = 'local') DESC, name ASC LIMIT 1"
    );
    crate::repo::query_one(conn, &sql, [], row_to_calendar)
}

/// Create a local calendar for an account.
pub fn create(conn: &Connection, account_id: &str, name: &str, color: &str) -> Result<Calendar> {
    let existing: i64 = conn.query_row(
        "SELECT COUNT(*) FROM calendar WHERE account_id = ?1",
        params![account_id],
        |row| row.get(0),
    )?;
    let mut calendar = Calendar {
        id: new_id(),
        account_id: account_id.to_string(),
        name: name.trim().to_string(),
        color: color.to_string(),
        is_read_only: false,
        is_visible: true,
        is_default: existing == 0,
        subscription_url: None,
    };
    insert(conn, &mut calendar)
}

pub fn insert(conn: &Connection, calendar: &mut Calendar) -> Result<Calendar> {
    if calendar.id.trim().is_empty() {
        calendar.id = new_id();
    }
    if calendar.name.trim().is_empty() {
        return Err(AppError::invalid_input("calendar name must not be empty"));
    }
    conn.execute(
        "INSERT INTO calendar (id, account_id, name, color, is_read_only, is_visible, is_default,
                               subscription_url, ctag, sync_token, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL, ?9, ?9)
         ON CONFLICT(id) DO UPDATE SET
             account_id = excluded.account_id,
             name = excluded.name,
             color = excluded.color,
             is_read_only = excluded.is_read_only,
             is_visible = excluded.is_visible,
             is_default = excluded.is_default,
             subscription_url = excluded.subscription_url,
             updated_at = excluded.updated_at",
        params![
            calendar.id,
            calendar.account_id,
            calendar.name,
            calendar.color,
            calendar.is_read_only as i64,
            calendar.is_visible as i64,
            calendar.is_default as i64,
            calendar.subscription_url,
            now_string()
        ],
    )?;
    get_required(conn, &calendar.id)
}

pub fn update(
    conn: &Connection,
    id: &str,
    name: Option<&str>,
    color: Option<&str>,
) -> Result<Calendar> {
    let current = get_required(conn, id)?;
    conn.execute(
        "UPDATE calendar SET name = ?2, color = ?3, updated_at = ?4 WHERE id = ?1",
        params![
            id,
            name.unwrap_or(&current.name),
            color.unwrap_or(&current.color),
            now_string()
        ],
    )?;
    get_required(conn, id)
}

pub fn set_visible(conn: &Connection, id: &str, value: bool) -> Result<()> {
    conn.execute(
        "UPDATE calendar SET is_visible = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, value as i64, now_string()],
    )?;
    Ok(())
}

pub fn set_default(conn: &Connection, id: &str) -> Result<()> {
    let calendar = get_required(conn, id)?;
    conn.execute(
        "UPDATE calendar SET is_default = 0 WHERE account_id = ?1",
        params![calendar.account_id],
    )?;
    conn.execute(
        "UPDATE calendar SET is_default = 1 WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    let changed = conn.execute("DELETE FROM calendar WHERE id = ?1", params![id])?;
    if changed == 0 {
        return Err(AppError::NotFound(format!("calendar {id} not found")));
    }
    Ok(())
}

/// Sync markers (ctag / sync token) for DAV-based calendars.
pub fn sync_markers(conn: &Connection, id: &str) -> Result<(Option<String>, Option<String>)> {
    Ok(conn.query_row(
        "SELECT ctag, sync_token FROM calendar WHERE id = ?1",
        params![id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

pub fn set_sync_markers(
    conn: &Connection,
    id: &str,
    ctag: Option<&str>,
    sync_token: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE calendar SET ctag = ?2, sync_token = ?3, updated_at = ?4 WHERE id = ?1",
        params![id, ctag, sync_token, now_string()],
    )?;
    Ok(())
}

pub fn set_read_only(conn: &Connection, id: &str, value: bool) -> Result<()> {
    conn.execute(
        "UPDATE calendar SET is_read_only = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, value as i64, now_string()],
    )?;
    Ok(())
}
