//! Per-message undo state machine tests.
//!
//! Each message is classified independently: pending operations are cancelled,
//! in-flight ones defer the request, and completed ones are compensated with
//! exactly one inverse operation.

#[allow(dead_code)]
mod support;

use std::sync::Arc;
use std::time::Duration;

use nuncio_core::models::{
    new_id, Account, Folder, FolderRole, MessageAction, MessageSummary, NewUndoEntry,
    NewUndoRequest, OAuthProvider, PendingOp, PendingOpState, ProviderKind, ServerConfig,
    StoredMessage, UndoEntryResult, UndoState,
};
use nuncio_core::{repo, CoreContext};
use nuncio_mail::ops::{
    ArchiveOp, DeleteOp, MarkJunkOp, MoveOp, OpMessageOutcome, OpMessageState, SetFlagsOp,
    KIND_SET_FLAGS,
};
use nuncio_mail::MailService;
use support::mock_graph::MockGraph;
use support::mock_imap::MockImap;
use support::{account, context, insert_account, message_raw, RecordingNotifier, StaticCreds};

fn server(port: u16) -> ServerConfig {
    ServerConfig {
        host: "127.0.0.1".into(),
        port,
        security: nuncio_core::models::Security::None,
        username: "test@example.com".into(),
    }
}

async fn connect(account: &Account) -> nuncio_mail::imap::ImapConnection {
    use nuncio_core::traits::MailAuth;
    let config = account.imap.as_ref().expect("imap config");
    let auth = MailAuth::Password {
        username: "test@example.com".into(),
        password: "secret".into(),
    };
    nuncio_mail::imap::connect(config, &auth)
        .await
        .expect("connect mock IMAP")
}

async fn sync_once(ctx: &CoreContext, account: &Account) {
    let mut connection = connect(account).await;
    nuncio_mail::sync::sync_account(ctx, &mut connection, account)
        .await
        .expect("initial sync");
}

async fn folder_id(ctx: &CoreContext, account_id: &str, name: &str) -> String {
    let account_id = account_id.to_string();
    let folders = ctx
        .db
        .read(move |db| repo::folders::list(db, &account_id))
        .await
        .expect("folders");
    folders
        .into_iter()
        .find(|folder| folder.remote_path.eq_ignore_ascii_case(name))
        .unwrap_or_else(|| panic!("folder {name} missing"))
        .id
}

async fn insert_stored(ctx: &CoreContext, account_id: &str, id: &str, folder_id: &str) -> i64 {
    let mut summary = MessageSummary::new(
        id,
        account_id,
        folder_id,
        format!("thread-{id}"),
        chrono::Utc::now(),
    );
    summary.subject = id.to_string();
    let mut stored = StoredMessage::new(summary);
    let uid = id
        .bytes()
        .fold(0i64, |acc, byte| {
            acc.wrapping_mul(31).wrapping_add(byte as i64)
        })
        .rem_euclid(1_000_000)
        + 1;
    stored.uid = Some(uid);
    stored.uidvalidity = Some(1);
    // A Message-ID lets undo locate the row if its target identity has to be
    // re-resolved from the folder.
    stored.message_id_header = Some(format!("<{id}@example.com>"));
    stored.summary.seen = false;
    stored.summary.flagged = false;
    ctx.db
        .write(move |db| repo::messages::upsert(db, &stored))
        .await
        .expect("insert stored message");
    uid
}

/// Id of the only visible message in `folder_id` after a sync.
async fn synced_message_id(ctx: &CoreContext, folder_id: &str) -> String {
    let folder_id = folder_id.to_string();
    ctx.db
        .read(move |db| {
            Ok(db.query_row(
                "SELECT id FROM message
                  WHERE folder_id = ?1 AND is_deleted_local = 0
                  ORDER BY date ASC LIMIT 1",
                rusqlite::params![folder_id],
                |row| row.get::<_, String>(0),
            )?)
        })
        .await
        .expect("synced message")
}

async fn stored(ctx: &CoreContext, id: &str) -> StoredMessage {
    let id = id.to_string();
    ctx.db
        .read(move |db| repo::messages::get_required(db, &id))
        .await
        .expect("stored message")
}

