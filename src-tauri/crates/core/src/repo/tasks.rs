//! Task repository with smart lists (My Day / Important / Planned / ...).

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::error::AppError;
use crate::models::{
    new_id, Importance, RecurrenceRule, SmartList, SmartListCounts, Task, TaskInput, TaskQuery,
    TaskStatus, TaskStep,
};
use crate::repo::{
    get_dt, get_opt_dt, get_opt_string, get_string, json_opt, json_or_default, json_text,
    now_string,
};
use crate::Result;

const COLUMNS: &str = "\
    id, list_id, account_id, title, notes, status, importance, due_date, reminder_at, recurrence_json,\
    my_day_date, completed_at, steps_json, sort_order, linked_message_id, created_at, updated_at";

fn row_to_task(row: &Row<'_>) -> Result<Task> {
    let status: String = get_string(row, "status")?;
    let importance: String = get_string(row, "importance")?;
    let recurrence: Option<String> = row.get("recurrence_json")?;
    let steps: Option<String> = row.get("steps_json")?;
    Ok(Task {
        id: get_string(row, "id")?,
        list_id: get_string(row, "list_id")?,
        account_id: get_string(row, "account_id")?,
        title: get_string(row, "title")?,
        notes: get_string(row, "notes")?,
        status: status.parse().unwrap_or(TaskStatus::NotStarted),
        importance: importance.parse().unwrap_or(Importance::Normal),
        due_date: get_opt_string(row, "due_date")?,
        reminder_at: get_opt_dt(row, "reminder_at")?,
        recurrence: json_opt::<RecurrenceRule>(recurrence.as_deref(), "recurrence_json"),
        my_day_date: get_opt_string(row, "my_day_date")?,
        completed_at: get_opt_dt(row, "completed_at")?,
        created_at: get_dt(row, "created_at")?,
        updated_at: get_dt(row, "updated_at")?,
        steps: json_or_default::<Vec<TaskStep>>(steps.as_deref(), "steps_json"),
        sort_order: row.get("sort_order")?,
        linked_message_id: get_opt_string(row, "linked_message_id")?,
    })
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Task>> {
    let sql = format!("SELECT {COLUMNS} FROM task WHERE id = ?1");
    crate::repo::query_one(conn, &sql, params![id], row_to_task)
}

/// Sync metadata kept for Google Tasks / Graph To Do / VTODO.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskSyncMeta {
    pub etag: Option<String>,
    pub remote_id: Option<String>,
}

pub fn sync_meta(conn: &Connection, id: &str) -> Result<Option<TaskSyncMeta>> {
    Ok(conn
        .query_row(
            "SELECT etag, remote_id FROM task WHERE id = ?1",
            params![id],
            |row| {
                Ok(TaskSyncMeta {
                    etag: row.get(0)?,
                    remote_id: row.get(1)?,
                })
            },
        )
        .optional()?)
}

pub fn get_required(conn: &Connection, id: &str) -> Result<Task> {
    get(conn, id)?.ok_or_else(|| AppError::NotFound(format!("task {id} not found")))
}

pub fn by_remote_id(conn: &Connection, list_id: &str, remote_id: &str) -> Result<Option<Task>> {
    let sql = format!("SELECT {COLUMNS} FROM task WHERE list_id = ?1 AND remote_id = ?2 LIMIT 1");
    crate::repo::query_one(conn, &sql, params![list_id, remote_id], row_to_task)
}

/// Every task row, completed or not (backup export / full restore).
pub fn list_all(conn: &Connection) -> Result<Vec<Task>> {
    let sql =
        format!("SELECT {COLUMNS} FROM task ORDER BY list_id ASC, sort_order ASC, created_at ASC");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut tasks = Vec::new();
    while let Some(row) = rows.next()? {
        tasks.push(row_to_task(row)?);
    }
    Ok(tasks)
}

