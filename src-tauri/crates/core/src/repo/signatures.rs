//! Signature repository.

use rusqlite::{params, Connection, Row};

use crate::error::AppError;
use crate::models::{new_id, Signature};
use crate::repo::{get_bool, get_opt_string, get_string, now_string};
use crate::Result;

const COLUMNS: &str = "id, account_id, name, html, use_for_new, use_for_reply";

fn row_to_signature(row: &Row<'_>) -> Result<Signature> {
    Ok(Signature {
        id: get_string(row, "id")?,
        account_id: get_opt_string(row, "account_id")?,
        name: get_string(row, "name")?,
        html: get_string(row, "html")?,
        use_for_new: get_bool(row, "use_for_new")?,
        use_for_reply: get_bool(row, "use_for_reply")?,
    })
}

pub fn list(conn: &Connection) -> Result<Vec<Signature>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM signature ORDER BY (account_id IS NULL) ASC, name ASC"
    ))?;
    let mut rows = stmt.query([])?;
    let mut signatures = Vec::new();
    while let Some(row) = rows.next()? {
        signatures.push(row_to_signature(row)?);
    }
    Ok(signatures)
}

/// Signatures usable for an account: its own plus global ones.
pub fn list_for_account(conn: &Connection, account_id: &str) -> Result<Vec<Signature>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM signature WHERE account_id = ?1 OR account_id IS NULL ORDER BY (account_id IS NULL) ASC, name ASC"
    ))?;
    let mut rows = stmt.query(params![account_id])?;
    let mut signatures = Vec::new();
    while let Some(row) = rows.next()? {
        signatures.push(row_to_signature(row)?);
    }
    Ok(signatures)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Signature>> {
    let sql = format!("SELECT {COLUMNS} FROM signature WHERE id = ?1");
    crate::repo::query_one(conn, &sql, params![id], row_to_signature)
}

/// Default signature for a new message (account-specific first).
pub fn default_for_new(conn: &Connection, account_id: &str) -> Result<Option<Signature>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM signature WHERE use_for_new = 1 AND (account_id = ?1 OR account_id IS NULL)
         ORDER BY (account_id IS NULL) ASC LIMIT 1"
    );
    crate::repo::query_one(conn, &sql, params![account_id], row_to_signature)
}

pub fn default_for_reply(conn: &Connection, account_id: &str) -> Result<Option<Signature>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM signature WHERE use_for_reply = 1 AND (account_id = ?1 OR account_id IS NULL)
         ORDER BY (account_id IS NULL) ASC LIMIT 1"
    );
    crate::repo::query_one(conn, &sql, params![account_id], row_to_signature)
}

pub fn upsert(conn: &Connection, signature: &mut Signature) -> Result<Signature> {
    if signature.id.trim().is_empty() {
        signature.id = new_id();
    }
    conn.execute(
        "INSERT INTO signature (id, account_id, name, html, use_for_new, use_for_reply, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
         ON CONFLICT(id) DO UPDATE SET
             account_id = excluded.account_id,
             name = excluded.name,
             html = excluded.html,
             use_for_new = excluded.use_for_new,
             use_for_reply = excluded.use_for_reply,
             updated_at = excluded.updated_at",
        params![
            signature.id,
            signature.account_id,
            signature.name,
            signature.html,
            signature.use_for_new as i64,
            signature.use_for_reply as i64,
            now_string()
        ],
    )?;
    Ok(signature.clone())
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    let changed = conn.execute("DELETE FROM signature WHERE id = ?1", params![id])?;
    if changed == 0 {
        return Err(AppError::NotFound(format!("signature {id} not found")));
    }
    Ok(())
}
