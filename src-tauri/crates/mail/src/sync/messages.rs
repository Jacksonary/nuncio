//! Metadata-first message synchronization.
//!
//! Strategy (see PLAN §6.1):
//! * initial sync: newest-first UID batches of 200 inside the account's sync
//!   window, metadata only, previews for the first text part (2 KiB);
//! * incremental sync: CONDSTORE `CHANGEDSINCE` for flags when available,
//!   otherwise a UID/FLAGS diff; new messages from `uidnext:*`; expunge
//!   detection via `UID SEARCH`;
//! * UIDVALIDITY changes clear the folder and force a fresh initial sync;
//! * a full flag reconcile runs every 12 hours.

use std::collections::{HashMap, HashSet};

use async_imap::imap_proto::{AttributeValue, MessageSection, Response, SectionPath, Status};
use async_imap::types::Fetch;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use futures::TryStreamExt;
use nuncio_core::models::SyncModule;
use nuncio_core::models::{Folder, FolderRole};
use nuncio_core::{events, repo, AppError, CoreContext, Result};
use rusqlite::OptionalExtension;
use tracing::{debug, warn};

use crate::imap::conn::{ImapConnection, FETCH_TIMEOUT};
use crate::imap::fetch::{meta_from_fetch, FlagSnapshot};
use crate::util::{imap_date, imap_error, uid_set};

const BATCH_SIZE: usize = 200;
const FLAG_BATCH_SIZE: usize = 500;
const PREVIEW_BYTES: usize = 2048;
const FULL_RECONCILE_HOURS: i64 = 12;

/// Result of syncing one folder.
#[derive(Debug, Default)]
pub struct SyncOutcome {
    pub new_messages: Vec<nuncio_core::models::MessageSummary>,
    pub updated_ids: Vec<String>,
    pub deleted_ids: Vec<String>,
    pub initial: bool,
    pub folder_id: String,
}

/// Synchronize one folder. Callers own the sync-status lifecycle
/// (`begin`/`finish`/`fail`) except for batch progress updates.
pub async fn sync_folder(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
) -> Result<SyncOutcome> {
    if !folder.is_selectable {
        return Ok(SyncOutcome {
            folder_id: folder.id.clone(),
            ..SyncOutcome::default()
        });
    }

    let (stored_validity, stored_uidnext, stored_modseq) = ctx
        .db
        .read({
            let folder_id = folder.id.clone();
            move |db| repo::folders::sync_flags(db, &folder_id)
        })
        .await?;

    let mailbox = if conn.caps.condstore {
        conn.select_condstore(&folder.remote_path).await
    } else {
        conn.select(&folder.remote_path).await
    }
    .map_err(|error| {
        if error.code() == "protocol" {
            AppError::not_found(format!("mailbox {} is not available", folder.remote_path))
        } else {
            error
        }
    })?;

    let current_validity = mailbox.uid_validity.map(|value| value as i64);
    let current_uidnext = mailbox.uid_next.map(|value| value as i64);
    let current_modseq = mailbox.highest_modseq.map(|value| value as i64);

    let mut initial = stored_uidnext.is_none();
    if let (Some(stored), Some(current)) = (stored_validity, current_validity) {
        if stored != current {
            warn!(
                folder = %folder.remote_path,
                stored, current,
                "UIDVALIDITY changed; clearing local folder"
            );
            let folder_id = folder.id.clone();
            ctx.db
                .write(move |db| {
                    repo::messages::clear_folder(db, &folder_id)?;
                    repo::folders::set_sync_flags(db, &folder_id, None, None, None)?;
                    repo::folders::recompute_counts(db, &folder_id)
                })
                .await?;
            initial = true;
        }
    }

    let mut outcome = SyncOutcome {
        initial,
        folder_id: folder.id.clone(),
        ..SyncOutcome::default()
    };

    if initial {
        initial_sync(
            ctx,
            conn,
            account,
            folder,
            current_validity,
            current_uidnext,
            &mut outcome,
        )
        .await?;
    } else {
        incremental_sync(
            ctx,
            conn,
            account,
            folder,
            current_validity,
            stored_uidnext,
            stored_modseq,
            &mut outcome,
        )
        .await?;
    }

    let folder_id = folder.id.clone();
    let last_sync = Utc::now();
    ctx.db
        .write(move |db| {
            repo::folders::set_sync_flags(
                db,
                &folder_id,
                current_validity,
                current_uidnext,
                current_modseq,
            )?;
            repo::folders::set_last_sync_at(db, &folder_id, last_sync)?;
            repo::folders::recompute_counts(db, &folder_id)
        })
        .await?;

    Ok(outcome)
}