/// Query a custom list or a smart list.
pub fn query(conn: &Connection, query: &TaskQuery) -> Result<Vec<Task>> {
    let mut clauses: Vec<String> = Vec::new();
    let mut values: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(list_id) = &query.list_id {
        clauses.push("list_id = ?".into());
        values.push(crate::repo::text(list_id));
    }
    let completed_only = matches!(query.smart, Some(SmartList::Completed));
    match query.smart {
        Some(SmartList::MyDay) => {
            clauses.push("my_day_date = ?".into());
            values.push(crate::repo::text(query.today.clone()));
        }
        Some(SmartList::Important) => clauses.push("importance = 'high'".into()),
        Some(SmartList::Planned) => clauses.push("due_date IS NOT NULL".into()),
        // A soft-deleted mail no longer counts: its row stays until the link
        // is pruned, but the task should leave the smart list immediately.
        Some(SmartList::FlaggedEmail) => clauses.push(
            "linked_message_id IS NOT NULL AND linked_message_id IN \
             (SELECT id FROM message WHERE is_deleted_local = 0)"
                .into(),
        ),
        Some(SmartList::Completed) => clauses.push("status = 'completed'".into()),
        Some(SmartList::All) | None => {}
    }
    if completed_only {
        // Already filtered to completed.
    } else if !query.include_completed {
        clauses.push("status <> 'completed'".into());
    }
    let where_sql = if clauses.is_empty() {
        "1 = 1".to_string()
    } else {
        clauses.join(" AND ")
    };
    let order = if completed_only {
        "completed_at DESC, sort_order ASC"
    } else {
        "CASE WHEN due_date IS NULL THEN 1 ELSE 0 END, due_date ASC, sort_order ASC, created_at DESC"
    };
    let sql = format!("SELECT {COLUMNS} FROM task WHERE {where_sql} ORDER BY {order}");
    let mut stmt = conn.prepare(&sql)?;
    let refs = crate::repo::query_params(&values);
    let mut rows = stmt.query(refs.as_slice())?;
    let mut tasks = Vec::new();
    while let Some(row) = rows.next()? {
        tasks.push(row_to_task(row)?);
    }
    Ok(tasks)
}

pub fn search(conn: &Connection, text: &str) -> Result<Vec<Task>> {
    let pattern = format!("%{}%", text.trim().to_lowercase());
    let sql = format!(
        "SELECT {COLUMNS} FROM task
         WHERE lower(title) LIKE ?1 OR lower(notes) LIKE ?1
         ORDER BY status ASC, sort_order ASC, created_at DESC LIMIT 200"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![pattern])?;
    let mut tasks = Vec::new();
    while let Some(row) = rows.next()? {
        tasks.push(row_to_task(row)?);
    }
    Ok(tasks)
}

pub fn smart_counts(conn: &Connection, today: &str) -> Result<SmartListCounts> {
    let counts = conn.query_row(
        "SELECT
             COALESCE(SUM(CASE WHEN my_day_date = ?1 AND status <> 'completed' THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN importance = 'high' AND status <> 'completed' THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN due_date IS NOT NULL AND status <> 'completed' THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN status <> 'completed' THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN status = 'completed' THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN linked_message_id IS NOT NULL AND status <> 'completed'
                               AND linked_message_id IN (SELECT id FROM message WHERE is_deleted_local = 0)
                          THEN 1 ELSE 0 END), 0)
         FROM task",
        params![today],
        |row| {
            Ok(SmartListCounts {
                my_day: row.get(0)?,
                important: row.get(1)?,
                planned: row.get(2)?,
                all: row.get(3)?,
                completed: row.get(4)?,
                flagged_email: row.get(5)?,
            })
        },
    )?;
    Ok(counts)
}

fn next_sort_order(conn: &Connection, list_id: &str) -> Result<i64> {
    // New tasks go to the top.
    Ok(conn.query_row(
        "SELECT COALESCE(MIN(sort_order), 1) - 1 FROM task WHERE list_id = ?1",
        params![list_id],
        |row| row.get(0),
    )?)
}

pub fn create(conn: &Connection, list_id: &str, input: &TaskInput) -> Result<Task> {
    if input.title.trim().is_empty() {
        return Err(AppError::invalid_input("task title must not be empty"));
    }
    let list = crate::repo::task_lists::get_required(conn, list_id)?;
    let id = new_id();
    let now = now_string();
    let sort_order = next_sort_order(conn, list_id)?;
    conn.execute(
        "INSERT INTO task (id, list_id, account_id, title, notes, status, importance, due_date, reminder_at,
                           recurrence_json, my_day_date, completed_at, steps_json, sort_order, linked_message_id,
                           created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'notStarted', ?6, ?7, ?8, ?9, ?10, NULL, ?11, ?12, NULL, ?13, ?13)",
        params![
            id,
            list_id,
            list.account_id,
            input.title,
            input.notes,
            input.importance.as_str(),
            input.due_date,
            input.reminder_at.map(crate::repo::dt_string),
            match &input.recurrence {
                Some(rule) => Some(json_text(rule)?),
                None => None,
            },
            input.my_day_date,
            json_text(&input.steps)?,
            sort_order,
            now
        ],
    )?;
    get_required(conn, &id)
}

