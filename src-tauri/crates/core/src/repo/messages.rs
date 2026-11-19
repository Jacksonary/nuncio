//! Message repository: bulk upsert, flags, local moves, paged listing
//! (folder / unified / search), conversation grouping and FTS5 indexing.

use chrono::{DateTime, Utc};
use rusqlite::types::Value;
use rusqlite::{params, Connection, OptionalExtension, Row};
use tracing::debug;

use crate::error::AppError;
use crate::models::{
    Address, FolderRole, Importance, MailFilter, MailScope, MessagePage, MessageSummary,
    PageCursor, StoredMessage,
};
use crate::repo::{
    dt_string, get_bool, get_dt, get_opt_dt, get_opt_string, get_string, int, json_or_default,
    json_text, now_string, parse_dt, text,
};
use crate::text::{cjk_bigrams, fts_query};
use crate::Result;

/// Select list for message rows (qualified with alias `m`).
pub const MESSAGE_COLUMNS: &str = "\
    m.id, m.account_id, m.folder_id, m.thread_id, m.subject, m.from_json, m.to_json, m.cc_json,\
    m.date, m.preview, m.seen, m.flagged, m.answered, m.forwarded, m.is_draft, m.has_attachments,\
    m.importance, m.size, m.bcc_json, m.reply_to_json, m.message_id_header, m.in_reply_to,\
    m.references_json, m.uid, m.uidvalidity, m.gm_msgid, m.gm_thrid, m.labels_json, m.raw_headers,\
    m.body_path, m.body_cached_at, m.internal_date, m.is_deleted_local";

fn addresses_from(row: &Row<'_>, column: &str) -> Result<Vec<Address>> {
    let value: Option<String> = row.get(column)?;
    Ok(json_or_default(value.as_deref(), column))
}

fn row_to_summary(row: &Row<'_>) -> Result<MessageSummary> {
    let from: Option<String> = row.get("from_json")?;
    let importance: String = get_string(row, "importance")?;
    Ok(MessageSummary {
        id: get_string(row, "id")?,
        account_id: get_string(row, "account_id")?,
        folder_id: get_string(row, "folder_id")?,
        thread_id: get_string(row, "thread_id")?,
        subject: get_string(row, "subject")?,
        r#from: crate::repo::json_opt(from.as_deref(), "from_json"),
        to: addresses_from(row, "to_json")?,
        cc: addresses_from(row, "cc_json")?,
        date: get_dt(row, "date")?,
        preview: get_string(row, "preview")?,
        seen: get_bool(row, "seen")?,
        flagged: get_bool(row, "flagged")?,
        answered: get_bool(row, "answered")?,
        forwarded: get_bool(row, "forwarded")?,
        is_draft: get_bool(row, "is_draft")?,
        has_attachments: get_bool(row, "has_attachments")?,
        importance: importance.parse().unwrap_or(Importance::Normal),
        size: row.get::<_, i64>("size")?.max(0) as u64,
        thread_count: 1,
        thread_unread: 0,
    })
}

fn row_to_stored(row: &Row<'_>) -> Result<StoredMessage> {
    let summary = row_to_summary(row)?;
    let is_deleted_local = get_bool(row, "is_deleted_local")?;
    let references: Option<String> = row.get("references_json")?;
    let labels: Option<String> = row.get("labels_json")?;
    Ok(StoredMessage {
        summary,
        bcc: addresses_from(row, "bcc_json")?,
        reply_to: addresses_from(row, "reply_to_json")?,
        message_id_header: get_opt_string(row, "message_id_header")?,
        in_reply_to: get_opt_string(row, "in_reply_to")?,
        references: json_or_default(references.as_deref(), "references_json"),
        uid: row.get("uid")?,
        uidvalidity: row.get("uidvalidity")?,
        gm_msgid: get_opt_string(row, "gm_msgid")?,
        gm_thrid: get_opt_string(row, "gm_thrid")?,
        labels: json_or_default(labels.as_deref(), "labels_json"),
        raw_headers: get_opt_string(row, "raw_headers")?,
        body_path: get_opt_string(row, "body_path")?,
        body_cached_at: get_opt_dt(row, "body_cached_at")?,
        internal_date: get_opt_dt(row, "internal_date")?,
        is_deleted_local,
    })
}

/* ─────────────────────────────── writes ───────────────────────────── */

