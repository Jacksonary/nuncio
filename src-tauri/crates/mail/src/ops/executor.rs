//! Execution of due `pending_op` rows on the account's worker connection.
//!
//! Operations are executed in creation order. Consecutive flag updates with
//! identical flags on the same folder are merged into a single `UID STORE`.
//! Failures retry with exponential backoff; permanent failures are reported via
//! `op://failed` and the affected folders are re-synchronized so local state
//! rolls back to the server's truth.

use std::collections::HashSet;
use std::time::Duration;

use chrono::Utc;
use futures::TryStreamExt;
use nuncio_core::models::{Account, Folder, FolderRole, PendingOp, PendingOpState};
use nuncio_core::traits::TokenResource;
use nuncio_core::{events, repo, AppError, CoreContext, Result};
use rusqlite::OptionalExtension;
use tracing::{debug, info};

use crate::graph::{ops as graph_ops, GraphMailClient};
use crate::imap::conn::{ImapConnection, COMMAND_TIMEOUT};
use crate::ops::{
    is_mail_kind, AppendDraftOp, AppendToSentOp, ArchiveOp, DeleteOp, DeleteRemoteDraftOp,
    FolderCreateOp, FolderDeleteOp, FolderMarkReadOp, FolderRenameOp, FolderSubscribeOp,
    MarkJunkOp, MoveOp, SendOp, SetFlagsOp, KIND_APPEND_DRAFT, KIND_APPEND_TO_SENT, KIND_ARCHIVE,
    KIND_DELETE, KIND_DELETE_REMOTE_DRAFT, KIND_FOLDER_CREATE, KIND_FOLDER_DELETE,
    KIND_FOLDER_MARK_READ, KIND_FOLDER_RENAME, KIND_FOLDER_SUBSCRIBE, KIND_MARK_JUNK, KIND_MOVE,
    KIND_SEND, KIND_SET_FLAGS,
};
use crate::util::{imap_error, uid_set};

const MAX_ATTEMPTS: u32 = 8;
const MAX_OPS_PER_PASS: usize = 64;

#[derive(Debug, Default)]
pub struct ExecOutcome {
    pub done: usize,
    pub retried: usize,
    pub failed: usize,
    /// Folders whose local state must be re-synced after move/archive.
    pub resync_folders: HashSet<String>,
}

/// Reset operations abandoned by a crash/forced shutdown so they retry.
async fn recover_in_progress(ctx: &CoreContext, account_id: &str) -> Result<()> {
    let account_id = account_id.to_string();
    let cutoff = Utc::now() - chrono::Duration::minutes(10);
    ctx.db
        .write(move |db| {
            // PIM operations share this table but are owned by their own
            // workers; resetting one mid-flight would let the mail worker claim
            // it on the next pass.
            db.execute(
                "UPDATE pending_op SET state = 'pending'
                 WHERE account_id = ?1 AND state = 'inProgress' AND updated_at < ?2
                   AND kind NOT LIKE 'pim.%'",
                rusqlite::params![account_id, nuncio_core::repo::dt_string(cutoff)],
            )?;
            Ok(())
        })
        .await
}

/// Execute due operations for one account.
pub async fn run_due(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &Account,
) -> Result<ExecOutcome> {
    recover_in_progress(ctx, &account.id).await?;
    let account_id = account.id.clone();
    let now = Utc::now();
    let ops = ctx
        .db
        .read({
            let account_id = account_id.clone();
            move |db| repo::pending_ops::list(db, Some(&account_id))
        })
        .await?;
    let due: Vec<PendingOp> = ops
        .into_iter()
        .filter(|op| {
            is_mail_kind(&op.kind)
                && op.state == PendingOpState::Pending
                && op.next_retry_at.map(|at| at <= now).unwrap_or(true)
        })
        .take(MAX_OPS_PER_PASS)
        .collect();
    if due.is_empty() {
        return Ok(ExecOutcome::default());
    }

    let mut outcome = ExecOutcome::default();
    let mut selected: Option<String> = None;
    let mut index = 0;
    while index < due.len() {
        let op = &due[index];
        if op.kind == KIND_SET_FLAGS {
            // Merge consecutive flag ops targeting the same folder and flags.
            let Ok(first) = repo::pending_ops::payload::<SetFlagsOp>(op) else {
                let _ =
                    fail_op(ctx, op, AppError::invalid_input("invalid setFlags payload")).await?;
                outcome.failed += 1;
                index += 1;
                continue;
            };
            let mut batch = vec![(op.clone(), first.clone())];
            let mut next = index + 1;
            while next < due.len() && due[next].kind == KIND_SET_FLAGS {
                let Ok(candidate) = repo::pending_ops::payload::<SetFlagsOp>(&due[next]) else {
                    break;
                };
                if candidate.folder_id == first.folder_id
                    && candidate.seen == first.seen
                    && candidate.flagged == first.flagged
                    && candidate.answered == first.answered
                    && candidate.forwarded == first.forwarded
                {
                    batch.push((due[next].clone(), candidate));
                    next += 1;
                } else {
                    break;
                }
            }
            let uids: Vec<i64> = batch
                .iter()
                .flat_map(|(_, payload)| payload.uids.iter().copied())
                .collect();
            let mut merged = first.clone();
            merged.uids = uids;
            merged.previous = batch
                .iter()
                .flat_map(|(_, payload)| payload.previous.iter().cloned())
                .collect();
            mark_in_progress(
                ctx,
                &batch
                    .iter()
                    .map(|(op, _)| op.id.clone())
                    .collect::<Vec<_>>(),
            )
            .await?;
            match execute_set_flags(ctx, conn, account, &mut selected, &merged).await {
                Ok(()) => {
                    mark_done(
                        ctx,
                        &batch
                            .iter()
                            .map(|(op, _)| op.id.clone())
                            .collect::<Vec<_>>(),
                    )
                    .await?;
                    outcome.done += batch.len();
                }
                Err(error) => {
                    for (op, _) in &batch {
                        outcome.resync_folders.extend(op_folders(op));
                        handle_failure(
                            ctx,
                            account,
                            op,
                            &error,
                            TokenResource::Mail,
                            &mut outcome,
                        )
                        .await?;
                    }
                }
            }
            index = next;
            continue;
        }

        let op = op.clone();
        mark_in_progress(ctx, std::slice::from_ref(&op.id)).await?;
        let result = execute_one(ctx, conn, account, &mut selected, &op).await;
        match result {
            Ok(()) => {
                mark_done(ctx, std::slice::from_ref(&op.id)).await?;
                outcome.done += 1;
                if op.kind == KIND_APPEND_TO_SENT {
                    if let Ok(payload) = repo::pending_ops::payload::<AppendToSentOp>(&op) {
                        let _ = tokio::fs::remove_file(&payload.rfc822_path).await;
                    }
                }
                if op.kind == KIND_APPEND_DRAFT {
                    if let Ok(payload) = repo::pending_ops::payload::<AppendDraftOp>(&op) {
                        outcome.resync_folders.insert(payload.folder_id);
                    }
                }
                if matches!(op.kind.as_str(), KIND_MOVE | KIND_ARCHIVE | KIND_MARK_JUNK) {
                    if let Ok(payload) = repo::pending_ops::payload::<MoveOp>(&op) {
                        outcome.resync_folders.insert(payload.target_folder_id);
                    }
                    if let Ok(payload) = repo::pending_ops::payload::<ArchiveOp>(&op) {
                        outcome.resync_folders.insert(payload.folder_id.clone());
                    }
                    if let Ok(payload) = repo::pending_ops::payload::<MarkJunkOp>(&op) {
                        outcome.resync_folders.insert(payload.folder_id.clone());
                    }
                }
            }
            Err(error) => {
                outcome.resync_folders.extend(op_folders(&op));
                handle_failure(
                    ctx,
                    account,
                    &op,
                    &error,
                    TokenResource::Mail,
                    &mut outcome,
                )
                .await?;
            }
        }
        index += 1;
    }

    if !outcome.resync_folders.is_empty() {
        debug!(
            folders = outcome.resync_folders.len(),
            "folders queued for resync after ops"
        );
    }
    Ok(outcome)
}

