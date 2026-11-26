//! Message repository: bulk upsert, flags, local moves, paged listing
//! (folder / unified / search), conversation grouping and FTS5 indexing.

use std::collections::{HashMap, HashSet};

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

/// A permanent delete may have reached the server even though the command
/// looked refused. Only a definitive `UID SEARCH` answer in the same
/// UIDVALIDITY generation may conclude that the message is gone.
pub const RECONCILE_PENDING_DELETE: &str = "pendingDelete";
/// The folder-local identity is unknown (the command may have executed). The
/// row must be bound again through a targeted server lookup.
pub const RECONCILE_REBIND: &str = "rebind";
/// A failed move left the optimistic projection in its target folder and the
/// source/target evidence was not conclusive. The op that moved it still owns
/// the decision.
pub const RECONCILE_MOVE_ROLLBACK: &str = "moveRollback";

/// Select list for message rows (qualified with alias `m`).
pub const MESSAGE_COLUMNS: &str = "\
    m.id, m.account_id, m.folder_id, m.thread_id, m.subject, m.from_json, m.to_json, m.cc_json,\
    m.date, m.preview, m.seen, m.flagged, m.answered, m.forwarded, m.is_draft, m.has_attachments,\
    m.importance, m.size, m.bcc_json, m.reply_to_json, m.message_id_header, m.in_reply_to,\
    m.references_json, m.uid, m.uidvalidity, m.gm_msgid, m.gm_thrid, m.labels_json, m.raw_headers,\
    m.body_path, m.body_identity, m.body_cached_at, m.internal_date, m.is_deleted_local,\
    m.reconcile_reason";

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
        reconcile_reason: get_opt_string(row, "reconcile_reason")?,
        draft_sync_state: None,
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
        body_identity: get_opt_string(row, "body_identity")?,
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
    -- A sync that resolved the row's folder-local identity clears the
    -- waiting-for-the-target marker written by an optimistic move. The
    -- reconciliation reason and the owning operation go with it: they only
    -- describe a row whose identity is unknown.
    target_pending = CASE WHEN excluded.uid IS NULL THEN message.target_pending ELSE 0 END,
    -- The row now owns a folder-local identity, so the confirmed move
    -- destination it was waiting for is no longer needed. Clearing it here
    -- (with the binding) keeps the temporary identity from outliving it.
    move_target_remote_id = CASE WHEN excluded.uid IS NULL THEN message.move_target_remote_id ELSE NULL END,
    move_target_folder_id = CASE WHEN excluded.uid IS NULL THEN message.move_target_folder_id ELSE NULL END,
    move_target_op_id = CASE WHEN excluded.uid IS NULL THEN message.move_target_op_id ELSE NULL END,
    move_target_seq = CASE WHEN excluded.uid IS NULL THEN message.move_target_seq ELSE NULL END,
    awaiting_reconcile = CASE WHEN excluded.uid IS NULL THEN message.awaiting_reconcile ELSE 0 END,
    reconcile_reason = CASE WHEN excluded.uid IS NULL THEN message.reconcile_reason ELSE NULL END,
    reconcile_op_id = CASE WHEN excluded.uid IS NULL THEN message.reconcile_op_id ELSE NULL END,
    reconcile_seq = CASE WHEN excluded.uid IS NULL THEN message.reconcile_seq ELSE NULL END,
    reconcile_uid = CASE WHEN excluded.uid IS NULL THEN message.reconcile_uid ELSE NULL END,
    reconcile_uidvalidity = CASE WHEN excluded.uid IS NULL THEN message.reconcile_uidvalidity ELSE NULL END,
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
///
/// `body_identity` is the provider copy key captured *before* the body was
/// fetched (see [`body_identity_key`]). It is persisted together with the path,
/// so a later reader can prove the cached bytes belong to the same copy the row
/// currently names. Passing `None` records "source unknown".
pub fn set_body(
    conn: &Connection,
    id: &str,
    body_path: Option<&str>,
    body_text: Option<&str>,
    body_identity: Option<&str>,
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
            "UPDATE message SET body_path = ?2, body_cached_at = ?3, body_identity = ?4,
                    updated_at = ?3 WHERE id = ?1",
            params![id, body_path, now_string(), body_identity],
        )?;
    } else {
        conn.execute(
            "UPDATE message SET body_path = NULL, body_cached_at = NULL, body_identity = NULL,
                    updated_at = ?2 WHERE id = ?1",
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

/// Ids among `ids` whose optimistic move has not been confirmed by a sync yet.
pub fn target_pending_ids(conn: &Connection, ids: &[String]) -> Result<Vec<String>> {
    let mut statement =
        conn.prepare("SELECT id FROM message WHERE id = ?1 AND target_pending = 1")?;
    let mut pending = Vec::new();
    for id in ids {
        let mut rows = statement.query(params![id])?;
        if rows.next()?.is_some() {
            pending.push(id.clone());
        }
    }
    Ok(pending)
}

/// A confirmed move destination recorded on a waiting projection row.
///
/// The identity is owned by the operation that wrote it. A row can leave and
/// later return to the same folder, so the owner (and its queue sequence, the
/// explicit trip order) is what tells the current trip's identity apart from
/// the receipt of an earlier one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveTargetIdentity {
    /// Local row id of the projection.
    pub local_id: String,
    /// Provider (Graph) id of the copy the move created.
    pub remote_id: String,
    /// Local folder the provider id belongs to.
    pub folder_id: String,
    /// Operation that wrote the identity. `None` for rows written by a build
    /// that predates the ownership column.
    pub owner_op_id: Option<String>,
    /// Queue sequence of `owner_op_id`, used to compare trips across
    /// operations.
    pub owner_sequence: Option<i64>,
}

/// Persist a confirmed move's destination identity on the projection row.
///
/// The operation queue is housekeeping-pruned, but a destination that has not
/// appeared in a delta yet must stay attributable: a stale update/removal
/// carrying the row's old provider id must not rebind or delete it, and a
/// second copy sharing the Message-ID must not consume it. Writing the receipt
/// onto the row itself makes that protection independent of `pending_op`
/// retention.
///
/// The update is conditional on the row still being the unbound projection the
/// move created: same account, already in the destination folder, no
/// folder-local identity, `target_pending = 1`. Anything else (a row that
/// already bound, or a move an undo reverted) must not receive this identity.
///
/// Ownership is checked too. The *effective* owner is the newer of the
/// move-target slot and the reconciliation owner (see
/// [`projection_owner`]): a failed trip clears its move slot on purpose, so
/// "move slot is NULL" must not be read as "unowned". An identity already
/// owned by the *same* operation may be refreshed, and a genuinely unowned row
/// (for example a receipt recorded before migration 0024) may be adopted. An
/// identity owned by another, newer operation must never be overwritten: the
/// retry of an old batch would otherwise replace the newer trip's destination
/// with the id of a copy that has already left, and an `@removed` for that id
/// would delete the new projection. Conversely, a strictly newer operation is
/// allowed to replace an older owner's leftover identity, which is what keeps
/// a legitimate confirmation from being blocked by stale evidence.
///
/// Returns the number of rows changed (`0` means the row is not this
/// operation's projection any more).
pub fn set_move_target_identity(
    conn: &Connection,
    account_id: &str,
    message_id: &str,
    target_folder_id: &str,
    target_remote_id: &str,
    owner_op_id: &str,
    owner_sequence: i64,
) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE message
            SET move_target_remote_id = ?4,
                move_target_folder_id = ?3,
                move_target_op_id = NULLIF(NULLIF(TRIM(?6), ''), '0'),
                move_target_seq = ?7,
                updated_at = ?5
          WHERE id = ?1 AND account_id = ?2 AND folder_id = ?3
            AND uid IS NULL AND target_pending = 1
            -- Ownership covers both records: the move-target slot and the
            -- reconciliation owner left by a failed trip. A blank token is
            -- not an owner; only the effective owner may be refreshed, and
            -- only a strictly newer trip may take the row over.
            AND (
                (NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0') IS NULL
                 AND NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0') IS NULL)
                OR CASE
                       WHEN NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0') IS NOT NULL
                            AND (NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0') IS NULL
                                 OR COALESCE(move_target_seq, 0)
                                    >= COALESCE(reconcile_seq, 0))
                       THEN TRIM(move_target_op_id)
                       ELSE TRIM(reconcile_op_id)
                   END = ?6
                OR COALESCE(
                       CASE
                           WHEN NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0') IS NOT NULL
                                AND (NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0') IS NULL
                                     OR COALESCE(move_target_seq, 0)
                                        >= COALESCE(reconcile_seq, 0))
                           THEN move_target_seq
                           ELSE reconcile_seq
                       END,
                       0) < ?7
            )",
        params![
            message_id,
            account_id,
            target_folder_id,
            target_remote_id,
            now_string(),
            owner_op_id,
            owner_sequence
        ],
    )?)
}

