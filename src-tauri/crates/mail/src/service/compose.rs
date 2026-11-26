//! Composer commands: new messages, replies/forwards, drafts (local + IMAP
//! Drafts folder), attachments, recipient suggestions and sending.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use nuncio_core::events::MailChangeKind;
use nuncio_core::models::{
    Account, Address, Draft, DraftAttachment, Folder, FolderRole, Identity, Importance, Language,
    MessageSummary, NewPendingOp, OutboxDraftCleanup, PendingOpState, ProviderKind,
    RemoteDraftAttribution, RemoteDraftIdentity, ReplyMode, Signature, StoredMessage,
};
use nuncio_core::{events, repo, AppError, Result};
use tracing::{debug, warn};

use crate::ops::{
    self, AppendDraftOp, DeleteRemoteDraftOp, RemoteDraftRef, SetFlagsOp, KIND_APPEND_DRAFT,
    KIND_DELETE_REMOTE_DRAFT,
};

use super::MailService;

const MAX_INLINE_BYTES: usize = 25 * 1024 * 1024;
const MAX_DRAFT_BYTES: u64 = 100 * 1024 * 1024;
/// Remote path of the local Drafts folder used when the server has none.
const LOCAL_DRAFTS_PATH: &str = "local:drafts";

/// How the remote copy of a draft is handled when the draft is discarded or
/// sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteDraftCleanup {
    /// A delete operation was queued (or already exists). The executor removes
    /// the local projection only after the server confirms.
    Queued,
    /// An in-flight APPEND owns the cleanup (it persisted `discardRequested`).
    /// Queuing another delete here would create a second cleanup path.
    Deferred,
    /// A remote copy exists but cannot be attributed to this client (no
    /// APPENDUID and no explicit binding). It must be kept, not guessed at.
    Unconfirmed,
    /// No remote copy is known.
    None,
}

/// A side-effect-free cleanup decision for a draft being sent.
///
/// The decision is computed with reads only. It is applied inside the outbox
/// insert transaction, so a failed send staging/insert cannot suppress the
/// draft or delete its remote copy. `Queued` is only a candidate: the applying
/// transaction re-checks whether an in-flight APPEND took ownership meanwhile.
#[derive(Debug, Clone)]
enum SendDraftCleanup {
    Queued(DeleteRemoteDraftOp),
    Unconfirmed,
    None,
}

/// Runs inside the outbox insert transaction. It returns the cleanup
/// classification that was actually applied, so the persisted outbox row and
/// the side effects cannot disagree.
type OutboxEnqueueHook =
    Box<dyn FnOnce(&rusqlite::Connection) -> Result<OutboxDraftCleanup> + Send>;

/// The newest durable evidence about who owns a draft's remote copy.
#[derive(Debug, Clone)]
enum AppendOwnership {
    /// The server returned an `APPENDUID` to this attempt.
    Confirmed {
        folder_id: String,
        uid: i64,
        uidvalidity: i64,
        /// The copy this APPEND explicitly replaced, when it carried one. Lets
        /// a save tell "this receipt superseded the old explicit binding" from
        /// "the user explicitly opened a different, newer copy". The generation
        /// is part of the identity: a reused UID in another UIDVALIDITY is a
        /// different message.
        superseded: Option<(String, i64, Option<i64>)>,
    },
    /// A Graph create receipt: the provider returned this exact message id to
    /// this client's upload. It is the Graph analogue of an `APPENDUID` and
    /// the only upload evidence that authorizes a delete.
    GraphConfirmed {
        folder_id: String,
        remote_id: String,
    },
    /// A settled attempt only found a same-header copy; no UID is attributable.
    CandidateOnly,
}

/// Scan the draft's APPEND operations for the newest durable ownership
/// evidence. Used inside write transactions as well as from async callers.
fn append_ownership_from(
    conn: &rusqlite::Connection,
    account_id: &str,
    draft_id: &str,
) -> Result<Option<AppendOwnership>> {
    let mut latest = None;
    for op in repo::pending_ops::list(conn, Some(account_id))? {
        if op.kind != KIND_APPEND_DRAFT {
            continue;
        }
        let Ok(payload) = repo::pending_ops::payload::<AppendDraftOp>(&op) else {
            continue;
        };
        if payload.draft_id != draft_id {
            continue;
        }
        if payload.candidate_only {
            latest = Some(AppendOwnership::CandidateOnly);
        } else if let Some(remote_id) = payload
            .appended_remote_id
            .clone()
            .filter(|value| !value.trim().is_empty())
        {
            // Graph has no UID; the returned message id is the receipt.
            latest = Some(AppendOwnership::GraphConfirmed {
                folder_id: payload.folder_id,
                remote_id,
            });
        } else if let (Some(uid), Some(uidvalidity)) =
            (payload.appended_uid, payload.appended_uidvalidity)
        {
            latest = Some(AppendOwnership::Confirmed {
                folder_id: payload.folder_id,
                uid,
                uidvalidity,
                superseded: payload.replaces.map(|replaces| {
                    (
                        replaces.folder_id,
                        replaces.uid,
                        replaces.source_uidvalidity,
                    )
                }),
            });
        }
    }
    Ok(latest)
}

/// Cancel the draft's queued APPENDs, or move an in-flight one to the cleanup
/// phase. Returns whether the executor now owns a cleanup.
///
/// The caller must have already suppressed the draft; this helper is shared by
/// the discard path and the send path's outbox transaction.
fn cancel_pending_appends_from(
    conn: &rusqlite::Connection,
    account_id: &str,
    draft_id: &str,
) -> Result<bool> {
    let mut cleanup = false;
    for op in repo::pending_ops::list(conn, Some(account_id))? {
        if op.kind != KIND_APPEND_DRAFT
            || !matches!(
                op.state,
                PendingOpState::Pending | PendingOpState::InProgress
            )
        {
            continue;
        }
        let Ok(mut payload) = repo::pending_ops::payload::<AppendDraftOp>(&op) else {
            continue;
        };
        if payload.draft_id != draft_id {
            continue;
        }
        match payload.append_state {
            // The literal provably never left the client.
            crate::ops::AppendState::Idle => {
                repo::pending_ops::cancel(conn, &op.id)?;
            }
            // The command may be on the wire; persist the cleanup intent and
            // let the executor settle it after the upload finishes (or after
            // verification).
            crate::ops::AppendState::Preparing
            | crate::ops::AppendState::Unknown
            | crate::ops::AppendState::Acked => {
                if !payload.discard_requested {
                    payload.discard_requested = true;
                    let value = serde_json::to_value(&payload).map_err(|error| {
                        AppError::internal(format!("cannot encode appendDraft: {error}"))
                    })?;
                    repo::pending_ops::update_payload(conn, &op.id, &value)?;
                }
                if op.state == PendingOpState::Pending {
                    // Bring the cleanup forward; a running worker owns the row
                    // and will see the flag itself.
                    repo::pending_ops::defer(conn, &op.id, nuncio_core::models::now())?;
                }
                cleanup = true;
            }
        }
    }
    Ok(cleanup)
}

/// The durable inputs a send needs to re-decide its cleanup inside the outbox
/// transaction.
///
/// The plan computed before the transaction is only a snapshot. An APPEND that
/// completes between that read and the commit changes the attribution, so the
/// applying transaction recomputes the plan from the state it sees under the
/// write lock and persists *that* decision.
#[derive(Debug, Clone)]
struct SendCleanupContext {
    account_id: String,
    draft_id: String,
    graph: bool,
    /// The composer's identity snapshot; only a fallback for an older build.
    pinned: Option<RemoteDraftIdentity>,
}

impl SendCleanupContext {
    fn new(account: &Account, draft: &Draft) -> Option<Self> {
        let draft_id = draft.id.clone()?;
        Some(Self {
            account_id: account.id.clone(),
            draft_id,
            graph: crate::graph::uses_graph(account),
            pinned: draft.remote_draft.clone(),
        })
    }
}

/// Apply a send's draft cleanup inside the outbox insert transaction.
///
/// Suppression, cancellation and the remote-delete intent commit together with
/// the outbox row: a failed insert rolls all of them back. The plan is
/// regenerated here (not taken from the caller) so an APPEND receipt that
/// landed after the pre-transaction read is honoured instead of deleting a
/// stale UID or leaving the new copy behind.
fn apply_send_cleanup(
    conn: &rusqlite::Connection,
    context: &SendCleanupContext,
) -> Result<OutboxDraftCleanup> {
    let account_id = context.account_id.as_str();
    let draft_id = context.draft_id.as_str();
    repo::drafts::set_sync_suppressed(conn, draft_id, true)?;
    if cancel_pending_appends_from(conn, account_id, draft_id)? {
        // An upload may already be on the wire; it owns the cleanup and a
        // second delete would race it.
        return Ok(OutboxDraftCleanup::Deferred);
    }
    match plan_remote_draft_cleanup_tx(conn, context)? {
        SendDraftCleanup::Queued(delete) => {
            if !pending_delete_from(conn, account_id, &delete.folder_id, delete.uid)? {
                let value = serde_json::to_value(delete).map_err(|error| {
                    AppError::internal(format!("cannot encode deleteRemoteDraft: {error}"))
                })?;
                repo::pending_ops::enqueue(
                    conn,
                    NewPendingOp::new(
                        account_id,
                        KIND_DELETE_REMOTE_DRAFT,
                        value,
                        "Remove remote draft",
                    ),
                )?;
            }
            Ok(OutboxDraftCleanup::Queued)
        }
        SendDraftCleanup::Unconfirmed => Ok(OutboxDraftCleanup::Unconfirmed),
        SendDraftCleanup::None => Ok(OutboxDraftCleanup::None),
    }
}

/// The synchronous twin of [`super::MailService::plan_remote_draft_cleanup`],
/// evaluated against the transaction's own view of the queue.
fn plan_remote_draft_cleanup_tx(
    conn: &rusqlite::Connection,
    context: &SendCleanupContext,
) -> Result<SendDraftCleanup> {
    let ownership = append_ownership_from(conn, &context.account_id, &context.draft_id)?;
    if matches!(ownership, Some(AppendOwnership::CandidateOnly)) {
        return Ok(SendDraftCleanup::Unconfirmed);
    }
    // Read the identity inside the transaction: a save/receipt may have
    // replaced the snapshot the composer carried.
    let pinned = repo::drafts::get(conn, &context.draft_id)?
        .and_then(|row| row.remote_draft)
        .or_else(|| context.pinned.clone())
        // A composer that switched accounts can still carry the old account's
        // identity snapshot. It names a folder in another mailbox, so it must
        // never authorize a delete here.
        .filter(|identity| identity_belongs_to_account(conn, identity, &context.account_id));
    if context.graph {
        // Only two identities may authorize a Graph delete:
        // * the message id returned to this draft's own create (the Graph
        //   analogue of `APPENDUID`), and
        // * the exact server row the user opened.
        // A same-header copy is never enough: another client can write one,
        // and deleting it would erase their draft.
        if let Some(AppendOwnership::GraphConfirmed {
            folder_id,
            remote_id,
        }) = ownership
        {
            let uid = crate::sync::messages::synthetic_uid(&remote_id);
            return Ok(SendDraftCleanup::Queued(DeleteRemoteDraftOp {
                folder_id,
                uid,
                remote_message_id: Some(remote_id),
                draft_id: Some(context.draft_id.clone()),
                source_uidvalidity: None,
                message_id: Some(draft_message_id(&context.draft_id)),
                outcome_unknown: false,
                quarantined: false,
                quarantine_reason: None,
            }));
        }
        if let Some(identity) = pinned.as_ref() {
            if identity.attribution == RemoteDraftAttribution::Candidate {
                return Ok(SendDraftCleanup::Unconfirmed);
            }
            if identity.is_deletable_graph() {
                let remote_id = identity.remote_id.clone().unwrap_or_default();
                let uid = identity
                    .uid
                    .unwrap_or_else(|| crate::sync::messages::synthetic_uid(&remote_id));
                return Ok(SendDraftCleanup::Queued(DeleteRemoteDraftOp {
                    folder_id: identity.folder_id.clone().unwrap_or_default(),
                    uid,
                    remote_message_id: Some(remote_id),
                    draft_id: Some(context.draft_id.clone()),
                    source_uidvalidity: identity.uidvalidity,
                    message_id: identity
                        .projection_id
                        .clone()
                        .or_else(|| Some(draft_message_id(&context.draft_id))),
                    outcome_unknown: false,
                    quarantined: false,
                    quarantine_reason: None,
                }));
            }
        }
        // Deliberately no lookup of a row by `remote_draft_message_id`: that is
        // a mutable local projection, and a sync can rebind it to an external
        // same-header copy. Only the pinned identity above or this draft's own
        // create receipt may authorize a Graph delete.
        //
        // A same-header copy is a candidate only: report it (so the caller
        // keeps a projection) but never derive a delete from it.
        return Ok(
            match find_header_remote_draft_tx(conn, &context.account_id, &context.draft_id)? {
                Some(_) => SendDraftCleanup::Unconfirmed,
                None => SendDraftCleanup::None,
            },
        );
    }
    if let Some(AppendOwnership::Confirmed {
        folder_id,
        uid,
        uidvalidity,
        ..
    }) = ownership
    {
        return Ok(SendDraftCleanup::Queued(DeleteRemoteDraftOp {
            folder_id,
            uid,
            remote_message_id: None,
            draft_id: Some(context.draft_id.clone()),
            source_uidvalidity: Some(uidvalidity),
            message_id: Some(draft_message_id(&context.draft_id)),
            outcome_unknown: false,
            quarantined: false,
            quarantine_reason: None,
        }));
    }
    if let Some(identity) = pinned {
        match identity.attribution {
            RemoteDraftAttribution::Candidate => return Ok(SendDraftCleanup::Unconfirmed),
            RemoteDraftAttribution::Confirmed | RemoteDraftAttribution::Explicit
                if identity.is_deletable() =>
            {
                return Ok(SendDraftCleanup::Queued(DeleteRemoteDraftOp {
                    folder_id: identity.folder_id.clone().unwrap_or_default(),
                    uid: identity.uid.unwrap_or_default(),
                    remote_message_id: identity.remote_id.clone(),
                    draft_id: Some(context.draft_id.clone()),
                    source_uidvalidity: identity.uidvalidity,
                    message_id: identity
                        .projection_id
                        .clone()
                        .or_else(|| Some(draft_message_id(&context.draft_id))),
                    outcome_unknown: false,
                    quarantined: false,
                    quarantine_reason: None,
                }));
            }
            _ => {}
        }
    }
    match find_header_remote_draft_tx(conn, &context.account_id, &context.draft_id)? {
        Some(_) => Ok(SendDraftCleanup::Unconfirmed),
        None => Ok(SendDraftCleanup::None),
    }
}

/// Whether a pinned identity's folder belongs to `account_id`.
///
/// The duplicate check is not enough: the composer can carry a snapshot from
/// before an account switch, and the same path/UID may exist in both mailboxes.
fn identity_belongs_to_account(
    conn: &rusqlite::Connection,
    identity: &RemoteDraftIdentity,
    account_id: &str,
) -> bool {
    let Some(folder_id) = identity.folder_id.as_deref() else {
        return false;
    };
    repo::folders::get(conn, folder_id)
        .ok()
        .flatten()
        .is_some_and(|folder| folder.account_id == account_id)
}

/// A remote copy matched only by the draft's stable Message-ID, read from the
/// local index (never the network) so it can run inside a write transaction.
fn find_header_remote_draft_tx(
    conn: &rusqlite::Connection,
    account_id: &str,
    draft_id: &str,
) -> Result<Option<StoredMessage>> {
    let Some(folder) = repo::folders::by_role(conn, account_id, FolderRole::Drafts)? else {
        return Ok(None);
    };
    if folder
        .remote_path
        .starts_with(crate::sync::folders::LOCAL_FOLDER_PREFIX)
    {
        return Ok(None);
    }
    let header = draft_message_id(draft_id);
    let rows = repo::messages::by_message_id(conn, &header, Some(account_id))?;
    Ok(rows
        .into_iter()
        .find(|row| row.summary.folder_id == folder.id))
}

