//! Recipient history + autocomplete suggestions (merged with contacts).

use rusqlite::{params, Connection};

use crate::models::Address;
use crate::repo::now_string;
use crate::Result;

/// Remember every recipient of a sent message.
pub fn record(conn: &Connection, addresses: &[Address]) -> Result<()> {
    for address in addresses {
        let email = address.email.trim().to_lowercase();
        if email.is_empty() || !email.contains('@') {
            continue;
        }
        conn.execute(
            "INSERT INTO recipient_history (email, name, use_count, last_used_at)
             VALUES (?1, ?2, 1, ?3)
             ON CONFLICT(email) DO UPDATE SET
                 use_count = recipient_history.use_count + 1,
                 last_used_at = excluded.last_used_at,
                 name = COALESCE(excluded.name, recipient_history.name)",
            params![email, address.name, now_string()],
        )?;
    }
    Ok(())
}

/// Suggestions from history and the contact book, deduplicated by e-mail.
///
/// Contacts come first (they carry curated names), then history ordered by use.
pub fn suggest(conn: &Connection, query: &str, limit: u32) -> Result<Vec<Address>> {
    let limit = limit.clamp(1, 50) as usize;
    let pattern = format!("%{}%", query.trim().to_lowercase());
    let mut results: Vec<Address> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut stmt = conn.prepare(
        "SELECT display_name, emails_json FROM contact
         WHERE (?1 = '%%' OR lower(display_name) LIKE ?1 OR lower(emails_json) LIKE ?1)
         ORDER BY is_favorite DESC, display_name COLLATE NOCASE ASC LIMIT ?2",
    )?;
    let mut rows = stmt.query(params![pattern, (limit * 2) as i64])?;
    while let Some(row) = rows.next()? {
        let display_name: String = row.get(0)?;
        let emails_json: String = row.get(1)?;
        let emails: Vec<crate::models::ContactField> =
            serde_json::from_str(&emails_json).unwrap_or_default();
        for contact_email in emails {
            let key = contact_email.value.trim().to_lowercase();
            if key.is_empty() || !seen.insert(key.clone()) {
                continue;
            }
            let name = if display_name.is_empty() {
                None
            } else {
                Some(display_name.clone())
            };
            results.push(Address {
                name,
                email: contact_email.value,
            });
            if results.len() >= limit {
                break;
            }
        }
        if results.len() >= limit {
            break;
        }
    }
    drop(rows);
    drop(stmt);

    if results.len() < limit {
        let mut stmt = conn.prepare(
            "SELECT email, name FROM recipient_history
             WHERE (?1 = '%%' OR lower(email) LIKE ?1 OR lower(name) LIKE ?1)
             ORDER BY use_count DESC, last_used_at DESC LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![pattern, limit as i64])?;
        while let Some(row) = rows.next()? {
            let email: String = row.get(0)?;
            let name: Option<String> = row.get(1)?;
            let key = email.to_lowercase();
            if !seen.insert(key) {
                continue;
            }
            results.push(Address { name, email });
            if results.len() >= limit {
                break;
            }
        }
    }

    Ok(results)
}

pub fn count(conn: &Connection) -> Result<i64> {
    Ok(
        conn.query_row("SELECT COUNT(*) FROM recipient_history", [], |row| {
            row.get(0)
        })?,
    )
}

pub fn clear(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM recipient_history", [])?;
    Ok(())
}
