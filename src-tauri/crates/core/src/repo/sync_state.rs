//! Per-account, per-module key/value sync state (sync tokens, cursors, modseq).

use rusqlite::{params, Connection, OptionalExtension};

use crate::models::SyncCursor;
use crate::repo::now_string;
use crate::Result;

pub fn get(conn: &Connection, account_id: &str, module: &str, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT value FROM sync_state WHERE account_id = ?1 AND module = ?2 AND key = ?3",
            params![account_id, module, key],
            |row| row.get(0),
        )
        .optional()?)
}

pub fn get_cursor(
    conn: &Connection,
    account_id: &str,
    module: &str,
    key: &str,
) -> Result<Option<SyncCursor>> {
    Ok(get(conn, account_id, module, key)?.map(|value| SyncCursor::new(key, value)))
}

pub fn set(
    conn: &Connection,
    account_id: &str,
    module: &str,
    key: &str,
    value: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_state (account_id, module, key, value, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(account_id, module, key) DO UPDATE SET
             value = excluded.value,
             updated_at = excluded.updated_at",
        params![account_id, module, key, value, now_string()],
    )?;
    Ok(())
}

pub fn set_json<T: serde::Serialize + ?Sized>(
    conn: &Connection,
    account_id: &str,
    module: &str,
    key: &str,
    value: &T,
) -> Result<()> {
    let encoded = serde_json::to_string(value).map_err(|error| {
        crate::error::AppError::internal(format!("sync state encode failed: {error}"))
    })?;
    set(conn, account_id, module, key, &encoded)
}

pub fn get_json<T: for<'de> serde::Deserialize<'de>>(
    conn: &Connection,
    account_id: &str,
    module: &str,
    key: &str,
) -> Result<Option<T>> {
    match get(conn, account_id, module, key)? {
        Some(value) => serde_json::from_str(&value).map(Some).map_err(|error| {
            crate::error::AppError::Db(format!("invalid sync state JSON: {error}"))
        }),
        None => Ok(None),
    }
}

pub fn delete_key(conn: &Connection, account_id: &str, module: &str, key: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM sync_state WHERE account_id = ?1 AND module = ?2 AND key = ?3",
        params![account_id, module, key],
    )?;
    Ok(())
}

pub fn delete_module(conn: &Connection, account_id: &str, module: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM sync_state WHERE account_id = ?1 AND module = ?2",
        params![account_id, module],
    )?;
    Ok(())
}

pub fn delete_account(conn: &Connection, account_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM sync_state WHERE account_id = ?1",
        params![account_id],
    )?;
    Ok(())
}

/// All keys of one module (e.g. per-folder cursors).
pub fn list_module(conn: &Connection, account_id: &str, module: &str) -> Result<Vec<SyncCursor>> {
    let mut stmt = conn.prepare(
        "SELECT key, value FROM sync_state WHERE account_id = ?1 AND module = ?2 ORDER BY key ASC",
    )?;
    let mut rows = stmt.query(params![account_id, module])?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next()? {
        entries.push(SyncCursor {
            key: row.get(0)?,
            value: row.get(1)?,
        });
    }
    Ok(entries)
}