/// Claim the identity slot of a fresh optimistic move for `owner_op_id`.
///
/// This runs in the same transaction that moves the projection and queues the
/// operation. Without it, the slot would stay unowned between the optimistic
/// move and the confirmation, and a retry of an *earlier* batch could install
/// its stale destination there in the meantime. The claim records only the
/// owner; the confirmed destination is written by
/// [`set_move_target_identity`] when the wire command succeeds.
///
/// The same ordering rule as [`set_move_target_identity`] applies: an unowned
/// row or the effective owner may claim, and a strictly newer operation may
/// take over from an older one, but an older operation must not displace a
/// newer claim — including a claim whose move slot was cleared by a failed
/// trip and now lives in the reconciliation owner.
pub fn claim_move_target_owner(
    conn: &Connection,
    account_id: &str,
    message_ids: &[String],
    folder_id: &str,
    owner_op_id: &str,
    owner_sequence: i64,
) -> Result<usize> {
    let now = now_string();
    let mut claimed = 0;
    for id in message_ids {
        claimed += conn.execute(
            "UPDATE message
                SET move_target_op_id = NULLIF(NULLIF(TRIM(?4), ''), '0'),
                    move_target_seq = ?5,
                    updated_at = ?6
              WHERE id = ?1 AND account_id = ?2 AND folder_id = ?3
                AND uid IS NULL AND target_pending = 1
                AND (
                    (NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0') IS NULL
                     AND NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0') IS NULL)
                    OR CASE
                           WHEN NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0') IS NOT NULL
                                AND (NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0') IS NULL
                                     OR COALESCE(move_target_seq, 0)
                                        >= COALESCE(reconcile_seq, 0))
                           THEN TRIM(move_target_op_id)
                           ELSE TRIM(reconcile_op_id)
                       END = ?4
                    OR COALESCE(
                           CASE
                               WHEN NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0') IS NOT NULL
                                    AND (NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0') IS NULL
                                         OR COALESCE(move_target_seq, 0)
                                            >= COALESCE(reconcile_seq, 0))
                               THEN move_target_seq
                               ELSE reconcile_seq
                           END,
                           0) < ?5
                )",
            params![id, account_id, folder_id, owner_op_id, owner_sequence, now],
        )?;
    }
    Ok(claimed)
}

/// Confirmed move destinations whose projection is still waiting in
/// `folder_id`, read from the rows themselves.
///
/// Only unbound projections are returned, and the recorded folder must match
/// the row's current folder so an identity left over from an earlier trip into
/// the same mailbox is ignored. Because the receipt lives on the row, this
/// survives pruning of the operation queue that originally confirmed the move.
///
/// A destination is only reported while the move-target slot is still the
/// effective owner. After a newer trip settled in reconciliation, the receipt
/// left on the row belongs to an older trip and must not resolve (or delete)
/// the current projection.
pub fn move_target_identities(
    conn: &Connection,
    account_id: &str,
    folder_id: &str,
) -> Result<Vec<MoveTargetIdentity>> {
    let mut statement = conn.prepare(
        "SELECT id, move_target_remote_id, move_target_folder_id,
                move_target_op_id, move_target_seq, reconcile_op_id, reconcile_seq
           FROM message
          WHERE account_id = ?1 AND folder_id = ?2
            AND uid IS NULL AND target_pending = 1
            AND move_target_remote_id IS NOT NULL
            AND TRIM(move_target_remote_id) <> ''
            AND (move_target_folder_id IS NULL OR move_target_folder_id = ?2)",
    )?;
    let mut rows = statement.query(params![account_id, folder_id])?;
    let mut identities = Vec::new();
    while let Some(row) = rows.next()? {
        let local_id: String = row.get(0)?;
        let remote_id: String = row.get(1)?;
        let folder: Option<String> = row.get(2)?;
        // A blank token means "written by nobody" (legacy rollback). Treat it
        // exactly like a missing owner everywhere below.
        let move_owner = normalized_owner_token(row.get(3)?);
        let move_sequence: Option<i64> = row.get(4)?;
        let reconcile_owner = normalized_owner_token(row.get(5)?);
        let reconcile_sequence: Option<i64> = row.get(6)?;
        let owner = effective_projection_owner(
            move_owner.clone(),
            move_sequence,
            reconcile_owner.clone(),
            reconcile_sequence,
        );
        // Only the owner that wrote the destination id may stand behind it. A
        // newer reconciliation trip (a failed move whose destination is not
        // known) leaves the older receipt on the row; that receipt is
        // historical evidence and must not resolve the current projection.
        let identity_is_current = match (&move_owner, &reconcile_owner) {
            (None, None) => true,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some(move_id), Some(reconcile_id)) => {
                move_id == reconcile_id
                    || move_sequence.unwrap_or(0) >= reconcile_sequence.unwrap_or(0)
            }
        };
        if !identity_is_current {
            continue;
        }
        identities.push(MoveTargetIdentity {
            local_id,
            remote_id,
            folder_id: folder.unwrap_or_else(|| folder_id.to_string()),
            owner_op_id: owner.as_ref().map(|owner| owner.op_id.clone()),
            owner_sequence: owner.as_ref().and_then(|owner| owner.sequence),
        });
    }
    Ok(identities)
}

