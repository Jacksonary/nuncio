//! Integration tests for the mail engine against in-process mock servers.

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nuncio_core::models::{
    Address, Draft, Folder, FolderRole, MailScope, MessageListQuery, MessageSummary, NewPendingOp,
    OutboxItem, ProviderKind, ReplyMode, ServerConfig, StoredMessage, SyncModule, SyncState,
};
use nuncio_core::traits::MailAuth;
use nuncio_core::{repo, CoreContext};
use nuncio_mail::{ops, smtp, Engine, EngineConfig};
use support::mock_imap::MockImap;
use support::mock_pop3::MockPop3;
use support::mock_smtp::MockSmtp;
use support::{account, context, insert_account, message_raw, RecordingNotifier, StaticCreds};

fn server(port: u16) -> ServerConfig {
    ServerConfig {
        host: "127.0.0.1".into(),
        port,
        security: nuncio_core::models::Security::None,
        username: "test@example.com".into(),
    }
}

async fn connect_account(
    account: &nuncio_core::models::Account,
) -> nuncio_mail::imap::ImapConnection {
    let config = account.imap.as_ref().expect("imap config");
    let auth = MailAuth::Password {
        username: "test@example.com".into(),
        password: "secret".into(),
    };
    nuncio_mail::imap::connect(config, &auth)
        .await
        .expect("connect mock imap")
}

async fn sync_account(ctx: &CoreContext, account: &nuncio_core::models::Account) {
    let mut connection = connect_account(account).await;
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

async fn folder_uidvalidity(ctx: &CoreContext, folder_id: &str) -> Option<i64> {
    let folder_id = folder_id.to_string();
    ctx.db
        .read(move |db| repo::folders::uidvalidity(db, &folder_id))
        .await
        .expect("uidvalidity")
}

async fn message_count(ctx: &CoreContext, folder_id: &str) -> i64 {
    let folder_id = folder_id.to_string();
    ctx.db
        .read(move |db| repo::messages::count_in_folder(db, &folder_id))
        .await
        .expect("count")
}

async fn subjects(ctx: &CoreContext, folder_id: &str) -> Vec<String> {
    let folder_id = folder_id.to_string();
    let query = MessageListQuery::new(MailScope::folder(folder_id))
        .with_conversation(false)
        .with_limit(200);
    ctx.db
        .read(move |db| repo::messages::list(db, &query))
        .await
        .expect("list")
        .items
        .into_iter()
        .map(|message| message.subject)
        .collect()
}

async fn message_ids(ctx: &CoreContext, folder_id: &str) -> Vec<String> {
    let folder_id = folder_id.to_string();
    let query = MessageListQuery::new(MailScope::folder(folder_id))
        .with_conversation(false)
        .with_limit(200);
    ctx.db
        .read(move |db| repo::messages::list(db, &query))
        .await
        .expect("list")
        .items
        .into_iter()
        .map(|message| message.id)
        .collect()
}

#[tokio::test]
async fn initial_sync_discovers_folders_messages_and_previews() {
    let mock = MockImap::start_with_folders(
        &["IDLE", "CONDSTORE", "MOVE", "UIDPLUS", "SPECIAL-USE"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Sent", &["\\Sent"]),
            ("Drafts", &["\\Drafts"]),
            ("Trash", &["\\Trash"]),
            ("Archive", &["\\Archive"]),
        ],
    )
    .await;
    mock.state.add_message(
        "INBOX",
        &message_raw(
            "Alice <alice@example.com>",
            "test@example.com",
            "First message",
            "hello preview one",
            "m1@example.com",
        ),
    );
    mock.state.add_message(
        "INBOX",
        &message_raw(
            "Bob <bob@example.com>",
            "test@example.com",
            "Second message",
            "hello preview two",
            "m2@example.com",
        ),
    );
    mock.state.add_message(
        "INBOX",
        &message_raw(
            "Carol <carol@example.com>",
            "test@example.com",
            "第三封邮件",
            "中文正文内容",
            "m3@example.com",
        ),
    );

    let notifier = Arc::new(RecordingNotifier::default());
    let creds = StaticCreds::password("test@example.com", "secret");
    let ctx = context(creds, notifier.clone()).await;
    let account = account(
        "acc",
        "test@example.com",
        Some(server(mock.addr.port())),
        None,
        None,
    );
    insert_account(&ctx, &account).await;

    sync_account(&ctx, &account).await;

    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    assert_eq!(message_count(&ctx, &inbox).await, 3);
    let mut subjects = subjects(&ctx, &inbox).await;
    subjects.sort();
    assert!(subjects.contains(&"First message".to_string()));
    assert!(subjects.contains(&"第三封邮件".to_string()));

    // Folder roles/favourites.
    let folders = ctx
        .db
        .read(|db| repo::folders::list(db, "acc"))
        .await
        .expect("folders");
    let roles: Vec<_> = folders.iter().map(|folder| folder.role).collect();
    assert!(roles.contains(&nuncio_core::models::FolderRole::Inbox));
    assert!(roles.contains(&nuncio_core::models::FolderRole::Sent));
    assert!(roles.contains(&nuncio_core::models::FolderRole::Drafts));
    assert!(roles.contains(&nuncio_core::models::FolderRole::Archive));
    let inbox_folder = folders
        .iter()
        .find(|f| f.role == nuncio_core::models::FolderRole::Inbox)
        .unwrap();
    assert!(inbox_folder.is_favorite);

    // Previews are filled from the first text part.
    let query = MessageListQuery::new(MailScope::folder(inbox.clone()))
        .with_conversation(false)
        .with_limit(10);
    let page = ctx
        .db
        .read(move |db| repo::messages::list(db, &query))
        .await
        .expect("page");
    assert!(page
        .items
        .iter()
        .any(|message| message.preview.contains("hello preview")));

    // Initial sync never notifies.
    assert!(notifier.new_mail.lock().expect("notifier").is_empty());
}

#[tokio::test]
async fn incremental_sync_updates_flags_adds_and_expunges() {
    let mock = MockImap::start(&["IDLE", "CONDSTORE", "MOVE", "UIDPLUS"]).await;
    let first = mock.state.add_message(
        "INBOX",
        &message_raw("a@example.com", "test@example.com", "One", "one", "one@x"),
    );
    mock.state.add_message(
        "INBOX",
        &message_raw("b@example.com", "test@example.com", "Two", "two", "two@x"),
    );

    let notifier = Arc::new(RecordingNotifier::default());
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        notifier.clone(),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    assert_eq!(message_count(&ctx, &inbox).await, 2);

    // Server-side changes: flag one, add one, expunge one.
    mock.state.set_flags("INBOX", 2, &["\\Seen"]);
    mock.state.add_message(
        "INBOX",
        &message_raw(
            "c@example.com",
            "test@example.com",
            "Three",
            "three",
            "three@x",
        ),
    );
    mock.state.remove_message("INBOX", first);
    notifier.new_mail.lock().expect("notifier").clear();
    notifier.badges.lock().expect("badges").clear();

    let mut connection = connect_account(&account).await;
    nuncio_mail::sync::sync_account(&ctx, &mut connection, &account)
        .await
        .expect("incremental sync");

    assert_eq!(message_count(&ctx, &inbox).await, 2);
    let subjects = subjects(&ctx, &inbox).await;
    assert!(!subjects.contains(&"One".to_string()));
    assert!(subjects.contains(&"Three".to_string()));

    let query = MessageListQuery::new(MailScope::folder(inbox.clone()))
        .with_conversation(false)
        .with_limit(10);
    let page = ctx
        .db
        .read(move |db| repo::messages::list(db, &query))
        .await
        .expect("page");
    let seen = page
        .items
        .iter()
        .find(|message| message.subject == "Two")
        .expect("Two");
    assert!(seen.seen);

    // New unseen INBOX mail produced a notification and a badge update.
    assert!(notifier
        .new_mail
        .lock()
        .expect("notifier")
        .iter()
        .any(|message| message.subject == "Three"));
    assert!(!notifier.badges.lock().expect("badges").is_empty());
}

#[tokio::test]
async fn uidvalidity_reset_clears_and_resyncs_folder() {
    let mock = MockImap::start(&["CONDSTORE"]).await;
    mock.state.add_message(
        "INBOX",
        &message_raw("a@example.com", "test@example.com", "Old", "old", "old@x"),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    assert_eq!(message_count(&ctx, &inbox).await, 1);

    // New UIDVALIDITY and a completely different message set.
    mock.state.bump_uidvalidity("INBOX", 99);
    mock.state.remove_message("INBOX", 1);
    mock.state.add_message(
        "INBOX",
        &message_raw(
            "b@example.com",
            "test@example.com",
            "Fresh",
            "fresh",
            "fresh@x",
        ),
    );
    let mut connection = connect_account(&account).await;
    nuncio_mail::sync::sync_account(&ctx, &mut connection, &account)
        .await
        .expect("sync after uidvalidity change");

    assert_eq!(message_count(&ctx, &inbox).await, 1);
    let subjects = subjects(&ctx, &inbox).await;
    assert_eq!(subjects, vec!["Fresh".to_string()]);
}

#[tokio::test]
async fn executes_flag_and_move_operations() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "MOVE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Archive", &["\\Archive"])],
    )
    .await;
    let uid = mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Move me",
            "body",
            "move@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    let uidvalidity = folder_uidvalidity(&ctx, &inbox).await;

    ops::enqueue_set_flags(
        &ctx,
        "acc",
        &inbox,
        &[uid as i64],
        Some(true),
        Some(true),
        None,
        None,
        uidvalidity,
        "mark read+flag",
    )
    .await
    .expect("enqueue flags");
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("execute flags");
    assert_eq!(outcome.done, 1);
    let flags = mock.state.message_flags("INBOX", uid);
    assert!(flags.iter().any(|flag| flag.eq_ignore_ascii_case("\\Seen")));
    assert!(flags
        .iter()
        .any(|flag| flag.eq_ignore_ascii_case("\\Flagged")));

    ops::enqueue_move(
        &ctx,
        "acc",
        &inbox,
        &archive,
        &[uid as i64],
        &[],
        uidvalidity,
        "archive it",
    )
    .await
    .expect("enqueue move");
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("execute move");
    assert_eq!(outcome.done, 1);
    assert_eq!(mock.state.message_count("INBOX"), 0);
    assert_eq!(mock.state.message_count("Archive"), 1);
    assert_eq!(message_count(&ctx, &inbox).await, 0);
}

/// A `UID MOVE` whose tagged completion is lost is an ambiguous outcome: the
/// server may have moved the message. The executor must not record a confirmed
/// per-message receipt, and a retry must not send the non-idempotent command
/// again.
#[tokio::test]
async fn move_without_tagged_completion_stays_unknown_and_is_not_resent() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "MOVE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Archive", &["\\Archive"])],
    )
    .await;
    let uid = mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Lost move receipt",
            "body",
            "lost-move@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    let uidvalidity = folder_uidvalidity(&ctx, &inbox).await;

    ops::enqueue_move(
        &ctx,
        "acc",
        &inbox,
        &archive,
        &[uid as i64],
        &[],
        uidvalidity,
        "lost receipt",
    )
    .await
    .expect("enqueue move");
    mock.state.set_move_drop_completion(true);
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first move pass");
    assert_eq!(outcome.retried, 1, "a lost receipt is an ambiguous failure");
    assert_eq!(outcome.done, 0);
    assert_eq!(mock.state.message_count("INBOX"), 0);
    assert_eq!(mock.state.message_count("Archive"), 1);

    let queued = ctx
        .db
        .read(|db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops")
        .into_iter()
        .find(|op| op.kind == ops::KIND_MOVE)
        .expect("move op");
    assert!(queued.send_intent && queued.outcome_unknown);
    let payload = repo::pending_ops::payload::<ops::MoveOp>(&queued).expect("payload");
    assert!(
        payload.outcomes.is_empty(),
        "an ambiguous MOVE must not record a confirmed receipt"
    );

    // The retry is allowed to run only after the backoff. Make it due and
    // clear the drop flag: if the executor resent it, the mock would tell.
    ctx.db
        .write({
            let id = queued.id.clone();
            move |db| {
                db.execute(
                    "UPDATE pending_op SET next_retry_at = NULL WHERE id = ?1",
                    rusqlite::params![id],
                )?;
                Ok(())
            }
        })
        .await
        .expect("make retry due");
    mock.state.set_move_drop_completion(false);
    let move_commands = |commands: Vec<String>| {
        commands
            .into_iter()
            .filter(|command| command.starts_with("UID MOVE"))
            .count()
    };
    let before = move_commands(mock.state.commands());
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("retry pass");
    assert_eq!(outcome.retried, 1);
    assert_eq!(
        move_commands(mock.state.commands()),
        before,
        "an MOVE with unknown evidence must never be sent again"
    );
}

/// Make a queued operation due immediately, so a test does not have to wait
/// out the retry backoff.
async fn make_op_due(ctx: &CoreContext, op_id: &str) {
    let op_id = op_id.to_string();
    ctx.db
        .write(move |db| {
            db.execute(
                "UPDATE pending_op SET next_retry_at = NULL WHERE id = ?1",
                rusqlite::params![op_id],
            )?;
            Ok(())
        })
        .await
        .expect("make op due");
}

async fn move_op_payload(ctx: &CoreContext, op_id: &str) -> ops::MoveOp {
    let op_id = op_id.to_string();
    let op = ctx
        .db
        .read(move |db| repo::pending_ops::get(db, &op_id))
        .await
        .expect("read op")
        .expect("move op row");
    repo::pending_ops::payload(&op).expect("move payload")
}

/// Apply the optimistic local move the command service performs when it queues
/// a remote move (the low-level `enqueue_move` helper only writes the payload).
async fn optimistic_move_pending(ctx: &CoreContext, message_ids: &[String], target: &str) {
    let message_ids = message_ids.to_vec();
    let target = target.to_string();
    ctx.db
        .write(move |db| repo::messages::move_to_folder_pending(db, &message_ids, &target))
        .await
        .expect("optimistic move");
}

async fn queued_move_op(ctx: &CoreContext) -> nuncio_core::models::PendingOp {
    ctx.db
        .read(|db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops")
        .into_iter()
        .find(|op| op.kind == ops::KIND_MOVE)
        .expect("queued move op")
}

fn count_copy_commands(commands: &[String]) -> usize {
    commands
        .iter()
        .filter(|command| command.to_ascii_uppercase().starts_with("UID COPY"))
        .count()
}

fn count_commands(commands: &[String], prefix: &str) -> usize {
    commands
        .iter()
        .filter(|command| command.to_ascii_uppercase().starts_with(prefix))
        .count()
}

/// COPY succeeded (with a COPYUID receipt) but the follow-up STORE failed. A
/// retry must resume at the STORE/EXPUNGE phases and must not COPY again: the
/// target folder would otherwise gain a second copy.
#[tokio::test]
async fn copy_fallback_retry_after_store_failure_skips_copy() {
    let mock = MockImap::start_with_folders(
        &["UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Archive", &["\\Archive"])],
    )
    .await;
    let uid = mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Resume after STORE",
            "body",
            "resume-store@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    let uidvalidity = folder_uidvalidity(&ctx, &inbox).await;
    let message_id = message_ids(&ctx, &inbox).await.pop().expect("message");

    ops::enqueue_move(
        &ctx,
        "acc",
        &inbox,
        &archive,
        &[uid as i64],
        std::slice::from_ref(&message_id),
        uidvalidity,
        "move",
    )
    .await
    .expect("enqueue move");
    optimistic_move_pending(&ctx, std::slice::from_ref(&message_id), &archive).await;
    mock.state.set_copy_uid_plus(true);
    mock.state.set_fail_store(true);

    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first pass");
    assert_eq!(outcome.retried, 1, "the STORE failure must be retried");
    assert_eq!(outcome.done, 0);
    assert_eq!(mock.state.message_count("Archive"), 1);
    assert_eq!(mock.state.message_count("INBOX"), 1);

    let op = queued_move_op(&ctx).await;
    let payload = move_op_payload(&ctx, &op.id).await;
    let recorded = payload
        .outcomes
        .iter()
        .find(|outcome| outcome.message_id == message_id)
        .expect("copy outcome");
    assert_eq!(recorded.copy_phase, ops::CopyPhase::Copied);
    assert_eq!(recorded.delete_flag_phase, ops::DeleteFlagPhase::Unknown);
    assert_eq!(recorded.expunge_phase, ops::ExpungePhase::NotStarted);

    mock.state.set_fail_store(false);
    make_op_due(&ctx, &op.id).await;
    let before = mock.state.commands();
    let copies_before = count_copy_commands(&before);
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("retry pass");
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.failed, 0);

    let after: Vec<String> = mock
        .state
        .commands()
        .into_iter()
        .skip(before.len())
        .collect();
    assert_eq!(
        count_copy_commands(&after),
        0,
        "a confirmed copy must never be sent again: {after:?}"
    );
    assert!(
        count_copy_commands(&mock.state.commands()) == copies_before,
        "the retry must not add a UID COPY"
    );
    assert_eq!(count_commands(&after, "UID STORE"), 1, "{after:?}");
    assert_eq!(count_commands(&after, "UID EXPUNGE"), 1, "{after:?}");
    assert_eq!(mock.state.message_count("Archive"), 1);
    assert_eq!(mock.state.message_count("INBOX"), 0);

    let payload = move_op_payload(&ctx, &op.id).await;
    let recorded = payload
        .outcomes
        .iter()
        .find(|outcome| outcome.message_id == message_id)
        .expect("outcome");
    assert_eq!(recorded.state, ops::OpMessageState::Confirmed);
    assert_eq!(recorded.copy_phase, ops::CopyPhase::Copied);
    assert_eq!(recorded.delete_flag_phase, ops::DeleteFlagPhase::Applied);
    assert_eq!(recorded.expunge_phase, ops::ExpungePhase::Applied);
    assert_eq!(
        recorded.target_uid,
        Some(1),
        "the COPYUID identity must survive the resumed phases"
    );
}