async fn ops(ctx: &CoreContext) -> Vec<PendingOp> {
    ctx.db
        .read(|db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops")
}

async fn ops_for(ctx: &CoreContext, account_id: &str) -> Vec<PendingOp> {
    let account_id = account_id.to_string();
    ctx.db
        .read(move |db| repo::pending_ops::list(db, Some(&account_id)))
        .await
        .expect("ops")
}

/// Force a queued operation into the state an executor pass would leave behind
/// when it skipped one message for a newer owner, without settling the row.
async fn settle_op_with_outcome(
    ctx: &CoreContext,
    op_id: &str,
    state: PendingOpState,
    outcome: OpMessageState,
) {
    settle_op_for_message(ctx, op_id, "gmsg1", state, outcome).await
}

async fn settle_op_for_message(
    ctx: &CoreContext,
    op_id: &str,
    message_id: &str,
    state: PendingOpState,
    outcome: OpMessageState,
) {
    let op_id = op_id.to_string();
    let message_id = message_id.to_string();
    ctx.db
        .write(move |db| {
            let op = repo::pending_ops::get(db, &op_id)?.expect("op row");
            let mut payload = repo::pending_ops::payload::<SetFlagsOp>(&op)?;
            payload.outcomes = vec![OpMessageOutcome {
                message_id,
                source_uid: None,
                state: outcome,
                target_folder_id: None,
                target_uid: None,
                target_uidvalidity: None,
                target_remote_id: None,
                attempt_remote_id: None,
                copy_phase: Default::default(),
                delete_flag_phase: Default::default(),
                expunge_phase: Default::default(),
            }];
            let json = serde_json::to_value(payload)
                .map_err(|error| nuncio_core::AppError::internal(error.to_string()))?
                .to_string();
            db.execute(
                "UPDATE pending_op SET state = ?2, payload_json = ?3 WHERE id = ?1",
                rusqlite::params![op_id, state.as_str(), json],
            )?;
            Ok(())
        })
        .await
        .expect("settle op");
}

async fn entry_for(ctx: &CoreContext, request_id: &str) -> nuncio_core::models::UndoRequestEntry {
    let request_id = request_id.to_string();
    ctx.db
        .read(move |db| repo::undo::list_entries(db, &request_id))
        .await
        .expect("entries")
        .into_iter()
        .next()
        .expect("entry")
}

/// A Graph account has no UID generation; undo must requeue the restored flag
/// with the confirmed provider id instead of silently dropping the command.
async fn setup_graph() -> (CoreContext, MailService) {
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("gacc", "graph@example.com", None, None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    let folder = Folder::new("gfld", "gacc", "Inbox", FolderRole::Inbox);
    ctx.db
        .write(move |db| repo::folders::insert(db, &folder))
        .await
        .expect("insert graph folder");
    ctx.db
        .write(|db| {
            let mut summary = MessageSummary::new(
                "gmsg1",
                "gacc",
                "gfld",
                "thread-g",
                nuncio_core::models::now(),
            );
            summary.subject = "graph".into();
            let mut stored = StoredMessage::new(summary);
            // Graph rows carry a synthetic UID and no UIDVALIDITY; the
            // provider id is the only identity.
            stored.uid = Some(777);
            stored.uidvalidity = None;
            stored.gm_msgid = Some("AAMk-graph-1".into());
            repo::messages::upsert(db, &stored)
        })
        .await
        .expect("insert graph message");
    let service = MailService::new(ctx.clone());
    (ctx, service)
}

/// `setup_graph` plus a loopback Graph server, for tests that run the real
/// worker and assert on the requests it makes.
async fn setup_graph_with_mock() -> (CoreContext, MailService, MockGraph) {
    let mock = MockGraph::start(0).await;
    let (ctx, service) = setup_graph().await;
    (ctx, service, mock)
}

async fn request_state(ctx: &CoreContext, id: &str) -> UndoState {
    let id = id.to_string();
    ctx.db
        .read(move |db| repo::undo::get_request(db, &id))
        .await
        .expect("request")
        .expect("request exists")
        .state
}

async fn entry_results(ctx: &CoreContext, id: &str) -> Vec<UndoEntryResult> {
    let id = id.to_string();
    ctx.db
        .read(move |db| repo::undo::list_entries(db, &id))
        .await
        .expect("entries")
        .into_iter()
        .map(|entry| entry.result)
        .collect()
}

async fn setup() -> (CoreContext, MailService, Arc<MockImap>, Account) {
    let mock = Arc::new(
        MockImap::start_with_folders(
            &["CONDSTORE", "MOVE", "UIDPLUS"],
            &[
                ("INBOX", &["\\Inbox"]),
                ("Archive", &["\\Archive"]),
                ("Trash", &["\\Trash"]),
                ("Junk", &["\\Junk"]),
            ],
        )
        .await,
    );
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        Some(server(mock.addr.port())),
        None,
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;
    let service = MailService::new(ctx.clone());
    (ctx, service, mock, account)
}

/// 1. Every operation is still `pending`: cancel, restore locally, `Done`.
#[tokio::test]
async fn undo_cancels_pending_operations_and_restores_locally() {
    let (ctx, service, _mock, _account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    insert_stored(&ctx, "acc", "m1", &inbox).await;

    let undo_id = service
        .message_act(
            &["m1".to_string()],
            &MessageAction::SetFlagged { value: true },
        )
        .await
        .expect("flag");
    assert!(stored(&ctx, "m1").await.summary.flagged);

    assert!(service.message_undo(&undo_id).await.expect("undo"));
    assert!(!stored(&ctx, "m1").await.summary.flagged);
    assert!(ops(&ctx)
        .await
        .iter()
        .all(|op| op.state == PendingOpState::Cancelled));
    assert_eq!(request_state(&ctx, &undo_id).await, UndoState::Done);
    assert_eq!(
        entry_results(&ctx, &undo_id).await,
        vec![UndoEntryResult::Done]
    );
}

/// Undoing a never-sent flag action while an earlier one is still queued must
/// hand the field back to that earlier action. Leaving the cancelled operation
/// as the writer made the executor skip the valid one, so the server kept the
/// undone value forever.
#[tokio::test]
async fn undo_of_an_unsent_flag_returns_ownership_to_the_previous_action() {
    let (ctx, service, _mock, _account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    insert_stored(&ctx, "acc", "m1", &inbox).await;

    service
        .message_act(&["m1".to_string()], &MessageAction::SetSeen { value: true })
        .await
        .expect("first mark read");
    let second = service
        .message_act(
            &["m1".to_string()],
            &MessageAction::SetSeen { value: false },
        )
        .await
        .expect("then mark unread");
    let first_op_id = ctx
        .db
        .read(|db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops")
        .into_iter()
        .find(|op| {
            op.kind == nuncio_mail::ops::KIND_SET_FLAGS && op.state == PendingOpState::Pending
        })
        .expect("first flag op")
        .id;
    let writer_seq_before_undo = ctx
        .db
        .read(|db| repo::messages::flag_writer(db, "m1", "seen"))
        .await
        .expect("writer")
        .expect("writer row")
        .seq;

    assert!(service.message_undo(&second).await.expect("undo unread"));
    let writer = ctx
        .db
        .read(|db| repo::messages::flag_writer(db, "m1", "seen"))
        .await
        .expect("writer")
        .expect("writer row");
    assert_eq!(
        writer.owner_id, first_op_id,
        "the earlier still-pending action must own the field again"
    );
    assert_eq!(writer.value, Some(true));
    assert!(
        stored(&ctx, "m1").await.summary.seen,
        "the local rollback restores the earlier action's value"
    );
    assert!(
        writer.seq > writer_seq_before_undo,
        "a writer hand-off must bump the revision auto mark-as-read observes"
    );
}

/// When the earlier action was *skipped* by the executor (it lost the field to
/// the newer action), it is `done` without ever sending. Undoing the newer,
/// still-unsent action must not hand the field back to that dead row: it has
/// to queue a fresh command, or the server keeps the undone value forever.
#[tokio::test]
async fn undo_of_an_unsent_flag_requeues_a_previous_skipped_action() {
    let (ctx, service, _mock, account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    insert_stored(&ctx, "acc", "m1", &inbox).await;

    // A = mark read, B = mark unread. B records its writer before A runs, so
    // the executor skips A.
    service
        .message_act(&["m1".to_string()], &MessageAction::SetSeen { value: true })
        .await
        .expect("first mark read");
    let second = service
        .message_act(
            &["m1".to_string()],
            &MessageAction::SetSeen { value: false },
        )
        .await
        .expect("then mark unread");

    let queued = ops(&ctx).await;
    let first_id = queued
        .iter()
        .find(|op| {
            repo::pending_ops::payload::<SetFlagsOp>(op)
                .map(|payload| payload.seen == Some(true))
                .unwrap_or(false)
        })
        .map(|op| op.id.clone())
        .expect("first flag op");
    let second_id = queued
        .iter()
        .find(|op| op.id != first_id)
        .map(|op| op.id.clone())
        .expect("second flag op");

    // Hold B so this pass executes only A. A settles as `done` after being
    // skipped (the field belongs to B).
    let future = nuncio_core::repo::dt_string(chrono::Utc::now() + chrono::Duration::days(1));
    ctx.db
        .write(move |db| {
            db.execute(
                "UPDATE pending_op SET next_retry_at = ?2 WHERE id = ?1",
                rusqlite::params![second_id, future],
            )?;
            Ok(())
        })
        .await
        .expect("hold the newer action");

    let mut connection = connect(&account).await;
    nuncio_mail::ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("execute the skipped action");

    let settled = ops(&ctx)
        .await
        .into_iter()
        .find(|op| op.id == first_id)
        .expect("first op");
    assert_eq!(settled.state, PendingOpState::Done);
    let payload = repo::pending_ops::payload::<SetFlagsOp>(&settled).expect("payload");
    assert!(
        payload.outcomes.iter().any(|outcome| {
            outcome.message_id == "m1" && outcome.state == OpMessageState::NotExecuted
        }),
        "the skip must be recorded as NotExecuted, not as a successful send"
    );

    assert!(service.message_undo(&second).await.expect("undo unread"));

    // The restored intent is a *new* pending command, not the dead `done` row.
    let after = ops(&ctx).await;
    let replacement = after
        .iter()
        .find(|op| {
            op.id != first_id
                && op.state == PendingOpState::Pending
                && repo::pending_ops::payload::<SetFlagsOp>(op)
                    .map(|payload| payload.seen == Some(true))
                    .unwrap_or(false)
        })
        .expect("re-queued restore");
    let writer = ctx
        .db
        .read(|db| repo::messages::flag_writer(db, "m1", "seen"))
        .await
        .expect("writer")
        .expect("writer row");
    assert_eq!(writer.owner_id, replacement.id);
    assert_eq!(writer.value, Some(true));
    assert!(
        stored(&ctx, "m1").await.summary.seen,
        "the local rollback restores the earlier action's value"
    );
}

/// A later flag action can be *explicitly refused* after the previous action
/// was skipped. Undoing the refused action must not settle after the local
/// restore alone: the refused row is still the field's durable writer, so the
/// restored value has to be handed back or re-queued, or the server keeps the
/// value the user undid.
#[tokio::test]
async fn undo_of_a_rejected_flag_requeues_the_skipped_action() {
    let (ctx, service) = setup_graph().await;

    // A = mark read, B = mark unread. B owns the writer, so A is skipped.
    service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: true },
        )
        .await
        .expect("first mark read");
    let second = service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: false },
        )
        .await
        .expect("then mark unread");

    let queued = ops_for(&ctx, "gacc").await;
    let first_id = queued
        .iter()
        .find(|op| {
            repo::pending_ops::payload::<SetFlagsOp>(op)
                .map(|payload| payload.seen == Some(true))
                .unwrap_or(false)
        })
        .map(|op| op.id.clone())
        .expect("first flag op");
    let second_id = queued
        .iter()
        .find(|op| op.id != first_id)
        .map(|op| op.id.clone())
        .expect("second flag op");

    // A settled after losing the field to B; B's PATCH was refused outright.
    settle_op_for_message(
        &ctx,
        &first_id,
        "gmsg1",
        PendingOpState::Done,
        OpMessageState::NotExecuted,
    )
    .await;
    settle_op_for_message(
        &ctx,
        &second_id,
        "gmsg1",
        PendingOpState::Failed,
        OpMessageState::NotExecuted,
    )
    .await;

    assert!(service.message_undo(&second).await.expect("undo unread"));

    let entry = entry_for(&ctx, &second).await;
    assert_ne!(
        entry.result,
        UndoEntryResult::Done,
        "a refused flag write must not settle after a local-only restore"
    );
    let replacement = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| entry.inverse_op_id.as_deref() == Some(op.id.as_str()))
        .expect("the restored value must be queued as the inverse");
    let payload = repo::pending_ops::payload::<SetFlagsOp>(&replacement).expect("payload");
    assert_eq!(payload.seen, Some(true));
    assert_eq!(
        payload.remote_ids,
        vec!["AAMk-graph-1".to_string()],
        "the restore must address the copy the skipped action selected"
    );
    assert_eq!(request_state(&ctx, &second).await, UndoState::Compensating);
    let writer = ctx
        .db
        .read(|db| repo::messages::flag_writer(db, "gmsg1", "seen"))
        .await
        .expect("writer")
        .expect("writer row");
    assert_eq!(
        writer.owner_id, replacement.id,
        "the replacement must own the field so the executor sends it"
    );
    assert_eq!(writer.value, Some(true));
    assert!(
        stored(&ctx, "gmsg1").await.summary.seen,
        "the local rollback restores the earlier action's value"
    );
}

/// A STORE whose tagged completion was lost may already have reached the
/// server. The IMAP executor must persist a per-message `Unknown` before the
/// command; a later retry that has to skip the message must not downgrade it to
/// `NotExecuted`, and undo must keep waiting instead of treating the write as
/// never sent.
#[tokio::test]
async fn imap_lost_store_receipt_keeps_unknown_and_undo_defers() {
    let (ctx, service, mock, account) = setup().await;
    mock.state.add_message(
        "INBOX",
        &message_raw(
            "alice@example.com",
            "test@example.com",
            "Flag me",
            "body",
            "flag@example.com",
        ),
    );
    sync_once(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let message_id = synced_message_id(&ctx, &inbox).await;
    let uid = stored(&ctx, &message_id).await.uid.expect("uid");

    let undo_id = service
        .message_act(
            std::slice::from_ref(&message_id),
            &MessageAction::SetSeen { value: true },
        )
        .await
        .expect("mark read");
    let original_id = ops(&ctx)
        .await
        .into_iter()
        .find(|op| op.kind == KIND_SET_FLAGS)
        .expect("flag op")
        .id;

    // The server applies the STORE but the client never sees the completion.
    mock.state.set_store_drop_completion(true);
    let mut connection = connect(&account).await;
    nuncio_mail::ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first pass");
    mock.state.set_store_drop_completion(false);

    let flags = mock.state.message_flags("INBOX", uid as u32);
    assert!(
        flags.iter().any(|flag| flag.eq_ignore_ascii_case("\\Seen")),
        "the server must have applied the STORE: {flags:?}"
    );
    let original = ops(&ctx)
        .await
        .into_iter()
        .find(|op| op.id == original_id)
        .expect("flag op");
    assert_eq!(
        original.state,
        PendingOpState::Pending,
        "a lost receipt must stay retryable"
    );
    let payload = repo::pending_ops::payload::<SetFlagsOp>(&original).expect("payload");
    assert!(
        payload.outcomes.iter().any(|outcome| {
            outcome.message_id == message_id && outcome.state == OpMessageState::Unknown
        }),
        "the take-off marker must be persisted before the STORE: {:?}",
        payload.outcomes
    );

    // A newer action takes the field over. The retry must skip this message.
    ctx.db
        .write({
            let message_id = message_id.clone();
            move |db| {
                repo::messages::record_flag_writer(
                    db,
                    &message_id,
                    "seen",
                    "later-op",
                    Some(false),
                    "setSeen",
                )
            }
        })
        .await
        .expect("later writer");
    ctx.db
        .write({
            let original_id = original_id.clone();
            move |db| {
                db.execute(
                    "UPDATE pending_op SET next_retry_at = NULL WHERE id = ?1",
                    rusqlite::params![original_id],
                )?;
                Ok(())
            }
        })
        .await
        .expect("release the retry");

    let mut connection = connect(&account).await;
    nuncio_mail::ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("retry pass");

    let settled = ops(&ctx)
        .await
        .into_iter()
        .find(|op| op.id == original_id)
        .expect("flag op");
    assert_eq!(settled.state, PendingOpState::Done);
    let payload = repo::pending_ops::payload::<SetFlagsOp>(&settled).expect("payload");
    assert!(
        payload.outcomes.iter().any(|outcome| {
            outcome.message_id == message_id && outcome.state == OpMessageState::Unknown
        }),
        "a skip must not overwrite the earlier Unknown: {:?}",
        payload.outcomes
    );
    let stores = mock
        .state
        .commands()
        .iter()
        .filter(|command| command.starts_with("UID STORE"))
        .count();
    assert_eq!(
        stores,
        1,
        "the skipped retry must not send another STORE: {:?}",
        mock.state.commands()
    );

    // Undo cannot prove the STORE never ran, so it must keep waiting rather
    // than restoring locally and reporting Done.
    assert!(service.message_undo(&undo_id).await.expect("undo deferred"));
    assert_ne!(
        entry_results(&ctx, &undo_id).await,
        vec![UndoEntryResult::Done]
    );
    assert_eq!(request_state(&ctx, &undo_id).await, UndoState::Deferred);
    // Repeated passes settle conservatively as Failed, never as Done.
    let _ = service.message_undo(&undo_id).await;
    let _ = service.message_undo(&undo_id).await;
    assert_eq!(request_state(&ctx, &undo_id).await, UndoState::Failed);
}

/// Graph has no UID generation. If an earlier action already decided to skip
/// (its per-message `NotExecuted` is recorded while the row is still in flight),
/// undoing the newer action must queue a fresh command addressed by the
/// confirmed provider id — and must keep tracking it as the undo's inverse.
#[tokio::test]
async fn graph_undo_requeues_a_skipped_action_by_provider_id() {
    let (ctx, service) = setup_graph().await;

    service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: true },
        )
        .await
        .expect("first mark read");
    let second = service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: false },
        )
        .await
        .expect("then mark unread");

    let queued = ops_for(&ctx, "gacc").await;
    let first_id = queued
        .iter()
        .find(|op| {
            repo::pending_ops::payload::<SetFlagsOp>(op)
                .map(|payload| payload.seen == Some(true))
                .unwrap_or(false)
        })
        .map(|op| op.id.clone())
        .expect("first flag op");
    // The executor recorded the skip, but has not settled the operation yet.
    settle_op_with_outcome(
        &ctx,
        &first_id,
        PendingOpState::InProgress,
        OpMessageState::NotExecuted,
    )
    .await;

    assert!(service.message_undo(&second).await.expect("undo unread"));

    let replacement = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| {
            op.id != first_id
                && op.state == PendingOpState::Pending
                && repo::pending_ops::payload::<SetFlagsOp>(op)
                    .map(|payload| payload.seen == Some(true))
                    .unwrap_or(false)
        })
        .expect("re-queued restore");
    let payload = repo::pending_ops::payload::<SetFlagsOp>(&replacement).expect("payload");
    assert_eq!(
        payload.remote_ids,
        vec!["AAMk-graph-1".to_string()],
        "Graph must address the confirmed provider id"
    );
    assert_eq!(payload.uids, vec![777]);
    assert_eq!(
        payload.source_uidvalidity, None,
        "Graph has no IMAP UIDVALIDITY"
    );

    let entry = entry_for(&ctx, &second).await;
    assert_eq!(
        entry.inverse_op_id.as_deref(),
        Some(replacement.id.as_str()),
        "the replacement command must be part of the undo's settlement"
    );
    assert_ne!(
        entry.result,
        UndoEntryResult::Done,
        "undo must not report done before the replacement settles"
    );
    assert_eq!(request_state(&ctx, &second).await, UndoState::Compensating);

    let writer = ctx
        .db
        .read(|db| repo::messages::flag_writer(db, "gmsg1", "seen"))
        .await
        .expect("writer")
        .expect("writer row");
    assert_eq!(writer.owner_id, replacement.id);
}