async fn execute_one(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &Account,
    selected: &mut Option<String>,
    op: &PendingOp,
) -> Result<()> {
    // Clear early dispatch: Microsoft accounts execute the same queued
    // operations through Graph.  The IMAP implementation below is untouched.
    if crate::graph::uses_graph(account) {
        return execute_graph_op(ctx, account, op).await;
    }
    if account.imap.is_none() {
        return apply_local_only(ctx, op).await;
    }
    match op.kind.as_str() {
        KIND_SET_FLAGS => {
            let payload: SetFlagsOp = repo::pending_ops::payload(op)?;
            execute_set_flags(ctx, conn, account, selected, &payload).await
        }
        KIND_MOVE => {
            let payload: MoveOp = repo::pending_ops::payload(op)?;
            if payload.folder_id == payload.target_folder_id {
                return Ok(());
            }
            execute_move(
                ctx,
                conn,
                account,
                selected,
                &payload.folder_id,
                &payload.target_folder_id,
                &payload.uids,
            )
            .await?;
            remove_local_messages(ctx, &payload.folder_id, &payload.uids).await
        }
        KIND_DELETE => {
            let payload: DeleteOp = repo::pending_ops::payload(op)?;
            execute_delete(ctx, conn, account, selected, &payload).await
        }
        KIND_ARCHIVE => {
            let payload: ArchiveOp = repo::pending_ops::payload(op)?;
            execute_archive(ctx, conn, account, selected, &payload).await
        }
        KIND_MARK_JUNK => {
            let payload: MarkJunkOp = repo::pending_ops::payload(op)?;
            execute_mark_junk(ctx, conn, account, selected, &payload).await
        }
        KIND_APPEND_DRAFT => {
            let payload: AppendDraftOp = repo::pending_ops::payload(op)?;
            execute_append(
                ctx,
                conn,
                selected,
                &payload.folder_id,
                &payload.rfc822_path,
                Some("(\\Draft)"),
            )
            .await
        }
        KIND_APPEND_TO_SENT => {
            let payload: AppendToSentOp = repo::pending_ops::payload(op)?;
            execute_append(
                ctx,
                conn,
                selected,
                &payload.folder_id,
                &payload.rfc822_path,
                Some("(\\Seen)"),
            )
            .await
        }
        KIND_DELETE_REMOTE_DRAFT => {
            let payload: DeleteRemoteDraftOp = repo::pending_ops::payload(op)?;
            execute_permanent_delete(ctx, conn, selected, &payload.folder_id, &[payload.uid]).await
        }
        KIND_FOLDER_CREATE => {
            let payload: FolderCreateOp = repo::pending_ops::payload(op)?;
            execute_folder_create(ctx, conn, account, &payload).await
        }
        KIND_FOLDER_RENAME => {
            let payload: FolderRenameOp = repo::pending_ops::payload(op)?;
            execute_folder_rename(ctx, conn, &payload).await
        }
        KIND_FOLDER_DELETE => {
            let payload: FolderDeleteOp = repo::pending_ops::payload(op)?;
            execute_folder_delete(ctx, conn, &payload).await
        }
        KIND_FOLDER_SUBSCRIBE => {
            let payload: FolderSubscribeOp = repo::pending_ops::payload(op)?;
            execute_folder_subscribe(ctx, conn, &payload).await
        }
        KIND_FOLDER_MARK_READ => {
            let payload: FolderMarkReadOp = repo::pending_ops::payload(op)?;
            execute_folder_mark_read(ctx, conn, selected, &payload).await
        }
        KIND_SEND => {
            let payload: SendOp = repo::pending_ops::payload(op)?;
            crate::smtp::outbox::process_one(ctx, account, &payload.outbox_id).await
        }
        other => Err(AppError::unsupported(format!(
            "unknown pending operation kind: {other}"
        ))),
    }
}

/* ───────────────────────── Graph operations ───────────────────────── */

/// Execute one queued operation through Microsoft Graph.
///
/// The queued payloads are provider-neutral, but Graph's data model differs
/// (no IMAP UIDs, new message ids after a move, `sendMail` files Sent copies
/// itself), so each kind is translated here.  Every call reads or writes the
/// same local rows the IMAP executor uses, which is what lets undo, resync and
/// failure rollback stay shared.
async fn execute_graph_op(ctx: &CoreContext, account: &Account, op: &PendingOp) -> Result<()> {
    let client = GraphMailClient::new();
    match op.kind.as_str() {
        KIND_SET_FLAGS => {
            let payload: SetFlagsOp = repo::pending_ops::payload(op)?;
            if payload.uids.is_empty() {
                return Ok(());
            }
            // Graph has no \Answered or $Forwarded message flag; an operation
            // that only touches those has nothing to send and completes.
            if payload.seen.is_none() && payload.flagged.is_none() {
                return Ok(());
            }
            for uid in &payload.uids {
                let remote = graph_remote_id(ctx, &account.id, &payload.folder_id, *uid).await?;
                graph_ops::set_flags(
                    &client,
                    ctx,
                    &account.id,
                    &remote,
                    payload.seen,
                    payload.flagged,
                )
                .await?;
            }
            Ok(())
        }
        KIND_MOVE => {
            let payload: MoveOp = repo::pending_ops::payload(op)?;
            let target = folder_by_id(ctx, &payload.target_folder_id).await?;
            graph_move_messages(
                ctx,
                account,
                &client,
                &payload.folder_id,
                &target,
                &payload.uids,
            )
            .await
        }
        KIND_DELETE => {
            let payload: DeleteOp = repo::pending_ops::payload(op)?;
            if !payload.permanent {
                let target = match payload.target_folder_id.as_deref() {
                    Some(id) => Some(folder_by_id(ctx, id).await?),
                    None => role_folder(ctx, &account.id, FolderRole::Trash).await?,
                };
                if let Some(target) = target {
                    let source = folder_by_id(ctx, &payload.folder_id).await?;
                    if source.id != target.id {
                        graph_move_messages(
                            ctx,
                            account,
                            &client,
                            &payload.folder_id,
                            &target,
                            &payload.uids,
                        )
                        .await?;
                        return Ok(());
                    }
                }
            }
            for uid in &payload.uids {
                let remote = graph_remote_id(ctx, &account.id, &payload.folder_id, *uid).await?;
                graph_ops::permanent_delete_message(&client, ctx, &account.id, &remote).await?;
            }
            let ids = local_message_ids(ctx, &payload.folder_id, &payload.uids).await?;
            if !ids.is_empty() {
                ctx.db
                    .write(move |db| repo::messages::delete_local(db, &ids, true))
                    .await?;
            }
            Ok(())
        }
        KIND_ARCHIVE => {
            let payload: ArchiveOp = repo::pending_ops::payload(op)?;
            let source = folder_by_id(ctx, &payload.folder_id).await?;
            if matches!(source.role, FolderRole::All | FolderRole::Archive) {
                return Ok(());
            }
            let target = match payload.target_folder_id.as_deref() {
                Some(id) => Some(folder_by_id(ctx, id).await?),
                None => role_folder(ctx, &account.id, FolderRole::Archive).await?,
            };
            let target = match target {
                Some(target) => target,
                None => {
                    // Graph exposes Archive as a normal folder on most
                    // mailboxes; create it when the account has none, exactly
                    // like the IMAP archive path.
                    graph_ops::create_folder(&client, ctx, &account.id, "Archive", None).await?;
                    crate::sync::folders::sync_graph(ctx, account).await?;
                    role_folder(ctx, &account.id, FolderRole::Archive)
                        .await?
                        .ok_or_else(|| {
                            AppError::protocol("Graph Archive folder could not be created")
                        })?
                }
            };
            graph_move_messages(
                ctx,
                account,
                &client,
                &payload.folder_id,
                &target,
                &payload.uids,
            )
            .await
        }
        KIND_MARK_JUNK => {
            let payload: MarkJunkOp = repo::pending_ops::payload(op)?;
            let role = if payload.value {
                FolderRole::Junk
            } else {
                FolderRole::Inbox
            };
            let target = match payload.target_folder_id.as_deref() {
                Some(id) => Some(folder_by_id(ctx, id).await?),
                None => role_folder(ctx, &account.id, role).await?,
            };
            let Some(target) = target else {
                return Err(AppError::unsupported(format!(
                    "account has no {} folder",
                    role.as_str()
                )));
            };
            graph_move_messages(
                ctx,
                account,
                &client,
                &payload.folder_id,
                &target,
                &payload.uids,
            )
            .await
        }
        KIND_APPEND_DRAFT => {
            let payload: AppendDraftOp = repo::pending_ops::payload(op)?;
            let bytes = read_staged_message(&payload.rfc822_path).await?;
            // Graph's MIME create endpoint files the draft in Drafts itself.
            graph_ops::append_draft(&client, ctx, &account.id, &bytes).await?;
            Ok(())
        }
        KIND_APPEND_TO_SENT => {
            // Graph saves the Sent copy as part of sendMail; appending again
            // would create a duplicate.
            Ok(())
        }
        KIND_DELETE_REMOTE_DRAFT => {
            let payload: DeleteRemoteDraftOp = repo::pending_ops::payload(op)?;
            match graph_remote_id_opt(ctx, &account.id, &payload.folder_id, payload.uid).await? {
                Some(remote) => graph_ops::delete_message(&client, ctx, &account.id, &remote).await,
                None => {
                    // The local draft row may already be gone (the composer
                    // removes it when the send supersedes the draft).  The
                    // deletion is idempotent, so treat a missing mapping as
                    // already deleted.
                    debug!(
                        folder = %payload.folder_id,
                        uid = payload.uid,
                        "no local row for the remote draft; nothing to delete"
                    );
                    Ok(())
                }
            }
        }
        KIND_FOLDER_CREATE => {
            let payload: FolderCreateOp = repo::pending_ops::payload(op)?;
            let parent_id = match payload.parent_id.as_deref() {
                Some(id) => {
                    let parent = folder_by_id(ctx, id).await?;
                    Some(graph_folder_remote_id(ctx, &account.id, &parent).await?)
                }
                None => None,
            };
            graph_ops::create_folder(
                &client,
                ctx,
                &account.id,
                &payload.name,
                parent_id.as_deref(),
            )
            .await?;
            crate::sync::folders::sync_graph(ctx, account).await?;
            Ok(())
        }
        KIND_FOLDER_RENAME => {
            let payload: FolderRenameOp = repo::pending_ops::payload(op)?;
            let folder = folder_by_id(ctx, &payload.folder_id).await?;
            // The server id does not change, but the local identity is the
            // readable path, so the rename must move that path too.  Otherwise
            // the next reconciliation would see a new path and replace the
            // folder (losing its local ids and flags).
            let folder_remote = graph_folder_remote_id(ctx, &account.id, &folder).await?;
            graph_ops::rename_folder(&client, ctx, &account.id, &folder_remote, &payload.new_name)
                .await?;
            let folder_id = folder.id.clone();
            let old_path = folder.remote_path.clone();
            let new_path = graph_renamed_path(&old_path, &payload.new_name);
            let name = payload.new_name;
            let mapping_account = account.id.clone();
            let old_prefix = format!("{old_path}/");
            let new_prefix = format!("{new_path}/");
            let account_for_query = account.id.clone();
            ctx.db
                .write(move |db| {
                    // Descendant folders are identified by their path prefix,
                    // so they must follow the renamed parent or the next
                    // reconciliation would recreate them under new ids.
                    let descendants: Vec<(String, String)> = {
                        let mut statement =
                            db.prepare("SELECT id, remote_path FROM folder WHERE account_id = ?1")?;
                        let rows = statement.query_map(
                            rusqlite::params![account_for_query],
                            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                        )?;
                        rows.collect::<rusqlite::Result<Vec<_>>>()?
                    };
                    let now = nuncio_core::repo::now_string();
                    for (id, path) in descendants {
                        let updated = if id == folder_id {
                            Some(new_path.clone())
                        } else {
                            path.strip_prefix(&old_prefix)
                                .map(|rest| format!("{new_prefix}{rest}"))
                        };
                        let Some(updated) = updated else { continue };
                        if id == folder_id {
                            db.execute(
                                "UPDATE folder SET remote_path = ?2, name = ?3, updated_at = ?4 WHERE id = ?1",
                                rusqlite::params![id, updated, name, now],
                            )?;
                        } else {
                            db.execute(
                                "UPDATE folder SET remote_path = ?2, updated_at = ?3 WHERE id = ?1",
                                rusqlite::params![id, updated, now],
                            )?;
                        }
                        // Move the provider-id mapping with the path.
                        let old_key = format!("graph_folder:{path}");
                        if let Some(provider_id) = repo::sync_state::get(
                            db,
                            &mapping_account,
                            "mail",
                            &old_key,
                        )? {
                            let new_key = format!("graph_folder:{updated}");
                            repo::sync_state::set(
                                db,
                                &mapping_account,
                                "mail",
                                &new_key,
                                &provider_id,
                            )?;
                            repo::sync_state::delete_key(db, &mapping_account, "mail", &old_key)?;
                        }
                    }
                    Ok(())
                })
                .await?;
            crate::sync::folders::sync_graph(ctx, account).await?;
            Ok(())
        }
        KIND_FOLDER_DELETE => {
            let payload: FolderDeleteOp = repo::pending_ops::payload(op)?;
            let folder = folder_by_id(ctx, &payload.folder_id).await?;
            let folder_remote = graph_folder_remote_id(ctx, &account.id, &folder).await?;
            graph_ops::delete_folder(&client, ctx, &account.id, &folder_remote).await?;
            let folder_id = folder.id.clone();
            ctx.db
                .write(move |db| repo::folders::delete(db, &folder_id))
                .await?;
            crate::sync::folders::sync_graph(ctx, account).await?;
            Ok(())
        }
        KIND_FOLDER_SUBSCRIBE => {
            // Graph folders have no subscription concept; completing the op is
            // the only meaningful outcome.
            Ok(())
        }
        KIND_FOLDER_MARK_READ => {
            let payload: FolderMarkReadOp = repo::pending_ops::payload(op)?;
            let folder = folder_by_id(ctx, &payload.folder_id).await?;
            let folder_remote = graph_folder_remote_id(ctx, &account.id, &folder).await?;
            graph_ops::mark_folder_read(&client, ctx, &account.id, &folder_remote).await?;
            let folder_id = folder.id.clone();
            ctx.db
                .write(move |db| {
                    repo::messages::mark_all_read(db, &folder_id)?;
                    Ok(())
                })
                .await
        }
        KIND_SEND => Err(AppError::unsupported(
            "queued sends are sent directly through Graph for Microsoft accounts",
        )),
        other => Err(AppError::unsupported(format!(
            "unknown pending operation kind: {other}"
        ))),
    }
}

