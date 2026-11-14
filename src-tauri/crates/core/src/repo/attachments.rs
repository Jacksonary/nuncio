//! Attachment repository.

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::error::AppError;
use crate::models::{new_id, Attachment};
use crate::repo::{get_bool, get_opt_string, get_string, now_string};
use crate::Result;

const COLUMNS: &str =
    "id, message_id, filename, mime_type, size, content_id, is_inline, is_downloaded";

fn row_to_attachment(row: &Row<'_>) -> Result<Attachment> {
    Ok(Attachment {
        id: get_string(row, "id")?,
        message_id: get_string(row, "message_id")?,
        filename: get_string(row, "filename")?,
        mime_type: get_string(row, "mime_type")?,
        size: row.get::<_, i64>("size")?.max(0) as u64,
        content_id: get_opt_string(row, "content_id")?,
        is_inline: get_bool(row, "is_inline")?,
        is_downloaded: get_bool(row, "is_downloaded")?,
    })
}

pub fn list(conn: &Connection, message_id: &str) -> Result<Vec<Attachment>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM attachment WHERE message_id = ?1 ORDER BY rowid ASC"
    ))?;
    let mut rows = stmt.query(params![message_id])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        result.push(row_to_attachment(row)?);
    }
    Ok(result)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Attachment>> {
    let sql = format!("SELECT {COLUMNS} FROM attachment WHERE id = ?1");
    crate::repo::query_one(conn, &sql, params![id], row_to_attachment)
}

pub fn get_required(conn: &Connection, id: &str) -> Result<Attachment> {
    get(conn, id)?.ok_or_else(|| AppError::NotFound(format!("attachment {id} not found")))
}

pub fn by_content_id(
    conn: &Connection,
    message_id: &str,
    content_id: &str,
) -> Result<Option<Attachment>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM attachment WHERE message_id = ?1 AND content_id = ?2 ORDER BY rowid ASC LIMIT 1"
    );
    crate::repo::query_one(
        conn,
        &sql,
        params![message_id, content_id],
        row_to_attachment,
    )
}

pub fn insert(conn: &Connection, attachment: &mut Attachment) -> Result<Attachment> {
    if attachment.id.trim().is_empty() {
        attachment.id = new_id();
    }
    conn.execute(
        "INSERT INTO attachment (id, message_id, filename, mime_type, size, content_id, is_inline, is_downloaded, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
         ON CONFLICT(id) DO UPDATE SET
             message_id = excluded.message_id,
             filename = excluded.filename,
             mime_type = excluded.mime_type,
             size = excluded.size,
             content_id = excluded.content_id,
             is_inline = excluded.is_inline,
             is_downloaded = excluded.is_downloaded,
             updated_at = excluded.updated_at",
        params![
            attachment.id,
            attachment.message_id,
            attachment.filename,
            attachment.mime_type,
            attachment.size as i64,
            attachment.content_id,
            attachment.is_inline as i64,
            attachment.is_downloaded as i64,
            now_string()
        ],
    )?;
    Ok(attachment.clone())
}

/// Mark an attachment as downloaded and remember its local path.
pub fn set_downloaded(conn: &Connection, id: &str, path: &str) -> Result<()> {
    let changed = conn.execute(
        "UPDATE attachment SET is_downloaded = 1, path = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, path, now_string()],
    )?;
    if changed == 0 {
        return Err(AppError::NotFound(format!("attachment {id} not found")));
    }
    Ok(())
}

pub fn local_path(conn: &Connection, id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT path FROM attachment WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()?
        .flatten())
}

pub fn delete_for_message(conn: &Connection, message_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM attachment WHERE message_id = ?1",
        params![message_id],
    )?;
    Ok(())
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM attachment WHERE id = ?1", params![id])?;
    Ok(())
}

/// Replace all attachments of a message in one go (body parse result).
pub fn replace_for_message(
    conn: &Connection,
    message_id: &str,
    attachments: &[Attachment],
) -> Result<()> {
    delete_for_message(conn, message_id)?;
    for attachment in attachments {
        let mut copy = attachment.clone();
        copy.message_id = message_id.to_string();
        insert(conn, &mut copy)?;
    }
    Ok(())
}
