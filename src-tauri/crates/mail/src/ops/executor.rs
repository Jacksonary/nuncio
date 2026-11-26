//! Execution of due `pending_op` rows on the account's worker connection.
//!
//! Operations are executed in creation order. Consecutive flag updates with
//! identical flags on the same folder are merged into a single `UID STORE`.
//! Failures retry with exponential backoff; permanent failures are reported via
//! `op://failed` and the affected folders are re-synchronized so local state
//! rolls back to the server's truth.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use async_imap::imap_proto::rfc4315::UidSetMember;
use async_imap::imap_proto::{Response, ResponseCode, Status};
use chrono::Utc;
use nuncio_core::models::{
    Account, Folder, FolderRole, PendingOp, PendingOpState, RemoteDraftIdentity, StoredMessage,
};
use nuncio_core::traits::TokenResource;
use nuncio_core::{events, repo, AppError, CoreContext, Result};
use rusqlite::OptionalExtension;
use tracing::{debug, info, warn};

use crate::graph::{ops as graph_ops, GraphMailClient};
use crate::imap::conn::{AppendedUid, ImapConnection};
use crate::ops::{
    confirmed_outcome, is_mail_kind, AppendDraftOp, AppendState, AppendToSentOp, ArchiveOp,
    CopyPhase, DeleteFlagPhase, DeleteOp, DeleteRemoteDraftOp, DraftRemovalState, ExpungePhase,
    FolderCreateOp, FolderDeleteOp, FolderMarkReadOp, FolderRenameOp, FolderSubscribeOp,
    MarkJunkOp, MoveOp, OpMessageOutcome, OpMessageState, RemoteDraftRef, SendOp, SetFlagsOp,
    KIND_APPEND_DRAFT, KIND_APPEND_TO_SENT, KIND_ARCHIVE, KIND_DELETE, KIND_DELETE_REMOTE_DRAFT,
    KIND_FOLDER_CREATE, KIND_FOLDER_DELETE, KIND_FOLDER_MARK_READ, KIND_FOLDER_RENAME,
    KIND_FOLDER_SUBSCRIBE, KIND_MARK_JUNK, KIND_MOVE, KIND_SEND, KIND_SET_FLAGS,
};
use crate::util::{draft_message_id, imap_error, uid_set};

const MAX_ATTEMPTS: u32 = 8;
const MAX_OPS_PER_PASS: usize = 64;

/// Result of the live checks a terminal failure performs before rolling back.
///
/// `uidvalidity` is the generation the server actually reports for the op's
/// source folder (a real SELECT, never the cached value). `present_uids` is the
/// subset of the payload's UIDs that a `UID SEARCH` still finds; `None` means
/// the check could not run, so no UID may be trusted.
/// What a targeted verification of an uncertain Graph delete proved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphDeleteVerification {
    /// The attributed id is unreachable (404). That is not proof the previous
    /// delete applied: a moved message also changes id, so the outcome must
    /// stay unknown.
    Gone,
    /// The attributed id is still in the folder it was deleted from.
    Present,
    /// No safe answer. The outcome stays `Unknown` and the command is not
    /// repeated.
    Unverified,
}

#[derive(Debug, Default, Clone)]
struct LiveSourceState {
    uidvalidity: Option<i64>,
    present_uids: Option<HashSet<i64>>,
    /// The account executes through Microsoft Graph. Graph rows keep their
    /// opaque id and synthetic UID across a refused command, so their rollback
    /// must not drop the identity the IMAP path has to reconcile.
    graph: bool,
}

#[derive(Debug, Default)]
pub struct ExecOutcome {
    pub done: usize,
    pub retried: usize,
    pub failed: usize,
    /// Operations deliberately left queued for another pass (an uncertain
    /// APPEND kept for verification, or one a discard settled before it could
    /// run). Not an error, and not a completed operation either.
    pub deferred: usize,
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
            // A row that had a persisted send intent may have reached the
            // server before the crash. Resetting it to a plain `pending` row
            // would let undo cancel it as "never sent"; keep the uncertainty
            // durable instead.
            db.execute(
                "UPDATE pending_op SET state = 'pending',
                        outcome_unknown = CASE
                            WHEN send_intent = 1 OR outcome_unknown = 1 THEN 1 ELSE 0 END
                 WHERE account_id = ?1 AND state = 'inProgress' AND updated_at < ?2
                   AND kind NOT LIKE 'pim.%'",
                rusqlite::params![account_id, nuncio_core::repo::dt_string(cutoff)],
            )?;
            Ok(())
        })
        .await
}

/// Persist the intent to issue this operation's wire command.
///
/// Called immediately before the first destructive command, after every local
/// validation. A crash or a transport error afterwards therefore cannot look
/// like an operation that never started.
async fn mark_send_intent(ctx: &CoreContext, op_id: &str) -> Result<()> {
    let op_id = op_id.to_string();
    ctx.db
        .write(move |db| repo::pending_ops::mark_send_intent(db, &op_id))
        .await?;
    Ok(())
}