fn stored_values(stored: &StoredMessage, now: &str) -> Result<Vec<Value>> {
    let summary = &stored.summary;
    let mut thread_id = summary.thread_id.clone();
    if thread_id.trim().is_empty() {
        thread_id = summary.id.clone();
    }
    Ok(vec![
        text(&summary.id),
        text(&summary.account_id),
        text(&summary.folder_id),
        stored.uid.map(Value::Integer).unwrap_or(Value::Null),
        stored
            .uidvalidity
            .map(Value::Integer)
            .unwrap_or(Value::Null),
        stored
            .gm_msgid
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        stored
            .gm_thrid
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        stored
            .message_id_header
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        stored
            .in_reply_to
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        text(json_text(&stored.references)?),
        text(thread_id),
        text(&summary.subject),
        match &summary.r#from {
            Some(address) => text(json_text(address)?),
            None => Value::Null,
        },
        text(json_text(&summary.to)?),
        text(json_text(&summary.cc)?),
        text(json_text(&stored.bcc)?),
        text(json_text(&stored.reply_to)?),
        text(dt_string(summary.date)),
        stored
            .internal_date
            .map(|value| text(dt_string(value)))
            .unwrap_or(Value::Null),
        text(&summary.preview),
        int(summary.seen as i64),
        int(summary.flagged as i64),
        int(summary.answered as i64),
        int(summary.forwarded as i64),
        int(summary.is_draft as i64),
        int(summary.has_attachments as i64),
        text(summary.importance.as_str()),
        int(summary.size as i64),
        stored
            .body_path
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        stored
            .body_cached_at
            .map(|value| text(dt_string(value)))
            .unwrap_or(Value::Null),
        text(json_text(&stored.labels)?),
        int(stored.is_deleted_local as i64),
        stored
            .raw_headers
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        text(now),
        text(now),
    ])
}

const UPSERT_SQL: &str = "\
INSERT INTO message (
    id, account_id, folder_id, uid, uidvalidity, gm_msgid, gm_thrid, message_id_header,
    in_reply_to, references_json, thread_id, subject, from_json, to_json, cc_json, bcc_json,
    reply_to_json, date, internal_date, preview, seen, flagged, answered, forwarded, is_draft,
    has_attachments, importance, size, body_path, body_cached_at, labels_json, is_deleted_local,
    raw_headers, created_at, updated_at
) VALUES (
    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20,
    ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35
)
ON CONFLICT(id) DO UPDATE SET
    account_id = excluded.account_id,
    folder_id = excluded.folder_id,
    uid = excluded.uid,
    uidvalidity = excluded.uidvalidity,
    gm_msgid = COALESCE(excluded.gm_msgid, message.gm_msgid),
    gm_thrid = COALESCE(excluded.gm_thrid, message.gm_thrid),
    message_id_header = COALESCE(excluded.message_id_header, message.message_id_header),
    in_reply_to = COALESCE(excluded.in_reply_to, message.in_reply_to),
    references_json = excluded.references_json,
    thread_id = excluded.thread_id,
    subject = excluded.subject,
    from_json = excluded.from_json,
    to_json = excluded.to_json,
    cc_json = excluded.cc_json,
    bcc_json = excluded.bcc_json,
    reply_to_json = excluded.reply_to_json,
    date = excluded.date,
    internal_date = COALESCE(excluded.internal_date, message.internal_date),
    -- Metadata sync carries no preview text; never let it wipe the one a full
    -- body fetch already stored.
    preview = CASE WHEN excluded.preview <> '' THEN excluded.preview ELSE message.preview END,
    seen = excluded.seen,
    flagged = excluded.flagged,
    answered = excluded.answered,
    forwarded = excluded.forwarded,
    is_draft = excluded.is_draft,
    has_attachments = excluded.has_attachments,
    importance = excluded.importance,
    size = excluded.size,
    body_path = COALESCE(excluded.body_path, message.body_path),
    body_cached_at = COALESCE(excluded.body_cached_at, message.body_cached_at),
    labels_json = excluded.labels_json,
    is_deleted_local = excluded.is_deleted_local,
    raw_headers = COALESCE(excluded.raw_headers, message.raw_headers),
    updated_at = excluded.updated_at";

pub fn upsert(conn: &Connection, stored: &StoredMessage) -> Result<()> {
    let now = now_string();
    let values = stored_values(stored, &now)?;
    let refs = crate::repo::query_params(&values);
    conn.execute(UPSERT_SQL, refs.as_slice())?;
    refresh_fts(conn, &stored.summary.id)
}

/// Insert or update many messages (one transaction when called from `Db::write`).
pub fn bulk_upsert(conn: &Connection, messages: &[StoredMessage]) -> Result<usize> {
    for message in messages {
        upsert(conn, message)?;
    }
    debug!(count = messages.len(), "upserted message summaries");
    Ok(messages.len())
}

