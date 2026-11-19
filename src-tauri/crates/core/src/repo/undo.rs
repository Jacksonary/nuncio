//! Undo log (one reversible action per mail operation).

use rusqlite::{params, Connection, Row};

use crate::models::{new_id, UndoEntry};
use crate::repo::{get_string, json_or_default, json_text, now_string};
use crate::Result;

const COLUMNS: &str = "id, account_id, kind, description, payload_json, created_at";

fn row_to_entry(row: &Row<'_>) -> Result<UndoEntry> {
    let payload: Option<String> = row.get("payload_json")?;
    Ok(UndoEntry {
        id: get_string(row, "id")?,
        account_id: get_string(row, "account_id")?,
        kind: get_string(row, "kind")?,
        description: get_string(row, "description")?,
        payload: json_or_default(payload.as_deref(), "payload_json"),
        created_at: crate::repo::get_dt(row, "created_at")?,
    })
}

pub fn push(
    conn: &Connection,
    account_id: &str,
    kind: &str,
    description: &str,
    payload: &serde_json::Value,
) -> Result<UndoEntry> {
    let id = new_id();
    let created_at = now_string();
    conn.execute(
        "INSERT INTO undo_log (id, account_id, kind, description, payload_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id,
            account_id,
            kind,
            description,
            json_text(payload)?,
            created_at
        ],
    )?;
    Ok(UndoEntry {
        id,
        account_id: account_id.to_string(),
        kind: kind.to_string(),
        description: description.to_string(),
        payload: payload.clone(),
        created_at: crate::repo::parse_dt(&created_at)?,
    })
}

/// Remove and return the most recent undo entry (optionally per account).
pub fn pop_last(conn: &Connection, account_id: Option<&str>) -> Result<Option<UndoEntry>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM undo_log WHERE (?1 IS NULL OR account_id = ?1) ORDER BY created_at DESC, rowid DESC LIMIT 1"
    );
    let Some(entry) = crate::repo::query_one(conn, &sql, params![account_id], row_to_entry)? else {
        return Ok(None);
    };
    conn.execute("DELETE FROM undo_log WHERE id = ?1", params![entry.id])?;
    Ok(Some(entry))
}

/// Remove and return one specific undo entry.
///
/// The toast for an action closes over the id it got back from the mutation, so
/// an unrelated later action must not be the one that gets reverted.
pub fn pop_by_id(conn: &Connection, id: &str) -> Result<Option<UndoEntry>> {
    let sql = format!("SELECT {COLUMNS} FROM undo_log WHERE id = ?1");
    let Some(entry) = crate::repo::query_one(conn, &sql, params![id], row_to_entry)? else {
        return Ok(None);
    };
    conn.execute("DELETE FROM undo_log WHERE id = ?1", params![id])?;
    Ok(Some(entry))
}

pub fn peek_last(conn: &Connection, account_id: Option<&str>) -> Result<Option<UndoEntry>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM undo_log WHERE (?1 IS NULL OR account_id = ?1) ORDER BY created_at DESC, rowid DESC LIMIT 1"
    );
    crate::repo::query_one(conn, &sql, params![account_id], row_to_entry)
}

pub fn clear(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM undo_log", [])?;
    Ok(())
}

/// Drop entries older than the given instant (default retention: 7 days).
pub fn prune(conn: &Connection, older_than: chrono::DateTime<chrono::Utc>) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM undo_log WHERE created_at < ?1",
        params![crate::repo::dt_string(older_than)],
    )?)
}

pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM undo_log", [], |row| row.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn setup() -> Db {
        let db = Db::open_in_memory().expect("db");
        db.migrate_core().expect("migrate");
        db
    }

    #[test]
    fn pop_by_id_removes_only_that_entry() {
        let db = setup();
        db.write_sync(|conn| {
            let first = push(
                conn,
                "acc",
                "archive",
                "Archive message",
                &serde_json::json!({ "n": 1 }),
            )?;
            let second = push(
                conn,
                "acc",
                "move",
                "Move message",
                &serde_json::json!({ "n": 2 }),
            )?;

            let popped = pop_by_id(conn, &first.id)?.expect("first entry");
            assert_eq!(popped.id, first.id);
            assert_eq!(count(conn)?, 1);

            // The entry is consumed exactly once and the other one is intact,
            // even though it is older and pop_last would not have picked it.
            assert!(pop_by_id(conn, &first.id)?.is_none());
            let remaining = pop_last(conn, None)?.expect("second entry");
            assert_eq!(remaining.id, second.id);
            assert_eq!(count(conn)?, 0);
            Ok(())
        })
        .expect("undo log");
    }

    #[test]
    fn pop_last_still_returns_the_newest_entry() {
        let db = setup();
        db.write_sync(|conn| {
            let first = push(conn, "acc", "archive", "Archive message", &serde_json::json!({}))?;
            let second = push(conn, "acc", "move", "Move message", &serde_json::json!({}))?;
            let newest = pop_last(conn, None)?.expect("newest");
            assert_eq!(newest.id, second.id);
            let older = pop_last(conn, None)?.expect("older");
            assert_eq!(older.id, first.id);
            Ok(())
        })
        .expect("undo log");
    }
}
