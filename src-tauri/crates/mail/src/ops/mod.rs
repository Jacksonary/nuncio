//! Persistent server-operation queue.
//!
//! The command-facing service applies changes locally first and then enqueues
//! one of the typed payloads below. The account worker drains due operations in
//! creation order, batches compatible flag changes, and retries with the
//! exponential backoff stored by `nuncio_core::repo::pending_ops`.

pub mod executor;

use chrono::{DateTime, Utc};
use nuncio_core::models::NewPendingOp;
use nuncio_core::{repo, AppError, CoreContext, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::debug;

/// Operation kind identifiers persisted in `pending_op.kind`.
pub const KIND_SET_FLAGS: &str = "setFlags";
pub const KIND_MOVE: &str = "move";
pub const KIND_DELETE: &str = "delete";
pub const KIND_ARCHIVE: &str = "archive";
pub const KIND_MARK_JUNK: &str = "markJunk";
pub const KIND_APPEND_DRAFT: &str = "appendDraft";
pub const KIND_DELETE_REMOTE_DRAFT: &str = "deleteRemoteDraft";
pub const KIND_APPEND_TO_SENT: &str = "appendToSent";
pub const KIND_FOLDER_CREATE: &str = "folderCreate";
pub const KIND_FOLDER_RENAME: &str = "folderRename";
pub const KIND_FOLDER_DELETE: &str = "folderDelete";
pub const KIND_FOLDER_SUBSCRIBE: &str = "folderSubscribe";
pub const KIND_FOLDER_MARK_READ: &str = "folderMarkRead";
pub const KIND_SEND: &str = "send";

/// Folder an archive action must project into and move to.
///
/// Gmail archives by removing the Inbox label, so the message ends up in All
/// Mail; a coincidental extra `\Archive` label must not become the target.
/// Every other IMAP account uses a real `\Archive`; `\All` is only a virtual
/// view and may be read-only, so it is never chosen here (the executor creates
/// an Archive when the account has none).
pub fn archive_target(gmail: bool, archive: Option<&str>, all: Option<&str>) -> Option<String> {
    if gmail {
        // Gmail archives with `-X-GM-LABELS (\Inbox)`, which never creates or
        // adds an `\Archive` label. Falling back to an `\Archive` folder here
        // would move the local projection to a folder the command does not
        // produce. Without All Mail there is no folder to project into.
        all.map(str::to_string)
    } else {
        archive.map(str::to_string)
    }
}

/// Kinds owned by the mail engine.
///
/// `pending_op` is shared by every module (mail, contacts, calendar, tasks) and
/// is keyed by account, not by kind. Each worker must therefore claim only its
/// own rows: without this filter the mail worker picks up `pim.*` operations,
/// fails them with "unknown pending operation kind", and either rolls nothing
/// back (IMAP/Graph) or marks them done without ever contacting the server
/// (POP3). PIM modules keep the `pim.` namespace reserved for themselves.
pub fn is_mail_kind(kind: &str) -> bool {
    matches!(
        kind,
        KIND_SET_FLAGS
            | KIND_MOVE
            | KIND_DELETE
            | KIND_ARCHIVE
            | KIND_MARK_JUNK
            | KIND_APPEND_DRAFT
            | KIND_DELETE_REMOTE_DRAFT
            | KIND_APPEND_TO_SENT
            | KIND_FOLDER_CREATE
            | KIND_FOLDER_RENAME
            | KIND_FOLDER_DELETE
            | KIND_FOLDER_SUBSCRIBE
            | KIND_FOLDER_MARK_READ
            | KIND_SEND
    )
}

/// Local flag state captured before an optimistic change, so a permanently
/// failed `UID STORE` can be rolled back.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlagRollback {
    pub uid: i64,
    pub seen: bool,
    pub flagged: bool,
    pub answered: bool,
    pub forwarded: bool,
}

/// Per-message outcome of a queued operation.
///
/// A batch command does not settle as one unit: Graph deletes/moves messages
/// one by one, and an ambiguous IMAP response can leave earlier messages in a
/// different state from later ones. Undo and rollback must use this evidence
/// instead of the operation's aggregate state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OpMessageState {
    /// The server confirmed this exact message's command.
    Confirmed,
    /// The command provably did not execute for this message.
    NotExecuted,
    /// The command may or may not have executed for this message.
    Unknown,
}

/// How far the COPY phase of an IMAP move fallback got for one message.
///
/// A server without MOVE moves by COPY + STORE `\Deleted` + UID EXPUNGE. The
/// phases are not atomic, so a retry must know which ones already reached the
/// server: a second COPY would leave a duplicate in the target folder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CopyPhase {
    /// No COPY was recorded for this message by an earlier attempt.
    #[default]
    NotStarted,
    /// A COPY was sent but its tagged completion was not received (or a
    /// payload from an older build proves only that *some* command was sent).
    /// The server may have created the copy, so it must never be sent again
    /// without positive evidence from the target folder.
    Unknown,
    /// The server accepted the COPY. `target_uid`/`target_uidvalidity` are set
    /// when the receipt (`COPYUID`) or the target folder binding identified
    /// the copy; they stay `None` while only the existence is known.
    Copied,
}

/// Whether the `\Deleted` STORE phase reached the server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeleteFlagPhase {
    #[default]
    NotStarted,
    /// The STORE was sent but its tagged completion was not received. STORE is
    /// idempotent, so a retry may send it again.
    Unknown,
    /// The server confirmed `+FLAGS \Deleted`.
    Applied,
}