fn refresh_fts(conn: &Connection, message_id: &str) -> Result<()> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT subject FROM message WHERE id = ?1",
            params![message_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(subject) = stored else {
        return Ok(());
    };
    let addresses: String = conn.query_row(
        "SELECT COALESCE(from_json, '') || ' ' || to_json || ' ' || cc_json FROM message WHERE id = ?1",
        params![message_id],
        |row| row.get(0),
    )?;
    let preview: String = conn.query_row(
        "SELECT preview FROM message WHERE id = ?1",
        params![message_id],
        |row| row.get(0),
    )?;
    let body: String = conn
        .query_row(
            "SELECT body FROM fts_mail WHERE message_id = ?1 LIMIT 1",
            params![message_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_default();
    let addresses = addresses.replace(['{', '}', '"'], " ");
    let bigram = cjk_bigrams(&format!("{subject} {addresses} {preview} {body}"));

    let updated = conn.execute(
        "UPDATE fts_mail SET subject = ?2, addresses = ?3, preview = ?4, bigram = ?5 WHERE message_id = ?1",
        params![message_id, subject, addresses, preview, bigram],
    )?;
    if updated == 0 {
        conn.execute(
            "INSERT INTO fts_mail (message_id, subject, addresses, preview, body, bigram)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![message_id, subject, addresses, preview, body, bigram],
        )?;
    }
    Ok(())
}

/// Store the parsed body text (indexed in FTS5) and optionally the on-disk path.
pub fn set_body(
    conn: &Connection,
    id: &str,
    body_path: Option<&str>,
    body_text: Option<&str>,
) -> Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM message WHERE id = ?1)",
        params![id],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(AppError::NotFound(format!("message {id} not found")));
    }
    if body_path.is_some() {
        conn.execute(
            "UPDATE message SET body_path = ?2, body_cached_at = ?3, updated_at = ?3 WHERE id = ?1",
            params![id, body_path, now_string()],
        )?;
    } else {
        conn.execute(
            "UPDATE message SET body_path = NULL, body_cached_at = NULL, updated_at = ?2 WHERE id = ?1",
            params![id, now_string()],
        )?;
    }
    if let Some(text) = body_text {
        refresh_fts(conn, id)?;
        let (subject, addresses, preview): (String, String, String) = conn.query_row(
            "SELECT subject, addresses, preview FROM fts_mail WHERE message_id = ?1 LIMIT 1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let bigram = cjk_bigrams(&format!("{subject} {addresses} {preview} {text}"));
        conn.execute(
            "UPDATE fts_mail SET body = ?2, bigram = ?3 WHERE message_id = ?1",
            params![id, text, bigram],
        )?;
    }
    Ok(())
}

/// Update the list preview text and its search index.
pub fn update_preview(conn: &Connection, id: &str, preview: &str) -> Result<()> {
    conn.execute(
        "UPDATE message SET preview = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, preview, now_string()],
    )?;
    refresh_fts(conn, id)
}

pub fn set_flags(
    conn: &Connection,
    ids: &[String],
    seen: Option<bool>,
    flagged: Option<bool>,
    answered: Option<bool>,
    forwarded: Option<bool>,
) -> Result<usize> {
    let mut count = 0;
    for id in ids {
        let changed = conn.execute(
            "UPDATE message SET
                 seen = COALESCE(?2, seen),
                 flagged = COALESCE(?3, flagged),
                 answered = COALESCE(?4, answered),
                 forwarded = COALESCE(?5, forwarded),
                 updated_at = ?6
             WHERE id = ?1",
            params![
                id,
                seen.map(|value| value as i64),
                flagged.map(|value| value as i64),
                answered.map(|value| value as i64),
                forwarded.map(|value| value as i64),
                now_string()
            ],
        )?;
        count += changed;
    }
    refresh_counts_for_messages(conn, ids)?;
    Ok(count)
}

