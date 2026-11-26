//! Undo for optimistic message actions.
//!
//! An undo is a durable, per-message state machine. The user action recorded in
//! `undo_log` is materialised into one `undo_request` with one `undo_entry` per
//! message; each entry is classified independently:
//!
//! * the operation has not started (`pending`) -> cancel it atomically and
//!   restore the local snapshot;
//! * the operation already ran (`done`) -> queue exactly one inverse operation
//!   using the message's *target* identity, and record its id on the entry;
//! * the operation is in flight (`inProgress`) or its evidence vanished -> keep
//!   the request `Deferred` and let the queue worker resume it once the
//!   operation settles.
//!
//! A request becomes `Done` only when every entry is done; a single failed entry
//! keeps it `Failed` and retryable.

use std::collections::{HashMap, HashSet};

use nuncio_core::events::MailChangeKind;
use nuncio_core::models::{
    new_id, NewPendingOp, NewUndoEntry, NewUndoRequest, PendingOp, PendingOpState, StoredMessage,
    UndoEntry, UndoEntryResult, UndoRequest, UndoRequestEntry, UndoState,
};
use nuncio_core::{events, repo, AppError, CoreContext, Result};
use rusqlite::{Connection, OptionalExtension};
use tracing::{debug, warn};

use crate::ops::{
    self, DeleteOp, MarkJunkOp, MoveOp, OpMessageOutcome, OpMessageState, SetFlagsOp, KIND_ARCHIVE,
    KIND_DELETE, KIND_MARK_JUNK, KIND_MOVE, KIND_SET_FLAGS,
};

use super::messages::{UndoEntryState, UndoPayload};
use super::MailService;

/// How many worker passes may defer an entry whose operation rows disappeared
/// before it is recorded as failed. The queue keeps settled rows for a long
/// retention, so this only triggers on corrupt/pruned evidence.
const MAX_UNDO_ATTEMPTS: u32 = 3;

/// How many fresh inverse operations may be queued after a predecessor was
/// provably never sent. Each re-plan is a new operation with a zero retry
/// count; without this bound a target that fails before every send (for
/// example a folder that disappeared) would re-plan forever.
const MAX_INVERSE_REPLANS: u32 = 3;

impl MailService {
    /// Revert the most recent undoable action (the keyboard shortcut path).
    pub(crate) async fn undo_last(&self) -> Result<bool> {
        undo_last(&self.inner.ctx).await
    }

    /// Revert one specific action, as identified by the id returned when it ran.
    ///
    /// Toasts use this so an action that happened afterwards is not the one
    /// that gets reverted.
    pub(crate) async fn undo_by_id(&self, undo_id: &str) -> Result<bool> {
        undo_by_id(&self.inner.ctx, undo_id).await
    }
}

/// Continue deferred undo requests after operations settle.
///
/// Called by the queue worker after a pass. Errors are per-request: one broken
/// request must not stop the others from finishing.
pub async fn resume_deferred_undo_requests(ctx: &CoreContext) -> Result<usize> {
    let requests = ctx
        .db
        .read(|db| repo::undo::resumable_requests(db, 32))
        .await?;
    let mut resumed = 0;
    for request in requests {
        match process_request(ctx, &request.id).await {
            Ok(_) => resumed += 1,
            Err(error) => {
                debug!(request = %request.id, %error, "could not resume deferred undo");
            }
        }
    }
    Ok(resumed)
}

async fn undo_last(ctx: &CoreContext) -> Result<bool> {
    let Some(request) = begin_or_resume(ctx, None).await? else {
        return Ok(false);
    };
    finish_undo(ctx, request.0.id).await
}

async fn undo_by_id(ctx: &CoreContext, undo_id: &str) -> Result<bool> {
    let Some(request) = begin_or_resume(ctx, Some(undo_id)).await? else {
        return Ok(false);
    };
    finish_undo(ctx, request.0.id).await
}

/// Run the state machine and translate its terminal state into the bool the
/// command layer expects.
async fn finish_undo(ctx: &CoreContext, request_id: String) -> Result<bool> {
    let outcome = process_request(ctx, &request_id).await?;
    if outcome.permanent_gone {
        return Err(AppError::conflict(
            "已删除，无法撤销：消息已在服务器上永久删除",
        ));
    }
    match outcome.state {
        UndoState::Done | UndoState::Deferred | UndoState::Compensating => Ok(true),
        UndoState::Pending => Ok(true),
        UndoState::Failed => {
            Err(AppError::conflict(outcome.last_error.unwrap_or_else(
                || "could not undo every message; retry later".to_string(),
            )))
        }
    }
}

/// Find an existing unfinished request (resume) or materialise the undo log row
/// (new request). The log row is consumed and the request created atomically.
async fn begin_or_resume(
    ctx: &CoreContext,
    undo_id: Option<&str>,
) -> Result<Option<(UndoRequest, Vec<UndoRequestEntry>)>> {
    let undo_id = undo_id.map(str::to_string);
    ctx.db
        .write(move |db| begin_or_resume_db(db, undo_id.as_deref()))
        .await
}

fn begin_or_resume_db(
    db: &Connection,
    undo_id: Option<&str>,
) -> Result<Option<(UndoRequest, Vec<UndoRequestEntry>)>> {
    if let Some(id) = undo_id {
        if let Some(request) = repo::undo::get_request(db, id)? {
            if request.state == UndoState::Done {
                return Ok(None);
            }
            let entries = repo::undo::list_entries(db, &request.id)?;
            return Ok(Some((request, entries)));
        }
        let Some(entry) = repo::undo::pop_by_id(db, id)? else {
            return Ok(None);
        };
        return create_request_from_log(db, entry);
    }
    if let Some(request) = repo::undo::latest_unfinished_request(db)? {
        let entries = repo::undo::list_entries(db, &request.id)?;
        return Ok(Some((request, entries)));
    }
    let Some(entry) = repo::undo::pop_last(db, None)? else {
        return Ok(None);
    };
    create_request_from_log(db, entry)
}

fn create_request_from_log(
    db: &Connection,
    entry: UndoEntry,
) -> Result<Option<(UndoRequest, Vec<UndoRequestEntry>)>> {
    let payload: UndoPayload = serde_json::from_value(entry.payload.clone())
        .map_err(|error| AppError::internal(format!("invalid undo payload: {error}")))?;
    if payload.entries.is_empty() {
        return Ok(None);
    }

    let (resolved, any_missing) = resolve_ops(db, &payload.op_ids)?;
    let mut new_entries = Vec::with_capacity(payload.entries.len());
    for state in &payload.entries {
        let covering: Vec<&ResolvedOp> = resolved
            .iter()
            .filter(|op| op.coverage.covers(state))
            .collect();
        let op_ids = covering.iter().map(|op| op.id.clone()).collect::<Vec<_>>();
        // A referenced operation that cannot be resolved is *missing
        // evidence*, not proof that this message never had a server command.
        let remote = remote_account(db, &state.account_id)?;
        let evidence_missing =
            any_missing || (remote && !payload.op_ids.is_empty() && covering.is_empty());

        let (mut target_folder, mut target_uid, mut target_uidvalidity) =
            if matches!(payload.action.as_str(), "setSeen" | "setFlagged") {
                (
                    Some(state.folder_id.clone()),
                    state.uid,
                    state.snapshot.uidvalidity,
                )
            } else {
                // Never infer the target from a folder/UID guess. Only the
                // original operation's own per-message result may name it.
                (state.target_folder_id.clone(), None, None)
            };
        let mut target_graph_id = state.snapshot.gm_msgid.clone();
        if !matches!(payload.action.as_str(), "setSeen" | "setFlagged") {
            for op in &covering {
                if let Some(outcome) = op.confirmed_outcome(&state.id) {
                    if let Some(folder) = outcome.target_folder_id.clone() {
                        target_folder = Some(folder);
                    }
                    target_uid = outcome.target_uid;
                    target_uidvalidity = outcome.target_uidvalidity;
                    if let Some(graph) = outcome.target_remote_id.clone() {
                        target_graph_id = Some(graph);
                    }
                    break;
                }
            }
        }
        new_entries.push(NewUndoEntry {
            id: new_id(),
            request_id: entry.id.clone(),
            account_id: state.account_id.clone(),
            message_id: state.id.clone(),
            original_folder_id: state.folder_id.clone(),
            target_folder_id: target_folder,
            target_uid,
            target_uidvalidity,
            target_graph_id,
            permanent: state.permanent,
            op_ids,
            evidence_missing,
            payload: serde_json::to_value(state).map_err(|error| {
                AppError::internal(format!("cannot encode undo entry: {error}"))
            })?,
            ordinal: 0,
        });
    }
    let request = NewUndoRequest::new(
        entry.id,
        entry.account_id,
        payload.action.clone(),
        payload.value,
        payload.op_ids.clone(),
    );
    let stored = repo::undo::create_request(db, request, &new_entries)?;
    let entries = repo::undo::list_entries(db, &stored.id)?;
    Ok(Some((stored, entries)))
}

/// What one queued operation covers, used to bind entries to their operations.
enum OpCoverage {
    Flags {
        folder_id: String,
        uids: HashSet<i64>,
    },
    Messages {
        folder_id: String,
        uids: HashSet<i64>,
        message_ids: HashSet<String>,
    },
}

impl OpCoverage {
    fn covers(&self, entry: &UndoEntryState) -> bool {
        match self {
            Self::Flags { folder_id, uids } => {
                entry.folder_id == *folder_id
                    && entry.uid.map(|uid| uids.contains(&uid)).unwrap_or(false)
            }
            Self::Messages {
                folder_id,
                uids,
                message_ids,
            } => {
                if message_ids.contains(&entry.id) {
                    return true;
                }
                message_ids.is_empty()
                    && entry.folder_id == *folder_id
                    && entry.uid.map(|uid| uids.contains(&uid)).unwrap_or(false)
            }
        }
    }
}

/// One resolved queued operation: what it covers, its per-message results and
/// the durable markers that decide whether it may still be cancelled.
struct ResolvedOp {
    id: String,
    coverage: OpCoverage,
    outcomes: Vec<OpMessageOutcome>,
}

impl ResolvedOp {
    fn parse(op: &PendingOp) -> Option<Self> {
        let (coverage, outcomes) = match op.kind.as_str() {
            KIND_SET_FLAGS => {
                let payload = repo::pending_ops::payload::<SetFlagsOp>(op).ok()?;
                (
                    OpCoverage::Flags {
                        folder_id: payload.folder_id,
                        uids: payload.uids.into_iter().collect(),
                    },
                    // Graph PATCHes one message at a time, so a setFlags batch
                    // can carry per-message receipts. Undo must see a message
                    // that was confirmed (compensate) or skipped (do not
                    // compensate) instead of treating the whole batch as sent.
                    payload.outcomes,
                )
            }
            KIND_MOVE => {
                let payload = repo::pending_ops::payload::<MoveOp>(op).ok()?;
                (move_coverage(&payload), payload.outcomes)
            }
            KIND_ARCHIVE => {
                let payload = repo::pending_ops::payload::<crate::ops::ArchiveOp>(op).ok()?;
                (
                    OpCoverage::Messages {
                        folder_id: payload.folder_id,
                        uids: payload.uids.into_iter().collect(),
                        message_ids: payload.message_ids.into_iter().collect(),
                    },
                    payload.outcomes,
                )
            }
            KIND_MARK_JUNK => {
                let payload = repo::pending_ops::payload::<MarkJunkOp>(op).ok()?;
                (
                    OpCoverage::Messages {
                        folder_id: payload.folder_id,
                        uids: payload.uids.into_iter().collect(),
                        message_ids: payload.message_ids.into_iter().collect(),
                    },
                    payload.outcomes,
                )
            }
            KIND_DELETE => {
                let payload = repo::pending_ops::payload::<DeleteOp>(op).ok()?;
                (
                    OpCoverage::Messages {
                        folder_id: payload.folder_id,
                        uids: payload.uids.into_iter().collect(),
                        message_ids: payload.message_ids.into_iter().collect(),
                    },
                    payload.outcomes,
                )
            }
            _ => return None,
        };
        Some(Self {
            id: op.id.clone(),
            coverage,
            outcomes,
        })
    }

    fn outcome(&self, message_id: &str) -> Option<&OpMessageOutcome> {
        self.outcomes
            .iter()
            .find(|outcome| outcome.message_id == message_id)
    }

    fn confirmed_outcome(&self, message_id: &str) -> Option<&OpMessageOutcome> {
        self.outcome(message_id)
            .filter(|outcome| outcome.state == OpMessageState::Confirmed)
    }
}

fn move_coverage(payload: &MoveOp) -> OpCoverage {
    OpCoverage::Messages {
        folder_id: payload.folder_id.clone(),
        uids: payload.uids.iter().copied().collect(),
        message_ids: payload.message_ids.iter().cloned().collect(),
    }
}

/// Resolve every referenced operation id, reporting any that vanished or whose
/// payload cannot be understood. Those are evidence gaps; callers must never
/// collapse them into "no operation was queued".
fn resolve_ops(db: &Connection, op_ids: &[String]) -> Result<(Vec<ResolvedOp>, bool)> {
    let mut resolved = Vec::new();
    let mut missing = false;
    for id in op_ids {
        match repo::pending_ops::get(db, id) {
            Ok(Some(op)) => match ResolvedOp::parse(&op) {
                Some(op) => resolved.push(op),
                None => missing = true,
            },
            Ok(None) | Err(_) => missing = true,
        }
    }
    Ok((resolved, missing))
}

/// Does this account queue server operations at all?
fn remote_account(db: &Connection, account_id: &str) -> Result<bool> {
    Ok(repo::accounts::get(db, account_id)?
        .map(|account| !crate::service::messages::is_local_only(account.provider))
        .unwrap_or(false))
}

/// Does this account's mail run over Graph rather than IMAP/POP3?
///
/// `gm_msgid` is overloaded: on Graph rows it holds the immutable Graph id, but
/// on Gmail IMAP rows it holds `X-GM-MSGID`. Only the Graph protocol may turn
/// it back into a server identity; treating the IMAP value as a Graph id would
/// bind a fabricated synthetic UID to a row the server never confirmed.
fn graph_account(db: &Connection, account_id: &str) -> Result<bool> {
    Ok(repo::accounts::get(db, account_id)?
        .map(|account| crate::graph::uses_graph(&account))
        .unwrap_or(false))
}

struct ProcessOutcome {
    state: UndoState,
    permanent_gone: bool,
    last_error: Option<String>,
}

struct PrepareOutcome {
    compensate: Vec<String>,
    permanent_gone: bool,
    /// The request attempt this pass started with. A compensation failure is
    /// only recorded while the request is still on this generation; a newer
    /// pass re-arms the same entries and owns their settlement.
    attempt: u32,
}

/// Run one pass of the request state machine.
async fn process_request(ctx: &CoreContext, request_id: &str) -> Result<ProcessOutcome> {
    let id = request_id.to_string();
    let prepared = match ctx.db.write(move |db| prepare_request(db, &id)).await? {
        Some(prepared) => prepared,
        None => {
            return Ok(ProcessOutcome {
                state: UndoState::Done,
                permanent_gone: false,
                last_error: None,
            });
        }
    };

    let attempt = prepared.attempt;
    for entry_id in prepared.compensate {
        let id = entry_id.clone();
        let result = ctx.db.write(move |db| compensate_entry(db, &id)).await;
        if let Err(error) = result {
            warn!(entry = %entry_id, %error, "undo compensation failed; keeping the entry retryable");
            let id = entry_id.clone();
            let reason = error.message();
            let request = request_id.to_string();
            ctx.db
                .write(move |db| {
                    record_compensation_failure(db, &id, &request, attempt, &reason)
                })
                .await?;
        }
    }

    let id = request_id.to_string();
    let mut outcome = ctx.db.write(move |db| finalize_request(db, &id)).await?;
    if prepared.permanent_gone {
        outcome.permanent_gone = true;
        // A permanently deleted message makes the undo fail, but it must not
        // force the whole request to `Failed` while other entries still wait
        // for their operation to settle (those are retryable work).
        if outcome.state == UndoState::Done {
            let id = request_id.to_string();
            ctx.db
                .write(move |db| {
                    repo::undo::update_state(db, &id, UndoState::Failed, Some("已删除，无法撤销"))?;
                    Ok(())
                })
                .await?;
            outcome.state = UndoState::Failed;
        }
    }

    emit_changes(ctx, request_id).await?;
    if let Err(error) = crate::sync::notify::update_badge(ctx).await {
        debug!(%error, "could not update unread badge after undo");
    }
    Ok(outcome)
}