/// Whether the directed EXPUNGE phase reached the server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExpungePhase {
    #[default]
    NotStarted,
    /// `UID EXPUNGE` was sent but its tagged completion was not received.
    /// Repeating it for the same still-unexpunged UIDs is safe within the
    /// generation, so a retry may send it again.
    Unknown,
    /// The server confirmed the source copy was removed.
    Applied,
}

/// One message's outcome inside a queued operation payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpMessageOutcome {
    pub message_id: String,
    /// Source UID the command addressed (kept for diagnostics and retries).
    #[serde(default)]
    pub source_uid: Option<i64>,
    pub state: OpMessageState,
    /// Folder-local destination identity, if the server reported one
    /// (`COPYUID`, a returned Graph id, or a binding this operation performed).
    #[serde(default)]
    pub target_folder_id: Option<String>,
    #[serde(default)]
    pub target_uid: Option<i64>,
    #[serde(default)]
    pub target_uidvalidity: Option<i64>,
    #[serde(default)]
    pub target_remote_id: Option<String>,
    /// The exact provider id a delete attempt addressed. Persisted together
    /// with `Unknown` *before* the request is sent, so a retry verifies that id
    /// instead of re-resolving a mapping a sync may have rebound to a
    /// different message. A later attempt must never negate this evidence.
    #[serde(default)]
    pub attempt_remote_id: Option<String>,
    /// COPY phase of the IMAP move fallback (see [`CopyPhase`]). Payloads from
    /// builds that predate phase tracking deserialize as `NotStarted`; the
    /// operation-level `sendIntent` still protects them from a blind re-COPY.
    #[serde(default)]
    pub copy_phase: CopyPhase,
    /// `\Deleted` STORE phase of the IMAP move fallback.
    #[serde(default)]
    pub delete_flag_phase: DeleteFlagPhase,
    /// Directed EXPUNGE phase of the IMAP move fallback.
    #[serde(default)]
    pub expunge_phase: ExpungePhase,
}

/// The outcome recorded for one message, if any.
pub fn outcome_for<'a>(
    outcomes: &'a [OpMessageOutcome],
    message_id: &str,
) -> Option<&'a OpMessageOutcome> {
    outcomes
        .iter()
        .find(|outcome| outcome.message_id == message_id)
}