async fn initial_sync(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    uidvalidity: Option<i64>,
    uidnext: Option<i64>,
    outcome: &mut SyncOutcome,
) -> Result<()> {
    let mut uids = search_uids(conn, account.sync_window_days).await?;
    uids.sort_unstable_by(|a, b| b.cmp(a)); // newest first
    let total = uids.len() as u64;

    ctx.sync_status
        .begin(&account.id, SyncModule::Mail, Some(total.max(1)));

    let mut done = 0u64;
    for chunk in uids.chunks(BATCH_SIZE) {
        let metas = fetch_metadata(conn, account, folder, uidvalidity, chunk).await?;
        let new = persist_metadata(ctx, conn, account, folder, metas, true).await?;
        let ids: Vec<String> = new.iter().map(|item| item.id.clone()).collect();
        if !ids.is_empty() {
            events::emit_mail_new(
                ctx.events.as_ref(),
                account.id.clone(),
                folder.id.clone(),
                ids,
            );
        }
        outcome.new_messages.extend(new);
        done += chunk.len() as u64;
        ctx.sync_status
            .progress(&account.id, SyncModule::Mail, done, total.max(1));
        debug!(
            folder = %folder.remote_path,
            done, total, "initial sync batch stored"
        );
    }

    // Record the server's uidnext even if the folder was empty.
    if uidnext.is_none() && total == 0 {
        debug!(folder = %folder.remote_path, "initial sync: empty folder");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn incremental_sync(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    uidvalidity: Option<i64>,
    uidnext: Option<i64>,
    stored_modseq: Option<i64>,
    outcome: &mut SyncOutcome,
) -> Result<()> {
    let full_reconcile = needs_full_reconcile(ctx, &account.id, &folder.id).await?;

    if conn.caps.condstore && !full_reconcile && stored_modseq.is_some() {
        if let Some(modseq) = stored_modseq {
            match fetch_changed_flags(conn, modseq as u64).await {
                Ok(changes) => {
                    let updated = apply_flag_updates(ctx, folder, &changes).await?;
                    outcome.updated_ids.extend(updated);
                }
                Err(error) => {
                    warn!(%error, folder = %folder.remote_path, "CHANGEDSINCE failed; falling back to UID diff");
                    let changes = fetch_all_flags(conn, None).await?;
                    let updated = apply_flag_updates(ctx, folder, &changes).await?;
                    outcome.updated_ids.extend(updated);
                }
            }
        }
    } else {
        let changes = fetch_all_flags(conn, None).await?;
        let updated = apply_flag_updates(ctx, folder, &changes).await?;
        outcome.updated_ids.extend(updated);
        mark_full_reconcile(ctx, &account.id, &folder.id).await?;
    }

    // New messages (do not apply the sync window here: any new UID is recent).
    if let Some(uidnext) = uidnext {
        let start = uidnext.max(1);
        let uids = uid_range(conn, start).await?;
        if !uids.is_empty() {
            for chunk in uids.chunks(BATCH_SIZE) {
                let metas = fetch_metadata(conn, account, folder, uidvalidity, chunk).await?;
                let new = persist_metadata(ctx, conn, account, folder, metas, false).await?;
                let ids: Vec<String> = new.iter().map(|item| item.id.clone()).collect();
                if !ids.is_empty() {
                    events::emit_mail_new(
                        ctx.events.as_ref(),
                        account.id.clone(),
                        folder.id.clone(),
                        ids,
                    );
                }
                outcome.new_messages.extend(new);
            }
        }
    }

    // Expunge detection: compare every server UID with the local set.
    let server_uids: HashSet<i64> = search_all_uids(conn)
        .await?
        .into_iter()
        .map(|uid| uid as i64)
        .collect();
    let local_uids = ctx
        .db
        .read({
            let folder_id = folder.id.clone();
            move |db| local_uids(db, &folder_id)
        })
        .await?;
    let mut removed_ids: Vec<String> = Vec::new();
    for (id, uid) in local_uids {
        if !server_uids.contains(&uid) {
            removed_ids.push(id);
        }
    }
    if !removed_ids.is_empty() {
        let ids = removed_ids.clone();
        ctx.db
            .write(move |db| repo::messages::delete_local(db, &ids, true))
            .await?;
        events::emit_mail_changed(
            ctx.events.as_ref(),
            account.id.clone(),
            vec![folder.id.clone()],
            removed_ids.clone(),
            events::MailChangeKind::Deleted,
        );
        outcome.deleted_ids.extend(removed_ids);
    }

    Ok(())
}

async fn search_uids(conn: &mut ImapConnection, window_days: u32) -> Result<Vec<u32>> {
    let criteria = if window_days == 0 {
        "ALL".to_string()
    } else {
        let since = Utc::now() - ChronoDuration::days(window_days as i64);
        format!("SINCE {}", imap_date(since))
    };
    let session = &mut conn.session;
    tokio::time::timeout(FETCH_TIMEOUT, session.uid_search(criteria))
        .await
        .map_err(|_| AppError::network("UID SEARCH timed out"))?
        .map(|set| set.into_iter().collect())
        .map_err(imap_error)
}

async fn search_all_uids(conn: &mut ImapConnection) -> Result<Vec<u32>> {
    let session = &mut conn.session;
    tokio::time::timeout(FETCH_TIMEOUT, session.uid_search("ALL"))
        .await
        .map_err(|_| AppError::network("UID SEARCH ALL timed out"))?
        .map(|set| set.into_iter().collect())
        .map_err(imap_error)
}

async fn uid_range(conn: &mut ImapConnection, start: i64) -> Result<Vec<u32>> {
    let query = format!("UID {}:*", start.max(1));
    let session = &mut conn.session;
    tokio::time::timeout(FETCH_TIMEOUT, session.uid_search(query))
        .await
        .map_err(|_| AppError::network("UID SEARCH timed out"))?
        .map(|set| set.into_iter().collect())
        .map_err(imap_error)
}

struct FlagChange {
    uid: i64,
    flags: FlagSnapshot,
}

async fn fetch_changed_flags(conn: &mut ImapConnection, modseq: u64) -> Result<Vec<FlagChange>> {
    let query = format!("(UID FLAGS MODSEQ) (CHANGEDSINCE {modseq})");
    let session = &mut conn.session;
    let fetches = tokio::time::timeout(FETCH_TIMEOUT, async {
        let stream = session.uid_fetch("1:*", query).await.map_err(imap_error)?;
        stream.try_collect::<Vec<Fetch>>().await.map_err(imap_error)
    })
    .await
    .map_err(|_| AppError::network("CHANGEDSINCE fetch timed out"))??;
    Ok(fetches
        .iter()
        .filter_map(|fetch| {
            fetch.uid.map(|uid| FlagChange {
                uid: uid as i64,
                flags: FlagSnapshot::from_fetch(fetch),
            })
        })
        .collect())
}

async fn fetch_all_flags(
    conn: &mut ImapConnection,
    limit: Option<usize>,
) -> Result<Vec<FlagChange>> {
    let uids = search_all_uids(conn).await?;
    let mut all = Vec::new();
    let total = limit.unwrap_or(uids.len()).min(uids.len());
    for chunk in uids[..total].chunks(FLAG_BATCH_SIZE) {
        let set = uid_set(&chunk.iter().map(|uid| *uid as i64).collect::<Vec<_>>());
        let session = &mut conn.session;
        let fetches = tokio::time::timeout(FETCH_TIMEOUT, async {
            let stream = session
                .uid_fetch(set, "(UID FLAGS)")
                .await
                .map_err(imap_error)?;
            stream.try_collect::<Vec<Fetch>>().await.map_err(imap_error)
        })
        .await
        .map_err(|_| AppError::network("flag fetch timed out"))??;
        all.extend(fetches.iter().filter_map(|fetch| {
            fetch.uid.map(|uid| FlagChange {
                uid: uid as i64,
                flags: FlagSnapshot::from_fetch(fetch),
            })
        }));
    }
    Ok(all)
}

async fn fetch_metadata(
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    uidvalidity: Option<i64>,
    uids: &[u32],
) -> Result<Vec<crate::imap::fetch::MessageMeta>> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let set = uid_set(&uids.iter().map(|uid| *uid as i64).collect::<Vec<_>>());
    let mut query = String::from(
        "(UID FLAGS INTERNALDATE ENVELOPE BODYSTRUCTURE RFC822.SIZE \
         BODY.PEEK[HEADER.FIELDS (REFERENCES IN-REPLY-TO IMPORTANCE X-PRIORITY LIST-UNSUBSCRIBE CONTENT-TYPE)])",
    );
    if conn.caps.gmail {
        query = query.trim_end_matches(')').to_string() + " X-GM-THRID X-GM-MSGID X-GM-LABELS)";
    }
    let session = &mut conn.session;
    let fetches = tokio::time::timeout(FETCH_TIMEOUT, async {
        let stream = session.uid_fetch(set, &query).await.map_err(imap_error)?;
        stream.try_collect::<Vec<Fetch>>().await.map_err(imap_error)
    })
    .await
    .map_err(|_| AppError::network("metadata fetch timed out"))??;

    // async-imap 0.12 does not expose parsed `X-GM-THRID` values, so ask for
    // them with a small raw FETCH and keep the parsed attributes. Without this
    // Gmail conversations fall back to References/subject matching.
    let gm_threads = if conn.caps.gmail {
        match fetch_gm_thread_ids(conn, uids).await {
            Ok(ids) => ids,
            Err(error) => {
                debug!(%error, "X-GM-THRID fetch failed");
                HashMap::new()
            }
        }
    } else {
        HashMap::new()
    };

    let now = Utc::now();
    let mut metas = Vec::new();
    for fetch in &fetches {
        let gm_thrid = fetch
            .uid
            .and_then(|uid| gm_threads.get(&(uid as i64)))
            .map(String::as_str);
        match meta_from_fetch(fetch, &account.id, &folder.id, uidvalidity, now, gm_thrid) {
            Ok(Some(meta)) => metas.push(meta),
            Ok(None) => {}
            Err(error) => warn!(%error, "skipping malformed FETCH response"),
        }
    }
    Ok(metas)
}

/// Fetch `X-GM-THRID` for a UID set. async-imap filters Gmail attributes out
/// of `Fetch`, but the raw response still carries `AttributeValue::GmailThrId`.
async fn fetch_gm_thread_ids(
    conn: &mut ImapConnection,
    uids: &[u32],
) -> Result<HashMap<i64, String>> {
    if uids.is_empty() {
        return Ok(HashMap::new());
    }
    let set = uid_set(&uids.iter().map(|uid| *uid as i64).collect::<Vec<_>>());
    let command = format!("UID FETCH {set} (UID X-GM-THRID)");
    let session = &mut conn.session;
    let tag = tokio::time::timeout(FETCH_TIMEOUT, session.run_command(command))
        .await
        .map_err(|_| AppError::network("X-GM-THRID command timed out"))?
        .map_err(imap_error)?;
    let mut result = HashMap::new();
    loop {
        let response = tokio::time::timeout(FETCH_TIMEOUT, session.read_response())
            .await
            .map_err(|_| AppError::network("X-GM-THRID response timed out"))?
            .map_err(|error| AppError::network(format!("X-GM-THRID read failed: {error}")))?;
        let Some(data) = response else {
            break;
        };
        match data.parsed() {
            Response::Fetch(_, attributes) => {
                let mut uid = None;
                let mut thrid = None;
                for attribute in attributes {
                    match attribute {
                        AttributeValue::Uid(value) => uid = Some(*value as i64),
                        AttributeValue::GmailThrId(value) => thrid = Some(value.to_string()),
                        _ => {}
                    }
                }
                if let (Some(uid), Some(thrid)) = (uid, thrid) {
                    result.insert(uid, thrid);
                }
            }
            Response::Done {
                tag: done, status, ..
            } => {
                if done.0 == tag.0 {
                    if !matches!(status, Status::Ok) {
                        debug!(?status, "server rejected X-GM-THRID fetch");
                    }
                    break;
                }
            }
            _ => {}
        }
    }
    Ok(result)
}

/// Insert/update fetched metadata, deduplicating Gmail messages across labels.
async fn persist_metadata(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    metas: Vec<crate::imap::fetch::MessageMeta>,
    initial: bool,
) -> Result<Vec<nuncio_core::models::MessageSummary>> {
    if metas.is_empty() {
        return Ok(Vec::new());
    }
    let account_id = account.id.clone();
    let folder_id = folder.id.clone();
    let incoming_role = folder.role;
    let gmail = account.capabilities.gmail_extensions;
    let account_email = account.email.to_lowercase();
    let metas_for_write = metas.clone();
    let result = ctx
        .db
        .write(move |db| {
            let mut inserted = Vec::new();
            for meta in &metas_for_write {
                let mut stored = meta.stored.clone();
                // Reuse the local id of a row that already represents this
                // Message-ID in the same folder (UIDs change after moves/copies).
                if let Some(header) = stored.message_id_header.as_deref() {
                    if let Some((existing_id, existing_folder)) = db
                        .query_row(
                            "SELECT id, folder_id FROM message
                             WHERE account_id = ?1 AND folder_id = ?2 AND message_id_header = ?3 LIMIT 1",
                            rusqlite::params![account_id, stored.summary.folder_id, header],
                            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                        )
                        .optional()?
                    {
                        if existing_folder == stored.summary.folder_id {
                            stored.summary.id = existing_id;
                        }
                    }
                }
                if gmail {
                    if let Some(gm_msgid) = stored.gm_msgid.as_deref() {
                        if let Some((existing_id, existing_folder, existing_labels)) = db
                            .query_row(
                                "SELECT id, folder_id, labels_json FROM message WHERE account_id = ?1 AND gm_msgid = ?2 LIMIT 1",
                                rusqlite::params![account_id, gm_msgid],
                                |row| {
                                    Ok((
                                        row.get::<_, String>(0)?,
                                        row.get::<_, String>(1)?,
                                        row.get::<_, Option<String>>(2)?,
                                    ))
                                },
                            )
                            .optional()?
                        {
                            if existing_folder == stored.summary.folder_id {
                                // Same label and message, but the UID may have moved.
                                stored.summary.id = existing_id;
                            } else {
                                let existing_role = repo::folders::get(db, &existing_folder)?
                                    .map(|f| f.role)
                                    .unwrap_or(FolderRole::Other);
                                if folder_priority(incoming_role) < folder_priority(existing_role) {
                                    // The incoming label is more specific (e.g. Inbox
                                    // before a custom label); make it the primary
                                    // folder while keeping every label.
                                    if repo::messages::by_uid(db, &folder_id, stored.uid.unwrap_or(0))?.is_some() {
                                        // Defensive: a stale row already owns this UID.
                                        continue;
                                    }
                                    stored.summary.id = existing_id;
                                    let existing_labels: Vec<String> =
                                        nuncio_core::repo::json_or_default(
                                            existing_labels.as_deref(),
                                            "labels_json",
                                        );
                                    stored.labels = union_labels(&existing_labels, &stored.labels);
                                } else {
                                    // Already owned by an equal or more specific
                                    // folder; merge labels only. Gmail label
                                    // folders query these labels, so nothing is lost.
                                    merge_labels(db, &existing_id, &stored.labels)?;
                                    continue;
                                }
                            }
                        }
                    }
                }
                // Reuse the parent conversation when it is already local; the
                // derived key only applies when no ancestor has been stored yet.
                resolve_thread_id(db, &account_id, &mut stored)?;
                let existed: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM message WHERE id = ?1)",
                    rusqlite::params![stored.summary.id],
                    |row| row.get(0),
                )?;
                repo::messages::upsert(db, &stored)?;
                // A late parent may arrive after its children; move those
                // children into the now-authoritative conversation.
                reconcile_threads(
                    db,
                    &account_id,
                    &stored.summary.thread_id,
                    stored.message_id_header.as_deref(),
                )?;
                if !existed {
                    inserted.push(stored.summary.clone());
                }
            }
            // Messages authored by this account feed recipient suggestions.
            let sent: Vec<nuncio_core::models::Address> = metas_for_write
                .iter()
                .filter(|meta| {
                    meta.stored
                        .summary
                        .r#from
                        .as_ref()
                        .map(|from| from.email.to_lowercase() == account_email)
                        .unwrap_or(false)
                })
                .flat_map(|meta| {
                    let mut recipients = meta.stored.summary.to.clone();
                    recipients.extend(meta.stored.summary.cc.clone());
                    recipients
                })
                .collect();
            if !sent.is_empty() {
                repo::recipient_history::record(db, &sent)?;
            }
            repo::folders::recompute_counts(db, &folder_id)?;
            Ok(inserted)
        })
        .await?;

    // Best-effort previews for the newly inserted rows. The full body fetch
    // later replaces these with the exact text.
    if !result.is_empty() {
        if let Err(error) = fill_previews(ctx, conn, account, folder, &metas, &result).await {
            debug!(%error, "preview prefetch failed");
        }
    }
    debug!(
        folder = %folder.remote_path,
        inserted = result.len(),
        initial,
        "metadata persisted"
    );
    Ok(result)
}