/// First pass: classify every entry and settle the ones that need no async
/// server work (cancel / local restore). Compensation entries are only resolved
/// and recorded here; their inverse operations are queued by
/// [`compensate_entry`], one transaction per entry.
fn prepare_request(db: &Connection, request_id: &str) -> Result<Option<PrepareOutcome>> {
    let Some(request) = repo::undo::get_request(db, request_id)? else {
        return Ok(None);
    };
    if request.state == UndoState::Done {
        return Ok(None);
    }
    repo::undo::bump_attempt(db, request_id)?;
    let attempt = repo::undo::get_request(db, request_id)?
        .map(|request| request.attempt)
        .unwrap_or_default();
    let entries = repo::undo::list_entries(db, request_id)?;
    let mut compensate = Vec::new();
    let mut permanent_gone = false;

    for entry in &entries {
        if entry.result == UndoEntryResult::Done {
            continue;
        }
        let state: UndoEntryState = serde_json::from_value(entry.payload.clone())
            .map_err(|error| AppError::internal(format!("invalid undo entry: {error}")))?;

        // An inverse operation was already queued. The entry is not done until
        // that operation actually settles; a failed inverse stays retryable.
        if let Some(inverse_id) = entry.inverse_op_id.as_deref() {
            let Some(inverse) = repo::pending_ops::get(db, inverse_id)? else {
                if attempt >= MAX_UNDO_ATTEMPTS {
                    // The row that proved the inverse was queued vanished.
                    // Keep its reference: clearing it would let the next Undo
                    // treat the entry as never compensated and enqueue the same
                    // inverse again, duplicating a command that may have run.
                    fail_entry(db, entry, "the inverse operation disappeared")?;
                }
                continue;
            };
            match inverse.state {
                PendingOpState::Pending | PendingOpState::InProgress => {
                    // Still queued/in flight: wait for the worker.
                    continue;
                }
                PendingOpState::Done | PendingOpState::Failed | PendingOpState::Cancelled => {
                    // The row's aggregate state is not delivery evidence. A
                    // `done` inverse may have lost its first receipt and then
                    // been skipped on the retry (the writer moved on), leaving
                    // the per-message outcome `Unknown`. Settle on what the
                    // inverse actually proved for *this* message.
                    match inverse_receipt(&inverse, &entry.message_id) {
                        InverseReceipt::Confirmed => {
                            // The server applied the inverse for this exact
                            // message, even when the operation row itself
                            // settled `failed` (for example on a local commit).
                            repo::undo::update_entry_result(db, &entry.id, UndoEntryResult::Done)?;
                            continue;
                        }
                        InverseReceipt::Unknown => {
                            // The inverse may already have reached the server.
                            // Keep its reference: re-queueing a fresh inverse
                            // could apply the action twice. Never clear it and
                            // never report the entry as done.
                            if attempt >= MAX_UNDO_ATTEMPTS {
                                fail_entry(
                                    db,
                                    entry,
                                    "the inverse operation may have reached the server and cannot be retried safely",
                                )?;
                            }
                            continue;
                        }
                        InverseReceipt::NotExecuted => {
                            // Provably never sent, even if the queue row says
                            // `done`: release the claim and re-plan this entry in
                            // the same pass. The fresh claim (made by
                            // `compensate_entry`) moves it back to `pending`
                            // atomically with the enqueue. Bound the number of
                            // fresh operations: a target that keeps failing
                            // before every send must reach a visible terminal
                            // state instead of re-planning forever.
                            let replans = repo::undo::bump_entry_replans(db, &entry.id)?;
                            if replans > MAX_INVERSE_REPLANS {
                                fail_entry(
                                    db,
                                    entry,
                                    "the inverse operation could not be sent after repeated attempts",
                                )?;
                                continue;
                            }
                            // The inverse's local restore already ran, but it
                            // provably never reached the server. Hand the
                            // field back to this entry so the re-planned
                            // operation is allowed to write it.
                            let owner = state
                                .flag_writer_id
                                .clone()
                                .or_else(|| entry.op_ids.first().cloned());
                            if let Some(owner) = owner {
                                repo::messages::reassign_flag_writers(db, inverse_id, &owner)?;
                            }
                            repo::undo::release_entry_compensation(db, &entry.id, inverse_id)?;
                        }
                    }
                }
            }
        }

        let ops = load_ops(db, &entry.op_ids)?;
        let missing =
            entry.evidence_missing || (!entry.op_ids.is_empty() && ops.len() != entry.op_ids.len());
        if missing {
            if attempt >= MAX_UNDO_ATTEMPTS {
                fail_entry(
                    db,
                    entry,
                    "the queued operation evidence disappeared before its outcome was known",
                )?;
            }
            continue;
        }

        let any_in_progress = ops.iter().any(|op| op.state == PendingOpState::InProgress);
        let any_pending = ops.iter().any(|op| op.state == PendingOpState::Pending);
        let any_settled = ops
            .iter()
            .any(|op| matches!(op.state, PendingOpState::Failed | PendingOpState::Cancelled));
        let any_done = ops.iter().any(|op| op.state == PendingOpState::Done);
        let any_unknown = ops.iter().any(|op| op.send_intent || op.outcome_unknown);

        // The operation's own per-message result outranks its aggregate state.
        // A batch can have deleted/moved one message and failed on the next.
        if let Some(evidence) = per_message_evidence(&ops, &entry.message_id) {
            match evidence {
                OpMessageState::Confirmed => {
                    if entry.permanent {
                        fail_entry(
                            db,
                            entry,
                            "the permanent delete already reached the server and cannot be undone",
                        )?;
                        permanent_gone = true;
                    } else {
                        compensate.push(entry.id.clone());
                    }
                }
                OpMessageState::NotExecuted => {
                    if any_pending || any_in_progress {
                        // A retry may still execute it; wait for the settle.
                        continue;
                    }
                    restore_local(db, &request, entry, &state, false, true, entry.permanent, None)?;
                    // This message provably never reached the server, but the
                    // settled operation may still be the field's durable writer
                    // (an explicit refusal, or a skip recorded while another
                    // action owned the field). Merely restoring locally would
                    // leave that owner in place and never send the value: the
                    // server keeps the stale flag. Hand the field back to the
                    // earlier action, or queue the restored value as this
                    // entry's inverse, and settle only on the actual result.
                    let handoff =
                        hand_back_settled_flag_writers(db, entry, &state, &request.kind, &ops)?;
                    settle_after_handoff(db, entry, handoff)?;
                }
                OpMessageState::Unknown => {
                    if any_pending || any_in_progress {
                        continue;
                    }
                    if attempt >= MAX_UNDO_ATTEMPTS {
                        fail_entry(
                            db,
                            entry,
                            "the server outcome for this message could not be determined",
                        )?;
                    }
                }
            }
            continue;
        }

        if any_in_progress {
            // Rule: an in-flight operation is never cancelled; the worker
            // resumes once it settles.
            continue;
        }

        if any_done {
            if entry.permanent {
                fail_entry(
                    db,
                    entry,
                    "the permanent delete already reached the server and cannot be undone",
                )?;
                permanent_gone = true;
            } else {
                compensate.push(entry.id.clone());
            }
            continue;
        }

        // A command that was handed to the transport may already have reached
        // the server. Never turn that uncertainty back into "not sent".
        if any_unknown {
            if any_settled && attempt >= MAX_UNDO_ATTEMPTS {
                fail_entry(
                    db,
                    entry,
                    "the operation may have reached the server and its outcome is unknown",
                )?;
            }
            continue;
        }

        // A permanent delete whose `\Deleted` STORE already succeeded may have
        // removed the message even though the EXPUNGE was refused. Restoring it
        // here would unhide a row the server may no longer have and would reuse
        // the old UID; keep it pending verification instead.
        if entry.permanent && delete_side_effect(db, &entry.op_ids)? {
            fail_entry(
                db,
                entry,
                "the permanent delete partially reached the server; waiting for reconciliation",
            )?;
            continue;
        }

        if any_pending {
            let mut cancelled = true;
            let mut cancelled_op_ids = HashSet::new();
            for op in &ops {
                if op.state != PendingOpState::Pending {
                    continue;
                }
                if repo::pending_ops::cancel_if_pending(db, &op.id)? == 0 {
                    // The row was claimed between listing and cancelling.
                    cancelled = false;
                } else if !op.send_intent && !op.outcome_unknown {
                    // Provably never reached the wire: its writer row may be
                    // handed back to the previous action in the same
                    // transaction as the local restore.
                    cancelled_op_ids.insert(op.id.clone());
                }
            }
            if !cancelled {
                continue;
            }
            // No send intent was ever persisted, so restoring the snapshot
            // (including its UID/generation) is safe.
            restore_local(db, &request, entry, &state, true, false, entry.permanent, None)?;
            // The cancelled operation was the field's durable last writer.
            // Leaving it there would make the earlier still-pending action look
            // stale to the executor and silently keep the server value.
            let handoff = hand_back_cancelled_flag_writers(
                db,
                entry,
                &state,
                &request.kind,
                &cancelled_op_ids,
            )?;
            settle_after_handoff(db, entry, handoff)?;
            continue;
        }

        // No operation is left running: it either failed/was cancelled (and was
        // rolled back) or this message never had a server operation. A settled
        // batch failure that never persisted a send intent, an uncertainty flag
        // or a per-message outcome is provably unsent for this message, exactly
        // like a per-message `NotExecuted`.
        restore_local(
            db,
            &request,
            entry,
            &state,
            ops.is_empty(),
            provably_unsent(&ops, &entry.message_id),
            entry.permanent,
            None,
        )?;
        // If a settled operation that provably never wrote the field is still
        // recorded as its owner, hand it back to the earlier action.
        let cancelled_op_ids: HashSet<String> = ops
            .iter()
            .filter(|op| {
                !op.send_intent
                    && !op.outcome_unknown
                    && matches!(op.state, PendingOpState::Cancelled | PendingOpState::Failed)
            })
            .map(|op| op.id.clone())
            .collect();
        let handoff =
            hand_back_cancelled_flag_writers(db, entry, &state, &request.kind, &cancelled_op_ids)?;
        settle_after_handoff(db, entry, handoff)?;
    }

    if !compensate.is_empty() {
        // Re-arm the entries this pass will compensate. A retry by id starts
        // from entries a previous pass left `failed`; if they stay `failed`
        // while the inverses are queued one transaction at a time, the first
        // compensation error aggregates the request back to terminal `Failed`
        // (and a crash before the remaining enqueues would strand them: the
        // resume worker skips `failed`). Moving them to `pending` first keeps
        // every failure between here and the last enqueue resumable.
        let recovering: HashSet<&str> = compensate.iter().map(String::as_str).collect();
        for entry in &entries {
            if entry.result == UndoEntryResult::Failed && recovering.contains(entry.id.as_str()) {
                repo::undo::update_entry_result(db, &entry.id, UndoEntryResult::Pending)?;
            }
        }
        // The entries in `compensate` are still `pending` and have no inverse
        // id yet, so aggregating would downgrade the request to `Deferred`.
        // Keep it explicitly `Compensating` while their inverses are queued in
        // the following transactions.
        repo::undo::update_state(db, request_id, UndoState::Compensating, None)?;
    } else {
        // A hand-off in this pass may have moved an entry back to `pending`
        // and recorded its freshly queued inverse without adding it to
        // `compensate` (for example when a `Failed` request is retried by id
        // and a cancelled predecessor no longer hides the earlier matching
        // action). Aggregate in the same transaction: `failed` is terminal for
        // the resume worker, so leaving it in place would strand the entry even
        // though its inverse is already queued and may still succeed.
        refresh_request_state(db, request_id, None)?;
    }
    Ok(Some(PrepareOutcome {
        compensate,
        permanent_gone,
        attempt,
    }))
}

fn load_ops(db: &Connection, op_ids: &[String]) -> Result<Vec<PendingOp>> {
    let mut ops = Vec::new();
    for id in op_ids {
        if let Some(op) = repo::pending_ops::get(db, id)? {
            ops.push(op);
        }
    }
    Ok(ops)
}

/// The per-message outcomes a move-like operation payload carries.
///
/// `setFlags` has no per-message outcomes: its only durable evidence is the
/// operation-level `send_intent` / `outcome_unknown` pair.
fn op_outcomes(op: &PendingOp) -> Option<Vec<OpMessageOutcome>> {
    match op.kind.as_str() {
        KIND_SET_FLAGS => repo::pending_ops::payload::<SetFlagsOp>(op)
            .ok()
            .map(|payload| payload.outcomes),
        KIND_MOVE => repo::pending_ops::payload::<MoveOp>(op)
            .ok()
            .map(|payload| payload.outcomes),
        KIND_ARCHIVE => repo::pending_ops::payload::<crate::ops::ArchiveOp>(op)
            .ok()
            .map(|payload| payload.outcomes),
        KIND_MARK_JUNK => repo::pending_ops::payload::<MarkJunkOp>(op)
            .ok()
            .map(|payload| payload.outcomes),
        KIND_DELETE => repo::pending_ops::payload::<DeleteOp>(op)
            .ok()
            .map(|payload| payload.outcomes),
        _ => None,
    }
}

/// The per-message result recorded by any covering operation.
fn per_message_evidence(ops: &[PendingOp], message_id: &str) -> Option<OpMessageState> {
    for op in ops {
        let Some(outcomes) = op_outcomes(op) else {
            continue;
        };
        if let Some(outcome) = outcomes
            .iter()
            .find(|outcome| outcome.message_id == message_id)
        {
            return Some(outcome.state);
        }
    }
    None
}

/// Do the covering operations prove this message's command never left the
/// client, even without a per-message outcome?
///
/// A Graph batch fails before the transport when a target folder mapping is
/// missing, and a retry can exhaust before any per-message outcome exists. Such
/// a settled failure has no `send_intent`, no `outcome_unknown` and no outcome
/// for this message, so it is the same proof as a per-message `NotExecuted`:
/// the copy in the source folder still carries the snapshot identity. Once any
/// operation persisted a send intent or raised the uncertainty flag the command
/// may have reached the server, and restoring the identity would be a guess.
fn provably_unsent(ops: &[PendingOp], message_id: &str) -> bool {
    !ops.is_empty()
        && !ops.iter().any(|op| op.send_intent || op.outcome_unknown)
        && per_message_evidence(ops, message_id).is_none()
}