/// Operations that own the identity slot of waiting projections in
/// `folder_id`, including rows that have claimed the slot but have not
/// recorded a destination yet.
///
/// Keyed by local row id -> `(operation id, queue sequence)`. A claim alone is
/// enough to say that a retained receipt from any other operation is
/// historical evidence: the alias/backing rules must not let it stand in for
/// the current trip's (not yet confirmed) destination.
///
/// The owner is the effective one ([`projection_owner`]): a trip that failed
/// in an ambiguous state keeps the row through `reconcile_op_id` even though
/// its move slot was cleared.
pub fn move_target_owners(
    conn: &Connection,
    account_id: &str,
    folder_id: &str,
) -> Result<HashMap<String, (String, Option<i64>)>> {
    let mut statement = conn.prepare(
        "SELECT id, move_target_op_id, move_target_seq, reconcile_op_id, reconcile_seq
           FROM message
          WHERE account_id = ?1 AND folder_id = ?2
            AND uid IS NULL AND target_pending = 1
            AND (NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0') IS NOT NULL
                 OR NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0') IS NOT NULL)
            AND (move_target_folder_id IS NULL OR move_target_folder_id = ?2)",
    )?;
    let mut rows = statement.query(params![account_id, folder_id])?;
    let mut owners = HashMap::new();
    while let Some(row) = rows.next()? {
        let local_id: String = row.get(0)?;
        let move_op_id: Option<String> = row.get(1)?;
        let move_sequence: Option<i64> = row.get(2)?;
        let reconcile_op_id: Option<String> = row.get(3)?;
        let reconcile_sequence: Option<i64> = row.get(4)?;
        if let Some(owner) = effective_projection_owner(
            move_op_id,
            move_sequence,
            reconcile_op_id,
            reconcile_sequence,
        ) {
            owners.insert(local_id, (owner.op_id, owner.sequence));
        }
    }
    Ok(owners)
}

/// Operation that currently owns `id`'s move-target identity slot, if any.
///
/// Failure recovery uses this to leave a newer move trip alone: only an
/// unowned slot (`None`) or one owned by the recovering operation may be
/// rewritten.
pub fn move_target_owner(conn: &Connection, id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0')
               FROM message WHERE id = ?1",
            params![id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// The trip that currently owns a pending projection.
///
/// A row can hold two ownership records: the confirmed-destination slot
/// (`move_target_op_id`/`move_target_seq`, written by a move's confirmation)
/// and the reconciliation owner (`reconcile_op_id`/`reconcile_seq`, written
/// when a trip settles in an ambiguous state). A terminal failure clears the
/// move slot on purpose, so "move slot is NULL" must **not** be read as "no
/// owner": the reconciliation owner is still the current trip and an older
/// batch's retry must not fill the slot with its stale destination.
///
/// When both records exist the one with the newer queue sequence owns the row;
/// the move slot wins a tie (it is written together with an actual confirmed
/// destination). A missing sequence is treated as the oldest possible value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionOwner {
    pub op_id: String,
    pub sequence: Option<i64>,
}

/// Read an ownership token, treating a blank or `"0"` value as "no owner".
///
/// An empty token is not a trip: legacy rollback code stored `""` where it
/// meant "nobody owns this projection yet". Reading it as an owner would make
/// every legitimate binding look like it came from another operation, so the
/// row would stay pending forever and the next delta would import a duplicate.
fn normalized_owner_token(token: Option<String>) -> Option<String> {
    token
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && value != "0")
}

/// Combine the move-target and reconciliation ownership records of one row.
fn effective_projection_owner(
    move_op_id: Option<String>,
    move_sequence: Option<i64>,
    reconcile_op_id: Option<String>,
    reconcile_sequence: Option<i64>,
) -> Option<ProjectionOwner> {
    let move_op_id = normalized_owner_token(move_op_id);
    let reconcile_op_id = normalized_owner_token(reconcile_op_id);
    match (move_op_id, reconcile_op_id) {
        (None, None) => None,
        (Some(op_id), None) => Some(ProjectionOwner {
            op_id,
            sequence: move_sequence,
        }),
        (None, Some(op_id)) => Some(ProjectionOwner {
            op_id,
            sequence: reconcile_sequence,
        }),
        (Some(move_id), Some(reconcile_id)) => {
            if move_sequence.unwrap_or(0) >= reconcile_sequence.unwrap_or(0) {
                Some(ProjectionOwner {
                    op_id: move_id,
                    sequence: move_sequence,
                })
            } else {
                Some(ProjectionOwner {
                    op_id: reconcile_id,
                    sequence: reconcile_sequence,
                })
            }
        }
    }
}

/// Current trip token (`op_id` + queue sequence) of `id`'s pending projection.
///
/// Covers both the move-target identity slot and the reconciliation owner.
/// Recovery and undo code compares this against the operation(s) that are
/// allowed to touch the row; an owner mismatch means the row belongs to a newer
/// trip.
pub fn projection_owner(conn: &Connection, id: &str) -> Result<Option<ProjectionOwner>> {
    let row = conn
        .query_row(
            "SELECT move_target_op_id, move_target_seq, reconcile_op_id, reconcile_seq
               FROM message WHERE id = ?1",
            params![id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .optional()?;
    Ok(row.and_then(|(move_id, move_seq, reconcile_id, reconcile_seq)| {
        effective_projection_owner(move_id, move_seq, reconcile_id, reconcile_seq)
    }))
}

/// Whether `id` is still an unbound projection waiting for a move target or a
/// reconciliation (`target_pending` / `awaiting_reconcile`).
///
/// Callers that rebuild a row from evidence captured before a long-running
/// request (`upsert`/body write-back) use this inside their write transaction:
/// such a row belongs to the trip that put it in that state, so a stale
/// snapshot must not write over its folder, identity or markers.
pub fn is_pending_projection(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT COALESCE(target_pending, 0) <> 0 OR COALESCE(awaiting_reconcile, 0) <> 0
               FROM message WHERE id = ?1",
            params![id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some_and(|value| value != 0))
}

/// Confirmed move destinations (provider id -> local row id) still waiting in
/// `folder_id`.
///
/// Convenience view over [`move_target_identities`] for callers that do not
/// need the ownership fields.
pub fn confirmed_move_projection_targets(
    conn: &Connection,
    account_id: &str,
    folder_id: &str,
) -> Result<HashMap<String, String>> {
    let mut targets = HashMap::new();
    for identity in move_target_identities(conn, account_id, folder_id)? {
        targets
            .entry(identity.remote_id)
            .or_insert(identity.local_id);
    }
    Ok(targets)
}

/// Ids among `ids` that are hidden for an unconfirmed permanent delete.
pub fn pending_delete_ids(conn: &Connection, ids: &[String]) -> Result<Vec<String>> {
    let mut statement =
        conn.prepare("SELECT id FROM message WHERE id = ?1 AND pending_delete = 1")?;
    let mut pending = Vec::new();
    for id in ids {
        let mut rows = statement.query(params![id])?;
        if rows.next()?.is_some() {
            pending.push(id.clone());
        }
    }
    Ok(pending)
}

/// Ids in a folder that are hidden for an unconfirmed permanent delete.
///
/// Sync uses this to leave the rows alone: their remote deletion has not been
/// acknowledged, so a UIDVALIDITY reset or an expunge pass must not drop them.
pub fn pending_delete_ids_in_folder(conn: &Connection, folder_id: &str) -> Result<Vec<String>> {
    let mut statement =
        conn.prepare("SELECT id FROM message WHERE folder_id = ?1 AND pending_delete = 1")?;
    let mut rows = statement.query(params![folder_id])?;
    let mut ids = Vec::new();
    while let Some(row) = rows.next()? {
        ids.push(row.get(0)?);
    }
    Ok(ids)
}

/// Ids from `ids` that still belong to `account_id`/`folder_id` and are hidden
/// for an unconfirmed permanent delete. Used to confirm a remote delete
/// without touching rows that a concurrent action claimed in the meantime.
pub fn pending_delete_rows(
    conn: &Connection,
    account_id: &str,
    folder_id: &str,
    ids: &[String],
) -> Result<Vec<String>> {
    let mut statement = conn.prepare(
        "SELECT id FROM message
          WHERE id = ?1 AND account_id = ?2 AND folder_id = ?3 AND pending_delete = 1",
    )?;
    let mut matched = Vec::new();
    for id in ids {
        let mut rows = statement.query(params![id, account_id, folder_id])?;
        if rows.next()?.is_some() {
            matched.push(id.clone());
        }
    }
    Ok(matched)
}

/// Hide rows before a permanent delete reaches the server.
///
/// The row keeps its full local state (body cache, FTS document and attachment
/// records); only the listing/visibility flag changes. When the server confirms
/// the command the caller deletes the row for real; when it is refused
/// [`release_pending_delete`] puts the row back.
pub fn hide_for_permanent_delete(conn: &Connection, ids: &[String]) -> Result<usize> {
    let folders = folders_for_ids(conn, ids)?;
    let now = now_string();
    let mut count = 0;
    for id in ids {
        count += conn.execute(
            "UPDATE message
                SET is_deleted_local = 1, pending_delete = 1, updated_at = ?2
              WHERE id = ?1 AND pending_delete = 0",
            params![id, now],
        )?;
    }
    for folder in folders {
        crate::repo::folders::recompute_counts(conn, &folder)?;
    }
    Ok(count)
}

/// Undo [`hide_for_permanent_delete`] without trusting any UID.
///
/// The conservative form used when the caller has no live confirmation: every
/// row drops its old identity and waits for the targeted reconciliation.
pub fn release_pending_delete(
    conn: &Connection,
    account_id: &str,
    folder_id: &str,
    ids: &[String],
) -> Result<Vec<String>> {
    release_pending_delete_with_trust(
        conn,
        account_id,
        folder_id,
        ids,
        &HashSet::new(),
        &[],
        None,
        "",
    )
}

/// Undo [`hide_for_permanent_delete`] after the server refused the command.
///
/// `trusted_ids` are rows whose folder-local identity was confirmed against the
/// server (the live UIDVALIDITY still matches *and* the UID was found in the
/// folder). Those keep their UID/generation and become usable immediately.
///
/// Every other row drops its old identity: a command that timed out may still
/// have executed, so the number can address another message (or none). Such a
/// row is marked `awaiting_reconcile` *and* `target_pending`, which keeps
/// UID-addressed actions away and, crucially, makes `clear_folder` preserve the
/// row across a UIDVALIDITY reset until a targeted, window-independent lookup
/// rebinds it through its Message-ID header.
///
/// `uids` is aligned with `ids` (the UID each row was queued with) and
/// `source_uidvalidity` is the generation the operation was queued in. Both are
/// recorded on an untrusted row so the next sync can ask the server a precise
/// question instead of guessing from an empty search result. `op_id` records
/// which queued operation owns the state; a blank id is stored as `NULL`
/// because "no operation owns this row" is not an owner token.
///
/// Ownership is re-checked in the same `UPDATE`: the row is only released while
/// its *effective* projection owner (the newer of the move-target slot and the
/// reconciliation owner, see [`projection_owner`]) is nobody or still `op_id`.
/// A row a newer trip has claimed must stay hidden — its operation, not this
/// rollback, decides what the server state means. Unclaimed rows are released
/// as before, and a blank `op_id` only ever releases unowned rows.
///
/// Returns the ids that left the pending-delete state.
#[allow(clippy::too_many_arguments)]
pub fn release_pending_delete_with_trust(
    conn: &Connection,
    account_id: &str,
    folder_id: &str,
    ids: &[String],
    trusted_ids: &HashSet<String>,
    uids: &[i64],
    source_uidvalidity: Option<i64>,
    op_id: &str,
) -> Result<Vec<String>> {
    let now = now_string();
    let mut released = Vec::new();
    for (index, id) in ids.iter().enumerate() {
        let trusted = trusted_ids.contains(id);
        let changed = if trusted {
            conn.execute(
                "UPDATE message
                    SET is_deleted_local = 0,
                        pending_delete = 0,
                        target_pending = 0,
                        awaiting_reconcile = 0,
                        reconcile_reason = NULL,
                        reconcile_op_id = NULL,
                        reconcile_seq = NULL,
                        reconcile_uid = NULL,
                        reconcile_uidvalidity = NULL,
                        updated_at = ?4
                  WHERE id = ?1 AND account_id = ?2 AND folder_id = ?3 AND pending_delete = 1
                    AND (
                        (NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0') IS NULL
                         AND NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0') IS NULL)
                        OR CASE
                               WHEN NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0') IS NOT NULL
                                    AND (NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0') IS NULL
                                         OR COALESCE(move_target_seq, 0)
                                            >= COALESCE(reconcile_seq, 0))
                               THEN TRIM(move_target_op_id)
                               ELSE TRIM(reconcile_op_id)
                           END = NULLIF(NULLIF(TRIM(?5), ''), '0')
                    )",
                params![id, account_id, folder_id, now, op_id],
            )?
        } else {
            let uid = uids.get(index).copied();
            conn.execute(
                "UPDATE message
                    SET is_deleted_local = 0,
                        pending_delete = 0,
                        uid = NULL,
                        uidvalidity = NULL,
                        gm_msgid = NULL,
                        target_pending = 1,
                        awaiting_reconcile = 1,
                        reconcile_reason = ?5,
                        -- An empty owner means no trip owns this row yet.
                        -- Store NULL so every owner guard reads it that way
                        -- instead of as an opaque foreign token.
                        reconcile_op_id = NULLIF(NULLIF(TRIM(?6), ''), '0'),
                        reconcile_seq = NULL,
                        reconcile_uid = ?7,
                        reconcile_uidvalidity = ?8,
                        updated_at = ?4
                  WHERE id = ?1 AND account_id = ?2 AND folder_id = ?3 AND pending_delete = 1
                    AND (
                        (NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0') IS NULL
                         AND NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0') IS NULL)
                        OR CASE
                               WHEN NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0') IS NOT NULL
                                    AND (NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0') IS NULL
                                         OR COALESCE(move_target_seq, 0)
                                            >= COALESCE(reconcile_seq, 0))
                               THEN TRIM(move_target_op_id)
                               ELSE TRIM(reconcile_op_id)
                           END = NULLIF(NULLIF(TRIM(?6), ''), '0')
                    )",
                params![
                    id,
                    account_id,
                    folder_id,
                    now,
                    RECONCILE_PENDING_DELETE,
                    op_id,
                    uid,
                    source_uidvalidity
                ],
            )?
        };
        if changed > 0 {
            released.push(id.clone());
        }
    }
    if !released.is_empty() {
        crate::repo::folders::recompute_counts(conn, folder_id)?;
    }
    Ok(released)
}

/// Undo a refused permanent delete for a provider whose message id does not
/// change and whose folder membership can be verified with a targeted request
/// (Microsoft Graph).
///
/// Unlike the IMAP rollback the row keeps its full identity: the Graph id and
/// the deterministic synthetic UID are still the same object on the server, so
/// dropping them would leave the row unusable until an (often empty) delta
/// arrives. The caller verifies the folder with a GET before trusting it.
///
/// `owner_op_id` is the operation asking to release the row. The release is
/// conditional on the row's *effective* projection owner being nobody or still
/// that operation: a newer trip that claimed the row keeps it hidden and keeps
/// its markers, and a blank owner only releases unowned rows. Returns the ids
/// that left the pending-delete state.
pub fn release_pending_delete_preserving_identity(
    conn: &Connection,
    account_id: &str,
    folder_id: &str,
    ids: &[String],
    owner_op_id: &str,
) -> Result<Vec<String>> {
    let now = now_string();
    let mut released = Vec::new();
    for id in ids {
        let changed = conn.execute(
            "UPDATE message
                SET is_deleted_local = 0,
                    pending_delete = 0,
                    target_pending = 0,
                    awaiting_reconcile = 0,
                    reconcile_reason = NULL,
                    reconcile_op_id = NULL,
                    reconcile_seq = NULL,
                    reconcile_uid = NULL,
                    reconcile_uidvalidity = NULL,
                    updated_at = ?4
              WHERE id = ?1 AND account_id = ?2 AND folder_id = ?3 AND pending_delete = 1
                AND (
                    (NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0') IS NULL
                     AND NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0') IS NULL)
                    OR CASE
                           WHEN NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0') IS NOT NULL
                                AND (NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0') IS NULL
                                     OR COALESCE(move_target_seq, 0)
                                        >= COALESCE(reconcile_seq, 0))
                           THEN TRIM(move_target_op_id)
                           ELSE TRIM(reconcile_op_id)
                       END = NULLIF(NULLIF(TRIM(?5), ''), '0')
                )",
            params![id, account_id, folder_id, now, owner_op_id],
        )?;
        if changed > 0 {
            released.push(id.clone());
        }
    }
    if !released.is_empty() {
        crate::repo::folders::recompute_counts(conn, folder_id)?;
    }
    Ok(released)
}

/// Identity fields of a row that still has to be reconciled with the server.
#[derive(Debug, Clone)]
pub struct PendingIdentity {
    pub id: String,
    pub uid: Option<i64>,
    pub uidvalidity: Option<i64>,
    pub message_id_header: Option<String>,
    pub target_pending: bool,
    pub awaiting_reconcile: bool,
    pub reconcile_reason: Option<String>,
    pub reconcile_op_id: Option<String>,
    pub reconcile_uid: Option<i64>,
    pub reconcile_uidvalidity: Option<i64>,
}

impl PendingIdentity {
    /// The reason to use when an older row has no explicit reason. Those rows
    /// were written by the previous build, where the flag meant either a
    /// rolled-back permanent delete or a move rollback; the safe reading is
    /// "unknown, rebind" (never "the server deleted it").
    pub fn reason(&self) -> &str {
        self.reconcile_reason.as_deref().unwrap_or(RECONCILE_REBIND)
    }
}

/// Rows of `folder_id` that a failed operation left waiting for a server
/// reconciliation: optimistic move projections (`target_pending`) and rows
/// whose permanent-delete rollback did not trust their old identity
/// (`awaiting_reconcile`). Rows still hidden for an unconfirmed delete are
/// owned by their queued operation and are not included.
pub fn pending_identity_rows(conn: &Connection, folder_id: &str) -> Result<Vec<PendingIdentity>> {
    let mut statement = conn.prepare(
        "SELECT id, uid, uidvalidity, message_id_header, target_pending, awaiting_reconcile,
                reconcile_reason,
                NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0'),
                reconcile_uid, reconcile_uidvalidity
           FROM message
          WHERE folder_id = ?1 AND pending_delete = 0
            AND (target_pending = 1 OR awaiting_reconcile = 1)",
    )?;
    let mut rows = statement.query(params![folder_id])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        result.push(PendingIdentity {
            id: row.get(0)?,
            uid: row.get(1)?,
            uidvalidity: row.get(2)?,
            message_id_header: row.get(3)?,
            target_pending: row.get::<_, i64>(4)? != 0,
            awaiting_reconcile: row.get::<_, i64>(5)? != 0,
            reconcile_reason: row.get(6)?,
            reconcile_op_id: row.get(7)?,
            reconcile_uid: row.get(8)?,
            reconcile_uidvalidity: row.get(9)?,
        });
    }
    Ok(result)
}

/// The server confirmed that the row is addressable again (its retained UID is
/// still present, or its Message-ID was found in the folder). The row stops
/// blocking UID-addressed actions.
pub fn confirm_pending_identity(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE message
            SET target_pending = 0,
                move_target_remote_id = NULL,
                move_target_folder_id = NULL,
                move_target_op_id = NULL,
                move_target_seq = NULL,
                awaiting_reconcile = 0,
                reconcile_reason = NULL,
                reconcile_op_id = NULL,
                reconcile_seq = NULL,
                reconcile_uid = NULL,
                reconcile_uidvalidity = NULL,
                updated_at = ?2
          WHERE id = ?1",
        params![id, now_string()],
    )?;
    Ok(())
}

/// Keep a row waiting for a targeted reconciliation without holding a UID.
///
/// The reason and the owning operation are recorded separately, so a later
/// sync never has to infer "the server deleted this" from a generic flag.
/// `uid`/`uidvalidity` are the identity the operation was queued with (when
/// known); they are kept only for the precise `UID SEARCH` check and never
/// become addressable again unless that check succeeds.
///
/// Every caller must name the operation that owns the write
/// ([`mark_reconcile_owned`]): there is deliberately no unguarded variant, so a
/// failure recovery can never clear a newer trip's projection. The row is only
/// touched while its *effective* projection owner — the move target slot or the
/// reconciliation owner, whichever is the newer trip — is unowned, still
/// `owner_op_id`, or strictly older than this operation. A row already owned by
/// a different (newer) operation must survive: the old operation's failure says
/// nothing about the newer projection's destination, and clearing it would make
/// a stale provider removal look authoritative again.
pub fn mark_reconcile_owned(
    conn: &Connection,
    ids: &[String],
    reason: &str,
    owner_op_id: &str,
    owner_sequence: i64,
    uid: Option<i64>,
    uidvalidity: Option<i64>,
) -> Result<()> {
    mark_reconcile_inner(
        conn,
        ids,
        reason,
        owner_op_id,
        owner_sequence,
        uid,
        uidvalidity,
    )
}

#[allow(clippy::too_many_arguments)]
fn mark_reconcile_inner(
    conn: &Connection,
    ids: &[String],
    reason: &str,
    owner_op_id: &str,
    owner_sequence: i64,
    uid: Option<i64>,
    uidvalidity: Option<i64>,
) -> Result<()> {
    let now = now_string();
    for id in ids {
        conn.execute(
            "UPDATE message
                SET uid = NULL,
                    uidvalidity = NULL,
                    gm_msgid = NULL,
                    target_pending = 1,
                    move_target_remote_id = NULL,
                    move_target_folder_id = NULL,
                    move_target_op_id = NULL,
                    move_target_seq = NULL,
                    awaiting_reconcile = 1,
                    reconcile_reason = ?2,
                    reconcile_op_id = NULLIF(NULLIF(TRIM(?3), ''), '0'),
                    reconcile_seq = ?4,
                    reconcile_uid = ?5,
                    reconcile_uidvalidity = ?6,
                    updated_at = ?7
              WHERE id = ?1
                AND (
                    (NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0') IS NULL
                     AND NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0') IS NULL)
                    OR CASE
                           WHEN NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0') IS NOT NULL
                                AND (NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0') IS NULL
                                     OR COALESCE(move_target_seq, 0)
                                        >= COALESCE(reconcile_seq, 0))
                           THEN TRIM(move_target_op_id)
                           ELSE TRIM(reconcile_op_id)
                       END = NULLIF(NULLIF(TRIM(?8), ''), '0')
                    OR COALESCE(
                           CASE
                               WHEN NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0') IS NOT NULL
                                    AND (NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0') IS NULL
                                         OR COALESCE(move_target_seq, 0)
                                            >= COALESCE(reconcile_seq, 0))
                               THEN move_target_seq
                               ELSE reconcile_seq
                           END,
                           0) < COALESCE(?9, 0)
                )",
            params![
                id,
                reason,
                owner_op_id,
                owner_sequence,
                uid,
                uidvalidity,
                now,
                owner_op_id,
                owner_sequence
            ],
        )?;
    }
    Ok(())
}

/// Drop a row's folder-local identity and wait for the next sync to bind a new
/// one (used when a permanent-delete rollback rebuilds the row from its
/// snapshot).
pub fn clear_uid_identity(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE message
            SET uid = NULL,
                uidvalidity = NULL,
                gm_msgid = NULL,
                target_pending = 1,
                move_target_remote_id = NULL,
                move_target_folder_id = NULL,
                move_target_op_id = NULL,
                move_target_seq = NULL,
                updated_at = ?2
          WHERE id = ?1",
        params![id, now_string()],
    )?;
    Ok(())
}

/// Bind an APPEND's server-assigned UID to the draft projection that owns it.
///
/// The update is conditional on the row still being the same draft projection
/// in the folder the APPEND targeted. A concurrent move (or delete) must not
/// receive a UID that belongs to a copy in another mailbox: reusing that
/// number later would address whatever message happens to hold it there.
///
/// The receipt is also checked against what the projection already knows, so a
/// retry that started in an older generation cannot undo work a sync already
/// did:
///
/// * a UID is only written when its UIDVALIDITY still matches the folder's
///   current generation and it is not older than an existing binding in that
///   same generation;
/// * a UID-less receipt (server without UIDPLUS) only clears the identity of
///   the copy it superseded (`superseded` = the queued old UID/generation),
///   never a UID a sync has already bound.
///
/// Returns the number of rows changed. `0` means the receipt was skipped (the
/// projection moved, a newer binding won, or there was nothing stale to clear);
/// the caller must leave the server copy to a targeted sync. `uid == None`
/// clears a stale identity (the server accepted the APPEND but sent no UIDPLUS
/// response code).
pub fn bind_appended_draft(
    conn: &Connection,
    id: &str,
    account_id: &str,
    folder_id: &str,
    uid: Option<i64>,
    uidvalidity: Option<i64>,
    superseded: Option<(i64, Option<i64>)>,
) -> Result<usize> {
    let Some(stored) = get(conn, id)? else {
        return Ok(0);
    };
    if stored.summary.account_id != account_id
        || stored.summary.folder_id != folder_id
        || !stored.summary.is_draft
        || stored.is_deleted_local
    {
        return Ok(0);
    }
    let pending_delete: bool = conn.query_row(
        "SELECT pending_delete != 0 FROM message WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )?;
    if pending_delete {
        return Ok(0);
    }
    let current_generation = crate::repo::folders::uidvalidity(conn, folder_id)?;
    let now = now_string();
    let changed = match uid {
        Some(uid) => {
            // A receipt from a generation the folder no longer has addresses a
            // different message now (or nothing at all).
            if let (Some(current), Some(receipt)) = (current_generation, uidvalidity) {
                if current != receipt {
                    return Ok(0);
                }
            }
            if let Some(existing) = stored.uid {
                if existing == uid {
                    // Already bound to this copy; only fill in a missing
                    // generation, otherwise there is nothing to do.
                    if stored.uidvalidity == uidvalidity {
                        return Ok(0);
                    }
                } else {
                    // A different identity already exists for this row. It may
                    // only be replaced when it is the copy this APPEND
                    // superseded, or when it belongs to a demonstrably older
                    // generation. An equal-generation binding (a sync won the
                    // race, or the draft was saved again) is left alone.
                    let points_at_superseded = superseded.is_some_and(|(old_uid, old_gen)| {
                        existing == old_uid
                            && old_gen
                                .map(|generation| stored.uidvalidity == Some(generation))
                                .unwrap_or(true)
                    });
                    // The row's own binding is only replaceable when it belongs
                    // to a generation the folder has already left behind. With
                    // no current generation to compare against, a known row
                    // binding is treated as authoritative.
                    let row_generation_is_stale = match (stored.uidvalidity, current_generation) {
                        (Some(old), Some(current)) => old != current,
                        (None, _) => true,
                        (Some(_), None) => false,
                    };
                    if !points_at_superseded && !row_generation_is_stale {
                        return Ok(0);
                    }
                }
            }
            conn.execute(
                "UPDATE message
                    SET uid = ?4,
                        uidvalidity = ?5,
                        gm_msgid = NULL,
                        target_pending = 0,
                        awaiting_reconcile = 0,
                        reconcile_reason = NULL,
                        reconcile_op_id = NULL,
                        reconcile_seq = NULL,
                        reconcile_uid = NULL,
                        reconcile_uidvalidity = NULL,
                        updated_at = ?6
                  WHERE id = ?1 AND account_id = ?2 AND folder_id = ?3
                    AND is_draft = 1 AND is_deleted_local = 0 AND pending_delete = 0",
                params![id, account_id, folder_id, uid, uidvalidity, now],
            )?
        }
        None => {
            // Without UIDPLUS the only safe cleanup is removing the identity of
            // the copy this APPEND replaced. If the row already carries another
            // UID (a sync bound the new copy), keep it.
            let Some((old_uid, old_generation)) = superseded else {
                return Ok(0);
            };
            if stored.uid != Some(old_uid) {
                return Ok(0);
            }
            if let Some(generation) = old_generation {
                if stored.uidvalidity != Some(generation) {
                    return Ok(0);
                }
            }
            conn.execute(
                "UPDATE message
                    SET uid = NULL,
                        uidvalidity = NULL,
                        gm_msgid = NULL,
                        target_pending = 1,
                        updated_at = ?4
                  WHERE id = ?1 AND account_id = ?2 AND folder_id = ?3
                    AND is_draft = 1 AND is_deleted_local = 0 AND pending_delete = 0",
                params![id, account_id, folder_id, now],
            )?
        }
    };
    Ok(changed)
}

/// Bind a freshly created Graph draft to its local projection.
///
/// Graph has no `APPENDUID`; the create response names the message directly.
/// That id is the equivalent receipt, so the synthetic UID it implies and the
/// `gm_msgid` are written together, replacing whichever copy the row carried
/// before (a save replaces the previous server copy in place). The guards keep
/// a concurrent discard/rebind from being overwritten: only the same draft
/// projection in the same account/folder, still visible, is updated.
pub fn bind_graph_appended_draft(
    conn: &Connection,
    id: &str,
    account_id: &str,
    folder_id: &str,
    uid: i64,
    remote_id: &str,
) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE message
            SET uid = ?4,
                uidvalidity = NULL,
                gm_msgid = ?5,
                target_pending = 0,
                awaiting_reconcile = 0,
                reconcile_reason = NULL,
                reconcile_op_id = NULL,
                reconcile_seq = NULL,
                reconcile_uid = NULL,
                reconcile_uidvalidity = NULL,
                updated_at = ?6
          WHERE id = ?1 AND account_id = ?2 AND folder_id = ?3
            AND is_draft = 1 AND is_deleted_local = 0 AND pending_delete = 0",
        params![id, account_id, folder_id, uid, remote_id, now_string()],
    )?)
}

/// Restore the folder-local identity of a row, e.g. after a rolled-back move
/// put it back into the folder it came from. A non-NULL UID means the server
/// confirmed the identity, so every reconciliation marker is cleared in the
/// same statement.
pub fn set_uid_identity(
    conn: &Connection,
    id: &str,
    uid: Option<i64>,
    uidvalidity: Option<i64>,
) -> Result<()> {
    conn.execute(
        "UPDATE message SET uid = ?2, uidvalidity = ?3, updated_at = ?4,
                target_pending = CASE WHEN ?2 IS NULL THEN target_pending ELSE 0 END,
                move_target_remote_id = CASE WHEN ?2 IS NULL THEN move_target_remote_id ELSE NULL END,
                move_target_folder_id = CASE WHEN ?2 IS NULL THEN move_target_folder_id ELSE NULL END,
                move_target_op_id = CASE WHEN ?2 IS NULL THEN move_target_op_id ELSE NULL END,
                move_target_seq = CASE WHEN ?2 IS NULL THEN move_target_seq ELSE NULL END,
                awaiting_reconcile = CASE WHEN ?2 IS NULL THEN awaiting_reconcile ELSE 0 END,
                reconcile_reason = CASE WHEN ?2 IS NULL THEN reconcile_reason ELSE NULL END,
                reconcile_op_id = CASE WHEN ?2 IS NULL THEN reconcile_op_id ELSE NULL END,
                reconcile_seq = CASE WHEN ?2 IS NULL THEN reconcile_seq ELSE NULL END,
                reconcile_uid = CASE WHEN ?2 IS NULL THEN reconcile_uid ELSE NULL END,
                reconcile_uidvalidity = CASE WHEN ?2 IS NULL THEN reconcile_uidvalidity ELSE NULL END
          WHERE id = ?1",
        params![id, uid, uidvalidity, now_string()],
    )?;
    Ok(())
}

/// Restore the full snapshot identity of a row whose command provably never
/// ran.
///
/// Unlike [`set_uid_identity`] this also restores the provider id. A Graph
/// undo that only put back the synthetic UID would leave `gm_msgid` NULL, and
/// the next permanent delete would have no pinned provider identity to send
/// (and must refuse rather than re-resolve the mutable folder mapping).
pub fn set_uid_and_graph_identity(
    conn: &Connection,
    id: &str,
    uid: Option<i64>,
    uidvalidity: Option<i64>,
    gm_msgid: Option<&str>,
) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE message
            SET uid = ?2,
                uidvalidity = ?3,
                gm_msgid = COALESCE(?4, gm_msgid),
                updated_at = ?5,
                target_pending = CASE WHEN ?2 IS NULL THEN target_pending ELSE 0 END,
                move_target_remote_id = CASE WHEN ?2 IS NULL THEN move_target_remote_id ELSE NULL END,
                move_target_folder_id = CASE WHEN ?2 IS NULL THEN move_target_folder_id ELSE NULL END,
                move_target_op_id = CASE WHEN ?2 IS NULL THEN move_target_op_id ELSE NULL END,
                move_target_seq = CASE WHEN ?2 IS NULL THEN move_target_seq ELSE NULL END,
                awaiting_reconcile = CASE WHEN ?2 IS NULL THEN awaiting_reconcile ELSE 0 END,
                reconcile_reason = CASE WHEN ?2 IS NULL THEN reconcile_reason ELSE NULL END,
                reconcile_op_id = CASE WHEN ?2 IS NULL THEN reconcile_op_id ELSE NULL END,
                reconcile_seq = CASE WHEN ?2 IS NULL THEN reconcile_seq ELSE NULL END,
                reconcile_uid = CASE WHEN ?2 IS NULL THEN reconcile_uid ELSE NULL END,
                reconcile_uidvalidity = CASE WHEN ?2 IS NULL THEN reconcile_uidvalidity ELSE NULL END
          WHERE id = ?1",
        params![id, uid, uidvalidity, gm_msgid, now_string()],
    )?)
}