/// A COPY whose tagged completion was lost may have committed. The retry must
/// not send a second COPY. A Message-ID match in the target folder is not
/// proof that *this* operation created that message, so the copy stays
/// `Unknown` even after the target sync binds the projection and the source is
/// never deleted.
#[tokio::test]
async fn lost_copy_receipt_stays_unknown_and_is_not_resent() {
    let mock = MockImap::start_with_folders(
        &["UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Archive", &["\\Archive"])],
    )
    .await;
    let uid = mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Lost copy receipt",
            "body",
            "lost-copy@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    let uidvalidity = folder_uidvalidity(&ctx, &inbox).await;
    let message_id = message_ids(&ctx, &inbox).await.pop().expect("message");

    ops::enqueue_move(
        &ctx,
        "acc",
        &inbox,
        &archive,
        &[uid as i64],
        std::slice::from_ref(&message_id),
        uidvalidity,
        "move",
    )
    .await
    .expect("enqueue move");
    optimistic_move_pending(&ctx, std::slice::from_ref(&message_id), &archive).await;
    let before = mock.state.commands();
    mock.state.set_copy_drop_completion(true);
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first pass");
    assert_eq!(outcome.retried, 1, "a lost COPY receipt is ambiguous");
    assert_eq!(outcome.done, 0);
    // The mock committed the copy before dropping the completion.
    assert_eq!(mock.state.message_count("Archive"), 1);
    assert_eq!(mock.state.message_count("INBOX"), 1);

    let op = queued_move_op(&ctx).await;
    let payload = move_op_payload(&ctx, &op.id).await;
    let recorded = payload
        .outcomes
        .iter()
        .find(|outcome| outcome.message_id == message_id)
        .expect("copy outcome");
    assert_eq!(recorded.copy_phase, ops::CopyPhase::Unknown);
    assert_eq!(recorded.state, ops::OpMessageState::Unknown);
    let first_pass: Vec<String> = mock
        .state
        .commands()
        .into_iter()
        .skip(before.len())
        .collect();
    assert_eq!(
        count_commands(&first_pass, "UID STORE"),
        0,
        "an unconfirmed copy must not delete the source: {first_pass:?}"
    );

    // The immediate retry cannot prove the copy exists (the target folder has
    // not been synced), so it must neither COPY nor delete the source.
    mock.state.set_copy_drop_completion(false);
    make_op_due(&ctx, &op.id).await;
    let copies_before = count_copy_commands(&mock.state.commands());
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("retry pass");
    assert_eq!(outcome.retried, 1);
    assert_eq!(
        count_copy_commands(&mock.state.commands()),
        copies_before,
        "a lost COPY receipt must never be re-sent"
    );
    let op = queued_move_op(&ctx).await;
    assert!(op.outcome_unknown, "the ambiguity must stay durable");
    let payload = move_op_payload(&ctx, &op.id).await;
    let recorded = payload
        .outcomes
        .iter()
        .find(|outcome| outcome.message_id == message_id)
        .expect("outcome");
    assert_eq!(recorded.copy_phase, ops::CopyPhase::Unknown);
    assert_eq!(mock.state.message_count("INBOX"), 1);

    // The target sync binds the copied message to the optimistic projection by
    // its unique Message-ID. That is still not proof that *this* COPY created
    // it (the take-off marker is written before the command is sent), so the
    // retry must keep the phase unknown and must never delete the only
    // provable copy in the source folder.
    sync_account(&ctx, &account).await;
    make_op_due(&ctx, &op.id).await;
    let copies_before = count_copy_commands(&mock.state.commands());
    let commands_before = mock.state.commands().len();
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("reconciled retry");
    assert_eq!(outcome.done, 0, "a Message-ID binding is not a receipt");
    assert_eq!(outcome.retried, 1);
    let after: Vec<String> = mock
        .state
        .commands()
        .into_iter()
        .skip(commands_before)
        .collect();
    assert_eq!(
        count_copy_commands(&mock.state.commands()),
        copies_before,
        "reconciliation must not re-COPY"
    );
    assert_eq!(count_commands(&after, "UID STORE"), 0, "{after:?}");
    assert_eq!(count_commands(&after, "UID EXPUNGE"), 0, "{after:?}");
    assert_eq!(mock.state.message_count("Archive"), 1);
    assert_eq!(mock.state.message_count("INBOX"), 1);

    let op = queued_move_op(&ctx).await;
    assert!(op.outcome_unknown);
    let payload = move_op_payload(&ctx, &op.id).await;
    let recorded = payload
        .outcomes
        .iter()
        .find(|outcome| outcome.message_id == message_id)
        .expect("outcome");
    assert_eq!(recorded.state, ops::OpMessageState::Unknown);
    assert_eq!(recorded.copy_phase, ops::CopyPhase::Unknown);
    assert_eq!(recorded.expunge_phase, ops::ExpungePhase::NotStarted);
}

/// A copy fallback that completes on the first attempt still binds the COPYUID
/// destination exactly once.
#[tokio::test]
async fn copy_fallback_with_copyuid_completes_on_first_pass() {
    let mock = MockImap::start_with_folders(
        &["UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Archive", &["\\Archive"])],
    )
    .await;
    let uid = mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "CopyUID complete",
            "body",
            "copyuid-complete@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    let uidvalidity = folder_uidvalidity(&ctx, &inbox).await;
    let message_id = message_ids(&ctx, &inbox).await.pop().expect("message");

    ops::enqueue_move(
        &ctx,
        "acc",
        &inbox,
        &archive,
        &[uid as i64],
        std::slice::from_ref(&message_id),
        uidvalidity,
        "move",
    )
    .await
    .expect("enqueue move");
    optimistic_move_pending(&ctx, std::slice::from_ref(&message_id), &archive).await;
    mock.state.set_copy_uid_plus(true);
    let before = mock.state.commands();
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first pass");
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.retried, 0);
    let commands: Vec<String> = mock
        .state
        .commands()
        .into_iter()
        .skip(before.len())
        .collect();
    assert_eq!(count_copy_commands(&commands), 1, "{commands:?}");
    assert_eq!(count_commands(&commands, "UID MOVE"), 0, "{commands:?}");
    assert_eq!(mock.state.message_count("Archive"), 1);
    assert_eq!(mock.state.message_count("INBOX"), 0);

    let op = queued_move_op(&ctx).await;
    let payload = move_op_payload(&ctx, &op.id).await;
    let recorded = payload
        .outcomes
        .iter()
        .find(|outcome| outcome.message_id == message_id)
        .expect("outcome");
    assert_eq!(recorded.state, ops::OpMessageState::Confirmed);
    assert_eq!(recorded.copy_phase, ops::CopyPhase::Copied);
    assert_eq!(recorded.target_uid, Some(1));
    assert_eq!(
        recorded.target_uidvalidity,
        folder_uidvalidity(&ctx, &archive).await
    );
}

/// The take-off marker is persisted *before* `UID COPY` is sent. A crash in
/// that window leaves `CopyUnknown`, and the next target sync can bind an
/// unrelated message that merely shares the Message-ID. That binding is not a
/// copy receipt, so the source must stay in place.
#[tokio::test]
async fn unrelated_same_message_id_copy_does_not_authorize_source_delete() {
    let mock = MockImap::start_with_folders(
        &["UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Archive", &["\\Archive"])],
    )
    .await;
    let uid = mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Source copy",
            "the source body",
            "duplicate-header@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    let uidvalidity = folder_uidvalidity(&ctx, &inbox).await;
    let message_id = message_ids(&ctx, &inbox).await.pop().expect("message");

    ops::enqueue_move(
        &ctx,
        "acc",
        &inbox,
        &archive,
        &[uid as i64],
        std::slice::from_ref(&message_id),
        uidvalidity,
        "move",
    )
    .await
    .expect("enqueue move");
    optimistic_move_pending(&ctx, std::slice::from_ref(&message_id), &archive).await;

    // Crash after the take-off marker was written but before `UID COPY` left
    // the process.
    let op = queued_move_op(&ctx).await;
    {
        let mut payload = move_op_payload(&ctx, &op.id).await;
        payload.outcomes = vec![ops::OpMessageOutcome {
            message_id: message_id.clone(),
            source_uid: Some(uid as i64),
            state: ops::OpMessageState::Unknown,
            target_folder_id: None,
            target_uid: None,
            target_uidvalidity: None,
            target_remote_id: None,
            attempt_remote_id: None,
            copy_phase: ops::CopyPhase::Unknown,
            delete_flag_phase: Default::default(),
            expunge_phase: Default::default(),
        }];
        let value = serde_json::to_value(&payload).expect("payload");
        let id = op.id.clone();
        ctx.db
            .write(move |db| {
                repo::pending_ops::update_payload(db, &id, &value)?;
                repo::pending_ops::mark_send_intent(db, &id)?;
                Ok(())
            })
            .await
            .expect("persist take-off");
    }

    // An unrelated message with the same Message-ID but a different body
    // appears in the target. The target sync binds it to the projection.
    mock.state.add_message(
        "Archive",
        &message_raw(
            "b@example.com",
            "test@example.com",
            "Unrelated",
            "a different body",
            "duplicate-header@x",
        ),
    );
    sync_account(&ctx, &account).await;
    // The target sync really did bind the unrelated message to the projection;
    // the executor must nevertheless refuse to treat it as a copy receipt.
    {
        let stored = ctx
            .db
            .read({
                let id = message_id.clone();
                move |db| repo::messages::get(db, &id)
            })
            .await
            .expect("read projection")
            .expect("projection row");
        assert_eq!(stored.summary.folder_id, archive);
        assert!(stored.uid.is_some(), "the target sync must bind a UID");
    }
    make_op_due(&ctx, &op.id).await;

    let before = mock.state.commands().len();
    let copies_before = count_copy_commands(&mock.state.commands());
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("retry pass");
    assert_eq!(outcome.done, 0);
    assert_eq!(outcome.retried, 1);
    let after: Vec<String> = mock
        .state
        .commands()
        .into_iter()
        .skip(before)
        .collect();
    assert_eq!(
        count_copy_commands(&mock.state.commands()),
        copies_before,
        "an unknown copy must not be re-sent"
    );
    assert_eq!(count_commands(&after, "UID STORE"), 0, "{after:?}");
    assert_eq!(count_commands(&after, "UID EXPUNGE"), 0, "{after:?}");
    assert_eq!(mock.state.message_count("INBOX"), 1);
    assert_eq!(mock.state.message_count("Archive"), 1);

    let payload = move_op_payload(&ctx, &op.id).await;
    let recorded = payload
        .outcomes
        .iter()
        .find(|outcome| outcome.message_id == message_id)
        .expect("outcome");
    assert_eq!(recorded.copy_phase, ops::CopyPhase::Unknown);
    assert_eq!(recorded.state, ops::OpMessageState::Unknown);
}

/// A historical `COPYUID` receipt proves the copy existed when it was
/// recorded, not that the target still holds it. When the destination copy
/// disappeared, resuming the source deletion would erase the only copy left.
#[tokio::test]
async fn disappeared_target_copy_does_not_authorize_source_delete() {
    let mock = MockImap::start_with_folders(
        &["UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Archive", &["\\Archive"])],
    )
    .await;
    let uid = mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Destination vanished",
            "body",
            "vanished@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    let uidvalidity = folder_uidvalidity(&ctx, &inbox).await;
    let message_id = message_ids(&ctx, &inbox).await.pop().expect("message");

    ops::enqueue_move(
        &ctx,
        "acc",
        &inbox,
        &archive,
        &[uid as i64],
        std::slice::from_ref(&message_id),
        uidvalidity,
        "move",
    )
    .await
    .expect("enqueue move");
    optimistic_move_pending(&ctx, std::slice::from_ref(&message_id), &archive).await;
    mock.state.set_copy_uid_plus(true);
    mock.state.set_fail_store(true);

    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first pass");
    assert_eq!(outcome.retried, 1);
    let op = queued_move_op(&ctx).await;
    let payload = move_op_payload(&ctx, &op.id).await;
    let recorded = payload
        .outcomes
        .iter()
        .find(|outcome| outcome.message_id == message_id)
        .expect("copy receipt");
    let target_uid = recorded.target_uid.expect("COPYUID destination") as u32;
    assert_eq!(recorded.copy_phase, ops::CopyPhase::Copied);

    // The copy is gone (moved away by another client, expunged, ...).
    mock.state.remove_message("Archive", target_uid);
    mock.state.set_fail_store(false);
    make_op_due(&ctx, &op.id).await;

    let before = mock.state.commands().len();
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("retry pass");
    assert_eq!(outcome.done, 0);
    assert_eq!(outcome.retried, 1);
    let after: Vec<String> = mock
        .state
        .commands()
        .into_iter()
        .skip(before)
        .collect();
    assert_eq!(count_commands(&after, "UID STORE"), 0, "{after:?}");
    assert_eq!(count_commands(&after, "UID EXPUNGE"), 0, "{after:?}");
    assert!(
        after
            .iter()
            .any(|command| command.to_ascii_uppercase().starts_with("UID SEARCH")),
        "the destination must be verified with UID SEARCH: {after:?}"
    );
    assert_eq!(mock.state.message_count("INBOX"), 1);
    assert_eq!(mock.state.message_count("Archive"), 0);

    let payload = move_op_payload(&ctx, &op.id).await;
    let recorded = payload
        .outcomes
        .iter()
        .find(|outcome| outcome.message_id == message_id)
        .expect("outcome");
    assert_eq!(recorded.state, ops::OpMessageState::Unknown);
    assert_eq!(recorded.copy_phase, ops::CopyPhase::Unknown);
}

/// A COPY acknowledged without `COPYUID` proves the command ran but not which
/// destination UID it created. When the real copy is removed and an unrelated
/// message with the same Message-ID takes its place, the target sync binds
/// that message to the projection. The retry must not treat the binding as a
/// receipt: it may neither delete the source nor record a confirmed undo.
#[tokio::test]
async fn resumed_copy_without_copyuid_does_not_trust_message_id_binding() {
    let mock = MockImap::start_with_folders(
        &["UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Archive", &["\\Archive"])],
    )
    .await;
    let uid = mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Source copy",
            "the source body",
            "resume-duplicate@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    let uidvalidity = folder_uidvalidity(&ctx, &inbox).await;
    let message_id = message_ids(&ctx, &inbox).await.pop().expect("message");

    ops::enqueue_move(
        &ctx,
        "acc",
        &inbox,
        &archive,
        &[uid as i64],
        std::slice::from_ref(&message_id),
        uidvalidity,
        "move",
    )
    .await
    .expect("enqueue move");
    optimistic_move_pending(&ctx, std::slice::from_ref(&message_id), &archive).await;
    // The server acknowledges UID COPY without a destination UID. The COPY is
    // confirmed, but its target identity is not.
    mock.state.set_copy_uid_plus(false);
    mock.state.set_fail_store(true);

    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first pass");
    assert_eq!(outcome.retried, 1);
    assert_eq!(outcome.done, 0);
    assert_eq!(mock.state.message_count("Archive"), 1);
    assert_eq!(mock.state.message_count("INBOX"), 1);

    let op = queued_move_op(&ctx).await;
    let payload = move_op_payload(&ctx, &op.id).await;
    let recorded = payload
        .outcomes
        .iter()
        .find(|outcome| outcome.message_id == message_id)
        .expect("copy outcome");
    assert_eq!(recorded.copy_phase, ops::CopyPhase::Copied);
    assert_eq!(
        recorded.target_uid, None,
        "without COPYUID the destination UID must stay unknown"
    );

    // The real copy disappears and an unrelated message with the same header
    // (different body) takes its place in the target folder.
    mock.state.remove_message("Archive", 1);
    mock.state.add_message(
        "Archive",
        &message_raw(
            "b@example.com",
            "test@example.com",
            "Unrelated",
            "a different body",
            "resume-duplicate@x",
        ),
    );
    sync_account(&ctx, &account).await;
    {
        let stored = ctx
            .db
            .read({
                let id = message_id.clone();
                move |db| repo::messages::get(db, &id)
            })
            .await
            .expect("read projection")
            .expect("projection row");
        assert_eq!(stored.summary.folder_id, archive);
        assert!(stored.uid.is_some(), "the target sync must bind a UID");
    }
    mock.state.set_fail_store(false);
    make_op_due(&ctx, &op.id).await;

    let before = mock.state.commands().len();
    let copies_before = count_copy_commands(&mock.state.commands());
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("retry pass");
    assert_eq!(outcome.done, 0, "a Message-ID binding is not a receipt");
    assert_eq!(outcome.retried, 1);
    let after: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert_eq!(
        count_copy_commands(&mock.state.commands()),
        copies_before,
        "a confirmed COPY must not be re-sent"
    );
    assert_eq!(count_commands(&after, "UID STORE"), 0, "{after:?}");
    assert_eq!(count_commands(&after, "UID EXPUNGE"), 0, "{after:?}");
    assert_eq!(mock.state.message_count("INBOX"), 1);
    assert_eq!(mock.state.message_count("Archive"), 1);

    let payload = move_op_payload(&ctx, &op.id).await;
    let recorded = payload
        .outcomes
        .iter()
        .find(|outcome| outcome.message_id == message_id)
        .expect("outcome");
    assert_eq!(recorded.state, ops::OpMessageState::Unknown);
    assert_eq!(recorded.target_uid, None);
    assert_ne!(recorded.expunge_phase, ops::ExpungePhase::Applied);
}

