//! Undo log (one reversible action per mail operation) and the durable
//! per-message undo state machine.

use rusqlite::{params, Connection, Row};

use crate::models::{
    new_id, NewUndoEntry, NewUndoRequest, UndoEntry, UndoEntryResult, UndoRequest,
    UndoRequestEntry, UndoState,
};
use crate::repo::{get_dt, get_opt_string, get_string, json_or_default, json_text, now_string};
use crate::Result;

const COLUMNS: &str = "id, account_id, kind, description, payload_json, created_at";

const REQUEST_COLUMNS: &str =
    "id, account_id, kind, value, state, attempt, op_ids_json, last_error, created_at, updated_at";

const ENTRY_COLUMNS: &str = "\
    id, request_id, account_id, message_id, original_folder_id, target_folder_id,\
    target_uid, target_uidvalidity, target_graph_id, permanent, result, inverse_op_id,\
    op_ids_json, evidence_missing, replans, payload_json, ordinal, created_at, updated_at";

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

/* ─────────────────── durable per-message undo state machine ─────────────── */

fn row_to_request(row: &Row<'_>) -> Result<UndoRequest> {
    let state: String = get_string(row, "state")?;
    let op_ids: Option<String> = row.get("op_ids_json")?;
    let value: Option<i64> = row.get("value")?;
    Ok(UndoRequest {
        id: get_string(row, "id")?,
        account_id: get_string(row, "account_id")?,
        kind: get_string(row, "kind")?,
        value: value.map(|value| value != 0),
        state: state.parse()?,
        attempt: row.get::<_, i64>("attempt")?.max(0) as u32,
        op_ids: json_or_default(op_ids.as_deref(), "op_ids_json"),
        last_error: get_opt_string(row, "last_error")?,
        created_at: get_dt(row, "created_at")?,
        updated_at: get_dt(row, "updated_at")?,
    })
}

fn row_to_request_entry(row: &Row<'_>) -> Result<UndoRequestEntry> {
    let result: String = get_string(row, "result")?;
    let op_ids: Option<String> = row.get("op_ids_json")?;
    let payload: Option<String> = row.get("payload_json")?;
    Ok(UndoRequestEntry {
        id: get_string(row, "id")?,
        request_id: get_string(row, "request_id")?,
        account_id: get_string(row, "account_id")?,
        message_id: get_string(row, "message_id")?,
        original_folder_id: get_string(row, "original_folder_id")?,
        target_folder_id: get_opt_string(row, "target_folder_id")?,
        target_uid: row.get::<_, Option<i64>>("target_uid")?,
        target_uidvalidity: row.get::<_, Option<i64>>("target_uidvalidity")?,
        target_graph_id: get_opt_string(row, "target_graph_id")?,
        permanent: row.get::<_, i64>("permanent")? != 0,
        result: result.parse()?,
        inverse_op_id: get_opt_string(row, "inverse_op_id")?,
        op_ids: json_or_default(op_ids.as_deref(), "op_ids_json"),
        evidence_missing: row.get::<_, i64>("evidence_missing")? != 0,
        replans: row.get::<_, i64>("replans")?.max(0) as u32,
        payload: json_or_default(payload.as_deref(), "payload_json"),
        ordinal: row.get::<_, i64>("ordinal")?,
        created_at: get_dt(row, "created_at")?,
        updated_at: get_dt(row, "updated_at")?,
    })
}