pub fn move_to_folder(conn: &Connection, ids: &[String], folder_id: &str) -> Result<usize> {
    // `is_draft` must follow the folder: the unified drafts view counts the
    // flag, so a draft moved to Trash (or any other folder) has to lose it.
    // Moving between two Drafts-role folders (the local placeholder and the
    // server folder) is a migration and keeps the flag.
    let target_is_drafts = conn
        .query_row(
            "SELECT role = ?2 FROM folder WHERE id = ?1",
            params![folder_id, FolderRole::Drafts.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some_and(|value| value != 0);
    let mut affected_folders = Vec::new();
    for id in ids {
        if let Some(old) = conn
            .query_row(
                "SELECT folder_id FROM message WHERE id = ?1",
                params![id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            if !affected_folders.contains(&old) {
                affected_folders.push(old);
            }
        }
    }
    let mut count = 0;
    for id in ids {
        count += conn.execute(
            "UPDATE message SET
                 folder_id = ?2,
                 is_draft = CASE
                     WHEN is_draft = 1 AND ?4 = 0
                          AND folder_id IN (SELECT id FROM folder WHERE role = ?5)
                     THEN 0 ELSE is_draft END,
                 updated_at = ?3
             WHERE id = ?1",
            params![
                id,
                folder_id,
                now_string(),
                target_is_drafts as i64,
                FolderRole::Drafts.as_str()
            ],
        )?;
    }
    affected_folders.push(folder_id.to_string());
    for folder in affected_folders {
        crate::repo::folders::recompute_counts(conn, &folder)?;
    }
    Ok(count)
}

/// Delete messages locally. `permanent = false` hides them (soft delete).
pub fn delete_local(conn: &Connection, ids: &[String], permanent: bool) -> Result<usize> {
    let folders = folders_for_ids(conn, ids)?;
    let mut count = 0;
    for id in ids {
        if permanent {
            conn.execute("DELETE FROM fts_mail WHERE message_id = ?1", params![id])?;
            count += conn.execute("DELETE FROM message WHERE id = ?1", params![id])?;
        } else {
            count += conn.execute(
                "UPDATE message SET is_deleted_local = 1, updated_at = ?2 WHERE id = ?1",
                params![id, now_string()],
            )?;
        }
    }
    for folder in folders {
        crate::repo::folders::recompute_counts(conn, &folder)?;
    }
    Ok(count)
}

fn folders_for_ids(conn: &Connection, ids: &[String]) -> Result<Vec<String>> {
    let mut folders: Vec<String> = Vec::new();
    for id in ids {
        if let Some(folder) = conn
            .query_row(
                "SELECT folder_id FROM message WHERE id = ?1",
                params![id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            if !folders.contains(&folder) {
                folders.push(folder);
            }
        }
    }
    Ok(folders)
}

/// Every message id stored in a folder, including rows hidden by a local
/// soft delete (they still belong to the folder and must move with it).
pub fn ids_in_folder(conn: &Connection, folder_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT id FROM message WHERE folder_id = ?1")?;
    let mut rows = stmt.query(params![folder_id])?;
    let mut ids = Vec::new();
    while let Some(row) = rows.next()? {
        ids.push(row.get(0)?);
    }
    Ok(ids)
}

/// Physically delete every message in a folder (empty trash / junk).
pub fn delete_all_in_folder(conn: &Connection, folder_id: &str) -> Result<usize> {
    conn.execute(
        "DELETE FROM fts_mail WHERE message_id IN (SELECT id FROM message WHERE folder_id = ?1)",
        params![folder_id],
    )?;
    let count = conn.execute(
        "DELETE FROM message WHERE folder_id = ?1",
        params![folder_id],
    )?;
    crate::repo::folders::recompute_counts(conn, folder_id)?;
    Ok(count)
}

pub fn mark_all_read(conn: &Connection, folder_id: &str) -> Result<usize> {
    let count = conn.execute(
        "UPDATE message SET seen = 1, updated_at = ?2 WHERE folder_id = ?1 AND seen = 0",
        params![folder_id, now_string()],
    )?;
    crate::repo::folders::recompute_counts(conn, folder_id)?;
    Ok(count)
}

fn refresh_counts_for_messages(conn: &Connection, ids: &[String]) -> Result<()> {
    let mut folders: Vec<String> = Vec::new();
    for id in ids {
        if let Some(folder) = conn
            .query_row(
                "SELECT folder_id FROM message WHERE id = ?1",
                params![id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            if !folders.contains(&folder) {
                folders.push(folder);
            }
        }
    }
    for folder in folders {
        crate::repo::folders::recompute_counts(conn, &folder)?;
    }
    Ok(())
}

/* ─────────────────────────────── reads ────────────────────────────── */

pub fn get(conn: &Connection, id: &str) -> Result<Option<StoredMessage>> {
    let sql = format!("SELECT {MESSAGE_COLUMNS} FROM message m WHERE m.id = ?1");
    crate::repo::query_one(conn, &sql, params![id], row_to_stored)
}

pub fn get_required(conn: &Connection, id: &str) -> Result<StoredMessage> {
    get(conn, id)?.ok_or_else(|| AppError::NotFound(format!("message {id} not found")))
}

pub fn get_summary(conn: &Connection, id: &str) -> Result<Option<MessageSummary>> {
    Ok(get(conn, id)?.map(|stored| stored.summary))
}

pub fn by_uid(conn: &Connection, folder_id: &str, uid: i64) -> Result<Option<StoredMessage>> {
    let sql =
        format!("SELECT {MESSAGE_COLUMNS} FROM message m WHERE m.folder_id = ?1 AND m.uid = ?2");
    crate::repo::query_one(conn, &sql, params![folder_id, uid], row_to_stored)
}

/// All messages carrying a given `Message-ID` header (optionally scoped to an account).
pub fn by_message_id(
    conn: &Connection,
    message_id: &str,
    account_id: Option<&str>,
) -> Result<Vec<StoredMessage>> {
    let sql = format!(
        "SELECT {MESSAGE_COLUMNS} FROM message m
         WHERE m.message_id_header = ?1 AND (?2 IS NULL OR m.account_id = ?2)
         ORDER BY m.date ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![message_id, account_id])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        result.push(row_to_stored(row)?);
    }
    Ok(result)
}

/// Conversation members, oldest first.
pub fn thread(conn: &Connection, account_id: &str, thread_id: &str) -> Result<Vec<MessageSummary>> {
    let sql = format!(
        "SELECT {MESSAGE_COLUMNS} FROM message m
         WHERE m.account_id = ?1 AND m.thread_id = ?2 AND m.is_deleted_local = 0
         ORDER BY m.date ASC, m.id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![account_id, thread_id])?;
    let mut items = Vec::new();
    while let Some(row) = rows.next()? {
        items.push(row_to_summary(row)?);
    }
    let count = items.len() as i64;
    let unread = items.iter().filter(|item| !item.seen).count() as i64;
    for item in &mut items {
        item.thread_count = count;
        item.thread_unread = unread;
    }
    Ok(items)
}

pub fn unread_total(conn: &Connection, account_id: Option<&str>) -> Result<i64> {
    let count = match account_id {
        Some(account_id) => conn.query_row(
            "SELECT COUNT(*) FROM message m
             JOIN folder f ON f.id = m.folder_id
             WHERE m.account_id = ?1 AND m.is_deleted_local = 0 AND m.seen = 0 AND f.role = 'inbox'",
            params![account_id],
            |row| row.get(0),
        )?,
        None => conn.query_row(
            "SELECT COUNT(*) FROM message m
             JOIN folder f ON f.id = m.folder_id
             JOIN account a ON a.id = m.account_id
             WHERE m.is_deleted_local = 0 AND m.seen = 0 AND f.role = 'inbox'
               AND a.include_in_unified = 1 AND a.mail_enabled = 1",
            [],
            |row| row.get(0),
        )?,
    };
    Ok(count)
}

pub fn unread_by_account(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT m.account_id, COUNT(*) FROM message m
         JOIN folder f ON f.id = m.folder_id
         WHERE m.is_deleted_local = 0 AND m.seen = 0 AND f.role = 'inbox'
         GROUP BY m.account_id",
    )?;
    let mut rows = stmt.query([])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        result.push((row.get::<_, String>(0)?, row.get::<_, i64>(1)?));
    }
    Ok(result)
}

pub fn count_in_folder(conn: &Connection, folder_id: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM message WHERE folder_id = ?1 AND is_deleted_local = 0",
        params![folder_id],
        |row| row.get(0),
    )?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyCacheEntry {
    pub id: String,
    pub body_path: String,
    pub cached_at: Option<DateTime<Utc>>,
}

/// Provider kinds whose cached bodies are the only local copy: POP3 mailboxes
/// are downloaded once and the server copy may be gone, so those rows must
/// never be treated as evictable cache.
pub const PERMANENT_BODY_PROVIDERS: &[&str] = &["pop3"];

/// Render a SQL list of account ids whose stored bodies are irreplaceable.
///
/// Callers embed the result as `account_id NOT IN ({list})`; an empty set is
/// rendered as `''` so the statement stays valid.
pub fn permanent_body_accounts_sql(accounts: &[crate::models::Account]) -> String {
    let ids: Vec<String> = accounts
        .iter()
        .filter(|account| PERMANENT_BODY_PROVIDERS.contains(&account.provider.as_str()))
        .map(|account| format!("'{}'", account.id.replace('\'', "''")))
        .collect();
    if ids.is_empty() {
        "''".to_string()
    } else {
        ids.join(", ")
    }
}

/// Body cache entries ordered by least recently cached (LRU trimming).
///
/// Rows whose provider keeps the only copy of the message (POP3) are excluded
/// in SQL so they cannot crowd out evictable IMAP entries.
pub fn body_lru_candidates(conn: &Connection, limit: u32) -> Result<Vec<BodyCacheEntry>> {
    let excluded: String = PERMANENT_BODY_PROVIDERS
        .iter()
        .map(|provider| format!("'{provider}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let stmt_sql = format!(
        "SELECT id, body_path, body_cached_at FROM message
         WHERE body_path IS NOT NULL
           AND account_id NOT IN (
               SELECT id FROM account WHERE provider IN ({excluded})
           )
         ORDER BY COALESCE(body_cached_at, '1970-01-01T00:00:00Z') ASC
         LIMIT ?1"
    );
    let mut stmt = conn.prepare(&stmt_sql)?;
    let mut rows = stmt.query(params![limit.max(1) as i64])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        result.push(BodyCacheEntry {
            id: row.get(0)?,
            body_path: row.get(1)?,
            cached_at: crate::repo::parse_dt_opt(row.get(2)?)?,
        });
    }
    Ok(result)
}

/* ───────────────────────────── listing ────────────────────────────── */

struct QueryBuilder {
    clauses: Vec<String>,
    params: Vec<Value>,
}

impl QueryBuilder {
    fn new() -> Self {
        Self {
            clauses: vec!["m.is_deleted_local = 0".to_string()],
            params: Vec::new(),
        }
    }

    fn push(&mut self, clause: impl Into<String>, params: Vec<Value>) {
        self.clauses.push(clause.into());
        self.params.extend(params);
    }

    fn where_sql(&self) -> String {
        self.clauses.join(" AND ")
    }
}

fn build_scope(builder: &mut QueryBuilder, scope: &MailScope) -> Result<()> {
    match scope {
        MailScope::Folder { folder_id } => {
            builder.push("m.folder_id = ?", vec![text(folder_id)]);
        }
        MailScope::Unified { role } => {
            let unified_account =
                "m.account_id IN (SELECT id FROM account WHERE include_in_unified = 1 AND mail_enabled = 1)";
            match role {
                FolderRole::Flagged => {
                    builder.push(format!("m.flagged = 1 AND {unified_account}"), Vec::new())
                }
                FolderRole::Important => builder.push(
                    format!("m.importance = 'high' AND {unified_account}"),
                    Vec::new(),
                ),
                FolderRole::Drafts => {
                    builder.push(format!("m.is_draft = 1 AND {unified_account}"), Vec::new())
                }
                FolderRole::All => builder.push(unified_account, Vec::new()),
                other => {
                    builder.push(
                        "m.folder_id IN (SELECT f.id FROM folder f JOIN account a ON a.id = f.account_id
                         WHERE f.role = ? AND a.include_in_unified = 1 AND a.mail_enabled = 1)",
                        vec![text(other.as_str())],
                    );
                }
            }
        }
        MailScope::Search { query } => {
            if let Some(account_id) = &query.account_id {
                builder.push("m.account_id = ?", vec![text(account_id)]);
            }
            if let Some(folder_id) = &query.folder_id {
                builder.push("m.folder_id = ?", vec![text(folder_id)]);
            }
            let match_expression = fts_query(&query.text);
            if !match_expression.is_empty() {
                builder.push(
                    "m.id IN (SELECT message_id FROM fts_mail WHERE fts_mail MATCH ?)",
                    vec![text(match_expression)],
                );
            }
            if let Some(from) = query
                .from
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                builder.push(
                    "lower(m.from_json) LIKE ?",
                    vec![text(format!("%{}%", from.to_lowercase()))],
                );
            }
            if let Some(to) = query.to.as_deref().filter(|value| !value.trim().is_empty()) {
                let pattern = format!("%{}%", to.to_lowercase());
                builder.push(
                    "(lower(m.to_json) LIKE ? OR lower(m.cc_json) LIKE ? OR lower(m.bcc_json) LIKE ?)",
                    vec![text(pattern.clone()), text(pattern.clone()), text(pattern)],
                );
            }
            if let Some(subject) = query
                .subject
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                builder.push(
                    "lower(m.subject) LIKE ?",
                    vec![text(format!("%{}%", subject.to_lowercase()))],
                );
            }
            if let Some(value) = query.has_attachment {
                builder.push("m.has_attachments = ?", vec![int(value as i64)]);
            }
            if let Some(value) = query.unread {
                builder.push("m.seen = ?", vec![int(!value as i64)]);
            }
            if let Some(value) = query.flagged {
                builder.push("m.flagged = ?", vec![int(value as i64)]);
            }
            if let Some(from) = query
                .date_from
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                builder.push("substr(m.date, 1, 10) >= ?", vec![text(from)]);
            }
            if let Some(to) = query
                .date_to
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                builder.push("substr(m.date, 1, 10) <= ?", vec![text(to)]);
            }
        }
    }
    Ok(())
}