/// A soft delete that already copied the message but failed to flag the source
/// must finish in the Trash folder pinned by that attempt. When the target copy
/// is removed and the folder loses its `\Trash` role, the retry may neither
/// address a different role folder nor fall through to a permanent delete: the
/// operation stays incomplete for reconciliation.
#[tokio::test]
async fn soft_delete_retry_does_not_upgrade_when_pinned_trash_disappears() {
    let mock = MockImap::start_with_folders(
        &["UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Trash", &["\\Trash"])],
    )
    .await;
    mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Soft delete",
            "body",
            "soft-delete@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let trash = folder_id(&ctx, "acc", "Trash").await;
    let message_id = message_ids(&ctx, &inbox).await.pop().expect("message");

    // Queue through the service: the optimistic projection moves the row into
    // Trash and the payload pins that folder.
    let service = nuncio_mail::MailService::new(ctx.clone());
    service
        .message_act(
            std::slice::from_ref(&message_id),
            &nuncio_core::models::MessageAction::Delete { permanent: false },
        )
        .await
        .expect("queue soft delete");

    let delete_op = |ctx: CoreContext| async move {
        ctx.db
            .read(|db| repo::pending_ops::list(db, Some("acc")))
            .await
            .expect("ops")
            .into_iter()
            .find(|op| op.kind == ops::KIND_DELETE)
            .expect("delete op")
    };
    let op = delete_op(ctx.clone()).await;
    let payload: ops::DeleteOp = repo::pending_ops::payload(&op).expect("delete payload");
    assert_eq!(payload.target_folder_id.as_deref(), Some(trash.as_str()));
    assert!(!payload.permanent);

    mock.state.set_copy_uid_plus(true);
    mock.state.set_fail_store(true);
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first pass");
    assert_eq!(outcome.retried, 1);
    assert_eq!(outcome.done, 0);
    assert_eq!(mock.state.message_count("Trash"), 1);
    assert_eq!(mock.state.message_count("INBOX"), 1);

    let progressed = delete_op(ctx.clone()).await;
    let payload: ops::DeleteOp = repo::pending_ops::payload(&progressed).expect("delete payload");
    let recorded = payload
        .outcomes
        .iter()
        .find(|outcome| outcome.message_id == message_id)
        .expect("copy receipt");
    assert_eq!(recorded.copy_phase, ops::CopyPhase::Copied);
    let target_uid = recorded.target_uid.expect("COPYUID destination") as u32;

    // The target copy is gone and the folder no longer has the Trash role.
    mock.state.remove_message("Trash", target_uid);
    {
        let trash_id = trash.clone();
        ctx.db
            .write(move |db| repo::folders::set_role(db, &trash_id, FolderRole::Other))
            .await
            .expect("clear Trash role");
    }
    mock.state.set_fail_store(false);
    make_op_due(&ctx, &op.id).await;

    let before = mock.state.commands().len();
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("retry pass");
    assert_eq!(outcome.done, 0, "the delete must stay incomplete");
    assert_eq!(outcome.failed, 0);
    assert_eq!(outcome.retried, 1);
    assert!(
        outcome.resync_folders.contains(&inbox),
        "the source folder must be reconciled"
    );
    assert!(
        outcome.resync_folders.contains(&trash),
        "the pinned target must be reconciled"
    );
    let after: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert_eq!(count_commands(&after, "UID STORE"), 0, "{after:?}");
    assert_eq!(count_commands(&after, "UID EXPUNGE"), 0, "{after:?}");
    assert_eq!(mock.state.message_count("INBOX"), 1);
    assert_eq!(mock.state.message_count("Trash"), 0);

    let stored = ctx
        .db
        .read({
            let id = message_id.clone();
            move |db| repo::messages::get(db, &id)
        })
        .await
        .expect("read projection");
    let stored = stored.expect("the optimistic row must not be erased");
    assert_eq!(stored.summary.folder_id, trash);

    let current = ctx
        .db
        .read({
            let id = op.id.clone();
            move |db| repo::pending_ops::get(db, &id)
        })
        .await
        .expect("read op")
        .expect("delete op row");
    assert_eq!(current.state, nuncio_core::models::PendingOpState::Pending);

    let payload: ops::DeleteOp = repo::pending_ops::payload(&current).expect("delete payload");
    let recorded = payload
        .outcomes
        .iter()
        .find(|outcome| outcome.message_id == message_id)
        .expect("outcome");
    assert_ne!(
        recorded.state,
        ops::OpMessageState::Confirmed,
        "no permanent delete receipt may be recorded"
    );
    assert_ne!(
        recorded.expunge_phase,
        ops::ExpungePhase::Applied,
        "the source must never be expunged by this retry"
    );
}

/// A soft delete queued before the first attempt must stay soft even if a
/// folder sync re-labels the source folder as Trash in the meantime. The
/// payload's `permanent: false` is authoritative: re-deriving "already in
/// Trash" from the source folder's runtime role must never upgrade the op to a
/// permanent delete. Because the same sync demotes the pinned destination, the
/// executor has to leave the source copy intact and keep the op pending for
/// reconciliation.
#[tokio::test]
async fn soft_delete_queued_before_source_becomes_trash_never_goes_permanent() {
    let mock = MockImap::start_with_folders(
        &["UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Trash", &["\\Trash"])],
    )
    .await;
    mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Re-roled source",
            "body",
            "re-roled@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let trash = folder_id(&ctx, "acc", "Trash").await;
    let message_id = message_ids(&ctx, &inbox).await.pop().expect("message");

    // Queue through the service: the deletion type is fixed as soft and the
    // optimistic projection moves the row into the pinned Trash folder.
    let service = nuncio_mail::MailService::new(ctx.clone());
    service
        .message_act(
            std::slice::from_ref(&message_id),
            &nuncio_core::models::MessageAction::Delete { permanent: false },
        )
        .await
        .expect("queue soft delete");

    let op = ctx
        .db
        .read(|db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops")
        .into_iter()
        .find(|op| op.kind == ops::KIND_DELETE)
        .expect("delete op");
    let payload: ops::DeleteOp = repo::pending_ops::payload(&op).expect("delete payload");
    assert!(!payload.permanent);
    assert_eq!(payload.target_folder_id.as_deref(), Some(trash.as_str()));
    assert!(
        payload.outcomes.is_empty(),
        "the op must not have executed yet"
    );

    // Before the first attempt the server re-labels INBOX as \Trash, and a
    // real folder sync applies that role locally. Both folders now claim
    // \Trash, so the first declaration wins and the pinned target is demoted.
    {
        let mut folders = mock.state.folders.lock().expect("mock folders lock");
        let source = folders
            .iter_mut()
            .find(|folder| folder.name.eq_ignore_ascii_case("INBOX"))
            .expect("INBOX");
        source.attributes = vec!["\\Trash".into()];
    }
    let mut connection = connect_account(&account).await;
    nuncio_mail::sync::folders::sync(&ctx, &mut connection, "acc")
        .await
        .expect("folder sync");
    let role_of = |ctx: CoreContext, id: String| async move {
        ctx.db
            .read(move |db| repo::folders::get(db, &id))
            .await
            .expect("folder")
            .expect("folder row")
            .role
    };
    assert_eq!(role_of(ctx.clone(), inbox.clone()).await, FolderRole::Trash);
    assert_eq!(role_of(ctx.clone(), trash.clone()).await, FolderRole::Other);
    let payload: ops::DeleteOp = repo::pending_ops::payload(&op).expect("delete payload");
    assert!(
        payload.outcomes.is_empty(),
        "no attempt may have been recorded before execution"
    );

    let before = mock.state.commands().len();
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("run due");
    assert_eq!(outcome.done, 0, "the delete must stay incomplete");
    assert_eq!(outcome.failed, 0);
    assert_eq!(outcome.retried, 1);
    assert!(
        outcome.resync_folders.contains(&inbox),
        "the source folder must be reconciled"
    );
    assert!(
        outcome.resync_folders.contains(&trash),
        "the pinned target must be reconciled"
    );

    let after: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert_eq!(count_commands(&after, "UID STORE"), 0, "{after:?}");
    assert_eq!(count_commands(&after, "UID EXPUNGE"), 0, "{after:?}");
    assert_eq!(
        mock.state.message_count("INBOX"),
        1,
        "the source copy must survive"
    );

    let stored = ctx
        .db
        .read({
            let id = message_id.clone();
            move |db| repo::messages::get(db, &id)
        })
        .await
        .expect("read projection");
    assert!(
        stored.is_some(),
        "the optimistic row must not be erased by a permanent delete"
    );

    let current = ctx
        .db
        .read({
            let id = op.id.clone();
            move |db| repo::pending_ops::get(db, &id)
        })
        .await
        .expect("read op")
        .expect("delete op row");
    assert_eq!(
        current.state,
        nuncio_core::models::PendingOpState::Pending,
        "the op must remain pending for reconciliation"
    );
    let payload: ops::DeleteOp = repo::pending_ops::payload(&current).expect("delete payload");
    assert!(
        payload
            .outcomes
            .iter()
            .all(|outcome| outcome.expunge_phase != ops::ExpungePhase::Applied),
        "no permanent-delete receipt may be recorded"
    );
}

/// A rebuilt destination mailbox gives the same UID a different meaning. A
/// historical receipt from the old generation must not authorize a delete.
#[tokio::test]
async fn rebuilt_target_generation_does_not_authorize_source_delete() {
    let mock = MockImap::start_with_folders(
        &["UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Archive", &["\\Archive"])],
    )
    .await;
    let uid = mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Destination rebuilt",
            "body",
            "rebuilt@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    let uidvalidity = folder_uidvalidity(&ctx, &inbox).await;
    let message_id = message_ids(&ctx, &inbox).await.pop().expect("message");

    ops::enqueue_move(
        &ctx,
        "acc",
        &inbox,
        &archive,
        &[uid as i64],
        std::slice::from_ref(&message_id),
        uidvalidity,
        "move",
    )
    .await
    .expect("enqueue move");
    optimistic_move_pending(&ctx, std::slice::from_ref(&message_id), &archive).await;
    mock.state.set_copy_uid_plus(true);
    mock.state.set_fail_store(true);

    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first pass");
    assert_eq!(outcome.retried, 1);
    let op = queued_move_op(&ctx).await;

    mock.state.bump_uidvalidity("Archive", 2);
    mock.state.set_fail_store(false);
    make_op_due(&ctx, &op.id).await;

    let before = mock.state.commands().len();
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("retry pass");
    assert_eq!(outcome.done, 0);
    assert_eq!(outcome.retried, 1);
    let after: Vec<String> = mock
        .state
        .commands()
        .into_iter()
        .skip(before)
        .collect();
    assert_eq!(count_commands(&after, "UID STORE"), 0, "{after:?}");
    assert_eq!(count_commands(&after, "UID EXPUNGE"), 0, "{after:?}");
    assert_eq!(mock.state.message_count("INBOX"), 1);
    assert_eq!(mock.state.message_count("Archive"), 1);

    let payload = move_op_payload(&ctx, &op.id).await;
    let recorded = payload
        .outcomes
        .iter()
        .find(|outcome| outcome.message_id == message_id)
        .expect("outcome");
    assert_eq!(recorded.state, ops::OpMessageState::Unknown);
    assert_eq!(recorded.copy_phase, ops::CopyPhase::Unknown);
}

/// The first COPY pins its destination in the payload. When the account's
/// `\Archive` role moves to another folder, the retry must keep finishing in
/// the folder that actually received the copy and keep the receipt with it.
#[tokio::test]
async fn role_switch_keeps_copyuid_receipt_in_the_pinned_folder() {
    let mock = MockImap::start_with_folders(
        &["UIDPLUS"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("ArchiveA", &["\\Archive"]),
            ("ArchiveB", &["\\Other"]),
        ],
    )
    .await;
    let uid = mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Pinned destination",
            "body",
            "pinned@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive_a = folder_id(&ctx, "acc", "ArchiveA").await;
    let archive_b = folder_id(&ctx, "acc", "ArchiveB").await;
    let uidvalidity = folder_uidvalidity(&ctx, &inbox).await;
    let message_id = message_ids(&ctx, &inbox).await.pop().expect("message");

    ops::enqueue_archive(&ctx, "acc", &inbox, &[uid as i64], uidvalidity, "archive")
        .await
        .expect("enqueue archive");
    // The service supplies the stable local ids; the bare helper does not.
    let op = {
        let op = ctx
            .db
            .read(|db| repo::pending_ops::list(db, Some("acc")))
            .await
            .expect("ops")
            .into_iter()
            .find(|op| op.kind == ops::KIND_ARCHIVE)
            .expect("archive op");
        let mut payload: ops::ArchiveOp = repo::pending_ops::payload(&op).expect("payload");
        payload.message_ids = vec![message_id.clone()];
        let value = serde_json::to_value(&payload).expect("payload");
        let id = op.id.clone();
        ctx.db
            .write(move |db| repo::pending_ops::update_payload(db, &id, &value))
            .await
            .expect("align archive payload");
        op
    };
    mock.state.set_copy_uid_plus(true);
    mock.state.set_fail_store(true);

    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first pass");
    assert_eq!(outcome.retried, 1);
    let payload: ops::ArchiveOp = {
        let op = ctx
            .db
            .read({
                let id = op.id.clone();
                move |db| repo::pending_ops::get(db, &id)
            })
            .await
            .expect("op")
            .expect("archive op");
        repo::pending_ops::payload(&op).expect("payload")
    };
    let recorded = payload
        .outcomes
        .iter()
        .find(|outcome| outcome.message_id == message_id)
        .expect("copy receipt");
    assert_eq!(
        recorded.target_folder_id.as_deref(),
        Some(archive_a.as_str()),
        "the first attempt must pin the destination it copied into"
    );
    assert_eq!(recorded.target_uid, Some(1));

    // The role moves to another folder before the retry.
    {
        let (a, b) = (archive_a.clone(), archive_b.clone());
        ctx.db
            .write(move |db| {
                repo::folders::set_role(db, &a, FolderRole::Other)?;
                repo::folders::set_role(db, &b, FolderRole::Archive)
            })
            .await
            .expect("move archive role");
    }
    mock.state.set_fail_store(false);
    make_op_due(&ctx, &op.id).await;

    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("retry pass");
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.retried, 0);
    assert_eq!(mock.state.message_count("ArchiveA"), 1);
    assert_eq!(mock.state.message_count("ArchiveB"), 0);
    assert_eq!(mock.state.message_count("INBOX"), 0);

    let final_payload: ops::ArchiveOp = {
        let op = ctx
            .db
            .read({
                let id = op.id.clone();
                move |db| repo::pending_ops::get(db, &id)
            })
            .await
            .expect("op")
            .expect("archive op");
        repo::pending_ops::payload(&op).expect("payload")
    };
    let recorded = final_payload
        .outcomes
        .iter()
        .find(|outcome| outcome.message_id == message_id)
        .expect("outcome");
    assert_eq!(recorded.state, ops::OpMessageState::Confirmed);
    assert_eq!(
        recorded.target_folder_id.as_deref(),
        Some(archive_a.as_str()),
        "the receipt must stay with the folder that received the copy"
    );
    assert_eq!(recorded.target_uid, Some(1));
    assert_eq!(
        recorded.target_uidvalidity,
        folder_uidvalidity(&ctx, &archive_a).await
    );
}

/// `UID EXPUNGE` answering OK only proves the command was accepted. If another
/// client cleared `\Deleted` in the meantime, nothing is removed; the move must
/// stay incomplete rather than record a successful delete.
#[tokio::test]
async fn empty_expunge_does_not_complete_the_move() {
    let mock = MockImap::start_with_folders(
        &["UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Archive", &["\\Archive"])],
    )
    .await;
    let uid = mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Empty expunge",
            "body",
            "empty-expunge@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    let uidvalidity = folder_uidvalidity(&ctx, &inbox).await;
    let message_id = message_ids(&ctx, &inbox).await.pop().expect("message");

    ops::enqueue_move(
        &ctx,
        "acc",
        &inbox,
        &archive,
        &[uid as i64],
        std::slice::from_ref(&message_id),
        uidvalidity,
        "move",
    )
    .await
    .expect("enqueue move");
    optimistic_move_pending(&ctx, std::slice::from_ref(&message_id), &archive).await;
    mock.state.set_copy_uid_plus(true);
    mock.state.set_fail_expunge(true);

    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first pass");
    assert_eq!(outcome.retried, 1);
    let op = queued_move_op(&ctx).await;
    let payload = move_op_payload(&ctx, &op.id).await;
    let recorded = payload
        .outcomes
        .iter()
        .find(|outcome| outcome.message_id == message_id)
        .expect("outcome");
    assert_eq!(recorded.delete_flag_phase, ops::DeleteFlagPhase::Applied);
    assert_eq!(recorded.expunge_phase, ops::ExpungePhase::Unknown);

    // Another client clears the flag, and the retry's EXPUNGE removes nothing.
    mock.state.set_flags("INBOX", uid, &[]);
    mock.state.set_fail_expunge(false);
    make_op_due(&ctx, &op.id).await;

    let before = mock.state.commands().len();
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("retry pass");
    assert_eq!(outcome.done, 0);
    assert_eq!(outcome.retried, 1);
    let after: Vec<String> = mock
        .state
        .commands()
        .into_iter()
        .skip(before)
        .collect();
    assert_eq!(count_commands(&after, "UID STORE"), 0, "{after:?}");
    assert_eq!(count_commands(&after, "UID EXPUNGE"), 1, "{after:?}");
    assert_eq!(mock.state.message_count("INBOX"), 1);
    assert_eq!(mock.state.message_count("Archive"), 1);

    let payload = move_op_payload(&ctx, &op.id).await;
    let recorded = payload
        .outcomes
        .iter()
        .find(|outcome| outcome.message_id == message_id)
        .expect("outcome");
    assert_eq!(recorded.state, ops::OpMessageState::Unknown);
    assert_eq!(recorded.expunge_phase, ops::ExpungePhase::Unknown);
}

/// The IMAP worker selects only `pending` rows. An isolated legacy DELETE is
/// persisted as `cancelled` with the `quarantined` payload marker (the shape a
/// reader that does not know the retired `quarantined` state string must
/// ignore), so it may never reach the wire and the audit row stays.
#[tokio::test]
async fn run_due_never_executes_a_cancelled_quarantined_delete() {
    let mock = MockImap::start_with_folders(&["CONDSTORE"], &[("Drafts", &["\\Drafts"])]).await;
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
    ctx.db
        .write(|db| {
            repo::folders::insert(
                db,
                &Folder::new("drafts", "acc", "Drafts", FolderRole::Drafts),
            )
        })
        .await
        .expect("insert drafts folder");

    let op_id = {
        let op = NewPendingOp::new(
            "acc",
            ops::KIND_DELETE_REMOTE_DRAFT,
            serde_json::json!({
                "folderId": "drafts",
                "uid": 7,
                "quarantined": true,
                "quarantineReason": "legacy isolation",
            }),
            "Legacy remove remote draft",
        );
        ctx.db
            .write(move |db| repo::pending_ops::enqueue(db, op))
            .await
            .expect("enqueue legacy delete")
            .id
    };
    {
        let id = op_id.clone();
        ctx.db
            .write(move |db| repo::pending_ops::quarantine(db, &id))
            .await
            .expect("isolate legacy delete");
    }

    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("execute due ops");
    assert_eq!(outcome.done, 0);
    assert_eq!(outcome.failed, 0);
    assert!(
        !mock
            .state
            .commands()
            .iter()
            .any(|command| command.contains("\\Deleted") || command.contains("EXPUNGE")),
        "the isolated delete must not produce a server command: {:?}",
        mock.state.commands()
    );

    let op = {
        let id = op_id.clone();
        ctx.db
            .read(move |db| repo::pending_ops::get_required(db, &id))
            .await
            .expect("auditable row")
    };
    assert_eq!(op.state, nuncio_core::models::PendingOpState::Cancelled);
    assert_eq!(op.payload["quarantined"], true);
    assert_eq!(op.payload["quarantineReason"], "legacy isolation");
}

