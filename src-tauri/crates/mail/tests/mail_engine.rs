//! Integration tests for the mail engine against in-process mock servers.

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nuncio_core::models::{
    MailScope, MessageListQuery, OutboxItem, ProviderKind, ServerConfig, SyncModule, SyncState,
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

    ops::enqueue_set_flags(
        &ctx,
        "acc",
        &inbox,
        &[uid as i64],
        Some(true),
        Some(true),
        None,
        None,
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

    ops::enqueue_move(&ctx, "acc", &inbox, &archive, &[uid as i64], "archive it")
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

    ops::enqueue_archive(&ctx, "acc", &inbox, &[uid as i64], "archive")
        .await
        .expect("enqueue archive");
    let mut connection = connect_account(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("execute archive");
    assert_eq!(mock.state.message_count("INBOX"), 0);
    assert_eq!(mock.state.message_count("Archive"), 1);
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
        .write(move |db| repo::messages::set_body(db, &message_id, None, None))
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

    ops::enqueue_move(&ctx, "acc", &inbox, "does-not-exist", &[1], "bad move")
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
    for index in 0..count {
        mock.state.add_message(
            "INBOX",
            &message_raw(
                "a@example.com",
                "test@example.com",
                &format!("Older {index}"),
                "body",
                &format!("older-{index}@x"),
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
    assert_eq!((first.loaded, first.has_more), (200, true));
    assert_eq!(min_uid(&ctx, &inbox).await, Some(101));
    assert_eq!(message_count(&ctx, &inbox).await, 350);

    let second = engine.load_older(&inbox).await.expect("second batch");
    assert_eq!((second.loaded, second.has_more), (100, false));
    assert_eq!(min_uid(&ctx, &inbox).await, Some(1));
    assert_eq!(message_count(&ctx, &inbox).await, 450);

    // Exhausted: UID 1 is stored, nothing below it, no request needed.
    let third = engine.load_older(&inbox).await.expect("third batch");
    assert_eq!((third.loaded, third.has_more), (0, false));

    // Unseen state is untouched: no STORE, no non-PEEK body fetch, and the
    // folder's unread counter covers every loaded row.
    let commands = mock.state.commands();
    assert!(
        commands.iter().all(|command| !command.contains(" STORE ")
            && !command.starts_with("STORE ")),
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
    let result = nuncio_mail::sync::messages::load_older_imap(&ctx, &mut connection, &account, &folder)
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
    let result = nuncio_mail::sync::messages::load_older_imap(&ctx, &mut connection, &account, &folder)
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
    let error = nuncio_mail::sync::messages::load_older_imap(&ctx, &mut connection, &account, &folder)
        .await
        .expect_err("uidvalidity changed");
    assert_eq!(error.code(), "conflict");
    // Nothing destructive happened.
    assert_eq!(message_count(&ctx, &inbox).await, before);
    assert_eq!(min_uid(&ctx, &inbox).await, Some(6));
}

#[tokio::test]
async fn load_older_is_a_no_op_for_pop3() {
    let mock = MockPop3::start().await;
    mock.add(
        "uid-0",
        &message_raw("a@example.com", "test@example.com", "Pop", "body", "pop-old@x"),
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
    assert_eq!(message_count(&ctx, &inbox).await, 6);
    let later: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        later.iter().any(|command| command.contains("UID SEARCH UID 6:*")),
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
    assert_eq!((first.loaded, first.has_more), (200, true));
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