/// Restore a Graph row's full identity after a targeted GET confirmed the
/// message and its folder. Graph ids are stable across a refused command and
/// the synthetic UID is derived from the same immutable id, so both can be
/// trusted again together.
pub fn set_graph_identity(conn: &Connection, id: &str, uid: i64, gm_msgid: &str) -> Result<()> {
    conn.execute(
        "UPDATE message SET uid = ?2,
                uidvalidity = NULL,
                gm_msgid = ?3,
                target_pending = 0,
                move_target_remote_id = NULL,
                move_target_folder_id = NULL,
                move_target_op_id = NULL,
                move_target_seq = NULL,
                awaiting_reconcile = 0,
                reconcile_reason = NULL,
                reconcile_op_id = NULL,
                reconcile_seq = NULL,
                reconcile_uid = NULL,
                reconcile_uidvalidity = NULL,
                updated_at = ?4
          WHERE id = ?1",
        params![id, uid, gm_msgid, now_string()],
    )?;
    Ok(())
}

/// Bind a Graph identity only while the row is still the unbound projection
/// this operation owns.
///
/// Recovery reads a folder from a targeted GET, then writes the identity. A
/// sync can bind a newer identity (or another operation can claim the row)
/// between those two steps; an unconditional `UPDATE` would overwrite that
/// newer binding with evidence about the old copy. The caller compares the
/// returned row count: `0` means the row moved on and must be left to the
/// newer owner.
pub fn set_graph_identity_if_pending(
    conn: &Connection,
    id: &str,
    account_id: &str,
    folder_id: &str,
    uid: i64,
    gm_msgid: &str,
    op_id: &str,
) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE message SET uid = ?2,
                uidvalidity = NULL,
                gm_msgid = ?3,
                target_pending = 0,
                move_target_remote_id = NULL,
                move_target_folder_id = NULL,
                move_target_op_id = NULL,
                move_target_seq = NULL,
                awaiting_reconcile = 0,
                reconcile_reason = NULL,
                reconcile_op_id = NULL,
                reconcile_seq = NULL,
                reconcile_uid = NULL,
                reconcile_uidvalidity = NULL,
                updated_at = ?4
          WHERE id = ?1
            AND account_id = ?5
            AND folder_id = ?6
            AND uid IS NULL
            AND gm_msgid IS NULL
            AND target_pending = 1
            AND (NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0') IS NULL
                 OR TRIM(reconcile_op_id) = ?7)
            AND (NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0') IS NULL
                 OR TRIM(move_target_op_id) = ?7)",
        params![
            id,
            uid,
            gm_msgid,
            now_string(),
            account_id,
            folder_id,
            op_id
        ],
    )?)
}

