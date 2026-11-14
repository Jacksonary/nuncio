//! Sender identities (aliases / send-as addresses).

use rusqlite::{params, Connection, Row};

use crate::error::AppError;
use crate::models::{new_id, Identity};
use crate::repo::{get_bool, get_opt_string, get_string, now_string};
use crate::Result;

const COLUMNS: &str = "id, account_id, name, email, reply_to, signature_id, is_default";

fn row_to_identity(row: &Row<'_>) -> Result<Identity> {
    Ok(Identity {
        id: get_string(row, "id")?,
        account_id: get_string(row, "account_id")?,
        name: get_string(row, "name")?,
        email: get_string(row, "email")?,
        reply_to: get_opt_string(row, "reply_to")?,
        signature_id: get_opt_string(row, "signature_id")?,
        is_default: get_bool(row, "is_default")?,
    })
}

pub fn list(conn: &Connection, account_id: Option<&str>) -> Result<Vec<Identity>> {
    let mut values: Vec<String> = Vec::new();
    let sql = match account_id {
        Some(id) => {
            values.push(id.to_string());
            format!("SELECT {COLUMNS} FROM identity WHERE account_id = ?1 ORDER BY is_default DESC, email ASC")
        }
        None => format!(
            "SELECT {COLUMNS} FROM identity ORDER BY account_id ASC, is_default DESC, email ASC"
        ),
    };
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(values.iter()))?;
    let mut identities = Vec::new();
    while let Some(row) = rows.next()? {
        identities.push(row_to_identity(row)?);
    }
    Ok(identities)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Identity>> {
    let sql = format!("SELECT {COLUMNS} FROM identity WHERE id = ?1");
    crate::repo::query_one(conn, &sql, params![id], row_to_identity)
}

pub fn get_required(conn: &Connection, id: &str) -> Result<Identity> {
    get(conn, id)?.ok_or_else(|| AppError::NotFound(format!("identity {id} not found")))
}

/// Default identity for an account; falls back to the first one.
pub fn default_for_account(conn: &Connection, account_id: &str) -> Result<Option<Identity>> {
    let sql = format!("SELECT {COLUMNS} FROM identity WHERE account_id = ?1 ORDER BY is_default DESC, email ASC LIMIT 1");
    crate::repo::query_one(conn, &sql, params![account_id], row_to_identity)
}

pub fn upsert(conn: &Connection, identity: &mut Identity) -> Result<Identity> {
    if identity.id.trim().is_empty() {
        identity.id = new_id();
    }
    if identity.email.trim().is_empty() {
        return Err(AppError::invalid_input("identity e-mail must not be empty"));
    }
    if identity.is_default {
        conn.execute(
            "UPDATE identity SET is_default = 0, updated_at = ?2 WHERE account_id = ?1",
            params![identity.account_id, now_string()],
        )?;
    }
    conn.execute(
        "INSERT INTO identity (id, account_id, name, email, reply_to, signature_id, is_default, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
         ON CONFLICT(id) DO UPDATE SET
             account_id = excluded.account_id,
             name = excluded.name,
             email = excluded.email,
             reply_to = excluded.reply_to,
             signature_id = excluded.signature_id,
             is_default = excluded.is_default,
             updated_at = excluded.updated_at",
        params![
            identity.id,
            identity.account_id,
            identity.name,
            identity.email,
            identity.reply_to,
            identity.signature_id,
            identity.is_default as i64,
            now_string()
        ],
    )?;
    Ok(identity.clone())
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    let changed = conn.execute("DELETE FROM identity WHERE id = ?1", params![id])?;
    if changed == 0 {
        return Err(AppError::NotFound(format!("identity {id} not found")));
    }
    Ok(())
}