/// The exact `(folder, UID)` a pre-`draftId` remote-draft DELETE may name.
///
/// Legacy payloads only carry folder+UID, so associating one with this draft
/// requires evidence about the draft's own server copy. Sources, strongest
/// first:
///
/// 1. the pinned identity (the copy this draft supersedes);
/// 2. the Graph message row the composer was opened from;
/// 3. the local Drafts projection, matched by the draft's stable Message-ID.
///
/// When the evidence is missing, or the projections name more than one copy,
/// this returns `None`. The caller must then quarantine every legacy DELETE
/// (keep the intent, never execute it): leaving one stale draft behind is safer
/// than deleting a copy the user keeps, and cancelling it outright would
/// silently discard the intent.
fn draft_remote_copy_tx(
    conn: &rusqlite::Connection,
    account_id: &str,
    draft: &Draft,
) -> Result<Option<(String, i64)>> {
    if let Some(identity) = draft
        .remote_draft
        .as_ref()
        .filter(|identity| identity_belongs_to_account(conn, identity, account_id))
    {
        if let (Some(folder_id), Some(uid)) = (identity.folder_id.as_ref(), identity.uid) {
            return Ok(Some((folder_id.clone(), uid)));
        }
    }
    if let Some(message_id) = draft.remote_draft_message_id.as_deref() {
        if let Some(stored) = repo::messages::get(conn, message_id)?
            .filter(|stored| stored.summary.account_id == account_id)
        {
            if let Some(uid) = stored.uid {
                return Ok(Some((stored.summary.folder_id.clone(), uid)));
            }
        }
    }
    let Some(draft_id) = draft.id.as_deref() else {
        return Ok(None);
    };
    // `compose_save` projects every draft under its stable Message-ID; the id
    // is also the row id, so both lookups usually find the same row.
    let header = draft_message_id(draft_id);
    let mut copies: Vec<(String, i64)> = Vec::new();
    let mut push = |folder_id: &str, uid: i64| {
        let copy = (folder_id.to_string(), uid);
        if !copies.contains(&copy) {
            copies.push(copy);
        }
    };
    if let Some(stored) = repo::messages::get(conn, &header)? {
        if stored.summary.account_id == account_id {
            if let Some(uid) = stored.uid {
                push(&stored.summary.folder_id, uid);
            }
        }
    }
    for stored in repo::messages::by_message_id(conn, &header, Some(account_id))? {
        if let Some(uid) = stored.uid {
            push(&stored.summary.folder_id, uid);
        }
    }
    match copies.as_slice() {
        [only] => Ok(Some(only.clone())),
        _ => Ok(None),
    }
}

/// Cancel the queued replacement DELETEs that belong to a draft whose account
/// changed, and isolate legacy deletes that cannot be attributed.
///
/// A Graph replacement is one logical operation split into a paired DELETE and
/// APPEND. Cancelling only the APPEND would leave the old account's DELETE
/// queued, so the worker would remove the remote draft the user still keeps
/// (`accounts` switched the *sending* identity; the old copy is deliberately
/// left alone). IMAP replacements do not queue a separate DELETE -- the old
/// copy travels in the APPEND's `replaces` -- so this only matches Graph pairs
/// and any draft-bound cleanup in the old account.
///
/// A pre-`draftId` DELETE is ambiguous. When it names this draft's exact copy
/// it is cancelled like a new payload; otherwise it is *quarantined* instead of
/// being left executable. Leaving it queued (the previous behaviour) let the
/// old account's worker send an unattributed delete for a draft the switch
/// deliberately kept.
fn cancel_pending_draft_deletes_from(
    conn: &rusqlite::Connection,
    account_id: &str,
    draft_id: &str,
) -> Result<()> {
    // The draft row still names the old account here: `save` runs after this
    // cleanup in the same transaction. Its remote identity is therefore the
    // authoritative answer to "which remote copy did this draft supersede".
    let remote_copy = repo::drafts::get(conn, draft_id)?
        .filter(|draft| draft.account_id == account_id)
        .map(|draft| draft_remote_copy_tx(conn, account_id, &draft))
        .transpose()?
        .flatten();
    for op in repo::pending_ops::list(conn, Some(account_id))? {
        if op.kind != KIND_DELETE_REMOTE_DRAFT
            || !matches!(
                op.state,
                PendingOpState::Pending | PendingOpState::InProgress
            )
        {
            continue;
        }
        let Ok(payload) = repo::pending_ops::payload::<DeleteRemoteDraftOp>(&op) else {
            continue;
        };
        match payload.draft_id.as_deref() {
            // New payloads name the draft directly.
            Some(id) if id == draft_id => cancel_draft_delete(conn, &op, &payload)?,
            // Another draft's cleanup is not this switch's business.
            Some(_) => {}
            // Legacy payload: only an exact folder+UID association with this
            // draft is safe to cancel. Without one it may belong to another
            // draft -- or to this draft's retained old copy -- so it must not
            // be executed by inference.
            None => {
                let belongs_to_draft = remote_copy.as_ref().is_some_and(|(folder_id, uid)| {
                    payload.folder_id == *folder_id && payload.uid == *uid
                });
                if belongs_to_draft {
                    cancel_draft_delete(conn, &op, &payload)?;
                } else {
                    quarantine_legacy_draft_delete(conn, &op, &payload)?;
                }
            }
        }
    }
    Ok(())
}

/// Isolate one legacy DELETE that cannot be attributed to the switching draft.
///
/// The intent is preserved and made unexecutable: the row is `Cancelled` (a
/// state older builds understand and never select), the payload carries
/// `quarantined` + `quarantineReason` as the audit record, and pruning retains
/// it. Using an unknown state string here would be unsafe on downgrade: an
/// older `row_to_op` falls back to `Pending` for unrecognised values and would
/// send the delete. The folder is queued for a re-sync so the local projection
/// converges with whatever is actually on the server rather than with a guess.
///
/// A row already claimed by a worker cannot be quarantined (the request may be
/// on the wire); it takes the in-flight path, which records `outcomeUnknown`
/// and reconciles the folder.
fn quarantine_legacy_draft_delete(
    conn: &rusqlite::Connection,
    op: &nuncio_core::models::PendingOp,
    payload: &DeleteRemoteDraftOp,
) -> Result<()> {
    if op.state == PendingOpState::InProgress {
        return cancel_draft_delete(conn, op, payload);
    }
    let mut payload = payload.clone();
    payload.quarantined = true;
    payload.quarantine_reason = Some(format!(
        "legacy deleteRemoteDraft for folder {} uid {} could not be attributed to a draft after an account switch",
        payload.folder_id, payload.uid
    ));
    let value = serde_json::to_value(&payload)
        .map_err(|error| AppError::internal(format!("cannot encode deleteRemoteDraft: {error}")))?;
    repo::pending_ops::update_payload(conn, &op.id, &value)?;
    if repo::pending_ops::quarantine(conn, &op.id)? == 0 {
        // A worker claimed the row between the listing and this update. Its
        // request may already be on the wire, so settle it as an unknown
        // outcome instead of pretending it was never sent.
        let latest = repo::pending_ops::get_required(conn, &op.id)?;
        if matches!(
            latest.state,
            PendingOpState::Pending | PendingOpState::InProgress
        ) {
            return cancel_draft_delete(conn, &latest, &payload);
        }
        return Ok(());
    }
    if repo::folders::get(conn, &payload.folder_id)?.is_some() {
        crate::ops::executor::force_full_resync(conn, &payload.folder_id)?;
    }
    warn!(
        op = %op.id,
        folder = %payload.folder_id,
        uid = payload.uid,
        "quarantined an unattributable legacy deleteRemoteDraft during an account switch"
    );
    Ok(())
}

/// Cancel one remote-draft delete without pretending an in-flight attempt
/// never reached the server.
///
/// A `pending` row was never claimed by a worker, so `cancelled` is exact. An
/// `inProgress` row may already have written the delete to the wire; cancelling
/// it stops only further retries, not the request, which can still succeed.
/// For those, persist `outcomeUnknown` on the payload (the row is retained by
/// `prune_finished`) and re-fetch the folder so the next sync reconciles the
/// local projection with what actually happened remotely.
fn cancel_draft_delete(
    conn: &rusqlite::Connection,
    op: &nuncio_core::models::PendingOp,
    payload: &DeleteRemoteDraftOp,
) -> Result<()> {
    if op.state == PendingOpState::InProgress {
        if !payload.outcome_unknown {
            let mut payload = payload.clone();
            payload.outcome_unknown = true;
            let value = serde_json::to_value(&payload).map_err(|error| {
                AppError::internal(format!("cannot encode deleteRemoteDraft: {error}"))
            })?;
            repo::pending_ops::update_payload(conn, &op.id, &value)?;
        }
        if repo::folders::get(conn, &payload.folder_id)?.is_some() {
            crate::ops::executor::force_full_resync(conn, &payload.folder_id)?;
        }
    }
    repo::pending_ops::cancel(conn, &op.id)?;
    Ok(())
}

/// Whether a delete for this exact server UID is already queued.
fn pending_delete_from(
    conn: &rusqlite::Connection,
    account_id: &str,
    folder_id: &str,
    uid: i64,
) -> Result<bool> {
    for op in repo::pending_ops::list(conn, Some(account_id))? {
        if op.kind != KIND_DELETE_REMOTE_DRAFT
            || !matches!(
                op.state,
                PendingOpState::Pending | PendingOpState::InProgress
            )
        {
            continue;
        }
        let Ok(payload) = repo::pending_ops::payload::<DeleteRemoteDraftOp>(&op) else {
            continue;
        };
        if payload.folder_id == folder_id && payload.uid == uid {
            return Ok(true);
        }
    }
    Ok(false)
}

impl MailService {
    /* ───────────────────────── creation ───────────────────────── */

    /// New blank draft with the default identity and signature.
    pub async fn compose_create(&self, account_id: Option<&str>) -> Result<Draft> {
        let account = match account_id {
            Some(id) => self.account(id).await?,
            None => self.default_mail_account().await?,
        };
        let identity = self.identity_for(&account, &[]).await?;
        let signature = self
            .signature_for_identity(&account.id, identity.signature_id.as_deref(), false)
            .await?;
        let html = format!("<p><br></p>{}", signature_block(signature.as_ref()));
        Ok(Draft {
            id: None,
            account_id: account.id.clone(),
            identity_id: Some(identity.id.clone()),
            from: identity_address(&identity),
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: String::new(),
            html,
            attachments: Vec::new(),
            reply_mode: None,
            source_message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            importance: Importance::Normal,
            request_read_receipt: false,
            remote_draft_message_id: None,
            remote_draft: None,
            sync_suppressed: false,
            updated_at: None,
        })
    }

    /// New draft pre-filled from a `mailto:` URL.
    pub async fn compose_from_mailto(&self, url: &str) -> Result<Draft> {
        let data = nuncio_mime::parse_mailto(url)?;
        let mut draft = self.compose_create(None).await?;
        draft.to = data.to;
        draft.cc = data.cc;
        draft.bcc = data.bcc;
        draft.subject = data.subject;
        if !data.body.trim().is_empty() {
            let signature_tail = draft
                .html
                .strip_prefix("<p><br></p>")
                .unwrap_or_default()
                .to_string();
            draft.html = format!(
                "{}{}{}",
                nuncio_mime::plain_text_to_html(&data.body),
                "<p><br></p>",
                signature_tail
            );
        }
        Ok(draft)
    }