/// Flag fields whose last writer is tracked for undo and rollback.
pub const FLAG_FIELDS: [&str; 4] = ["seen", "flagged", "answered", "forwarded"];

const FLAG_WRITER_COLUMNS: &str = "message_id, field, owner_id, value, seq, action";

/// One `(message, field)` last-writer record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagWriter {
    pub message_id: String,
    pub field: String,
    pub owner_id: String,
    pub value: Option<bool>,
    pub seq: i64,
    pub action: String,
}

/// Allocate the next flag-writer revision.
///
/// The counter lives in its own table, independent of the rows that carry the
/// current revision. `MAX(seq) + 1` would hand the same value out again after
/// the newest writer row was deleted (undoing an action with no predecessor
/// removes it), and a reader that only compares `seq` would then mistake a
/// later write for the one it observed.
fn next_flag_writer_seq(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "INSERT INTO flag_writer_seq (id, value) VALUES (1, 1)
         ON CONFLICT(id) DO UPDATE SET value = value + 1
         RETURNING value",
        [],
        |row| row.get(0),
    )?)
}

/// Record that `owner_id` wrote one flag field of one message.
///
/// The row is upserted: the last write owns the field. `seq` is monotonic
/// across the table, so a caller can order two writes even when their operation
/// sequences were assigned in different transactions.
///
/// The message-level [`flag_revision`] is stamped with the same value. Unlike
/// this row it survives the writer being deleted, so a reader that observed
/// "no writer" still sees that the field changed afterwards.
pub fn record_flag_writer(
    conn: &Connection,
    message_id: &str,
    field: &str,
    owner_id: &str,
    value: Option<bool>,
    action: &str,
) -> Result<()> {
    let seq = next_flag_writer_seq(conn)?;
    conn.execute(
        "INSERT INTO message_flag_writer
             (message_id, field, owner_id, value, seq, action, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(message_id, field) DO UPDATE SET
             owner_id = excluded.owner_id,
             value = excluded.value,
             seq = excluded.seq,
             action = excluded.action,
             created_at = excluded.created_at",
        params![
            message_id,
            field,
            owner_id,
            value.map(|value| value as i64),
            seq,
            action,
            now_string()
        ],
    )?;
    set_flag_revision(conn, message_id, seq)?;
    Ok(())
}

/// Newest revision of any flag-field write on this message.
///
/// `message_flag_writer` only carries the *current* writer. Undoing an action
/// with no predecessor deletes that row, so a reader that observed "no writer"
/// would see `None` again after the undo restored the value and overwrote it.
/// The revision lives on the message itself and advances on every writer
/// mutation, including the delete, so "unchanged" and "changed back to no
/// writer" stay distinguishable.
pub fn flag_revision(conn: &Connection, message_id: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT flag_revision FROM message WHERE id = ?1",
            params![message_id],
            |row| row.get(0),
        )
        .optional()?)
}