fn apply_input(task: &mut Task, input: &TaskInput) {
    task.title = input.title.clone();
    task.notes = input.notes.clone();
    task.importance = input.importance;
    task.due_date = input.due_date.clone();
    task.reminder_at = input.reminder_at;
    task.recurrence = input.recurrence.clone();
    task.my_day_date = input.my_day_date.clone();
    task.steps = input.steps.clone();
}

pub fn update(conn: &Connection, id: &str, input: &TaskInput) -> Result<Task> {
    if input.title.trim().is_empty() {
        return Err(AppError::invalid_input("task title must not be empty"));
    }
    let mut task = get_required(conn, id)?;
    apply_input(&mut task, input);
    conn.execute(
        "UPDATE task SET title = ?2, notes = ?3, importance = ?4, due_date = ?5, reminder_at = ?6,
                         recurrence_json = ?7, my_day_date = ?8, steps_json = ?9, updated_at = ?10
         WHERE id = ?1",
        params![
            id,
            task.title,
            task.notes,
            task.importance.as_str(),
            task.due_date,
            task.reminder_at.map(crate::repo::dt_string),
            match &task.recurrence {
                Some(rule) => Some(json_text(rule)?),
                None => None,
            },
            task.my_day_date,
            json_text(&task.steps)?,
            now_string()
        ],
    )?;
    get_required(conn, id)
}

pub fn set_completed(conn: &Connection, id: &str, value: bool) -> Result<Task> {
    let now = now_string();
    let changed = conn.execute(
        "UPDATE task SET status = ?2, completed_at = ?3, updated_at = ?3 WHERE id = ?1",
        params![
            id,
            if value {
                TaskStatus::Completed.as_str()
            } else {
                TaskStatus::NotStarted.as_str()
            },
            if value { Some(now) } else { None }
        ],
    )?;
    if changed == 0 {
        return Err(AppError::NotFound(format!("task {id} not found")));
    }
    get_required(conn, id)
}

pub fn set_importance(conn: &Connection, id: &str, importance: Importance) -> Result<Task> {
    conn.execute(
        "UPDATE task SET importance = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, importance.as_str(), now_string()],
    )?;
    get_required(conn, id)
}

pub fn set_my_day(conn: &Connection, id: &str, date: Option<&str>) -> Result<Task> {
    conn.execute(
        "UPDATE task SET my_day_date = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, date, now_string()],
    )?;
    get_required(conn, id)
}