/// Resolve the Graph message id behind a queued `(folder_id, uid)` pair.
async fn graph_remote_id(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    uid: i64,
) -> Result<String> {
    graph_remote_id_opt(ctx, account_id, folder_id, uid)
        .await?
        .ok_or_else(|| {
            AppError::not_found(format!("message uid {uid} not found in folder {folder_id}"))
        })
}

/// Look up the Graph id recorded when the message was synced.
///
/// The mapping is stored separately from the message row because a permanent
/// delete removes the row before the queued operation executes, and an
/// optimistic move changes the row's `folder_id`.  `sync_folder_graph` writes
/// `graph_remote:{folder_id}:{uid}` for every message, and a successful move
/// rewrites it under the destination folder.  When the requested folder has no
/// entry, the newest mapping for the uid wins before the row is consulted; the
/// row lookup only covers messages synced by a version that predates mappings.
async fn graph_remote_id_opt(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    uid: i64,
) -> Result<Option<String>> {
    let key = format!("graph_remote:{folder_id}:{uid}");
    let mapped_account = account_id.to_string();
    let mapped = {
        let key = key.clone();
        ctx.db
            .read(move |db| repo::sync_state::get(db, &mapped_account, "mail", &key))
            .await?
    };
    if let Some(remote) = mapped.filter(|value| !value.trim().is_empty()) {
        return Ok(Some(remote));
    }

    // A move rewrites the live mapping under the destination folder, so an
    // operation queued against the old folder can miss the exact key.  Prefer
    // the most recently written mapping for this uid over the row below: the
    // row can still carry the pre-move `gm_msgid`, and sending that id to
    // Graph answers 404 and rolls the operation back.  Synthetic uids are
    // derived from one Graph id, so a uid identifies one message account-wide.
    let pattern = format!("graph_remote:%:{uid}");
    let mapped_account = account_id.to_string();
    let freshest = ctx
        .db
        .read(move |db| {
            Ok(db
                .query_row(
                    "SELECT value FROM sync_state
                     WHERE account_id = ?1 AND module = 'mail' AND key LIKE ?2
                       AND value <> ''
                     ORDER BY updated_at DESC LIMIT 1",
                    rusqlite::params![mapped_account, pattern],
                    |row| row.get::<_, String>(0),
                )
                .optional()?)
        })
        .await?;
    if let Some(remote) = freshest {
        return Ok(Some(remote));
    }

    // Fallback: the optimistic action may have moved the row to another
    // folder, so search the whole account rather than the source folder.
    let account_id = account_id.to_string();
    ctx.db
        .read(move |db| {
            let row = db
                .query_row(
                    "SELECT gm_msgid, id FROM message
                     WHERE account_id = ?1 AND uid = ?2 LIMIT 1",
                    rusqlite::params![account_id, uid],
                    |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            Ok(row.map(|(gm_msgid, id)| {
                gm_msgid
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or(id)
            }))
        })
        .await
}

/// Resolve the opaque Graph folder id recorded for a local folder row.
async fn graph_folder_remote_id(
    ctx: &CoreContext,
    account_id: &str,
    folder: &Folder,
) -> Result<String> {
    crate::sync::folders::graph_folder_id(ctx, account_id, &folder.remote_path)
        .await?
        .ok_or_else(|| {
            AppError::protocol(format!(
                "no Graph folder id is recorded for {}; refresh folders first",
                folder.remote_path
            ))
        })
}

/// New local remote path after renaming a Graph folder.
///
/// Mirrors `graph::folders::escape_segment`: `/` is the local hierarchy
/// separator, so it and `%` must be escaped to keep the path injective.
fn graph_renamed_path(old_path: &str, new_name: &str) -> String {
    let segment = new_name.replace('%', "%25").replace('/', "%2F");
    match old_path.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => format!("{parent}/{segment}"),
        _ => segment,
    }
}

/// Move every UID of `source_folder_id` into `target` and drop the local source
/// rows; a later folder resync repopulates the destination from Graph.
///
/// Graph assigns a new id per moved message, so the uid -> Graph id mapping is
/// rewritten under the destination folder as part of the same local write.
/// Without that, a later mark-read/delete queued against the destination folder
/// finds no mapping, falls back to the optimistic row (which still carries the
/// pre-move id) and sends Graph a 404, failing and rolling back an operation
/// the server would otherwise accept.
async fn graph_move_messages(
    ctx: &CoreContext,
    account: &Account,
    client: &GraphMailClient,
    source_folder_id: &str,
    target: &Folder,
    uids: &[i64],
) -> Result<()> {
    if uids.is_empty() {
        return Ok(());
    }
    // Mirror the IMAP executor: a move into the folder the message already
    // lives in is a no-op.  It also keeps the remap below from deleting the
    // mapping it just wrote when source and destination are the same folder.
    if target.id == source_folder_id {
        return Ok(());
    }
    let target_id = graph_folder_remote_id(ctx, &account.id, target).await?;
    let mut moved: Vec<(i64, String)> = Vec::with_capacity(uids.len());
    for uid in uids {
        let remote = graph_remote_id(ctx, &account.id, source_folder_id, *uid).await?;
        let new_id =
            graph_ops::move_message(client, ctx, &account.id, &remote, &target_id).await?;
        moved.push((*uid, new_id));
    }

    // Resolve the source rows before the write: after an optimistic move they
    // already live in the target folder and this is empty, but operations
    // queued by background paths still expect their source rows dropped.
    let source_ids = local_message_ids(ctx, source_folder_id, uids).await?;
    let account_id = account.id.clone();
    let source = source_folder_id.to_string();
    let destination = target.id.clone();
    ctx.db
        .write(move |db| {
            remap_moved_ids(db, &account_id, &source, &destination, &moved)?;
            if !source_ids.is_empty() {
                repo::messages::delete_local(db, &source_ids, true)?;
            }
            Ok(())
        })
        .await
}