/// Persist a new request and all of its entries in one transaction.
///
/// The caller runs this inside a write transaction together with the
/// consumption of the `undo_log` row, so an undo cannot be started twice.
pub fn create_request(
    conn: &Connection,
    request: NewUndoRequest,
    entries: &[NewUndoEntry],
) -> Result<UndoRequest> {
    let now = now_string();
    conn.execute(
        "INSERT INTO undo_request
             (id, account_id, kind, value, state, attempt, op_ids_json, last_error,
              created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'pending', 0, ?5, NULL, ?6, ?6)",
        params![
            request.id,
            request.account_id,
            request.kind,
            request.value.map(|value| value as i64),
            json_text(&request.op_ids)?,
            now,
        ],
    )?;
    for (ordinal, entry) in entries.iter().enumerate() {
        conn.execute(
            "INSERT INTO undo_entry
                 (id, request_id, account_id, message_id, original_folder_id,
                  target_folder_id, target_uid, target_uidvalidity, target_graph_id,
                  permanent, result, inverse_op_id, op_ids_json, evidence_missing,
                  replans, payload_json, ordinal, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending', NULL, ?11, ?12,
                     0, ?13, ?14, ?15, ?15)",
            params![
                entry.id,
                entry.request_id,
                entry.account_id,
                entry.message_id,
                entry.original_folder_id,
                entry.target_folder_id,
                entry.target_uid,
                entry.target_uidvalidity,
                entry.target_graph_id,
                entry.permanent as i64,
                json_text(&entry.op_ids)?,
                entry.evidence_missing as i64,
                json_text(&entry.payload)?,
                ordinal as i64,
                now,
            ],
        )?;
    }
    get_request(conn, &request.id)?
        .ok_or_else(|| crate::AppError::internal("undo request disappeared after insert"))
}

pub fn get_request(conn: &Connection, id: &str) -> Result<Option<UndoRequest>> {
    let sql = format!("SELECT {REQUEST_COLUMNS} FROM undo_request WHERE id = ?1");
    crate::repo::query_one(conn, &sql, params![id], row_to_request)
}

/// Newest request that is still making progress, so the keyboard undo shortcut
/// resumes that state machine instead of reverting an older action. A `failed`
/// request is only retried through its explicit undo id.
pub fn latest_unfinished_request(conn: &Connection) -> Result<Option<UndoRequest>> {
    let sql = format!(
        "SELECT {REQUEST_COLUMNS} FROM undo_request
          WHERE state IN ('pending', 'deferred', 'compensating')
         ORDER BY updated_at DESC, rowid DESC LIMIT 1"
    );
    crate::repo::query_one(conn, &sql, [], row_to_request)
}

/// Requests a worker may continue: waiting for an operation to settle, or
/// interrupted mid-pass (`pending` / `compensating`). `done` and `failed` are
/// terminal.
pub fn resumable_requests(conn: &Connection, limit: u32) -> Result<Vec<UndoRequest>> {
    let sql = format!(
        "SELECT {REQUEST_COLUMNS} FROM undo_request
          WHERE state IN ('pending', 'deferred', 'compensating')
         ORDER BY updated_at ASC LIMIT ?1"
    );
    let mut statement = conn.prepare(&sql)?;
    let mut rows = statement.query(params![limit.max(1) as i64])?;
    let mut requests = Vec::new();
    while let Some(row) = rows.next()? {
        requests.push(row_to_request(row)?);
    }
    Ok(requests)
}

pub fn list_entries(conn: &Connection, request_id: &str) -> Result<Vec<UndoRequestEntry>> {
    let sql = format!(
        "SELECT {ENTRY_COLUMNS} FROM undo_entry WHERE request_id = ?1 ORDER BY ordinal ASC"
    );
    let mut statement = conn.prepare(&sql)?;
    let mut rows = statement.query(params![request_id])?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next()? {
        entries.push(row_to_request_entry(row)?);
    }
    Ok(entries)
}

pub fn get_entry(conn: &Connection, id: &str) -> Result<Option<UndoRequestEntry>> {
    let sql = format!("SELECT {ENTRY_COLUMNS} FROM undo_entry WHERE id = ?1");
    crate::repo::query_one(conn, &sql, params![id], row_to_request_entry)
}

/// Move a request to `state` and optionally record the reason it failed.
pub fn update_state(
    conn: &Connection,
    id: &str,
    state: UndoState,
    error: Option<&str>,
) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE undo_request SET state = ?2, last_error = ?3, updated_at = ?4 WHERE id = ?1",
        params![id, state.as_str(), error, now_string()],
    )?)
}

/// Count a processing pass. Used to bound retries when an operation row has
/// vanished (pruned) without leaving any other evidence behind.
pub fn bump_attempt(conn: &Connection, id: &str) -> Result<u32> {
    conn.execute(
        "UPDATE undo_request SET attempt = attempt + 1, updated_at = ?2 WHERE id = ?1",
        params![id, now_string()],
    )?;
    Ok(conn.query_row(
        "SELECT attempt FROM undo_request WHERE id = ?1",
        params![id],
        |row| row.get::<_, i64>(0),
    )? as u32)
}