/// Did a later queued operation write the flag field this undo would revert?
///
/// Three actions "read -> unread -> read" leave the current value equal to the
/// first action's value. Undoing the first by id must still not write it: the
/// third action is the field's last writer, and both the local rollback and
/// the inverse STORE would silently overwrite it. Values alone cannot tell the
/// writers apart, so compare operation order (`pending_op.seq`) instead.
///
/// A cancelled operation never reached the transport, and a failed one with no
/// send intent provably did not either; neither can own the field. Pending,
/// in-progress, done and uncertain operations may still write it.
fn flag_field_written_later(
    db: &Connection,
    account_id: &str,
    entry: &UndoRequestEntry,
    state: &UndoEntryState,
    kind: &str,
) -> Result<bool> {
    let touches = |payload: &SetFlagsOp| match kind {
        "setSeen" => payload.seen.is_some(),
        "setFlagged" => payload.flagged.is_some(),
        _ => false,
    };
    let field = match kind {
        "setSeen" => "seen",
        "setFlagged" => "flagged",
        _ => return Ok(false),
    };
    // The durable per-field writer is authoritative: an action that ran after
    // this one owns the field even when the current value happens to equal the
    // value this action wrote. This also covers local/POP3 actions, which have
    // no queued operation to compare sequence numbers with.
    if let Some(writer) = repo::messages::flag_writer(db, &entry.message_id, field)? {
        let owns = entry.op_ids.iter().any(|op_id| op_id == &writer.owner_id)
            || state.flag_writer_id.as_deref() == Some(writer.owner_id.as_str());
        return Ok(!owns);
    }
    // Payloads queued before the writer table existed have no row: fall back
    // to the operation-order scan below.
    if !matches!(kind, "setSeen" | "setFlagged") {
        return Ok(false);
    }
    // The newest operation this undo actually owns. Operations with a larger
    // sequence were queued after it and therefore write the field last.
    let own = load_ops(db, &entry.op_ids)?;
    let Some(baseline) = own.iter().map(|op| op.sequence).max() else {
        return Ok(false);
    };
    // This entry's own inverse is the compensation being performed, not a
    // later user action. The database row is authoritative: the caller may
    // hold a snapshot taken before it claimed the inverse slot.
    let own_inverse = repo::undo::get_entry(db, &entry.id)?.and_then(|row| row.inverse_op_id);
    for op in repo::pending_ops::list(db, Some(account_id))? {
        if op.sequence <= baseline || op.kind != KIND_SET_FLAGS {
            continue;
        }
        if own_inverse.as_deref() == Some(op.id.as_str()) {
            continue;
        }
        // A cancelled or failed operation that never persisted a send intent
        // provably never wrote anything. Once a send intent exists the command
        // may have reached the server (and the local projection), so it counts
        // as a writer even if the row later settled as cancelled/failed.
        let provably_unsent = !op.send_intent && !op.outcome_unknown;
        if provably_unsent && matches!(op.state, PendingOpState::Cancelled | PendingOpState::Failed)
        {
            continue;
        }
        let Ok(payload) = repo::pending_ops::payload::<SetFlagsOp>(&op) else {
            continue;
        };
        let covers = payload
            .message_ids
            .iter()
            .any(|message_id| message_id == &entry.message_id)
            || (payload.folder_id == entry.original_folder_id
                && state
                    .uid
                    .map(|uid| payload.uids.contains(&uid))
                    .unwrap_or(false));
        if covers && touches(&payload) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The newest confirmed per-message receipt across every covering operation.
///
/// The undo entry snapshots the target identity when it is created, but a
/// deferred entry may be classified *before* its operation reports the
/// destination (`COPYUID` / a returned Graph id). Compensation must therefore
/// re-read the durable outcomes instead of trusting that initial snapshot.
fn per_message_confirmed_outcome(ops: &[PendingOp], message_id: &str) -> Option<OpMessageOutcome> {
    let mut latest = None;
    for op in ops {
        let Some(outcomes) = op_outcomes(op) else {
            continue;
        };
        if let Some(outcome) = outcomes.into_iter().find(|outcome| {
            outcome.message_id == message_id && outcome.state == OpMessageState::Confirmed
        }) {
            latest = Some(outcome);
        }
    }
    latest
}

/// What a finished inverse operation proved about one entry.
enum InverseReceipt {
    /// The server applied the inverse for this message.
    Confirmed,
    /// The command provably never left the client.
    NotExecuted,
    /// It may have reached the server; a retry must not duplicate it.
    Unknown,
}

/// Classify a settled inverse operation (including `done`) without clearing
/// evidence. The queue's aggregate state is never treated as delivery proof:
/// only a per-message `Confirmed` receipt settles the entry.
fn inverse_receipt(inverse: &PendingOp, message_id: &str) -> InverseReceipt {
    if let Some(outcomes) = op_outcomes(inverse) {
        if let Some(outcome) = outcomes
            .iter()
            .find(|outcome| outcome.message_id == message_id)
        {
            return match outcome.state {
                OpMessageState::Confirmed => InverseReceipt::Confirmed,
                OpMessageState::NotExecuted => InverseReceipt::NotExecuted,
                OpMessageState::Unknown => InverseReceipt::Unknown,
            };
        }
    }
    if inverse.send_intent || inverse.outcome_unknown {
        // No per-message outcome exists (or it was never written), but a wire
        // command may have been issued.
        InverseReceipt::Unknown
    } else {
        InverseReceipt::NotExecuted
    }
}

/// The field a flag undo touches and the value that action wrote.
fn undoing_flag_field(kind: &str) -> Option<&'static str> {
    match kind {
        "setSeen" => Some("seen"),
        "setFlagged" => Some("flagged"),
        _ => None,
    }
}

/// What an earlier flag action proves about the value this undo restored.
///
/// Only `Delivered` may receive the field back. `NotDelivered` and `Uncertain`
/// both require a fresh tracked command: the earlier row is settled and will
/// never execute again, so handing it the writer would silently lose the
/// write. `Waiting` defers until the earlier row settles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreviousDelivery {
    /// The action is still queued (it will run and see the handed-back
    /// writer), or it already wrote the value (`Confirmed`, or a `done` row
    /// whose command provably reached the server).
    Delivered,
    /// The action settled without writing, or was provably skipped for this
    /// message. It will never run again.
    NotDelivered,
    /// The action settled but its command may have reached the server
    /// (`Unknown`). A terminal `Unknown` is not proof of delivery and the row
    /// will not run again, so the restored value must be re-sent as a tracked
    /// inverse (absolute flag writes are idempotent).
    Uncertain,
    /// The action is queued or in flight without a definite per-message
    /// result. The hand-back must wait: it may already have decided to skip
    /// because the newer action owned the field.
    Waiting,
}

/// Result of transferring a flag field away from an operation this undo
/// cancelled.
///
/// The caller may record the entry as `Done` only for `Settled` and
/// `NothingToSend`. `Blocked` means a fresh tracked inverse is queued and has
/// not settled yet: the entry must stay open and the cancelled writer (or its
/// replacement) must keep blocking older operations from writing a stale copy.
/// `Refused` is the deterministic counterpart: the value cannot be handed back
/// or re-sent at all (identity/value mismatch, missing provider identity,
/// unclassifiable account). The writer must stay in place just the same, but the
/// entry becomes a visible `Failed` instead of a `Deferred` request that would
/// block every later shortcut undo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriterHandoff {
    /// Ownership moved to an action that will write the field, or the write
    /// provably already happened.
    Settled,
    /// No server command is needed: the local row is already the value the
    /// undo restored (local-only row, no writer, or a later live owner).
    NothingToSend,
    /// The restored value was queued as a tracked inverse and has not settled
    /// yet. The entry must not settle.
    Blocked,
    /// The value can never be handed back or re-sent safely. The entry must
    /// become visible `Failed`; the blocking writer stays in place.
    Refused,
}

impl WriterHandoff {
    /// Whether the entry may be recorded as `Done`.
    fn may_settle(self) -> bool {
        matches!(self, Self::Settled | Self::NothingToSend)
    }
}

/// Apply a writer hand-off result to the entry.
///
/// `Refused` deliberately keeps the cancelled operation as the field's writer
/// (nothing here deletes or reassigns it), so an older queued action still
/// cannot write a stale copy. Recording the entry `Failed` only makes the
/// request terminal; it must never be read as "the value was written".
fn settle_after_handoff(
    db: &Connection,
    entry: &UndoRequestEntry,
    handoff: WriterHandoff,
) -> Result<()> {
    if handoff.may_settle() {
        repo::undo::update_entry_result(db, &entry.id, UndoEntryResult::Done)?;
    } else if handoff == WriterHandoff::Refused {
        fail_entry(
            db,
            entry,
            "the restored flag could not be written safely: the earlier action addressed a different copy or value",
        )?;
    }
    Ok(())
}

/// The newest *earlier* operation that still owns `field` for this message.
///
/// Used when a never-sent operation is cancelled: the field must go back to
/// the action before it, not to the cancelled one. Otherwise the executor
/// would skip that earlier operation as "not the owner" and the server would
/// keep the stale value after the queue drains.
///
/// A `done` operation is not automatically a delivered one: it may have
/// settled after the executor skipped it because a later action had taken the
/// field over. The per-message `NotExecuted` evidence distinguishes that case;
/// handing ownership back to such a row would leave the field unwritten
/// forever because the row will never run again.
///
/// A terminal `Unknown` is not proof of delivery either. The row will never
/// run again, and its command may or may not have reached the server, so
/// ownership must not be handed back to it; the restored absolute flag value
/// is re-queued instead.
struct PreviousFlagOwner {
    op_id: String,
    /// Value this earlier action wrote (and that the undo restored locally).
    value: Option<bool>,
    action: String,
    /// What this earlier action proves about the value the undo restored.
    delivery: PreviousDelivery,
    /// Provider identity this earlier action was queued against. It is taken
    /// from that action's payload (or a per-message receipt it recorded), never
    /// from the row's current binding: a sync may have rebound the row to
    /// another copy between the action and the undo.
    ///
    /// `target_folder` is part of that identity: a UID and its generation are
    /// only meaningful inside the folder the earlier action addressed.
    target_folder: Option<String>,
    target_uid: Option<i64>,
    target_uidvalidity: Option<i64>,
    target_graph_id: Option<String>,
}

fn previous_flag_owner(
    db: &Connection,
    entry: &UndoRequestEntry,
    state: &UndoEntryState,
    field: &str,
) -> Result<Option<PreviousFlagOwner>> {
    let own = load_ops(db, &entry.op_ids)?;
    let Some(baseline) = own.iter().map(|op| op.sequence).min() else {
        return Ok(None);
    };
    let touches = |payload: &SetFlagsOp| match field {
        "seen" => payload.seen,
        "flagged" => payload.flagged,
        _ => None,
    };
    let action = match field {
        "seen" => "setSeen",
        "flagged" => "setFlagged",
        other => other,
    };
    let mut best: Option<(i64, PreviousFlagOwner)> = None;
    for op in repo::pending_ops::list(db, Some(&entry.account_id))? {
        if op.sequence >= baseline || op.kind != KIND_SET_FLAGS {
            continue;
        }
        if entry.op_ids.iter().any(|id| id == &op.id) {
            continue;
        }
        // A cancelled/failed operation that never persisted a send intent
        // provably wrote nothing; it cannot be the effective previous owner.
        let provably_unsent = !op.send_intent && !op.outcome_unknown;
        if provably_unsent && matches!(op.state, PendingOpState::Cancelled | PendingOpState::Failed)
        {
            continue;
        }
        let Ok(payload) = repo::pending_ops::payload::<SetFlagsOp>(&op) else {
            continue;
        };
        let Some(value) = touches(&payload) else {
            continue;
        };
        let covers = payload
            .message_ids
            .iter()
            .any(|message_id| message_id == &entry.message_id)
            || (payload.folder_id == entry.original_folder_id
                && state
                    .uid
                    .map(|uid| payload.uids.contains(&uid))
                    .unwrap_or(false));
        if !covers {
            continue;
        }
        // `NotExecuted` is definitive: this message was skipped, so this
        // operation cannot write the restored value — even while the operation
        // row is still `inProgress` and has not settled yet.
        let per_message = payload
            .outcomes
            .iter()
            .find(|outcome| outcome.message_id == entry.message_id)
            .map(|outcome| outcome.state);
        let delivery = match per_message {
            Some(OpMessageState::NotExecuted) => PreviousDelivery::NotDelivered,
            Some(OpMessageState::Confirmed) => PreviousDelivery::Delivered,
            // A terminal `Unknown` is *not* proof that the value reached the
            // server. While the row is still queued/in flight it may run and
            // settle; once it has settled it will never run again, so the
            // restored value must be re-sent as a tracked inverse instead of
            // handing the field back to a dead row.
            Some(OpMessageState::Unknown) => match op.state {
                PendingOpState::Pending | PendingOpState::InProgress => PreviousDelivery::Waiting,
                PendingOpState::Done | PendingOpState::Failed | PendingOpState::Cancelled => {
                    PreviousDelivery::Uncertain
                }
            },
            None => match op.state {
                // A queued action has not run yet, so it will observe the
                // writer this hand-back leaves behind.
                PendingOpState::Pending => PreviousDelivery::Delivered,
                // In flight with no per-message result. A batch-level send
                // intent only proves *some* message of the batch may have been
                // written (a Graph batch PATCHes one message at a time); it
                // says nothing about this one. The executor may still be
                // between deciding to skip this message and recording
                // `NotExecuted`, so handing ownership back now could settle the
                // entry as `Done` while the value is never written. Wait.
                PendingOpState::InProgress => PreviousDelivery::Waiting,
                // The operation settled. `done` after every target was skipped
                // is distinguishable by the per-message outcomes above; absent
                // one, a persisted send intent is the only evidence it ran. An
                // uncertain outcome is explicitly *not* evidence of delivery.
                PendingOpState::Done => {
                    if op.outcome_unknown {
                        PreviousDelivery::Uncertain
                    } else if op.send_intent {
                        PreviousDelivery::Delivered
                    } else {
                        PreviousDelivery::NotDelivered
                    }
                }
                PendingOpState::Failed | PendingOpState::Cancelled => {
                    if op.outcome_unknown {
                        PreviousDelivery::Uncertain
                    } else {
                        PreviousDelivery::NotDelivered
                    }
                }
            },
        };
        // Locate this message inside the earlier payload so the replacement
        // command can address the copy that action selected. `message_ids` is
        // aligned with `uids`/`remote_ids`; legacy payloads without it can only
        // be aligned by the entry's own snapshot UID within the same folder.
        let index = payload
            .message_ids
            .iter()
            .position(|message_id| message_id == &entry.message_id)
            .or_else(|| {
                if payload.folder_id != entry.original_folder_id {
                    return None;
                }
                let uid = entry.target_uid?;
                payload.uids.iter().position(|value| *value == uid)
            });
        let target_folder = index.map(|_| payload.folder_id.clone());
        let target_uid = index.and_then(|index| payload.uids.get(index)).copied();
        let target_uidvalidity = payload.source_uidvalidity;
        let target_graph_id = index
            .and_then(|index| payload.remote_ids.get(index))
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                payload
                    .outcomes
                    .iter()
                    .find(|outcome| outcome.message_id == entry.message_id)
                    .and_then(|outcome| outcome.target_remote_id.as_deref())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            });
        let candidate = PreviousFlagOwner {
            op_id: op.id.clone(),
            value: Some(value),
            action: action.to_string(),
            delivery,
            target_folder,
            target_uid,
            target_uidvalidity,
            target_graph_id,
        };
        if best
            .as_ref()
            .is_none_or(|(sequence, _)| op.sequence > *sequence)
        {
            best = Some((op.sequence, candidate));
        }
    }
    Ok(best.map(|(_, owner)| owner))
}

/// May this earlier action's identity carry the value this undo restored?
///
/// `state` is the snapshot of the action being undone; `previous` comes from
/// the earlier action's own payload. A move or a provider rebind between them
/// changes the complete identity (account + folder + UID + generation, or the
/// Graph provider id), and pairing the mismatched halves could change an
/// unrelated message. The earlier action must also have written exactly the
/// value the undo restored; otherwise giving it ownership would let its stale
/// payload win over the state the user reverted to.
///
/// Used both when re-queueing for a settled action and when handing ownership
/// back to a still-pending one: the latter has no new payload, but authorising
/// its PATCH is a remote side effect all the same.
fn previous_flag_matches(
    entry: &UndoRequestEntry,
    state: &UndoEntryState,
    previous: &PreviousFlagOwner,
    restored: bool,
    graph: bool,
) -> bool {
    let same_copy = if graph {
        let current = state
            .snapshot
            .gm_msgid
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        match (current, previous.target_graph_id.as_deref()) {
            (Some(current), Some(earlier)) => current == earlier,
            _ => false,
        }
    } else {
        previous.target_folder.as_deref() == Some(entry.original_folder_id.as_str())
            && previous.target_uid == state.uid
            && previous.target_uidvalidity == state.snapshot.uidvalidity
    };
    same_copy && previous.value == Some(restored)
}

/// Queue the intent an earlier, never-delivered action was meant to write.
///
/// Handing the field back to that action is not enough: it is already `done`
/// (skipped when a newer action took ownership) or its outcome is terminal but
/// unknown, so the executor will never send it. A fresh operation carrying the
/// value the undo restored keeps the server and the local row in agreement.
///
/// Returns [`WriterHandoff::Blocked`] when the replacement was queued (the
/// entry settles only when that tracked inverse does) and
/// [`WriterHandoff::Refused`] when the re-queue is refused (no usable provider
/// identity, or the two actions addressed a different copy/value). On every
/// path reachable for a flag field it never returns
/// `Settled`/`NothingToSend`, so the caller must not record the entry as `Done`
/// on this path.
///
/// Rejecting the re-queue must *not* delete the writer row. The earlier action
/// is still queued, and the executor's ownership check treats a missing writer
/// as a legacy payload it may send; only a writer owned by another operation
/// blocks it. The cancelled operation therefore stays as the blocking owner,
/// exactly like the hand-back rejection path. This path changes no flag value,
/// so it does not need to move the revision either.
fn requeue_restored_flag(
    db: &Connection,
    entry: &UndoRequestEntry,
    state: &UndoEntryState,
    field: &str,
    from_owner: &str,
    previous: &PreviousFlagOwner,
) -> Result<WriterHandoff> {
    // Keep the cancelled operation as the field's writer. It will never run,
    // but its ownership is what stops the still-queued earlier action from
    // PATCHing a copy this undo refused to address.
    let keep_blocker = |_db: &Connection| -> Result<WriterHandoff> {
        debug!(
            message = %entry.message_id,
            field,
            owner = %from_owner,
            "undo: keeping the cancelled writer as the blocking owner"
        );
        Ok(WriterHandoff::Refused)
    };
    let restored = match field {
        "seen" => state.seen,
        "flagged" => state.flagged,
        _ => return Ok(WriterHandoff::NothingToSend),
    };
    let (seen, flagged) = match field {
        "seen" => (Some(restored), None),
        "flagged" => (None, Some(restored)),
        _ => return Ok(WriterHandoff::NothingToSend),
    };
    let Some(account) = repo::accounts::get(db, &entry.account_id)? else {
        return keep_blocker(db);
    };
    let graph = crate::graph::uses_graph(&account);
    // Before one action's identity may carry the other's value, confirm both
    // actions addressed the same server copy and wrote the same value.
    if !previous_flag_matches(entry, state, previous, restored, graph) {
        debug!(
            message = %entry.message_id,
            field,
            "undo: the earlier action addressed a different copy or value; not re-sending"
        );
        return keep_blocker(db);
    }
    let mut payload = SetFlagsOp {
        // Every part of the address comes from the earlier action: the folder
        // it was queued in and the UID/generation that belong to that folder.
        // The row's current binding may already point at another copy.
        folder_id: previous
            .target_folder
            .clone()
            .unwrap_or_else(|| entry.original_folder_id.clone()),
        uids: Vec::new(),
        seen,
        flagged,
        answered: None,
        forwarded: None,
        source_uidvalidity: None,
        // The local row already holds the restored value; this command does
        // not change it optimistically, so there is nothing to roll back.
        previous: Vec::new(),
        message_ids: vec![entry.message_id.clone()],
        outcomes: Vec::new(),
        remote_ids: Vec::new(),
    };
    if graph {
        // Graph has no UID generation: only a provider id pinned by that
        // action (or a receipt it recorded) can address the copy. Without one
        // there is nothing attributable to send.
        let Some(remote) = previous.target_graph_id.as_deref() else {
            return keep_blocker(db);
        };
        let uid = previous
            .target_uid
            .unwrap_or_else(|| crate::sync::messages::synthetic_uid(remote));
        payload.uids = vec![uid];
        payload.remote_ids = vec![remote.to_string()];
    } else {
        // IMAP needs both the UID and the generation the earlier action was
        // queued in. A missing generation means the number may address another
        // message: do not invent one.
        let (Some(uid), Some(uidvalidity)) = (previous.target_uid, previous.target_uidvalidity)
        else {
            return keep_blocker(db);
        };
        payload.uids = vec![uid];
        payload.source_uidvalidity = Some(uidvalidity);
    }
    let op = repo::pending_ops::enqueue(
        db,
        NewPendingOp::new(
            entry.account_id.clone(),
            KIND_SET_FLAGS,
            serde_json::to_value(payload).map_err(|error| {
                AppError::internal(format!("cannot encode undo flag restore: {error}"))
            })?,
            "Undo: restore flag state",
        ),
    )?;
    repo::messages::reassign_flag_writer(
        db,
        &entry.message_id,
        field,
        from_owner,
        Some(&op.id),
        Some(restored),
        Some(&previous.action),
    )?;
    // Tie the replacement to this undo entry so its outcome — including a
    // failure after the user was told "undone" — still decides the result.
    let claimed = repo::undo::claim_entry_compensation(db, &entry.id, &op.id)?;
    debug!(
        message = %entry.message_id,
        field,
        owner = %op.id,
        claimed,
        "undo: re-queued a flag that the previous action never delivered"
    );
    // A zero claim means another pass already associated an inverse; keep the
    // entry pending either way rather than reporting a premature `done`.
    Ok(WriterHandoff::Blocked)
}

