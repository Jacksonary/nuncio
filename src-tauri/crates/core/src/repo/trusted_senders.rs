//! Trusted senders for remote content (images).

use rusqlite::{params, Connection};
use uuid::Uuid;

use crate::repo::now_string;
use crate::Result;

fn normalize(value: &str) -> String {
    value.trim().trim_matches('"').to_lowercase()
}

/// Remember an e-mail sender (`user@example.com`) or a whole domain
/// (`@example.com` / `example.com`).
pub fn add(conn: &Connection, value: &str, account_id: Option<&str>) -> Result<()> {
    let value = normalize(value);
    if value.is_empty() {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO trusted_sender (id, value, account_id, created_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(value) DO NOTHING",
        params![Uuid::new_v4().to_string(), value, account_id, now_string()],
    )?;
    Ok(())
}

pub fn remove(conn: &Connection, value: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM trusted_sender WHERE value = ?1",
        params![normalize(value)],
    )?;
    Ok(())
}

pub fn list(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT value FROM trusted_sender ORDER BY value ASC")?;
    let mut rows = stmt.query([])?;
    let mut values = Vec::new();
    while let Some(row) = rows.next()? {
        values.push(row.get(0)?);
    }
    Ok(values)
}

/// Check whether a sender or its domain is trusted.
pub fn is_trusted(conn: &Connection, email: &str, domain: &str) -> Result<bool> {
    let email = normalize(email);
    let domain = normalize(domain);
    let at_domain = format!("@{domain}");
    let trusted: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM trusted_sender WHERE value = ?1 OR value = ?2 OR value = ?3)",
        params![email, at_domain, domain],
        |row| row.get(0),
    )?;
    Ok(trusted)
}

pub fn clear(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM trusted_sender", [])?;
    Ok(())
}