fn build_filter(builder: &mut QueryBuilder, filter: MailFilter) {
    match filter {
        MailFilter::All => {}
        MailFilter::Unread => builder.push("m.seen = 0", Vec::new()),
        MailFilter::Flagged => builder.push("m.flagged = 1", Vec::new()),
        MailFilter::Attachments => builder.push("m.has_attachments = 1", Vec::new()),
        MailFilter::MentionsMe => builder.push(
            "EXISTS (SELECT 1 FROM account a WHERE a.id = m.account_id
             AND (lower(m.to_json) LIKE '%' || lower(a.email) || '%'
                  OR lower(m.cc_json) LIKE '%' || lower(a.email) || '%'))",
            Vec::new(),
        ),
    }
}

/// Queued outgoing mail projected onto `MessageSummary` for the outbox view.
///
/// Outbox rows are not messages: they live in `outbox` until the SMTP worker
/// sends them. The unified outbox sidebar exposes them (its count comes from
/// the same table), so `message_list` must return them instead of an empty
/// page. `account_id = None` means "every unified account".
fn list_outbox(
    conn: &Connection,
    query: &crate::models::MessageListQuery,
    account_id: Option<&str>,
    unified_only: bool,
) -> Result<MessagePage> {
    let unified_clause = if unified_only {
        "AND a.include_in_unified = 1 AND a.mail_enabled = 1"
    } else {
        ""
    };
    let sql = format!(
        "SELECT o.id, o.account_id, o.subject, o.recipients_json, o.created_at, o.last_error,
                COALESCE(i.email, a.email) AS sender_email,
                COALESCE(NULLIF(i.name, ''), a.display_name) AS sender_name,
                f.id AS outbox_folder_id
         FROM outbox o
         JOIN account a ON a.id = o.account_id
         LEFT JOIN identity i ON i.id = (
             SELECT i2.id FROM identity i2
             WHERE i2.account_id = o.account_id
             ORDER BY i2.is_default DESC, i2.email ASC
             LIMIT 1
         )
         LEFT JOIN folder f ON f.id = (
             SELECT f2.id FROM folder f2
             WHERE f2.account_id = o.account_id AND f2.role = 'outbox'
             ORDER BY f2.sort_order ASC, f2.id ASC
             LIMIT 1
         )
         WHERE o.state <> 'sent' AND (?1 IS NULL OR o.account_id = ?1) {unified_clause}
         ORDER BY o.created_at DESC, o.id DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![account_id])?;
    let mut items = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get("id")?;
        let account: String = row.get("account_id")?;
        let created_at = get_dt(row, "created_at")?;
        let folder_id =
            get_opt_string(row, "outbox_folder_id")?.unwrap_or_else(|| format!("outbox:{account}"));
        let sender_name =
            get_opt_string(row, "sender_name")?.filter(|name| !name.trim().is_empty());
        let sender_email = get_opt_string(row, "sender_email")?;
        let recipients: Option<String> = row.get("recipients_json")?;
        let mut summary = MessageSummary::new(id.clone(), account, folder_id, id, created_at);
        summary.subject = get_string(row, "subject")?;
        summary.r#from = sender_email.map(|email| Address {
            name: sender_name,
            email,
        });
        summary.to = json_or_default(recipients.as_deref(), "recipients_json");
        summary.preview = get_opt_string(row, "last_error")?.unwrap_or_default();
        // Queued mail is never unread; a failed item shows its error as preview.
        summary.seen = true;
        summary.thread_count = 1;
        summary.thread_unread = 0;
        items.push(summary);
    }

    // Outbox entries carry no read/flag/attachment state, so every filter but
    // `all` legitimately matches nothing.
    if query.filter != MailFilter::All {
        items.clear();
    }
    let total = items.len() as i64;
    if let Some(raw) = &query.cursor {
        let cursor = PageCursor::decode(raw)?;
        items.retain(|item| {
            let key = item.date.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            key < cursor.key || (key == cursor.key && item.id < cursor.id)
        });
    }
    let limit = query.limit.clamp(1, 500) as usize;
    let has_more = items.len() > limit;
    items.truncate(limit);
    let next_cursor = if has_more {
        items
            .last()
            .map(|item| PageCursor::message(item.date, item.id.clone()).encode())
    } else {
        None
    };
    Ok(MessagePage {
        items,
        next_cursor,
        total,
    })
}