/// A message outcome with a confirmed server command.
pub fn confirmed_outcome<'a>(
    outcomes: &'a [OpMessageOutcome],
    message_id: &str,
) -> Option<&'a OpMessageOutcome> {
    outcome_for(outcomes, message_id).filter(|outcome| outcome.state == OpMessageState::Confirmed)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetFlagsOp {
    pub folder_id: String,
    pub uids: Vec<i64>,
    pub seen: Option<bool>,
    pub flagged: Option<bool>,
    pub answered: Option<bool>,
    pub forwarded: Option<bool>,
    /// UIDVALIDITY of the folder when the op was queued. A UID is only
    /// meaningful within that generation, so the executor refuses the command
    /// (and lets reconciliation take over) when the folder was rebuilt.
    #[serde(default)]
    pub source_uidvalidity: Option<i64>,
    /// Flags before the optimistic local update (empty for ops queued by
    /// background code paths that did not change local flags first).
    #[serde(default)]
    pub previous: Vec<FlagRollback>,
    /// Local message ids covered by the change. Optional ownership aid for undo
    /// compensation; the command itself still addresses rows by UID within the
    /// queued generation.
    #[serde(default)]
    pub message_ids: Vec<String>,
    /// Confirmed provider ids aligned with `uids` (Graph).
    ///
    /// Graph has no UID generation, so a synthetic UID alone cannot prove which
    /// copy a queued command means. When set, the Graph executor sends the
    /// PATCH to exactly this id instead of re-resolving the mutable uid
    /// mapping. An empty slot keeps the older resolution behaviour.
    #[serde(default)]
    pub remote_ids: Vec<String>,
    /// Per-message results that distinguish a settled command from a command
    /// that was skipped because a later action took the field over.
    ///
    /// A batch is settled as one `done` row, but a message whose field belongs
    /// to a newer operation was never written. Undo must not hand that field
    /// back to this row (it will not run again); instead it re-queues the
    /// restored value. An absent entry means the command either ran or is
    /// still queued, and is treated as delivered when the row carries a send
    /// intent.
    #[serde(default)]
    pub outcomes: Vec<OpMessageOutcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveOp {
    pub folder_id: String,
    pub target_folder_id: String,
    pub uids: Vec<i64>,
    /// UIDVALIDITY of the source folder when the op was queued; a rollback
    /// restores the source UID only if the folder still has this generation.
    #[serde(default)]
    pub source_uidvalidity: Option<i64>,
    /// Local message ids of the moved rows. The optimistic move clears
    /// `uid`/`uidvalidity` (the server assigns a new UID in the target
    /// folder), so a rollback cannot find the rows by UID any more; these are
    /// the stable handles. Empty for payloads queued before this field
    /// existed, which fall back to the UID lookup.
    #[serde(default)]
    pub message_ids: Vec<String>,
    /// Per-message results persisted as the command progresses. Undo only
    /// compensates a message whose outcome is `confirmed` and only uses the
    /// destination identity recorded here.
    #[serde(default)]
    pub outcomes: Vec<OpMessageOutcome>,
    /// Direct remote ids aligned with `uids`. Graph assigns a new id on move;
    /// an inverse queued by undo uses the id the original operation returned
    /// instead of re-resolving a UID that a later sync may have rebound.
    #[serde(default)]
    pub remote_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteOp {
    pub folder_id: String,
    pub uids: Vec<i64>,
    pub permanent: bool,
    /// Folder the optimistic local change moved the rows into (Trash).
    #[serde(default)]
    pub target_folder_id: Option<String>,
    /// Rows were hidden with `is_deleted_local` instead of being moved.
    #[serde(default)]
    pub soft_hidden: bool,
    /// Local ids of the affected rows; the optimistic move clears their UID,
    /// so a rollback cannot resolve them by UID any more.
    #[serde(default)]
    pub message_ids: Vec<String>,
    /// Source-folder generation captured when the op was queued.
    #[serde(default)]
    pub source_uidvalidity: Option<i64>,
    /// The optimistic local change hid the rows with `is_deleted_local` and
    /// kept their full state (attachments and FTS) until the server confirms
    /// the permanent delete. A refused command unhides them again; payloads
    /// queued by older versions hard-deleted the rows and have this unset.
    #[serde(default)]
    pub pending_delete: bool,
    /// `+FLAGS \Deleted` already reached the server for this payload. A later
    /// `EXPUNGE` may have been refused (or its outcome is unknown), so the
    /// message is in a side-effected state: unhiding it or trusting the old
    /// UID would be wrong until the folder is reconciled.
    #[serde(default)]
    pub deleted_flag_set: bool,
    /// Row contents captured before an optimistic *permanent* delete removed
    /// them locally. A permanent delete cannot roll back by moving the row
    /// (there is no row left), and a full folder re-sync is limited to the
    /// sync window, so an old message would never come back. Rebuilding from
    /// this snapshot is the recovery path; payloads queued by older versions
    /// have no snapshot and still fall back to the re-sync.
    #[serde(default)]
    pub snapshot: Vec<crate::service::messages::StoredSnapshot>,
    /// Per-message results. Graph deletes messages one by one, so a failure
    /// after the first success must not roll back the whole batch; undo uses
    /// these outcomes instead of the operation's aggregate state.
    #[serde(default)]
    pub outcomes: Vec<OpMessageOutcome>,
    /// Provider ids pinned when the action was queued, aligned with `uids`.
    ///
    /// A Graph sync can rebind the local row (whose synthetic UID stays stable
    /// because it is derived from the row id) to a different provider message
    /// before the queue transaction runs. Re-resolving the UID at execution
    /// time would then address that other copy. The delete may only ever send
    /// this pinned identity; a mapping that no longer names it is refused and
    /// reconciled. Empty for IMAP payloads and for payloads queued by older
    /// builds.
    #[serde(default)]
    pub remote_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveOp {
    pub folder_id: String,
    pub uids: Vec<i64>,
    /// Archive folder chosen when the op was queued (rollback target). This is
    /// also the folder the optimistic local move used, so the projection and
    /// the command agree even when the account has only `\All` (or a Gmail
    /// "All Mail" label) instead of a dedicated `\Archive`.
    #[serde(default)]
    pub target_folder_id: Option<String>,
    /// The optimistic change hid the row in place because there was no archive
    /// target folder yet. A failed command unhides it by local id (the row was
    /// never moved, so the move rollback would not find it).
    #[serde(default)]
    pub soft_hidden: bool,
    /// Local ids of the moved rows (their UIDs are cleared by the move).
    #[serde(default)]
    pub message_ids: Vec<String>,
    /// Source-folder generation captured when the op was queued.
    #[serde(default)]
    pub source_uidvalidity: Option<i64>,
    /// Per-message results (see [`MoveOp::outcomes`]).
    #[serde(default)]
    pub outcomes: Vec<OpMessageOutcome>,
    /// Direct remote ids (see [`MoveOp::remote_ids`]).
    #[serde(default)]
    pub remote_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkJunkOp {
    pub folder_id: String,
    pub uids: Vec<i64>,
    pub value: bool,
    /// Destination folder chosen when the op was queued (rollback target).
    #[serde(default)]
    pub target_folder_id: Option<String>,
    /// Local ids of the moved rows (their UIDs are cleared by the move).
    #[serde(default)]
    pub message_ids: Vec<String>,
    /// Source-folder generation captured when the op was queued.
    #[serde(default)]
    pub source_uidvalidity: Option<i64>,
    /// Per-message results (see [`MoveOp::outcomes`]).
    #[serde(default)]
    pub outcomes: Vec<OpMessageOutcome>,
    /// Direct remote ids (see [`MoveOp::remote_ids`]).
    #[serde(default)]
    pub remote_ids: Vec<String>,
}

/// A remote draft copy that a replacement APPEND supersedes.
///
/// Carried in the APPEND payload (rather than queued as a separate delete
/// operation) so the executor can decide atomically: remove the old copy
/// first, then append the new one, or do neither. That ordering is what makes
/// "do it, or do not do it" hold when the server has no directed expunge.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDraftRef {
    /// Account the old copy belongs to. Recorded explicitly so the executor
    /// can refuse a projection row owned by another account even if ids were
    /// ever reused.
    #[serde(default)]
    pub account_id: Option<String>,
    pub folder_id: String,
    pub uid: i64,
    /// UIDVALIDITY of the Drafts folder when the replacement was queued.
    #[serde(default)]
    pub source_uidvalidity: Option<i64>,
    /// Local row that projected the old copy when the draft was saved. For a
    /// draft created by another client this is *not* the new projection the
    /// APPEND creates, so ownership must be checked against this row too.
    #[serde(default)]
    pub message_id: Option<String>,
}

/// How far an APPEND draft operation got before it stopped.
///
/// Persisted in the operation payload so a retry never repeats a server-side
/// APPEND that may already have been committed:
///
/// * `Idle` – nothing was sent yet. This is the only phase a discard/send may
///   cancel outright: the wire command is provably not out. Payloads from
///   older builds that carry no phase are *not* treated as `Idle`; they
///   deserialize as `Unknown` (see the field attribute).
/// * `Preparing` – the executor claimed the operation before its first
///   destructive step (deleting the previous server copy). No literal has been
///   written yet, so a retry may safely run the replacement again. It is kept
///   distinct from `Unknown` so a failed delete/read is retried instead of
///   being mistaken for a possibly-committed APPEND.
/// * `Unknown` – the literal was sent but no tagged completion was received.
///   The next attempt must verify the Drafts folder before doing anything; the
///   APPEND must never be repeated while it cannot be proven absent.
/// * `Acked` – the server accepted the APPEND. Only the local commit may be
///   retried; the APPEND itself must never be re-sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AppendState {
    #[default]
    Idle,
    Preparing,
    Unknown,
    Acked,
}

/// Payloads written by builds that predate the phase markers cannot prove the
/// literal was never sent. Defaulting them to `Idle` would re-APPEND a draft
/// that may already be on the server, so an absent phase means `Unknown`.
fn default_append_state() -> AppendState {
    AppendState::Unknown
}

/// Whether the previous server copy was removed before the APPEND.
///
/// The replacement deletes the superseded copy before uploading. If the
/// process stops in between, a blind retry would either delete an unrelated
/// message (the UID may belong to another generation) or skip the upload when
/// the local row disappears during a sync. The removal phase is persisted
/// separately from [`AppendState`] so a retry can tell "the old copy is gone,
/// only the APPEND is missing" from "the removal may or may not have happened".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DraftRemovalState {
    /// No removal was attempted yet (or there is nothing to remove).
    #[default]
    NotStarted,
    /// A directed expunge was about to be (or was) issued; the outcome is not
    /// recorded. A retry must check the mailbox before deleting again.
    PendingRemove,
    /// The server confirmed the previous copy no longer exists.
    Removed,
    /// The removal was attempted but its outcome cannot be determined. Keep
    /// the operation queued and verify instead of repeating the delete.
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppendDraftOp {
    pub draft_id: String,
    /// Drafts folder id.
    pub folder_id: String,
    pub rfc822_path: String,
    /// Previous server copy this APPEND replaces, if one was found when the
    /// draft was saved. The executor refuses the APPEND when the old copy
    /// cannot be removed safely (no UIDPLUS).
    #[serde(default)]
    pub replaces: Option<RemoteDraftRef>,
    /// A previous draft row exists but its server UID is not known yet (the
    /// APPEND ran before the Drafts folder was synced). Uploading again could
    /// leave a second server copy that cannot be identified for deletion, so
    /// the executor skips the APPEND until a sync binds the UID.
    #[serde(default)]
    pub replaces_unidentified: bool,
    /// Phase reached by earlier attempts. Payloads queued by older builds
    /// (which have no phase evidence) deserialize as `Unknown`: the literal
    /// may already be on the server, so the executor verifies instead of
    /// re-APPENDing.
    #[serde(default = "default_append_state")]
    pub append_state: AppendState,
    /// How far the removal of `replaces` got. Kept separate from
    /// [`AppendState`] so an APPEND that was provably not committed can resume
    /// without deleting the previous copy twice.
    #[serde(default)]
    pub removal_state: DraftRemovalState,
    /// UID the server assigned when `append_state == Acked`. Kept separately
    /// (instead of only on the projection row) so a failed local commit can be
    /// retried without contacting the server.
    #[serde(default)]
    pub appended_uid: Option<i64>,
    #[serde(default)]
    pub appended_uidvalidity: Option<i64>,
    /// Graph message id returned by a successful MIME create. The direct
    /// analogue of `appended_uid` for accounts without UIDs; kept on the
    /// operation so a cleanup can address the exact copy even after the local
    /// projection was removed.
    #[serde(default)]
    pub appended_remote_id: Option<String>,
    /// The cleanup commanded by `discard_requested` issued its delete but its
    /// outcome could not be determined. The operation stays queued and the next
    /// attempt verifies the exact Graph id before deleting again; the marker is
    /// retained past `Done`/`Failed` so the uncertainty is never lost.
    #[serde(default)]
    pub outcome_unknown: bool,
    /// Drafts `UIDNEXT` observed just before the literal was handed to the
    /// socket. A verified copy with a lower UID is a copy that already existed
    /// (e.g. the superseded one), not evidence that this APPEND landed.
    #[serde(default)]
    pub attempt_uidnext: Option<i64>,
    /// UIDVALIDITY observed when the literal was sent. A later verification
    /// that sees a different generation cannot correlate the copy safely.
    #[serde(default)]
    pub attempt_uidvalidity: Option<i64>,
    /// Size of the validated bytes handed to the socket. A verified copy whose
    /// size differs is not this upload's copy and must never be deleted.
    #[serde(default)]
    pub attempt_size: Option<u64>,
    /// When the literal was written. Together with the server-recorded
    /// `INTERNALDATE` it bounds the copy this upload created.
    #[serde(default)]
    pub attempt_at: Option<DateTime<Utc>>,
    /// The user discarded/sent the draft after the APPEND left `Idle`. The
    /// executor finishes the upload (it cannot be cancelled atomically) and
    /// then deletes the copy instead of binding it.
    #[serde(default)]
    pub discard_requested: bool,
    /// The cleanup demanded by `discard_requested` claimed its destructive
    /// step. The claim commits in the same transaction as the ownership check,
    /// exactly like [`AppendState`]: a keep-local that commits before it makes
    /// the claim fail (nothing is sent), while one that commits after it cannot
    /// stop the already-authorized command -- only its outcome is recorded.
    #[serde(default)]
    pub cleanup_claimed: bool,
    /// The Graph cleanup delete was accepted by the provider. It is persisted
    /// *before* the local projection/mapping is settled, so a crash or a failed
    /// local commit retries only that settlement instead of issuing a second
    /// `permanentDelete`.
    #[serde(default)]
    pub cleanup_acked: bool,
    /// The cleanup demanded by `discard_requested` completed. The finishing
    /// transaction must not mark the row done while this is still `false`.
    #[serde(default)]
    pub cleanup_done: bool,
    /// No `APPENDUID` identified the copy this attempt created. A copy bound
    /// later by the Drafts sync is only a *candidate*: the same stable header
    /// can be written by another client, so it must never be deleted by
    /// inference. Kept on the settled operation (and retained by the pruning
    /// pass) so the protection outlives `Done`.
    #[serde(default)]
    pub candidate_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteRemoteDraftOp {
    pub folder_id: String,
    pub uid: i64,
    /// Exact Graph message id to delete, when it is known. Graph has no UIDs;
    /// an id returned by this client's own upload may be used directly instead
    /// of resolving a synthetic UID through the sync mapping.
    #[serde(default)]
    pub remote_message_id: Option<String>,
    /// Draft this delete is the replacement cleanup for. Lets a keep-local
    /// cancel the paired cleanup before it runs; a delete queued for an
    /// unrelated reason leaves it `None`.
    #[serde(default)]
    pub draft_id: Option<String>,
    /// UIDVALIDITY of the Drafts folder when the op was queued; see
    /// [`SetFlagsOp::source_uidvalidity`].
    #[serde(default)]
    pub source_uidvalidity: Option<i64>,
    /// Local row that projects this remote draft. Deleted only after the
    /// server confirms the removal; a failed/skipped delete leaves it visible
    /// in Drafts instead of pretending the copy is gone.
    #[serde(default)]
    pub message_id: Option<String>,
    /// The operation was cancelled while it was in flight, so the request may
    /// already have reached the server and can still succeed. The row is kept
    /// (see `pending_ops::prune_finished`) as the durable record that the
    /// outcome is unknown; the folder is re-synced so the local projection
    /// converges to whichever side won.
    #[serde(default)]
    pub outcome_unknown: bool,
    /// This legacy payload (queued before `draftId` existed) named a remote
    /// copy that could not be attributed to any draft when a draft moved to
    /// another account. Its row is `Cancelled` (so every build, including
    /// older ones, keeps it out of the executor) and this marker plus
    /// [`Self::quarantine_reason`] are the auditable record. The executor
    /// rejects the payload defensively, because the target may be a copy the
    /// user deliberately keeps, while the intent itself may still belong to
    /// another draft. The folder is re-synced so the local projection converges
    /// on its own.
    #[serde(default)]
    pub quarantined: bool,
    /// Why the row was isolated; kept on the payload as the auditable record.
    #[serde(default)]
    pub quarantine_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppendToSentOp {
    pub folder_id: String,
    pub rfc822_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderCreateOp {
    pub parent_id: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderRenameOp {
    pub folder_id: String,
    pub new_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderDeleteOp {
    pub folder_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderSubscribeOp {
    pub folder_id: String,
    pub subscribe: bool,
}

/// Mark every message in a folder as seen (`UID STORE 1:* +FLAGS \Seen`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderMarkReadOp {
    pub folder_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendOp {
    pub outbox_id: String,
}

/// Record a local flag value, its queue row and its durable writer in one
/// transaction — but only while the row still names the copy the caller read.
///
/// The reply/forward and SMTP post-send paths read the source message outside
/// the write transaction. A sync may delete the projection or rebind the same
/// local id (a new IMAP UID generation or a new Graph provider id) before the
/// transaction runs. Writing the queue row anyway would authorize the executor
/// to address a copy that no longer exists; `message_ids` makes that payload
/// subject to the writer gate, so the writer must only be created with proof.
pub(crate) fn apply_source_flags_in_db(
    db: &rusqlite::Connection,
    identity: &repo::messages::MessageIdentity,
    answered: bool,
    forwarded: bool,
    queued: Option<&(String, serde_json::Value)>,
) -> Result<()> {
    if !repo::messages::identity_matches(db, identity)? {
        return Ok(());
    }
    repo::messages::set_flags(
        db,
        std::slice::from_ref(&identity.id),
        None,
        None,
        answered.then_some(true),
        forwarded.then_some(true),
    )?;
    if let Some((account_id, payload)) = queued {
        let op = repo::pending_ops::enqueue(
            db,
            NewPendingOp::new(
                account_id.clone(),
                KIND_SET_FLAGS,
                payload.clone(),
                "Mark original as replied/forwarded",
            ),
        )?;
        repo::messages::record_flag_writers(
            db,
            &identity.id,
            &op.id,
            if answered {
                "setAnswered"
            } else {
                "setForwarded"
            },
            None,
            None,
            answered.then_some(true),
            forwarded.then_some(true),
        )?;
    }
    Ok(())
}

async fn enqueue(
    ctx: &CoreContext,
    account_id: &str,
    kind: &str,
    payload: serde_json::Value,
    description: &str,
) -> Result<()> {
    enqueue_operation(ctx, account_id, kind, payload, description)
        .await
        .map(|_| ())
}

/// Queue one server operation and return the persisted row (the command layer
/// records its id so that "undo last" can cancel a still-pending action).
pub async fn enqueue_operation(
    ctx: &CoreContext,
    account_id: &str,
    kind: &str,
    payload: serde_json::Value,
    description: &str,
) -> Result<nuncio_core::models::PendingOp> {
    let op = NewPendingOp::new(account_id, kind, payload, description);
    let account_id = account_id.to_string();
    let stored = ctx
        .db
        .write(move |db| repo::pending_ops::enqueue(db, op))
        .await?;
    debug!(account = account_id, kind, "queued server operation");
    Ok(stored)
}

/// Claim the destination-identity slot of the projections a move-style
/// operation is about to move optimistically.
///
/// The identity written when the wire command confirms is owned by this
/// operation. Recording the owner in the same transaction as the optimistic
/// move closes the window in which a retry of an earlier batch could install
/// its stale destination on a row that already belongs to a newer trip.
pub(crate) fn claim_move_target_owner(
    db: &rusqlite::Connection,
    account_id: &str,
    kind: &str,
    payload: &serde_json::Value,
    op_id: &str,
    sequence: i64,
) -> Result<()> {
    let (target_folder_id, message_ids) = match kind {
        KIND_MOVE => {
            let payload: MoveOp = serde_json::from_value(payload.clone())
                .map_err(|error| AppError::internal(format!("invalid move payload: {error}")))?;
            (Some(payload.target_folder_id), payload.message_ids)
        }
        KIND_ARCHIVE => {
            let payload: ArchiveOp = serde_json::from_value(payload.clone())
                .map_err(|error| AppError::internal(format!("invalid archive payload: {error}")))?;
            (payload.target_folder_id, payload.message_ids)
        }
        KIND_MARK_JUNK => {
            let payload: MarkJunkOp = serde_json::from_value(payload.clone()).map_err(|error| {
                AppError::internal(format!("invalid markJunk payload: {error}"))
            })?;
            (payload.target_folder_id, payload.message_ids)
        }
        KIND_DELETE => {
            let payload: DeleteOp = serde_json::from_value(payload.clone())
                .map_err(|error| AppError::internal(format!("invalid delete payload: {error}")))?;
            (payload.target_folder_id, payload.message_ids)
        }
        _ => return Ok(()),
    };
    let Some(folder_id) = target_folder_id else {
        return Ok(());
    };
    repo::messages::claim_move_target_owner(
        db,
        account_id,
        &message_ids,
        &folder_id,
        op_id,
        sequence,
    )?;
    Ok(())
}

/// Queue a (batched) flag change.
#[allow(clippy::too_many_arguments)]
pub async fn enqueue_set_flags(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    uids: &[i64],
    seen: Option<bool>,
    flagged: Option<bool>,
    answered: Option<bool>,
    forwarded: Option<bool>,
    source_uidvalidity: Option<i64>,
    description: &str,
) -> Result<()> {
    if uids.is_empty()
        || (seen.is_none() && flagged.is_none() && answered.is_none() && forwarded.is_none())
    {
        return Ok(());
    }
    let payload = SetFlagsOp {
        folder_id: folder_id.to_string(),
        uids: uids.to_vec(),
        seen,
        flagged,
        answered,
        forwarded,
        source_uidvalidity,
        previous: Vec::new(),
        message_ids: Vec::new(),
        outcomes: Vec::new(),
        remote_ids: Vec::new(),
    };
    enqueue(
        ctx,
        account_id,
        KIND_SET_FLAGS,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode setFlags payload: {error}"))
        })?,
        description,
    )
    .await
}

/// Queue a move of several messages to another folder.
#[allow(clippy::too_many_arguments)]
pub async fn enqueue_move(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    target_folder_id: &str,
    uids: &[i64],
    message_ids: &[String],
    source_uidvalidity: Option<i64>,
    description: &str,
) -> Result<()> {
    if uids.is_empty() {
        return Ok(());
    }
    let payload = MoveOp {
        folder_id: folder_id.to_string(),
        target_folder_id: target_folder_id.to_string(),
        uids: uids.to_vec(),
        message_ids: message_ids.to_vec(),
        source_uidvalidity,
        outcomes: Vec::new(),
        remote_ids: Vec::new(),
    };
    enqueue(
        ctx,
        account_id,
        KIND_MOVE,
        serde_json::to_value(payload)
            .map_err(|error| AppError::internal(format!("cannot encode move payload: {error}")))?,
        description,
    )
    .await
}

/// Queue a delete (trash move or permanent expunge).
pub async fn enqueue_delete(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    uids: &[i64],
    permanent: bool,
    source_uidvalidity: Option<i64>,
    description: &str,
) -> Result<()> {
    let payload = DeleteOp {
        folder_id: folder_id.to_string(),
        uids: uids.to_vec(),
        permanent,
        target_folder_id: None,
        soft_hidden: false,
        message_ids: Vec::new(),
        source_uidvalidity,
        pending_delete: false,
        deleted_flag_set: false,
        snapshot: Vec::new(),
        outcomes: Vec::new(),
        remote_ids: Vec::new(),
    };
    enqueue(
        ctx,
        account_id,
        KIND_DELETE,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode delete payload: {error}"))
        })?,
        description,
    )
    .await
}

/// Queue an archive (Gmail label removal or move to the Archive folder).
pub async fn enqueue_archive(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    uids: &[i64],
    source_uidvalidity: Option<i64>,
    description: &str,
) -> Result<()> {
    if uids.is_empty() {
        return Ok(());
    }
    let payload = ArchiveOp {
        folder_id: folder_id.to_string(),
        uids: uids.to_vec(),
        target_folder_id: None,
        soft_hidden: false,
        message_ids: Vec::new(),
        source_uidvalidity,
        outcomes: Vec::new(),
        remote_ids: Vec::new(),
    };
    enqueue(
        ctx,
        account_id,
        KIND_ARCHIVE,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode archive payload: {error}"))
        })?,
        description,
    )
    .await
}

/// Queue moving messages to/from the junk folder.
pub async fn enqueue_mark_junk(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    uids: &[i64],
    value: bool,
    source_uidvalidity: Option<i64>,
    description: &str,
) -> Result<()> {
    let payload = MarkJunkOp {
        folder_id: folder_id.to_string(),
        uids: uids.to_vec(),
        value,
        target_folder_id: None,
        message_ids: Vec::new(),
        source_uidvalidity,
        outcomes: Vec::new(),
        remote_ids: Vec::new(),
    };
    enqueue(
        ctx,
        account_id,
        KIND_MARK_JUNK,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode markJunk payload: {error}"))
        })?,
        description,
    )
    .await
}