    /// Reply / reply-all / forward / forward-as-attachment / edit-as-new.
    pub async fn compose_reply(&self, message_id: &str, mode: ReplyMode) -> Result<Draft> {
        let mut stored = self.stored(message_id).await?;
        let raw = self.raw_message(&stored).await?.bytes;
        stored = self.stored(message_id).await?;
        let parsed = nuncio_mime::parse(&raw)?;
        let account = self.account(&stored.summary.account_id).await?;

        let mut preferred = parsed.headers.to.clone();
        preferred.extend(parsed.headers.cc.clone());
        preferred.extend(parsed.headers.bcc.clone());
        let identity = self.identity_for(&account, &preferred).await?;

        let mut me: Vec<String> = {
            let account_id = account.id.clone();
            self.inner
                .ctx
                .db
                .read(move |db| repo::identities::list(db, Some(&account_id)))
                .await?
                .into_iter()
                .map(|identity| identity.email)
                .collect()
        };
        if me.is_empty() {
            me.push(account.email.clone());
        }
        let settings = self.inner.ctx.settings.current();
        let lang = match settings.language {
            Language::EnUs => "en-US",
            _ => "zh-CN",
        };
        let parts = nuncio_mime::build_reply(&parsed, mode, &me, lang, true);

        let signature = self
            .signature_for_identity(
                &account.id,
                identity.signature_id.as_deref(),
                !matches!(mode, ReplyMode::EditAsNew),
            )
            .await?;
        let mut html = match mode {
            ReplyMode::EditAsNew => parts.html.clone(),
            _ => insert_signature(&parts.html, signature.as_ref()),
        };
        if html.trim().is_empty() {
            html = format!("<p><br></p>{}", signature_block(signature.as_ref()));
        }

        let staging = self
            .inner
            .ctx
            .paths
            .staging_dir()
            .join("compose")
            .join(nuncio_core::models::new_id());
        tokio::fs::create_dir_all(&staging)
            .await
            .map_err(|error| AppError::io(format!("cannot create staging dir: {error}")))?;
        let mut attachments = Vec::new();
        let mut used_names: HashSet<String> = HashSet::new();

        if matches!(mode, ReplyMode::ForwardAsAttachment) {
            let filename = format!("{}.eml", safe_stem(&parsed.headers.subject));
            let dest = staging.join(&filename);
            tokio::fs::write(&dest, &raw).await.map_err(|error| {
                AppError::io(format!("cannot stage forwarded message: {error}"))
            })?;
            attachments.push(DraftAttachment {
                id: nuncio_core::models::new_id(),
                filename,
                mime_type: "message/rfc822".to_string(),
                size: raw.len() as u64,
                path: dest.to_string_lossy().to_string(),
                content_id: None,
            });
        } else {
            for attachment in &parts.attachments_to_forward {
                let bytes = match nuncio_mime::extract_part(&raw, &attachment.part) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        warn!(%error, filename = %attachment.filename, "could not stage forwarded attachment");
                        continue;
                    }
                };
                let filename = unique_name(&staging, &attachment.filename, &mut used_names).await;
                let dest = staging.join(&filename);
                tokio::fs::write(&dest, &bytes).await.map_err(|error| {
                    AppError::io(format!("cannot stage forwarded attachment: {error}"))
                })?;
                let content_id = if attachment.is_inline {
                    Some(
                        attachment
                            .content_id
                            .clone()
                            .unwrap_or_else(|| nuncio_mime::attachments::content_id_for(&filename)),
                    )
                } else {
                    None
                };
                attachments.push(DraftAttachment {
                    id: nuncio_core::models::new_id(),
                    filename,
                    mime_type: attachment.mime_type.clone(),
                    size: bytes.len() as u64,
                    path: dest.to_string_lossy().to_string(),
                    content_id,
                });
            }
        }

        Ok(Draft {
            id: None,
            account_id: account.id.clone(),
            identity_id: Some(identity.id.clone()),
            from: identity_address(&identity),
            to: parts.to,
            cc: parts.cc,
            bcc: Vec::new(),
            subject: parts.subject,
            html,
            attachments,
            reply_mode: Some(mode),
            source_message_id: Some(message_id.to_string()),
            in_reply_to: parts.in_reply_to,
            references: parts.references,
            importance: parsed.headers.importance,
            request_read_receipt: false,
            remote_draft_message_id: None,
            remote_draft: None,
            sync_suppressed: false,
            updated_at: None,
        })
    }

    /// Provider identity a draft's cached body was read from.
    ///
    /// A sync can rebind the local row to another server copy while the raw
    /// MIME is being read (the row id stays stable). The body then belongs to
    /// one provider message and the identity pinned for a later delete would
    /// name another. Comparing this key on both sides of the read detects it.
    fn draft_copy_key(
        stored: &StoredMessage,
    ) -> (String, String, Option<i64>, Option<i64>, Option<String>) {
        (
            stored.summary.account_id.clone(),
            stored.summary.folder_id.clone(),
            stored.uid,
            stored.uidvalidity,
            stored.gm_msgid.clone(),
        )
    }

    /// Whether the bytes the open actually read belong to the copy the row
    /// names now.
    ///
    /// The read reports the version encoded in the cache file name and the
    /// identity recorded with that path in the same snapshot. For a versioned
    /// file we recompute the version from the current copy key and the exact
    /// bytes: only the copy the bytes were published for can produce it. A
    /// legacy unversioned path falls back to the identity captured *before* the
    /// read, never to a binding re-queried afterwards (which a sync can move to
    /// another copy while the old bytes are still in memory).
    fn body_source_matches(stored: &StoredMessage, source: &super::RawMessage, raw: &[u8]) -> bool {
        if stored.body_path.is_none() {
            return false;
        }
        let Some(current) = repo::messages::body_identity_key(stored) else {
            return false;
        };
        match source.version.as_deref() {
            Some(version) => crate::sync::body::body_cache_version(Some(&current), raw) == version,
            None => source.identity.as_deref() == Some(current.as_str()),
        }
    }

    /// Open a message from the Drafts folder as an editable draft.
    pub async fn compose_open_draft(&self, message_id: &str) -> Result<Draft> {
        let mut stored = self.stored(message_id).await?;
        // The Drafts copy on the server can lag behind the local draft:
        // `compose_save` persists the latest edits locally before the
        // debounced APPEND runs. If this row is a projection of a local draft,
        // that local row is the newest version and must win; parsing the stale
        // server copy would overwrite edits and re-upload them on the next
        // save.
        if let Some(draft_id) = stored
            .message_id_header
            .as_deref()
            .and_then(crate::util::draft_id_from_header)
        {
            let id = draft_id.to_string();
            let local = self
                .inner
                .ctx
                .db
                .read(move |db| repo::drafts::get(db, &id))
                .await?;
            if let Some(mut draft) =
                local.filter(|draft| draft.account_id == stored.summary.account_id)
            {
                // Deliberately do *not* record the local projection's row id as
                // `remote_draft_message_id`: that row is a mutable projection a
                // sync can rebind to an external same-header copy, and a save
                // or send would then treat it as "the server row the user
                // opened" and authorize a delete. Only the durable identity
                // below (this draft's own APPEND/Graph-create receipt) may do
                // that.
                //
                // Pin the identity of the server copy while we are looking at
                // the exact row the user opened. Later saves must not resolve
                // it again from a projection that a sync can rebind.
                if draft.remote_draft.is_none() {
                    let pinned = self
                        .identity_for_open(&stored.summary.account_id, draft_id, &stored)
                        .await;
                    if let Some(identity) = pinned.as_ref() {
                        let id = draft_id.to_string();
                        let identity_for_db = identity.clone();
                        let changed = self
                            .inner
                            .ctx
                            .db
                            .write(move |db| {
                                repo::drafts::set_remote_draft(db, &id, Some(&identity_for_db))
                            })
                            .await?;
                        if changed > 0 {
                            draft.remote_draft = pinned;
                        }
                    }
                }
                return Ok(draft);
            }
        }
        // Read the body, then decide what the bytes are from. The read reports
        // the version of the cache file it actually used (a digest of the
        // provider copy and those exact bytes), so a rebind that keeps the old
        // `body_path` cannot be mistaken for a matching body.
        let opened_key = Self::draft_copy_key(&stored);
        let mut read = self.raw_message(&stored).await?;
        let mut refreshed = self.stored(message_id).await?;
        if Self::draft_copy_key(&refreshed) != opened_key {
            // The row was rebound while the body was read. Re-read the body so
            // the editor shows content that at least corresponds to the row's
            // current namespace, but never grant `Explicit`: the copy key
            // changed across the open, so the bytes may still belong to the
            // previous copy.
            read = self.raw_message(&refreshed).await?;
            refreshed = self.stored(message_id).await?;
        }
        // A file under the composer's staging root is this client's own
        // `draft.eml`, never a downloaded provider body. Refuse it even when a
        // pre-fix build stamped a provider identity on it: local bytes are not
        // evidence about any server copy.
        let local_staged_body = refreshed
            .body_path
            .as_deref()
            .is_some_and(|path| Path::new(path).starts_with(self.inner.ctx.paths.staging_dir()));
        // Only bytes proven to belong to the copy the row names now may
        // authorize a later delete. No cache, an unknown source or a stale
        // version all degrade to `Candidate`. The proof comes from what was
        // actually read, not from a binding re-queried after the read.
        let body_matches =
            !local_staged_body && Self::body_source_matches(&refreshed, &read, &read.bytes);
        let same_copy = Self::draft_copy_key(&refreshed) == opened_key && body_matches;
        stored = refreshed;
        let raw = read.bytes;
        let parsed = nuncio_mime::parse(&raw)?;
        let account = self.account(&stored.summary.account_id).await?;
        let preferred = parsed.headers.from.iter().cloned().collect::<Vec<_>>();
        let identity = self.identity_for(&account, &preferred).await?;

        let staging = self
            .inner
            .ctx
            .paths
            .staging_dir()
            .join("compose")
            .join(nuncio_core::models::new_id());
        tokio::fs::create_dir_all(&staging)
            .await
            .map_err(|error| AppError::io(format!("cannot create staging dir: {error}")))?;
        let mut used_names: HashSet<String> = HashSet::new();
        let mut attachments = Vec::new();
        for attachment in &parsed.attachments {
            let bytes = match nuncio_mime::extract_part(&raw, &attachment.part) {
                Ok(bytes) => bytes,
                Err(error) => {
                    warn!(%error, filename = %attachment.filename, "could not stage draft attachment");
                    continue;
                }
            };
            let filename = unique_name(&staging, &attachment.filename, &mut used_names).await;
            let dest = staging.join(&filename);
            tokio::fs::write(&dest, &bytes)
                .await
                .map_err(|error| AppError::io(format!("cannot stage draft attachment: {error}")))?;
            attachments.push(DraftAttachment {
                id: nuncio_core::models::new_id(),
                filename,
                mime_type: attachment.mime_type.clone(),
                size: bytes.len() as u64,
                path: dest.to_string_lossy().to_string(),
                content_id: if attachment.is_inline {
                    attachment.content_id.clone()
                } else {
                    None
                },
            });
        }
        let html = parsed
            .html
            .clone()
            .or_else(|| parsed.text.as_deref().map(nuncio_mime::plain_text_to_html))
            .unwrap_or_else(|| "<p><br></p>".to_string());
        // A row projected by `compose_save` carries its local draft id in the
        // Message-ID, so reopening edits the same draft instead of cloning it.
        let local_draft_id = stored
            .message_id_header
            .as_deref()
            .and_then(crate::util::draft_id_from_header)
            .map(str::to_string);

        Ok(Draft {
            id: local_draft_id,
            account_id: account.id.clone(),
            identity_id: Some(identity.id.clone()),
            from: identity_address(&identity),
            to: parsed.headers.to.clone(),
            cc: parsed.headers.cc.clone(),
            bcc: parsed.headers.bcc.clone(),
            subject: parsed.headers.subject.clone(),
            html,
            attachments,
            reply_mode: None,
            source_message_id: None,
            in_reply_to: parsed.headers.in_reply_to.clone(),
            references: parsed.headers.references.clone(),
            importance: parsed.headers.importance,
            request_read_receipt: !parsed.headers.disposition_notification_to.is_empty(),
            remote_draft_message_id: same_copy.then(|| message_id.to_string()),
            // Only a body whose provider identity stayed stable across the
            // read may authorize a later replace/delete. When the projection
            // was rebound, the content and the identity belong to different
            // copies: keep the content but grant a Candidate, which may not
            // delete anything.
            remote_draft: Some(if !same_copy {
                RemoteDraftIdentity::candidate(
                    Some(stored.summary.folder_id.clone()),
                    Some(message_id.to_string()),
                )
            } else if crate::graph::uses_graph(&account) {
                match stored
                    .gm_msgid
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                {
                    // The provider id is fixed here, at open time. Later saves
                    // must not re-read it from the (mutable) row.
                    Some(remote_id) => RemoteDraftIdentity::explicit_graph(
                        &stored.summary.folder_id,
                        &remote_id,
                        Some(message_id.to_string()),
                    ),
                    // No provider identity was known when the row was opened.
                    // Falling back to the local row id would name something the
                    // provider never issued; a candidate grants no delete right.
                    None => RemoteDraftIdentity::candidate(
                        Some(stored.summary.folder_id.clone()),
                        Some(message_id.to_string()),
                    ),
                }
            } else {
                RemoteDraftIdentity::explicit(
                    &stored.summary.folder_id,
                    stored.uid,
                    stored.uidvalidity,
                    Some(message_id.to_string()),
                )
            }),
            sync_suppressed: false,
            updated_at: None,
        })
    }

    pub async fn compose_get_draft(&self, draft_id: &str) -> Result<Draft> {
        let id = draft_id.to_string();
        self.inner
            .ctx
            .db
            .read(move |db| repo::drafts::get_required(db, &id))
            .await
    }

    /* ───────────────────────── saving ───────────────────────── */

    /// Persist the draft locally and queue a debounced IMAP Drafts APPEND.
    pub async fn compose_save(&self, draft: &Draft) -> Result<Draft> {
        let account = self.account(&draft.account_id).await?;
        let draft_id = draft
            .id
            .clone()
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(nuncio_core::models::new_id);
        let mut draft = draft.clone();
        draft.id = Some(draft_id.clone());

        // Move staged files into the draft directory so they survive rebuilds.
        let dir = self.inner.ctx.paths.draft_staging_dir(&draft_id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|error| AppError::io(format!("cannot create draft dir: {error}")))?;
        let mut used_names: HashSet<String> = HashSet::new();
        let mut attachments = Vec::new();
        for attachment in &draft.attachments {
            let source = PathBuf::from(&attachment.path);
            let metadata = tokio::fs::metadata(&source).await.map_err(|error| {
                AppError::io(format!(
                    "draft attachment {} is missing: {error}",
                    attachment.filename
                ))
            })?;
            if !metadata.is_file() {
                return Err(AppError::invalid_input(format!(
                    "draft attachment {} is not a file",
                    attachment.filename
                )));
            }
            let already_staged = source.parent() == Some(dir.as_path())
                && !used_names.contains(&attachment.filename.to_lowercase())
                && tokio::fs::metadata(&source).await.is_ok();
            let filename = if already_staged {
                used_names.insert(attachment.filename.to_lowercase());
                attachment.filename.clone()
            } else {
                unique_name(&dir, &attachment.filename, &mut used_names).await
            };
            let dest = dir.join(&filename);
            if source != dest {
                tokio::fs::copy(&source, &dest)
                    .await
                    .map_err(|error| AppError::io(format!("cannot stage draft file: {error}")))?;
            }
            attachments.push(DraftAttachment {
                id: if attachment.id.trim().is_empty() {
                    nuncio_core::models::new_id()
                } else {
                    attachment.id.clone()
                },
                filename,
                mime_type: if attachment.mime_type.trim().is_empty() {
                    mime_guess::from_path(&dest)
                        .first_or_octet_stream()
                        .to_string()
                } else {
                    attachment.mime_type.clone()
                },
                size: metadata.len(),
                path: dest.to_string_lossy().to_string(),
                content_id: attachment.content_id.clone(),
            });
        }
        draft.attachments = attachments;

        // Build the RFC822 message with a deterministic Message-ID so an
        // uploaded draft can be matched back to this local row.
        let identity = self
            .identity_for(&account, std::slice::from_ref(&draft.from))
            .await?;
        let header_id = draft_message_id(&draft_id);
        let outgoing = self.outgoing_from_draft(&draft, &identity, Some(header_id.clone()))?;
        // Drafts keep the Bcc header locally and in the server Drafts copy so
        // reopening a draft still shows every recipient.
        let (bytes, _) = nuncio_mime::build_message_with_bcc(outgoing)?;
        if bytes.len() as u64 > MAX_DRAFT_BYTES {
            return Err(AppError::invalid_input("draft exceeds 100 MB"));
        }
        let temp = dir.join("draft.eml.tmp");
        let final_path = dir.join("draft.eml");
        tokio::fs::write(&temp, &bytes)
            .await
            .map_err(|error| AppError::io(format!("cannot write draft message: {error}")))?;
        tokio::fs::rename(&temp, &final_path)
            .await
            .map_err(|error| AppError::io(format!("cannot finalize draft message: {error}")))?;

        let drafts_folder = self.draft_folder(&account).await?;
        let server_drafts_folder = drafts_folder.as_ref().filter(|folder| {
            !folder
                .remote_path
                .starts_with(crate::sync::folders::LOCAL_FOLDER_PREFIX)
        });
        let graph = crate::graph::uses_graph(&account);

        // Snapshot of the durable state at the start of this save. The write
        // transaction below re-reads it: a confirmation that lands while the
        // composer is saving is newer evidence and must not be overwritten by
        // this request's older snapshot.
        let existing_draft = {
            let id = draft_id.clone();
            self.inner
                .ctx
                .db
                .read(move |db| repo::drafts::get(db, &id))
                .await?
        };
        // The composer can switch the sending account (an identity from another
        // account). The old account's remote binding and projection must not
        // travel with the draft: two accounts may use the same Drafts
        // path/generation/UID, so a delete derived from the old binding would
        // address unrelated mail in the new account.
        let account_switched = existing_draft
            .as_ref()
            .is_some_and(|row| row.account_id != account.id);
        let switched_from = if account_switched {
            existing_draft
                .as_ref()
                .map(|row| row.account_id.clone())
                .filter(|old| *old != account.id)
        } else {
            None
        };
        if account_switched {
            draft.remote_draft = None;
            draft.remote_draft_message_id = None;
        }
        let previous_identity = if account_switched {
            None
        } else {
            existing_draft
                .as_ref()
                .and_then(|row| row.remote_draft.clone())
        };
        let ownership = self.append_ownership(&account.id, &draft_id).await;

        // A Graph delete target comes only from the durable identity decided
        // inside the save transaction below (this draft's own create receipt,
        // or the provider id pinned when the user opened the exact server row).
        // `remote_draft_message_id` is deliberately not consulted here: it is a
        // local projection row that the Drafts sync can rebind to an external
        // same-header copy, which must never inherit delete authority.
        let mut remote_found_snapshot = graph
            && (previous_identity
                .as_ref()
                .is_some_and(|identity| identity.attribution != RemoteDraftAttribution::None)
                || matches!(ownership, Some(AppendOwnership::GraphConfirmed { .. })));
        if let Some(folder) = server_drafts_folder {
            if graph {
                // Existence only: a same-header copy must not produce a delete,
                // but it still means a remote copy exists.
                remote_found_snapshot |= self
                    .find_remote_draft(&account, folder, &draft, &header_id)
                    .await?
                    .is_some();
            } else {
                remote_found_snapshot = previous_identity
                    .as_ref()
                    .is_some_and(|identity| identity.attribution != RemoteDraftAttribution::None)
                    || ownership.is_some();
            }
        }
        // A same-header copy without an attributable UID is still a remote copy:
        // the executor must not append a duplicate before a sync binds it.
        let header_copy_found = match server_drafts_folder {
            Some(folder) if !graph => self
                .find_remote_draft(&account, folder, &draft, &header_id)
                .await?
                .is_some(),
            _ => false,
        };
        if graph && remote_found_snapshot {
            // Graph deletes the old server draft through its own operation, so
            // the new draft is no longer tied to the old local row.
            draft.remote_draft_message_id = None;
        }
        // For IMAP the old row and `remote_draft_message_id` are deliberately
        // kept: the executor removes the projection only once the server has
        // confirmed the replacement. If it cannot, the old draft remains
        // visible instead of silently disappearing from Drafts.

        // The identity and the replacement are decided *inside* the write
        // transaction with the latest durable state. A stale composer snapshot
        // therefore cannot overwrite an APPENDUID the executor confirmed (or a
        // keep-local decision) between the read above and this write.
        let account_id = account.id.clone();
        let incoming_identity = draft.remote_draft.clone();
        let tx_draft_id = draft_id.clone();
        let (saved, effective_identity, replaces, ownership_seen, graph_old_remote) = self
            .inner
            .ctx
            .db
            .write(move |db| {
                let existing = repo::drafts::get(db, &tx_draft_id)?;
                if let Some(old_account) = switched_from.as_ref() {
                    // Leave the old account's copy alone, but stop its queued or
                    // in-flight upload for this draft: it must not land (and
                    // overwrite the projection) after the draft moved accounts.
                    cancel_pending_appends_from(db, old_account, &tx_draft_id)?;
                    // The Graph replacement's DELETE is paired with that APPEND.
                    // Cancelling only the upload would let it delete the remote
                    // draft the switch just decided to keep.
                    cancel_pending_draft_deletes_from(db, old_account, &tx_draft_id)?;
                }
                let latest_ownership = append_ownership_from(db, &account_id, &tx_draft_id)?;
                // A switched draft starts with no binding in the new account;
                // only evidence belonging to that account (latest_ownership)
                // may produce one.
                let existing_identity = if switched_from.is_some() {
                    None
                } else {
                    existing.as_ref().and_then(|row| row.remote_draft.clone())
                };
                let effective = choose_remote_identity(
                    existing_identity,
                    incoming_identity,
                    latest_ownership.as_ref(),
                    &tx_draft_id,
                );
                let mut to_save = draft.clone();
                to_save.remote_draft = effective.clone();
                // "Keep local draft" is a deliberate user choice. A composer
                // loaded before it was set must not clear it by saving stale
                // state. A draft that moved accounts starts fresh: the
                // suppression referred to the old account's remote copy.
                if switched_from.is_none() {
                    if let Some(existing) = existing.as_ref() {
                        to_save.sync_suppressed |= existing.sync_suppressed;
                    }
                }
                let saved = repo::drafts::save(db, &to_save)?;
                // IMAP replacements are carried by the APPEND itself so the
                // executor can delete-then-append atomically. Build it from the
                // identity just persisted, never from the pre-transaction
                // snapshot.
                let replaces = if graph {
                    None
                } else {
                    effective
                        .as_ref()
                        .filter(|identity| identity.is_deletable())
                        .map(|identity| RemoteDraftRef {
                            account_id: Some(account_id.clone()),
                            folder_id: identity.folder_id.clone().unwrap_or_default(),
                            uid: identity.uid.unwrap_or_default(),
                            source_uidvalidity: identity.uidvalidity,
                            message_id: Some(
                                identity
                                    .projection_id
                                    .clone()
                                    .unwrap_or_else(|| draft_message_id(&tx_draft_id)),
                            ),
                        })
                };
                // Graph replaces the previous copy through its own DELETE.
                // Only the identity pinned for this draft (our own create
                // receipt, or the row the user explicitly opened) may name it;
                // a same-header copy is never deleted by inference.
                let graph_old_remote = if graph {
                    effective
                        .as_ref()
                        .filter(|identity| identity.is_deletable_graph())
                        .and_then(|identity| {
                            let remote_id = identity.remote_id.clone()?;
                            let folder_id = identity.folder_id.clone()?;
                            let uid = identity.uid.unwrap_or_else(|| {
                                crate::sync::messages::synthetic_uid(&remote_id)
                            });
                            Some((folder_id, uid, identity.uidvalidity, remote_id))
                        })
                } else {
                    None
                };
                Ok((
                    saved,
                    effective,
                    replaces,
                    latest_ownership.is_some(),
                    graph_old_remote,
                ))
            })
            .await?;
        // A previous copy is known (its projection exists or the sync bound a
        // candidate) but no server UID is attributable to this client: the
        // earlier APPEND may already have created a copy that this one could
        // not identify for deletion.
        let replaces_unidentified = !graph
            && replaces.is_none()
            && (remote_found_snapshot
                || ownership_seen
                || header_copy_found
                || effective_identity
                    .as_ref()
                    .is_some_and(|identity| identity.attribution != RemoteDraftAttribution::None));

        // Local Drafts projection: the composer stays usable offline and the
        // row becomes the synced APPEND copy once the server echoes it back.
        // Written *before* the operation is queued so the executor can persist
        // the APPEND's UID (or the "appended, not yet bound" marker) against a
        // row that already exists; otherwise a fast worker could upload the
        // draft while the projection was still missing.
        if let Some(folder) = drafts_folder.as_ref() {
            if let Err(error) = self
                .store_draft_message(
                    &account,
                    &saved,
                    folder,
                    &header_id,
                    final_path.to_string_lossy().as_ref(),
                    bytes.len() as u64,
                )
                .await
            {
                warn!(%error, draft = %draft_id, "could not project the draft into Drafts");
            }
        }

        if account.imap.is_some() {
            // The old-copy DELETE (Graph) and the new APPEND share one
            // suppression/enqueue decision. A draft the user kept local must
            // not have its remote copy deleted while its upload is skipped.
            let delete_old = if graph {
                graph_old_remote
                    .as_ref()
                    .map(|(folder_id, uid, uidvalidity, remote_id)| {
                        DeleteRemoteDraftOp {
                            folder_id: folder_id.clone(),
                            uid: *uid,
                            // The exact id from our own receipt or the row the
                            // user opened. The executor must not resolve a
                            // different id by inference.
                            remote_message_id: Some(remote_id.clone()),
                            // Binds the cleanup to the replacement that queued
                            // it, so a keep-local can cancel it.
                            draft_id: Some(draft_id.clone()),
                            source_uidvalidity: *uidvalidity,
                            message_id: None,
                            outcome_unknown: false,
                            quarantined: false,
                            quarantine_reason: None,
                        }
                    })
            } else {
                None
            };
            let append_payload = server_drafts_folder.map(|folder| AppendDraftOp {
                draft_id: draft_id.clone(),
                folder_id: folder.id.clone(),
                rfc822_path: final_path.to_string_lossy().to_string(),
                replaces,
                replaces_unidentified,
                append_state: crate::ops::AppendState::Idle,
                removal_state: crate::ops::DraftRemovalState::NotStarted,
                appended_uid: None,
                appended_uidvalidity: None,
                appended_remote_id: None,
                outcome_unknown: false,
                attempt_uidnext: None,
                attempt_uidvalidity: None,
                attempt_size: None,
                attempt_at: None,
                discard_requested: false,
                cleanup_claimed: false,
                cleanup_acked: false,
                cleanup_done: false,
                candidate_only: false,
            });
            if self
                .enqueue_draft_sync_ops(&account.id, &draft_id, append_payload, delete_old)
                .await?
            {
                if let Some(folder) = server_drafts_folder {
                    events::emit_folders_changed(
                        self.inner.ctx.events.as_ref(),
                        account.id.clone(),
                        vec![folder.id.clone()],
                    );
                }
            }
        }

        debug!(draft = %draft_id, bytes = bytes.len(), "saved draft");
        Ok(saved)
    }

    /// Delete a draft locally, its staged files and its remote copy.
    pub async fn compose_discard(&self, draft_id: &str) -> Result<()> {
        let draft = self.compose_get_draft(draft_id).await?;
        let account = self.account(&draft.account_id).await?;
        // The APPEND operation is the single owner of an in-flight cleanup.
        // Its return value must be honoured: enqueuing a header-based delete
        // here would bypass the executor's ownership gate.
        let cleanup_deferred = self
            .cancel_pending_draft_appends(&draft.account_id, draft_id)
            .await?;
        let cleanup = self
            .cleanup_remote_draft(&account, &draft, cleanup_deferred)
            .await;
        let mut folders: Vec<String> = Vec::new();
        // A queued delete removes the projection after the server confirms; an
        // unconfirmed copy stays visible because it is still on the server.
        // When the APPEND owns the cleanup, the executor removes the
        // projection once it can attribute the copy. A projection without a
        // UID has no server copy to open and is reclaimed now instead of
        // lingering as a ghost Drafts row.
        let keep_projection = match cleanup {
            RemoteDraftCleanup::None => false,
            RemoteDraftCleanup::Queued => true,
            RemoteDraftCleanup::Deferred | RemoteDraftCleanup::Unconfirmed => {
                self.draft_projection_addressable(draft_id).await
            }
        };
        // The local draft is gone; any unresolved remote cleanup lives on the
        // APPEND operation (or is intentionally abandoned), not in a row that
        // pretends a local draft still exists.
        let id = draft_id.to_string();
        self.inner
            .ctx
            .db
            .write(move |db| repo::drafts::delete(db, &id))
            .await?;
        if !keep_projection {
            match self.remove_draft_message(draft_id).await {
                Ok(Some(folder_id)) => folders.push(folder_id),
                Ok(None) => {}
                Err(error) => {
                    debug!(%error, draft = %draft_id, "could not remove the draft projection")
                }
            }
        }
        if let Some(folder_id) = self.drafts_folder_id(&draft.account_id).await? {
            folders.push(folder_id);
        }
        let dir = self.inner.ctx.paths.draft_staging_dir(draft_id);
        let _ = tokio::fs::remove_dir_all(dir).await;
        events::emit_mail_changed(
            self.inner.ctx.events.as_ref(),
            draft.account_id.clone(),
            folders,
            Vec::new(),
            MailChangeKind::Updated,
        );
        self.update_badge().await;
        Ok(())
    }

    /// Stop retrying remote sync for a draft whose copy could not be confirmed.
    ///
    /// This is the local-only exit offered for a long-running `Unknown`
    /// APPEND: active operations are cancelled without recording a cleanup
    /// intent, and the draft stays local. Nothing is deleted on the server, so
    /// the deletion rules are unchanged.
    pub async fn compose_keep_local_draft(&self, message_id: &str) -> Result<()> {
        let stored = self.stored(message_id).await?;
        let draft_id = stored
            .message_id_header
            .as_deref()
            .and_then(crate::util::draft_id_from_header)
            .map(str::to_string)
            .or_else(|| crate::util::draft_id_from_header(message_id).map(str::to_string))
            .ok_or_else(|| AppError::invalid_input("message is not a draft projection"))?;
        let account_id = stored.summary.account_id.clone();
        let folder_id = stored.summary.folder_id.clone();
        let id = draft_id.clone();
        let account_for_db = account_id.clone();
        // The suppression marker and the cancellation of active APPENDs commit
        // in one transaction. A concurrent autosave therefore either sees the
        // marker before it enqueues, or enqueues before the cancellation; it can
        // never pass the check and enqueue after "keep local" succeeded.
        self.inner
            .ctx
            .db
            .write(move |db| {
                let Some(draft) = repo::drafts::get(db, &id)? else {
                    // "Keep local" is only offered for a draft that still
                    // exists. Reporting success after updating zero rows would
                    // hide that the draft was discarded meanwhile.
                    return Err(AppError::not_found("the local draft no longer exists"));
                };
                let mut identity = draft.remote_draft.clone();
                let mut candidate = identity.as_ref().is_some_and(|identity| {
                    identity.attribution == RemoteDraftAttribution::Candidate
                });
                for op in repo::pending_ops::list(db, Some(&account_for_db))? {
                    // A queued replacement cleanup is paired with this draft's
                    // APPEND. "Keep local" must cancel it too, or the old copy
                    // would still be deleted while the upload is skipped.
                    if op.kind == KIND_DELETE_REMOTE_DRAFT && op.state == PendingOpState::Pending {
                        if repo::pending_ops::payload::<DeleteRemoteDraftOp>(&op)
                            .ok()
                            .and_then(|payload| payload.draft_id)
                            .is_some_and(|id| id == draft_id)
                        {
                            repo::pending_ops::cancel(db, &op.id)?;
                        }
                        continue;
                    }
                    if op.kind != KIND_APPEND_DRAFT
                        || !matches!(
                            op.state,
                            PendingOpState::Pending | PendingOpState::InProgress
                        )
                    {
                        continue;
                    }
                    let Ok(mut payload) = repo::pending_ops::payload::<AppendDraftOp>(&op) else {
                        continue;
                    };
                    if payload.draft_id != draft_id {
                        continue;
                    }
                    match payload.append_state {
                        // Nothing of this attempt is on the wire. An older
                        // identity, if any, remains valid.
                        crate::ops::AppendState::Idle => {}
                        // No upload was claimed yet, but the executor may have
                        // already claimed the removal of the superseded copy.
                        // That destructive phase cannot be undone by this
                        // cancellation, so its outcome is unconfirmed and the
                        // attribution limit must be preserved.
                        crate::ops::AppendState::Preparing => {
                            if matches!(
                                payload.removal_state,
                                crate::ops::DraftRemovalState::PendingRemove
                                    | crate::ops::DraftRemovalState::Unknown
                            ) {
                                candidate = true;
                                if !payload.candidate_only {
                                    payload.candidate_only = true;
                                    let value =
                                        serde_json::to_value(&payload).map_err(|error| {
                                            AppError::internal(format!(
                                                "cannot encode appendDraft: {error}"
                                            ))
                                        })?;
                                    repo::pending_ops::update_payload(db, &op.id, &value)?;
                                }
                            }
                        }
                        // The literal may have landed, but no receipt
                        // attributes a copy to this attempt. Preserve that
                        // limit across the cancellation so a later cleanup
                        // cannot turn the candidate into an explicit binding.
                        crate::ops::AppendState::Unknown => {
                            candidate = true;
                            if !payload.candidate_only {
                                payload.candidate_only = true;
                                let value = serde_json::to_value(&payload).map_err(|error| {
                                    AppError::internal(format!(
                                        "cannot encode appendDraft: {error}"
                                    ))
                                })?;
                                repo::pending_ops::update_payload(db, &op.id, &value)?;
                            }
                        }
                        // Accepted upload: a receipt is a confirmed identity; a
                        // missing one is still only a candidate.
                        crate::ops::AppendState::Acked => {
                            match (
                                payload.appended_uid,
                                payload.appended_uidvalidity,
                                payload.appended_remote_id.as_deref(),
                            ) {
                                (Some(uid), uidvalidity, _) => {
                                    identity = Some(RemoteDraftIdentity::confirmed(
                                        &payload.folder_id,
                                        uid,
                                        uidvalidity,
                                        Some(draft_message_id(&draft_id)),
                                    ));
                                }
                                // Graph receipt: the provider id is the exact
                                // analogue of `APPENDUID`, so keep it pinned.
                                (None, _, Some(remote_id)) => {
                                    identity = Some(RemoteDraftIdentity::confirmed_graph(
                                        &payload.folder_id,
                                        remote_id,
                                        Some(draft_message_id(&draft_id)),
                                    ));
                                }
                                (None, _, None) => {
                                    candidate = true;
                                    if !payload.candidate_only {
                                        payload.candidate_only = true;
                                        let value =
                                            serde_json::to_value(&payload).map_err(|error| {
                                                AppError::internal(format!(
                                                    "cannot encode appendDraft: {error}"
                                                ))
                                            })?;
                                        repo::pending_ops::update_payload(db, &op.id, &value)?;
                                    }
                                }
                            }
                        }
                    }
                    // Keep-local wins only while the cleanup has not claimed
                    // its destructive step. Clearing the intent here makes a
                    // later atomic claim fail (the row is cancelled as well);
                    // once the claim has committed, the command may be on the
                    // wire and its outcome must stay recorded instead of being
                    // rewritten as "not sent".
                    if payload.discard_requested && !payload.cleanup_claimed {
                        payload.discard_requested = false;
                        let value = serde_json::to_value(&payload).map_err(|error| {
                            AppError::internal(format!("cannot encode appendDraft: {error}"))
                        })?;
                        repo::pending_ops::update_payload(db, &op.id, &value)?;
                    }
                    repo::pending_ops::cancel(db, &op.id)?;
                }
                if candidate {
                    identity = Some(RemoteDraftIdentity::candidate(
                        identity
                            .as_ref()
                            .and_then(|identity| identity.folder_id.clone()),
                        Some(draft_message_id(&draft_id)),
                    ));
                }
                if identity != draft.remote_draft {
                    repo::drafts::set_remote_draft(db, &id, identity.as_ref())?;
                }
                let changed = repo::drafts::set_sync_suppressed(db, &id, true)?;
                if changed == 0 {
                    return Err(AppError::not_found("the local draft no longer exists"));
                }
                Ok(())
            })
            .await?;
        events::emit_mail_changed(
            self.inner.ctx.events.as_ref(),
            account_id,
            vec![folder_id],
            vec![message_id.to_string()],
            MailChangeKind::Updated,
        );
        Ok(())
    }

    /// Validate, build MIME, queue to the outbox and remove the draft.
    pub async fn compose_send(&self, draft: &Draft) -> Result<()> {
        let account = self.account(&draft.account_id).await?;
        // Clear early dispatch: Microsoft accounts send through Graph, which
        // also files the Sent copy, so the SMTP/outbox path is skipped.
        if crate::graph::uses_graph(&account) {
            return self.compose_send_graph(&account, draft).await;
        }
        let recipients = all_recipients(draft)?;
        if account.smtp.is_none() {
            return Err(AppError::unsupported(
                "this account has no outgoing (SMTP) server configured",
            ));
        }
        let identity = self
            .identity_for(&account, std::slice::from_ref(&draft.from))
            .await?;
        let outgoing = self.outgoing_from_draft(draft, &identity, None)?;
        let (bytes, _) = nuncio_mime::build_message(outgoing)?;
        if bytes.len() as u64 > MAX_DRAFT_BYTES {
            return Err(AppError::invalid_input("message exceeds 100 MB"));
        }

        let draft_id = draft.id.clone();
        // Suppression/cancellation and the remote-delete decision are applied
        // *inside* the outbox insert transaction below, so a staging or insert
        // failure deletes nothing and suppresses nothing. The decision is
        // regenerated there from the latest attribution: an APPEND that
        // completes while the send is being queued must not be missed (which
        // would leave its copy behind) and its predecessor's UID must not be
        // deleted again from a stale plan.
        let cleanup_context = SendCleanupContext::new(&account, draft);
        let cleanup_action: Option<OutboxEnqueueHook> =
            cleanup_context.clone().map(|context| -> OutboxEnqueueHook {
                Box::new(move |conn: &rusqlite::Connection| apply_send_cleanup(conn, &context))
            });
        let (sent_now, applied_cleanup) = self
            .queue_outbox(
                &account,
                draft_id.clone(),
                draft.subject.clone(),
                recipients,
                bytes,
                OutboxDraftCleanup::None,
                cleanup_action,
            )
            .await?;
        // The projection is kept only when a cleanup that still owns the
        // server copy will remove it (or when the copy is unconfirmed and the
        // row is still addressable). Use the classification the transaction
        // actually stored, never the pre-transaction snapshot.
        let keep_projection = match applied_cleanup {
            OutboxDraftCleanup::Queued => true,
            OutboxDraftCleanup::Unconfirmed | OutboxDraftCleanup::Deferred => {
                match draft_id.as_deref() {
                    Some(id) => self.draft_projection_addressable(id).await,
                    None => false,
                }
            }
            OutboxDraftCleanup::None => false,
        };
        // The outbox applies source flags only when it can still read the
        // draft row; cover the offline and unsaved-draft cases ourselves.
        let handled_by_outbox = sent_now && draft_id.is_some();
        if !handled_by_outbox {
            if let Err(error) = self.apply_source_flags(&account, draft).await {
                warn!(%error, "could not mark the source message as replied/forwarded");
            }
        }

        // `queue_outbox` above is the point of no return: the message is durable
        // and the sender will be driven from the outbox. Everything below is
        // cleanup, and a failure here must NOT surface as "the send failed" —
        // the client would re-submit a message that is already queued, and the
        // draft it recreates would be a second copy of mail that is on its way.
        let mut affected_folders: Vec<String> = Vec::new();
        if let Some(draft_id) = draft_id {
            let id = draft_id.clone();
            if let Err(error) = self
                .inner
                .ctx
                .db
                .write(move |db| repo::drafts::delete(db, &id))
                .await
            {
                warn!(%error, draft = %draft_id, "could not delete the sent draft");
            }
            if !keep_projection {
                match self.remove_draft_message(&draft_id).await {
                    Ok(Some(folder_id)) => affected_folders.push(folder_id),
                    Ok(None) => {}
                    Err(error) => {
                        debug!(%error, draft = %draft_id, "could not remove the draft projection")
                    }
                }
            }
            let dir = self.inner.ctx.paths.draft_staging_dir(&draft_id);
            let _ = tokio::fs::remove_dir_all(dir).await;
        }
        events::emit_mail_changed(
            self.inner.ctx.events.as_ref(),
            account.id.clone(),
            affected_folders,
            Vec::new(),
            MailChangeKind::Updated,
        );
        self.update_badge().await;
        Ok(())
    }

    /// Send a draft through Microsoft Graph.
    ///
    /// Graph's `sendMail` accepts a base64 MIME payload and reads the
    /// recipients from its headers, so the staged copy must keep the Bcc
    /// header (`build_message_with_bcc`) -- unlike SMTP there is no envelope
    /// to carry Bcc separately.  Graph files the Sent copy itself, so no
    /// `appendToSent` operation is queued.  A failed send leaves the draft and
    /// its local projection intact so the user can retry.
    async fn compose_send_graph(&self, account: &Account, draft: &Draft) -> Result<()> {
        self.compose_send_graph_with(account, draft, &crate::graph::GraphMailClient::new())
            .await
    }

    /// [`Self::compose_send_graph`] against an explicit client.
    ///
    /// Exposed like [`crate::ops::executor::run_due_graph_with`] so integration
    /// tests can drive the send against a loopback Graph server while still
    /// exercising the real cleanup/suppression logic.
    #[doc(hidden)]
    pub async fn compose_send_graph_with(
        &self,
        account: &Account,
        draft: &Draft,
        client: &crate::graph::GraphMailClient,
    ) -> Result<()> {
        all_recipients(draft)?;
        let identity = self
            .identity_for(account, std::slice::from_ref(&draft.from))
            .await?;
        let outgoing = self.outgoing_from_draft(draft, &identity, None)?;
        let (bytes, _) = nuncio_mime::build_message_with_bcc(outgoing)?;
        if bytes.len() as u64 > MAX_DRAFT_BYTES {
            return Err(AppError::invalid_input("message exceeds 100 MB"));
        }

        let draft_id = draft.id.clone();
        let cleanup_deferred = if let Some(draft_id) = draft_id.as_deref() {
            self.cancel_pending_draft_appends(&account.id, draft_id)
                .await?
        } else {
            false
        };

        crate::graph::ops::send_raw(client, &self.inner.ctx, &account.id, &bytes).await?;

        // The message is sent; everything below is best-effort cleanup and must
        // not turn a delivered message into a reported failure.
        self.cleanup_remote_draft(account, draft, cleanup_deferred)
            .await;
        if let Err(error) = self.apply_source_flags(account, draft).await {
            warn!(%error, "could not mark the source message as replied/forwarded");
        }

        let mut affected_folders: Vec<String> = Vec::new();
        if let Some(draft_id) = draft_id {
            let id = draft_id.clone();
            if let Err(error) = self
                .inner
                .ctx
                .db
                .write(move |db| repo::drafts::delete(db, &id))
                .await
            {
                warn!(%error, draft = %draft_id, "could not delete the sent draft");
            }
            match self.remove_draft_message(&draft_id).await {
                Ok(Some(folder_id)) => affected_folders.push(folder_id),
                Ok(None) => {}
                Err(error) => {
                    debug!(%error, draft = %draft_id, "could not remove the draft projection")
                }
            }
            let dir = self.inner.ctx.paths.draft_staging_dir(&draft_id);
            let _ = tokio::fs::remove_dir_all(dir).await;
        }
        events::emit_mail_changed(
            self.inner.ctx.events.as_ref(),
            account.id.clone(),
            affected_folders,
            Vec::new(),
            MailChangeKind::Updated,
        );
        self.update_badge().await;
        Ok(())
    }

    /// Decide, without side effects, how to clean up a draft's remote copy.
    ///
    /// Only two identities may be queued for deletion:
    /// * an `APPENDUID` returned to one of this draft's APPEND attempts, and
    /// * a UID already bound to this draft by an explicit remote id (the user
    ///   opened that exact server draft).
    ///
    /// A same-header copy found by search is never enough: another client can
    /// write such a copy, so it is reported as unconfirmed and kept.
    async fn plan_remote_draft_cleanup(
        &self,
        account: &Account,
        draft: &Draft,
    ) -> SendDraftCleanup {
        let graph = crate::graph::uses_graph(account);
        let ownership = self
            .append_ownership(&account.id, draft.id.as_deref().unwrap_or_default())
            .await;
        if matches!(ownership, Some(AppendOwnership::CandidateOnly)) {
            // The newest attempt could not attribute its copy. Even an
            // explicit row may point at that candidate.
            return SendDraftCleanup::Unconfirmed;
        }
        if graph {
            // Only the Graph create receipt (`APPENDUID` analogue) or the exact
            // server row the user opened may authorize a delete. A same-header
            // copy can belong to another client and must be kept.
            if let Some(AppendOwnership::GraphConfirmed {
                folder_id,
                remote_id,
            }) = ownership
            {
                let uid = crate::sync::messages::synthetic_uid(&remote_id);
                return SendDraftCleanup::Queued(DeleteRemoteDraftOp {
                    folder_id,
                    uid,
                    remote_message_id: Some(remote_id),
                    draft_id: draft.id.clone(),
                    source_uidvalidity: None,
                    message_id: draft.id.as_deref().map(crate::util::draft_message_id),
                    outcome_unknown: false,
                    quarantined: false,
                    quarantine_reason: None,
                });
            }
            if let Some(identity) = self.pinned_remote_identity(draft).await {
                if self
                    .identity_belongs_to_account(&account.id, &identity)
                    .await
                {
                    if identity.attribution == RemoteDraftAttribution::Candidate {
                        return SendDraftCleanup::Unconfirmed;
                    }
                    if identity.is_deletable_graph() {
                        let remote_id = identity.remote_id.clone().unwrap_or_default();
                        let folder_id = identity.folder_id.clone().unwrap_or_default();
                        let uid = identity
                            .uid
                            .unwrap_or_else(|| crate::sync::messages::synthetic_uid(&remote_id));
                        return SendDraftCleanup::Queued(DeleteRemoteDraftOp {
                            folder_id,
                            uid,
                            remote_message_id: Some(remote_id),
                            draft_id: draft.id.clone(),
                            source_uidvalidity: identity.uidvalidity,
                            message_id: identity
                                .projection_id
                                .clone()
                                .or_else(|| draft.id.as_deref().map(crate::util::draft_message_id)),
                            outcome_unknown: false,
                            quarantined: false,
                            quarantine_reason: None,
                        });
                    }
                }
            }
            // The exact server row the user opened was pinned as an explicit
            // provider identity when it was opened. Never re-derive a delete
            // target from `remote_draft_message_id` here: it names a local
            // projection row that a sync can rebind to an external copy.
            return match self.find_header_remote_draft(account, draft).await {
                Some(_) => SendDraftCleanup::Unconfirmed,
                None => SendDraftCleanup::None,
            };
        }
        if let Some(AppendOwnership::Confirmed {
            folder_id,
            uid,
            uidvalidity,
            ..
        }) = ownership
        {
            return SendDraftCleanup::Queued(DeleteRemoteDraftOp {
                folder_id,
                uid,
                remote_message_id: None,
                draft_id: draft.id.clone(),
                source_uidvalidity: Some(uidvalidity),
                message_id: draft.id.as_deref().map(crate::util::draft_message_id),
                outcome_unknown: false,
                quarantined: false,
                quarantine_reason: None,
            });
        }
        // Explicit binding: the identity was pinned when the user opened this
        // exact server row (or when an APPEND receipt was accepted). Read it
        // from the durable snapshot, never from the projection's current UID.
        if let Some(identity) = self.pinned_remote_identity(draft).await {
            // A snapshot from before an account switch names the old
            // account's copy; it must not be deleted through this account.
            if !self
                .identity_belongs_to_account(&account.id, &identity)
                .await
            {
                return match self.find_header_remote_draft(account, draft).await {
                    Some(_) => SendDraftCleanup::Unconfirmed,
                    None => SendDraftCleanup::None,
                };
            }
            match identity.attribution {
                RemoteDraftAttribution::Candidate => return SendDraftCleanup::Unconfirmed,
                RemoteDraftAttribution::Confirmed | RemoteDraftAttribution::Explicit
                    if identity.is_deletable() =>
                {
                    return SendDraftCleanup::Queued(DeleteRemoteDraftOp {
                        folder_id: identity.folder_id.clone().unwrap_or_default(),
                        uid: identity.uid.unwrap_or_default(),
                        remote_message_id: identity.remote_id.clone(),
                        draft_id: draft.id.clone(),
                        source_uidvalidity: identity.uidvalidity,
                        message_id: identity
                            .projection_id
                            .clone()
                            .or_else(|| draft.id.as_deref().map(crate::util::draft_message_id)),
                        outcome_unknown: false,
                        quarantined: false,
                        quarantine_reason: None,
                    });
                }
                _ => {}
            }
        }
        // No confirmed identity. If a same-header copy exists it is a
        // candidate: keep it visible instead of deleting a guess.
        match self.find_header_remote_draft(account, draft).await {
            Some(_) => SendDraftCleanup::Unconfirmed,
            None => SendDraftCleanup::None,
        }
    }

    /// Decide how to clean up a draft's remote copy and apply it immediately.
    ///
    /// Used by discard and Graph send, where no outbox transaction is involved.
    /// The SMTP send path applies the decision inside its outbox transaction
    /// instead.
    async fn cleanup_remote_draft(
        &self,
        account: &Account,
        draft: &Draft,
        cleanup_deferred: bool,
    ) -> RemoteDraftCleanup {
        if cleanup_deferred {
            // The APPEND operation persisted the cleanup intent; it owns the
            // remote copy and this path must not enqueue a second delete.
            return RemoteDraftCleanup::Deferred;
        }
        match self.plan_remote_draft_cleanup(account, draft).await {
            SendDraftCleanup::Queued(payload) => {
                self.enqueue_remote_draft_delete_payload(account, payload)
                    .await;
                RemoteDraftCleanup::Queued
            }
            SendDraftCleanup::Unconfirmed => RemoteDraftCleanup::Unconfirmed,
            SendDraftCleanup::None => RemoteDraftCleanup::None,
        }
    }

    /// The identity pinned on the draft row, falling back to the snapshot the
    /// composer carried from `compose_open_draft`.
    async fn pinned_remote_identity(&self, draft: &Draft) -> Option<RemoteDraftIdentity> {
        if let Some(id) = draft.id.clone() {
            let existing = self
                .inner
                .ctx
                .db
                .read(move |db| repo::drafts::get(db, &id))
                .await
                .ok()
                .flatten()
                .and_then(|row| row.remote_draft);
            if existing.is_some() {
                return existing;
            }
        }
        draft.remote_draft.clone()
    }

    /// Whether the pinned identity names a folder that belongs to `account_id`.
    async fn identity_belongs_to_account(
        &self,
        account_id: &str,
        identity: &RemoteDraftIdentity,
    ) -> bool {
        let Some(folder_id) = identity.folder_id.clone() else {
            return false;
        };
        let account_id = account_id.to_string();
        self.inner
            .ctx
            .db
            .read(move |db| {
                Ok(repo::folders::get(db, &folder_id)?
                    .is_some_and(|folder| folder.account_id == account_id))
            })
            .await
            .unwrap_or(false)
    }

    /// Whether the Drafts projection of this draft carries a server UID and can
    /// therefore still be opened. A projection without one is a local-only echo
    /// of a draft that no longer exists.
    async fn draft_projection_addressable(&self, draft_id: &str) -> bool {
        let message_id = crate::util::draft_message_id(draft_id);
        self.inner
            .ctx
            .db
            .read(move |db| repo::messages::get(db, &message_id))
            .await
            .ok()
            .flatten()
            .is_some_and(|stored| stored.uid.is_some())
    }

    /// Pin the identity of the server copy that backs a projection.
    ///
    /// A UID found only on the projection is not evidence of ownership: the
    /// Drafts sync can bind a same-header copy written by another client. Only
    /// an APPENDUID receipt (or an explicit open, handled by the caller)
    /// authorizes a later delete.
    async fn identity_for_open(
        &self,
        account_id: &str,
        draft_id: &str,
        stored: &StoredMessage,
    ) -> Option<RemoteDraftIdentity> {
        if let Some(ownership) = self.append_ownership(account_id, draft_id).await {
            return Some(match ownership {
                AppendOwnership::Confirmed {
                    folder_id,
                    uid,
                    uidvalidity,
                    ..
                } => RemoteDraftIdentity::confirmed(
                    &folder_id,
                    uid,
                    Some(uidvalidity),
                    Some(draft_message_id(draft_id)),
                ),
                AppendOwnership::GraphConfirmed {
                    folder_id,
                    remote_id,
                } => RemoteDraftIdentity::confirmed_graph(
                    &folder_id,
                    &remote_id,
                    Some(draft_message_id(draft_id)),
                ),
                AppendOwnership::CandidateOnly => RemoteDraftIdentity::candidate(
                    Some(stored.summary.folder_id.clone()),
                    Some(draft_message_id(draft_id)),
                ),
            });
        }
        stored.uid.map(|_| {
            RemoteDraftIdentity::candidate(
                Some(stored.summary.folder_id.clone()),
                Some(draft_message_id(draft_id)),
            )
        })
    }

    /// The newest durable ownership evidence left by this draft's APPENDs.
    ///
    /// Settled operations are included: `candidateOnly` is exactly the record
    /// that must survive `Done` so a later discard/send cannot bypass the
    /// deletion gate. A newer candidate-only attempt deliberately shadows an
    /// older confirmed receipt (the newest version's copy is the ambiguous
    /// one).
    async fn append_ownership(&self, account_id: &str, draft_id: &str) -> Option<AppendOwnership> {
        let draft_id = draft_id.to_string();
        let account_id = account_id.to_string();
        self.inner
            .ctx
            .db
            .read(move |db| append_ownership_from(db, &account_id, &draft_id))
            .await
            .ok()
            .flatten()
    }

    /// A remote copy matched only by the draft's stable Message-ID.
    async fn find_header_remote_draft(
        &self,
        account: &Account,
        draft: &Draft,
    ) -> Option<StoredMessage> {
        let draft_id = draft.id.as_deref()?;
        let folder = self
            .folder_by_role(&account.id, FolderRole::Drafts)
            .await
            .ok()
            .flatten()?;
        if folder
            .remote_path
            .starts_with(crate::sync::folders::LOCAL_FOLDER_PREFIX)
        {
            return None;
        }
        let header = draft_message_id(draft_id);
        self.find_remote_draft(account, &folder, draft, &header)
            .await
            .ok()
            .flatten()
    }

    /// Queue a delete for a UID that is already attributed to this client.
    async fn enqueue_remote_draft_delete_payload(
        &self,
        account: &Account,
        payload: DeleteRemoteDraftOp,
    ) {
        let Ok(value) = serde_json::to_value(payload) else {
            return;
        };
        if let Err(error) = ops::enqueue_operation(
            &self.inner.ctx,
            &account.id,
            KIND_DELETE_REMOTE_DRAFT,
            value,
            "Remove remote draft",
        )
        .await
        {
            debug!(%error, "could not queue remote draft deletion");
        }
    }

    /* ───────────────────── attachments / suggestions ───────────────────── */

    /// Copy files into the compose staging area.
    pub async fn compose_add_attachments(&self, paths: &[String]) -> Result<Vec<DraftAttachment>> {
        let dir = self
            .inner
            .ctx
            .paths
            .staging_dir()
            .join("compose")
            .join(nuncio_core::models::new_id());
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|error| AppError::io(format!("cannot create staging dir: {error}")))?;
        let mut used_names: HashSet<String> = HashSet::new();
        let mut result = Vec::new();
        for path in paths {
            let source = PathBuf::from(path);
            let metadata = tokio::fs::metadata(&source)
                .await
                .map_err(|error| AppError::io(format!("cannot read attachment {path}: {error}")))?;
            if !metadata.is_file() {
                return Err(AppError::invalid_input(format!("{path} is not a file")));
            }
            let filename = source
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("attachment")
                .to_string();
            let filename = unique_name(&dir, &filename, &mut used_names).await;
            let dest = dir.join(&filename);
            tokio::fs::copy(&source, &dest)
                .await
                .map_err(|error| AppError::io(format!("cannot stage attachment: {error}")))?;
            let mime_type = mime_guess::from_path(&dest)
                .first_or_octet_stream()
                .to_string();
            result.push(DraftAttachment {
                id: nuncio_core::models::new_id(),
                filename: filename.clone(),
                mime_type,
                size: metadata.len(),
                path: dest.to_string_lossy().to_string(),
                content_id: None,
            });
        }
        Ok(result)
    }

    /// Stage a pasted image and give it a `cid:` content id.
    pub async fn compose_add_inline_image(
        &self,
        filename: &str,
        mime_type: &str,
        base64_data: &str,
    ) -> Result<DraftAttachment> {
        let payload = base64_data
            .split_once(',')
            .filter(|(prefix, _)| prefix.starts_with("data:"))
            .map(|(_, data)| data)
            .unwrap_or(base64_data)
            .trim();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
            .map_err(|error| AppError::invalid_input(format!("invalid base64 image: {error}")))?;
        if bytes.len() > MAX_INLINE_BYTES {
            return Err(AppError::invalid_input("inline image exceeds 25 MB"));
        }
        let dir = self
            .inner
            .ctx
            .paths
            .staging_dir()
            .join("compose")
            .join(nuncio_core::models::new_id());
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|error| AppError::io(format!("cannot create staging dir: {error}")))?;
        let mut used_names = HashSet::new();
        let filename = unique_name(&dir, filename, &mut used_names).await;
        let dest = dir.join(&filename);
        tokio::fs::write(&dest, &bytes)
            .await
            .map_err(|error| AppError::io(format!("cannot stage inline image: {error}")))?;
        let mime_type = if mime_type.trim().is_empty() {
            mime_guess::from_path(&dest)
                .first_or_octet_stream()
                .to_string()
        } else {
            mime_type.to_string()
        };
        let content_id = nuncio_mime::attachments::content_id_for(&filename);
        Ok(DraftAttachment {
            id: nuncio_core::models::new_id(),
            filename,
            mime_type,
            size: bytes.len() as u64,
            path: dest.to_string_lossy().to_string(),
            content_id: Some(content_id),
        })
    }

    /// Recipient autocomplete: contacts first, then recently used addresses.
    pub async fn compose_suggest_recipients(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Vec<Address>> {
        let limit = limit.clamp(1, 50);
        let query_owned = query.trim().to_string();
        let contacts = self
            .inner
            .ctx
            .db
            .read(move |db| {
                let summaries = repo::contacts::search(db, &query_owned, limit)?;
                let mut addresses = Vec::new();
                for summary in summaries {
                    if let Some(contact) = repo::contacts::get(db, &summary.id)? {
                        if let Some(address) = repo::contacts::address_for(&contact) {
                            addresses.push(address);
                        }
                    }
                }
                Ok(addresses)
            })
            .await?;
        let query_owned = query.trim().to_string();
        let history = self
            .inner
            .ctx
            .db
            .read(move |db| repo::recipient_history::suggest(db, &query_owned, limit))
            .await?;
        let mut seen: HashSet<String> = HashSet::new();
        let mut result = Vec::new();
        for address in contacts.into_iter().chain(history) {
            let key = nuncio_core::text::normalize_email(&address.email);
            if key.is_empty() || !seen.insert(key) {
                continue;
            }
            result.push(address);
            if result.len() >= limit as usize {
                break;
            }
        }
        Ok(result)
    }

    /* ───────────────────────── helpers ───────────────────────── */

    async fn default_mail_account(&self) -> Result<nuncio_core::models::Account> {
        let accounts = self
            .inner
            .ctx
            .db
            .read(repo::accounts::list_mail_accounts)
            .await?;
        accounts
            .into_iter()
            .find(|account| {
                account.mail_enabled && !matches!(account.provider, ProviderKind::Local)
            })
            .ok_or_else(|| AppError::not_found("no mail account is configured"))
    }

    pub(crate) async fn identity_for(
        &self,
        account: &nuncio_core::models::Account,
        preferred: &[Address],
    ) -> Result<Identity> {
        let account_id = account.id.clone();
        let mut identities = self
            .inner
            .ctx
            .db
            .read(move |db| repo::identities::list(db, Some(&account_id)))
            .await?;
        if identities.is_empty() {
            return self.ensure_default_identity(&account.id).await;
        }
        let preferred_keys: Vec<String> = preferred
            .iter()
            .map(|address| nuncio_core::text::normalize_email(&address.email))
            .filter(|email| !email.is_empty())
            .collect();
        if let Some(identity) = identities.iter().find(|identity| {
            preferred_keys.contains(&nuncio_core::text::normalize_email(&identity.email))
        }) {
            return Ok(identity.clone());
        }
        if let Some(identity) = identities.iter().find(|identity| identity.is_default) {
            return Ok(identity.clone());
        }
        Ok(identities.remove(0))
    }

    /// Signature configured on the selected identity, falling back to the
    /// account/global `useForNew` / `useForReply` default.
    async fn signature_for_identity(
        &self,
        account_id: &str,
        signature_id: Option<&str>,
        for_reply: bool,
    ) -> Result<Option<Signature>> {
        if let Some(signature_id) = signature_id.filter(|value| !value.trim().is_empty()) {
            let id = signature_id.to_string();
            let account_id_owned = account_id.to_string();
            let configured = self
                .inner
                .ctx
                .db
                .read(move |db| repo::signatures::get(db, &id))
                .await?;
            if let Some(signature) = configured {
                let usable = signature
                    .account_id
                    .as_deref()
                    .map(|owner| owner == account_id_owned)
                    .unwrap_or(true);
                if usable {
                    return Ok(Some(signature));
                }
            }
        }
        self.signature_for(account_id, for_reply).await
    }

    /// Signature selected for the account (kept for the fallback path).
    async fn signature_for(&self, account_id: &str, for_reply: bool) -> Result<Option<Signature>> {
        let account_id = account_id.to_string();
        self.inner
            .ctx
            .db
            .read(move |db| {
                if for_reply {
                    repo::signatures::default_for_reply(db, &account_id)
                } else {
                    repo::signatures::default_for_new(db, &account_id)
                }
            })
            .await
    }

    /// Translate a `Draft` into [`nuncio_mime::OutgoingMessage`].
    fn outgoing_from_draft(
        &self,
        draft: &Draft,
        identity: &Identity,
        message_id: Option<String>,
    ) -> Result<nuncio_mime::OutgoingMessage> {
        let mut message = nuncio_mime::OutgoingMessage::new(
            Address {
                name: if draft.from.name.is_some() {
                    draft.from.name.clone()
                } else if identity.name.trim().is_empty() {
                    None
                } else {
                    Some(identity.name.clone())
                },
                email: if draft.from.email.trim().is_empty() {
                    identity.email.clone()
                } else {
                    draft.from.email.clone()
                },
            },
            draft.subject.clone(),
            draft.html.clone(),
        );
        if draft.from.email.trim().is_empty() && identity.email.trim().is_empty() {
            return Err(AppError::invalid_input("draft has no From address"));
        }
        message.to = draft.to.clone();
        message.cc = draft.cc.clone();
        message.bcc = draft.bcc.clone();
        if let Some(reply_to) = identity
            .reply_to
            .as_deref()
            .and_then(nuncio_mime::parse_one_address)
        {
            message.reply_to = vec![reply_to];
        }
        message.in_reply_to = draft.in_reply_to.clone();
        message.references = draft.references.clone();
        message.importance = draft.importance;
        message.request_read_receipt = draft.request_read_receipt;
        message.message_id = message_id;
        message.date = Some(nuncio_core::models::now());
        for attachment in &draft.attachments {
            let path = PathBuf::from(&attachment.path);
            let mime_type = if attachment.mime_type.trim().is_empty() {
                mime_guess::from_path(&path)
                    .first_or_octet_stream()
                    .to_string()
            } else {
                attachment.mime_type.clone()
            };
            match attachment.content_id.as_deref() {
                Some(content_id) if !content_id.trim().is_empty() => {
                    message
                        .inline_images
                        .push((content_id.to_string(), mime_type, path));
                }
                _ => {
                    message
                        .attachments
                        .push((attachment.filename.clone(), mime_type, path));
                }
            }
        }
        Ok(message)
    }

    /// Mark the original message as answered/forwarded (local + queued).
    pub(crate) async fn apply_source_flags(
        &self,
        _account: &nuncio_core::models::Account,
        draft: &Draft,
    ) -> Result<()> {
        let Some(source_id) = draft.source_message_id.clone() else {
            return Ok(());
        };
        let stored = match self.stored(&source_id).await {
            Ok(stored) => stored,
            Err(error) if error.is_not_found() => return Ok(()),
            Err(error) => return Err(error),
        };
        let answered = matches!(
            draft.reply_mode,
            Some(ReplyMode::Reply) | Some(ReplyMode::ReplyAll)
        );
        let forwarded = matches!(
            draft.reply_mode,
            Some(ReplyMode::Forward) | Some(ReplyMode::ForwardAsAttachment)
        );
        if !answered && !forwarded {
            return Ok(());
        }
        // Captured outside the transaction; re-checked inside it. The row may
        // be deleted or rebound between this read and the write below, and a
        // queued UID that no longer names this copy must never reach the wire.
        let identity = repo::messages::MessageIdentity::of(&stored);
        // The source copy may belong to a different account than the one the
        // reply/forward is sent from. The folder/UID in the payload only mean
        // anything to that *source* account: queueing under the sender would
        // make its worker SELECT the same folder/UID with the wrong mailbox and
        // mark an unrelated message. Everything about this operation (payload
        // identity and queue ownership) must therefore name the source account.
        let source_account_id = stored.summary.account_id.clone();
        let source_local_only = self
            .inner
            .ctx
            .db
            .read({
                let source_account_id = source_account_id.clone();
                move |db| repo::accounts::get(db, &source_account_id)
            })
            .await?
            .map(|source| is_local_only_account(source.provider))
            // A vanished source account cannot receive server mail; never
            // create an orphan queue row under it.
            .unwrap_or(true);
        let queued = if source_local_only {
            None
        } else {
            stored
                .uid
                .map(|uid| {
                    let payload = SetFlagsOp {
                        folder_id: stored.summary.folder_id.clone(),
                        uids: vec![uid],
                        seen: None,
                        flagged: None,
                        answered: answered.then_some(true),
                        forwarded: forwarded.then_some(true),
                        source_uidvalidity: stored.uidvalidity,
                        previous: vec![crate::ops::FlagRollback {
                            uid,
                            seen: stored.summary.seen,
                            flagged: stored.summary.flagged,
                            answered: stored.summary.answered,
                            forwarded: stored.summary.forwarded,
                        }],
                        message_ids: vec![stored.summary.id.clone()],
                        outcomes: Vec::new(),
                        // Pin the Graph copy this reply/forward addressed. A
                        // rebind of the synthetic UID must not make the queued
                        // PATCH target a different provider message.
                        remote_ids: stored
                            .gm_msgid
                            .as_deref()
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(|value| vec![value.to_string()])
                            .unwrap_or_default(),
                    };
                    serde_json::to_value(payload)
                        .map(|payload| (source_account_id.clone(), payload))
                        .map_err(|error| {
                            AppError::internal(format!("cannot encode setFlags: {error}"))
                        })
                })
                .transpose()?
        };
        // The local value, the per-message writer and the queue row are one
        // transaction, and they are written only while the source row still
        // names the copy this reply/forward was based on. Without the re-check
        // a deleted projection could be given a fresh writer that authorizes
        // an IMAP STORE with its old UID.
        self.inner
            .ctx
            .db
            .write(move |db| {
                crate::ops::apply_source_flags_in_db(
                    db,
                    &identity,
                    answered,
                    forwarded,
                    queued.as_ref(),
                )
            })
            .await?;
        Ok(())
    }

    /// Drafts folder used by the composer: the server folder when it exists,
    /// otherwise a local folder so POP3/local/IMAP-without-Drafts accounts can
    /// still list and reopen closed drafts offline.
    async fn draft_folder(&self, account: &nuncio_core::models::Account) -> Result<Option<Folder>> {
        let account_id = account.id.clone();
        let folders = self
            .inner
            .ctx
            .db
            .read({
                let account_id = account_id.clone();
                move |db| repo::folders::list(db, &account_id)
            })
            .await?;
        if let Some(folder) = folders.iter().find(|folder| {
            folder.role == FolderRole::Drafts
                && !folder
                    .remote_path
                    .starts_with(crate::sync::folders::LOCAL_FOLDER_PREFIX)
        }) {
            return Ok(Some(folder.clone()));
        }
        if let Some(folder) = folders.iter().find(|folder| {
            folder.role == FolderRole::Drafts
                && folder
                    .remote_path
                    .starts_with(crate::sync::folders::LOCAL_FOLDER_PREFIX)
        }) {
            return Ok(Some(folder.clone()));
        }
        let folder = Folder {
            id: nuncio_core::models::new_id(),
            account_id,
            parent_id: None,
            remote_path: LOCAL_DRAFTS_PATH.to_string(),
            name: "Drafts".to_string(),
            role: FolderRole::Drafts,
            unread_count: 0,
            total_count: 0,
            is_favorite: false,
            sync_enabled: false,
            is_selectable: true,
            sort_order: 2,
            last_sync_at: None,
        };
        let folder_for_db = folder.clone();
        self.inner
            .ctx
            .db
            .write(move |db| repo::folders::insert(db, &folder_for_db))
            .await?;
        events::emit_folders_changed(
            self.inner.ctx.events.as_ref(),
            account.id.clone(),
            vec![folder.id.clone()],
        );
        debug!(account = %account.id, folder = %folder.id, "created local Drafts folder");
        Ok(Some(folder))
    }

    /// Project a composer draft into the Drafts folder as an `is_draft` message
    /// row. This makes offline drafts visible/reopenable before (or without)
    /// a server APPEND.
    async fn store_draft_message(
        &self,
        account: &nuncio_core::models::Account,
        draft: &Draft,
        folder: &Folder,
        header_id: &str,
        rfc822_path: &str,
        size: u64,
    ) -> Result<String> {
        let draft_id = draft.id.clone().ok_or_else(|| {
            AppError::internal("draft id missing while storing the draft message")
        })?;
        let message_id = crate::util::draft_message_id(&draft_id);
        let now = draft.updated_at.unwrap_or_else(nuncio_core::models::now);
        let text = nuncio_mime::html_to_text(&draft.html);
        let mut summary = MessageSummary::new(
            message_id.clone(),
            account.id.clone(),
            folder.id.clone(),
            format!("draft:{draft_id}"),
            now,
        );
        summary.subject = draft.subject.clone();
        summary.r#from = Some(Address {
            name: draft
                .from
                .name
                .clone()
                .filter(|name| !name.trim().is_empty()),
            email: if draft.from.email.trim().is_empty() {
                account.email.clone()
            } else {
                draft.from.email.clone()
            },
        });
        summary.to = draft.to.clone();
        summary.cc = draft.cc.clone();
        summary.preview = nuncio_core::text::trim_preview(&text, 200);
        summary.seen = true;
        summary.is_draft = true;
        summary.has_attachments = !draft.attachments.is_empty();
        summary.importance = draft.importance;
        summary.size = size;

        let mut stored = StoredMessage::new(summary);
        stored.bcc = draft.bcc.clone();
        stored.message_id_header = Some(header_id.to_string());
        stored.in_reply_to = draft.in_reply_to.clone();
        stored.references = draft.references.clone();
        stored.body_path = Some(rfc822_path.to_string());
        stored.body_cached_at = Some(nuncio_core::models::now());
        stored.internal_date = Some(now);

        let message_id_for_db = message_id.clone();
        let folder_id = folder.id.clone();
        let path_for_db = rfc822_path.to_string();
        let text_for_fts = text;
        let preview = stored.summary.preview.clone();
        self.inner
            .ctx
            .db
            .write(move |db| {
                let mut stored = stored;
                if let Some(existing) = repo::messages::get(db, &message_id_for_db)? {
                    // Keep server-side identity/flags when this row is also
                    // the synced APPEND copy. A row that belonged to another
                    // account is a different mailbox: its UID/validity (and
                    // Graph ids) must not be adopted as this account's copy.
                    if existing.summary.account_id == stored.summary.account_id {
                        stored.uid = existing.uid;
                        stored.uidvalidity = existing.uidvalidity;
                        stored.gm_msgid = existing.gm_msgid;
                        stored.gm_thrid = existing.gm_thrid;
                        stored.labels = existing.labels;
                        stored.raw_headers = existing.raw_headers;
                    }
                }
                repo::messages::upsert(db, &stored)?;
                // `draft.eml` was produced locally, never fetched from the
                // provider copy this row may name. A Drafts sync can rebind the
                // projection to an external same-header draft between saves,
                // and this row then keeps that copy's UID/provider id while
                // `body_path` still points at our bytes. Recording that
                // identity (before or after the upsert) would let opening the
                // retained projection "prove" these local bytes came from the
                // external copy and authorize deleting it. Persist "source
                // unknown" instead: the open degrades to a Candidate, which
                // grants no delete right.
                repo::messages::set_body(
                    db,
                    &message_id_for_db,
                    Some(&path_for_db),
                    Some(&text_for_fts),
                    None,
                )?;
                repo::messages::update_preview(db, &message_id_for_db, &preview)?;
                repo::folders::recompute_counts(db, &folder_id)?;
                Ok(())
            })
            .await?;

        events::emit_mail_changed(
            self.inner.ctx.events.as_ref(),
            account.id.clone(),
            vec![folder.id.clone()],
            vec![message_id.clone()],
            MailChangeKind::Updated,
        );
        Ok(message_id)
    }

    /// Remove the Drafts projection of a local draft (send/discard).
    async fn remove_draft_message(&self, draft_id: &str) -> Result<Option<String>> {
        let message_id = crate::util::draft_message_id(draft_id);
        let stored = {
            let id = message_id.clone();
            self.inner
                .ctx
                .db
                .read(move |db| repo::messages::get(db, &id))
                .await?
        };
        let Some(stored) = stored else {
            return Ok(None);
        };
        let folder_id = stored.summary.folder_id.clone();
        let id = message_id.clone();
        self.inner
            .ctx
            .db
            .write(move |db| repo::messages::delete_local(db, &[id], true))
            .await?;
        events::emit_mail_changed(
            self.inner.ctx.events.as_ref(),
            stored.summary.account_id.clone(),
            vec![folder_id.clone()],
            vec![message_id],
            MailChangeKind::Deleted,
        );
        Ok(Some(folder_id))
    }

    async fn drafts_folder_id(&self, account_id: &str) -> Result<Option<String>> {
        Ok(self
            .folder_by_role(account_id, FolderRole::Drafts)
            .await?
            .map(|folder| folder.id))
    }

    /// Queue the server-side work of a save in one transaction.
    ///
    /// The suppression check, the old-copy DELETE (Graph) and the new APPEND
    /// share one decision: a draft the user kept local must enqueue neither.
    /// The same transaction stops a concurrent "keep local" that commits first,
    /// and prevents a second APPEND in the same debounce window.
    ///
    /// Returns whether an APPEND was enqueued.
    async fn enqueue_draft_sync_ops(
        &self,
        account_id: &str,
        draft_id: &str,
        append: Option<AppendDraftOp>,
        delete: Option<DeleteRemoteDraftOp>,
    ) -> Result<bool> {
        let account_id = account_id.to_string();
        let draft_id = draft_id.to_string();
        let staged = self
            .inner
            .ctx
            .db
            .write(move |db| {
                let Some(draft) = repo::drafts::get(db, &draft_id)? else {
                    // A concurrent discard removed the local draft; uploading
                    // it now would resurrect a draft the user deleted.
                    return Ok(false);
                };
                if draft.sync_suppressed {
                    // Suppressed means "keep this draft local": neither the
                    // upload nor the deletion of its remote copy may run.
                    return Ok(false);
                }
                // A Graph create whose completion was lost (a 2xx without a
                // usable id, a transport error, or a 5xx) may already have
                // created a remote copy. That uncertainty survives even after
                // the operation exhausts its retries and becomes `failed`:
                // without this check a later autosave would POST a second
                // copy. Keep saving locally, but do not upload again.
                let graph = repo::accounts::get(db, &account_id)?
                    .map(|account| crate::graph::uses_graph(&account))
                    .unwrap_or(false);
                if graph {
                    for op in repo::pending_ops::list(db, Some(&account_id))? {
                        if op.kind != KIND_APPEND_DRAFT {
                            continue;
                        }
                        let Ok(payload) = repo::pending_ops::payload::<AppendDraftOp>(&op) else {
                            continue;
                        };
                        if payload.draft_id == draft_id
                            && payload.appended_remote_id.is_none()
                            && (payload.outcome_unknown
                                || payload.append_state == crate::ops::AppendState::Unknown)
                        {
                            debug!(
                                draft = %draft_id,
                                state = ?op.state,
                                "a Graph create with an unknown outcome blocks another upload"
                            );
                            return Ok(false);
                        }
                    }
                }
                // A replacement is a pair: the old-copy DELETE is only valid
                // together with the APPEND that supersedes it. Without an
                // APPEND there is no replacement, so a lone delete is never
                // queued (it would remove the draft the user is still editing).
                let Some(append) = append else {
                    return Ok(false);
                };
                // Decide whether the *whole* replacement may be queued before
                // writing anything: a second APPEND in the same debounce window
                // means this one is stale, and its paired DELETE must not be
                // committed on its own.
                for op in repo::pending_ops::list(db, Some(&account_id))? {
                    if op.kind != KIND_APPEND_DRAFT
                        || !matches!(
                            op.state,
                            PendingOpState::Pending | PendingOpState::InProgress
                        )
                    {
                        continue;
                    }
                    if repo::pending_ops::payload::<AppendDraftOp>(&op)
                        .map(|payload| payload.draft_id == draft_id)
                        .unwrap_or(false)
                    {
                        return Ok(false);
                    }
                }
                // Graph replacement ordering is deliberate: the old-copy DELETE
                // is enqueued before the APPEND, so the queue runs it first.
                // If the APPEND then fails, the old *remote* copy is already
                // gone while the local draft stays in Drafts and its APPEND is
                // retried (or can be retried by the user after a permanent
                // failure). The local draft is the source of truth, so no
                // content is lost; this is accepted instead of a staged
                // append-first replacement, which would leave two server copies
                // whenever the follow-up delete cannot be attributed. IMAP does
                // not use this pair: it carries the old copy in the APPEND's
                // `replaces` and deletes it only in the same execution.
                if let Some(delete) = delete {
                    if !pending_delete_from(db, &account_id, &delete.folder_id, delete.uid)? {
                        let value = serde_json::to_value(delete).map_err(|error| {
                            AppError::internal(format!("cannot encode deleteRemoteDraft: {error}"))
                        })?;
                        repo::pending_ops::enqueue(
                            db,
                            NewPendingOp::new(
                                &account_id,
                                KIND_DELETE_REMOTE_DRAFT,
                                value,
                                "Remove old remote draft",
                            ),
                        )?;
                    }
                }
                let value = serde_json::to_value(append).map_err(|error| {
                    AppError::internal(format!("cannot encode appendDraft: {error}"))
                })?;
                repo::pending_ops::enqueue(
                    db,
                    NewPendingOp::new(
                        &account_id,
                        KIND_APPEND_DRAFT,
                        value,
                        "Save draft to server",
                    ),
                )?;
                Ok(true)
            })
            .await?;
        Ok(staged)
    }

    /// Stop the queued APPEND for a draft that is being discarded or sent.
    ///
    /// Only an `Idle` APPEND may be cancelled outright: `Preparing` and later
    /// phases may already have replaced/created a server copy, and cancelling
    /// the row would leave that copy behind with nothing to clean it up. For
    /// those phases the cleanup intent is persisted on the operation itself; the
    /// executor finishes the upload (it cannot be un-sent) and then deletes the
    /// copy once its identity is known.
    ///
    /// The suppression marker is written in the same transaction as the
    /// cancellations, so a concurrent autosave cannot pass its own check after
    /// this call and still enqueue an upload.
    ///
    /// Returns whether a cleanup was deferred to the executor.
    async fn cancel_pending_draft_appends(&self, account_id: &str, draft_id: &str) -> Result<bool> {
        let account_id = account_id.to_string();
        let draft_id = draft_id.to_string();
        self.inner
            .ctx
            .db
            .write(move |db| {
                // The draft is gone from the composer's point of view. Suppress
                // further uploads even before the row is deleted.
                repo::drafts::set_sync_suppressed(db, &draft_id, true)?;
                cancel_pending_appends_from(db, &account_id, &draft_id)
            })
            .await
    }

    async fn find_remote_draft(
        &self,
        account: &nuncio_core::models::Account,
        drafts_folder: &nuncio_core::models::Folder,
        draft: &Draft,
        header_id: &str,
    ) -> Result<Option<StoredMessage>> {
        if let Some(message_id) = draft.remote_draft_message_id.as_deref() {
            if let Ok(stored) = self.stored(message_id).await {
                if stored.summary.account_id == account.id
                    && stored.summary.folder_id == drafts_folder.id
                {
                    return Ok(Some(stored));
                }
            }
        }
        let account_id = account.id.clone();
        let header = header_id.to_string();
        let folder_id = drafts_folder.id.clone();
        let found = self
            .inner
            .ctx
            .db
            .read(move |db| {
                let rows = repo::messages::by_message_id(db, &header, Some(&account_id))?;
                Ok(rows
                    .into_iter()
                    .find(|row| row.summary.folder_id == folder_id))
            })
            .await?;
        Ok(found)
    }
}