fn set_flag_revision(conn: &Connection, message_id: &str, revision: i64) -> Result<()> {
    conn.execute(
        "UPDATE message SET flag_revision = ?2 WHERE id = ?1",
        params![message_id, revision],
    )?;
    Ok(())
}

/// Advance the message-level flag revision without touching the writer row.
///
/// Used when a local write changes a flag value while ownership must stay with
/// the existing writer: undoing a local/POP3 flag action has no queued
/// operation to reassign, so nothing else would mark the change. Readers that
/// snapshotted the previous revision — the automatic mark-as-read during a body
/// load — must still see that the value changed and not overwrite it.
pub fn bump_flag_revision(conn: &Connection, message_id: &str) -> Result<i64> {
    let seq = next_flag_writer_seq(conn)?;
    set_flag_revision(conn, message_id, seq)?;
    Ok(seq)
}

/// Record every flag field a write actually touched (`Some`).
#[allow(clippy::too_many_arguments)]
pub fn record_flag_writers(
    conn: &Connection,
    message_id: &str,
    owner_id: &str,
    action: &str,
    seen: Option<bool>,
    flagged: Option<bool>,
    answered: Option<bool>,
    forwarded: Option<bool>,
) -> Result<()> {
    for (field, value) in [
        ("seen", seen),
        ("flagged", flagged),
        ("answered", answered),
        ("forwarded", forwarded),
    ] {
        if value.is_some() {
            record_flag_writer(conn, message_id, field, owner_id, value, action)?;
        }
    }
    Ok(())
}

/// The newest writer of one `(message, field)`, if one was recorded.
pub fn flag_writer(conn: &Connection, message_id: &str, field: &str) -> Result<Option<FlagWriter>> {
    let sql = format!(
        "SELECT {FLAG_WRITER_COLUMNS} FROM message_flag_writer
          WHERE message_id = ?1 AND field = ?2"
    );
    crate::repo::query_one(conn, &sql, params![message_id, field], |row| {
        Ok(FlagWriter {
            message_id: row.get(0)?,
            field: row.get(1)?,
            owner_id: row.get(2)?,
            value: row.get::<_, Option<i64>>(3)?.map(|value| value != 0),
            seq: row.get(4)?,
            action: row.get(5)?,
        })
    })
}

/// Hand a cancelled/never-sent compensation's field ownership back to the undo
/// entry that will re-plan it.
///
/// The compensation's local restore already ran, so its value stays on the
/// row; only the owner changes. Without this, the re-planned entry would see
/// its own abandoned operation as a "later writer" and finish as if the field
/// had been taken over.
pub fn reassign_flag_writers(conn: &Connection, from_owner: &str, to_owner: &str) -> Result<usize> {
    // A hand-off changes who owns the field, so it must also change the
    // revision readers use to notice concurrent changes (auto mark-as-read).
    // Stamp every touched message, not just the rows: the per-field seq alone
    // disappears with a deleted writer row.
    let messages: Vec<String> = {
        let mut statement = conn
            .prepare("SELECT DISTINCT message_id FROM message_flag_writer WHERE owner_id = ?1")?;
        let rows = statement.query_map(params![from_owner], |row| row.get(0))?;
        rows.collect::<std::result::Result<Vec<String>, _>>()?
    };
    let seq = next_flag_writer_seq(conn)?;
    let changed = conn.execute(
        "UPDATE message_flag_writer SET owner_id = ?2, seq = ?3 WHERE owner_id = ?1",
        params![from_owner, to_owner, seq],
    )?;
    for message_id in messages {
        set_flag_revision(conn, &message_id, seq)?;
    }
    Ok(changed)
}

/// Drop the writer rows of deleted messages so a reused local id starts clean.
pub fn delete_flag_writers(conn: &Connection, ids: &[String]) -> Result<()> {
    for id in ids {
        conn.execute(
            "DELETE FROM message_flag_writer WHERE message_id = ?1",
            params![id],
        )?;
    }
    Ok(())
}

pub fn set_uid(conn: &Connection, id: &str, uid: Option<i64>) -> Result<()> {
    conn.execute(
        "UPDATE message SET uid = ?2, updated_at = ?3,
                target_pending = CASE WHEN ?2 IS NULL THEN target_pending ELSE 0 END,
                awaiting_reconcile = CASE WHEN ?2 IS NULL THEN awaiting_reconcile ELSE 0 END,
                reconcile_reason = CASE WHEN ?2 IS NULL THEN reconcile_reason ELSE NULL END,
                reconcile_op_id = CASE WHEN ?2 IS NULL THEN reconcile_op_id ELSE NULL END,
                reconcile_seq = CASE WHEN ?2 IS NULL THEN reconcile_seq ELSE NULL END,
                reconcile_uid = CASE WHEN ?2 IS NULL THEN reconcile_uid ELSE NULL END,
                reconcile_uidvalidity = CASE WHEN ?2 IS NULL THEN reconcile_uidvalidity ELSE NULL END
          WHERE id = ?1",
        params![id, uid, now_string()],
    )?;
    Ok(())
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

/// Move rows to another folder without touching their folder-local identity.
///
/// Only accounts that never queue a server operation (Local/POP3), rollback
/// paths and folder migrations may use this: a server move changes the
/// message's UID, so the optimistic path uses [`move_to_folder_pending`]
/// instead.
pub fn move_to_folder(conn: &Connection, ids: &[String], folder_id: &str) -> Result<usize> {
    move_rows(conn, ids, folder_id, false)
}

/// Optimistic move for a row whose server-side MOVE has just been queued.
///
/// UIDs are folder-local: the source UID means nothing (and can alias another
/// message) in the target folder. Clear it and mark the row with
/// `target_pending` so no UID-keyed action can hit the wrong message while the
/// move operation is in flight; the next sync of the target folder
/// re-associates the row through its Message-ID header.
pub fn move_to_folder_pending(conn: &Connection, ids: &[String], folder_id: &str) -> Result<usize> {
    move_rows(conn, ids, folder_id, true)
}

/// Undo's optimistic move back into `folder_id`, conditional on the row still
/// being owned by one of `allowed_owner_ids` (the operations the undone action
/// queued), by nobody at all, or by a trip older than `takeover_sequence`.
///
/// `takeover_sequence` is the queue sequence of the compensation inverse that
/// is being queued in the same transaction; it lets a legitimate newer undo
/// supersede the previous owner's claim, exactly like
/// [`claim_move_target_owner`]. A non-compensating restore passes `None` and
/// only touches a row it still owns.
///
/// Folder membership alone is not ownership: a newer move can claim the row
/// while the row is in the same target folder, and pulling it back would revert
/// the newer trip and clear its owner. Must run inside the caller's write
/// transaction so the ownership read and the move are one atomic step.
pub fn move_to_folder_pending_if_owner(
    conn: &Connection,
    ids: &[String],
    folder_id: &str,
    allowed_owner_ids: &[String],
    takeover_sequence: Option<i64>,
) -> Result<usize> {
    let mut movable = Vec::new();
    for id in ids {
        match projection_owner(conn, id)? {
            Some(owner)
                if !allowed_owner_ids.iter().any(|allowed| allowed == &owner.op_id)
                    && !takeover_sequence
                        .is_some_and(|sequence| owner.sequence.unwrap_or(0) < sequence) =>
            {
                continue;
            }
            _ => movable.push(id.clone()),
        }
    }
    if movable.is_empty() {
        return Ok(0);
    }
    move_rows(conn, &movable, folder_id, true)
}

/// Roll an optimistic move back into its source folder.
///
/// A sync of the target folder may already have bound the *target* UID to the
/// row. Moving that row back with [`move_to_folder`] would carry the target
/// identity along: it can collide with `idx_message_unique_uid` in the source
/// folder (so the restore never reaches the UID fix-up), or — when the number
/// is free — leave a target UID behind that addresses a different message.
///
/// The target identity is therefore cleared in the *same* `UPDATE` that
/// changes `folder_id`, never in a separate statement that could hit the
/// unique index first. The row only moves when it is still exactly the
/// projection this operation created: same id, account and current
/// `folder_id == expected_folder_id`. `target_pending = 1` records that the
/// row is waiting for the caller to restore the source generation (or for the
/// next sync to re-associate it).
///
/// `owner_op_id` is the operation attempting the rollback. The row is only
/// moved while its move-target identity slot is unowned or still owned by that
/// operation; a newer move trip's projection must never be pulled back by an
/// older batch's failure.
///
/// Returns the ids that were restored. Rows that no longer matched are left
/// untouched; the caller must reconcile them from the server instead.
pub fn move_to_folder_rollback(
    conn: &Connection,
    ids: &[String],
    folder_id: &str,
    expected_folder_id: &str,
    account_id: &str,
    owner_op_id: &str,
) -> Result<Vec<String>> {
    let target_is_drafts = conn
        .query_row(
            "SELECT role = ?2 FROM folder WHERE id = ?1",
            params![folder_id, FolderRole::Drafts.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some_and(|value| value != 0);
    let now = now_string();
    let mut restored = Vec::new();
    let mut affected_folders: Vec<String> = Vec::new();
    for id in ids {
        let changed = conn.execute(
            "UPDATE message SET
                 folder_id = ?2,
                 uid = NULL,
                 uidvalidity = NULL,
                 gm_msgid = NULL,
                 target_pending = 1,
                 move_target_remote_id = NULL,
                 move_target_folder_id = NULL,
                 move_target_op_id = NULL,
                 move_target_seq = NULL,
                 awaiting_reconcile = 1,
                 reconcile_reason = NULL,
                 reconcile_op_id = NULL,
                 reconcile_seq = NULL,
                 reconcile_uid = NULL,
                 reconcile_uidvalidity = NULL,
                 is_draft = CASE
                     WHEN is_draft = 1 AND ?3 = 0
                          AND folder_id IN (SELECT id FROM folder WHERE role = ?4)
                     THEN 0 ELSE is_draft END,
                 updated_at = ?5
             WHERE id = ?1 AND account_id = ?6 AND folder_id = ?7
               AND target_pending = 1
               AND (NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0') IS NULL
                    OR TRIM(move_target_op_id) = ?8)
               AND (NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0') IS NULL
                    OR TRIM(reconcile_op_id) = ?8)",
            params![
                id,
                folder_id,
                target_is_drafts as i64,
                FolderRole::Drafts.as_str(),
                now,
                account_id,
                expected_folder_id,
                owner_op_id
            ],
        )?;
        if changed > 0 {
            restored.push(id.clone());
            if !affected_folders
                .iter()
                .any(|folder| folder == expected_folder_id)
            {
                affected_folders.push(expected_folder_id.to_string());
            }
        }
    }
    if !restored.is_empty() {
        affected_folders.push(folder_id.to_string());
        for folder in affected_folders {
            crate::repo::folders::recompute_counts(conn, &folder)?;
        }
    }
    Ok(restored)
}

fn move_rows(conn: &Connection, ids: &[String], folder_id: &str, pending: bool) -> Result<usize> {
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
                 uid = CASE WHEN ?4 = 1 AND folder_id <> ?2 THEN NULL ELSE uid END,
                 uidvalidity = CASE WHEN ?4 = 1 AND folder_id <> ?2 THEN NULL ELSE uidvalidity END,
                 gm_msgid = CASE WHEN ?4 = 1 AND folder_id <> ?2 THEN NULL ELSE gm_msgid END,
                 target_pending = CASE WHEN ?4 = 1 AND folder_id <> ?2 THEN 1 ELSE target_pending END,
                 -- Any folder change invalidates the destination identity a
                 -- previous confirmation recorded on this row.
                 move_target_remote_id = CASE WHEN folder_id <> ?2 THEN NULL ELSE move_target_remote_id END,
                 move_target_folder_id = CASE WHEN folder_id <> ?2 THEN NULL ELSE move_target_folder_id END,
                 move_target_op_id = CASE WHEN folder_id <> ?2 THEN NULL ELSE move_target_op_id END,
                 move_target_seq = CASE WHEN folder_id <> ?2 THEN NULL ELSE move_target_seq END,
                 is_draft = CASE
                     WHEN is_draft = 1 AND ?5 = 0
                          AND folder_id IN (SELECT id FROM folder WHERE role = ?6)
                     THEN 0 ELSE is_draft END,
                 updated_at = ?3
             WHERE id = ?1",
            params![
                id,
                folder_id,
                now_string(),
                pending as i64,
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
            conn.execute(
                "DELETE FROM message_flag_writer WHERE message_id = ?1",
                params![id],
            )?;
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
        "DELETE FROM message_flag_writer
          WHERE message_id IN (SELECT id FROM message WHERE folder_id = ?1)",
        params![folder_id],
    )?;
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

/// `(message_id, uid, uidvalidity, owned)` for one row of a folder-wide
/// mark-read.
pub type FolderReadTarget = (String, Option<i64>, Option<i64>, bool);

/// Per-message targets of a folder-wide mark-read, with ownership.
///
/// Returns `(message_id, uid, uidvalidity, owned)` for every row in the
/// folder. A row is owned by `owner_id` when it has no `seen` writer or that
/// writer is this operation. A newer single-message action records its own
/// writer, so the folder operation must neither send `+FLAGS \Seen` for that
/// UID nor settle the row locally: that would overwrite the user's newer
/// intent.
///
/// `uidvalidity` is the generation the cached `uid` was bound to. A targeted
/// STORE must not use a UID whose generation no longer matches the selected
/// mailbox: the number may address an unrelated message.
pub fn folder_read_targets(
    conn: &Connection,
    folder_id: &str,
    owner_id: &str,
) -> Result<Vec<FolderReadTarget>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.uid, m.uidvalidity,
                CASE WHEN w.owner_id IS NULL OR w.owner_id = ?2 THEN 1 ELSE 0 END
           FROM message m
           LEFT JOIN message_flag_writer w
                  ON w.message_id = m.id AND w.field = 'seen'
          WHERE m.folder_id = ?1
          ORDER BY m.id",
    )?;
    let rows = stmt.query_map(params![folder_id, owner_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, i64>(3)? != 0,
        ))
    })?;
    let mut targets = Vec::new();
    for row in rows {
        targets.push(row?);
    }
    Ok(targets)
}