/// Ranking used to decide which Gmail label becomes a message's primary
/// folder. Lower wins; `All Mail` is the least specific because every message
/// is in it.
fn folder_priority(role: FolderRole) -> u8 {
    match role {
        FolderRole::Inbox => 0,
        FolderRole::Drafts => 1,
        FolderRole::Sent => 2,
        FolderRole::Junk => 3,
        FolderRole::Trash => 4,
        FolderRole::Archive => 5,
        FolderRole::Flagged => 6,
        FolderRole::Important => 7,
        FolderRole::Other => 8,
        FolderRole::All => 9,
        FolderRole::Outbox => 10,
    }
}

fn union_labels(existing: &[String], incoming: &[String]) -> Vec<String> {
    let mut result: Vec<String> = existing.to_vec();
    for label in incoming {
        if !result.iter().any(|current| current == label) {
            result.push(label.clone());
        }
    }
    result
}

fn merge_labels(conn: &rusqlite::Connection, id: &str, incoming: &[String]) -> Result<()> {
    if incoming.is_empty() {
        return Ok(());
    }
    let existing: Option<String> = conn
        .query_row(
            "SELECT labels_json FROM message WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let existing: Vec<String> =
        nuncio_core::repo::json_or_default(existing.as_deref(), "labels_json");
    let merged = union_labels(&existing, incoming);
    if merged != existing {
        repo::messages::set_labels(conn, id, &merged)?;
    }
    Ok(())
}

/// Prefer the conversation of a local ancestor over the key derived from the
/// headers. This keeps chains together when the local copy of a parent arrived
/// with a truncated `References` header.
pub(crate) fn resolve_thread_id(
    conn: &rusqlite::Connection,
    account_id: &str,
    stored: &mut nuncio_core::models::StoredMessage,
) -> Result<()> {
    if stored.summary.thread_id.starts_with("gm:") {
        return Ok(());
    }
    let own_id = stored.message_id_header.clone().unwrap_or_default();
    let mut candidates: Vec<String> = Vec::new();
    let mut push_candidate = |value: String| {
        if !value.is_empty()
            && value != own_id
            && !candidates.iter().any(|current| current == &value)
        {
            candidates.push(value);
        }
    };
    if let Some(parent) = stored.in_reply_to.as_deref() {
        push_candidate(nuncio_mime::normalize_message_id(parent));
    }
    // The nearest reference is the last one.
    for reference in stored.references.iter().rev() {
        push_candidate(nuncio_mime::normalize_message_id(reference));
    }
    if candidates.is_empty() {
        return Ok(());
    }
    let mut thread_by_header: HashMap<String, String> = HashMap::new();
    for chunk in candidates.chunks(200) {
        let placeholders = (2..=chunk.len() + 1)
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT message_id_header, thread_id FROM message
             WHERE account_id = ?1 AND message_id_header IN ({placeholders})
             ORDER BY date ASC"
        );
        let mut values: Vec<rusqlite::types::Value> =
            vec![nuncio_core::repo::text(account_id.to_string())];
        values.extend(chunk.iter().cloned().map(rusqlite::types::Value::Text));
        let refs = nuncio_core::repo::query_params(&values);
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(refs.as_slice())?;
        while let Some(row) = rows.next()? {
            let header: String = row.get(0)?;
            let thread: String = row.get(1)?;
            if !thread.trim().is_empty() {
                thread_by_header.entry(header).or_insert(thread);
            }
        }
    }
    for candidate in candidates {
        if let Some(thread) = thread_by_header.get(&candidate) {
            stored.summary.thread_id = thread.clone();
            return Ok(());
        }
    }
    Ok(())
}