fn is_local_only_account(provider: ProviderKind) -> bool {
    matches!(provider, ProviderKind::Local | ProviderKind::Pop3)
}

pub(crate) fn identity_address(identity: &Identity) -> Address {
    Address {
        name: if identity.name.trim().is_empty() {
            None
        } else {
            Some(identity.name.clone())
        },
        email: identity.email.clone(),
    }
}

fn signature_block(signature: Option<&Signature>) -> String {
    match signature {
        Some(signature) if !signature.html.trim().is_empty() => {
            // Signatures are HTML supplied by the user (or restored from a
            // backup); sanitize before inserting into the composer.
            let sanitized = nuncio_mime::sanitize_fragment(&signature.html);
            if sanitized.trim().is_empty() {
                return String::new();
            }
            format!("<div class=\"nuncio-signature\"><br>{sanitized}</div>")
        }
        _ => String::new(),
    }
}

fn insert_signature(body: &str, signature: Option<&Signature>) -> String {
    let block = signature_block(signature);
    if block.is_empty() {
        return body.to_string();
    }
    if let Some(rest) = body.strip_prefix("<p><br></p>") {
        format!("<p><br></p>{block}{rest}")
    } else {
        format!("{block}{body}")
    }
}

fn draft_message_id(draft_id: &str) -> String {
    format!("nuncio-draft-{draft_id}@nuncio.local")
}