/// Hand a flag field back from the operations this undo just cancelled.
///
/// Runs in the same transaction as the cancellation and the local restore, so
/// no executor pass can observe "cancelled writer, restored value, no owner".
///
/// The result is deliberately four-state: only `Settled` (a live writer owns
/// the field) and `NothingToSend` (the local row is already correct and no
/// server command is attributable) let the entry settle as `done`. `Blocked`
/// covers a value queued as a tracked inverse or a predecessor still in flight;
/// the entry must stay open and the blocking writer must remain in place.
/// `Refused` covers every deterministic refusal to send a stale copy/value: the
/// blocking writer stays in place too, but the entry must become a visible
/// `Failed` so it cannot defer the request forever.
fn hand_back_cancelled_flag_writers(
    db: &Connection,
    entry: &UndoRequestEntry,
    state: &UndoEntryState,
    kind: &str,
    cancelled_op_ids: &HashSet<String>,
) -> Result<WriterHandoff> {
    let Some(field) = undoing_flag_field(kind) else {
        return Ok(WriterHandoff::NothingToSend);
    };
    let Some(writer) = repo::messages::flag_writer(db, &entry.message_id, field)? else {
        return Ok(WriterHandoff::NothingToSend);
    };
    if !cancelled_op_ids.contains(&writer.owner_id) {
        // A different (live) action owns the field, so this undo has nothing
        // to send for it. The local restore already skipped the field.
        return Ok(WriterHandoff::NothingToSend);
    }
    let previous = previous_flag_owner(db, entry, state, field)?;
    match previous {
        // The earlier action will still run (or already delivered its value):
        // it may own the field again, but only if it addressed the same copy
        // and wrote the value this undo restored. Handing ownership to a stale
        // payload is still a remote side effect: the executor treats the owner
        // as authorisation and PATCHes the copy this undo is not reverting.
        Some(previous) if previous.delivery == PreviousDelivery::Delivered => {
            let restored = match field {
                "seen" => state.seen,
                "flagged" => state.flagged,
                _ => return Ok(WriterHandoff::NothingToSend),
            };
            // Without the account the provider identity cannot be classified;
            // do not authorise a write on a guess. Keep the cancelled writer
            // as the blocker, but this is deterministic: the entry must fail
            // visibly instead of deferring the request forever.
            let Some(account) = repo::accounts::get(db, &entry.account_id)? else {
                return Ok(WriterHandoff::Refused);
            };
            let graph = crate::graph::uses_graph(&account);
            if !previous_flag_matches(entry, state, &previous, restored, graph) {
                debug!(
                    message = %entry.message_id,
                    field,
                    "undo: the earlier action addressed a different copy or value; not handing the flag back"
                );
                // Keep the cancelled writer in place. Clearing it would fall
                // back to the executor's legacy "no writer row" check and let
                // the earlier action write the stale copy anyway. The mismatch
                // is deterministic (both identities are immutable), so report
                // `Refused`: the caller records a visible `Failed` while the
                // writer keeps blocking the stale copy.
                return Ok(WriterHandoff::Refused);
            }
            repo::messages::reassign_flag_writer(
                db,
                &entry.message_id,
                field,
                &writer.owner_id,
                Some(&previous.op_id),
                previous.value,
                Some(&previous.action),
            )?;
            debug!(
                message = %entry.message_id,
                field,
                owner = %previous.op_id,
                "undo: handed the flag field back to the previous writer"
            );
            Ok(WriterHandoff::Settled)
        }
        // The earlier action settled without ever sending, was skipped, or may
        // have sent an `Unknown` command. In every case it will never run
        // again, so handing it the field would leave the server at the stale
        // value. Queue the restored value as this entry's tracked inverse. A
        // refusal to re-queue keeps the cancelled writer as the blocker and
        // records a visible `Failed` entry.
        Some(previous)
            if matches!(
                previous.delivery,
                PreviousDelivery::NotDelivered | PreviousDelivery::Uncertain
            ) =>
        {
            requeue_restored_flag(db, entry, state, field, &writer.owner_id, &previous)
        }
        // The earlier action is queued/in flight and has not produced a
        // definite per-message result yet. It may already have skipped because
        // this (newer) action owned the field, so handing ownership back now
        // could lose the write. Leave the cancelled writer in place and let
        // the next pass decide once it settles.
        Some(_) => {
            // The local restore already changed the value, but ownership stays
            // on the cancelled writer while we wait. Bump the revision in
            // place: an automatic mark-as-read that started loading the body
            // must see that this value is not the one it observed.
            repo::messages::reassign_flag_writer(
                db,
                &entry.message_id,
                field,
                &writer.owner_id,
                Some(&writer.owner_id),
                None,
                None,
            )?;
            debug!(
                message = %entry.message_id,
                field,
                "undo: waiting for the previous action to settle before handing the flag back"
            );
            Ok(WriterHandoff::Blocked)
        }
        None => {
            // No earlier action owns this field at all: the local restore is
            // the whole fix and there is nothing to send.
            repo::messages::reassign_flag_writer(
                db,
                &entry.message_id,
                field,
                &writer.owner_id,
                None,
                None,
                None,
            )?;
            Ok(WriterHandoff::NothingToSend)
        }
    }
}

/// Hand a flag field back from settled operations that provably did not write
/// this message.
///
/// A per-message `NotExecuted` may belong to a batch operation that is `done`
/// as a whole; the per-message evidence still proves this row's write never
/// happened. Cancelled/failed rows without a send intent are equally provable.
fn hand_back_settled_flag_writers(
    db: &Connection,
    entry: &UndoRequestEntry,
    state: &UndoEntryState,
    kind: &str,
    ops: &[PendingOp],
) -> Result<WriterHandoff> {
    let ids: HashSet<String> = ops
        .iter()
        .filter(|op| {
            let per_message_not_executed = op_outcomes(op).is_some_and(|outcomes| {
                outcomes.iter().any(|outcome| {
                    outcome.message_id == entry.message_id
                        && outcome.state == OpMessageState::NotExecuted
                })
            });
            per_message_not_executed
                || (!op.send_intent
                    && !op.outcome_unknown
                    && matches!(op.state, PendingOpState::Cancelled | PendingOpState::Failed))
        })
        .map(|op| op.id.clone())
        .collect();
    hand_back_cancelled_flag_writers(db, entry, state, kind, &ids)
}

/// Recompute and persist the request state from all of its entries.
///
/// `failure_reason` is the reason an entry just failed in this transaction.
/// Pending entries stay in charge: while any entry still waits for an operation
/// or an inverse receipt, the request remains `Deferred` / `Compensating`
/// instead of being committed `Failed`. `failed` is terminal for the resume
/// worker ([`repo::undo::resumable_requests`]), so committing it early would
/// strand a mixed request after a restart: its queued inverse may still finish,
/// but no worker pass would ever settle the entry.
fn refresh_request_state(
    db: &Connection,
    request_id: &str,
    failure_reason: Option<&str>,
) -> Result<Option<(UndoState, Option<String>)>> {
    let Some(request) = repo::undo::get_request(db, request_id)? else {
        return Ok(None);
    };
    let entries = repo::undo::list_entries(db, request_id)?;
    let mut pending = false;
    let mut compensating = false;
    let mut failed = false;
    for entry in &entries {
        match entry.result {
            UndoEntryResult::Done => {}
            UndoEntryResult::Failed => failed = true,
            UndoEntryResult::Pending => {
                // A pending entry still waits for an operation to settle, or
                // its compensation pass did not complete. Either way the worker
                // retries it; it must not make the request `Done` or `Failed`.
                pending = true;
                if entry.inverse_op_id.is_some() {
                    // The inverse is queued but has not settled yet: the undo
                    // is compensating, not complete.
                    compensating = true;
                }
            }
        }
    }

    let state = if pending {
        if compensating {
            UndoState::Compensating
        } else {
            UndoState::Deferred
        }
    } else if failed {
        UndoState::Failed
    } else {
        UndoState::Done
    };
    let error = if state == UndoState::Failed {
        failure_reason
            .map(str::to_string)
            .or_else(|| request.last_error.clone())
            .or_else(|| Some("could not undo every message; retry later".to_string()))
    } else {
        None
    };
    repo::undo::update_state(db, request_id, state, error.as_deref())?;
    Ok(Some((state, error)))
}

/// Record that [`compensate_entry`] failed, without ever overwriting a newer
/// pass that recovered the same entry.
///
/// Compensation runs in its own transaction, so its error only proves that
/// *this* attempt failed. Between that rollback and this settlement another
/// worker pass may have observed the now-resolved evidence, claimed the entry
/// and queued its inverse (`inverse_op_id` set, result `pending`). Marking it
/// `failed` here would discard that claim; once the remaining entries settle,
/// the parent request becomes terminal `failed`, the resume worker skips it,
/// and even a successful inverse never settles the entry.
///
/// Both checks run inside the settling transaction (which serialises with the
/// competing pass via the write lock), so exactly one side wins:
///
/// * a committed inverse claim means the new pass owns the entry;
/// * a bumped [`UndoRequest::attempt`] means a newer pass re-armed it
///   (`prepare_request`) and will classify/settle it;
/// * otherwise this failure is still the newest knowledge and is recorded.
fn record_compensation_failure(
    db: &Connection,
    entry_id: &str,
    request_id: &str,
    expected_attempt: u32,
    reason: &str,
) -> Result<()> {
    let Some(entry) = repo::undo::get_entry(db, entry_id)? else {
        return Ok(());
    };
    if entry.result == UndoEntryResult::Done {
        return Ok(());
    }
    if entry.inverse_op_id.is_some() {
        debug!(
            entry = %entry_id,
            "undo compensation recovered concurrently; not marking the entry failed"
        );
        return Ok(());
    }
    let Some(request) = repo::undo::get_request(db, request_id)? else {
        return Ok(());
    };
    if request.attempt != expected_attempt {
        debug!(
            entry = %entry_id,
            attempt = request.attempt,
            expected_attempt,
            "undo compensation attempt was superseded; not marking the entry failed"
        );
        return Ok(());
    }
    repo::undo::update_entry_result(db, entry_id, UndoEntryResult::Failed)?;
    // Aggregate in the same transaction as the entry update: a mixed request
    // must stay resumable if the process exits before `finalize_request` runs.
    refresh_request_state(db, request_id, Some(reason))?;
    Ok(())
}

fn fail_entry(db: &Connection, entry: &UndoRequestEntry, reason: &str) -> Result<()> {
    repo::undo::update_entry_result(db, &entry.id, UndoEntryResult::Failed)?;
    // The request state is aggregated in the same transaction as the entry
    // update. A mixed request must not be committed `Failed` while another
    // entry is still unsettled, or a restart would never resume it.
    refresh_request_state(db, &entry.request_id, Some(reason))?;
    Ok(())
}