/// Move every local descendant that references `message_id_header` into
/// `thread_id`, walking the reference tree so a reply chain is repaired even
/// when the root arrives last.
pub(crate) fn reconcile_threads(
    conn: &rusqlite::Connection,
    account_id: &str,
    thread_id: &str,
    message_id_header: Option<&str>,
) -> Result<()> {
    let Some(header) = message_id_header
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };
    if thread_id.is_empty() || thread_id.starts_with("subj:") || thread_id == "unthreaded" {
        return Ok(());
    }
    let mut frontier = vec![header.to_string()];
    let mut visited: HashSet<String> = frontier.iter().cloned().collect();
    for _ in 0..32 {
        if frontier.is_empty() {
            break;
        }
        let mut next = Vec::new();
        for parent in frontier.drain(..) {
            let quoted = serde_json::to_string(&parent).unwrap_or_default();
            let mut statement = conn.prepare(
                "SELECT id, thread_id, message_id_header FROM message
                 WHERE account_id = ?1
                   AND (in_reply_to = ?2 OR instr(COALESCE(references_json, ''), ?3) > 0)",
            )?;
            let mut rows = statement.query(rusqlite::params![account_id, parent, quoted])?;
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let child_thread: String = row.get(1)?;
                let child_header: Option<String> = row.get(2)?;
                if child_thread != thread_id {
                    repo::messages::set_thread(conn, &id, thread_id)?;
                }
                if let Some(child_header) = child_header {
                    if !child_header.trim().is_empty() && visited.insert(child_header.clone()) {
                        next.push(child_header);
                    }
                }
            }
        }
        frontier = next;
    }
    Ok(())
}