pub fn move_task(conn: &Connection, id: &str, list_id: &str) -> Result<Task> {
    let list = crate::repo::task_lists::get_required(conn, list_id)?;
    let sort_order = next_sort_order(conn, list_id)?;
    conn.execute(
        "UPDATE task SET list_id = ?2, account_id = ?3, sort_order = ?4, updated_at = ?5 WHERE id = ?1",
        params![id, list_id, list.account_id, sort_order, now_string()],
    )?;
    get_required(conn, id)
}

pub fn reorder(conn: &Connection, list_id: &str, ids: &[String]) -> Result<()> {
    for (index, id) in ids.iter().enumerate() {
        conn.execute(
            "UPDATE task SET sort_order = ?3, updated_at = ?4 WHERE id = ?1 AND list_id = ?2",
            params![id, list_id, index as i64, now_string()],
        )?;
    }
    Ok(())
}

pub fn set_linked_message(conn: &Connection, id: &str, message_id: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE task SET linked_message_id = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, message_id, now_string()],
    )?;
    Ok(())
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    let changed = conn.execute("DELETE FROM task WHERE id = ?1", params![id])?;
    if changed == 0 {
        return Err(AppError::NotFound(format!("task {id} not found")));
    }
    Ok(())
}

pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM task", [], |row| row.get(0))?)
}

pub fn count_in_list(conn: &Connection, list_id: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM task WHERE list_id = ?1",
        params![list_id],
        |row| row.get(0),
    )?)
}

/// Upsert a task synced from Google Tasks / Graph To Do / CalDAV VTODO.
pub fn upsert_remote(
    conn: &Connection,
    task: &Task,
    etag: Option<&str>,
    remote_id: Option<&str>,
) -> Result<Task> {
    let mut task = task.clone();
    if task.id.trim().is_empty() {
        task.id = new_id();
    }
    conn.execute(
        "INSERT INTO task (id, list_id, account_id, title, notes, status, importance, due_date, reminder_at,
                           recurrence_json, my_day_date, completed_at, steps_json, sort_order, linked_message_id,
                           etag, remote_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
         ON CONFLICT(id) DO UPDATE SET
             list_id = excluded.list_id,
             account_id = excluded.account_id,
             title = excluded.title,
             notes = excluded.notes,
             status = excluded.status,
             importance = excluded.importance,
             due_date = excluded.due_date,
             reminder_at = excluded.reminder_at,
             recurrence_json = excluded.recurrence_json,
             my_day_date = excluded.my_day_date,
             completed_at = excluded.completed_at,
             steps_json = excluded.steps_json,
             sort_order = excluded.sort_order,
             linked_message_id = excluded.linked_message_id,
             etag = COALESCE(excluded.etag, task.etag),
             remote_id = COALESCE(excluded.remote_id, task.remote_id),
             updated_at = excluded.updated_at",
        params![
            task.id,
            task.list_id,
            task.account_id,
            task.title,
            task.notes,
            task.status.as_str(),
            task.importance.as_str(),
            task.due_date,
            task.reminder_at.map(crate::repo::dt_string),
            match &task.recurrence {
                Some(rule) => Some(json_text(rule)?),
                None => None,
            },
            task.my_day_date,
            task.completed_at.map(crate::repo::dt_string),
            json_text(&task.steps)?,
            task.sort_order,
            task.linked_message_id,
            etag,
            remote_id,
            now_string(),
            now_string()
        ],
    )?;
    get_required(conn, &task.id)
}

/// Tasks due for reminders (used by the scheduler).
pub fn reminder_candidates(
    conn: &Connection,
    from: chrono::DateTime<chrono::Utc>,
    to: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<Task>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM task
         WHERE status <> 'completed' AND reminder_at IS NOT NULL AND reminder_at >= ?1 AND reminder_at < ?2
         ORDER BY reminder_at ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![
        crate::repo::dt_string(from),
        crate::repo::dt_string(to)
    ])?;
    let mut tasks = Vec::new();
    while let Some(row) = rows.next()? {
        tasks.push(row_to_task(row)?);
    }
    Ok(tasks)
}