/// Point the queued-operation id mapping at the ids Graph assigned to a move.
///
/// The destination entry is the one later operations resolve through, so it is
/// written with the new id.  The source entry is removed: the message is no
/// longer in that folder, and leaving it behind would make a lookup retry a
/// message id Graph has already retired.
fn remap_moved_ids(
    db: &rusqlite::Connection,
    account_id: &str,
    source_folder_id: &str,
    target_folder_id: &str,
    moved: &[(i64, String)],
) -> Result<()> {
    for (uid, new_id) in moved {
        let destination_key = format!("graph_remote:{target_folder_id}:{uid}");
        repo::sync_state::set(db, account_id, "mail", &destination_key, new_id)?;
        // Same-folder moves never reach here (the caller returns early), but
        // deleting an identical key would erase the mapping just written.
        if source_folder_id != target_folder_id {
            let source_key = format!("graph_remote:{source_folder_id}:{uid}");
            repo::sync_state::delete_key(db, account_id, "mail", &source_key)?;
        }
    }
    Ok(())
}

/// Read a staged outgoing message, enforcing the same 100 MB cap as IMAP.
async fn read_staged_message(path: &str) -> Result<Vec<u8>> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| AppError::io(format!("cannot read staged message: {error}")))?;
    if metadata.len() > 100 * 1024 * 1024 {
        return Err(AppError::invalid_input("staged message exceeds 100 MB"));
    }
    tokio::fs::read(path)
        .await
        .map_err(|error| AppError::io(format!("cannot read staged message: {error}")))
}

/// Execute due Graph operations for one account.
///
/// This is the Graph counterpart of [`run_due`]: same ordering, retry and
/// rollback semantics, but no IMAP connection.  The account actor calls it
/// from the Graph loop.
pub async fn run_due_graph(ctx: &CoreContext, account: &Account) -> Result<ExecOutcome> {
    recover_in_progress(ctx, &account.id).await?;
    let account_id = account.id.clone();
    let now = Utc::now();
    let ops = ctx
        .db
        .read({
            let account_id = account_id.clone();
            move |db| repo::pending_ops::list(db, Some(&account_id))
        })
        .await?;
    let due: Vec<PendingOp> = ops
        .into_iter()
        .filter(|op| {
            is_mail_kind(&op.kind)
                && op.state == PendingOpState::Pending
                && op.next_retry_at.map(|at| at <= now).unwrap_or(true)
        })
        .take(MAX_OPS_PER_PASS)
        .collect();
    if due.is_empty() {
        return Ok(ExecOutcome::default());
    }

    let mut outcome = ExecOutcome::default();
    for op in due {
        mark_in_progress(ctx, std::slice::from_ref(&op.id)).await?;
        match execute_graph_op(ctx, account, &op).await {
            Ok(()) => {
                mark_done(ctx, std::slice::from_ref(&op.id)).await?;
                outcome.done += 1;
                if op.kind == KIND_APPEND_TO_SENT {
                    if let Ok(payload) = repo::pending_ops::payload::<AppendToSentOp>(&op) {
                        let _ = tokio::fs::remove_file(&payload.rfc822_path).await;
                    }
                }
                if op.kind == KIND_APPEND_DRAFT {
                    if let Ok(payload) = repo::pending_ops::payload::<AppendDraftOp>(&op) {
                        outcome.resync_folders.insert(payload.folder_id);
                    }
                }
                if matches!(op.kind.as_str(), KIND_MOVE | KIND_ARCHIVE | KIND_MARK_JUNK) {
                    if let Ok(payload) = repo::pending_ops::payload::<MoveOp>(&op) {
                        // Resync the source as well: the old local row was
                        // moved optimistically and only the source delta
                        // reports it as removed.
                        outcome.resync_folders.insert(payload.folder_id.clone());
                        outcome.resync_folders.insert(payload.target_folder_id);
                    }
                    if let Ok(payload) = repo::pending_ops::payload::<ArchiveOp>(&op) {
                        outcome.resync_folders.insert(payload.folder_id.clone());
                    }
                    if let Ok(payload) = repo::pending_ops::payload::<MarkJunkOp>(&op) {
                        outcome.resync_folders.insert(payload.folder_id.clone());
                    }
                }
                if op.kind == KIND_DELETE {
                    if let Ok(payload) = repo::pending_ops::payload::<DeleteOp>(&op) {
                        outcome.resync_folders.insert(payload.folder_id.clone());
                        if let Some(target) = payload.target_folder_id {
                            outcome.resync_folders.insert(target);
                        }
                    }
                }
            }
            Err(error) => {
                outcome.resync_folders.extend(op_folders(&op));
                // Report against MicrosoftGraph, not Mail: sharing the IMAP
                // resource would invalidate the Outlook IMAP token and can mark
                // an otherwise healthy account as needing re-authentication.
                handle_failure(
                    ctx,
                    account,
                    &op,
                    &error,
                    TokenResource::MicrosoftGraph,
                    &mut outcome,
                )
                .await?;
            }
        }
    }
    Ok(outcome)
}

/// Execute one freshly queued operation through Graph and settle it now.
///
/// Used by the Graph command loop for folder create/rename/delete, which need
/// a synchronous result for the command layer.
///
/// Every error goes through [`handle_failure`], exactly like the queue pass:
/// retryable failures (network, service, auth, transient protocol) leave the row
/// `pending` with backoff, everything else is rolled back and marked failed.
/// Returning early without settling the row would leave it `inProgress` until
/// the ten-minute recovery sweep, so a user retry in between could run a
/// non-idempotent operation (folder create) twice.
pub async fn run_one_graph_now(ctx: &CoreContext, account: &Account, op: &PendingOp) -> Result<()> {
    mark_in_progress(ctx, std::slice::from_ref(&op.id)).await?;
    match execute_graph_op(ctx, account, op).await {
        Ok(()) => mark_done(ctx, std::slice::from_ref(&op.id)).await,
        Err(error) => {
            let mut outcome = ExecOutcome::default();
            handle_failure(
                ctx,
                account,
                op,
                &error,
                TokenResource::MicrosoftGraph,
                &mut outcome,
            )
            .await?;
            Err(error)
        }
    }
}

async fn execute_set_flags(
    _ctx: &CoreContext,
    conn: &mut ImapConnection,
    _account: &Account,
    selected: &mut Option<String>,
    payload: &SetFlagsOp,
) -> Result<()> {
    if payload.uids.is_empty() {
        return Ok(());
    }
    let folder = folder_by_id(_ctx, &payload.folder_id).await?;
    ensure_selected(conn, selected, &folder.remote_path).await?;

    let mut add = Vec::new();
    let mut remove = Vec::new();
    for (value, name) in [
        (payload.seen, "\\Seen"),
        (payload.flagged, "\\Flagged"),
        (payload.answered, "\\Answered"),
        (payload.forwarded, "$Forwarded"),
    ] {
        match value {
            Some(true) => add.push(name),
            Some(false) => remove.push(name),
            None => {}
        }
    }

    let uids = uid_set(&payload.uids);
    if !add.is_empty() {
        store(conn, &uids, &format!("+FLAGS.SILENT ({})", add.join(" "))).await?;
    }
    if !remove.is_empty() {
        store(
            conn,
            &uids,
            &format!("-FLAGS.SILENT ({})", remove.join(" ")),
        )
        .await?;
    }
    Ok(())
}

async fn execute_move(
    _ctx: &CoreContext,
    conn: &mut ImapConnection,
    _account: &Account,
    selected: &mut Option<String>,
    source_id: &str,
    target_id: &str,
    uids: &[i64],
) -> Result<()> {
    if uids.is_empty() {
        return Ok(());
    }
    let source = folder_by_id(_ctx, source_id).await?;
    let target = folder_by_id(_ctx, target_id).await?;
    if source.id == target.id {
        return Ok(());
    }
    ensure_selected(conn, selected, &source.remote_path).await?;
    let set = uid_set(uids);
    if conn.caps.r#move {
        tokio::time::timeout(
            COMMAND_TIMEOUT,
            conn.session.uid_mv(&set, &target.remote_path),
        )
        .await
        .map_err(|_| AppError::network("UID MOVE timed out"))?
        .map_err(imap_error)?;
    } else {
        tokio::time::timeout(
            COMMAND_TIMEOUT,
            conn.session.uid_copy(&set, &target.remote_path),
        )
        .await
        .map_err(|_| AppError::network("UID COPY timed out"))?
        .map_err(imap_error)?;
        store(conn, &set, "+FLAGS.SILENT (\\Deleted)").await?;
        expire(conn, &set).await?;
    }
    debug!(source = %source.remote_path, target = %target.remote_path, "moved messages");
    Ok(())
}

