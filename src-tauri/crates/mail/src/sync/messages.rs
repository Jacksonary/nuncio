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
use nuncio_core::models::{Folder, FolderRole, PendingOpState};
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

    if crate::graph::uses_graph(account) {
        // Clear early dispatch: a Microsoft account never touches the IMAP
        // session that was opened for it (Exchange refuses the auth anyway).
        return sync_folder_graph(ctx, account, folder).await;
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
            // Some servers (NetEase 163) never report UIDNEXT. Without a stored
            // value every later sync would look like a first sync: a full
            // window scan each time, and no increments. The highest UID we
            // hold, plus one, is a safe stand-in because UIDs only grow within
            // one UIDVALIDITY.
            //
            // With nothing stored (an empty folder, or only mail older than the
            // sync window) there is nothing to derive it from: keep it unset
            // rather than store 1, which would make the next incremental sync
            // request `UID 1:*` and pull the whole history past the window.
            let uidnext = match current_uidnext {
                Some(value) => Some(value),
                None => db
                    .query_row(
                        "SELECT MAX(uid) + 1 FROM message
                         WHERE folder_id = ?1 AND uid IS NOT NULL",
                        rusqlite::params![folder_id],
                        |row| row.get::<_, Option<i64>>(0),
                    )
                    .ok()
                    .flatten(),
            };
            repo::folders::set_sync_flags(
                db,
                &folder_id,
                current_validity,
                uidnext,
                current_modseq,
            )?;
            repo::folders::set_last_sync_at(db, &folder_id, last_sync)?;
            repo::folders::recompute_counts(db, &folder_id)
        })
        .await?;

    Ok(outcome)
}

/// Synchronize one folder through Microsoft Graph, without an IMAP connection.
///
/// The transport and delta-token recovery live in [`crate::graph::messages`];
/// this function is the bridge into the same local model the IMAP path writes:
/// every delta entry goes through `repo::messages::upsert`, deletions through
/// `repo::messages::delete_local`, and folder counts are recomputed.  Reusing
/// the repo layer (instead of a Graph-specific table writer) is what keeps
/// listing, search, threading, rules and notifications provider agnostic.
///
/// Graph has no IMAP UID.  The existing service layer only queues server
/// operations for rows that carry one, so each Graph message is given a
/// deterministic synthetic UID derived from its immutable Graph id.  The
/// executor resolves that UID back to the Graph id through the durable
/// `graph_remote:{folder_id}:{uid}` mapping written here; the value never
/// leaves the local database and is not sent to Microsoft.
pub async fn sync_folder_graph(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    folder: &Folder,
) -> Result<SyncOutcome> {
    if !folder.is_selectable {
        return Ok(SyncOutcome {
            folder_id: folder.id.clone(),
            ..SyncOutcome::default()
        });
    }

    let client = crate::graph::GraphMailClient::new();
    let cursor_key = crate::graph::messages::delta_cursor_key(&folder.id);
    let cursor = {
        let account_id = account.id.clone();
        let cursor_key = cursor_key.clone();
        ctx.db
            .read(move |db| repo::sync_state::get(db, &account_id, "mail", &cursor_key))
            .await?
    };

    // `Folder::remote_path` is the readable local path; Graph delta and
    // operation endpoints need the opaque folder id recorded at folder sync.
    let graph_path = crate::sync::folders::graph_folder_id(ctx, &account.id, &folder.remote_path)
        .await?
        .ok_or_else(|| {
            AppError::not_found(format!(
                "Graph folder id is unknown for {}; refresh folders first",
                folder.remote_path
            ))
        })?;
    let mut graph_folder = folder.clone();
    graph_folder.remote_path = graph_path;

    let delta = crate::graph::messages::sync_messages(
        &client,
        ctx,
        &account.id,
        &graph_folder,
        cursor.as_deref(),
        account.sync_window_days,
    )
    .await?;
    apply_graph_delta(ctx, account, folder, delta).await
}