/// The replacement command must address the copy the earlier action selected,
/// even if a sync rebound the local row to another provider copy before the
/// undo ran. Re-reading the row's current `gm_msgid` would PATCH the wrong
/// message.
#[tokio::test]
async fn graph_requeue_uses_the_original_provider_id_after_a_rebind() {
    let (ctx, service) = setup_graph().await;

    service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: true },
        )
        .await
        .expect("first mark read");
    let second = service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: false },
        )
        .await
        .expect("then mark unread");

    let first_id = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| {
            repo::pending_ops::payload::<SetFlagsOp>(op)
                .map(|payload| payload.seen == Some(true))
                .unwrap_or(false)
        })
        .map(|op| op.id.clone())
        .expect("first flag op");
    // The earlier action was skipped for this message; its own payload pinned
    // the copy it was queued for.
    settle_op_with_outcome(
        &ctx,
        &first_id,
        PendingOpState::InProgress,
        OpMessageState::NotExecuted,
    )
    .await;
    // A sync rebinds the same local row to a different server copy.
    ctx.db
        .write(|db| {
            db.execute(
                "UPDATE message SET gm_msgid = 'AAMk-graph-2' WHERE id = 'gmsg1'",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("rebind row");

    assert!(service.message_undo(&second).await.expect("undo unread"));

    let entry = entry_for(&ctx, &second).await;
    let replacement = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| entry.inverse_op_id.as_deref() == Some(op.id.as_str()))
        .expect("tracked replacement");
    let payload = repo::pending_ops::payload::<SetFlagsOp>(&replacement).expect("payload");
    assert_eq!(
        payload.remote_ids,
        vec!["AAMk-graph-1".to_string()],
        "the restore must address the copy the earlier action selected, not the rebound row"
    );
    assert_eq!(payload.folder_id, "gfld");
}

/// A move between the two flag actions changes the copy behind the same local
/// row. Even when both folders report the same UIDVALIDITY, the restore must
/// not send the earlier action's UID to the folder the row now lives in.
#[tokio::test]
async fn undo_after_a_move_does_not_requeue_on_the_new_folder() {
    let (ctx, service, _mock, _account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    insert_stored(&ctx, "acc", "m1", &inbox).await;
    // The collision a UID-only check would miss: both generations are equal.
    {
        let inbox = inbox.clone();
        let archive = archive.clone();
        ctx.db
            .write(move |db| {
                repo::folders::set_sync_flags(db, &inbox, Some(1), None, None)?;
                repo::folders::set_sync_flags(db, &archive, Some(1), None, None)?;
                Ok(())
            })
            .await
            .expect("folder generations");
    }

    service
        .message_act(&["m1".to_string()], &MessageAction::SetSeen { value: true })
        .await
        .expect("first mark read");
    let first_id = ops(&ctx)
        .await
        .into_iter()
        .find(|op| op.kind == nuncio_mail::ops::KIND_SET_FLAGS)
        .map(|op| op.id)
        .expect("first flag op");

    // The message moves before the second action.
    {
        let archive = archive.clone();
        ctx.db
            .write(move |db| repo::messages::move_to_folder(db, &["m1".to_string()], &archive))
            .await
            .expect("move");
    }
    let second = service
        .message_act(
            &["m1".to_string()],
            &MessageAction::SetSeen { value: false },
        )
        .await
        .expect("then mark unread");
    let second_id = ops(&ctx)
        .await
        .into_iter()
        .find(|op| op.id != first_id)
        .map(|op| op.id)
        .expect("second flag op");
    // The earlier action was skipped for this message.
    settle_op_for_message(
        &ctx,
        &first_id,
        "m1",
        PendingOpState::InProgress,
        OpMessageState::NotExecuted,
    )
    .await;

    assert!(
        service.message_undo(&second).await.is_err(),
        "the deterministic refusal must be reported to the caller"
    );
    let entry = entry_for(&ctx, &second).await;
    assert_eq!(
        entry.inverse_op_id, None,
        "a restore must not mix the earlier folder's UID with the new folder"
    );
    assert_eq!(
        entry.result,
        UndoEntryResult::Failed,
        "a deterministic refusal must become visible, not report Done"
    );
    assert_eq!(request_state(&ctx, &second).await, UndoState::Failed);
    assert!(
        !ops(&ctx).await.iter().any(|op| {
            op.id != first_id
                && op.state == PendingOpState::Pending
                && repo::pending_ops::payload::<SetFlagsOp>(op)
                    .map(|payload| payload.seen == Some(true))
                    .unwrap_or(false)
        }),
        "no replacement may be queued for a different server copy"
    );
    let writer = ctx
        .db
        .read(|db| repo::messages::flag_writer(db, "m1", "seen"))
        .await
        .expect("writer")
        .expect("blocking writer row");
    assert_eq!(
        writer.owner_id, second_id,
        "the cancelled action must stay the blocking owner so the earlier \
         action cannot write the old folder's UID"
    );
}

/// A Graph row rebound to a different provider message *between* the two
/// actions makes them target different copies. The restore must not pair the
/// newer snapshot's value with the earlier action's provider id.
#[tokio::test]
async fn graph_undo_does_not_requeue_a_copy_rebound_between_the_actions() {
    let (ctx, service, mock) = setup_graph_with_mock().await;

    service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: true },
        )
        .await
        .expect("first mark read");
    let first_id = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| {
            repo::pending_ops::payload::<SetFlagsOp>(op)
                .map(|payload| payload.seen == Some(true))
                .unwrap_or(false)
        })
        .map(|op| op.id)
        .expect("first flag op");

    // The second action is prepared after the rebind, so its snapshot names the
    // new copy while the earlier payload pins the old one.
    ctx.db
        .write(|db| {
            db.execute(
                "UPDATE message SET gm_msgid = 'AAMk-graph-2' WHERE id = 'gmsg1'",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("rebind row");
    let second = service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: false },
        )
        .await
        .expect("then mark unread");
    let second_id = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| op.id != first_id)
        .map(|op| op.id)
        .expect("second flag op");
    // A batch can record `NotExecuted` for this message and still be retried:
    // another message in the same batch failed, so the row goes back to
    // `Pending` while carrying the per-message skip.
    settle_op_with_outcome(
        &ctx,
        &first_id,
        PendingOpState::Pending,
        OpMessageState::NotExecuted,
    )
    .await;

    assert!(
        service.message_undo(&second).await.is_err(),
        "the deterministic refusal must be reported to the caller"
    );
    let entry = entry_for(&ctx, &second).await;
    assert_eq!(
        entry.inverse_op_id, None,
        "the value and the provider id must never come from different copies"
    );
    assert_eq!(
        entry.result,
        UndoEntryResult::Failed,
        "a deterministic refusal must become visible, not report Done"
    );
    assert_eq!(request_state(&ctx, &second).await, UndoState::Failed);
    assert!(
        !ops_for(&ctx, "gacc").await.iter().any(|op| {
            op.id != first_id
                && op.state == PendingOpState::Pending
                && repo::pending_ops::payload::<SetFlagsOp>(op)
                    .map(|payload| payload.seen == Some(true))
                    .unwrap_or(false)
        }),
        "no replacement may be queued when the two actions name different copies"
    );
    // Run the real Graph worker first: the earlier action is still queued and
    // now retries. This is the regression. A deleted writer row makes the
    // executor treat the payload as legacy and PATCH the stale copy; the
    // cancelled operation must stay on the row to block it.
    // (The writer assertion below runs afterwards so a regression fails on the
    // actual wire request, not just on the row's contents.)
    let account = ctx
        .db
        .read(|db| repo::accounts::get(db, "gacc"))
        .await
        .expect("account")
        .expect("graph account");
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome = nuncio_mail::ops::executor::run_due_graph_with(&ctx, &account, &client)
        .await
        .expect("graph pass");
    assert_eq!(
        outcome.done, 1,
        "the skipped earlier action must settle instead of retrying forever"
    );
    let requests = mock.requests();
    assert!(
        requests
            .iter()
            .all(|request| !request.starts_with("/me/messages/")),
        "the retried action must not PATCH any copy: {requests:?}"
    );
    let settled = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| op.id == first_id)
        .expect("earlier op");
    assert_eq!(settled.state, PendingOpState::Done);
    let payload = repo::pending_ops::payload::<SetFlagsOp>(&settled).expect("payload");
    assert!(
        payload.outcomes.iter().any(|outcome| {
            outcome.message_id == "gmsg1" && outcome.state == OpMessageState::NotExecuted
        }),
        "the retry must record the skip, not a send"
    );
    let writer = ctx
        .db
        .read(|db| repo::messages::flag_writer(db, "gmsg1", "seen"))
        .await
        .expect("writer")
        .expect("blocking writer row");
    assert_eq!(
        writer.owner_id, second_id,
        "the cancelled operation must stay the field's writer"
    );
}

/// The projection can disappear between the undo and the retry: a Graph sync
/// that no longer sees the row calls `delete_local(permanent)`, which drops the
/// message *and* its writer rows. The still-queued earlier action then has no
/// durable evidence that it owns the field; it must not fall back to the
/// legacy "no writer = send" permission and PATCH the provider id it pinned.
#[tokio::test]
async fn graph_retry_after_projection_delete_sends_nothing() {
    let (ctx, service, mock) = setup_graph_with_mock().await;

    service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: true },
        )
        .await
        .expect("first mark read");
    let first_id = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| {
            repo::pending_ops::payload::<SetFlagsOp>(op)
                .map(|payload| payload.seen == Some(true))
                .unwrap_or(false)
        })
        .map(|op| op.id)
        .expect("first flag op");

    // The second action names the rebound copy; the earlier payload pins the
    // original one.
    ctx.db
        .write(|db| {
            db.execute(
                "UPDATE message SET gm_msgid = 'AAMk-graph-2' WHERE id = 'gmsg1'",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("rebind row");
    let second = service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: false },
        )
        .await
        .expect("then mark unread");
    settle_op_with_outcome(
        &ctx,
        &first_id,
        PendingOpState::Pending,
        OpMessageState::NotExecuted,
    )
    .await;
    assert!(
        service.message_undo(&second).await.is_err(),
        "the deterministic refusal must be visible to the caller"
    );

    // A later sync no longer sees the projection and deletes it permanently.
    // That is exactly what `sync/messages.rs` does for removed rows, and it
    // also removes the writer row the undo left as the blocker.
    ctx.db
        .write(|db| repo::messages::delete_local(db, &["gmsg1".to_string()], true))
        .await
        .expect("delete projection");
    assert!(
        ctx.db
            .read(|db| repo::messages::flag_writer(db, "gmsg1", "seen"))
            .await
            .expect("writer")
            .is_none(),
        "the projection delete also drops the writer row"
    );

    let account = ctx
        .db
        .read(|db| repo::accounts::get(db, "gacc"))
        .await
        .expect("account")
        .expect("graph account");
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome = nuncio_mail::ops::executor::run_due_graph_with(&ctx, &account, &client)
        .await
        .expect("graph pass");
    assert_eq!(
        outcome.done, 1,
        "the stale action must settle instead of retrying forever"
    );
    let requests = mock.requests();
    assert!(
        requests
            .iter()
            .all(|request| request != "/me/messages/AAMk-graph-1"),
        "the retry must not PATCH the retired provider id: {requests:?}"
    );
    assert!(
        requests
            .iter()
            .all(|request| !request.starts_with("/me/messages/")),
        "no message write may be sent: {requests:?}"
    );
    let settled = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| op.id == first_id)
        .expect("earlier op");
    assert_eq!(settled.state, PendingOpState::Done);
    let payload = repo::pending_ops::payload::<SetFlagsOp>(&settled).expect("payload");
    assert!(
        payload.outcomes.iter().any(|outcome| {
            outcome.message_id == "gmsg1" && outcome.state == OpMessageState::NotExecuted
        }),
        "the retry must record NotExecuted, not a send"
    );
}

/// The stricter rule only applies when the writer evidence is missing. An
/// operation that still owns the field must keep sending its PATCH.
#[tokio::test]
async fn graph_worker_patches_when_the_current_writer_owns_the_field() {
    let (ctx, service, mock) = setup_graph_with_mock().await;

    service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: true },
        )
        .await
        .expect("mark read");
    let account = ctx
        .db
        .read(|db| repo::accounts::get(db, "gacc"))
        .await
        .expect("account")
        .expect("graph account");
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome = nuncio_mail::ops::executor::run_due_graph_with(&ctx, &account, &client)
        .await
        .expect("graph pass");
    assert_eq!(outcome.done, 1);
    assert!(
        mock.requests()
            .iter()
            .any(|request| request == "/me/messages/AAMk-graph-1"),
        "the owned action must PATCH its pinned copy: {:?}",
        mock.requests()
    );
}

/// A Graph batch checks every message's writer before the first PATCH. While
/// that request is on the wire, a projection for a later message can be
/// deleted (dropping its writer) or replaced. The claim must therefore be
/// re-checked immediately before each message is addressed; the skipped
/// message must be recorded as `NotExecuted` instead of being sent with the
/// evidence captured at batch start.
#[tokio::test]
async fn graph_batch_rechecks_every_message_before_sending() {
    let mock = MockGraph::start_with_patch_gate(0).await;
    let (ctx, service) = setup_graph().await;
    ctx.db
        .write(|db| {
            let mut summary = MessageSummary::new(
                "gmsg2",
                "gacc",
                "gfld",
                "thread-g2",
                nuncio_core::models::now(),
            );
            summary.subject = "graph second".into();
            let mut stored = StoredMessage::new(summary);
            stored.uid = Some(778);
            stored.uidvalidity = None;
            stored.gm_msgid = Some("AAMk-graph-2".into());
            repo::messages::upsert(db, &stored)
        })
        .await
        .expect("second graph message");

    service
        .message_act(
            &["gmsg1".to_string(), "gmsg2".to_string()],
            &MessageAction::SetFlagged { value: true },
        )
        .await
        .expect("flag batch");

    let account = ctx
        .db
        .read(|db| repo::accounts::get(db, "gacc"))
        .await
        .expect("account")
        .expect("graph account");
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let worker_ctx = ctx.clone();
    let handle = tokio::spawn(async move {
        nuncio_mail::ops::executor::run_due_graph_with(&worker_ctx, &account, &client).await
    });

    // The first PATCH is now blocked inside the mock. Find out which copy it
    // addressed, then delete the *other* projection before letting it finish.
    mock.wait_for_patch().await;
    let first = mock
        .requests()
        .into_iter()
        .find(|request| request.starts_with("/me/messages/"))
        .expect("first PATCH");
    let (kept_provider_id, removed, removed_provider_id) = if first.contains("AAMk-graph-1") {
        ("AAMk-graph-1", "gmsg2", "AAMk-graph-2")
    } else {
        ("AAMk-graph-2", "gmsg1", "AAMk-graph-1")
    };
    let removed_id = removed.to_string();
    ctx.db
        .write(move |db| repo::messages::delete_local(db, &[removed_id], true))
        .await
        .expect("delete the still-unsent projection");
    mock.release_patch();

    let outcome = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("graph worker timed out")
        .expect("worker join")
        .expect("graph pass");
    assert_eq!(outcome.done, 1);
    let requests = mock.requests();
    assert!(
        requests
            .iter()
            .any(|request| request.contains(kept_provider_id)),
        "the first, still-owned copy must be patched: {requests:?}"
    );
    assert!(
        requests
            .iter()
            .all(|request| !request.contains(removed_provider_id)),
        "a projection deleted while the first PATCH was in flight must not be patched: {requests:?}"
    );
    let op = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| op.kind == nuncio_mail::ops::KIND_SET_FLAGS)
        .expect("flag op");
    let payload = repo::pending_ops::payload::<SetFlagsOp>(&op).expect("payload");
    assert!(
        payload.outcomes.iter().any(|outcome| {
            outcome.message_id == removed && outcome.state == OpMessageState::NotExecuted
        }),
        "the skipped message must record NotExecuted: {:?}",
        payload.outcomes
    );
}