/// A queued UID is only meaningful in the generation it was built against.
/// When the source folder was rebuilt while the op was offline, the command
/// must not be sent at all: the same number may address another message.
#[tokio::test]
async fn stale_uidvalidity_skips_the_move_command() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "MOVE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Archive", &["\\Archive"])],
    )
    .await;
    let uid = mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Stale",
            "body",
            "stale@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    let wrong = Some(folder_uidvalidity(&ctx, &inbox).await.unwrap_or(0) + 1);

    ops::enqueue_move(
        &ctx,
        "acc",
        &inbox,
        &archive,
        &[uid as i64],
        &[],
        wrong,
        "stale move",
    )
    .await
    .expect("enqueue");
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("run due");
    assert_eq!(outcome.failed, 1, "the stale op must fail, not retry");
    assert_eq!(mock.state.message_count("INBOX"), 1);
    assert_eq!(
        mock.state.message_count("Archive"),
        0,
        "UID MOVE must not be sent with a stale UIDVALIDITY"
    );
}

/// Payloads queued before `source_uidvalidity` existed cannot prove their UIDs
/// belong to the current folder. They are skipped and reconciled instead of
/// being sent blind.
#[tokio::test]
async fn missing_uidvalidity_skips_the_move_command() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "MOVE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Archive", &["\\Archive"])],
    )
    .await;
    let uid = mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "No generation",
            "body",
            "nogeneration@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;

    ops::enqueue_move(
        &ctx,
        "acc",
        &inbox,
        &archive,
        &[uid as i64],
        &[],
        None,
        "legacy move",
    )
    .await
    .expect("enqueue");
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("run due");
    assert_eq!(outcome.failed, 1);
    assert_eq!(mock.state.message_count("INBOX"), 1);
    assert_eq!(mock.state.message_count("Archive"), 0);
}

/// A `folderDelete` SELECT used to bypass the executor's `SelectedFolder`
/// cache. When the DELETE failed, the session was left with the deleted
/// mailbox selected while the cache still described INBOX, so the next INBOX
/// UID command passed the generation check and was sent to the wrong mailbox.
#[tokio::test]
async fn failed_folder_delete_does_not_retarget_later_uid_commands() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "MOVE", "UIDPLUS"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Newsletter", &[]),
            ("Archive", &["\\Archive"]),
        ],
    )
    .await;
    let inbox_first = mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "First",
            "body",
            "first@x",
        ),
    );
    let inbox_second = mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Second",
            "body",
            "second@x",
        ),
    );
    // Same numeric UIDs as INBOX: the stale UID command would hit these.
    mock.state.add_message(
        "Newsletter",
        &message_raw(
            "n@example.com",
            "test@example.com",
            "Newsletter one",
            "body",
            "nl1@x",
        ),
    );
    let newsletter_second = mock.state.add_message(
        "Newsletter",
        &message_raw(
            "n@example.com",
            "test@example.com",
            "Newsletter two",
            "body",
            "nl2@x",
        ),
    );
    mock.state.fail_delete("Newsletter");

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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let newsletter = folder_id(&ctx, "acc", "Newsletter").await;
    let uidvalidity = folder_uidvalidity(&ctx, &inbox).await;

    // 1. Select INBOX and execute a flag command (populates the cache).
    ops::enqueue_set_flags(
        &ctx,
        "acc",
        &inbox,
        &[inbox_first as i64],
        Some(true),
        None,
        None,
        None,
        uidvalidity,
        "first",
    )
    .await
    .expect("enqueue first flags");
    // 2. Select Newsletter and fail to delete it.
    ops::enqueue_folder_delete(&ctx, "acc", &newsletter)
        .await
        .expect("enqueue folder delete");
    // 3. A UID command for INBOX with the same numeric UID as a Newsletter
    //    message. It must be re-addressed to INBOX, not sent to Newsletter.
    ops::enqueue_set_flags(
        &ctx,
        "acc",
        &inbox,
        &[inbox_second as i64],
        None,
        Some(true),
        None,
        None,
        uidvalidity,
        "second",
    )
    .await
    .expect("enqueue second flags");

    // `created_at` has second precision; pin the order of the batch by its
    // explicit insertion sequence.
    let op_ids: Vec<String> = ctx
        .db
        .read(|db| {
            let mut statement = db.prepare("SELECT id FROM pending_op ORDER BY seq ASC")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
        .expect("ops");
    assert_eq!(op_ids.len(), 3);
    ctx.db
        .write(move |db| {
            for (index, id) in op_ids.iter().enumerate() {
                db.execute(
                    "UPDATE pending_op SET created_at = ?2 WHERE id = ?1",
                    rusqlite::params![id, format!("2026-01-01T00:00:0{}Z", index + 1)],
                )?;
            }
            Ok(())
        })
        .await
        .expect("pin op order");

    let mut connection = connect_account(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("run due");

    let inbox_flags = mock.state.message_flags("INBOX", inbox_second);
    assert!(
        inbox_flags
            .iter()
            .any(|flag| flag.eq_ignore_ascii_case("\\Flagged")),
        "the INBOX message must be flagged"
    );
    let newsletter_flags = mock.state.message_flags("Newsletter", newsletter_second);
    assert!(
        !newsletter_flags
            .iter()
            .any(|flag| flag.eq_ignore_ascii_case("\\Flagged")),
        "the UID command must not be sent to the folder left selected by the failed DELETE"
    );

    let commands = mock.state.commands();
    let delete_at = commands
        .iter()
        .position(|command| command.to_ascii_uppercase().starts_with("DELETE "))
        .expect("folder delete command");
    assert!(
        commands[delete_at..]
            .iter()
            .any(|command| command.eq_ignore_ascii_case("SELECT INBOX")),
        "the executor must SELECT INBOX again after the selection cache was invalidated"
    );
}

#[tokio::test]
async fn executes_archive_via_move_fallback() {
    // No MOVE capability: COPY + STORE \Deleted + UID EXPUNGE.
    let mock = MockImap::start_with_folders(
        &["UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Archive", &["\\Archive"])],
    )
    .await;
    let uid = mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Keep",
            "body",
            "keep@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let uidvalidity = folder_uidvalidity(&ctx, &inbox).await;

    ops::enqueue_archive(&ctx, "acc", &inbox, &[uid as i64], uidvalidity, "archive")
        .await
        .expect("enqueue archive");
    let mut connection = connect_account(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("execute archive");
    assert_eq!(mock.state.message_count("INBOX"), 0);
    assert_eq!(mock.state.message_count("Archive"), 1);
}

/// `UID MOVE` copies and removes in one server-side step, so it does not need
/// UIDPLUS (nor a follow-up UID EXPUNGE).
#[tokio::test]
async fn move_without_uidplus_is_still_allowed() {
    let mock = MockImap::start_with_folders(
        &["MOVE"],
        &[("INBOX", &["\\Inbox"]), ("Archive", &["\\Archive"])],
    )
    .await;
    let uid = mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Move",
            "body",
            "move-no-uidplus@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let uidvalidity = folder_uidvalidity(&ctx, &inbox).await;

    ops::enqueue_archive(&ctx, "acc", &inbox, &[uid as i64], uidvalidity, "archive")
        .await
        .expect("enqueue archive");
    let before = mock.state.commands().len();
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("execute archive");
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.failed, 0);

    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        commands
            .iter()
            .any(|command| command.to_ascii_uppercase().starts_with("UID MOVE")),
        "the server must perform the move itself: {commands:?}"
    );
    assert!(
        !commands.iter().any(|command| {
            let command = command.trim().to_ascii_uppercase();
            command.starts_with("UID COPY")
                || command == "EXPUNGE"
                || command.starts_with("UID EXPUNGE")
        }),
        "UID MOVE needs no COPY/EXPUNGE follow-up: {commands:?}"
    );
    assert_eq!(mock.state.message_count("INBOX"), 0);
    assert_eq!(mock.state.message_count("Archive"), 1);
}

/// Without MOVE the fallback is COPY + STORE `\Deleted` + UID EXPUNGE. On a
/// server with no directed expunge that fallback cannot finish, so the move
/// must be refused before the COPY: otherwise Archive keeps a second copy that
/// no later command can safely remove.
#[tokio::test]
async fn archive_without_move_or_directed_expunge_is_refused_before_copy() {
    // No Archive folder: without the early guard the executor would CREATE
    // one and only then find out that the move cannot finish.
    let mock = MockImap::start_with_folders(&["CONDSTORE"], &[("INBOX", &["\\Inbox"])]).await;
    let uid = mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Keep",
            "body",
            "keep-no-uidplus@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let uidvalidity = folder_uidvalidity(&ctx, &inbox).await;

    ops::enqueue_archive(&ctx, "acc", &inbox, &[uid as i64], uidvalidity, "archive")
        .await
        .expect("enqueue archive");
    let before = mock.state.commands().len();
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("execute archive");
    assert_eq!(outcome.done, 0);
    assert_eq!(outcome.failed, 1);
    assert_eq!(outcome.retried, 0, "a capability refusal must not retry");

    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    for forbidden in ["CREATE", "UID COPY", "UID MOVE", "UID STORE", "EXPUNGE"] {
        assert!(
            !commands.iter().any(|command| {
                let command = command.trim().to_ascii_uppercase();
                command == forbidden || command.starts_with(&format!("{forbidden} "))
            }),
            "a refused move must not send {forbidden}: {commands:?}"
        );
    }
    assert_eq!(mock.state.message_count("Archive"), 0);
    assert_eq!(mock.state.message_count("INBOX"), 1);
    let flags = mock.state.message_flags("INBOX", uid);
    assert!(
        !flags
            .iter()
            .any(|flag| flag.eq_ignore_ascii_case("\\Deleted")),
        "a refused archive must not flag the source: {flags:?}"
    );
}

#[tokio::test]
async fn smtp_outbox_sends_and_marks_sent() {
    let smtp_mock = MockSmtp::start().await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        None,
        Some(server(smtp_mock.addr.port())),
        None,
    );
    insert_account(&ctx, &account).await;

    let raw = message_raw(
        "test@example.com",
        "bob@example.com",
        "Outbox hello",
        "hello from outbox",
        "outbox@x",
    );
    let path = ctx.paths.staging_dir().join("outbox-test.eml");
    ctx.paths.ensure_parent(&path).expect("staging dir");
    std::fs::write(&path, &raw).expect("write staged");
    let item = OutboxItem::new(
        "acc",
        None,
        vec![nuncio_core::models::Address::new("bob@example.com")],
        "Outbox hello",
        path.to_string_lossy().to_string(),
    );
    let item = ctx
        .db
        .write({
            let item = item.clone();
            move |db| repo::outbox::enqueue(db, &item)
        })
        .await
        .expect("enqueue outbox");

    smtp::outbox::process_one(&ctx, &account, &item.id)
        .await
        .expect("process outbox");
    let stored = ctx
        .db
        .read({
            let id = item.id.clone();
            move |db| repo::outbox::get_required(db, &id)
        })
        .await
        .expect("outbox item");
    assert_eq!(stored.state, nuncio_core::models::OutboxState::Sent);

    let messages = smtp_mock.messages.lock().expect("messages");
    assert_eq!(messages.len(), 1);
    assert!(String::from_utf8_lossy(&messages[0]).contains("Outbox hello"));
}

#[tokio::test]
async fn pop3_uidl_diff_downloads_new_and_removes_deleted() {
    let mock = MockPop3::start().await;
    mock.add(
        "uid-a",
        &message_raw("a@example.com", "test@example.com", "Pop A", "a", "pop-a@x"),
    );
    mock.add(
        "uid-b",
        &message_raw("b@example.com", "test@example.com", "Pop B", "b", "pop-b@x"),
    );
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        None,
        None,
        Some(server(mock.addr.port())),
    );
    insert_account(&ctx, &account).await;

    nuncio_mail::pop3::sync::sync_account(&ctx, &account)
        .await
        .expect("first pop3 sync");
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    assert_eq!(message_count(&ctx, &inbox).await, 2);

    mock.remove("uid-a");
    mock.add(
        "uid-c",
        &message_raw("c@example.com", "test@example.com", "Pop C", "c", "pop-c@x"),
    );
    nuncio_mail::pop3::sync::sync_account(&ctx, &account)
        .await
        .expect("second pop3 sync");
    assert_eq!(message_count(&ctx, &inbox).await, 2);
    let subjects = subjects(&ctx, &inbox).await;
    assert!(!subjects.contains(&"Pop A".to_string()));
    assert!(subjects.contains(&"Pop B".to_string()));
    assert!(subjects.contains(&"Pop C".to_string()));
}

/// Listing downloads headers only (`TOP`), never a body.
#[tokio::test]
async fn pop3_sync_stores_headers_without_fetching_bodies() {
    let mock = MockPop3::start().await;
    mock.add(
        "uid-a",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Pop A",
            "body a",
            "pop-a@x",
        ),
    );
    mock.add(
        "uid-b",
        &message_raw(
            "b@example.com",
            "test@example.com",
            "Pop B",
            "body b",
            "pop-b@x",
        ),
    );
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        None,
        None,
        Some(server(mock.addr.port())),
    );
    insert_account(&ctx, &account).await;

    nuncio_mail::pop3::sync::sync_account(&ctx, &account)
        .await
        .expect("pop3 sync");

    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    assert_eq!(message_count(&ctx, &inbox).await, 2);
    assert_eq!(mock.counters.top(), 2);
    assert_eq!(mock.counters.retr(), 0, "listing must not download bodies");

    // Rows are listable in date order and carry headers but no body.
    let subjects = subjects(&ctx, &inbox).await;
    assert!(subjects.contains(&"Pop A".to_string()));
    assert!(subjects.contains(&"Pop B".to_string()));
    let folder = inbox.clone();
    let stored = ctx
        .db
        .read(move |db| {
            let mut statement =
                db.prepare("SELECT body_path, preview, subject FROM message WHERE folder_id = ?1")?;
            let mut rows = statement.query([folder])?;
            let mut result = Vec::new();
            while let Some(row) = rows.next()? {
                result.push((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ));
            }
            Ok(result)
        })
        .await
        .expect("rows");
    assert_eq!(stored.len(), 2);
    for (body_path, preview, subject) in stored {
        assert!(body_path.is_none(), "{subject} must not have a cached body");
        assert!(preview.is_empty(), "{subject} has no body text yet");
    }
}

/// A server without TOP still lists, by falling back to a size-guarded RETR.
#[tokio::test]
async fn pop3_sync_falls_back_to_retr_when_top_is_rejected() {
    let mock = MockPop3::without_top().await;
    mock.add(
        "uid-a",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Pop A",
            "fallback body",
            "pop-a@x",
        ),
    );
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        None,
        None,
        Some(server(mock.addr.port())),
    );
    insert_account(&ctx, &account).await;

    nuncio_mail::pop3::sync::sync_account(&ctx, &account)
        .await
        .expect("pop3 sync with RETR fallback");

    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    assert_eq!(message_count(&ctx, &inbox).await, 1);
    assert_eq!(mock.counters.retr(), 1);
    // The fallback downloaded the body, so it is cached from the start.
    let id = message_ids(&ctx, &inbox).await.pop().expect("message id");
    let path = ctx
        .db
        .read(move |db| repo::messages::get(db, &id))
        .await
        .expect("stored")
        .expect("row")
        .body_path;
    assert!(
        path.is_some(),
        "RETR fallback caches the body it downloaded"
    );
}

/// Opening a message downloads its body once and caches it.
#[tokio::test]
async fn pop3_body_is_fetched_on_demand_and_cached() {
    let mock = MockPop3::start().await;
    mock.add(
        "uid-a",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Body on demand",
            "unique body text",
            "pop-body@x",
        ),
    );
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        None,
        None,
        Some(server(mock.addr.port())),
    );
    insert_account(&ctx, &account).await;
    nuncio_mail::pop3::sync::sync_account(&ctx, &account)
        .await
        .expect("pop3 sync");

    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let id = message_ids(&ctx, &inbox).await.pop().expect("message id");
    assert_eq!(mock.counters.retr(), 0);

    let first = nuncio_mail::pop3::body::fetch_body(&ctx, &account, &id)
        .await
        .expect("body download");
    let raw = tokio::fs::read(&first).await.expect("cached body");
    assert!(String::from_utf8_lossy(&raw).contains("unique body text"));
    assert_eq!(mock.counters.retr(), 1);

    // Second call is served from disk: no new RETR.
    let second = nuncio_mail::pop3::body::fetch_body(&ctx, &account, &id)
        .await
        .expect("cached body");
    assert_eq!(first, second);
    assert_eq!(mock.counters.retr(), 1);

    // The parsed body text and preview are now indexed.
    let stored = ctx
        .db
        .read(move |db| repo::messages::get(db, &id))
        .await
        .expect("stored")
        .expect("row");
    assert!(stored.body_path.is_some());
    assert!(!stored.summary.preview.is_empty());
}

/// A body that can no longer be downloaded (already DELETEd on the server)
/// reports a clear not-found error instead of retrying or panicking.
#[tokio::test]
async fn pop3_body_of_a_deleted_message_fails_cleanly() {
    let mock = MockPop3::start().await;
    mock.add(
        "uid-a",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Gone",
            "gone body",
            "pop-gone@x",
        ),
    );
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        None,
        None,
        Some(server(mock.addr.port())),
    );
    insert_account(&ctx, &account).await;
    nuncio_mail::pop3::sync::sync_account(&ctx, &account)
        .await
        .expect("pop3 sync");

    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let id = message_ids(&ctx, &inbox).await.pop().expect("message id");

    // Another client removes the message; the header row is still listable.
    mock.remove("uid-a");
    let error = nuncio_mail::pop3::body::fetch_body(&ctx, &account, &id)
        .await
        .expect_err("body is gone");
    assert_eq!(error.code(), "notFound");
    assert!(error.message().contains("no longer available"));

    // The miss is remembered, so reopening does not reconnect to the server.
    let uidls_before = mock.counters.retr();
    let again = nuncio_mail::pop3::body::fetch_body(&ctx, &account, &id)
        .await
        .expect_err("still gone");
    assert_eq!(again.code(), "notFound");
    assert_eq!(mock.counters.retr(), uidls_before);

    // The row survives: only its body is unavailable.
    let stored = ctx
        .db
        .read(move |db| repo::messages::get(db, &id))
        .await
        .expect("stored")
        .expect("row");
    assert!(stored.body_path.is_none());
}