/// Persist one Graph batch (delta pass or older-mail page) into the local
/// model and emit the matching events. Shared by [`sync_folder_graph`] and
/// [`load_older_graph`] so both write rows exactly the same way.
async fn apply_graph_delta(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    delta: crate::graph::messages::MessageDelta,
) -> Result<SyncOutcome> {
    let initial = delta.full_sync;
    // Snapshot the ids before the messages move into the write closure.  A
    // complete delta scan is the Graph equivalent of an IMAP expunge pass: any
    // local Graph row that the server did not return is gone.
    let seen_ids: HashSet<String> = delta
        .messages
        .iter()
        .map(|stored| stored.summary.id.clone())
        .collect();

    let account_id = account.id.clone();
    let folder_id = folder.id.clone();
    // Copy out of the borrowed account for the `'static` write closure.
    let sync_window_days = account.sync_window_days;
    let messages = delta.messages;
    let mut removed_ids = delta.removed_remote_ids;
    let resync_folder = folder.id.clone();

    let (inserted, updated, removed_ids) = ctx
        .db
        .write(move |db| {
            let mut inserted = Vec::new();
            let mut updated = Vec::new();
            for mut stored in messages {
                // Graph, like IMAP, can hand out a new provider id for a
                // message whose content did not change: every draft save
                // deletes the old server copy and APPENDs a new one, and a
                // move/copy also changes the id.  The Message-ID header is
                // stable across that churn, so adopt the row that already
                // represents it in this folder -- the same lookup the IMAP
                // metadata path performs.  Without this the composer's local
                // projection row and the APPEND echo become two rows for one
                // draft.
                let remote_id = stored.summary.id.clone();
                if let Some(header) = stored.message_id_header.as_deref() {
                    if let Some(existing_id) =
                        local_id_for_message_id(db, &account_id, &folder_id, header)?
                    {
                        stored.summary.id = existing_id;
                    }
                }
                // The service layer needs a UID to group queue entries; derive
                // it from the row id that is actually used so it stays stable
                // when a re-APPEND changes the Graph id of a draft.
                if stored.uid.is_none() {
                    stored.uid = Some(synthetic_uid(&stored.summary.id));
                }
                // Graph has no per-message answered/forwarded flag; both are
                // local-only (set when the user replies or forwards). Carry
                // the stored values into this upsert so a delta pass cannot
                // reset them. IMAP is unaffected: FETCH returns real
                // `\Answered`/`\Forwarded` flags and its path keeps calling
                // `repo::messages::upsert` directly.
                let existed = preserve_local_flags(db, &mut stored)?;
                // Adopt a local ancestor conversation when one already exists,
                // exactly like the IMAP metadata path does.
                resolve_thread_id(db, &account_id, &mut stored)?;
                repo::messages::upsert(db, &stored)?;
                // Persist the uid -> Graph id mapping independently of the
                // message row.  Optimistic actions move or even delete the row
                // before the queued operation runs (permanent delete removes
                // it outright), so the executor cannot always recover the
                // Graph id from `message`.  The value must stay the Graph id,
                // never the local row id: the executor sends it back to
                // Microsoft, and the two differ after the reuse above.
                if let Some(uid) = stored.uid {
                    let key = format!("graph_remote:{folder_id}:{uid}");
                    repo::sync_state::set(db, &account_id, "mail", &key, &remote_id)?;
                }
                if existed {
                    updated.push(stored.summary.id.clone());
                } else {
                    inserted.push(stored.summary.clone());
                }
            }

            if initial {
                // A completed scan is authoritative for every synced row in
                // the folder, including rows left over from the IMAP era.
                removed_ids.extend(graph_prune_ids(
                    db,
                    &account_id,
                    &folder_id,
                    &seen_ids,
                    sync_window_days,
                )?);
            }
            if !removed_ids.is_empty() {
                // Drop the uid mapping together with the row.  The mapping is
                // keyed by that row's uid and a later sync derives a fresh uid
                // for a new row, so a stale entry could only mislead.
                for id in &removed_ids {
                    let uid: Option<Option<i64>> = db
                        .query_row(
                            "SELECT uid FROM message WHERE id = ?1",
                            rusqlite::params![id],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if let Some(Some(uid)) = uid {
                        let key = format!("graph_remote:{folder_id}:{uid}");
                        repo::sync_state::delete_key(db, &account_id, "mail", &key)?;
                    }
                }
                repo::messages::delete_local(db, &removed_ids, true)?;
            }
            repo::folders::set_last_sync_at(db, &resync_folder, Utc::now())?;
            repo::folders::recompute_counts(db, &resync_folder)?;
            Ok((inserted, updated, removed_ids))
        })
        .await?;

    let inserted_ids: Vec<String> = inserted.iter().map(|item| item.id.clone()).collect();
    if !inserted_ids.is_empty() {
        events::emit_mail_new(
            ctx.events.as_ref(),
            account.id.clone(),
            folder.id.clone(),
            inserted_ids,
        );
    }
    if !updated.is_empty() {
        events::emit_mail_changed(
            ctx.events.as_ref(),
            account.id.clone(),
            vec![folder.id.clone()],
            updated,
            events::MailChangeKind::Updated,
        );
    }
    let deleted_ids = removed_ids.clone();
    if !removed_ids.is_empty() {
        events::emit_mail_changed(
            ctx.events.as_ref(),
            account.id.clone(),
            vec![folder.id.clone()],
            removed_ids,
            events::MailChangeKind::Deleted,
        );
    }

    debug!(
        account = %account.id,
        folder = %folder.remote_path,
        inserted = inserted.len(),
        initial,
        "Graph message sync applied"
    );

    Ok(SyncOutcome {
        new_messages: inserted,
        deleted_ids,
        initial,
        folder_id: folder.id.clone(),
    })
}

/// Deterministic synthetic UID for a Graph message.
///
/// Graph ids are opaque strings; the local queue keys flag/move operations by
/// `(folder_id, uid)`.  A v5 UUID of the Graph id gives a stable positive
/// 63-bit integer: stable across restarts (unlike `DefaultHasher`, whose keys
/// are randomised per process) and effectively collision-free for one folder.
fn synthetic_uid(remote_id: &str) -> i64 {
    let uuid = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, remote_id.as_bytes());
    let mut value = 0u64;
    for byte in &uuid.as_bytes()[..8] {
        value = (value << 8) | u64::from(*byte);
    }
    // Keep the sign bit clear so the value is a normal positive UID; 0 is
    // reserved so it can never look like "no uid" in a numeric check.
    let value = value & (i64::MAX as u64);
    if value == 0 {
        1
    } else {
        value as i64
    }
}