/// A 401 forces the client to refresh its token before retrying. The writer
/// evidence must be re-read *after* that refresh: a projection deleted while
/// the token is refreshing must suppress the second PATCH, and the skipped
/// attempt must be recorded as `NotExecuted`.
#[tokio::test]
async fn graph_401_refresh_rechecks_writer_before_resending() {
    let mock = MockGraph::start_with_unauthorized_gate(0).await;
    let (ctx, service) = setup_graph().await;

    service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: true },
        )
        .await
        .expect("mark read");
    let account = ctx
        .db
        .read(|db| repo::accounts::get(db, "gacc"))
        .await
        .expect("account")
        .expect("graph account");
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let worker_ctx = ctx.clone();
    let handle = tokio::spawn(async move {
        nuncio_mail::ops::executor::run_due_graph_with(&worker_ctx, &account, &client).await
    });

    // The first PATCH has been refused and the client is about to refresh its
    // token. Drop the projection (and therefore its writer) in that window.
    mock.wait_for_unauthorized().await;
    ctx.db
        .write(|db| repo::messages::delete_local(db, &["gmsg1".to_string()], true))
        .await
        .expect("delete projection");
    mock.release_unauthorized();

    let outcome = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("graph worker timed out")
        .expect("worker join")
        .expect("graph pass");
    assert_eq!(outcome.done, 1);

    let requests = mock.requests();
    let patches = requests
        .iter()
        .filter(|request| request.as_str() == "/me/messages/AAMk-graph-1")
        .count();
    assert_eq!(
        patches, 1,
        "the retry after a 401 must be skipped when the writer is gone: {requests:?}"
    );
    let op = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| op.kind == nuncio_mail::ops::KIND_SET_FLAGS)
        .expect("flag op");
    assert_eq!(op.state, PendingOpState::Done);
    let payload = repo::pending_ops::payload::<SetFlagsOp>(&op).expect("payload");
    assert!(
        payload.outcomes.iter().any(|outcome| {
            outcome.message_id == "gmsg1" && outcome.state == OpMessageState::NotExecuted
        }),
        "the skipped retry must record NotExecuted: {:?}",
        payload.outcomes
    );
}

/// A batch can succeed for the first message and fail for the second. The
/// success is persisted per message; a later retry whose writer changed must
/// skip that message without downgrading the earlier `Confirmed` receipt (undo
/// would otherwise skip the remote compensation).
#[tokio::test]
async fn graph_retry_skip_preserves_earlier_confirmed_patch() {
    let mock = MockGraph::start(0).await;
    let (ctx, service) = setup_graph().await;
    ctx.db
        .write(|db| {
            let mut summary = MessageSummary::new(
                "gmsg2",
                "gacc",
                "gfld",
                "thread-g2",
                nuncio_core::models::now(),
            );
            summary.subject = "graph second".into();
            let mut stored = StoredMessage::new(summary);
            stored.uid = Some(778);
            stored.uidvalidity = None;
            stored.gm_msgid = Some("AAMk-graph-2".into());
            repo::messages::upsert(db, &stored)
        })
        .await
        .expect("second graph message");
    // The first PATCH (gmsg1) succeeds; only the second is refused.
    mock.fail_patch_for("AAMk-graph-2", 500);

    service
        .message_act(
            &["gmsg1".to_string(), "gmsg2".to_string()],
            &MessageAction::SetFlagged { value: true },
        )
        .await
        .expect("flag batch");
    let account = ctx
        .db
        .read(|db| repo::accounts::get(db, "gacc"))
        .await
        .expect("account")
        .expect("graph account");
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let first = nuncio_mail::ops::executor::run_due_graph_with(&ctx, &account, &client)
        .await
        .expect("first pass");
    assert_eq!(first.retried, 1, "the refused second PATCH retries");

    let op = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| op.kind == nuncio_mail::ops::KIND_SET_FLAGS)
        .expect("flag op");
    let payload = repo::pending_ops::payload::<SetFlagsOp>(&op).expect("payload");
    assert!(
        payload.outcomes.iter().any(|outcome| {
            outcome.message_id == "gmsg1" && outcome.state == OpMessageState::Confirmed
        }),
        "the successful PATCH must be durable before the batch settles: {:?}",
        payload.outcomes
    );

    // Before the retry, the first message's writer disappears (a sync deleted
    // the projection). The retry may no longer write it, but must keep the
    // earlier Confirmed receipt.
    mock.clear_patch_failures();
    ctx.db
        .write(|db| {
            repo::messages::delete_local(db, &["gmsg1".to_string()], true)?;
            db.execute("UPDATE pending_op SET next_retry_at = NULL", [])?;
            Ok(())
        })
        .await
        .expect("delete projected writer and make retry due");
    let second = nuncio_mail::ops::executor::run_due_graph_with(&ctx, &account, &client)
        .await
        .expect("retry pass");
    assert_eq!(second.done, 1);

    let requests = mock.requests();
    let first_patches = requests
        .iter()
        .filter(|request| request.as_str() == "/me/messages/AAMk-graph-1")
        .count();
    assert_eq!(
        first_patches, 1,
        "the confirmed message must not be PATCHed again: {requests:?}"
    );
    let op = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| op.kind == nuncio_mail::ops::KIND_SET_FLAGS)
        .expect("flag op");
    let payload = repo::pending_ops::payload::<SetFlagsOp>(&op).expect("payload");
    assert!(
        payload.outcomes.iter().any(|outcome| {
            outcome.message_id == "gmsg1" && outcome.state == OpMessageState::Confirmed
        }),
        "a later skip must not overwrite the earlier Confirmed receipt: {:?}",
        payload.outcomes
    );
    assert!(
        payload.outcomes.iter().any(|outcome| {
            outcome.message_id == "gmsg2" && outcome.state == OpMessageState::Confirmed
        }),
        "the retried message must now be confirmed: {:?}",
        payload.outcomes
    );
}

/// A payload whose folder belongs to another account must never be executed
/// with the executing account's credentials: the same folder path/UID there
/// can name an unrelated message.
#[tokio::test]
async fn graph_set_flags_rejects_a_folder_owned_by_another_account() {
    let (ctx, _service, mock) = setup_graph_with_mock().await;
    // A second Graph account whose worker will pick up the mismatched row.
    let mut other = account("other", "other@example.com", None, None, None);
    other.provider = ProviderKind::Outlook;
    other.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &other).await;

    // The payload names `gfld` (owned by `gacc`) and its synthetic UID.
    nuncio_mail::ops::enqueue_set_flags(
        &ctx,
        "other",
        "gfld",
        &[777],
        Some(true),
        None,
        None,
        None,
        None,
        "cross-account mark read",
    )
    .await
    .expect("queue mismatched flag");

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome = nuncio_mail::ops::executor::run_due_graph_with(&ctx, &other, &client)
        .await
        .expect("graph pass");
    assert_eq!(outcome.failed, 1, "the mismatch must fail, not execute");
    assert!(
        mock.requests()
            .iter()
            .all(|request| !request.starts_with("/me/messages/")),
        "no PATCH may be sent for another account's folder: {:?}",
        mock.requests()
    );
    let op = ops_for(&ctx, "other")
        .await
        .into_iter()
        .next()
        .expect("op row");
    assert_eq!(op.state, PendingOpState::Failed);
}

/// A payload without `message_ids` predates the writer table and keeps the
/// legacy permission: its synthetic UID still resolves the provider id.
#[tokio::test]
async fn graph_worker_still_sends_a_legacy_payload_without_message_ids() {
    let (ctx, _service, mock) = setup_graph_with_mock().await;

    nuncio_mail::ops::enqueue_set_flags(
        &ctx,
        "gacc",
        "gfld",
        &[777],
        Some(true),
        None,
        None,
        None,
        None,
        "legacy mark read",
    )
    .await
    .expect("queue legacy flag");

    let account = ctx
        .db
        .read(|db| repo::accounts::get(db, "gacc"))
        .await
        .expect("account")
        .expect("graph account");
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome = nuncio_mail::ops::executor::run_due_graph_with(&ctx, &account, &client)
        .await
        .expect("graph pass");
    assert_eq!(outcome.done, 1);
    assert!(
        mock.requests()
            .iter()
            .any(|request| request == "/me/messages/AAMk-graph-1"),
        "a legacy payload keeps sending: {:?}",
        mock.requests()
    );
}

/// Without a provider id pinned by the earlier action (or a receipt), the undo
/// cannot address a server copy. It must keep the cancelled writer blocking
/// instead of guessing from the row's current binding.
#[tokio::test]
async fn graph_requeue_without_a_pinned_identity_sends_nothing() {
    let (ctx, service) = setup_graph().await;

    service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: true },
        )
        .await
        .expect("first mark read");
    let second = service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: false },
        )
        .await
        .expect("then mark unread");

    let second_id = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| {
            repo::pending_ops::payload::<SetFlagsOp>(op)
                .map(|payload| payload.seen == Some(false))
                .unwrap_or(false)
        })
        .map(|op| op.id)
        .expect("second flag op");
    let first_id = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| {
            repo::pending_ops::payload::<SetFlagsOp>(op)
                .map(|payload| payload.seen == Some(true))
                .unwrap_or(false)
        })
        .map(|op| op.id.clone())
        .expect("first flag op");
    // Simulate a payload queued before Graph identities were pinned.
    ctx.db
        .write({
            let first_id = first_id.clone();
            move |db| {
                let op = repo::pending_ops::get(db, &first_id)?.expect("op row");
                let mut payload = repo::pending_ops::payload::<SetFlagsOp>(&op)?;
                payload.remote_ids.clear();
                db.execute(
                    "UPDATE pending_op SET state = 'inProgress', payload_json = ?2 WHERE id = ?1",
                    rusqlite::params![first_id, serde_json::to_string(&payload).unwrap()],
                )?;
                Ok(())
            }
        })
        .await
        .expect("strip pinned identity");
    settle_op_with_outcome(
        &ctx,
        &first_id,
        PendingOpState::InProgress,
        OpMessageState::NotExecuted,
    )
    .await;

    assert!(
        service.message_undo(&second).await.is_err(),
        "the unaddressable restore must be reported to the caller"
    );

    let entry = entry_for(&ctx, &second).await;
    assert_eq!(
        entry.inverse_op_id, None,
        "an unaddressable restore must not queue a command"
    );
    assert_eq!(
        entry.result,
        UndoEntryResult::Failed,
        "an unaddressable restore must become visible, not report Done"
    );
    assert_eq!(request_state(&ctx, &second).await, UndoState::Failed);
    assert!(
        !ops_for(&ctx, "gacc").await.iter().any(|op| {
            op.id != first_id
                && op.state == PendingOpState::Pending
                && repo::pending_ops::payload::<SetFlagsOp>(op)
                    .map(|payload| payload.seen == Some(true))
                    .unwrap_or(false)
        }),
        "no replacement should be sent without a pinned provider id"
    );
    let writer = ctx
        .db
        .read(|db| repo::messages::flag_writer(db, "gmsg1", "seen"))
        .await
        .expect("writer")
        .expect("blocking writer row");
    assert_eq!(
        writer.owner_id, second_id,
        "the cancelled operation must stay the blocking owner"
    );
}

/// While the earlier action is still in flight without a per-message result,
/// the hand-back waits. Once it settles as skipped, resuming the deferred undo
/// queues the restore instead of handing ownership to a row that will not run.
#[tokio::test]
async fn undo_waits_for_an_in_flight_previous_action_before_hand_back() {
    let (ctx, service) = setup_graph().await;

    service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: true },
        )
        .await
        .expect("first mark read");
    let second = service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: false },
        )
        .await
        .expect("then mark unread");

    let queued = ops_for(&ctx, "gacc").await;
    let first_id = queued
        .iter()
        .find(|op| {
            repo::pending_ops::payload::<SetFlagsOp>(op)
                .map(|payload| payload.seen == Some(true))
                .unwrap_or(false)
        })
        .map(|op| op.id.clone())
        .expect("first flag op");
    let second_id = queued
        .iter()
        .find(|op| op.id != first_id)
        .map(|op| op.id.clone())
        .expect("second flag op");
    // The previous batch already sent some of its messages before failing, so
    // it carries a batch-level send intent. That marker says nothing about
    // this message, and no per-message outcome exists for it yet.
    ctx.db
        .write({
            let first_id = first_id.clone();
            move |db| {
                db.execute(
                    "UPDATE pending_op SET state = 'inProgress', send_intent = 1 WHERE id = ?1",
                    rusqlite::params![first_id],
                )?;
                Ok(())
            }
        })
        .await
        .expect("hold first op in flight");

    let seq_before_defer = ctx
        .db
        .read(|db| repo::messages::flag_writer(db, "gmsg1", "seen"))
        .await
        .expect("writer")
        .expect("writer row")
        .seq;
    assert!(service.message_undo(&second).await.expect("undo unread"));
    let entry = entry_for(&ctx, &second).await;
    assert_eq!(
        entry.result,
        UndoEntryResult::Pending,
        "an in-flight predecessor must keep the entry open"
    );
    assert!(entry.inverse_op_id.is_none());
    assert_eq!(request_state(&ctx, &second).await, UndoState::Deferred);
    let writer = ctx
        .db
        .read(|db| repo::messages::flag_writer(db, "gmsg1", "seen"))
        .await
        .expect("writer")
        .expect("writer row");
    assert_eq!(
        writer.owner_id, second_id,
        "the cancelled action must stay the writer until the predecessor settles"
    );
    assert!(
        writer.seq > seq_before_defer,
        "restoring the value while waiting must still bump the observed revision"
    );

    // The earlier action settles as skipped (it saw the newer owner).
    settle_op_with_outcome(
        &ctx,
        &first_id,
        PendingOpState::Done,
        OpMessageState::NotExecuted,
    )
    .await;
    nuncio_mail::service::resume_deferred_undo_requests(&ctx)
        .await
        .expect("resume");

    let entry = entry_for(&ctx, &second).await;
    assert_eq!(request_state(&ctx, &second).await, UndoState::Compensating);
    let replacement = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| entry.inverse_op_id.as_deref() == Some(op.id.as_str()))
        .expect("tracked replacement");
    let payload = repo::pending_ops::payload::<SetFlagsOp>(&replacement).expect("payload");
    assert_eq!(payload.remote_ids, vec!["AAMk-graph-1".to_string()]);
}