async fn execute_delete(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &Account,
    selected: &mut Option<String>,
    payload: &DeleteOp,
) -> Result<()> {
    if payload.uids.is_empty() {
        return Ok(());
    }
    let folder = folder_by_id(ctx, &payload.folder_id).await?;
    let already_gone = matches!(folder.role, FolderRole::Trash | FolderRole::Junk);
    if !payload.permanent && !already_gone {
        if let Some(trash) = role_folder(ctx, &account.id, FolderRole::Trash).await? {
            execute_move(
                ctx,
                conn,
                account,
                selected,
                &folder.id,
                &trash.id,
                &payload.uids,
            )
            .await?;
            remove_local_messages(ctx, &folder.id, &payload.uids).await?;
            return Ok(());
        }
    }
    execute_permanent_delete(ctx, conn, selected, &folder.id, &payload.uids).await?;
    let ids = local_message_ids(ctx, &folder.id, &payload.uids).await?;
    if !ids.is_empty() {
        ctx.db
            .write(move |db| repo::messages::delete_local(db, &ids, true))
            .await?;
    }
    Ok(())
}

async fn execute_archive(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &Account,
    selected: &mut Option<String>,
    payload: &ArchiveOp,
) -> Result<()> {
    if payload.uids.is_empty() {
        return Ok(());
    }
    let folder = folder_by_id(ctx, &payload.folder_id).await?;
    if matches!(folder.role, FolderRole::All | FolderRole::Archive) {
        return Ok(());
    }
    ensure_selected(conn, selected, &folder.remote_path).await?;
    let set = uid_set(&payload.uids);

    if account.capabilities.gmail_extensions {
        store(conn, &set, "-X-GM-LABELS.SILENT (\\Inbox)").await?;
        remove_local_messages(ctx, &folder.id, &payload.uids).await?;
        return Ok(());
    }

    if let Some(archive) = role_folder(ctx, &account.id, FolderRole::Archive).await? {
        execute_move(
            ctx,
            conn,
            account,
            selected,
            &folder.id,
            &archive.id,
            &payload.uids,
        )
        .await?;
        remove_local_messages(ctx, &folder.id, &payload.uids).await?;
        return Ok(());
    }

    // No Archive folder: create a standard one and retry.
    if let Err(error) = conn.session.create("Archive").await {
        debug!(%error, "could not create Archive folder");
    }
    crate::sync::folders::sync(ctx, conn, &account.id).await?;
    if let Some(archive) = role_folder(ctx, &account.id, FolderRole::Archive).await? {
        execute_move(
            ctx,
            conn,
            account,
            selected,
            &folder.id,
            &archive.id,
            &payload.uids,
        )
        .await?;
        remove_local_messages(ctx, &folder.id, &payload.uids).await?;
        Ok(())
    } else {
        Err(AppError::protocol(
            "server has no Archive folder and it could not be created",
        ))
    }
}

async fn execute_mark_junk(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &Account,
    selected: &mut Option<String>,
    payload: &MarkJunkOp,
) -> Result<()> {
    let target_role = if payload.value {
        FolderRole::Junk
    } else {
        FolderRole::Inbox
    };
    let Some(target) = role_folder(ctx, &account.id, target_role).await? else {
        return Err(AppError::unsupported(format!(
            "account has no {} folder",
            target_role.as_str()
        )));
    };
    execute_move(
        ctx,
        conn,
        account,
        selected,
        &payload.folder_id,
        &target.id,
        &payload.uids,
    )
    .await?;
    remove_local_messages(ctx, &payload.folder_id, &payload.uids).await?;
    Ok(())
}

async fn execute_permanent_delete(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    selected: &mut Option<String>,
    folder_id: &str,
    uids: &[i64],
) -> Result<()> {
    if uids.is_empty() {
        return Ok(());
    }
    let folder = folder_by_id(ctx, folder_id).await?;
    ensure_selected(conn, selected, &folder.remote_path).await?;
    let set = uid_set(uids);
    store(conn, &set, "+FLAGS.SILENT (\\Deleted)").await?;
    expire(conn, &set).await?;
    Ok(())
}

async fn execute_append(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    selected: &mut Option<String>,
    folder_id: &str,
    path: &str,
    flags: Option<&str>,
) -> Result<()> {
    let folder = folder_by_id(ctx, folder_id).await?;
    // APPEND works without selecting, but selecting clears `selected` state.
    *selected = None;
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| AppError::io(format!("cannot read staged message: {error}")))?;
    if metadata.len() > 100 * 1024 * 1024 {
        return Err(AppError::invalid_input("staged message exceeds 100 MB"));
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| AppError::io(format!("cannot read staged message: {error}")))?;
    conn.append(&folder.remote_path, flags, &bytes).await
}

async fn execute_folder_create(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &Account,
    payload: &FolderCreateOp,
) -> Result<()> {
    let parent = match payload.parent_id.as_deref() {
        Some(id) => Some(folder_by_id(ctx, id).await?),
        None => None,
    };
    let remote = match parent {
        Some(parent) => {
            let delimiter = if parent.remote_path.contains('/') {
                "/"
            } else if parent.remote_path.contains('.') {
                "."
            } else {
                "/"
            };
            format!(
                "{}{delimiter}{}",
                parent.remote_path.trim_end_matches(delimiter),
                payload.name
            )
        }
        None => payload.name.clone(),
    };
    conn.session.create(&remote).await.map_err(imap_error)?;
    crate::sync::folders::sync(ctx, conn, &account.id).await?;
    Ok(())
}

async fn execute_folder_rename(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    payload: &FolderRenameOp,
) -> Result<()> {
    let folder = folder_by_id(ctx, &payload.folder_id).await?;
    let delimiter = if folder.remote_path.contains('/') {
        '/'
    } else if folder.remote_path.contains('.') {
        '.'
    } else {
        '/'
    };
    let new_remote = match folder.remote_path.rsplit_once(delimiter) {
        Some((parent, _)) if !parent.is_empty() => {
            format!("{parent}{delimiter}{}", payload.new_name)
        }
        _ => payload.new_name.clone(),
    };
    conn.session
        .rename(&folder.remote_path, &new_remote)
        .await
        .map_err(imap_error)?;
    // Update the local row before syncing so the folder id (and its messages)
    // survive the rename instead of being deleted/recreated by path.
    let folder_id = folder.id.clone();
    let updated_name = payload.new_name.clone();
    let updated_remote = new_remote.clone();
    ctx.db
        .write(move |db| {
            db.execute(
                "UPDATE folder SET remote_path = ?2, name = ?3, updated_at = ?4 WHERE id = ?1",
                rusqlite::params![
                    folder_id,
                    updated_remote,
                    updated_name,
                    nuncio_core::repo::now_string()
                ],
            )?;
            Ok(())
        })
        .await?;
    crate::sync::folders::sync(ctx, conn, &folder.account_id).await?;
    Ok(())
}

async fn execute_folder_delete(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    payload: &FolderDeleteOp,
) -> Result<()> {
    let folder = folder_by_id(ctx, &payload.folder_id).await?;
    // Empty the mailbox first; some servers refuse to delete a non-empty one.
    if folder.is_selectable && conn.select(&folder.remote_path).await.is_ok() {
        let _ = store(conn, "1:*", "+FLAGS.SILENT (\\Deleted)").await;
        let _ = expire(conn, "1:*").await;
    }
    if let Err(error) = conn.session.delete(&folder.remote_path).await {
        debug!(%error, folder = %folder.remote_path, "DELETE mailbox failed");
        return Err(imap_error(error));
    }
    let folder_id = folder.id.clone();
    ctx.db
        .write(move |db| repo::folders::delete(db, &folder_id))
        .await?;
    crate::sync::folders::sync(ctx, conn, &folder.account_id).await?;
    Ok(())
}

async fn execute_folder_mark_read(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    selected: &mut Option<String>,
    payload: &FolderMarkReadOp,
) -> Result<()> {
    let folder = folder_by_id(ctx, &payload.folder_id).await?;
    ensure_selected(conn, selected, &folder.remote_path).await?;
    store(conn, "1:*", "+FLAGS.SILENT (\\Seen)").await?;
    let folder_id = payload.folder_id.clone();
    ctx.db
        .write(move |db| {
            repo::messages::mark_all_read(db, &folder_id)?;
            Ok(())
        })
        .await
}

async fn execute_folder_subscribe(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    payload: &FolderSubscribeOp,
) -> Result<()> {
    let folder = folder_by_id(ctx, &payload.folder_id).await?;
    if payload.subscribe {
        conn.session
            .subscribe(&folder.remote_path)
            .await
            .map_err(imap_error)
    } else {
        conn.session
            .unsubscribe(&folder.remote_path)
            .await
            .map_err(imap_error)
    }
}

async fn apply_local_only(ctx: &CoreContext, op: &PendingOp) -> Result<()> {
    match op.kind.as_str() {
        KIND_MOVE => {
            let payload: MoveOp = repo::pending_ops::payload(op)?;
            let ids = local_message_ids(ctx, &payload.folder_id, &payload.uids).await?;
            let target = payload.target_folder_id.clone();
            ctx.db
                .write(move |db| repo::messages::move_to_folder(db, &ids, &target))
                .await?;
            Ok(())
        }
        KIND_DELETE => {
            let payload: DeleteOp = repo::pending_ops::payload(op)?;
            let ids = local_message_ids(ctx, &payload.folder_id, &payload.uids).await?;
            let permanent = payload.permanent;
            ctx.db
                .write(move |db| repo::messages::delete_local(db, &ids, permanent))
                .await?;
            Ok(())
        }
        KIND_FOLDER_MARK_READ => {
            let payload: FolderMarkReadOp = repo::pending_ops::payload(op)?;
            let folder_id = payload.folder_id.clone();
            ctx.db
                .write(move |db| {
                    repo::messages::mark_all_read(db, &folder_id)?;
                    Ok(())
                })
                .await
        }
        KIND_ARCHIVE | KIND_MARK_JUNK => Ok(()),
        _ => Ok(()),
    }
}