/// Paged message listing for the list pane.
pub fn list(conn: &Connection, query: &crate::models::MessageListQuery) -> Result<MessagePage> {
    // The outbox is a queue table, not a folder of messages.
    match &query.scope {
        MailScope::Unified {
            role: FolderRole::Outbox,
        } => return list_outbox(conn, query, None, true),
        MailScope::Folder { folder_id } => {
            let outbox_account = conn
                .query_row(
                    "SELECT account_id FROM folder WHERE id = ?1 AND role = ?2",
                    params![folder_id, FolderRole::Outbox.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if let Some(account_id) = outbox_account {
                return list_outbox(conn, query, Some(&account_id), false);
            }
        }
        _ => {}
    }

    let limit = query.limit.clamp(1, 500) as i64;
    let mut builder = QueryBuilder::new();
    build_scope(&mut builder, &query.scope)?;
    build_filter(&mut builder, query.filter);
    let where_sql = builder.where_sql();

    // Total (conversations counted once when grouping).
    let total_sql = if query.conversation {
        format!("SELECT COUNT(DISTINCT m.thread_id) FROM message m WHERE {where_sql}")
    } else {
        format!("SELECT COUNT(*) FROM message m WHERE {where_sql}")
    };
    let total_params = crate::repo::query_params(&builder.params);
    let total: i64 = conn.query_row(&total_sql, total_params.as_slice(), |row| row.get(0))?;

    let mut params = builder.params.clone();
    let mut cursor_clause = String::new();
    if let Some(raw) = &query.cursor {
        let cursor = PageCursor::decode(raw)?;
        if query.conversation {
            cursor_clause = " AND (date < ? OR (date = ? AND id < ?))".to_string();
        } else {
            cursor_clause = " AND (m.date < ? OR (m.date = ? AND m.id < ?))".to_string();
        }
        params.push(text(cursor.key.clone()));
        params.push(text(cursor.key));
        params.push(text(cursor.id));
    }
    params.push(int(limit + 1));

    let mut items: Vec<MessageSummary> = Vec::new();
    if query.conversation {
        let sql = format!(
            "WITH filtered AS (
                 SELECT m.* FROM message m WHERE {where_sql}
             ),
             ranked AS (
                 SELECT *,
                     ROW_NUMBER() OVER (PARTITION BY thread_id ORDER BY date DESC, id DESC) AS rn,
                     COUNT(*) OVER (PARTITION BY thread_id) AS tcount,
                     SUM(CASE WHEN seen = 0 THEN 1 ELSE 0 END) OVER (PARTITION BY thread_id) AS tunread
                 FROM filtered
             )
             SELECT * FROM ranked WHERE rn = 1{cursor_clause}
             ORDER BY date DESC, id DESC LIMIT ?"
        );
        let refs = crate::repo::query_params(&params);
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(refs.as_slice())?;
        while let Some(row) = rows.next()? {
            let mut summary = row_to_summary(row)?;
            summary.thread_count = row.get::<_, Option<i64>>("tcount")?.unwrap_or(1).max(1);
            summary.thread_unread = row.get::<_, Option<i64>>("tunread")?.unwrap_or(0).max(0);
            items.push(summary);
        }
    } else {
        let sql = format!(
            "SELECT {MESSAGE_COLUMNS}, 1 AS tcount,
                    CASE WHEN m.seen = 0 THEN 1 ELSE 0 END AS tunread
             FROM message m WHERE {where_sql}{cursor_clause}
             ORDER BY m.date DESC, m.id DESC LIMIT ?"
        );
        let refs = crate::repo::query_params(&params);
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(refs.as_slice())?;
        while let Some(row) = rows.next()? {
            let mut summary = row_to_summary(row)?;
            summary.thread_count = 1;
            summary.thread_unread = if summary.seen { 0 } else { 1 };
            items.push(summary);
        }
    }

    let has_more = items.len() as i64 > limit;
    if has_more {
        items.truncate(limit as usize);
    }
    let next_cursor = if has_more {
        items
            .last()
            .map(|item| PageCursor::message(item.date, item.id.clone()).encode())
    } else {
        None
    };

    Ok(MessagePage {
        items,
        next_cursor,
        total,
    })
}

/// Delete a whole thread locally (used by `message_act_threads` when needed).
pub fn thread_message_ids(
    conn: &Connection,
    account_id: &str,
    thread_id: &str,
) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM message WHERE account_id = ?1 AND thread_id = ?2 AND is_deleted_local = 0",
    )?;
    let mut rows = stmt.query(params![account_id, thread_id])?;
    let mut ids = Vec::new();
    while let Some(row) = rows.next()? {
        ids.push(row.get(0)?);
    }
    Ok(ids)
}