/// 2. A mix of `done` and `pending`: compensate the done message and cancel the
///    pending one, ending `Done`.
#[tokio::test]
async fn undo_compensates_done_entries_and_cancels_pending_ones() {
    let (ctx, service, _mock, _account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    insert_stored(&ctx, "acc", "m-inbox", &inbox).await;
    insert_stored(&ctx, "acc", "m-archive", &archive).await;

    let undo_id = service
        .message_act(
            &["m-inbox".to_string(), "m-archive".to_string()],
            &MessageAction::SetFlagged { value: true },
        )
        .await
        .expect("flag");
    let queued = ops(&ctx).await;
    assert_eq!(queued.len(), 2, "one operation per source folder");

    // Mark only the INBOX operation done, as if it had reached the server.
    let inbox_op = queued
        .iter()
        .find(|op| {
            repo::pending_ops::payload::<SetFlagsOp>(op)
                .map(|payload| payload.folder_id == inbox)
                .unwrap_or(false)
        })
        .expect("inbox op")
        .clone();
    ctx.db
        .write({
            let id = inbox_op.id.clone();
            move |db| repo::pending_ops::mark_done(db, &id)
        })
        .await
        .expect("mark done");

    assert!(service.message_undo(&undo_id).await.expect("undo"));

    // The done message gets exactly one inverse operation.
    let entries = {
        let id = undo_id.clone();
        ctx.db
            .read(move |db| repo::undo::list_entries(db, &id))
            .await
            .expect("entries")
    };
    let inbox_entry = entries
        .iter()
        .find(|entry| entry.message_id == "m-inbox")
        .expect("inbox entry");
    assert!(inbox_entry.inverse_op_id.is_some());
    assert_eq!(
        inbox_entry.result,
        UndoEntryResult::Pending,
        "queueing the inverse is not proof that it completed"
    );
    let archive_entry = entries
        .iter()
        .find(|entry| entry.message_id == "m-archive")
        .expect("archive entry");
    assert!(archive_entry.inverse_op_id.is_none());
    assert_eq!(archive_entry.result, UndoEntryResult::Done);

    // The inverse operation moves back the same folder-local identity.
    let inverse_id = inbox_entry.inverse_op_id.as_deref().unwrap();
    let inverse = ops(&ctx)
        .await
        .into_iter()
        .find(|op| op.id == inverse_id)
        .expect("inverse op");
    assert_eq!(inverse.state, PendingOpState::Pending);
    let payload = repo::pending_ops::payload::<SetFlagsOp>(&inverse).expect("payload");
    assert_eq!(payload.folder_id, inbox);
    assert_eq!(payload.seen, None);
    assert_eq!(payload.flagged, Some(false));

    // The pending operation was cancelled instead of compensated.
    let cancelled = ops(&ctx)
        .await
        .into_iter()
        .find(|op| op.id == queued.iter().find(|op| op.id != inbox_op.id).unwrap().id)
        .expect("archive op");
    assert_eq!(cancelled.state, PendingOpState::Cancelled);

    // Both rows are back to their pre-action flags.
    assert!(!stored(&ctx, "m-inbox").await.summary.flagged);
    assert!(!stored(&ctx, "m-archive").await.summary.flagged);
    assert_eq!(
        request_state(&ctx, &undo_id).await,
        UndoState::Compensating,
        "the request is not done until the inverse operation settles"
    );

    // The inverse operation settles with a confirmed receipt; the worker's
    // resume pass then finishes the entry. `done` alone is not evidence.
    settle_op_for_message(
        &ctx,
        inverse_id,
        "m-inbox",
        PendingOpState::Done,
        OpMessageState::Confirmed,
    )
    .await;
    nuncio_mail::service::resume_deferred_undo_requests(&ctx)
        .await
        .expect("resume");
    assert_eq!(request_state(&ctx, &undo_id).await, UndoState::Done);
    assert_eq!(
        entry_results(&ctx, &undo_id).await,
        vec![UndoEntryResult::Done, UndoEntryResult::Done]
    );
}

/// 3. An in-flight operation defers the request; once it settles the worker's
///    resume pass finishes the compensation.
#[tokio::test]
async fn undo_defers_for_in_flight_operation_then_resumes() {
    let (ctx, service, _mock, _account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    insert_stored(&ctx, "acc", "m1", &inbox).await;

    let undo_id = service
        .message_act(
            &["m1".to_string()],
            &MessageAction::SetFlagged { value: true },
        )
        .await
        .expect("flag");
    let op = ops(&ctx).await.into_iter().next().expect("op");
    ctx.db
        .write({
            let id = op.id.clone();
            move |db| repo::pending_ops::mark_in_progress(db, &id)
        })
        .await
        .expect("mark in progress");

    // Accepted, but not applied: the operation may already be on the wire.
    assert!(service.message_undo(&undo_id).await.expect("defer"));
    assert_eq!(request_state(&ctx, &undo_id).await, UndoState::Deferred);
    assert!(stored(&ctx, "m1").await.summary.flagged);
    assert_eq!(
        ops(&ctx)
            .await
            .into_iter()
            .find(|queued| queued.id == op.id)
            .expect("op")
            .state,
        PendingOpState::InProgress
    );

    // The operation settles; the next worker pass resumes the undo.
    ctx.db
        .write({
            let id = op.id.clone();
            move |db| repo::pending_ops::mark_done(db, &id)
        })
        .await
        .expect("mark done");
    let resumed = nuncio_mail::service::resume_deferred_undo_requests(&ctx)
        .await
        .expect("resume");
    assert_eq!(resumed, 1);

    assert_eq!(
        request_state(&ctx, &undo_id).await,
        UndoState::Compensating,
        "the inverse is queued but has not run yet"
    );
    assert!(!stored(&ctx, "m1").await.summary.flagged);
    let inverse = ops(&ctx)
        .await
        .into_iter()
        .find(|queued| queued.id != op.id)
        .expect("inverse op");
    assert_eq!(inverse.state, PendingOpState::Pending);
    assert!(repo::pending_ops::payload::<SetFlagsOp>(&inverse).is_ok());

    // Only the inverse's actual confirmed result makes the entry (and request)
    // done; the queue row's `done` state alone is not delivery evidence.
    let inverse_id = inverse.id.clone();
    settle_op_for_message(
        &ctx,
        &inverse_id,
        "m1",
        PendingOpState::Done,
        OpMessageState::Confirmed,
    )
    .await;
    let resumed = nuncio_mail::service::resume_deferred_undo_requests(&ctx)
        .await
        .expect("resume");
    assert_eq!(resumed, 1);
    assert_eq!(request_state(&ctx, &undo_id).await, UndoState::Done);
    assert_eq!(
        entry_results(&ctx, &undo_id).await,
        vec![UndoEntryResult::Done]
    );
}

/// A deferred entry is created before its operation reports a destination.
/// Compensation must re-read the later `COPYUID` receipt instead of using the
/// empty identity captured when the undo began.
#[tokio::test]
async fn deferred_undo_uses_a_receipt_learned_after_the_entry_was_created() {
    let (ctx, service, _mock, _account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    let uid = insert_stored(&ctx, "acc", "m1", &inbox).await;

    let undo_id = service
        .message_act(
            &["m1".to_string()],
            &MessageAction::Move {
                folder_id: archive.clone(),
            },
        )
        .await
        .expect("move");
    let op = ops(&ctx).await.into_iter().next().expect("op");
    // The worker claimed the move but has not reported its destination yet.
    ctx.db
        .write({
            let id = op.id.clone();
            move |db| repo::pending_ops::mark_in_progress(db, &id)
        })
        .await
        .expect("mark in progress");

    assert!(service.message_undo(&undo_id).await.expect("defer"));
    assert_eq!(request_state(&ctx, &undo_id).await, UndoState::Deferred);
    let entry = {
        let id = undo_id.clone();
        ctx.db
            .read(move |db| repo::undo::list_entries(db, &id))
            .await
            .expect("entries")
            .remove(0)
    };
    assert!(entry.target_uid.is_none(), "no receipt existed yet");

    // The move later reports its destination UID.
    let destination = 4_242_i64;
    let mut payload = repo::pending_ops::payload::<MoveOp>(&op).expect("move payload");
    payload.outcomes = vec![OpMessageOutcome {
        message_id: "m1".into(),
        source_uid: Some(uid),
        state: OpMessageState::Confirmed,
        target_folder_id: Some(archive.clone()),
        target_uid: Some(destination),
        target_uidvalidity: Some(1),
        target_remote_id: None,
        attempt_remote_id: None,
        copy_phase: Default::default(),
        delete_flag_phase: Default::default(),
        expunge_phase: Default::default(),
    }];
    let value = serde_json::to_value(&payload).expect("json");
    ctx.db
        .write({
            let id = op.id.clone();
            move |db| {
                repo::pending_ops::update_payload(db, &id, &value)?;
                repo::pending_ops::mark_done(db, &id)
            }
        })
        .await
        .expect("settle move");

    let resumed = nuncio_mail::service::resume_deferred_undo_requests(&ctx)
        .await
        .expect("resume");
    assert_eq!(resumed, 1);
    assert_eq!(request_state(&ctx, &undo_id).await, UndoState::Compensating);
    let inverse = ops(&ctx)
        .await
        .into_iter()
        .find(|queued| queued.id != op.id)
        .expect("inverse op");
    let inverse_payload = repo::pending_ops::payload::<MoveOp>(&inverse).expect("inverse payload");
    assert_eq!(inverse_payload.folder_id, archive);
    assert_eq!(inverse_payload.target_folder_id, inbox);
    assert_eq!(
        inverse_payload.uids,
        vec![destination],
        "the inverse addresses the receipt the operation learned later"
    );
}

/// 4. Failing to queue the inverse operation marks the entry failed, keeps the
///    request retryable and never queues a duplicate.
#[tokio::test]
async fn undo_failed_inverse_enqueue_is_retryable_without_duplicates() {
    let (ctx, service, _mock, _account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let uid = insert_stored(&ctx, "acc", "m1", &inbox).await;

    let undo_id = service
        .message_act(
            &["m1".to_string()],
            &MessageAction::SetFlagged { value: true },
        )
        .await
        .expect("flag");
    let op = ops(&ctx).await.into_iter().next().expect("op");
    ctx.db
        .write({
            let id = op.id.clone();
            move |db| repo::pending_ops::mark_done(db, &id)
        })
        .await
        .expect("mark done");

    // Rebuild the request against a missing account: queueing the inverse
    // operation then violates the `pending_op.account_id` foreign key.
    let payload_entry = ctx
        .db
        .read(|db| {
            let log = repo::undo::peek_last(db, None)?.expect("undo log");
            Ok(log.payload["entries"][0].clone())
        })
        .await
        .expect("payload entry");
    let op_id = op.id.clone();
    let request_id = undo_id.clone();
    let message_id = "m1".to_string();
    let folder = inbox.clone();
    ctx.db
        .write(move |db| {
            repo::undo::create_request(
                db,
                NewUndoRequest::new(
                    request_id.clone(),
                    "ghost-account",
                    "setFlagged",
                    Some(true),
                    vec![op_id.clone()],
                ),
                &[NewUndoEntry {
                    id: new_id(),
                    request_id,
                    account_id: "ghost-account".into(),
                    message_id,
                    original_folder_id: folder.clone(),
                    target_folder_id: Some(folder),
                    target_uid: Some(uid),
                    target_uidvalidity: Some(1),
                    target_graph_id: None,
                    permanent: false,
                    op_ids: vec![op_id],
                    evidence_missing: false,
                    payload: payload_entry,
                    ordinal: 0,
                }],
            )?;
            Ok(())
        })
        .await
        .expect("request");

    let ops_before = ops(&ctx).await.len();
    assert!(service.message_undo(&undo_id).await.is_err());
    assert_eq!(request_state(&ctx, &undo_id).await, UndoState::Failed);
    assert_eq!(
        entry_results(&ctx, &undo_id).await,
        vec![UndoEntryResult::Failed]
    );
    // No inverse operation was committed.
    assert_eq!(ops(&ctx).await.len(), ops_before);
    let entry = {
        let id = undo_id.clone();
        ctx.db
            .read(move |db| repo::undo::list_entries(db, &id))
            .await
            .expect("entries")
            .remove(0)
    };
    assert!(entry.inverse_op_id.is_none());

    // Retrying fails again but still does not queue anything twice.
    assert!(service.message_undo(&undo_id).await.is_err());
    assert_eq!(ops(&ctx).await.len(), ops_before);
    let entry = {
        let id = undo_id.clone();
        ctx.db
            .read(move |db| repo::undo::list_entries(db, &id))
            .await
            .expect("entries")
            .remove(0)
    };
    assert!(entry.inverse_op_id.is_none());
    assert_eq!(entry.result, UndoEntryResult::Failed);
}

/// 5. A permanent delete that already reached the server cannot be undone and
///    is returned as an explicit failure.
#[tokio::test]
async fn undo_confirmed_permanent_delete_fails_without_restoring() {
    let (ctx, service, _mock, _account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    insert_stored(&ctx, "acc", "m1", &inbox).await;

    let undo_id = service
        .message_act(
            &["m1".to_string()],
            &MessageAction::Delete { permanent: true },
        )
        .await
        .expect("delete");
    let op = ops(&ctx).await.into_iter().next().expect("op");
    assert!(
        repo::pending_ops::payload::<DeleteOp>(&op)
            .expect("payload")
            .permanent
    );
    ctx.db
        .write({
            let id = op.id.clone();
            move |db| repo::pending_ops::mark_done(db, &id)
        })
        .await
        .expect("mark done");

    let error = service.message_undo(&undo_id).await.expect_err("must fail");
    assert!(
        error.to_string().contains("已删除，无法撤销"),
        "unexpected error: {error}"
    );
    let row = stored(&ctx, "m1").await;
    assert!(row.is_deleted_local, "the row must stay hidden");
    assert_eq!(request_state(&ctx, &undo_id).await, UndoState::Failed);
    assert_eq!(
        entry_results(&ctx, &undo_id).await,
        vec![UndoEntryResult::Failed]
    );
}

/// 6. `+FLAGS \Deleted` succeeded but the EXPUNGE was refused: the undo must not
///    unhide the row or reuse its old UID.
#[tokio::test]
async fn undo_does_not_unhide_after_store_succeeded_but_expunge_failed() {
    let (ctx, service, _mock, _account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let original_uid = insert_stored(&ctx, "acc", "m1", &inbox).await;

    let undo_id = service
        .message_act(
            &["m1".to_string()],
            &MessageAction::Delete { permanent: true },
        )
        .await
        .expect("delete");
    let op = ops(&ctx).await.into_iter().next().expect("op");
    let mut payload = repo::pending_ops::payload::<DeleteOp>(&op).expect("payload");
    payload.deleted_flag_set = true;
    let value = serde_json::to_value(&payload).expect("json");
    ctx.db
        .write({
            let id = op.id.clone();
            move |db| repo::pending_ops::update_payload(db, &id, &value)
        })
        .await
        .expect("side-effect marker");
    ctx.db
        .write({
            let id = op.id.clone();
            move |db| repo::pending_ops::mark_failed(db, &id, "EXPUNGE refused")
        })
        .await
        .expect("mark failed");

    let result = service.message_undo(&undo_id).await;
    assert!(
        result.is_err(),
        "the partially applied delete is not undoable"
    );

    let row = stored(&ctx, "m1").await;
    assert!(
        row.is_deleted_local,
        "a successful \\Deleted STORE must keep the row hidden"
    );
    let pending: i64 = ctx
        .db
        .read(|db| {
            Ok(db.query_row(
                "SELECT pending_delete FROM message WHERE id = 'm1'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .expect("pending_delete");
    assert_eq!(pending, 1);
    assert_eq!(
        row.uid,
        Some(original_uid),
        "the old UID must not be reused"
    );
    assert_eq!(request_state(&ctx, &undo_id).await, UndoState::Failed);
}

/// A completed move is compensated through the target folder's identity, and
/// the local row returns without carrying the target UID into the source.
#[tokio::test]
async fn undo_of_a_done_move_uses_the_target_identity() {
    let (ctx, service, _mock, _account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    let source_uid = insert_stored(&ctx, "acc", "m1", &inbox).await;

    let undo_id = service
        .message_act(
            &["m1".to_string()],
            &MessageAction::Move {
                folder_id: archive.clone(),
            },
        )
        .await
        .expect("move");
    let op = ops(&ctx).await.into_iter().next().expect("op");
    // Simulate the executor's real success path: the MOVE reached the server,
    // COPYUID named the destination, and the executor persisted that
    // per-message result before settling the operation.
    let mut payload = repo::pending_ops::payload::<MoveOp>(&op).expect("payload");
    payload.outcomes = vec![OpMessageOutcome {
        message_id: "m1".into(),
        source_uid: Some(source_uid),
        state: OpMessageState::Confirmed,
        target_folder_id: Some(archive.clone()),
        target_uid: Some(700),
        target_uidvalidity: Some(1),
        target_remote_id: None,
        attempt_remote_id: None,
        copy_phase: Default::default(),
        delete_flag_phase: Default::default(),
        expunge_phase: Default::default(),
    }];
    let value = serde_json::to_value(&payload).expect("json");
    ctx.db
        .write({
            let id = op.id.clone();
            move |db| {
                repo::pending_ops::update_payload(db, &id, &value)?;
                repo::messages::set_uid_identity(db, "m1", Some(700), Some(1))?;
                repo::pending_ops::mark_done(db, &id)
            }
        })
        .await
        .expect("bind target identity");

    assert!(service.message_undo(&undo_id).await.expect("undo move"));

    let restored = stored(&ctx, "m1").await;
    assert_eq!(restored.summary.folder_id, inbox);
    assert_eq!(
        restored.uid, None,
        "the target UID must not leak into INBOX"
    );
    let pending = ctx
        .db
        .read(|db| repo::messages::target_pending_ids(db, &["m1".to_string()]))
        .await
        .expect("target pending");
    assert_eq!(pending, vec!["m1".to_string()]);

    let inverse = ops(&ctx)
        .await
        .into_iter()
        .find(|queued| queued.id != op.id)
        .expect("inverse op");
    let payload = repo::pending_ops::payload::<MoveOp>(&inverse).expect("payload");
    assert_eq!(payload.folder_id, archive);
    assert_eq!(payload.target_folder_id, inbox);
    assert_eq!(payload.uids, vec![700]);
    assert_eq!(payload.message_ids, vec!["m1".to_string()]);
    assert_eq!(
        request_state(&ctx, &undo_id).await,
        UndoState::Compensating,
        "the inverse MOVE is queued, not confirmed"
    );
}

/// A crash after the send intent was persisted but before the receipt must not
/// make undo treat the operation as "never sent". Recovery keeps the operation
/// retryable but marks its outcome unknown, so undo defers instead of
/// cancelling it and restoring the snapshot.
#[tokio::test]
async fn recovered_in_flight_operation_is_never_cancelled_as_unsent() {
    let (ctx, service, mock, account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    insert_stored(&ctx, "acc", "m1", &inbox).await;

    let undo_id = service
        .message_act(
            &["m1".to_string()],
            &MessageAction::SetFlagged { value: true },
        )
        .await
        .expect("flag");
    let op = ops(&ctx).await.into_iter().next().expect("op");
    // Crash model: the intent was durable, the row was in flight, and the
    // process stopped before the server's answer was recorded.
    ctx.db
        .write({
            let id = op.id.clone();
            move |db| {
                db.execute(
                    "UPDATE pending_op
                        SET state = 'inProgress', send_intent = 1, updated_at = ?2
                      WHERE id = ?1",
                    rusqlite::params![
                        id,
                        nuncio_core::repo::dt_string(
                            chrono::Utc::now() - chrono::Duration::hours(1)
                        )
                    ],
                )?;
                Ok(())
            }
        })
        .await
        .expect("stale in-flight row");

    // The next worker pass recovers the row and retries it; the server refuses
    // the retry, but the first attempt may still have applied.
    mock.state.set_fail_store(true);
    let mut connection = connect(&account).await;
    nuncio_mail::ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("recover and retry");

    let recovered = ops(&ctx)
        .await
        .into_iter()
        .find(|queued| queued.id == op.id)
        .expect("recovered op");
    assert!(
        recovered.outcome_unknown,
        "recovery must keep the unknown outcome marker"
    );
    assert_ne!(
        recovered.state,
        PendingOpState::Cancelled,
        "the retryable row must stay executable"
    );

    // Undo must defer, not cancel/restore.
    assert!(service.message_undo(&undo_id).await.expect("defer"));
    assert_eq!(request_state(&ctx, &undo_id).await, UndoState::Deferred);
    assert!(
        stored(&ctx, "m1").await.summary.flagged,
        "an operation that may have reached the server must not be rolled back"
    );
    assert_ne!(
        ops(&ctx)
            .await
            .into_iter()
            .find(|queued| queued.id == op.id)
            .expect("op")
            .state,
        PendingOpState::Cancelled
    );
}

/// A reverse MOVE is only emitted when the original operation recorded the
/// destination identity itself (COPYUID). Without it, Message-ID/UID guessing
/// could move an unrelated copy, so the entry stays retryable.
#[tokio::test]
async fn undo_does_not_guess_a_move_destination_without_copyuid() {
    let (ctx, service, mock, account) = setup().await;
    mock.state.add_message(
        "INBOX",
        &message_raw(
            "alice@example.com",
            "test@example.com",
            "Move me",
            "body",
            "move@example.com",
        ),
    );
    sync_once(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    let message_id = synced_message_id(&ctx, &inbox).await;

    let undo_id = service
        .message_act(
            std::slice::from_ref(&message_id),
            &MessageAction::Move {
                folder_id: archive.clone(),
            },
        )
        .await
        .expect("move");
    let mut connection = connect(&account).await;
    nuncio_mail::ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("execute move");

    let queued = ops(&ctx).await;
    assert_eq!(queued.len(), 1, "no inverse may be queued");
    let error = service
        .message_undo(&undo_id)
        .await
        .expect_err("an unattributable move is not undoable");
    assert!(
        error.to_string().contains("destination identity"),
        "unexpected error: {error}"
    );
    assert_eq!(request_state(&ctx, &undo_id).await, UndoState::Failed);
    assert_eq!(
        entry_results(&ctx, &undo_id).await,
        vec![UndoEntryResult::Failed]
    );
    assert_eq!(ops(&ctx).await.len(), 1, "still no inverse op");

    // The projection is left in the target for the sync; undo never moved a
    // guessed copy.
    assert_eq!(stored(&ctx, &message_id).await.summary.folder_id, archive);
}

/// With a real `COPYUID` receipt the executor persists the destination UID and
/// the inverse MOVE addresses exactly that UID.
#[tokio::test]
async fn undo_of_a_copyuid_move_targets_the_recorded_destination() {
    let (ctx, service, mock, account) = setup().await;
    mock.state.set_copy_uid_plus(true);
    mock.state.add_message(
        "INBOX",
        &message_raw(
            "alice@example.com",
            "test@example.com",
            "Move me",
            "body",
            "move@example.com",
        ),
    );
    sync_once(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    let message_id = synced_message_id(&ctx, &inbox).await;

    let undo_id = service
        .message_act(
            std::slice::from_ref(&message_id),
            &MessageAction::Move {
                folder_id: archive.clone(),
            },
        )
        .await
        .expect("move");
    let mut connection = connect(&account).await;
    nuncio_mail::ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("execute move");

    let original = ops(&ctx).await.into_iter().next().expect("op");
    let payload = repo::pending_ops::payload::<MoveOp>(&original).expect("payload");
    let destination = payload.outcomes[0]
        .target_uid
        .expect("COPYUID destination must be persisted");

    assert!(service.message_undo(&undo_id).await.expect("undo"));
    let inverse = ops(&ctx)
        .await
        .into_iter()
        .find(|queued| queued.id != original.id)
        .expect("inverse op");
    let inverse_payload = repo::pending_ops::payload::<MoveOp>(&inverse).expect("inverse payload");
    assert_eq!(inverse_payload.uids, vec![destination]);
    assert_eq!(inverse_payload.folder_id, archive);
    assert_eq!(inverse_payload.target_folder_id, inbox);
}

/// The inverse operation is queued synchronously, but the entry is only done
/// when that operation actually settles. An inverse that was sent and then
/// failed may already have applied, so its reference is kept: re-queueing a
/// fresh inverse could repeat the command.
#[tokio::test]
async fn failed_inverse_with_send_evidence_is_never_requeued() {
    let (ctx, service, mock, account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    insert_stored(&ctx, "acc", "m1", &inbox).await;

    let undo_id = service
        .message_act(
            &["m1".to_string()],
            &MessageAction::SetFlagged { value: true },
        )
        .await
        .expect("flag");
    let original = ops(&ctx).await.into_iter().next().expect("op");
    let mut connection = connect(&account).await;
    nuncio_mail::ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("execute flag");
    assert_eq!(
        ops(&ctx)
            .await
            .into_iter()
            .find(|queued| queued.id == original.id)
            .expect("op")
            .state,
        PendingOpState::Done
    );

    // Undo queues the inverse and restores the flag optimistically.
    assert!(service.message_undo(&undo_id).await.expect("undo"));
    assert_eq!(request_state(&ctx, &undo_id).await, UndoState::Compensating);
    let inverse = ops(&ctx)
        .await
        .into_iter()
        .find(|queued| queued.id != original.id)
        .expect("inverse op");

    // The server refuses the inverse `UID STORE` on every attempt.
    mock.state.set_fail_store(true);
    let mut connection = connect(&account).await;
    let inverse_id = inverse.id.clone();
    for _ in 0..3 {
        nuncio_mail::ops::executor::run_due(&ctx, &mut connection, &account)
            .await
            .expect("inverse pass");
        let due_id = inverse_id.clone();
        ctx.db
            .write(move |db| {
                db.execute(
                    "UPDATE pending_op SET next_retry_at = NULL WHERE id = ?1",
                    rusqlite::params![due_id],
                )?;
                Ok(())
            })
            .await
            .expect("make due");
    }

    let inverse = ops(&ctx)
        .await
        .into_iter()
        .find(|queued| queued.id == inverse.id)
        .expect("inverse op");
    assert_eq!(inverse.state, PendingOpState::Failed);
    assert_eq!(request_state(&ctx, &undo_id).await, UndoState::Failed);
    let entry = {
        let id = undo_id.clone();
        ctx.db
            .read(move |db| repo::undo::list_entries(db, &id))
            .await
            .expect("entries")
            .remove(0)
    };
    assert_eq!(entry.result, UndoEntryResult::Failed);
    assert_eq!(
        entry.inverse_op_id.as_deref(),
        Some(inverse_id.as_str()),
        "an inverse that reached the wire keeps its reference; a retry must not queue a second one"
    );
}

/// An inverse that never reached the wire may be re-planned. The fresh
/// enqueue must move the entry back to `pending`; otherwise the request stays
/// terminal and the worker never observes the new inverse.
#[tokio::test]
async fn inverse_that_never_ran_is_requeued_and_the_entry_returns_to_pending() {
    let (ctx, service, _mock, account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    insert_stored(&ctx, "acc", "m1", &inbox).await;

    let undo_id = service
        .message_act(
            &["m1".to_string()],
            &MessageAction::SetFlagged { value: true },
        )
        .await
        .expect("flag");
    let original = ops(&ctx).await.into_iter().next().expect("op");
    let mut connection = connect(&account).await;
    nuncio_mail::ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("execute flag");

    assert!(service.message_undo(&undo_id).await.expect("undo"));
    let first_inverse = ops(&ctx)
        .await
        .into_iter()
        .find(|queued| queued.id != original.id)
        .expect("inverse op");
    // Cancelled before any command was issued: no send intent.
    ctx.db
        .write({
            let id = first_inverse.id.clone();
            move |db| repo::pending_ops::cancel(db, &id)
        })
        .await
        .expect("cancel inverse");

    assert!(service.message_undo(&undo_id).await.expect("retry undo"));
    assert_eq!(
        request_state(&ctx, &undo_id).await,
        UndoState::Compensating,
        "the request must keep running until the new inverse settles"
    );
    let entry = {
        let id = undo_id.clone();
        ctx.db
            .read(move |db| repo::undo::list_entries(db, &id))
            .await
            .expect("entries")
            .remove(0)
    };
    assert_eq!(entry.result, UndoEntryResult::Pending);
    let replacement = ops(&ctx)
        .await
        .into_iter()
        .find(|queued| queued.id != original.id && queued.id != first_inverse.id)
        .expect("replacement inverse");
    assert_eq!(
        entry.inverse_op_id.as_deref(),
        Some(replacement.id.as_str()),
        "the claim records the replacement inverse"
    );
    assert_eq!(replacement.state, PendingOpState::Pending);
}

/// Undoing mark-as-read must not clear a star that a later action set.
#[tokio::test]
async fn flag_undo_only_reverts_the_field_it_owns() {
    let (ctx, service, _mock, _account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    insert_stored(&ctx, "acc", "m1", &inbox).await;

    let seen_undo = service
        .message_act(&["m1".to_string()], &MessageAction::SetSeen { value: true })
        .await
        .expect("seen");
    service
        .message_act(
            &["m1".to_string()],
            &MessageAction::SetFlagged { value: true },
        )
        .await
        .expect("flag");
    assert!(stored(&ctx, "m1").await.summary.seen);
    assert!(stored(&ctx, "m1").await.summary.flagged);

    assert!(service.message_undo(&seen_undo).await.expect("undo seen"));
    let row = stored(&ctx, "m1").await;
    assert!(!row.summary.seen, "the seen change is reverted");
    assert!(
        row.summary.flagged,
        "a later star must survive undoing an earlier mark-as-read"
    );
}

/// A referenced operation that cannot be resolved is missing evidence, not
/// proof that the message never had a server operation.
#[tokio::test]
async fn missing_operation_evidence_is_never_treated_as_no_remote_operation() {
    let (ctx, service, _mock, _account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    insert_stored(&ctx, "acc", "m1", &inbox).await;

    let undo_id = service
        .message_act(
            &["m1".to_string()],
            &MessageAction::SetFlagged { value: true },
        )
        .await
        .expect("flag");
    let op = ops(&ctx).await.into_iter().next().expect("op");
    ctx.db
        .write({
            let id = op.id.clone();
            move |db| {
                db.execute(
                    "DELETE FROM pending_op WHERE id = ?1",
                    rusqlite::params![id],
                )?;
                Ok(())
            }
        })
        .await
        .expect("drop evidence");

    assert!(service.message_undo(&undo_id).await.expect("defer"));
    assert_eq!(request_state(&ctx, &undo_id).await, UndoState::Deferred);
    assert!(
        stored(&ctx, "m1").await.summary.flagged,
        "missing evidence must not restore the local snapshot"
    );
    let entry = {
        let id = undo_id.clone();
        ctx.db
            .read(move |db| repo::undo::list_entries(db, &id))
            .await
            .expect("entries")
            .remove(0)
    };
    assert!(entry.evidence_missing);
    assert!(entry.op_ids.is_empty());

    // The retry budget is finite: after it is exhausted the entry fails
    // instead of silently becoming "no remote operation".
    assert!(service.message_undo(&undo_id).await.expect("defer again"));
    let result = service.message_undo(&undo_id).await;
    assert!(result.is_err(), "the evidence gap must eventually fail");
    assert_eq!(request_state(&ctx, &undo_id).await, UndoState::Failed);
    assert!(
        stored(&ctx, "m1").await.summary.flagged,
        "the local row was never restored"
    );
}

/// Compilation guard: every inverse payload kind stays constructible from the
/// fields the state machine persists (spotted by dead-code lints otherwise).
#[allow(dead_code)]
fn _payload_kinds(_: MoveOp, _: ArchiveOp, _: MarkJunkOp, _: DeleteOp, _: SetFlagsOp) {}

/// "read -> unread -> read": undoing the first action must not write the field
/// locally, because a later action is its last writer.
#[tokio::test]
async fn undo_of_an_overwritten_flag_keeps_the_latest_local_writer() {
    let (ctx, service, _mock, _account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    insert_stored(&ctx, "acc", "m1", &inbox).await;

    let first = service
        .message_act(&["m1".to_string()], &MessageAction::SetSeen { value: true })
        .await
        .expect("read");
    service
        .message_act(
            &["m1".to_string()],
            &MessageAction::SetSeen { value: false },
        )
        .await
        .expect("unread");
    service
        .message_act(&["m1".to_string()], &MessageAction::SetSeen { value: true })
        .await
        .expect("read again");
    assert!(stored(&ctx, "m1").await.summary.seen);
    let ops_before = ops(&ctx).await.len();
    assert_eq!(ops_before, 3);

    assert!(service.message_undo(&first).await.expect("undo first"));

    assert!(
        stored(&ctx, "m1").await.summary.seen,
        "the newest action's value must survive undoing an older one"
    );
    let after = ops(&ctx).await;
    assert_eq!(after.len(), ops_before, "no extra inverse is queued");
    assert_eq!(
        after
            .iter()
            .filter(|op| op.state == PendingOpState::Cancelled)
            .count(),
        1,
        "only the named action's pending operation is cancelled"
    );
}

/// Local/POP3 rows have no queued operation, so operation order cannot prove
/// ownership. The per-field writer must still keep an older undo from
/// reverting "read -> unread -> read".
#[tokio::test]
async fn local_flag_undo_does_not_override_a_later_local_write() {
    use nuncio_core::models::{Folder, FolderRole, ProviderKind};

    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut local = account("acc", "local@example.com", None, None, None);
    local.provider = ProviderKind::Local;
    insert_account(&ctx, &local).await;
    ctx.db
        .write(|db| {
            repo::folders::insert(
                db,
                &Folder::new("local-inbox", "acc", "Inbox", FolderRole::Inbox),
            )
        })
        .await
        .expect("insert inbox");
    insert_stored(&ctx, "acc", "m1", "local-inbox").await;
    let service = MailService::new(ctx.clone());

    let first = service
        .message_act(&["m1".to_string()], &MessageAction::SetSeen { value: true })
        .await
        .expect("read");
    service
        .message_act(
            &["m1".to_string()],
            &MessageAction::SetSeen { value: false },
        )
        .await
        .expect("unread");
    service
        .message_act(&["m1".to_string()], &MessageAction::SetSeen { value: true })
        .await
        .expect("read again");
    assert!(stored(&ctx, "m1").await.summary.seen);

    assert!(service.message_undo(&first).await.expect("undo first"));
    assert!(
        stored(&ctx, "m1").await.summary.seen,
        "the latest local write must survive the older undo without a remote op"
    );
    assert!(
        ops(&ctx).await.is_empty(),
        "a local account never queues a server operation"
    );
}

/// With every operation settled, undoing the oldest of three identical writes
/// must still not queue an inverse STORE for that field.
#[tokio::test]
async fn undo_of_an_overwritten_flag_queues_no_inverse_store() {
    let (ctx, service, _mock, _account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    insert_stored(&ctx, "acc", "m1", &inbox).await;

    let first = service
        .message_act(&["m1".to_string()], &MessageAction::SetSeen { value: true })
        .await
        .expect("read");
    service
        .message_act(
            &["m1".to_string()],
            &MessageAction::SetSeen { value: false },
        )
        .await
        .expect("unread");
    service
        .message_act(&["m1".to_string()], &MessageAction::SetSeen { value: true })
        .await
        .expect("read again");
    let queued = ops(&ctx).await;
    assert_eq!(queued.len(), 3);
    ctx.db
        .write(move |db| {
            for op in queued {
                repo::pending_ops::mark_done(db, &op.id)?;
            }
            Ok(())
        })
        .await
        .expect("settle all");

    assert!(service.message_undo(&first).await.expect("undo first"));

    assert!(
        stored(&ctx, "m1").await.summary.seen,
        "the latest write must not be reverted by an older action's undo"
    );
    assert_eq!(ops(&ctx).await.len(), 3, "no inverse STORE is queued");
    assert_eq!(request_state(&ctx, &first).await, UndoState::Done);
}

/// The reference to a missing inverse operation is evidence that it was
/// queued. Clearing it would let the next Undo enqueue the same command again.
#[tokio::test]
async fn a_missing_inverse_operation_keeps_its_reference() {
    let (ctx, service, _mock, _account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    insert_stored(&ctx, "acc", "m1", &inbox).await;

    let undo_id = service
        .message_act(
            &["m1".to_string()],
            &MessageAction::SetFlagged { value: true },
        )
        .await
        .expect("flag");
    let op = ops(&ctx).await.into_iter().next().expect("op");
    ctx.db
        .write({
            let id = op.id.clone();
            move |db| repo::pending_ops::mark_done(db, &id)
        })
        .await
        .expect("mark done");
    assert!(service.message_undo(&undo_id).await.expect("compensate"));
    let (entry_id, inverse_id) = {
        let id = undo_id.clone();
        let entries = ctx
            .db
            .read(move |db| repo::undo::list_entries(db, &id))
            .await
            .expect("entries");
        let entry = entries.into_iter().next().expect("entry");
        (
            entry.id,
            entry.inverse_op_id.expect("the inverse was queued"),
        )
    };
    let ops_before = ops(&ctx).await.len();
    // The operation row vanishes (pruned/corrupt), but the entry still proves
    // that an inverse was once enqueued.
    ctx.db
        .write({
            let entry_id = entry_id.clone();
            move |db| {
                db.execute(
                    "UPDATE undo_entry SET inverse_op_id = 'vanished' WHERE id = ?1",
                    [entry_id],
                )?;
                Ok(())
            }
        })
        .await
        .expect("replace the reference");

    for _ in 0..4 {
        let _ = service.message_undo(&undo_id).await;
    }

    assert_eq!(request_state(&ctx, &undo_id).await, UndoState::Failed);
    let entry = {
        let id = undo_id.clone();
        ctx.db
            .read(move |db| repo::undo::list_entries(db, &id))
            .await
            .expect("entries")
            .remove(0)
    };
    assert_eq!(entry.result, UndoEntryResult::Failed);
    assert_eq!(
        entry.inverse_op_id.as_deref(),
        Some("vanished"),
        "missing evidence must never be cleared"
    );
    assert_eq!(
        ops(&ctx).await.len(),
        ops_before,
        "a retry must not enqueue a fresh inverse after the evidence vanished"
    );
    let _ = inverse_id;
}

/// A predecessor that provably never ran may be re-planned, but only a bounded
/// number of times; persistent failure becomes visible instead of looping.
#[tokio::test]
async fn inverse_requeueing_is_bounded() {
    let (ctx, service, _mock, _account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    insert_stored(&ctx, "acc", "m1", &inbox).await;

    let undo_id = service
        .message_act(
            &["m1".to_string()],
            &MessageAction::SetFlagged { value: true },
        )
        .await
        .expect("flag");
    let op = ops(&ctx).await.into_iter().next().expect("op");
    ctx.db
        .write({
            let id = op.id.clone();
            move |db| repo::pending_ops::mark_done(db, &id)
        })
        .await
        .expect("mark done");
    assert!(service.message_undo(&undo_id).await.expect("compensate"));

    for _ in 0..4 {
        // Model the executor's definitive pre-send failure for the newest
        // inverse: Failed without any send evidence.
        let entry = {
            let id = undo_id.clone();
            ctx.db
                .read(move |db| repo::undo::list_entries(db, &id))
                .await
                .expect("entries")
                .remove(0)
        };
        if let Some(inverse_id) = entry.inverse_op_id.clone() {
            ctx.db
                .write(move |db| {
                    repo::pending_ops::mark_failed(db, &inverse_id, "target folder disappeared")
                })
                .await
                .expect("fail the inverse");
        }
        let _ = service.message_undo(&undo_id).await;
    }

    assert_eq!(
        request_state(&ctx, &undo_id).await,
        UndoState::Failed,
        "a persistent pre-send failure must become visible"
    );
    let entry = {
        let id = undo_id.clone();
        ctx.db
            .read(move |db| repo::undo::list_entries(db, &id))
            .await
            .expect("entries")
            .remove(0)
    };
    assert_eq!(entry.result, UndoEntryResult::Failed);
    assert!(
        entry.inverse_op_id.is_some(),
        "the last inverse reference is kept"
    );
    let ops_before = ops(&ctx).await.len();
    let _ = service.message_undo(&undo_id).await;
    assert_eq!(
        ops(&ctx).await.len(),
        ops_before,
        "after the budget is spent no fresh inverse is queued"
    );
}

/// Undoing a local/POP3 flag action has no queued operation whose hand-back
/// would stamp the change, so the restore itself must advance the revision.
/// An open that snapshotted the pre-undo revision would otherwise treat it as
/// its baseline and mark the message read again while the body loads.
#[tokio::test]
async fn local_flag_undo_advances_the_revision() {
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut local = account("acc", "local@example.com", None, None, None);
    local.provider = ProviderKind::Local;
    insert_account(&ctx, &local).await;
    ctx.db
        .write(|db| {
            repo::folders::insert(
                db,
                &Folder::new("local-inbox", "acc", "Inbox", FolderRole::Inbox),
            )
        })
        .await
        .expect("insert inbox");
    insert_stored(&ctx, "acc", "m1", "local-inbox").await;
    let service = MailService::new(ctx.clone());

    let undo_id = service
        .message_act(&["m1".to_string()], &MessageAction::SetSeen { value: true })
        .await
        .expect("mark read");
    let read_revision = ctx
        .db
        .read(|db| repo::messages::flag_revision(db, "m1"))
        .await
        .expect("revision")
        .expect("message row");

    assert!(service.message_undo(&undo_id).await.expect("undo"));
    assert!(!stored(&ctx, "m1").await.summary.seen);
    let undone_revision = ctx
        .db
        .read(|db| repo::messages::flag_revision(db, "m1"))
        .await
        .expect("revision")
        .expect("message row");
    assert!(
        undone_revision > read_revision,
        "a local flag restore must advance the revision auto mark-as-read observes"
    );
    assert!(
        ops(&ctx).await.is_empty(),
        "a local account never queues a server operation"
    );
    // Nothing to send: the local row is the whole projection, so the entry
    // must still settle as Done rather than waiting for a writer hand-off.
    assert_eq!(
        entry_for(&ctx, &undo_id).await.result,
        UndoEntryResult::Done,
        "a local-only restore with nothing to send must settle"
    );
    assert_eq!(request_state(&ctx, &undo_id).await, UndoState::Done);
}

/// A Graph rebind between two flag actions leaves the earlier action targeting
/// the old provider copy. Undoing the newer action must not hand it the field:
/// the executor treats ownership as authorisation, so the old PATCH would be
/// sent even though this undo creates no new payload.
#[tokio::test]
async fn undo_does_not_hand_the_flag_to_a_pending_action_on_another_copy() {
    let (ctx, service) = setup_graph().await;

    service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: true },
        )
        .await
        .expect("first mark read");
    let first_id = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| {
            repo::pending_ops::payload::<SetFlagsOp>(op)
                .map(|payload| payload.seen == Some(true))
                .unwrap_or(false)
        })
        .map(|op| op.id)
        .expect("first flag op");

    // The second action is prepared after the rebind, so its snapshot names the
    // new copy while the earlier payload pins the old one.
    ctx.db
        .write(|db| {
            db.execute(
                "UPDATE message SET gm_msgid = 'AAMk-graph-2' WHERE id = 'gmsg1'",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("rebind row");
    let second = service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: false },
        )
        .await
        .expect("then mark unread");
    let second_id = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| op.id != first_id)
        .map(|op| op.id)
        .expect("second flag op");

    assert!(
        service.message_undo(&second).await.is_err(),
        "the deterministic refusal must be reported to the caller"
    );
    let writer = ctx
        .db
        .read(|db| repo::messages::flag_writer(db, "gmsg1", "seen"))
        .await
        .expect("writer")
        .expect("writer row");
    assert_eq!(
        writer.owner_id, second_id,
        "a pending action on another copy must not regain ownership"
    );
    assert_eq!(
        entry_for(&ctx, &second).await.result,
        UndoEntryResult::Failed,
        "refusing to hand the field back must become visible, not report Done"
    );
    assert_eq!(request_state(&ctx, &second).await, UndoState::Failed);
    let first = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| op.id == first_id)
        .expect("first flag op");
    assert_eq!(
        first.state,
        PendingOpState::Pending,
        "the earlier action stays queued but loses the field"
    );
}

/// A sync may rewrite the local flag between two actions without recording a
/// writer. The earlier action then wrote a different value than the undo
/// restores, so handing it ownership would let that stale value be sent.
#[tokio::test]
async fn undo_does_not_hand_the_flag_to_a_pending_action_with_another_value() {
    let (ctx, service) = setup_graph().await;

    service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: true },
        )
        .await
        .expect("first mark read");
    let first_id = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .next()
        .expect("first flag op")
        .id;

    // A sync flips the row to unread; it records no flag writer.
    ctx.db
        .write(|db| {
            repo::messages::set_flags(db, &["gmsg1".to_string()], Some(false), None, None, None)
        })
        .await
        .expect("sync flag");
    let second = service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: false },
        )
        .await
        .expect("then mark unread");
    let second_id = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| op.id != first_id)
        .map(|op| op.id)
        .expect("second flag op");

    assert!(
        service.message_undo(&second).await.is_err(),
        "the deterministic refusal must be reported to the caller"
    );
    let writer = ctx
        .db
        .read(|db| repo::messages::flag_writer(db, "gmsg1", "seen"))
        .await
        .expect("writer")
        .expect("writer row");
    assert_eq!(
        writer.owner_id, second_id,
        "an earlier action with a different value must not regain ownership"
    );
    assert_eq!(
        entry_for(&ctx, &second).await.result,
        UndoEntryResult::Failed,
        "refusing to hand the field back must become visible, not report Done"
    );
    assert_eq!(request_state(&ctx, &second).await, UndoState::Failed);
    assert!(!stored(&ctx, "gmsg1").await.summary.seen);
}

/// A terminal `Unknown` predecessor proves nothing about what reached the
/// server. It is settled, so it will never execute again: handing it the field
/// would lose the write, and recording the undo as `Done` would leave the
/// local and remote values disagreeing. The restored absolute value must be
/// queued as a tracked inverse instead.
#[tokio::test]
async fn undo_does_not_trust_a_terminal_unknown_predecessor() {
    let (ctx, service) = setup_graph().await;

    // A = mark read, B = mark unread. B owns the field, so A's retry is
    // skipped after its first PATCH left an unknown receipt.
    service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: true },
        )
        .await
        .expect("first mark read");
    let second = service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: false },
        )
        .await
        .expect("then mark unread");

    let queued = ops_for(&ctx, "gacc").await;
    let first_id = queued
        .iter()
        .find(|op| {
            repo::pending_ops::payload::<SetFlagsOp>(op)
                .map(|payload| payload.seen == Some(true))
                .unwrap_or(false)
        })
        .map(|op| op.id.clone())
        .expect("first flag op");
    // The executor sent A's PATCH but never saw the receipt; the skipped retry
    // preserves `Unknown`, and the operation settles as `done`.
    settle_op_with_outcome(
        &ctx,
        &first_id,
        PendingOpState::Done,
        OpMessageState::Unknown,
    )
    .await;

    assert!(service.message_undo(&second).await.expect("undo unread"));

    let entry = entry_for(&ctx, &second).await;
    assert_ne!(
        entry.result,
        UndoEntryResult::Done,
        "a terminal Unknown predecessor must not be treated as delivered"
    );
    let replacement = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| entry.inverse_op_id.as_deref() == Some(op.id.as_str()))
        .expect("the restored value must be queued as a tracked inverse");
    let payload = repo::pending_ops::payload::<SetFlagsOp>(&replacement).expect("payload");
    assert_eq!(payload.seen, Some(true));
    assert_eq!(
        payload.remote_ids,
        vec!["AAMk-graph-1".to_string()],
        "the restore must address the copy the earlier action pinned"
    );
    assert_eq!(request_state(&ctx, &second).await, UndoState::Compensating);
    let writer = ctx
        .db
        .read(|db| repo::messages::flag_writer(db, "gmsg1", "seen"))
        .await
        .expect("writer")
        .expect("writer row");
    assert_eq!(
        writer.owner_id, replacement.id,
        "the replacement must own the field so the executor sends it"
    );
    assert_eq!(writer.value, Some(true));
    assert!(stored(&ctx, "gmsg1").await.summary.seen);
}

/// Regression: the queue row of an inverse can be `done` while its per-message
/// receipt is `Unknown` (the first send lost its completion and the retry was
/// skipped because the writer had moved on). Undo must settle on the
/// per-message evidence, never on the queue's aggregate state.
#[tokio::test]
async fn undo_does_not_settle_a_done_inverse_with_an_unknown_receipt() {
    let (ctx, service, _mock, _account) = setup().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    insert_stored(&ctx, "acc", "m1", &inbox).await;

    let undo_id = service
        .message_act(
            &["m1".to_string()],
            &MessageAction::SetFlagged { value: true },
        )
        .await
        .expect("flag");
    let original = ops(&ctx).await.into_iter().next().expect("op");
    ctx.db
        .write({
            let id = original.id.clone();
            move |db| repo::pending_ops::mark_done(db, &id)
        })
        .await
        .expect("mark done");

    assert!(service.message_undo(&undo_id).await.expect("compensate"));
    let inverse_id = entry_for(&ctx, &undo_id)
        .await
        .inverse_op_id
        .expect("inverse queued");

    // First send lost its completion; the retry was skipped because the writer
    // moved on, so the row is `done` while the message stays `Unknown`.
    settle_op_for_message(
        &ctx,
        &inverse_id,
        "m1",
        PendingOpState::Done,
        OpMessageState::Unknown,
    )
    .await;

    assert!(service.message_undo(&undo_id).await.expect("resume"));
    assert_eq!(
        entry_for(&ctx, &undo_id).await.result,
        UndoEntryResult::Pending,
        "a done inverse with an unknown receipt is not delivery evidence"
    );
    assert_eq!(
        request_state(&ctx, &undo_id).await,
        UndoState::Compensating
    );

    // It never settles as done. The bounded retry budget makes it visible as
    // failed instead.
    let _ = service.message_undo(&undo_id).await;
    assert_eq!(request_state(&ctx, &undo_id).await, UndoState::Failed);
    let entry = entry_for(&ctx, &undo_id).await;
    assert_eq!(entry.result, UndoEntryResult::Failed);
    assert_eq!(
        entry.inverse_op_id.as_deref(),
        Some(inverse_id.as_str()),
        "an inverse that may have reached the server keeps its reference"
    );
}

/// Regression: a deterministic hand-back refusal must not leave the request
/// `Deferred` forever. It becomes a visible `Failed` while the cancelled writer
/// keeps blocking the stale action, so a later Ctrl+Z still consumes the newer
/// undo log row.
#[tokio::test]
async fn shortcut_undo_skips_a_deterministically_refused_request() {
    let (ctx, service) = setup_graph().await;

    // Action A is still queued.
    service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: true },
        )
        .await
        .expect("first mark read");
    // A sync flips the local value without recording a writer, so A's payload
    // writes a different value than the undo of B restores.
    ctx.db
        .write(|db| {
            repo::messages::set_flags(
                db,
                &["gmsg1".to_string()],
                Some(false),
                None,
                None,
                None,
            )
        })
        .await
        .expect("sync flag");
    // Action B is the one whose undo will be refused deterministically.
    let refused = service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetSeen { value: false },
        )
        .await
        .expect("then mark unread");
    let refused_op_id = ops_for(&ctx, "gacc")
        .await
        .into_iter()
        .find(|op| {
            repo::pending_ops::payload::<SetFlagsOp>(op)
                .map(|payload| payload.seen == Some(false))
                .unwrap_or(false)
        })
        .map(|op| op.id)
        .expect("second flag op");

    assert!(
        service.message_undo(&refused).await.is_err(),
        "the deterministic refusal must be visible to the caller"
    );
    assert_eq!(request_state(&ctx, &refused).await, UndoState::Failed);
    assert_eq!(
        entry_for(&ctx, &refused).await.result,
        UndoEntryResult::Failed
    );
    // The cancelled action stays the field's writer: the older queued action
    // still must not send its stale value.
    let writer = ctx
        .db
        .read(|db| repo::messages::flag_writer(db, "gmsg1", "seen"))
        .await
        .expect("writer")
        .expect("writer row");
    assert_eq!(writer.owner_id, refused_op_id);

    // A later, unrelated action produces a new undo log row. Ctrl+Z must skip
    // the failed request and revert this action instead of resuming it.
    let later = service
        .message_act(
            &["gmsg1".to_string()],
            &MessageAction::SetFlagged { value: true },
        )
        .await
        .expect("star");
    assert!(stored(&ctx, "gmsg1").await.summary.flagged);

    assert!(service.message_undo_last().await.expect("undo last"));
    assert!(
        !stored(&ctx, "gmsg1").await.summary.flagged,
        "the shortcut must revert the newer action"
    );
    assert_eq!(
        request_state(&ctx, &refused).await,
        UndoState::Failed,
        "the shortcut must not resume the refused request"
    );
    assert_eq!(request_state(&ctx, &later).await, UndoState::Done);
}
