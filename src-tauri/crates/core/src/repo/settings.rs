//! Settings persistence (single row, JSON document).

use rusqlite::{params, Connection, OptionalExtension};

use crate::models::AppSettings;
use crate::repo::now_string;
use crate::Result;

pub fn load(conn: &Connection) -> Result<Option<AppSettings>> {
    let value: Option<String> = conn
        .query_row("SELECT json FROM settings WHERE id = 1", [], |row| {
            row.get(0)
        })
        .optional()?;
    match value {
        Some(json) => {
            let settings: AppSettings = serde_json::from_str(&json).map_err(|error| {
                crate::error::AppError::Db(format!("invalid settings JSON: {error}"))
            })?;
            Ok(Some(settings))
        }
        None => Ok(None),
    }
}

pub fn save(conn: &Connection, settings: &AppSettings) -> Result<()> {
    let json = serde_json::to_string(settings).map_err(|error| {
        crate::error::AppError::internal(format!("settings encode failed: {error}"))
    })?;
    conn.execute(
        "INSERT INTO settings (id, json, updated_at) VALUES (1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET json = excluded.json, updated_at = excluded.updated_at",
        params![json, now_string()],
    )?;
    Ok(())
}