/// Rebuild the search index for one message (from the stored body, if any).
pub fn reindex_fts(conn: &Connection, id: &str) -> Result<()> {
    refresh_fts(conn, id)?;
    let body: Option<String> = conn
        .query_row(
            "SELECT body FROM fts_mail WHERE message_id = ?1 LIMIT 1",
            params![id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(body) = body {
        let path = get(conn, id)?.and_then(|stored| stored.body_path);
        set_body(conn, id, path.as_deref(), Some(&body))?;
    }
    Ok(())
}

/// Latest message date in a folder (drives freshness checks).
pub fn latest_date(conn: &Connection, folder_id: &str) -> Result<Option<DateTime<Utc>>> {
    let value: Option<String> = conn
        .query_row(
            "SELECT MAX(date) FROM message WHERE folder_id = ?1",
            params![folder_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    match value {
        Some(value) => parse_dt(&value).map(Some),
        None => Ok(None),
    }
}

/// Number of messages whose body is cached (used by cache settings UI).
pub fn cached_body_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM message WHERE body_path IS NOT NULL",
        [],
        |row| row.get(0),
    )?)
}

/// Export helpers used by `message_save_eml` tests / backup.
pub fn body_path(conn: &Connection, id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT body_path FROM message WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()?
        .flatten())
}

pub fn set_thread(conn: &Connection, id: &str, thread_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE message SET thread_id = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, thread_id, now_string()],
    )?;
    Ok(())
}

pub fn set_labels(conn: &Connection, id: &str, labels: &[String]) -> Result<()> {
    conn.execute(
        "UPDATE message SET labels_json = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, json_text(&labels)?, now_string()],
    )?;
    Ok(())
}

pub fn set_deleted_local(conn: &Connection, id: &str, value: bool) -> Result<()> {
    conn.execute(
        "UPDATE message SET is_deleted_local = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, value as i64, now_string()],
    )?;
    Ok(())
}

/// UIDVALIDITY changed: drop cached metadata for a folder.
pub fn clear_folder(conn: &Connection, folder_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM fts_mail WHERE message_id IN (SELECT id FROM message WHERE folder_id = ?1)",
        params![folder_id],
    )?;
    conn.execute(
        "DELETE FROM message WHERE folder_id = ?1",
        params![folder_id],
    )?;
    crate::repo::folders::recompute_counts(conn, folder_id)
}