pub(crate) async fn store(conn: &mut ImapConnection, set: &str, query: &str) -> Result<()> {
    let session = &mut conn.session;
    let stream = tokio::time::timeout(COMMAND_TIMEOUT, session.uid_store(set, query))
        .await
        .map_err(|_| AppError::network("UID STORE timed out"))?
        .map_err(imap_error)?;
    tokio::time::timeout(COMMAND_TIMEOUT, stream.try_collect::<Vec<_>>())
        .await
        .map_err(|_| AppError::network("UID STORE response timed out"))?
        .map_err(imap_error)?;
    Ok(())
}

pub(crate) async fn expire(conn: &mut ImapConnection, set: &str) -> Result<()> {
    if conn.caps.uidplus {
        let session = &mut conn.session;
        let stream = tokio::time::timeout(COMMAND_TIMEOUT, session.uid_expunge(set))
            .await
            .map_err(|_| AppError::network("UID EXPUNGE timed out"))?
            .map_err(imap_error)?;
        tokio::time::timeout(COMMAND_TIMEOUT, stream.try_collect::<Vec<_>>())
            .await
            .map_err(|_| AppError::network("UID EXPUNGE response timed out"))?
            .map_err(imap_error)?;
    } else {
        let session = &mut conn.session;
        let stream = tokio::time::timeout(COMMAND_TIMEOUT, session.expunge())
            .await
            .map_err(|_| AppError::network("EXPUNGE timed out"))?
            .map_err(imap_error)?;
        tokio::time::timeout(COMMAND_TIMEOUT, stream.try_collect::<Vec<_>>())
            .await
            .map_err(|_| AppError::network("EXPUNGE response timed out"))?
            .map_err(imap_error)?;
    }
    Ok(())
}

async fn ensure_selected(
    conn: &mut ImapConnection,
    selected: &mut Option<String>,
    remote_path: &str,
) -> Result<()> {
    if selected.as_deref() == Some(remote_path) {
        return Ok(());
    }
    conn.select(remote_path).await?;
    *selected = Some(remote_path.to_string());
    Ok(())
}

async fn role_folder(
    ctx: &CoreContext,
    account_id: &str,
    role: FolderRole,
) -> Result<Option<Folder>> {
    let account_id = account_id.to_string();
    ctx.db
        .read(move |db| repo::folders::by_role(db, &account_id, role))
        .await
}

async fn folder_by_id(ctx: &CoreContext, id: &str) -> Result<Folder> {
    let id = id.to_string();
    ctx.db
        .read(move |db| repo::folders::get_required(db, &id))
        .await
}

async fn local_message_ids(
    ctx: &CoreContext,
    folder_id: &str,
    uids: &[i64],
) -> Result<Vec<String>> {
    let folder_id = folder_id.to_string();
    let uids = uids.to_vec();
    ctx.db
        .read(move |db| {
            let mut ids = Vec::new();
            for uid in uids {
                if let Some(stored) = repo::messages::by_uid(db, &folder_id, uid)? {
                    ids.push(stored.summary.id);
                }
            }
            Ok(ids)
        })
        .await
}

async fn remove_local_messages(ctx: &CoreContext, folder_id: &str, uids: &[i64]) -> Result<()> {
    let ids = local_message_ids(ctx, folder_id, uids).await?;
    if ids.is_empty() {
        return Ok(());
    }
    ctx.db
        .write(move |db| repo::messages::delete_local(db, &ids, true))
        .await?;
    Ok(())
}

async fn mark_in_progress(ctx: &CoreContext, ids: &[String]) -> Result<()> {
    let ids = ids.to_vec();
    ctx.db
        .write(move |db| {
            for id in ids {
                repo::pending_ops::mark_in_progress(db, &id)?;
            }
            Ok(())
        })
        .await
}

async fn mark_done(ctx: &CoreContext, ids: &[String]) -> Result<()> {
    let ids = ids.to_vec();
    ctx.db
        .write(move |db| {
            for id in ids {
                repo::pending_ops::mark_done(db, &id)?;
            }
            Ok(())
        })
        .await
}