/// Pick the identity a save should use for the copy it supersedes.
///
/// A receipt for the APPEND that just replaced the old copy is newer evidence
/// than the binding the composer carried, so it must win even over an
/// `Explicit` binding of the copy it deleted. The exception is a genuinely
/// newer explicit open of a *different* copy: the receipt only wins when it
/// names the same copy or its `replaces` is exactly the explicit binding. A
/// candidate-only attempt shadows an older confirmed receipt (the newest
/// version's copy is the ambiguous one), but not an explicit binding the user
/// just opened.
fn choose_remote_identity(
    existing: Option<RemoteDraftIdentity>,
    incoming: Option<RemoteDraftIdentity>,
    ownership: Option<&AppendOwnership>,
    draft_id: &str,
) -> Option<RemoteDraftIdentity> {
    match ownership {
        Some(AppendOwnership::Confirmed {
            folder_id,
            uid,
            uidvalidity,
            superseded,
        }) => {
            let owned = RemoteDraftIdentity::confirmed(
                folder_id,
                *uid,
                Some(*uidvalidity),
                Some(draft_message_id(draft_id)),
            );
            // "Same copy" includes UIDVALIDITY: a UID is only meaningful inside
            // its generation, so equal folder/UID in another generation names a
            // different message.
            let keep_explicit = existing.as_ref().is_some_and(|existing| {
                existing.attribution == RemoteDraftAttribution::Explicit
                    && !(existing.folder_id == owned.folder_id
                        && existing.uid == owned.uid
                        && existing.uidvalidity == owned.uidvalidity)
                    && !superseded
                        .as_ref()
                        .is_some_and(|(folder_id, uid, uidvalidity)| {
                            existing.folder_id.as_deref() == Some(folder_id.as_str())
                                && existing.uid == Some(*uid)
                                && existing.uidvalidity == *uidvalidity
                        })
            });
            if keep_explicit {
                existing
            } else {
                Some(owned)
            }
        }
        Some(AppendOwnership::GraphConfirmed {
            folder_id,
            remote_id,
        }) => {
            let owned = RemoteDraftIdentity::confirmed_graph(
                folder_id,
                remote_id,
                Some(draft_message_id(draft_id)),
            );
            // A genuinely newer explicit open of a different server row wins;
            // otherwise the receipt names the copy this save supersedes.
            let keep_explicit = existing.as_ref().is_some_and(|identity| {
                identity.attribution == RemoteDraftAttribution::Explicit
                    && identity.remote_id.as_deref() != Some(remote_id.as_str())
            });
            if keep_explicit {
                existing
            } else {
                Some(owned)
            }
        }
        Some(AppendOwnership::CandidateOnly) => {
            if existing
                .as_ref()
                .is_some_and(|identity| identity.attribution == RemoteDraftAttribution::Explicit)
            {
                return existing;
            }
            let folder_id = incoming
                .as_ref()
                .and_then(|identity| identity.folder_id.clone())
                .or_else(|| {
                    existing
                        .as_ref()
                        .and_then(|identity| identity.folder_id.clone())
                });
            Some(RemoteDraftIdentity::candidate(
                folder_id,
                Some(draft_message_id(draft_id)),
            ))
        }
        None => match (existing, incoming) {
            (Some(existing), _) => Some(existing),
            (None, incoming) => incoming,
        },
    }
}