/// Mark read only the rows whose `seen` writer is still `owner_id`.
///
/// A row taken over by a later action is left untouched, so a retried
/// folder-wide operation cannot overwrite that action's local value.
pub fn mark_read_owned(conn: &Connection, folder_id: &str, owner_id: &str) -> Result<usize> {
    let count = conn.execute(
        "UPDATE message SET seen = 1, updated_at = ?2
          WHERE folder_id = ?1 AND seen = 0
            AND NOT EXISTS (
                SELECT 1 FROM message_flag_writer w
                 WHERE w.message_id = message.id
                   AND w.field = 'seen'
                   AND w.owner_id <> ?3
            )",
        params![folder_id, now_string(), owner_id],
    )?;
    crate::repo::folders::recompute_counts(conn, folder_id)?;
    Ok(count)
}

/// Hand a flag field from one owner to another (or clear it).
///
/// Undoing a never-sent operation must not leave that cancelled operation as
/// the durable writer: the previous (still pending) action would then be
/// skipped by the executor as "not the owner" and the server would keep the
/// stale value. The `value`/`action` describe the earlier write.
pub fn reassign_flag_writer(
    conn: &Connection,
    message_id: &str,
    field: &str,
    from_owner: &str,
    to_owner: Option<&str>,
    value: Option<bool>,
    action: Option<&str>,
) -> Result<usize> {
    match to_owner {
        Some(to_owner) => {
            // The revision must move on a hand-off: a reader that observed the
            // previous owner/sequence (for example auto mark-as-read during a
            // body load) would otherwise still see the same `seq` after an
            // undo restored the value and returned ownership to an earlier
            // action.
            let seq = next_flag_writer_seq(conn)?;
            let changed = conn.execute(
                "UPDATE message_flag_writer
                    SET owner_id = ?1,
                        value = COALESCE(?2, value),
                        action = COALESCE(?3, action),
                        seq = ?7
                  WHERE message_id = ?4 AND field = ?5 AND owner_id = ?6",
                params![
                    to_owner,
                    value.map(|value| value as i64),
                    action,
                    message_id,
                    field,
                    from_owner,
                    seq
                ],
            )?;
            // Only a real hand-off is a change a reader must observe; a stale
            // `from_owner` changed nothing.
            if changed > 0 {
                set_flag_revision(conn, message_id, seq)?;
            }
            Ok(changed)
        }
        None => {
            // Clearing the owner is a change too. Without a message-level
            // revision the row simply disappears, and a reader that observed
            // `None` before could not tell it apart from "still no writer"
            // after an undo restored the field.
            let seq = next_flag_writer_seq(conn)?;
            let changed = conn.execute(
                "DELETE FROM message_flag_writer
                  WHERE message_id = ?1 AND field = ?2 AND owner_id = ?3",
                params![message_id, field, from_owner],
            )?;
            if changed > 0 {
                set_flag_revision(conn, message_id, seq)?;
            }
            Ok(changed)
        }
    }
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

/// The local row that unambiguously represents `header` in one folder.
///
/// A Message-ID is not unique: the same mail copied twice has two rows with the
/// same header and different UIDs. Reusing a row through `LIMIT 1` would
/// overwrite one copy and clear its waiting/reconcile markers, so the caller
/// must pass how many server messages in the batch carry this header and only
/// a single local candidate (not hidden for a pending delete) may be reused.
pub fn unambiguous_message_id_row(
    conn: &Connection,
    account_id: &str,
    folder_id: &str,
    header: &str,
    incoming_count: usize,
) -> Result<Option<String>> {
    if incoming_count != 1 {
        return Ok(None);
    }
    let mut statement = conn.prepare(
        "SELECT id, pending_delete FROM message
          WHERE account_id = ?1 AND folder_id = ?2 AND message_id_header = ?3",
    )?;
    let mut rows = statement.query(params![account_id, folder_id, header])?;
    let mut candidates = Vec::new();
    while let Some(row) = rows.next()? {
        let pending_delete: i64 = row.get(1)?;
        if pending_delete == 0 {
            candidates.push(row.get::<_, String>(0)?);
        }
    }
    Ok(if candidates.len() == 1 {
        candidates.pop()
    } else {
        None
    })
}

/// Local row that already represents the exact provider copy `header` names.
///
/// [`unambiguous_message_id_row`] matches by Message-ID alone. That is fine
/// when the provider id is stable (Graph), but IMAP reuses the header for the
/// server copy and the APPEND copy of the same letter. Reusing the row that a
/// *different* UID already owns would overwrite its UID, UIDVALIDITY and Bcc,
/// destroying one provider identity and the only Bcc in the database.
///
/// The identity rules are therefore:
///
/// * a row bound to the incoming UID inside the same UIDVALIDITY is the copy
///   itself and may be reused. This is decided **before** looking at the
///   batch or the Message-ID: the row's local id may have been preserved from
///   another folder by a move/reconcile, so it is not the deterministic id
///   derived from the new UID, and a duplicate header in the same batch says
///   nothing about which copy the UID owns. Refusing here used to leave the
///   incoming entry to insert the derived id while the exact row still owned
///   `(folder_id, uid)`, failing the whole batch on `idx_message_unique_uid`;
/// * when no row carries that identity, a Message-ID may bind **one**
///   identity-less projection (`uid IS NULL`, waiting for a move/reconcile)
///   only when the batch has exactly one message with the header and it is the
///   folder's only candidate. This keeps the contract the reconciliation path
///   relies on: with a second local copy sharing the header the binding is
///   ambiguous and must not clear anyone's markers;
/// * a row bound to a different UID is never a candidate. That incoming entry
///   is a separate provider copy and gets its own row.
///
/// A pending permanent delete is returned like any other exact identity. The
/// caller already has the folder's `pending_delete` set and must skip the
/// entry instead of writing it; matching it here keeps the skip from being
/// bypassed by a derived id that fails the unique index.
pub fn unambiguous_message_id_row_for_identity(
    conn: &Connection,
    account_id: &str,
    folder_id: &str,
    header: &str,
    incoming_count: usize,
    uid: Option<i64>,
    uidvalidity: Option<i64>,
) -> Result<Option<String>> {
    let Some(uid) = uid else {
        return Ok(None);
    };
    // UIDs are never reused inside one UIDVALIDITY, so this pair names the
    // provider copy itself. `IS` compares NULL generations correctly.
    let exact: Option<String> = conn
        .query_row(
            "SELECT id FROM message
              WHERE account_id = ?1 AND folder_id = ?2 AND uid = ?3 AND uidvalidity IS ?4",
            params![account_id, folder_id, uid, uidvalidity],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = exact {
        return Ok(Some(id));
    }
    // A row stored before the generation was known still owns this UID; the
    // unique index leaves room for only one row, so reuse it instead of
    // inserting a colliding derived id.
    if uidvalidity.is_some() {
        let unknown_generation: Option<String> = conn
            .query_row(
                "SELECT id FROM message
                  WHERE account_id = ?1 AND folder_id = ?2 AND uid = ?3
                    AND uidvalidity IS NULL",
                params![account_id, folder_id, uid],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = unknown_generation {
            return Ok(Some(id));
        }
    }
    // No row owns the incoming identity: this is a new provider copy. A
    // Message-ID may only bind a single identity-less projection when the
    // batch is unambiguous and nothing else in the folder shares the header;
    // otherwise the entry is imported as its own row.
    if incoming_count != 1 {
        return Ok(None);
    }
    let mut statement = conn.prepare(
        "SELECT id, uid, uidvalidity FROM message
          WHERE account_id = ?1 AND folder_id = ?2 AND message_id_header = ?3
            AND pending_delete = 0",
    )?;
    let mut rows = statement.query(params![account_id, folder_id, header])?;
    let mut candidates: Vec<(String, Option<i64>)> = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let row_uid: Option<i64> = row.get(1)?;
        candidates.push((id, row_uid));
    }
    if candidates.len() == 1 && candidates[0].1.is_none() {
        return Ok(Some(candidates.remove(0).0));
    }
    Ok(None)
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
        coverage: None,
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
    attach_draft_sync_state(conn, &mut items)?;
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
        coverage: None,
    })
}

