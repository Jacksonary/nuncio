//! Local draft repository (compose windows and the Drafts folder).

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::error::AppError;
use crate::models::{
    new_id, Address, Draft, DraftAttachment, Importance, RemoteDraftIdentity, ReplyMode,
};
use crate::repo::{
    get_bool, get_opt_dt, get_opt_string, get_string, json_opt, json_or_default, json_text,
    now_string,
};
use crate::Result;

const COLUMNS: &str = "\
    id, account_id, identity_id, from_json, to_json, cc_json, bcc_json, subject, html,\
    attachments_json, reply_mode, source_message_id, in_reply_to, references_json, importance,\
    request_read_receipt, remote_draft_message_id, remote_draft_json, sync_suppressed, updated_at";

fn row_to_draft(row: &Row<'_>) -> Result<Draft> {
    let from: Option<String> = row.get("from_json")?;
    let attachments: Option<String> = row.get("attachments_json")?;
    let references: Option<String> = row.get("references_json")?;
    let importance: String = get_string(row, "importance")?;
    let reply_mode: Option<String> = get_opt_string(row, "reply_mode")?;
    Ok(Draft {
        id: Some(get_string(row, "id")?),
        account_id: get_string(row, "account_id")?,
        identity_id: get_opt_string(row, "identity_id")?,
        from: json_opt(from.as_deref(), "from_json").unwrap_or_else(|| Address::new("")),
        to: json_or_default(
            row.get::<_, Option<String>>("to_json")?.as_deref(),
            "to_json",
        ),
        cc: json_or_default(
            row.get::<_, Option<String>>("cc_json")?.as_deref(),
            "cc_json",
        ),
        bcc: json_or_default(
            row.get::<_, Option<String>>("bcc_json")?.as_deref(),
            "bcc_json",
        ),
        subject: get_string(row, "subject")?,
        html: get_string(row, "html")?,
        attachments: json_or_default::<Vec<DraftAttachment>>(
            attachments.as_deref(),
            "attachments_json",
        ),
        reply_mode: reply_mode.and_then(|mode| mode.parse::<ReplyMode>().ok()),
        source_message_id: get_opt_string(row, "source_message_id")?,
        in_reply_to: get_opt_string(row, "in_reply_to")?,
        references: json_or_default(references.as_deref(), "references_json"),
        importance: importance.parse().unwrap_or(Importance::Normal),
        request_read_receipt: get_bool(row, "request_read_receipt")?,
        remote_draft_message_id: get_opt_string(row, "remote_draft_message_id")?,
        remote_draft: get_opt_string(row, "remote_draft_json")?
            .map(|json| serde_json::from_str::<RemoteDraftIdentity>(&json))
            .transpose()
            .map_err(|error| {
                AppError::internal(format!("invalid draft remote identity: {error}"))
            })?,
        sync_suppressed: get_bool(row, "sync_suppressed")?,
        updated_at: get_opt_dt(row, "updated_at")?,
    })
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Draft>> {
    let sql = format!("SELECT {COLUMNS} FROM draft WHERE id = ?1");
    crate::repo::query_one(conn, &sql, params![id], row_to_draft)
}

pub fn get_required(conn: &Connection, id: &str) -> Result<Draft> {
    get(conn, id)?.ok_or_else(|| AppError::NotFound(format!("draft {id} not found")))
}

pub fn list_for_account(conn: &Connection, account_id: &str) -> Result<Vec<Draft>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM draft WHERE account_id = ?1 ORDER BY updated_at DESC"
    ))?;
    let mut rows = stmt.query(params![account_id])?;
    let mut drafts = Vec::new();
    while let Some(row) = rows.next()? {
        drafts.push(row_to_draft(row)?);
    }
    Ok(drafts)
}