/// Queue appending a draft to the server Drafts folder.
pub async fn enqueue_append_draft(
    ctx: &CoreContext,
    account_id: &str,
    draft_id: &str,
    folder_id: &str,
    rfc822_path: &str,
) -> Result<()> {
    let payload = AppendDraftOp {
        draft_id: draft_id.to_string(),
        folder_id: folder_id.to_string(),
        rfc822_path: rfc822_path.to_string(),
        replaces: None,
        replaces_unidentified: false,
        append_state: AppendState::Idle,
        removal_state: DraftRemovalState::NotStarted,
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
    };
    enqueue(
        ctx,
        account_id,
        KIND_APPEND_DRAFT,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode appendDraft payload: {error}"))
        })?,
        "Save draft to server",
    )
    .await
}

/// Queue deletion of a previously uploaded remote draft.
pub async fn enqueue_delete_remote_draft(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    uid: i64,
    source_uidvalidity: Option<i64>,
    message_id: Option<&str>,
) -> Result<()> {
    let payload = DeleteRemoteDraftOp {
        folder_id: folder_id.to_string(),
        uid,
        remote_message_id: None,
        draft_id: None,
        source_uidvalidity,
        message_id: message_id.map(str::to_string),
        outcome_unknown: false,
        quarantined: false,
        quarantine_reason: None,
    };
    enqueue(
        ctx,
        account_id,
        KIND_DELETE_REMOTE_DRAFT,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode deleteRemoteDraft payload: {error}"))
        })?,
        "Remove old remote draft",
    )
    .await
}