async fn apply_flag_updates(
    ctx: &CoreContext,
    folder: &Folder,
    changes: &[FlagChange],
) -> Result<Vec<String>> {
    if changes.is_empty() {
        return Ok(Vec::new());
    }
    let folder_id = folder.id.clone();
    let changes: Vec<(i64, FlagSnapshot)> = changes
        .iter()
        .map(|change| (change.uid, change.flags))
        .collect();
    ctx.db
        .write(move |db| {
            let mut updated = Vec::new();
            for (uid, flags) in changes {
                let current = db
                    .query_row(
                        "SELECT id, seen, flagged, answered, forwarded, is_draft
                         FROM message WHERE folder_id = ?1 AND uid = ?2",
                        rusqlite::params![folder_id, uid],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)? != 0,
                                row.get::<_, i64>(2)? != 0,
                                row.get::<_, i64>(3)? != 0,
                                row.get::<_, i64>(4)? != 0,
                                row.get::<_, i64>(5)? != 0,
                            ))
                        },
                    )
                    .optional()?;
                let Some((id, seen, flagged, answered, forwarded, is_draft)) = current else {
                    continue;
                };
                if seen == flags.seen
                    && flagged == flags.flagged
                    && answered == flags.answered
                    && forwarded == flags.forwarded
                    && is_draft == flags.is_draft
                {
                    continue;
                }
                repo::messages::set_flags(
                    db,
                    std::slice::from_ref(&id),
                    Some(flags.seen),
                    Some(flags.flagged),
                    Some(flags.answered),
                    Some(flags.forwarded),
                )?;
                if is_draft != flags.is_draft {
                    db.execute(
                        "UPDATE message SET is_draft = ?2, updated_at = ?3 WHERE id = ?1",
                        rusqlite::params![id, flags.is_draft as i64, Utc::now().to_rfc3339()],
                    )?;
                }
                updated.push(id);
            }
            Ok(updated)
        })
        .await
}