/// Carry the local-only flags of an existing row into a Graph upsert.
///
/// Microsoft Graph exposes no per-message `answered` or `forwarded` state, so
/// [`crate::graph::messages::map_message`] always maps them as `false`. The
/// columns are set locally when the user replies or forwards
/// (`service::compose`), and `repo::messages::upsert` deliberately takes the
/// incoming value for them. Without this step the next delta pass would erase
/// those flags.
///
/// This lives in the Graph bridge on purpose. The IMAP path gets real
/// `\Answered` and `\Forwarded` flags from FETCH, so it must keep writing the
/// fetched values through `repo::messages::upsert` unchanged.
///
/// Returns whether the row already existed, which the caller uses to classify
/// the event (insert vs update).
fn preserve_local_flags(
    conn: &rusqlite::Connection,
    stored: &mut nuncio_core::models::StoredMessage,
) -> Result<bool> {
    let existing: Option<(i64, i64)> = conn
        .query_row(
            "SELECT answered, forwarded FROM message WHERE id = ?1",
            rusqlite::params![stored.summary.id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match existing {
        Some((answered, forwarded)) => {
            stored.summary.answered = answered != 0;
            stored.summary.forwarded = forwarded != 0;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Local row id already representing `message_id_header` in `folder_id`.
///
/// Provider ids are not stable (Graph changes a message's id when a draft is
/// re-APPENDed or a message is copied/moved) while the Message-ID is, so the
/// sync updates the existing row instead of inserting a second one.  This is
/// the same lookup the IMAP metadata path performs before its upsert.
fn local_id_for_message_id(
    conn: &rusqlite::Connection,
    account_id: &str,
    folder_id: &str,
    header: &str,
) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT id FROM message
             WHERE account_id = ?1 AND folder_id = ?2 AND message_id_header = ?3
             LIMIT 1",
            rusqlite::params![account_id, folder_id, header],
            |row| row.get(0),
        )
        .optional()?)
}

/// Local ids a completed Graph full scan must delete.
///
/// The scan is authoritative for every row that has a remote counterpart, but
/// rows without a `uid` are never candidates:
///
/// * a draft projection written by `compose_save` before its APPEND ran (the
///   composer owns that row and deletes it on send/discard);
/// * a row that was never synced at all.
///
/// Everything else is compared against the Graph ids the scan returned.  That
/// deliberately includes rows from a previous IMAP sync, which carry a real
/// `uid` but a NULL `gm_msgid`: they can never match a Graph id and must be
/// removed, otherwise switching a Microsoft account from IMAP to Graph leaves
/// every message listed twice.  A projection that was already synced but is
/// being re-APPENDed (a pending `appendDraft` operation exists) is kept,
/// because the disappearance of its old remote copy is expected and the new
/// copy has not necessarily been echoed yet.
///
/// `sync_window_days` matters because a Graph full scan is `$filter`ed to that
/// window: a row older than the window is simply not returned, so it cannot be
/// treated as deleted.  Only rows clearly inside the window (one extra day of
/// slack for clock/query skew) are judged.
fn graph_prune_ids(
    conn: &rusqlite::Connection,
    account_id: &str,
    folder_id: &str,
    seen_ids: &HashSet<String>,
    sync_window_days: u32,
) -> Result<Vec<String>> {
    let cutoff = (sync_window_days > 0)
        .then(|| Utc::now() - ChronoDuration::days(i64::from(sync_window_days).saturating_add(1)));
    let awaiting_append = pending_append_draft_ids(conn, account_id)?;
    let mut statement = conn.prepare(
        "SELECT id, gm_msgid, is_draft, message_id_header, date FROM message
         WHERE folder_id = ?1 AND uid IS NOT NULL",
    )?;
    let mut rows = statement.query(rusqlite::params![folder_id])?;
    let mut removed = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let remote_id: Option<String> = row.get(1)?;
        if remote_id
            .as_deref()
            .is_some_and(|remote| seen_ids.contains(remote))
        {
            continue;
        }
        if let Some(cutoff) = cutoff {
            let date: String = row.get(4)?;
            let in_window = DateTime::parse_from_rfc3339(&date)
                .map(|date| date.with_timezone(&Utc) >= cutoff)
                .unwrap_or(false);
            if !in_window {
                continue;
            }
        }
        let is_draft: i64 = row.get(2)?;
        let header: Option<String> = row.get(3)?;
        let awaiting = is_draft != 0
            && header
                .as_deref()
                .and_then(crate::util::draft_id_from_header)
                .is_some_and(|draft_id| awaiting_append.contains(draft_id));
        if !awaiting {
            removed.push(id);
        }
    }
    Ok(removed)
}

/// Draft ids whose server APPEND is still queued or running.
///
/// `compose_save` writes the projection row before it queues the APPEND, so a
/// pending operation is the durable marker that the row's missing (or
/// soon-to-be-replaced) server copy is temporary rather than a remote deletion
/// to mirror locally.
fn pending_append_draft_ids(
    conn: &rusqlite::Connection,
    account_id: &str,
) -> Result<HashSet<String>> {
    let mut result = HashSet::new();
    for op in repo::pending_ops::list(conn, Some(account_id))? {
        if op.kind != crate::ops::KIND_APPEND_DRAFT
            || !matches!(
                op.state,
                PendingOpState::Pending | PendingOpState::InProgress
            )
        {
            continue;
        }
        if let Ok(payload) = repo::pending_ops::payload::<crate::ops::AppendDraftOp>(&op) {
            result.insert(payload.draft_id);
        }
    }
    Ok(result)
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
                    apply_flag_updates(ctx, folder, &changes).await?;
                }
                Err(error) => {
                    warn!(%error, folder = %folder.remote_path, "CHANGEDSINCE failed; falling back to UID diff");
                    let changes = fetch_all_flags(conn, None).await?;
                    apply_flag_updates(ctx, folder, &changes).await?;
                }
            }
        }
    } else {
        let changes = fetch_all_flags(conn, None).await?;
        apply_flag_updates(ctx, folder, &changes).await?;
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

/// First UID range width tried by [`older_uid_batch`]; widened x4 while the
/// mailbox is sparse so a big folder never costs a full `UID SEARCH ALL`.
const OLDER_RANGE_START: u32 = 1000;

/// Load the next batch (up to [`BATCH_SIZE`]) of messages OLDER than anything
/// stored for an IMAP folder, ignoring the sync window.
///
/// The cursor is derived, not stored: it is the minimum UID already in the
/// local database for the folder (or the server `UIDNEXT` when the window
/// excluded everything). Rows go through the same `fetch_metadata` /
/// `persist_metadata` pair as the initial sync (`BODY.PEEK`, so no message is
/// ever marked read). Folder sync flags (UIDVALIDITY/UIDNEXT/MODSEQ) are not
/// touched, so the incremental sync is unaffected.
pub async fn load_older_imap(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
) -> Result<nuncio_core::models::LoadOlderResult> {
    use nuncio_core::models::LoadOlderResult;
    if !folder.is_selectable {
        return Ok(LoadOlderResult::default());
    }
    let (stored_validity, stored_uidnext, _) = ctx
        .db
        .read({
            let folder_id = folder.id.clone();
            move |db| repo::folders::sync_flags(db, &folder_id)
        })
        .await?;
    let mailbox = conn.select(&folder.remote_path).await.map_err(|error| {
        if error.code() == "protocol" {
            AppError::not_found(format!("mailbox {} is not available", folder.remote_path))
        } else {
            error
        }
    })?;
    let current_validity = mailbox.uid_validity.map(|value| value as i64);
    if let (Some(stored), Some(current)) = (stored_validity, current_validity) {
        if stored != current {
            // Nothing is cleared here: the next regular sync owns that reset.
            return Err(AppError::conflict(
                "UIDVALIDITY changed on the server; sync the folder before loading older mail",
            ));
        }
    }

    let min_uid: Option<i64> = ctx
        .db
        .read({
            let folder_id = folder.id.clone();
            move |db| {
                Ok(db.query_row(
                    "SELECT MIN(uid) FROM message WHERE folder_id = ?1 AND uid IS NOT NULL",
                    rusqlite::params![folder_id],
                    |row| row.get::<_, Option<i64>>(0),
                )?)
            }
        })
        .await?;
    // Exclusive upper bound: everything strictly below it is "older".
    // The cursor is the lowest UID we already hold; it needs no stored UIDNEXT
    // (163 does not report one). With nothing stored the server's UIDNEXT, or a
    // previously stored one, bounds the first batch; with none of those the
    // folder simply has not been synced.
    let Some(upper) = min_uid.or(mailbox.uid_next.map(|value| value as i64)).or(stored_uidnext)
    else {
        return Err(AppError::conflict(
            "the folder has not been synced yet; sync it before loading older mail",
        ));
    };
    if upper <= 1 {
        return Ok(LoadOlderResult::default());
    }
    let upper = upper.min(i64::from(u32::MAX)) as u32;

    let (uids, has_more) = older_uid_batch(conn, upper - 1).await?;
    if uids.is_empty() {
        return Ok(LoadOlderResult {
            loaded: 0,
            has_more: false,
        });
    }
    let metas = fetch_metadata(conn, account, folder, current_validity, &uids).await?;
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
    let folder_id = folder.id.clone();
    ctx.db
        .write(move |db| repo::folders::recompute_counts(db, &folder_id))
        .await?;
    debug!(
        folder = %folder.remote_path,
        loaded = new.len(),
        has_more,
        "older mail loaded"
    );
    Ok(LoadOlderResult {
        loaded: new.len() as u32,
        has_more,
    })
}

/// Up to [`BATCH_SIZE`] UIDs at or below `hi`, newest first, plus whether more
/// exist below them. Uses bounded `UID SEARCH UID lo:hi` ranges that widen x4
/// until the batch is full (one surplus UID proves `has_more`) or UID 1 is
/// reached (an exhaustive answer), so exhaustion is exact without ever
/// listing a large mailbox.
async fn older_uid_batch(conn: &mut ImapConnection, hi: u32) -> Result<(Vec<u32>, bool)> {
    let mut width = OLDER_RANGE_START;
    loop {
        let lo = hi.saturating_sub(width - 1).max(1);
        let query = format!("UID {lo}:{hi}");
        let session = &mut conn.session;
        let mut found: Vec<u32> = tokio::time::timeout(FETCH_TIMEOUT, session.uid_search(query))
            .await
            .map_err(|_| AppError::network("UID SEARCH timed out"))?
            .map(|set| set.into_iter().collect())
            .map_err(imap_error)?;
        if found.len() > BATCH_SIZE || lo == 1 {
            found.sort_unstable_by(|a, b| b.cmp(a));
            let has_more = found.len() > BATCH_SIZE;
            found.truncate(BATCH_SIZE);
            return Ok((found, has_more));
        }
        width = width.saturating_mul(4);
    }
}

/// Microsoft Graph counterpart of [`load_older_imap`]: one batch of messages
/// received at or before the oldest locally stored one, persisted through the
/// same writer as [`sync_folder_graph`].
pub async fn load_older_graph(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    folder: &Folder,
) -> Result<nuncio_core::models::LoadOlderResult> {
    load_older_graph_with(ctx, &crate::graph::GraphMailClient::new(), account, folder).await
}

/// [`load_older_graph`] with an explicit client (loopback mock in tests).
pub async fn load_older_graph_with(
    ctx: &CoreContext,
    client: &crate::graph::GraphMailClient,
    account: &nuncio_core::models::Account,
    folder: &Folder,
) -> Result<nuncio_core::models::LoadOlderResult> {
    use nuncio_core::models::LoadOlderResult;
    if !folder.is_selectable {
        return Ok(LoadOlderResult::default());
    }
    let graph_path = crate::sync::folders::graph_folder_id(ctx, &account.id, &folder.remote_path)
        .await?
        .ok_or_else(|| {
            AppError::not_found(format!(
                "Graph folder id is unknown for {}; refresh folders first",
                folder.remote_path
            ))
        })?;
    let mut graph_folder = folder.clone();
    graph_folder.remote_path = graph_path;

    let oldest: Option<String> = ctx
        .db
        .read({
            let folder_id = folder.id.clone();
            move |db| {
                Ok(db
                    .query_row(
                        "SELECT date FROM message WHERE folder_id = ?1 AND uid IS NOT NULL
                         ORDER BY date ASC LIMIT 1",
                        rusqlite::params![folder_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?)
            }
        })
        .await?;
    let before = oldest
        .and_then(|date| DateTime::parse_from_rfc3339(&date).ok())
        .map(|date| date.with_timezone(&Utc));

    let page =
        crate::graph::messages::list_older(client, ctx, &account.id, &graph_folder, before)
            .await?;
    let has_more = page.has_more;
    let delta = crate::graph::messages::MessageDelta {
        messages: page.messages,
        delta_link: None,
        full_sync: false,
        removed_remote_ids: Vec::new(),
    };
    let outcome = apply_graph_delta(ctx, account, folder, delta).await?;
    Ok(LoadOlderResult {
        loaded: outcome.new_messages.len() as u32,
        has_more,
    })
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

/// Store the server's flags for the messages that changed and announce them.
///
/// Flags are changed by other mail clients too, so the local cache update is
/// invisible to the UI unless it is published: `unread_count` is derived from
/// these rows, hence the recompute inside the write before the event goes out.
async fn apply_flag_updates(
    ctx: &CoreContext,
    folder: &Folder,
    changes: &[FlagChange],
) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let folder_id = folder.id.clone();
    let changes: Vec<(i64, FlagSnapshot)> = changes
        .iter()
        .map(|change| (change.uid, change.flags))
        .collect();
    let updated = ctx
        .db
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
            if !updated.is_empty() {
                // The unread/total counts are snapshots of the message rows, so
                // they have to be consistent before the UI reacts to the event.
                repo::folders::recompute_counts(db, &folder_id)?;
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
    Ok(())
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
            let entry = groups
                .entry(part.to_string())
                .or_insert_with(|| (Vec::new(), HashMap::new()));
            entry.0.push(uid);
            entry.1.insert(
                uid,
                (
                    meta.text_part_encoding.clone(),
                    meta.text_part_charset.clone(),
                ),
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
    cache_hits(ctx, conn, account, folder, uidvalidity, found).await
}

/// Fetch and persist metadata for server hits that are not stored locally and
/// return the local ids of every hit.
async fn cache_hits(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    uidvalidity: Option<i64>,
    found: Vec<u32>,
) -> Result<Vec<String>> {
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

/// Newest-first cap on how many message headers a fallback scan reads. 5000
/// is 25 round trips of [`BATCH_SIZE`] and roughly 5 MB of header text: a
/// few seconds on a home connection while still covering several years of a
/// typical personal mailbox. Larger folders report `scanned < total`.
pub const SCAN_CAP: usize = 5000;

/// Header fields requested by the fallback scan.
const SCAN_FIELDS: &str = "(UID BODY.PEEK[HEADER.FIELDS (FROM TO CC SUBJECT DATE MESSAGE-ID)])";

/// Outcome of [`server_scan`].
#[derive(Debug, Clone, Default)]
pub struct ScanOutcome {
    /// Local ids of every matched message (persisted when not yet stored).
    pub ids: Vec<String>,
    /// Headers actually read.
    pub scanned: u32,
    /// Candidate messages the server reported (before the cap).
    pub total: u32,
}

/// Terms used by the header matcher of the fallback scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanTerms {
    /// Lower-cased free-text terms; every term must hit some header field.
    pub text: Vec<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub subject: Option<String>,
}

impl ScanTerms {
    pub fn from_query(query: &nuncio_core::models::SearchQuery) -> Self {
        let clean = |value: &Option<String>| {
            value
                .as_deref()
                .map(|value| value.trim().to_lowercase())
                .filter(|value| !value.is_empty())
        };
        Self {
            text: query
                .text
                .split_whitespace()
                .map(|term| term.trim_matches('"').to_lowercase())
                .filter(|term| !term.is_empty())
                .collect(),
            from: clean(&query.from),
            to: clean(&query.to),
            subject: clean(&query.subject),
        }
    }

    /// True when there is anything a header scan can match on.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.from.is_none() && self.to.is_none() && self.subject.is_none()
    }
}

/// Structural-only criteria for the scan: the keys every server answers
/// correctly (`ALL`, `UNSEEN`, `FLAGGED`, `SINCE`, `BEFORE`). Content keys are
/// matched locally against the fetched headers.
pub fn imap_scan_criteria(query: &nuncio_core::models::SearchQuery) -> String {
    let mut structural = query.clone();
    structural.text.clear();
    structural.from = None;
    structural.to = None;
    structural.subject = None;
    imap_search_criteria(&structural).unwrap_or_else(|| "ALL".to_string())
}

/// Case-insensitive substring match of `terms` against a raw header block.
/// RFC 2047 words are decoded first so CJK subjects and display names match.
pub fn header_matches(raw_headers: &[u8], terms: &ScanTerms) -> bool {
    let Ok(headers) = nuncio_mime::parse_headers_only(raw_headers) else {
        return false;
    };
    let addresses = |list: &[nuncio_core::models::Address]| {
        list.iter()
            .map(|address| format!("{} {}", address.name.as_deref().unwrap_or(""), address.email))
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    let subject = headers.subject.to_lowercase();
    let from = headers
        .from
        .as_ref()
        .map(|address| addresses(std::slice::from_ref(address)))
        .unwrap_or_default();
    let to = addresses(&headers.to);
    let cc = addresses(&headers.cc);
    let message_id = headers.message_id.clone().unwrap_or_default().to_lowercase();
    if let Some(wanted) = &terms.from {
        if !from.contains(wanted.as_str()) {
            return false;
        }
    }
    if let Some(wanted) = &terms.to {
        if !to.contains(wanted.as_str()) && !cc.contains(wanted.as_str()) {
            return false;
        }
    }
    if let Some(wanted) = &terms.subject {
        if !subject.contains(wanted.as_str()) {
            return false;
        }
    }
    terms.text.iter().all(|term| {
        [&subject, &from, &to, &cc, &message_id]
            .iter()
            .any(|field| field.contains(term.as_str()))
    })
}

/// Fallback for servers whose content `SEARCH` is broken (NetEase, QQ):
/// `EXAMINE` the folder read-only, `UID SEARCH` structural criteria, then read
/// headers newest-first in [`BATCH_SIZE`] UID batches and match locally.
/// Matches that are not stored yet are cached through the same path as
/// [`server_search`]. `cancel` is checked between batches.
pub async fn server_scan(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    query: &nuncio_core::models::SearchQuery,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<ScanOutcome> {
    server_scan_with_cap(ctx, conn, account, folder, query, cancel, SCAN_CAP).await
}

/// [`server_scan`] with an explicit newest-first cap (tests use a tiny one).
pub async fn server_scan_with_cap(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    query: &nuncio_core::models::SearchQuery,
    cancel: &std::sync::atomic::AtomicBool,
    cap: usize,
) -> Result<ScanOutcome> {
    use std::sync::atomic::Ordering;
    let terms = ScanTerms::from_query(query);
    if terms.is_empty() {
        return Ok(ScanOutcome::default());
    }
    let mailbox = {
        let session = &mut conn.session;
        crate::util::with_timeout(crate::imap::conn::COMMAND_TIMEOUT, "EXAMINE", async move {
            session.examine(&folder.remote_path).await.map_err(imap_error)
        })
        .await?
    };
    let uidvalidity = mailbox.uid_validity.map(|value| value as i64);
    let mut uids = run_search(conn, &imap_scan_criteria(query)).await?;
    uids.sort_unstable_by(|a, b| b.cmp(a));
    let total = uids.len() as u32;
    uids.truncate(cap);

    let mut matched: Vec<u32> = Vec::new();
    let mut scanned = 0u32;
    for chunk in uids.chunks(BATCH_SIZE) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let set = uid_set(&chunk.iter().map(|uid| *uid as i64).collect::<Vec<_>>());
        let session = &mut conn.session;
        let fetches = tokio::time::timeout(FETCH_TIMEOUT, async {
            let stream = session
                .uid_fetch(set, SCAN_FIELDS)
                .await
                .map_err(imap_error)?;
            stream.try_collect::<Vec<Fetch>>().await.map_err(imap_error)
        })
        .await
        .map_err(|_| AppError::network("header scan timed out"))??;
        scanned += chunk.len() as u32;
        for fetch in &fetches {
            let (Some(uid), Some(raw)) = (
                fetch.uid,
                fetch.section(&SectionPath::Full(MessageSection::Header)),
            ) else {
                continue;
            };
            if header_matches(raw, &terms) {
                matched.push(uid);
            }
        }
    }
    matched.sort_unstable_by(|a, b| b.cmp(a));
    // Caching needs a read-write view only for local state; FETCH of metadata
    // is read-only on the wire, so the EXAMINE selection stays valid.
    let ids = cache_hits(ctx, conn, account, folder, uidvalidity, matched).await?;
    Ok(ScanOutcome {
        ids,
        scanned,
        total,
    })
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
    fn header_matcher_handles_ascii_cjk_and_encoded_words() {
        let raw = "From: =?UTF-8?B?5byg5LiJ?= <zhang@example.com>\r\nTo: me@example.com\r\n\
                   Subject: =?UTF-8?B?5Lya6K6u57qq6KaB?= Re: Budget\r\nMessage-ID: <abc@x>\r\n\r\n";
        let terms = |text: &str| ScanTerms {
            text: text.split_whitespace().map(str::to_lowercase).collect(),
            ..ScanTerms::default()
        };
        assert!(header_matches(raw.as_bytes(), &terms("budget")));
        assert!(header_matches(raw.as_bytes(), &terms("BUDGET re:")));
        assert!(header_matches(raw.as_bytes(), &terms("会议纪要")));
        assert!(header_matches(raw.as_bytes(), &terms("张三")));
        assert!(header_matches(raw.as_bytes(), &terms("zhang@example")));
        assert!(!header_matches(raw.as_bytes(), &terms("budget missing")));
        let subject_only = ScanTerms {
            subject: Some("zhang".into()),
            ..ScanTerms::default()
        };
        assert!(!header_matches(raw.as_bytes(), &subject_only));
        assert!(!header_matches(b"", &terms("x")));
    }

    #[test]
    fn scan_criteria_keep_only_structural_keys() {
        let mut query = nuncio_core::models::SearchQuery::text("会议");
        query.from = Some("a".into());
        assert_eq!(imap_scan_criteria(&query), "ALL");
        query.unread = Some(true);
        query.date_from = Some("2026-11-01".into());
        assert_eq!(imap_scan_criteria(&query), "UNSEEN SINCE 01-Nov-2026");
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

    #[test]
    fn synthetic_uids_are_stable_positive_and_distinct() {
        let id = "AAMkAD/abc== +x";
        let first = synthetic_uid(id);
        assert!(first > 0, "a synthetic UID must look like a real UID");
        assert_eq!(
            first,
            synthetic_uid(id),
            "the same Graph id must map stably"
        );
        assert_ne!(first, synthetic_uid("AAMkAD/other"));
    }

    #[test]
    fn graph_resync_keeps_local_answered_and_forwarded_flags() {
        let db = nuncio_core::db::Db::open_in_memory().expect("in-memory database");
        db.migrate_core().expect("core migrations");
        let now = nuncio_core::models::now().to_rfc3339();

        db.write_sync(|conn| {
            conn.execute(
                "INSERT INTO account (id, provider, email, auth_kind, created_at, updated_at)
                 VALUES ('acc', 'outlook', 'me@example.com', 'oauth', ?1, ?1)",
                rusqlite::params![now],
            )?;
            conn.execute(
                "INSERT INTO folder (id, account_id, remote_path, name, role, created_at, updated_at)
                 VALUES ('fld', 'acc', 'Inbox', 'Inbox', 'inbox', ?1, ?1)",
                rusqlite::params![now],
            )?;

            // A row exactly as the Graph mapper builds it: answered/forwarded
            // default to false because Graph has no such flags.
            let graph_row = |id: &str| {
                let summary = nuncio_core::models::MessageSummary::new(
                    id,
                    "acc",
                    "fld",
                    "thread",
                    nuncio_core::models::now(),
                );
                nuncio_core::models::StoredMessage::new(summary)
            };

            // First sync inserts the row; the user then replies and forwards,
            // which sets the local-only flags.
            let mut first = graph_row("graph-1");
            first.summary.answered = true;
            first.summary.forwarded = true;
            repo::messages::upsert(conn, &first)?;

            // Next delta pass maps the same Graph message back to false.
            let mut resynced = graph_row("graph-1");
            assert!(!resynced.summary.answered && !resynced.summary.forwarded);
            assert!(preserve_local_flags(conn, &mut resynced)?);
            repo::messages::upsert(conn, &resynced)?;

            let (answered, forwarded): (i64, i64) = conn.query_row(
                "SELECT answered, forwarded FROM message WHERE id = 'graph-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert_eq!((answered, forwarded), (1, 1));

            // A genuinely new message is not reported as existing and keeps
            // the mapped values.
            let mut fresh = graph_row("graph-2");
            assert!(!preserve_local_flags(conn, &mut fresh)?);
            assert!(!fresh.summary.answered && !fresh.summary.forwarded);
            Ok(())
        })
        .expect("seed and re-sync");
    }

    #[tokio::test]
    async fn server_flag_changes_emit_updated_event_and_refresh_counts() {
        use nuncio_core::traits::testing::{
            NullCredentialProvider, NullNotifier, RecordingEventSink,
        };

        let sink = std::sync::Arc::new(RecordingEventSink::new());
        let ctx = nuncio_core::CoreContext::new(
            nuncio_core::db::Db::open_in_memory().expect("in-memory database"),
            nuncio_core::Paths::new(std::env::temp_dir().join(nuncio_core::models::new_id())),
            sink.clone(),
            std::sync::Arc::new(NullCredentialProvider),
            std::sync::Arc::new(NullNotifier),
        )
        .await
        .expect("core context");

        ctx.db
            .write(|db| {
                seed_graph_folder(db);
                insert_message(db, "m1", Some(1), None, false, None);
                insert_message(db, "m2", Some(2), None, false, None);
                // m1 was unread locally; another client has now read it.
                db.execute("UPDATE message SET seen = 0 WHERE id = 'm1'", [])?;
                Ok(())
            })
            .await
            .expect("seed");

        let folder = ctx
            .db
            .read(|db| repo::folders::get_required(db, "fld"))
            .await
            .expect("folder");

        let read = FlagSnapshot {
            seen: true,
            ..FlagSnapshot::default()
        };
        let changes = vec![
            FlagChange {
                uid: 1,
                flags: read,
            },
            // Already stored exactly like this: it must not be reported.
            FlagChange {
                uid: 2,
                flags: read,
            },
        ];
        apply_flag_updates(&ctx, &folder, &changes)
            .await
            .expect("apply flags");

        let seen: i64 = ctx
            .db
            .read(|db| {
                Ok(
                    db.query_row("SELECT seen FROM message WHERE id = 'm1'", [], |row| {
                        row.get(0)
                    })?,
                )
            })
            .await
            .expect("seen flag");
        assert_eq!(seen, 1, "the server flag is stored");

        let (event, payload) = sink.last().expect("a change event");
        assert_eq!(event, events::MAIL_CHANGED);
        assert_eq!(payload["accountId"], "acc");
        assert_eq!(payload["folderIds"], serde_json::json!(["fld"]));
        assert_eq!(payload["messageIds"], serde_json::json!(["m1"]));
        assert_eq!(payload["kind"], "updated");

        let folder = ctx
            .db
            .read(|db| repo::folders::get_required(db, "fld"))
            .await
            .expect("folder");
        assert_eq!(folder.unread_count, 0, "both rows are read now");
        assert_eq!(
            folder.total_count, 2,
            "counts are recomputed before the event"
        );

        // No changes, no event.
        let before = sink.len();
        apply_flag_updates(&ctx, &folder, &[])
            .await
            .expect("no changes");
        assert_eq!(sink.len(), before);
    }

    /// Seed the account/folder Graph sync tests write into.
    fn seed_graph_folder(conn: &rusqlite::Connection) {
        let now = nuncio_core::models::now().to_rfc3339();
        conn.execute(
            "INSERT INTO account (id, provider, email, auth_kind, created_at, updated_at)
             VALUES ('acc', 'outlook', 'me@example.com', 'oauth', ?1, ?1)",
            rusqlite::params![now],
        )
        .expect("account");
        conn.execute(
            "INSERT INTO folder (id, account_id, remote_path, name, role, created_at, updated_at)
             VALUES ('fld', 'acc', 'Inbox', 'Inbox', 'inbox', ?1, ?1)",
            rusqlite::params![now],
        )
        .expect("folder");
    }

    /// Insert a `message` row with only the columns the prune reads.
    fn insert_message(
        conn: &rusqlite::Connection,
        id: &str,
        uid: Option<i64>,
        gm_msgid: Option<&str>,
        is_draft: bool,
        header: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO message (
                 id, account_id, folder_id, uid, gm_msgid, message_id_header, thread_id,
                 subject, date, preview, seen, flagged, answered, forwarded, is_draft,
                 has_attachments, importance, size, created_at, updated_at
             ) VALUES (?1, 'acc', 'fld', ?2, ?3, ?4, 'thread', 'subject', ?5, '', 1, 0, 0, 0,
                       ?6, 0, 'normal', 0, ?5, ?5)",
            rusqlite::params![
                id,
                uid,
                gm_msgid,
                header,
                nuncio_core::models::now().to_rfc3339(),
                is_draft as i64,
            ],
        )
        .expect("insert message");
    }

    #[test]
    fn graph_prune_preserves_draft_projections() {
        let db = nuncio_core::db::Db::open_in_memory().expect("in-memory database");
        db.migrate_core().expect("core migrations");

        db.write_sync(|conn| {
            seed_graph_folder(conn);
            // Never synced: the composer's projection for a draft whose APPEND
            // has not run yet.
            insert_message(
                conn,
                "draft-local",
                None,
                None,
                true,
                Some("nuncio-draft-abc@nuncio.local"),
            );
            // Already synced once and now being re-APPENDed: its old Graph id
            // is gone from the server, but that is expected.
            insert_message(
                conn,
                "draft-resynced",
                Some(synthetic_uid("draft-resynced")),
                Some("graph-old-draft"),
                true,
                Some("nuncio-draft-def@nuncio.local"),
            );
            repo::pending_ops::enqueue(
                conn,
                nuncio_core::models::NewPendingOp::new(
                    "acc",
                    crate::ops::KIND_APPEND_DRAFT,
                    serde_json::json!({
                        "draftId": "def",
                        "folderId": "fld",
                        "rfc822Path": "/tmp/draft.eml"
                    }),
                    "Save draft to server",
                ),
            )?;

            // The scan returned a different message; neither draft may be
            // pruned.
            let seen: HashSet<String> = ["graph-live".to_string()].into_iter().collect();
            let removed = graph_prune_ids(conn, "acc", "fld", &seen, 0)?;
            assert!(
                removed.is_empty(),
                "draft projections must survive a full scan: {removed:?}"
            );
            Ok(())
        })
        .expect("prune drafts");
    }

    #[test]
    fn graph_prune_removes_imap_era_rows() {
        let db = nuncio_core::db::Db::open_in_memory().expect("in-memory database");
        db.migrate_core().expect("core migrations");

        db.write_sync(|conn| {
            seed_graph_folder(conn);
            // A row synced over IMAP before the switch: real UID, no Graph id,
            // so it can never appear in the Graph scan.
            insert_message(
                conn,
                "imap-era",
                Some(42),
                None,
                false,
                Some("<old@example.com>"),
            );

            let seen: HashSet<String> = ["graph-live".to_string()].into_iter().collect();
            let removed = graph_prune_ids(conn, "acc", "fld", &seen, 0)?;
            assert_eq!(removed, vec!["imap-era".to_string()]);
            Ok(())
        })
        .expect("prune imap era row");
    }

    #[test]
    fn graph_prune_keeps_rows_the_scan_returned() {
        let db = nuncio_core::db::Db::open_in_memory().expect("in-memory database");
        db.migrate_core().expect("core migrations");

        db.write_sync(|conn| {
            seed_graph_folder(conn);
            insert_message(
                conn,
                "graph-live",
                Some(synthetic_uid("graph-live")),
                Some("graph-live"),
                false,
                Some("<live@example.com>"),
            );

            let seen: HashSet<String> = ["graph-live".to_string()].into_iter().collect();
            let removed = graph_prune_ids(conn, "acc", "fld", &seen, 0)?;
            assert!(
                removed.is_empty(),
                "a message the scan returned must be kept: {removed:?}"
            );
            Ok(())
        })
        .expect("keep live graph row");
    }

    #[test]
    fn graph_sync_reuses_the_row_with_the_same_message_id() {
        let db = nuncio_core::db::Db::open_in_memory().expect("in-memory database");
        db.migrate_core().expect("core migrations");

        db.write_sync(|conn| {
            seed_graph_folder(conn);
            insert_message(
                conn,
                "draft-local",
                None,
                None,
                true,
                Some("nuncio-draft-abc@nuncio.local"),
            );

            assert_eq!(
                local_id_for_message_id(conn, "acc", "fld", "nuncio-draft-abc@nuncio.local")?,
                Some("draft-local".to_string()),
                "the APPEND echo must update the composer's projection row"
            );
            assert_eq!(
                local_id_for_message_id(conn, "acc", "fld", "<unknown@example.com>")?,
                None
            );
            Ok(())
        })
        .expect("reuse message id");
    }

    /// The Graph mapper must hand the reuse lookup a bare Message-ID.
    ///
    /// Graph returns `internetMessageId` wrapped in angle brackets while the
    /// composer's projection row is written with
    /// `nuncio-draft-<uuid>@nuncio.local`. The lookup is
    /// `WHERE message_id_header = ?`, an exact string comparison, so a
    /// bracketed value from `map_message` never matches: the APPEND echo would
    /// be inserted as a second row for the same draft. This drives the real
    /// mapper with a Graph-shaped payload and asserts the reuse actually fires.
    #[test]
    fn graph_echo_with_bracketed_message_id_reuses_the_draft_projection() {
        let db = nuncio_core::db::Db::open_in_memory().expect("in-memory database");
        db.migrate_core().expect("core migrations");

        db.write_sync(|conn| {
            seed_graph_folder(conn);
            // The projection `compose_save` writes before queuing the APPEND.
            insert_message(
                conn,
                "draft-local",
                None,
                None,
                true,
                Some("nuncio-draft-abc@nuncio.local"),
            );

            // The server copy of that draft, as Graph reports it.
            let echoed = crate::graph::messages::map_graph_json(
                serde_json::json!({
                    "id": "AAMkAD-draft-server-id",
                    "subject": "Draft subject",
                    "isDraft": true,
                    "receivedDateTime": "2026-11-16T08:00:00Z",
                    "internetMessageId": "<nuncio-draft-abc@nuncio.local>",
                }),
                "acc",
                "fld",
            )
            .expect("mapped");

            let header = echoed
                .message_id_header
                .as_deref()
                .expect("a draft always carries a Message-ID");
            assert_eq!(header, "nuncio-draft-abc@nuncio.local");
            // The second line of defence: recovering the local draft id from
            // the header only works on the bare form.
            assert_eq!(crate::util::draft_id_from_header(header), Some("abc"));

            let reused = local_id_for_message_id(conn, "acc", "fld", header)?
                .expect("the APPEND echo must match the projection row");
            assert_eq!(reused, "draft-local");

            // Apply the same id swap the sync performs and check no second row
            // appears.
            let mut adopted = echoed;
            adopted.summary.id = reused;
            adopted.uid = Some(synthetic_uid(&adopted.summary.id));
            repo::messages::upsert(conn, &adopted)?;
            let rows: i64 = conn.query_row(
                "SELECT COUNT(*) FROM message WHERE message_id_header = ?1",
                rusqlite::params!["nuncio-draft-abc@nuncio.local"],
                |row| row.get(0),
            )?;
            assert_eq!(rows, 1, "one draft must stay one row");

            // Ordinary (non-draft) messages are normalized the same way, so a
            // re-sync after a Graph id change also lands on the existing row.
            insert_message(
                conn,
                "mail-local",
                None,
                None,
                false,
                Some("abc@example.com"),
            );
            let mailed = crate::graph::messages::map_graph_json(
                serde_json::json!({
                    "id": "AAMkAD-mail-server-id",
                    "receivedDateTime": "2026-11-16T08:00:00Z",
                    "internetMessageId": "<abc@example.com>",
                }),
                "acc",
                "fld",
            )
            .expect("mapped");
            assert_eq!(
                local_id_for_message_id(
                    conn,
                    "acc",
                    "fld",
                    mailed.message_id_header.as_deref().expect("header"),
                )?,
                Some("mail-local".to_string())
            );
            Ok(())
        })
        .expect("reuse the bracketed graph message id");
    }
}