/// Did an IMAP `+FLAGS \Deleted` already reach the server for this entry?
fn delete_side_effect(db: &Connection, op_ids: &[String]) -> Result<bool> {
    for id in op_ids {
        let Some(op) = repo::pending_ops::get(db, id)? else {
            continue;
        };
        if op.kind != KIND_DELETE {
            continue;
        }
        if let Ok(payload) = repo::pending_ops::payload::<DeleteOp>(&op) {
            if payload.deleted_flag_set {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Queue the inverse operation for one entry and restore its local snapshot.
///
/// The claim on the entry and the operation insert share one transaction: if
/// the insert fails nothing is claimed, so a retry can try again; once it
/// commits, `inverse_op_id` proves no second inverse may be queued.
fn compensate_entry(db: &Connection, entry_id: &str) -> Result<()> {
    let Some(entry) = repo::undo::get_entry(db, entry_id)? else {
        return Ok(());
    };
    if entry.result == UndoEntryResult::Done {
        return Ok(());
    }
    // Already queued: its settlement is consumed by `prepare_request`.
    if entry.inverse_op_id.is_some() {
        return Ok(());
    }
    let Some(request) = repo::undo::get_request(db, &entry.request_id)? else {
        return Ok(());
    };
    let state: UndoEntryState = serde_json::from_value(entry.payload.clone())
        .map_err(|error| AppError::internal(format!("invalid undo entry: {error}")))?;

    // Re-check the evidence inside the compensation transaction: an operation
    // that was `done` during classification must not be compensated if it has
    // been re-queued/reopened since.
    let ops = load_ops(db, &entry.op_ids)?;
    if entry.evidence_missing || (!entry.op_ids.is_empty() && ops.len() != entry.op_ids.len()) {
        return Err(AppError::conflict(
            "the original operation evidence is missing; retry after the next sync",
        ));
    }
    if ops.iter().any(|op| {
        matches!(
            op.state,
            PendingOpState::Pending | PendingOpState::InProgress
        )
    }) {
        return Ok(());
    }

    // A later flag action owns this field. Undoing the older one must not queue
    // an inverse STORE for it, even when the current value happens to equal the
    // value the older action wrote.
    if flag_field_written_later(db, &entry.account_id, &entry, &state, &request.kind)? {
        repo::undo::update_entry_result(db, &entry.id, UndoEntryResult::Done)?;
        return Ok(());
    }

    // The entry's identity was captured when the undo started. If a covering
    // operation reported a better receipt since (a COPYUID destination or a
    // returned Graph id), use it here, inside the same transaction that builds
    // and queues the inverse. Otherwise a deferred undo keeps failing on a
    // stale empty target.
    let mut resolved = entry.clone();
    if let Some(outcome) = per_message_confirmed_outcome(&ops, &entry.message_id) {
        if outcome.target_folder_id.is_some() {
            resolved.target_folder_id = outcome.target_folder_id.clone();
        }
        if outcome.target_uid.is_some() {
            resolved.target_uid = outcome.target_uid;
        }
        if outcome.target_uidvalidity.is_some() {
            resolved.target_uidvalidity = outcome.target_uidvalidity;
        }
        if outcome.target_remote_id.is_some() {
            resolved.target_graph_id = outcome.target_remote_id.clone();
        }
        repo::undo::update_entry_identity(
            db,
            &entry.id,
            resolved.target_folder_id.as_deref(),
            resolved.target_uid,
            resolved.target_uidvalidity,
            resolved.target_graph_id.as_deref(),
        )?;
    }
    let entry = &resolved;

    match per_message_evidence(&ops, &entry.message_id) {
        Some(OpMessageState::Confirmed) => {}
        Some(OpMessageState::NotExecuted) => {
            // This message's command provably never ran. Restore it without
            // queueing an inverse, but never trust the old destination UID.
            restore_local(db, &request, entry, &state, false, true, entry.permanent, None)?;
            let handoff = hand_back_settled_flag_writers(db, entry, &state, &request.kind, &ops)?;
            settle_after_handoff(db, entry, handoff)?;
            return Ok(());
        }
        Some(OpMessageState::Unknown) => {
            return Err(AppError::conflict(
                "the server outcome for this message could not be determined",
            ));
        }
        None => {
            if !ops.is_empty() && !ops.iter().any(|op| op.state == PendingOpState::Done) {
                if entry.permanent && delete_side_effect(db, &entry.op_ids)? {
                    return Err(AppError::conflict(
                        "the permanent delete partially reached the server; waiting for reconciliation",
                    ));
                }
                // The operation settled without reaching the server between
                // the two passes; no inverse is needed. Only a failure that
                // never persisted a send intent / uncertainty flag or a
                // per-message outcome is provably unsent; otherwise the
                // conservative (pending) restore is kept.
                restore_local(
                    db,
                    &request,
                    entry,
                    &state,
                    false,
                    provably_unsent(&ops, &entry.message_id),
                    entry.permanent,
                    None,
                )?;
                let handoff =
                    hand_back_settled_flag_writers(db, entry, &state, &request.kind, &ops)?;
                settle_after_handoff(db, entry, handoff)?;
                return Ok(());
            }
            // A move-like operation without a per-message destination identity
            // cannot be reversed safely: guessing by Message-ID/UID could move
            // an unrelated copy. Keep the entry retryable.
            if matches!(
                request.kind.as_str(),
                "move" | "archive" | "delete" | "markJunk"
            ) && ops.iter().any(|op| op.state == PendingOpState::Done)
            {
                return Err(AppError::conflict(
                    "the moved message has no confirmed destination identity; retry after the next sync",
                ));
            }
        }
    }

    let Some(inverse) = build_inverse(db, &request, entry, &state)? else {
        let reason = if matches!(
            request.kind.as_str(),
            "move" | "archive" | "delete" | "markJunk"
        ) {
            "the moved message has no confirmed destination identity; retry after the next sync"
        } else {
            "could not locate the message on the server; retry after the next sync"
        };
        return Err(AppError::conflict(reason));
    };

    let op_id = new_id();
    if repo::undo::claim_entry_compensation(db, &entry.id, &op_id)? == 0 {
        // Another pass already claimed the inverse slot. Its settlement will
        // finish this entry; do not queue a duplicate or mark it done.
        return Ok(());
    }
    let inverse_payload = inverse.payload.clone();
    let inverse_kind = inverse.kind;
    let inverse_op = repo::pending_ops::enqueue_with_id(
        db,
        &op_id,
        NewPendingOp::new(
            entry.account_id.clone(),
            inverse.kind,
            inverse.payload,
            inverse.description,
        ),
    )?;
    // Move the projection back optimistically. The entry itself stays
    // `pending` until the inverse operation actually settles. The check inside
    // `restore_local` must see the pre-undo writer, so the inverse is only
    // recorded as the new writer afterwards.
    restore_local(
        db,
        &request,
        entry,
        &state,
        false,
        false,
        false,
        Some(inverse_op.sequence),
    )?;
    // The inverse is the newer trip into the source folder. Claim the
    // projection's identity slot in the same transaction, so an earlier
    // operation's retry cannot install its stale destination there before the
    // inverse confirms.
    crate::ops::claim_move_target_owner(
        db,
        &entry.account_id,
        inverse_kind,
        &inverse_payload,
        &inverse_op.id,
        inverse_op.sequence,
    )?;
    if inverse.kind == KIND_SET_FLAGS {
        if let Ok(payload) = serde_json::from_value::<SetFlagsOp>(inverse_payload) {
            for message_id in &payload.message_ids {
                repo::messages::record_flag_writers(
                    db,
                    message_id,
                    &op_id,
                    "undo",
                    payload.seen,
                    payload.flagged,
                    payload.answered,
                    payload.forwarded,
                )?;
            }
        }
    }
    // Aggregate the request in the same transaction that queued the inverse.
    // The entry just moved back to `pending` with an inverse id; if the process
    // exits before `finalize_request`, the request must already be
    // `Compensating`. Otherwise it keeps the terminal `Failed` a previous
    // entry's compensation error wrote, and the resume worker never settles
    // this queued inverse after a restart.
    refresh_request_state(db, &entry.request_id, None)?;
    Ok(())
}

struct InverseOp {
    kind: &'static str,
    payload: serde_json::Value,
    description: &'static str,
}

fn build_inverse(
    db: &Connection,
    request: &UndoRequest,
    entry: &UndoRequestEntry,
    state: &UndoEntryState,
) -> Result<Option<InverseOp>> {
    if entry.permanent {
        return Ok(None);
    }
    match request.kind.as_str() {
        "setSeen" | "setFlagged" => {
            let folder = entry
                .target_folder_id
                .clone()
                .unwrap_or_else(|| entry.original_folder_id.clone());
            let Some(uid) = entry.target_uid.or(state.uid) else {
                return Ok(None);
            };
            let uidvalidity = entry.target_uidvalidity.or(state.snapshot.uidvalidity);
            let (seen, flagged) = if request.kind == "setSeen" {
                (Some(state.seen), None)
            } else {
                (None, Some(state.flagged))
            };
            let payload = SetFlagsOp {
                folder_id: folder,
                uids: vec![uid],
                seen,
                flagged,
                answered: None,
                forwarded: None,
                source_uidvalidity: uidvalidity,
                previous: vec![ops::FlagRollback {
                    uid,
                    seen: if request.kind == "setSeen" {
                        !state.seen
                    } else {
                        state.seen
                    },
                    flagged: if request.kind == "setFlagged" {
                        !state.flagged
                    } else {
                        state.flagged
                    },
                    answered: state.answered,
                    forwarded: state.forwarded,
                }],
                message_ids: vec![entry.message_id.clone()],
                outcomes: Vec::new(),
                // Pin the provider copy this entry's action addressed.
                remote_ids: entry.target_graph_id.clone().into_iter().collect(),
            };
            Ok(Some(InverseOp {
                kind: KIND_SET_FLAGS,
                payload: serde_json::to_value(payload).map_err(|error| {
                    AppError::internal(format!("cannot encode undo setFlags: {error}"))
                })?,
                description: "Undo: restore flag state",
            }))
        }
        "move" | "archive" | "delete" => {
            let Some((folder, uid, uidvalidity)) = target_identity(db, entry)? else {
                warn!(message = %entry.message_id, "undo: cannot locate the moved message");
                return Ok(None);
            };
            let payload = MoveOp {
                folder_id: folder,
                target_folder_id: entry.original_folder_id.clone(),
                uids: vec![uid],
                message_ids: vec![entry.message_id.clone()],
                source_uidvalidity: uidvalidity,
                outcomes: Vec::new(),
                remote_ids: entry.target_graph_id.clone().into_iter().collect(),
            };
            Ok(Some(InverseOp {
                kind: KIND_MOVE,
                payload: serde_json::to_value(payload).map_err(|error| {
                    AppError::internal(format!("cannot encode undo move: {error}"))
                })?,
                description: "Undo: move message back",
            }))
        }
        "markJunk" => {
            let value = request.value.unwrap_or(false);
            let Some((folder, uid, uidvalidity)) = target_identity(db, entry)? else {
                warn!(message = %entry.message_id, "undo: cannot locate the junked message");
                return Ok(None);
            };
            let payload = MarkJunkOp {
                folder_id: folder,
                uids: vec![uid],
                value: !value,
                target_folder_id: Some(entry.original_folder_id.clone()),
                message_ids: vec![entry.message_id.clone()],
                source_uidvalidity: uidvalidity,
                outcomes: Vec::new(),
                remote_ids: entry.target_graph_id.clone().into_iter().collect(),
            };
            Ok(Some(InverseOp {
                kind: KIND_MARK_JUNK,
                payload: serde_json::to_value(payload).map_err(|error| {
                    AppError::internal(format!("cannot encode undo markJunk: {error}"))
                })?,
                description: "Undo: move message back",
            }))
        }
        _ => Ok(None),
    }
}

/// Resolve the destination identity recorded by the original operation.
///
/// Only evidence the operation itself persisted (a `COPYUID` destination, a
/// returned Graph id, or a binding it performed) may be used. A Message-ID or
/// "some copy in another folder" guess could select an unrelated message, so
/// when no such identity exists the inverse MOVE is refused and the entry stays
/// retryable until the evidence improves.
fn target_identity(
    db: &Connection,
    entry: &UndoRequestEntry,
) -> Result<Option<(String, i64, Option<i64>)>> {
    let (Some(folder), Some(uid)) = (entry.target_folder_id.clone(), entry.target_uid) else {
        return Ok(None);
    };
    if !identity_trusted(db, &folder, entry.target_uidvalidity)? {
        return Ok(None);
    }
    Ok(Some((folder, uid, entry.target_uidvalidity)))
}

/// A folder-local identity is usable only within the folder generation it was
/// captured in. Graph rows have no UIDVALIDITY and rely on their stable id.
fn identity_trusted(db: &Connection, folder_id: &str, uidvalidity: Option<i64>) -> Result<bool> {
    let Some(expected) = uidvalidity else {
        return Ok(true);
    };
    match repo::folders::uidvalidity(db, folder_id)? {
        Some(current) => Ok(current == expected),
        None => Ok(false),
    }
}

/// Kinds whose local projection can be claimed by a newer move/reconcile trip.
fn is_move_like(kind: &str) -> bool {
    matches!(kind, "move" | "archive" | "delete" | "markJunk")
}

/// Whether `entry` still owns its message's current projection.
///
/// A move-like action claims the projection's identity slot when it queues its
/// operation, and a failed trip keeps ownership through `reconcile_op_id` even
/// after its move slot is cleared. A newer action can take that slot over, and
/// the row may even be back in the same folder; folder membership alone proves
/// nothing. An unowned row, one owned by an operation this entry queued, or
/// (for a compensation) one older than the inverse about to be queued may be
/// restored.
fn entry_owns_projection(
    db: &Connection,
    entry: &UndoRequestEntry,
    takeover_sequence: Option<i64>,
) -> Result<bool> {
    Ok(
        match repo::messages::projection_owner(db, &entry.message_id)? {
            None => true,
            Some(owner) => {
                entry.op_ids.iter().any(|op_id| op_id == &owner.op_id)
                    || takeover_sequence
                        .is_some_and(|sequence| owner.sequence.unwrap_or(0) < sequence)
            }
        },
    )
}

/// Restore the local row from the snapshot captured when the action ran.
///
/// Undo may only touch the fields this action owns:
///
/// * a flag action restores only its own flag, and only while the row still
///   carries the value this action wrote (a later flag change is left alone);
/// * a move-like action restores the folder only while the row is still the
///   projection this action created;
/// * a row that a different action deleted is never resurrected by a flag or
///   move undo.
///
/// Identity (`uid`/`uidvalidity`) is cleared in the same `UPDATE` that changes
/// the folder. Reusing a destination UID in the source folder can collide with
/// a different message; it is only written back when the operation provably
/// never ran and the source generation still matches.
///
/// `not_executed` marks the per-message `NotExecuted` verdict: the command
/// provably never reached the server, so the *source* copy still carries the
/// provider id captured in the snapshot. In that case the Graph identity is
/// restored even when `trust_uid` is false (the UID itself stays untrusted for
/// IMAP). Leaving the row as an unowned pending projection would let a
/// retained receipt from an earlier trip be re-adopted through the
/// `latest_owner` fallback, and an `@removed` for that old destination would
/// then delete the restored message.
///
/// `undo_pending_delete` rolls back a hidden permanent delete. When the command
/// provably never ran (`trust_uid` or `not_executed`), the snapshot identity is
/// written back by protocol: a Graph row keeps its immutable provider id (with
/// a synthetic UID when the snapshot has no usable one), and an IMAP row keeps
/// its UID only inside the same UIDVALIDITY. The restore clears
/// `target_pending`, so the next delta updates the row instead of importing a
/// duplicate.
#[allow(clippy::too_many_arguments)]
fn restore_local(
    db: &Connection,
    request: &UndoRequest,
    entry: &UndoRequestEntry,
    state: &UndoEntryState,
    trust_uid: bool,
    not_executed: bool,
    undo_pending_delete: bool,
    takeover_sequence: Option<i64>,
) -> Result<()> {
    let id = entry.message_id.clone();
    let message_id = vec![id.clone()];
    let flag_action = matches!(request.kind.as_str(), "setSeen" | "setFlagged");
    let delete_action = request.kind == "delete";
    let archive_action = request.kind == "archive";

    // Ownership is a trip token, not folder membership. A newer move may have
    // claimed the row (and moved it into the same target folder after this
    // action was refused); restoring the old snapshot would revert that trip
    // and clear its owner. A compensation inverse queued in this transaction
    // is the one exception: it is newer than the current owner by construction
    // and the claim right after this restore takes the slot over.
    if !entry.permanent
        && is_move_like(&request.kind)
        && !entry_owns_projection(db, entry, takeover_sequence)?
    {
        return Ok(());
    }

    // A snapshot UID is only meaningful in the folder generation it was taken
    // in. Local/POP3 rows have no generation and are always trustworthy.
    let current_uidvalidity = repo::folders::uidvalidity(db, &entry.original_folder_id)?;
    let uidvalidity_matches = match state.snapshot.uidvalidity {
        Some(expected) => current_uidvalidity == Some(expected),
        None => true,
    };

    if undo_pending_delete {
        // A command that provably never reached the wire leaves the source
        // copy exactly where the snapshot found it, so its identity is
        // recoverable rather than a guess. `trust_uid` (cancelled while still
        // pending, with no send intent) and `not_executed` (a per-message
        // definitive skip) are the proofs.
        let never_sent = trust_uid || not_executed;
        let graph = graph_account(db, &entry.account_id)?;
        let snapshot_graph_id = state
            .snapshot
            .gm_msgid
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        // Graph has no UIDVALIDITY: the immutable provider id is the identity.
        // IMAP keeps the generation check, because a UID is only meaningful
        // inside the UIDVALIDITY it was read in.
        let identity_provable = never_sent
            && if graph {
                snapshot_graph_id.is_some()
            } else {
                state.snapshot.uid.is_some()
                    && state.snapshot.uidvalidity.is_some()
                    && uidvalidity_matches
            };
        let trusted: HashSet<String> = if identity_provable {
            [id.clone()].into_iter().collect()
        } else {
            HashSet::new()
        };
        let uids: Vec<i64> = state.snapshot.uid.into_iter().collect();
        // The release is owner-checked in the repository. Only a row this entry
        // still owns (any of its covering operations) or an unowned row may be
        // unhidden; a newer trip keeps its projection and its markers.
        let release_owner = match repo::messages::projection_owner(db, &id)? {
            None => String::new(),
            Some(owner) if entry.op_ids.iter().any(|op_id| op_id == &owner.op_id) => owner.op_id,
            Some(_) => {
                // Not released: returning `Ok(())` here would report the undo
                // as done while the message stays hidden. The newer trip owns
                // the decision, so fail this attempt instead of lying about it.
                debug!(
                    message = %id,
                    "undo: projection is owned by a newer trip; leaving it hidden"
                );
                return Err(AppError::conflict("消息已被更新的操作接管，暂时无法撤销"));
            }
        };
        let released = repo::messages::release_pending_delete_with_trust(
            db,
            &entry.account_id,
            &entry.original_folder_id,
            &message_id,
            &trusted,
            &uids,
            state.snapshot.uidvalidity,
            &release_owner,
        )?;
        if identity_provable && released.contains(&id) {
            // Write the snapshot identity back explicitly. Without this a Graph
            // row would stay `uid IS NULL, gm_msgid IS NULL,
            // target_pending = 1` and the next delta would import a second
            // row for the same message. The synthetic UID is derived from the
            // immutable local id, so it is always safe for Graph; a snapshot
            // UID is only reused in its own generation.
            let (uid, uidvalidity) = if graph {
                match state.snapshot.uid.filter(|_| uidvalidity_matches) {
                    Some(uid) => (Some(uid), state.snapshot.uidvalidity),
                    None => (Some(crate::sync::messages::synthetic_uid(&id)), None),
                }
            } else {
                (state.snapshot.uid, state.snapshot.uidvalidity)
            };
            repo::messages::set_uid_and_graph_identity(
                db,
                &id,
                uid,
                uidvalidity,
                snapshot_graph_id,
            )?;
        }
    }

    let current = repo::messages::get(db, &id)?;
    if let Some(current) = current {
        if flag_action {
            // Only revert the field this action changed, and only while this
            // action is its last writer. A later flag action may have written
            // the same value again ("read -> unread -> read"), so comparing the
            // value alone would silently undo the newest change.
            let later_writer =
                flag_field_written_later(db, &entry.account_id, entry, state, &request.kind)?;
            if let (false, Some(value)) = (later_writer, request.value) {
                if request.kind == "setSeen" {
                    if current.summary.seen == value {
                        repo::messages::set_flags(
                            db,
                            &message_id,
                            Some(state.seen),
                            None,
                            None,
                            None,
                        )?;
                        // Advance the revision in this same transaction. A
                        // local/POP3 flag action has no queued operation whose
                        // hand-back would bump it, so without this an open
                        // that snapshotted the pre-undo revision would still
                        // consider itself the baseline and mark the message
                        // read again.
                        repo::messages::bump_flag_revision(db, &id)?;
                    }
                } else if current.summary.flagged == value {
                    repo::messages::set_flags(
                        db,
                        &message_id,
                        None,
                        Some(state.flagged),
                        None,
                        None,
                    )?;
                    // See the `seen` branch: the restore must be observable to
                    // readers that snapshotted the previous revision.
                    repo::messages::bump_flag_revision(db, &id)?;
                }
            }
        } else {
            // A row hidden for an unconfirmed permanent delete belongs to that
            // later action. Undoing an earlier move/delete must not unhide (or
            // move) it.
            let pending_delete = db
                .query_row(
                    "SELECT pending_delete FROM message WHERE id = ?1",
                    rusqlite::params![id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .is_some_and(|value| value != 0);
            if pending_delete && !entry.permanent {
                return Ok(());
            }
            let owns_projection = entry
                .target_folder_id
                .as_deref()
                .map(|target| current.summary.folder_id == target)
                .unwrap_or(current.summary.folder_id == entry.original_folder_id);
            if owns_projection && current.summary.folder_id != entry.original_folder_id {
                // Moving with the destination UID would collide in (or address
                // an unrelated message in) the source folder. Clear it in the
                // same UPDATE; only restore the snapshot identity when the
                // command provably never ran (`trust_uid`, or a per-message
                // `NotExecuted` handled below). The move is conditional on this
                // entry (or no one) still owning the projection, so a newer
                // trip's row is never pulled back.
                let moved = repo::messages::move_to_folder_pending_if_owner(
                    db,
                    &message_id,
                    &entry.original_folder_id,
                    &entry.op_ids,
                    takeover_sequence,
                )?;
                if moved == 0 {
                    return Ok(());
                }
                if trust_uid && uidvalidity_matches {
                    // Restore the *whole* snapshot identity. Putting back only
                    // the UID would leave a Graph row with no provider id, and
                    // the next permanent delete could then only guess through
                    // the mutable folder mapping.
                    repo::messages::set_uid_and_graph_identity(
                        db,
                        &id,
                        state.snapshot.uid,
                        state.snapshot.uidvalidity,
                        state.snapshot.gm_msgid.as_deref(),
                    )?;
                } else if not_executed {
                    // `NotExecuted` is per-message and definitive: the command
                    // never reached the server, so the copy in the source
                    // folder still carries the identity captured in the
                    // snapshot. Restoring that identity ends the projection's
                    // wait. Without it the row stays `uid IS NULL,
                    // target_pending = 1` with no owner, and a retained
                    // confirmation from an earlier trip can be re-adopted
                    // through the `latest_owner` fallback — an `@removed` for
                    // that old destination then deletes the restored message.
                    if graph_account(db, &entry.account_id)? {
                        // Only a Graph row may turn `gm_msgid` back into a
                        // provider id: on Gmail IMAP it is `X-GM-MSGID`. The
                        // synthetic UID is derived from the immutable row id,
                        // so it is safe even when the snapshot had none.
                        if let Some(remote) = state.snapshot.gm_msgid.as_deref() {
                            // The snapshot UID is only reusable inside its own
                            // folder generation; the synthetic UID is derived
                            // from the immutable row id and is always safe.
                            let (uid, uidvalidity) =
                                match state.snapshot.uid.filter(|_| uidvalidity_matches) {
                                    Some(uid) => (uid, state.snapshot.uidvalidity),
                                    None => (crate::sync::messages::synthetic_uid(&id), None),
                                };
                            repo::messages::set_uid_and_graph_identity(
                                db,
                                &id,
                                Some(uid),
                                uidvalidity,
                                Some(remote),
                            )?;
                        }
                    } else if state.snapshot.uidvalidity.is_some()
                        && uidvalidity_matches
                        && state.snapshot.uid.is_some()
                    {
                        // IMAP keeps the generation check. The snapshot UID is
                        // only meaningful inside the UIDVALIDITY it was taken
                        // in; when that is missing or changed the row must stay
                        // `target_pending` for reconciliation. Never write a
                        // synthetic UID here: it would address an unrelated
                        // message on an IMAP server.
                        repo::messages::set_uid_and_graph_identity(
                            db,
                            &id,
                            state.snapshot.uid,
                            state.snapshot.uidvalidity,
                            state.snapshot.gm_msgid.as_deref(),
                        )?;
                    }
                }
            }
            if delete_action || archive_action {
                repo::messages::set_deleted_local(db, &id, false)?;
            }
            if delete_action {
                repo::attachments::replace_for_message(db, &id, &state.attachments)?;
            }
        }
    } else if delete_action && entry.permanent {
        // Only a permanent delete can own the row's disappearance (an older
        // build erased it before the confirmation). A non-permanent delete
        // moves or hides the row, so a missing row belongs to a later action
        // and must not be resurrected.
        let mut snapshot: StoredMessage = state.snapshot.clone().into_stored();
        snapshot.is_deleted_local = false;
        if !trust_uid {
            // Never reuse a UID that may belong to another message after the
            // server side effect. The next sync rebinds the row.
            snapshot.uid = None;
            snapshot.uidvalidity = None;
        }
        // Re-check inside this write transaction, next to the read above: if a
        // row (or its projection owner) appeared for the id in the meantime, a
        // newer trip owns it and the stale snapshot must not replace it.
        if repo::messages::get(db, &id)?.is_some()
            || repo::messages::projection_owner(db, &id)?.is_some()
        {
            return Ok(());
        }
        repo::messages::upsert(db, &snapshot)?;
        repo::attachments::replace_for_message(db, &id, &state.attachments)?;
    }

    for folder in [
        Some(entry.original_folder_id.clone()),
        entry.target_folder_id.clone(),
    ]
    .into_iter()
    .flatten()
    {
        repo::folders::recompute_counts(db, &folder)?;
    }
    Ok(())
}

fn finalize_request(db: &Connection, request_id: &str) -> Result<ProcessOutcome> {
    let Some(request) = repo::undo::get_request(db, request_id)? else {
        return Ok(ProcessOutcome {
            state: UndoState::Done,
            permanent_gone: false,
            last_error: None,
        });
    };
    if request.state == UndoState::Done {
        return Ok(ProcessOutcome {
            state: UndoState::Done,
            permanent_gone: false,
            last_error: request.last_error,
        });
    }
    match refresh_request_state(db, request_id, None)? {
        Some((state, last_error)) => Ok(ProcessOutcome {
            state,
            permanent_gone: false,
            last_error,
        }),
        None => Ok(ProcessOutcome {
            state: UndoState::Done,
            permanent_gone: false,
            last_error: None,
        }),
    }
}

/// Emit `mail://changed` for the folders/messages this request touched.
async fn emit_changes(ctx: &CoreContext, request_id: &str) -> Result<()> {
    let request_id = request_id.to_string();
    let entries = ctx
        .db
        .read(move |db| repo::undo::list_entries(db, &request_id))
        .await?;
    let mut per_account: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();
    for entry in entries {
        let bucket = per_account.entry(entry.account_id.clone()).or_default();
        for folder in [
            Some(entry.original_folder_id.clone()),
            entry.target_folder_id.clone(),
        ]
        .into_iter()
        .flatten()
        {
            if !bucket.0.contains(&folder) {
                bucket.0.push(folder);
            }
        }
        bucket.1.push(entry.message_id.clone());
    }
    for (account_id, (folders, message_ids)) in per_account {
        events::emit_mail_changed(
            ctx.events.as_ref(),
            account_id,
            folders,
            message_ids,
            MailChangeKind::Updated,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::messages::{StoredSnapshot, UndoEntryState};
    use nuncio_core::models::{
        Account, AccountState, AuthKind, Folder, FolderRole, MessageSummary, OAuthProvider,
        ProviderKind,
    };
    use nuncio_core::Paths;

    fn entry_payload(message_id: &str, folder_id: &str) -> serde_json::Value {
        let mut summary = MessageSummary::new(
            message_id,
            "acc",
            folder_id,
            format!("thread-{message_id}"),
            nuncio_core::models::now(),
        );
        summary.subject = message_id.to_string();
        summary.seen = true;
        let mut message = StoredMessage::new(summary);
        message.uid = Some(7);
        message.uidvalidity = Some(1);
        let state = UndoEntryState {
            id: message_id.to_string(),
            account_id: "acc".into(),
            folder_id: folder_id.into(),
            target_folder_id: Some(folder_id.into()),
            uid: Some(7),
            permanent: false,
            seen: true,
            flagged: false,
            answered: false,
            forwarded: false,
            is_deleted_local: false,
            snapshot: StoredSnapshot::from(&message),
            attachments: Vec::new(),
            flag_writer_id: None,
        };
        serde_json::to_value(&state).expect("undo entry payload")
    }

    fn inverse_payload(message_id: &str, folder_id: &str) -> serde_json::Value {
        serde_json::to_value(&SetFlagsOp {
            folder_id: folder_id.into(),
            uids: vec![7],
            seen: None,
            flagged: Some(false),
            answered: None,
            forwarded: None,
            source_uidvalidity: Some(1),
            previous: Vec::new(),
            message_ids: vec![message_id.into()],
            remote_ids: Vec::new(),
            outcomes: Vec::new(),
        })
        .expect("inverse payload")
    }

    async fn request_state(ctx: &CoreContext, request_id: &str) -> UndoState {
        let request_id = request_id.to_string();
        ctx.db
            .read(move |db| repo::undo::get_request(db, &request_id))
            .await
            .expect("request")
            .expect("request exists")
            .state
    }

    async fn entry_results(ctx: &CoreContext, request_id: &str) -> Vec<UndoEntryResult> {
        let request_id = request_id.to_string();
        ctx.db
            .read(move |db| repo::undo::list_entries(db, &request_id))
            .await
            .expect("entries")
            .into_iter()
            .map(|entry| entry.result)
            .collect()
    }

    /// A mixed request must stay resumable after the pass that fails one of its
    /// entries. The failing entry and the request were written together in the
    /// first transaction; if the process exits before `finalize_request`, the
    /// request must not be committed `Failed`, because `failed` is terminal for
    /// the resume worker and the entry waiting for an inverse receipt would
    /// never settle.
    #[tokio::test]
    async fn a_failed_entry_does_not_strand_a_mixed_request_across_a_restart() {
        let root = std::env::temp_dir().join(format!("nuncio-undo-mixed-{}", new_id()));
        let ctx = CoreContext::for_test(Paths::new(root))
            .await
            .expect("context");
        let request_id = new_id();
        let failed_entry_id = new_id();
        let waiting_entry_id = new_id();
        let inverse_id = new_id();
        let folder_id = "fld";

        ctx.db
            .write({
                let request_id = request_id.clone();
                let failed_entry_id = failed_entry_id.clone();
                let waiting_entry_id = waiting_entry_id.clone();
                let inverse_id = inverse_id.clone();
                move |db| {
                    let account = Account {
                        id: "acc".into(),
                        provider: ProviderKind::Outlook,
                        email: "test@example.com".into(),
                        display_name: "Test".into(),
                        color: "#000000".into(),
                        auth_kind: AuthKind::Oauth,
                        oauth_provider: Some(OAuthProvider::Microsoft),
                        imap: None,
                        smtp: None,
                        pop3: None,
                        caldav_url: None,
                        carddav_url: None,
                        sent_copy: None,
                        sync_window_days: 90,
                        mail_enabled: true,
                        calendar_enabled: false,
                        contacts_enabled: false,
                        tasks_enabled: false,
                        notifications_enabled: true,
                        include_in_unified: true,
                        sort_order: 0,
                        state: AccountState::Ok,
                        state_message: None,
                        capabilities: Default::default(),
                        created_at: nuncio_core::models::now(),
                    };
                    repo::accounts::insert(db, &account)?;

                    // The waiting entry already queued its inverse; it is in
                    // flight and has not produced a receipt yet.
                    repo::pending_ops::enqueue_with_id(
                        db,
                        &inverse_id,
                        NewPendingOp::new(
                            "acc",
                            KIND_SET_FLAGS,
                            inverse_payload("m-waiting", folder_id),
                            "undo",
                        ),
                    )?;
                    repo::pending_ops::mark_in_progress(db, &inverse_id)?;

                    let request = NewUndoRequest::new(
                        &request_id,
                        "acc",
                        "setSeen",
                        Some(true),
                        vec![inverse_id.clone()],
                    );
                    let failed_entry = NewUndoEntry {
                        id: failed_entry_id.clone(),
                        request_id: request_id.clone(),
                        account_id: "acc".into(),
                        message_id: "m-failed".into(),
                        original_folder_id: folder_id.into(),
                        target_folder_id: Some(folder_id.into()),
                        target_uid: Some(7),
                        target_uidvalidity: Some(1),
                        target_graph_id: None,
                        permanent: false,
                        op_ids: Vec::new(),
                        // The referenced operation rows are gone: this entry
                        // fails deterministically once the retry bound is hit.
                        evidence_missing: true,
                        payload: entry_payload("m-failed", folder_id),
                        ordinal: 0,
                    };
                    let waiting_entry = NewUndoEntry {
                        id: waiting_entry_id.clone(),
                        request_id: request_id.clone(),
                        account_id: "acc".into(),
                        message_id: "m-waiting".into(),
                        original_folder_id: folder_id.into(),
                        target_folder_id: Some(folder_id.into()),
                        target_uid: Some(7),
                        target_uidvalidity: Some(1),
                        target_graph_id: None,
                        permanent: false,
                        op_ids: vec![inverse_id.clone()],
                        evidence_missing: false,
                        payload: entry_payload("m-waiting", folder_id),
                        ordinal: 1,
                    };
                    repo::undo::create_request(db, request, &[failed_entry, waiting_entry])?;
                    repo::undo::claim_entry_compensation(db, &waiting_entry_id, &inverse_id)?;
                    // The next pass reaches MAX_UNDO_ATTEMPTS.
                    repo::undo::bump_attempt(db, &request_id)?;
                    repo::undo::bump_attempt(db, &request_id)?;
                    Ok(())
                }
            })
            .await
            .expect("seed mixed request");

        // Run only the first transaction, then act as if the process exited:
        // `finalize_request` and the event emission never ran.
        ctx.db
            .write({
                let request_id = request_id.clone();
                move |db| prepare_request(db, &request_id)
            })
            .await
            .expect("prepare");

        assert_eq!(
            entry_results(&ctx, &request_id).await,
            vec![UndoEntryResult::Failed, UndoEntryResult::Pending]
        );
        assert_eq!(
            request_state(&ctx, &request_id).await,
            UndoState::Compensating,
            "a failed entry must not commit the request Failed while another entry is unsettled"
        );

        // The restart reads the request back and asks the worker which
        // requests it may continue.
        let resumable = ctx
            .db
            .read({
                let request_id = request_id.clone();
                move |db| {
                    Ok(repo::undo::resumable_requests(db, 32)?
                        .into_iter()
                        .any(|request| request.id == request_id))
                }
            })
            .await
            .expect("resumable");
        assert!(
            resumable,
            "the worker must still resume a mixed request after a restart"
        );

        // A worker pass before the inverse settles keeps it open.
        let resumed = resume_deferred_undo_requests(&ctx).await.expect("resume");
        assert_eq!(resumed, 1);
        assert_eq!(
            request_state(&ctx, &request_id).await,
            UndoState::Compensating
        );

        // The inverse now completes with a confirmed receipt for its message.
        ctx.db
            .write({
                let inverse_id = inverse_id.clone();
                move |db| {
                    let op = repo::pending_ops::get_required(db, &inverse_id)?;
                    let mut payload = repo::pending_ops::payload::<SetFlagsOp>(&op)?;
                    payload.outcomes = vec![OpMessageOutcome {
                        message_id: "m-waiting".into(),
                        source_uid: None,
                        state: OpMessageState::Confirmed,
                        target_folder_id: None,
                        target_uid: None,
                        target_uidvalidity: None,
                        target_remote_id: None,
                        attempt_remote_id: None,
                        copy_phase: Default::default(),
                        delete_flag_phase: Default::default(),
                        expunge_phase: Default::default(),
                    }];
                    let value = serde_json::to_value(&payload)
                        .map_err(|error| AppError::internal(error.to_string()))?;
                    repo::pending_ops::update_payload(db, &inverse_id, &value)?;
                    repo::pending_ops::mark_done(db, &inverse_id)
                }
            })
            .await
            .expect("settle inverse");
        assert_eq!(
            ctx.db
                .read({
                    let inverse_id = inverse_id.clone();
                    move |db| repo::pending_ops::get_required(db, &inverse_id)
                })
                .await
                .expect("inverse")
                .state,
            PendingOpState::Done
        );

        // The worker still owns the request and settles the waiting entry. The
        // request then becomes Failed only because the other entry is terminal.
        let resumed = resume_deferred_undo_requests(&ctx).await.expect("resume");
        assert_eq!(resumed, 1);
        assert_eq!(
            entry_results(&ctx, &request_id).await,
            vec![UndoEntryResult::Failed, UndoEntryResult::Done],
            "the inverse receipt must settle the waiting entry automatically"
        );
        assert_eq!(request_state(&ctx, &request_id).await, UndoState::Failed);
    }

    /// Regression: retrying a `Failed` request by id can re-queue an entry's
    /// inverse. Once the previously blocking (non-matching) predecessor has
    /// been cancelled, the hand-off finds an earlier matching action that was
    /// skipped, queues a fresh inverse and moves the entry back to `pending`.
    ///
    /// That re-queue does not go through the `compensate` list, so
    /// `prepare_request` must aggregate the request state in the same
    /// transaction. Otherwise the request stays terminal `Failed`; the resume
    /// worker skips it and a successful inverse never settles the entry after
    /// a restart.
    #[tokio::test]
    async fn requeued_inverse_does_not_leave_a_failed_request_terminal() {
        let root = std::env::temp_dir().join(format!("nuncio-undo-requeue-{}", new_id()));
        let ctx = CoreContext::for_test(Paths::new(root))
            .await
            .expect("context");
        let request_id = "undo-requeue";
        let folder_id = "fld";

        let flags = |seen: Option<bool>, remote: &str, outcome: Option<OpMessageState>| SetFlagsOp {
            folder_id: folder_id.into(),
            uids: vec![777],
            seen,
            flagged: None,
            answered: None,
            forwarded: None,
            source_uidvalidity: None,
            previous: Vec::new(),
            message_ids: vec!["m1".into()],
            remote_ids: vec![remote.into()],
            outcomes: outcome
                .map(|state| OpMessageOutcome {
                    message_id: "m1".into(),
                    source_uid: None,
                    state,
                    target_folder_id: None,
                    target_uid: None,
                    target_uidvalidity: None,
                    target_remote_id: None,
                    attempt_remote_id: None,
                    copy_phase: Default::default(),
                    delete_flag_phase: Default::default(),
                    expunge_phase: Default::default(),
                })
                .into_iter()
                .collect(),
        };
        // A: the earlier matching action (mark read) that was skipped, settled
        // `done` with a definitive `NotExecuted` receipt.
        let earlier_payload = serde_json::to_value(flags(
            Some(true),
            "AAMk-graph-1",
            Some(OpMessageState::NotExecuted),
        ))
        .expect("earlier payload");
        // P: the non-matching predecessor. It used to make the undo refuse;
        // now it is cancelled and provably unsent, so it is skipped.
        let nonmatching_payload =
            serde_json::to_value(flags(Some(false), "AAMk-graph-other", None))
                .expect("non-matching payload");
        // B: the action being undone (mark unread), cancelled before it ran.
        let undone_payload = serde_json::to_value(flags(Some(false), "AAMk-graph-1", None))
            .expect("undone payload");

        let account = Account {
            id: "acc".into(),
            provider: ProviderKind::Outlook,
            email: "test@example.com".into(),
            display_name: "Test".into(),
            color: "#000000".into(),
            auth_kind: AuthKind::Oauth,
            oauth_provider: Some(OAuthProvider::Microsoft),
            imap: None,
            smtp: None,
            pop3: None,
            caldav_url: None,
            carddav_url: None,
            sent_copy: None,
            sync_window_days: 90,
            mail_enabled: true,
            calendar_enabled: false,
            contacts_enabled: false,
            tasks_enabled: false,
            notifications_enabled: true,
            include_in_unified: true,
            sort_order: 0,
            state: AccountState::Ok,
            state_message: None,
            capabilities: Default::default(),
            created_at: nuncio_core::models::now(),
        };
        let mut summary =
            MessageSummary::new("m1", "acc", folder_id, "thread-m1", nuncio_core::models::now());
        summary.subject = "m1".into();
        summary.seen = false;
        let mut message = StoredMessage::new(summary);
        message.uid = Some(777);
        message.uidvalidity = None;
        message.gm_msgid = Some("AAMk-graph-1".into());
        let snapshot = StoredSnapshot::from(&message);
        let payload = serde_json::to_value(UndoEntryState {
            id: "m1".into(),
            account_id: "acc".into(),
            folder_id: folder_id.into(),
            target_folder_id: Some(folder_id.into()),
            uid: Some(777),
            permanent: false,
            // The undo restores the value that existed before B wrote `false`.
            seen: true,
            flagged: false,
            answered: false,
            forwarded: false,
            is_deleted_local: false,
            snapshot,
            attachments: Vec::new(),
            flag_writer_id: Some("op-b".into()),
        })
        .expect("entry payload");

        ctx.db
            .write(move |db| {
                repo::accounts::insert(db, &account)?;
                repo::folders::insert(
                    db,
                    &Folder::new(folder_id, "acc", "Inbox", FolderRole::Inbox),
                )?;
                repo::messages::upsert(db, &message)?;

                repo::pending_ops::enqueue_with_id(
                    db,
                    "op-a",
                    NewPendingOp::new("acc", KIND_SET_FLAGS, earlier_payload, "earlier mark read"),
                )?;
                repo::pending_ops::mark_done(db, "op-a")?;
                repo::pending_ops::enqueue_with_id(
                    db,
                    "op-p",
                    NewPendingOp::new(
                        "acc",
                        KIND_SET_FLAGS,
                        nonmatching_payload,
                        "non-matching predecessor",
                    ),
                )?;
                repo::pending_ops::cancel_if_pending(db, "op-p")?;
                repo::pending_ops::enqueue_with_id(
                    db,
                    "op-b",
                    NewPendingOp::new("acc", KIND_SET_FLAGS, undone_payload, "undone mark unread"),
                )?;
                repo::pending_ops::cancel_if_pending(db, "op-b")?;

                repo::messages::record_flag_writer(
                    db,
                    "m1",
                    "seen",
                    "op-b",
                    Some(false),
                    "setSeen",
                )?;

                repo::undo::create_request(
                    db,
                    NewUndoRequest::new(
                        request_id,
                        "acc",
                        "setSeen",
                        Some(false),
                        vec!["op-b".into()],
                    ),
                    &[NewUndoEntry {
                        id: "entry-requeue".into(),
                        request_id: request_id.into(),
                        account_id: "acc".into(),
                        message_id: "m1".into(),
                        original_folder_id: folder_id.into(),
                        target_folder_id: Some(folder_id.into()),
                        target_uid: Some(777),
                        target_uidvalidity: None,
                        target_graph_id: Some("AAMk-graph-1".into()),
                        permanent: false,
                        op_ids: vec!["op-b".into()],
                        evidence_missing: false,
                        payload,
                        ordinal: 0,
                    }],
                )?;
                // The previous pass committed `Failed` (the non-matching
                // predecessor refused the hand-back then).
                repo::undo::update_state(
                    db,
                    request_id,
                    UndoState::Failed,
                    Some("the earlier action addressed a different copy"),
                )?;
                Ok(())
            })
            .await
            .expect("seed failed request");

        // Retry by id: run only the first transaction and act as if the
        // process exited before `finalize_request` and the event emission.
        ctx.db
            .write(move |db| prepare_request(db, request_id))
            .await
            .expect("prepare");

        let (result, inverse_id) = ctx
            .db
            .read(move |db| {
                let entry = repo::undo::list_entries(db, request_id)?
                    .into_iter()
                    .next()
                    .expect("undo entry");
                Ok((entry.result, entry.inverse_op_id))
            })
            .await
            .expect("entry");
        assert_eq!(
            result,
            UndoEntryResult::Pending,
            "the re-queued inverse must move the entry back to pending"
        );
        let inverse_id = inverse_id.expect("the restored value must be queued as a tracked inverse");
        assert_eq!(
            request_state(&ctx, request_id).await,
            UndoState::Compensating,
            "a request with an unsettled inverse must not stay terminal Failed"
        );

        let inverse = ctx
            .db
            .read({
                let inverse_id = inverse_id.clone();
                move |db| repo::pending_ops::get_required(db, &inverse_id)
            })
            .await
            .expect("inverse op");
        let inverse_payload =
            repo::pending_ops::payload::<SetFlagsOp>(&inverse).expect("inverse payload");
        assert_eq!(inverse_payload.seen, Some(true));
        assert_eq!(
            inverse_payload.remote_ids,
            vec!["AAMk-graph-1".to_string()],
            "the replacement must address the copy the earlier action pinned"
        );

        // The restart reads the request back and asks the worker which
        // requests it may continue.
        let resumable = ctx
            .db
            .read(move |db| {
                Ok(repo::undo::resumable_requests(db, 32)?
                    .into_iter()
                    .any(|request| request.id == request_id))
            })
            .await
            .expect("resumable");
        assert!(
            resumable,
            "the worker must still resume the retried request after a restart"
        );

        let resumed = resume_deferred_undo_requests(&ctx).await.expect("resume");
        assert_eq!(resumed, 1);
        assert_eq!(
            request_state(&ctx, request_id).await,
            UndoState::Compensating
        );

        // The inverse now completes with a confirmed receipt for its message.
        ctx.db
            .write({
                let inverse_id = inverse_id.clone();
                move |db| {
                    let op = repo::pending_ops::get_required(db, &inverse_id)?;
                    let mut payload = repo::pending_ops::payload::<SetFlagsOp>(&op)?;
                    payload.outcomes = vec![OpMessageOutcome {
                        message_id: "m1".into(),
                        source_uid: None,
                        state: OpMessageState::Confirmed,
                        target_folder_id: None,
                        target_uid: None,
                        target_uidvalidity: None,
                        target_remote_id: None,
                        attempt_remote_id: None,
                        copy_phase: Default::default(),
                        delete_flag_phase: Default::default(),
                        expunge_phase: Default::default(),
                    }];
                    let value = serde_json::to_value(&payload)
                        .map_err(|error| AppError::internal(error.to_string()))?;
                    repo::pending_ops::update_payload(db, &inverse_id, &value)?;
                    repo::pending_ops::mark_done(db, &inverse_id)
                }
            })
            .await
            .expect("settle inverse");

        let resumed = resume_deferred_undo_requests(&ctx).await.expect("resume");
        assert_eq!(resumed, 1);
        assert_eq!(
            entry_results(&ctx, request_id).await,
            vec![UndoEntryResult::Done],
            "the inverse receipt must settle the re-queued entry automatically"
        );
        assert_eq!(request_state(&ctx, request_id).await, UndoState::Done);
    }

    /// Mark a queued inverse as settled with a confirmed receipt for one
    /// message, as the queue worker would after the server applied it.
    async fn settle_inverse_confirmed(ctx: &CoreContext, inverse_id: &str, message_id: &str) {
        let inverse_id = inverse_id.to_string();
        let message_id = message_id.to_string();
        ctx.db
            .write(move |db| {
                let op = repo::pending_ops::get_required(db, &inverse_id)?;
                let mut payload = repo::pending_ops::payload::<SetFlagsOp>(&op)?;
                payload.outcomes = vec![OpMessageOutcome {
                    message_id,
                    source_uid: None,
                    state: OpMessageState::Confirmed,
                    target_folder_id: None,
                    target_uid: None,
                    target_uidvalidity: None,
                    target_remote_id: None,
                    attempt_remote_id: None,
                    copy_phase: Default::default(),
                    delete_flag_phase: Default::default(),
                    expunge_phase: Default::default(),
                }];
                let value = serde_json::to_value(&payload)
                    .map_err(|error| AppError::internal(error.to_string()))?;
                repo::pending_ops::update_payload(db, &inverse_id, &value)?;
                repo::pending_ops::mark_done(db, &inverse_id)
            })
            .await
            .expect("settle inverse");
    }

    /// Regression: retrying a `Failed` request by id when more than one entry
    /// needs compensation. The first compensation fails (its target identity is
    /// not known yet) and the second queues its inverse successfully.
    ///
    /// If the process exits before `finalize_request`, the request must already
    /// be `Compensating` rather than terminal `Failed`; `prepare_request` must
    /// re-arm the failed entries it plans to compensate, and the enqueue
    /// transaction itself must aggregate the request. Otherwise
    /// `resumable_requests` skips the request and the queued inverse never
    /// settles after the restart.
    #[tokio::test]
    async fn retrying_multiple_failed_entries_stays_compensating_across_a_restart() {
        let root = std::env::temp_dir().join(format!("nuncio-undo-multi-{}", new_id()));
        let ctx = CoreContext::for_test(Paths::new(root))
            .await
            .expect("context");
        let request_id = new_id();
        let first_entry_id = new_id();
        let second_entry_id = new_id();
        let folder_id = "fld";

        let stored = |message_id: &str| {
            let mut summary = MessageSummary::new(
                message_id,
                "acc",
                folder_id,
                format!("thread-{message_id}"),
                nuncio_core::models::now(),
            );
            summary.subject = message_id.to_string();
            summary.seen = true;
            let mut message = StoredMessage::new(summary);
            message.uid = Some(7);
            message.uidvalidity = Some(1);
            message
        };
        let entry_payload = |message_id: &str, uid: Option<i64>| {
            serde_json::to_value(UndoEntryState {
                id: message_id.to_string(),
                account_id: "acc".into(),
                folder_id: folder_id.into(),
                target_folder_id: Some(folder_id.into()),
                uid,
                permanent: false,
                seen: false,
                flagged: false,
                answered: false,
                forwarded: false,
                is_deleted_local: false,
                snapshot: StoredSnapshot::from(&stored(message_id)),
                attachments: Vec::new(),
                flag_writer_id: None,
            })
            .expect("undo entry payload")
        };
        let confirmed_flags = |message_id: &str| SetFlagsOp {
            folder_id: folder_id.into(),
            uids: vec![7],
            seen: Some(true),
            flagged: None,
            answered: None,
            forwarded: None,
            source_uidvalidity: Some(1),
            previous: Vec::new(),
            message_ids: vec![message_id.into()],
            remote_ids: Vec::new(),
            outcomes: vec![OpMessageOutcome {
                message_id: message_id.into(),
                source_uid: None,
                state: OpMessageState::Confirmed,
                target_folder_id: None,
                target_uid: None,
                target_uidvalidity: None,
                target_remote_id: None,
                attempt_remote_id: None,
                copy_phase: Default::default(),
                delete_flag_phase: Default::default(),
                expunge_phase: Default::default(),
            }],
        };

        let first_payload = entry_payload("m-first", None);
        let second_payload = entry_payload("m-second", Some(7));
        let first_op_payload =
            serde_json::to_value(confirmed_flags("m-first")).expect("first op payload");
        let second_op_payload =
            serde_json::to_value(confirmed_flags("m-second")).expect("second op payload");
        let account = Account {
            id: "acc".into(),
            provider: ProviderKind::Outlook,
            email: "test@example.com".into(),
            display_name: "Test".into(),
            color: "#000000".into(),
            auth_kind: AuthKind::Oauth,
            oauth_provider: Some(OAuthProvider::Microsoft),
            imap: None,
            smtp: None,
            pop3: None,
            caldav_url: None,
            carddav_url: None,
            sent_copy: None,
            sync_window_days: 90,
            mail_enabled: true,
            calendar_enabled: false,
            contacts_enabled: false,
            tasks_enabled: false,
            notifications_enabled: true,
            include_in_unified: true,
            sort_order: 0,
            state: AccountState::Ok,
            state_message: None,
            capabilities: Default::default(),
            created_at: nuncio_core::models::now(),
        };
        let first_message = stored("m-first");
        let mut second_message = stored("m-second");
        // UIDs are unique per folder; the second message needs its own row.
        second_message.uid = Some(8);

        ctx.db
            .write({
                let request_id = request_id.clone();
                let first_entry_id = first_entry_id.clone();
                let second_entry_id = second_entry_id.clone();
                move |db| {
                    repo::accounts::insert(db, &account)?;
                    repo::folders::insert(
                        db,
                        &Folder::new(folder_id, "acc", "Inbox", FolderRole::Inbox),
                    )?;
                    repo::messages::upsert(db, &first_message)?;
                    repo::messages::upsert(db, &second_message)?;

                    for (op_id, payload) in [
                        ("op-first", first_op_payload),
                        ("op-second", second_op_payload),
                    ] {
                        repo::pending_ops::enqueue_with_id(
                            db,
                            op_id,
                            NewPendingOp::new("acc", KIND_SET_FLAGS, payload, "mark read"),
                        )?;
                        repo::pending_ops::mark_done(db, op_id)?;
                    }

                    repo::undo::create_request(
                        db,
                        NewUndoRequest::new(
                            request_id.clone(),
                            "acc",
                            "setSeen",
                            Some(true),
                            vec!["op-first".into(), "op-second".into()],
                        ),
                        &[
                            NewUndoEntry {
                                id: first_entry_id.clone(),
                                request_id: request_id.clone(),
                                account_id: "acc".into(),
                                message_id: "m-first".into(),
                                original_folder_id: folder_id.into(),
                                target_folder_id: Some(folder_id.into()),
                                // The destination identity is not known yet, so
                                // this entry's compensation fails this pass.
                                target_uid: None,
                                target_uidvalidity: Some(1),
                                target_graph_id: None,
                                permanent: false,
                                op_ids: vec!["op-first".into()],
                                evidence_missing: false,
                                payload: first_payload,
                                ordinal: 0,
                            },
                            NewUndoEntry {
                                id: second_entry_id.clone(),
                                request_id: request_id.clone(),
                                account_id: "acc".into(),
                                message_id: "m-second".into(),
                                original_folder_id: folder_id.into(),
                                target_folder_id: Some(folder_id.into()),
                                target_uid: Some(7),
                                target_uidvalidity: Some(1),
                                target_graph_id: None,
                                permanent: false,
                                op_ids: vec!["op-second".into()],
                                evidence_missing: false,
                                payload: second_payload,
                                ordinal: 1,
                            },
                        ],
                    )?;
                    // An earlier pass left both entries failed and committed
                    // the request terminal; the user retries it by id.
                    for entry_id in [&first_entry_id, &second_entry_id] {
                        repo::undo::update_entry_result(db, entry_id, UndoEntryResult::Failed)?;
                    }
                    repo::undo::update_state(
                        db,
                        &request_id,
                        UndoState::Failed,
                        Some("could not undo every message; retry later"),
                    )?;
                    Ok(())
                }
            })
            .await
            .expect("seed failed request");

        // Retry by id. `process_request` runs `prepare_request` as one
        // transaction and every compensation as another; reproduce exactly
        // those transactions and act as if the process exited before
        // `finalize_request` and the event emission.
        let prepared = ctx
            .db
            .write({
                let request_id = request_id.clone();
                move |db| {
                    Ok::<_, AppError>(prepare_request(db, &request_id)?.expect("prepared request"))
                }
            })
            .await
            .expect("prepare");
        assert_eq!(
            prepared.compensate,
            vec![first_entry_id.clone(), second_entry_id.clone()]
        );
        assert_eq!(
            entry_results(&ctx, &request_id).await,
            vec![UndoEntryResult::Pending, UndoEntryResult::Pending],
            "prepare_request must re-arm the failed entries it is about to compensate"
        );
        assert_eq!(
            request_state(&ctx, &request_id).await,
            UndoState::Compensating
        );

        // First compensation fails: this entry has no target identity.
        let error = ctx
            .db
            .write({
                let first_entry_id = first_entry_id.clone();
                move |db| compensate_entry(db, &first_entry_id)
            })
            .await
            .expect_err("the first entry cannot build its inverse yet");
        let reason = error.message();
        ctx.db
            .write({
                let first_entry_id = first_entry_id.clone();
                let request_id = request_id.clone();
                move |db| {
                    repo::undo::update_entry_result(db, &first_entry_id, UndoEntryResult::Failed)?;
                    refresh_request_state(db, &request_id, Some(&reason))?;
                    Ok(())
                }
            })
            .await
            .expect("record the failed compensation");
        assert_eq!(
            request_state(&ctx, &request_id).await,
            UndoState::Deferred,
            "the re-armed second entry keeps the request resumable after the first failure"
        );

        // Second compensation succeeds and queues its inverse.
        ctx.db
            .write({
                let second_entry_id = second_entry_id.clone();
                move |db| compensate_entry(db, &second_entry_id)
            })
            .await
            .expect("the second entry queues its inverse");

        let entries = ctx
            .db
            .read({
                let request_id = request_id.clone();
                move |db| repo::undo::list_entries(db, &request_id)
            })
            .await
            .expect("entries");
        assert_eq!(entries[0].result, UndoEntryResult::Failed);
        assert_eq!(entries[1].result, UndoEntryResult::Pending);
        let second_inverse_id = entries[1]
            .inverse_op_id
            .clone()
            .expect("the second entry must have a queued inverse");
        assert_eq!(
            request_state(&ctx, &request_id).await,
            UndoState::Compensating,
            "the enqueue transaction must not leave the request terminal Failed"
        );

        // The restart asks the worker which requests it may continue.
        let resumable = ctx
            .db
            .read({
                let request_id = request_id.clone();
                move |db| {
                    Ok(repo::undo::resumable_requests(db, 32)?
                        .into_iter()
                        .any(|request| request.id == request_id))
                }
            })
            .await
            .expect("resumable");
        assert!(
            resumable,
            "the worker must still resume the retried request after a restart"
        );
        let resumed = resume_deferred_undo_requests(&ctx).await.expect("resume");
        assert_eq!(resumed, 1);
        assert_eq!(
            request_state(&ctx, &request_id).await,
            UndoState::Compensating
        );

        // A later sync resolves the first entry's target identity.
        ctx.db
            .write({
                let first_entry_id = first_entry_id.clone();
                move |db| {
                    repo::undo::update_entry_identity(
                        db,
                        &first_entry_id,
                        Some(folder_id),
                        Some(7),
                        Some(1),
                        None,
                    )
                }
            })
            .await
            .expect("resolve first identity");

        // The second inverse settles; the worker now queues the first one.
        settle_inverse_confirmed(&ctx, &second_inverse_id, "m-second").await;
        let resumed = resume_deferred_undo_requests(&ctx).await.expect("resume");
        assert_eq!(resumed, 1);
        let entries = ctx
            .db
            .read({
                let request_id = request_id.clone();
                move |db| repo::undo::list_entries(db, &request_id)
            })
            .await
            .expect("entries");
        assert_eq!(entries[1].result, UndoEntryResult::Done);
        assert_eq!(entries[0].result, UndoEntryResult::Pending);
        let first_inverse_id = entries[0]
            .inverse_op_id
            .clone()
            .expect("the first entry must have queued its inverse");
        assert_eq!(
            request_state(&ctx, &request_id).await,
            UndoState::Compensating
        );

        // The first inverse settles too and the request finally completes.
        settle_inverse_confirmed(&ctx, &first_inverse_id, "m-first").await;
        let resumed = resume_deferred_undo_requests(&ctx).await.expect("resume");
        assert_eq!(resumed, 1);
        assert_eq!(
            entry_results(&ctx, &request_id).await,
            vec![UndoEntryResult::Done, UndoEntryResult::Done],
            "both queued inverses must settle after the restart"
        );
        assert_eq!(request_state(&ctx, &request_id).await, UndoState::Done);
    }

    /// Regression: a stale compensation error must not clobber
    /// a concurrent pass that recovered the same entry.
    ///
    /// The interleaving reproduced here is exactly the one
    /// `process_request` allows, because compensation and failure settlement
    /// are separate transactions:
    ///
    /// 1. pass A classifies the entry, then its compensation fails because the
    ///    target identity is not resolved yet;
    /// 2. a sync fills in the target identity between the two transactions;
    /// 3. pass B re-classifies and now queues the inverse successfully,
    ///    claiming the entry (`inverse_op_id` set, result back to `pending`);
    /// 4. pass A's old error is finally recorded.
    ///
    /// Unconditionally writing `failed` in step 4 would discard the claim.
    /// Once every other entry was settled the parent request would become
    /// terminal `failed`, the resume worker would skip it, and the queued
    /// inverse could never settle the entry. The parent must stay
    /// `Compensating`, and the confirmed inverse must finish both the entry and
    /// the request as `Done`.
    #[tokio::test]
    async fn stale_compensation_error_does_not_clobber_a_recovered_inverse() {
        let root = std::env::temp_dir().join(format!("nuncio-undo-stale-{}", new_id()));
        let ctx = CoreContext::for_test(Paths::new(root))
            .await
            .expect("context");
        let request_id = new_id();
        let entry_id = new_id();
        let folder_id = "fld";

        let mut summary = MessageSummary::new(
            "m1",
            "acc",
            folder_id,
            "thread-m1",
            nuncio_core::models::now(),
        );
        summary.seen = true;
        let mut message = StoredMessage::new(summary);
        message.uid = Some(7);
        message.uidvalidity = Some(1);

        let entry_payload = serde_json::to_value(&UndoEntryState {
            id: "m1".into(),
            account_id: "acc".into(),
            folder_id: folder_id.into(),
            target_folder_id: Some(folder_id.into()),
            // Deliberately missing: the first compensation cannot build its
            // inverse until sync resolves the target identity.
            uid: None,
            permanent: false,
            seen: false,
            flagged: false,
            answered: false,
            forwarded: false,
            is_deleted_local: false,
            snapshot: StoredSnapshot::from(&message),
            attachments: Vec::new(),
            flag_writer_id: None,
        })
        .expect("entry payload");
        let op_payload = serde_json::to_value(SetFlagsOp {
            folder_id: folder_id.into(),
            uids: vec![7],
            seen: Some(true),
            flagged: None,
            answered: None,
            forwarded: None,
            source_uidvalidity: Some(1),
            previous: Vec::new(),
            message_ids: vec!["m1".into()],
            remote_ids: Vec::new(),
            outcomes: vec![OpMessageOutcome {
                message_id: "m1".into(),
                source_uid: None,
                state: OpMessageState::Confirmed,
                target_folder_id: Some(folder_id.into()),
                target_uid: None,
                target_uidvalidity: None,
                target_remote_id: None,
                attempt_remote_id: None,
                copy_phase: Default::default(),
                delete_flag_phase: Default::default(),
                expunge_phase: Default::default(),
            }],
        })
        .expect("op payload");
        let account = Account {
            id: "acc".into(),
            provider: ProviderKind::Outlook,
            email: "test@example.com".into(),
            display_name: "Test".into(),
            color: "#000000".into(),
            auth_kind: AuthKind::Oauth,
            oauth_provider: Some(OAuthProvider::Microsoft),
            imap: None,
            smtp: None,
            pop3: None,
            caldav_url: None,
            carddav_url: None,
            sent_copy: None,
            sync_window_days: 90,
            mail_enabled: true,
            calendar_enabled: false,
            contacts_enabled: false,
            tasks_enabled: false,
            notifications_enabled: true,
            include_in_unified: true,
            sort_order: 0,
            state: AccountState::Ok,
            state_message: None,
            capabilities: Default::default(),
            created_at: nuncio_core::models::now(),
        };

        ctx.db
            .write({
                let request_id = request_id.clone();
                let entry_id = entry_id.clone();
                move |db| {
                    repo::accounts::insert(db, &account)?;
                    repo::folders::insert(
                        db,
                        &Folder::new(folder_id, "acc", "Inbox", FolderRole::Inbox),
                    )?;
                    repo::messages::upsert(db, &message)?;
                    repo::pending_ops::enqueue_with_id(
                        db,
                        "op-m1",
                        NewPendingOp::new("acc", KIND_SET_FLAGS, op_payload, "mark read"),
                    )?;
                    repo::pending_ops::mark_done(db, "op-m1")?;
                    repo::undo::create_request(
                        db,
                        NewUndoRequest::new(
                            request_id.clone(),
                            "acc",
                            "setSeen",
                            Some(true),
                            vec!["op-m1".into()],
                        ),
                        &[NewUndoEntry {
                            id: entry_id.clone(),
                            request_id: request_id.clone(),
                            account_id: "acc".into(),
                            message_id: "m1".into(),
                            original_folder_id: folder_id.into(),
                            target_folder_id: Some(folder_id.into()),
                            target_uid: None,
                            target_uidvalidity: Some(1),
                            target_graph_id: None,
                            permanent: false,
                            op_ids: vec!["op-m1".into()],
                            evidence_missing: false,
                            payload: entry_payload,
                            ordinal: 0,
                        }],
                    )?;
                    Ok(())
                }
            })
            .await
            .expect("seed request");

        // Pass A classifies the entry as compensable.
        let prepared_a = ctx
            .db
            .write({
                let request_id = request_id.clone();
                move |db| {
                    Ok::<_, AppError>(prepare_request(db, &request_id)?.expect("prepared"))
                }
            })
            .await
            .expect("prepare A");
        assert_eq!(prepared_a.compensate, vec![entry_id.clone()]);
        assert_eq!(prepared_a.attempt, 1);

        // Pass A's compensation fails: the target identity is still unknown.
        let error = ctx
            .db
            .write({
                let entry_id = entry_id.clone();
                move |db| compensate_entry(db, &entry_id)
            })
            .await
            .expect_err("the target identity is missing");
        let reason = error.message();

        // Between the two transactions, sync resolves the target identity.
        ctx.db
            .write({
                let entry_id = entry_id.clone();
                move |db| {
                    repo::undo::update_entry_identity(
                        db,
                        &entry_id,
                        Some(folder_id),
                        Some(7),
                        Some(1),
                        None,
                    )
                }
            })
            .await
            .expect("sync resolves the target identity");

        // Pass B starts a new generation and re-classifies the entry.
        let prepared_b = ctx
            .db
            .write({
                let request_id = request_id.clone();
                move |db| {
                    Ok::<_, AppError>(prepare_request(db, &request_id)?.expect("prepared"))
                }
            })
            .await
            .expect("prepare B");
        assert_eq!(prepared_b.compensate, vec![entry_id.clone()]);
        assert_eq!(
            prepared_b.attempt, 2,
            "a second pass must bump the request attempt"
        );

        // If pass A's error lands before pass B claims the entry, the attempt
        // generation alone must keep the entry retryable.
        ctx.db
            .write({
                let entry_id = entry_id.clone();
                let request_id = request_id.clone();
                let reason = reason.clone();
                move |db| {
                    record_compensation_failure(
                        db,
                        &entry_id,
                        &request_id,
                        prepared_a.attempt,
                        &reason,
                    )
                }
            })
            .await
            .expect("stale failure is ignored");
        assert_eq!(
            entry_results(&ctx, &request_id).await,
            vec![UndoEntryResult::Pending],
            "an error from a superseded attempt must not fail the entry"
        );

        // Pass B now queues the inverse successfully.
        ctx.db
            .write({
                let entry_id = entry_id.clone();
                move |db| compensate_entry(db, &entry_id)
            })
            .await
            .expect("pass B queues the inverse");
        let entries = ctx
            .db
            .read({
                let request_id = request_id.clone();
                move |db| repo::undo::list_entries(db, &request_id)
            })
            .await
            .expect("entries");
        assert_eq!(entries[0].result, UndoEntryResult::Pending);
        let inverse_id = entries[0]
            .inverse_op_id
            .clone()
            .expect("pass B must have queued an inverse");

        // Pass A's old error now lands after the inverse was claimed.
        ctx.db
            .write({
                let entry_id = entry_id.clone();
                let request_id = request_id.clone();
                let reason = reason.clone();
                move |db| {
                    record_compensation_failure(
                        db,
                        &entry_id,
                        &request_id,
                        prepared_a.attempt,
                        &reason,
                    )
                }
            })
            .await
            .expect("stale failure is ignored");

        // Pass A finalizes too: it must not commit the request terminal.
        let outcome = ctx
            .db
            .write({
                let request_id = request_id.clone();
                move |db| finalize_request(db, &request_id)
            })
            .await
            .expect("finalize A");
        assert_eq!(outcome.state, UndoState::Compensating);
        let entries = ctx
            .db
            .read({
                let request_id = request_id.clone();
                move |db| repo::undo::list_entries(db, &request_id)
            })
            .await
            .expect("entries");
        assert_eq!(entries[0].result, UndoEntryResult::Pending);
        assert_eq!(
            entries[0].inverse_op_id.as_deref(),
            Some(inverse_id.as_str()),
            "the stale error must not discard the inverse claim"
        );
        assert_eq!(
            request_state(&ctx, &request_id).await,
            UndoState::Compensating,
            "the parent must stay resumable while the queued inverse is unsettled"
        );
        let resumable = ctx
            .db
            .read({
                let request_id = request_id.clone();
                move |db| {
                    Ok(repo::undo::resumable_requests(db, 32)?
                        .into_iter()
                        .any(|request| request.id == request_id))
                }
            })
            .await
            .expect("resumable");
        assert!(resumable, "the resume worker must still own the request");

        // The inverse settles with a confirmed receipt and finishes everything.
        settle_inverse_confirmed(&ctx, &inverse_id, "m1").await;
        let resumed = resume_deferred_undo_requests(&ctx).await.expect("resume");
        assert_eq!(resumed, 1);
        assert_eq!(
            entry_results(&ctx, &request_id).await,
            vec![UndoEntryResult::Done],
            "the confirmed inverse must settle the entry automatically"
        );
        assert_eq!(request_state(&ctx, &request_id).await, UndoState::Done);
    }
}
