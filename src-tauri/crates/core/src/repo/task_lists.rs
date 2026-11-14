//! Task list repository.

use rusqlite::{params, Connection, Row};

use crate::error::AppError;
use crate::models::{new_id, TaskList};
use crate::repo::{get_bool, get_string, now_string};
use crate::Result;

const COLUMNS: &str =
    "l.id, l.account_id, l.name, l.color, l.is_default, l.is_read_only, l.sort_order";

fn row_to_list(row: &Row<'_>) -> Result<TaskList> {
    Ok(TaskList {
        id: get_string(row, "id")?,
        account_id: get_string(row, "account_id")?,
        name: get_string(row, "name")?,
        color: get_string(row, "color")?,
        is_default: get_bool(row, "is_default")?,
        is_read_only: get_bool(row, "is_read_only")?,
        incomplete_count: row.get("incomplete_count")?,
        sort_order: row.get("sort_order")?,
    })
}

pub fn list(conn: &Connection) -> Result<Vec<TaskList>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS},
                (SELECT COUNT(*) FROM task t WHERE t.list_id = l.id AND t.status <> 'completed') AS incomplete_count
         FROM task_list l
         ORDER BY l.account_id ASC, l.sort_order ASC, l.name COLLATE NOCASE ASC"
    ))?;
    let mut rows = stmt.query([])?;
    let mut lists = Vec::new();
    while let Some(row) = rows.next()? {
        lists.push(row_to_list(row)?);
    }
    Ok(lists)
}

pub fn list_for_account(conn: &Connection, account_id: &str) -> Result<Vec<TaskList>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS},
                (SELECT COUNT(*) FROM task t WHERE t.list_id = l.id AND t.status <> 'completed') AS incomplete_count
         FROM task_list l WHERE l.account_id = ?1
         ORDER BY l.sort_order ASC, l.name COLLATE NOCASE ASC"
    ))?;
    let mut rows = stmt.query(params![account_id])?;
    let mut lists = Vec::new();
    while let Some(row) = rows.next()? {
        lists.push(row_to_list(row)?);
    }
    Ok(lists)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<TaskList>> {
    let sql = format!(
        "SELECT {COLUMNS},
                (SELECT COUNT(*) FROM task t WHERE t.list_id = l.id AND t.status <> 'completed') AS incomplete_count
         FROM task_list l WHERE l.id = ?1"
    );
    crate::repo::query_one(conn, &sql, params![id], row_to_list)
}

pub fn get_required(conn: &Connection, id: &str) -> Result<TaskList> {
    get(conn, id)?.ok_or_else(|| AppError::NotFound(format!("task list {id} not found")))
}

pub fn default_for_account(conn: &Connection, account_id: &str) -> Result<Option<TaskList>> {
    let mut lists = list_for_account(conn, account_id)?;
    lists.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| a.sort_order.cmp(&b.sort_order))
    });
    Ok(lists.into_iter().next())
}

pub fn create(conn: &Connection, account_id: &str, name: &str, color: &str) -> Result<TaskList> {
    if name.trim().is_empty() {
        return Err(AppError::invalid_input("task list name must not be empty"));
    }
    let existing: i64 = conn.query_row(
        "SELECT COUNT(*) FROM task_list WHERE account_id = ?1",
        params![account_id],
        |row| row.get(0),
    )?;
    let id = new_id();
    let now = now_string();
    conn.execute(
        "INSERT INTO task_list (id, account_id, name, color, is_default, is_read_only, sort_order, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7, ?7)",
        params![
            id,
            account_id,
            name,
            color,
            (existing == 0) as i64,
            existing,
            now
        ],
    )?;
    get_required(conn, &id)
}

pub fn update(
    conn: &Connection,
    id: &str,
    name: Option<&str>,
    color: Option<&str>,
) -> Result<TaskList> {
    let current = get_required(conn, id)?;
    conn.execute(
        "UPDATE task_list SET name = ?2, color = ?3, updated_at = ?4 WHERE id = ?1",
        params![
            id,
            name.unwrap_or(&current.name),
            color.unwrap_or(&current.color),
            now_string()
        ],
    )?;
    get_required(conn, id)
}

pub fn set_default(conn: &Connection, id: &str) -> Result<()> {
    let list = get_required(conn, id)?;
    conn.execute(
        "UPDATE task_list SET is_default = 0 WHERE account_id = ?1",
        params![list.account_id],
    )?;
    conn.execute(
        "UPDATE task_list SET is_default = 1 WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

pub fn reorder(conn: &Connection, ids: &[String]) -> Result<()> {
    for (index, id) in ids.iter().enumerate() {
        conn.execute(
            "UPDATE task_list SET sort_order = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, index as i64, now_string()],
        )?;
    }
    Ok(())
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    let changed = conn.execute("DELETE FROM task_list WHERE id = ?1", params![id])?;
    if changed == 0 {
        return Err(AppError::NotFound(format!("task list {id} not found")));
    }
    Ok(())
}

pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM task_list", [], |row| row.get(0))?)
}

/// Account that owns a task list.
pub fn account_for(conn: &Connection, id: &str) -> Result<String> {
    Ok(conn.query_row(
        "SELECT account_id FROM task_list WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )?)
}