/// leave-on-server = false deletes as soon as the header row is stored, and
/// the local row stays listable afterwards.
#[tokio::test]
async fn pop3_header_store_is_enough_for_server_deletion() {
    let mock = MockPop3::start().await;
    mock.add(
        "uid-a",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Spam",
            "spam body",
            "pop-spam@x",
        ),
    );
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        None,
        None,
        Some(server(mock.addr.port())),
    );
    insert_account(&ctx, &account).await;
    let account_id = account.id.clone();
    ctx.db
        .write(move |db| repo::sync_state::set(db, &account_id, "pop3", "leave_on_server", "false"))
        .await
        .expect("leave_on_server");

    nuncio_mail::pop3::sync::sync_account(&ctx, &account)
        .await
        .expect("pop3 sync");

    assert_eq!(mock.counters.dele(), 1, "deleted once its row was stored");
    assert_eq!(mock.counters.retr(), 0, "deletion does not need the body");
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    assert_eq!(message_count(&ctx, &inbox).await, 1);
    // The server copy is gone (QUIT applied the delete).
    assert!(mock.messages.lock().expect("messages").is_empty());
}

#[tokio::test]
async fn engine_starts_syncs_and_fetches_bodies() {
    let mock = MockImap::start(&["IDLE", "CONDSTORE", "MOVE", "UIDPLUS"]).await;
    mock.state.add_message(
        "INBOX",
        &message_raw(
            "alice@example.com",
            "test@example.com",
            "Engine test",
            "engine body text",
            "engine@x",
        ),
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

    let engine = Engine::new(ctx.clone());
    engine.start().await.expect("engine start");
    engine
        .sync_now(Some("acc"), None)
        .await
        .expect("engine sync");

    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let query = MessageListQuery::new(MailScope::folder(inbox))
        .with_conversation(false)
        .with_limit(10);
    let page = ctx
        .db
        .read(move |db| repo::messages::list(db, &query))
        .await
        .expect("page");
    let message = page.items.first().expect("message").clone();

    // Force a fresh body download by clearing the prefetched cache row.
    let message_id = message.id.clone();
    ctx.db
        .write(move |db| repo::messages::set_body(db, &message_id, None, None, None))
        .await
        .expect("clear body");
    let path = engine.fetch_body(&message.id).await.expect("fetch body");
    assert!(path.exists());
    let content = std::fs::read_to_string(&path).expect("read body");
    assert!(content.contains("engine body text"));

    engine.shutdown().await;
}

#[tokio::test]
async fn engine_reports_folder_roles_for_gmail_style_names() {
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let folders = [
        ("[Gmail]/All Mail", "all"),
        ("[Gmail]/Important", "important"),
        ("[Gmail]/Sent Mail", "sent"),
        ("[Gmail]/Starred", "flagged"),
        ("已发送", "sent"),
        ("垃圾邮件", "junk"),
    ];
    for (name, expected) in folders {
        let role = nuncio_mail::folder_role_for(name, Some("/"), &[]);
        assert_eq!(role.as_str(), expected, "{name}");
    }
    let _ = ctx;
}

// Keep imported items used on all platforms.
const _: Option<PathBuf> = None;
const _: Option<ProviderKind> = None;

#[tokio::test]
async fn permanent_operation_failure_emits_op_failed() {
    let mock = MockImap::start(&["CONDSTORE"]).await;
    mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Rollback",
            "body",
            "rollback@x",
        ),
    );
    let (ctx, sink) = support::context_with_sink(
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;

    ops::enqueue_move(
        &ctx,
        "acc",
        &inbox,
        "does-not-exist",
        &[1],
        &[],
        folder_uidvalidity(&ctx, &inbox).await,
        "bad move",
    )
    .await
    .expect("enqueue");
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("run due");
    assert_eq!(outcome.failed, 1);

    let op = ctx
        .db
        .read(move |db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops")
        .into_iter()
        .next()
        .expect("op");
    assert_eq!(op.state, nuncio_core::models::PendingOpState::Failed);
    assert!(sink
        .events()
        .iter()
        .any(|(event, _)| event == "op://failed"));
}

/// A permanent delete that the server confirms must erase the row for real:
/// the optimistic phase only hid it, so the attachment records and the FTS
/// document would otherwise leak forever.
#[tokio::test]
async fn confirmed_permanent_delete_erases_row_attachments_and_fts() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "MOVE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"])],
    )
    .await;
    mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Delete for real",
            "body",
            "delete-real@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let uidvalidity = folder_uidvalidity(&ctx, &inbox).await;

    let message = {
        let folder = inbox.clone();
        let ids = ctx
            .db
            .read(move |db| repo::messages::ids_in_folder(db, &folder))
            .await
            .expect("ids");
        let id = ids.into_iter().next().expect("synced message");
        let id_for_read = id.clone();
        ctx.db
            .read(move |db| repo::messages::get_required(db, &id_for_read))
            .await
            .expect("message")
    };
    let id = message.summary.id.clone();
    let uid = message.uid.expect("server UID");

    {
        let id = id.clone();
        ctx.db
            .write(move |db| {
                let mut attachment = nuncio_core::models::Attachment {
                    id: String::new(),
                    message_id: id.clone(),
                    filename: "report.pdf".into(),
                    mime_type: "application/pdf".into(),
                    size: 42,
                    content_id: None,
                    is_inline: false,
                    is_downloaded: true,
                };
                repo::attachments::insert(db, &mut attachment)?;
                repo::messages::set_body(db, &id, None, Some("unique delete token"), None)?;
                // The optimistic phase hides the row; the executor erases it
                // only after the server acknowledges the command.
                repo::messages::hide_for_permanent_delete(db, std::slice::from_ref(&id))?;
                Ok(())
            })
            .await
            .expect("prepare permanent delete");
    }

    let payload = ops::DeleteOp {
        folder_id: inbox.clone(),
        uids: vec![uid],
        permanent: true,
        target_folder_id: None,
        soft_hidden: false,
        message_ids: vec![id.clone()],
        source_uidvalidity: uidvalidity,
        pending_delete: true,
        deleted_flag_set: false,
        snapshot: Vec::new(),
        outcomes: Vec::new(),
        remote_ids: Vec::new(),
    };
    ctx.db
        .write({
            let payload = serde_json::to_value(payload).expect("payload");
            move |db| {
                repo::pending_ops::enqueue(
                    db,
                    nuncio_core::models::NewPendingOp::new(
                        "acc",
                        ops::KIND_DELETE,
                        payload,
                        "Delete message permanently",
                    ),
                )
            }
        })
        .await
        .expect("enqueue");

    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("run due");
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.failed, 0);
    assert!(
        !mock
            .state
            .commands()
            .iter()
            .any(|command| command.trim().eq_ignore_ascii_case("EXPUNGE")),
        "a UIDPLUS server must never receive a whole-folder EXPUNGE"
    );
    let uid_text = uid.to_string();
    assert!(
        mock.state.commands().iter().any(|command| {
            let command = command.to_ascii_uppercase();
            command.starts_with("UID EXPUNGE") && command.contains(&uid_text)
        }),
        "the selected UID must be expunged explicitly: {:?}",
        mock.state.commands()
    );
    assert_eq!(
        mock.state.message_count("INBOX"),
        0,
        "the server must have removed the message"
    );

    let id_for_read = id.clone();
    let gone = ctx
        .db
        .read(move |db| repo::messages::get(db, &id_for_read))
        .await
        .expect("read");
    assert!(gone.is_none(), "the confirmed delete must erase the row");

    let id_for_attachments = id.clone();
    let attachments = ctx
        .db
        .read(move |db| repo::attachments::list(db, &id_for_attachments))
        .await
        .expect("attachments");
    assert!(attachments.is_empty(), "attachment records must be gone");

    let id_for_fts = id.clone();
    let fts: i64 = ctx
        .db
        .read(move |db| {
            Ok(db.query_row(
                "SELECT COUNT(*) FROM fts_mail WHERE message_id = ?1",
                rusqlite::params![id_for_fts],
                |row| row.get(0),
            )?)
        })
        .await
        .expect("fts count");
    assert_eq!(fts, 0, "the FTS document must be gone");
}

/// A permanent delete needs a directed expunge (UID EXPUNGE). Without one the
/// executor refuses *before* the STORE: the optimistic hidden row is rolled
/// back, the server keeps the message and its flags, and the op settles as a
/// failure rather than Done. Undo therefore stays available.
#[tokio::test]
async fn permanent_delete_without_directed_expunge_is_refused_and_undoable() {
    let mock = MockImap::start_with_folders(&["CONDSTORE"], &[("INBOX", &["\\Inbox"])]).await;
    mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Keep on server",
            "body",
            "keep-on-server@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;

    let message = {
        let folder = inbox.clone();
        let ids = ctx
            .db
            .read(move |db| repo::messages::ids_in_folder(db, &folder))
            .await
            .expect("ids");
        let id = ids.into_iter().next().expect("synced message");
        ctx.db
            .read(move |db| repo::messages::get_required(db, &id))
            .await
            .expect("message")
    };
    let id = message.summary.id.clone();
    let uid = message.uid.expect("server UID");

    // Queue the action through the normal service path: the optimistic
    // projection hides the row and records the undo entry before the executor
    // ever runs.
    let service = nuncio_mail::MailService::new(ctx.clone());
    let undo_id = service
        .message_act(
            std::slice::from_ref(&id),
            &nuncio_core::models::MessageAction::Delete { permanent: true },
        )
        .await
        .expect("queue permanent delete");
    let id_for_hidden = id.clone();
    let hidden = ctx
        .db
        .read(move |db| repo::messages::get_required(db, &id_for_hidden))
        .await
        .expect("message");
    assert!(
        hidden.is_deleted_local,
        "the optimistic phase hides the row"
    );

    let before = mock.state.commands().len();
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("run due");
    assert_eq!(outcome.done, 0, "a refusal is not a completed delete");
    assert_eq!(outcome.failed, 1);
    assert_eq!(outcome.retried, 0, "a capability refusal must not retry");

    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        !commands.iter().any(|command| {
            let command = command.trim().to_ascii_uppercase();
            command.starts_with("UID STORE")
                || command == "EXPUNGE"
                || command.starts_with("UID EXPUNGE")
        }),
        "the refusal must happen before the STORE: {commands:?}"
    );

    assert_eq!(
        mock.state.message_count("INBOX"),
        1,
        "the message must stay on the server"
    );
    let flags = mock.state.message_flags("INBOX", uid as u32);
    assert!(
        !flags
            .iter()
            .any(|flag| flag.eq_ignore_ascii_case("\\Deleted")),
        "the server must not keep a \\Deleted flag: {flags:?}"
    );

    let id_for_read = id.clone();
    let stored = ctx
        .db
        .read(move |db| repo::messages::get_required(db, &id_for_read))
        .await
        .expect("the local row must not be hard-deleted");
    assert!(
        !stored.is_deleted_local,
        "the failed op must unhide the row"
    );
    let id_for_pending = id.clone();
    let pending_delete: i64 = ctx
        .db
        .read(move |db| {
            Ok(db.query_row(
                "SELECT pending_delete FROM message WHERE id = ?1",
                rusqlite::params![id_for_pending],
                |row| row.get(0),
            )?)
        })
        .await
        .expect("pending flag");
    assert_eq!(
        pending_delete, 0,
        "a refused delete must not leave pending_delete set"
    );

    let op = ctx
        .db
        .read(move |db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops")
        .into_iter()
        .next()
        .expect("op");
    assert_eq!(op.state, nuncio_core::models::PendingOpState::Failed);

    // The op never reached Done, so the permanent delete is reversible.
    assert!(
        service.message_undo(&undo_id).await.expect("undo"),
        "a refused permanent delete must still be undoable"
    );
    let id_for_restored = id.clone();
    let restored = ctx
        .db
        .read(move |db| repo::messages::get_required(db, &id_for_restored))
        .await
        .expect("restored message");
    assert!(!restored.is_deleted_local);
    assert_eq!(restored.summary.folder_id, inbox);
}

/// A server that advertises only `IMAP4rev2` (no UIDPLUS atom) does *not* get
/// a directed permanent delete. The client never enables rev2 (its `SEARCH`
/// response would be `ESEARCH`, which the parser does not support), so rev2's
/// base `UID EXPUNGE` is out of reach and the delete is refused up front.
#[tokio::test]
async fn imap4rev2_alone_does_not_supply_directed_expunge() {
    let mock =
        MockImap::start_with_folders(&["CONDSTORE", "IMAP4rev2"], &[("INBOX", &["\\Inbox"])]).await;
    mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Delete on rev2",
            "body",
            "delete-rev2@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let uidvalidity = folder_uidvalidity(&ctx, &inbox).await;

    let (id, uid) = {
        let folder = inbox.clone();
        let ids = ctx
            .db
            .read(move |db| repo::messages::ids_in_folder(db, &folder))
            .await
            .expect("ids");
        let id = ids.into_iter().next().expect("synced message");
        let id_for_read = id.clone();
        let stored = ctx
            .db
            .read(move |db| repo::messages::get_required(db, &id_for_read))
            .await
            .expect("message");
        (id, stored.uid.expect("server UID"))
    };

    let payload = ops::DeleteOp {
        folder_id: inbox.clone(),
        uids: vec![uid],
        permanent: true,
        target_folder_id: None,
        soft_hidden: false,
        message_ids: vec![id.clone()],
        source_uidvalidity: uidvalidity,
        pending_delete: true,
        deleted_flag_set: false,
        snapshot: Vec::new(),
        outcomes: Vec::new(),
        remote_ids: Vec::new(),
    };
    ctx.db
        .write({
            let id = id.clone();
            let payload = serde_json::to_value(payload).expect("payload");
            move |db| {
                repo::messages::hide_for_permanent_delete(db, std::slice::from_ref(&id))?;
                repo::pending_ops::enqueue(
                    db,
                    nuncio_core::models::NewPendingOp::new(
                        "acc",
                        ops::KIND_DELETE,
                        payload,
                        "Delete message permanently",
                    ),
                )
            }
        })
        .await
        .expect("enqueue");

    let before = mock.state.commands().len();
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("run due");
    assert_eq!(outcome.done, 0, "rev2 alone cannot authorise the delete");
    assert_eq!(outcome.failed, 1);
    assert_eq!(outcome.retried, 0, "a capability refusal must not retry");

    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        !commands.iter().any(|command| {
            let command = command.trim().to_ascii_uppercase();
            command == "ENABLE IMAP4REV2" || command.starts_with("ENABLE IMAP4REV2 ")
        }),
        "the client must stay in rev1 mode instead of enabling rev2: {commands:?}"
    );
    for forbidden in [
        "UID STORE",
        "UID COPY",
        "UID MOVE",
        "EXPUNGE",
        "UID EXPUNGE",
    ] {
        assert!(
            !commands.iter().any(|command| {
                let command = command.trim().to_ascii_uppercase();
                command == forbidden || command.starts_with(&format!("{forbidden} "))
            }),
            "a rev2-only server must not receive {forbidden}: {commands:?}"
        );
    }
    assert_eq!(mock.state.message_count("INBOX"), 1);
}

/// A server advertising both `IMAP4rev1` and `IMAP4rev2` also supplies no
/// directed expunge without UIDPLUS: the session deliberately stays rev1, so
/// the delete must be refused before any STORE and nothing may be expunged.
#[tokio::test]
async fn dual_revision_capability_does_not_supply_directed_expunge() {
    let mock =
        MockImap::start_with_folders(&["CONDSTORE", "IMAP4rev2"], &[("INBOX", &["\\Inbox"])]).await;
    mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Keep on rev1",
            "body",
            "keep-rev1@x",
        ),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let uidvalidity = folder_uidvalidity(&ctx, &inbox).await;

    let (id, uid) = {
        let folder = inbox.clone();
        let ids = ctx
            .db
            .read(move |db| repo::messages::ids_in_folder(db, &folder))
            .await
            .expect("ids");
        let id = ids.into_iter().next().expect("synced message");
        let id_for_read = id.clone();
        let stored = ctx
            .db
            .read(move |db| repo::messages::get_required(db, &id_for_read))
            .await
            .expect("message");
        (id, stored.uid.expect("server UID"))
    };

    let payload = ops::DeleteOp {
        folder_id: inbox.clone(),
        uids: vec![uid],
        permanent: true,
        target_folder_id: None,
        soft_hidden: false,
        message_ids: vec![id.clone()],
        source_uidvalidity: uidvalidity,
        pending_delete: true,
        deleted_flag_set: false,
        snapshot: Vec::new(),
        outcomes: Vec::new(),
        remote_ids: Vec::new(),
    };
    ctx.db
        .write({
            let id = id.clone();
            let payload = serde_json::to_value(payload).expect("payload");
            move |db| {
                repo::messages::hide_for_permanent_delete(db, std::slice::from_ref(&id))?;
                repo::pending_ops::enqueue(
                    db,
                    nuncio_core::models::NewPendingOp::new(
                        "acc",
                        ops::KIND_DELETE,
                        payload,
                        "Delete message permanently",
                    ),
                )
            }
        })
        .await
        .expect("enqueue");

    let before = mock.state.commands().len();
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("run due");
    assert_eq!(outcome.done, 0, "a refusal is not a completed delete");
    assert_eq!(outcome.failed, 1);
    assert_eq!(outcome.retried, 0, "a capability refusal must not retry");

    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        !commands.iter().any(|command| {
            let command = command.trim().to_ascii_uppercase();
            command == "ENABLE IMAP4REV2" || command.starts_with("ENABLE IMAP4REV2 ")
        }),
        "the client must stay in rev1 mode: {commands:?}"
    );
    for forbidden in [
        "UID STORE",
        "UID COPY",
        "UID MOVE",
        "EXPUNGE",
        "UID EXPUNGE",
    ] {
        assert!(
            !commands.iter().any(|command| {
                let command = command.trim().to_ascii_uppercase();
                command == forbidden || command.starts_with(&format!("{forbidden} "))
            }),
            "a dual-revision session must not send {forbidden}: {commands:?}"
        );
    }
    assert_eq!(mock.state.message_count("INBOX"), 1);
}