/// Queue appending a sent message to the Sent folder.
pub async fn enqueue_append_to_sent(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    rfc822_path: &str,
) -> Result<()> {
    let payload = AppendToSentOp {
        folder_id: folder_id.to_string(),
        rfc822_path: rfc822_path.to_string(),
    };
    enqueue(
        ctx,
        account_id,
        KIND_APPEND_TO_SENT,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode appendToSent payload: {error}"))
        })?,
        "Copy sent message to Sent",
    )
    .await
}

pub async fn enqueue_folder_create(
    ctx: &CoreContext,
    account_id: &str,
    parent_id: Option<&str>,
    name: &str,
) -> Result<()> {
    let payload = FolderCreateOp {
        parent_id: parent_id.map(str::to_string),
        name: name.to_string(),
    };
    enqueue(
        ctx,
        account_id,
        KIND_FOLDER_CREATE,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode folderCreate payload: {error}"))
        })?,
        "Create folder",
    )
    .await
}

pub async fn enqueue_folder_rename(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    new_name: &str,
) -> Result<()> {
    let payload = FolderRenameOp {
        folder_id: folder_id.to_string(),
        new_name: new_name.to_string(),
    };
    enqueue(
        ctx,
        account_id,
        KIND_FOLDER_RENAME,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode folderRename payload: {error}"))
        })?,
        "Rename folder",
    )
    .await
}