/// Insert or update a draft. A missing id is generated; the returned draft
/// always has an id and an `updated_at` timestamp.
pub fn save(conn: &Connection, draft: &Draft) -> Result<Draft> {
    let id = match &draft.id {
        Some(id) if !id.trim().is_empty() => id.clone(),
        _ => new_id(),
    };
    let created_at: String = conn
        .query_row(
            "SELECT created_at FROM draft WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_else(now_string);
    let updated_at = now_string();
    conn.execute(
        "INSERT INTO draft (
            id, account_id, identity_id, from_json, to_json, cc_json, bcc_json, subject, html,
            attachments_json, reply_mode, source_message_id, in_reply_to, references_json,
            importance, request_read_receipt, remote_draft_message_id, remote_draft_json,
            sync_suppressed, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)
         ON CONFLICT(id) DO UPDATE SET
             account_id = excluded.account_id,
             identity_id = excluded.identity_id,
             from_json = excluded.from_json,
             to_json = excluded.to_json,
             cc_json = excluded.cc_json,
             bcc_json = excluded.bcc_json,
             subject = excluded.subject,
             html = excluded.html,
             attachments_json = excluded.attachments_json,
             reply_mode = excluded.reply_mode,
             source_message_id = excluded.source_message_id,
             in_reply_to = excluded.in_reply_to,
             references_json = excluded.references_json,
             importance = excluded.importance,
             request_read_receipt = excluded.request_read_receipt,
             remote_draft_message_id = excluded.remote_draft_message_id,
             remote_draft_json = excluded.remote_draft_json,
             sync_suppressed = excluded.sync_suppressed,
             updated_at = excluded.updated_at",
        params![
            id,
            draft.account_id,
            draft.identity_id,
            json_text(&draft.from)?,
            json_text(&draft.to)?,
            json_text(&draft.cc)?,
            json_text(&draft.bcc)?,
            draft.subject,
            draft.html,
            json_text(&draft.attachments)?,
            draft.reply_mode.map(|mode| mode.as_str()),
            draft.source_message_id,
            draft.in_reply_to,
            json_text(&draft.references)?,
            draft.importance.as_str(),
            draft.request_read_receipt as i64,
            draft.remote_draft_message_id,
            draft
                .remote_draft
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|error| {
                    AppError::internal(format!("cannot encode remote draft identity: {error}"))
                })?,
            draft.sync_suppressed as i64,
            created_at,
            updated_at
        ],
    )?;
    let mut saved = draft.clone();
    saved.id = Some(id);
    saved.updated_at = get_dt_from_string(&updated_at);
    Ok(saved)
}

fn get_dt_from_string(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    crate::repo::parse_dt(value).ok()
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM draft WHERE id = ?1", params![id])?;
    Ok(())
}

/// Stop retrying remote sync for a draft the user chose to keep locally.
///
/// Returns the number of rows changed so a caller can refuse a stale request
/// for a draft that no longer exists.
pub fn set_sync_suppressed(conn: &Connection, id: &str, suppressed: bool) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE draft SET sync_suppressed = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, suppressed as i64, now_string()],
    )?)
}

/// Replace the pinned remote identity of a draft. A missing draft is a no-op.
pub fn set_remote_draft(
    conn: &Connection,
    id: &str,
    identity: Option<&RemoteDraftIdentity>,
) -> Result<usize> {
    let json = identity
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| {
            AppError::internal(format!("cannot encode remote draft identity: {error}"))
        })?;
    Ok(conn.execute(
        "UPDATE draft SET remote_draft_json = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, json, now_string()],
    )?)
}

/// Whether the draft still exists and is suppressed from remote sync.
pub fn is_sync_suppressed(conn: &Connection, id: &str) -> Result<bool> {
    Ok(get(conn, id)?.is_some_and(|draft| draft.sync_suppressed))
}

pub fn set_remote_id(conn: &Connection, id: &str, remote_id: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE draft SET remote_draft_message_id = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, remote_id, now_string()],
    )?;
    Ok(())
}

pub fn find_by_remote_id(conn: &Connection, remote_id: &str) -> Result<Option<Draft>> {
    let sql = format!("SELECT {COLUMNS} FROM draft WHERE remote_draft_message_id = ?1 LIMIT 1");
    crate::repo::query_one(conn, &sql, params![remote_id], row_to_draft)
}

pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM draft", [], |row| row.get(0))?)
}