#[tokio::test]
async fn outbox_sends_draft_and_appends_to_sent() {
    let imap_mock = MockImap::start_with_folders(
        &["UIDPLUS", "MOVE"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Sent", &["\\Sent"]),
            ("Drafts", &["\\Drafts"]),
        ],
    )
    .await;
    let smtp_mock = MockSmtp::start().await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        Some(server(imap_mock.addr.port())),
        Some(server(smtp_mock.addr.port())),
        None,
    );
    insert_account(&ctx, &account).await;
    // Populate folders and the Sent folder id.
    sync_account(&ctx, &account).await;

    let draft = nuncio_core::models::Draft {
        id: None,
        account_id: "acc".into(),
        identity_id: None,
        from: nuncio_core::models::Address::new("test@example.com"),
        to: vec![nuncio_core::models::Address::new("bob@example.com")],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "Draft subject".into(),
        html: "<p>hi</p>".into(),
        attachments: Vec::new(),
        reply_mode: None,
        source_message_id: None,
        in_reply_to: None,
        references: Vec::new(),
        importance: nuncio_core::models::Importance::Normal,
        request_read_receipt: false,
        remote_draft_message_id: None,
        remote_draft: None,
        sync_suppressed: false,
        updated_at: None,
    };
    let draft = ctx
        .db
        .write(move |db| repo::drafts::save(db, &draft))
        .await
        .expect("draft");
    let draft_id = draft.id.clone().expect("draft id");

    let raw = message_raw(
        "test@example.com",
        "bob@example.com",
        "Draft subject",
        "hi",
        "draft@x",
    );
    let path = ctx.paths.staging_dir().join("draft-test.eml");
    ctx.paths.ensure_parent(&path).expect("staging");
    std::fs::write(&path, &raw).expect("write");
    let item = OutboxItem::new(
        "acc",
        Some(draft_id.clone()),
        vec![nuncio_core::models::Address::new("bob@example.com")],
        "Draft subject",
        path.to_string_lossy().to_string(),
    );
    let item = ctx
        .db
        .write({
            let item = item.clone();
            move |db| repo::outbox::enqueue(db, &item)
        })
        .await
        .expect("outbox");

    smtp::outbox::process_one(&ctx, &account, &item.id)
        .await
        .expect("send");
    assert_eq!(smtp_mock.messages.lock().expect("smtp").len(), 1);
    assert!(ctx
        .db
        .read({
            let id = draft_id.clone();
            move |db| repo::drafts::get(db, &id)
        })
        .await
        .expect("draft")
        .is_none());

    // The Sent copy is queued as an APPEND operation and executed.
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("run append");
    assert!(outcome.done >= 1);
    assert_eq!(imap_mock.state.message_count("Sent"), 1);

    let suggestions = ctx
        .db
        .read(|db| repo::recipient_history::suggest(db, "bob", 10))
        .await
        .expect("suggest");
    assert!(suggestions
        .iter()
        .any(|address| address.email == "bob@example.com"));
}

/// The post-send Answered/$Forwarded write is not an undoable mail action, so
/// it never passes through `message_act`'s writer recording. It must still
/// queue a payload that carries `messageIds` and record the writer in the same
/// transaction; otherwise the Graph/IMAP executor treats it as a legacy
/// payload and can replay a flag the user has since cancelled.
#[tokio::test]
async fn post_send_reply_flags_carry_message_id_and_writer() {
    let imap_mock = MockImap::start_with_folders(
        &["UIDPLUS", "MOVE"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Sent", &["\\Sent"]),
            ("Drafts", &["\\Drafts"]),
        ],
    )
    .await;
    let smtp_mock = MockSmtp::start().await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        Some(server(imap_mock.addr.port())),
        Some(server(smtp_mock.addr.port())),
        None,
    );
    insert_account(&ctx, &account).await;
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;

    let source_id = ctx
        .db
        .write(move |db| {
            let mut summary = MessageSummary::new(
                "source",
                "acc",
                inbox,
                "thread-source",
                nuncio_core::models::now(),
            );
            summary.subject = "Original".into();
            summary.seen = true;
            let mut stored = StoredMessage::new(summary);
            stored.uid = Some(42);
            stored.uidvalidity = Some(1);
            repo::messages::upsert(db, &stored)?;
            Ok(stored.summary.id)
        })
        .await
        .expect("source message");

    let draft = Draft {
        id: None,
        account_id: "acc".into(),
        identity_id: None,
        from: Address::new("test@example.com"),
        to: vec![Address::new("bob@example.com")],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "Re: Original".into(),
        html: "<p>reply</p>".into(),
        attachments: Vec::new(),
        reply_mode: Some(ReplyMode::Reply),
        source_message_id: Some(source_id.clone()),
        in_reply_to: None,
        references: Vec::new(),
        importance: nuncio_core::models::Importance::Normal,
        request_read_receipt: false,
        remote_draft_message_id: None,
        remote_draft: None,
        sync_suppressed: false,
        updated_at: None,
    };
    let draft = ctx
        .db
        .write(move |db| repo::drafts::save(db, &draft))
        .await
        .expect("draft");
    let draft_id = draft.id.clone().expect("draft id");

    let raw = message_raw(
        "test@example.com",
        "bob@example.com",
        "Re: Original",
        "reply",
        "reply@x",
    );
    let path = ctx.paths.staging_dir().join("draft-reply.eml");
    ctx.paths.ensure_parent(&path).expect("staging");
    std::fs::write(&path, &raw).expect("write");
    let item = OutboxItem::new(
        "acc",
        Some(draft_id),
        vec![Address::new("bob@example.com")],
        "Re: Original",
        path.to_string_lossy().to_string(),
    );
    let item = ctx
        .db
        .write(move |db| repo::outbox::enqueue(db, &item))
        .await
        .expect("outbox");

    smtp::outbox::process_one(&ctx, &account, &item.id)
        .await
        .expect("send");
    assert_eq!(smtp_mock.messages.lock().expect("smtp").len(), 1);

    let ops = ctx
        .db
        .read(|db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops");
    let flag_op = ops
        .into_iter()
        .find(|op| op.kind == ops::KIND_SET_FLAGS)
        .expect("the reply must queue a setFlags operation");
    let payload: ops::SetFlagsOp = repo::pending_ops::payload(&flag_op).expect("setFlags payload");
    assert_eq!(
        payload.message_ids,
        vec![source_id.clone()],
        "the queued payload must carry the per-message id"
    );
    assert_eq!(payload.answered, Some(true));
    let writer = ctx
        .db
        .read({
            let id = source_id.clone();
            move |db| repo::messages::flag_writer(db, &id, "answered")
        })
        .await
        .expect("writer")
        .expect("the queued operation must own the answered field");
    assert_eq!(writer.owner_id, flag_op.id);
    let stored = ctx
        .db
        .read({
            let id = source_id.clone();
            move |db| repo::messages::get_required(db, &id)
        })
        .await
        .expect("stored source");
    assert!(stored.summary.answered, "the local flag is applied");
}

/// A reply sent from one account can target a message held by another. The
/// folder/UID in the payload only identify a copy inside the *source* mailbox,
/// so the queued answer flag must belong to the source account. Queueing it
/// under the sender would make that worker SELECT the same folder/UID on a
/// different server and mark an unrelated message.
#[tokio::test]
async fn reply_source_flags_are_queued_under_the_source_account() {
    let smtp_mock = MockSmtp::start().await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let source_account = account("src", "source@example.com", Some(server(1)), None, None);
    let sender = account(
        "sender",
        "sender@example.com",
        None,
        Some(server(smtp_mock.addr.port())),
        None,
    );
    insert_account(&ctx, &source_account).await;
    insert_account(&ctx, &sender).await;
    let folder = Folder::new("src-inbox", "src", "INBOX", FolderRole::Inbox);
    ctx.db
        .write(move |db| repo::folders::insert(db, &folder))
        .await
        .expect("source folder");

    let source_id = ctx
        .db
        .write(|db| {
            let mut summary = MessageSummary::new(
                "source",
                "src",
                "src-inbox",
                "thread-source",
                nuncio_core::models::now(),
            );
            summary.subject = "Original".into();
            summary.seen = true;
            let mut stored = StoredMessage::new(summary);
            stored.uid = Some(42);
            stored.uidvalidity = Some(1);
            repo::messages::upsert(db, &stored)?;
            Ok(stored.summary.id)
        })
        .await
        .expect("source message");

    let draft = Draft {
        id: None,
        account_id: "sender".into(),
        identity_id: None,
        from: Address::new("sender@example.com"),
        to: vec![Address::new("bob@example.com")],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "Re: Original".into(),
        html: "<p>reply</p>".into(),
        attachments: Vec::new(),
        reply_mode: Some(ReplyMode::Reply),
        source_message_id: Some(source_id.clone()),
        in_reply_to: None,
        references: Vec::new(),
        importance: nuncio_core::models::Importance::Normal,
        request_read_receipt: false,
        remote_draft_message_id: None,
        remote_draft: None,
        sync_suppressed: false,
        updated_at: None,
    };
    let draft = ctx
        .db
        .write(move |db| repo::drafts::save(db, &draft))
        .await
        .expect("draft");
    let draft_id = draft.id.clone().expect("draft id");

    let raw = message_raw(
        "sender@example.com",
        "bob@example.com",
        "Re: Original",
        "reply",
        "reply@x",
    );
    let path = ctx.paths.staging_dir().join("draft-cross-account.eml");
    ctx.paths.ensure_parent(&path).expect("staging");
    std::fs::write(&path, &raw).expect("write");
    let item = OutboxItem::new(
        "sender",
        Some(draft_id),
        vec![Address::new("bob@example.com")],
        "Re: Original",
        path.to_string_lossy().to_string(),
    );
    let item = ctx
        .db
        .write(move |db| repo::outbox::enqueue(db, &item))
        .await
        .expect("outbox");

    smtp::outbox::process_one(&ctx, &sender, &item.id)
        .await
        .expect("send");
    assert_eq!(smtp_mock.messages.lock().expect("smtp").len(), 1);

    let source_ops = ctx
        .db
        .read(|db| repo::pending_ops::list(db, Some("src")))
        .await
        .expect("source ops");
    let flag_ops: Vec<_> = source_ops
        .into_iter()
        .filter(|op| op.kind == ops::KIND_SET_FLAGS)
        .collect();
    assert_eq!(
        flag_ops.len(),
        1,
        "the reply must queue its answer flag under the source account"
    );
    let payload: ops::SetFlagsOp =
        repo::pending_ops::payload(&flag_ops[0]).expect("setFlags payload");
    assert_eq!(payload.folder_id, "src-inbox");
    assert_eq!(payload.uids, vec![42]);
    assert_eq!(payload.message_ids, vec![source_id.clone()]);

    let sender_ops = ctx
        .db
        .read(|db| repo::pending_ops::list(db, Some("sender")))
        .await
        .expect("sender ops");
    assert!(
        sender_ops.iter().all(|op| op.kind != ops::KIND_SET_FLAGS),
        "the sender account must not own a queue row for the source copy: {sender_ops:?}"
    );

    let stored = ctx
        .db
        .read({
            let id = source_id.clone();
            move |db| repo::messages::get_required(db, &id)
        })
        .await
        .expect("stored source");
    assert!(stored.summary.answered, "the local flag is applied");
}

#[tokio::test]
async fn body_fetch_extracts_attachment_metadata_and_bytes() {
    let mock = MockImap::start(&["CONDSTORE"]).await;
    let raw = concat!(
        "From: a@example.com\r\n",
        "To: test@example.com\r\n",
        "Subject: With attachment\r\n",
        "Date: Thu, 12 Nov 2026 08:00:00 +0000\r\n",
        "Message-ID: <attach@x>\r\n",
        "MIME-Version: 1.0\r\n",
        "Content-Type: multipart/mixed; boundary=\"BOUND\"\r\n",
        "\r\n",
        "--BOUND\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "\r\n",
        "attachment body\r\n",
        "--BOUND\r\n",
        "Content-Type: application/pdf; name=\"doc.pdf\"\r\n",
        "Content-Disposition: attachment; filename=\"doc.pdf\"\r\n",
        "Content-Transfer-Encoding: base64\r\n",
        "\r\n",
        "aGVsbG8=\r\n",
        "--BOUND--\r\n"
    );
    mock.state.add_message("INBOX", raw.as_bytes());
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let query = MessageListQuery::new(MailScope::folder(inbox))
        .with_conversation(false)
        .with_limit(5);
    let page = ctx
        .db
        .read(move |db| repo::messages::list(db, &query))
        .await
        .expect("page");
    let message = page.items.first().expect("message").clone();
    let message_id = message.id.clone();
    let mut connection = connect_account(&account).await;
    nuncio_mail::sync::body::fetch_body(&ctx, &mut connection, "acc", &message_id)
        .await
        .expect("body");
    let attachments = ctx
        .db
        .read({
            let id = message_id.clone();
            move |db| repo::attachments::list(db, &id)
        })
        .await
        .expect("attachments");
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].filename, "doc.pdf");
    let parsed = nuncio_mime::parse(raw.as_bytes()).expect("parse raw");
    let attachment = parsed.attachments.first().expect("parsed attachment");
    let bytes = nuncio_mail::sync::body::fetch_attachment(
        &ctx,
        &mut connection,
        &message_id,
        &attachment.index.to_string(),
    )
    .await
    .expect("attachment bytes");
    assert_eq!(bytes, b"hello".to_vec());
}

#[tokio::test]
async fn engine_executes_folder_and_append_commands() {
    let mock = MockImap::start(&["CONDSTORE", "MOVE", "UIDPLUS"]).await;
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
    let engine = Engine::new(ctx.clone());
    engine.start().await.expect("start");
    engine.sync_now(Some("acc"), None).await.expect("sync");

    engine
        .create_folder("acc", None, "Projects")
        .await
        .expect("create folder");
    let folders = engine.list_folders("acc").await.expect("list");
    let projects = folders
        .iter()
        .find(|folder| folder.name == "Projects")
        .expect("created folder")
        .clone();
    engine
        .rename_folder("acc", &projects.id, "Renamed")
        .await
        .expect("rename");
    let folders = engine.list_folders("acc").await.expect("list");
    assert!(folders.iter().any(|folder| folder.name == "Renamed"));

    let inbox = folders
        .iter()
        .find(|folder| folder.role == nuncio_core::models::FolderRole::Inbox)
        .expect("inbox")
        .id
        .clone();
    let raw = message_raw(
        "test@example.com",
        "bob@example.com",
        "Appended",
        "append body",
        "append@x",
    );
    engine
        .append("acc", &inbox, raw, vec!["\\Seen".into()])
        .await
        .expect("append");
    assert_eq!(mock.state.message_count("INBOX"), 1);
    engine.expunge_folder("acc", &inbox).await.expect("expunge");
    assert_eq!(mock.state.message_count("INBOX"), 0);

    let renamed_id = folders
        .iter()
        .find(|folder| folder.name == "Renamed")
        .expect("renamed")
        .id
        .clone();
    engine
        .delete_folder("acc", &renamed_id)
        .await
        .expect("delete");
    let folders = engine.list_folders("acc").await.expect("list");
    assert!(!folders.iter().any(|folder| folder.name == "Renamed"));
    engine.shutdown().await;
}

/// Emptying a folder is destructive for every message in it. Without a
/// directed expunge the actor must refuse before the STORE: no `\Deleted`
/// flags, no EXPUNGE, no local rows erased. The caller gets `false`.
#[tokio::test]
async fn expunge_folder_without_directed_expunge_is_refused_before_store() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE"],
        &[("INBOX", &["\\Inbox"]), ("Junk", &["\\Junk"])],
    )
    .await;
    mock.state.add_message(
        "Junk",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Spam",
            "body",
            "spam@x",
        ),
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
    let engine = Engine::new(ctx.clone());
    engine.start().await.expect("start");
    engine.sync_now(Some("acc"), None).await.expect("sync");
    let junk = folder_id(&ctx, "acc", "Junk").await;

    let before = mock.state.commands().len();
    let expunged = engine.expunge_folder("acc", &junk).await.expect("expunge");
    assert!(
        !expunged,
        "without UIDPLUS the folder cannot be emptied safely"
    );

    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        !commands.iter().any(|command| {
            let command = command.trim().to_ascii_uppercase();
            command.starts_with("UID STORE")
                || command.contains("\\DELETED")
                || command == "EXPUNGE"
                || command.starts_with("UID EXPUNGE")
        }),
        "a refused empty must not flag or expunge anything: {commands:?}"
    );
    assert_eq!(mock.state.message_count("Junk"), 1);

    let junk_for_ids = junk.clone();
    let ids = ctx
        .db
        .read(move |db| repo::messages::ids_in_folder(db, &junk_for_ids))
        .await
        .expect("local rows");
    assert_eq!(ids.len(), 1, "the local row must be kept");
    let id = ids[0].clone();
    let stored = ctx
        .db
        .read(move |db| repo::messages::get_required(db, &id))
        .await
        .expect("row");
    assert!(!stored.is_deleted_local);
    engine.shutdown().await;
}

/// NetEase 163 closes idle sockets quickly; the poll loop must detect the
/// stale session, reconnect transparently and continue without surfacing an
/// error status to the UI.
#[tokio::test]
async fn poll_loop_recovers_stale_connection_without_error_status() {
    // No IDLE capability: INBOX is polled on a short cadence.
    let mock = MockImap::start(&["CONDSTORE"]).await;
    mock.state
        .set_idle_timeout(Some(Duration::from_millis(400)));
    mock.state.add_message(
        "INBOX",
        &message_raw("a@example.com", "test@example.com", "One", "one", "one@x"),
    );
    let (ctx, sink) = support::context_with_sink(
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

    let config = EngineConfig {
        liveness_probe_after: Duration::from_millis(50),
        // Disable the explicit keepalive so only the before-use probe can
        // rescue the session.
        keepalive_interval: Duration::from_secs(3600),
        no_idle_poll_interval: Duration::from_millis(600),
        ..EngineConfig::default()
    };
    let engine = Engine::with_config(ctx.clone(), config);
    engine.start().await.expect("engine start");
    engine
        .sync_now(Some("acc"), None)
        .await
        .expect("initial sync");

    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    assert_eq!(message_count(&ctx, &inbox).await, 1);

    // Wait for the mock to actually drop the idle socket (its timeout is
    // 400ms) *before* sampling the connection count.  Sampling first made the
    // assertion race with the drop: on a slower scheduler the poll could find
    // the socket still open, sync over it, and never open a new connection.
    tokio::time::sleep(Duration::from_millis(900)).await;
    let baseline = mock.state.connection_count();

    // The worker's socket is gone by now; add a message the poll must find.
    mock.state.add_message(
        "INBOX",
        &message_raw("b@example.com", "test@example.com", "Two", "two", "two@x"),
    );
    let mut recovered = false;
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if message_count(&ctx, &inbox).await >= 2 {
            recovered = true;
            break;
        }
    }
    assert!(
        recovered,
        "poll loop did not recover from the stale session"
    );
    assert!(
        mock.state.connection_count() > baseline,
        "stale session should have been replaced"
    );

    tokio::time::sleep(Duration::from_millis(150)).await;
    let status = ctx
        .sync_status
        .get("acc", SyncModule::Mail)
        .expect("sync status");
    assert_eq!(status.state, SyncState::Idle, "status: {status:?}");

    let error_events: Vec<_> = sink
        .events()
        .into_iter()
        .filter(|(event, payload)| {
            event == nuncio_core::events::SYNC_STATUS
                && payload.get("state").and_then(|value| value.as_str()) == Some("error")
        })
        .collect();
    assert!(
        error_events.is_empty(),
        "transparent reconnect emitted an error status: {error_events:?}"
    );
    engine.shutdown().await;
}