fn all_recipients(draft: &Draft) -> Result<Vec<Address>> {
    let mut recipients = draft.to.clone();
    recipients.extend(draft.cc.clone());
    recipients.extend(draft.bcc.clone());
    let recipients = nuncio_mime::dedupe_addresses(recipients);
    if recipients.is_empty() {
        return Err(AppError::invalid_input(
            "add at least one recipient before sending",
        ));
    }
    for address in &recipients {
        let email = address.email.trim();
        if !email.contains('@') || email.starts_with('@') || email.ends_with('@') {
            return Err(AppError::invalid_input(format!(
                "invalid recipient address: {email}"
            )));
        }
    }
    Ok(recipients)
}

pub(crate) async fn unique_name(dir: &Path, filename: &str, used: &mut HashSet<String>) -> String {
    let sanitized = nuncio_core::paths::sanitize_filename(filename);
    let base = if sanitized.is_empty() {
        "attachment".to_string()
    } else {
        sanitized
    };
    let (stem, extension) = match base.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => {
            (stem.to_string(), Some(extension.to_string()))
        }
        _ => (base.clone(), None),
    };
    let mut index = 0u32;
    loop {
        let candidate = if index == 0 {
            base.clone()
        } else {
            match &extension {
                Some(extension) => format!("{stem} ({index}).{extension}"),
                None => format!("{stem} ({index})"),
            }
        };
        let key = candidate.to_lowercase();
        let exists = used.contains(&key) || tokio::fs::metadata(dir.join(&candidate)).await.is_ok();
        if !exists {
            used.insert(key);
            return candidate;
        }
        index += 1;
        if index > 10_000 {
            return format!("{}-{}", nuncio_core::models::new_id(), base);
        }
    }
}