pub async fn enqueue_folder_delete(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
) -> Result<()> {
    let payload = FolderDeleteOp {
        folder_id: folder_id.to_string(),
    };
    enqueue(
        ctx,
        account_id,
        KIND_FOLDER_DELETE,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode folderDelete payload: {error}"))
        })?,
        "Delete folder",
    )
    .await
}

pub async fn enqueue_folder_subscribe(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    subscribe: bool,
) -> Result<()> {
    let payload = FolderSubscribeOp {
        folder_id: folder_id.to_string(),
        subscribe,
    };
    enqueue(
        ctx,
        account_id,
        KIND_FOLDER_SUBSCRIBE,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode folderSubscribe payload: {error}"))
        })?,
        if subscribe {
            "Subscribe folder"
        } else {
            "Unsubscribe folder"
        },
    )
    .await
}

pub async fn enqueue_folder_mark_read(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
) -> Result<()> {
    let payload = FolderMarkReadOp {
        folder_id: folder_id.to_string(),
    };
    enqueue(
        ctx,
        account_id,
        KIND_FOLDER_MARK_READ,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode folderMarkRead payload: {error}"))
        })?,
        "Mark folder as read",
    )
    .await
}

pub async fn enqueue_send(ctx: &CoreContext, account_id: &str, outbox_id: &str) -> Result<()> {
    enqueue(
        ctx,
        account_id,
        KIND_SEND,
        json!({ "outboxId": outbox_id }),
        "Send message",
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::models::{AccountState, ProviderKind};

    /// Payloads queued by older builds have no phase evidence. They must not be
    /// mistaken for an untouched operation, because the literal may already be
    /// on the server.
    #[test]
    fn append_payload_without_a_phase_deserializes_as_unknown() {
        let payload: AppendDraftOp = serde_json::from_value(serde_json::json!({
            "draftId": "draft-1",
            "folderId": "drafts",
            "rfc822Path": "/tmp/draft.eml",
        }))
        .expect("old payload");
        assert_eq!(payload.append_state, AppendState::Unknown);
        assert_eq!(payload.removal_state, DraftRemovalState::NotStarted);
        assert!(!payload.discard_requested);
        assert!(!payload.cleanup_done);
    }

    async fn test_ctx() -> CoreContext {
        let dir =
            std::env::temp_dir().join(format!("nuncio-mail-ops-{}", nuncio_core::models::new_id()));
        CoreContext::for_test(nuncio_core::Paths::new(dir))
            .await
            .expect("context")
    }

    async fn account(ctx: &CoreContext) -> nuncio_core::models::Account {
        let now = nuncio_core::models::now();
        let account = nuncio_core::models::Account {
            id: "acc".into(),
            provider: ProviderKind::Imap,
            email: "me@example.com".into(),
            display_name: "Me".into(),
            color: "#000000".into(),
            auth_kind: nuncio_core::models::AuthKind::Password,
            oauth_provider: None,
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
            created_at: now,
        };
        ctx.db
            .write({
                let account = account.clone();
                move |db| repo::accounts::insert(db, &account)
            })
            .await
            .expect("account insert");
        account
    }

    #[tokio::test]
    async fn enqueues_typed_payloads() {
        let ctx = test_ctx().await;
        let account = account(&ctx).await;
        enqueue_set_flags(
            &ctx,
            &account.id,
            "folder",
            &[1, 2],
            Some(true),
            None,
            None,
            None,
            Some(7),
            "read",
        )
        .await
        .expect("queue");
        enqueue_move(
            &ctx,
            &account.id,
            "a",
            "b",
            &[3],
            &["m1".into()],
            Some(7),
            "move",
        )
        .await
        .expect("queue");
        let ops = ctx
            .db
            .read(move |db| repo::pending_ops::list(db, Some("acc")))
            .await
            .expect("list");
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].kind, KIND_SET_FLAGS);
        let payload: SetFlagsOp = repo::pending_ops::payload(&ops[0]).expect("payload");
        assert_eq!(payload.uids, vec![1, 2]);
        assert_eq!(payload.seen, Some(true));
        assert_eq!(payload.source_uidvalidity, Some(7));
        let move_op = ops.iter().find(|op| op.kind == KIND_MOVE).expect("move op");
        let payload: MoveOp = repo::pending_ops::payload(move_op).expect("move payload");
        assert_eq!(payload.source_uidvalidity, Some(7));
    }

    /// Payloads written before the move phases existed must keep working: the
    /// absent fields default to `NotStarted` instead of failing to parse.
    #[test]
    fn move_outcome_without_phase_fields_deserializes_as_not_started() {
        let outcome: OpMessageOutcome = serde_json::from_value(serde_json::json!({
            "messageId": "m1",
            "sourceUid": 4,
            "state": "unknown",
            "targetUid": 9,
        }))
        .expect("legacy outcome");
        assert_eq!(outcome.copy_phase, CopyPhase::NotStarted);
        assert_eq!(outcome.delete_flag_phase, DeleteFlagPhase::NotStarted);
        assert_eq!(outcome.expunge_phase, ExpungePhase::NotStarted);
    }

    #[test]
    fn mail_kind_claim_does_not_include_pim() {
        for kind in [
            KIND_SET_FLAGS,
            KIND_MOVE,
            KIND_DELETE,
            KIND_ARCHIVE,
            KIND_MARK_JUNK,
            KIND_APPEND_DRAFT,
            KIND_DELETE_REMOTE_DRAFT,
            KIND_APPEND_TO_SENT,
            KIND_FOLDER_CREATE,
            KIND_FOLDER_RENAME,
            KIND_FOLDER_DELETE,
            KIND_FOLDER_SUBSCRIBE,
            KIND_FOLDER_MARK_READ,
            KIND_SEND,
        ] {
            assert!(is_mail_kind(kind), "{kind} must be claimed by mail");
        }
        // `pending_op` is shared with contacts, calendar and tasks. The mail
        // worker must leave those rows for their own workers.
        for kind in [
            "pim.task.save",
            "pim.task.delete",
            "pim.task.list.delete",
            "pim.contact.save",
            "pim.contact.group.delete",
            "pim.event.update",
            "pim.calendar.create",
        ] {
            assert!(!is_mail_kind(kind), "{kind} must not be claimed by mail");
        }
    }

    /// Gmail archives by dropping the Inbox label, so All Mail is the real
    /// target; a coincidental extra `\Archive` label must not win. Every other
    /// provider uses `\Archive` and never the virtual `\All`.
    #[test]
    fn archive_target_is_provider_aware() {
        assert_eq!(
            archive_target(true, Some("archive"), Some("all")).as_deref(),
            Some("all")
        );
        assert_eq!(
            archive_target(true, Some("archive"), None),
            None,
            "the label command never produces an Archive folder"
        );
        assert_eq!(archive_target(false, None, Some("all")), None);
        assert_eq!(
            archive_target(false, Some("archive"), Some("all")).as_deref(),
            Some("archive")
        );
    }

    #[test]
    fn empty_flag_updates_are_not_queued() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let ctx = test_ctx().await;
            let account = account(&ctx).await;
            enqueue_set_flags(
                &ctx,
                &account.id,
                "f",
                &[],
                None,
                None,
                None,
                None,
                None,
                "noop",
            )
            .await
            .expect("noop");
            let count = ctx
                .db
                .read(move |db| repo::pending_ops::pending_count(db, Some("acc")))
                .await
                .expect("count");
            assert_eq!(count, 0);
        });
    }
}