/// The NOOP keepalive must keep the worker connection alive across a server
/// that drops sockets idle for 300 ms.
#[tokio::test]
async fn keepalive_noop_prevents_idle_connection_drop() {
    let mock = MockImap::start(&["CONDSTORE"]).await;
    mock.state
        .set_idle_timeout(Some(Duration::from_millis(300)));
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

    let config = EngineConfig {
        liveness_probe_after: Duration::from_millis(50),
        keepalive_interval: Duration::from_millis(80),
        // Keep the INBOX poll out of the picture.
        no_idle_poll_interval: Duration::from_secs(3600),
        ..EngineConfig::default()
    };
    let engine = Engine::with_config(ctx.clone(), config);
    engine.start().await.expect("engine start");
    engine
        .sync_now(Some("acc"), None)
        .await
        .expect("initial sync");

    // Let the startup connections settle, then record the baseline.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let baseline = mock.state.connection_count();
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(
        mock.state.connection_count(),
        baseline,
        "keepalive should have kept the worker connection open"
    );
    engine.shutdown().await;
}

/// Incremental sync is a pure UIDL diff: known messages cost no TOP/RETR, and
/// a message already DELETEd is not fetched again even though it vanished.
#[tokio::test]
async fn pop3_incremental_sync_skips_known_and_already_deleted_messages() {
    let mock = MockPop3::start().await;
    mock.add(
        "uid-old",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Old",
            "old body",
            "pop-old@x",
        ),
    );
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        None,
        None,
        Some(server(mock.addr.port())),
    );
    insert_account(&ctx, &account).await;
    let account_id = account.id.clone();
    ctx.db
        .write(move |db| repo::sync_state::set(db, &account_id, "pop3", "leave_on_server", "false"))
        .await
        .expect("leave_on_server");

    nuncio_mail::pop3::sync::sync_account(&ctx, &account)
        .await
        .expect("first sync");
    assert_eq!(mock.counters.dele(), 1);
    assert!(mock.messages.lock().expect("messages").is_empty());
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    assert_eq!(message_count(&ctx, &inbox).await, 1);

    // A second pass sees an empty server: the local row must survive because
    // its UIDL is recorded as deleted rather than silently pruned.
    mock.reset_counters();
    nuncio_mail::pop3::sync::sync_account(&ctx, &account)
        .await
        .expect("second sync");
    assert_eq!(mock.counters.top(), 0);
    assert_eq!(mock.counters.retr(), 0);
    assert_eq!(message_count(&ctx, &inbox).await, 1);

    // A brand-new message costs exactly one header fetch.
    mock.add(
        "uid-new",
        &message_raw(
            "b@example.com",
            "test@example.com",
            "New",
            "new body",
            "pop-new@x",
        ),
    );
    nuncio_mail::pop3::sync::sync_account(&ctx, &account)
        .await
        .expect("third sync");
    assert_eq!(mock.counters.top(), 1, "only the new message is fetched");
    assert_eq!(mock.counters.retr(), 0);
    assert_eq!(message_count(&ctx, &inbox).await, 2);
}

/// Every UIDL is known locally, so paging older POP3 mail is a pure SQL
/// query: the second page needs no server connection at all.
#[tokio::test]
async fn pop3_paging_is_served_locally_without_a_server_round_trip() {
    let mock = MockPop3::start().await;
    for index in 0..3 {
        mock.add(
            &format!("uid-{index}"),
            &message_raw(
                "a@example.com",
                "test@example.com",
                &format!("Pop {index}"),
                "body",
                &format!("pop-page-{index}@x"),
            ),
        );
    }
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        None,
        None,
        Some(server(mock.addr.port())),
    );
    insert_account(&ctx, &account).await;
    nuncio_mail::pop3::sync::sync_account(&ctx, &account)
        .await
        .expect("pop3 sync");
    assert_eq!(mock.counters.retr(), 0);
    let inbox = folder_id(&ctx, "acc", "INBOX").await;

    let first = {
        let query = MessageListQuery::new(MailScope::folder(inbox.clone()))
            .with_conversation(false)
            .with_limit(2);
        ctx.db
            .read(move |db| repo::messages::list(db, &query))
            .await
            .expect("first page")
    };
    assert_eq!(first.items.len(), 2);
    assert_eq!(first.total, 3);
    let cursor = first.next_cursor.expect("next cursor");

    let top = mock.counters.top();
    let second = {
        let query = MessageListQuery::new(MailScope::folder(inbox))
            .with_conversation(false)
            .with_limit(2)
            .with_cursor(Some(cursor));
        ctx.db
            .read(move |db| repo::messages::list(db, &query))
            .await
            .expect("second page")
    };
    assert_eq!(second.items.len(), 1);
    assert_eq!(mock.counters.top(), top, "paging must not touch the server");
    assert_eq!(mock.counters.retr(), 0);
}

/* ───────────────────────────── load older mail ───────────────────────────── */

/// Add `count` messages to the mock INBOX (UIDs `1..=count`).
fn seed_inbox(mock: &MockImap, count: usize) {
    // Message-IDs must stay unique across separate seed_inbox calls: two
    // provider UIDs are two messages even when they look alike, and the
    // identity-aware sync import keeps them apart.
    let base = mock.state.message_count("INBOX");
    for index in 0..count {
        let unique = base + index;
        mock.state.add_message(
            "INBOX",
            &message_raw(
                "a@example.com",
                "test@example.com",
                &format!("Older {unique}"),
                "body",
                &format!("older-{unique}@x"),
            ),
        );
    }
}

/// Emulate a sync window that excluded old mail: drop the local rows whose UID
/// is at or below `max_uid` (the server still has them).
async fn forget_local_up_to(ctx: &CoreContext, folder_id: &str, max_uid: i64) {
    let folder_id = folder_id.to_string();
    ctx.db
        .write(move |db| {
            db.execute(
                "DELETE FROM message WHERE folder_id = ?1 AND uid <= ?2",
                rusqlite::params![folder_id, max_uid],
            )?;
            repo::folders::recompute_counts(db, &folder_id)
        })
        .await
        .expect("forget old rows");
}

async fn min_uid(ctx: &CoreContext, folder_id: &str) -> Option<i64> {
    let folder_id = folder_id.to_string();
    ctx.db
        .read(move |db| {
            Ok(db.query_row(
                "SELECT MIN(uid) FROM message WHERE folder_id = ?1",
                rusqlite::params![folder_id],
                |row| row.get::<_, Option<i64>>(0),
            )?)
        })
        .await
        .expect("min uid")
}

async fn synced_inbox(mock: &MockImap) -> (CoreContext, nuncio_core::models::Account, String) {
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    (ctx, account, inbox)
}

#[tokio::test]
async fn load_older_imap_pulls_batches_below_the_min_uid_until_exhausted() {
    let mock = MockImap::start(&["IDLE", "CONDSTORE", "UIDPLUS"]).await;
    seed_inbox(&mock, 450);
    let (ctx, _account, inbox) = synced_inbox(&mock).await;
    assert_eq!(message_count(&ctx, &inbox).await, 450);
    forget_local_up_to(&ctx, &inbox, 300).await;
    assert_eq!(message_count(&ctx, &inbox).await, 150);
    assert_eq!(min_uid(&ctx, &inbox).await, Some(301));

    // Through the engine/actor path so the command plumbing is covered too.
    let engine = Engine::new(ctx.clone());
    engine.start().await.expect("engine start");

    let first = engine.load_older(&inbox).await.expect("first batch");
    assert_eq!((first.loaded, first.has_more), (50, true));
    assert_eq!(min_uid(&ctx, &inbox).await, Some(251));
    assert_eq!(message_count(&ctx, &inbox).await, 200);

    // `load_older` uses its own batch size: every step must stay at or below
    // 50 UIDs, and `has_more` keeps its meaning until UID 1 is reached.
    let mut total = first.loaded as usize;
    let mut has_more = first.has_more;
    while has_more {
        let step = engine.load_older(&inbox).await.expect("next batch");
        assert!(
            step.loaded <= 50,
            "load_older must not exceed its batch size"
        );
        total += step.loaded as usize;
        has_more = step.has_more;
    }
    assert_eq!(total, 300, "every older message was fetched once");
    assert_eq!(min_uid(&ctx, &inbox).await, Some(1));
    assert_eq!(message_count(&ctx, &inbox).await, 450);

    // Exhausted: UID 1 is stored, nothing below it, no request needed.
    let third = engine.load_older(&inbox).await.expect("third batch");
    assert_eq!((third.loaded, third.has_more), (0, false));

    // Unseen state is untouched: no STORE, no non-PEEK body fetch, and the
    // folder's unread counter covers every loaded row.
    let commands = mock.state.commands();
    assert!(
        commands
            .iter()
            .all(|command| !command.contains(" STORE ") && !command.starts_with("STORE ")),
        "load older must never write flags"
    );
    assert!(commands
        .iter()
        .filter(|command| command.contains("FETCH"))
        .all(|command| !command.contains("BODY[") || command.contains("BODY.PEEK[")));
    for uid in [1u32, 150, 300, 450] {
        assert!(mock.state.message_flags("INBOX", uid).is_empty());
    }
    let folder = {
        let inbox = inbox.clone();
        ctx.db
            .read(move |db| repo::folders::get_required(db, &inbox))
            .await
            .expect("folder")
    };
    assert_eq!(folder.total_count, 450);
    assert_eq!(folder.unread_count, 450);

    engine.shutdown().await;
}

#[tokio::test]
async fn load_older_imap_on_an_empty_folder_reports_nothing_more() {
    let mock = MockImap::start(&["CONDSTORE"]).await;
    let (ctx, account, inbox) = synced_inbox(&mock).await;
    let folder = {
        let inbox = inbox.clone();
        ctx.db
            .read(move |db| repo::folders::get_required(db, &inbox))
            .await
            .expect("folder")
    };
    let mut connection = connect_account(&account).await;
    let result =
        nuncio_mail::sync::messages::load_older_imap(&ctx, &mut connection, &account, &folder)
            .await
            .expect("empty folder");
    assert_eq!((result.loaded, result.has_more), (0, false));
}

#[tokio::test]
async fn load_older_imap_widens_the_range_over_sparse_uids() {
    let mock = MockImap::start(&["CONDSTORE"]).await;
    seed_inbox(&mock, 3000);
    // Leave UIDs 1..=5 and 3000; everything in between was expunged.
    for uid in 6..3000u32 {
        mock.state.remove_message("INBOX", uid);
    }
    let (ctx, account, inbox) = synced_inbox(&mock).await;
    assert_eq!(message_count(&ctx, &inbox).await, 6);
    forget_local_up_to(&ctx, &inbox, 5).await;
    assert_eq!(min_uid(&ctx, &inbox).await, Some(3000));

    let folder = {
        let inbox = inbox.clone();
        ctx.db
            .read(move |db| repo::folders::get_required(db, &inbox))
            .await
            .expect("folder")
    };
    let mut connection = connect_account(&account).await;
    let result =
        nuncio_mail::sync::messages::load_older_imap(&ctx, &mut connection, &account, &folder)
            .await
            .expect("sparse");
    assert_eq!((result.loaded, result.has_more), (5, false));
    assert_eq!(min_uid(&ctx, &inbox).await, Some(1));
    // Bounded ranges only: never an unbounded `SEARCH ALL` for this request.
    let searches: Vec<String> = mock
        .state
        .commands()
        .into_iter()
        .filter(|command| command.contains("SEARCH") && command.contains("UID 1:"))
        .collect();
    assert!(!searches.is_empty());
}

#[tokio::test]
async fn load_older_imap_refuses_after_a_uidvalidity_change() {
    let mock = MockImap::start(&["CONDSTORE"]).await;
    seed_inbox(&mock, 10);
    let (ctx, account, inbox) = synced_inbox(&mock).await;
    forget_local_up_to(&ctx, &inbox, 5).await;
    let before = message_count(&ctx, &inbox).await;

    mock.state.bump_uidvalidity("INBOX", 99);
    let folder = {
        let inbox = inbox.clone();
        ctx.db
            .read(move |db| repo::folders::get_required(db, &inbox))
            .await
            .expect("folder")
    };
    let mut connection = connect_account(&account).await;
    let error =
        nuncio_mail::sync::messages::load_older_imap(&ctx, &mut connection, &account, &folder)
            .await
            .expect_err("uidvalidity changed");
    assert_eq!(error.code(), "conflict");
    // Nothing destructive happened.
    assert_eq!(message_count(&ctx, &inbox).await, before);
    assert_eq!(min_uid(&ctx, &inbox).await, Some(6));
}

/// A folder-wide mark-read normally writes `1:*`, but when a newer
/// single-message action owns one row it falls back to a targeted STORE of
/// cached UIDs. Those UIDs are only meaningful in their synced generation: if
/// the mailbox was rebuilt, the operation must refuse instead of marking
/// unrelated messages read.
#[tokio::test]
async fn folder_mark_read_refuses_cached_uids_from_another_generation() {
    let mock = MockImap::start(&["UIDPLUS"]).await;
    seed_inbox(&mock, 2);
    let (ctx, account, inbox) = synced_inbox(&mock).await;

    let ids = {
        let inbox = inbox.clone();
        ctx.db
            .read(move |db| repo::messages::ids_in_folder(db, &inbox))
            .await
            .expect("ids")
    };
    assert_eq!(ids.len(), 2);
    let op_id = "folder-read-op".to_string();
    {
        let inbox = inbox.clone();
        let held = ids[1].clone();
        let op_id_for_db = op_id.clone();
        ctx.db
            .write(move |db| {
                // A newer action took over this row, so the folder operation
                // cannot use `1:*`.
                repo::messages::record_flag_writer(
                    db,
                    &held,
                    "seen",
                    "later-action",
                    Some(false),
                    "setSeen",
                )?;
                repo::messages::mark_all_read(db, &inbox)?;
                let payload = ops::FolderMarkReadOp {
                    folder_id: inbox.clone(),
                };
                repo::pending_ops::enqueue_with_id(
                    db,
                    &op_id_for_db,
                    NewPendingOp::new(
                        "acc",
                        ops::KIND_FOLDER_MARK_READ,
                        serde_json::to_value(payload).expect("payload"),
                        "Mark folder as read",
                    ),
                )?;
                Ok(())
            })
            .await
            .expect("queue folder mark read");
    }

    mock.state.bump_uidvalidity("INBOX", 99);
    let before = mock.state.commands().len();
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("executor pass");
    assert_eq!(outcome.failed, 1);

    let commands: Vec<String> = mock
        .state
        .commands()
        .into_iter()
        .skip(before)
        .filter(|command| command.to_ascii_uppercase().starts_with("UID STORE"))
        .collect();
    assert!(
        commands.is_empty(),
        "no STORE may address a UID from another generation: {commands:?}"
    );

    let op = {
        let id = op_id.clone();
        ctx.db
            .read(move |db| repo::pending_ops::get(db, &id))
            .await
            .expect("op")
            .expect("op row")
    };
    assert_eq!(op.state, nuncio_core::models::PendingOpState::Failed);
    assert!(
        op.last_error
            .as_deref()
            .is_some_and(|error| error.contains("rebuilt")),
        "the failure must say the mailbox was rebuilt: {:?}",
        op.last_error
    );
}

/// A body is addressed by the row's cached UID. After a mailbox rebuild that
/// number may name another message, so the fetch must verify the live
/// UIDVALIDITY and refuse instead of caching the wrong bytes under this row.
#[tokio::test]
async fn body_fetch_refuses_a_uid_from_another_generation() {
    let mock = MockImap::start(&["CONDSTORE"]).await;
    seed_inbox(&mock, 1);
    let (ctx, account, inbox) = synced_inbox(&mock).await;
    let message_id = {
        let inbox = inbox.clone();
        ctx.db
            .read(move |db| repo::messages::ids_in_folder(db, &inbox))
            .await
            .expect("ids")
            .into_iter()
            .next()
            .expect("synced message")
    };
    {
        let id = message_id.clone();
        ctx.db
            .write(move |db| repo::messages::set_body(db, &id, None, None, None))
            .await
            .expect("clear body");
    }

    mock.state.bump_uidvalidity("INBOX", 99);
    let mut connection = connect_account(&account).await;
    let error = nuncio_mail::sync::body::fetch_body(&ctx, &mut connection, "acc", &message_id)
        .await
        .expect_err("the UID belongs to another generation");
    assert_eq!(error.code(), "conflict");

    let stored = {
        let id = message_id.clone();
        ctx.db
            .read(move |db| repo::messages::get(db, &id))
            .await
            .expect("stored")
            .expect("row")
    };
    assert!(
        stored.body_path.is_none(),
        "no body may be cached from an unverified generation"
    );
}

#[tokio::test]
async fn load_older_is_a_no_op_for_pop3() {
    let mock = MockPop3::start().await;
    mock.add(
        "uid-0",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "Pop",
            "body",
            "pop-old@x",
        ),
    );
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        None,
        None,
        Some(server(mock.addr.port())),
    );
    insert_account(&ctx, &account).await;
    nuncio_mail::pop3::sync::sync_account(&ctx, &account)
        .await
        .expect("pop3 sync");
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let top = mock.counters.top();
    let retr = mock.counters.retr();

    let engine = Engine::new(ctx.clone());
    let result = engine.load_older(&inbox).await.expect("pop3 no-op");
    assert_eq!((result.loaded, result.has_more), (0, false));
    assert_eq!(mock.counters.top(), top);
    assert_eq!(mock.counters.retr(), retr);
}

