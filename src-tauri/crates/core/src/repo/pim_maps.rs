//! Provider mappings for calendars, address books and task lists.
//!
//! The tables themselves are created by the `pim` migration namespace, but
//! they live in the same SQLite database. A restore must carry them: without a
//! mapping the first sync treats every remote calendar / address book / task
//! list as new and duplicates it on the server.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

use crate::Result;

/// Mapping kinds, matching the PIM tables.
pub const KIND_CALENDAR: &str = "calendar";
pub const KIND_ADDRESS_BOOK: &str = "books";
pub const KIND_TASK_LIST: &str = "tasks";

/// One provider mapping row. `local_id` is the calendar/address book/task list
/// id selected by `kind`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PimMapRecord {
    pub kind: String,
    pub local_id: String,
    pub account_id: String,
    pub provider: String,
    pub remote_id: String,
    pub href: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `(kind, table, local id column)` for every mapping table.
const TABLES: &[(&str, &str, &str)] = &[
    (KIND_CALENDAR, "pim_calendar_map", "calendar_id"),
    (KIND_ADDRESS_BOOK, "pim_address_book_map", "address_book_id"),
    (KIND_TASK_LIST, "pim_task_list_map", "task_list_id"),
];

fn table_for(kind: &str) -> Option<(&'static str, &'static str)> {
    TABLES
        .iter()
        .find(|(candidate, _, _)| *candidate == kind)
        .map(|(_, table, column)| (*table, *column))
}

pub fn list_all(conn: &Connection) -> Result<Vec<PimMapRecord>> {
    let mut maps = Vec::new();
    for (kind, table, id_column) in TABLES {
        let sql = format!(
            "SELECT {id_column}, account_id, provider, remote_id, href, created_at, updated_at
             FROM {table} ORDER BY created_at ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            maps.push(PimMapRecord {
                kind: (*kind).to_string(),
                local_id: crate::repo::get_string(row, id_column)?,
                account_id: crate::repo::get_string(row, "account_id")?,
                provider: crate::repo::get_string(row, "provider")?,
                remote_id: crate::repo::get_string(row, "remote_id")?,
                href: crate::repo::get_opt_string(row, "href")?,
                created_at: crate::repo::get_dt(row, "created_at")?,
                updated_at: crate::repo::get_dt(row, "updated_at")?,
            });
        }
    }
    Ok(maps)
}

/// Insert a mapping with its original timestamps (restore path).
///
/// Unknown kinds are ignored: a corrupt or newer backup must not abort the
/// whole restore.
pub fn insert(conn: &Connection, map: &PimMapRecord) -> Result<()> {
    let Some((table, id_column)) = table_for(&map.kind) else {
        return Ok(());
    };
    let sql = format!(
        "INSERT INTO {table}
             ({id_column}, account_id, provider, remote_id, href, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT DO UPDATE SET
             account_id = excluded.account_id,
             provider = excluded.provider,
             remote_id = excluded.remote_id,
             href = excluded.href,
             updated_at = excluded.updated_at"
    );
    conn.execute(
        &sql,
        params![
            map.local_id,
            map.account_id,
            map.provider,
            map.remote_id,
            map.href,
            crate::repo::dt_string(map.created_at),
            crate::repo::dt_string(map.updated_at)
        ],
    )?;
    Ok(())
}