/// Recover the local draft id from the stable draft Message-ID header.
fn draft_id_from_header(header: &str) -> Option<&str> {
    header
        .trim()
        .strip_prefix("nuncio-draft-")
        .and_then(|rest| rest.strip_suffix("@nuncio.local"))
        .filter(|value| !value.is_empty())
}

/// Annotate draft projections with their remote-sync state.
///
/// The state is derived (not stored on `message`) so it cannot go stale: it
/// reflects the active APPEND operations and the user's "keep local" choice.
/// Ordinary mail is left untouched.
pub fn attach_draft_sync_state(conn: &Connection, items: &mut [MessageSummary]) -> Result<()> {
    let message_ids: Vec<String> = items
        .iter()
        .filter(|item| item.is_draft)
        .map(|item| item.id.clone())
        .collect();
    if message_ids.is_empty() {
        return Ok(());
    }
    let mut draft_by_message: HashMap<String, String> = HashMap::new();
    for chunk in message_ids.chunks(300) {
        let placeholders = (1..=chunk.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT id, message_id_header FROM message WHERE id IN ({placeholders})");
        let values: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(values.as_slice())?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let header: Option<String> = row.get(1)?;
            if let Some(draft_id) = header.as_deref().and_then(draft_id_from_header) {
                draft_by_message.insert(id, draft_id.to_string());
            }
        }
    }
    if draft_by_message.is_empty() {
        return Ok(());
    }
    let draft_ids: HashSet<String> = draft_by_message.values().cloned().collect();

    // Drafts the user explicitly kept local, and which drafts still exist. A
    // projection whose local draft row is gone (a discarded draft whose remote
    // cleanup is unresolved) must not offer "keep local draft": that action
    // would update nothing.
    let mut local_only: HashSet<String> = HashSet::new();
    let mut existing_drafts: HashSet<String> = HashSet::new();
    let ids: Vec<String> = draft_ids.iter().cloned().collect();
    for chunk in ids.chunks(300) {
        let placeholders = (1..=chunk.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT id, sync_suppressed FROM draft WHERE id IN ({placeholders})");
        let values: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(values.as_slice())?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let suppressed: i64 = row.get(1)?;
            existing_drafts.insert(id.clone());
            if suppressed != 0 {
                local_only.insert(id);
            }
        }
    }

    // APPENDs whose result is still unconfirmed. Settled operations that ended
    // as `candidateOnly` are included: the ownership limit outlives `Done`, so
    // the draft keeps showing "pending confirmation" until the user resolves
    // it. The payload is JSON; this deliberately mirrors the streamed serde
    // representation. For active attempts only a retried/deferred one is
    // surfaced: a fresh upload is `inProgress` with no next attempt and should
    // not flash a warning.
    let mut pending: HashSet<String> = HashSet::new();
    {
        let mut statement = conn.prepare(
            "SELECT payload_json, attempts, next_retry_at, state FROM pending_op
              WHERE kind = 'appendDraft'
                AND (state IN ('pending', 'inProgress')
                     OR COALESCE(json_extract(payload_json, '$.candidateOnly'), 0) = 1)",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let attempts: i64 = row.get(1)?;
            let next_retry_at: Option<String> = row.get(2)?;
            let state: String = row.get(3)?;
            let active = matches!(state.as_str(), "pending" | "inProgress");
            if active && attempts <= 0 && next_retry_at.is_none() {
                continue;
            }
            let payload: Option<String> = row.get(0)?;
            let Some(payload) = payload else { continue };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&payload) else {
                continue;
            };
            let Some(draft_id) = value.get("draftId").and_then(|value| value.as_str()) else {
                continue;
            };
            if !draft_ids.contains(draft_id) {
                continue;
            }
            let append_state = value
                .get("appendState")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown");
            let discard_requested = value
                .get("discardRequested")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let cleanup_done = value
                .get("cleanupDone")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let removal_state = value
                .get("removalState")
                .and_then(|value| value.as_str())
                .unwrap_or("notStarted");
            let candidate_only = value
                .get("candidateOnly")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            if candidate_only
                || matches!(append_state, "unknown" | "acked")
                || (discard_requested && !cleanup_done)
                || removal_state == "unknown"
            {
                pending.insert(draft_id.to_string());
            }
        }
    }

    for item in items.iter_mut() {
        let Some(draft_id) = draft_by_message.get(&item.id) else {
            continue;
        };
        item.draft_sync_state = if local_only.contains(draft_id) {
            Some("localOnly".to_string())
        } else if pending.contains(draft_id) && existing_drafts.contains(draft_id) {
            // A settled candidate with no local draft left is only a server
            // draft; there is nothing for the user to keep or resolve here.
            Some("pendingConfirmation".to_string())
        } else {
            None
        };
    }
    Ok(())
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
        let identity = body_identity(conn, id)?;
        set_body(conn, id, path.as_deref(), Some(&body), identity.as_deref())?;
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

/// Provider copy a cached body was read from, recorded with `body_path`.
pub fn body_identity(conn: &Connection, id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT body_identity FROM message WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()?
        .flatten())
}

/// Key of the server copy a row currently names.
///
/// The cached-MIME binding and the row's current identity are compared as these
/// strings: a Graph row is identified by its provider message id (the synthetic
/// UID can stay the same across a rebind only in tests, and the id is the only
/// identity Graph accepts), an IMAP row by folder + UID + UIDVALIDITY. A row
/// with no provider identity has no key, so a cached body can never be
/// attributed to it.
pub fn body_identity_key(stored: &StoredMessage) -> Option<String> {
    if let Some(remote) = stored
        .gm_msgid
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(format!("graph:{remote}"));
    }
    let uid = stored.uid?;
    Some(format!(
        "imap:{}:{}:{}",
        stored.summary.folder_id,
        uid,
        stored
            .uidvalidity
            .map(|value| value.to_string())
            .unwrap_or_default()
    ))
}

/// Identity of a message row captured before a queued command is built.
///
/// A command is prepared from a row read outside the enqueue transaction. By
/// the time that transaction runs, a sync may have deleted the projection or
/// rebound the same local id to another copy (a new UID generation for IMAP, a
/// new provider id for Graph). Re-checking this value inside the transaction
/// prevents the queue row and its flag writer from being created for a copy
/// that no longer exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageIdentity {
    pub id: String,
    pub account_id: String,
    pub folder_id: String,
    pub uid: Option<i64>,
    pub uidvalidity: Option<i64>,
    pub gm_msgid: Option<String>,
}

impl MessageIdentity {
    pub fn of(stored: &StoredMessage) -> Self {
        Self {
            id: stored.summary.id.clone(),
            account_id: stored.summary.account_id.clone(),
            folder_id: stored.summary.folder_id.clone(),
            uid: stored.uid,
            uidvalidity: stored.uidvalidity,
            gm_msgid: stored.gm_msgid.clone(),
        }
    }
}

/// Whether `expected.id` still names the same copy the caller read.
///
/// Every component is compared: account and folder identify the row's place,
/// `uid`+`uidvalidity` identify an IMAP generation, and `gm_msgid` identifies a
/// Graph copy even when a rebind kept the synthetic UID. A missing row returns
/// `false`, which is what makes a deleted projection refuse the queue write.
pub fn identity_matches(conn: &Connection, expected: &MessageIdentity) -> Result<bool> {
    let Some(stored) = get(conn, &expected.id)? else {
        return Ok(false);
    };
    Ok(stored.summary.account_id == expected.account_id
        && stored.summary.folder_id == expected.folder_id
        && stored.uid == expected.uid
        && stored.uidvalidity == expected.uidvalidity
        && stored.gm_msgid == expected.gm_msgid)
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
///
/// Rows waiting for a permanent delete to be confirmed, and rows whose failed
/// operation is still waiting for a targeted reconciliation, are preserved: the
/// server has not acknowledged their final state, so dropping them here would
/// lose their attachments and FTS documents and leave a refused operation with
/// nothing to restore. Their UIDs belong to the old generation, so the identity
/// is cleared and `target_pending` is set until a sync binds the new one.
pub fn clear_folder(conn: &Connection, folder_id: &str) -> Result<()> {
    // The old generation is gone, so the recorded pre-operation UID cannot be
    // checked any more. Keep the reason and the owning operation: the row must
    // still be reconciled, just only through its Message-ID header.
    conn.execute(
        "UPDATE message
            SET uid = NULL,
                uidvalidity = NULL,
                gm_msgid = NULL,
                target_pending = 1,
                move_target_remote_id = NULL,
                move_target_folder_id = NULL,
                move_target_op_id = NULL,
                move_target_seq = NULL,
                reconcile_uid = NULL,
                reconcile_uidvalidity = NULL,
                updated_at = ?2
          WHERE folder_id = ?1 AND (pending_delete = 1 OR awaiting_reconcile = 1)",
        params![folder_id, now_string()],
    )?;
    conn.execute(
        "DELETE FROM message_flag_writer WHERE message_id IN (
             SELECT id FROM message
              WHERE folder_id = ?1 AND pending_delete = 0 AND awaiting_reconcile = 0
         )",
        params![folder_id],
    )?;
    conn.execute(
        "DELETE FROM fts_mail WHERE message_id IN (
             SELECT id FROM message
              WHERE folder_id = ?1 AND pending_delete = 0 AND awaiting_reconcile = 0
         )",
        params![folder_id],
    )?;
    conn.execute(
        "DELETE FROM message
          WHERE folder_id = ?1 AND pending_delete = 0 AND awaiting_reconcile = 0",
        params![folder_id],
    )?;
    crate::repo::folders::recompute_counts(conn, folder_id)
}