#[tokio::test]
async fn load_older_graph_resumes_below_the_oldest_stored_message() {
    let mock = support::mock_graph::MockGraph::start(250).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut account = account("acc", "test@example.com", None, None, None);
    account.provider = ProviderKind::Outlook;
    insert_account(&ctx, &account).await;
    let folder = nuncio_core::models::Folder::new(
        "f-inbox",
        "acc",
        "Inbox",
        nuncio_core::models::FolderRole::Inbox,
    );
    {
        let folder = folder.clone();
        ctx.db
            .write(move |db| {
                repo::folders::insert(db, &folder)?;
                repo::sync_state::set(db, "acc", "mail", "graph_folder:Inbox", "AAMk-inbox")
            })
            .await
            .expect("seed folder");
    }
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let load = || async {
        nuncio_mail::sync::messages::load_older_graph_with(&ctx, &client, &account, &folder)
            .await
            .expect("graph load older")
    };

    // No local rows yet: the newest page, no date bound.
    let first = load().await;
    assert_eq!((first.loaded, first.has_more), (100, true));
    // Second click is bounded by the oldest stored message (boundary row is
    // re-sent once and not counted as new).
    let second = load().await;
    assert_eq!((second.loaded, second.has_more), (99, true));
    let third = load().await;
    assert_eq!((third.loaded, third.has_more), (51, false));
    // Everything is local; a further click finds only the boundary row.
    let fourth = load().await;
    assert_eq!((fourth.loaded, fourth.has_more), (0, false));
    assert_eq!(message_count(&ctx, &folder.id).await, 250);

    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    assert!(requests
        .iter()
        .all(|url| url.starts_with("/me/mailFolders/AAMk-inbox/messages?")));
    assert!(!requests[0].contains("filter"));
    assert!(requests[1].contains("%24filter=receivedDateTime%20le%20"));
    assert!(requests[1].contains("%24orderby=receivedDateTime%20desc"));
    assert!(requests[1].contains("%24top=100"));
}

/// NetEase 163 never reports UIDNEXT. Before the fix a stored UIDNEXT of NULL
/// made every sync look like a first sync (full window scan, no increments)
/// and made "load older" refuse with "not synced yet".
#[tokio::test]
async fn a_server_without_uidnext_still_gets_incremental_sync() {
    let mock = MockImap::start(&["IDLE", "UIDPLUS"]).await;
    mock.state.set_omit_uidnext(true);
    seed_inbox(&mock, 5);
    let (ctx, account, inbox) = synced_inbox(&mock).await;
    assert_eq!(message_count(&ctx, &inbox).await, 5);

    // The stand-in (highest UID + 1) is stored, so the next sync is incremental.
    let (_, uidnext, _) = ctx
        .db
        .read({
            let inbox = inbox.clone();
            move |db| nuncio_core::repo::folders::sync_flags(db, &inbox)
        })
        .await
        .expect("sync flags");
    assert_eq!(uidnext, Some(6));

    // A new message arrives; the second sync must fetch only that one.
    let before = mock.state.commands().len();
    seed_inbox(&mock, 6);
    sync_account(&ctx, &account).await;
    assert_eq!(message_count(&ctx, &inbox).await, 11);
    let later: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        later
            .iter()
            .any(|command| command.contains("UID SEARCH UID 6:*")),
        "expected an incremental UID range, got {later:?}"
    );
}

#[tokio::test]
async fn load_older_works_when_the_server_reports_no_uidnext() {
    let mock = MockImap::start(&["IDLE", "UIDPLUS"]).await;
    mock.state.set_omit_uidnext(true);
    seed_inbox(&mock, 450);
    let (ctx, _account, inbox) = synced_inbox(&mock).await;
    forget_local_up_to(&ctx, &inbox, 300).await;

    let engine = Engine::new(ctx.clone());
    engine.start().await.expect("engine start");
    let first = engine
        .load_older(&inbox)
        .await
        .expect("load older must not need a stored UIDNEXT");
    assert_eq!((first.loaded, first.has_more), (50, true));
}

/// With nothing stored there is nothing to derive UIDNEXT from. Storing 1 would
/// make the next incremental sync ask for `UID 1:*`, i.e. the whole history
/// beyond the sync window.
#[tokio::test]
async fn an_empty_folder_on_a_server_without_uidnext_stores_no_uidnext() {
    let mock = MockImap::start(&["IDLE", "UIDPLUS"]).await;
    mock.state.set_omit_uidnext(true);
    let (ctx, _account, inbox) = synced_inbox(&mock).await;
    assert_eq!(message_count(&ctx, &inbox).await, 0);
    let (_, uidnext, _) = ctx
        .db
        .read({
            let inbox = inbox.clone();
            move |db| nuncio_core::repo::folders::sync_flags(db, &inbox)
        })
        .await
        .expect("sync flags");
    assert_eq!(uidnext, None, "an unknown UIDNEXT must stay unknown");
}

/* ─────────────── IMAP command timeouts and worker supervision ─────────────── */

fn test_imap_auth() -> MailAuth {
    MailAuth::Password {
        username: "test@example.com".into(),
        password: "secret".into(),
    }
}

/// ① A command the server receives but never acknowledges must fail with a
/// retryable network error inside the configured budget, and the connection
/// must be poisoned so no later command can consume its half-read response.
#[tokio::test]
async fn imap_command_timeout_poisons_the_connection() {
    let mock = MockImap::start(&["CONDSTORE"]).await;
    let timeouts = nuncio_mail::imap::ConnectTimeouts {
        command: Duration::from_millis(150),
        // Keep the socket watchdog out of the way: this test is about the
        // command budget, not about IO inactivity.
        io_read: Duration::from_secs(30),
        ..nuncio_mail::imap::ConnectTimeouts::default()
    };
    let mut connection = nuncio_mail::imap::connect_with_timeouts(
        &server(mock.addr.port()),
        &test_imap_auth(),
        timeouts,
    )
    .await
    .expect("connect mock imap");

    mock.state.hang_next_command("NOOP");
    let started = Instant::now();
    let error = connection.noop().await.expect_err("NOOP must time out");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "command returned after {elapsed:?}, not within its budget"
    );
    assert_eq!(error.code(), "network");
    assert!(
        nuncio_mail::util::is_command_timeout(&error),
        "expected a retryable command-timeout error, got {error:?}"
    );
    assert!(connection.is_poisoned(), "connection must be poisoned");

    // A poisoned connection must refuse further commands without writing to
    // the old socket.
    let before = mock.state.commands_on_connection(0).len();
    let refused = connection.noop().await.expect_err("poisoned connection");
    assert!(
        nuncio_mail::util::is_command_timeout(&refused),
        "reuse must be reported as a retryable timeout, got {refused:?}"
    );
    assert_eq!(
        mock.state.commands_on_connection(0).len(),
        before,
        "a poisoned connection must not receive another command"
    );
}

/// ② After a sync command times out, the worker rebuilds the connection and
/// the old socket receives nothing further; the retried sync completes on the
/// fresh connection.
#[tokio::test]
async fn command_timeout_reconnects_on_a_fresh_connection() {
    let mock = MockImap::start(&["CONDSTORE"]).await;
    mock.state.add_message(
        "INBOX",
        &message_raw("a@example.com", "test@example.com", "One", "one", "one@x"),
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

    let config = EngineConfig {
        command_timeout: Duration::from_millis(200),
        // Keep the IDLE/poll/keepalive timers out of the picture: only the
        // worker connection may be opened by this test.
        keepalive_interval: Duration::from_secs(3600),
        no_idle_poll_interval: Duration::from_secs(3600),
        ..EngineConfig::default()
    };
    let engine = Engine::with_config(ctx.clone(), config);
    engine.start().await.expect("engine start");
    // The first SELECT hangs; the retry (same account command) must run on a
    // new worker connection and succeed.
    mock.state.hang_next_command("SELECT");
    engine
        .sync_now(Some("acc"), None)
        .await
        .expect("sync retries on a new connection");

    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    assert_eq!(message_count(&ctx, &inbox).await, 1);
    assert!(
        mock.state.connection_count() >= 2,
        "worker should have rebuilt the connection"
    );

    let first = mock.state.commands_on_connection(0);
    let hung = first
        .iter()
        .position(|command| command.starts_with("SELECT"))
        .expect("first connection received the SELECT");
    assert_eq!(
        hung,
        first.len() - 1,
        "old connection received {} after the hung SELECT: {:?}",
        first.len() - 1 - hung,
        &first[hung + 1..]
    );
    let second = mock.state.commands_on_connection(1);
    assert!(
        second.iter().any(|command| command.starts_with("SELECT")),
        "new connection should carry the retried SELECT, got {second:?}"
    );
    engine.shutdown().await;
}

/// ③ A long IDLE wait is not interrupted by the ordinary command or socket
/// timeouts: the waiting connection stays open far past both budgets.
#[tokio::test]
async fn idle_wait_is_exempt_from_command_and_io_timeouts() {
    let mock = MockImap::start(&["IDLE", "CONDSTORE"]).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut account = account(
        "acc",
        "test@example.com",
        Some(server(mock.addr.port())),
        None,
        None,
    );
    // The persisted flag makes the idle loop open its dedicated connection;
    // the live CAPABILITY confirms IDLE.
    account.capabilities.idle = true;
    insert_account(&ctx, &account).await;

    let config = EngineConfig {
        command_timeout: Duration::from_millis(100),
        io_read_timeout: Duration::from_millis(100),
        io_write_timeout: Duration::from_millis(100),
        keepalive_interval: Duration::from_secs(3600),
        no_idle_poll_interval: Duration::from_secs(3600),
        ..EngineConfig::default()
    };
    let engine = Engine::with_config(ctx.clone(), config);
    engine.start().await.expect("engine start");

    // Engine start also requests a sync, so wait until the dedicated IDLE
    // connection has issued IDLE and the worker connection has settled.
    let mut idling = false;
    for _ in 0..200 {
        if mock
            .state
            .commands()
            .iter()
            .any(|command| command.starts_with("IDLE"))
        {
            idling = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(idling, "IDLE was never issued");
    let open_before = mock.state.open_connection_count();
    let accepted_before = mock.state.connection_count();
    assert!(open_before >= 1, "IDLE connection should be open");

    // Several multiples of both budgets. If either watchdog applied to the
    // IDLE read, the IDLE connection would close; without a reconnect before
    // the backoff expires, the open count must drop and the loop would later
    // accept another connection.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        mock.state.open_connection_count(),
        open_before,
        "an IMAP connection was interrupted by a timeout during IDLE"
    );
    assert_eq!(
        mock.state.connection_count(),
        accepted_before,
        "IDLE loop reconnected instead of waiting"
    );
    engine.shutdown().await;
}

/// ④ A normal command path is unaffected: the new budgets are far above the
/// mock's latency, the connection stays usable and results are unchanged.
#[tokio::test]
async fn normal_commands_are_not_affected_by_the_new_timeouts() {
    let mock = MockImap::start(&["CONDSTORE", "UIDPLUS"]).await;
    mock.state.add_message(
        "INBOX",
        &message_raw("a@example.com", "test@example.com", "One", "one", "one@x"),
    );
    let mut connection = nuncio_mail::imap::connect_with_timeouts(
        &server(mock.addr.port()),
        &test_imap_auth(),
        nuncio_mail::imap::ConnectTimeouts::default(),
    )
    .await
    .expect("connect mock imap");

    let started = Instant::now();
    connection.noop().await.expect("NOOP");
    let mailbox = connection.select("INBOX").await.expect("SELECT");
    assert_eq!(mailbox.exists, 1);
    let found = connection
        .uid_search("ALL")
        .await
        .expect("UID SEARCH succeeds");
    assert_eq!(found, [1].into_iter().collect());
    assert!(!connection.is_poisoned(), "healthy path must not poison");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "normal commands took {:?}",
        started.elapsed()
    );
}

/// Regression: a checked `UID SEARCH` does the same server-side work as the
/// typed one and must use the FETCH/SEARCH budget, not the interactive one.
/// The mock holds this SEARCH for 300ms while `command` is 100ms and `fetch`
/// is 2s. The tagged-OK check must still be in place (the result is asserted,
/// not just `is_ok`).
#[tokio::test]
async fn audit_imap_checked_search_uses_the_fetch_search_budget() {
    let mock = MockImap::start(&["CONDSTORE"]).await;
    mock.state.add_message(
        "INBOX",
        &message_raw(
            "a@example.com",
            "test@example.com",
            "audit-slow-search",
            "slow search",
            "audit-slow-search@x",
        ),
    );
    let timeouts = nuncio_mail::imap::ConnectTimeouts {
        command: Duration::from_millis(100),
        fetch: Duration::from_secs(2),
        io_read: Duration::from_secs(10),
        ..Default::default()
    };
    let mut conn = nuncio_mail::imap::connect_with_timeouts(
        &server(mock.addr.port()),
        &test_imap_auth(),
        timeouts,
    )
    .await
    .expect("connect mock imap");
    conn.select("INBOX").await.expect("SELECT");

    let expected = conn
        .uid_search("HEADER SUBJECT audit-slow-search")
        .await
        .expect("normal SEARCH is within its 2s budget");
    assert_eq!(expected, [1].into_iter().collect());

    let checked = conn
        .uid_search_checked("HEADER SUBJECT audit-slow-search")
        .await
        .expect("checked SEARCH must share the FETCH/SEARCH budget");
    assert_eq!(
        checked, expected,
        "the checked path must return the same tagged-OK result"
    );
    assert!(
        !conn.is_poisoned(),
        "a 300ms SEARCH must not poison a connection whose fetch budget is 2s"
    );
}

/// Regression: unsolicited chatter between APPEND reads must not renew the
/// total budget. The mock sends eight `EXISTS` (40ms apart, 320ms in total)
/// after receiving the literal, while both budgets are shorter. The APPEND
/// must give up at the absolute deadline, poison the connection and report a
/// retryable error that remembers the literal was already sent.
#[tokio::test]
async fn audit_imap_append_chatter_does_not_extend_the_whole_command_budget() {
    let mock = MockImap::start(&["UIDPLUS"]).await;
    let timeouts = nuncio_mail::imap::ConnectTimeouts {
        command: Duration::from_millis(100),
        fetch: Duration::from_millis(120),
        io_read: Duration::from_secs(10),
        io_write: Duration::from_secs(10),
        ..Default::default()
    };
    let mut conn = nuncio_mail::imap::connect_with_timeouts(
        &server(mock.addr.port()),
        &test_imap_auth(),
        timeouts,
    )
    .await
    .expect("connect mock imap");
    let raw = message_raw(
        "a@example.com",
        "test@example.com",
        "Append budget",
        "AUDIT_APPEND_CHATTER",
        "append-budget@test",
    );

    let started = Instant::now();
    let error = conn
        .append_with_uid("INBOX", None, &raw)
        .await
        .expect_err("untagged EXISTS must not renew APPEND's total budget");
    assert!(
        conn.is_poisoned(),
        "an expired APPEND budget must poison the connection"
    );
    assert!(
        error.sent,
        "the literal reached the socket, so the failure must stay `sent`"
    );
    assert!(
        nuncio_mail::util::is_command_timeout(&error.error),
        "the failure must stay retryable, got {:?}",
        error.error
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "APPEND was renewed by chatter; took {:?}",
        started.elapsed()
    );
}

/// Raw Sent copy for the duplicate-copy regressions below. `bcc` is what the
/// 163 APPEND copy restores and the server copy omits.
fn audit_sent_raw(header: &str, bcc: Option<&str>) -> Vec<u8> {
    let date = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S +0000");
    let bcc = bcc
        .map(|address| format!("Bcc: {address}\r\n"))
        .unwrap_or_default();
    format!(
        "From: test@example.com\r\nTo: bob@example.com\r\n{bcc}\
         Subject: Audit sent copy\r\nDate: {date}\r\n\
         Message-ID: <{header}>\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\nbody\r\n"
    )
    .into_bytes()
}

/// An exact UID/UIDVALIDITY binding under a preserved local id must be reused
/// even when the same batch carries a second copy with the same Message-ID.
/// Refusing it used to insert the deterministic id over an existing
/// `(folder_id, uid)` and roll the whole sync back.
#[tokio::test]
async fn audit_sent_exact_uid_binding_survives_a_duplicate_header_batch() {
    let mock = MockImap::start_with_folders(
        &["IDLE", "CONDSTORE", "MOVE", "UIDPLUS", "SPECIAL-USE"],
        &[("INBOX", &["\\Inbox"]), ("Sent", &["\\Sent"])],
    )
    .await;
    mock.state.add_message(
        "INBOX",
        &audit_sent_raw("rebound@audit", Some("private@example.com")),
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
    sync_account(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let sent = folder_id(&ctx, "acc", "Sent").await;
    let local_id = message_ids(&ctx, &inbox).await.remove(0);

    let service = nuncio_mail::MailService::new(ctx.clone());
    service
        .message_act(
            std::slice::from_ref(&local_id),
            &nuncio_core::models::MessageAction::Move {
                folder_id: sent.clone(),
            },
        )
        .await
        .expect("queue move");
    let mut connection = connect_account(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("run move");
    assert_eq!(outcome.failed, 0);
    sync_account(&ctx, &account).await;
    let moved = ctx
        .db
        .read({
            let id = local_id.clone();
            move |db| repo::messages::get_required(db, &id)
        })
        .await
        .expect("moved row");
    assert_eq!(moved.summary.folder_id, sent);
    assert!(moved.uid.is_some());

    // The provider's own second copy, same Message-ID, different UID.
    mock.state
        .add_message("Sent", &audit_sent_raw("rebound@audit", None));
    sync_account(&ctx, &account).await;

    // Production `force_full_resync` preserves rows and clears
    // UIDNEXT/MODSEQ. Reproduce that same write before a metadata pass that
    // contains both UIDs.
    ctx.db
        .write({
            let sent = sent.clone();
            move |db| {
                let (validity, _, _) = repo::folders::sync_flags(db, &sent)?;
                repo::folders::set_sync_flags(db, &sent, validity, None, None)
            }
        })
        .await
        .expect("force full resync flags");
    let mut connection = connect_account(&account).await;
    let folder = ctx
        .db
        .read({
            let id = sent.clone();
            move |db| repo::folders::get_required(db, &id)
        })
        .await
        .expect("sent folder");
    let result =
        nuncio_mail::sync::messages::sync_folder(&ctx, &mut connection, &account, &folder).await;
    assert!(
        result.is_ok(),
        "an exact UID already bound under a preserved local id must be reusable even when two batch entries share its Message-ID: {result:?}"
    );
}