async fn mark_send_intents(ctx: &CoreContext, op_ids: &[String]) -> Result<()> {
    let op_ids = op_ids.to_vec();
    ctx.db
        .write(move |db| {
            for id in op_ids {
                repo::pending_ops::mark_send_intent(db, &id)?;
            }
            Ok(())
        })
        .await?;
    Ok(())
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
        resume_deferred_undo(ctx).await;
        return Ok(ExecOutcome::default());
    }

    let mut outcome = ExecOutcome::default();
    let mut selected: Option<SelectedFolder> = None;
    let mut index = 0;
    while index < due.len() {
        let op = &due[index];
        if op.kind == KIND_SET_FLAGS {
            // Merge consecutive flag ops targeting the same folder and flags.
            let Ok(first) = repo::pending_ops::payload::<SetFlagsOp>(op) else {
                let _ = fail_op(
                    ctx,
                    op,
                    AppError::invalid_input("invalid setFlags payload"),
                    LiveSourceState::default(),
                )
                .await?;
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
                    // UIDs from different generations must never be batched:
                    // one op's UIDVALIDITY cannot vouch for another op's UIDs.
                    && candidate.source_uidvalidity == first.source_uidvalidity
                {
                    batch.push((due[next].clone(), candidate));
                    next += 1;
                } else {
                    break;
                }
            }
            let batch_ids: Vec<String> = batch.iter().map(|(op, _)| op.id.clone()).collect();
            let claimed = claim_ops(ctx, &batch_ids).await?;
            let batch: Vec<(PendingOp, SetFlagsOp)> = if claimed.len() == batch.len() {
                batch
            } else {
                // A concurrent undo/cancel settled some rows. Execute only the
                // ones this worker actually claimed.
                batch
                    .into_iter()
                    .filter(|(op, _)| claimed.iter().any(|id| id == &op.id))
                    .collect()
            };
            if batch.is_empty() {
                index = next;
                continue;
            }
            let batch_ids: Vec<String> = batch.iter().map(|(op, _)| op.id.clone()).collect();
            match execute_set_flags(ctx, conn, account, &mut selected, &batch).await {
                Ok(()) => {
                    outcome.done += finish_ops(ctx, &batch_ids).await?;
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
                            None,
                            &mut outcome,
                            LiveSourceState::default(),
                        )
                        .await?;
                    }
                }
            }
            index = next;
            continue;
        }

        let op = op.clone();
        if claim_ops(ctx, std::slice::from_ref(&op.id))
            .await?
            .is_empty()
        {
            // Cancelled between listing and claiming; do not touch the server.
            index += 1;
            continue;
        }
        let result = execute_one(ctx, conn, account, &mut selected, &op).await;
        match result {
            Ok(()) => {
                if finish_ops(ctx, std::slice::from_ref(&op.id)).await? > 0 {
                    outcome.done += 1;
                } else {
                    // The executor deliberately left the row queued (uncertain
                    // APPEND verification) or a discard claimed it mid-flight.
                    outcome.deferred += 1;
                }
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
                if op.kind == KIND_DELETE {
                    if let Ok(payload) = repo::pending_ops::payload::<DeleteOp>(&op) {
                        if let Some(target) = payload.target_folder_id {
                            outcome.resync_folders.insert(target);
                        }
                    }
                }
                if op.kind == KIND_DELETE_REMOTE_DRAFT {
                    if let Ok(payload) = repo::pending_ops::payload::<DeleteRemoteDraftOp>(&op) {
                        outcome.resync_folders.insert(payload.folder_id);
                    }
                }
                if matches!(op.kind.as_str(), KIND_MOVE | KIND_ARCHIVE | KIND_MARK_JUNK) {
                    if let Ok(payload) = repo::pending_ops::payload::<MoveOp>(&op) {
                        outcome.resync_folders.insert(payload.target_folder_id);
                    }
                    if let Ok(payload) = repo::pending_ops::payload::<ArchiveOp>(&op) {
                        outcome.resync_folders.insert(payload.folder_id.clone());
                        // The optimistic projection moved the row into the
                        // target; only that folder's sync can bind its new UID.
                        if let Some(target) = payload.target_folder_id {
                            outcome.resync_folders.insert(target);
                        }
                    }
                    if let Ok(payload) = repo::pending_ops::payload::<MarkJunkOp>(&op) {
                        outcome.resync_folders.insert(payload.folder_id.clone());
                        if let Some(target) = payload.target_folder_id {
                            outcome.resync_folders.insert(target);
                        }
                    }
                }
            }
            Err(error) => {
                outcome.resync_folders.extend(op_folders(&op));
                // Only a terminal failure rolls back. Live SELECT/UID SEARCH
                // results (never the cached generation) decide whether the
                // source UIDs may be written back or only reconciled.
                let live = if failure_is_terminal(&op, &error) {
                    select_live_source_state(ctx, conn, &mut selected, &op).await
                } else {
                    LiveSourceState::default()
                };
                handle_failure(
                    ctx,
                    account,
                    &op,
                    &error,
                    TokenResource::Mail,
                    None,
                    &mut outcome,
                    live,
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
    resume_deferred_undo(ctx).await;
    Ok(outcome)
}

/// Continue undo requests that were waiting for an operation to settle.
///
/// Runs after every queue pass: a `Deferred` request whose operations are now
/// `done`/`failed` can finish its per-message compensation. Failures are only
/// logged — the request stays persisted and the next pass retries it.
async fn resume_deferred_undo(ctx: &CoreContext) {
    if let Err(error) = crate::service::undo::resume_deferred_undo_requests(ctx).await {
        debug!(%error, "could not resume deferred undo requests");
    }
}

async fn execute_one(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &Account,
    selected: &mut Option<SelectedFolder>,
    op: &PendingOp,
) -> Result<()> {
    // Clear early dispatch: Microsoft accounts execute the same queued
    // operations through Graph.  The IMAP implementation below is untouched.
    if crate::graph::uses_graph(account) {
        return execute_graph_op(ctx, account, &GraphMailClient::new(), op).await;
    }
    if account.imap.is_none() {
        return apply_local_only(ctx, op).await;
    }
    match op.kind.as_str() {
        KIND_SET_FLAGS => {
            let payload: SetFlagsOp = repo::pending_ops::payload(op)?;
            execute_set_flags(ctx, conn, account, selected, &[(op.clone(), payload)]).await
        }
        KIND_MOVE => {
            let payload: MoveOp = repo::pending_ops::payload(op)?;
            if payload.folder_id == payload.target_folder_id {
                return Ok(());
            }
            let mapping = execute_move(
                ctx,
                conn,
                account,
                selected,
                &payload.folder_id,
                &payload.target_folder_id,
                &payload.uids,
                &payload.message_ids,
                payload.source_uidvalidity,
                Some(&op.id),
            )
            .await?;
            if let Some(mapping) = mapping.as_ref() {
                // COPYUID is authoritative: bind the optimistic projection
                // now instead of letting a later Message-ID match guess.
                apply_transfer_uids(
                    ctx,
                    &account.id,
                    &payload.target_folder_id,
                    &payload.message_ids,
                    &payload.uids,
                    mapping,
                )
                .await?;
            }
            record_move_outcomes(
                ctx,
                &op.id,
                &payload.message_ids,
                &payload.uids,
                &payload.target_folder_id,
                mapping.as_ref(),
            )
            .await?;
            remove_local_messages(ctx, &payload.folder_id, &payload.uids).await
        }
        KIND_DELETE => {
            let payload: DeleteOp = repo::pending_ops::payload(op)?;
            execute_delete(ctx, conn, account, selected, op, &payload).await
        }
        KIND_ARCHIVE => {
            let payload: ArchiveOp = repo::pending_ops::payload(op)?;
            execute_archive(ctx, conn, account, selected, op, &payload).await
        }
        KIND_MARK_JUNK => {
            let payload: MarkJunkOp = repo::pending_ops::payload(op)?;
            execute_mark_junk(ctx, conn, account, selected, op, &payload).await
        }
        KIND_APPEND_DRAFT => {
            // The listing that produced `op` predates the claim. A discard or
            // a phase update may have landed since; resuming from that stale
            // snapshot would upload or clean up against an old intent. Read the
            // durable row again inside the claimed window.
            let latest = load_pending_op(ctx, &op.id).await?;
            let Some(latest) = latest else {
                return Ok(());
            };
            if latest.state != PendingOpState::InProgress {
                return Ok(());
            }
            let payload: AppendDraftOp = repo::pending_ops::payload(&latest)?;
            execute_append_draft(ctx, conn, selected, account, &latest, &payload).await
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
            .map(|_| ())
        }
        KIND_DELETE_REMOTE_DRAFT => {
            let payload: DeleteRemoteDraftOp = repo::pending_ops::payload(op)?;
            if payload.quarantined {
                // Same guard as the Graph path: an isolated intent must never
                // be turned into a server command.
                return Err(AppError::unsupported(
                    "quarantined deleteRemoteDraft is never executed",
                ));
            }
            execute_delete_remote_draft(ctx, conn, selected, &account.id, &payload).await
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
            execute_folder_delete(ctx, conn, selected, &payload).await
        }
        KIND_FOLDER_SUBSCRIBE => {
            let payload: FolderSubscribeOp = repo::pending_ops::payload(op)?;
            execute_folder_subscribe(ctx, conn, &payload).await
        }
        KIND_FOLDER_MARK_READ => {
            let payload: FolderMarkReadOp = repo::pending_ops::payload(op)?;
            execute_folder_mark_read(ctx, conn, selected, &op.id, &payload).await
        }
        KIND_SEND => {
            let payload: SendOp = repo::pending_ops::payload(op)?;
            crate::smtp::outbox::process_one(ctx, account, &payload.outbox_id)
                .await
                .map(|_| ())
        }
        other => Err(AppError::unsupported(format!(
            "unknown pending operation kind: {other}"
        ))),
    }
}

/* ───────────────────────── Graph operations ───────────────────────── */

/// Drop the local projection(s) a confirmed Graph delete just removed.
///
/// The successful (or 404 "already gone") response is evidence about this exact
/// Graph id, so the rows are removed directly instead of waiting for a folder
/// scan. That matters when the account has a non-zero sync window: clearing the
/// delta cursor forces a full resync, but the scan is `$filter`ed by date, so a
/// draft older than the window is never returned and the scan could never prove
/// the projection was gone. Its absence from the window is *not* evidence of
/// deletion.
///
/// Rows are matched only by the confirmed `gm_msgid` in this folder/account;
/// the payload's `messageId` is consulted too but only when it still names the
/// same Graph id. When nothing matches, the projection is left for a later
/// sync instead of being deleted by inference.
///
/// Mapping cleanup is independent of the projection: the receipt is about the
/// Graph id, so every `graph_remote:*` mapping to it is retired even when no
/// local row could be found. Without this a leftover mapping (for example from
/// a draft whose row was already removed) would let a later queued operation
/// resolve and act on an id the server has already confirmed as gone.
async fn confirm_graph_remote_draft_removal(
    ctx: &CoreContext,
    account_id: &str,
    payload: &DeleteRemoteDraftOp,
    remote_id: &str,
) -> Result<()> {
    let account = account_id.to_string();
    let folder_id = payload.folder_id.clone();
    let remote = remote_id.to_string();
    let projection = payload.message_id.clone();
    let removed = ctx
        .db
        .write(move |db| {
            // Resolve projection rows only when the folder still belongs to
            // this account; a legacy payload may name another mailbox.
            let mut ids: Vec<String> = Vec::new();
            if let Some(folder) = repo::folders::get(db, &folder_id)? {
                if folder.account_id == account {
                    {
                        let mut statement = db.prepare(
                            "SELECT id FROM message WHERE folder_id = ?1 AND gm_msgid = ?2",
                        )?;
                        let mut rows = statement.query(rusqlite::params![folder_id, remote])?;
                        while let Some(row) = rows.next()? {
                            ids.push(row.get(0)?);
                        }
                    }
                    if let Some(id) = projection {
                        if !ids.contains(&id) {
                            if let Some(stored) = repo::messages::get(db, &id)? {
                                if stored.summary.account_id == account
                                    && stored.summary.folder_id == folder_id
                                    && stored.gm_msgid.as_deref() == Some(remote.as_str())
                                {
                                    ids.push(id);
                                }
                            }
                        }
                    }
                }
            }

            // Every mapping to this retired Graph id is dead: a later queued
            // operation must not resolve it again. The key prefix is part of
            // the predicate so unrelated sync_state values are never touched;
            // GLOB (not LIKE) keeps `_` literal.
            let mappings = db.execute(
                "DELETE FROM sync_state
                  WHERE account_id = ?1 AND module = 'mail'
                    AND key GLOB 'graph_remote:*' AND value = ?2",
                rusqlite::params![account, remote],
            )?;
            if mappings > 0 {
                debug!(
                    account = %account,
                    remote_id = %remote,
                    mappings,
                    "dropped retired Graph remote-id mappings"
                );
            }

            if ids.is_empty() {
                return Ok(ids);
            }
            repo::messages::delete_local(db, &ids, true)?;
            Ok(ids)
        })
        .await?;
    if !removed.is_empty() {
        events::emit_mail_changed(
            ctx.events.as_ref(),
            account_id.to_string(),
            vec![payload.folder_id.clone()],
            removed,
            events::MailChangeKind::Deleted,
        );
    }
    Ok(())
}

/// Execute one queued operation through Microsoft Graph.
///
/// The queued payloads are provider-neutral, but Graph's data model differs
/// (no IMAP UIDs, new message ids after a move, `sendMail` files Sent copies
/// itself), so each kind is translated here.  Every call reads or writes the
/// same local rows the IMAP executor uses, which is what lets undo, resync and
/// failure rollback stay shared.
async fn execute_graph_op(
    ctx: &CoreContext,
    account: &Account,
    client: &GraphMailClient,
    op: &PendingOp,
) -> Result<()> {
    match op.kind.as_str() {
        KIND_SET_FLAGS => {
            let payload: SetFlagsOp = repo::pending_ops::payload(op)?;
            if payload.uids.is_empty() {
                return Ok(());
            }
            // The folder (and the provider id resolved inside it) identifies a
            // copy only within the account that owns the folder. A payload
            // queued under a different account — a reply sent from B that
            // targeted a message in A — must never be executed with B's
            // credentials, or B's unrelated message gets PATCHed.
            let folder = folder_by_id(ctx, &payload.folder_id).await?;
            if folder.account_id != account.id {
                return Err(AppError::unsupported(format!(
                    "setFlags folder {} belongs to account {}, not the executing account {}",
                    folder.id, folder.account_id, account.id
                )));
            }
            // Graph has no \Answered or $Forwarded message flag; an operation
            // that only touches those has nothing to send and completes.
            if payload.seen.is_none() && payload.flagged.is_none() {
                return Ok(());
            }
            // Graph applies one PATCH per message. A later action may have
            // taken a field over while this operation sat in backoff; skip
            // those messages instead of writing the stale value.
            let mut targets: Vec<(i64, Option<String>, Option<String>)> = Vec::new();
            let mut skipped: Vec<(String, String)> = Vec::new();
            for (index, uid) in payload.uids.iter().enumerate() {
                let owned = match payload.message_ids.get(index) {
                    Some(message_id) => {
                        flag_claim_owned_for(ctx, &op.id, message_id, &payload).await?
                    }
                    // A payload without message ids predates the writer table:
                    // it keeps the legacy permission.
                    None if payload.message_ids.is_empty() => true,
                    // Per-message ids exist but not for this uid. That is not a
                    // legacy payload; there is no evidence for this target.
                    None => false,
                };
                if owned {
                    // A pinned provider id names the exact copy the action was
                    // queued against. Re-resolving the synthetic UID would
                    // address whatever the mutable mapping names after a sync
                    // rebind.
                    let pinned = payload
                        .remote_ids
                        .get(index)
                        .map(|value| value.trim())
                        .filter(|value| !value.is_empty())
                        .map(str::to_string);
                    targets.push((*uid, pinned, payload.message_ids.get(index).cloned()));
                } else {
                    if let Some(message_id) = payload.message_ids.get(index) {
                        skipped.push((op.id.clone(), message_id.clone()));
                    }
                    debug!(
                        op = %op.id,
                        uid,
                        "Graph setFlags: no durable writer proves this operation still owns the field; skipping the stale write"
                    );
                }
            }
            record_set_flags_not_executed(ctx, &skipped).await?;
            if targets.is_empty() {
                return Ok(());
            }
            // Persist the send intent before the first request: a failure
            // after some messages were updated is an ambiguous batch, never
            // "nothing was sent".
            mark_send_intent(ctx, &op.id).await?;
            // The batch check above ran before the first PATCH. Each Graph
            // PATCH may block on the network for a long time, so the evidence
            // for the next message can disappear while its predecessor is in
            // flight: a sync may delete the projection (dropping its writer) or
            // a newer action may take the field over. Claim every message
            // again immediately before addressing it, and record a definitive
            // `NotExecuted` for the ones this operation may no longer write.
            for (uid, pinned, message_id) in targets {
                // Evidence from an *earlier* attempt outranks this run. A
                // message this operation already PATCHed must never be
                // rewritten, and a later skip must not turn its success into
                // "not executed" (undo would then skip remote compensation).
                let prior = match message_id.as_deref() {
                    Some(message_id) => graph_message_outcome(ctx, &op.id, message_id).await?,
                    None => None,
                };
                let prior_state = prior.as_ref().map(|outcome| outcome.state);
                if prior_state == Some(OpMessageState::Confirmed) {
                    debug!(
                        op = %op.id,
                        uid,
                        "Graph setFlags: this message was already confirmed; not resending"
                    );
                    continue;
                }
                if let Some(message_id) = message_id.as_deref() {
                    if !flag_claim_owned_for(ctx, &op.id, message_id, &payload).await? {
                        record_set_flags_not_executed(
                            ctx,
                            &[(op.id.clone(), message_id.to_string())],
                        )
                        .await?;
                        debug!(
                            op = %op.id,
                            uid,
                            "Graph setFlags: writer/ownership changed before this message was sent; skipping the stale write"
                        );
                        continue;
                    }
                }
                let remote = match pinned {
                    Some(remote) => remote,
                    None => graph_remote_id(ctx, &account.id, &payload.folder_id, uid).await?,
                };
                // Only an `Unknown` written by *this* attempt may later become
                // `NotExecuted`. A prior attempt's `Unknown`/`Confirmed` stays:
                // it may already have reached the server.
                let replace_this_attempts_unknown = !matches!(
                    prior_state,
                    Some(OpMessageState::Unknown | OpMessageState::Confirmed)
                );
                let result = match message_id.as_deref() {
                    Some(message_id) => {
                        // Re-check writer/ownership *after* the token has been
                        // read and immediately before each actual request. A
                        // 401 makes `send_authed` wait for a refreshed token and
                        // retry; the evidence may have changed during that wait,
                        // and the retry must be skipped rather than sent.
                        //
                        // The final check and the take-off marker share one
                        // write transaction: a projection delete or a writer
                        // hand-over that commits between a read and a later
                        // write must not be able to authorise a stale PATCH.
                        // Only a send that passed the check may claim the
                        // marker; a skip records no `Unknown`, and an earlier
                        // attempt's evidence is preserved by the merge rules.
                        graph_ops::set_flags_checked(
                            client,
                            ctx,
                            &account.id,
                            &remote,
                            payload.seen,
                            payload.flagged,
                            || async {
                                claim_graph_flag_send_for(
                                    ctx,
                                    &op.id,
                                    message_id,
                                    &payload,
                                    uid,
                                    replace_this_attempts_unknown,
                                )
                                .await
                            },
                        )
                        .await
                    }
                    // Legacy payload without message ids: no writer evidence to
                    // re-check, so it keeps the old unconditional send.
                    None => graph_ops::set_flags(
                        client,
                        ctx,
                        &account.id,
                        &remote,
                        payload.seen,
                        payload.flagged,
                    )
                    .await
                    .map(|()| true),
                };
                match result {
                    Ok(true) => {
                        if let Some(message_id) = message_id.as_deref() {
                            record_set_flags_outcome(
                                ctx,
                                &op.id,
                                message_id,
                                uid,
                                OpMessageState::Confirmed,
                                replace_this_attempts_unknown,
                            )
                            .await?;
                        }
                    }
                    Ok(false) => {
                        if let Some(message_id) = message_id.as_deref() {
                            record_set_flags_outcome(
                                ctx,
                                &op.id,
                                message_id,
                                uid,
                                OpMessageState::NotExecuted,
                                replace_this_attempts_unknown,
                            )
                            .await?;
                        }
                        debug!(
                            op = %op.id,
                            uid,
                            "Graph setFlags: writer/ownership changed after the token was read; skipping the stale write"
                        );
                    }
                    Err(error) => {
                        if let Some(message_id) = message_id.as_deref() {
                            // A definitive refusal explains only this attempt.
                            // If an earlier attempt is still unknown, keep it.
                            let state = if prior_state == Some(OpMessageState::Unknown)
                                || graph_outcome_unknown(&error)
                            {
                                OpMessageState::Unknown
                            } else {
                                OpMessageState::NotExecuted
                            };
                            record_set_flags_outcome(
                                ctx,
                                &op.id,
                                message_id,
                                uid,
                                state,
                                replace_this_attempts_unknown,
                            )
                            .await?;
                        }
                        return Err(error);
                    }
                }
            }
            Ok(())
        }
        KIND_MOVE => {
            let payload: MoveOp = repo::pending_ops::payload(op)?;
            let target = folder_by_id(ctx, &payload.target_folder_id).await?;
            graph_move_messages(
                ctx,
                account,
                client,
                Some(&op.id),
                &payload.folder_id,
                &target,
                &payload.uids,
                &payload.message_ids,
                &payload.remote_ids,
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
                // A soft delete must never fall through to `permanentDelete`.
                // Without a Trash folder the message would become
                // unrecoverable, while the undo entry is still modelled as a
                // reversible delete. Wait for the folder sync to discover one
                // (the operation stays queued) instead of upgrading it.
                let Some(target) = target else {
                    return Err(AppError::service(
                        "Graph soft delete has no Trash folder; refusing to upgrade it to a permanent delete",
                    ));
                };
                if target.id == payload.folder_id {
                    // The service models a delete from Trash as permanent. A
                    // payload that still says otherwise must not be upgraded
                    // silently, or undo would claim it can restore the message.
                    return Err(AppError::unsupported(
                        "Graph soft delete: the message is already in Trash; refusing to upgrade it to a permanent delete",
                    ));
                }
                return graph_move_messages(
                    ctx,
                    account,
                    client,
                    Some(&op.id),
                    &payload.folder_id,
                    &target,
                    &payload.uids,
                    &payload.message_ids,
                    &[],
                )
                .await;
            }
            let mut current = payload.clone();
            mark_send_intent(ctx, &op.id).await?;
            // The batch is settled per message: a failure after an earlier
            // success must not make undo/rollback treat the whole batch as
            // rolled back, nor may a missing local row be read as "deleted".
            for (index, uid) in payload.uids.iter().enumerate() {
                let message_id = payload
                    .message_ids
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| format!("uid:{uid}"));
                if confirmed_outcome(&current.outcomes, &message_id).is_some() {
                    // A previous attempt already deleted this message.
                    continue;
                }
                // An earlier attempt may have sent this delete without
                // recording its result. A later definitive refusal only proves
                // that *this* attempt did not run; it cannot retract the
                // earlier unknown one, so the evidence below is kept.
                let prior = current
                    .outcomes
                    .iter()
                    .find(|outcome| outcome.message_id == message_id)
                    .cloned();
                let prior_unknown = prior
                    .as_ref()
                    .is_some_and(|outcome| outcome.state == OpMessageState::Unknown);
                // The id a previous attempt actually sent. Verification on a
                // retry must use this exact id: re-resolving the UID may name a
                // different message after a sync rebound the mapping, and a
                // missing/changed mapping must never negate the earlier send.
                let persisted_id = prior
                    .as_ref()
                    .and_then(|outcome| outcome.attempt_remote_id.clone())
                    .filter(|value| !value.trim().is_empty());
                // The provider id pinned in the queue transaction. A Graph
                // sync may rebind the stable local row (and its synthetic UID)
                // to another message, so re-resolving the UID must never
                // decide which copy gets deleted.
                let pinned_id = payload
                    .remote_ids
                    .get(index)
                    .map(|value| value.trim())
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                if let (Some(pinned), Some(sent)) = (pinned_id.as_deref(), persisted_id.as_deref())
                {
                    if pinned != sent {
                        // Two durable identities disagree. Neither may be sent
                        // or verified automatically; reconcile the folder.
                        record_delete_outcome(
                            ctx,
                            &op.id,
                            &mut current,
                            &message_id,
                            *uid,
                            OpMessageState::Unknown,
                            Some(sent),
                        )
                        .await?;
                        return Err(AppError::conflict(
                            "Graph delete identity changed; the message must be reconciled",
                        ));
                    }
                }
                let used_persisted_id = persisted_id.is_some() || pinned_id.is_some();
                let stored = ctx
                    .db
                    .read({
                        let message_id = message_id.clone();
                        move |db| repo::messages::get(db, &message_id)
                    })
                    .await?;
                if let Some(pinned) = pinned_id.as_deref() {
                    if !prior_unknown
                        && !graph_pinned_id_is_current(
                            ctx,
                            &account.id,
                            &payload.folder_id,
                            stored.as_ref(),
                            *uid,
                            pinned,
                        )
                        .await?
                    {
                        // The row now names a different provider message. Do
                        // not send the old id blindly: keep the row hidden and
                        // let a folder reconciliation decide what it is.
                        record_delete_outcome(
                            ctx,
                            &op.id,
                            &mut current,
                            &message_id,
                            *uid,
                            OpMessageState::Unknown,
                            None,
                        )
                        .await?;
                        return Err(AppError::conflict(
                            "Graph delete identity changed; refusing to delete a different copy",
                        ));
                    }
                }
                let chosen_id = pinned_id.clone().or(persisted_id.clone());
                let remote = match chosen_id {
                    Some(remote) => remote,
                    None => {
                        // Neither the queue transaction nor a previous attempt
                        // named a provider id. Re-resolving the synthetic UID
                        // would address whatever the mutable mapping points at
                        // now, which may be a different message after a sync
                        // rebind. Never send a permanent delete on that basis;
                        // keep the row hidden and let reconciliation decide.
                        record_delete_outcome(
                            ctx,
                            &op.id,
                            &mut current,
                            &message_id,
                            *uid,
                            OpMessageState::Unknown,
                            None,
                        )
                        .await?;
                        return Err(AppError::conflict(
                            "Graph permanent delete has no pinned provider id; refusing to delete an unproven copy",
                        ));
                    }
                };
                // A 404 is only evidence about this reader's identity when the
                // id is genuinely attributable to the row. A local row id used
                // as a last-resort remote id does not qualify. An id that a
                // previous attempt actually sent is attributable by definition.
                let attributed = if prior_unknown && used_persisted_id {
                    true
                } else {
                    match stored.as_ref() {
                        Some(stored) => {
                            graph_delete_id_is_attributed(
                                ctx,
                                &account.id,
                                &payload.folder_id,
                                stored,
                                &remote,
                            )
                            .await?
                        }
                        None => {
                            graph_mapped_remote_id(ctx, &account.id, &payload.folder_id, *uid)
                                .await?
                                .as_deref()
                                == Some(remote.as_str())
                        }
                    }
                };
                if prior_unknown {
                    // Never repeat the command blindly: a targeted GET must
                    // first prove the exact id either gone or still in the
                    // folder it was requested from.
                    let expected_folder = match folder_by_id(ctx, &payload.folder_id).await {
                        Ok(folder) => {
                            crate::sync::folders::graph_folder_id(
                                ctx,
                                &account.id,
                                &folder.remote_path,
                            )
                            .await?
                        }
                        Err(_) => None,
                    };
                    let verified = if !attributed {
                        GraphDeleteVerification::Unverified
                    } else {
                        match graph_ops::message_parent_folder(client, ctx, &account.id, &remote)
                            .await?
                        {
                            graph_ops::MessageFolderState::Gone => GraphDeleteVerification::Gone,
                            graph_ops::MessageFolderState::InFolder { folder_id, .. }
                                if expected_folder.as_deref() == Some(folder_id.as_str()) =>
                            {
                                GraphDeleteVerification::Present
                            }
                            graph_ops::MessageFolderState::InFolder { .. }
                            | graph_ops::MessageFolderState::Unverified => {
                                GraphDeleteVerification::Unverified
                            }
                        }
                    };
                    match verified {
                        GraphDeleteVerification::Gone => {
                            // A 404 proves only that this id is unreachable: the
                            // message may have moved to a new Graph id, so the
                            // copy can still exist. Keep the unknown evidence
                            // and do not settle the operation on it.
                            record_delete_outcome(
                                ctx,
                                &op.id,
                                &mut current,
                                &message_id,
                                *uid,
                                OpMessageState::Unknown,
                                Some(remote.as_str()),
                            )
                            .await?;
                            return Err(AppError::service(
                                "Graph delete id is unreachable; keeping the outcome unknown because the message may have moved",
                            ));
                        }
                        GraphDeleteVerification::Present => {
                            // The exact id is still in its folder, so the
                            // earlier request did not take effect *yet*. This
                            // attempt may run, but it must not erase the fact
                            // that the first request is still unaccounted for:
                            // a definitive refusal below stays `Unknown`.
                        }
                        GraphDeleteVerification::Unverified => {
                            // The previous request may or may not have applied
                            // and the id cannot be tied to this target. Keep
                            // the unknown outcome instead of re-sending.
                            return Err(AppError::service(
                                "Graph delete outcome is unknown; refusing to resend until the exact id is verified",
                            ));
                        }
                    }
                }
                // Persist the take-off marker and the exact id this attempt is
                // about to send, before the wire call.
                record_delete_outcome(
                    ctx,
                    &op.id,
                    &mut current,
                    &message_id,
                    *uid,
                    OpMessageState::Unknown,
                    Some(remote.as_str()),
                )
                .await?;
                match graph_ops::permanent_delete_message(client, ctx, &account.id, &remote).await {
                    Ok(graph_ops::PermanentDeleteOutcome::Deleted) => {
                        record_delete_outcome(
                            ctx,
                            &op.id,
                            &mut current,
                            &message_id,
                            *uid,
                            OpMessageState::Confirmed,
                            Some(remote.as_str()),
                        )
                        .await?;
                    }
                    Ok(graph_ops::PermanentDeleteOutcome::NotFound) => {
                        // A 404 is "the id is unreachable", not a delete
                        // receipt: the message may have moved to a new id.
                        // Persist the ambiguity and let the retry's targeted
                        // verification settle it; never hard-delete local rows
                        // on this answer alone.
                        record_delete_outcome(
                            ctx,
                            &op.id,
                            &mut current,
                            &message_id,
                            *uid,
                            OpMessageState::Unknown,
                            Some(remote.as_str()),
                        )
                        .await?;
                        return Err(AppError::service(
                            "Graph permanentDelete reported the id as unreachable; verifying before settling",
                        ));
                    }
                    Err(error) => {
                        // A definitive refusal explains only this attempt. If
                        // an earlier attempt is still unknown, keep it so.
                        let state = if prior_unknown || graph_outcome_unknown(&error) {
                            OpMessageState::Unknown
                        } else {
                            OpMessageState::NotExecuted
                        };
                        record_delete_outcome(
                            ctx,
                            &op.id,
                            &mut current,
                            &message_id,
                            *uid,
                            state,
                            (state == OpMessageState::Unknown).then_some(remote.as_str()),
                        )
                        .await?;
                        return Err(error);
                    }
                }
            }
            confirm_permanent_delete(ctx, account, &current).await
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
                    graph_ops::create_folder(client, ctx, &account.id, "Archive", None).await?;
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
                client,
                Some(&op.id),
                &payload.folder_id,
                &target,
                &payload.uids,
                &payload.message_ids,
                &payload.remote_ids,
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
                client,
                Some(&op.id),
                &payload.folder_id,
                &target,
                &payload.uids,
                &payload.message_ids,
                &payload.remote_ids,
            )
            .await
        }
        KIND_APPEND_DRAFT => {
            // The listing that produced `op` predates the claim. A discard or
            // a phase update may have landed since; resuming from that stale
            // snapshot would create (or clean up) against an old intent. Read
            // the durable row again inside the claimed window, exactly like the
            // IMAP path.
            let latest = load_pending_op(ctx, &op.id).await?;
            let Some(latest) = latest else {
                return Ok(());
            };
            if latest.state != PendingOpState::InProgress {
                return Ok(());
            }
            let payload: AppendDraftOp = repo::pending_ops::payload(&latest)?;
            execute_graph_append_draft(ctx, account, client, &latest, &payload).await
        }
        KIND_APPEND_TO_SENT => {
            // Graph saves the Sent copy as part of sendMail; appending again
            // would create a duplicate.
            Ok(())
        }
        KIND_DELETE_REMOTE_DRAFT => {
            // The first half of a Graph replacement pair (see
            // `compose::enqueue_draft_sync_ops`). Deleting the old remote copy
            // before the APPEND is accepted semantics: if the upload fails, the
            // local draft is still the source of truth and its APPEND retries.
            let payload: DeleteRemoteDraftOp = repo::pending_ops::payload(op)?;
            if payload.quarantined {
                // An unattributable legacy delete is isolated, never sent: its
                // target may be a copy the user deliberately keeps.
                return Err(AppError::unsupported(
                    "quarantined deleteRemoteDraft is never executed",
                ));
            }
            // A receipt from this client's own upload names the exact Graph
            // message; use it directly instead of resolving a synthetic UID
            // through a mapping that a projection cleanup may have removed.
            let resolved = match payload.remote_message_id.clone() {
                Some(remote) => Some(remote),
                None => {
                    graph_remote_id_opt(ctx, &account.id, &payload.folder_id, payload.uid).await?
                }
            };
            match resolved {
                Some(remote) => {
                    graph_ops::delete_message(client, ctx, &account.id, &remote).await?;
                    // The success response is direct evidence about this exact
                    // Graph id. Drop the projection now instead of relying on
                    // a window-limited full scan, which can never return a
                    // draft older than the sync window.
                    confirm_graph_remote_draft_removal(ctx, &account.id, &payload, &remote).await
                }
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
                client,
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
            graph_ops::rename_folder(client, ctx, &account.id, &folder_remote, &payload.new_name)
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
            graph_ops::delete_folder(client, ctx, &account.id, &folder_remote).await?;
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
            // Only rows whose durable `seen` writer is still this operation may
            // be written. A newer single-message "mark unread" must survive a
            // retried folder operation, so when any row was taken over the
            // all-or-nothing folder endpoint cannot be used.
            let targets = {
                let folder_id = payload.folder_id.clone();
                let op_id = op.id.clone();
                ctx.db
                    .read(move |db| repo::messages::folder_read_targets(db, &folder_id, &op_id))
                    .await?
            };
            let all_owned = targets.iter().all(|(_, _, _, owned)| *owned);
            if all_owned {
                let folder_remote = graph_folder_remote_id(ctx, &account.id, &folder).await?;
                graph_ops::mark_folder_read(client, ctx, &account.id, &folder_remote).await?;
            } else {
                mark_send_intent(ctx, &op.id).await?;
                for (_, uid, _, owned) in &targets {
                    if !*owned {
                        debug!(
                            op = %op.id,
                            uid = ?uid,
                            "folderMarkRead: a later action owns this row; skipping the stale write"
                        );
                        continue;
                    }
                    let Some(uid) = uid else { continue };
                    let remote =
                        graph_remote_id(ctx, &account.id, &payload.folder_id, *uid).await?;
                    graph_ops::set_flags(client, ctx, &account.id, &remote, Some(true), None)
                        .await?;
                }
            }
            let folder_id = folder.id.clone();
            let op_id = op.id.clone();
            ctx.db
                .write(move |db| {
                    // Local settlement is owner-limited too: a row a later
                    // action made unread stays unread.
                    repo::messages::mark_read_owned(db, &folder_id, &op_id)?;
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

/* ──────────────────── Graph draft APPEND lifecycle ─────────────────── */

/// Execute a Graph draft upload with the same phase discipline as IMAP.
///
/// Graph has no `APPENDUID`; the MIME create response returns the message id,
/// which is the equivalent receipt. The phase markers are persisted on the
/// operation exactly like IMAP so a discard/send that lands while the request
/// is on the wire does not cancel an upload that may already have created a
/// remote copy:
///
/// * `Idle` -> `Unknown` is the send right, committed before the POST. A
///   keep-local that wins that race makes the claim fail and nothing is sent.
/// * A successful create records `Acked` plus `appendedRemoteId` before any
///   local row is touched.
/// * `Unknown` on a retry never repeats the create: a lost completion cannot be
///   attributed, so the copy is kept (same rule as IMAP without `APPENDUID`).
async fn execute_graph_append_draft(
    ctx: &CoreContext,
    account: &Account,
    client: &GraphMailClient,
    op: &PendingOp,
    payload: &AppendDraftOp,
) -> Result<()> {
    if payload.discard_requested && payload.cleanup_done {
        return Ok(());
    }
    // A folder id from another account is a stale/cross-account binding. Two
    // mailboxes can share a Drafts path, so the create would land in the wrong
    // mailbox.
    let folder = folder_by_id(ctx, &payload.folder_id).await?;
    if folder.account_id != account.id {
        return Err(AppError::protocol(format!(
            "draft folder {} belongs to another account; refusing the Graph upload",
            folder.remote_path
        )));
    }
    match payload.append_state {
        // Graph has no delete-before-append phase of its own; `Preparing` is
        // treated like `Idle` (both prove no literal was written).
        AppendState::Idle | AppendState::Preparing => {
            // Read and validate the staged bytes *before* claiming the send
            // right: a missing/empty file must not leave the operation stuck in
            // `Unknown` with nothing uploaded.
            let bytes = read_staged_message(&payload.rfc822_path).await?;
            if !mark_graph_append_unknown(ctx, &op.id).await? {
                debug!(
                    draft = %payload.draft_id,
                    "Graph append was cancelled or kept local before its create"
                );
                return Ok(());
            }
            let remote_id = match graph_ops::append_draft(client, ctx, &account.id, &bytes).await {
                Ok(remote_id) => remote_id,
                Err(error) => {
                    if graph_append_definitely_rejected(&error) {
                        // A response (4xx) proves the create did not commit;
                        // reset so the retry may send it again.
                        if let Err(reset) = reset_graph_append_idle(ctx, &op.id).await {
                            debug!(%reset, "could not reset the Graph APPEND phase");
                        }
                    } else {
                        // Transport/5xx, and a 2xx whose body was unreadable or
                        // carried no id, all leave the create's outcome unknown.
                        // The phase already stays `Unknown`; also persist the
                        // uncertainty itself so it survives a later terminal
                        // failure and keeps autosaves from POSTing a second
                        // copy.
                        if let Err(record) = record_append_payload(ctx, &op.id, |payload| {
                            if payload.outcome_unknown {
                                return false;
                            }
                            payload.outcome_unknown = true;
                            true
                        })
                        .await
                        {
                            debug!(%record, "could not record the uncertain Graph APPEND outcome");
                        }
                    }
                    return Err(error);
                }
            };
            // Persist the receipt before touching the projection: a commit
            // failure must not lose the only address of the created copy.
            record_graph_append_acked(ctx, &op.id, &remote_id).await?;
            persist_graph_appended_draft(ctx, &account.id, payload, &remote_id).await?;
            // Re-read the latest intent: a discard/send may have been recorded
            // while the request was on the wire.
            let latest = load_append_payload(ctx, &op.id).await?;
            if latest.is_some_and(|payload| payload.discard_requested) {
                return discard_graph_appended_copy(ctx, client, account, op, payload, &remote_id)
                    .await;
            }
            Ok(())
        }
        AppendState::Unknown => {
            // The create may have committed, but its completion (and therefore
            // its id) is lost. Repeating the POST could create a second copy,
            // and deleting by header inference is forbidden. Keep the record.
            if payload.discard_requested {
                if !draft_row_exists(ctx, &payload.draft_id).await? {
                    debug!(
                        draft = %payload.draft_id,
                        "unattributed Graph cleanup has no local draft left; keeping the remote copy"
                    );
                    mark_append_candidate_only(ctx, &op.id).await?;
                    mark_cleanup_done(ctx, &op.id).await?;
                    return Ok(());
                }
                debug!(
                    draft = %payload.draft_id,
                    "Graph create outcome is unknown; keeping the cleanup pending"
                );
            }
            defer_append(ctx, &op.id).await?;
            Ok(())
        }
        AppendState::Acked => {
            let Some(remote_id) = payload.appended_remote_id.clone() else {
                // An accepted create without a recorded id cannot be addressed.
                // Never guess; let the Drafts sync bind the copy by header and
                // settle as candidate-only.
                mark_append_candidate_only(ctx, &op.id).await?;
                return Ok(());
            };
            if payload.discard_requested {
                return discard_graph_appended_copy(ctx, client, account, op, payload, &remote_id)
                    .await;
            }
            // Retry of the local commit only; the create is never repeated.
            persist_graph_appended_draft(ctx, &account.id, payload, &remote_id).await?;
            Ok(())
        }
    }
}

/// Whether a Graph create error proves the request did not commit.
///
/// Graph answers 4xx with a status and body, so the server rejected the
/// request. Transport errors and 5xx/429 (`AppError::Service`) are ambiguous:
/// the create may already have run, so the phase stays `Unknown`.
fn graph_append_definitely_rejected(error: &AppError) -> bool {
    matches!(
        error,
        AppError::NotFound(_)
            | AppError::Auth(_)
            | AppError::Protocol(_)
            | AppError::InvalidInput(_)
            | AppError::Conflict(_)
            | AppError::Unsupported(_)
    )
}

/// Claim the Graph APPEND's send right: `Idle`/`Preparing` -> `Unknown`.
///
/// The claim commits in the same transaction as the ownership checks, exactly
/// like IMAP: a keep-local/discard that wins the race makes it fail and no
/// request is sent; one that lands afterwards cannot stop the already-authorized
/// command, only record its outcome.
async fn mark_graph_append_unknown(ctx: &CoreContext, op_id: &str) -> Result<bool> {
    claim_append_phase(ctx, op_id, |payload| {
        if !matches!(
            payload.append_state,
            AppendState::Idle | AppendState::Preparing
        ) {
            return false;
        }
        payload.append_state = AppendState::Unknown;
        true
    })
    .await
}

/// Record the Graph create receipt, even if the row was cancelled while the
/// request was on the wire. The id is the address a cleanup needs.
async fn record_graph_append_acked(
    ctx: &CoreContext,
    op_id: &str,
    remote_id: &str,
) -> Result<bool> {
    let remote_id = remote_id.to_string();
    record_append_payload(ctx, op_id, move |payload| {
        payload.append_state = AppendState::Acked;
        payload.appended_remote_id = Some(remote_id);
        payload.appended_uid = None;
        payload.appended_uidvalidity = None;
        true
    })
    .await
}

/// Undo the phase marker after a create that provably did not commit.
async fn reset_graph_append_idle(ctx: &CoreContext, op_id: &str) -> Result<bool> {
    mutate_append_payload(ctx, op_id, |payload| {
        payload.append_state = AppendState::Idle;
        payload.appended_remote_id = None;
        payload.outcome_unknown = false;
        true
    })
    .await
}

/// Bind a Graph create receipt to the durable draft identity and projection.
///
/// The identity is the Graph analogue of an `APPENDUID` binding; the projection
/// row gets the same id plus the synthetic UID the sync derives from it, so a
/// later discard/send can resolve and delete exactly this copy without waiting
/// for a sync.
async fn persist_graph_appended_draft(
    ctx: &CoreContext,
    account_id: &str,
    payload: &AppendDraftOp,
    remote_id: &str,
) -> Result<()> {
    let identity = RemoteDraftIdentity::confirmed_graph(
        &payload.folder_id,
        remote_id,
        Some(draft_message_id(&payload.draft_id)),
    );
    let pin = pin_draft_remote_identity(ctx, account_id, &payload.draft_id, Some(identity)).await?;
    if matches!(pin, IdentityPin::Untrusted | IdentityPin::MissingDraft) {
        debug!(
            draft = %payload.draft_id,
            ?pin,
            "Graph create receipt was not bound to the local draft"
        );
    }

    let id = draft_message_id(&payload.draft_id);
    let account = account_id.to_string();
    let folder_id = payload.folder_id.clone();
    let remote = remote_id.to_string();
    let uid = crate::sync::messages::synthetic_uid(remote_id);
    let mapping = format!("graph_remote:{folder_id}:{uid}");
    let changed = ctx
        .db
        .write(move |db| {
            let changed = repo::messages::bind_graph_appended_draft(
                db, &id, &account, &folder_id, uid, &remote,
            )?;
            if changed > 0 {
                repo::sync_state::set(db, &account, "mail", &mapping, &remote)?;
            }
            Ok(changed)
        })
        .await?;
    if changed == 0 {
        debug!(
            draft = %payload.draft_id,
            folder = %payload.folder_id,
            "draft projection moved or disappeared; leaving the Graph receipt for a targeted sync"
        );
    }
    Ok(())
}

/// Finish the cleanup demanded by `discard_requested` for a created Graph copy.
///
/// The exact id returned by this client's create is the only evidence that may
/// authorize a delete. If a previous attempt's outcome is unknown, the id is
/// verified with a targeted GET first; otherwise the delete is issued and its
/// success is confirmed against the projection. An ambiguous failure keeps the
/// operation queued with `outcome_unknown`, leaves the projection intact, and
/// forces a full folder resync so the next pass can verify the exact id.
async fn discard_graph_appended_copy(
    ctx: &CoreContext,
    client: &GraphMailClient,
    account: &Account,
    op: &PendingOp,
    payload: &AppendDraftOp,
    remote_id: &str,
) -> Result<()> {
    // The caller's snapshot may predate the receipt/cleanup markers this
    // attempt just persisted (the create path records them and then calls this
    // function with the older payload). Read the durable row again.
    let payload = load_append_payload(ctx, &op.id)
        .await?
        .unwrap_or(payload.clone());
    let payload = &payload;
    // The delete may already be accepted even though the local settlement did
    // not commit. Settle locally only; never send a second `permanentDelete`.
    if payload.cleanup_acked {
        confirm_graph_deleted_draft(ctx, account, payload, remote_id).await?;
        clear_graph_draft_identity_if_matches(ctx, &payload.draft_id, remote_id).await?;
        mark_graph_cleanup_finished(ctx, &op.id).await?;
        return Ok(());
    }
    // A delete is only ever sent to an id this draft provably owns. A same-id
    // receipt alone does not say where that copy is now, so the exact id must
    // first be shown to still be a draft in the expected Drafts folder. Both
    // the first attempt and a retry obey this rule: a 404 proves only that the
    // old id is unreachable (a moved draft gets a new id), never that this
    // draft's copy was deleted.
    if !graph_cleanup_id_is_attributed(ctx, payload, remote_id).await? {
        debug!(
            draft = %payload.draft_id,
            remote_id,
            "Graph cleanup target is not attributed; not deleting"
        );
        defer_append(ctx, &op.id).await?;
        return Ok(());
    }
    let expected_folder = match folder_by_id(ctx, &payload.folder_id).await {
        Ok(folder) => {
            crate::sync::folders::graph_folder_id(ctx, &account.id, &folder.remote_path).await?
        }
        Err(_) => None,
    };
    let Some(expected_folder) = expected_folder else {
        debug!(
            draft = %payload.draft_id,
            remote_id,
            "Graph cleanup has no expected Drafts folder id; keeping the copy"
        );
        defer_append(ctx, &op.id).await?;
        return Ok(());
    };
    match graph_ops::message_parent_folder(client, ctx, &account.id, remote_id).await {
        Ok(graph_ops::MessageFolderState::InFolder {
            folder_id,
            is_draft: Some(true),
        }) if folder_id == expected_folder => {
            // Confirmed: this exact id is still a draft in the Drafts folder.
            // A previous uncertain delete therefore did not take effect, so a
            // fresh attempt may run. Its own okay/refusal is recorded below.
        }
        Ok(graph_ops::MessageFolderState::Gone) => {
            // The old id is unreachable. That is *not* a delete receipt: the
            // draft may have been moved and now live under a different id, so
            // settling here would hide a copy that still exists.
            debug!(
                draft = %payload.draft_id,
                remote_id,
                "Graph cleanup target is unreachable; keeping the outcome unknown"
            );
            defer_append(ctx, &op.id).await?;
            return Ok(());
        }
        Ok(graph_ops::MessageFolderState::InFolder { .. }) => {
            debug!(
                draft = %payload.draft_id,
                remote_id,
                "Graph cleanup target is not in the expected Drafts folder; keeping it"
            );
            defer_append(ctx, &op.id).await?;
            return Ok(());
        }
        Ok(graph_ops::MessageFolderState::Unverified) => {
            // A 200 without `parentFolderId`/`isDraft` proves neither presence
            // nor absence; deleting on this answer could erase another copy.
            debug!(
                draft = %payload.draft_id,
                remote_id,
                "Graph cleanup verification was inconclusive; keeping the projection"
            );
            defer_append(ctx, &op.id).await?;
            return Ok(());
        }
        Err(error) => {
            debug!(
                draft = %payload.draft_id,
                %error,
                "could not verify the Graph draft cleanup target"
            );
            return Err(error);
        }
    }
    if !claim_cleanup_phase(ctx, &op.id).await? {
        debug!(
            draft = %payload.draft_id,
            "Graph discard cleanup lost the claim to a keep-local; skipping the delete"
        );
        return Ok(());
    }
    match graph_ops::permanent_delete_message(client, ctx, &account.id, remote_id).await {
        Ok(graph_ops::PermanentDeleteOutcome::Deleted) => {
            // A 2xx is the provider's delete receipt. Persist it *before*
            // dropping the projection/mapping: a crash or failed local commit
            // must resume at the settlement, not re-send the delete.
            record_graph_cleanup_acked(ctx, &op.id).await?;
            confirm_graph_deleted_draft(ctx, account, payload, remote_id).await?;
            clear_graph_draft_identity_if_matches(ctx, &payload.draft_id, remote_id).await?;
            mark_graph_cleanup_finished(ctx, &op.id).await?;
            Ok(())
        }
        Ok(graph_ops::PermanentDeleteOutcome::NotFound) => {
            // 404 means the id is unreachable, not that the delete applied.
            // Keep the uncertainty (and the projection) and let the retry's
            // targeted GET decide whether the exact copy is still there.
            record_graph_cleanup_uncertain(ctx, &op.id).await?;
            if let Err(resync) = force_full_resync_db(ctx, &payload.folder_id).await {
                debug!(%resync, "could not queue the Graph draft folder resync");
            }
            Err(AppError::service(
                "Graph draft cleanup reported the id as unreachable; keeping the outcome unknown",
            ))
        }
        Err(error) => {
            // The delete may have reached the server. Record the uncertainty
            // durably, keep the projection, and let the retry verify the exact
            // id before trying again.
            record_graph_cleanup_uncertain(ctx, &op.id).await?;
            if let Err(resync) = force_full_resync_db(ctx, &payload.folder_id).await {
                debug!(%resync, "could not queue the Graph draft folder resync");
            }
            Err(error)
        }
    }
}

/// Persist an uncertain Graph draft-cleanup outcome without losing the claim.
async fn record_graph_cleanup_uncertain(ctx: &CoreContext, op_id: &str) -> Result<()> {
    record_append_payload(ctx, op_id, |payload| {
        if payload.outcome_unknown {
            return false;
        }
        payload.outcome_unknown = true;
        true
    })
    .await?;
    Ok(())
}

/// Record that the provider accepted the cleanup delete.
///
/// Like the APPEND receipt, this must commit before the local projection and
/// mappings are touched.
async fn record_graph_cleanup_acked(ctx: &CoreContext, op_id: &str) -> Result<()> {
    record_append_payload(ctx, op_id, |payload| {
        if payload.cleanup_acked {
            return false;
        }
        payload.cleanup_acked = true;
        true
    })
    .await?;
    Ok(())
}

/// Whether `remote_id` is this draft's attributable Graph copy.
///
/// Only the id returned by this operation's own create (`appendedRemoteId`) or
/// the identity pinned when the user explicitly opened that exact server row
/// may authorize a delete. A same-header copy or a plain `Some(folder)` answer
/// is never enough.
async fn graph_cleanup_id_is_attributed(
    ctx: &CoreContext,
    payload: &AppendDraftOp,
    remote_id: &str,
) -> Result<bool> {
    if payload.appended_remote_id.as_deref() == Some(remote_id) {
        return Ok(true);
    }
    let draft_id = payload.draft_id.clone();
    let remote = remote_id.to_string();
    ctx.db
        .read(move |db| {
            let Some(row) = repo::drafts::get(db, &draft_id)? else {
                return Ok(false);
            };
            Ok(row.remote_draft.as_ref().is_some_and(|identity| {
                identity.is_deletable_graph()
                    && identity.remote_id.as_deref() == Some(remote.as_str())
            }))
        })
        .await
}

/// Confirm a Graph draft delete against the exact provider id.
///
/// Reuses the remote-draft confirmation so only rows/mappings that carried this
/// id are dropped; a projection a sync rebound to a newer copy survives.
async fn confirm_graph_deleted_draft(
    ctx: &CoreContext,
    account: &Account,
    payload: &AppendDraftOp,
    remote_id: &str,
) -> Result<()> {
    let delete = DeleteRemoteDraftOp {
        folder_id: payload.folder_id.clone(),
        uid: 0,
        remote_message_id: Some(remote_id.to_string()),
        draft_id: Some(payload.draft_id.clone()),
        source_uidvalidity: None,
        message_id: Some(draft_message_id(&payload.draft_id)),
        outcome_unknown: false,
        quarantined: false,
        quarantine_reason: None,
    };
    confirm_graph_remote_draft_removal(ctx, &account.id, &delete, remote_id).await
}

/// Record a finished Graph cleanup and retire the unknown-outcome marker.
async fn mark_graph_cleanup_finished(ctx: &CoreContext, op_id: &str) -> Result<()> {
    record_append_payload(ctx, op_id, |payload| {
        let changed = payload.outcome_unknown || !payload.cleanup_done;
        payload.outcome_unknown = false;
        payload.cleanup_done = true;
        changed
    })
    .await?;
    Ok(())
}

/// Clear the pinned identity only while it still names the deleted Graph id.
async fn clear_graph_draft_identity_if_matches(
    ctx: &CoreContext,
    draft_id: &str,
    remote_id: &str,
) -> Result<()> {
    let id = draft_id.to_string();
    let remote = remote_id.to_string();
    ctx.db
        .write(move |db| {
            let Some(row) = repo::drafts::get(db, &id)? else {
                return Ok(());
            };
            let matches = row
                .remote_draft
                .as_ref()
                .and_then(|identity| identity.remote_id.as_deref())
                == Some(remote.as_str());
            if matches {
                repo::drafts::set_remote_draft(db, &id, None)?;
            }
            Ok(())
        })
        .await?;
    Ok(())
}

/// The Graph id a `(folder, uid)` mapping names, without falling back to the
/// local row id.
///
/// The fallback in [`graph_remote_id_opt`] assumes a synced row's id *is* its
/// provider id. That holds for rows written by the Graph sync, but a local-only
/// row can have an id the provider has never seen. Verification evidence (a
/// 404, or the id used to retry a delete) must therefore come from an explicit
/// provider identity or a mapping, never from that assumption.
async fn graph_mapped_remote_id(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    uid: i64,
) -> Result<Option<String>> {
    let key = format!("graph_remote:{folder_id}:{uid}");
    let account = account_id.to_string();
    let exact = {
        let key = key.clone();
        ctx.db
            .read(move |db| repo::sync_state::get(db, &account, "mail", &key))
            .await?
    };
    if let Some(remote) = exact.filter(|value| !value.trim().is_empty()) {
        return Ok(Some(remote));
    }
    let pattern = format!("graph_remote:%:{uid}");
    let account = account_id.to_string();
    ctx.db
        .read(move |db| {
            Ok(db
                .query_row(
                    "SELECT value FROM sync_state
                     WHERE account_id = ?1 AND module = 'mail' AND key LIKE ?2
                       AND value <> ''
                     ORDER BY updated_at DESC LIMIT 1",
                    rusqlite::params![account, pattern],
                    |row| row.get::<_, String>(0),
                )
                .optional()?)
        })
        .await
}

/// Whether a pinned provider id still names this row's current copy.
///
/// The pinned id is the only identity a delete may send. If the live mapping
/// (or, without one, the row's own Graph id) names a different message, the
/// row was rebound and the command must be refused instead of deleting the
/// wrong copy.
async fn graph_pinned_id_is_current(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    stored: Option<&StoredMessage>,
    uid: i64,
    pinned: &str,
) -> Result<bool> {
    if let Some(mapped) = graph_mapped_remote_id(ctx, account_id, folder_id, uid).await? {
        return Ok(mapped == pinned);
    }
    if let Some(stored) = stored {
        if let Some(remote) = stored
            .gm_msgid
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            return Ok(remote == pinned);
        }
    }
    // No evidence at all: the queue transaction's check has already been
    // invalidated, so a blind delete is not safe.
    Ok(false)
}

/// Whether `remote_id` is a provider identity attributable to `stored`.
///
/// A 404 is only deletion evidence when the id came from the row's own Graph
/// id or from a `graph_remote` mapping. A local row id used as a last-resort
/// remote id does not qualify.
async fn graph_delete_id_is_attributed(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    stored: &StoredMessage,
    remote_id: &str,
) -> Result<bool> {
    if stored
        .gm_msgid
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty() && value == remote_id)
    {
        return Ok(true);
    }
    let Some(uid) = stored.uid else {
        return Ok(false);
    };
    Ok(graph_mapped_remote_id(ctx, account_id, folder_id, uid)
        .await?
        .as_deref()
        == Some(remote_id))
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
#[allow(clippy::too_many_arguments)]
async fn graph_move_messages(
    ctx: &CoreContext,
    account: &Account,
    client: &GraphMailClient,
    op_id: Option<&str>,
    source_folder_id: &str,
    target: &Folder,
    uids: &[i64],
    message_ids: &[String],
    remote_ids: &[String],
) -> Result<()> {
    if uids.is_empty() {
        return Ok(());
    }
    // Mirror the IMAP executor: a move into the folder the message already
    // lives in is a no-op.  It also keeps the remap below from deleting the
    // mapping it just wrote when source and destination are the same folder.
    if target.id == source_folder_id {
        if let Some(op_id) = op_id {
            for (index, uid) in uids.iter().enumerate() {
                if let Some(message_id) = message_ids.get(index) {
                    record_graph_move_outcome(
                        ctx,
                        op_id,
                        message_id,
                        *uid,
                        target.id.clone(),
                        OpMessageState::NotExecuted,
                        None,
                        Some(*uid),
                    )
                    .await?;
                }
            }
        }
        return Ok(());
    }
    let target_id = graph_folder_remote_id(ctx, &account.id, target).await?;
    if let Some(op_id) = op_id {
        mark_send_intent(ctx, op_id).await?;
    }
    let mut moved: Vec<(i64, String)> = Vec::with_capacity(uids.len());
    for (index, uid) in uids.iter().enumerate() {
        let message_id = message_ids
            .get(index)
            .cloned()
            .unwrap_or_else(|| format!("uid:{uid}"));
        // A previous attempt may already have moved this message. The
        // destination id is persisted with the outcome; never move it twice.
        // Any other prior state is kept as-is below: a definitive refusal for
        // *this* attempt cannot retract an earlier `Unknown` one.
        let prior_state = if let Some(op_id) = op_id {
            match graph_message_outcome(ctx, op_id, &message_id).await? {
                Some(prior) if prior.state == OpMessageState::Confirmed => {
                    if let Some(new_id) = prior.target_remote_id {
                        // A receipt recorded by an older build may only live
                        // in the operation payload. Mirror it onto the
                        // projection row before this operation can be pruned.
                        persist_move_target_identity(
                            ctx,
                            &account.id,
                            &message_id,
                            &target.id,
                            &new_id,
                            op_id,
                        )
                        .await?;
                        moved.push((*uid, new_id));
                    }
                    continue;
                }
                Some(prior) => Some(prior.state),
                None => None,
            }
        } else {
            None
        };
        if prior_state == Some(OpMessageState::Unknown) {
            // A previous attempt may already have moved this message. Repeating
            // the POST would either address a stale id (404) or move the copy
            // again, and a definitive answer for *this* attempt could not
            // retract the earlier unknown one. Gate the whole operation until
            // the terminal recovery can establish the real folder.
            return Err(AppError::service(
                "Graph move outcome is unknown; refusing to send it again",
            ));
        }
        // An inverse queued by undo names the exact Graph id the original
        // operation returned; preferring it avoids re-resolving a UID that a
        // later sync may have rebound to a different id.
        let direct = remote_ids
            .get(index)
            .filter(|remote| !remote.trim().is_empty())
            .cloned();
        let remote = match match direct {
            Some(remote) => Ok(remote),
            None => graph_remote_id(ctx, &account.id, source_folder_id, *uid).await,
        } {
            Ok(remote) => remote,
            Err(error) => {
                if let Some(op_id) = op_id {
                    // A missing mapping now proves only that this attempt
                    // cannot run; an earlier attempt may already have moved
                    // the message, so keep its `Unknown` verdict.
                    let state = if prior_state == Some(OpMessageState::Unknown) {
                        OpMessageState::Unknown
                    } else {
                        OpMessageState::NotExecuted
                    };
                    record_graph_move_outcome(
                        ctx,
                        op_id,
                        &message_id,
                        *uid,
                        target.id.clone(),
                        state,
                        None,
                        Some(*uid),
                    )
                    .await?;
                }
                return Err(error);
            }
        };
        if let Some(op_id) = op_id {
            record_graph_move_outcome(
                ctx,
                op_id,
                &message_id,
                *uid,
                target.id.clone(),
                OpMessageState::Unknown,
                None,
                None,
            )
            .await?;
        }
        match graph_ops::move_message(client, ctx, &account.id, &remote, &target_id).await {
            Ok(new_id) => {
                let target_uid = crate::sync::messages::synthetic_uid(&new_id);
                if let Some(op_id) = op_id {
                    record_graph_move_outcome(
                        ctx,
                        op_id,
                        &message_id,
                        *uid,
                        target.id.clone(),
                        OpMessageState::Confirmed,
                        Some(new_id.clone()),
                        Some(target_uid),
                    )
                    .await?;
                }
                moved.push((*uid, new_id));
            }
            Err(error) => {
                // A clear refusal explains *this* attempt only. If a previous
                // attempt is still `Unknown`, it may have moved the message;
                // never replace that evidence with `NotExecuted`.
                let state = if prior_state == Some(OpMessageState::Unknown)
                    || graph_outcome_unknown(&error)
                {
                    OpMessageState::Unknown
                } else {
                    OpMessageState::NotExecuted
                };
                if let Some(op_id) = op_id {
                    record_graph_move_outcome(
                        ctx,
                        op_id,
                        &message_id,
                        *uid,
                        target.id.clone(),
                        state,
                        None,
                        None,
                    )
                    .await?;
                }
                return Err(error);
            }
        }
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

/// Record one Graph move outcome and the destination id mapping in one write.
///
/// The mapping under both the source UID and the synthetic destination UID
/// makes the moved copy addressable before the next folder sync; undo uses that
/// identity to build its inverse MOVE.
#[allow(clippy::too_many_arguments)]
async fn record_graph_move_outcome(
    ctx: &CoreContext,
    op_id: &str,
    message_id: &str,
    source_uid: i64,
    target_folder_id: String,
    state: OpMessageState,
    target_remote_id: Option<String>,
    target_uid: Option<i64>,
) -> Result<()> {
    let op_id = op_id.to_string();
    let message_id = message_id.to_string();
    ctx.db
        .write(move |db| {
            let Some(op) = repo::pending_ops::get(db, &op_id)? else {
                return Ok(());
            };
            if state == OpMessageState::Confirmed {
                // Persist the receipt on the projection row in this same
                // transaction. Housekeeping prunes the operation queue after a
                // week; a destination the delta has not reported yet must
                // stay attributable even then.
                if let Some(remote) = target_remote_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    repo::messages::set_move_target_identity(
                        db,
                        &op.account_id,
                        &message_id,
                        &target_folder_id,
                        remote,
                        &op.id,
                        op.sequence,
                    )?;
                }
                if let (Some(uid), Some(remote)) = (target_uid, target_remote_id.as_deref()) {
                    let key = format!("graph_remote:{target_folder_id}:{uid}");
                    repo::sync_state::set(db, &op.account_id, "mail", &key, remote)?;
                    let alias = format!("graph_remote:{target_folder_id}:{source_uid}");
                    repo::sync_state::set(db, &op.account_id, "mail", &alias, remote)?;
                }
            }
            let outcome = OpMessageOutcome {
                message_id,
                source_uid: Some(source_uid),
                state,
                target_folder_id: Some(target_folder_id),
                target_uid,
                target_uidvalidity: None,
                target_remote_id,
                attempt_remote_id: None,
                copy_phase: Default::default(),
                delete_flag_phase: Default::default(),
                expunge_phase: Default::default(),
            };
            store_outcome_in_payload(db, &op, outcome)?;
            Ok(())
        })
        .await
}

/// Mirror a confirmed move destination onto its projection row.
///
/// Kept separate from [`record_graph_move_outcome`] for the retry path: the
/// outcome may already be durable while the row still lacks the identity (an
/// operation recorded by a build that predates the column).
async fn persist_move_target_identity(
    ctx: &CoreContext,
    account_id: &str,
    message_id: &str,
    target_folder_id: &str,
    target_remote_id: &str,
    op_id: &str,
) -> Result<()> {
    let account_id = account_id.to_string();
    let message_id = message_id.to_string();
    let target_folder_id = target_folder_id.to_string();
    let target_remote_id = target_remote_id.to_string();
    let op_id = op_id.to_string();
    ctx.db
        .write(move |db| {
            // The retry of an earlier batch must never replace a newer trip's
            // destination. The write compares the row's owner (and its queue
            // sequence) with this operation; read the sequence in the same
            // transaction so the comparison cannot race a queue rewrite.
            let Some(op) = repo::pending_ops::get(db, &op_id)? else {
                return Ok(());
            };
            repo::messages::set_move_target_identity(
                db,
                &account_id,
                &message_id,
                &target_folder_id,
                &target_remote_id,
                &op.id,
                op.sequence,
            )?;
            Ok(())
        })
        .await
}

/// Read back the outcome a previous attempt recorded for one Graph message.
///
/// Used both to skip a confirmed move and to avoid downgrading an earlier
/// `Unknown` result when the retry only learns about *this* attempt.
async fn graph_message_outcome(
    ctx: &CoreContext,
    op_id: &str,
    message_id: &str,
) -> Result<Option<OpMessageOutcome>> {
    let op_id = op_id.to_string();
    let message_id = message_id.to_string();
    ctx.db
        .read(move |db| {
            let Some(op) = repo::pending_ops::get(db, &op_id)? else {
                return Ok(None);
            };
            payload_outcome(&op, &message_id)
        })
        .await
}

/// Outcomes carried by a move-style operation payload.
fn outcomes_from_op(op: &PendingOp) -> Result<Vec<OpMessageOutcome>> {
    let outcomes = match op.kind.as_str() {
        KIND_MOVE => repo::pending_ops::payload::<MoveOp>(op)?.outcomes,
        KIND_ARCHIVE => repo::pending_ops::payload::<ArchiveOp>(op)?.outcomes,
        KIND_MARK_JUNK => repo::pending_ops::payload::<MarkJunkOp>(op)?.outcomes,
        KIND_DELETE => repo::pending_ops::payload::<DeleteOp>(op)?.outcomes,
        KIND_SET_FLAGS => repo::pending_ops::payload::<SetFlagsOp>(op)?.outcomes,
        _ => Vec::new(),
    };
    Ok(outcomes)
}

/// Serialize a complete outcome list back into the operation payload.
fn outcomes_payload_value(
    op: &PendingOp,
    outcomes: Vec<OpMessageOutcome>,
) -> Result<serde_json::Value> {
    let value = match op.kind.as_str() {
        KIND_MOVE => {
            let mut payload: MoveOp = repo::pending_ops::payload(op)?;
            payload.outcomes = outcomes;
            serde_json::to_value(payload)
        }
        KIND_ARCHIVE => {
            let mut payload: ArchiveOp = repo::pending_ops::payload(op)?;
            payload.outcomes = outcomes;
            serde_json::to_value(payload)
        }
        KIND_MARK_JUNK => {
            let mut payload: MarkJunkOp = repo::pending_ops::payload(op)?;
            payload.outcomes = outcomes;
            serde_json::to_value(payload)
        }
        KIND_DELETE => {
            let mut payload: DeleteOp = repo::pending_ops::payload(op)?;
            payload.outcomes = outcomes;
            serde_json::to_value(payload)
        }
        KIND_SET_FLAGS => {
            let mut payload: SetFlagsOp = repo::pending_ops::payload(op)?;
            payload.outcomes = outcomes;
            serde_json::to_value(payload)
        }
        _ => return Ok(op.payload.clone()),
    }
    .map_err(|error| AppError::internal(format!("cannot encode operation outcomes: {error}")))?;
    Ok(value)
}

/// Replace this message's outcome inside the operation payload.
fn store_outcome_in_payload(
    db: &rusqlite::Connection,
    op: &PendingOp,
    outcome: OpMessageOutcome,
) -> Result<()> {
    let mut outcomes = outcomes_from_op(op)?;
    outcomes.retain(|existing| existing.message_id != outcome.message_id);
    outcomes.push(outcome);
    let value = outcomes_payload_value(op, outcomes)?;
    repo::pending_ops::update_payload(db, &op.id, &value)?;
    Ok(())
}

/// The outcome a payload already carries for one message.
fn payload_outcome(op: &PendingOp, message_id: &str) -> Result<Option<OpMessageOutcome>> {
    Ok(outcomes_from_op(op)?
        .into_iter()
        .find(|outcome| outcome.message_id == message_id))
}

/// One per-message phase transition of the IMAP move fallback.
#[derive(Debug, Clone)]
enum MovePhaseUpdate {
    /// A tagged COPY refusal proved the copy never happened.
    CopyNotStarted,
    /// A COPY is on the wire but its completion was not received.
    ///
    /// The destination folder and the UIDVALIDITY observed *before* the
    /// command are pinned here. A later attempt must keep addressing that
    /// folder/generation even if the account's role folders changed.
    CopyUnknown {
        target_folder_id: Option<String>,
        target_uidvalidity: Option<i64>,
    },
    /// The server accepted the COPY. `target_uid` is `None` when the receipt
    /// did not name it; `target_uidvalidity` is still the generation the
    /// destination was pinned to, even before its UID is known.
    CopyKnown {
        target_uid: Option<i64>,
        target_uidvalidity: Option<i64>,
        target_folder_id: Option<String>,
    },
    /// Keep an already proven COPY but record that its destination cannot be
    /// verified yet. The phase is preserved so a later attempt can still
    /// resolve the identity without ever re-sending the COPY.
    VerifyPending,
    /// Record the destination folder before any destructive phase. Used to
    /// backfill payloads written by a build that did not pin it yet; it never
    /// rewrites a folder a receipt is already attached to.
    BindTarget {
        target_folder_id: String,
    },
    FlagUnknown,
    FlagApplied,
    ExpungeUnknown,
    ExpungeApplied,
}

impl MovePhaseUpdate {
    fn apply(self, outcome: &mut OpMessageOutcome) {
        // A confirmed receipt is final; a retry may learn nothing new about
        // this message except that it is done.
        if outcome.state == OpMessageState::Confirmed {
            return;
        }
        match self {
            MovePhaseUpdate::CopyNotStarted => {
                outcome.copy_phase = CopyPhase::NotStarted;
                outcome.target_uid = None;
                outcome.target_uidvalidity = None;
                if outcome.delete_flag_phase == DeleteFlagPhase::NotStarted
                    && outcome.expunge_phase == ExpungePhase::NotStarted
                {
                    outcome.state = OpMessageState::NotExecuted;
                }
            }
            MovePhaseUpdate::CopyUnknown {
                target_folder_id,
                target_uidvalidity,
            } => {
                outcome.copy_phase = CopyPhase::Unknown;
                // The pin is written together with the take-off marker. Once
                // recorded it is evidence about where this operation went, so
                // a later role change must not replace it.
                if let Some(folder_id) = target_folder_id {
                    outcome.target_folder_id.get_or_insert(folder_id);
                }
                if let Some(uidvalidity) = target_uidvalidity {
                    outcome.target_uidvalidity = Some(uidvalidity);
                }
                outcome.state = OpMessageState::Unknown;
            }
            MovePhaseUpdate::CopyKnown {
                target_uid,
                target_uidvalidity,
                target_folder_id,
            } => {
                outcome.copy_phase = CopyPhase::Copied;
                if target_uid.is_some() {
                    outcome.target_uid = target_uid;
                }
                if target_uidvalidity.is_some() {
                    // The generation is authoritative for this destination and
                    // is kept even while the UID itself is still unknown.
                    outcome.target_uidvalidity = target_uidvalidity;
                }
                if let Some(folder_id) = target_folder_id {
                    outcome.target_folder_id.get_or_insert(folder_id);
                }
                outcome.state = OpMessageState::Unknown;
            }
            MovePhaseUpdate::VerifyPending => {
                // Never downgrade the phase: the COPY is known to have
                // completed, only its destination receipt is still missing.
                outcome.state = OpMessageState::Unknown;
            }
            MovePhaseUpdate::BindTarget { target_folder_id } => {
                outcome.target_folder_id.get_or_insert(target_folder_id);
            }
            MovePhaseUpdate::FlagUnknown => {
                outcome.delete_flag_phase = DeleteFlagPhase::Unknown;
                outcome.state = OpMessageState::Unknown;
            }
            MovePhaseUpdate::FlagApplied => {
                outcome.delete_flag_phase = DeleteFlagPhase::Applied;
                outcome.state = OpMessageState::Unknown;
            }
            MovePhaseUpdate::ExpungeUnknown => {
                outcome.expunge_phase = ExpungePhase::Unknown;
                outcome.state = OpMessageState::Unknown;
            }
            MovePhaseUpdate::ExpungeApplied => {
                outcome.expunge_phase = ExpungePhase::Applied;
                outcome.state = OpMessageState::Unknown;
            }
        }
    }
}

/// Persist a batch of per-message phase transitions in one transaction.
async fn record_move_phases(
    ctx: &CoreContext,
    op_id: &str,
    updates: Vec<(String, Option<i64>, MovePhaseUpdate)>,
    mark_intent: bool,
) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }
    let op_id = op_id.to_string();
    ctx.db
        .write(move |db| {
            let Some(op) = repo::pending_ops::get(db, &op_id)? else {
                return Ok(());
            };
            if mark_intent {
                repo::pending_ops::mark_send_intent(db, &op.id)?;
            }
            let mut outcomes = outcomes_from_op(&op)?;
            for (message_id, source_uid, update) in updates {
                let index = outcomes
                    .iter()
                    .position(|outcome| outcome.message_id == message_id);
                let outcome = match index {
                    Some(index) => &mut outcomes[index],
                    None => {
                        outcomes.push(OpMessageOutcome {
                            message_id,
                            source_uid,
                            state: OpMessageState::Unknown,
                            target_folder_id: None,
                            target_uid: None,
                            target_uidvalidity: None,
                            target_remote_id: None,
                            attempt_remote_id: None,
                            copy_phase: CopyPhase::NotStarted,
                            delete_flag_phase: DeleteFlagPhase::NotStarted,
                            expunge_phase: ExpungePhase::NotStarted,
                        });
                        outcomes.last_mut().expect("just pushed")
                    }
                };
                if outcome.source_uid.is_none() {
                    outcome.source_uid = source_uid;
                }
                update.apply(outcome);
            }
            let value = outcomes_payload_value(&op, outcomes)?;
            repo::pending_ops::update_payload(db, &op.id, &value)?;
            Ok(())
        })
        .await
}

async fn record_delete_outcome(
    ctx: &CoreContext,
    op_id: &str,
    payload: &mut DeleteOp,
    message_id: &str,
    source_uid: i64,
    state: OpMessageState,
    attempt_remote_id: Option<&str>,
) -> Result<()> {
    payload
        .outcomes
        .retain(|outcome| outcome.message_id != message_id);
    payload.outcomes.push(OpMessageOutcome {
        message_id: message_id.to_string(),
        source_uid: Some(source_uid),
        state,
        target_folder_id: None,
        target_uid: None,
        target_uidvalidity: None,
        target_remote_id: None,
        attempt_remote_id: attempt_remote_id.map(str::to_string),
        copy_phase: Default::default(),
        delete_flag_phase: Default::default(),
        expunge_phase: Default::default(),
    });
    let value = serde_json::to_value(&*payload)
        .map_err(|error| AppError::internal(format!("cannot encode delete outcomes: {error}")))?;
    let op_id = op_id.to_string();
    ctx.db
        .write(move |db| repo::pending_ops::update_payload(db, &op_id, &value))
        .await?;
    Ok(())
}

/// Record the confirmed results of an IMAP move-style command.
async fn record_move_outcomes(
    ctx: &CoreContext,
    op_id: &str,
    message_ids: &[String],
    source_uids: &[i64],
    target_folder_id: &str,
    mapping: Option<&TransferUids>,
) -> Result<()> {
    if message_ids.is_empty() {
        return Ok(());
    }
    let op_id = op_id.to_string();
    let message_ids = message_ids.to_vec();
    let source_uids = source_uids.to_vec();
    let target_folder_id = target_folder_id.to_string();
    let mapping = mapping.cloned();
    ctx.db
        .write(move |db| {
            let Some(op) = repo::pending_ops::get(db, &op_id)? else {
                return Ok(());
            };
            // A fallback retry may have persisted a COPYUID identity on an
            // earlier attempt. The final receipt must keep it instead of
            // dropping it because this attempt did not copy the message again.
            let mut outcomes = outcomes_from_op(&op)?;
            for (index, message_id) in message_ids.iter().enumerate() {
                let source_uid = source_uids.get(index).copied();
                let previous = outcomes
                    .iter()
                    .find(|outcome| &outcome.message_id == message_id)
                    .cloned();
                let mapped = source_uid.and_then(|uid| {
                    mapping.as_ref().and_then(|mapping| {
                        mapping
                            .pairs
                            .iter()
                            .find(|(source, _)| *source == uid)
                            .map(|(_, target)| *target)
                    })
                });
                let target_uid =
                    mapped.or(previous.as_ref().and_then(|outcome| outcome.target_uid));
                let target_uidvalidity = if mapped.is_some() {
                    mapping.as_ref().map(|mapping| mapping.target_uidvalidity)
                } else {
                    previous
                        .as_ref()
                        .and_then(|outcome| outcome.target_uidvalidity)
                };
                // The receipt and the folder it was recorded against travel
                // together. A retry resolves `target_id` from the pinned
                // outcome; the caller's current role folder must not rewrite
                // an A receipt as B just because the role moved.
                let effective_target = previous
                    .as_ref()
                    .and_then(|outcome| outcome.target_folder_id.clone())
                    .unwrap_or_else(|| target_folder_id.clone());
                outcomes.retain(|outcome| &outcome.message_id != message_id);
                outcomes.push(OpMessageOutcome {
                    message_id: message_id.clone(),
                    source_uid,
                    state: OpMessageState::Confirmed,
                    target_folder_id: Some(effective_target),
                    target_uid,
                    target_uidvalidity,
                    target_remote_id: None,
                    attempt_remote_id: None,
                    copy_phase: previous
                        .as_ref()
                        .map(|outcome| outcome.copy_phase)
                        .unwrap_or_default(),
                    delete_flag_phase: previous
                        .as_ref()
                        .map(|outcome| outcome.delete_flag_phase)
                        .unwrap_or_default(),
                    expunge_phase: previous
                        .as_ref()
                        .map(|outcome| outcome.expunge_phase)
                        .unwrap_or_default(),
                });
            }
            let value = outcomes_payload_value(&op, outcomes)?;
            repo::pending_ops::update_payload(db, &op.id, &value)?;
            Ok(())
        })
        .await
}

/// Persist one message's Graph `setFlags` evidence.
///
/// Graph PATCHes one message at a time, so a batch can succeed for one target
/// and fail for another. The take-off marker (`Unknown`) is written before the
/// request and the receipt (`Confirmed`) after it succeeds; a later retry that
/// has to skip this message then keeps the earlier receipt instead of
/// downgrading it to `NotExecuted` (which would let undo skip the remote
/// compensation for a write that did happen).
///
/// `replace_unknown` selects whether this attempt may replace an `Unknown`
/// marker. It is true only when the previous attempt left no ambiguity; a
/// marker from an earlier attempt may already have reached the server and must
/// survive.
async fn record_set_flags_outcome(
    ctx: &CoreContext,
    op_id: &str,
    message_id: &str,
    source_uid: i64,
    state: OpMessageState,
    replace_unknown: bool,
) -> Result<()> {
    let op_id = op_id.to_string();
    let message_id = message_id.to_string();
    ctx.db
        .write(move |db| {
            record_set_flags_outcome_in_db(
                db,
                &op_id,
                &message_id,
                source_uid,
                state,
                replace_unknown,
            )
        })
        .await
}

/// The transaction body of [`record_set_flags_outcome`].
fn record_set_flags_outcome_in_db(
    db: &rusqlite::Connection,
    op_id: &str,
    message_id: &str,
    source_uid: i64,
    state: OpMessageState,
    replace_unknown: bool,
) -> Result<()> {
    let Some(op) = repo::pending_ops::get(db, op_id)? else {
        return Ok(());
    };
    let mut payload: SetFlagsOp = repo::pending_ops::payload(&op)?;
    if let Some(existing) = payload
        .outcomes
        .iter()
        .find(|outcome| outcome.message_id == message_id)
    {
        // A confirmed send is final: this or any later attempt must never
        // downgrade it.
        if existing.state == OpMessageState::Confirmed && state != OpMessageState::Confirmed {
            return Ok(());
        }
        // An earlier attempt's `Unknown` may already have reached the server.
        // Only this attempt's own marker may be replaced.
        if state == OpMessageState::NotExecuted
            && existing.state == OpMessageState::Unknown
            && !replace_unknown
        {
            return Ok(());
        }
    }
    payload
        .outcomes
        .retain(|outcome| outcome.message_id != message_id);
    payload.outcomes.push(OpMessageOutcome {
        message_id: message_id.to_string(),
        source_uid: Some(source_uid),
        state,
        target_folder_id: None,
        target_uid: None,
        target_uidvalidity: None,
        target_remote_id: None,
        attempt_remote_id: None,
        copy_phase: Default::default(),
        delete_flag_phase: Default::default(),
        expunge_phase: Default::default(),
    });
    let value = serde_json::to_value(&payload)
        .map_err(|error| AppError::internal(format!("cannot encode setFlags outcomes: {error}")))?;
    repo::pending_ops::update_payload(db, op_id, &value)?;
    Ok(())
}

/// Atomically verify the writer claim and persist the Graph take-off marker.
///
/// The ownership check and the `Unknown` marker share one write transaction.
/// A concurrent sync that deletes the projection (dropping its writer), or a
/// newer action that takes the field over, therefore either commits before the
/// transaction — the claim fails and nothing is written — or after it, when the
/// marker is already durable and the next guard still sees the new owner. There
/// is no window in which a stale read authorises a PATCH.
fn claim_graph_flag_send_in_db(
    db: &rusqlite::Connection,
    op_id: &str,
    message_id: &str,
    payload: &SetFlagsOp,
    source_uid: i64,
    replace_unknown: bool,
) -> Result<bool> {
    if repo::messages::get(db, message_id)?.is_none() {
        return Ok(false);
    }
    if !flag_write_owned(
        db,
        op_id,
        message_id,
        payload.seen,
        payload.flagged,
        payload.answered,
        payload.forwarded,
        true,
    )? {
        return Ok(false);
    }
    record_set_flags_outcome_in_db(
        db,
        op_id,
        message_id,
        source_uid,
        OpMessageState::Unknown,
        replace_unknown,
    )?;
    Ok(true)
}

async fn claim_graph_flag_send_for(
    ctx: &CoreContext,
    op_id: &str,
    message_id: &str,
    payload: &SetFlagsOp,
    source_uid: i64,
    replace_unknown: bool,
) -> Result<bool> {
    let op_id = op_id.to_string();
    let message_id = message_id.to_string();
    let payload = payload.clone();
    ctx.db
        .write(move |db| {
            claim_graph_flag_send_in_db(
                db,
                &op_id,
                &message_id,
                &payload,
                source_uid,
                replace_unknown,
            )
        })
        .await
}

/// Record that `setFlags` settled without writing a message.
///
/// The operation still becomes `done` (replaying it is pointless: a newer
/// action owns the field), but the durable per-message `NotExecuted` evidence
/// tells undo that this row never reached the server. Undo then re-queues the
/// restored intent instead of handing the field back to a row that will never
/// run again.
async fn record_set_flags_not_executed(
    ctx: &CoreContext,
    skipped: &[(String, String)],
) -> Result<()> {
    if skipped.is_empty() {
        return Ok(());
    }
    let skipped = skipped.to_vec();
    ctx.db
        .write(move |db| {
            let mut by_op: HashMap<String, Vec<String>> = HashMap::new();
            for (op_id, message_id) in skipped {
                by_op.entry(op_id).or_default().push(message_id);
            }
            for (op_id, message_ids) in by_op {
                let Some(op) = repo::pending_ops::get(db, &op_id)? else {
                    continue;
                };
                let mut payload: SetFlagsOp = repo::pending_ops::payload(&op)?;
                for message_id in message_ids {
                    // A previous attempt may already have proved this message
                    // was sent; a later skip must not retract that evidence.
                    if payload.outcomes.iter().any(|outcome| {
                        outcome.message_id == message_id
                            && outcome.state != OpMessageState::NotExecuted
                    }) {
                        continue;
                    }
                    payload
                        .outcomes
                        .retain(|outcome| outcome.message_id != message_id);
                    let source_uid = payload
                        .message_ids
                        .iter()
                        .position(|id| id == &message_id)
                        .and_then(|index| payload.uids.get(index).copied());
                    payload.outcomes.push(OpMessageOutcome {
                        message_id,
                        source_uid,
                        state: OpMessageState::NotExecuted,
                        target_folder_id: None,
                        target_uid: None,
                        target_uidvalidity: None,
                        target_remote_id: None,
                        attempt_remote_id: None,
                        copy_phase: Default::default(),
                        delete_flag_phase: Default::default(),
                        expunge_phase: Default::default(),
                    });
                }
                let value = serde_json::to_value(&payload).map_err(|error| {
                    AppError::internal(format!("cannot encode setFlags outcomes: {error}"))
                })?;
                repo::pending_ops::update_payload(db, &op_id, &value)?;
            }
            Ok(())
        })
        .await
}

/// Does this error leave the server-side outcome unknown?
fn graph_outcome_unknown(error: &AppError) -> bool {
    matches!(
        error,
        AppError::Network(_)
            | AppError::Service(_)
            | AppError::Auth(_)
            | AppError::Db(_)
            | AppError::Io(_)
    )
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
    run_due_graph_with(ctx, account, &GraphMailClient::new()).await
}

/// [`run_due_graph`] against an explicit client; tests use a loopback server.
pub async fn run_due_graph_with(
    ctx: &CoreContext,
    account: &Account,
    client: &GraphMailClient,
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
        resume_deferred_undo(ctx).await;
        return Ok(ExecOutcome::default());
    }

    let mut outcome = ExecOutcome::default();
    for op in due {
        if claim_ops(ctx, std::slice::from_ref(&op.id))
            .await?
            .is_empty()
        {
            continue;
        }
        match execute_graph_op(ctx, account, client, &op).await {
            Ok(()) => {
                if finish_ops(ctx, std::slice::from_ref(&op.id)).await? > 0 {
                    outcome.done += 1;
                } else {
                    outcome.deferred += 1;
                }
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
                        if let Some(target) = payload.target_folder_id {
                            outcome.resync_folders.insert(target);
                        }
                    }
                    if let Ok(payload) = repo::pending_ops::payload::<MarkJunkOp>(&op) {
                        outcome.resync_folders.insert(payload.folder_id.clone());
                        if let Some(target) = payload.target_folder_id {
                            outcome.resync_folders.insert(target);
                        }
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
                if op.kind == KIND_DELETE_REMOTE_DRAFT {
                    if let Ok(payload) = repo::pending_ops::payload::<DeleteRemoteDraftOp>(&op) {
                        // Even when the delete succeeded, a cancellation that
                        // landed while the request was on the wire means the
                        // local projection is no longer trustworthy. Queue the
                        // folder so the caller's Graph sync reconciles it.
                        outcome.resync_folders.insert(payload.folder_id);
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
                    Some(client),
                    &mut outcome,
                    LiveSourceState::default(),
                )
                .await?;
            }
        }
    }
    resume_deferred_undo(ctx).await;
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
    match execute_graph_op(ctx, account, &GraphMailClient::new(), op).await {
        Ok(()) => mark_done(ctx, std::slice::from_ref(&op.id)).await,
        Err(error) => {
            let mut outcome = ExecOutcome::default();
            handle_failure(
                ctx,
                account,
                op,
                &error,
                TokenResource::MicrosoftGraph,
                None,
                &mut outcome,
                LiveSourceState::default(),
            )
            .await?;
            Err(error)
        }
    }
}

/// Is `op_id` still the last writer of every flag field it sets on `message_id`?
///
/// Value equality is not ownership: "read -> unread -> read" leaves the second
/// action's value equal to the first's.
///
/// The owner is compared by id, not by whether it will ever run. Undo keeps a
/// cancelled operation as the writer precisely so that a still-queued action
/// loses the field; treating such a row as "no writer" would let that action
/// PATCH the stale copy.
///
/// `require_writer` separates the two callers. A payload that records
/// per-message ids (`message_ids`) was queued in the same transaction as its
/// writer rows, so a missing row means the projection (and its evidence) was
/// removed afterwards — there is no proof this operation still owns the field.
/// Such an operation must never fall back to the legacy "no writer, allowed"
/// rule, or a retry would PATCH a provider id the deleted projection no longer
/// proves. Only payloads without `message_ids` (older builds) keep that
/// fallback; rollback also passes `false`.
#[allow(clippy::too_many_arguments)]
fn flag_write_owned(
    db: &rusqlite::Connection,
    op_id: &str,
    message_id: &str,
    seen: Option<bool>,
    flagged: Option<bool>,
    answered: Option<bool>,
    forwarded: Option<bool>,
    require_writer: bool,
) -> Result<bool> {
    for (field, value) in [
        ("seen", seen),
        ("flagged", flagged),
        ("answered", answered),
        ("forwarded", forwarded),
    ] {
        if value.is_none() {
            continue;
        }
        match repo::messages::flag_writer(db, message_id, field)? {
            Some(writer) => {
                if writer.owner_id != op_id {
                    return Ok(false);
                }
            }
            None if require_writer => return Ok(false),
            None => {}
        }
    }
    Ok(true)
}

async fn flag_write_owned_for(
    ctx: &CoreContext,
    op_id: &str,
    message_id: &str,
    payload: &SetFlagsOp,
    require_writer: bool,
) -> Result<bool> {
    let op_id = op_id.to_string();
    let message_id = message_id.to_string();
    let seen = payload.seen;
    let flagged = payload.flagged;
    let answered = payload.answered;
    let forwarded = payload.forwarded;
    ctx.db
        .read(move |db| {
            flag_write_owned(
                db,
                &op_id,
                &message_id,
                seen,
                flagged,
                answered,
                forwarded,
                require_writer,
            )
        })
        .await
}

/// The per-message send claim for a payload that carries `message_ids`.
///
/// Unlike [`flag_write_owned`] this also requires the local projection to
/// still exist. The writer row normally disappears with the row, but the
/// executor is the last line of defense: a message that is no longer in the
/// table has no local evidence that the queued UID/provider id still means
/// what the payload says.
async fn flag_claim_owned_for(
    ctx: &CoreContext,
    op_id: &str,
    message_id: &str,
    payload: &SetFlagsOp,
) -> Result<bool> {
    let op_id = op_id.to_string();
    let message_id = message_id.to_string();
    let seen = payload.seen;
    let flagged = payload.flagged;
    let answered = payload.answered;
    let forwarded = payload.forwarded;
    ctx.db
        .read(move |db| {
            if repo::messages::get(db, &message_id)?.is_none() {
                return Ok(false);
            }
            flag_write_owned(
                db,
                &op_id,
                &message_id,
                seen,
                flagged,
                answered,
                forwarded,
                true,
            )
        })
        .await
}

/// One IMAP message whose STORE this pass intends to send.
struct FlagSendClaim {
    op_id: String,
    uid: i64,
    message_id: String,
}

/// Apply one merged batch of `setFlags` operations.
///
/// Ownership is re-checked immediately before the wire command: a later action
/// may have taken a field over while this operation sat in backoff, and sending
/// the old value would overwrite the user's newest intent locally and remotely.
async fn execute_set_flags(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &Account,
    selected: &mut Option<SelectedFolder>,
    batch: &[(PendingOp, SetFlagsOp)],
) -> Result<()> {
    let Some((_, first)) = batch.first() else {
        return Ok(());
    };
    if first.uids.is_empty() {
        return Ok(());
    }
    let folder = folder_by_id(ctx, &first.folder_id).await?;
    // Folder and UID only identify a copy inside the folder's own account. A
    // payload queued under another account (for example a reply sent from B
    // that targeted a message in A) must never be executed by B's worker: B
    // would SELECT the same path/UID on its own mailbox and mark an unrelated
    // message. Refuse before the connection is even selected.
    if folder.account_id != account.id {
        return Err(AppError::unsupported(format!(
            "setFlags folder {} belongs to account {}, not the executing account {}",
            folder.id, folder.account_id, account.id
        )));
    }
    let uidvalidity = ensure_selected(conn, selected, &folder.remote_path).await?;
    verify_uidvalidity(
        uidvalidity,
        first.source_uidvalidity,
        &folder.remote_path,
        "setFlags",
    )?;

    // Only UIDs this operation still owns may be written. A payload without
    // message ids predates the writer table; it keeps the legacy behaviour.
    let mut claims: Vec<FlagSendClaim> = Vec::new();
    let mut legacy_uids: Vec<i64> = Vec::new();
    let mut active_op_ids: Vec<String> = Vec::new();
    // Messages this command must not write. Recording them per message is what
    // lets undo tell a settled-but-skipped row apart from one that was sent:
    // handing the field back to this row would leave it unwritten forever.
    let mut skipped: Vec<(String, String)> = Vec::new();
    for (op, payload) in batch {
        let mut active = false;
        for (index, uid) in payload.uids.iter().enumerate() {
            match payload.message_ids.get(index) {
                Some(message_id) => {
                    if flag_write_owned_for(ctx, &op.id, message_id, payload, true).await? {
                        active = true;
                        claims.push(FlagSendClaim {
                            op_id: op.id.clone(),
                            uid: *uid,
                            message_id: message_id.clone(),
                        });
                    } else {
                        skipped.push((op.id.clone(), message_id.clone()));
                        debug!(
                            op = %op.id,
                            uid,
                            "setFlags: no durable writer proves this operation still owns the field; skipping the stale write"
                        );
                    }
                }
                // A payload without message ids predates the writer table: it
                // keeps the legacy permission and has no per-message evidence.
                None if payload.message_ids.is_empty() => {
                    active = true;
                    if !legacy_uids.contains(uid) {
                        legacy_uids.push(*uid);
                    }
                }
                // Per-message ids exist but not for this uid; do not send
                // without evidence.
                None => {
                    debug!(
                        op = %op.id,
                        uid,
                        "setFlags: the payload has no message id for this UID; skipping the stale write"
                    );
                }
            }
        }
        if active {
            active_op_ids.push(op.id.clone());
        }
    }
    // This evidence is independent of the wire command: the skipped fields
    // were never addressed, whether the remaining STORE succeeds or not.
    record_set_flags_not_executed(ctx, &skipped).await?;
    if claims.is_empty() && legacy_uids.is_empty() {
        debug!("setFlags: every target is owned by a later action; nothing to send");
        return Ok(());
    }

    let mut add = Vec::new();
    let mut remove = Vec::new();
    for (value, name) in [
        (first.seen, "\\Seen"),
        (first.flagged, "\\Flagged"),
        (first.answered, "\\Answered"),
        (first.forwarded, "$Forwarded"),
    ] {
        match value {
            Some(true) => add.push(name),
            Some(false) => remove.push(name),
            None => {}
        }
    }
    if add.is_empty() && remove.is_empty() {
        return Ok(());
    }
    mark_send_intents(ctx, &active_op_ids).await?;

    // Re-verify every claim and persist its per-message `Unknown` marker in the
    // same write transaction. IMAP STOREs a UID *set*: the tagged completion
    // can be lost after the server applied the change, so the marker must
    // already be durable. A later retry that has to skip this message (the
    // writer moved on) must not overwrite that `Unknown` with `NotExecuted`;
    // undo would then treat the STORE as never sent and skip compensation.
    let (claims, lost) = claim_imap_flag_sends(ctx, claims).await?;
    record_set_flags_not_executed(ctx, &lost).await?;
    let mut allowed = legacy_uids;
    for claim in &claims {
        if !allowed.contains(&claim.uid) {
            allowed.push(claim.uid);
        }
    }
    if allowed.is_empty() {
        debug!("setFlags: every target lost its writer before the STORE; nothing to send");
        return Ok(());
    }

    let uids = uid_set(&allowed);
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
    // Only a fully successful command may turn the take-off markers into
    // receipts. A lost completion leaves them `Unknown` for undo.
    confirm_imap_flag_sends(ctx, claims).await?;
    Ok(())
}

/// Atomically re-check every IMAP `setFlags` claim and write its take-off
/// `Unknown` marker in the same transaction as the writer verification.
async fn claim_imap_flag_sends(
    ctx: &CoreContext,
    claims: Vec<FlagSendClaim>,
) -> Result<(Vec<FlagSendClaim>, Vec<(String, String)>)> {
    if claims.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    ctx.db
        .write(move |db| {
            let mut confirmed = Vec::new();
            let mut lost = Vec::new();
            for claim in claims {
                let Some(op) = repo::pending_ops::get(db, &claim.op_id)? else {
                    lost.push((claim.op_id, claim.message_id));
                    continue;
                };
                let Ok(payload) = repo::pending_ops::payload::<SetFlagsOp>(&op) else {
                    lost.push((claim.op_id, claim.message_id));
                    continue;
                };
                let owned = repo::messages::get(db, &claim.message_id)?.is_some()
                    && flag_write_owned(
                        db,
                        &claim.op_id,
                        &claim.message_id,
                        payload.seen,
                        payload.flagged,
                        payload.answered,
                        payload.forwarded,
                        true,
                    )?;
                if !owned {
                    lost.push((claim.op_id, claim.message_id));
                    continue;
                }
                // `false` keeps an `Unknown` written by an earlier attempt:
                // that STORE may already have reached the server.
                record_set_flags_outcome_in_db(
                    db,
                    &claim.op_id,
                    &claim.message_id,
                    claim.uid,
                    OpMessageState::Unknown,
                    false,
                )?;
                confirmed.push(claim);
            }
            Ok((confirmed, lost))
        })
        .await
}

/// Persist the receipts for a successful IMAP STORE.
async fn confirm_imap_flag_sends(ctx: &CoreContext, claims: Vec<FlagSendClaim>) -> Result<()> {
    if claims.is_empty() {
        return Ok(());
    }
    ctx.db
        .write(move |db| {
            for claim in claims {
                record_set_flags_outcome_in_db(
                    db,
                    &claim.op_id,
                    &claim.message_id,
                    claim.uid,
                    OpMessageState::Confirmed,
                    true,
                )?;
            }
            Ok(())
        })
        .await
}

/// Destination UIDs reported by an IMAP `COPYUID` response.
#[derive(Debug, Clone, Default)]
struct TransferUids {
    target_uidvalidity: i64,
    /// `(source UID, destination UID)` in the order the server reported them.
    pairs: Vec<(i64, i64)>,
}

/// One message's durable progress through the COPY fallback.
#[derive(Debug, Clone)]
struct MoveFallbackPlan {
    message_id: String,
    source_uid: i64,
    copy_phase: CopyPhase,
    target_uid: Option<i64>,
    target_uidvalidity: Option<i64>,
    delete_flag_phase: DeleteFlagPhase,
    expunge_phase: ExpungePhase,
}

fn mapped_destination(mapping: Option<&TransferUids>, source_uid: i64) -> Option<i64> {
    mapping.and_then(|mapping| {
        mapping
            .pairs
            .iter()
            .find(|(source, _)| *source == source_uid)
            .map(|(_, destination)| *destination)
    })
}

#[allow(clippy::too_many_arguments)]
async fn execute_move(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &Account,
    selected: &mut Option<SelectedFolder>,
    source_id: &str,
    target_id: &str,
    uids: &[i64],
    message_ids: &[String],
    source_uidvalidity: Option<i64>,
    op_id: Option<&str>,
) -> Result<Option<TransferUids>> {
    if uids.is_empty() {
        return Ok(None);
    }
    // "Do it, or do not do it": without MOVE the only safe fallback is
    // COPY + STORE \Deleted + UID EXPUNGE. A server with neither MOVE nor a
    // directed expunge would leave a second copy behind that can never be
    // removed without erasing unrelated mail, so refuse before touching the
    // server at all.
    if !conn.caps.r#move && !conn.caps.directed_expunge() {
        return Err(AppError::unsupported(
            "moving messages requires MOVE or UIDPLUS",
        ));
    }
    let source = folder_by_id(ctx, source_id).await?;
    let uidvalidity = ensure_selected(conn, selected, &source.remote_path).await?;
    verify_uidvalidity(uidvalidity, source_uidvalidity, &source.remote_path, "move")?;
    let set = uid_set(uids);
    let aligned = op_id.is_some() && !message_ids.is_empty() && message_ids.len() == uids.len();
    let prior = match op_id {
        Some(op_id) => load_pending_op(ctx, op_id).await?,
        None => None,
    };
    let prior_outcomes = prior
        .as_ref()
        .map(outcomes_from_op)
        .transpose()?
        .unwrap_or_default();
    // The destination is fixed by the first attempt that touched the server.
    // A later attempt must not follow a moved role folder (Archive A -> B):
    // that would address a folder this operation never copied into, and the
    // COPYUID receipt of A would be recorded against B. An operation whose
    // payloads disagree about their destination cannot be resumed safely.
    let mut pinned_target: Option<String> = None;
    for outcome in &prior_outcomes {
        // A UID receipt without the folder it belongs to cannot be attributed:
        // the current role folder may be a different mailbox than the one the
        // first attempt copied into. Refuse to guess instead of moving the
        // receipt (and a later undo) across folders.
        if outcome.target_uid.is_some() && outcome.target_folder_id.is_none() {
            return Err(AppError::service(
                "move receipt has no destination folder; refusing to attribute it to the current target",
            ));
        }
        let Some(folder_id) = outcome.target_folder_id.as_deref() else {
            continue;
        };
        match pinned_target.as_deref() {
            None => pinned_target = Some(folder_id.to_string()),
            Some(existing) if existing == folder_id => {}
            Some(_) => {
                return Err(AppError::service(
                    "move operation records more than one destination folder; refusing to resume it",
                ));
            }
        }
    }
    let target_id = pinned_target.as_deref().unwrap_or(target_id);
    let target = folder_by_id(ctx, target_id).await?;
    if source.id == target.id {
        return Ok(None);
    }
    // Persisted fallback progress must win over a changed CAPABILITY response:
    // switching to UID MOVE after a COPY could create a second target copy.
    // Any recorded outcome means an earlier attempt took the COPY fallback
    // (the UID MOVE path records nothing until the operation settles).
    let fallback_in_progress = !prior_outcomes.is_empty();

    if conn.caps.r#move && !fallback_in_progress {
        if let Some(op_id) = op_id {
            // A previous attempt may have sent the command and lost its tagged
            // response (EOF or timeout). MOVE is not idempotent: repeating it
            // can move a second copy or address a UID the server reassigned.
            // Keep the outcome unknown and let the folder reconciliation
            // settle it.
            if prior
                .as_ref()
                .is_some_and(|op| op.send_intent || op.outcome_unknown)
            {
                let id = op_id.to_string();
                ctx.db
                    .write(move |db| repo::pending_ops::mark_outcome_unknown(db, &id))
                    .await?;
                return Err(AppError::service(
                    "UID MOVE may already have reached the server; refusing to send it again",
                ));
            }
            // Persist the take-off marker only after every local validation
            // passed: an operation refused before this point provably never
            // reached the server and undo may still cancel it outright.
            mark_send_intent(ctx, op_id).await?;
        }
        let mapping = uid_transfer(conn, &format!("UID MOVE {set} {}", target.remote_path)).await?;
        debug!(source = %source.remote_path, target = %target.remote_path, "moved messages");
        return Ok(mapping);
    }

    // ── COPY fallback ──────────────────────────────────────────────────────
    // The copy and the source deletion are separate commands, so each phase is
    // persisted per message. A retry resumes only the phases that did not
    // finish; in particular it never sends a second COPY for a message whose
    // copy already succeeded (or may have succeeded).
    //
    // Resuming a partially copied operation also requires a directed expunge:
    // without it the source copy could never be removed safely, so refuse
    // rather than leave a duplicate that no later command can address.
    if !conn.caps.directed_expunge() {
        return Err(AppError::unsupported(
            "moving messages requires MOVE or UIDPLUS",
        ));
    }
    if !aligned {
        // Legacy payloads carry no per-message handles. Keep the old
        // all-or-nothing behaviour: an operation that may already have sent
        // COPY/MOVE is never repeated blindly.
        if let Some(op_id) = op_id {
            if prior
                .as_ref()
                .is_some_and(|op| op.send_intent || op.outcome_unknown)
            {
                let id = op_id.to_string();
                ctx.db
                    .write(move |db| repo::pending_ops::mark_outcome_unknown(db, &id))
                    .await?;
                return Err(AppError::service(
                    "UID COPY may already have reached the server; refusing to send it again",
                ));
            }
            mark_send_intent(ctx, op_id).await?;
        }
        let mapping = uid_transfer(conn, &format!("UID COPY {set} {}", target.remote_path)).await?;
        store(conn, &set, "+FLAGS.SILENT (\\Deleted)").await?;
        expire(conn, &set).await?;
        // The same rule as the resumed path: a tagged OK is not proof the
        // source messages were removed (another client may have cleared
        // `\Deleted`). Never report a move that left the source behind.
        if !matches!(search_present_uids(conn, uids).await, Some(present) if present.is_empty()) {
            return Err(AppError::service(
                "UID EXPUNGE did not remove the source messages; leaving the move incomplete",
            ));
        }
        debug!(source = %source.remote_path, target = %target.remote_path, "moved messages");
        return Ok(mapping);
    }
    let op_id = op_id.expect("aligned progress requires an operation id");
    let prior =
        prior.ok_or_else(|| AppError::NotFound(format!("pending operation {op_id} not found")))?;
    // A payload queued by a build that predates phase tracking cannot prove
    // that its COPY was never sent. Treat every unstarted copy as unknown so
    // the retry verifies the target folder instead of creating a duplicate.
    let legacy_ambiguous = prior.send_intent && prior_outcomes.is_empty();

    let mut plans: Vec<MoveFallbackPlan> = message_ids
        .iter()
        .zip(uids.iter())
        .map(|(message_id, source_uid)| {
            let previous = prior_outcomes
                .iter()
                .find(|outcome| &outcome.message_id == message_id);
            let previous_phase = previous.map(|o| o.copy_phase).unwrap_or_default();
            MoveFallbackPlan {
                message_id: message_id.clone(),
                source_uid: *source_uid,
                copy_phase: if legacy_ambiguous && previous_phase == CopyPhase::NotStarted {
                    CopyPhase::Unknown
                } else {
                    previous_phase
                },
                target_uid: previous.and_then(|o| o.target_uid),
                target_uidvalidity: previous.and_then(|o| o.target_uidvalidity),
                delete_flag_phase: previous.map(|o| o.delete_flag_phase).unwrap_or_default(),
                expunge_phase: previous.map(|o| o.expunge_phase).unwrap_or_default(),
            }
        })
        .collect();

    // Only `COPYUID` attributes a destination identity to this operation. A
    // target-folder sync that merely matched the Message-ID cannot tell the
    // copy this operation created from an unrelated message that already
    // shared the header (or replaced it later), so that binding is never used
    // to authorize deleting the source nor to produce an undo receipt. A
    // resumed `Copied` plan without a receipt stays unknown in the
    // destination-verification phase below.
    let pending_copy: Vec<usize> = plans
        .iter()
        .enumerate()
        .filter(|(_, plan)| plan.copy_phase == CopyPhase::NotStarted)
        .map(|(index, _)| index)
        .collect();
    // Copies performed by this attempt are verified by the tagged completion
    // itself. Only a history receipt has to be re-checked against the live
    // destination before the source may be deleted.
    let mut fresh_copies: HashSet<usize> = HashSet::new();
    if !pending_copy.is_empty() {
        let pending_uids: Vec<i64> = pending_copy
            .iter()
            .map(|index| plans[*index].source_uid)
            .collect();
        let pending_ids: Vec<String> = pending_copy
            .iter()
            .map(|index| plans[*index].message_id.clone())
            .collect();
        // Pin the destination generation *before* the command so a retry can
        // tell whether the folder was rebuilt, and a `COPYUID` receipt can be
        // attributed to the generation it was reported in.
        let target_generation = ensure_selected(conn, selected, &target.remote_path).await?;
        let source_now = ensure_selected(conn, selected, &source.remote_path).await?;
        verify_uidvalidity(source_now, source_uidvalidity, &source.remote_path, "move")?;
        // The take-off marker, the pinned destination and the send intent
        // commit together. A crash after this point leaves an unknown copy,
        // which is never blindly re-sent.
        let take_off: Vec<(String, Option<i64>, MovePhaseUpdate)> = pending_copy
            .iter()
            .map(|index| {
                (
                    plans[*index].message_id.clone(),
                    Some(plans[*index].source_uid),
                    MovePhaseUpdate::CopyUnknown {
                        target_folder_id: Some(target.id.clone()),
                        target_uidvalidity: target_generation,
                    },
                )
            })
            .collect();
        record_move_phases(ctx, op_id, take_off, true).await?;
        for index in &pending_copy {
            plans[*index].target_uidvalidity = target_generation;
        }

        let copy_set = uid_set(&pending_uids);
        match uid_transfer(conn, &format!("UID COPY {copy_set} {}", target.remote_path)).await {
            Ok(mapping) => {
                let mut updates = Vec::new();
                for index in &pending_copy {
                    let plan = &mut plans[*index];
                    let mapped = mapped_destination(mapping.as_ref(), plan.source_uid);
                    // Only `COPYUID` names the destination UID. Without it the
                    // COPY is still confirmed by this attempt's tagged
                    // completion, but its identity stays unknown: a target
                    // Message-ID match is not proof that this COPY created the
                    // row, so it must not be recorded as an undo receipt.
                    let (target_uid, target_uidvalidity) = match mapped {
                        Some(destination) => (
                            Some(destination),
                            mapping
                                .as_ref()
                                .map(|m| m.target_uidvalidity)
                                .or(target_generation),
                        ),
                        None => (None, target_generation),
                    };
                    plan.copy_phase = CopyPhase::Copied;
                    plan.target_uid = target_uid;
                    plan.target_uidvalidity = target_uidvalidity;
                    fresh_copies.insert(*index);
                    updates.push((
                        plan.message_id.clone(),
                        Some(plan.source_uid),
                        MovePhaseUpdate::CopyKnown {
                            target_uid,
                            target_uidvalidity,
                            target_folder_id: Some(target.id.clone()),
                        },
                    ));
                }
                record_move_phases(ctx, op_id, updates, false).await?;
                if let Some(mapping) = mapping.as_ref() {
                    apply_transfer_uids(
                        ctx,
                        &account.id,
                        &target.id,
                        &pending_ids,
                        &pending_uids,
                        mapping,
                    )
                    .await?;
                }
            }
            Err(error) => {
                // A tagged refusal proves this COPY did not execute; clear the
                // take-off marker so the retry may copy these messages again.
                if matches!(error, AppError::Protocol(_)) {
                    let resets: Vec<(String, Option<i64>, MovePhaseUpdate)> = pending_copy
                        .iter()
                        .map(|index| {
                            (
                                plans[*index].message_id.clone(),
                                Some(plans[*index].source_uid),
                                MovePhaseUpdate::CopyNotStarted,
                            )
                        })
                        .collect();
                    record_move_phases(ctx, op_id, resets, false).await?;
                }
                return Err(error);
            }
        }
    }

    if plans
        .iter()
        .any(|plan| plan.copy_phase == CopyPhase::Unknown)
    {
        let id = op_id.to_string();
        ctx.db
            .write(move |db| repo::pending_ops::mark_outcome_unknown(db, &id))
            .await?;
        return Err(AppError::service(
            "COPY may already have reached the server; waiting for the target folder to bind it",
        ));
    }

    // ── destination verification before deleting the source ────────────────
    // Backfill the pinned destination for payloads written before this build.
    // `BindTarget` only fills a missing folder, so it can never move a receipt
    // from the folder it was recorded against.
    let pin: Vec<(String, Option<i64>, MovePhaseUpdate)> = plans
        .iter()
        .map(|plan| {
            (
                plan.message_id.clone(),
                Some(plan.source_uid),
                MovePhaseUpdate::BindTarget {
                    target_folder_id: target.id.clone(),
                },
            )
        })
        .collect();
    record_move_phases(ctx, op_id, pin, false).await?;

    // A `COPYUID` receipt is history: the target copy may have been moved,
    // expunged or lost when the mailbox was rebuilt. Before the source is
    // deleted, prove the pinned destination still carries that exact UID in
    // the pinned generation. A copy sent by *this* attempt is proven by its
    // tagged completion and does not need the round trip; a historical copy
    // without any receipt has no identity to prove and stays unverified.
    let history_copies: Vec<usize> = plans
        .iter()
        .enumerate()
        .filter(|(index, plan)| {
            !fresh_copies.contains(index)
                && plan.copy_phase == CopyPhase::Copied
                && plan.expunge_phase != ExpungePhase::Applied
        })
        .map(|(index, _)| index)
        .collect();
    let mut unverified: Vec<(String, Option<i64>, MovePhaseUpdate)> = Vec::new();
    if !history_copies.is_empty() {
        let mut identities: Vec<(usize, i64)> = Vec::new();
        for index in &history_copies {
            let plan = &plans[*index];
            match plan.target_uid {
                Some(uid) => identities.push((*index, uid)),
                None => unverified.push((
                    plan.message_id.clone(),
                    Some(plan.source_uid),
                    MovePhaseUpdate::VerifyPending,
                )),
            }
        }
        if !identities.is_empty() {
            let target_now = ensure_selected(conn, selected, &target.remote_path).await?;
            let mut searchable: Vec<(usize, i64)> = Vec::new();
            for (index, uid) in identities {
                let plan = &plans[index];
                // A UID only names the same message inside the generation the
                // receipt was recorded in. A rebuilt target makes it address
                // an unrelated message (or nothing), so the receipt is dead.
                if plan.target_uidvalidity.is_some() && plan.target_uidvalidity == target_now {
                    searchable.push((index, uid));
                } else {
                    unverified.push((
                        plan.message_id.clone(),
                        Some(plan.source_uid),
                        MovePhaseUpdate::CopyUnknown {
                            target_folder_id: None,
                            target_uidvalidity: None,
                        },
                    ));
                }
            }
            if !searchable.is_empty() {
                let uids: Vec<i64> = searchable.iter().map(|(_, uid)| *uid).collect();
                match search_present_uids(conn, &uids).await {
                    Some(present) => {
                        for (index, uid) in searchable {
                            if !present.contains(&uid) {
                                let plan = &plans[index];
                                unverified.push((
                                    plan.message_id.clone(),
                                    Some(plan.source_uid),
                                    MovePhaseUpdate::CopyUnknown {
                                        target_folder_id: None,
                                        target_uidvalidity: None,
                                    },
                                ));
                            }
                        }
                    }
                    None => {
                        // Inconclusive (tagged NO/BAD, timeout, lost
                        // connection): keep the proven COPY so a later attempt
                        // can verify it again, but never delete the source on
                        // an absent answer.
                        for (index, _) in searchable {
                            let plan = &plans[index];
                            unverified.push((
                                plan.message_id.clone(),
                                Some(plan.source_uid),
                                MovePhaseUpdate::VerifyPending,
                            ));
                        }
                    }
                }
            }
            // Selecting the target above deselected the source. Re-select it
            // and require the same generation the queued UIDs were captured in
            // before any STORE/EXPUNGE.
            let source_now = ensure_selected(conn, selected, &source.remote_path).await?;
            verify_uidvalidity(source_now, source_uidvalidity, &source.remote_path, "move")?;
        }
    }
    if !unverified.is_empty() {
        record_move_phases(ctx, op_id, unverified, false).await?;
        let id = op_id.to_string();
        ctx.db
            .write(move |db| repo::pending_ops::mark_outcome_unknown(db, &id))
            .await?;
        return Err(AppError::service(
            "the destination copy could not be verified; leaving the source in place",
        ));
    }

    // ── source deletion: STORE \Deleted, then directed EXPUNGE ─────────────
    let flag_pending: Vec<usize> = plans
        .iter()
        .enumerate()
        .filter(|(_, plan)| {
            plan.copy_phase == CopyPhase::Copied
                && plan.delete_flag_phase != DeleteFlagPhase::Applied
        })
        .map(|(index, _)| index)
        .collect();
    if !flag_pending.is_empty() {
        let pending_uids: Vec<i64> = flag_pending
            .iter()
            .map(|index| plans[*index].source_uid)
            .collect();
        let updates: Vec<(String, Option<i64>, MovePhaseUpdate)> = flag_pending
            .iter()
            .map(|index| {
                (
                    plans[*index].message_id.clone(),
                    Some(plans[*index].source_uid),
                    MovePhaseUpdate::FlagUnknown,
                )
            })
            .collect();
        record_move_phases(ctx, op_id, updates, false).await?;
        store(conn, &uid_set(&pending_uids), "+FLAGS.SILENT (\\Deleted)").await?;
        let updates: Vec<(String, Option<i64>, MovePhaseUpdate)> = flag_pending
            .iter()
            .map(|index| {
                (
                    plans[*index].message_id.clone(),
                    Some(plans[*index].source_uid),
                    MovePhaseUpdate::FlagApplied,
                )
            })
            .collect();
        record_move_phases(ctx, op_id, updates, false).await?;
        for index in &flag_pending {
            plans[*index].delete_flag_phase = DeleteFlagPhase::Applied;
        }
    }

    let expunge_pending: Vec<usize> = plans
        .iter()
        .enumerate()
        .filter(|(_, plan)| {
            plan.delete_flag_phase == DeleteFlagPhase::Applied
                && plan.expunge_phase != ExpungePhase::Applied
        })
        .map(|(index, _)| index)
        .collect();
    if !expunge_pending.is_empty() {
        // The capability guard at the top already proved a directed expunge is
        // available: a whole-folder EXPUNGE is never an option.
        let pending_uids: Vec<i64> = expunge_pending
            .iter()
            .map(|index| plans[*index].source_uid)
            .collect();
        let updates: Vec<(String, Option<i64>, MovePhaseUpdate)> = expunge_pending
            .iter()
            .map(|index| {
                (
                    plans[*index].message_id.clone(),
                    Some(plans[*index].source_uid),
                    MovePhaseUpdate::ExpungeUnknown,
                )
            })
            .collect();
        record_move_phases(ctx, op_id, updates, false).await?;
        expire(conn, &uid_set(&pending_uids)).await?;
        // `UID EXPUNGE` returning OK only proves the command was accepted, not
        // that it removed anything: another client may have cleared
        // `\Deleted` first. Confirm the source UIDs actually disappeared in
        // the generation the payload was queued in. An empty set is not
        // success.
        let removed = search_present_uids(conn, &pending_uids).await;
        let still_present = match removed.as_ref() {
            Some(present) => pending_uids.iter().any(|uid| present.contains(uid)),
            None => true,
        };
        if still_present {
            // The pre-expunge `ExpungeUnknown` marker stays; a retry may send
            // the idempotent EXPUNGE again within this generation.
            let id = op_id.to_string();
            ctx.db
                .write(move |db| repo::pending_ops::mark_outcome_unknown(db, &id))
                .await?;
            return Err(AppError::service(
                "UID EXPUNGE did not remove the source messages; leaving the move incomplete",
            ));
        }
        let updates: Vec<(String, Option<i64>, MovePhaseUpdate)> = expunge_pending
            .iter()
            .map(|index| {
                (
                    plans[*index].message_id.clone(),
                    Some(plans[*index].source_uid),
                    MovePhaseUpdate::ExpungeApplied,
                )
            })
            .collect();
        record_move_phases(ctx, op_id, updates, false).await?;
        for index in &expunge_pending {
            plans[*index].expunge_phase = ExpungePhase::Applied;
        }
    }

    if plans
        .iter()
        .any(|plan| plan.expunge_phase != ExpungePhase::Applied)
    {
        return Err(AppError::service(
            "move phases are incomplete; leaving the operation queued",
        ));
    }
    debug!(source = %source.remote_path, target = %target.remote_path, "moved messages");
    // Every receipt of the aligned fallback was persisted per message above,
    // together with its destination folder. Returning a batch mapping here
    // would let a caller re-apply the pinned folder's UIDs against the
    // *current* role folder, which may be a different mailbox.
    Ok(None)
}

/// Run `UID MOVE`/`UID COPY` directly so a UIDPLUS `COPYUID` response can be
/// read. async-imap discards the response, but the destination UID is the only
/// unambiguous way to bind the optimistic projection: Message-ID alone cannot
/// distinguish two copies of the same mail.
async fn uid_transfer(conn: &mut ImapConnection, command: &str) -> Result<Option<TransferUids>> {
    // Mailbox names come from the server and never contain CRLF; refuse one
    // that would let a crafted name inject a second command.
    if command.contains('\r') || command.contains('\n') {
        return Err(AppError::protocol(
            "refusing a mailbox name with a line break",
        ));
    }
    let budget = conn.command_timeout();
    let command = command.to_string();
    conn.with_session(budget, "UID MOVE/COPY", move |session| {
        Box::pin(async move {
            let tag = session.run_command(&command).await.map_err(imap_error)?;
            let mut mapping = None;
            loop {
                let response = session.read_response().await.map_err(|error| {
                    AppError::network(format!("UID MOVE/COPY read failed: {error}"))
                })?;
                let Some(data) = response else {
                    // EOF before the tagged completion. The server may have
                    // applied the command (the mock/real provider commits the
                    // move before closing), so this is an ambiguous outcome,
                    // never a success with no mapping.
                    return Err(AppError::network(
                        "UID MOVE/COPY connection ended before the tagged response",
                    ));
                };
                let Response::Done {
                    tag: done,
                    status,
                    outcome,
                } = data.parsed()
                else {
                    continue;
                };
                if done.0 != tag.0 {
                    continue;
                }
                if let Some(ResponseCode::CopyUid(validity, source, destination)) =
                    outcome.code.as_ref()
                {
                    let source = expand_uid_set(source);
                    let destination = expand_uid_set(destination);
                    if source.len() == destination.len() {
                        mapping = Some(TransferUids {
                            target_uidvalidity: i64::from(*validity),
                            pairs: source.into_iter().zip(destination).collect(),
                        });
                    }
                }
                if !matches!(status, Status::Ok) {
                    return Err(AppError::protocol("server rejected the move"));
                }
                break;
            }
            Ok(mapping)
        })
    })
    .await
}

fn expand_uid_set(members: &[UidSetMember]) -> Vec<i64> {
    let mut result = Vec::new();
    for member in members {
        match member {
            UidSetMember::Uid(uid) => result.push(*uid as i64),
            UidSetMember::UidRange(range) => {
                result.extend((*range.start() as i64)..=(*range.end() as i64));
            }
        }
    }
    result
}

/// Bind the optimistic projection to the exact destination UID the server
/// reported, without going through a Message-ID match.
async fn apply_transfer_uids(
    ctx: &CoreContext,
    account_id: &str,
    target_folder_id: &str,
    message_ids: &[String],
    source_uids: &[i64],
    mapping: &TransferUids,
) -> Result<()> {
    if message_ids.len() != source_uids.len() || mapping.pairs.is_empty() {
        return Ok(());
    }
    let mut updates: Vec<(String, i64)> = Vec::new();
    for (id, source_uid) in message_ids.iter().zip(source_uids.iter()) {
        if let Some((_, destination)) = mapping
            .pairs
            .iter()
            .find(|(source, _)| source == source_uid)
        {
            updates.push((id.clone(), *destination));
        }
    }
    if updates.is_empty() {
        return Ok(());
    }
    let account_id = account_id.to_string();
    let target_folder_id = target_folder_id.to_string();
    let validity = mapping.target_uidvalidity;
    ctx.db
        .write(move |db| {
            for (id, uid) in &updates {
                // Only a row that is still this account's projection in the
                // target folder may be re-addressed.
                let owned = repo::messages::get(db, id)?.is_some_and(|stored| {
                    stored.summary.account_id == account_id
                        && stored.summary.folder_id == target_folder_id
                });
                if owned {
                    repo::messages::set_uid_identity(db, id, Some(*uid), Some(validity))?;
                }
            }
            Ok(())
        })
        .await
}

/// Resolve the destination folder of a soft delete.
///
/// The folder pinned when the operation was queued (and by every earlier
/// attempt) is authoritative: that is where the optimistic projection moved
/// the rows, and a retry must finish the same move. Falling back to the
/// account's current Trash role would address a mailbox this operation never
/// copied into; a target whose role disappeared, whose row vanished or whose
/// identity no longer matches must leave the operation incomplete instead of
/// upgrading it to a permanent delete.
async fn soft_delete_target(
    ctx: &CoreContext,
    account: &Account,
    payload: &DeleteOp,
) -> Result<Folder> {
    let mut pinned: Option<&str> = payload.target_folder_id.as_deref();
    for outcome in &payload.outcomes {
        let Some(folder_id) = outcome.target_folder_id.as_deref() else {
            continue;
        };
        match pinned {
            None => pinned = Some(folder_id),
            Some(existing) if existing == folder_id => {}
            Some(_) => {
                return Err(AppError::service(
                    "soft delete records more than one destination folder; refusing to resume it",
                ));
            }
        }
    }
    let target = match pinned {
        Some(id) => {
            let id = id.to_string();
            ctx.db.read(move |db| repo::folders::get(db, &id)).await?
        }
        None => role_folder(ctx, &account.id, FolderRole::Trash).await?,
    };
    let Some(target) = target else {
        return Err(AppError::service(
            "soft delete has no Trash folder; refusing to upgrade it to a permanent delete",
        ));
    };
    if target.id == payload.folder_id {
        return Err(AppError::service(
            "soft delete destination is the source folder; refusing to upgrade it to a permanent delete",
        ));
    }
    if target.role != FolderRole::Trash {
        return Err(AppError::service(
            "soft delete destination is no longer the Trash folder; refusing to upgrade it to a permanent delete",
        ));
    }
    Ok(target)
}

async fn execute_delete(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &Account,
    selected: &mut Option<SelectedFolder>,
    op: &PendingOp,
    payload: &DeleteOp,
) -> Result<()> {
    if payload.uids.is_empty() {
        return Ok(());
    }
    let folder = folder_by_id(ctx, &payload.folder_id).await?;
    if !payload.permanent {
        // The deletion type was decided when the op was queued and is
        // authoritative. The source folder's role at execution time must never
        // change it: a sync can re-label the source to Trash/Junk before the
        // first attempt, and re-deriving "already gone" from that role would
        // let a queued soft delete fall through to `execute_permanent_delete`
        // and erase the only copy. A soft delete either finishes as a move
        // into its pinned destination or stays incomplete for reconciliation.
        let trash = soft_delete_target(ctx, account, payload).await?;
        let mapping = execute_move(
            ctx,
            conn,
            account,
            selected,
            &folder.id,
            &trash.id,
            &payload.uids,
            &payload.message_ids,
            payload.source_uidvalidity,
            Some(&op.id),
        )
        .await?;
        if let Some(mapping) = mapping.as_ref() {
            apply_transfer_uids(
                ctx,
                &account.id,
                &trash.id,
                &payload.message_ids,
                &payload.uids,
                mapping,
            )
            .await?;
        }
        record_move_outcomes(
            ctx,
            &op.id,
            &payload.message_ids,
            &payload.uids,
            &trash.id,
            mapping.as_ref(),
        )
        .await?;
        return remove_local_messages(ctx, &folder.id, &payload.uids).await;
    }
    execute_permanent_delete(
        ctx,
        conn,
        selected,
        &PermanentDelete {
            account_id: &account.id,
            folder_id: &folder.id,
            uids: &payload.uids,
            source_uidvalidity: payload.source_uidvalidity,
            side_effect: Some((op, payload)),
        },
    )
    .await?;
    // The server confirmed the delete: now erase the row (and with it the
    // attachments and FTS document) that the optimistic phase only hid.
    confirm_permanent_delete(ctx, account, payload).await
}

/// Erase the local rows of a permanent delete after the server confirmed it.
///
/// Payloads queued by this version hid the rows and recorded their ids; those
/// ids are authoritative (a UID may have disappeared from the folder after the
/// command). Older payloads removed the rows optimistically, so the UID lookup
/// is only a best-effort cleanup.
async fn confirm_permanent_delete(
    ctx: &CoreContext,
    account: &Account,
    payload: &DeleteOp,
) -> Result<()> {
    if payload.pending_delete && !payload.message_ids.is_empty() {
        let account_id = account.id.clone();
        let folder_id = payload.folder_id.clone();
        let message_ids = payload.message_ids.clone();
        ctx.db
            .write(move |db| {
                let ids =
                    repo::messages::pending_delete_rows(db, &account_id, &folder_id, &message_ids)?;
                if !ids.is_empty() {
                    repo::messages::delete_local(db, &ids, true)?;
                }
                Ok(())
            })
            .await?;
        return Ok(());
    }
    let ids = local_message_ids(ctx, &payload.folder_id, &payload.uids).await?;
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
    selected: &mut Option<SelectedFolder>,
    op: &PendingOp,
    payload: &ArchiveOp,
) -> Result<()> {
    if payload.uids.is_empty() {
        return Ok(());
    }
    let folder = folder_by_id(ctx, &payload.folder_id).await?;
    if matches!(folder.role, FolderRole::All | FolderRole::Archive) {
        return Ok(());
    }
    let uidvalidity = ensure_selected(conn, selected, &folder.remote_path).await?;
    verify_uidvalidity(
        uidvalidity,
        payload.source_uidvalidity,
        &folder.remote_path,
        "archive",
    )?;
    let set = uid_set(&payload.uids);

    if account.capabilities.gmail_extensions {
        // Gmail archives by dropping the Inbox label; the message stays in
        // All Mail. The local projection was queued against that same folder
        // (never against a coincidental extra `\Archive` label), so leave it in
        // place for the target sync to bind instead of erasing it here.
        mark_send_intent(ctx, &op.id).await?;
        store(conn, &set, "-X-GM-LABELS.SILENT (\\Inbox)").await?;
        // The label removal keeps the message's identity in All Mail, but this
        // command does not report the destination UID. Record the confirmed
        // outcome without an identity: undo must wait for a real binding
        // instead of guessing one by Message-ID.
        record_move_outcomes(
            ctx,
            &op.id,
            &payload.message_ids,
            &payload.uids,
            payload.target_folder_id.as_deref().unwrap_or(&folder.id),
            None,
        )
        .await?;
        return remove_local_messages(ctx, &folder.id, &payload.uids).await;
    }

    // The same all-or-nothing guard as `execute_move`, applied before any
    // fallback folder is created: creating `Archive` only to have the move
    // refused would leave a stray mailbox behind.
    if !conn.caps.r#move && !conn.caps.directed_expunge() {
        return Err(AppError::unsupported(
            "archiving messages requires MOVE or UIDPLUS",
        ));
    }

    // General IMAP only archives to a real `\Archive`. `\All` is a virtual view
    // and may be read-only, so it is never used as a move target: without an
    // Archive the folder is created below.
    let payload_target = match payload.target_folder_id.as_deref() {
        Some(id) => folder_by_id(ctx, id).await.ok(),
        None => None,
    };
    let target = payload_target
        .filter(|target| target.role == FolderRole::Archive)
        .or(role_folder(ctx, &account.id, FolderRole::Archive).await?);
    if let Some(target) = target.filter(|target| target.id != folder.id) {
        let mapping = execute_move(
            ctx,
            conn,
            account,
            selected,
            &folder.id,
            &target.id,
            &payload.uids,
            &payload.message_ids,
            payload.source_uidvalidity,
            Some(&op.id),
        )
        .await?;
        if let Some(mapping) = mapping.as_ref() {
            apply_transfer_uids(
                ctx,
                &account.id,
                &target.id,
                &payload.message_ids,
                &payload.uids,
                mapping,
            )
            .await?;
        }
        record_move_outcomes(
            ctx,
            &op.id,
            &payload.message_ids,
            &payload.uids,
            &target.id,
            mapping.as_ref(),
        )
        .await?;
        return remove_local_messages(ctx, &folder.id, &payload.uids).await;
    }

    // No Archive or All folder: create a standard one and retry.
    if let Err(error) = conn.create("Archive").await {
        if crate::util::is_command_timeout(&error) {
            return Err(error);
        }
        debug!(%error, "could not create Archive folder");
    }
    crate::sync::folders::sync(ctx, conn, &account.id).await?;
    if let Some(archive) = role_folder(ctx, &account.id, FolderRole::Archive).await? {
        let mapping = execute_move(
            ctx,
            conn,
            account,
            selected,
            &folder.id,
            &archive.id,
            &payload.uids,
            &payload.message_ids,
            payload.source_uidvalidity,
            Some(&op.id),
        )
        .await?;
        if let Some(mapping) = mapping.as_ref() {
            apply_transfer_uids(
                ctx,
                &account.id,
                &archive.id,
                &payload.message_ids,
                &payload.uids,
                mapping,
            )
            .await?;
        }
        record_move_outcomes(
            ctx,
            &op.id,
            &payload.message_ids,
            &payload.uids,
            &archive.id,
            mapping.as_ref(),
        )
        .await?;
        remove_local_messages(ctx, &folder.id, &payload.uids).await
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
    selected: &mut Option<SelectedFolder>,
    op: &PendingOp,
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
    let mapping = execute_move(
        ctx,
        conn,
        account,
        selected,
        &payload.folder_id,
        &target.id,
        &payload.uids,
        &payload.message_ids,
        payload.source_uidvalidity,
        Some(&op.id),
    )
    .await?;
    if let Some(mapping) = mapping.as_ref() {
        apply_transfer_uids(
            ctx,
            &account.id,
            &target.id,
            &payload.message_ids,
            &payload.uids,
            mapping,
        )
        .await?;
    }
    record_move_outcomes(
        ctx,
        &op.id,
        &payload.message_ids,
        &payload.uids,
        &target.id,
        mapping.as_ref(),
    )
    .await?;
    remove_local_messages(ctx, &payload.folder_id, &payload.uids).await
}

struct PermanentDelete<'a> {
    account_id: &'a str,
    folder_id: &'a str,
    uids: &'a [i64],
    source_uidvalidity: Option<i64>,
    /// Set when the caller owns a `delete` operation whose payload must record
    /// that the `\Deleted` STORE already reached the server.
    side_effect: Option<(&'a PendingOp, &'a DeleteOp)>,
}

async fn execute_permanent_delete(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    selected: &mut Option<SelectedFolder>,
    request: &PermanentDelete<'_>,
) -> Result<()> {
    let PermanentDelete {
        account_id,
        folder_id,
        uids,
        source_uidvalidity,
        side_effect,
    } = *request;
    if uids.is_empty() {
        return Ok(());
    }
    // Refuse *before* the STORE. Setting `\Deleted` and then settling the op
    // would leave the message hidden locally while still present on the
    // server (and permanently un-undoable), and a whole-folder EXPUNGE would
    // erase mail other clients flagged. No directed expunge means no safe
    // permanent delete, so nothing is attempted.
    if !conn.caps.directed_expunge() {
        return Err(AppError::unsupported("permanent delete requires UIDPLUS"));
    }
    let folder = folder_by_id(ctx, folder_id).await?;
    // A folder id from another account is a stale/cross-account binding. Two
    // mailboxes can share a Drafts path and UIDVALIDITY, so selecting its path
    // on this account's connection would address unrelated mail. Refuse and let
    // the post-op resync reconcile the row.
    if folder.account_id != account_id {
        return Err(AppError::protocol(format!(
            "folder {} belongs to another account; refusing to delete",
            folder.remote_path
        )));
    }
    let uidvalidity = ensure_selected(conn, selected, &folder.remote_path).await?;
    verify_uidvalidity(
        uidvalidity,
        source_uidvalidity,
        &folder.remote_path,
        "delete",
    )?;
    if let Some((op, _)) = side_effect {
        // The STORE is the first destructive step. Persist the intent before
        // it: a crash after the store but before `deleted_flag_set` is written
        // still leaves durable evidence that the server may have the flag.
        mark_send_intent(ctx, &op.id).await?;
    }
    let set = uid_set(uids);
    store(conn, &set, "+FLAGS.SILENT (\\Deleted)").await?;
    // Persist the side effect before the expunge. If the EXPUNGE is refused or
    // its outcome becomes unknown, the operation must not be rolled back as if
    // nothing reached the server: unhiding the row or trusting its old UID
    // would be wrong until the folder is reconciled.
    if let Some((op, payload)) = side_effect {
        if !payload.deleted_flag_set {
            let mut side_effected = payload.clone();
            side_effected.deleted_flag_set = true;
            let value = serde_json::to_value(&side_effected)
                .map_err(|error| AppError::internal(format!("cannot encode delete: {error}")))?;
            let op_id = op.id.clone();
            ctx.db
                .write(move |db| repo::pending_ops::update_payload(db, &op_id, &value))
                .await?;
        }
    }
    expire(conn, &set).await
}

/// APPEND a draft, replacing the previous server copy when the payload names
/// one.
///
/// The replacement is deliberately all-or-nothing: without a directed expunge
/// the old copy cannot be removed without erasing unrelated mail, so the
/// executor appends nothing. The local draft is already persisted by
/// `compose_save` and the old remote copy (with its local index row) stays
/// visible in Drafts. This is what keeps repeated autosaves from piling up
/// one server copy per save.
async fn execute_append_draft(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    selected: &mut Option<SelectedFolder>,
    account: &Account,
    op: &PendingOp,
    payload: &AppendDraftOp,
) -> Result<()> {
    // A folder id from another account is a stale/cross-account binding. Two
    // mailboxes can share the Drafts path and UIDVALIDITY, so the commands
    // below would run against unrelated mail in this account. Refuse before
    // any destructive step and let the post-op resync reconcile the row.
    let folder = folder_by_id(ctx, &payload.folder_id).await?;
    if folder.account_id != account.id {
        return Err(AppError::protocol(format!(
            "draft folder {} belongs to another account; refusing the APPEND",
            folder.remote_path
        )));
    }
    if payload.discard_requested && payload.cleanup_done {
        // The cleanup already ran (possibly in a previous process); repeating
        // it could address a UID the folder has since reused or rebuilt.
        return Ok(());
    }
    // Resume from the phase the previous attempt reached. A server-side APPEND
    // is never repeated once its literal may have been committed.
    match payload.append_state {
        AppendState::Acked => resume_acked_append(ctx, conn, selected, account, op, payload).await,
        AppendState::Unknown => {
            resume_unknown_append(ctx, conn, selected, account, op, payload).await
        }
        // `Preparing` proves no literal was written yet; the replacement may be
        // run again even if a crash interrupted the delete.
        AppendState::Preparing | AppendState::Idle => {
            run_append_replacement(ctx, conn, selected, account, op, payload).await
        }
    }
}

/// Run (or re-run) the delete-then-append replacement.
///
/// The staged bytes are read and validated exactly once per attempt and that
/// same buffer is uploaded after the old copy is removed. Nothing between the
/// two phases can swap in a different, unvalidated message.
async fn run_append_replacement(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    selected: &mut Option<SelectedFolder>,
    account: &Account,
    op: &PendingOp,
    payload: &AppendDraftOp,
) -> Result<()> {
    if payload.discard_requested {
        // The draft was discarded/sent after this phase was persisted but
        // before the literal was written. `Idle` is cancelled by the service;
        // `Preparing` is completed here without uploading. The service kept
        // the local projection so this cleanup could still own its removal;
        // there is no upload to clean up, so the row may settle. The previous
        // copy is not deleted here: without a confirmed identity the service
        // never queued a header-derived delete for it.
        debug!(
            draft = %payload.draft_id,
            phase = ?payload.append_state,
            "not uploading a discarded draft"
        );
        mark_cleanup_done(ctx, &op.id).await?;
        remove_draft_projection(ctx, account, payload, None).await?;
        return Ok(());
    }
    let previous = payload.replaces.as_ref();
    if payload.append_state == AppendState::Idle
        && previous.is_none()
        && payload.replaces_unidentified
    {
        // An earlier APPEND for this draft may already have created a
        // server copy, but the Drafts sync has not bound its UID yet.
        // Uploading now would leave a copy we cannot address for deletion,
        // so wait for the sync instead of accumulating drafts.
        debug!(
            draft = %payload.draft_id,
            "previous remote draft is not indexed yet; not appending a duplicate"
        );
        return Ok(());
    }

    // A replacement naming another account's copy is never safe: the paths and
    // UIDVALIDITY can coincide, so the expunge would address unrelated mail on
    // this connection. Refuse before the removal claim and let the resync
    // reconcile the operation.
    if let Some(previous) = previous {
        if previous
            .account_id
            .as_deref()
            .is_some_and(|id| id != account.id)
        {
            return Err(AppError::protocol(
                "superseded draft belongs to another account; refusing the replacement",
            ));
        }
        match folder_by_id(ctx, &previous.folder_id).await {
            Ok(previous_folder) if previous_folder.account_id != account.id => {
                return Err(AppError::protocol(format!(
                    "superseded draft folder {} belongs to another account; refusing the replacement",
                    previous_folder.remote_path
                )));
            }
            Ok(_) => {}
            Err(error) => {
                // The folder row is gone (account removed or folder deleted);
                // there is nothing this connection could safely delete.
                debug!(
                    draft = %payload.draft_id,
                    folder = %previous.folder_id,
                    %error,
                    "superseded draft folder is unavailable; waiting for a sync"
                );
                defer_append(ctx, &op.id).await?;
                return Ok(());
            }
        }
    }

    // Track the removal phase locally as well: the payload snapshot was read
    // once and must not be used to decide a later destructive step.
    let mut removal = payload.removal_state;
    if let Some(previous) = previous {
        if removal != DraftRemovalState::Removed && !conn.caps.directed_expunge() {
            debug!(
                draft = %payload.draft_id,
                folder = %previous.folder_id,
                uid = previous.uid,
                "no directed expunge: keeping the previous remote draft instead of appending a replacement"
            );
            return Ok(());
        }
        match removal {
            DraftRemovalState::Removed => {}
            DraftRemovalState::NotStarted => {
                // The queued UID is only trustworthy while the projection that
                // owns it still points at it. For a draft created by another
                // client the old UID belongs to the external row, not to the
                // new projection, so both rows are inspected (see
                // `remote_draft_owns_uid`).
                if !remote_draft_owns_uid(ctx, account, payload, previous).await? {
                    debug!(
                        draft = %payload.draft_id,
                        folder = %previous.folder_id,
                        uid = previous.uid,
                        "previous remote draft no longer owns the queued UID; waiting for sync"
                    );
                    return Ok(());
                }
            }
            DraftRemovalState::PendingRemove | DraftRemovalState::Unknown => {
                // A previous attempt may already have expunged the copy. Never
                // repeat a destructive command on a guess: ask the selected
                // mailbox whether the UID still exists.
                match previous_removal_status(ctx, conn, selected, &account.id, previous).await? {
                    PreviousRemoval::Removed => {
                        record_removal_state(ctx, &op.id, DraftRemovalState::Removed).await?;
                        removal = DraftRemovalState::Removed;
                    }
                    PreviousRemoval::Present => {
                        // The copy still exists; the claim below re-checks that
                        // this worker may still expunge it.
                        removal = DraftRemovalState::PendingRemove;
                    }
                    PreviousRemoval::Unknown => {
                        record_removal_state(ctx, &op.id, DraftRemovalState::Unknown).await?;
                        defer_append(ctx, &op.id).await?;
                        return Ok(());
                    }
                }
            }
        }
    }

    // Validate the replacement *before* deleting the previous copy: a missing,
    // unreadable or empty staged file must never cost the user their last
    // server draft. The validated buffer is the one uploaded below.
    let prepared = prepare_append(ctx, &payload.folder_id, &payload.rfc822_path).await?;

    if payload.append_state == AppendState::Idle {
        // Claim the operation in the database *before* the first destructive
        // step. A discard/send that wins this race cancels the row and no
        // APPEND is ever sent; after this point the upload owns the copy and a
        // discard is carried out by the cleanup pass instead.
        let (uidnext, uidvalidity) = append_marker(ctx, &payload.folder_id).await?;
        if !reserve_append(ctx, &op.id, uidnext, uidvalidity).await? {
            debug!(
                draft = %payload.draft_id,
                "append was cancelled before it reached the wire"
            );
            return Ok(());
        }
    }

    if let Some(previous) = previous {
        if removal != DraftRemovalState::Removed {
            // Atomically claim the destructive phase: the claim check, the
            // suppression check and `PendingRemove` commit in one transaction.
            // A keep-local/discard that won the race makes this fail, and no
            // expunge may follow. Persisting the intent before the command also
            // means a crash after the server removed the copy is verified on
            // the retry instead of deleting again or skipping the upload.
            if !claim_removal_phase(ctx, &op.id).await? {
                debug!(
                    draft = %payload.draft_id,
                    "replacement was cancelled or kept local; skipping the expunge"
                );
                return Ok(());
            }
            // Remove the old copy first. Appending first would leave two
            // server copies whenever the delete turns out to be impossible (or
            // the UID belongs to a replaced generation).
            match execute_permanent_delete(
                ctx,
                conn,
                selected,
                &PermanentDelete {
                    account_id: &account.id,
                    folder_id: &previous.folder_id,
                    uids: &[previous.uid],
                    source_uidvalidity: previous.source_uidvalidity,
                    side_effect: None,
                },
            )
            .await
            {
                Ok(()) => {
                    record_removal_state(ctx, &op.id, DraftRemovalState::Removed).await?;
                    removal = DraftRemovalState::Removed;
                }
                Err(error) => {
                    // The literal has not been sent yet, so keep `Preparing`
                    // and let the retry verify the removal. Marking the
                    // removal unknown prevents a blind second expunge.
                    record_removal_state(ctx, &op.id, DraftRemovalState::Unknown).await?;
                    *selected = None;
                    return Err(error);
                }
            }
            *selected = None;
        }
    }

    // Record that the literal may reach the server before it is written, so a
    // crash between the write and the completion cannot lead to a blind retry.
    // The marker snapshot lets a later verification tell this copy apart from
    // the one the replacement just deleted. Size and time narrow the evidence
    // to one copy before anything may be deleted.
    let (uidnext, uidvalidity) = append_marker(ctx, &payload.folder_id).await?;
    if !mark_append_unknown(
        ctx,
        &op.id,
        uidnext,
        uidvalidity,
        prepared.bytes.len() as u64,
        Utc::now(),
    )
    .await?
    {
        debug!(
            draft = %payload.draft_id,
            "append was discarded before it reached the wire"
        );
        return Ok(());
    }
    // The `Unknown` claim above is the send right for the literal. A
    // cancellation that commits after it cannot be treated as "not sent":
    // the upload may already be on the wire, so it proceeds and its receipt is
    // recorded by `record_append_acked` even on a cancelled row. A
    // cancellation that commits before the claim made the claim fail above.
    *selected = None;
    let appended = match conn
        .append_with_uid(&prepared.remote_path, Some("(\\Draft)"), &prepared.bytes)
        .await
    {
        Ok(appended) => appended,
        Err(failure) => {
            if !failure.sent || failure.rejected {
                // Either the literal never left the client, or the server
                // explicitly refused it: the APPEND provably did not commit.
                if matches!(removal, DraftRemovalState::Removed) && payload.replaces.is_some() {
                    // The old copy is already gone. Resume with the APPEND
                    // only: `Preparing` skips the reservation, and the
                    // persisted `Removed` skips the delete on the retry.
                    if let Err(error) = resume_append_after_removal(ctx, &op.id).await {
                        debug!(%error, "could not record the APPEND-only retry state");
                    }
                } else if let Err(error) = reset_append_idle(ctx, &op.id).await {
                    debug!(%error, "could not reset the APPEND phase after an unsent failure");
                }
            }
            return Err(failure.into_error());
        }
    };
    // Keep the accepted UID on the operation itself before touching any local
    // row: if the commit below fails, the retry must not APPEND again.
    record_append_acked(
        ctx,
        &op.id,
        appended.map(|appended| appended.uid),
        appended.map(|appended| appended.uidvalidity),
    )
    .await?;
    if load_append_payload(ctx, &op.id)
        .await?
        .is_some_and(|payload| payload.discard_requested)
    {
        // The user discarded/sent the draft while the upload was in flight. The
        // copy is now addressable (or will be after a verification pass), so
        // remove it instead of binding it to the projection.
        return discard_appended_copy(ctx, conn, selected, account, op, payload, appended).await;
    }
    commit_appended_draft(ctx, account, &op.id, payload, appended).await
}

/// Resume an APPEND whose completion was lost.
///
/// An `Unknown` operation never re-appends: absence in the Drafts folder is not
/// proof that a delayed literal did not land, and treating the superseded copy
/// as the new one would leave the user without an upload. The operation stays
/// queued until the copy is found or the user discards/sends the draft.
async fn resume_unknown_append(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    selected: &mut Option<SelectedFolder>,
    account: &Account,
    op: &PendingOp,
    payload: &AppendDraftOp,
) -> Result<()> {
    if payload.discard_requested && !draft_row_exists(ctx, &payload.draft_id).await? {
        // The draft was discarded and the local row is gone. The completion
        // was lost, so this attempt can never obtain an APPENDUID and no later
        // attempt will re-receive the tagged response. The copy is kept (a
        // same-header candidate is never deletable), and re-verifying every five
        // minutes could not change that. Settle as "keep the remote copy".
        debug!(
            draft = %payload.draft_id,
            "unattributed cleanup has no local draft left; keeping the remote copy"
        );
        mark_append_candidate_only(ctx, &op.id).await?;
        mark_cleanup_done(ctx, &op.id).await?;
        return Ok(());
    }
    match verify_appended_draft(ctx, conn, selected, account, payload).await? {
        VerifiedAppend::Found { candidate } => {
            // The header proves *a* copy exists, not that this attempt wrote
            // it. Keep that fact on the settled operation so a later
            // discard/send cannot treat the synced UID as owned.
            mark_append_candidate_only(ctx, &op.id).await?;
            pin_draft_remote_identity(
                ctx,
                &account.id,
                &payload.draft_id,
                Some(RemoteDraftIdentity::candidate(
                    Some(payload.folder_id.clone()),
                    Some(draft_message_id(&payload.draft_id)),
                )),
            )
            .await?;
            debug!(
                draft = %payload.draft_id,
                addressable = candidate.is_some(),
                "verified an uncertain APPEND copy; it is candidate only"
            );
            if payload.discard_requested {
                // No APPENDUID was received by this attempt, so no UID can be
                // attributed to it. Deleting the candidate could erase another
                // client's draft; keep the cleanup pending and verify again.
                debug!(
                    draft = %payload.draft_id,
                    "the APPEND copy cannot be attributed to this attempt; keeping the discard pending"
                );
                defer_append(ctx, &op.id).await?;
                return Ok(());
            }
            // The targeted sync bound the copy where it could; drop the
            // superseded projection row if it is still the external one.
            remove_replaced_projection(ctx, account, payload).await?;
            Ok(())
        }
        VerifiedAppend::Inconclusive => {
            // Not found is not the same as absent: a delayed literal, a missing
            // header or a copy outside the sync window all look the same. Keep
            // the operation in `Unknown` and check again later.
            debug!(
                draft = %payload.draft_id,
                "cannot prove the uncertain APPEND was absent; keeping it for verification"
            );
            defer_append(ctx, &op.id).await?;
            Ok(())
        }
    }
}

/// Apply the persisted cleanup intent for an accepted APPEND.
async fn resume_acked_append(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    selected: &mut Option<SelectedFolder>,
    account: &Account,
    op: &PendingOp,
    payload: &AppendDraftOp,
) -> Result<()> {
    let appended = match payload.appended_uid {
        Some(uid) => payload
            .appended_uidvalidity
            .map(|uidvalidity| AppendedUid { uid, uidvalidity }),
        None => None,
    };
    if payload.discard_requested {
        return discard_appended_copy(ctx, conn, selected, account, op, payload, appended).await;
    }
    // Only the local commit is retried; the APPEND already succeeded.
    commit_appended_draft(ctx, account, &op.id, payload, appended).await
}

/// Delete the copy an accepted APPEND created after the draft was discarded.
///
/// The operation still owns the upload, so it must finish the cleanup even
/// though the projection row may already be gone: a UID-addressed delete when
/// the receipt is still in the current generation, otherwise a header
/// verification first.
async fn discard_appended_copy(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    selected: &mut Option<SelectedFolder>,
    account: &Account,
    op: &PendingOp,
    payload: &AppendDraftOp,
    appended: Option<AppendedUid>,
) -> Result<()> {
    // Prefer the UID this attempt just received; the persisted payload copy is
    // the fallback for a retry.
    let appended = appended.or(match (payload.appended_uid, payload.appended_uidvalidity) {
        (Some(uid), Some(uidvalidity)) => Some(AppendedUid { uid, uidvalidity }),
        _ => None,
    });
    if appended.is_none() && !draft_row_exists(ctx, &payload.draft_id).await? {
        // The local draft was discarded, and neither the accepted APPEND nor a
        // lost completion returned an APPENDUID. No future attempt can obtain
        // one, so the cleanup is settled as "keep the remote copy" instead of
        // being verified every five minutes forever.
        debug!(
            draft = %payload.draft_id,
            "unattributed discard cleanup has no local draft left; keeping the remote copy"
        );
        mark_append_candidate_only(ctx, &op.id).await?;
        mark_cleanup_done(ctx, &op.id).await?;
        return Ok(());
    }
    let current = current_folder_generation(ctx, &payload.folder_id).await?;
    if let Some(appended) =
        appended.filter(|appended| current.is_none() || current == Some(appended.uidvalidity))
    {
        // Claim the destructive step atomically. This is the cleanup's own
        // send right, independent of the APPEND's phase markers: a keep-local
        // that commits before the claim cancels the row and makes it fail
        // (nothing is sent), while one that commits after cannot stop the
        // command -- its outcome is recorded even on a cancelled row.
        if !claim_cleanup_phase(ctx, &op.id).await? {
            debug!(
                draft = %payload.draft_id,
                "discard cleanup lost the claim to a keep-local; skipping the expunge"
            );
            return Ok(());
        }
        // The server-assigned UID still addresses this copy.
        if let Err(error) = execute_permanent_delete(
            ctx,
            conn,
            selected,
            &PermanentDelete {
                account_id: &account.id,
                folder_id: &payload.folder_id,
                uids: &[appended.uid],
                source_uidvalidity: Some(appended.uidvalidity),
                side_effect: None,
            },
        )
        .await
        {
            // The claim already committed; the retry owns the same UID. The
            // generation was verified above, so re-issuing the directed
            // expunge cannot address a different message.
            *selected = None;
            return Err(error);
        }
        *selected = None;
        mark_cleanup_done(ctx, &op.id).await?;
        clear_draft_identity_if_matches(ctx, payload, appended).await?;
        remove_draft_projection(ctx, account, payload, Some(appended)).await?;
        remove_replaced_projection(ctx, account, payload).await?;
        return Ok(());
    }
    match verify_appended_draft(ctx, conn, selected, account, payload).await? {
        // A same-header copy exists, but without this attempt's APPENDUID it
        // cannot be attributed. Deleting it could erase another client's
        // draft, so the cleanup stays pending until the user resolves it.
        VerifiedAppend::Found { .. } => {
            mark_append_candidate_only(ctx, &op.id).await?;
            pin_draft_remote_identity(
                ctx,
                &account.id,
                &payload.draft_id,
                Some(RemoteDraftIdentity::candidate(
                    Some(payload.folder_id.clone()),
                    Some(draft_message_id(&payload.draft_id)),
                )),
            )
            .await?;
            defer_append(ctx, &op.id).await?;
            Ok(())
        }
        VerifiedAppend::Inconclusive => {
            defer_append(ctx, &op.id).await?;
            Ok(())
        }
    }
}

/// Bind an accepted APPEND to the local projection and clean up a superseded
/// external draft row.
async fn commit_appended_draft(
    ctx: &CoreContext,
    account: &Account,
    op_id: &str,
    payload: &AppendDraftOp,
    appended: Option<AppendedUid>,
) -> Result<()> {
    // Pin the durable identity first: it is what a later save or cleanup
    // consults, independently of where a sync moves the projection.
    let identity = match appended {
        Some(appended) => RemoteDraftIdentity::confirmed(
            &payload.folder_id,
            appended.uid,
            Some(appended.uidvalidity),
            Some(draft_message_id(&payload.draft_id)),
        ),
        None => {
            // The server accepted the literal but did not return an APPENDUID.
            // The sync can still bind the copy by header, but that UID is not
            // evidence of ownership: a later discard/send must not delete it
            // automatically. Record the limitation on the operation itself so
            // the fact survives `Done` (and the pruning pass).
            mark_append_candidate_only(ctx, op_id).await?;
            RemoteDraftIdentity::candidate(
                Some(payload.folder_id.clone()),
                Some(draft_message_id(&payload.draft_id)),
            )
        }
    };
    let pin =
        pin_draft_remote_identity(ctx, &account.id, &payload.draft_id, Some(identity)).await?;
    if matches!(pin, IdentityPin::Untrusted | IdentityPin::MissingDraft) {
        // A receipt that no longer matches the draft was deliberately not
        // pinned; the projection below is account/folder-guarded as well.
        debug!(
            draft = %payload.draft_id,
            ?pin,
            "APPEND receipt was not bound to the local draft"
        );
    }
    let bound = persist_appended_draft_uid(ctx, account, payload, appended).await?;
    if !bound {
        // The projection moved (or was deleted) while the APPEND was in
        // flight. Do not write the UID into whatever folder now owns the row:
        // the targeted resync scheduled for this operation adopts the server
        // copy instead, so nothing is lost and nothing is misbound.
        debug!(
            draft = %payload.draft_id,
            folder = %payload.folder_id,
            "draft projection moved or disappeared; leaving the APPEND for a targeted sync"
        );
    }
    remove_replaced_projection(ctx, account, payload).await?;
    Ok(())
}

/// Remove the local Drafts projection of a draft after its remote copy was
/// confirmed deleted (discard/send cleanup).
///
/// The id is the stable projection id; the row is removed only when it still
/// belongs to the operation's account and folder, so a moved/rebound row is
/// left to the sync.
async fn remove_draft_projection(
    ctx: &CoreContext,
    account: &Account,
    payload: &AppendDraftOp,
    appended: Option<AppendedUid>,
) -> Result<()> {
    let id = draft_message_id(&payload.draft_id);
    let account_id = account.id.clone();
    let folder_id = payload.folder_id.clone();
    let delete_id = id.clone();
    let removed = ctx
        .db
        .write(move |db| {
            let Some(stored) = repo::messages::get(db, &delete_id)? else {
                return Ok(false);
            };
            if stored.summary.account_id != account_id || stored.summary.folder_id != folder_id {
                return Ok(false);
            }
            // Only remove the projection that still carries the cleanup's UID
            // and generation. A row rebound by a sync after the expunge belongs
            // to a newer server copy and must survive.
            if let Some(appended) = appended {
                if stored.uid != Some(appended.uid)
                    || stored.uidvalidity != Some(appended.uidvalidity)
                {
                    return Ok(false);
                }
            }
            repo::messages::delete_local(db, &[delete_id], true)?;
            Ok(true)
        })
        .await?;
    if removed {
        events::emit_mail_changed(
            ctx.events.as_ref(),
            account.id.clone(),
            vec![payload.folder_id.clone()],
            vec![id],
            events::MailChangeKind::Deleted,
        );
    }
    Ok(())
}

/// Remove the local row that projected a draft created by another client once
/// its replacement was accepted.
///
/// The replacement creates a new projection (with the stable draft Message-ID)
/// because the external row cannot be addressed by UID any more. Keeping the
/// old row would leave a second, stale draft in Drafts and make the next save
/// target a UID the server has already deleted.
async fn remove_replaced_projection(
    ctx: &CoreContext,
    account: &Account,
    payload: &AppendDraftOp,
) -> Result<()> {
    let Some(previous) = payload.replaces.as_ref() else {
        return Ok(());
    };
    let Some(original_id) = previous.message_id.clone() else {
        return Ok(());
    };
    if original_id == draft_message_id(&payload.draft_id) {
        return Ok(());
    }
    let account_id = account.id.clone();
    let folder_id = previous.folder_id.clone();
    let uid = previous.uid;
    let uidvalidity = previous.source_uidvalidity;
    let message_id = original_id.clone();
    let removed = ctx
        .db
        .write(move |db| {
            let Some(stored) = repo::messages::get(db, &message_id)? else {
                return Ok(false);
            };
            if stored.summary.account_id != account_id
                || stored.summary.folder_id != folder_id
                || stored.uid != Some(uid)
            {
                // The row changed since the replacement was queued (moved,
                // rebound or edited); leave it to the sync to resolve.
                return Ok(false);
            }
            if let Some(generation) = uidvalidity {
                if stored.uidvalidity != Some(generation) {
                    // The row was rebuilt in a newer generation; the queued UID
                    // addresses something else now.
                    return Ok(false);
                }
            }
            repo::messages::delete_local(db, &[message_id], true)?;
            Ok(true)
        })
        .await?;
    if removed {
        events::emit_mail_changed(
            ctx.events.as_ref(),
            account.id.clone(),
            vec![previous.folder_id.clone()],
            vec![original_id],
            events::MailChangeKind::Deleted,
        );
    }
    Ok(())
}

/// Whether the queued replacement UID is still owned by a local row that
/// projects this draft.
///
/// For a draft created by another client the old UID belongs to the external
/// message row, while the save writes a *new* projection. Checking only the
/// new projection therefore never matched and silently skipped every
/// replacement. Both rows are accepted, but only when they agree on account,
/// folder, UID and generation.
async fn remote_draft_owns_uid(
    ctx: &CoreContext,
    account: &Account,
    payload: &AppendDraftOp,
    previous: &RemoteDraftRef,
) -> Result<bool> {
    if previous
        .account_id
        .as_deref()
        .is_some_and(|id| id != account.id)
    {
        // Another account's binding never authorizes a delete here.
        return Ok(false);
    }
    let projection_id = draft_message_id(&payload.draft_id);
    let mut ids = vec![projection_id.clone()];
    if let Some(original) = previous.message_id.as_ref() {
        if *original != projection_id {
            ids.push(original.clone());
        }
    }
    let account_id = previous.account_id.clone();
    let current_account = account.id.clone();
    let folder_id = previous.folder_id.clone();
    let uid = previous.uid;
    let uidvalidity = previous.source_uidvalidity;
    ctx.db
        .read(move |db| {
            let folder_owned = repo::folders::get(db, &folder_id)?
                .is_some_and(|folder| folder.account_id == current_account);
            if !folder_owned {
                return Ok(false);
            }
            for id in ids {
                let Some(stored) = repo::messages::get(db, &id)? else {
                    continue;
                };
                if let Some(expected) = account_id.as_ref() {
                    if stored.summary.account_id != *expected {
                        continue;
                    }
                }
                if stored.summary.account_id != current_account {
                    continue;
                }
                if stored.summary.folder_id != folder_id || stored.uid != Some(uid) {
                    continue;
                }
                let generation_matches = match uidvalidity {
                    Some(validity) => stored.uidvalidity == Some(validity),
                    None => true,
                };
                if generation_matches {
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .await
}

/// What a targeted Drafts sync found for an APPEND whose completion was lost.
enum VerifiedAppend {
    /// A copy carrying this draft's stable header exists in Drafts.
    ///
    /// `candidate` is addressable evidence that *a* copy exists. It only
    /// restores the local projection's addressability and is **never**
    /// sufficient to delete anything: another client can write a copy with
    /// the same header, size and arrival time between the persisted `Unknown`
    /// marker and the literal. Only an `APPENDUID` received by this attempt
    /// (or another verifiable attempt-specific marker) may authorize
    /// deletion.
    Found {
        candidate: Option<(i64, Option<i64>)>,
    },
    /// The folder could not answer the question: no header, the copy may be
    /// outside the sync window, only the superseded copy is visible, or the
    /// folder changed generation. Absence is not proven.
    Inconclusive,
}

/// Check whether a previous `Unknown` APPEND actually landed.
///
/// The only reliable evidence is the stable draft Message-ID in the Drafts
/// folder. The targeted sync fetches the new messages and binds the local row
/// by header, so a successful check also restores addressability.
///
/// "Not found" is deliberately not reported as absent. A lost completion can
/// land after this query, a server may omit the header, and the visible copy
/// may be the one the replacement was supposed to delete.
///
/// A found copy is **candidate only**. The header, the pre-send `UIDNEXT`
/// watermark, the generation, a unique candidate, the size and the arrival
/// time can all be matched by a copy another client wrote between the
/// persisted `Unknown` marker and the literal, so they may narrow the search
/// for addressability but must never authorize deletion. Only an
/// `APPENDUID` received by this attempt (or another verifiable
/// attempt-specific marker) does that.
async fn verify_appended_draft(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    selected: &mut Option<SelectedFolder>,
    account: &Account,
    payload: &AppendDraftOp,
) -> Result<VerifiedAppend> {
    let folder = folder_by_id(ctx, &payload.folder_id).await?;
    // The sync issues its own SELECT; the cached selection is stale afterwards.
    *selected = None;
    crate::sync::sync_folder_by_id_with(ctx, conn, account, &folder.id, false).await?;
    *selected = None;
    let header = crate::util::draft_header_id(&payload.draft_id);
    let folder_id = folder.id;
    let account_id = account.id.clone();
    let previous = payload.replaces.clone();
    let marker = payload.attempt_uidnext;
    ctx.db
        .read(move |db| {
            let rows = repo::messages::by_message_id(db, &header, Some(&account_id))?;
            if rows.is_empty() {
                // No header: either the server did not store our bytes intact
                // or the copy is outside the synced window.
                return Ok(VerifiedAppend::Inconclusive);
            }
            let mut candidates = Vec::new();
            for row in rows {
                if row.summary.folder_id != folder_id {
                    continue;
                }
                let Some(uid) = row.uid else {
                    continue;
                };
                // The copy the replacement was meant to delete is not evidence
                // that the new APPEND landed.
                let superseded = previous.as_ref().is_some_and(|previous| {
                    previous.uid == uid
                        && match previous.source_uidvalidity {
                            Some(generation) => row.uidvalidity == Some(generation),
                            None => true,
                        }
                });
                if superseded {
                    continue;
                }
                // A UID below the pre-send high-water mark already existed when
                // the literal was written; it cannot be this APPEND's copy.
                if marker.is_some_and(|marker| uid < marker) {
                    continue;
                }
                candidates.push(row);
            }
            let Some(first) = candidates.first() else {
                return Ok(VerifiedAppend::Inconclusive);
            };
            // A copy whose header exists but is not addressable (no UID) is a
            // candidate only; the next sync may bind it.
            Ok(VerifiedAppend::Found {
                candidate: first.uid.map(|uid| (uid, first.uidvalidity)),
            })
        })
        .await
}

/// Bind the new server copy to the local projection immediately.
///
/// The update only succeeds while the row is still the same draft projection
/// in the expected Drafts folder. With a UIDPLUS `APPENDUID` the projection is
/// addressable again without waiting for a Drafts sync; without one, the old
/// UID must still be discarded so the next save does not delete a number that
/// no longer exists. Returns whether the projection was actually updated.
async fn persist_appended_draft_uid(
    ctx: &CoreContext,
    account: &Account,
    payload: &AppendDraftOp,
    appended: Option<AppendedUid>,
) -> Result<bool> {
    let id = draft_message_id(&payload.draft_id);
    let account_id = account.id.clone();
    let folder_id = payload.folder_id.clone();
    let superseded = payload
        .replaces
        .as_ref()
        .map(|previous| (previous.uid, previous.source_uidvalidity));
    let changed = ctx
        .db
        .write(move |db| {
            if let Some(appended) = appended {
                // A concurrent sync may already own this UID in the folder
                // (e.g. the projection moved and a new row adopted the copy).
                // Binding it to a second row would violate the folder/UID
                // uniqueness or address the wrong message.
                if let Some(existing) = repo::messages::by_uid(db, &folder_id, appended.uid)? {
                    if existing.summary.id != id {
                        return Ok(0);
                    }
                }
            }
            repo::messages::bind_appended_draft(
                db,
                &id,
                &account_id,
                &folder_id,
                appended.map(|appended| appended.uid),
                appended.map(|appended| appended.uidvalidity),
                superseded,
            )
        })
        .await?;
    Ok(changed > 0)
}

/// The folder's last known `(UIDNEXT, UIDVALIDITY)`, used to fingerprint the
/// point in the mailbox where an APPEND was written.
async fn append_marker(ctx: &CoreContext, folder_id: &str) -> Result<(Option<i64>, Option<i64>)> {
    let folder_id = folder_id.to_string();
    ctx.db
        .read(move |db| {
            let (uidvalidity, uidnext, _) = repo::folders::sync_flags(db, &folder_id)?;
            Ok((uidnext, uidvalidity))
        })
        .await
}

async fn current_folder_generation(ctx: &CoreContext, folder_id: &str) -> Result<Option<i64>> {
    let folder_id = folder_id.to_string();
    ctx.db
        .read(move |db| repo::folders::uidvalidity(db, &folder_id))
        .await
}

/// Whether the local draft row still exists.
///
/// A cleanup whose draft row is gone was queued by a discard/send. If no
/// `APPENDUID` was ever received, no future attempt can obtain one, so there
/// is nothing left to attribute: the remote copy is kept and the verification
/// loop must stop instead of running every five minutes forever.
async fn draft_row_exists(ctx: &CoreContext, draft_id: &str) -> Result<bool> {
    let draft_id = draft_id.to_string();
    ctx.db
        .read(move |db| Ok(repo::drafts::get(db, &draft_id)?.is_some()))
        .await
}

/// Read the latest persisted APPEND payload (it may carry a discard intent
/// written while this worker was running).
async fn load_append_payload(ctx: &CoreContext, op_id: &str) -> Result<Option<AppendDraftOp>> {
    let op_id = op_id.to_string();
    ctx.db
        .read(move |db| {
            let Some(op) = repo::pending_ops::get(db, &op_id)? else {
                return Ok(None);
            };
            Ok(Some(repo::pending_ops::payload::<AppendDraftOp>(&op)?))
        })
        .await
}

/// Mutate the persisted APPEND payload in one serialized write.
///
/// The closure sees the *latest* payload, so a discard intent written while the
/// worker was busy is preserved instead of being clobbered by a stale copy.
/// A cancelled row is never rewritten. Returns whether the payload changed.
async fn mutate_append_payload(
    ctx: &CoreContext,
    op_id: &str,
    mutate: impl FnOnce(&mut AppendDraftOp) -> bool + Send + 'static,
) -> Result<bool> {
    let op_id = op_id.to_string();
    ctx.db
        .write(move |db| {
            let Some(op) = repo::pending_ops::get(db, &op_id)? else {
                return Ok(false);
            };
            if op.state == PendingOpState::Cancelled {
                return Ok(false);
            }
            let mut payload: AppendDraftOp = repo::pending_ops::payload(&op)?;
            if !mutate(&mut payload) {
                return Ok(false);
            }
            let value = serde_json::to_value(payload).map_err(|error| {
                AppError::internal(format!("cannot encode appendDraft: {error}"))
            })?;
            Ok(repo::pending_ops::update_payload(db, &op_id, &value)? > 0)
        })
        .await
}

/// Atomically verify that this worker still owns the next phase and apply the
/// phase update in the same serialized write transaction.
///
/// This is the send-right claim. A "keep local" that commits its cancellation
/// (or suppression marker) before this call makes the claim fail, and no
/// destructive command may follow a failed claim. A keep-local that arrives
/// after a successful claim cannot treat the phase as "not sent": the outcome
/// is recorded by [`record_append_payload`] even on a cancelled row.
async fn claim_append_phase(
    ctx: &CoreContext,
    op_id: &str,
    mutate: impl FnOnce(&mut AppendDraftOp) -> bool + Send + 'static,
) -> Result<bool> {
    let op_id = op_id.to_string();
    ctx.db
        .write(move |db| {
            let Some(op) = repo::pending_ops::get(db, &op_id)? else {
                return Ok(false);
            };
            // The worker claimed the row before running; anything else means a
            // keep-local/discard won the race and this phase must not run.
            if op.state != PendingOpState::InProgress {
                return Ok(false);
            }
            let mut payload: AppendDraftOp = repo::pending_ops::payload(&op)?;
            // A discard/send records a cleanup intent and may suppress further
            // autosaves; that intent explicitly owns the destructive steps. Any
            // other suppression means "keep local", which forbids them.
            if !payload.discard_requested
                && repo::drafts::is_sync_suppressed(db, &payload.draft_id)?
            {
                return Ok(false);
            }
            if !mutate(&mut payload) {
                return Ok(false);
            }
            let value = serde_json::to_value(payload).map_err(|error| {
                AppError::internal(format!("cannot encode appendDraft: {error}"))
            })?;
            Ok(repo::pending_ops::update_payload(db, &op_id, &value)? > 0)
        })
        .await
}

/// Persist a *result* produced by a command that has already been issued.
///
/// Unlike [`mutate_append_payload`], this also writes to a row that was
/// cancelled while the command was in flight. The row stays cancelled, but the
/// receipt/removal outcome must survive: it is the only evidence a later
/// cleanup or a reopened draft has about what may exist on the server.
async fn record_append_payload(
    ctx: &CoreContext,
    op_id: &str,
    mutate: impl FnOnce(&mut AppendDraftOp) -> bool + Send + 'static,
) -> Result<bool> {
    let op_id = op_id.to_string();
    ctx.db
        .write(move |db| {
            let Some(op) = repo::pending_ops::get(db, &op_id)? else {
                return Ok(false);
            };
            let mut payload: AppendDraftOp = repo::pending_ops::payload(&op)?;
            if !mutate(&mut payload) {
                return Ok(false);
            }
            let value = serde_json::to_value(payload).map_err(|error| {
                AppError::internal(format!("cannot encode appendDraft: {error}"))
            })?;
            Ok(repo::pending_ops::update_payload(db, &op_id, &value)? > 0)
        })
        .await
}

/// Atomically move `Idle` -> `Preparing` before the first destructive step.
///
/// `Preparing` is the proof a discard needs: cancelling is only allowed while
/// the literal cannot be on the wire. After this point the operation owns the
/// upload and a discard is recorded as cleanup intent instead.
async fn reserve_append(
    ctx: &CoreContext,
    op_id: &str,
    uidnext: Option<i64>,
    uidvalidity: Option<i64>,
) -> Result<bool> {
    claim_append_phase(ctx, op_id, move |payload| {
        if payload.append_state != AppendState::Idle {
            return false;
        }
        payload.append_state = AppendState::Preparing;
        payload.attempt_uidnext = uidnext;
        payload.attempt_uidvalidity = uidvalidity;
        true
    })
    .await
}

/// Move `Preparing` -> `Unknown` immediately before the literal is written.
///
/// The size and start time are stored with the marker: they are half of the
/// evidence a later verification needs before it may delete the copy.
async fn mark_append_unknown(
    ctx: &CoreContext,
    op_id: &str,
    uidnext: Option<i64>,
    uidvalidity: Option<i64>,
    size: u64,
    at: chrono::DateTime<Utc>,
) -> Result<bool> {
    // This is the send-right claim for the literal. Cancellation that commits
    // before it wins and nothing is written; after it, the upload is treated as
    // possibly sent and its result is recorded even if the row was cancelled
    // while the completion was outstanding.
    claim_append_phase(ctx, op_id, move |payload| {
        if !matches!(
            payload.append_state,
            AppendState::Preparing | AppendState::Idle
        ) {
            return false;
        }
        payload.append_state = AppendState::Unknown;
        payload.attempt_uidnext = uidnext;
        payload.attempt_uidvalidity = uidvalidity;
        payload.attempt_size = Some(size);
        payload.attempt_at = Some(at);
        true
    })
    .await
}

/// Move `Unknown` -> `Acked`, keeping the accepted UID on the operation.
async fn record_append_acked(
    ctx: &CoreContext,
    op_id: &str,
    uid: Option<i64>,
    uidvalidity: Option<i64>,
) -> Result<bool> {
    // The literal already reached the server. A keep-local/discard that
    // committed while the completion was outstanding must not erase the
    // receipt: it is what decides whether a remote copy exists and whether a
    // later cleanup may address it.
    record_append_payload(ctx, op_id, move |payload| {
        payload.append_state = AppendState::Acked;
        payload.appended_uid = uid;
        payload.appended_uidvalidity = uidvalidity;
        true
    })
    .await
}

/// Undo the phase marker after an APPEND that provably did not commit.
async fn reset_append_idle(ctx: &CoreContext, op_id: &str) -> Result<bool> {
    mutate_append_payload(ctx, op_id, |payload| {
        payload.append_state = AppendState::Idle;
        payload.appended_uid = None;
        payload.appended_uidvalidity = None;
        payload.attempt_uidnext = None;
        payload.attempt_uidvalidity = None;
        payload.attempt_size = None;
        payload.attempt_at = None;
        true
    })
    .await
}

/// Record that the previous copy is gone and only the APPEND remains.
///
/// `Preparing` is deliberate: the retry must not re-run the reservation or the
/// replacement. The persisted `Removed` removal phase makes it skip the delete
/// even though the local row may have been removed by a Drafts sync.
async fn resume_append_after_removal(ctx: &CoreContext, op_id: &str) -> Result<bool> {
    mutate_append_payload(ctx, op_id, |payload| {
        payload.append_state = AppendState::Preparing;
        payload.appended_uid = None;
        payload.appended_uidvalidity = None;
        payload.attempt_uidnext = None;
        payload.attempt_uidvalidity = None;
        payload.attempt_size = None;
        payload.attempt_at = None;
        true
    })
    .await
}

/// Persist the outcome of the removal phase.
///
/// This records what a command already did (or could not determine), so it is
/// written even if a keep-local cancelled the row meanwhile. The row never
/// leaves `cancelled`; only its evidence is updated.
async fn record_removal_state(
    ctx: &CoreContext,
    op_id: &str,
    state: DraftRemovalState,
) -> Result<()> {
    record_append_payload(ctx, op_id, move |payload| {
        if payload.removal_state == state {
            return false;
        }
        payload.removal_state = state;
        true
    })
    .await?;
    Ok(())
}

/// Atomically claim the right to expunge the superseded copy.
///
/// The claim and the `PendingRemove` phase marker commit in one transaction. A
/// keep-local/suppression that won the race makes this fail, so the following
/// destructive command is never issued on behalf of an operation that no
/// longer owns it.
async fn claim_removal_phase(ctx: &CoreContext, op_id: &str) -> Result<bool> {
    claim_append_phase(ctx, op_id, |payload| {
        if payload.removal_state == DraftRemovalState::Removed {
            return false;
        }
        payload.removal_state = DraftRemovalState::PendingRemove;
        true
    })
    .await
}

/// Atomically claim the right to delete the copy an accepted APPEND created.
///
/// This is the cleanup's own send right, persisted in the same transaction as
/// the state/suppression check (see [`claim_append_phase`]). A "keep local"
/// that commits first cancels the row (and clears the discard intent), so the
/// claim fails and no `STORE`/`EXPUNGE` is issued. A claim that commits first
/// records the intent (`cleanupClaimed`) so the caller proceeds and records the
/// result even if the row is cancelled while the command is in flight.
async fn claim_cleanup_phase(ctx: &CoreContext, op_id: &str) -> Result<bool> {
    claim_append_phase(ctx, op_id, |payload| {
        if payload.cleanup_done {
            // Already completed (possibly in a previous process); a retry must
            // not address a UID the folder may have rebuilt.
            return false;
        }
        if payload.cleanup_claimed {
            // A previous attempt claimed the same cleanup but its command may
            // have failed before the result was recorded. The caller verified
            // the generation, so finishing it is safe.
            return true;
        }
        payload.cleanup_claimed = true;
        true
    })
    .await
}

/// Clear the pinned remote identity only when it still names the copy this
/// cleanup just removed.
///
/// A newer save/receipt may have pinned a different copy while the expunge was
/// in flight; erasing that identity would make the next send/discard lose the
/// only evidence of ownership.
async fn clear_draft_identity_if_matches(
    ctx: &CoreContext,
    payload: &AppendDraftOp,
    appended: AppendedUid,
) -> Result<()> {
    let id = payload.draft_id.clone();
    let folder_id = payload.folder_id.clone();
    ctx.db
        .write(move |db| {
            let Some(row) = repo::drafts::get(db, &id)? else {
                return Ok(());
            };
            let matches = row.remote_draft.as_ref().is_some_and(|identity| {
                identity.folder_id.as_deref() == Some(folder_id.as_str())
                    && identity.uid == Some(appended.uid)
                    && identity.uidvalidity == Some(appended.uidvalidity)
            });
            if matches {
                repo::drafts::set_remote_draft(db, &id, None)?;
            }
            Ok(())
        })
        .await
}

/// Remember that the remote copy this operation is associated with cannot be
/// attributed to this attempt. The flag is durable even after the operation
/// settles as `Done`, so a later discard/send cannot delete a copy by header
/// inference alone.
async fn mark_append_candidate_only(ctx: &CoreContext, op_id: &str) -> Result<()> {
    // Ownership evidence, not a decision: keep it even on a cancelled row so a
    // later cleanup can never turn the copy into a deletable UID.
    record_append_payload(ctx, op_id, |payload| {
        if payload.candidate_only {
            return false;
        }
        payload.candidate_only = true;
        true
    })
    .await?;
    Ok(())
}

/// Outcome of persisting a remote-draft identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentityPin {
    /// The draft still belongs to the executing account and (when present) the
    /// identity's folder does too. The identity was written.
    Pinned,
    /// The draft moved to another account, or the folder is no longer owned by
    /// this account, between the receipt and this write. Nothing was written
    /// and a full resync was queued for the draft's current Drafts folder.
    Untrusted,
    /// The local draft no longer exists (discarded or sent).
    MissingDraft,
}

/// Persist the durable identity of the remote copy this operation owns.
///
/// The receipt can arrive after the user switched the draft to another account.
/// This helper validates the *current* durable row inside the same transaction
/// as the write: an old account's folder/UID must never be pinned to a draft
/// that now belongs elsewhere (a later save would build a `replaces` for the
/// wrong mailbox and be rejected forever). A mismatch is treated as untrusted
/// evidence: the write is skipped, it is logged, and the draft's current Drafts
/// folder is queued for a full resync so the projection follows its own account.
async fn pin_draft_remote_identity(
    ctx: &CoreContext,
    account_id: &str,
    draft_id: &str,
    identity: Option<RemoteDraftIdentity>,
) -> Result<IdentityPin> {
    let id = draft_id.to_string();
    let account_id = account_id.to_string();
    let account_for_db = account_id.clone();
    let requested = identity;
    let (outcome, resync_folder) = ctx
        .db
        .write(move |db| {
            let Some(draft) = repo::drafts::get(db, &id)? else {
                return Ok((IdentityPin::MissingDraft, None));
            };
            let folder_owned = match requested
                .as_ref()
                .and_then(|identity| identity.folder_id.as_deref())
            {
                Some(folder_id) => repo::folders::get(db, folder_id)?
                    .is_some_and(|folder| folder.account_id == account_for_db),
                None => true,
            };
            if draft.account_id != account_for_db || !folder_owned {
                // Resync the folder of the account that owns the draft now; the
                // old account's copy is not ours to bind or clean up here.
                let resync = repo::folders::by_role(db, &draft.account_id, FolderRole::Drafts)?
                    .map(|folder| folder.id);
                if resync.is_none() {
                    // The receiving account has no Drafts folder yet, so there
                    // is no folder id to resync. Record the request; the next
                    // folder reconciliation consumes it once the folder exists.
                    repo::sync_state::set(
                        db,
                        &draft.account_id,
                        "mail",
                        crate::sync::folders::DRAFTS_RESYNC_KEY,
                        &nuncio_core::repo::now_string(),
                    )?;
                }
                return Ok((IdentityPin::Untrusted, resync));
            }
            repo::drafts::set_remote_draft(db, &id, requested.as_ref())?;
            Ok((IdentityPin::Pinned, None))
        })
        .await?;
    if outcome == IdentityPin::Untrusted {
        warn!(
            draft = %draft_id,
            account = %account_id,
            "ignoring a draft identity receipt that no longer belongs to this account"
        );
        if let Some(folder_id) = resync_folder {
            force_full_resync_db(ctx, &folder_id).await?;
        }
    }
    Ok(outcome)
}

/// Record that the cleanup demanded by `discard_requested` finished. The
/// finishing transaction only settles an operation whose cleanup is done.
async fn mark_cleanup_done(ctx: &CoreContext, op_id: &str) -> Result<()> {
    // The cleanup already ran; record it even if the row was cancelled while it
    // was in flight (the finishing pass will not resurrect the cancelled row).
    record_append_payload(ctx, op_id, |payload| {
        if payload.cleanup_done {
            return false;
        }
        payload.cleanup_done = true;
        true
    })
    .await?;
    Ok(())
}

/// Leave an inconclusive operation queued for another verification pass
/// without spending a retry.
async fn defer_append(ctx: &CoreContext, op_id: &str) -> Result<()> {
    let at = Utc::now() + chrono::Duration::minutes(5);
    let op_id = op_id.to_string();
    ctx.db
        .write(move |db| repo::pending_ops::defer(db, &op_id, at))
        .await?;
    Ok(())
}

/// The current durable row for an operation, re-read after the claim.
async fn load_pending_op(ctx: &CoreContext, op_id: &str) -> Result<Option<PendingOp>> {
    let op_id = op_id.to_string();
    ctx.db
        .read(move |db| repo::pending_ops::get(db, &op_id))
        .await
}

/// Whether the superseded copy a replacement was deleting still exists.
enum PreviousRemoval {
    Removed,
    Present,
    /// The mailbox could not answer (SELECT/SEARCH failed, no generation was
    /// recorded, or the folder was rebuilt). Never treat this as `Removed`.
    Unknown,
}

/// Ask the selected mailbox whether the superseded UID is still present.
///
/// The check is deliberately live rather than based on local rows: a Drafts
/// sync may already have removed the row that projected the old copy, so the
/// index cannot prove the message is gone.
async fn previous_removal_status(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    selected: &mut Option<SelectedFolder>,
    account_id: &str,
    previous: &RemoteDraftRef,
) -> Result<PreviousRemoval> {
    if previous
        .account_id
        .as_deref()
        .is_some_and(|id| id != account_id)
    {
        // The queued replacement belongs to another account; this session's
        // mailbox cannot say anything about it.
        return Ok(PreviousRemoval::Unknown);
    }
    let Some(expected_generation) = previous.source_uidvalidity else {
        debug!(
            folder = %previous.folder_id,
            uid = previous.uid,
            "previous draft has no generation; cannot verify its removal"
        );
        return Ok(PreviousRemoval::Unknown);
    };
    let Ok(folder) = folder_by_id(ctx, &previous.folder_id).await else {
        return Ok(PreviousRemoval::Unknown);
    };
    if folder.account_id != account_id {
        // Selecting this path on the current account's connection would ask the
        // wrong mailbox; neither presence nor absence is conclusive.
        return Ok(PreviousRemoval::Unknown);
    }
    // Force a fresh SELECT: the cached entry may describe a mailbox that was
    // closed or rebuilt since the previous command, and the UID may only be
    // interpreted against the generation the server reports now.
    *selected = None;
    let current = match ensure_selected(conn, selected, &folder.remote_path).await {
        Ok(current) => current,
        Err(error) => {
            debug!(%error, folder = %folder.remote_path, "cannot select Drafts to verify removal");
            return Ok(PreviousRemoval::Unknown);
        }
    };
    if current != Some(expected_generation) {
        // The UID belongs to a generation the mailbox no longer has; neither
        // presence nor absence can be concluded for the queued copy.
        return Ok(PreviousRemoval::Unknown);
    }
    match search_present_uids(conn, &[previous.uid]).await {
        Some(found) if found.contains(&previous.uid) => Ok(PreviousRemoval::Present),
        Some(_) => Ok(PreviousRemoval::Removed),
        None => Ok(PreviousRemoval::Unknown),
    }
}

/// Delete a superseded remote draft and, only after the server confirms it,
/// remove the local row that projected it.
///
/// A server without a directed expunge deterministically refuses this; the
/// operation fails and the projection row is kept, so the user can still see
/// the old draft in Drafts instead of a locally-faked deletion.
async fn execute_delete_remote_draft(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    selected: &mut Option<SelectedFolder>,
    account_id: &str,
    payload: &DeleteRemoteDraftOp,
) -> Result<()> {
    if !conn.caps.directed_expunge() {
        return Err(AppError::unsupported(
            "removing a remote draft requires UIDPLUS",
        ));
    }
    execute_permanent_delete(
        ctx,
        conn,
        selected,
        &PermanentDelete {
            account_id,
            folder_id: &payload.folder_id,
            uids: &[payload.uid],
            source_uidvalidity: payload.source_uidvalidity,
            side_effect: None,
        },
    )
    .await?;
    if let Some(message_id) = payload.message_id.as_deref() {
        let id = message_id.to_string();
        let account = account_id.to_string();
        let folder_id = payload.folder_id.clone();
        let uid = payload.uid;
        let uidvalidity = payload.source_uidvalidity;
        let message_id = message_id.to_string();
        // The delete confirmed the *queued* UID. A sync may have rebound the
        // row to another copy meanwhile; deleting by id alone would erase that
        // newer message, so require the row to still name this exact UID and
        // generation in this account and folder.
        let removed = ctx
            .db
            .write(move |db| {
                let Some(stored) = repo::messages::get(db, &id)? else {
                    return Ok(false);
                };
                if stored.summary.account_id != account
                    || stored.summary.folder_id != folder_id
                    || stored.uid != Some(uid)
                {
                    return Ok(false);
                }
                if let Some(expected) = uidvalidity {
                    if stored.uidvalidity != Some(expected) {
                        return Ok(false);
                    }
                }
                repo::messages::delete_local(db, &[id], true)?;
                Ok(true)
            })
            .await?;
        if removed {
            events::emit_mail_changed(
                ctx.events.as_ref(),
                account_id.to_string(),
                vec![payload.folder_id.clone()],
                vec![message_id],
                events::MailChangeKind::Deleted,
            );
        }
    }
    Ok(())
}

/// A staged RFC822 message validated and read before any destructive command.
struct PreparedAppend {
    remote_path: String,
    bytes: Vec<u8>,
}

/// Read and validate a staged message before the executor touches the server.
async fn prepare_append(ctx: &CoreContext, folder_id: &str, path: &str) -> Result<PreparedAppend> {
    let folder = folder_by_id(ctx, folder_id).await?;
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| AppError::io(format!("cannot read staged message: {error}")))?;
    if !metadata.is_file() {
        return Err(AppError::invalid_input("staged message is not a file"));
    }
    if metadata.len() == 0 {
        return Err(AppError::invalid_input("staged message is empty"));
    }
    if metadata.len() > 100 * 1024 * 1024 {
        return Err(AppError::invalid_input("staged message exceeds 100 MB"));
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| AppError::io(format!("cannot read staged message: {error}")))?;
    if bytes.is_empty() {
        return Err(AppError::invalid_input("staged message is empty"));
    }
    Ok(PreparedAppend {
        remote_path: folder.remote_path,
        bytes,
    })
}

async fn execute_append(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    selected: &mut Option<SelectedFolder>,
    folder_id: &str,
    path: &str,
    flags: Option<&str>,
) -> Result<Option<AppendedUid>> {
    let prepared = prepare_append(ctx, folder_id, path).await?;
    // APPEND works without selecting, but selecting clears `selected` state.
    *selected = None;
    conn.append_with_uid(&prepared.remote_path, flags, &prepared.bytes)
        .await
        .map_err(crate::imap::conn::AppendError::into_error)
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
    conn.create(&remote).await?;
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
    conn.rename(&folder.remote_path, &new_remote).await?;
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
    selected: &mut Option<SelectedFolder>,
    payload: &FolderDeleteOp,
) -> Result<()> {
    let folder = folder_by_id(ctx, &payload.folder_id).await?;
    // Empty the mailbox first; some servers refuse to delete a non-empty one.
    //
    // This SELECT must go through the same cache as every other UID command:
    // selecting another folder changes what UID STORE/MOVE act on. The cache
    // is cleared *before* the attempt because RFC 3501 says a failed SELECT
    // leaves no mailbox selected, and after a failed DELETE the server may
    // still have this folder selected. Either way the next operation must not
    // trust the UIDVALIDITY cached for a different folder.
    *selected = None;
    if folder.is_selectable {
        match conn.select(&folder.remote_path).await {
            Ok(mailbox) => {
                *selected = Some(SelectedFolder {
                    remote_path: folder.remote_path.clone(),
                    uidvalidity: mailbox.uid_validity.map(|value| value as i64),
                });
                // Emptying first is only an aid for servers that refuse to
                // DELETE a non-empty mailbox. It must never leave a whole
                // folder flagged `\Deleted` on a server that cannot expunge
                // only those messages, so skip it unless a directed expunge
                // is available; the DELETE below is the real deletion.
                if conn.caps.directed_expunge() {
                    let _ = store(conn, "1:*", "+FLAGS.SILENT (\\Deleted)").await;
                    let _ = expire(conn, "1:*").await;
                }
            }
            Err(error) => {
                debug!(%error, folder = %folder.remote_path, "SELECT for folder delete failed");
            }
        }
    }
    let delete_result = conn.delete(&folder.remote_path).await;
    // A successful DELETE removes the selected mailbox; a failed one may have
    // left it selected. Neither outcome can be described by the old cache, so
    // drop it and let the next operation SELECT (and verify) again.
    *selected = None;
    if let Err(error) = delete_result {
        debug!(%error, folder = %folder.remote_path, "DELETE mailbox failed");
        return Err(error);
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
    selected: &mut Option<SelectedFolder>,
    op_id: &str,
    payload: &FolderMarkReadOp,
) -> Result<()> {
    let folder = folder_by_id(ctx, &payload.folder_id).await?;
    // `1:*` does not depend on UID generations, but a targeted STORE does: the
    // cached numbers may have been synced before the mailbox was rebuilt.
    let live_uidvalidity = ensure_selected(conn, selected, &folder.remote_path).await?;
    // A single-message "mark unread" that ran while this operation was in
    // backoff wrote its own writer row. Sending `1:*` would overwrite that
    // newer intent on the server, so restrict the STORE to rows this operation
    // still owns (and fall back to `1:*` only when nothing was taken over).
    let targets = {
        let folder_id = payload.folder_id.clone();
        let op_id = op_id.to_string();
        ctx.db
            .read(move |db| repo::messages::folder_read_targets(db, &folder_id, &op_id))
            .await?
    };
    let all_owned = targets.iter().all(|(_, _, _, owned)| *owned);
    if all_owned {
        store(conn, "1:*", "+FLAGS.SILENT (\\Seen)").await?;
    } else {
        let mut owned_uids: Vec<i64> = Vec::new();
        for (_, uid, row_uidvalidity, owned) in &targets {
            if !*owned {
                continue;
            }
            let Some(uid) = uid else { continue };
            // A UID only addresses the message it was synced with inside one
            // generation. Never send a number whose generation the selected
            // mailbox does not confirm: it may mark an unrelated message read.
            let generation_matches = matches!(
                (row_uidvalidity, live_uidvalidity),
                (Some(row), Some(live)) if *row == live
            );
            if !generation_matches {
                warn!(
                    op = %op_id,
                    uid,
                    row = ?row_uidvalidity,
                    live = ?live_uidvalidity,
                    folder = %folder.remote_path,
                    "folderMarkRead skipped: a cached UID belongs to another generation; reconciling"
                );
                return Err(AppError::conflict(format!(
                    "folderMarkRead skipped: {} was rebuilt since these UIDs were synced",
                    folder.remote_path
                )));
            }
            owned_uids.push(*uid);
        }
        if !owned_uids.is_empty() {
            store(conn, &uid_set(&owned_uids), "+FLAGS.SILENT (\\Seen)").await?;
        }
    }
    let folder_id = payload.folder_id.clone();
    let op_id = op_id.to_string();
    ctx.db
        .write(move |db| {
            repo::messages::mark_read_owned(db, &folder_id, &op_id)?;
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
    conn.subscribe(&folder.remote_path, payload.subscribe).await
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
            let op_id = op.id.clone();
            ctx.db
                .write(move |db| {
                    repo::messages::mark_read_owned(db, &folder_id, &op_id)?;
                    Ok(())
                })
                .await
        }
        KIND_ARCHIVE | KIND_MARK_JUNK => Ok(()),
        _ => Ok(()),
    }
}

pub(crate) async fn store(conn: &mut ImapConnection, set: &str, query: &str) -> Result<()> {
    if set.contains('\r') || set.contains('\n') || query.contains('\r') || query.contains('\n') {
        return Err(AppError::invalid_input(
            "refusing a UID STORE argument with a line break",
        ));
    }
    // `async-imap`'s typed `uid_store` returns a stream that stops at the
    // tagged response without inspecting it, so a `NO`/`BAD` looks like an
    // empty success. `run_command_and_check_ok` reads the completion and turns
    // any rejection (or a dropped connection) into an error.
    conn.raw_command_ok(&format!("UID STORE {set} {query}"))
        .await
}

/// Remove the selected `\Deleted` messages from the server.
///
/// Only `UID EXPUNGE` is used. A plain `EXPUNGE` removes *every* message the
/// mailbox has flagged `\Deleted`, including messages another client flagged
/// but did not intend to remove, so the executor never falls back to it.
///
/// Callers must have established directed-expunge support (UIDPLUS) before
/// doing anything destructive. The guard is repeated here so a future caller
/// cannot accidentally degrade into a whole-folder EXPUNGE.
pub(crate) async fn expire(conn: &mut ImapConnection, set: &str) -> Result<()> {
    if !conn.caps.directed_expunge() {
        return Err(AppError::unsupported("permanent delete requires UIDPLUS"));
    }
    if set.contains('\r') || set.contains('\n') {
        return Err(AppError::invalid_input(
            "refusing a UID EXPUNGE argument with a line break",
        ));
    }
    // See `store`: the typed stream swallows the tagged completion, so a
    // refused EXPUNGE must be read through `run_command_and_check_ok`.
    conn.raw_command_ok(&format!("UID EXPUNGE {set}")).await
}

/// Folder currently selected on this session, together with the UIDVALIDITY
/// the server reported for it. Queued payloads carry UIDs from one generation
/// only, so the executor must compare before sending a UID command.
#[derive(Debug, Clone)]
struct SelectedFolder {
    remote_path: String,
    uidvalidity: Option<i64>,
}

async fn ensure_selected(
    conn: &mut ImapConnection,
    selected: &mut Option<SelectedFolder>,
    remote_path: &str,
) -> Result<Option<i64>> {
    if let Some(current) = selected.as_ref() {
        if current.remote_path == remote_path {
            return Ok(current.uidvalidity);
        }
    }
    // SELECT first deselects whichever mailbox was open, and a failed SELECT
    // leaves none selected (RFC 3501 §6.3.1). Clearing the cache before the
    // attempt keeps it from describing a mailbox the session no longer has:
    // otherwise a later command for the *old* folder would reuse its
    // UIDVALIDITY and skip the SELECT that would have failed.
    *selected = None;
    let mailbox = conn.select(remote_path).await?;
    let uidvalidity = mailbox.uid_validity.map(|value| value as i64);
    *selected = Some(SelectedFolder {
        remote_path: remote_path.to_string(),
        uidvalidity,
    });
    Ok(uidvalidity)
}

/// The folder a move-style operation read its source UIDs from.
fn move_source_folder(op: &PendingOp) -> Option<String> {
    match op.kind.as_str() {
        KIND_MOVE => repo::pending_ops::payload::<MoveOp>(op)
            .ok()
            .map(|payload| payload.folder_id),
        KIND_DELETE => repo::pending_ops::payload::<DeleteOp>(op)
            .ok()
            .map(|payload| payload.folder_id),
        KIND_ARCHIVE => repo::pending_ops::payload::<ArchiveOp>(op)
            .ok()
            .map(|payload| payload.folder_id),
        KIND_MARK_JUNK => repo::pending_ops::payload::<MarkJunkOp>(op)
            .ok()
            .map(|payload| payload.folder_id),
        _ => None,
    }
}

/// UIDs carried by a move-style operation payload.
fn operation_uids(op: &PendingOp) -> Vec<i64> {
    match op.kind.as_str() {
        KIND_MOVE => repo::pending_ops::payload::<MoveOp>(op)
            .map(|payload| payload.uids)
            .unwrap_or_default(),
        KIND_DELETE => repo::pending_ops::payload::<DeleteOp>(op)
            .map(|payload| payload.uids)
            .unwrap_or_default(),
        KIND_ARCHIVE => repo::pending_ops::payload::<ArchiveOp>(op)
            .map(|payload| payload.uids)
            .unwrap_or_default(),
        KIND_MARK_JUNK => repo::pending_ops::payload::<MarkJunkOp>(op)
            .map(|payload| payload.uids)
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// SELECT the operation's source folder and ask the server whether the queued
/// UIDs still exist.
///
/// A rollback must not trust `stored`/the session cache here: the command that
/// just failed may have been refused because the mailbox was rebuilt, and a
/// cached generation would then let the old UID be written back into a folder
/// that no longer addresses that message. `None` means the SELECT or SEARCH
/// failed, which makes the caller leave the rows unaddressed and reconcile
/// instead.
async fn select_live_source_state(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    selected: &mut Option<SelectedFolder>,
    op: &PendingOp,
) -> LiveSourceState {
    let Some(source_id) = move_source_folder(op) else {
        return LiveSourceState::default();
    };
    let Ok(folder) = folder_by_id(ctx, &source_id).await else {
        return LiveSourceState::default();
    };
    // Always issue a real SELECT: a cached entry may describe a mailbox the
    // session no longer has selected, or a generation that has since changed.
    *selected = None;
    let uidvalidity = match conn.select(&folder.remote_path).await {
        Ok(mailbox) => {
            let uidvalidity = mailbox.uid_validity.map(|value| value as i64);
            *selected = Some(SelectedFolder {
                remote_path: folder.remote_path,
                uidvalidity,
            });
            uidvalidity
        }
        Err(error) => {
            debug!(
                %error,
                folder = %folder.remote_path,
                "rollback: SELECT for the live source UIDVALIDITY failed"
            );
            return LiveSourceState::default();
        }
    };
    let uids = operation_uids(op);
    let present_uids = if uids.is_empty() {
        None
    } else {
        search_present_uids(conn, &uids).await
    };
    LiveSourceState {
        uidvalidity,
        present_uids,
        graph: false,
    }
}

/// Which of `uids` a `UID SEARCH` still returns.
///
/// `None` when the mailbox did not give a definitive answer: a tagged
/// `NO`/`BAD`, a lost connection or a timeout are all "unknown", never an
/// empty set. `async-imap`'s typed helper discards the completion status, so
/// this uses the checked command instead.
async fn search_present_uids(conn: &mut ImapConnection, uids: &[i64]) -> Option<HashSet<i64>> {
    let criteria = format!("UID {}", uid_set(uids));
    match conn.uid_search_checked(&criteria).await {
        Ok(found) => Some(found.into_iter().map(|uid| uid as i64).collect()),
        Err(error) => {
            debug!(%error, "UID SEARCH was not confirmed; no UID may be treated as absent");
            None
        }
    }
}

/// Refuse a UID command whose payload was built against another generation.
///
/// `Conflict` is deliberately non-retryable: `handle_failure` then rolls the
/// optimistic local change back and re-synchronizes the folders instead of
/// replaying the same UIDs against a mailbox that no longer addresses the same
/// messages. Missing generations are treated the same way (skip and reconcile)
/// rather than blindly trusting a number from an older payload.
fn verify_uidvalidity(
    current: Option<i64>,
    expected: Option<i64>,
    remote_path: &str,
    operation: &str,
) -> Result<()> {
    let Some(expected) = expected else {
        warn!(
            folder = remote_path,
            operation,
            "queued UID operation has no source UIDVALIDITY; skipping it and reconciling"
        );
        return Err(AppError::conflict(format!(
            "{operation} skipped: the queued operation has no source UIDVALIDITY"
        )));
    };
    if current != Some(expected) {
        warn!(
            folder = remote_path,
            operation,
            expected,
            current = ?current,
            "source UIDVALIDITY changed; skipping UID command and reconciling"
        );
        return Err(AppError::conflict(format!(
            "{operation} skipped: {remote_path} was rebuilt since the operation was queued"
        )));
    }
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

/// Claim the operations that are still `pending`; returns the ids actually
/// claimed. A row cancelled between listing and claiming is left untouched.
async fn claim_ops(ctx: &CoreContext, ids: &[String]) -> Result<Vec<String>> {
    let ids = ids.to_vec();
    ctx.db
        .write(move |db| {
            let mut claimed = Vec::new();
            for id in ids {
                if repo::pending_ops::claim(db, &id)? > 0 {
                    claimed.push(id);
                }
            }
            Ok(claimed)
        })
        .await
}

async fn mark_in_progress(ctx: &CoreContext, ids: &[String]) -> Result<()> {
    claim_ops(ctx, ids).await.map(|_| ())
}

/// Settle claimed operations. Returns how many rows actually moved to `done`;
/// a row cancelled while the worker ran stays cancelled.
///
/// For an APPEND this is the last chance to notice an intent written while the
/// executor was busy. The check and the `done` transition happen in the same
/// write transaction, so a discard that lands after the executor's last read
/// cannot be silently settled: the row stays executable and the next pass
/// performs the cleanup.
async fn finish_ops(ctx: &CoreContext, ids: &[String]) -> Result<usize> {
    let ids = ids.to_vec();
    ctx.db
        .write(move |db| {
            let now = Utc::now();
            let mut changed = 0;
            for id in ids {
                let Some(op) = repo::pending_ops::get(db, &id)? else {
                    continue;
                };
                if op.state != PendingOpState::InProgress {
                    // A remote-draft delete cancelled while this worker was
                    // executing it may still have succeeded. It is already
                    // recorded as `outcomeUnknown`; force the folder to be
                    // re-fetched so the projection converges instead of
                    // trusting the cancellation.
                    if op.state == PendingOpState::Cancelled && op.kind == KIND_DELETE_REMOTE_DRAFT
                    {
                        if let Ok(payload) = repo::pending_ops::payload::<DeleteRemoteDraftOp>(&op)
                        {
                            if payload.outcome_unknown
                                && repo::folders::get(db, &payload.folder_id)?.is_some()
                            {
                                force_full_resync(db, &payload.folder_id)?;
                            }
                        }
                    }
                    continue;
                }
                if op.kind == KIND_APPEND_DRAFT {
                    if let Ok(payload) = repo::pending_ops::payload::<AppendDraftOp>(&op) {
                        if payload.discard_requested && !payload.cleanup_done {
                            // A cleanup is still owed. Keep the operation
                            // executable instead of reporting it done.
                            repo::pending_ops::defer(db, &id, now)?;
                            continue;
                        }
                    }
                }
                changed += repo::pending_ops::finish(db, &id)?;
            }
            Ok(changed)
        })
        .await
}

async fn mark_done(ctx: &CoreContext, ids: &[String]) -> Result<()> {
    finish_ops(ctx, ids).await.map(|_| ())
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
/// Whether the error is worth retrying, using the same attempt budget as
/// [`handle_failure`]. Kept separate so the caller can decide whether it is
/// worth a live SELECT to constrain a rollback.
fn retryable_error(op: &PendingOp, error: &AppError) -> bool {
    // Graph policy denials arrive as `Unsupported` (see
    // `GraphMailClient::failure`), so they fall through to the terminal path:
    // refreshing a valid token cannot grant a permission.
    matches!(
        error,
        AppError::Network(_)
            | AppError::Service(_)
            | AppError::Auth(_)
            | AppError::Db(_)
            | AppError::Io(_)
    ) || matches!(
        error,
        AppError::Protocol(_) if op.attempts < 2
    )
}

fn failure_is_terminal(op: &PendingOp, error: &AppError) -> bool {
    !retryable_error(op, error) || op.attempts + 1 >= MAX_ATTEMPTS
}

#[allow(clippy::too_many_arguments)]
async fn handle_failure(
    ctx: &CoreContext,
    account: &Account,
    op: &PendingOp,
    error: &AppError,
    token: TokenResource,
    graph_client: Option<&GraphMailClient>,
    outcome: &mut ExecOutcome,
    mut live: LiveSourceState,
) -> Result<()> {
    live.graph = crate::graph::uses_graph(account);
    let retryable = retryable_error(op, error);
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
        let graph = live.graph;
        let folders = fail_op(ctx, op, AppError::internal(error.message()), live).await?;
        outcome.resync_folders.extend(folders);
        outcome.failed += 1;
        if graph {
            // The rollback kept the Graph id and synthetic UID. Verify the
            // folder directly instead of waiting for a delta that an unchanged
            // mailbox will never produce. The executing worker's client is
            // reused when available so tests (and proxies) see the same
            // endpoint.
            let default_client;
            let client = match graph_client {
                Some(client) => client,
                None => {
                    default_client = GraphMailClient::new();
                    &default_client
                }
            };
            let recovery = if op.kind == KIND_DELETE {
                let permanent = repo::pending_ops::payload::<DeleteOp>(op)
                    .map(|payload| payload.permanent)
                    .unwrap_or(false);
                if permanent {
                    recover_graph_failed_delete(ctx, account, client, op).await
                } else {
                    recover_graph_failed_move(ctx, account, client, op).await
                }
            } else if matches!(op.kind.as_str(), KIND_MOVE | KIND_ARCHIVE | KIND_MARK_JUNK) {
                recover_graph_failed_move(ctx, account, client, op).await
            } else {
                Ok(())
            };
            if let Err(recovery_error) = recovery {
                debug!(op = %op.id, %recovery_error, "Graph recovery could not verify the message");
            }
        }
    }
    Ok(())
}

/// Verify a permanently-deleted Graph message after a terminal failure.
///
/// `execute_graph_op` sends `DELETE /me/messages/{id}` and may see an error
/// even though Microsoft applied it. A delta pass is not a reliable answer: if
/// nothing else changed, the folder yields no entries and the cursor never
/// revisits the message, so a row whose identity was dropped would be stuck
/// forever. A targeted GET settles it:
///
/// * in the expected folder -> the refusal was real only when the attempt
///   provably never reached the server; an `Unknown` request may still apply,
///   so the row stays hidden and protected;
/// * gone (404) -> the delete did apply; erase the local row (attachments and
///   FTS included);
/// * in another folder -> mirror the move locally and let that folder's sync
///   bind it;
/// * any other error -> keep the row with its identity; a later action or sync
///   can verify it again.
async fn recover_graph_failed_delete(
    ctx: &CoreContext,
    account: &Account,
    client: &GraphMailClient,
    op: &PendingOp,
) -> Result<()> {
    // The caller holds the snapshot claimed before execution, which predates
    // the per-message outcomes and the send intent written during the attempt.
    // Re-read the durable row: the decision below must see the evidence the
    // executor persisted, not the pre-execution view.
    let latest = ctx
        .db
        .read({
            let id = op.id.clone();
            move |db| repo::pending_ops::get(db, &id)
        })
        .await?;
    let op = latest.as_ref().unwrap_or(op);
    let payload: DeleteOp = repo::pending_ops::payload(op)?;
    if !payload.permanent || !payload.pending_delete || payload.message_ids.is_empty() {
        return Ok(());
    }
    let folder = folder_by_id(ctx, &payload.folder_id).await?;
    let expected_folder =
        crate::sync::folders::graph_folder_id(ctx, &account.id, &folder.remote_path).await?;

    for id in &payload.message_ids {
        let stored = ctx
            .db
            .read({
                let id = id.clone();
                move |db| repo::messages::get(db, &id)
            })
            .await?;
        let Some(stored) = stored else {
            continue;
        };
        // The row is filtered by account/folder so a concurrent action that
        // claimed it is left alone.
        if stored.summary.account_id != account.id || stored.summary.folder_id != payload.folder_id
        {
            continue;
        }
        let outcome = payload
            .outcomes
            .iter()
            .find(|outcome| outcome.message_id == *id);
        // The exact id a previous attempt sent outranks anything the mutable
        // projection/mapping says now: a sync may have rebound the row to a
        // different Graph message, and verifying that one would answer a
        // question this operation never asked.
        let persisted_attempt = outcome
            .and_then(|outcome| outcome.attempt_remote_id.clone())
            .filter(|value| !value.trim().is_empty());
        let used_persisted_attempt = persisted_attempt.is_some();
        let remote_id = match persisted_attempt {
            Some(remote_id) => Some(remote_id),
            None => match stored.gm_msgid.clone() {
                Some(remote_id) => Some(remote_id),
                None => match stored.uid {
                    Some(uid) => {
                        graph_remote_id_opt(ctx, &account.id, &payload.folder_id, uid).await?
                    }
                    None => None,
                },
            },
        };
        let Some(remote_id) = remote_id else {
            continue;
        };
        // A 404 (or a "still there" answer) is evidence about this operation
        // only when the id is genuinely attributed to the row. An id this
        // operation actually sent is attributable by definition; a local row
        // id used as a last-resort remote id does not qualify.
        let attributed = used_persisted_attempt
            || graph_delete_id_is_attributed(
                ctx,
                &account.id,
                &payload.folder_id,
                &stored,
                &remote_id,
            )
            .await?;
        match graph_ops::message_parent_folder(client, ctx, &account.id, &remote_id).await? {
            graph_ops::MessageFolderState::InFolder { folder_id, .. }
                if expected_folder.as_deref() == Some(folder_id.as_str()) =>
            {
                if !attributed {
                    debug!(
                        message = %id,
                        %remote_id,
                        "Graph reports the message is still present, but the id is not attributable; keeping the row"
                    );
                    continue;
                }
                // "Still present" only disproves an attempt that provably
                // never reached the server. An unknown request may still be in
                // flight, so releasing the protection on this answer alone
                // could resurrect a message the provider is about to erase.
                let may_have_run = match payload
                    .outcomes
                    .iter()
                    .find(|outcome| outcome.message_id == *id)
                    .map(|outcome| outcome.state)
                {
                    Some(OpMessageState::Unknown) => true,
                    Some(_) => false,
                    None => op.send_intent || op.outcome_unknown,
                };
                if may_have_run {
                    debug!(
                        message = %id,
                        %remote_id,
                        "Graph reports the message is still present, but an earlier request may still apply; keeping it hidden"
                    );
                    continue;
                }
                // The refusal was real: release the row so later syncs can
                // reconcile it normally.
                let account_id = account.id.clone();
                let folder_id = payload.folder_id.clone();
                let ids = vec![id.clone()];
                let owner_op_id = op.id.clone();
                let released = ctx
                    .db
                    .write(move |db| {
                        repo::messages::release_pending_delete_preserving_identity(
                            db,
                            &account_id,
                            &folder_id,
                            &ids,
                            &owner_op_id,
                        )
                    })
                    .await?;
                if released.is_empty() {
                    debug!(
                        message = %id,
                        "Graph refused the delete, but a newer trip owns the row; leaving it to that trip"
                    );
                } else {
                    debug!(message = %id, "Graph refused the delete; message is still in its folder");
                }
            }
            graph_ops::MessageFolderState::InFolder { folder_id, .. } => {
                if !attributed {
                    debug!(
                        message = %id,
                        %remote_id,
                        "Graph reports another folder, but the id is not attributable; leaving the row"
                    );
                    continue;
                }
                // Moved elsewhere despite the delete attempt. Mirror the folder
                // so the row is not shown in the wrong place.
                let target = graph_local_folder(ctx, &account.id, &folder_id).await?;
                if let Some(target) = target {
                    let ids = vec![id.clone()];
                    let target_id = target.id.clone();
                    ctx.db
                        .write(move |db| repo::messages::move_to_folder(db, &ids, &target_id))
                        .await?;
                    force_full_resync_db(ctx, &target.id).await?;
                }
            }
            graph_ops::MessageFolderState::Gone => {
                // A 404 only proves the id is unreachable. The message may have
                // been moved to a new Graph id, so its copy can still exist.
                // Never erase the local row on this answer: keep it hidden with
                // its identity until a sync proves the real state.
                debug!(
                    message = %id,
                    %remote_id,
                    "Graph reports 404; the id is unreachable but not proof of deletion, keeping the row"
                );
            }
            graph_ops::MessageFolderState::Unverified => {
                // A 200 without `parentFolderId` proves nothing. Keep the row
                // hidden and its identity intact for a later verification.
                debug!(
                    message = %id,
                    %remote_id,
                    "Graph verification omitted parentFolderId; keeping the row"
                );
            }
        }
    }
    Ok(())
}

/// Whether a Graph move attempt may still apply on the server.
///
/// A per-message `NotExecuted` is a definitive answer for that attempt, so a
/// later GET can safely release the projection. Anything else — `Unknown`, or
/// no per-message outcome while the operation persisted a send intent — could
/// still land after the GET and must keep the reconciliation protection.
fn graph_move_may_have_run(
    outcome: Option<OpMessageState>,
    send_intent: bool,
    outcome_unknown: bool,
) -> bool {
    match outcome {
        Some(OpMessageState::NotExecuted) => false,
        Some(OpMessageState::Unknown) => true,
        // A `Confirmed` move settles the operation; recovery is not reached.
        // Treat it as "may have run" so a defensive caller never restores.
        Some(OpMessageState::Confirmed) => true,
        None => send_intent || outcome_unknown,
    }
}

/// Verify the folder of a Graph move-style operation after a terminal failure.
///
/// Graph's identity is stable, but the optimistic projection already moved the
/// row into the target and cleared its UID. The IMAP rollback requires a source
/// UID that Graph does not have, so it keeps the projection and records a
/// `moveRollback`. This targeted GET then settles where the message really is:
/// in the source (put the row back), in the target (bind the synthetic UID), or
/// in another folder (mirror it).
async fn recover_graph_failed_move(
    ctx: &CoreContext,
    account: &Account,
    client: &GraphMailClient,
    op: &PendingOp,
) -> Result<()> {
    // `handle_failure` passes the row it claimed before execution. The last
    // attempt may have written a fresh per-message `Unknown` since; recovery
    // must base its protection decision on the durable row, not the stale
    // snapshot. It is re-read again inside the settlement transaction.
    let latest = ctx
        .db
        .read({
            let id = op.id.clone();
            move |db| repo::pending_ops::get(db, &id)
        })
        .await?;
    let op = latest.as_ref().unwrap_or(op);
    let payload = &op.payload;
    let Some(source_folder_id) = payload.get("folderId").and_then(|value| value.as_str()) else {
        return Ok(());
    };
    let message_ids: Vec<String> = payload
        .get("messageIds")
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let uids: Vec<i64> = payload
        .get("uids")
        .and_then(|value| value.as_array())
        .map(|values| values.iter().filter_map(|value| value.as_i64()).collect())
        .unwrap_or_default();
    let expected_target_id = payload
        .get("targetFolderId")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    if message_ids.is_empty() || message_ids.len() != uids.len() {
        return Ok(());
    }
    let source = folder_by_id(ctx, source_folder_id).await?;
    let source_graph =
        crate::sync::folders::graph_folder_id(ctx, &account.id, &source.remote_path).await?;

    for (id, uid) in message_ids.iter().zip(uids.iter()) {
        let stored = ctx
            .db
            .read({
                let id = id.clone();
                move |db| repo::messages::get(db, &id)
            })
            .await?;
        let Some(stored) = stored else {
            continue;
        };
        if stored.summary.account_id != account.id
            || stored.summary.folder_id == source.id
            || stored.uid.is_some()
        {
            // Another action claimed it, it already went back, or a sync has
            // already bound the target identity.
            continue;
        }
        // Verify this row is still the unbound projection this operation
        // left behind. A sync may have bound it, or another rollback may have
        // claimed it, between the failure and this GET.
        let (target_pending, reconcile_owner, move_owner) = ctx
            .db
            .read({
                let id = id.clone();
                move |db| {
                    Ok(db
                        .query_row(
                            "SELECT target_pending,
                                    -- A blank token is not an owner; read it as
                                    -- unowned so it cannot block this recovery.
                                    NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0'),
                                    NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0')
                               FROM message WHERE id = ?1",
                            rusqlite::params![id],
                            |row| {
                                Ok((
                                    row.get::<_, i64>(0)?,
                                    row.get::<_, Option<String>>(1)?,
                                    row.get::<_, Option<String>>(2)?,
                                ))
                            },
                        )
                        .optional()?)
                }
            })
            .await?
            .unwrap_or((0, None, None));
        if target_pending != 1
            || reconcile_owner
                .as_deref()
                .is_some_and(|owner| owner != op.id)
            || move_owner.as_deref().is_some_and(|owner| owner != op.id)
        {
            debug!(
                message = %id,
                "Graph move recovery: the projection no longer belongs to this operation"
            );
            continue;
        }
        let target_folder_id = expected_target_id
            .clone()
            .unwrap_or_else(|| stored.summary.folder_id.clone());
        if stored.summary.folder_id != target_folder_id {
            // A later action moved the row; this operation no longer owns it.
            continue;
        }
        // A previous attempt that may have reached the server is never
        // disproved by a single GET: the timed-out POST can still apply after
        // this read. Only a definitive "not executed" permits releasing the
        // reconciliation protection.
        let may_have_run = graph_move_may_have_run(
            payload_outcome(op, id)?.map(|outcome| outcome.state),
            op.send_intent,
            op.outcome_unknown,
        );
        let remote_id = match stored.gm_msgid.clone() {
            Some(remote_id) => Some(remote_id),
            None => graph_remote_id_opt(ctx, &account.id, &source.id, *uid).await?,
        };
        let Some(remote_id) = remote_id else {
            continue;
        };
        let target_folder = folder_by_id(ctx, &target_folder_id).await?;
        let target_graph =
            crate::sync::folders::graph_folder_id(ctx, &account.id, &target_folder.remote_path)
                .await?;
        let current_folder =
            match graph_ops::message_parent_folder(client, ctx, &account.id, &remote_id).await? {
                graph_ops::MessageFolderState::InFolder { folder_id, .. } => folder_id,
                // Gone (404) or a 200 without `parentFolderId`: neither proves
                // where the message is. Leave the projection marked; only a
                // sync may decide what happened to the copied message.
                graph_ops::MessageFolderState::Gone | graph_ops::MessageFolderState::Unverified => {
                    continue
                }
            };
        if source_graph.as_deref() == Some(current_folder.as_str()) {
            if may_have_run {
                // The MOVE POST may still be in flight (or was committed after
                // the timeout). Restoring the source identity would release
                // the `moveRollback` protection and could later point at a UID
                // the provider consumed. Keep the projection unknown.
                debug!(
                    message = %id,
                    %remote_id,
                    "Graph reports the message in the source, but the MOVE may still apply; keeping it reconciled"
                );
                continue;
            }
            let ids = vec![id.clone()];
            let source_id = source.id.clone();
            let target_id = target_folder_id.clone();
            let account_id = account.id.clone();
            let uid = *uid;
            let remote_id = remote_id.clone();
            let op_id = op.id.clone();
            let op_fallback = op.clone();
            ctx.db
                .write(move |db| {
                    // Settle from the latest durable evidence, inside the same
                    // transaction that may release the moveRollback
                    // protection. The attempt that just failed may have
                    // recorded `Unknown` after the snapshot the caller held.
                    let latest =
                        repo::pending_ops::get(db, &op_id)?.unwrap_or(op_fallback);
                    let may_have_run = graph_move_may_have_run(
                        payload_outcome(&latest, &ids[0])?.map(|outcome| outcome.state),
                        latest.send_intent,
                        latest.outcome_unknown,
                    );
                    if may_have_run {
                        debug!(
                            message = %ids[0],
                            %remote_id,
                            "Graph source check: a late attempt may still apply; keeping the row reconciled"
                        );
                        return Ok(());
                    }
                    // Re-check that this row is still the unbound projection
                    // this exact operation owns. A sync may have bound it (or
                    // another action claimed it) while the GET was in flight.
                    let Some(current) = repo::messages::get(db, &ids[0])? else {
                        return Ok(());
                    };
                    if current.summary.account_id != account_id
                        || current.summary.folder_id != target_id
                        || current.uid.is_some()
                    {
                        return Ok(());
                    }
                    let (pending, owner, move_owner): (i64, Option<String>, Option<String>) = db
                        .query_row(
                            "SELECT target_pending,
                                    -- A blank token is not an owner; read it as
                                    -- unowned so it cannot block this recovery.
                                    NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0'),
                                    NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0')
                               FROM message WHERE id = ?1",
                            rusqlite::params![&ids[0]],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                        )?;
                    if pending != 1
                        || owner.as_deref().is_some_and(|owner| owner != op_id)
                        || move_owner.as_deref().is_some_and(|owner| owner != op_id)
                    {
                        return Ok(());
                    }
                    let restored = repo::messages::move_to_folder_rollback(
                        db,
                        &ids,
                        &source_id,
                        &target_id,
                        &account_id,
                        &op_id,
                    )?;
                    if restored.contains(&ids[0]) {
                        repo::messages::set_graph_identity(db, &ids[0], uid, &remote_id)?;
                    }
                    Ok(())
                })
                .await?;
        } else if target_graph.as_deref() == Some(current_folder.as_str()) {
            // Conditional: a sync may have bound a newer identity while the
            // GET was in flight. Never overwrite it from a stale snapshot.
            let id = id.clone();
            let remote_id = remote_id.clone();
            let uid = *uid;
            let account_id = account.id.clone();
            let folder_id = target_folder_id.clone();
            let op_id = op.id.clone();
            ctx.db
                .write(move |db| {
                    repo::messages::set_graph_identity_if_pending(
                        db,
                        &id,
                        &account_id,
                        &folder_id,
                        uid,
                        &remote_id,
                        &op_id,
                    )?;
                    Ok(())
                })
                .await?;
        } else if let Some(other) = graph_local_folder(ctx, &account.id, &current_folder).await? {
            let ids = vec![id.clone()];
            let other_id = other.id.clone();
            let uid = *uid;
            let remote_id = remote_id.clone();
            let row_id = id.clone();
            let account_id = account.id.clone();
            let branch_id = target_folder_id.clone();
            let op_id = op.id.clone();
            ctx.db
                .write(move |db| {
                    // Re-check inside the write transaction: only the unbound
                    // projection this operation still owns may be mirrored.
                    let Some(current) = repo::messages::get(db, &row_id)? else {
                        return Ok(());
                    };
                    if current.summary.account_id != account_id
                        || current.summary.folder_id != branch_id
                        || current.uid.is_some()
                    {
                        return Ok(());
                    }
                    let (pending, owner, move_owner): (i64, Option<String>, Option<String>) = db
                        .query_row(
                            "SELECT target_pending,
                                    -- A blank token is not an owner; read it as
                                    -- unowned so it cannot block this recovery.
                                    NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0'),
                                    NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0')
                               FROM message WHERE id = ?1",
                            rusqlite::params![row_id],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                        )?;
                    if pending != 1
                        || owner.as_deref().is_some_and(|owner| owner != op_id)
                        || move_owner.as_deref().is_some_and(|owner| owner != op_id)
                    {
                        return Ok(());
                    }
                    repo::messages::move_to_folder(db, &ids, &other_id)?;
                    repo::messages::set_graph_identity(db, &row_id, uid, &remote_id)
                })
                .await?;
            force_full_resync_db(ctx, &other.id).await?;
        }
    }
    Ok(())
}

/// Local folder whose Graph folder id is `remote_id`, if one is tracked.
async fn graph_local_folder(
    ctx: &CoreContext,
    account_id: &str,
    remote_id: &str,
) -> Result<Option<Folder>> {
    let account_id_owned = account_id.to_string();
    let folders = ctx
        .db
        .read(move |db| repo::folders::list(db, &account_id_owned))
        .await?;
    for folder in folders {
        if crate::sync::folders::graph_folder_id(ctx, account_id, &folder.remote_path)
            .await?
            .as_deref()
            == Some(remote_id)
        {
            return Ok(Some(folder));
        }
    }
    Ok(None)
}

/// Queue a full re-sync for a folder from an async context.
async fn force_full_resync_db(ctx: &CoreContext, folder_id: &str) -> Result<()> {
    let folder_id = folder_id.to_string();
    ctx.db
        .write(move |db| force_full_resync(db, &folder_id))
        .await
}

/// Mark an operation failed permanently, roll the optimistic local change back
/// and report which folders must be re-synchronized. The server is the source
/// of truth: if the inverse operation cannot fully restore local state, a
/// forced folder re-sync repairs it.
async fn fail_op(
    ctx: &CoreContext,
    op: &PendingOp,
    error: AppError,
    live: LiveSourceState,
) -> Result<Vec<String>> {
    let op_id = op.id.clone();
    let snapshot = op.clone();
    let message = error.message();
    let message_for_db = message.clone();
    // Re-read the operation inside the write transaction: mid-flight attempts
    // persist evidence (`deleted_flag_set`, per-message outcomes) that the
    // snapshot read before execution does not contain. Rolling back from the
    // stale snapshot could unhide a message the server already expunged.
    let folders = ctx
        .db
        .write(move |db| {
            let latest = repo::pending_ops::get(db, &op_id)?.unwrap_or(snapshot);
            let folders = match rollback_in_db(db, &latest, &live) {
                Ok(folders) => folders,
                Err(rollback_error) => {
                    tracing::warn!(op = %latest.id, %rollback_error, "local rollback failed");
                    op_folders(&latest)
                }
            };
            repo::pending_ops::mark_failed(db, &op_id, &message_for_db)?;
            Ok(folders)
        })
        .await?;
    for folder in &folders {
        events::emit_mail_changed(
            ctx.events.as_ref(),
            op.account_id.clone(),
            vec![folder.clone()],
            Vec::new(),
            events::MailChangeKind::Updated,
        );
    }
    events::emit_op_failed(
        ctx.events.as_ref(),
        op.account_id.clone(),
        &op.description,
        &message,
    );
    info!(op = %op.id, kind = %op.kind, error = %message, "operation failed permanently and was rolled back");
    Ok(folders)
}

/// Roll back a permanent delete using its persisted per-message outcomes.
///
/// * `Confirmed` – the server deleted this exact message: erase the hidden row.
/// * `NotExecuted` – the command provably never ran: unhide the row.
/// * `Unknown` (or no outcome) – the row stays hidden until a sync proves the
///   server state; restoring it could resurrect a deleted message or reuse a
///   UID the server already reassigned.
fn rollback_per_message_delete(
    db: &rusqlite::Connection,
    op: &PendingOp,
    payload: &DeleteOp,
    live: &LiveSourceState,
) -> Result<Vec<String>> {
    let mut confirmed = Vec::new();
    let mut not_executed = Vec::new();
    let mut not_executed_uids = Vec::new();
    let mut unknown = 0usize;

    for (index, message_id) in payload.message_ids.iter().enumerate() {
        match payload
            .outcomes
            .iter()
            .find(|outcome| outcome.message_id == *message_id)
            .map(|outcome| outcome.state)
        {
            Some(OpMessageState::Confirmed) => confirmed.push(message_id.clone()),
            Some(OpMessageState::NotExecuted) => {
                not_executed.push(message_id.clone());
                not_executed_uids.push(payload.uids.get(index).copied());
            }
            Some(OpMessageState::Unknown) | None => unknown += 1,
        }
    }
    // Outcomes may name rows the payload's id list does not (defensive): any
    // unmatched confirmed message is still erased by its recorded id.
    for outcome in &payload.outcomes {
        if outcome.state == OpMessageState::Confirmed
            && !confirmed.iter().any(|id| id == &outcome.message_id)
        {
            confirmed.push(outcome.message_id.clone());
        }
    }

    if !confirmed.is_empty() && payload.pending_delete {
        repo::messages::delete_local(db, &confirmed, true)?;
    }
    if !not_executed.is_empty() {
        if live.graph {
            repo::messages::release_pending_delete_preserving_identity(
                db,
                &op.account_id,
                &payload.folder_id,
                &not_executed,
                &op.id,
            )?;
        } else {
            let trusted = trusted_delete_ids(payload, live);
            let uids: Vec<i64> = not_executed_uids
                .into_iter()
                .map(|uid| uid.unwrap_or_default())
                .collect();
            repo::messages::release_pending_delete_with_trust(
                db,
                &op.account_id,
                &payload.folder_id,
                &not_executed,
                &trusted,
                &uids,
                payload.source_uidvalidity,
                &op.id,
            )?;
        }
    }
    if unknown > 0 || !confirmed.is_empty() {
        warn!(
            folder = %payload.folder_id,
            confirmed = confirmed.len(),
            not_executed = not_executed.len(),
            unknown,
            "permanent-delete rollback reconciled per message; forcing a folder resync"
        );
        force_full_resync(db, &payload.folder_id)?;
    }
    repo::folders::recompute_counts(db, &payload.folder_id)?;
    Ok(vec![payload.folder_id.clone()])
}

/// Local inverse of a queueable operation. Uses the state captured in the
/// payload; payloads queued by older versions (or background paths) simply
/// have nothing to roll back and are repaired by the follow-up re-sync.
fn rollback_in_db(
    db: &rusqlite::Connection,
    op: &PendingOp,
    live: &LiveSourceState,
) -> Result<Vec<String>> {
    let mut folders: Vec<String> = Vec::new();
    match op.kind.as_str() {
        KIND_SET_FLAGS => {
            let payload: SetFlagsOp = repo::pending_ops::payload(op)?;
            // `previous` addresses rows by UID, so it is only trustworthy in
            // the generation the op was queued in. A missing or changed
            // generation means the number may point at another message:
            // reconcile instead of restoring flags onto the wrong row.
            let generation_matches = match payload.source_uidvalidity {
                Some(expected) => {
                    repo::folders::uidvalidity(db, &payload.folder_id)? == Some(expected)
                }
                None => false,
            };
            if generation_matches {
                for previous in &payload.previous {
                    if let Some(stored) =
                        repo::messages::by_uid(db, &payload.folder_id, previous.uid)?
                    {
                        if stored.summary.account_id != op.account_id {
                            continue;
                        }
                        // A rollback owns only the fields this operation
                        // changed, and only while this operation is still the
                        // durable last writer of that field. Writing all four
                        // snapshot flags clobbered a later action; comparing
                        // values alone is not ownership either, because a
                        // later action can write the same value again.
                        let owned = |field: &str,
                                     written: Option<bool>,
                                     current: bool|
                         -> Result<Option<bool>> {
                            let only = |name: &str| {
                                if field == name {
                                    written
                                } else {
                                    None
                                }
                            };
                            if written.is_none()
                                || !flag_write_owned(
                                    db,
                                    &op.id,
                                    &stored.summary.id,
                                    only("seen"),
                                    only("flagged"),
                                    only("answered"),
                                    only("forwarded"),
                                    false,
                                )?
                            {
                                return Ok(None);
                            }
                            Ok(written.filter(|value| current == *value))
                        };
                        let seen = owned("seen", payload.seen, stored.summary.seen)?
                            .map(|_| previous.seen);
                        let flagged = owned("flagged", payload.flagged, stored.summary.flagged)?
                            .map(|_| previous.flagged);
                        let answered =
                            owned("answered", payload.answered, stored.summary.answered)?
                                .map(|_| previous.answered);
                        let forwarded =
                            owned("forwarded", payload.forwarded, stored.summary.forwarded)?
                                .map(|_| previous.forwarded);
                        if seen.is_none()
                            && flagged.is_none()
                            && answered.is_none()
                            && forwarded.is_none()
                        {
                            continue;
                        }
                        repo::messages::set_flags(
                            db,
                            std::slice::from_ref(&stored.summary.id),
                            seen,
                            flagged,
                            answered,
                            forwarded,
                        )?;
                    }
                }
            } else {
                warn!(
                    folder = %payload.folder_id,
                    "rollback: setFlags source UIDVALIDITY changed or missing; re-syncing"
                );
                force_full_resync(db, &payload.folder_id)?;
            }
            folders.push(payload.folder_id);
        }
        KIND_MOVE => {
            let payload: MoveOp = repo::pending_ops::payload(op)?;
            rollback_moved_rows(
                db,
                &op.account_id,
                &payload.folder_id,
                &payload.target_folder_id,
                &payload.message_ids,
                &payload.uids,
                payload.source_uidvalidity,
                live,
                &op.id,
                op.sequence,
                &payload.outcomes,
            )?;
            folders.push(payload.folder_id);
            folders.push(payload.target_folder_id);
        }
        KIND_DELETE => {
            let payload: DeleteOp = repo::pending_ops::payload(op)?;
            if payload.permanent {
                // A batch that was persisted per message settles per message.
                // The aggregate `failed` state must not restore a message the
                // server already deleted, nor delete one that never ran.
                if !payload.outcomes.is_empty() {
                    return rollback_per_message_delete(db, op, &payload, live);
                }
                if payload.deleted_flag_set || op.outcome_unknown || op.send_intent {
                    // `+FLAGS \Deleted` already succeeded; only the EXPUNGE
                    // failed or is unknown. The message may be gone, so keep it
                    // hidden and drop its UID identity instead of restoring a
                    // number the server may have reused. A full re-sync decides
                    // whether the row survives.
                    warn!(
                        folder = %payload.folder_id,
                        "permanent-delete rollback: STORE already reached the server; \
                         keeping the rows hidden until reconciliation"
                    );
                    force_full_resync(db, &payload.folder_id)?;
                    folders.push(payload.folder_id);
                    return Ok(folders);
                }
                if payload.pending_delete {
                    // Unhide the row. Only a UID that the live folder still
                    // reports under the queued generation is trusted again;
                    // everything else records *why* it must be reconciled
                    // (`pendingDelete`, this op) and keeps the pre-operation
                    // number separately for the precise UID check.
                    //
                    // Graph is the exception: its message id is stable and the
                    // folder can be verified with a targeted GET, so dropping
                    // the (synthetic) identity would leave the row unusable
                    // until a delta that may never arrive.
                    let restored = if live.graph {
                        repo::messages::release_pending_delete_preserving_identity(
                            db,
                            &op.account_id,
                            &payload.folder_id,
                            &payload.message_ids,
                            &op.id,
                        )?
                    } else {
                        let trusted = trusted_delete_ids(&payload, live);
                        repo::messages::release_pending_delete_with_trust(
                            db,
                            &op.account_id,
                            &payload.folder_id,
                            &payload.message_ids,
                            &trusted,
                            &payload.uids,
                            payload.source_uidvalidity,
                            &op.id,
                        )?
                    };
                    if restored.len() != payload.message_ids.len() {
                        warn!(
                            folder = %payload.folder_id,
                            expected = payload.message_ids.len(),
                            restored = restored.len(),
                            "permanent-delete rollback lost a row; rebuilding it from the snapshot"
                        );
                        restore_deleted_snapshot(
                            db,
                            &op.account_id,
                            &payload.folder_id,
                            &payload.snapshot,
                            true,
                            &op.id,
                            op.sequence,
                        )?;
                    }
                } else {
                    // Payload from an older build: the row was already removed,
                    // so rebuild it from the snapshot captured at queue time
                    // (a full re-sync only covers the account's sync window).
                    let restored = restore_deleted_snapshot(
                        db,
                        &op.account_id,
                        &payload.folder_id,
                        &payload.snapshot,
                        false,
                        &op.id,
                        op.sequence,
                    )?;
                    if restored == 0 && payload.snapshot.is_empty() {
                        warn!(
                            folder = %payload.folder_id,
                            "permanent-delete rollback has no snapshot; relying on a window-limited re-sync"
                        );
                    }
                }
                force_full_resync(db, &payload.folder_id)?;
            } else if let Some(target) = payload.target_folder_id.as_deref() {
                rollback_moved_rows(
                    db,
                    &op.account_id,
                    &payload.folder_id,
                    target,
                    &payload.message_ids,
                    &payload.uids,
                    payload.source_uidvalidity,
                    live,
                    &op.id,
                    op.sequence,
                    &payload.outcomes,
                )?;
                folders.push(target.to_string());
            } else if payload.soft_hidden {
                // Rows hidden in place. Explicit ids are authoritative and
                // must still belong to this account and folder; a payload
                // queued before ids were recorded falls back to the UID only
                // when the folder still has the generation it was queued in.
                let ids = match rollback_ids(
                    db,
                    &op.account_id,
                    &payload.message_ids,
                    &payload.folder_id,
                )? {
                    Some(ids) => ids,
                    None => {
                        let generation_matches = match payload.source_uidvalidity {
                            Some(expected) => {
                                repo::folders::uidvalidity(db, &payload.folder_id)?
                                    == Some(expected)
                            }
                            None => false,
                        };
                        if !generation_matches {
                            warn!(
                                folder = %payload.folder_id,
                                "soft-delete rollback has no ids and no matching UIDVALIDITY; re-syncing"
                            );
                            force_full_resync(db, &payload.folder_id)?;
                        }
                        let mut ids = Vec::new();
                        if generation_matches {
                            for uid in &payload.uids {
                                if let Some(stored) =
                                    repo::messages::by_uid(db, &payload.folder_id, *uid)?
                                {
                                    if stored.summary.account_id == op.account_id {
                                        ids.push(stored.summary.id);
                                    }
                                }
                            }
                        }
                        ids
                    }
                };
                for id in ids {
                    repo::messages::set_deleted_local(db, &id, false)?;
                }
            }
            folders.push(payload.folder_id);
        }
        KIND_ARCHIVE => {
            let payload: ArchiveOp = repo::pending_ops::payload(op)?;
            if payload.soft_hidden {
                // There was no archive target, so the row was hidden in place
                // instead of moved. Unhide it by local id; a move rollback
                // would look in a target folder that never existed.
                let ids = match rollback_ids(
                    db,
                    &op.account_id,
                    &payload.message_ids,
                    &payload.folder_id,
                )? {
                    Some(ids) => ids,
                    None => {
                        // Payload queued before ids were recorded: only trust
                        // the UID when the live SELECT still reports the queued
                        // generation.
                        let generation_matches =
                            match (payload.source_uidvalidity, live.uidvalidity) {
                                (Some(expected), Some(live)) => expected == live,
                                _ => false,
                            };
                        if !generation_matches {
                            warn!(
                                folder = %payload.folder_id,
                                "archive rollback has no ids and no live matching UIDVALIDITY; re-syncing"
                            );
                            force_full_resync(db, &payload.folder_id)?;
                        }
                        let mut ids = Vec::new();
                        if generation_matches {
                            for uid in &payload.uids {
                                if let Some(stored) =
                                    repo::messages::by_uid(db, &payload.folder_id, *uid)?
                                {
                                    if stored.summary.account_id == op.account_id {
                                        ids.push(stored.summary.id);
                                    }
                                }
                            }
                        }
                        ids
                    }
                };
                for id in ids {
                    repo::messages::set_deleted_local(db, &id, false)?;
                }
            } else {
                // General IMAP only archives to a real `\Archive`; `\All` is a
                // virtual view and may be read-only. A Gmail projection carries
                // the account's All Mail id explicitly.
                let target = match payload.target_folder_id.clone() {
                    Some(target) => Some(target),
                    None => role_folder_in_db(db, &op.account_id, FolderRole::Archive)?
                        .map(|folder| folder.id),
                };
                if let Some(target) = target {
                    rollback_moved_rows(
                        db,
                        &op.account_id,
                        &payload.folder_id,
                        &target,
                        &payload.message_ids,
                        &payload.uids,
                        payload.source_uidvalidity,
                        live,
                        &op.id,
                        op.sequence,
                        &payload.outcomes,
                    )?;
                    folders.push(target);
                } else {
                    force_full_resync(db, &payload.folder_id)?;
                }
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
                rollback_moved_rows(
                    db,
                    &op.account_id,
                    &payload.folder_id,
                    &target,
                    &payload.message_ids,
                    &payload.uids,
                    payload.source_uidvalidity,
                    live,
                    &op.id,
                    op.sequence,
                    &payload.outcomes,
                )?;
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

/// Move the rows of a move-style operation back and restore their source
/// identity.
///
/// A sync may have bound the target UID to the row before the operation
/// failed. The restore therefore goes through
/// [`repo::messages::move_to_folder_rollback`], which clears that target
/// identity atomically instead of carrying it into the source folder (where it
/// can collide with `idx_message_unique_uid` or address a different message).
/// The source UID (and generation) is only written back when the source folder
/// still has the queued generation; otherwise the row stays marked
/// `target_pending` for the next sync, and both folders are reconciled.
#[allow(clippy::too_many_arguments)]
fn rollback_moved_rows(
    db: &rusqlite::Connection,
    account_id: &str,
    source_folder_id: &str,
    target_folder_id: &str,
    message_ids: &[String],
    uids: &[i64],
    source_uidvalidity: Option<i64>,
    live: &LiveSourceState,
    op_id: &str,
    op_sequence: i64,
    outcomes: &[OpMessageOutcome],
) -> Result<bool> {
    if message_ids.is_empty() {
        // Payload from an older build: guessing the rows by UID could hit a
        // different message that happens to share the number.
        reconcile_rollback(db, source_folder_id, target_folder_id)?;
        return Ok(false);
    }

    // The operation itself may have landed even though the client saw a
    // failure. A move is only reverted when both ends agree: the source UID is
    // positively present in the queued generation *and* the row is still this
    // operation's unbound target projection. Anything else stays where a sync
    // can reconcile it per operation.
    let generation_matches = source_uidvalidity.is_some() && live.uidvalidity == source_uidvalidity;
    let source_uid_state = |uid: i64| -> Option<bool> {
        if !generation_matches {
            return None;
        }
        live.present_uids
            .as_ref()
            .map(|present| present.contains(&uid))
    };

    let mut restore_ids: Vec<String> = Vec::new();
    let mut restore_uids: Vec<i64> = Vec::new();
    let mut unresolved: Vec<(String, Option<i64>)> = Vec::new();
    let mut broken = false;
    let mut unresolved_count = 0usize;

    for (index, id) in message_ids.iter().enumerate() {
        let uid = uids.get(index).copied();
        let Some(stored) = repo::messages::get(db, id)? else {
            broken = true;
            continue;
        };
        // A per-message receipt is final. Another message's failure in the
        // same batch must not roll this confirmed move back or mark it for
        // reconciliation: the server already reported it as moved.
        if confirmed_outcome(outcomes, id).is_some() {
            continue;
        }
        // The identity slot may have been claimed by a newer move trip, or the
        // row may be owned by a newer reconciliation trip whose move slot has
        // already been cleared. Both are the current owner; an older batch's
        // failure must not clear it (mark_reconcile would) or move the row
        // back.
        if matches!(
            repo::messages::projection_owner(db, id)?,
            Some(owner) if owner.op_id != op_id
        ) {
            continue;
        }
        if stored.summary.account_id != account_id || stored.summary.folder_id != target_folder_id {
            // Another action already claimed the row: this operation no longer
            // owns its projection.
            broken = true;
            continue;
        }
        // A target sync may already have bound a target UID. That is target-
        // side evidence the remote move happened, so the row stays put.
        if stored.uid.is_some() || !stored_target_pending(db, id)? {
            continue;
        }
        let Some(uid) = uid else {
            unresolved.push((id.clone(), None));
            continue;
        };
        match source_uid_state(uid) {
            Some(true) => {
                // Do not overwrite a (folder, UID) another local row owns. Per
                // RFC 9051 §2.3.1.1 this cannot be a legitimate reuse, so the
                // local state is inconsistent and the row must be reconciled.
                match repo::messages::by_uid(db, source_folder_id, uid)? {
                    Some(owner) if owner.summary.id != *id => {
                        unresolved.push((id.clone(), Some(uid)));
                    }
                    _ => {
                        restore_ids.push(id.clone());
                        restore_uids.push(uid);
                    }
                }
            }
            Some(false) => {
                // The source no longer has the number in the same generation:
                // the remote move almost certainly reached the server. Leave
                // the projection in the target for its sync to bind.
                unresolved.push((id.clone(), Some(uid)));
            }
            None => {
                // The server could not be asked (SELECT/SEARCH failed) or the
                // generation changed. Do not guess.
                unresolved.push((id.clone(), Some(uid)));
            }
        }
    }

    if !restore_ids.is_empty() {
        let restored = repo::messages::move_to_folder_rollback(
            db,
            &restore_ids,
            source_folder_id,
            target_folder_id,
            account_id,
            op_id,
        )?;
        for id in &restored {
            let index = restore_ids
                .iter()
                .position(|candidate| candidate == id)
                .expect("restored id came from this batch");
            repo::messages::set_uid_identity(
                db,
                id,
                Some(restore_uids[index]),
                source_uidvalidity,
            )?;
        }
        if restored.len() != restore_ids.len() {
            broken = true;
        }
    }

    // Every row that could not be proven to be back in the source keeps its
    // projection and stays owned by this operation, with the reason recorded so
    // the next sync asks the right question.
    unresolved_count += unresolved.len();
    for (id, uid) in unresolved {
        repo::messages::mark_reconcile_owned(
            db,
            std::slice::from_ref(&id),
            repo::messages::RECONCILE_MOVE_ROLLBACK,
            op_id,
            op_sequence,
            uid,
            source_uidvalidity,
        )?;
        repo::folders::recompute_counts(db, target_folder_id)?;
    }

    if broken {
        // Rows changed under the operation (or a legacy payload had no ids):
        // a targeted re-sync is the only reliable repair.
        converge_failed_projection(
            db,
            account_id,
            source_folder_id,
            target_folder_id,
            message_ids,
            uids,
            source_uidvalidity,
            live,
            op_id,
            op_sequence,
            outcomes,
        )?;
        reconcile_rollback(db, source_folder_id, target_folder_id)?;
        return Ok(false);
    }
    Ok(unresolved_count == 0)
}

/// Whether the row is still waiting for a target sync to bind its UID.
fn stored_target_pending(db: &rusqlite::Connection, id: &str) -> Result<bool> {
    let pending: Option<i64> = db
        .query_row(
            "SELECT target_pending FROM message WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(pending == Some(1))
}

/// Move the rows that are still this operation's *unbound* target projection
/// back to their source folder when the server confirms the source UID was not
/// consumed by the failed command.
///
/// This is the targeted repair for a batch whose pre-check refuses the
/// all-or-nothing restore: without it those rows stay in the target folder with
/// `uid IS NULL`, where no sync pass will ever prune or rebind them when the
/// remote command did not move anything.
///
/// Only rows that are still `target_pending` with no UID (that is, no sync has
/// already bound them and no later action has moved them) are considered. A
/// row whose source UID is absent was moved (or deleted) remotely; it is left
/// for the target folder's sync to bind by Message-ID. `present_uids = None`
/// means the server could not be asked, so nothing is guessed.
#[allow(clippy::too_many_arguments)]
fn converge_failed_projection(
    db: &rusqlite::Connection,
    account_id: &str,
    source_folder_id: &str,
    target_folder_id: &str,
    message_ids: &[String],
    uids: &[i64],
    source_uidvalidity: Option<i64>,
    live: &LiveSourceState,
    op_id: &str,
    op_sequence: i64,
    outcomes: &[OpMessageOutcome],
) -> Result<()> {
    if message_ids.len() != uids.len() {
        return Ok(());
    }
    // `present_uids` was collected with the *live* source generation. It only
    // means anything when that generation is the one the op was queued in.
    let generation_matches = source_uidvalidity.is_some() && live.uidvalidity == source_uidvalidity;
    let present = live.present_uids.as_ref().filter(|_| generation_matches);
    let mut eligible: Vec<(String, i64)> = Vec::new();
    let mut unresolved: Vec<(String, i64)> = Vec::new();
    for (id, uid) in message_ids.iter().zip(uids.iter()) {
        let Some(stored) = repo::messages::get(db, id)? else {
            continue;
        };
        // A confirmed receipt is final; a sibling's failure must not turn it
        // into a reconciliation row.
        if confirmed_outcome(outcomes, id).is_some() {
            continue;
        }
        // A newer move trip's destination, or a newer reconciliation trip whose
        // move slot has already been cleared, must not be cleared by this
        // operation's failure recovery.
        if matches!(
            repo::messages::projection_owner(db, id)?,
            Some(owner) if owner.op_id != op_id
        ) {
            continue;
        }
        if stored.summary.account_id != account_id
            || stored.summary.folder_id != target_folder_id
            || stored.uid.is_some()
        {
            // A rebound row, or one another action already claimed, is not
            // this operation's projection any more.
            continue;
        }
        let still_pending: Option<i64> = db
            .query_row(
                "SELECT target_pending FROM message WHERE id = ?1",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .optional()?;
        if still_pending != Some(1) {
            continue;
        }
        match present {
            // Both ends agree: absent from the source means the remote move
            // probably landed, so the projection stays in the target.
            Some(present) if !present.contains(uid) => unresolved.push((id.clone(), *uid)),
            Some(_) => eligible.push((id.clone(), *uid)),
            // No trustworthy server answer: keep the projection and record the
            // owning op so the next sync asks again.
            None => unresolved.push((id.clone(), *uid)),
        }
    }
    if !eligible.is_empty() {
        let ids: Vec<String> = eligible.iter().map(|(id, _)| id.clone()).collect();
        let restored = repo::messages::move_to_folder_rollback(
            db,
            &ids,
            source_folder_id,
            target_folder_id,
            account_id,
            op_id,
        )?;
        for (id, uid) in eligible {
            if !restored.contains(&id) {
                continue;
            }
            match repo::messages::by_uid(db, source_folder_id, uid)? {
                None => {
                    repo::messages::set_uid_identity(db, &id, Some(uid), source_uidvalidity)?;
                }
                Some(owner) if owner.summary.id == id => {
                    repo::messages::confirm_pending_identity(db, &id)?;
                }
                Some(_) => unresolved.push((id, uid)),
            }
        }
    }
    for (id, uid) in unresolved {
        repo::messages::mark_reconcile_owned(
            db,
            std::slice::from_ref(&id),
            repo::messages::RECONCILE_MOVE_ROLLBACK,
            op_id,
            op_sequence,
            Some(uid),
            source_uidvalidity,
        )?;
    }
    Ok(())
}

/// Ids of a permanent-delete payload whose folder-local UID is confirmed by the
/// live server state: the queued generation is still current and `UID SEARCH`
/// returned the number.
fn trusted_delete_ids(payload: &DeleteOp, live: &LiveSourceState) -> HashSet<String> {
    let mut trusted = HashSet::new();
    if payload.message_ids.len() != payload.uids.len() {
        return trusted;
    }
    let generation_matches =
        payload.source_uidvalidity.is_some() && live.uidvalidity == payload.source_uidvalidity;
    if !generation_matches {
        return trusted;
    }
    let Some(present) = live.present_uids.as_ref() else {
        return trusted;
    };
    for (id, uid) in payload.message_ids.iter().zip(payload.uids.iter()) {
        if present.contains(uid) {
            trusted.insert(id.clone());
        }
    }
    trusted
}

/// Rebuild rows removed by an optimistic permanent delete from the snapshot
/// carried by the payload.
///
/// Trade-off (see [`DeleteOp::snapshot`]): the snapshot exists only on payloads
/// queued by this version; older payloads still fall back to a window-limited
/// folder re-sync. Rebuilding is preferred because the row's metadata is
/// already local and the message may be older than the sync window, so a
/// re-sync alone would never bring it back.
///
/// Rows that a sync has already restored are left alone. The queued UID is
/// only written back when the live folder still has the same generation and no
/// other local row owns it. Otherwise the row drops its identity and waits for
/// the targeted reconciliation: the command may have executed, so this build
/// cannot know which server identity (if any) the row now has. The follow-up
/// sync re-associates it through its Message-ID header.
fn restore_deleted_snapshot(
    db: &rusqlite::Connection,
    account_id: &str,
    folder_id: &str,
    snapshot: &[crate::service::messages::StoredSnapshot],
    drop_identity: bool,
    op_id: &str,
    op_sequence: i64,
) -> Result<usize> {
    if snapshot.is_empty() {
        return Ok(0);
    }
    if repo::folders::get(db, folder_id)?.is_none() {
        // The folder itself is gone; re-inserting its messages would violate
        // the foreign key, and there is nothing local to repair.
        tracing::warn!(
            folder = folder_id,
            "permanent-delete rollback snapshot has no folder to restore into; skipping"
        );
        return Ok(0);
    }
    let generation = repo::folders::uidvalidity(db, folder_id)?;
    let mut restored = 0;
    for entry in snapshot {
        let mut stored = entry.clone().into_stored();
        if stored.summary.account_id != account_id || stored.summary.folder_id != folder_id {
            tracing::warn!(
                message = %stored.summary.id,
                "permanent-delete rollback snapshot does not belong to this operation; skipping"
            );
            continue;
        }
        // Re-read the row and its effective owner inside this write transaction
        // before touching anything. The snapshot was captured when the delete
        // was queued; a sync, the user or a newer trip may have re-created the
        // id in the meantime. Such a row owns its identity and markers, so the
        // stale snapshot must neither upsert over it nor take its ownership.
        if repo::messages::get(db, &stored.summary.id)?.is_some() {
            continue;
        }
        if repo::messages::projection_owner(db, &stored.summary.id)?.is_some() {
            continue;
        }
        let queued_uid = stored.uid;
        let queued_validity = stored.uidvalidity;
        let uid_usable = !drop_identity
            && match queued_uid {
                Some(uid) if queued_validity == generation => {
                    repo::messages::by_uid(db, folder_id, uid)?.is_none()
                }
                _ => false,
            };
        if !uid_usable {
            stored.uid = None;
            stored.uidvalidity = None;
            stored.gm_msgid = None;
        }
        stored.is_deleted_local = false;
        repo::messages::upsert(db, &stored)?;
        if !uid_usable {
            // A timed-out command may have reached the server: do not let any
            // later UID action address the old number until the exact queued
            // UID has been checked against the live generation. The owner-checked
            // variant is used so a row a newer trip claimed in the meantime keeps
            // its own markers.
            repo::messages::mark_reconcile_owned(
                db,
                &[stored.summary.id.clone()],
                repo::messages::RECONCILE_PENDING_DELETE,
                op_id,
                op_sequence,
                queued_uid,
                queued_validity,
            )?;
        }
        restored += 1;
    }
    Ok(restored)
}

/// Rows a rollback has to restore in place (soft delete). The optimistic
/// change hides the row without touching its UID, so the local ids captured at
/// queue time are the only reliable handles. A payload without ids (queued by
/// an older version) returns `None`: guessing by UID could hit a different
/// message that happens to share the number, so the caller reconciles the
/// folder instead.
fn rollback_ids(
    db: &rusqlite::Connection,
    account_id: &str,
    message_ids: &[String],
    expected_folder_id: &str,
) -> Result<Option<Vec<String>>> {
    if message_ids.is_empty() {
        return Ok(None);
    }
    let mut ids = Vec::new();
    for id in message_ids {
        if let Some(stored) = repo::messages::get(db, id)? {
            if stored.summary.account_id == account_id
                && stored.summary.folder_id == expected_folder_id
            {
                ids.push(id.clone());
            }
        }
    }
    Ok(Some(ids))
}

/// No reliable row identity for the rollback: drop the local shortcut and let
/// a full re-sync of both folders repair the projection.
fn reconcile_rollback(
    db: &rusqlite::Connection,
    folder_id: &str,
    target_folder_id: &str,
) -> Result<()> {
    tracing::warn!(
        folder = folder_id,
        target = target_folder_id,
        "rollback without local ids; re-syncing both folders"
    );
    force_full_resync(db, folder_id)?;
    force_full_resync(db, target_folder_id)?;
    Ok(())
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
///
/// Graph has no UIDNEXT: its incremental evidence is the folder's delta
/// cursor. That cursor is cleared too, so a Graph folder whose remote outcome
/// is unknown is actually re-scanned in full instead of relying on a delta
/// that may never mention the in-flight change.
///
/// Also used when a draft-delete was cancelled while in flight: its outcome is
/// unknown, so the folder has to be reconciled instead of assuming the delete
/// did or did not happen.
pub(crate) fn force_full_resync(db: &rusqlite::Connection, folder_id: &str) -> Result<()> {
    let (uidvalidity, _, _) = repo::folders::sync_flags(db, folder_id)?;
    repo::folders::set_sync_flags(db, folder_id, uidvalidity, None, None)?;
    if let Some(folder) = repo::folders::get(db, folder_id)? {
        repo::sync_state::delete_key(
            db,
            &folder.account_id,
            "mail",
            &crate::graph::messages::delta_cursor_key(folder_id),
        )?;
    }
    debug!(
        folder = folder_id,
        "queued a full folder re-sync after rollback or an unknown remote outcome"
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
            let live = select_live_source_state(ctx, conn, &mut selected, op).await;
            let folders = fail_op(ctx, op, AppError::internal(error.message()), live).await?;
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

    fn live_state(uidvalidity: Option<i64>) -> LiveSourceState {
        LiveSourceState {
            uidvalidity,
            present_uids: None,
            graph: false,
        }
    }

    /// Server state that positively reports these UIDs in the source folder.
    fn live_state_with(
        uidvalidity: Option<i64>,
        uids: impl IntoIterator<Item = i64>,
    ) -> LiveSourceState {
        LiveSourceState {
            uidvalidity,
            present_uids: Some(uids.into_iter().collect()),
            graph: false,
        }
    }

    fn pending(kind: &str, payload: serde_json::Value) -> PendingOp {
        PendingOp {
            id: "op".into(),
            account_id: "acc".into(),
            kind: kind.to_string(),
            payload,
            state: PendingOpState::InProgress,
            sequence: 1,
            attempts: 8,
            next_retry_at: None,
            created_at: Utc::now(),
            description: "test".into(),
            last_error: None,
            send_intent: false,
            outcome_unknown: false,
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
            repo::sync_state::set(conn, "acc", "mail", "graph_remote:src:5", "old-id")?;
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

    /// Two folders can hold the same UID. An optimistic move used to keep the
    /// source UID, which both violated `idx_message_unique_uid` and left the
    /// row addressable as "the message with UID n" inside the target folder,
    /// where that UID belongs to a different mail.
    #[test]
    fn move_clears_source_uid_and_rolls_back_by_id() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::insert(conn, &folder("dst", "Archive", FolderRole::Archive))?;
            repo::messages::upsert(conn, &message("m1", "src", 5, true))?;
            // Same UID, already present in the target folder.
            repo::messages::upsert(conn, &message("other", "dst", 5, false))?;

            repo::messages::move_to_folder_pending(conn, &["m1".to_string()], "dst")?;
            let moved = repo::messages::get(conn, "m1")?.expect("moved message");
            assert_eq!(moved.summary.folder_id, "dst");
            assert!(moved.uid.is_none(), "source UID must not survive a move");

            // UID 5 in the target folder is still the other message.
            let by_uid = repo::messages::by_uid(conn, "dst", 5)?.expect("uid lookup");
            assert_eq!(by_uid.summary.id, "other");

            // Moving a row into the folder it is already in (undo restores
            // flags that way) must not drop its UID.
            repo::messages::move_to_folder(conn, &["other".to_string()], "dst")?;
            let same_folder = repo::messages::get(conn, "other")?.expect("row");
            assert_eq!(same_folder.uid, Some(5), "same-folder move keeps the UID");

            // A failed op must restore the row even though its UID is gone.
            let payload = MoveOp {
                folder_id: "src".into(),
                target_folder_id: "dst".into(),
                uids: vec![5],
                message_ids: vec!["m1".into()],
                source_uidvalidity: Some(1),

                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            rollback_in_db(
                conn,
                &pending(KIND_MOVE, serde_json::to_value(payload).expect("json")),
                &live_state_with(Some(1), [5]),
            )?;
            let restored = repo::messages::get(conn, "m1")?.expect("restored message");
            assert_eq!(restored.summary.folder_id, "src");
            Ok(())
        })
        .expect("move and rollback");
    }

    #[test]
    fn rollback_restores_flags_folder_and_soft_delete() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::insert(conn, &folder("dst", "Archive", FolderRole::Archive))?;
            repo::folders::set_sync_flags(conn, "src", Some(1), None, None)?;
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
                source_uidvalidity: Some(1),
                previous: vec![FlagRollback {
                    uid: 5,
                    seen: false,
                    flagged: false,
                    answered: false,
                    forwarded: false,
                }],
                message_ids: Vec::new(),
                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            rollback_in_db(
                conn,
                &pending(KIND_SET_FLAGS, serde_json::to_value(payload).expect("json")),
                &LiveSourceState::default(),
            )?;
            let stored = repo::messages::get(conn, "m1")?.expect("message");
            assert!(!stored.summary.seen, "flag rollback must restore seen");

            // 2. Move: the row is in the target folder; rollback moves it back.
            repo::messages::move_to_folder_pending(conn, &["m1".to_string()], "dst")?;
            let moved = repo::messages::get(conn, "m1")?.expect("message");
            assert_eq!(moved.summary.folder_id, "dst");
            assert!(
                moved.uid.is_none(),
                "an optimistic move must clear the source UID"
            );
            let payload = MoveOp {
                folder_id: "src".into(),
                target_folder_id: "dst".into(),
                uids: vec![5],
                message_ids: vec!["m1".into()],
                source_uidvalidity: Some(1),

                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            rollback_in_db(
                conn,
                &pending(KIND_MOVE, serde_json::to_value(payload).expect("json")),
                &live_state_with(Some(1), [5]),
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
                message_ids: vec!["m1".into()],
                source_uidvalidity: Some(1),
                pending_delete: false,
                deleted_flag_set: false,
                snapshot: Vec::new(),
                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            rollback_in_db(
                conn,
                &pending(KIND_DELETE, serde_json::to_value(payload).expect("json")),
                &LiveSourceState::default(),
            )?;
            let stored = repo::messages::get(conn, "m1")?.expect("message");
            assert!(!stored.is_deleted_local, "soft delete must be reverted");
            Ok(())
        })
        .expect("rollback");
    }

    /// A failed flag command may only roll back the field it owned. Writing the
    /// whole snapshot would clear a star that a later action set.
    #[test]
    fn flag_rollback_leaves_fields_the_operation_did_not_write() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::set_sync_flags(conn, "src", Some(1), None, None)?;
            // Mark-as-read ran optimistically; the user then starred it.
            let mut stored = message("m1", "src", 5, true);
            stored.summary.flagged = true;
            repo::messages::upsert(conn, &stored)?;

            let payload = SetFlagsOp {
                folder_id: "src".into(),
                uids: vec![5],
                seen: Some(true),
                flagged: None,
                answered: None,
                forwarded: None,
                source_uidvalidity: Some(1),
                previous: vec![FlagRollback {
                    uid: 5,
                    seen: false,
                    flagged: false,
                    answered: false,
                    forwarded: false,
                }],
                message_ids: vec!["m1".into()],
                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            rollback_in_db(
                conn,
                &pending(KIND_SET_FLAGS, serde_json::to_value(payload).expect("json")),
                &LiveSourceState::default(),
            )?;
            let stored = repo::messages::get_required(conn, "m1")?;
            assert!(!stored.summary.seen, "the owned flag is restored");
            assert!(
                stored.summary.flagged,
                "the later star must survive the rollback"
            );
            Ok(())
        })
        .expect("rollback");
    }

    /// A field a later action took over is no longer this operation's to undo.
    #[test]
    fn flag_rollback_skips_a_field_changed_by_a_later_action() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::set_sync_flags(conn, "src", Some(1), None, None)?;
            // The failed op wanted seen=true, but a later action set it back to
            // false (and the user starred it).
            let mut stored = message("m1", "src", 5, false);
            stored.summary.flagged = true;
            repo::messages::upsert(conn, &stored)?;

            let payload = SetFlagsOp {
                folder_id: "src".into(),
                uids: vec![5],
                seen: Some(true),
                flagged: None,
                answered: None,
                forwarded: None,
                source_uidvalidity: Some(1),
                previous: vec![FlagRollback {
                    uid: 5,
                    seen: false,
                    flagged: false,
                    answered: false,
                    forwarded: false,
                }],
                message_ids: vec!["m1".into()],
                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            rollback_in_db(
                conn,
                &pending(KIND_SET_FLAGS, serde_json::to_value(payload).expect("json")),
                &LiveSourceState::default(),
            )?;
            let stored = repo::messages::get_required(conn, "m1")?;
            assert!(!stored.summary.seen, "the later value is kept");
            assert!(stored.summary.flagged, "the star is kept");
            Ok(())
        })
        .expect("rollback");
    }

    /// Ownership is per message and per field, and value equality does not
    /// prove it: a later action may have written the same value. The rollback
    /// and the send claim must both consult the durable writer.
    #[test]
    fn flag_ownership_uses_the_writer_not_the_value() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::set_sync_flags(conn, "src", Some(1), None, None)?;
            // The stale operation wrote seen=true; a later action wrote the
            // same value again. The current value alone cannot tell them apart.
            repo::messages::upsert(conn, &message("m1", "src", 5, true))?;
            repo::messages::record_flag_writer(
                conn,
                "m1",
                "seen",
                "later-op",
                Some(true),
                "setSeen",
            )?;
            assert!(
                !flag_write_owned(conn, "op", "m1", Some(true), None, None, None, false)?,
                "the send claim must see the later writer"
            );

            let payload = SetFlagsOp {
                folder_id: "src".into(),
                uids: vec![5],
                seen: Some(true),
                flagged: None,
                answered: None,
                forwarded: None,
                source_uidvalidity: Some(1),
                previous: vec![FlagRollback {
                    uid: 5,
                    seen: false,
                    flagged: false,
                    answered: false,
                    forwarded: false,
                }],
                message_ids: vec!["m1".into()],
                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            rollback_in_db(
                conn,
                &pending(KIND_SET_FLAGS, serde_json::to_value(payload).expect("json")),
                &LiveSourceState::default(),
            )?;
            let stored = repo::messages::get_required(conn, "m1")?;
            assert!(
                stored.summary.seen,
                "the later writer's value must survive a stale rollback"
            );
            Ok(())
        })
        .expect("writer ownership");
    }

    /// Undo keeps a cancelled operation as the writer when it refuses to
    /// re-queue a restore. That row will never be executed, but it is still a
    /// blocking record: a different queued operation must lose the field
    /// instead of falling back to the legacy "no writer row" permission.
    #[test]
    fn a_writer_owned_by_a_cancelled_operation_still_blocks() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::messages::upsert(conn, &message("m1", "src", 5, true))?;
            repo::messages::record_flag_writer(
                conn,
                "m1",
                "seen",
                "cancelled-op",
                Some(false),
                "setSeen",
            )?;
            assert!(
                !flag_write_owned(conn, "queued-op", "m1", Some(true), None, None, None, false,)?,
                "a writer row owned by a cancelled operation must still block the PATCH"
            );
            Ok(())
        })
        .expect("cancelled writer blocks");
    }

    /// A payload that recorded per-message ids was queued with its writer rows
    /// in one transaction. If those rows are gone (a sync deleted the
    /// projection), there is no evidence the operation still owns the field,
    /// so it must not be sent. A legacy payload without message ids keeps the
    /// fallback.
    #[test]
    fn a_missing_writer_blocks_a_payload_with_message_ids() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::messages::upsert(conn, &message("m1", "src", 5, true))?;
            assert!(
                !flag_write_owned(conn, "queued-op", "m1", Some(true), None, None, None, true)?,
                "a message-id payload without a writer row has no authorization evidence"
            );
            assert!(
                flag_write_owned(conn, "legacy-op", "m1", Some(true), None, None, None, false)?,
                "a legacy payload keeps the no-writer fallback"
            );
            Ok(())
        })
        .expect("missing writer blocks");
    }

    /// A single GET cannot disprove a move that timed out: the server may
    /// apply the POST afterwards. Only a definitive `NotExecuted` may release
    /// the reconciliation protection.
    #[test]
    fn graph_move_recovery_only_trusts_a_definitive_not_executed() {
        assert!(graph_move_may_have_run(
            Some(OpMessageState::Unknown),
            false,
            false
        ));
        assert!(graph_move_may_have_run(None, true, false));
        assert!(graph_move_may_have_run(None, false, true));
        assert!(!graph_move_may_have_run(None, false, false));
        assert!(!graph_move_may_have_run(
            Some(OpMessageState::NotExecuted),
            true,
            true
        ));
        assert!(
            graph_move_may_have_run(Some(OpMessageState::Confirmed), false, false),
            "defensive: a confirmed move is never 'safe to restore'"
        );
    }

    /// Graph move recovery writes a target identity after a GET. A sync may
    /// bind a newer identity while that GET is in flight; the write must not
    /// overwrite it or clear its reconciliation state.
    #[test]
    fn graph_identity_recovery_never_overwrites_a_newer_binding() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::insert(conn, &folder("dst", "Archive", FolderRole::Archive))?;
            repo::folders::set_sync_flags(conn, "src", Some(1), None, None)?;
            let mut stored = message("m1", "src", 5, true);
            stored.uidvalidity = Some(1);
            stored.gm_msgid = Some("graph-source".into());
            repo::messages::upsert(conn, &stored)?;
            repo::messages::move_to_folder_pending(conn, &["m1".to_string()], "dst")?;
            repo::messages::mark_reconcile_owned(
                conn,
                &["m1".to_string()],
                repo::messages::RECONCILE_MOVE_ROLLBACK,
                "op",
                1,
                Some(5),
                Some(1),
            )?;

            // An unbound projection that still belongs to the operation is
            // bound by the recovery.
            let changed = repo::messages::set_graph_identity_if_pending(
                conn,
                "m1",
                "acc",
                "dst",
                5,
                "graph-target",
                "op",
            )?;
            assert_eq!(changed, 1);
            let bound = repo::messages::get_required(conn, "m1")?;
            assert_eq!(bound.uid, Some(5));
            assert_eq!(bound.gm_msgid.as_deref(), Some("graph-target"));

            // Reset to an unbound projection, then model a sync that bound a
            // newer identity before recovery's write lands.
            repo::messages::mark_reconcile_owned(
                conn,
                &["m1".to_string()],
                repo::messages::RECONCILE_MOVE_ROLLBACK,
                "op",
                1,
                Some(5),
                Some(1),
            )?;
            repo::messages::set_uid_identity(conn, "m1", Some(99), None)?;
            let changed = repo::messages::set_graph_identity_if_pending(
                conn,
                "m1",
                "acc",
                "dst",
                5,
                "graph-target",
                "op",
            )?;
            assert_eq!(changed, 0, "a newer binding must not be overwritten");
            let kept = repo::messages::get_required(conn, "m1")?;
            assert_eq!(kept.uid, Some(99));
            assert_eq!(kept.gm_msgid, None);
            Ok(())
        })
        .expect("graph recovery ownership");
    }

    /// A rollback must restore the UID *and* its generation; otherwise the
    /// next action on the row cannot build a verifiable payload at all.
    #[test]
    fn rollback_restores_uidvalidity_and_allows_a_later_move() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::insert(conn, &folder("dst", "Archive", FolderRole::Archive))?;
            repo::folders::set_sync_flags(conn, "src", Some(1), None, None)?;
            let mut stored = message("m1", "src", 5, true);
            stored.uidvalidity = Some(1);
            repo::messages::upsert(conn, &stored)?;

            repo::messages::move_to_folder_pending(conn, &["m1".to_string()], "dst")?;
            let moved = repo::messages::get_required(conn, "m1")?;
            assert!(moved.uid.is_none() && moved.uidvalidity.is_none());

            let payload = MoveOp {
                folder_id: "src".into(),
                target_folder_id: "dst".into(),
                uids: vec![5],
                message_ids: vec!["m1".into()],
                source_uidvalidity: Some(1),

                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            rollback_in_db(
                conn,
                &pending(KIND_MOVE, serde_json::to_value(payload).expect("json")),
                &live_state_with(Some(1), [5]),
            )?;
            let restored = repo::messages::get_required(conn, "m1")?;
            assert_eq!(restored.summary.folder_id, "src");
            assert_eq!(restored.uid, Some(5));
            assert_eq!(restored.uidvalidity, Some(1));
            assert!(repo::messages::target_pending_ids(conn, &["m1".to_string()])?.is_empty());

            // A later optimistic move starts from a fully addressed row again.
            repo::messages::move_to_folder_pending(conn, &["m1".to_string()], "dst")?;
            let moved = repo::messages::get_required(conn, "m1")?;
            assert_eq!(moved.summary.folder_id, "dst");
            assert!(moved.uid.is_none());
            Ok(())
        })
        .expect("rollback and move again");
    }

    /// The target folder's sync may bind its new UID to the row before the
    /// operation fails. Rolling back with a plain move used to carry that UID
    /// into the source folder, where it either collided with
    /// `idx_message_unique_uid` or stayed behind as a wrong address.
    #[test]
    fn rollback_after_target_uid_neither_collides_nor_keeps_the_target_uid() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::insert(conn, &folder("dst", "Archive", FolderRole::Archive))?;
            repo::folders::set_sync_flags(conn, "src", Some(7), None, None)?;
            let mut original = message("m1", "src", 5, true);
            original.uidvalidity = Some(7);
            repo::messages::upsert(conn, &original)?;
            // A different message already owns UID 9 in the source folder.
            repo::messages::upsert(conn, &message("neighbour", "src", 9, false))?;

            repo::messages::move_to_folder_pending(conn, &["m1".to_string()], "dst")?;
            // The target folder synced while the op was in flight: the row now
            // carries the target UID 9 (which is legal in this folder).
            let mut bound = repo::messages::get_required(conn, "m1")?;
            bound.summary.folder_id = "dst".into();
            bound.uid = Some(9);
            bound.uidvalidity = Some(3);
            repo::messages::upsert(conn, &bound)?;

            let payload = MoveOp {
                folder_id: "src".into(),
                target_folder_id: "dst".into(),
                uids: vec![5],
                message_ids: vec!["m1".into()],
                source_uidvalidity: Some(7),

                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            rollback_in_db(
                conn,
                &pending(KIND_MOVE, serde_json::to_value(payload).expect("json")),
                &live_state_with(Some(7), [5]),
            )?;

            // A target sync bound UID 9 to the row: that is positive evidence
            // the remote move happened, so the projection stays in the target
            // and must not carry the source UID (or collide with the source
            // folder's UID 9).
            let kept = repo::messages::get_required(conn, "m1")?;
            assert_eq!(kept.summary.folder_id, "dst");
            assert_eq!(kept.uid, Some(9), "the target UID is the confirmed one");
            assert_eq!(kept.uidvalidity, Some(3));
            assert!(
                repo::messages::target_pending_ids(conn, &["m1".to_string()])?.is_empty(),
                "a bound row is no longer waiting for its target"
            );
            assert_eq!(
                repo::messages::by_uid(conn, "dst", 9)?.map(|row| row.summary.id),
                Some("m1".to_string()),
                "the confirmed target UID belongs to the moved row"
            );
            assert_eq!(
                repo::messages::by_uid(conn, "src", 9)?.map(|row| row.summary.id),
                Some("neighbour".to_string()),
                "the source neighbour must keep its UID"
            );
            Ok(())
        })
        .expect("rollback after target uid");
    }

    /// The rollback is a batch: if one row is no longer the projection the
    /// operation moved, nothing may be restored. The old code moved the
    /// matching rows first and only then compared the count.
    #[test]
    fn rollback_does_not_partially_restore_a_changed_batch() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::insert(conn, &folder("dst", "Archive", FolderRole::Archive))?;
            repo::folders::set_sync_flags(conn, "src", Some(7), None, None)?;
            let mut first = message("m1", "src", 5, true);
            first.uidvalidity = Some(7);
            repo::messages::upsert(conn, &first)?;
            let mut second = message("m2", "src", 6, true);
            second.uidvalidity = Some(7);
            repo::messages::upsert(conn, &second)?;

            repo::messages::move_to_folder_pending(
                conn,
                &["m1".to_string(), "m2".to_string()],
                "dst",
            )?;
            // A concurrent action claimed the second row before the failed op
            // was rolled back (it is back in the source folder).
            repo::messages::move_to_folder(conn, &["m2".to_string()], "src")?;

            let payload = MoveOp {
                folder_id: "src".into(),
                target_folder_id: "dst".into(),
                uids: vec![5, 6],
                message_ids: vec!["m1".into(), "m2".into()],
                source_uidvalidity: Some(7),

                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            rollback_in_db(
                conn,
                &pending(KIND_MOVE, serde_json::to_value(payload).expect("json")),
                &live_state(Some(7)),
            )?;

            let first = repo::messages::get_required(conn, "m1")?;
            assert_eq!(
                first.summary.folder_id, "dst",
                "no row may be moved back when the batch is incomplete"
            );
            assert!(first.uid.is_none());
            let second = repo::messages::get_required(conn, "m2")?;
            assert_eq!(second.summary.folder_id, "src");
            Ok(())
        })
        .expect("partial rollback");
    }

    /// Without a definitive `UID SEARCH` answer the rollback must not guess:
    /// the projection stays in the target and is reconciled through its
    /// operation.
    #[test]
    fn rollback_keeps_the_projection_when_the_source_cannot_be_confirmed() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::insert(conn, &folder("dst", "Archive", FolderRole::Archive))?;
            repo::folders::set_sync_flags(conn, "src", Some(7), None, None)?;
            let mut original = message("m1", "src", 5, true);
            original.uidvalidity = Some(7);
            repo::messages::upsert(conn, &original)?;
            repo::messages::move_to_folder_pending(conn, &["m1".to_string()], "dst")?;

            let payload = MoveOp {
                folder_id: "src".into(),
                target_folder_id: "dst".into(),
                uids: vec![5],
                message_ids: vec!["m1".into()],
                source_uidvalidity: Some(7),

                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            // The SELECT/SEARCH could not run: no UID may be trusted.
            rollback_in_db(
                conn,
                &pending(KIND_MOVE, serde_json::to_value(payload).expect("json")),
                &live_state(Some(7)),
            )?;

            let moved = repo::messages::get_required(conn, "m1")?;
            assert_eq!(
                moved.summary.folder_id, "dst",
                "without evidence the projection must not move"
            );
            assert!(moved.uid.is_none());
            let reason: Option<String> = conn.query_row(
                "SELECT reconcile_reason FROM message WHERE id = 'm1'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(
                reason.as_deref(),
                Some(repo::messages::RECONCILE_MOVE_ROLLBACK),
                "the row is reconciled through its owning operation"
            );
            let op_id: Option<String> = conn.query_row(
                "SELECT reconcile_op_id FROM message WHERE id = 'm1'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(op_id.as_deref(), Some("op"));
            Ok(())
        })
        .expect("unconfirmed source rollback");
    }

    /// MOVE actually landed on the server but the client saw a failure. The
    /// source UID is gone in the same generation, so the row must stay in the
    /// target instead of being restored to a number that is no longer there.
    #[test]
    fn rollback_keeps_the_target_when_the_source_uid_is_gone() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::insert(conn, &folder("dst", "Archive", FolderRole::Archive))?;
            repo::folders::set_sync_flags(conn, "src", Some(7), None, None)?;
            let mut original = message("m1", "src", 5, true);
            original.uidvalidity = Some(7);
            repo::messages::upsert(conn, &original)?;
            repo::messages::move_to_folder_pending(conn, &["m1".to_string()], "dst")?;

            let payload = MoveOp {
                folder_id: "src".into(),
                target_folder_id: "dst".into(),
                uids: vec![5],
                message_ids: vec!["m1".into()],
                source_uidvalidity: Some(7),

                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            rollback_in_db(
                conn,
                &pending(KIND_MOVE, serde_json::to_value(payload).expect("json")),
                &live_state_with(Some(7), []),
            )?;

            let kept = repo::messages::get_required(conn, "m1")?;
            assert_eq!(
                kept.summary.folder_id, "dst",
                "the server moved it; the target sync owns the binding"
            );
            assert!(kept.uid.is_none());
            Ok(())
        })
        .expect("source-absent rollback");
    }

    /// A permanent delete removes the local row before the server confirms. If
    /// the command is refused (e.g. an old payload without UIDVALIDITY), the
    /// row must be rebuilt from the payload snapshot instead of waiting for a
    /// window-limited re-sync that may never return it.
    #[test]
    fn permanent_delete_rollback_rebuilds_the_row_from_its_snapshot() {
        use crate::service::messages::StoredSnapshot;

        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::set_sync_flags(conn, "src", Some(7), None, None)?;
            let mut stored = message("m1", "src", 5, true);
            stored.uidvalidity = Some(7);
            stored.summary.subject = "Recover me".into();
            stored.message_id_header = Some("<recover@x>".into());
            repo::messages::upsert(conn, &stored)?;
            let snapshot = StoredSnapshot::from(&stored);

            // The optimistic permanent delete removed the row.
            repo::messages::delete_local(conn, &["m1".to_string()], true)?;
            assert!(repo::messages::get(conn, "m1")?.is_none());
            // Another message took the freed UID in the meantime.
            repo::messages::upsert(conn, &message("other", "src", 5, false))?;

            let payload = DeleteOp {
                folder_id: "src".into(),
                uids: vec![5],
                permanent: true,
                target_folder_id: None,
                soft_hidden: false,
                message_ids: vec!["m1".into()],
                // An old payload has no source generation: the executor
                // refuses the command and rolls back.
                source_uidvalidity: None,
                pending_delete: false,
                deleted_flag_set: false,
                snapshot: vec![snapshot],
                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            rollback_in_db(
                conn,
                &pending(KIND_DELETE, serde_json::to_value(payload).expect("json")),
                &LiveSourceState::default(),
            )?;

            let restored = repo::messages::get_required(conn, "m1")?;
            assert_eq!(restored.summary.folder_id, "src");
            assert_eq!(restored.summary.subject, "Recover me");
            assert_eq!(restored.message_id_header.as_deref(), Some("<recover@x>"));
            assert!(
                restored.uid.is_none(),
                "an address already owned by another local row is not overwritten"
            );
            assert_eq!(
                repo::messages::by_uid(conn, "src", 5)?.map(|row| row.summary.id),
                Some("other".to_string())
            );
            Ok(())
        })
        .expect("permanent delete rollback");
    }

    /// A permanent-delete rollback rebuilds a missing row from its snapshot. A
    /// row that a newer trip has re-created/claimed in the meantime owns its id:
    /// the stale snapshot must not overwrite its identity, folder or owner.
    #[test]
    fn snapshot_restore_never_overwrites_a_newer_trip() {
        use crate::service::messages::StoredSnapshot;

        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::insert(conn, &folder("dst", "Archive", FolderRole::Archive))?;
            repo::folders::set_sync_flags(conn, "src", Some(1), None, None)?;

            let mut original = message("m1", "src", 5, true);
            original.uidvalidity = Some(1);
            original.summary.subject = "old snapshot".into();
            let snapshot = StoredSnapshot::from(&original);

            // A newer trip re-created the id in another folder and claimed its
            // identity slot after the snapshot was taken.
            let mut claimed = message("m1", "dst", 9, false);
            claimed.uidvalidity = None;
            claimed.summary.subject = "newer trip".into();
            repo::messages::upsert(conn, &claimed)?;
            conn.execute(
                "UPDATE message
                    SET uid = NULL, gm_msgid = NULL, target_pending = 1,
                        move_target_op_id = 'op-new', move_target_seq = 2,
                        move_target_remote_id = 'remote-new'
                  WHERE id = 'm1'",
                [],
            )?;

            assert_eq!(
                restore_deleted_snapshot(
                    conn,
                    "acc",
                    "src",
                    std::slice::from_ref(&snapshot),
                    true,
                    "op-old",
                    1,
                )?,
                0
            );
            let kept = repo::messages::get_required(conn, "m1")?;
            assert_eq!(kept.summary.folder_id, "dst");
            assert_eq!(kept.summary.subject, "newer trip");
            assert!(kept.uid.is_none());
            let owner: Option<String> = conn.query_row(
                "SELECT move_target_op_id FROM message WHERE id = 'm1'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(owner.as_deref(), Some("op-new"));
            Ok(())
        })
        .expect("newer trip owns the id");
    }

    /// The matching-owner path is unchanged: a genuinely missing row is rebuilt
    /// and marked as owned by the recovering operation.
    #[test]
    fn snapshot_restore_rebuilds_a_missing_row_for_its_operation() {
        use crate::service::messages::StoredSnapshot;

        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::set_sync_flags(conn, "src", Some(1), None, None)?;

            let mut original = message("m1", "src", 5, true);
            original.uidvalidity = Some(1);
            let snapshot = StoredSnapshot::from(&original);
            repo::messages::delete_local(conn, &["m1".to_string()], true)?;

            assert_eq!(
                restore_deleted_snapshot(
                    conn,
                    "acc",
                    "src",
                    std::slice::from_ref(&snapshot),
                    true,
                    "op-recover",
                    4,
                )?,
                1
            );
            let restored = repo::messages::get_required(conn, "m1")?;
            assert_eq!(restored.summary.folder_id, "src");
            assert!(restored.uid.is_none());
            let (pending, owner, sequence): (i64, Option<String>, Option<i64>) = conn.query_row(
                "SELECT target_pending, reconcile_op_id, reconcile_seq
                   FROM message WHERE id = 'm1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            assert_eq!(pending, 1);
            assert_eq!(owner.as_deref(), Some("op-recover"));
            assert_eq!(sequence, Some(4));
            Ok(())
        })
        .expect("snapshot restore");
    }

    /// The optimistic permanent delete only hides the row now. A refused
    /// command must unhide it, keep the attachment record, and drop the old
    /// UID because the command may still have executed: the row waits for the
    /// exact-generation UID check before it is addressable again.
    #[test]
    fn pending_permanent_delete_rollback_unhides_and_drops_the_uid() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::set_sync_flags(conn, "src", Some(7), None, None)?;
            let mut stored = message("m1", "src", 5, true);
            stored.uidvalidity = Some(7);
            stored.summary.has_attachments = true;
            repo::messages::upsert(conn, &stored)?;
            let mut attachment = nuncio_core::models::Attachment {
                id: String::new(),
                message_id: "m1".into(),
                filename: "report.pdf".into(),
                mime_type: "application/pdf".into(),
                size: 42,
                content_id: None,
                is_inline: false,
                is_downloaded: true,
            };
            repo::attachments::insert(conn, &mut attachment)?;

            repo::messages::hide_for_permanent_delete(conn, &["m1".to_string()])?;
            let hidden = repo::messages::get_required(conn, "m1")?;
            assert!(hidden.is_deleted_local, "the row must be hidden first");
            assert_eq!(
                repo::attachments::list(conn, "m1")?.len(),
                1,
                "hiding must keep the attachment record"
            );

            let payload = DeleteOp {
                folder_id: "src".into(),
                uids: vec![5],
                permanent: true,
                target_folder_id: None,
                soft_hidden: false,
                message_ids: vec!["m1".into()],
                source_uidvalidity: Some(7),
                pending_delete: true,
                deleted_flag_set: false,
                snapshot: Vec::new(),
                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            rollback_in_db(
                conn,
                &pending(KIND_DELETE, serde_json::to_value(payload).expect("json")),
                &LiveSourceState::default(),
            )?;

            let restored = repo::messages::get_required(conn, "m1")?;
            assert!(
                !restored.is_deleted_local,
                "the refusal must unhide the row"
            );
            assert!(restored.uid.is_none(), "the old UID is no longer trusted");
            assert!(restored.uidvalidity.is_none());
            assert_eq!(
                repo::messages::target_pending_ids(conn, &["m1".to_string()])?,
                vec!["m1".to_string()],
                "a sync must rebind the identity before any UID action"
            );
            assert_eq!(
                repo::attachments::list(conn, "m1")?.len(),
                1,
                "the attachment record must survive the rollback"
            );
            let pending: i64 = conn.query_row(
                "SELECT pending_delete FROM message WHERE id = 'm1'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(pending, 0);
            Ok(())
        })
        .expect("pending permanent delete rollback");
    }

    /// A UID that the live folder still reports under the queued generation is
    /// not dropped: the row becomes addressable immediately instead of waiting
    /// for a reconciliation that only exists to repair an unknown state.
    #[test]
    fn pending_permanent_delete_rollback_keeps_a_confirmed_uid() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::set_sync_flags(conn, "src", Some(7), None, None)?;
            let mut stored = message("m1", "src", 5, true);
            stored.uidvalidity = Some(7);
            repo::messages::upsert(conn, &stored)?;
            repo::messages::hide_for_permanent_delete(conn, &["m1".to_string()])?;

            let payload = DeleteOp {
                folder_id: "src".into(),
                uids: vec![5],
                permanent: true,
                target_folder_id: None,
                soft_hidden: false,
                message_ids: vec!["m1".into()],
                source_uidvalidity: Some(7),
                pending_delete: true,
                deleted_flag_set: false,
                snapshot: Vec::new(),
                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            rollback_in_db(
                conn,
                &pending(KIND_DELETE, serde_json::to_value(payload).expect("json")),
                &LiveSourceState {
                    uidvalidity: Some(7),
                    present_uids: Some(std::collections::HashSet::from([5])),
                    graph: false,
                },
            )?;

            let restored = repo::messages::get_required(conn, "m1")?;
            assert!(!restored.is_deleted_local);
            assert_eq!(restored.uid, Some(5), "a confirmed UID is kept");
            assert_eq!(restored.uidvalidity, Some(7));
            assert!(
                repo::messages::target_pending_ids(conn, &["m1".to_string()])?.is_empty(),
                "no reconciliation is needed"
            );
            let pending: i64 = conn.query_row(
                "SELECT pending_delete FROM message WHERE id = 'm1'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(pending, 0);
            Ok(())
        })
        .expect("trusted permanent delete rollback");
    }

    /// When the server no longer reports the restored UID, the row must not
    /// keep it, but it must also not keep `pending_delete`: it becomes a
    /// window-independent reconciliation candidate instead.
    #[test]
    fn pending_permanent_delete_rollback_marks_an_unknown_identity() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::set_sync_flags(conn, "src", Some(7), None, None)?;
            let mut stored = message("m1", "src", 5, true);
            stored.uidvalidity = Some(7);
            repo::messages::upsert(conn, &stored)?;
            repo::messages::hide_for_permanent_delete(conn, &["m1".to_string()])?;

            let payload = DeleteOp {
                folder_id: "src".into(),
                uids: vec![5],
                permanent: true,
                target_folder_id: None,
                soft_hidden: false,
                message_ids: vec!["m1".into()],
                source_uidvalidity: Some(7),
                pending_delete: true,
                deleted_flag_set: false,
                snapshot: Vec::new(),
                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            rollback_in_db(
                conn,
                &pending(KIND_DELETE, serde_json::to_value(payload).expect("json")),
                &LiveSourceState {
                    uidvalidity: Some(7),
                    present_uids: Some(std::collections::HashSet::new()),
                    graph: false,
                },
            )?;

            let restored = repo::messages::get_required(conn, "m1")?;
            assert!(!restored.is_deleted_local);
            assert!(restored.uid.is_none());
            let awaiting: i64 = conn.query_row(
                "SELECT awaiting_reconcile FROM message WHERE id = 'm1'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(awaiting, 1);
            Ok(())
        })
        .expect("unknown permanent delete rollback");
    }

    /// Graph ids do not change when a command is refused, so the rollback must
    /// keep the opaque id and the synthetic UID. Dropping them leaves the row
    /// unusable: an unchanged mailbox produces an empty delta, so no later
    /// sync would ever bring the identity back.
    #[test]
    fn graph_permanent_delete_rollback_keeps_the_synthetic_identity() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::set_sync_flags(conn, "src", Some(7), None, None)?;
            let mut stored = message("m1", "src", 42, true);
            stored.uidvalidity = Some(7);
            stored.gm_msgid = Some("AAMk-graph-id".into());
            repo::messages::upsert(conn, &stored)?;
            repo::messages::hide_for_permanent_delete(conn, &["m1".to_string()])?;

            let payload = DeleteOp {
                folder_id: "src".into(),
                uids: vec![42],
                permanent: true,
                target_folder_id: None,
                soft_hidden: false,
                message_ids: vec!["m1".into()],
                source_uidvalidity: Some(7),
                pending_delete: true,
                deleted_flag_set: false,
                snapshot: Vec::new(),
                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            rollback_in_db(
                conn,
                &pending(KIND_DELETE, serde_json::to_value(payload).expect("json")),
                &LiveSourceState {
                    uidvalidity: Some(7),
                    present_uids: None,
                    graph: true,
                },
            )?;

            let restored = repo::messages::get_required(conn, "m1")?;
            assert!(!restored.is_deleted_local);
            assert_eq!(restored.uid, Some(42), "the synthetic UID stays usable");
            assert_eq!(restored.gm_msgid.as_deref(), Some("AAMk-graph-id"));
            assert!(
                repo::messages::target_pending_ids(conn, &["m1".to_string()])?.is_empty(),
                "no sync is required to make the row actionable again"
            );
            assert_eq!(
                repo::messages::by_uid(conn, "src", 42)?.map(|row| row.summary.id),
                Some("m1".to_string())
            );
            Ok(())
        })
        .expect("graph identity-preserving rollback");
    }

    /// A batch pre-check failure used to leave every optimistic projection in
    /// the target folder forever. Rows the server still holds in the source are
    /// now moved back and re-addressed by local id; rows another action already
    /// claimed are left alone.
    #[test]
    fn failed_projection_batch_is_converged_by_local_id() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::insert(conn, &folder("dst", "Archive", FolderRole::Archive))?;
            repo::folders::set_sync_flags(conn, "src", Some(7), None, None)?;
            for (id, uid) in [("m1", 5), ("m2", 6)] {
                let mut stored = message(id, "src", uid, true);
                stored.uidvalidity = Some(7);
                repo::messages::upsert(conn, &stored)?;
            }
            repo::messages::move_to_folder_pending(
                conn,
                &["m1".to_string(), "m2".to_string()],
                "dst",
            )?;
            // A concurrent action already put m2 back in the source folder; the
            // batch is therefore no longer one intact projection.
            repo::messages::move_to_folder(conn, &["m2".to_string()], "src")?;

            let payload = MoveOp {
                folder_id: "src".into(),
                target_folder_id: "dst".into(),
                uids: vec![5, 6],
                message_ids: vec!["m1".into(), "m2".into()],
                source_uidvalidity: Some(7),

                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            rollback_in_db(
                conn,
                &pending(KIND_MOVE, serde_json::to_value(payload).expect("json")),
                &LiveSourceState {
                    uidvalidity: Some(7),
                    present_uids: Some(std::collections::HashSet::from([5, 6])),
                    graph: false,
                },
            )?;

            let first = repo::messages::get_required(conn, "m1")?;
            assert_eq!(first.summary.folder_id, "src");
            assert_eq!(first.uid, Some(5), "the confirmed source UID returns");
            assert!(repo::messages::target_pending_ids(conn, &["m1".to_string()])?.is_empty());
            let second = repo::messages::get_required(conn, "m2")?;
            assert_eq!(
                second.summary.folder_id, "src",
                "the row another action claimed is untouched"
            );
            Ok(())
        })
        .expect("converge failed projection");
    }

    /// An older batch that fails on one of its remaining rows must not erase
    /// the destination a newer move trip recorded on the same projection, and
    /// must not move that projection back to the source folder.
    #[test]
    fn an_older_batch_failure_keeps_a_newer_trip_ownership() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::insert(conn, &folder("dst", "Archive", FolderRole::Archive))?;
            repo::folders::set_sync_flags(conn, "src", Some(7), None, None)?;
            let mut stored = message("m1", "src", 5, true);
            stored.uidvalidity = Some(7);
            repo::messages::upsert(conn, &stored)?;
            repo::messages::move_to_folder_pending(conn, &["m1".to_string()], "dst")?;

            // A newer trip claimed the slot and recorded its destination.
            repo::messages::claim_move_target_owner(
                conn,
                "acc",
                &["m1".to_string()],
                "dst",
                "op-new",
                2,
            )?;
            assert_eq!(
                repo::messages::set_move_target_identity(
                    conn,
                    "acc",
                    "m1",
                    "dst",
                    "m-current",
                    "op-new",
                    2
                )?,
                1
            );

            // The older batch still lists a row that is already gone, which is
            // what used to trigger the failing converge pass.
            let payload = MoveOp {
                folder_id: "src".into(),
                target_folder_id: "dst".into(),
                uids: vec![5, 6],
                message_ids: vec!["m1".into(), "missing".into()],
                source_uidvalidity: Some(7),
                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            rollback_in_db(
                conn,
                &pending(KIND_MOVE, serde_json::to_value(payload).expect("json")),
                &live_state_with(Some(7), [5, 6]),
            )?;

            let kept = repo::messages::get_required(conn, "m1")?;
            assert_eq!(kept.summary.folder_id, "dst");
            assert!(kept.uid.is_none());
            let (remote, owner, reconcile): (Option<String>, Option<String>, Option<String>) = conn
                .query_row(
                    "SELECT move_target_remote_id, move_target_op_id, reconcile_op_id
                       FROM message WHERE id = 'm1'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
            assert_eq!(remote.as_deref(), Some("m-current"));
            assert_eq!(owner.as_deref(), Some("op-new"));
            assert_eq!(reconcile, None, "the old failure must not claim the row");
            assert_eq!(
                repo::messages::confirmed_move_projection_targets(conn, "acc", "dst")?
                    .get("m-current")
                    .map(String::as_str),
                Some("m1")
            );
            Ok(())
        })
        .expect("older batch keeps newer ownership");
    }

    /// A per-message `Confirmed` receipt survives another message's failure in
    /// the same batch: the confirmed move is never rolled back or downgraded
    /// to a reconciliation row.
    #[test]
    fn a_confirmed_move_survives_a_sibling_failure_in_the_same_batch() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::folders::insert(conn, &folder("dst", "Archive", FolderRole::Archive))?;
            repo::folders::set_sync_flags(conn, "src", Some(7), None, None)?;
            let mut stored = message("m1", "src", 5, true);
            stored.uidvalidity = Some(7);
            repo::messages::upsert(conn, &stored)?;
            repo::messages::move_to_folder_pending(conn, &["m1".to_string()], "dst")?;
            repo::messages::claim_move_target_owner(
                conn,
                "acc",
                &["m1".to_string()],
                "dst",
                "op",
                1,
            )?;
            assert_eq!(
                repo::messages::set_move_target_identity(
                    conn,
                    "acc",
                    "m1",
                    "dst",
                    "m-confirmed",
                    "op",
                    1
                )?,
                1
            );

            let confirmed = OpMessageOutcome {
                message_id: "m1".into(),
                source_uid: Some(5),
                state: OpMessageState::Confirmed,
                target_folder_id: Some("dst".into()),
                target_uid: None,
                target_uidvalidity: None,
                target_remote_id: Some("m-confirmed".into()),
                attempt_remote_id: None,
                copy_phase: Default::default(),
                delete_flag_phase: Default::default(),
                expunge_phase: Default::default(),
            };
            let payload = MoveOp {
                folder_id: "src".into(),
                target_folder_id: "dst".into(),
                uids: vec![5, 6],
                message_ids: vec!["m1".into(), "missing".into()],
                source_uidvalidity: Some(7),
                outcomes: vec![confirmed],
                remote_ids: Vec::new(),
            };
            rollback_in_db(
                conn,
                &pending(KIND_MOVE, serde_json::to_value(payload).expect("json")),
                &live_state_with(Some(7), [5, 6]),
            )?;

            let kept = repo::messages::get_required(conn, "m1")?;
            assert_eq!(
                kept.summary.folder_id, "dst",
                "the confirmed move must not be rolled back to the source"
            );
            assert!(kept.uid.is_none());
            let (remote, owner, pending, reconcile): (
                Option<String>,
                Option<String>,
                i64,
                Option<String>,
            ) = conn.query_row(
                "SELECT move_target_remote_id, move_target_op_id, target_pending, reconcile_op_id
                   FROM message WHERE id = 'm1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
            assert_eq!(remote.as_deref(), Some("m-confirmed"));
            assert_eq!(owner.as_deref(), Some("op"));
            assert_eq!(pending, 1);
            assert_eq!(
                reconcile, None,
                "a sibling failure must not downgrade a confirmed message"
            );
            Ok(())
        })
        .expect("confirmed receipt survives batch failure");
    }

    /// An archive rule with no target folder hides the row in place. Its
    /// rollback has to unhide by local id; the move rollback would look in a
    /// target folder that never existed.
    #[test]
    fn archive_soft_hidden_rollback_unhides_in_place() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::messages::upsert(conn, &message("m1", "src", 5, true))?;
            repo::messages::delete_local(conn, &["m1".to_string()], false)?;

            let payload = ArchiveOp {
                folder_id: "src".into(),
                uids: vec![5],
                target_folder_id: None,
                soft_hidden: true,
                message_ids: vec!["m1".into()],
                source_uidvalidity: Some(1),

                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            rollback_in_db(
                conn,
                &pending(KIND_ARCHIVE, serde_json::to_value(payload).expect("json")),
                &LiveSourceState::default(),
            )?;

            let restored = repo::messages::get_required(conn, "m1")?;
            assert_eq!(restored.summary.folder_id, "src");
            assert!(!restored.is_deleted_local);
            Ok(())
        })
        .expect("archive soft-hidden rollback");
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

    /// A remote-draft DELETE that completed after the operation was cancelled
    /// must clear the Graph delta cursor, not only UIDNEXT: Graph incremental
    /// sync does not use UIDNEXT, so leaving the cursor behind would keep the
    /// projection stale forever.
    #[tokio::test]
    async fn cancelled_in_flight_graph_delete_clears_the_delta_cursor() {
        let ctx = context_with_account().await;
        let folder_id = "graph-drafts";
        let cursor_key = crate::graph::messages::delta_cursor_key(folder_id);
        ctx.db
            .write({
                let cursor_key = cursor_key.clone();
                move |db| {
                    repo::folders::insert(db, &folder(folder_id, "Drafts", FolderRole::Drafts))?;
                    repo::folders::set_sync_flags(db, folder_id, Some(1), Some(55), None)?;
                    repo::sync_state::set(
                        db,
                        "acc",
                        "mail",
                        &cursor_key,
                        "https://graph.test/delta-cursor",
                    )?;
                    Ok(())
                }
            })
            .await
            .expect("seed folder");
        let queued = ctx
            .db
            .write(|db| {
                let payload = DeleteRemoteDraftOp {
                    folder_id: folder_id.into(),
                    uid: 7,
                    remote_message_id: None,
                    draft_id: Some("draft-1".into()),
                    source_uidvalidity: Some(1),
                    message_id: None,
                    outcome_unknown: true,
                    quarantined: false,
                    quarantine_reason: None,
                };
                repo::pending_ops::enqueue(
                    db,
                    nuncio_core::models::NewPendingOp::new(
                        "acc",
                        KIND_DELETE_REMOTE_DRAFT,
                        serde_json::to_value(payload).expect("json"),
                        "Remove remote draft",
                    ),
                )
            })
            .await
            .expect("queue");
        // The worker claimed the row; the switch then recorded the unknown
        // outcome and cancelled it while the DELETE was still on the wire.
        ctx.db
            .write({
                let id = queued.id.clone();
                move |db| {
                    repo::pending_ops::mark_in_progress(db, &id)?;
                    repo::pending_ops::cancel(db, &id)
                }
            })
            .await
            .expect("cancel in flight");

        let changed = finish_ops(&ctx, std::slice::from_ref(&queued.id))
            .await
            .expect("finish");
        assert_eq!(changed, 0, "a cancelled row is not marked done");
        ctx.db
            .read({
                let cursor_key = cursor_key.clone();
                move |db| {
                    let (_, uidnext, _) = repo::folders::sync_flags(db, folder_id)?;
                    assert!(uidnext.is_none(), "UIDNEXT is cleared for a full resync");
                    assert_eq!(
                        repo::sync_state::get(db, "acc", "mail", &cursor_key)?,
                        None,
                        "the Graph delta cursor must be cleared so the next sync is a full scan"
                    );
                    Ok(())
                }
            })
            .await
            .expect("assert resync markers");
    }

    /// The Graph send claim must verify the writer and persist its take-off
    /// marker in the *same* write transaction. A projection deleted after an
    /// earlier check (the batch pass or a stale guard read) must therefore
    /// refuse the send instead of leaving an intent that authorises a PATCH
    /// against the vanished copy.
    #[test]
    fn graph_send_claim_rechecks_the_projection_in_the_intent_transaction() {
        let db = setup();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder("src", "INBOX", FolderRole::Inbox))?;
            repo::messages::upsert(conn, &message("m1", "src", 5, false))?;
            let payload = SetFlagsOp {
                folder_id: "src".into(),
                uids: vec![5],
                seen: Some(true),
                flagged: None,
                answered: None,
                forwarded: None,
                source_uidvalidity: Some(1),
                previous: Vec::new(),
                message_ids: vec!["m1".into()],
                outcomes: Vec::new(),
                remote_ids: vec!["AAMk-graph-1".into()],
            };
            repo::pending_ops::enqueue_with_id(
                conn,
                "op",
                nuncio_core::models::NewPendingOp::new(
                    "acc",
                    KIND_SET_FLAGS,
                    serde_json::to_value(&payload).expect("payload"),
                    "flag",
                ),
            )?;
            repo::messages::record_flag_writer(conn, "m1", "seen", "op", Some(true), "setSeen")?;
            // The earlier batch/guard read sees a valid claim...
            assert!(flag_write_owned(
                conn,
                "op",
                "m1",
                Some(true),
                None,
                None,
                None,
                true
            )?);
            // ...but the projection is deleted before the send claim can
            // commit its take-off marker.
            repo::messages::delete_local(conn, &["m1".to_string()], true)?;

            let claimed = claim_graph_flag_send_in_db(conn, "op", "m1", &payload, 5, true)?;
            assert!(
                !claimed,
                "a delete committed after the read must block the PATCH"
            );
            let op = repo::pending_ops::get_required(conn, "op")?;
            let payload: SetFlagsOp = repo::pending_ops::payload(&op)?;
            assert!(
                payload.outcomes.is_empty(),
                "a refused send must not leave a take-off marker: {:?}",
                payload.outcomes
            );
            Ok(())
        })
        .expect("atomic send claim");
    }
}