fn op_folders(op: &PendingOp) -> Vec<String> {
    match op.kind.as_str() {
        KIND_SET_FLAGS => repo::pending_ops::payload::<SetFlagsOp>(op)
            .map(|payload| vec![payload.folder_id])
            .unwrap_or_default(),
        KIND_MOVE => repo::pending_ops::payload::<MoveOp>(op)
            .map(|payload| vec![payload.folder_id, payload.target_folder_id])
            .unwrap_or_default(),
        KIND_DELETE => repo::pending_ops::payload::<DeleteOp>(op)
            .map(|payload| match payload.target_folder_id {
                Some(target) => vec![payload.folder_id, target],
                None => vec![payload.folder_id],
            })
            .unwrap_or_default(),
        KIND_ARCHIVE => repo::pending_ops::payload::<ArchiveOp>(op)
            .map(|payload| match payload.target_folder_id {
                Some(target) => vec![payload.folder_id, target],
                None => vec![payload.folder_id],
            })
            .unwrap_or_default(),
        KIND_MARK_JUNK => repo::pending_ops::payload::<MarkJunkOp>(op)
            .map(|payload| match payload.target_folder_id {
                Some(target) => vec![payload.folder_id, target],
                None => vec![payload.folder_id],
            })
            .unwrap_or_default(),
        KIND_APPEND_DRAFT => repo::pending_ops::payload::<AppendDraftOp>(op)
            .map(|payload| vec![payload.folder_id])
            .unwrap_or_default(),
        KIND_DELETE_REMOTE_DRAFT => repo::pending_ops::payload::<DeleteRemoteDraftOp>(op)
            .map(|payload| vec![payload.folder_id])
            .unwrap_or_default(),
        KIND_APPEND_TO_SENT => repo::pending_ops::payload::<AppendToSentOp>(op)
            .map(|payload| vec![payload.folder_id])
            .unwrap_or_default(),
        KIND_FOLDER_MARK_READ => repo::pending_ops::payload::<FolderMarkReadOp>(op)
            .map(|payload| vec![payload.folder_id])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Settle a failed operation: retry with backoff when the error is transient,
/// otherwise roll the optimistic change back and fail the row.
///
/// `token` is the credential resource the operation actually used.  It must be
/// supplied by the caller: a Graph operation reported under `Mail` would
/// invalidate the Outlook IMAP token and can wrongly mark the account as
/// needing re-authentication, even though the Graph token is fine.
async fn handle_failure(
    ctx: &CoreContext,
    account: &Account,
    op: &PendingOp,
    error: &AppError,
    token: TokenResource,
    outcome: &mut ExecOutcome,
) -> Result<()> {
    // Graph policy denials arrive as `Unsupported` (see
    // `GraphMailClient::failure`), so they fall through to the terminal path:
    // refreshing a valid token cannot grant a permission.
    let retryable = matches!(
        error,
        AppError::Network(_)
            | AppError::Service(_)
            | AppError::Auth(_)
            | AppError::Db(_)
            | AppError::Io(_)
    ) || matches!(
        error,
        AppError::Protocol(_) if op.attempts < 2
    );
    if error.code() == "auth" {
        ctx.creds.report_auth_failure(&account.id, token).await;
    }
    if retryable && op.attempts + 1 < MAX_ATTEMPTS {
        let op_id = op.id.clone();
        let message = error.message();
        ctx.db
            .write({
                let now = Utc::now();
                move |db| repo::pending_ops::mark_retry(db, &op_id, &message, now)
            })
            .await?;
        outcome.retried += 1;
        debug!(op = %op.id, %error, "operation will retry");
    } else {
        let folders = fail_op(ctx, op, AppError::internal(error.message())).await?;
        outcome.resync_folders.extend(folders);
        outcome.failed += 1;
    }
    Ok(())
}

/// Mark an operation failed permanently, roll the optimistic local change back
/// and report which folders must be re-synchronized. The server is the source
/// of truth: if the inverse operation cannot fully restore local state, a
/// forced folder re-sync repairs it.
async fn fail_op(ctx: &CoreContext, op: &PendingOp, error: AppError) -> Result<Vec<String>> {
    let folders = match rollback_operation(ctx, op).await {
        Ok(folders) => folders,
        Err(rollback_error) => {
            tracing::warn!(op = %op.id, %rollback_error, "local rollback failed");
            op_folders(op)
        }
    };
    let op_id = op.id.clone();
    let message = error.message();
    for folder in &folders {
        events::emit_mail_changed(
            ctx.events.as_ref(),
            op.account_id.clone(),
            vec![folder.clone()],
            Vec::new(),
            events::MailChangeKind::Updated,
        );
    }
    let message_for_db = message.clone();
    ctx.db
        .write(move |db| repo::pending_ops::mark_failed(db, &op_id, &message_for_db))
        .await?;
    events::emit_op_failed(
        ctx.events.as_ref(),
        op.account_id.clone(),
        &op.description,
        &message,
    );
    info!(op = %op.id, kind = %op.kind, error = %message, "operation failed permanently and was rolled back");
    Ok(folders)
}

async fn rollback_operation(ctx: &CoreContext, op: &PendingOp) -> Result<Vec<String>> {
    let op = op.clone();
    ctx.db.write(move |db| rollback_in_db(db, &op)).await
}

/// Local inverse of a queueable operation. Uses the state captured in the
/// payload; payloads queued by older versions (or background paths) simply
/// have nothing to roll back and are repaired by the follow-up re-sync.
fn rollback_in_db(db: &rusqlite::Connection, op: &PendingOp) -> Result<Vec<String>> {
    let mut folders: Vec<String> = Vec::new();
    match op.kind.as_str() {
        KIND_SET_FLAGS => {
            let payload: SetFlagsOp = repo::pending_ops::payload(op)?;
            for previous in &payload.previous {
                if let Some(stored) = repo::messages::by_uid(db, &payload.folder_id, previous.uid)?
                {
                    repo::messages::set_flags(
                        db,
                        std::slice::from_ref(&stored.summary.id),
                        Some(previous.seen),
                        Some(previous.flagged),
                        Some(previous.answered),
                        Some(previous.forwarded),
                    )?;
                }
            }
            folders.push(payload.folder_id);
        }
        KIND_MOVE => {
            let payload: MoveOp = repo::pending_ops::payload(op)?;
            let ids = ids_by_uids(db, &payload.target_folder_id, &payload.uids)?;
            if !ids.is_empty() {
                repo::messages::move_to_folder(db, &ids, &payload.folder_id)?;
            }
            folders.push(payload.folder_id);
            folders.push(payload.target_folder_id);
        }
        KIND_DELETE => {
            let payload: DeleteOp = repo::pending_ops::payload(op)?;
            if payload.permanent {
                // The row was deleted locally; force a full re-fetch so the
                // message comes back from the server.
                force_full_resync(db, &payload.folder_id)?;
            } else if let Some(target) = payload.target_folder_id.as_deref() {
                let ids = ids_by_uids(db, target, &payload.uids)?;
                if !ids.is_empty() {
                    repo::messages::move_to_folder(db, &ids, &payload.folder_id)?;
                }
                folders.push(target.to_string());
            } else if payload.soft_hidden {
                for uid in &payload.uids {
                    if let Some(stored) = repo::messages::by_uid(db, &payload.folder_id, *uid)? {
                        repo::messages::set_deleted_local(db, &stored.summary.id, false)?;
                    }
                }
            }
            folders.push(payload.folder_id);
        }
        KIND_ARCHIVE => {
            let payload: ArchiveOp = repo::pending_ops::payload(op)?;
            let target = match payload.target_folder_id.clone() {
                Some(target) => Some(target),
                None => match role_folder_in_db(db, &op.account_id, FolderRole::Archive)? {
                    Some(folder) => Some(folder.id),
                    None => role_folder_in_db(db, &op.account_id, FolderRole::All)?
                        .map(|folder| folder.id),
                },
            };
            if let Some(target) = target {
                let ids = ids_by_uids(db, &target, &payload.uids)?;
                if !ids.is_empty() {
                    repo::messages::move_to_folder(db, &ids, &payload.folder_id)?;
                }
                folders.push(target);
            }
            folders.push(payload.folder_id);
        }
        KIND_MARK_JUNK => {
            let payload: MarkJunkOp = repo::pending_ops::payload(op)?;
            let target = match payload.target_folder_id.clone() {
                Some(target) => Some(target),
                None => {
                    let role = if payload.value {
                        FolderRole::Junk
                    } else {
                        FolderRole::Inbox
                    };
                    role_folder_in_db(db, &op.account_id, role)?.map(|folder| folder.id)
                }
            };
            if let Some(target) = target {
                let ids = ids_by_uids(db, &target, &payload.uids)?;
                if !ids.is_empty() {
                    repo::messages::move_to_folder(db, &ids, &payload.folder_id)?;
                }
                folders.push(target);
            }
            folders.push(payload.folder_id);
        }
        _ => {}
    }
    folders.sort();
    folders.dedup();
    for folder in &folders {
        repo::folders::recompute_counts(db, folder)?;
    }
    Ok(folders)
}

fn ids_by_uids(db: &rusqlite::Connection, folder_id: &str, uids: &[i64]) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for uid in uids {
        if let Some(stored) = repo::messages::by_uid(db, folder_id, *uid)? {
            ids.push(stored.summary.id);
        }
    }
    Ok(ids)
}

fn role_folder_in_db(
    db: &rusqlite::Connection,
    account_id: &str,
    role: FolderRole,
) -> Result<Option<Folder>> {
    repo::folders::by_role(db, account_id, role)
}

/// Clear the UIDNEXT marker so the next sync re-fetches the folder, and leave
/// UIDVALIDITY intact. The message id is deterministic from the UID, so rows
/// restored this way keep their identity.
fn force_full_resync(db: &rusqlite::Connection, folder_id: &str) -> Result<()> {
    let (uidvalidity, _, _) = repo::folders::sync_flags(db, folder_id)?;
    repo::folders::set_sync_flags(db, folder_id, uidvalidity, None, None)?;
    debug!(
        folder = folder_id,
        "queued a full folder re-sync after rollback"
    );
    Ok(())
}

/// Helper used by the worker to drain without holding `ctx` borrows for long.
pub async fn run_due_with_timeout(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &Account,
) -> Result<ExecOutcome> {
    match tokio::time::timeout(Duration::from_secs(300), run_due(ctx, conn, account)).await {
        Ok(result) => result,
        Err(_) => Err(AppError::network("operation queue pass timed out")),
    }
}

/// Queue a folder operation immediately before executing it (folder commands
/// need a synchronous result for the command layer).
async fn enqueue_now(
    ctx: &CoreContext,
    account_id: &str,
    kind: &str,
    payload: serde_json::Value,
    description: &str,
) -> Result<PendingOp> {
    let op = nuncio_core::models::NewPendingOp::new(account_id, kind, payload, description);
    ctx.db
        .write(move |db| repo::pending_ops::enqueue(db, op))
        .await
}

pub async fn enqueue_folder_create_now(
    ctx: &CoreContext,
    account_id: &str,
    parent_id: Option<&str>,
    name: &str,
) -> Result<PendingOp> {
    let payload = FolderCreateOp {
        parent_id: parent_id.map(str::to_string),
        name: name.to_string(),
    };
    enqueue_now(
        ctx,
        account_id,
        KIND_FOLDER_CREATE,
        serde_json::to_value(payload)
            .map_err(|error| AppError::internal(format!("encode folderCreate: {error}")))?,
        "Create folder",
    )
    .await
}

pub async fn enqueue_folder_rename_now(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    name: &str,
) -> Result<PendingOp> {
    let payload = FolderRenameOp {
        folder_id: folder_id.to_string(),
        new_name: name.to_string(),
    };
    enqueue_now(
        ctx,
        account_id,
        KIND_FOLDER_RENAME,
        serde_json::to_value(payload)
            .map_err(|error| AppError::internal(format!("encode folderRename: {error}")))?,
        "Rename folder",
    )
    .await
}

pub async fn enqueue_folder_delete_now(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
) -> Result<PendingOp> {
    let payload = FolderDeleteOp {
        folder_id: folder_id.to_string(),
    };
    enqueue_now(
        ctx,
        account_id,
        KIND_FOLDER_DELETE,
        serde_json::to_value(payload)
            .map_err(|error| AppError::internal(format!("encode folderDelete: {error}")))?,
        "Delete folder",
    )
    .await
}

pub async fn enqueue_folder_subscribe_now(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    subscribe: bool,
) -> Result<PendingOp> {
    let payload = FolderSubscribeOp {
        folder_id: folder_id.to_string(),
        subscribe,
    };
    enqueue_now(
        ctx,
        account_id,
        KIND_FOLDER_SUBSCRIBE,
        serde_json::to_value(payload)
            .map_err(|error| AppError::internal(format!("encode folderSubscribe: {error}")))?,
        "Subscribe folder",
    )
    .await
}

/// Execute one freshly queued operation and settle it immediately.
pub async fn run_one_now(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &Account,
    op: &PendingOp,
) -> Result<()> {
    mark_in_progress(ctx, std::slice::from_ref(&op.id)).await?;
    let mut selected = None;
    match execute_one(ctx, conn, account, &mut selected, op).await {
        Ok(()) => mark_done(ctx, std::slice::from_ref(&op.id)).await,
        Err(error) if matches!(error.code(), "network" | "service" | "auth") => {
            // Immediate command: leave the op pending so the next queue pass
            // retries it with backoff.
            Err(error)
        }
        Err(error) => {
            let folders = fail_op(ctx, op, AppError::internal(error.message())).await?;
            for folder in folders {
                if let Err(sync_error) =
                    crate::sync::sync_folder_by_id(ctx, conn, account, &folder).await
                {
                    debug!(%sync_error, folder = %folder, "post-failure resync failed");
                }
            }
            Err(error)
        }
    }
}

/// Execute queued operations for accounts without an IMAP connection (POP3):
/// only local state can change.
pub async fn run_due_local(ctx: &CoreContext, account: &Account) -> Result<usize> {
    recover_in_progress(ctx, &account.id).await?;
    let account_id = account.id.clone();
    let now = Utc::now();
    let ops = ctx
        .db
        .read({
            let account_id = account_id.clone();
            move |db| repo::pending_ops::list(db, Some(&account_id))
        })
        .await?;
    let due: Vec<PendingOp> = ops
        .into_iter()
        .filter(|op| {
            is_mail_kind(&op.kind)
                && op.state == PendingOpState::Pending
                && op.next_retry_at.map(|at| at <= now).unwrap_or(true)
        })
        .take(MAX_OPS_PER_PASS)
        .collect();
    let mut done = 0;
    for op in due {
        let op_id = op.id.clone();
        ctx.db
            .write({
                let op_id = op_id.clone();
                move |db| repo::pending_ops::mark_in_progress(db, &op_id)
            })
            .await?;
        let result = apply_local_only(ctx, &op).await;
        let op_id = op.id.clone();
        match result {
            Ok(()) => {
                ctx.db
                    .write(move |db| repo::pending_ops::mark_done(db, &op_id))
                    .await?;
                done += 1;
            }
            Err(error) => {
                let message = error.message();
                ctx.db
                    .write(move |db| repo::pending_ops::mark_failed(db, &op_id, &message))
                    .await?;
            }
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::FlagRollback;
    use nuncio_core::models::{FolderRole, MessageSummary, StoredMessage};

    fn setup() -> nuncio_core::Db {
        let db = nuncio_core::Db::open_in_memory().expect("db");
        db.migrate_core().expect("migrate");
        db
    }

    fn account() -> nuncio_core::models::Account {
        use nuncio_core::models::{AccountState, AuthKind, ProviderCapabilities, ProviderKind};
        nuncio_core::models::Account {
            id: "acc".into(),
            provider: ProviderKind::Imap,
            email: "test@example.com".into(),
            display_name: "Test".into(),
            color: "#336699".into(),
            auth_kind: AuthKind::Password,
            oauth_provider: None,
            imap: None,
            smtp: None,
            pop3: None,
            caldav_url: None,
            carddav_url: None,
            sent_copy: None,
            sync_window_days: 0,
            mail_enabled: true,
            calendar_enabled: false,
            contacts_enabled: false,
            tasks_enabled: false,
            notifications_enabled: false,
            include_in_unified: true,
            sort_order: 0,
            state: AccountState::Ok,
            state_message: None,
            capabilities: ProviderCapabilities::defaults_for(ProviderKind::Imap),
            created_at: nuncio_core::models::now(),
        }
    }

    fn folder(id: &str, remote: &str, role: FolderRole) -> Folder {
        Folder {
            id: id.to_string(),
            account_id: "acc".into(),
            parent_id: None,
            remote_path: remote.to_string(),
            name: remote.to_string(),
            role,
            unread_count: 0,
            total_count: 0,
            is_favorite: false,
            sync_enabled: true,
            is_selectable: true,
            sort_order: 0,
            last_sync_at: None,
        }
    }

    fn message(id: &str, folder_id: &str, uid: i64, seen: bool) -> StoredMessage {
        let mut summary = MessageSummary::new(
            id,
            "acc",
            folder_id,
            format!("thread:{id}"),
            nuncio_core::models::now(),
        );
        summary.subject = "Subject".into();
        summary.seen = seen;
        let mut stored = StoredMessage::new(summary);
        stored.uid = Some(uid);
        stored
    }

    fn pending(kind: &str, payload: serde_json::Value) -> PendingOp {
        PendingOp {
            id: "op".into(),
            account_id: "acc".into(),
            kind: kind.to_string(),
            payload,
            state: PendingOpState::InProgress,
            attempts: 8,
            next_retry_at: None,
            created_at: Utc::now(),
            description: "test".into(),
            last_error: None,
        }
    }

    #[test]
    fn graph_renamed_path_keeps_parent_and_escapes_separators() {
        assert_eq!(graph_renamed_path("Projects/2026", "Q1"), "Projects/Q1");
        assert_eq!(graph_renamed_path("Projects", "Q1"), "Q1");
        assert_eq!(graph_renamed_path("Projects/2026", "A/B"), "Projects/A%2FB");
        assert_eq!(
            graph_renamed_path("Projects/2026", "100%"),
            "Projects/100%25"
        );
    }

    #[test]
    fn move_remap_writes_the_destination_mapping() {
        let db = setup();
        db.write_sync(|conn| {
            repo::sync_state::set(
                conn,
                "acc",
                "mail",
                "graph_remote:src:5",
                "old-id",
            )?;
            remap_moved_ids(conn, "acc", "src", "dst", &[(5, "new-id".to_string())])?;
            // Later operations queued against the destination folder resolve
            // through this key, so it must carry the id Graph returned.
            assert_eq!(
                repo::sync_state::get(conn, "acc", "mail", "graph_remote:dst:5")?,
                Some("new-id".to_string())
            );
            // The source key pointed at the retired id; leaving it behind would
            // let a lookup 404 against the old folder.
            assert_eq!(
                repo::sync_state::get(conn, "acc", "mail", "graph_remote:src:5")?,
                None
            );
            Ok(())
        })
        .expect("remap");
    }

    #[test]
    fn rollback_restores_flags_folder_and_soft_delete() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::insert(conn, &folder("dst", "Archive", FolderRole::Archive))?;
            // Flag change happened optimistically (seen = true).
            repo::messages::upsert(conn, &message("m1", "src", 5, true))?;

            // 1. Flags: restore seen=false.
            let payload = SetFlagsOp {
                folder_id: "src".into(),
                uids: vec![5],
                seen: Some(true),
                flagged: None,
                answered: None,
                forwarded: None,
                previous: vec![FlagRollback {
                    uid: 5,
                    seen: false,
                    flagged: false,
                    answered: false,
                    forwarded: false,
                }],
            };
            rollback_in_db(
                conn,
                &pending(KIND_SET_FLAGS, serde_json::to_value(payload).expect("json")),
            )?;
            let stored = repo::messages::get(conn, "m1")?.expect("message");
            assert!(!stored.summary.seen, "flag rollback must restore seen");

            // 2. Move: the row is in the target folder; rollback moves it back.
            repo::messages::move_to_folder(conn, &["m1".to_string()], "dst")?;
            let payload = MoveOp {
                folder_id: "src".into(),
                target_folder_id: "dst".into(),
                uids: vec![5],
            };
            rollback_in_db(
                conn,
                &pending(KIND_MOVE, serde_json::to_value(payload).expect("json")),
            )?;
            let stored = repo::messages::get(conn, "m1")?.expect("message");
            assert_eq!(stored.summary.folder_id, "src");

            // 3. Soft delete: unhide the row.
            repo::messages::delete_local(conn, &["m1".to_string()], false)?;
            let payload = DeleteOp {
                folder_id: "src".into(),
                uids: vec![5],
                permanent: false,
                target_folder_id: None,
                soft_hidden: true,
            };
            rollback_in_db(
                conn,
                &pending(KIND_DELETE, serde_json::to_value(payload).expect("json")),
            )?;
            let stored = repo::messages::get(conn, "m1")?.expect("message");
            assert!(!stored.is_deleted_local, "soft delete must be reverted");
            Ok(())
        })
        .expect("rollback");
    }

    async fn context_with_account() -> CoreContext {
        let dir = std::env::temp_dir().join(format!(
            "nuncio-mail-executor-{}",
            nuncio_core::models::new_id()
        ));
        let ctx = CoreContext::for_test(nuncio_core::Paths::new(dir))
            .await
            .expect("ctx");
        ctx.db
            .write({
                let account = account();
                move |db| repo::accounts::insert(db, &account)
            })
            .await
            .expect("account");
        ctx
    }

    async fn state_of(ctx: &CoreContext, id: &str) -> PendingOpState {
        let id = id.to_string();
        ctx.db
            .read(move |db| repo::pending_ops::get(db, &id))
            .await
            .expect("read")
            .expect("op")
            .state
    }

    /// `pending_op` is shared with contacts, calendar and tasks. The mail
    /// worker used to claim every row for the account, fail `pim.*` with
    /// "unknown pending operation kind" (IMAP/Graph) or mark it done without
    /// contacting any server (POP3), which is why task edits and checklist
    /// steps never reached Microsoft To Do.
    #[tokio::test]
    async fn mail_queue_does_not_claim_pim_operations() {
        let ctx = context_with_account().await;
        let account = account();
        let queued = ctx
            .db
            .write(|db| {
                repo::pending_ops::enqueue(
                    db,
                    nuncio_core::models::NewPendingOp::new(
                        "acc",
                        "pim.task.save",
                        serde_json::json!({ "op": "save", "taskId": "task-1", "snapshot": null }),
                        "更新任务「测试todo」",
                    ),
                )
            })
            .await
            .expect("queue");

        let outcome = run_due_graph(&ctx, &account).await.expect("graph pass");
        assert_eq!(outcome.done, 0);
        assert_eq!(outcome.failed, 0);
        assert_eq!(outcome.retried, 0);
        let done = run_due_local(&ctx, &account).await.expect("local pass");
        assert_eq!(done, 0);
        assert_eq!(state_of(&ctx, &queued.id).await, PendingOpState::Pending);

        // A PIM row left `inProgress` by a crash belongs to the PIM worker;
        // mail must not reset it and grab it on the next pass.
        ctx.db
            .write({
                let id = queued.id.clone();
                move |db| {
                    db.execute(
                        "UPDATE pending_op SET state = 'inProgress', updated_at = ?2 WHERE id = ?1",
                        rusqlite::params![
                            id,
                            nuncio_core::repo::dt_string(Utc::now() - chrono::Duration::hours(1))
                        ],
                    )?;
                    Ok(())
                }
            })
            .await
            .expect("age");
        run_due_graph(&ctx, &account).await.expect("second pass");
        assert_eq!(state_of(&ctx, &queued.id).await, PendingOpState::InProgress);
    }
}