fn local_uids(db: &rusqlite::Connection, folder_id: &str) -> Result<Vec<(String, i64)>> {
    let mut statement =
        db.prepare("SELECT id, uid FROM message WHERE folder_id = ?1 AND uid IS NOT NULL")?;
    let mut rows = statement.query(rusqlite::params![folder_id])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        result.push((row.get(0)?, row.get(1)?));
    }
    Ok(result)
}

async fn needs_full_reconcile(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
) -> Result<bool> {
    let key = format!("full_reconcile:{folder_id}");
    let account_id = account_id.to_string();
    let value = ctx
        .db
        .read(move |db| repo::sync_state::get(db, &account_id, "mail", &key))
        .await?;
    let Some(value) = value else {
        return Ok(true);
    };
    Ok(DateTime::parse_from_rfc3339(&value)
        .map(|date| {
            Utc::now() - date.with_timezone(&Utc) > ChronoDuration::hours(FULL_RECONCILE_HOURS)
        })
        .unwrap_or(true))
}

async fn mark_full_reconcile(ctx: &CoreContext, account_id: &str, folder_id: &str) -> Result<()> {
    let key = format!("full_reconcile:{folder_id}");
    let value = Utc::now().to_rfc3339();
    let account_id = account_id.to_string();
    ctx.db
        .write(move |db| repo::sync_state::set(db, &account_id, "mail", &key, &value))
        .await
}

/// Fetch the first 2 KiB of the first text part for newly inserted messages.
async fn fill_previews(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    _account: &nuncio_core::models::Account,
    folder: &Folder,
    metas: &[crate::imap::fetch::MessageMeta],
    inserted: &[nuncio_core::models::MessageSummary],
) -> Result<()> {
    let inserted_ids: HashSet<&str> = inserted.iter().map(|item| item.id.as_str()).collect();
    // Section -> (uids, transfer encoding, charset). Messages that share a
    // section layout usually share the encoding too, but keep the first
    // observed value per message by storing them alongside the uid.
    let mut groups: HashMap<String, (Vec<i64>, HashMap<i64, (Option<String>, Option<String>)>)> =
        HashMap::new();
    for meta in metas {
        if !inserted_ids.contains(meta.stored.summary.id.as_str()) {
            continue;
        }
        if let (Some(uid), Some(part)) = (meta.stored.uid, meta.text_part.as_deref()) {
            let entry = groups.entry(part.to_string()).or_insert_with(|| {
                (Vec::new(), HashMap::new())
            });
            entry.0.push(uid);
            entry.1.insert(
                uid,
                (meta.text_part_encoding.clone(), meta.text_part_charset.clone()),
            );
        }
    }
    for (part, (uids, part_meta)) in groups {
        let Some(section) = section_path(&part) else {
            continue;
        };
        for chunk in uids.chunks(50) {
            let set = uid_set(chunk);
            let query = format!("(UID BODY.PEEK[{part}]<0.{PREVIEW_BYTES}>)");
            let session = &mut conn.session;
            let fetches = match tokio::time::timeout(FETCH_TIMEOUT, async {
                let stream = session.uid_fetch(&set, &query).await.map_err(imap_error)?;
                stream.try_collect::<Vec<Fetch>>().await.map_err(imap_error)
            })
            .await
            {
                Ok(Ok(fetches)) => fetches,
                Ok(Err(error)) => {
                    debug!(%error, "preview fetch failed");
                    continue;
                }
                Err(_) => {
                    debug!("preview fetch timed out");
                    continue;
                }
            };
            let mut updates = Vec::new();
            for fetch in &fetches {
                if let Some(uid) = fetch.uid {
                    let bytes = if part.eq_ignore_ascii_case("TEXT") {
                        fetch.text()
                    } else {
                        fetch.section(&section)
                    };
                    if let Some(bytes) = bytes {
                        let (encoding, charset) = part_meta
                            .get(&(uid as i64))
                            .map(|(encoding, charset)| (encoding.clone(), charset.clone()))
                            .unwrap_or((None, None));
                        let text = nuncio_mime::decode_preview_part(
                            bytes,
                            encoding.as_deref(),
                            charset.as_deref(),
                        );
                        if !text.trim().is_empty() {
                            updates.push((uid as i64, nuncio_core::text::trim_preview(&text, 200)));
                        }
                    }
                }
            }
            if updates.is_empty() {
                continue;
            }
            let folder_id = folder.id.clone();
            let updated = ctx
                .db
                .write(move |db| {
                    let mut updated = Vec::new();
                    for (uid, preview) in updates {
                        if let Some(stored) = repo::messages::by_uid(db, &folder_id, uid)? {
                            repo::messages::update_preview(db, &stored.summary.id, &preview)?;
                            updated.push(stored.summary.id);
                        }
                    }
                    Ok(updated)
                })
                .await?;
            if !updated.is_empty() {
                events::emit_mail_changed(
                    ctx.events.as_ref(),
                    folder.account_id.clone(),
                    vec![folder.id.clone()],
                    updated,
                    events::MailChangeKind::Updated,
                );
            }
        }
    }
    Ok(())
}