fn safe_stem(subject: &str) -> String {
    let stem: String = subject
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, ' ' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let stem = stem.trim().trim_matches('_').to_string();
    if stem.is_empty() {
        "message".to_string()
    } else {
        stem.chars().take(60).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `compose_open_draft` compares the provider identity on both sides of
    /// the body read. A rebind may keep the local row id (and even its UID)
    /// while changing the Graph id, or change the UID for IMAP; both must be
    /// visible to the comparison so the returned identity is downgraded.
    #[test]
    fn draft_copy_key_sees_a_provider_rebind() {
        let mut summary =
            MessageSummary::new("row", "acc", "drafts", "thread", nuncio_core::models::now());
        summary.is_draft = true;
        let mut before = StoredMessage::new(summary.clone());
        before.uid = Some(7);
        before.uidvalidity = Some(3);
        before.gm_msgid = Some("copy-a".into());
        assert_eq!(
            MailService::draft_copy_key(&before),
            MailService::draft_copy_key(&before.clone())
        );

        let mut graph_rebound = before.clone();
        graph_rebound.gm_msgid = Some("copy-b".into());
        assert_ne!(
            MailService::draft_copy_key(&before),
            MailService::draft_copy_key(&graph_rebound),
            "the same row/UID bound to another Graph id is a different copy"
        );

        let mut imap_rebound = before.clone();
        imap_rebound.uid = Some(8);
        assert_ne!(
            MailService::draft_copy_key(&before),
            MailService::draft_copy_key(&imap_rebound),
            "a new UID is a different server copy"
        );
    }

    /// A cached body must be proven to belong to the provider copy it was read
    /// from. Comparing only the row identity across the open misses a rebind
    /// that happened *before* it: the row, the UID and even `gm_msgid` already
    /// name B while `body_path` still holds A's MIME.
    #[test]
    fn cached_body_must_belong_to_the_current_copy() {
        let mut summary =
            MessageSummary::new("row", "acc", "drafts", "thread", nuncio_core::models::now());
        summary.is_draft = true;
        let mut copy_a = StoredMessage::new(summary.clone());
        copy_a.uid = Some(7);
        copy_a.uidvalidity = Some(3);
        copy_a.body_path = Some("/tmp/row-0123456789abcdef.eml".into());
        let key_a = repo::messages::body_identity_key(&copy_a).expect("copy key");
        let raw = b"Subject: draft\r\n\r\nA's bytes";
        let version_a = crate::sync::body::body_cache_version(Some(&key_a), raw);
        let source_a = crate::service::RawMessage {
            bytes: raw.to_vec(),
            version: Some(version_a.clone()),
            identity: Some(key_a.clone()),
        };
        assert!(MailService::body_source_matches(&copy_a, &source_a, raw));

        let mut copy_b = copy_a.clone();
        copy_b.uid = Some(8);
        assert!(
            !MailService::body_source_matches(&copy_b, &source_a, raw),
            "A's cached bytes must not authorize B"
        );
        assert!(
            !MailService::body_source_matches(
                &copy_b,
                &crate::service::RawMessage {
                    bytes: raw.to_vec(),
                    version: None,
                    identity: None,
                },
                raw,
            ),
            "a legacy/unbound cache cannot prove its source"
        );

        // A legacy unversioned path still proves its source through the
        // identity snapshot taken with the path, but only for that same copy.
        let legacy_a = crate::service::RawMessage {
            bytes: raw.to_vec(),
            version: None,
            identity: Some(key_a.clone()),
        };
        assert!(MailService::body_source_matches(&copy_a, &legacy_a, raw));
        assert!(!MailService::body_source_matches(&copy_b, &legacy_a, raw));

        // A Graph rebind keeps neither the synthetic UID nor the provider id,
        // so the version computed for B cannot match A's file.
        let mut graph_a = StoredMessage::new(summary.clone());
        graph_a.uid = Some(11);
        graph_a.gm_msgid = Some("copy-a".into());
        graph_a.body_path = Some("/tmp/graph-0123456789abcdef.eml".into());
        let graph_key = repo::messages::body_identity_key(&graph_a).expect("graph key");
        let graph_version = crate::sync::body::body_cache_version(Some(&graph_key), raw);
        let graph_source = crate::service::RawMessage {
            bytes: raw.to_vec(),
            version: Some(graph_version),
            identity: Some(graph_key),
        };
        assert!(MailService::body_source_matches(
            &graph_a,
            &graph_source,
            raw
        ));
        let mut graph_b = graph_a.clone();
        graph_b.gm_msgid = Some("copy-b".into());
        assert!(!MailService::body_source_matches(
            &graph_b,
            &graph_source,
            raw
        ));

        // Without a cached path the body is fetched during this open; before
        // that fetch has recorded its source there is no binding to trust.
        let mut uncached = copy_b.clone();
        uncached.body_path = None;
        assert!(
            !MailService::body_source_matches(&uncached, &source_a, raw),
            "an uncached body cannot prove its source yet"
        );
    }

    #[test]
    fn cache_file_names_expose_only_the_version_token() {
        use std::path::Path;
        assert_eq!(
            crate::sync::body::body_cache_version_from_path(Path::new(
                "/data/mime/acc/ro/row-0123456789abcdef.eml"
            ))
            .as_deref(),
            Some("0123456789abcdef")
        );
        assert_eq!(
            crate::sync::body::body_cache_version_from_path(Path::new("/tmp/row.eml")),
            None
        );
        assert_eq!(
            crate::sync::body::body_cache_version_from_path(Path::new(
                "/tmp/row-0123456789ABCDEF.eml"
            )),
            None,
            "only the lowercase hex produced by the writer is a version"
        );
    }

    #[test]
    fn signature_block_is_sanitized_and_marked_for_swapping() {
        let signature = Signature {
            id: "sig-1".into(),
            account_id: None,
            name: "Work".into(),
            html: concat!(
                "<p onclick=\"steal()\">Hi</p>",
                "<script>alert(1)</script>",
                "<a href=\"javascript:alert(1)\">click</a>",
                "<img src=\"https://example.com/a.png\">"
            )
            .into(),
            use_for_new: true,
            use_for_reply: true,
        };
        let block = signature_block(Some(&signature));
        assert!(block.starts_with("<div class=\"nuncio-signature\">"));
        assert!(block.ends_with("</div>"));
        assert!(block.contains("Hi"));
        assert!(!block.to_ascii_lowercase().contains("<script"));
        assert!(!block.to_ascii_lowercase().contains("onclick"));
        assert!(!block.to_ascii_lowercase().contains("javascript:"));
    }

    #[test]
    fn empty_signatures_produce_no_block() {
        assert_eq!(signature_block(None), "");
        let signature = Signature {
            id: "sig-2".into(),
            account_id: None,
            name: "Empty".into(),
            html: "   ".into(),
            use_for_new: false,
            use_for_reply: false,
        };
        assert_eq!(signature_block(Some(&signature)), "");
        assert_eq!(insert_signature("<p><br></p>x", None), "<p><br></p>x");
    }

    /// The reply/forward flag is prepared from a row read outside the queue
    /// transaction. If that projection is deleted or rebound before the
    /// transaction runs, neither the queue row nor its writer may be created:
    /// otherwise the executor sees a writer authorizing the old UID and can
    /// `UID STORE` whatever now owns that number.
    #[test]
    fn source_flags_refuse_a_deleted_or_rebound_projection() {
        use nuncio_core::models::{
            Account, AccountState, AuthKind, Folder, FolderRole, MessageSummary,
            ProviderCapabilities, ProviderKind, StoredMessage,
        };

        fn account() -> Account {
            Account {
                id: "acc".into(),
                provider: ProviderKind::Imap,
                email: "test@example.com".into(),
                display_name: "Test".into(),
                color: "#000000".into(),
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

        fn stored() -> StoredMessage {
            let mut summary = MessageSummary::new(
                "m1",
                "acc",
                "inbox",
                "thread-source",
                nuncio_core::models::now(),
            );
            summary.seen = false;
            let mut stored = StoredMessage::new(summary);
            stored.uid = Some(42);
            stored.uidvalidity = Some(7);
            stored
        }

        let db = nuncio_core::Db::open_in_memory().expect("db");
        db.migrate_core().expect("migrate");
        let folder = Folder::new("inbox", "acc", "INBOX", FolderRole::Inbox);
        let message = stored();
        db.write_sync(|conn| {
            repo::accounts::insert(conn, &account())?;
            repo::folders::insert(conn, &folder)?;
            repo::messages::upsert(conn, &message)?;
            Ok(())
        })
        .expect("seed");

        let identity = repo::messages::MessageIdentity::of(&message);
        let queued = (
            "acc".to_string(),
            serde_json::json!({
                "folderId": "inbox",
                "uids": [42],
                "seen": null,
                "flagged": null,
                "answered": true,
                "forwarded": null,
                "sourceUidvalidity": 7,
                "messageIds": ["m1"],
            }),
        );
        let op_count = |db: &nuncio_core::Db| {
            db.read_sync(|conn| repo::pending_ops::pending_count(conn, Some("acc")))
                .expect("pending count")
        };
        let writer = |db: &nuncio_core::Db| {
            db.read_sync(|conn| repo::messages::flag_writer(conn, "m1", "answered"))
                .expect("writer")
        };

        // 1. The projection was permanently deleted after the read.
        db.write_sync(|conn| repo::messages::delete_local(conn, &["m1".to_string()], true))
            .expect("delete projection");
        db.write_sync(|conn| {
            crate::ops::apply_source_flags_in_db(conn, &identity, true, false, Some(&queued))
        })
        .expect("apply after delete");
        assert_eq!(op_count(&db), 0, "a deleted projection must not queue");
        assert!(writer(&db).is_none(), "a deleted projection gets no writer");

        // 2. The same local id now names a different UID generation.
        db.write_sync(|conn| repo::messages::upsert(conn, &message))
            .expect("restore row");
        let mut rebound = message.clone();
        rebound.uid = Some(43);
        db.write_sync(|conn| repo::messages::upsert(conn, &rebound))
            .expect("rebind row");
        db.write_sync(|conn| {
            crate::ops::apply_source_flags_in_db(conn, &identity, true, false, Some(&queued))
        })
        .expect("apply after rebind");
        assert_eq!(op_count(&db), 0, "a rebound projection must not queue");
        assert!(writer(&db).is_none(), "a rebound projection gets no writer");
        let current = db
            .read_sync(|conn| repo::messages::get_required(conn, "m1"))
            .expect("row after rebind");
        assert_eq!(current.uid, Some(43), "the rebinding is left intact");
        assert!(!current.summary.answered, "the stale action wrote nothing");

        // 3. The identity still matches: the same transaction writes the local
        // value, the queue row and the writer.
        db.write_sync(|conn| repo::messages::upsert(conn, &message))
            .expect("restore copy");
        db.write_sync(|conn| {
            crate::ops::apply_source_flags_in_db(conn, &identity, true, false, Some(&queued))
        })
        .expect("apply to matching row");
        assert_eq!(op_count(&db), 1);
        let op = db
            .read_sync(|conn| repo::pending_ops::list(conn, Some("acc")))
            .expect("ops")
            .into_iter()
            .find(|op| op.kind == crate::ops::KIND_SET_FLAGS)
            .expect("setFlags op");
        assert_eq!(op.payload["messageIds"], serde_json::json!(["m1"]));
        let writer = writer(&db).expect("answered writer");
        assert_eq!(writer.owner_id, op.id);
        let current = db
            .read_sync(|conn| repo::messages::get_required(conn, "m1"))
            .expect("row");
        assert!(current.summary.answered);
    }
}