/// Count one automatic re-plan of an entry whose previous inverse provably
/// never ran, returning the new count. Bounded by the service so a persistent
/// pre-send failure cannot keep creating fresh operations forever.
pub fn bump_entry_replans(conn: &Connection, id: &str) -> Result<u32> {
    conn.execute(
        "UPDATE undo_entry SET replans = replans + 1, updated_at = ?2 WHERE id = ?1",
        params![id, now_string()],
    )?;
    Ok(conn.query_row(
        "SELECT replans FROM undo_entry WHERE id = ?1",
        params![id],
        |row| row.get::<_, i64>(0),
    )? as u32)
}

pub fn update_entry_result(conn: &Connection, id: &str, result: UndoEntryResult) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE undo_entry SET result = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, result.as_str(), now_string()],
    )?)
}

/// Settle an entry whose inverse operation has finished.
///
/// The inverse claim is cleared in the same statement: a failed inverse must
/// remain retryable, so a later pass may enqueue a fresh one. Leaving the old
/// id in place made the retry look "already compensated".
pub fn settle_entry_compensation(
    conn: &Connection,
    id: &str,
    result: UndoEntryResult,
) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE undo_entry SET result = ?2, inverse_op_id = NULL, updated_at = ?3 WHERE id = ?1",
        params![id, result.as_str(), now_string()],
    )?)
}

/// Record the resolved target identity of a message.
pub fn update_entry_identity(
    conn: &Connection,
    id: &str,
    folder_id: Option<&str>,
    uid: Option<i64>,
    uidvalidity: Option<i64>,
    graph_id: Option<&str>,
) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE undo_entry
            SET target_folder_id = COALESCE(?2, target_folder_id),
                target_uid = COALESCE(?3, target_uid),
                target_uidvalidity = COALESCE(?4, target_uidvalidity),
                target_graph_id = COALESCE(?5, target_graph_id),
                updated_at = ?6
          WHERE id = ?1",
        params![id, folder_id, uid, uidvalidity, graph_id, now_string()],
    )?)
}

/// Atomically claim the inverse operation slot of an entry.
///
/// Returns `1` when this call stored `inverse_op_id` (and may enqueue the
/// operation), `0` when the entry already has one. Running the enqueue in the
/// same transaction makes the pair all-or-nothing: a failed insert rolls the
/// claim back, so a retry can try again without ever queueing a duplicate.
///
/// The entry is moved back to `pending` in the same statement. A previous
/// inverse may have failed and left the entry `failed`; without this reset the
/// request would stay terminal and the worker would never observe the freshly
/// queued inverse, even though it is already running.
pub fn claim_entry_compensation(conn: &Connection, id: &str, inverse_op_id: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE undo_entry SET inverse_op_id = ?2, result = 'pending', updated_at = ?3
          WHERE id = ?1 AND inverse_op_id IS NULL",
        params![id, inverse_op_id, now_string()],
    )?)
}

/// Release an inverse claim that provably never reached the server.
///
/// Unlike [`settle_entry_compensation`], this leaves the entry retryable: the
/// caller re-plans it and may queue a fresh inverse. Only a failed/cancelled
/// operation *without* a send intent may be released; a command that may have
/// reached the server must keep its reference so a retry cannot duplicate it.
pub fn release_entry_compensation(
    conn: &Connection,
    id: &str,
    inverse_op_id: &str,
) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE undo_entry SET inverse_op_id = NULL, result = 'pending', updated_at = ?3
          WHERE id = ?1 AND inverse_op_id = ?2 AND result != 'done'",
        params![id, inverse_op_id, now_string()],
    )?)
}

pub fn delete_request(conn: &Connection, id: &str) -> Result<usize> {
    Ok(conn.execute("DELETE FROM undo_request WHERE id = ?1", params![id])?)
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
            let first = push(
                conn,
                "acc",
                "archive",
                "Archive message",
                &serde_json::json!({}),
            )?;
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