fn section_path(part: &str) -> Option<SectionPath> {
    if part.eq_ignore_ascii_case("TEXT") {
        return Some(SectionPath::Full(MessageSection::Text));
    }
    let parts: Option<Vec<u32>> = part
        .split('.')
        .map(|segment| segment.parse::<u32>().ok())
        .collect();
    parts.map(|parts| SectionPath::Part(parts, None))
}

/// Build an IMAP search criteria string from a [`SearchQuery`].
///
/// `hasAttachment` has no portable IMAP equivalent and is intentionally left
/// to the local index (the server result is a superset). Strings are quoted and
/// escaped per RFC 3501; non-ASCII keys are sent as UTF-8 under an explicit
/// `CHARSET UTF-8` directive (the IMAP client library has no literal writer),
/// and [`server_search`] retries without the directive when a server rejects it.
pub fn imap_search_criteria(query: &nuncio_core::models::SearchQuery) -> Option<String> {
    fn escape(value: &str) -> String {
        value.replace('\\', "\\\\").replace('"', "\\\"")
    }
    let mut parts = Vec::new();
    let mut non_ascii = false;
    let push_string = |parts: &mut Vec<String>, key: &str, value: &str, non_ascii: &mut bool| {
        if value.chars().any(|character| !character.is_ascii()) {
            *non_ascii = true;
        }
        parts.push(format!("{key} \"{}\"", escape(value)));
    };
    if !query.text.trim().is_empty() {
        push_string(&mut parts, "TEXT", query.text.trim(), &mut non_ascii);
    }
    if let Some(from) = query
        .from
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        push_string(&mut parts, "FROM", from.trim(), &mut non_ascii);
    }
    if let Some(to) = query.to.as_deref().filter(|value| !value.trim().is_empty()) {
        push_string(&mut parts, "TO", to.trim(), &mut non_ascii);
    }
    if let Some(subject) = query
        .subject
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        push_string(&mut parts, "SUBJECT", subject.trim(), &mut non_ascii);
    }
    if query.unread == Some(true) {
        parts.push("UNSEEN".into());
    }
    if query.flagged == Some(true) {
        parts.push("FLAGGED".into());
    }
    if let Some(from) = query.date_from.as_deref() {
        if let Ok(date) = chrono::NaiveDate::parse_from_str(from, "%Y-%m-%d") {
            let value = date
                .and_hms_opt(0, 0, 0)
                .map(|naive| chrono::DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
                .unwrap_or_else(Utc::now);
            parts.push(format!("SINCE {}", imap_date(value)));
        }
    }
    if let Some(to) = query.date_to.as_deref() {
        if let Ok(date) = chrono::NaiveDate::parse_from_str(to, "%Y-%m-%d") {
            let value = date
                .and_hms_opt(0, 0, 0)
                .map(|naive| chrono::DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
                .unwrap_or_else(Utc::now)
                + chrono::Duration::days(1);
            parts.push(format!("BEFORE {}", imap_date(value)));
        }
    }
    if parts.is_empty() {
        None
    } else {
        let criteria = parts.join(" ");
        // RFC 3501 §6.4.4: non-ASCII search keys require an explicit charset.
        // Servers that reject CHARSET cause the caller to fall back to the
        // local index, which always supports UTF-8 text.
        if non_ascii {
            Some(format!("CHARSET UTF-8 {criteria}"))
        } else {
            Some(criteria)
        }
    }
}

/// The charset directive prepended to non-ASCII search criteria.
const UTF8_CHARSET_PREFIX: &str = "CHARSET UTF-8 ";

/// Criteria with the `CHARSET UTF-8` directive removed, for servers that do not
/// implement RFC 3501 §6.4.4.
pub fn search_without_charset(criteria: &str) -> Option<&str> {
    criteria.strip_prefix(UTF8_CHARSET_PREFIX)
}

/// Run a `UID SEARCH`, retrying once without `CHARSET UTF-8` when the server
/// rejects the charset. A server that rejects both is treated as "no server
/// hits" so callers can still use the local UTF-8 FTS index.
async fn search_with_fallback(conn: &mut ImapConnection, criteria: &str) -> Result<Vec<u32>> {
    match run_search(conn, criteria).await {
        Ok(uids) => Ok(uids),
        Err(error) => {
            let Some(fallback) = search_without_charset(criteria) else {
                return Err(error);
            };
            if error.code() != "protocol" {
                return Err(error);
            }
            debug!(%error, "server rejected CHARSET UTF-8; retrying without it");
            match run_search(conn, fallback).await {
                Ok(uids) => Ok(uids),
                Err(error) => {
                    debug!(
                        %error,
                        "server search without CHARSET failed; using the local index only"
                    );
                    Ok(Vec::new())
                }
            }
        }
    }
}

async fn run_search(conn: &mut ImapConnection, criteria: &str) -> Result<Vec<u32>> {
    let session = &mut conn.session;
    tokio::time::timeout(FETCH_TIMEOUT, session.uid_search(criteria))
        .await
        .map_err(|_| AppError::network("server search timed out"))?
        .map(|set| set.into_iter().collect())
        .map_err(imap_error)
}

/// Run an IMAP `UID SEARCH` and cache metadata for any matching messages that
/// are not stored locally yet. Returns local message ids.
pub async fn server_search(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    criteria: &str,
) -> Result<Vec<String>> {
    let mailbox = if conn.caps.condstore {
        conn.select_condstore(&folder.remote_path).await
    } else {
        conn.select(&folder.remote_path).await
    }?;
    let uidvalidity = mailbox.uid_validity.map(|value| value as i64);
    let found = search_with_fallback(conn, criteria).await?;
    if found.is_empty() {
        return Ok(Vec::new());
    }

    let folder_id = folder.id.clone();
    let known = ctx
        .db
        .read({
            let folder_id = folder_id.clone();
            move |db| {
                let rows = local_uids(db, &folder_id)?;
                Ok(rows
                    .into_iter()
                    .map(|(id, uid)| (uid, id))
                    .collect::<HashMap<_, _>>())
            }
        })
        .await?;
    let known_uids: HashSet<i64> = known.keys().copied().collect();
    let missing: Vec<u32> = found
        .iter()
        .copied()
        .filter(|uid| !known_uids.contains(&(*uid as i64)))
        .collect();
    for chunk in missing.chunks(BATCH_SIZE) {
        let metas = fetch_metadata(conn, account, folder, uidvalidity, chunk).await?;
        persist_metadata(ctx, conn, account, folder, metas, false).await?;
    }

    let found_set: HashSet<i64> = found.iter().map(|uid| *uid as i64).collect();
    let ids = ctx
        .db
        .read({
            let folder_id = folder_id.clone();
            move |db| {
                let rows = local_uids(db, &folder_id)?;
                Ok(rows
                    .into_iter()
                    .filter(|(_, uid)| found_set.contains(uid))
                    .map(|(id, _)| id)
                    .collect::<Vec<_>>())
            }
        })
        .await?;
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_criteria_are_quoted_and_combined() {
        let mut query = nuncio_core::models::SearchQuery::text("invoice \"2026\"");
        query.from = Some("billing@example.com".into());
        query.unread = Some(true);
        query.date_from = Some("2026-11-01".into());
        let criteria = imap_search_criteria(&query).expect("criteria");
        assert!(criteria.contains("TEXT \"invoice \\\"2026\\\"\""));
        assert!(criteria.contains("FROM \"billing@example.com\""));
        assert!(criteria.contains("UNSEEN"));
        assert!(criteria.contains("SINCE 01-Nov-2026"));
        assert!(imap_search_criteria(&nuncio_core::models::SearchQuery::default()).is_none());
    }

    #[test]
    fn search_criteria_declare_utf8_for_non_ascii_keys() {
        let query = nuncio_core::models::SearchQuery::text("会议纪要");
        let criteria = imap_search_criteria(&query).expect("criteria");
        assert_eq!(criteria, "CHARSET UTF-8 TEXT \"会议纪要\"");

        let mut query = nuncio_core::models::SearchQuery::text("report");
        query.subject = Some("周报".into());
        let criteria = imap_search_criteria(&query).expect("criteria");
        assert!(criteria.starts_with("CHARSET UTF-8 "));
        assert!(criteria.contains("TEXT \"report\""));
        assert!(criteria.contains("SUBJECT \"周报\""));

        // ASCII-only searches keep the classic form.
        let criteria = imap_search_criteria(&nuncio_core::models::SearchQuery::text("invoice"))
            .expect("criteria");
        assert_eq!(criteria, "TEXT \"invoice\"");
    }

    #[test]
    fn charset_fallback_strips_only_the_utf8_directive() {
        assert_eq!(
            search_without_charset("CHARSET UTF-8 TEXT \"会议纪要\""),
            Some("TEXT \"会议纪要\"")
        );
        assert_eq!(search_without_charset("TEXT \"invoice\""), None);
    }

    #[test]
    fn section_paths_round_trip() {
        assert_eq!(
            section_path("TEXT"),
            Some(SectionPath::Full(MessageSection::Text))
        );
        assert_eq!(
            section_path("1.2"),
            Some(SectionPath::Part(vec![1, 2], None))
        );
        assert_eq!(section_path("bad"), None);
    }
}
