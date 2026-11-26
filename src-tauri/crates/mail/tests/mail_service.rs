//! Integration tests for `nuncio_mail::MailService` against the in-process
//! mock IMAP / SMTP servers and an in-memory database.

mod support;

use std::sync::Arc;
use std::time::Duration;

use nuncio_core::models::{
    Account, Address, Folder, FolderRole, MailFilter, MailRule, MailScope, MailSettingsPatch,
    MessageAction, MessageListQuery, MessageSummary, OAuthProvider, ProviderCapabilities,
    ProviderKind, ReplyMode, RuleAction, RuleActionKind, RuleCondition, RuleField, RuleOp,
    SearchQuery, ServerConfig, SettingsPatch, Signature, StoredMessage,
};
use nuncio_core::traits::MailAuth;
use nuncio_core::{repo, CoreContext};
use nuncio_mail::{ops, MailService};
use support::mock_graph::MockGraph;
use support::mock_imap::MockImap;
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

async fn connect(account: &Account) -> nuncio_mail::imap::ImapConnection {
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

async fn summaries(ctx: &CoreContext, folder_id: &str) -> Vec<nuncio_core::models::MessageSummary> {
    let folder_id = folder_id.to_string();
    let query = MessageListQuery::new(MailScope::folder(folder_id))
        .with_conversation(false)
        .with_limit(100);
    ctx.db
        .read(move |db| repo::messages::list(db, &query))
        .await
        .expect("list")
        .items
}

async fn only_message(ctx: &CoreContext, folder_id: &str) -> nuncio_core::models::MessageSummary {
    let items = summaries(ctx, folder_id).await;
    assert_eq!(items.len(), 1, "expected exactly one message");
    items.into_iter().next().expect("message")
}

fn raw_with_parts(from: &str, to: &str, subject: &str, message_id: &str) -> Vec<u8> {
    let inline_png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    let calendar = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Nuncio Test//EN\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:invite-1@example.com\r\nDTSTAMP:20261112T080000Z\r\nDTSTART:20261113T090000Z\r\nDTEND:20261113T100000Z\r\nSUMMARY:Project sync\r\nORGANIZER;CN=Alice:mailto:alice@example.com\r\nATTENDEE;CN=Me;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:test@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    format!(
        "From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nDate: Thu, 12 Nov 2026 08:00:00 +0000\r\nMessage-ID: <{message_id}>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b1\"\r\n\r\n--b1\r\nContent-Type: multipart/alternative; boundary=\"b2\"\r\n\r\n--b2\r\nContent-Type: text/plain; charset=utf-8\r\n\r\ninvitation body text\r\n\r\n--b2\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<html><body><p>invitation <b>body</b><img src=\"https://remote.example/banner.png\"></p><p><img src=\"cid:inline-1@nuncio\"></p></body></html>\r\n\r\n--b2--\r\n--b1\r\nContent-Type: image/png; name=\"pixel.png\"\r\nContent-Disposition: inline; filename=\"pixel.png\"\r\nContent-ID: <inline-1@nuncio>\r\nContent-Transfer-Encoding: base64\r\n\r\n{inline}\r\n\r\n--b1\r\nContent-Type: text/plain; name=\"report.txt\"\r\nContent-Disposition: attachment; filename=\"report.txt\"\r\nContent-Transfer-Encoding: base64\r\n\r\ncmVwb3J0IGJvZHk=\r\n\r\n--b1\r\nContent-Type: text/calendar; method=REQUEST; charset=utf-8\r\n\r\n{calendar}\r\n--b1--\r\n",
        inline = inline_png,
    )
    .into_bytes()
}

#[tokio::test]
async fn lists_searches_reads_and_renders_messages() {
    let mock = MockImap::start_with_folders(
        &["IDLE", "CONDSTORE", "MOVE", "UIDPLUS", "SPECIAL-USE"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Sent", &["\\Sent"]),
            ("Drafts", &["\\Drafts"]),
            ("Trash", &["\\Trash"]),
            ("Archive", &["\\Archive"]),
            ("Junk", &["\\Junk"]),
        ],
    )
    .await;
    mock.state.add_message(
        "INBOX",
        &raw_with_parts(
            "Alice <alice@example.com>",
            "test@example.com",
            "Project invitation",
            "invite@example.com",
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
    sync_once(&ctx, &account).await;
    // The product default is "always" (matching the Outlook/163 web clients);
    // this test asserts the blocking path, so opt into "ask" explicitly.
    ctx.settings
        .update(SettingsPatch {
            mail: Some(MailSettingsPatch {
                remote_images: Some(nuncio_core::models::RemoteImagesPolicy::Ask),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect("settings");
    let service = MailService::new(ctx.clone());
    let inbox = folder_id(&ctx, "acc", "INBOX").await;

    let folders = service.folder_list("acc").await.expect("folders");
    assert!(folders
        .iter()
        .any(|folder| folder.role == nuncio_core::models::FolderRole::Trash));
    let counts = service.folder_unified_counts().await.expect("counts");
    assert_eq!(counts.inbox, 1);

    let page = service
        .message_list(
            &MessageListQuery::new(MailScope::folder(inbox.clone()))
                .with_conversation(false)
                .with_limit(20),
        )
        .await
        .expect("list");
    assert_eq!(page.items.len(), 1);
    let summary = page.items[0].clone();

    // Local FTS search.
    let search = SearchQuery {
        text: "invitation".into(),
        ..SearchQuery::default()
    };
    let searched = service
        .message_list(
            &MessageListQuery::new(MailScope::search(search))
                .with_conversation(false)
                .with_limit(20),
        )
        .await
        .expect("search");
    assert_eq!(searched.items.len(), 1);

    // Server search fallback (the engine is not running in this test).
    let server_search = SearchQuery {
        text: "invitation".into(),
        include_server: true,
        ..SearchQuery::default()
    };
    let searched = service
        .message_list(
            &MessageListQuery::new(MailScope::search(server_search))
                .with_conversation(false)
                .with_limit(20),
        )
        .await
        .expect("server search");
    assert_eq!(searched.items.len(), 1);

    let detail = service
        .message_get(&summary.id, false)
        .await
        .expect("detail");
    assert!(detail.summary.seen, "opening a message marks it read");

    // A view-only re-render (the reader's "view on white" toggle) must not mark
    // the message read again: the user may have marked it unread since opening.
    service
        .message_act(
            std::slice::from_ref(&summary.id),
            &MessageAction::SetSeen { value: false },
        )
        .await
        .expect("mark unread");
    let rerendered = service
        .message_get_with(&summary.id, false, true)
        .await
        .expect("re-render on white");
    assert!(
        !rerendered.summary.seen,
        "a rendering-only request must not undo mark-as-unread"
    );
    service
        .message_get(&summary.id, false)
        .await
        .expect("open again");
    assert!(detail.has_remote_content);
    assert!(!detail.remote_content_allowed);
    assert!(!detail.html.contains(" src=\"https://remote.example"));
    assert!(detail.html.contains("data-src=\"https://remote.example"));
    // The reader iframe is served from a Tauri custom protocol whose URL
    // shape is platform dependent: Windows only routes `http://<scheme>.localhost`,
    // everywhere else `nuncio://localhost`.  `MailService::cid_base` mirrors
    // that, so the expectation has to as well.
    let cid_prefix = if cfg!(target_os = "windows") {
        "http://nuncio.localhost/cid/"
    } else {
        "nuncio://localhost/cid/"
    };
    assert!(detail.html.contains(cid_prefix), "html: {}", detail.html);
    assert!(detail.invite.is_some());
    let invite = detail.invite.as_ref().expect("invite");
    assert_eq!(invite.method, "REQUEST");
    assert!(invite.can_respond);
    assert_eq!(
        invite.my_response,
        Some(nuncio_core::models::AttendeeStatus::NeedsAction)
    );
    assert_eq!(detail.attachments.len(), 2);
    let inline = detail
        .attachments
        .iter()
        .find(|attachment| attachment.is_inline)
        .expect("inline attachment");
    let (mime, bytes) = service
        .resolve_cid(&summary.id, "inline-1@nuncio")
        .await
        .expect("cid bytes");
    assert_eq!(mime, inline.mime_type);
    assert!(!bytes.is_empty());

    // Trusting the sender allows remote content.
    service
        .message_trust_sender("alice@example.com")
        .await
        .expect("trust");
    let detail = service
        .message_get(&summary.id, false)
        .await
        .expect("detail 2");
    assert!(detail.remote_content_allowed);
    assert!(detail.html.contains("https://remote.example"));

    // Explicit override wins regardless of policy.
    ctx.settings
        .update(SettingsPatch {
            mail: Some(MailSettingsPatch {
                remote_images: Some(nuncio_core::models::RemoteImagesPolicy::Always),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect("settings");
    let detail = service
        .message_get(&summary.id, false)
        .await
        .expect("detail always");
    assert!(detail.remote_content_allowed);
    let missing = service
        .message_get("missing-message", true)
        .await
        .expect_err("unknown message");
    assert_eq!(missing.code(), "notFound");
}

/// Local/POP3 accounts never queue a server move, so nothing will ever
/// re-associate a target UID. They must not enter the `target_pending` state:
/// doing so used to refuse every later action on the message.
#[tokio::test]
async fn local_accounts_keep_acting_after_repeated_moves() {
    for provider in [ProviderKind::Local, ProviderKind::Pop3] {
        let ctx = context(
            StaticCreds::password("me@local.test", "secret"),
            Arc::new(RecordingNotifier::default()),
        )
        .await;
        let mut account = account("acc", "me@local.test", None, None, None);
        account.provider = provider;
        account.capabilities = ProviderCapabilities::defaults_for(provider);
        insert_account(&ctx, &account).await;
        let inbox = Folder::new("inbox", "acc", "INBOX", FolderRole::Inbox);
        let archive = Folder::new("archive", "acc", "Archive", FolderRole::Archive);
        ctx.db
            .write(move |db| {
                repo::folders::upsert_remote(db, &inbox)?;
                repo::folders::upsert_remote(db, &archive)?;
                Ok(())
            })
            .await
            .expect("folders");
        insert_stored(&ctx, "acc", "m1", "inbox", false).await;

        let service = MailService::new(ctx.clone());
        let move_to = |folder: &str| MessageAction::Move {
            folder_id: folder.to_string(),
        };

        service
            .message_act(&["m1".to_string()], &move_to("archive"))
            .await
            .expect("first move");
        service
            .message_act(&["m1".to_string()], &MessageAction::SetSeen { value: true })
            .await
            .expect("flag after a move must not be blocked");
        service
            .message_act(&["m1".to_string()], &move_to("inbox"))
            .await
            .expect("second move");
        service
            .message_act(
                &["m1".to_string()],
                &MessageAction::Delete { permanent: false },
            )
            .await
            .expect("delete after a move");

        let stored = ctx
            .db
            .read(move |db| {
                let pending = repo::messages::target_pending_ids(db, &["m1".to_string()])?;
                Ok((repo::messages::get_required(db, "m1")?, pending))
            })
            .await
            .expect("stored");
        assert!(stored.0.is_deleted_local, "delete still applies locally");
        assert!(
            stored.1.is_empty(),
            "{provider:?} accounts must never be marked target_pending"
        );
        let ops = ctx
            .db
            .read(move |db| repo::pending_ops::list(db, Some("acc")))
            .await
            .expect("ops");
        assert!(
            ops.is_empty(),
            "{provider:?} accounts queue no server operations: {ops:?}"
        );
    }
}

/// One action can touch messages whose cached UIDs belong to different
/// UIDVALIDITY generations (a folder was rebuilt between two syncs). Each
/// payload must carry only its own generation, so a valid row cannot vouch for
/// a stale UID.
#[tokio::test]
async fn mixed_uidvalidity_batch_is_split_per_generation() {
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account("acc", "test@example.com", None, None, None);
    insert_account(&ctx, &account).await;
    let inbox = Folder::new("inbox", "acc", "INBOX", FolderRole::Inbox);
    let archive = Folder::new("archive", "acc", "Archive", FolderRole::Archive);
    ctx.db
        .write(move |db| {
            repo::folders::upsert_remote(db, &inbox)?;
            repo::folders::upsert_remote(db, &archive)?;
            for (id, uid, uidvalidity) in [("m1", 10i64, 1i64), ("m2", 20, 2)] {
                let mut summary = MessageSummary::new(
                    id,
                    "acc",
                    "inbox",
                    format!("thread:{id}"),
                    chrono::Utc::now(),
                );
                summary.seen = false;
                let mut stored = StoredMessage::new(summary);
                stored.uid = Some(uid);
                stored.uidvalidity = Some(uidvalidity);
                repo::messages::upsert(db, &stored)?;
            }
            Ok(())
        })
        .await
        .expect("seed");

    let service = MailService::new(ctx.clone());
    service
        .message_act(
            &["m1".to_string(), "m2".to_string()],
            &MessageAction::Move {
                folder_id: "archive".to_string(),
            },
        )
        .await
        .expect("mixed move");

    let ops = ctx
        .db
        .read(move |db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops");
    let mut pairs: Vec<(i64, Option<i64>)> = ops
        .iter()
        .filter(|op| op.kind == ops::KIND_MOVE)
        .map(|op| {
            let payload: ops::MoveOp = repo::pending_ops::payload(op).expect("payload");
            assert_eq!(payload.uids.len(), 1, "generations must not be merged");
            (payload.uids[0], payload.source_uidvalidity)
        })
        .collect();
    pairs.sort();
    assert_eq!(pairs, vec![(10, Some(1)), (20, Some(2))]);

    let pending = ctx
        .db
        .read(move |db| {
            repo::messages::target_pending_ids(db, &["m1".to_string(), "m2".to_string()])
        })
        .await
        .expect("pending");
    assert_eq!(
        pending.len(),
        2,
        "both rows queue a server move and wait for the target sync"
    );

    // Flag batches are grouped the same way: two generations must not share a
    // payload that claims one UIDVALIDITY.
    ctx.db
        .write(|db| {
            for (id, uid, uidvalidity) in [("m3", 30i64, 1i64), ("m4", 40, 2)] {
                let mut summary = MessageSummary::new(
                    id,
                    "acc",
                    "inbox",
                    format!("thread:{id}"),
                    chrono::Utc::now(),
                );
                summary.seen = false;
                let mut stored = StoredMessage::new(summary);
                stored.uid = Some(uid);
                stored.uidvalidity = Some(uidvalidity);
                repo::messages::upsert(db, &stored)?;
            }
            Ok(())
        })
        .await
        .expect("seed flags");
    service
        .message_act(
            &["m3".to_string(), "m4".to_string()],
            &MessageAction::SetSeen { value: true },
        )
        .await
        .expect("mixed flag batch");
    let ops = ctx
        .db
        .read(move |db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops");
    let mut flag_pairs: Vec<(i64, Option<i64>)> = ops
        .iter()
        .filter(|op| op.kind == ops::KIND_SET_FLAGS)
        .map(|op| {
            let payload: ops::SetFlagsOp = repo::pending_ops::payload(op).expect("payload");
            assert_eq!(payload.uids.len(), 1);
            (payload.uids[0], payload.source_uidvalidity)
        })
        .collect();
    flag_pairs.sort();
    assert_eq!(flag_pairs, vec![(30, Some(1)), (40, Some(2))]);
}

#[tokio::test]
async fn message_actions_are_optimistic_and_undoable() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "MOVE", "UIDPLUS"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Sent", &["\\Sent"]),
            ("Drafts", &["\\Drafts"]),
            ("Trash", &["\\Trash"]),
            ("Archive", &["\\Archive"]),
            ("Junk", &["\\Junk"]),
        ],
    )
    .await;
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
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    let trash = folder_id(&ctx, "acc", "Trash").await;
    let junk = folder_id(&ctx, "acc", "Junk").await;
    let message = only_message(&ctx, &inbox).await;

    // Flag + undo.
    service
        .message_act(
            std::slice::from_ref(&message.id),
            &MessageAction::SetFlagged { value: true },
        )
        .await
        .expect("flag");
    let stored = ctx
        .db
        .read({
            let id = message.id.clone();
            move |db| repo::messages::get_required(db, &id)
        })
        .await
        .expect("stored");
    assert!(stored.summary.flagged);
    assert!(!ctx
        .db
        .read(move |db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops")
        .is_empty());
    assert!(service.message_undo_last().await.expect("undo"));
    let stored = ctx
        .db
        .read({
            let id = message.id.clone();
            move |db| repo::messages::get_required(db, &id)
        })
        .await
        .expect("stored");
    assert!(!stored.summary.flagged);
    let ops = ctx
        .db
        .read(move |db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops");
    assert!(ops
        .iter()
        .all(|op| op.state == nuncio_core::models::PendingOpState::Cancelled));

    // Move + undo restores the folder.
    service
        .message_act(
            std::slice::from_ref(&message.id),
            &MessageAction::Move {
                folder_id: archive.clone(),
            },
        )
        .await
        .expect("move");
    let moved = ctx
        .db
        .read({
            let id = message.id.clone();
            move |db| repo::messages::get_required(db, &id)
        })
        .await
        .expect("moved");
    assert_eq!(moved.summary.folder_id, archive);
    assert!(service.message_undo_last().await.expect("undo move"));
    let restored = ctx
        .db
        .read({
            let id = message.id.clone();
            move |db| repo::messages::get_required(db, &id)
        })
        .await
        .expect("restored");
    assert_eq!(restored.summary.folder_id, inbox);

    // Delete (to Trash) + undo; mark junk + undo.
    service
        .message_act(
            std::slice::from_ref(&message.id),
            &MessageAction::Delete { permanent: false },
        )
        .await
        .expect("delete");
    assert!(summaries(&ctx, &trash)
        .await
        .iter()
        .any(|item| item.id == message.id));
    assert!(service.message_undo_last().await.expect("undo delete"));
    service
        .message_act(
            std::slice::from_ref(&message.id),
            &MessageAction::MarkJunk { value: true },
        )
        .await
        .expect("junk");
    assert!(summaries(&ctx, &junk)
        .await
        .iter()
        .any(|item| item.id == message.id));
    assert!(service.message_undo_last().await.expect("undo junk"));

    // Thread action applies to every message in the conversation.
    mock.state.add_message(
        "INBOX",
        "From: bob@example.com\r\nTo: test@example.com\r\nSubject: Re: Flag me\r\nDate: Thu, 12 Nov 2026 09:00:00 +0000\r\nMessage-ID: <reply@example.com>\r\nIn-Reply-To: <flag@example.com>\r\nReferences: <flag@example.com>\r\nContent-Type: text/plain\r\n\r\nreply\r\n"
            .as_bytes(),
    );
    sync_once(&ctx, &account).await;
    let messages = summaries(&ctx, &inbox).await;
    assert_eq!(messages.len(), 2);
    let thread_id = messages[0].thread_id.clone();
    assert!(
        messages.iter().all(|item| item.thread_id == thread_id),
        "{:?}",
        messages
            .iter()
            .map(|item| (&item.subject, &item.thread_id))
            .collect::<Vec<_>>()
    );
    service
        .message_act_threads("acc", &[thread_id], &MessageAction::SetSeen { value: true })
        .await
        .expect("thread seen");
    assert!(summaries(&ctx, &inbox).await.iter().all(|item| item.seen));

    // Filtered listing (flagged) respects the contract filter.
    let page = service
        .message_list(
            &MessageListQuery::new(MailScope::folder(inbox))
                .with_conversation(false)
                .with_filter(MailFilter::Unread)
                .with_limit(10),
        )
        .await
        .expect("unread list");
    assert!(page.items.is_empty());
}

/// The rollback must read the operation's payload *inside* the write
/// transaction. The executor persists `deleted_flag_set` after the STORE and
/// before the EXPUNGE; a terminal EXPUNGE failure in that same pass would
/// unhide and reuse the UID if rollback trusted the pre-execution snapshot.
#[tokio::test]
async fn permanent_delete_rollback_reads_the_latest_side_effect_evidence() {
    let mock = MockImap::start_with_folders(
        &["UIDPLUS", "MOVE"],
        &[("INBOX", &["\\Inbox"]), ("Junk", &["\\Junk"])],
    )
    .await;
    mock.state.add_message(
        "INBOX",
        &message_raw(
            "alice@example.com",
            "test@example.com",
            "Latest evidence",
            "body",
            "latest@example.com",
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
    sync_once(&ctx, &account).await;
    let service = MailService::new(ctx.clone());
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let message = only_message(&ctx, &inbox).await;
    let original_uid = ctx
        .db
        .read({
            let id = message.id.clone();
            move |db| repo::messages::get_required(db, &id)
        })
        .await
        .expect("message")
        .uid;

    service
        .message_act(
            std::slice::from_ref(&message.id),
            &MessageAction::Delete { permanent: true },
        )
        .await
        .expect("delete");
    let op = ctx
        .db
        .read(|db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops")
        .into_iter()
        .next()
        .expect("op");
    // Make the very first EXPUNGE refusal terminal, so it happens in the same
    // pass that records `deleted_flag_set`.
    ctx.db
        .write({
            let id = op.id.clone();
            move |db| {
                db.execute(
                    "UPDATE pending_op SET attempts = 7, next_retry_at = NULL WHERE id = ?1",
                    rusqlite::params![id],
                )?;
                Ok(())
            }
        })
        .await
        .expect("make terminal");
    mock.state.set_fail_expunge(true);

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("terminal expunge failure");

    let row = ctx
        .db
        .read({
            let id = message.id.clone();
            move |db| repo::messages::get_required(db, &id)
        })
        .await
        .expect("row");
    assert!(
        row.is_deleted_local,
        "the STORE already reached the server; the row must stay hidden"
    );
    assert_eq!(
        row.uid, original_uid,
        "the old UID must not be reused after a side effect"
    );
    let failed = ctx
        .db
        .read(|db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops")
        .into_iter()
        .next()
        .expect("op");
    assert_eq!(failed.state, nuncio_core::models::PendingOpState::Failed);
    assert!(
        failed.send_intent,
        "the intent is persisted before the destructive command"
    );
    let payload = repo::pending_ops::payload::<ops::DeleteOp>(&failed).expect("payload");
    assert!(
        payload.deleted_flag_set,
        "the side effect stays recorded for undo and reconciliation"
    );
}

/// Undoing a permanent delete that has not run yet must clear `pending_delete`
/// in the same transaction that unhides the row. Leaving it set made every
/// later sync skip the message forever.
#[tokio::test]
async fn undo_of_a_pending_permanent_delete_clears_pending_delete() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "MOVE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Junk", &["\\Junk"])],
    )
    .await;
    mock.state.add_message(
        "INBOX",
        &message_raw(
            "alice@example.com",
            "test@example.com",
            "Keep me",
            "body",
            "keep@example.com",
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
    sync_once(&ctx, &account).await;
    let service = MailService::new(ctx.clone());
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let message = only_message(&ctx, &inbox).await;
    let original_uid = {
        let id = message.id.clone();
        ctx.db
            .read(move |db| repo::messages::get_required(db, &id))
            .await
            .expect("synced message")
            .uid
    };

    service
        .message_act(
            std::slice::from_ref(&message.id),
            &MessageAction::Delete { permanent: true },
        )
        .await
        .expect("permanent delete");

    let id = message.id.clone();
    let (hidden, pending_delete): (i64, i64) = ctx
        .db
        .read(move |db| {
            Ok(db.query_row(
                "SELECT is_deleted_local, pending_delete FROM message WHERE id = ?1",
                rusqlite::params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .await
        .expect("hidden row");
    assert_eq!(hidden, 1);
    assert_eq!(pending_delete, 1);

    assert!(service.message_undo_last().await.expect("undo delete"));

    let id = message.id.clone();
    let restored = ctx
        .db
        .read(move |db| repo::messages::get_required(db, &id))
        .await
        .expect("restored");
    assert!(!restored.is_deleted_local);
    assert_eq!(
        restored.uid, original_uid,
        "the generation is still trusted"
    );
    let pending: i64 = {
        let id = message.id.clone();
        ctx.db
            .read(move |db| {
                Ok(db.query_row(
                    "SELECT pending_delete FROM message WHERE id = ?1",
                    rusqlite::params![id],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("pending flag")
    };
    assert_eq!(pending, 0, "the undo must clear pending_delete");
    assert!(repo_pending_target_ids(&ctx, &message.id).await.is_empty());
}

async fn repo_pending_target_ids(ctx: &CoreContext, id: &str) -> Vec<String> {
    let id = id.to_string();
    ctx.db
        .read(move |db| repo::messages::target_pending_ids(db, std::slice::from_ref(&id)))
        .await
        .expect("pending ids")
}

async fn insert_stored(
    ctx: &CoreContext,
    account_id: &str,
    id: &str,
    folder_id: &str,
    is_draft: bool,
) {
    let mut summary = MessageSummary::new(
        id,
        account_id,
        folder_id,
        format!("thread-{id}"),
        chrono::Utc::now(),
    );
    summary.subject = id.to_string();
    summary.is_draft = is_draft;
    let mut stored = StoredMessage::new(summary);
    // Synced rows always carry a folder-local UID; the service refuses actions
    // on rows whose UID is still unknown (an in-flight optimistic move).
    let uid = id
        .bytes()
        .fold(0i64, |acc, byte| {
            acc.wrapping_mul(31).wrapping_add(byte as i64)
        })
        .rem_euclid(1_000_000)
        + 1;
    stored.uid = Some(uid);
    stored.uidvalidity = Some(1);
    ctx.db
        .write(move |db| repo::messages::upsert(db, &stored))
        .await
        .expect("insert stored message");
}

/// Mark a Graph test row with the provider id its sync would have written.
///
/// Graph actions require the exact provider id the user acted on; a row with
/// `gm_msgid` unset is refused before it can be queued.
async fn set_graph_id(ctx: &CoreContext, id: &str) {
    let id = id.to_string();
    ctx.db
        .write(move |db| {
            db.execute(
                "UPDATE message SET gm_msgid = ?2 WHERE id = ?1",
                rusqlite::params![id, id],
            )?;
            Ok(())
        })
        .await
        .expect("set graph id");
}

async fn stored_folder(ctx: &CoreContext, id: &str) -> Option<String> {
    let id = id.to_string();
    ctx.db
        .read(move |db| Ok(repo::messages::get(db, &id)?.map(|row| row.summary.folder_id)))
        .await
        .expect("stored message")
}

#[tokio::test]
async fn delete_decides_per_message_and_discards_drafts() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "MOVE", "UIDPLUS"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Drafts", &["\\Drafts"]),
            ("Trash", &["\\Trash"]),
        ],
    )
    .await;
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
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let drafts = folder_id(&ctx, "acc", "Drafts").await;
    let trash = folder_id(&ctx, "acc", "Trash").await;

    insert_stored(&ctx, "acc", "inbox-msg", &inbox, false).await;
    insert_stored(&ctx, "acc", "draft-msg", &drafts, true).await;

    // One call, one `permanent` flag, two different real folders: the backend
    // must decide per message. The inbox message moves to Trash; the draft is
    // discarded outright instead of becoming a Trash row that still counts as
    // a draft.
    let undo_id = service
        .message_act(
            &["inbox-msg".to_string(), "draft-msg".to_string()],
            &MessageAction::Delete { permanent: false },
        )
        .await
        .expect("delete");
    assert!(!undo_id.is_empty(), "an undo entry must be recorded");

    assert_eq!(
        stored_folder(&ctx, "inbox-msg").await.as_deref(),
        Some(trash.as_str())
    );
    // The draft is discarded permanently. The row is hidden right away but
    // kept (with its attachments and FTS entry) until the server confirms the
    // delete; a refused command unhides it instead of losing the message.
    let draft = ctx
        .db
        .read(|db| repo::messages::get_required(db, "draft-msg"))
        .await
        .expect("draft row");
    assert!(
        draft.is_deleted_local,
        "the draft must disappear from lists"
    );
    assert_eq!(draft.summary.folder_id, drafts);
    let pending: i64 = ctx
        .db
        .read(|db| {
            Ok(db.query_row(
                "SELECT pending_delete FROM message WHERE id = 'draft-msg'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .expect("pending delete flag");
    assert_eq!(pending, 1);
}

#[tokio::test]
async fn undo_by_id_reverts_the_action_it_names() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "MOVE", "UIDPLUS"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Trash", &["\\Trash"]),
            ("Archive", &["\\Archive"]),
        ],
    )
    .await;
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
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let trash = folder_id(&ctx, "acc", "Trash").await;

    insert_stored(&ctx, "acc", "delete-me", &inbox, false).await;
    insert_stored(&ctx, "acc", "archive-me", &inbox, false).await;

    let delete_undo = service
        .message_act(
            &["delete-me".to_string()],
            &MessageAction::Delete { permanent: false },
        )
        .await
        .expect("delete");
    let archive_undo = service
        .message_act(&["archive-me".to_string()], &MessageAction::Archive)
        .await
        .expect("archive");
    assert_ne!(delete_undo, archive_undo);
    assert_eq!(
        stored_folder(&ctx, "delete-me").await.as_deref(),
        Some(trash.as_str())
    );

    // The delete's toast must revert the delete even though the archive ran
    // afterwards and is the most recent undo entry.
    assert!(service
        .message_undo(&delete_undo)
        .await
        .expect("undo delete"));
    assert_eq!(
        stored_folder(&ctx, "delete-me").await.as_deref(),
        Some(inbox.as_str())
    );
    // The archive is still in effect, and its own id still works afterwards.
    assert_ne!(
        stored_folder(&ctx, "archive-me").await.as_deref(),
        Some(inbox.as_str())
    );
    assert!(service
        .message_undo(&archive_undo)
        .await
        .expect("undo archive"));
    assert_eq!(
        stored_folder(&ctx, "archive-me").await.as_deref(),
        Some(inbox.as_str())
    );
}

#[tokio::test]
async fn compose_reply_save_and_send_through_outbox() {
    let imap = MockImap::start_with_folders(
        &["UIDPLUS", "MOVE"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Sent", &["\\Sent"]),
            ("Drafts", &["\\Drafts"]),
        ],
    )
    .await;
    let smtp = MockSmtp::start().await;
    imap.state.add_message(
        "INBOX",
        &message_raw(
            "Alice <alice@example.com>",
            "test@example.com",
            "Original subject",
            "original body text",
            "original@example.com",
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
        Some(server(imap.addr.port())),
        Some(server(smtp.addr.port())),
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;
    let service = MailService::new(ctx.clone());
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let message = only_message(&ctx, &inbox).await;

    // A reply signature is applied when configured.
    let mut signature = Signature {
        id: String::new(),
        account_id: Some("acc".into()),
        name: "Work".into(),
        html: "<p>-- <br>Test User</p>".into(),
        use_for_new: false,
        use_for_reply: true,
    };
    ctx.db
        .write(move |db| repo::signatures::upsert(db, &mut signature))
        .await
        .expect("signature");

    let draft = service
        .compose_reply(&message.id, ReplyMode::Reply)
        .await
        .expect("reply draft");
    assert_eq!(draft.to.len(), 1);
    assert_eq!(draft.to[0].email, "alice@example.com");
    assert_eq!(draft.subject, "Re: Original subject");
    assert!(draft.html.contains("original body text"));
    assert!(draft.html.contains("Test User"));
    assert!(draft.in_reply_to.is_some());

    let mut draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    assert!(ctx
        .paths
        .draft_staging_dir(&draft_id)
        .join("draft.eml")
        .exists());

    // A second save while the APPEND is still pending must not duplicate ops.
    draft = service.compose_save(&draft).await.expect("save again");
    let appended = ctx
        .db
        .read(move |db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops")
        .into_iter()
        .filter(|op| op.kind == ops::KIND_APPEND_DRAFT)
        .count();
    assert_eq!(appended, 1);

    // Execute the APPEND and resync Drafts.
    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("draft append");
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts resync");
    assert_eq!(imap.state.message_count("Drafts"), 1);
    let draft_message = only_message(&ctx, &drafts_folder).await;
    let opened = service
        .compose_open_draft(&draft_message.id)
        .await
        .expect("open draft");
    assert_eq!(opened.subject, "Re: Original subject");
    assert!(opened.html.contains("original body text"));

    // Attach a file and send through the mock SMTP.
    let attachment_path = std::env::temp_dir().join(format!(
        "nuncio-service-attachment-{}.txt",
        nuncio_core::models::new_id()
    ));
    std::fs::write(&attachment_path, b"attachment body").expect("temp attachment");
    let attachments = service
        .compose_add_attachments(&[attachment_path.to_string_lossy().to_string()])
        .await
        .expect("add attachments");
    assert_eq!(attachments.len(), 1);
    draft.attachments.extend(attachments);
    let draft = service
        .compose_save(&draft)
        .await
        .expect("save with attachment");
    service.compose_send(&draft).await.expect("send");

    let sent = smtp.messages.lock().expect("smtp").clone();
    assert_eq!(sent.len(), 1, "one message reached SMTP");
    let text = String::from_utf8_lossy(&sent[0]);
    assert!(text.contains("alice@example.com"));
    assert!(text.contains("Subject: Re: Original subject"));
    assert!(text.contains("nuncio-service-attachment-"));
    assert!(ctx
        .db
        .read(move |db| repo::drafts::get(db, &draft_id))
        .await
        .expect("draft lookup")
        .is_none());
    let pending = ctx
        .db
        .read(|db| repo::outbox::pending_count(db, Some("acc")))
        .await
        .expect("outbox pending");
    assert_eq!(pending, 0, "sent message must leave the pending outbox");
    let (total, sent): (i64, i64) = ctx
        .db
        .read(|db| {
            db.query_row(
                "SELECT COUNT(*), SUM(CASE WHEN state = 'sent' THEN 1 ELSE 0 END) FROM outbox",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(nuncio_core::AppError::from)
        })
        .await
        .expect("outbox counts");
    assert_eq!(total, 1);
    assert_eq!(sent, 1);

    let _ = std::fs::remove_file(attachment_path);
}

#[tokio::test]
async fn bcc_recipients_are_delivered_but_hidden_from_transmitted_data() {
    let imap = MockImap::start_with_folders(
        &["UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Sent", &["\\Sent"])],
    )
    .await;
    let smtp = MockSmtp::start().await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        Some(server(imap.addr.port())),
        Some(server(smtp.addr.port())),
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service
        .compose_create(Some("acc"))
        .await
        .expect("compose draft");
    draft.to = vec![Address::new("alice@example.com")];
    draft.bcc = vec![Address::new("hidden@example.com")];
    draft.subject = "Bcc test".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    // The saved draft file (and therefore the server Drafts APPEND) keeps Bcc.
    let saved = tokio::fs::read(ctx.paths.draft_staging_dir(&draft_id).join("draft.eml"))
        .await
        .expect("read saved draft");
    let parsed = nuncio_mime::parse(&saved).expect("parse saved draft");
    assert_eq!(parsed.headers.bcc.len(), 1);
    assert_eq!(parsed.headers.bcc[0].email, "hidden@example.com");

    service.compose_send(&draft).await.expect("send");

    // Envelope includes the Bcc recipient...
    let recipients = smtp.recipients.lock().expect("smtp recipients").clone();
    assert!(recipients.contains(&"alice@example.com".to_string()));
    assert!(
        recipients.contains(&"hidden@example.com".to_string()),
        "Bcc recipient must be in RCPT TO: {recipients:?}"
    );

    // ...but the DATA payload does not expose it.
    let sent = smtp.messages.lock().expect("smtp messages").clone();
    assert_eq!(sent.len(), 1, "one message reached SMTP");
    let text = String::from_utf8_lossy(&sent[0]).to_ascii_lowercase();
    assert!(text.contains("to: <alice@example.com>"), "payload: {text}");
    assert!(!text.contains("bcc:"));
    assert!(!text.contains("hidden@example.com"));

    // The Sent copy queued for APPEND restores the Bcc header.
    let append = ctx
        .db
        .read(|db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("pending ops")
        .into_iter()
        .find(|op| op.kind == ops::KIND_APPEND_TO_SENT)
        .expect("sent append queued");
    let payload: ops::AppendToSentOp = repo::pending_ops::payload(&append).expect("append payload");
    let copy = tokio::fs::read(&payload.rfc822_path)
        .await
        .expect("read sent copy");
    let parsed = nuncio_mime::parse(&copy).expect("parse sent copy");
    assert_eq!(parsed.headers.bcc.len(), 1);
    assert_eq!(parsed.headers.bcc[0].email, "hidden@example.com");
}

#[tokio::test]
async fn attachments_eml_export_and_external_open() {
    let imap = MockImap::start_with_folders(&["UIDPLUS"], &[("INBOX", &["\\Inbox"])]).await;
    imap.state.add_message(
        "INBOX",
        &raw_with_parts(
            "Alice <alice@example.com>",
            "test@example.com",
            "Files",
            "files@example.com",
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
        Some(server(imap.addr.port())),
        None,
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;
    let service = MailService::new(ctx.clone());
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let message = only_message(&ctx, &inbox).await;
    let detail = service
        .message_get(&message.id, false)
        .await
        .expect("detail");
    assert!(detail.attachments.len() >= 2);
    let regular = detail
        .attachments
        .iter()
        .find(|attachment| !attachment.is_inline)
        .expect("regular attachment");
    let opened = service.attachment_open(&regular.id).await.expect("open");
    assert_eq!(std::fs::read(&opened).expect("bytes"), b"report body");

    let dir = std::env::temp_dir().join(format!(
        "nuncio-attachments-{}",
        nuncio_core::models::new_id()
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    let saved = service
        .attachment_save_all(&message.id, &dir.to_string_lossy())
        .await
        .expect("save all");
    assert!(saved.len() >= 2);
    assert!(saved.iter().all(|path| path.exists()));
    assert!(service.attachment_is_dangerous("setup.exe"));
    assert!(!service.attachment_is_dangerous("report.txt"));

    let eml = dir.join("saved.eml");
    service
        .message_save_eml(&message.id, &eml.to_string_lossy())
        .await
        .expect("save eml");
    let external = service
        .message_open_eml(&eml.to_string_lossy())
        .await
        .expect("open external eml");
    assert_eq!(external.summary.subject, "Files");
    assert!(external.external_path.is_some());
    assert!(external.attachments.len() >= 2);
    let external_attachment = external
        .attachments
        .iter()
        .find(|attachment| !attachment.is_inline)
        .expect("external attachment");
    let path = service
        .attachment_open(&external_attachment.id)
        .await
        .expect("open external attachment");
    assert_eq!(std::fs::read(&path).expect("bytes"), b"report body");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn folder_commands_and_rule_execution() {
    let imap = MockImap::start_with_folders(
        &["IDLE", "MOVE", "UIDPLUS", "SPECIAL-USE"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Sent", &["\\Sent"]),
            ("Drafts", &["\\Drafts"]),
            ("Trash", &["\\Trash"]),
            ("Archive", &["\\Archive"]),
        ],
    )
    .await;
    imap.state.add_message(
        "INBOX",
        &message_raw(
            "newsletter@example.com",
            "test@example.com",
            "Weekly digest",
            "newsletter body",
            "news@example.com",
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
        Some(server(imap.addr.port())),
        None,
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;

    let service = MailService::new(ctx.clone());
    service.start().await.expect("start");
    tokio::time::sleep(Duration::from_millis(150)).await;
    let created = service
        .folder_create("acc", None, "Projects")
        .await
        .expect("create folder");
    assert_eq!(created.name, "Projects");
    let renamed = service
        .folder_rename(&created.id, "Projects 2026")
        .await
        .unwrap_or_else(|error| panic!("rename folder: {error}"));
    assert_eq!(renamed.name, "Projects 2026");
    let moved = service
        .folder_move(&renamed.id, None)
        .await
        .expect("move folder");
    assert_eq!(moved.parent_id, None);
    service
        .folder_delete(&moved.id)
        .await
        .expect("delete folder");
    assert!(!service
        .folder_list("acc")
        .await
        .expect("folders")
        .iter()
        .any(|folder| folder.name == "Projects 2026"));

    service.shutdown().await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    service
        .folder_mark_all_read(&inbox)
        .await
        .expect("mark all read");
    let message = only_message(&ctx, &inbox).await;
    assert!(message.seen);
    let op = ctx
        .db
        .read(move |db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops")
        .into_iter()
        .find(|op| op.kind == ops::KIND_FOLDER_MARK_READ)
        .expect("folder mark read op");
    assert!(matches!(
        op.state,
        nuncio_core::models::PendingOpState::Pending
            | nuncio_core::models::PendingOpState::InProgress
    ));
    // The folder operation id is also the `seen` writer owner, so a later
    // single-message action can be told apart from this bulk intent.
    let writer = ctx
        .db
        .read({
            let id = message.id.clone();
            move |db| repo::messages::flag_writer(db, &id, "seen")
        })
        .await
        .expect("writer")
        .expect("seen writer");
    assert_eq!(writer.owner_id, op.id);

    // Rule run now: flag and archive the newsletter.
    let rule = MailRule {
        id: "rule-1".into(),
        account_id: Some("acc".into()),
        name: "Newsletters".into(),
        enabled: true,
        match_all: true,
        conditions: vec![RuleCondition {
            field: RuleField::From,
            op: RuleOp::Contains,
            value: "newsletter@example.com".into(),
        }],
        actions: vec![
            RuleAction {
                kind: RuleActionKind::Flag,
                folder_id: None,
            },
            RuleAction {
                kind: RuleActionKind::Move,
                folder_id: Some(folder_id(&ctx, "acc", "Archive").await),
            },
        ],
        stop_processing: true,
        sort_order: 0,
    };
    ctx.db
        .write(move |db| repo::rules::upsert(db, &rule))
        .await
        .expect("rule");
    let affected = service
        .rule_run_now("rule-1", &inbox)
        .await
        .expect("rule run");
    assert_eq!(affected, 1);
    let archive = folder_id(&ctx, "acc", "Archive").await;
    let archived = summaries(&ctx, &archive).await;
    assert_eq!(archived.len(), 1);
    assert!(archived[0].flagged);
}

#[tokio::test]
async fn unsubscribe_through_mailto_outbox() {
    let imap = MockImap::start_with_folders(&["UIDPLUS"], &[("INBOX", &["\\Inbox"])]).await;
    let smtp = MockSmtp::start().await;
    imap.state.add_message(
        "INBOX",
        "From: list@example.com\r\nTo: test@example.com\r\nSubject: Newsletter\r\nDate: Thu, 12 Nov 2026 08:00:00 +0000\r\nMessage-ID: <unsub@example.com>\r\nList-Unsubscribe: <mailto:unsubscribe@example.com?subject=unsubscribe>\r\nContent-Type: text/plain\r\n\r\nbody\r\n"
            .as_bytes(),
    );
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        Some(server(imap.addr.port())),
        Some(server(smtp.addr.port())),
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;
    let service = MailService::new(ctx.clone());
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let message = only_message(&ctx, &inbox).await;
    service
        .message_unsubscribe(&message.id)
        .await
        .expect("unsubscribe");
    let sent = smtp.messages.lock().expect("smtp").clone();
    assert_eq!(sent.len(), 1);
    let text = String::from_utf8_lossy(&sent[0]);
    assert!(
        text.contains("unsubscribe@example.com"),
        "sent message was:\n{text}"
    );
}

#[tokio::test]
async fn suggestions_merge_contacts_and_history() {
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account("acc", "test@example.com", None, None, None);
    insert_account(&ctx, &account).await;
    let service = MailService::new(ctx.clone());
    let mut contact = nuncio_core::models::Contact::new("c1", "local-contacts", "local");
    contact.display_name = "Alice Cooper".into();
    contact.emails = vec![nuncio_core::models::ContactField {
        r#type: nuncio_core::models::ContactFieldType::Work,
        value: "alice@example.com".into(),
        is_primary: true,
    }];
    ctx.db
        .write(move |db| repo::contacts::create(db, &contact))
        .await
        .expect("contact");
    ctx.db
        .write(|db| {
            repo::recipient_history::record(db, &[Address::with_name("Bob", "bob@example.com")])
        })
        .await
        .expect("history");
    let suggestions = service
        .compose_suggest_recipients("example.com", 10)
        .await
        .expect("suggest");
    assert!(suggestions
        .iter()
        .any(|address| address.email == "alice@example.com"));
    assert!(suggestions
        .iter()
        .any(|address| address.email == "bob@example.com"));
}

#[tokio::test]
async fn message_actions_execute_on_the_server() {
    let imap = MockImap::start_with_folders(
        &["CONDSTORE", "MOVE", "UIDPLUS"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Sent", &["\\Sent"]),
            ("Drafts", &["\\Drafts"]),
            ("Trash", &["\\Trash"]),
            ("Archive", &["\\Archive"]),
        ],
    )
    .await;
    imap.state.add_message(
        "INBOX",
        &message_raw(
            "alice@example.com",
            "test@example.com",
            "Server ops",
            "body",
            "server-ops@example.com",
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
        Some(server(imap.addr.port())),
        None,
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;
    let service = MailService::new(ctx.clone());
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    let trash = folder_id(&ctx, "acc", "Trash").await;
    let message = only_message(&ctx, &inbox).await;

    // Flag → the worker issues UID STORE.
    service
        .message_act(
            std::slice::from_ref(&message.id),
            &MessageAction::SetFlagged { value: true },
        )
        .await
        .expect("flag");
    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("execute flag");
    assert!(imap
        .state
        .message_flags("INBOX", 1)
        .iter()
        .any(|flag| flag == "\\Flagged"));

    // Move → UID MOVE to Archive.
    service
        .message_act(
            std::slice::from_ref(&message.id),
            &MessageAction::Move {
                folder_id: archive.clone(),
            },
        )
        .await
        .expect("move");
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("execute move");
    assert_eq!(imap.state.message_count("Archive"), 1);
    assert_eq!(imap.state.message_count("INBOX"), 0);

    // Resync restores the moved message with the target folder's UID.
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &archive)
        .await
        .expect("archive sync");
    let archived = summaries(&ctx, &archive).await;
    assert_eq!(archived.len(), 1);
    let archived_id = archived[0].id.clone();

    // Delete from Archive → moved to Trash.
    service
        .message_act(
            std::slice::from_ref(&archived_id),
            &MessageAction::Delete { permanent: false },
        )
        .await
        .expect("delete");
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("execute delete");
    assert_eq!(imap.state.message_count("Trash"), 1);
    assert_eq!(imap.state.message_count("Archive"), 0);
    let trashed = summaries(&ctx, &trash).await;
    assert_eq!(trashed.len(), 1);
}

/* ─────────────────── regression tests for review findings ─────────────── */

#[tokio::test]
async fn outbox_items_are_listed_viewable_and_discardable() {
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account("acc", "test@example.com", None, None, None);
    insert_account(&ctx, &account).await;
    let service = MailService::new(ctx.clone());

    let path = ctx.paths.staging_dir().join("outbox-test.eml");
    ctx.paths.ensure_parent(&path).expect("staging dir");
    std::fs::write(
        &path,
        message_raw(
            "test@example.com",
            "Bob <bob@example.com>",
            "Queued subject",
            "queued body",
            "queued@example.com",
        ),
    )
    .expect("write staged message");
    let item = nuncio_core::models::OutboxItem::new(
        "acc",
        None,
        vec![Address::new("bob@example.com")],
        "Queued subject",
        path.to_string_lossy().to_string(),
    );
    let item = ctx
        .db
        .write({
            let item = item.clone();
            move |db| repo::outbox::enqueue(db, &item)
        })
        .await
        .expect("enqueue outbox item");

    let counts = service.folder_unified_counts().await.expect("counts");
    assert_eq!(counts.outbox, 1);

    let page = service
        .message_list(
            &MessageListQuery::new(MailScope::unified(nuncio_core::models::FolderRole::Outbox))
                .with_conversation(false)
                .with_limit(20),
        )
        .await
        .expect("outbox list");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].subject, "Queued subject");
    assert_eq!(page.items[0].to[0].email, "bob@example.com");
    assert!(page.items[0].preview.contains("Queued"));
    assert!(page.items[0].id.starts_with("outbox:"));

    // The synthetic row opens without a message-table entry.
    let detail = service
        .message_get(&page.items[0].id, false)
        .await
        .expect("outbox detail");
    assert_eq!(detail.summary.subject, "Queued subject");
    assert!(detail.text.unwrap_or_default().contains("queued body"));

    // Delete from the list discards the queued message.
    service
        .message_act(
            &[page.items[0].id.clone()],
            &MessageAction::Delete { permanent: true },
        )
        .await
        .expect("discard outbox item");
    let pending = ctx
        .db
        .read(|db| repo::outbox::pending_count(db, Some("acc")))
        .await
        .expect("pending count");
    assert_eq!(pending, 0);
    assert!(ctx
        .db
        .read(move |db| repo::outbox::get(db, &item.id))
        .await
        .expect("outbox row")
        .is_none());
}

#[tokio::test]
async fn offline_drafts_appear_in_a_local_drafts_folder() {
    let imap = MockImap::start_with_folders(&["UIDPLUS"], &[("INBOX", &["\\Inbox"])]).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        Some(server(imap.addr.port())),
        None,
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.subject = "Offline draft".into();
    draft.to = vec![Address::new("bob@example.com")];
    draft.html = "<p>draft body</p>".into();
    let saved = service.compose_save(&draft).await.expect("save draft");
    assert!(saved.id.is_some());

    let folders = service.folder_list("acc").await.expect("folders");
    let drafts = folders
        .iter()
        .find(|folder| folder.role == nuncio_core::models::FolderRole::Drafts)
        .expect("local Drafts folder");
    assert!(drafts.remote_path.starts_with("local:"));

    let page = service
        .message_list(
            &MessageListQuery::new(MailScope::folder(drafts.id.clone()))
                .with_conversation(false)
                .with_limit(20),
        )
        .await
        .expect("drafts list");
    assert_eq!(page.items.len(), 1, "closed offline draft must be listed");
    assert!(page.items[0].is_draft);
    assert_eq!(page.items[0].subject, "Offline draft");

    let reopened = service
        .compose_open_draft(&page.items[0].id)
        .await
        .expect("reopen draft");
    assert_eq!(reopened.subject, "Offline draft");
    assert_eq!(reopened.id, saved.id, "reopening edits the same draft");
}

#[tokio::test]
async fn late_parent_reconciles_reply_threads() {
    let imap = MockImap::start_with_folders(&["UIDPLUS"], &[("INBOX", &["\\Inbox"])]).await;
    // Child arrives first and only knows its direct parent.
    imap.state.add_message(
        "INBOX",
        "From: carol@example.com\r\nTo: test@example.com\r\nSubject: Re: plan\r\nDate: Thu, 12 Nov 2026 11:00:00 +0000\r\nMessage-ID: <child@example.com>\r\nIn-Reply-To: <parent@example.com>\r\nReferences: <parent@example.com>\r\nContent-Type: text/plain\r\n\r\nchild\r\n"
            .as_bytes(),
    );
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        Some(server(imap.addr.port())),
        None,
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;

    // The parent (which knows the real root) arrives later.
    imap.state.add_message(
        "INBOX",
        "From: bob@example.com\r\nTo: test@example.com\r\nSubject: plan\r\nDate: Thu, 12 Nov 2026 10:00:00 +0000\r\nMessage-ID: <parent@example.com>\r\nIn-Reply-To: <root@example.com>\r\nReferences: <root@example.com>\r\nContent-Type: text/plain\r\n\r\nparent\r\n"
            .as_bytes(),
    );
    sync_once(&ctx, &account).await;

    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let messages = summaries(&ctx, &inbox).await;
    assert_eq!(messages.len(), 2);
    let threads: std::collections::HashSet<String> = messages
        .iter()
        .map(|message| message.thread_id.clone())
        .collect();
    assert_eq!(
        threads.len(),
        1,
        "reply and late parent must share a thread"
    );
    assert!(threads.iter().next().unwrap().ends_with("root@example.com"));
}

#[tokio::test]
async fn gmail_uses_thread_ids_and_label_membership() {
    let imap = MockImap::start_with_folders(
        &["X-GM-EXT-1", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Work", &[])],
    )
    .await;
    imap.state.add_gmail_message(
        "INBOX",
        &message_raw(
            "Alice <alice@example.com>",
            "test@example.com",
            "Labeled message",
            "hello label body",
            "labeled@example.com",
        ),
        4242,
        777,
        &["\\Inbox", "Work"],
    );
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut account = account(
        "acc",
        "test@example.com",
        Some(server(imap.addr.port())),
        None,
        None,
    );
    account.provider = nuncio_core::models::ProviderKind::Gmail;
    account.capabilities = nuncio_core::models::ProviderCapabilities::defaults_for(
        nuncio_core::models::ProviderKind::Gmail,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;

    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let message = only_message(&ctx, &inbox).await;
    assert_eq!(message.thread_id, "gm:777");

    let service = MailService::new(ctx.clone());
    let work = folder_id(&ctx, "acc", "Work").await;
    let page = service
        .message_list(
            &MessageListQuery::new(MailScope::folder(work))
                .with_conversation(false)
                .with_limit(20),
        )
        .await
        .expect("label folder list");
    assert_eq!(
        page.items.len(),
        1,
        "a message labeled Work must appear in the Work folder even when Inbox is its primary folder"
    );
    assert_eq!(page.items[0].subject, "Labeled message");
}

#[tokio::test]
async fn server_search_hits_are_merged_with_the_local_index() {
    let imap = MockImap::start_with_folders(&["UIDPLUS"], &[("INBOX", &["\\Inbox"])]).await;
    let mut body = "padding ".repeat(500);
    body.push_str("the-needle-is-beyond-the-preview-window");
    imap.state.add_message(
        "INBOX",
        &message_raw(
            "Alice <alice@example.com>",
            "test@example.com",
            "Long message",
            &body,
            "long@example.com",
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
        Some(server(imap.addr.port())),
        None,
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;
    let service = MailService::new(ctx.clone());

    let mut search = SearchQuery::text("the-needle-is-beyond-the-preview-window");
    search.include_server = true;
    let page = service
        .message_list(
            &MessageListQuery::new(MailScope::search(search))
                .with_conversation(false)
                .with_limit(20),
        )
        .await
        .expect("merged search");
    assert_eq!(
        page.items.len(),
        1,
        "body-only matches reported by the server must survive the local filter"
    );
    assert_eq!(page.items[0].subject, "Long message");
}

#[tokio::test]
async fn server_search_is_skipped_for_providers_with_broken_text_search() {
    // QQ accepts UID SEARCH but always answers 0 hits. The mock here answers
    // every UID, so the search term below is deliberately absent from the
    // message: only a server query could surface it.
    let imap = MockImap::start_with_folders(&["UIDPLUS"], &[("INBOX", &["\\Inbox"])]).await;
    imap.state.add_message(
        "INBOX",
        &message_raw(
            "Alice <alice@example.com>",
            "test@qq.com",
            "Greeting",
            "hello world",
            "qq@example.com",
        ),
    );
    let ctx = context(
        StaticCreds::password("test@qq.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@qq.com",
        Some(server(imap.addr.port())),
        None,
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;
    let service = MailService::new(ctx.clone());

    let mut search = SearchQuery::text("zzznotpresent");
    search.include_server = true;

    // Control: the fixture would answer this criteria, so an empty page below
    // means the provider gate skipped the account rather than a dead server.
    let inbox_id = folder_id(&ctx, "acc", "INBOX").await;
    let inbox = {
        let id = inbox_id.clone();
        ctx.db
            .read(move |db| repo::folders::get(db, &id))
            .await
            .expect("folder")
            .expect("inbox")
    };
    let criteria = nuncio_mail::sync::messages::imap_search_criteria(&search).expect("criteria");
    let mut connection = connect(&account).await;
    let direct = nuncio_mail::sync::messages::server_search(
        &ctx,
        &mut connection,
        &account,
        &inbox,
        &criteria,
    )
    .await
    .expect("direct server search");
    assert_eq!(direct.len(), 1, "the mock answers UID SEARCH");

    let page = service
        .message_list(
            &MessageListQuery::new(MailScope::search(search))
                .with_conversation(false)
                .with_limit(20),
        )
        .await
        .expect("local search");
    assert!(
        page.items.is_empty(),
        "account with broken server text search must fall back to the local index"
    );
}

/// A 163-like fixture: content SEARCH answers zero hits, FETCH works.
async fn scan_fixture(email: &str) -> (MockImap, CoreContext, Account, MailService) {
    let imap = MockImap::start_with_folders(&["UIDPLUS"], &[("INBOX", &["\\Inbox"])]).await;
    imap.state.set_content_search_empty(true);
    for (subject, id) in [("Hello there", "a"), ("Weekly notes", "b")] {
        imap.state.add_message(
            "INBOX",
            &message_raw("Alice <alice@example.com>", email, subject, "text", id),
        );
    }
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account("acc", email, Some(server(imap.addr.port())), None, None);
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;
    // Arrive after the initial sync: only a server-side look can find these.
    for (subject, id) in [("Quarterly Budget review", "c"), ("会议纪要 十一月", "d")] {
        imap.state.add_message(
            "INBOX",
            &message_raw("Bob <bob@example.com>", email, subject, "text", id),
        );
    }
    let service = MailService::new(ctx.clone());
    (imap, ctx, account, service)
}

async fn scan_search(service: &MailService, text: &str) -> nuncio_core::models::MessagePage {
    let mut search = SearchQuery::text(text);
    search.include_server = true;
    service
        .message_list(
            &MessageListQuery::new(MailScope::search(search))
                .with_conversation(false)
                .with_limit(20),
        )
        .await
        .expect("search")
}

#[tokio::test]
async fn header_scan_finds_unsynced_ascii_and_cjk_subjects() {
    use nuncio_core::models::SearchCoverage;
    let (imap, _ctx, _account, service) = scan_fixture("test@163.com").await;

    let page = scan_search(&service, "BUDGET").await;
    assert_eq!(
        page.items.len(),
        1,
        "unsynced ASCII match must be listable: {:?} {:?}",
        page.coverage,
        imap.state.commands()
    );
    assert_eq!(page.items[0].subject, "Quarterly Budget review");
    assert_eq!(
        page.coverage,
        Some(SearchCoverage::Scanned {
            scanned: 4,
            total: 4
        })
    );

    let page = scan_search(&service, "会议纪要").await;
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].subject, "会议纪要 十一月");

    let commands = imap.state.commands();
    assert!(
        commands.iter().any(|c| c.starts_with("EXAMINE")),
        "scan must open the folder read-only: {commands:?}"
    );
    assert!(
        commands.iter().any(|c| c.contains("SEARCH ALL")),
        "{commands:?}"
    );
    assert!(commands
        .iter()
        .any(|c| c.contains("FETCH")
            && c.contains("HEADER.FIELDS (FROM TO CC SUBJECT DATE MESSAGE-ID)")));
    let after_examine = commands
        .iter()
        .skip_while(|c| !c.starts_with("EXAMINE"))
        .collect::<Vec<_>>();
    assert!(
        !after_examine
            .iter()
            .any(|c| c.contains("BODY.PEEK[TEXT]") && c.contains("1:4")),
        "the scan must not fetch bodies (only matched hits get a 2 KiB preview): {after_examine:?}"
    );
    // A miss still reports how much was covered instead of a bare empty page.
    let page = scan_search(&service, "zzznotpresent").await;
    assert!(page.items.is_empty());
    assert!(matches!(
        page.coverage,
        Some(SearchCoverage::Scanned { total: 4, .. })
    ));
}

#[tokio::test]
async fn header_scan_respects_the_cap_and_cancellation() {
    let (_imap, ctx, account, _service) = scan_fixture("test@163.com").await;
    let inbox_id = folder_id(&ctx, "acc", "INBOX").await;
    let inbox = ctx
        .db
        .read(move |db| repo::folders::get(db, &inbox_id))
        .await
        .expect("folder")
        .expect("inbox");
    let mut connection = connect(&account).await;
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let outcome = nuncio_mail::sync::messages::server_scan_with_cap(
        &ctx,
        &mut connection,
        &account,
        &inbox,
        &SearchQuery::text("hello"),
        &cancel,
        2,
    )
    .await
    .expect("scan");
    assert_eq!((outcome.scanned, outcome.total), (2, 4));
    assert!(
        outcome.ids.is_empty(),
        "'Hello there' is older than the capped window"
    );

    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    let outcome = nuncio_mail::sync::messages::server_scan(
        &ctx,
        &mut connection,
        &account,
        &inbox,
        &SearchQuery::text("budget"),
        &cancel,
    )
    .await
    .expect("cancelled scan");
    assert_eq!(outcome.scanned, 0, "a cancelled scan stops before fetching");
}

#[tokio::test]
async fn search_reports_how_it_was_produced() {
    use nuncio_core::models::SearchCoverage;
    // Generic provider: native SEARCH is trusted.
    let imap = MockImap::start_with_folders(&["UIDPLUS"], &[("INBOX", &["\\Inbox"])]).await;
    imap.state.add_message(
        "INBOX",
        &message_raw(
            "Alice <alice@example.com>",
            "test@example.com",
            "Budget",
            "x",
            "n@example.com",
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
        Some(server(imap.addr.port())),
        None,
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;
    let service = MailService::new(ctx.clone());

    let page = scan_search(&service, "budget").await;
    assert_eq!(page.coverage, Some(SearchCoverage::Server));

    let page = service
        .message_list(
            &MessageListQuery::new(MailScope::search(SearchQuery::text("budget")))
                .with_conversation(false),
        )
        .await
        .expect("local search");
    assert_eq!(page.coverage, Some(SearchCoverage::LocalOnly));
    assert_eq!(page.items.len(), 1);
}

#[tokio::test]
async fn outbox_cancel_returns_an_editable_draft() {
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account("acc", "test@example.com", None, None, None);
    insert_account(&ctx, &account).await;
    let service = MailService::new(ctx.clone());

    let path = ctx.paths.staging_dir().join("outbox-cancel.eml");
    ctx.paths.ensure_parent(&path).expect("staging dir");
    std::fs::write(
        &path,
        raw_with_parts(
            "test@example.com",
            "Alice <alice@example.com>",
            "Cancel me",
            "cancel@example.com",
        ),
    )
    .expect("write staged message");
    let item = nuncio_core::models::OutboxItem::new(
        "acc",
        None,
        vec![Address::new("alice@example.com")],
        "Cancel me",
        path.to_string_lossy().to_string(),
    );
    let item = ctx
        .db
        .write({
            let item = item.clone();
            move |db| repo::outbox::enqueue(db, &item)
        })
        .await
        .expect("enqueue outbox item");

    let draft = service
        .outbox_cancel(&item.id)
        .await
        .expect("cancel outbox item");
    assert_eq!(draft.account_id, "acc");
    assert_eq!(draft.subject, "Cancel me");
    assert_eq!(draft.to[0].email, "alice@example.com");
    assert!(draft.html.contains("invitation"));
    assert_eq!(draft.attachments.len(), 2, "attachments are re-staged");
    for attachment in &draft.attachments {
        assert!(std::path::Path::new(&attachment.path).exists());
    }
    assert!(!path.exists(), "the staged outbox message is removed");
    assert!(
        ctx.db
            .read({
                let id = item.id.clone();
                move |db| repo::outbox::get(db, &id)
            })
            .await
            .expect("outbox row")
            .is_none(),
        "the outbox row is removed"
    );
}

#[tokio::test]
async fn outbox_cancel_refuses_a_message_that_is_sending() {
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account("acc", "test@example.com", None, None, None);
    insert_account(&ctx, &account).await;
    let service = MailService::new(ctx.clone());

    let path = ctx.paths.staging_dir().join("outbox-sending.eml");
    ctx.paths.ensure_parent(&path).expect("staging dir");
    std::fs::write(&path, b"Subject: sending\r\n\r\nbody\r\n").expect("write staged message");
    let item = nuncio_core::models::OutboxItem::new(
        "acc",
        None,
        vec![Address::new("bob@example.com")],
        "sending",
        path.to_string_lossy().to_string(),
    );
    let item = ctx
        .db
        .write({
            let item = item.clone();
            move |db| repo::outbox::enqueue(db, &item)
        })
        .await
        .expect("enqueue outbox item");
    let id = item.id.clone();
    ctx.db
        .write(move |db| {
            db.execute(
                "UPDATE outbox SET state = 'sending' WHERE id = ?1",
                rusqlite::params![id],
            )?;
            Ok(())
        })
        .await
        .expect("mark sending");

    let error = service
        .outbox_cancel(&item.id)
        .await
        .expect_err("cancel while sending must fail");
    assert_eq!(error.code(), "conflict");
}

#[tokio::test]
async fn outbox_retry_none_restarts_every_failed_item() {
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account("acc", "test@example.com", None, None, None);
    insert_account(&ctx, &account).await;
    let service = MailService::new(ctx.clone());

    let path = ctx.paths.staging_dir().join("outbox-retry.eml");
    ctx.paths.ensure_parent(&path).expect("staging dir");
    std::fs::write(
        &path,
        b"Subject: retry\r\nTo: bob@example.com\r\n\r\nbody\r\n",
    )
    .expect("write staged message");
    let item = nuncio_core::models::OutboxItem::new(
        "acc",
        None,
        vec![Address::new("bob@example.com")],
        "retry",
        path.to_string_lossy().to_string(),
    );
    let item = ctx
        .db
        .write({
            let item = item.clone();
            move |db| repo::outbox::enqueue(db, &item)
        })
        .await
        .expect("enqueue outbox item");
    let id = item.id.clone();
    ctx.db
        .write(move |db| {
            db.execute(
                "UPDATE outbox SET state = 'failed', attempts = 8, last_error = 'boom' WHERE id = ?1",
                rusqlite::params![id],
            )?;
            Ok(())
        })
        .await
        .expect("mark failed");

    service.outbox_retry(None).await.expect("retry all");
    let after = ctx
        .db
        .read({
            let id = item.id.clone();
            move |db| repo::outbox::get_required(db, &id)
        })
        .await
        .expect("outbox row");
    // The account has no SMTP server, so the fresh attempt fails again, but the
    // attempt budget was reset (otherwise it would stay permanently failed).
    assert_ne!(after.state, nuncio_core::models::OutboxState::Failed);
    assert_eq!(after.attempts, 1);
}

#[tokio::test]
async fn folder_empty_only_allows_trash_and_junk() {
    let mock = MockImap::start_with_folders(
        &["UIDPLUS"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Trash", &["\\Trash"]),
            ("Junk", &["\\Junk"]),
        ],
    )
    .await;
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

    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let error = service
        .folder_empty(&inbox)
        .await
        .expect_err("inbox must not be emptiable");
    assert_eq!(error.code(), "invalidInput");

    let junk = folder_id(&ctx, "acc", "Junk").await;
    service
        .folder_empty(&junk)
        .await
        .expect("junk can be emptied");
}

/// The direct-IMAP fallback used when the account worker is not running must
/// follow the same rule as the actor: no directed expunge means no whole-folder
/// STORE `\Deleted`, no EXPUNGE, and `false` to the caller.
#[tokio::test]
async fn folder_empty_fallback_without_directed_expunge_is_a_no_op() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE"],
        &[("INBOX", &["\\Inbox"]), ("Junk", &["\\Junk"])],
    )
    .await;
    mock.state.add_message(
        "Junk",
        &raw_with_parts(
            "a@example.com",
            "test@example.com",
            "Spam",
            "spam-fallback@x",
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
    sync_once(&ctx, &account).await;
    // The engine is deliberately not started, so `folder_empty` takes the
    // direct-connection fallback.
    let service = MailService::new(ctx.clone());
    let junk = folder_id(&ctx, "acc", "Junk").await;

    let before = mock.state.commands().len();
    let expunged = service.folder_empty(&junk).await.expect("folder empty");
    assert!(
        !expunged,
        "without UIDPLUS the fallback must report a refusal"
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
        "the fallback must not flag or expunge anything: {commands:?}"
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
}

#[tokio::test]
async fn trusted_only_ignores_the_one_off_remote_override() {
    let mock = MockImap::start_with_folders(&["UIDPLUS"], &[("INBOX", &["\\Inbox"])]).await;
    mock.state.add_message(
        "INBOX",
        &raw_with_parts(
            "Alice <alice@example.com>",
            "test@example.com",
            "Remote images",
            "remote-policy@example.com",
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
    sync_once(&ctx, &account).await;
    let service = MailService::new(ctx.clone());
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let message = only_message(&ctx, &inbox).await;

    ctx.settings
        .update(SettingsPatch {
            mail: Some(MailSettingsPatch {
                remote_images: Some(nuncio_core::models::RemoteImagesPolicy::TrustedOnly),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect("settings");

    // The per-view override is ignored under trustedOnly.
    let detail = service
        .message_get(&message.id, true)
        .await
        .expect("detail");
    assert!(!detail.remote_content_allowed);
    assert!(!detail.html.contains(" src=\"https://remote.example"));

    // Trusting the sender unlocks it.
    service
        .message_trust_sender("alice@example.com")
        .await
        .expect("trust");
    let detail = service
        .message_get(&message.id, false)
        .await
        .expect("trusted detail");
    assert!(detail.remote_content_allowed);
    assert!(detail.html.contains("https://remote.example"));
}

/// Opening a POP3 message materialises its body on demand; listing alone did
/// not download it.
#[tokio::test]
async fn pop3_message_detail_fetches_the_body_on_demand() {
    let mock = support::mock_pop3::MockPop3::start().await;
    mock.add(
        "uid-a",
        &message_raw(
            "alice@example.com",
            "test@example.com",
            "POP3 detail",
            "pop3 on-demand body",
            "pop3-detail@x",
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
    assert_eq!(mock.counters.retr(), 0);

    let service = MailService::new(ctx.clone());
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let message = only_message(&ctx, &inbox).await;
    assert_eq!(mock.counters.retr(), 0, "listing must stay header-only");

    let detail = service
        .message_get(&message.id, false)
        .await
        .expect("detail");
    assert!(detail.html.contains("pop3 on-demand body"));
    assert_eq!(mock.counters.retr(), 1);

    // Re-opening is served from the cached body.
    let _ = service
        .message_get(&message.id, false)
        .await
        .expect("second detail");
    assert_eq!(mock.counters.retr(), 1);
}

/// A POP3 attachment is resolved from the on-demand body: the list (header
/// only) reports no attachments yet, and asking for one downloads the body
/// once and then serves the part from the cached `.eml`.
#[tokio::test]
async fn pop3_attachment_downloads_the_body_once() {
    let mock = support::mock_pop3::MockPop3::start().await;
    mock.add(
        "uid-a",
        &raw_with_parts(
            "Alice <alice@example.com>",
            "test@example.com",
            "Files",
            "pop3-files@x",
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
    assert_eq!(mock.counters.retr(), 0);

    let service = MailService::new(ctx.clone());
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let message = only_message(&ctx, &inbox).await;
    let detail = service
        .message_get(&message.id, false)
        .await
        .expect("detail");
    let regular = detail
        .attachments
        .iter()
        .find(|attachment| !attachment.is_inline)
        .expect("regular attachment");
    let opened = service
        .attachment_open(&regular.id)
        .await
        .expect("open attachment");
    assert_eq!(std::fs::read(&opened).expect("bytes"), b"report body");
    // One RETR for the detail, none extra for the attachment.
    assert_eq!(mock.counters.retr(), 1);
}

/// The archive rule must use provider-aware `ArchiveOp` semantics and project
/// the row into the same folder the executor will target. With a real Archive
/// folder, `\All` is not used.
#[tokio::test]
async fn archive_rule_targets_the_archive_folder_when_present() {
    let imap = MockImap::start_with_folders(
        &["MOVE", "UIDPLUS", "SPECIAL-USE"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Archive", &["\\Archive"]),
            ("All Mail", &["\\All"]),
        ],
    )
    .await;
    imap.state.add_message(
        "INBOX",
        &message_raw(
            "archive@rule.test",
            "test@example.com",
            "Archive me",
            "body",
            "archive-rule@x",
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
        Some(server(imap.addr.port())),
        None,
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    let all = folder_id(&ctx, "acc", "All Mail").await;
    let message = only_message(&ctx, &inbox).await;

    let rule = MailRule {
        id: "archive-rule".into(),
        account_id: Some("acc".into()),
        name: "Archive".into(),
        enabled: true,
        match_all: true,
        conditions: vec![RuleCondition {
            field: RuleField::From,
            op: RuleOp::Contains,
            value: "archive@rule.test".into(),
        }],
        actions: vec![RuleAction {
            kind: RuleActionKind::Archive,
            folder_id: None,
        }],
        stop_processing: true,
        sort_order: 0,
    };
    ctx.db
        .write(move |db| repo::rules::upsert(db, &rule).map(|_| ()))
        .await
        .expect("rule");
    nuncio_mail::rules::apply_rules(&ctx, &account, std::slice::from_ref(&message))
        .await
        .expect("apply archive rule");

    assert_eq!(repo_get_folder(&ctx, &message.id).await, archive);
    assert_ne!(repo_get_folder(&ctx, &message.id).await, all);
    let pending = ctx
        .db
        .read(|db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops");
    let op = pending
        .iter()
        .find(|op| op.kind == ops::KIND_ARCHIVE)
        .expect("archive op");
    let payload: ops::ArchiveOp = repo::pending_ops::payload(op).expect("payload");
    assert_eq!(payload.target_folder_id.as_deref(), Some(archive.as_str()));
    assert!(!payload.soft_hidden);
    assert!(
        pending.iter().all(|op| op.kind != ops::KIND_MOVE),
        "an archive must not be queued as a generic move"
    );
}

/// A general IMAP account with only `\All` (a virtual, possibly read-only
/// view) must not be used as the archive target. The rule hides the row in
/// place and queues a target-less archive so the executor creates `\Archive`
/// and moves there.
#[tokio::test]
async fn archive_rule_creates_archive_instead_of_moving_to_all() {
    let imap = MockImap::start_with_folders(
        &["MOVE", "UIDPLUS", "SPECIAL-USE"],
        &[("INBOX", &["\\Inbox"]), ("All Mail", &["\\All"])],
    )
    .await;
    imap.state.add_message(
        "INBOX",
        &message_raw(
            "all@rule.test",
            "test@example.com",
            "Archive to All",
            "body",
            "archive-all@x",
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
        Some(server(imap.addr.port())),
        None,
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let all = folder_id(&ctx, "acc", "All Mail").await;
    let message = only_message(&ctx, &inbox).await;

    let rule = MailRule {
        id: "archive-all-rule".into(),
        account_id: Some("acc".into()),
        name: "Archive to All".into(),
        enabled: true,
        match_all: true,
        conditions: vec![RuleCondition {
            field: RuleField::From,
            op: RuleOp::Contains,
            value: "all@rule.test".into(),
        }],
        actions: vec![RuleAction {
            kind: RuleActionKind::Archive,
            folder_id: None,
        }],
        stop_processing: true,
        sort_order: 0,
    };
    ctx.db
        .write(move |db| repo::rules::upsert(db, &rule).map(|_| ()))
        .await
        .expect("rule");
    nuncio_mail::rules::apply_rules(&ctx, &account, std::slice::from_ref(&message))
        .await
        .expect("apply archive rule");

    // The projection stays in the source folder, hidden until the executor
    // creates Archive and the target sync binds the new copy.
    let stored = {
        let id = message.id.clone();
        ctx.db
            .read(move |db| repo::messages::get(db, &id))
            .await
            .expect("read")
            .expect("message")
    };
    assert_eq!(stored.summary.folder_id, inbox);
    assert!(
        stored.is_deleted_local,
        "the optimistic archive hides the row"
    );
    assert!(
        stored.summary.folder_id != all,
        "a virtual All Mail folder must never be the archive target"
    );
    let pending = ctx
        .db
        .read(|db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops");
    let op = pending
        .iter()
        .find(|op| op.kind == ops::KIND_ARCHIVE)
        .expect("archive op");
    let payload: ops::ArchiveOp = repo::pending_ops::payload(op).expect("payload");
    assert!(payload.target_folder_id.is_none());
    assert!(payload.soft_hidden);
}

/// A later action in the same rule chain must not enqueue another command for
/// the source UID that the first move already consumed. The old code reused
/// the `StoredMessage` captured when the rule matched, so Move→Move queued the
/// same source UID twice and Move→Delete tried to delete a message that had
/// already left the folder.
#[tokio::test]
async fn rule_chains_do_not_requeue_a_moved_message() {
    let imap = MockImap::start_with_folders(
        &["MOVE", "UIDPLUS", "SPECIAL-USE"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Archive", &["\\Archive"]),
            ("Trash", &["\\Trash"]),
        ],
    )
    .await;
    imap.state.add_message(
        "INBOX",
        &message_raw(
            "first@chain.test",
            "test@example.com",
            "First chain",
            "body",
            "chain-1@x",
        ),
    );
    imap.state.add_message(
        "INBOX",
        &message_raw(
            "second@chain.test",
            "test@example.com",
            "Second chain",
            "body",
            "chain-2@x",
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
        Some(server(imap.addr.port())),
        None,
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;
    let inbox = folder_id(&ctx, "acc", "INBOX").await;
    let archive = folder_id(&ctx, "acc", "Archive").await;
    let trash = folder_id(&ctx, "acc", "Trash").await;
    let messages = summaries(&ctx, &inbox).await;
    let first = messages
        .iter()
        .find(|message| message.subject == "First chain")
        .expect("first message")
        .clone();
    let second = messages
        .iter()
        .find(|message| message.subject == "Second chain")
        .expect("second message")
        .clone();

    let move_rule = |id: &str, from: &str, actions: Vec<RuleAction>| MailRule {
        id: id.into(),
        account_id: Some("acc".into()),
        name: id.into(),
        enabled: true,
        match_all: true,
        conditions: vec![RuleCondition {
            field: RuleField::From,
            op: RuleOp::Contains,
            value: from.into(),
        }],
        actions,
        stop_processing: false,
        sort_order: 0,
    };

    // Move → Move.
    let rule = move_rule(
        "move-twice",
        "first@chain.test",
        vec![
            RuleAction {
                kind: RuleActionKind::Move,
                folder_id: Some(archive.clone()),
            },
            RuleAction {
                kind: RuleActionKind::Move,
                folder_id: Some(trash.clone()),
            },
        ],
    );
    ctx.db
        .write(move |db| repo::rules::upsert(db, &rule).map(|_| ()))
        .await
        .expect("rule");
    nuncio_mail::rules::apply_rules(&ctx, &account, std::slice::from_ref(&first))
        .await
        .expect("apply move rule");

    let pending = ctx
        .db
        .read(|db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops");
    assert_eq!(
        pending
            .iter()
            .filter(|op| op.kind == ops::KIND_MOVE)
            .count(),
        1,
        "Move→Move must queue exactly one move"
    );
    assert_eq!(
        pending
            .iter()
            .filter(|op| op.kind == ops::KIND_DELETE)
            .count(),
        0
    );
    assert_eq!(
        repo_get_folder(&ctx, &first.id).await,
        archive,
        "the first move wins"
    );

    // Move → Delete.
    let rule = move_rule(
        "move-then-delete",
        "second@chain.test",
        vec![
            RuleAction {
                kind: RuleActionKind::Move,
                folder_id: Some(archive.clone()),
            },
            RuleAction {
                kind: RuleActionKind::Delete,
                folder_id: None,
            },
        ],
    );
    ctx.db
        .write(move |db| repo::rules::upsert(db, &rule).map(|_| ()))
        .await
        .expect("rule");
    nuncio_mail::rules::apply_rules(&ctx, &account, std::slice::from_ref(&second))
        .await
        .expect("apply move+delete rule");

    let pending = ctx
        .db
        .read(|db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops");
    assert_eq!(
        pending
            .iter()
            .filter(|op| op.kind == ops::KIND_MOVE)
            .count(),
        2,
        "each message queues one move"
    );
    assert_eq!(
        pending
            .iter()
            .filter(|op| op.kind == ops::KIND_DELETE)
            .count(),
        0,
        "Delete after Move must not queue a command for the old source"
    );
    assert_eq!(repo_get_folder(&ctx, &second.id).await, archive);
}

async fn repo_get_folder(ctx: &CoreContext, message_id: &str) -> String {
    let message_id = message_id.to_string();
    ctx.db
        .read(move |db| repo::messages::get_required(db, &message_id))
        .await
        .expect("message")
        .summary
        .folder_id
}

async fn drafts_account(mock: &MockImap) -> (CoreContext, Account) {
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
    (ctx, account)
}

/// Without UIDPLUS a draft cannot be replaced safely. The first
/// autosave still uploads one copy (there is nothing to remove), but later
/// autosaves must not keep appending new copies: the executor skips the
/// replacement APPEND and the previous server draft stays indexed and visible.
#[tokio::test]
async fn autosave_without_directed_expunge_keeps_a_single_remote_draft() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "First version".into();
    draft.html = "<p>first</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first append");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    assert_eq!(mock.state.message_count("Drafts"), 1);

    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    draft.subject = "Second version".into();
    draft.html = "<p>second</p>".into();
    let saved = service.compose_save(&draft).await.expect("save again");
    assert_eq!(saved.id.as_deref(), Some(draft_id.as_str()));

    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("replacement append");
    assert_eq!(
        outcome.done, 1,
        "the replacement op settles without failing"
    );
    assert_eq!(outcome.failed, 0);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    for forbidden in ["APPEND", "UID STORE", "UID COPY", "EXPUNGE", "UID EXPUNGE"] {
        assert!(
            !commands.iter().any(|command| {
                let command = command.trim().to_ascii_uppercase();
                command == forbidden || command.starts_with(&format!("{forbidden} "))
            }),
            "an unsafe replacement must do nothing: {commands:?}"
        );
    }

    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts resync");
    assert_eq!(
        mock.state.message_count("Drafts"),
        1,
        "repeated autosaves must not accumulate server copies"
    );

    // The local draft still holds the latest edits, and the old server copy
    // remains indexed in Drafts.
    let reopened = service
        .compose_get_draft(&draft_id)
        .await
        .expect("local draft");
    assert_eq!(reopened.subject, "Second version");
    let indexed = only_message(&ctx, &drafts_folder).await;
    assert!(indexed.is_draft, "the old remote draft stays visible");
}

/// If a previous APPEND ran but the Drafts folder has not been synced, the old
/// server UID is still unknown. A second autosave must wait rather than
/// upload a copy it cannot later identify for deletion.
#[tokio::test]
async fn autosave_before_drafts_sync_does_not_duplicate_remote_draft() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Before sync".into();
    draft.html = "<p>first</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first append");
    // Deliberately no Drafts sync: the server copy exists but has no local UID.

    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    draft.subject = "Before sync, second".into();
    let draft = service.compose_save(&draft).await.expect("save again");
    assert_eq!(draft.id.as_deref(), Some(draft_id.as_str()));

    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("second append");
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.failed, 0);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        !commands
            .iter()
            .any(|command| { command.trim().to_ascii_uppercase().starts_with("APPEND") }),
        "an unindexed previous copy must not be duplicated: {commands:?}"
    );

    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    assert_eq!(mock.state.message_count("Drafts"), 1);
}

/// Opening a Drafts message that projects a local draft must load the newest
/// local edits, not the (possibly stale) server copy. A save is persisted
/// locally before its APPEND reaches the server, so parsing the server copy
/// would silently overwrite the unsent edit.
#[tokio::test]
async fn opening_a_draft_prefers_the_latest_local_edits() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Server version".into();
    draft.html = "<p>server body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first append");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    let remote = only_message(&ctx, &drafts_folder).await;

    // Newer local edit, saved but deliberately not uploaded yet.
    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    draft.subject = "Local version".into();
    draft.html = "<p>local body</p>".into();
    service.compose_save(&draft).await.expect("save locally");

    let opened = service
        .compose_open_draft(&remote.id)
        .await
        .expect("open draft");
    assert_eq!(opened.id.as_deref(), Some(draft_id.as_str()));
    assert_eq!(opened.subject, "Local version");
    assert!(opened.html.contains("local body"), "{}", opened.html);
    // Opening the local projection must not record its mutable row id as a
    // provider identity: only this draft's own upload receipt may authorize a
    // later delete. The durable identity is what the save path must use.
    assert!(
        opened.remote_draft_message_id.is_none(),
        "a local projection's row id must not become an explicit target"
    );
    assert!(
        opened.remote_draft.is_some(),
        "the upload receipt is pinned instead"
    );
}

/// When the server returns UIDPLUS `APPENDUID`, the local projection is bound
/// to the new copy immediately. A replacement can then address it without
/// waiting for a Drafts sync.
#[tokio::test]
async fn appenduid_binds_the_draft_without_a_sync() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "First version".into();
    draft.html = "<p>first</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first append");
    // No Drafts sync: APPENDUID alone must make the new copy addressable.
    let projection = stored_draft(&ctx, &draft_id).await;
    assert_eq!(
        projection.uid,
        Some(1),
        "the server-assigned UID is persisted immediately"
    );
    assert_eq!(projection.uidvalidity, Some(1));

    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    draft.subject = "Second version".into();
    draft.html = "<p>second</p>".into();
    service.compose_save(&draft).await.expect("save again");

    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("replacement append");
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.failed, 0);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        commands.iter().any(|command| {
            command
                .trim()
                .to_ascii_uppercase()
                .starts_with("UID EXPUNGE 1")
        }),
        "the bound UID must be the one deleted: {commands:?}"
    );
    assert_eq!(mock.state.message_count("Drafts"), 1);
    let replaced = stored_draft(&ctx, &draft_id).await;
    assert_eq!(
        replaced.uid,
        Some(2),
        "the second APPENDUID is persisted too"
    );
}

/// A successful replacement must invalidate the old local UID. If the next
/// autosave runs before a Drafts sync, the projection must not still point at
/// the deleted copy: doing so would delete a UID that no longer exists (which
/// many servers answer with OK) and APPEND a third copy.
#[tokio::test]
async fn autosave_after_replacement_without_sync_does_not_append_again() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "First version".into();
    draft.html = "<p>first</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first append");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    assert_eq!(mock.state.message_count("Drafts"), 1);

    // Replace the synced copy: the old UID is deleted and a new copy appended.
    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    draft.subject = "Second version".into();
    draft.html = "<p>second</p>".into();
    service.compose_save(&draft).await.expect("save again");
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("replacement append");
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.failed, 0);
    assert_eq!(mock.state.message_count("Drafts"), 1);

    // Third save, deliberately without syncing the replacement: the executor
    // must wait for a sync instead of appending another copy.
    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    draft.subject = "Third version".into();
    draft.html = "<p>third</p>".into();
    service
        .compose_save(&draft)
        .await
        .expect("save before sync");

    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("third save");
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.failed, 0);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    for forbidden in ["APPEND", "UID STORE", "UID EXPUNGE", "EXPUNGE"] {
        assert!(
            !commands.iter().any(|command| {
                let command = command.trim().to_ascii_uppercase();
                command == forbidden || command.starts_with(&format!("{forbidden} "))
            }),
            "a stale replacement UID must not be acted on: {commands:?}"
        );
    }
    assert_eq!(
        mock.state.message_count("Drafts"),
        1,
        "the replacement copy is the only remote draft"
    );

    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts resync");
    assert_eq!(mock.state.message_count("Drafts"), 1);
}

/// The replacement payload is validated before the old copy is touched: a
/// missing staged file must leave the previous server draft and its local
/// projection intact instead of deleting the only copy and then failing.
#[tokio::test]
async fn missing_staged_file_keeps_the_previous_remote_draft() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "First version".into();
    draft.html = "<p>first</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first append");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    let original = stored_draft(&ctx, &draft_id).await;
    let original_uid = original.uid.expect("synced draft UID");

    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    draft.subject = "Second version".into();
    draft.html = "<p>second</p>".into();
    service.compose_save(&draft).await.expect("save again");
    tokio::fs::remove_file(ctx.paths.draft_staging_dir(&draft_id).join("draft.eml"))
        .await
        .expect("remove staged draft");

    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("replacement with a missing file");
    assert_eq!(outcome.done, 0);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    for forbidden in ["APPEND", "UID STORE", "UID EXPUNGE", "EXPUNGE"] {
        assert!(
            !commands.iter().any(|command| {
                let command = command.trim().to_ascii_uppercase();
                command == forbidden || command.starts_with(&format!("{forbidden} "))
            }),
            "validation must run before any destructive command: {commands:?}"
        );
    }
    assert_eq!(mock.state.message_count("Drafts"), 1);
    assert_eq!(
        stored_draft(&ctx, &draft_id).await.uid,
        Some(original_uid),
        "the previous server copy stays addressable"
    );
}

/// The local projection of a draft, used to assert that a failed replacement
/// did not lose the previous server identity.
async fn stored_draft(ctx: &CoreContext, draft_id: &str) -> StoredMessage {
    let id = nuncio_mail::util::draft_message_id(draft_id);
    ctx.db
        .read(move |db| repo::messages::get(db, &id))
        .await
        .expect("read draft projection")
        .expect("draft projection exists")
}

/// The queued APPEND operation for a draft (any state), with its current
/// payload.
async fn append_op(ctx: &CoreContext, draft_id: &str) -> (String, ops::AppendDraftOp) {
    let draft_id = draft_id.to_string();
    ctx.db
        .read(move |db| {
            let mut fallback = None;
            for op in repo::pending_ops::list(db, Some("acc"))? {
                if op.kind != ops::KIND_APPEND_DRAFT {
                    continue;
                }
                let Ok(payload) = repo::pending_ops::payload::<ops::AppendDraftOp>(&op) else {
                    continue;
                };
                if payload.draft_id != draft_id {
                    continue;
                }
                // Prefer the active operation; a settled earlier autosave for
                // the same draft is only a fallback.
                if matches!(
                    op.state,
                    nuncio_core::models::PendingOpState::Pending
                        | nuncio_core::models::PendingOpState::InProgress
                ) {
                    return Ok((op.id, payload));
                }
                fallback = Some((op.id, payload));
            }
            fallback.ok_or_else(|| nuncio_core::AppError::internal("append op missing"))
        })
        .await
        .expect("read append op")
}

/// Rewrite the APPEND payload and make the operation runnable again. Models a
/// crash/failure that left the durable phase in the middle of the upload.
async fn force_append_payload<F>(ctx: &CoreContext, op_id: &str, mutate: F)
where
    F: FnOnce(&mut ops::AppendDraftOp) + Send + 'static,
{
    let op_id = op_id.to_string();
    ctx.db
        .write(move |db| {
            let op = repo::pending_ops::get_required(db, &op_id)?;
            let mut payload: ops::AppendDraftOp = repo::pending_ops::payload(&op)?;
            mutate(&mut payload);
            let value = serde_json::to_value(payload)
                .map_err(|error| nuncio_core::AppError::internal(error.to_string()))?;
            repo::pending_ops::update_payload(db, &op_id, &value)?;
            db.execute(
                "UPDATE pending_op SET state = 'pending', next_retry_at = NULL, attempts = 0
                  WHERE id = ?1",
                [&op_id],
            )?;
            Ok(())
        })
        .await
        .expect("rewrite append payload");
}

async fn append_state(ctx: &CoreContext, draft_id: &str) -> ops::AppendDraftOp {
    append_op(ctx, draft_id).await.1
}

/// A tagged `NO` from `UID STORE` must abort the replacement before APPEND,
/// and the old projection (with its still-valid UID) must be left untouched.
#[tokio::test]
async fn replacement_aborts_when_store_is_rejected() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "First version".into();
    draft.html = "<p>first</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first append");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    let original = stored_draft(&ctx, &draft_id).await;
    let original_uid = original.uid.expect("synced draft UID");

    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    draft.subject = "Second version".into();
    draft.html = "<p>second</p>".into();
    service.compose_save(&draft).await.expect("save again");

    mock.state.set_fail_store(true);
    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("replacement with refused store");
    assert_eq!(outcome.done, 0, "a refused STORE is not a completed append");
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        commands
            .iter()
            .any(|command| { command.trim().to_ascii_uppercase().starts_with("UID STORE") }),
        "the rejected command must actually have been attempted: {commands:?}"
    );
    assert!(
        !commands
            .iter()
            .any(|command| { command.trim().to_ascii_uppercase().starts_with("APPEND") }),
        "no copy may be appended after a refused delete: {commands:?}"
    );
    assert_eq!(mock.state.message_count("Drafts"), 1);
    let after = stored_draft(&ctx, &draft_id).await;
    assert_eq!(
        after.uid,
        Some(original_uid),
        "the old projection keeps the UID it can still address"
    );
}

/// A tagged `NO` from `UID EXPUNGE` must abort the replacement too: the old
/// copy is flagged but not removed, so appending now would leave two drafts.
#[tokio::test]
async fn replacement_aborts_when_expunge_is_rejected() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "First version".into();
    draft.html = "<p>first</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first append");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    let original = stored_draft(&ctx, &draft_id).await;
    let original_uid = original.uid.expect("synced draft UID");

    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    draft.subject = "Second version".into();
    draft.html = "<p>second</p>".into();
    service.compose_save(&draft).await.expect("save again");

    mock.state.set_fail_expunge(true);
    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("replacement with refused expunge");
    assert_eq!(
        outcome.done, 0,
        "a refused EXPUNGE is not a completed append"
    );
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        commands.iter().any(|command| {
            command
                .trim()
                .to_ascii_uppercase()
                .starts_with("UID EXPUNGE")
        }),
        "the rejected expunge must actually have been attempted: {commands:?}"
    );
    assert!(
        !commands
            .iter()
            .any(|command| { command.trim().to_ascii_uppercase().starts_with("APPEND") }),
        "no copy may be appended after a refused expunge: {commands:?}"
    );
    assert_eq!(mock.state.message_count("Drafts"), 1);
    let after = stored_draft(&ctx, &draft_id).await;
    assert_eq!(
        after.uid,
        Some(original_uid),
        "the old projection keeps the UID it can still address"
    );
}

/// Discarding a draft must not erase the local projection until the server
/// confirms the delete. A refused EXPUNGE keeps both the remote draft and the
/// local row so the user can still see it in Drafts.
#[tokio::test]
async fn discard_with_rejected_expunge_keeps_the_local_projection() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Discard me".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("append draft");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    assert_eq!(mock.state.message_count("Drafts"), 1);

    service.compose_discard(&draft_id).await.expect("discard");
    mock.state.set_fail_expunge(true);
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("delete remote draft");
    assert_eq!(outcome.done, 0, "the delete was not confirmed");
    assert_eq!(
        summaries(&ctx, &drafts_folder).await.len(),
        1,
        "an unconfirmed delete must not hard-delete the local projection"
    );
    assert_eq!(mock.state.message_count("Drafts"), 1);
}

/// Discarding a draft without a directed expunge must not hide the server copy
/// locally. The delete op fails honestly, and the Drafts row remains visible.
#[tokio::test]
async fn discard_without_directed_expunge_keeps_remote_draft_visible() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Discard me".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("append draft");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    assert_eq!(mock.state.message_count("Drafts"), 1);

    service.compose_discard(&draft_id).await.expect("discard");
    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("delete remote draft");
    assert_eq!(outcome.done, 0);
    // Without UIDPLUS there is no APPENDUID, so the copy cannot be attributed
    // to this client. Nothing is queued or guessed at.
    assert_eq!(outcome.failed, 0);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        !commands.iter().any(|command| {
            let command = command.trim().to_ascii_uppercase();
            command.starts_with("UID STORE")
                || command.starts_with("UID EXPUNGE")
                || command == "EXPUNGE"
        }),
        "an unattributed draft copy must never be deleted: {commands:?}"
    );

    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts resync");
    assert_eq!(
        mock.state.message_count("Drafts"),
        1,
        "the old remote draft must stay on the server"
    );
    let indexed = only_message(&ctx, &drafts_folder).await;
    assert!(
        indexed.is_draft,
        "the old remote draft must stay visible in Drafts"
    );
}

/// With UIDPLUS the same discard path cleans up both the server copy and its
/// local projection.
#[tokio::test]
async fn discard_with_uidplus_removes_the_remote_draft() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Discard cleanly".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("append draft");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    assert_eq!(mock.state.message_count("Drafts"), 1);

    service.compose_discard(&draft_id).await.expect("discard");
    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("delete remote draft");
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.failed, 0);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        commands.iter().any(|command| {
            let command = command.to_ascii_uppercase();
            command.starts_with("UID EXPUNGE")
        }),
        "the remote draft must be removed with a directed expunge: {commands:?}"
    );
    assert_eq!(mock.state.message_count("Drafts"), 0);
    assert!(
        summaries(&ctx, &drafts_folder).await.is_empty(),
        "the projection row is removed only after the server confirms"
    );
}

/// Sending a draft with UIDPLUS cleans up the superseded server draft.
#[tokio::test]
async fn send_with_uidplus_removes_the_superseded_remote_draft() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS", "MOVE"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Drafts", &["\\Drafts"]),
            ("Sent", &["\\Sent"]),
        ],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let smtp = MockSmtp::start().await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        Some(server(mock.addr.port())),
        Some(server(smtp.addr.port())),
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Send me".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("append draft");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    assert_eq!(mock.state.message_count("Drafts"), 1);

    service.compose_send(&draft).await.expect("send");
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("cleanup ops");

    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts resync");
    assert_eq!(
        mock.state.message_count("Drafts"),
        0,
        "the send supersedes and removes the remote draft"
    );
    assert!(
        summaries(&ctx, &drafts_folder).await.is_empty(),
        "the local projection is removed after the confirmed delete"
    );
}

/// Opening a draft created by another client must still replace its server
/// copy: the old UID belongs to the external message row, not to the new local
/// projection, so the ownership check has to consider both identities.
#[tokio::test]
async fn external_draft_is_replaced_instead_of_skipped() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let external_uid = mock.state.add_message(
        "Drafts",
        &message_raw(
            "bob@example.com",
            "test@example.com",
            "External draft",
            "external body",
            "external-draft@example.com",
        ),
    );
    mock.state.set_flags("Drafts", external_uid, &["\\Draft"]);
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let external = only_message(&ctx, &drafts_folder).await;
    assert!(external.is_draft, "the server copy is a draft");
    let mut draft = service
        .compose_open_draft(&external.id)
        .await
        .expect("open external draft");
    assert!(draft.id.is_none(), "an external draft has no local id yet");
    draft.subject = "Updated external draft".into();
    draft.html = "<p>updated body</p>".into();
    let saved = service
        .compose_save(&draft)
        .await
        .expect("save external draft");
    let draft_id = saved.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("replace external draft");
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.failed, 0);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        commands
            .iter()
            .any(|command| { command.trim().to_ascii_uppercase().starts_with("APPEND") }),
        "the replacement must actually append: {commands:?}"
    );
    assert_eq!(
        mock.state.message_count("Drafts"),
        1,
        "only one remote draft remains"
    );
    let bodies = mock.state.message_bodies("Drafts");
    let body = String::from_utf8_lossy(&bodies[0]);
    assert!(
        body.contains("Updated external draft"),
        "the remaining copy holds the new content: {body}"
    );

    let projection = stored_draft(&ctx, &draft_id).await;
    assert_eq!(
        projection.uid,
        Some(2),
        "APPENDUID binds the replacement to the new projection"
    );
    let external_id = external.id.clone();
    let superseded = ctx
        .db
        .read(move |db| repo::messages::get(db, &external_id))
        .await
        .expect("read external row");
    assert!(
        superseded.is_none(),
        "the superseded external projection must not linger in Drafts"
    );

    // A later save keeps replacing the one remaining copy instead of skipping.
    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    draft.subject = "Third external edit".into();
    draft.html = "<p>third</p>".into();
    service.compose_save(&draft).await.expect("save again");
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("second replacement");
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.failed, 0);
    assert_eq!(mock.state.message_count("Drafts"), 1);
}

/// A UID may only be written back to the exact Drafts projection it belongs
/// to. Moving the not-yet-uploaded draft to another folder (both folders have
/// the same UIDVALIDITY) must not receive the APPENDUID.
#[tokio::test]
async fn appenduid_does_not_bind_a_projection_moved_to_another_folder() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Drafts", &["\\Drafts"]),
            ("Archive", &["\\Archive"]),
        ],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let archive_folder = folder_id(&ctx, "acc", "Archive").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Moved before upload".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    // Both folders report UIDVALIDITY 1 in the mock, so only the folder/state
    // condition can prevent a misbinding.
    let projection_id = nuncio_mail::util::draft_message_id(&draft_id);
    service
        .message_act(
            std::slice::from_ref(&projection_id),
            &MessageAction::Move {
                folder_id: archive_folder.clone(),
            },
        )
        .await
        .expect("move the pending projection");

    let mut connection = connect(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("append after the move");
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.failed, 0);
    assert!(
        outcome.resync_folders.contains(&drafts_folder),
        "the Drafts folder must be scheduled for a targeted sync"
    );
    assert_eq!(
        mock.state.message_count("Drafts"),
        1,
        "the server copy is preserved"
    );

    let moved_id = projection_id.clone();
    let moved = ctx
        .db
        .read(move |db| repo::messages::get_required(db, &moved_id))
        .await
        .expect("moved row");
    assert_eq!(moved.summary.folder_id, archive_folder);
    assert_eq!(
        moved.uid, None,
        "the APPENDUID must not be written into the folder the row moved to"
    );

    // The targeted sync adopts the server copy in Drafts without touching the
    // moved row.
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    let moved_id = projection_id.clone();
    let moved = ctx
        .db
        .read(move |db| repo::messages::get_required(db, &moved_id))
        .await
        .expect("moved row after sync");
    assert_eq!(moved.summary.folder_id, archive_folder);
    assert_eq!(moved.uid, None);
    assert_eq!(summaries(&ctx, &drafts_folder).await.len(), 1);
}

/// An APPEND whose literal reached the server but whose completion was lost
/// must be verified in Drafts, not repeated.
#[tokio::test]
async fn lost_append_receipt_is_verified_instead_of_reappended() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    mock.state.set_append_drop_completion(true);
    let (ctx, account) = drafts_account(&mock).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Lost receipt".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("append with a lost completion");
    assert_eq!(outcome.done, 0);
    assert_eq!(outcome.retried, 1);
    assert_eq!(
        mock.state.message_count("Drafts"),
        1,
        "the server committed the message even though the client saw an error"
    );

    // Next attempt: the server is healthy again, and the operation must check
    // Drafts before doing anything.
    mock.state.set_append_drop_completion(false);
    make_due(&ctx, ops::KIND_APPEND_DRAFT).await;
    let mut connection = connect(&account).await;
    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("retry after the lost completion");
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.failed, 0);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        !commands
            .iter()
            .any(|command| { command.trim().to_ascii_uppercase().starts_with("APPEND") }),
        "a verified APPEND must not be repeated: {commands:?}"
    );
    assert_eq!(mock.state.message_count("Drafts"), 1);
    let projection = stored_draft(&ctx, &draft_id).await;
    assert_eq!(
        projection.uid,
        Some(1),
        "the Drafts sync binds the copy that already landed"
    );
}

/// After the server accepted the APPEND, a failed local commit is retried on
/// its own; the APPEND itself is never sent again.
#[tokio::test]
async fn acked_append_retries_only_the_local_commit() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Acked commit".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    // Simulate the durable state left by a successful APPEND whose local
    // commit failed: the UID is on the operation, not on the projection.
    let kind = ops::KIND_APPEND_DRAFT.to_string();
    ctx.db
        .write(move |db| {
            for op in repo::pending_ops::list(db, Some("acc"))? {
                if op.kind != kind {
                    continue;
                }
                let mut payload: ops::AppendDraftOp = repo::pending_ops::payload(&op)?;
                payload.append_state = ops::AppendState::Acked;
                payload.appended_uid = Some(7);
                payload.appended_uidvalidity = Some(1);
                let value = serde_json::to_value(payload)
                    .map_err(|error| nuncio_core::AppError::internal(error.to_string()))?;
                repo::pending_ops::update_payload(db, &op.id, &value)?;
            }
            Ok(())
        })
        .await
        .expect("mark the APPEND acked");

    let before = mock.state.commands().len();
    let mut connection = connect(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("acked retry");
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.failed, 0);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        !commands
            .iter()
            .any(|command| { command.trim().to_ascii_uppercase().starts_with("APPEND") }),
        "an acked APPEND must never be re-sent: {commands:?}"
    );
    let projection = stored_draft(&ctx, &draft_id).await;
    assert_eq!(projection.uid, Some(7));
    assert_eq!(projection.uidvalidity, Some(1));
    assert_eq!(mock.state.message_count("Drafts"), 0);
}

/// Make every queued operation of `kind` run on the next pass, ignoring the
/// exponential backoff a failed attempt scheduled.
async fn make_due(ctx: &CoreContext, kind: &str) {
    let kind = kind.to_string();
    ctx.db
        .write(move |db| {
            db.execute(
                "UPDATE pending_op SET next_retry_at = NULL WHERE kind = ?1",
                [kind],
            )?;
            Ok(())
        })
        .await
        .expect("reset retry time");
}

fn set_append_gate(mock: &MockImap, permits: usize) -> Arc<tokio::sync::Semaphore> {
    let gate = Arc::new(tokio::sync::Semaphore::new(permits));
    mock.state.set_append_gate(gate.clone());
    gate
}

async fn wait_for_append_command(mock: &MockImap) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if mock
            .state
            .commands()
            .iter()
            .any(|command| command.trim().to_ascii_uppercase().starts_with("APPEND"))
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the APPEND never reached the server"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn append_command_count(mock: &MockImap) -> usize {
    mock.state
        .commands()
        .iter()
        .filter(|command| command.trim().to_ascii_uppercase().starts_with("APPEND"))
        .count()
}

/// Wait for one more APPEND than `previous` (a first, settled append already
/// logged one on this mock).
async fn wait_for_new_append(mock: &MockImap, previous: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if append_command_count(mock) > previous {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the next APPEND never reached the server"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A lost completion whose copy is not visible yet must not trigger a second
/// APPEND. The copy may land later (or be outside this query's view), so
/// absence is not proof; the operation stays queued for verification.
#[tokio::test]
async fn unknown_append_is_not_resent_before_the_copy_is_visible() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    mock.state.set_append_drop_completion(true);
    let (ctx, account) = drafts_account(&mock).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Delayed landing".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("lost completion");
    assert_eq!(outcome.retried, 1, "the lost completion is retryable");
    assert_eq!(mock.state.message_count("Drafts"), 1);
    assert_eq!(
        append_state(&ctx, &draft_id).await.append_state,
        ops::AppendState::Unknown,
        "the durable phase remembers the uncertainty"
    );

    // The committed copy is not visible to the next query (a delayed request
    // landing after it, or a server that has not exposed it yet).
    mock.state.remove_message("Drafts", 1);
    mock.state.set_append_drop_completion(false);
    make_due(&ctx, ops::KIND_APPEND_DRAFT).await;
    // The lost completion also dropped the mock connection.
    let mut connection = connect(&account).await;
    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("verify without a visible copy");
    assert_eq!(outcome.done, 0, "the operation must not settle as done");
    assert!(
        outcome.deferred >= 1,
        "an inconclusive verification stays queued: {outcome:?}"
    );
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        !commands
            .iter()
            .any(|command| { command.trim().to_ascii_uppercase().starts_with("APPEND") }),
        "an unproven absence must never cause a second APPEND: {commands:?}"
    );
    assert_eq!(
        append_state(&ctx, &draft_id).await.append_state,
        ops::AppendState::Unknown
    );

    // The previous literal finally becomes visible; verification now finds it
    // and settles without uploading again.
    let staged_path = append_state(&ctx, &draft_id).await.rfc822_path;
    let raw = tokio::fs::read(&staged_path).await.expect("staged draft");
    mock.state.add_message("Drafts", &raw);
    make_due(&ctx, ops::KIND_APPEND_DRAFT).await;
    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("verify after the copy lands");
    assert_eq!(outcome.done, 1);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        !commands
            .iter()
            .any(|command| { command.trim().to_ascii_uppercase().starts_with("APPEND") }),
        "the verified copy must not be appended again: {commands:?}"
    );
    assert_eq!(mock.state.message_count("Drafts"), 1);
}

/// A server copy without the stable draft header cannot be correlated with the
/// pending APPEND. That is inconclusive, not evidence of absence.
#[tokio::test]
async fn unknown_append_without_a_draft_header_stays_queued() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    mock.state.set_append_drop_completion(true);
    let (ctx, account) = drafts_account(&mock).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Headerless".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("lost completion");
    // Replace the committed copy with one that lacks the draft header.
    mock.state.remove_message("Drafts", 1);
    mock.state.add_message(
        "Drafts",
        &message_raw(
            "bob@example.com",
            "test@example.com",
            "not our header",
            "body",
            "unrelated@example.com",
        ),
    );
    mock.state.set_append_drop_completion(false);

    make_due(&ctx, ops::KIND_APPEND_DRAFT).await;
    // The dropped completion closed the previous session.
    let mut connection = connect(&account).await;
    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("verify without a header");
    assert_eq!(outcome.done, 0);
    assert!(outcome.deferred >= 1);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        !commands
            .iter()
            .any(|command| { command.trim().to_ascii_uppercase().starts_with("APPEND") }),
        "a missing header must not be mistaken for absence: {commands:?}"
    );
    assert_eq!(
        append_state(&ctx, &draft_id).await.append_state,
        ops::AppendState::Unknown
    );
}

/// The only copy the sync can see may be the one the replacement was supposed
/// to delete. That is not proof that the new APPEND landed.
#[tokio::test]
async fn unknown_append_showing_only_the_old_copy_stays_queued() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "First version".into();
    draft.html = "<p>first</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first append");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    let old_uid = stored_draft(&ctx, &draft_id).await.uid.expect("bound uid");

    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    draft.subject = "Second version".into();
    draft.html = "<p>second</p>".into();
    service.compose_save(&draft).await.expect("second save");
    let (op_id, _) = append_op(&ctx, &draft_id).await;
    // Model a crash after the phase was persisted but before/around the
    // upload: the old copy is still the only one visible and the pre-send
    // UIDNEXT marker is unavailable (e.g. a server that does not report it).
    force_append_payload(&ctx, &op_id, move |payload| {
        payload.append_state = ops::AppendState::Unknown;
        payload.attempt_uidnext = None;
        payload.attempt_uidvalidity = Some(1);
    })
    .await;

    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("verify old copy");
    assert_eq!(outcome.done, 0);
    assert!(outcome.deferred >= 1);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    for forbidden in ["APPEND", "UID STORE", "UID EXPUNGE", "EXPUNGE"] {
        assert!(
            !commands.iter().any(|command| {
                let command = command.trim().to_ascii_uppercase();
                command == forbidden || command.starts_with(&format!("{forbidden} "))
            }),
            "the superseded copy must not be treated as the new APPEND: {commands:?}"
        );
    }
    assert_eq!(mock.state.message_count("Drafts"), 1);
    assert_eq!(stored_draft(&ctx, &draft_id).await.uid, Some(old_uid));
}

/// An `Acked` receipt from an older UIDVALIDITY must not overwrite a binding a
/// later sync installed in the current generation.
#[tokio::test]
async fn acked_append_after_generation_change_does_not_rebind() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Generation".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let (op_id, _) = append_op(&ctx, &draft_id).await;

    let projection_id = nuncio_mail::util::draft_message_id(&draft_id);
    let folder_for_db = drafts_folder.clone();
    let projection_for_db = projection_id.clone();
    ctx.db
        .write(move |db| {
            // The folder was re-fetched in a new generation and the projection
            // was rebuilt with its new identity.
            repo::folders::set_sync_flags(db, &folder_for_db, Some(2), None, None)?;
            db.execute(
                "UPDATE message SET uid = 9, uidvalidity = 2, updated_at = ?2 WHERE id = ?1",
                rusqlite::params![projection_for_db, nuncio_core::repo::now_string()],
            )?;
            Ok(())
        })
        .await
        .expect("install new generation");
    force_append_payload(&ctx, &op_id, |payload| {
        payload.append_state = ops::AppendState::Acked;
        payload.appended_uid = Some(7);
        payload.appended_uidvalidity = Some(1);
        payload.discard_requested = false;
    })
    .await;

    let mut connection = connect(&account).await;
    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("acked after generation change");
    assert_eq!(outcome.done, 1);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        !commands
            .iter()
            .any(|command| { command.trim().to_ascii_uppercase().starts_with("APPEND") }),
        "an acked APPEND is never re-sent: {commands:?}"
    );
    let projection = stored_draft(&ctx, &draft_id).await;
    assert_eq!(
        projection.uid,
        Some(9),
        "the stale receipt must not replace the new-generation binding"
    );
    assert_eq!(projection.uidvalidity, Some(2));
}

/// A UID-less receipt (no UIDPLUS) must not clear a UID a sync already bound.
#[tokio::test]
async fn acked_without_uid_keeps_a_synced_binding() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "No uid receipt".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("append");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    let synced_uid = stored_draft(&ctx, &draft_id).await.uid.expect("bound uid");

    let (op_id, _) = append_op(&ctx, &draft_id).await;
    let folder_for_db = drafts_folder.clone();
    let projection_id = nuncio_mail::util::draft_message_id(&draft_id);
    ctx.db
        .write(move |db| {
            repo::folders::set_sync_flags(db, &folder_for_db, Some(1), None, None)?;
            db.execute(
                "UPDATE message SET uid = ?2, uidvalidity = 1 WHERE id = ?1",
                rusqlite::params![projection_id, synced_uid],
            )?;
            Ok(())
        })
        .await
        .expect("restore synced binding");
    force_append_payload(&ctx, &op_id, |payload| {
        payload.append_state = ops::AppendState::Acked;
        payload.appended_uid = None;
        payload.appended_uidvalidity = None;
        payload.discard_requested = false;
    })
    .await;

    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("acked without uid");
    assert_eq!(outcome.done, 1);
    let projection = stored_draft(&ctx, &draft_id).await;
    assert_eq!(
        projection.uid,
        Some(synced_uid),
        "a receipt without UIDPLUS has nothing stale to clear"
    );
    assert_eq!(projection.uidvalidity, Some(1));
}

/// The staged bytes validated before the replacement are the bytes uploaded.
/// If the staging area changes between validation and upload, the executor must
/// not delete the old copy and then send an unvalidated version.
#[tokio::test]
async fn staged_change_between_validation_and_upload_uploads_the_validated_bytes() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "First validated".into();
    draft.html = "<p>first</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first append");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");

    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    draft.subject = "Second validated".into();
    draft.html = "<p>second</p>".into();
    service.compose_save(&draft).await.expect("second save");
    let (_, payload) = append_op(&ctx, &draft_id).await;
    let path = payload.rfc822_path.clone();
    mock.state.set_on_expunge(move || {
        // Runs while the old copy is expunged, i.e. after the validation read
        // and before the upload.
        std::fs::write(
            &path,
            b"From: evil@example.com\r\nSubject: unvalidated\r\n\r\nswapped\r\n",
        )
        .expect("replace staged draft");
    });

    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("replacement append");
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.failed, 0);
    assert_eq!(mock.state.message_count("Drafts"), 1);
    let body = String::from_utf8_lossy(&mock.state.message_bodies("Drafts")[0]).into_owned();
    assert!(
        body.contains("Second validated"),
        "the validated bytes must be the ones uploaded: {body}"
    );
    assert!(
        !body.contains("unvalidated"),
        "the swapped staging file must never reach the server: {body}"
    );
}

/// Discarding while the APPEND is genuinely in flight keeps the cleanup intent
/// and removes the uploaded copy once its UID is known.
#[tokio::test]
async fn discard_while_append_is_in_flight_removes_the_copy() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let gate = set_append_gate(&mock, 0);
    let (ctx, account) = drafts_account(&mock).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "In flight discard".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let worker_ctx = ctx.clone();
    let worker_account = account.clone();
    let worker = tokio::spawn(async move {
        let mut connection = connect(&worker_account).await;
        ops::executor::run_due(&worker_ctx, &mut connection, &worker_account).await
    });
    wait_for_append_command(&mock).await;
    assert_eq!(
        append_state(&ctx, &draft_id).await.append_state,
        ops::AppendState::Unknown,
        "the upload is on the wire and cannot be cancelled atomically"
    );

    service.compose_discard(&draft_id).await.expect("discard");
    let payload = append_state(&ctx, &draft_id).await;
    assert!(
        payload.discard_requested,
        "the cleanup intent must survive the discard"
    );
    gate.add_permits(1);
    let outcome = worker.await.expect("worker task").expect("run due");
    assert_eq!(outcome.done, 1);
    assert_eq!(
        mock.state.message_count("Drafts"),
        0,
        "the in-flight copy must be removed after the upload completes"
    );
    let (_, payload) = append_op(&ctx, &draft_id).await;
    assert_eq!(payload.append_state, ops::AppendState::Acked);
}

/// Sending while the APPEND is in flight must deliver the mail and still remove
/// the draft copy the upload created.
#[tokio::test]
async fn send_while_append_is_in_flight_removes_the_copy() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS", "MOVE"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Drafts", &["\\Drafts"]),
            ("Sent", &["\\Sent"]),
        ],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let smtp = MockSmtp::start().await;
    let gate = set_append_gate(&mock, 0);
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        Some(server(mock.addr.port())),
        Some(server(smtp.addr.port())),
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "In flight send".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let worker_ctx = ctx.clone();
    let worker_account = account.clone();
    let worker = tokio::spawn(async move {
        let mut connection = connect(&worker_account).await;
        ops::executor::run_due(&worker_ctx, &mut connection, &worker_account).await
    });
    wait_for_append_command(&mock).await;
    assert_eq!(
        append_state(&ctx, &draft_id).await.append_state,
        ops::AppendState::Unknown
    );

    service.compose_send(&draft).await.expect("send");
    gate.add_permits(1);
    let outcome = worker.await.expect("worker task").expect("run due");
    assert_eq!(outcome.done, 1);
    assert_eq!(
        mock.state.message_count("Drafts"),
        0,
        "the superseded in-flight draft copy must be removed"
    );
    assert_eq!(
        smtp.messages.lock().expect("smtp").len(),
        1,
        "the send must deliver the message"
    );
}

/// A failure to persist the APPEND receipt must leave the operation in
/// `Unknown`; the retry verifies the committed copy instead of appending again.
#[tokio::test]
async fn failed_receipt_persistence_verifies_instead_of_reappending() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Receipt failure".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    ctx.db
        .write(|db| {
            db.execute(
                "CREATE TRIGGER fail_ack_receipt BEFORE UPDATE OF payload_json ON pending_op
                 WHEN NEW.payload_json LIKE '%\"appendState\":\"acked\"%'
                 BEGIN SELECT RAISE(ABORT, 'receipt write failed'); END;",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("install receipt fault");

    let mut connection = connect(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("receipt failure pass");
    assert_eq!(outcome.done, 0);
    assert_eq!(outcome.retried, 1, "the DB error is retryable");
    assert_eq!(mock.state.message_count("Drafts"), 1);
    assert_eq!(
        append_state(&ctx, &draft_id).await.append_state,
        ops::AppendState::Unknown,
        "the lost ack must be recorded as unknown, not re-sent"
    );

    ctx.db
        .write(|db| {
            db.execute("DROP TRIGGER fail_ack_receipt", [])?;
            Ok(())
        })
        .await
        .expect("remove receipt fault");
    make_due(&ctx, ops::KIND_APPEND_DRAFT).await;
    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("retry verification");
    assert_eq!(outcome.done, 1);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        !commands
            .iter()
            .any(|command| { command.trim().to_ascii_uppercase().starts_with("APPEND") }),
        "the committed copy must be verified, not appended again: {commands:?}"
    );
    assert_eq!(mock.state.message_count("Drafts"), 1);
}

/// A failure to bind the server UID to the projection must not lose the
/// receipt: the retry only commits locally and never APPENDs again.
#[tokio::test]
async fn failed_projection_commit_retries_the_binding_only() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Commit failure".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let projection_id = nuncio_mail::util::draft_message_id(&draft_id);

    let trigger_sql = format!(
        "CREATE TRIGGER fail_projection_commit BEFORE UPDATE ON message
         WHEN NEW.id = '{projection_id}' AND NEW.uid IS NOT NULL
         BEGIN SELECT RAISE(ABORT, 'projection commit failed'); END;"
    );
    ctx.db
        .write(move |db| {
            db.execute(&trigger_sql, [])?;
            Ok(())
        })
        .await
        .expect("install projection fault");

    let mut connection = connect(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("projection failure pass");
    assert_eq!(outcome.done, 0);
    assert_eq!(outcome.retried, 1);
    assert_eq!(mock.state.message_count("Drafts"), 1);
    let payload = append_state(&ctx, &draft_id).await;
    assert_eq!(
        payload.append_state,
        ops::AppendState::Acked,
        "the accepted UID is durable even though the projection write failed"
    );

    ctx.db
        .write(|db| {
            db.execute("DROP TRIGGER fail_projection_commit", [])?;
            Ok(())
        })
        .await
        .expect("remove projection fault");
    make_due(&ctx, ops::KIND_APPEND_DRAFT).await;
    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("retry binding");
    assert_eq!(outcome.done, 1);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        !commands
            .iter()
            .any(|command| { command.trim().to_ascii_uppercase().starts_with("APPEND") }),
        "an acked APPEND must never be re-sent: {commands:?}"
    );
    let projection = stored_draft(&ctx, &draft_id).await;
    assert_eq!(projection.uid, Some(1));
    assert_eq!(mock.state.message_count("Drafts"), 1);
}

/// A same-header copy made by another client (or a previous attempt) makes the
/// unacknowledged APPEND ambiguous. The discard must stay pending instead of
/// deleting one of the candidates.
#[tokio::test]
async fn discard_of_ambiguous_same_header_copies_never_deletes() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    mock.state.set_append_drop_completion(true);
    let (ctx, account) = drafts_account(&mock).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Ambiguous copy".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("lost completion");
    assert_eq!(mock.state.message_count("Drafts"), 1);

    // Another copy with the same stable draft header appears while the client
    // never learned the first upload's identity.
    let raw = mock.state.message_bodies("Drafts").remove(0);
    mock.state.add_message("Drafts", &raw);
    mock.state.set_append_drop_completion(false);

    service.compose_discard(&draft_id).await.expect("discard");
    make_due(&ctx, ops::KIND_APPEND_DRAFT).await;
    // The lost completion also closed the previous session.
    let mut connection = connect(&account).await;
    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("verify ambiguous copies");
    assert_eq!(
        outcome.done, 1,
        "an unattributable cleanup becomes a terminal keep-remote state"
    );
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    for forbidden in ["APPEND", "UID STORE", "UID EXPUNGE", "EXPUNGE"] {
        assert!(
            !commands.iter().any(|command| {
                let command = command.trim().to_ascii_uppercase();
                command == forbidden || command.starts_with(&format!("{forbidden} "))
            }),
            "two same-header copies must never be deleted by guess: {commands:?}"
        );
    }
    assert_eq!(mock.state.message_count("Drafts"), 2);
    let payload = append_state(&ctx, &draft_id).await;
    assert!(payload.discard_requested);
    assert!(payload.candidate_only, "the kept copy stays unattributable");
    assert!(
        payload.cleanup_done,
        "keep remote is a finished cleanup, not an endless verification"
    );
}

/// Without the pre-send UIDNEXT watermark a found copy cannot be attributed to
/// this upload. The discard keeps it instead of deleting a guess.
#[tokio::test]
async fn discard_without_a_send_watermark_never_deletes() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    mock.state.set_append_drop_completion(true);
    let (ctx, account) = drafts_account(&mock).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "No watermark".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("lost completion");
    let (op_id, _) = append_op(&ctx, &draft_id).await;
    force_append_payload(&ctx, &op_id, |payload| {
        payload.append_state = ops::AppendState::Unknown;
        payload.attempt_uidnext = None;
        payload.discard_requested = true;
        payload.cleanup_done = false;
    })
    .await;
    // The lost completion also closed the previous session.
    let mut connection = connect(&account).await;

    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("verify without a watermark");
    // The local draft row still exists here, so an unconfirmed copy stays
    // queued; only a removed draft settles as keep-remote.
    assert_eq!(outcome.done, 0);
    assert!(outcome.deferred >= 1);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        !commands.iter().any(|command| {
            let command = command.trim().to_ascii_uppercase();
            command.starts_with("UID STORE")
                || command.starts_with("UID EXPUNGE")
                || command == "EXPUNGE"
        }),
        "a copy without a watermark must never be deleted: {commands:?}"
    );
    assert_eq!(mock.state.message_count("Drafts"), 1);
    assert!(append_state(&ctx, &draft_id).await.discard_requested);
}

/// A folder rebuild between the upload and the verification makes the queued
/// UID meaningless. The candidate must be kept.
#[tokio::test]
async fn discard_across_a_generation_change_never_deletes() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    mock.state.set_append_drop_completion(true);
    let (ctx, account) = drafts_account(&mock).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Rebuilt folder".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("lost completion");
    // The server rebuilt Drafts while the verification was pending.
    mock.state.bump_uidvalidity("Drafts", 2);
    service.compose_discard(&draft_id).await.expect("discard");
    make_due(&ctx, ops::KIND_APPEND_DRAFT).await;
    // The lost completion also closed the previous session.
    let mut connection = connect(&account).await;

    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("verify across generations");
    assert_eq!(outcome.done, 1);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        !commands.iter().any(|command| {
            let command = command.trim().to_ascii_uppercase();
            command.starts_with("UID STORE")
                || command.starts_with("UID EXPUNGE")
                || command == "EXPUNGE"
        }),
        "a cross-generation UID must not be used for deletion: {commands:?}"
    );
    assert_eq!(mock.state.message_count("Drafts"), 1);
    let payload = append_state(&ctx, &draft_id).await;
    assert!(payload.discard_requested);
    assert!(payload.cleanup_done);
}

/// A lost completion leaves no APPENDUID, so even a unique, fully matching
/// same-header copy is only a candidate. The stable header, size and arrival
/// time can all be reproduced by another client between the persisted
/// `Unknown` marker and the literal, so the copy must be kept.
#[tokio::test]
async fn discard_with_unique_lost_append_evidence_never_deletes() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    mock.state.set_append_drop_completion(true);
    let (ctx, account) = drafts_account(&mock).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Unique evidence".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("lost completion");
    mock.state.set_append_drop_completion(false);
    service.compose_discard(&draft_id).await.expect("discard");
    make_due(&ctx, ops::KIND_APPEND_DRAFT).await;
    // The lost completion also closed the previous session.
    let mut connection = connect(&account).await;

    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("verify the candidate copy");
    assert_eq!(
        outcome.done, 1,
        "a keep-remote cleanup is terminal once the local draft is gone"
    );
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    for forbidden in ["APPEND", "UID STORE", "UID EXPUNGE", "EXPUNGE"] {
        assert!(
            !commands.iter().any(|command| {
                let command = command.trim().to_ascii_uppercase();
                command == forbidden || command.starts_with(&format!("{forbidden} "))
            }),
            "a candidate copy must never be deleted or re-appended: {commands:?}"
        );
    }
    assert_eq!(mock.state.message_count("Drafts"), 1);
    let payload = append_state(&ctx, &draft_id).await;
    assert!(payload.discard_requested);
    assert!(payload.candidate_only, "the ownership limit is durable");
    assert!(
        payload.cleanup_done,
        "keep remote is terminal; no five-minute verification remains"
    );
}

/// A discard written after the executor's last intent read (during the local
/// commit) must still be honoured by the finishing transaction; the copy is
/// cleaned up on the next pass.
#[tokio::test]
async fn discard_written_before_finish_keeps_the_row_executable() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Late discard".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let projection_id = nuncio_mail::util::draft_message_id(&draft_id);

    // Fires while the executor binds the accepted UID: after its last
    // `load_append_payload`, before `finish_ops`.
    let trigger = format!(
        "CREATE TRIGGER inject_late_discard AFTER UPDATE OF uid ON message
         WHEN NEW.uid IS NOT NULL AND NEW.id = '{projection_id}'
         BEGIN
           UPDATE pending_op
              SET payload_json = json_set(payload_json, '$.discardRequested', json('true'))
            WHERE kind = 'appendDraft' AND state IN ('pending', 'inProgress');
         END;"
    );
    ctx.db
        .write(move |db| {
            db.execute(&trigger, [])?;
            Ok(())
        })
        .await
        .expect("install late discard trigger");

    let mut connection = connect(&account).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("append with a late discard");
    assert_eq!(outcome.done, 0, "a pending cleanup must not settle as done");
    assert!(outcome.deferred >= 1);
    assert_eq!(mock.state.message_count("Drafts"), 1);
    let payload = append_state(&ctx, &draft_id).await;
    assert!(payload.discard_requested);
    assert!(!payload.cleanup_done);

    ctx.db
        .write(|db| {
            db.execute("DROP TRIGGER inject_late_discard", [])?;
            Ok(())
        })
        .await
        .expect("remove trigger");
    make_due(&ctx, ops::KIND_APPEND_DRAFT).await;
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("clean up the late discard");
    assert_eq!(outcome.done, 1);
    assert_eq!(
        mock.state.message_count("Drafts"),
        0,
        "the late discard must eventually remove the uploaded copy"
    );
}

/// Once the old copy is confirmed removed, a rejected APPEND retries only the
/// upload. A sync may already have dropped the old local row; that must not
/// make the retry skip the upload or delete the old UID again.
#[tokio::test]
async fn rejected_append_after_removal_resumes_the_upload_only() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "First version".into();
    draft.html = "<p>first</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first append");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    let old_uid = stored_draft(&ctx, &draft_id).await.uid.expect("old uid");
    assert_eq!(mock.state.message_count("Drafts"), 1);

    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    draft.subject = "Second version".into();
    draft.html = "<p>second</p>".into();
    service.compose_save(&draft).await.expect("second save");

    // The delete succeeds, the upload is explicitly refused.
    mock.state.set_fail_append(true);
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("replacement with a refused APPEND");
    assert_eq!(outcome.done, 0);
    assert_eq!(
        mock.state.message_count("Drafts"),
        0,
        "the old copy is gone and the new one was not stored"
    );
    let payload = append_state(&ctx, &draft_id).await;
    assert_eq!(
        payload.removal_state,
        ops::DraftRemovalState::Removed,
        "the confirmed removal is durable"
    );
    assert_eq!(
        payload.append_state,
        ops::AppendState::Preparing,
        "only the APPEND remains to be retried"
    );

    // A Drafts sync sees the old UID is gone and drops/rebinds the local row.
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("sync after removal");

    mock.state.set_fail_append(false);
    make_due(&ctx, ops::KIND_APPEND_DRAFT).await;
    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("retry the upload only");
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.failed, 0);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    let deleted_old = commands.iter().any(|command| {
        let command = command.trim().to_ascii_uppercase();
        command.starts_with("UID STORE") || command.starts_with("UID EXPUNGE")
    });
    assert!(
        !deleted_old,
        "the old UID must not be deleted a second time: {commands:?} (old uid {old_uid})"
    );
    assert_eq!(mock.state.message_count("Drafts"), 1);
    let bodies = mock.state.message_bodies("Drafts");
    assert!(
        String::from_utf8_lossy(&bodies[0]).contains("Second version"),
        "the replacement must actually be uploaded"
    );
}

/// A crash between the confirmed EXPUNGE and recording it leaves
/// `PendingRemove`. A sync may have dropped the old local row; the retry still
/// has to verify the server before uploading.
#[tokio::test]
async fn crash_after_removal_is_verified_before_uploading() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "First crash version".into();
    draft.html = "<p>first</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first append");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");

    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    draft.subject = "Second crash version".into();
    draft.html = "<p>second</p>".into();
    service.compose_save(&draft).await.expect("second save");
    let (op_id, _) = append_op(&ctx, &draft_id).await;

    // Model a crash after the EXPUNGE succeeded but before `Removed` was
    // recorded: delete the copy out of band, then force the durable phase.
    let old_uid = stored_draft(&ctx, &draft_id).await.uid.expect("old uid");
    mock.state.remove_message("Drafts", old_uid as u32);
    force_append_payload(&ctx, &op_id, |payload| {
        payload.append_state = ops::AppendState::Preparing;
        payload.removal_state = ops::DraftRemovalState::PendingRemove;
        payload.appended_uid = None;
        payload.appended_uidvalidity = None;
    })
    .await;
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("sync after the crash");

    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("verify removal and upload");
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.failed, 0);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        !commands.iter().any(|command| {
            let command = command.trim().to_ascii_uppercase();
            command.starts_with("UID STORE") || command.starts_with("UID EXPUNGE")
        }),
        "a verified-absent old copy must not be expunged again: {commands:?}"
    );
    assert_eq!(mock.state.message_count("Drafts"), 1);
    assert!(
        String::from_utf8_lossy(&mock.state.message_bodies("Drafts")[0])
            .contains("Second crash version"),
        "the retry must upload the new draft"
    );
}

/// A stale cached watermark can sit below a copy that already existed. The
/// copy's arrival time predates the literal, so it is not evidence of this
/// upload and must not be deleted.
#[tokio::test]
async fn discard_with_a_stale_watermark_never_deletes() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    mock.state.set_append_drop_completion(true);
    let (ctx, account) = drafts_account(&mock).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Stale watermark".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("lost completion");
    let (op_id, _) = append_op(&ctx, &draft_id).await;
    force_append_payload(&ctx, &op_id, |payload| {
        payload.append_state = ops::AppendState::Unknown;
        // The cache believed the next UID was 0 while the visible copy already
        // had a higher UID, and it arrived before the send.
        payload.attempt_uidnext = Some(0);
        payload.attempt_at = Some(chrono::Utc::now() + chrono::Duration::hours(2));
        payload.discard_requested = true;
        payload.cleanup_done = false;
    })
    .await;
    let mut connection = connect(&account).await;

    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("verify with a stale watermark");
    assert_eq!(outcome.done, 0);
    assert!(outcome.deferred >= 1);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        !commands.iter().any(|command| {
            let command = command.trim().to_ascii_uppercase();
            command.starts_with("UID STORE")
                || command.starts_with("UID EXPUNGE")
                || command == "EXPUNGE"
        }),
        "a copy predating the literal must not be deleted: {commands:?}"
    );
    assert_eq!(mock.state.message_count("Drafts"), 1);
    assert!(append_state(&ctx, &draft_id).await.discard_requested);
}

/// The dangerous race: the durable `Unknown` marker is written, the process
/// dies before the literal, and another client writes a copy with the same
/// header. Every inference condition (unique copy, size, arrival time, UID
/// above the watermark) can match, but nothing attributes it to this attempt.
/// Recovery must keep it.
#[tokio::test]
async fn unknown_append_without_upload_never_deletes_an_external_copy() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "External copy".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let (op_id, payload) = append_op(&ctx, &draft_id).await;
    let raw = tokio::fs::read(&payload.rfc822_path)
        .await
        .expect("staged draft");

    // Another client writes the same bytes while this attempt never uploaded.
    mock.state.add_message("Drafts", &raw);
    // Model a crash right after `mark_append_unknown`: no APPENDUID exists.
    let raw_len = raw.len() as u64;
    force_append_payload(&ctx, &op_id, move |payload| {
        payload.append_state = ops::AppendState::Unknown;
        payload.attempt_uidnext = Some(0);
        payload.attempt_uidvalidity = Some(1);
        payload.attempt_size = Some(raw_len);
        payload.attempt_at = Some(chrono::Utc::now());
        payload.appended_uid = None;
        payload.appended_uidvalidity = None;
        payload.discard_requested = true;
        payload.cleanup_done = false;
    })
    .await;

    let mut connection = connect(&account).await;
    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("verify without an upload");
    assert_eq!(outcome.done, 0);
    assert!(outcome.deferred >= 1);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    for forbidden in ["APPEND", "UID STORE", "UID EXPUNGE", "EXPUNGE"] {
        assert!(
            !commands.iter().any(|command| {
                let command = command.trim().to_ascii_uppercase();
                command == forbidden || command.starts_with(&format!("{forbidden} "))
            }),
            "an external same-header copy must never be deleted: {commands:?}"
        );
    }
    assert_eq!(mock.state.message_count("Drafts"), 1);
    let payload = append_state(&ctx, &draft_id).await;
    assert!(payload.candidate_only);
    assert!(payload.discard_requested);
    assert!(!payload.cleanup_done);
}

/// Discarding a draft after a same-header candidate has been synced must not
/// turn that candidate into a deletable UID.
#[tokio::test]
async fn discard_after_syncing_a_same_header_candidate_never_deletes() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Candidate discard".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let staged = tokio::fs::read(service_draft_path(&ctx, &draft_id).await)
        .await
        .expect("staged draft");
    mock.state.add_message("Drafts", &staged);

    let mut connection = connect(&account).await;
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    assert_eq!(mock.state.message_count("Drafts"), 1);

    service.compose_discard(&draft_id).await.expect("discard");
    let before = mock.state.commands().len();
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("run cleanup ops");
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    for forbidden in ["UID STORE", "UID EXPUNGE", "EXPUNGE"] {
        assert!(
            !commands.iter().any(|command| {
                let command = command.trim().to_ascii_uppercase();
                command == forbidden || command.starts_with(&format!("{forbidden} "))
            }),
            "discard must not delete a same-header candidate: {commands:?}"
        );
    }
    assert_eq!(mock.state.message_count("Drafts"), 1);
}

/// Sending a draft after a same-header candidate was synced must not queue a
/// header-derived delete either.
#[tokio::test]
async fn send_after_syncing_a_same_header_candidate_never_deletes() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS", "MOVE"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Drafts", &["\\Drafts"]),
            ("Sent", &["\\Sent"]),
        ],
    )
    .await;
    let smtp = MockSmtp::start().await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        Some(server(mock.addr.port())),
        Some(server(smtp.addr.port())),
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Candidate send".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let staged = tokio::fs::read(service_draft_path(&ctx, &draft_id).await)
        .await
        .expect("staged draft");
    mock.state.add_message("Drafts", &staged);

    let mut connection = connect(&account).await;
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");

    service.compose_send(&draft).await.expect("send");
    let before = mock.state.commands().len();
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("run cleanup ops");
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    for forbidden in ["UID STORE", "UID EXPUNGE", "EXPUNGE"] {
        assert!(
            !commands.iter().any(|command| {
                let command = command.trim().to_ascii_uppercase();
                command == forbidden || command.starts_with(&format!("{forbidden} "))
            }),
            "send must not delete a same-header candidate: {commands:?}"
        );
    }
    assert_eq!(mock.state.message_count("Drafts"), 1);
    assert_eq!(smtp.messages.lock().expect("smtp").len(), 1);
}

/// A tagged `NO`/`BAD` from `UID SEARCH` is not an empty result. Recovery must
/// stay `Unknown` and must not continue with the APPEND.
#[tokio::test]
async fn unconfirmed_search_never_concludes_the_old_copy_is_gone() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Search refusal".into();
    draft.html = "<p>v1</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first append");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");

    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    draft.subject = "Search refusal v2".into();
    draft.html = "<p>v2</p>".into();
    service.compose_save(&draft).await.expect("second save");
    let (op_id, _) = append_op(&ctx, &draft_id).await;
    force_append_payload(&ctx, &op_id, |payload| {
        payload.append_state = ops::AppendState::Preparing;
        payload.removal_state = ops::DraftRemovalState::PendingRemove;
        payload.appended_uid = None;
        payload.appended_uidvalidity = None;
    })
    .await;

    for status in ["NO", "BAD"] {
        mock.state.set_fail_search(Some(status));
        make_due(&ctx, ops::KIND_APPEND_DRAFT).await;
        let before = mock.state.commands().len();
        let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
            .await
            .expect("verify with a refused SEARCH");
        assert_eq!(outcome.done, 0, "a refused SEARCH is not a removal");
        assert!(outcome.deferred >= 1);
        let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
        for forbidden in ["APPEND", "UID STORE", "UID EXPUNGE", "EXPUNGE"] {
            assert!(
                !commands.iter().any(|command| {
                    let command = command.trim().to_ascii_uppercase();
                    command == forbidden || command.starts_with(&format!("{forbidden} "))
                }),
                "{status} must not authorize deletion or a second upload: {commands:?}"
            );
        }
        assert_eq!(mock.state.message_count("Drafts"), 1);
        let payload = append_state(&ctx, &draft_id).await;
        assert_eq!(
            payload.removal_state,
            ops::DraftRemovalState::Unknown,
            "the refused check leaves the removal unknown"
        );
    }
}

/// A draft stuck in `Unknown` is visible in the list, and "keep local draft"
/// stops the retries without deleting anything.
#[tokio::test]
async fn unconfirmed_draft_is_visible_and_can_be_kept_local() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    mock.state.set_append_drop_completion(true);
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Keep me local".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("lost completion");
    // The dropped completion closed the previous session.
    let mut connection = connect(&account).await;
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    assert_eq!(
        append_state(&ctx, &draft_id).await.append_state,
        ops::AppendState::Unknown
    );

    let query = MessageListQuery {
        scope: MailScope::folder(drafts_folder.clone()),
        filter: MailFilter::All,
        conversation: false,
        cursor: None,
        limit: 50,
    };
    let page = service.message_list(&query).await.expect("drafts list");
    let item = page
        .items
        .iter()
        .find(|item| item.is_draft)
        .expect("draft projection");
    assert_eq!(
        item.draft_sync_state.as_deref(),
        Some("pendingConfirmation"),
        "an unconfirmed draft is visible to the user"
    );

    let projection_id = nuncio_mail::util::draft_message_id(&draft_id);
    service
        .compose_keep_local_draft(&projection_id)
        .await
        .expect("keep local");

    let (active_appends, delete_ops, suppressed) = {
        let draft_id = draft_id.clone();
        ctx.db
            .read(move |db| {
                let mut active = 0;
                let mut deletes = 0;
                for op in repo::pending_ops::list(db, Some("acc"))? {
                    if op.state != nuncio_core::models::PendingOpState::Pending
                        && op.state != nuncio_core::models::PendingOpState::InProgress
                    {
                        continue;
                    }
                    if op.kind == ops::KIND_APPEND_DRAFT {
                        active += 1;
                    }
                    if op.kind == ops::KIND_DELETE_REMOTE_DRAFT {
                        deletes += 1;
                    }
                }
                let draft = repo::drafts::get_required(db, &draft_id)?;
                Ok((active, deletes, draft.sync_suppressed))
            })
            .await
            .expect("read state")
    };
    assert_eq!(active_appends, 0, "keep-local cancels the retry");
    assert_eq!(delete_ops, 0, "keep-local never queues a remote delete");
    assert!(suppressed, "the choice is durable");
    assert_eq!(mock.state.message_count("Drafts"), 1);

    let page = service.message_list(&query).await.expect("drafts list");
    let item = page
        .items
        .iter()
        .find(|item| item.is_draft)
        .expect("draft projection");
    assert_eq!(item.draft_sync_state.as_deref(), Some("localOnly"));

    // Later autosaves stay local: no new APPEND is queued.
    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    draft.subject = "Kept local edit".into();
    service.compose_save(&draft).await.expect("save locally");
    let queued = ctx
        .db
        .read(move |db| {
            let mut queued = 0;
            for op in repo::pending_ops::list(db, Some("acc"))? {
                if op.kind == ops::KIND_APPEND_DRAFT
                    && matches!(
                        op.state,
                        nuncio_core::models::PendingOpState::Pending
                            | nuncio_core::models::PendingOpState::InProgress
                    )
                {
                    queued += 1;
                }
            }
            Ok(queued)
        })
        .await
        .expect("count appends");
    assert_eq!(queued, 0, "a kept-local draft is not uploaded again");
}

/// Staged RFC822 path of a draft, used to model another client's copy.
async fn service_draft_path(ctx: &CoreContext, draft_id: &str) -> String {
    let dir = ctx.paths.draft_staging_dir(draft_id);
    dir.join("draft.eml").to_string_lossy().to_string()
}

/// After a candidate-only APPEND settles, binding the same-header copy with a
/// Drafts sync must not make a later autosave delete it: without an APPENDUID
/// the UID is still only a candidate.
#[tokio::test]
async fn autosave_after_candidate_binding_never_deletes_the_candidate() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    // The server accepts the APPEND but sends no APPENDUID.
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Candidate copy".into();
    draft.html = "<p>first</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("append without a receipt");
    let payload = append_state(&ctx, &draft_id).await;
    assert!(payload.candidate_only, "no APPENDUID means candidate only");
    assert_eq!(payload.append_state, ops::AppendState::Acked);

    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    assert_eq!(
        stored_draft(&ctx, &draft_id).await.uid,
        Some(1),
        "the sync binds the candidate copy by header"
    );

    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    draft.subject = "Candidate copy, edited".into();
    draft.html = "<p>second</p>".into();
    service.compose_save(&draft).await.expect("save again");

    let before = mock.state.commands().len();
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("second save");
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    for forbidden in ["APPEND", "UID STORE", "UID EXPUNGE", "EXPUNGE"] {
        assert!(
            !commands.iter().any(|command| {
                let command = command.trim().to_ascii_uppercase();
                command == forbidden || command.starts_with(&format!("{forbidden} "))
            }),
            "a bound candidate must never be deleted or re-uploaded: {commands:?}"
        );
    }
    assert_eq!(mock.state.message_count("Drafts"), 1);
    assert_eq!(
        draft_identity(&ctx, &draft_id).await.attribution,
        nuncio_core::models::RemoteDraftAttribution::Candidate
    );
    let deletes = delete_remote_draft_ops(&ctx).await;
    assert_eq!(deletes, 0, "no delete may be queued for a candidate");

    // SHOULD FIX: the draft keeps showing "pending confirmation" even after
    // the candidate-only APPEND settled as done.
    let page = service
        .message_list(&MessageListQuery {
            scope: MailScope::folder(drafts_folder.clone()),
            filter: MailFilter::All,
            conversation: false,
            cursor: None,
            limit: 50,
        })
        .await
        .expect("drafts list");
    let item = page
        .items
        .iter()
        .find(|item| item.is_draft)
        .expect("draft projection");
    assert_eq!(
        item.draft_sync_state.as_deref(),
        Some("pendingConfirmation"),
        "a candidate-only draft still needs confirmation"
    );
}

/// SMTP post-send processing must consume the cleanup decision stored with the
/// outbox item. A projection id alone is not evidence that the server copy may
/// be deleted.
#[tokio::test]
async fn smtp_post_send_uses_the_persisted_cleanup_decision() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS", "MOVE"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Drafts", &["\\Drafts"]),
            ("Sent", &["\\Sent"]),
        ],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let smtp = MockSmtp::start().await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        Some(server(mock.addr.port())),
        Some(server(smtp.addr.port())),
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Queued with projection".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("append draft");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    assert!(stored_draft(&ctx, &draft_id).await.uid.is_some());

    // Model a queued message that still has its draft row and projection id.
    let raw = b"From: test@example.com\r\nTo: bob@example.com\r\nSubject: queued\r\n\r\nbody\r\n"
        .to_vec();
    let path = ctx
        .paths
        .staging_dir()
        .join(format!("outbox-{}.eml", nuncio_core::models::new_id()));
    ctx.paths.ensure_parent(&path).expect("outbox path");
    tokio::fs::write(&path, &raw).await.expect("stage outbox");
    let mut item = nuncio_core::models::OutboxItem::new(
        "acc",
        Some(draft_id.clone()),
        vec![Address::new("bob@example.com")],
        "Queued with projection",
        path.to_string_lossy().to_string(),
    );
    item.draft_cleanup = nuncio_core::models::OutboxDraftCleanup::Unconfirmed;
    let item = ctx
        .db
        .write(move |db| repo::outbox::enqueue(db, &item))
        .await
        .expect("queue outbox item");

    nuncio_mail::smtp::outbox::process_one(&ctx, &account, &item.id)
        .await
        .expect("send queued item");
    assert_eq!(smtp.messages.lock().expect("smtp").len(), 1);
    assert_eq!(
        delete_remote_draft_ops(&ctx).await,
        0,
        "post-send must not derive a delete from the projection id"
    );
    assert_eq!(
        mock.state.message_count("Drafts"),
        1,
        "the unconfirmed remote draft is kept"
    );
    assert!(
        stored_draft(&ctx, &draft_id).await.uid.is_some(),
        "the addressable projection is left for the persistent cleanup path"
    );
}

/// Retrying an offline send must not add a second, projection-derived delete.
#[tokio::test]
async fn offline_send_retry_does_not_bypass_the_cleanup_decision() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS", "MOVE"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Drafts", &["\\Drafts"]),
            ("Sent", &["\\Sent"]),
        ],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let smtp = MockSmtp::start().await;
    // A bound-then-dropped listener yields a port whose connection is refused.
    let dead = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind dead port");
    let dead_port = dead.local_addr().expect("dead addr").port();
    drop(dead);
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let offline_account = account(
        "acc",
        "test@example.com",
        Some(server(mock.addr.port())),
        Some(server(dead_port)),
        None,
    );
    insert_account(&ctx, &offline_account).await;
    sync_once(&ctx, &offline_account).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Offline send".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let mut connection = connect(&offline_account).await;
    ops::executor::run_due(&ctx, &mut connection, &offline_account)
        .await
        .expect("append draft");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &offline_account, &drafts_folder)
        .await
        .expect("drafts sync");

    // Queue while SMTP is unreachable. The cleanup decision is persisted with
    // the outbox row; the draft row is gone by the time it is retried.
    service
        .compose_send(&draft)
        .await
        .expect("queue offline send");
    let item = ctx
        .db
        .read(move |db| repo::outbox::list(db, Some("acc")))
        .await
        .expect("outbox list")
        .into_iter()
        .find(|item| item.draft_id.as_deref() == Some(draft_id.as_str()))
        .expect("queued item");
    assert_eq!(
        item.draft_cleanup,
        nuncio_core::models::OutboxDraftCleanup::Queued,
        "the cleanup decision is durable"
    );
    let deletes_before = delete_remote_draft_ops(&ctx).await;
    assert_eq!(deletes_before, 1, "the confirmed copy is queued once");

    let live = account(
        "acc",
        "test@example.com",
        Some(server(mock.addr.port())),
        Some(server(smtp.addr.port())),
        None,
    );
    nuncio_mail::smtp::outbox::process_one(&ctx, &live, &item.id)
        .await
        .expect("retry send");
    assert_eq!(smtp.messages.lock().expect("smtp").len(), 1);
    assert_eq!(
        delete_remote_draft_ops(&ctx).await,
        deletes_before,
        "the retry consumes the decision instead of queueing another delete"
    );
}

/// Cancelling an Unknown APPEND for "keep local" must preserve the ownership
/// limit: a later cleanup must not treat the candidate as an explicit binding.
#[tokio::test]
async fn keep_local_of_an_unknown_append_preserves_candidate_attribution() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    mock.state.set_append_drop_completion(true);
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Unknown then local".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("lost completion");
    let mut connection = connect(&account).await;
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    assert_eq!(
        append_state(&ctx, &draft_id).await.append_state,
        ops::AppendState::Unknown
    );

    let projection_id = nuncio_mail::util::draft_message_id(&draft_id);
    service
        .compose_keep_local_draft(&projection_id)
        .await
        .expect("keep local");

    let identity = draft_identity(&ctx, &draft_id).await;
    assert_eq!(
        identity.attribution,
        nuncio_core::models::RemoteDraftAttribution::Candidate,
        "cancelling Unknown keeps the attribution limit"
    );
    let draft_row = ctx
        .db
        .read({
            let draft_id = draft_id.clone();
            move |db| repo::drafts::get_required(db, &draft_id)
        })
        .await
        .expect("draft row");
    assert!(draft_row.sync_suppressed);
    let payload = append_state(&ctx, &draft_id).await;
    assert!(payload.candidate_only);

    // Discarding later must not delete the candidate.
    service.compose_discard(&draft_id).await.expect("discard");
    let before = mock.state.commands().len();
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("cleanup ops");
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    for forbidden in ["UID STORE", "UID EXPUNGE", "EXPUNGE"] {
        assert!(
            !commands.iter().any(|command| {
                let command = command.trim().to_ascii_uppercase();
                command == forbidden || command.starts_with(&format!("{forbidden} "))
            }),
            "a candidate must stay undeletable after keep-local: {commands:?}"
        );
    }
    assert_eq!(delete_remote_draft_ops(&ctx).await, 0);
    assert_eq!(mock.state.message_count("Drafts"), 1);
}

/// A discard of a never-uploaded draft must reclaim its UID-less projection
/// instead of leaving an unopenable ghost row in Drafts.
#[tokio::test]
async fn discard_reclaims_an_unaddressable_draft_projection() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    let (ctx, _account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Never uploaded".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    assert!(stored_draft(&ctx, &draft_id).await.uid.is_none());

    service.compose_discard(&draft_id).await.expect("discard");
    assert_eq!(delete_remote_draft_ops(&ctx).await, 0);
    assert!(
        summaries(&ctx, &drafts_folder).await.is_empty(),
        "a projection without a UID cannot be opened and must not linger"
    );
    assert_eq!(mock.state.message_count("Drafts"), 0);
    assert!(ctx
        .db
        .read(move |db| repo::drafts::get(db, &draft_id))
        .await
        .expect("draft lookup")
        .is_none());
}

/// "Keep local draft" must fail when the local draft row is already gone
/// instead of reporting a successful zero-row update.
#[tokio::test]
async fn keep_local_fails_when_the_draft_row_is_gone() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Discarded first".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("append without a receipt");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");

    service.compose_discard(&draft_id).await.expect("discard");
    let projection_id = nuncio_mail::util::draft_message_id(&draft_id);
    let error = service
        .compose_keep_local_draft(&projection_id)
        .await
        .expect_err("the local draft no longer exists");
    assert!(error.is_not_found(), "expected not found, got {error:?}");

    // The remote candidate stays visible as an ordinary server draft, but the
    // stale "keep local" affordance is not offered.
    let page = service
        .message_list(&MessageListQuery {
            scope: MailScope::folder(drafts_folder),
            filter: MailFilter::All,
            conversation: false,
            cursor: None,
            limit: 50,
        })
        .await
        .expect("drafts list");
    assert!(page
        .items
        .iter()
        .all(|item| item.draft_sync_state.is_none()));
}

/// A "keep local" that commits after the save reserved the APPEND but before
/// the executor claims the removal phase must stop the replacement: the claim
/// transaction sees the cancelled row/suppression and no STORE/EXPUNGE/APPEND
/// may follow.
#[tokio::test]
async fn keep_local_before_the_removal_claim_prevents_the_replacement() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Version one".into();
    draft.html = "<p>first</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first append");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    assert_eq!(mock.state.message_count("Drafts"), 1);

    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    draft.subject = "Version two".into();
    draft.html = "<p>second</p>".into();
    service.compose_save(&draft).await.expect("second save");
    // Model a worker that already reserved the send right but has not reached
    // its removal claim yet.
    let (op_id, _) = append_op(&ctx, &draft_id).await;
    force_append_payload(&ctx, &op_id, |payload| {
        payload.append_state = ops::AppendState::Preparing;
        payload.removal_state = ops::DraftRemovalState::NotStarted;
    })
    .await;

    let projection_id = nuncio_mail::util::draft_message_id(&draft_id);
    service
        .compose_keep_local_draft(&projection_id)
        .await
        .expect("keep local");

    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("run cancelled replacement");
    assert_eq!(outcome.done, 0);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    for forbidden in ["APPEND", "UID STORE", "UID EXPUNGE", "EXPUNGE"] {
        assert!(
            !commands.iter().any(|command| {
                let command = command.trim().to_ascii_uppercase();
                command == forbidden || command.starts_with(&format!("{forbidden} "))
            }),
            "a keep-local must stop both the expunge and the upload: {commands:?}"
        );
    }
    assert_eq!(mock.state.message_count("Drafts"), 1);
    assert!(
        String::from_utf8_lossy(&mock.state.message_bodies("Drafts")[0]).contains("Version one")
    );
    assert!(
        ctx.db
            .read({
                let draft_id = draft_id.clone();
                move |db| repo::drafts::get_required(db, &draft_id)
            })
            .await
            .expect("draft row")
            .sync_suppressed
    );
}

/// A "keep local" that commits after the executor claimed the destructive
/// phase but while the EXPUNGE is on the wire cannot undo that command. The
/// removal outcome must still be recorded and no upload may follow.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keep_local_during_the_removal_claim_records_the_removal() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Claimed removal".into();
    draft.html = "<p>first</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first append");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");

    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    draft.subject = "Claimed removal, edited".into();
    draft.html = "<p>second</p>".into();
    service.compose_save(&draft).await.expect("second save");

    // Block the mock while it handles the claimed EXPUNGE, so the test can
    // commit "keep local" after the claim and before the tagged completion.
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let started_tx = std::sync::Mutex::new(started_tx);
    let release_rx = std::sync::Mutex::new(release_rx);
    mock.state.set_on_expunge(move || {
        if let Ok(tx) = started_tx.lock() {
            let _ = tx.send(());
        }
        if let Ok(rx) = release_rx.lock() {
            let _ = rx.recv_timeout(Duration::from_secs(5));
        }
    });

    let appends_before = mock
        .state
        .commands()
        .iter()
        .filter(|command| command.trim().to_ascii_uppercase().starts_with("APPEND"))
        .count();
    let worker_ctx = ctx.clone();
    let worker_account = account.clone();
    let worker = tokio::spawn(async move {
        let mut connection = connect(&worker_account).await;
        ops::executor::run_due(&worker_ctx, &mut connection, &worker_account).await
    });
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the expunge reached the server");

    let projection_id = nuncio_mail::util::draft_message_id(&draft_id);
    service
        .compose_keep_local_draft(&projection_id)
        .await
        .expect("keep local after the claim");
    let _ = release_tx.send(());
    let outcome = worker.await.expect("worker task").expect("run due");
    assert_eq!(outcome.done, 0, "the cancelled row must not settle as done");

    let payload = append_state(&ctx, &draft_id).await;
    assert_eq!(
        payload.removal_state,
        ops::DraftRemovalState::Removed,
        "the claimed removal is recorded even though the row was cancelled"
    );
    assert_eq!(
        payload.append_state,
        ops::AppendState::Preparing,
        "the cancellation prevents the upload phase from being claimed"
    );
    assert_eq!(
        mock.state.message_count("Drafts"),
        0,
        "an already-claimed expunge cannot be undone"
    );
    let appends_after = mock
        .state
        .commands()
        .iter()
        .filter(|command| command.trim().to_ascii_uppercase().starts_with("APPEND"))
        .count();
    assert_eq!(
        appends_after, appends_before,
        "keep-local after the claim must still stop the upload"
    );
    let identity = draft_identity(&ctx, &draft_id).await;
    assert_eq!(
        identity.attribution,
        nuncio_core::models::RemoteDraftAttribution::Candidate,
        "a claimed-but-unconfirmed removal must not keep a deletable identity"
    );
}

/// A "keep local" that commits while the APPEND completion is outstanding must
/// be treated as "possibly sent": the receipt is recorded, the pinned identity
/// becomes confirmed, and nothing is deleted.
#[tokio::test]
async fn keep_local_during_the_append_completion_records_the_receipt() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let gate = set_append_gate(&mock, 0);
    let (ctx, account) = drafts_account(&mock).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "In flight keep local".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let worker_ctx = ctx.clone();
    let worker_account = account.clone();
    let worker = tokio::spawn(async move {
        let mut connection = connect(&worker_account).await;
        ops::executor::run_due(&worker_ctx, &mut connection, &worker_account).await
    });
    wait_for_append_command(&mock).await;
    assert_eq!(
        append_state(&ctx, &draft_id).await.append_state,
        ops::AppendState::Unknown,
        "the upload was claimed and is on the wire"
    );

    let projection_id = nuncio_mail::util::draft_message_id(&draft_id);
    service
        .compose_keep_local_draft(&projection_id)
        .await
        .expect("keep local");
    gate.add_permits(1);
    worker.await.expect("worker task").expect("run due");

    let payload = append_state(&ctx, &draft_id).await;
    assert_eq!(
        payload.append_state,
        ops::AppendState::Acked,
        "the receipt must survive the cancellation"
    );
    let uid = payload.appended_uid.expect("APPENDUID recorded");
    let identity = draft_identity(&ctx, &draft_id).await;
    assert_eq!(
        identity.attribution,
        nuncio_core::models::RemoteDraftAttribution::Confirmed,
        "a real receipt wins over the candidate the cancellation recorded"
    );
    assert_eq!(identity.uid, Some(uid));
    assert_eq!(
        mock.state.message_count("Drafts"),
        1,
        "keep-local never deletes the remote copy"
    );
    assert_eq!(delete_remote_draft_ops(&ctx).await, 0);
    assert!(
        ctx.db
            .read({
                let draft_id = draft_id.clone();
                move |db| repo::drafts::get_required(db, &draft_id)
            })
            .await
            .expect("draft row")
            .sync_suppressed
    );
}

/// A stale autosave that read the draft before the executor confirmed a new UID
/// must not write its old `Explicit` snapshot back over the confirmation. The
/// next save must build its replacement from the confirmed UID instead of
/// stalling on the removed external row.
#[tokio::test]
async fn stale_autosave_keeps_the_confirmed_replacement_identity() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    // An external draft (no nuncio header) is opened explicitly.
    mock.state.add_message(
        "Drafts",
        &message_raw(
            "test@example.com",
            "bob@example.com",
            "External draft",
            "external body",
            "external@example.com",
        ),
    );
    let mut connection = connect(&account).await;
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    let external_id = {
        let drafts_folder = drafts_folder.clone();
        ctx.db
            .read(move |db| {
                let query = MessageListQuery::new(MailScope::folder(drafts_folder))
                    .with_conversation(false)
                    .with_limit(50);
                Ok(repo::messages::list(db, &query)?
                    .items
                    .into_iter()
                    .find(|summary| summary.subject == "External draft")
                    .map(|summary| summary.id))
            })
            .await
            .expect("drafts list")
            .expect("external draft")
    };
    let mut opened = service
        .compose_open_draft(&external_id)
        .await
        .expect("open external draft");
    assert_eq!(
        draft_identity_of(&opened).attribution,
        nuncio_core::models::RemoteDraftAttribution::Explicit
    );
    opened.subject = "External draft edited".into();
    let saved = service
        .compose_save(&opened)
        .await
        .expect("save external draft");
    let draft_id = saved.id.clone().expect("draft id");

    // Snapshot exactly what an autosave in flight would hold: the external
    // explicit identity, before the executor confirms the replacement.
    let mut stale = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    assert_eq!(
        draft_identity_of(&stale).attribution,
        nuncio_core::models::RemoteDraftAttribution::Explicit
    );

    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("replace the external draft");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    let confirmed = draft_identity(&ctx, &draft_id).await;
    assert_eq!(
        confirmed.attribution,
        nuncio_core::models::RemoteDraftAttribution::Confirmed
    );
    let confirmed_uid = confirmed.uid.expect("confirmed uid");
    let old_uid = confirmed_uid;

    // The stale autosave commits after the confirmation.
    stale.subject = "Stale autosave".into();
    let written = service.compose_save(&stale).await.expect("stale save");
    assert_eq!(
        written
            .remote_draft
            .as_ref()
            .map(|identity| identity.attribution),
        Some(nuncio_core::models::RemoteDraftAttribution::Confirmed),
        "the stale snapshot must not overwrite the newer confirmation"
    );
    let stored = draft_identity(&ctx, &draft_id).await;
    assert_eq!(
        stored.attribution,
        nuncio_core::models::RemoteDraftAttribution::Confirmed
    );
    assert_eq!(stored.uid, Some(confirmed_uid));

    // The replacement carried by the stale save names the confirmed copy, not
    // the external UID that was already removed.
    let (_, payload) = append_op(&ctx, &draft_id).await;
    assert_eq!(
        payload.replaces.as_ref().map(|replaces| replaces.uid),
        Some(old_uid),
        "the queued replacement must target the confirmed copy"
    );

    let mut connection = connect(&account).await;
    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("replace the confirmed copy");
    assert_eq!(outcome.done, 1);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        commands.iter().any(|command| {
            let command = command.trim().to_ascii_uppercase();
            command.starts_with("UID STORE") || command.starts_with("UID EXPUNGE")
        }),
        "the next save must replace the confirmed copy instead of waiting forever: {commands:?}"
    );
    assert_eq!(mock.state.message_count("Drafts"), 1);
    assert!(
        String::from_utf8_lossy(&mock.state.message_bodies("Drafts")[0]).contains("Stale autosave")
    );
}

/// A suppressed (kept-local) Graph autosave must enqueue neither its upload nor
/// the deletion of the old server draft: the two decisions are one.
#[tokio::test]
async fn suppressed_graph_autosave_enqueues_no_server_ops() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
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
    // Seed the folders over IMAP, then make the stored account a Graph one.
    sync_once(&ctx, &account).await;
    ctx.db
        .write(|db| {
            db.execute(
                "UPDATE account SET oauth_provider = 'microsoft' WHERE id = 'acc'",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("switch to Graph");
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Graph kept local".into();
    draft.html = "<p>first</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    // Model the old server copy being visible and bound to this draft.
    let staged = tokio::fs::read(service_draft_path(&ctx, &draft_id).await)
        .await
        .expect("staged draft");
    mock.state.add_message("Drafts", &staged);
    let mut connection = connect(&account).await;
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    assert!(stored_draft(&ctx, &draft_id).await.uid.is_some());
    // The first save's queued upload is irrelevant here.
    ctx.db
        .write(|db| {
            db.execute("DELETE FROM pending_op", [])?;
            Ok(())
        })
        .await
        .expect("clear queued ops");

    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    ctx.db
        .write({
            let draft_id = draft_id.clone();
            move |db| repo::drafts::set_sync_suppressed(db, &draft_id, true)
        })
        .await
        .expect("suppress draft");
    draft.subject = "Graph kept local edited".into();
    service.compose_save(&draft).await.expect("save locally");

    let (appends, deletes) = ctx
        .db
        .read(move |db| {
            let mut appends = 0;
            let mut deletes = 0;
            for op in repo::pending_ops::list(db, Some("acc"))? {
                if !matches!(
                    op.state,
                    nuncio_core::models::PendingOpState::Pending
                        | nuncio_core::models::PendingOpState::InProgress
                ) {
                    continue;
                }
                if op.kind == ops::KIND_APPEND_DRAFT {
                    appends += 1;
                }
                if op.kind == ops::KIND_DELETE_REMOTE_DRAFT {
                    deletes += 1;
                }
            }
            Ok((appends, deletes))
        })
        .await
        .expect("count server ops");
    assert_eq!(appends, 0, "a kept-local draft is not uploaded");
    assert_eq!(deletes, 0, "its remote copy must not be deleted either");
}

/// A failed outbox insert must roll back the send's draft cleanup: nothing is
/// suppressed and no remote delete is queued for a send that never happened.
#[tokio::test]
async fn outbox_failure_does_not_suppress_or_delete_the_draft() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS", "MOVE"],
        &[
            ("INBOX", &["\\Inbox"]),
            ("Drafts", &["\\Drafts"]),
            ("Sent", &["\\Sent"]),
        ],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let smtp = MockSmtp::start().await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        Some(server(mock.addr.port())),
        Some(server(smtp.addr.port())),
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Outbox failure".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("append draft");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    assert_eq!(delete_remote_draft_ops(&ctx).await, 0);

    ctx.db
        .write(|db| {
            db.execute(
                "CREATE TRIGGER fail_outbox BEFORE INSERT ON outbox
                 BEGIN SELECT RAISE(ABORT, 'outbox rejected'); END;",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("install outbox fault");

    service
        .compose_send(&draft)
        .await
        .expect_err("the outbox insert is rejected");

    assert_eq!(
        delete_remote_draft_ops(&ctx).await,
        0,
        "a send that never reached the outbox must not delete the remote draft"
    );
    assert!(mock.state.message_count("Drafts") >= 1);
    assert_eq!(smtp.messages.lock().expect("smtp").len(), 0);
    let row = ctx
        .db
        .read({
            let draft_id = draft_id.clone();
            move |db| repo::drafts::get_required(db, &draft_id)
        })
        .await
        .expect("draft row");
    assert!(
        !row.sync_suppressed,
        "the failed send must not suppress later autosaves"
    );
}

/// An accepted APPEND without an APPENDUID leaves an unattributable cleanup.
/// Once the local draft is gone there is nothing left to attribute, so the
/// operation settles as "keep the remote copy" instead of verifying forever.
#[tokio::test]
async fn acked_cleanup_without_a_receipt_settles_as_keep_remote() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    // No APPENDUID: the server accepts the literal but identifies no UID.
    let gate = set_append_gate(&mock, 0);
    let (ctx, account) = drafts_account(&mock).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Accepted without a receipt".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let worker_ctx = ctx.clone();
    let worker_account = account.clone();
    let worker = tokio::spawn(async move {
        let mut connection = connect(&worker_account).await;
        ops::executor::run_due(&worker_ctx, &mut connection, &worker_account).await
    });
    wait_for_append_command(&mock).await;
    service.compose_discard(&draft_id).await.expect("discard");
    gate.add_permits(1);
    let outcome = worker.await.expect("worker task").expect("run due");
    assert_eq!(
        outcome.done, 1,
        "the unattributable cleanup must reach a terminal state"
    );

    let (op_state, payload) = append_op(&ctx, &draft_id).await;
    assert!(
        payload.candidate_only,
        "the ownership limit is preserved for the kept copy"
    );
    assert!(
        payload.cleanup_done,
        "the cleanup is recorded as finished (keep remote)"
    );
    let state = ctx
        .db
        .read(move |db| repo::pending_ops::get_required(db, &op_state))
        .await
        .expect("op row")
        .state;
    assert_eq!(state, nuncio_core::models::PendingOpState::Done);
    assert_eq!(
        mock.state.message_count("Drafts"),
        1,
        "the unattributable copy is kept"
    );
    assert_eq!(delete_remote_draft_ops(&ctx).await, 0);

    // No periodic verification remains: a second pass does nothing.
    make_due(&ctx, ops::KIND_APPEND_DRAFT).await;
    let mut connection = connect(&account).await;
    let before = mock.state.commands().len();
    let outcome = ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("second pass");
    assert_eq!(outcome.done, 0);
    assert_eq!(mock.state.commands().len(), before);
}

fn draft_identity_of(
    draft: &nuncio_core::models::Draft,
) -> nuncio_core::models::RemoteDraftIdentity {
    draft
        .remote_draft
        .clone()
        .expect("draft carries a remote identity")
}

async fn draft_identity(
    ctx: &CoreContext,
    draft_id: &str,
) -> nuncio_core::models::RemoteDraftIdentity {
    let draft_id = draft_id.to_string();
    ctx.db
        .read(move |db| repo::drafts::get_required(db, &draft_id))
        .await
        .expect("draft row")
        .remote_draft
        .expect("pinned remote identity")
}

async fn delete_remote_draft_ops(ctx: &CoreContext) -> usize {
    ctx.db
        .read(move |db| {
            let mut count = 0;
            for op in repo::pending_ops::list(db, Some("acc"))? {
                if op.kind == ops::KIND_DELETE_REMOTE_DRAFT
                    && matches!(
                        op.state,
                        nuncio_core::models::PendingOpState::Pending
                            | nuncio_core::models::PendingOpState::InProgress
                    )
                {
                    count += 1;
                }
            }
            Ok(count)
        })
        .await
        .expect("count delete ops")
}

/* ───────────── account switch, cleanup claim and stale plans ───────────── */

struct PendingAppend {
    payload: ops::AppendDraftOp,
    state: nuncio_core::models::PendingOpState,
}

/// The newest queued/settled APPEND operation for a draft, in insertion order.
async fn append_op_for(
    ctx: &CoreContext,
    account_id: &str,
    draft_id: &str,
) -> Option<PendingAppend> {
    let account_id = account_id.to_string();
    let draft_id = draft_id.to_string();
    ctx.db
        .read(move |db| {
            let mut found = None;
            for op in repo::pending_ops::list(db, Some(&account_id))? {
                if op.kind != ops::KIND_APPEND_DRAFT {
                    continue;
                }
                let Ok(payload) = repo::pending_ops::payload::<ops::AppendDraftOp>(&op) else {
                    continue;
                };
                if payload.draft_id == draft_id {
                    found = Some(PendingAppend {
                        payload,
                        state: op.state,
                    });
                }
            }
            Ok(found)
        })
        .await
        .expect("read append ops")
}

struct PendingDelete {
    payload: ops::DeleteRemoteDraftOp,
    state: nuncio_core::models::PendingOpState,
}

async fn delete_ops_for(ctx: &CoreContext, account_id: &str, draft_id: &str) -> Vec<PendingDelete> {
    let account_id = account_id.to_string();
    let draft_id = draft_id.to_string();
    ctx.db
        .read(move |db| {
            let mut found = Vec::new();
            for op in repo::pending_ops::list(db, Some(&account_id))? {
                if op.kind != ops::KIND_DELETE_REMOTE_DRAFT {
                    continue;
                }
                let Ok(payload) = repo::pending_ops::payload::<ops::DeleteRemoteDraftOp>(&op)
                else {
                    continue;
                };
                if payload.draft_id.as_deref() == Some(draft_id.as_str()) {
                    found.push(PendingDelete {
                        payload,
                        state: op.state,
                    });
                }
            }
            Ok(found)
        })
        .await
        .expect("read delete ops")
}

/// Switching the composer's sending account must not carry the old account's
/// remote binding into the new mailbox, even when both Drafts folders use the
/// same path, UIDVALIDITY and UID. Sending and discarding the switched draft
/// must never address the new account's unrelated message.
#[tokio::test]
async fn switching_account_drops_the_old_remote_binding() {
    let mock_a = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock_a.state.set_append_uid_plus(true);
    let mock_b = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock_b.state.set_append_uid_plus(true);
    // The new account's Drafts contains an unrelated message at UID 1 in
    // UIDVALIDITY 1 -- exactly the coordinates the old binding names.
    mock_b.state.add_message(
        "Drafts",
        &message_raw(
            "bob@example.com",
            "b@example.com",
            "Unrelated draft in B",
            "do not touch B",
            "unrelated-b@example.com",
        ),
    );
    mock_b.state.set_flags("Drafts", 1, &["\\Draft"]);
    let smtp = MockSmtp::start().await;

    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account_a = account(
        "acc-a",
        "a@example.com",
        Some(server(mock_a.addr.port())),
        None,
        None,
    );
    let account_b = account(
        "acc-b",
        "b@example.com",
        Some(server(mock_b.addr.port())),
        Some(server(smtp.addr.port())),
        None,
    );
    insert_account(&ctx, &account_a).await;
    insert_account(&ctx, &account_b).await;
    sync_once(&ctx, &account_a).await;
    sync_once(&ctx, &account_b).await;
    let a_drafts = folder_id(&ctx, "acc-a", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut connection_a = connect(&account_a).await;
    let mut connection_b = connect(&account_b).await;

    // Draft one: appended to account A at UID 1 / UIDVALIDITY 1.
    let mut to_send = service.compose_create(Some("acc-a")).await.expect("draft");
    to_send.to = vec![Address::new("bob@example.com")];
    to_send.subject = "Switched send".into();
    to_send.html = "<p>send body</p>".into();
    let to_send = service.compose_save(&to_send).await.expect("save");
    let send_id = to_send.id.clone().expect("draft id");
    ops::executor::run_due(&ctx, &mut connection_a, &account_a)
        .await
        .expect("append in A");

    // Draft two: same, to exercise discard instead of send.
    let mut to_discard = service.compose_create(Some("acc-a")).await.expect("draft");
    to_discard.to = vec![Address::new("bob@example.com")];
    to_discard.subject = "Switched discard".into();
    to_discard.html = "<p>discard body</p>".into();
    let to_discard = service.compose_save(&to_discard).await.expect("save");
    let discard_id = to_discard.id.clone().expect("draft id");
    ops::executor::run_due(&ctx, &mut connection_a, &account_a)
        .await
        .expect("append in A");

    let switch = |draft: &mut nuncio_core::models::Draft, uid: i64| {
        draft.account_id = "acc-b".into();
        // A composer loaded before the switch still carries A's binding.
        draft.remote_draft = Some(nuncio_core::models::RemoteDraftIdentity::confirmed(
            &a_drafts,
            uid,
            Some(1),
            Some(nuncio_mail::util::draft_message_id(
                draft.id.as_deref().expect("draft id"),
            )),
        ));
        draft.remote_draft_message_id = Some(nuncio_mail::util::draft_message_id(
            draft.id.as_deref().expect("draft id"),
        ));
    };

    let mut switched_send = service.compose_get_draft(&send_id).await.expect("reload");
    switch(&mut switched_send, 1);
    service
        .compose_save(&switched_send)
        .await
        .expect("save after switch");
    let queued = append_op_for(&ctx, "acc-b", &send_id)
        .await
        .expect("append queued for B");
    assert!(
        queued.payload.replaces.is_none(),
        "the old account's binding must not travel with the draft: {:?}",
        queued.payload.replaces
    );
    assert_eq!(queued.state, nuncio_core::models::PendingOpState::Pending);

    let mut switched_discard = service
        .compose_get_draft(&discard_id)
        .await
        .expect("reload");
    switch(&mut switched_discard, 2);
    service
        .compose_save(&switched_discard)
        .await
        .expect("save after switch");
    let queued = append_op_for(&ctx, "acc-b", &discard_id)
        .await
        .expect("append queued for B");
    assert!(
        queued.payload.replaces.is_none(),
        "the old account's binding must not travel with the draft: {:?}",
        queued.payload.replaces
    );

    // Both switched drafts upload into B (UIDs 2 and 3, UIDVALIDITY 1).
    ops::executor::run_due(&ctx, &mut connection_b, &account_b)
        .await
        .expect("append switched drafts in B");
    assert_eq!(mock_b.state.message_count("Drafts"), 3);

    // Send one: only its own copy may be removed.
    let send_draft = service.compose_get_draft(&send_id).await.expect("draft");
    service.compose_send(&send_draft).await.expect("send");
    ops::executor::run_due(&ctx, &mut connection_b, &account_b)
        .await
        .expect("send cleanup");
    assert_eq!(
        mock_b.state.message_count("Drafts"),
        2,
        "exactly the switched draft's copy is removed"
    );

    // Discard the other: the unrelated B draft must survive both operations.
    service.compose_discard(&discard_id).await.expect("discard");
    ops::executor::run_due(&ctx, &mut connection_b, &account_b)
        .await
        .expect("discard cleanup");
    assert_eq!(
        mock_b.state.message_count("Drafts"),
        1,
        "the switched draft's copy is removed, the unrelated mail is not"
    );
    let bodies = mock_b.state.message_bodies("Drafts");
    assert!(
        String::from_utf8_lossy(&bodies[0]).contains("do not touch B"),
        "B's unrelated draft must be intact"
    );
    assert!(
        mock_b
            .state
            .message_flags("Drafts", 1)
            .iter()
            .any(|flag| flag.eq_ignore_ascii_case("\\Draft")),
        "the unrelated draft keeps its flags"
    );
}

/// A keep-local that wins before the cleanup claim must stop the expunge and
/// leave the local projection alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keep_local_before_the_cleanup_claim_prevents_the_expunge() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "First".into();
    draft.html = "<p>first</p>".into();
    let draft = service.compose_save(&draft).await.expect("save");
    let draft_id = draft.id.clone().expect("draft id");
    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first append");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");

    let mut draft = service.compose_get_draft(&draft_id).await.expect("reload");
    draft.subject = "Second".into();
    draft.html = "<p>second</p>".into();
    service.compose_save(&draft).await.expect("second save");
    // Gate only the upload under test, after the first append is settled.
    let gate = set_append_gate(&mock, 0);

    // The first (settled) append already logged an APPEND, so wait for the
    // next one: the upload under test must be the one in flight.
    let appends_before = append_command_count(&mock);
    let worker_ctx = ctx.clone();
    let worker_account = account.clone();
    let worker = tokio::spawn(async move {
        let mut connection = connect(&worker_account).await;
        ops::executor::run_due(&worker_ctx, &mut connection, &worker_account).await
    });
    wait_for_new_append(&mock, appends_before).await;
    // The replacement has removed the superseded copy; only the upload is
    // outstanding. Everything after this point must not delete anything.
    let before = mock.state.commands().len();

    service.compose_discard(&draft_id).await.expect("discard");
    // Model a keep-local that commits in the cleanup claim's window: the row
    // becomes cancelled and the claim's payload write is skipped.
    ctx.db
        .write(|db| {
            db.execute(
                "CREATE TRIGGER cleanup_claim_loses
                   BEFORE UPDATE OF payload_json ON pending_op
                   WHEN NEW.payload_json LIKE '%\"cleanupClaimed\":true%'
                 BEGIN
                   UPDATE pending_op SET state = 'cancelled' WHERE id = NEW.id;
                   SELECT RAISE(IGNORE);
                 END",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("install cleanup gate");

    gate.add_permits(1);
    let outcome = tokio::time::timeout(Duration::from_secs(15), worker)
        .await
        .expect("worker must finish after the upload completes")
        .expect("worker task")
        .expect("run due");
    assert_eq!(outcome.done, 0);
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    for forbidden in ["UID STORE", "UID EXPUNGE", "EXPUNGE"] {
        assert!(
            !commands.iter().any(|command| {
                let command = command.trim().to_ascii_uppercase();
                command == forbidden || command.starts_with(&format!("{forbidden} "))
            }),
            "a lost cleanup claim must not delete anything: {commands:?}"
        );
    }
    let payload = append_state(&ctx, &draft_id).await;
    assert!(!payload.cleanup_claimed);
    assert!(!payload.cleanup_done);
    // The old projection row still exists and still names the copy it can
    // address; the skipped cleanup must not erase it.
    let projection = nuncio_mail::util::draft_message_id(&draft_id);
    assert!(
        ctx.db
            .read({
                let projection = projection.clone();
                move |db| repo::messages::get(db, &projection)
            })
            .await
            .expect("read projection")
            .is_some(),
        "the still-addressable projection must be preserved"
    );
}

/// Once the cleanup claim wins, a later keep-local cannot stop the expunge; its
/// outcome is recorded and a projection that no longer matches the deleted UID
/// is preserved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cleanup_claim_winning_records_the_result() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account) = drafts_account(&mock).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "First".into();
    draft.html = "<p>first</p>".into();
    let draft = service.compose_save(&draft).await.expect("save");
    let draft_id = draft.id.clone().expect("draft id");
    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first append");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");

    let mut draft = service.compose_get_draft(&draft_id).await.expect("reload");
    draft.subject = "Second".into();
    draft.html = "<p>second</p>".into();
    service.compose_save(&draft).await.expect("second save");
    // Gate only the upload under test, after the first append is settled.
    let gate = set_append_gate(&mock, 0);

    let appends_before = append_command_count(&mock);
    let worker_ctx = ctx.clone();
    let worker_account = account.clone();
    let worker = tokio::spawn(async move {
        let mut connection = connect(&worker_account).await;
        ops::executor::run_due(&worker_ctx, &mut connection, &worker_account).await
    });
    wait_for_new_append(&mock, appends_before).await;
    let before = mock.state.commands().len();

    service.compose_discard(&draft_id).await.expect("discard");
    // The claim commits first; the row is cancelled only afterwards.
    ctx.db
        .write(|db| {
            db.execute(
                "CREATE TRIGGER cleanup_claim_wins
                   AFTER UPDATE OF payload_json ON pending_op
                   WHEN NEW.payload_json LIKE '%\"cleanupClaimed\":true%'
                 BEGIN
                   UPDATE pending_op SET state = 'cancelled' WHERE id = NEW.id;
                 END",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("install cleanup gate");

    gate.add_permits(1);
    let outcome = tokio::time::timeout(Duration::from_secs(15), worker)
        .await
        .expect("worker must finish after the upload completes")
        .expect("worker task")
        .expect("run due");
    assert_eq!(outcome.done, 0, "the cancelled row must not settle as done");

    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        commands.iter().any(|command| {
            command
                .trim()
                .to_ascii_uppercase()
                .starts_with("UID EXPUNGE")
        }),
        "a claim that committed first must finish its expunge: {commands:?}"
    );
    let payload = append_state(&ctx, &draft_id).await;
    assert!(payload.cleanup_claimed, "the claim is persisted");
    assert!(
        payload.cleanup_done,
        "the result is recorded even though the row was cancelled"
    );
    assert_eq!(
        mock.state.message_count("Drafts"),
        0,
        "both the superseded and the accepted copies are gone"
    );
    // The projection pointed at the superseded UID, which the replacement
    // deleted. It does not name the copy this cleanup removed, so it is kept
    // for the sync to rebind instead of being deleted by projection id.
    let projection = nuncio_mail::util::draft_message_id(&draft_id);
    let stored = ctx
        .db
        .read({
            let projection = projection.clone();
            move |db| repo::messages::get(db, &projection)
        })
        .await
        .expect("read projection");
    assert!(
        stored.as_ref().is_some_and(|row| row.uid == Some(1)),
        "a rebound/mismatched projection must not be removed by the cleanup"
    );
}

/// A Graph replacement is one unit: with an active APPEND nothing is queued,
/// and when the pair is queued, keep-local cancels both halves.
#[tokio::test]
async fn graph_replacement_is_paired_and_keep_local_cancels_it() {
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("graph", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    let drafts = Folder::new("graph-drafts", "graph", "Drafts", FolderRole::Drafts);
    let drafts_id = drafts.id.clone();
    {
        let drafts = drafts.clone();
        ctx.db
            .write(move |db| repo::folders::insert(db, &drafts))
            .await
            .expect("insert drafts folder");
    }
    let service = MailService::new(ctx.clone());

    // g1 already has an active APPEND: its replacement cannot be queued.
    insert_stored(&ctx, "graph", "remote-old-g1", &drafts_id, true).await;
    ops::enqueue_append_draft(&ctx, "graph", "g1", &drafts_id, "/nonexistent/g1.eml")
        .await
        .expect("seed active append");
    let mut draft = service.compose_create(Some("graph")).await.expect("draft");
    draft.id = Some("g1".into());
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Graph g1".into();
    draft.html = "<p>one</p>".into();
    draft.remote_draft_message_id = Some("remote-old-g1".into());
    draft.remote_draft = Some(nuncio_core::models::RemoteDraftIdentity::explicit_graph(
        &drafts_id,
        "remote-old-g1",
        Some("remote-old-g1".into()),
    ));
    service.compose_save(&draft).await.expect("save g1");
    assert!(
        delete_ops_for(&ctx, "graph", "g1").await.is_empty(),
        "an active APPEND must block the whole replacement, including its DELETE"
    );
    let append_ops = ctx
        .db
        .read(|db| {
            Ok(repo::pending_ops::list(db, Some("graph"))?
                .into_iter()
                .filter(|op| op.kind == ops::KIND_APPEND_DRAFT)
                .count())
        })
        .await
        .expect("count appends");
    assert_eq!(append_ops, 1, "no second APPEND is queued");

    // g2 has no active APPEND: DELETE and APPEND commit together, and
    // keep-local cancels both.
    insert_stored(&ctx, "graph", "remote-old-g2", &drafts_id, true).await;
    let mut draft = service.compose_create(Some("graph")).await.expect("draft");
    draft.id = Some("g2".into());
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Graph g2".into();
    draft.html = "<p>two</p>".into();
    draft.remote_draft_message_id = Some("remote-old-g2".into());
    draft.remote_draft = Some(nuncio_core::models::RemoteDraftIdentity::explicit_graph(
        &drafts_id,
        "remote-old-g2",
        Some("remote-old-g2".into()),
    ));
    service.compose_save(&draft).await.expect("save g2");
    let deletes = delete_ops_for(&ctx, "graph", "g2").await;
    assert_eq!(deletes.len(), 1, "the old copy is queued with the APPEND");
    assert_eq!(
        deletes[0].state,
        nuncio_core::models::PendingOpState::Pending
    );
    let appends = append_op_for(&ctx, "graph", "g2")
        .await
        .expect("append queued");
    assert_eq!(appends.state, nuncio_core::models::PendingOpState::Pending);

    let projection = nuncio_mail::util::draft_message_id("g2");
    service
        .compose_keep_local_draft(&projection)
        .await
        .expect("keep local");
    assert!(
        delete_ops_for(&ctx, "graph", "g2")
            .await
            .iter()
            .all(|op| op.state == nuncio_core::models::PendingOpState::Cancelled),
        "keep-local must cancel the paired old-copy delete"
    );
    assert_eq!(
        append_op_for(&ctx, "graph", "g2")
            .await
            .expect("append")
            .state,
        nuncio_core::models::PendingOpState::Cancelled,
        "keep-local cancels the APPEND too"
    );
}

/// Switching a draft to another account must cancel the *paired* Graph DELETE
/// as well as its APPEND. Otherwise the old account's worker would delete a
/// remote draft the switch deliberately left alone.
#[tokio::test]
async fn switching_account_cancels_the_paired_graph_delete() {
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph_a = account("acc-a", "a@example.com", Some(server(9)), None, None);
    graph_a.provider = ProviderKind::Outlook;
    graph_a.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph_a).await;
    let mut graph_b = account("acc-b", "b@example.com", Some(server(9)), None, None);
    graph_b.provider = ProviderKind::Outlook;
    graph_b.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph_b).await;

    let drafts_a = Folder::new("a-drafts", "acc-a", "Drafts", FolderRole::Drafts);
    let drafts_b = Folder::new("b-drafts", "acc-b", "Drafts", FolderRole::Drafts);
    for folder in [drafts_a.clone(), drafts_b] {
        ctx.db
            .write(move |db| repo::folders::insert(db, &folder))
            .await
            .expect("insert drafts folder");
    }
    let service = MailService::new(ctx.clone());

    // The old account already has a remote draft that a replacement would
    // delete. Saving the draft once queues the DELETE + APPEND pair.
    insert_stored(&ctx, "acc-a", "remote-old-a", &drafts_a.id, true).await;
    let mut draft = service.compose_create(Some("acc-a")).await.expect("draft");
    draft.id = Some("switch-graph".into());
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Graph switch".into();
    draft.html = "<p>v1</p>".into();
    draft.remote_draft_message_id = Some("remote-old-a".into());
    draft.remote_draft = Some(nuncio_core::models::RemoteDraftIdentity::explicit_graph(
        &drafts_a.id,
        "remote-old-a",
        Some("remote-old-a".into()),
    ));
    service.compose_save(&draft).await.expect("save in A");

    let deletes = delete_ops_for(&ctx, "acc-a", "switch-graph").await;
    assert_eq!(
        deletes.len(),
        1,
        "the replacement pairs a DELETE with its APPEND"
    );
    assert_eq!(
        deletes[0].state,
        nuncio_core::models::PendingOpState::Pending
    );
    assert_eq!(
        append_op_for(&ctx, "acc-a", "switch-graph")
            .await
            .expect("append in A")
            .state,
        nuncio_core::models::PendingOpState::Pending
    );

    let mut switched = service
        .compose_get_draft("switch-graph")
        .await
        .expect("reload");
    switched.account_id = "acc-b".into();
    service
        .compose_save(&switched)
        .await
        .expect("save after switch");

    for delete in delete_ops_for(&ctx, "acc-a", "switch-graph").await {
        assert_eq!(
            delete.state,
            nuncio_core::models::PendingOpState::Cancelled,
            "the old account's paired DELETE must be cancelled with its APPEND"
        );
    }
    assert_eq!(
        append_op_for(&ctx, "acc-a", "switch-graph")
            .await
            .expect("append in A")
            .state,
        nuncio_core::models::PendingOpState::Cancelled
    );
    // A's queue therefore has nothing to run: the old remote draft is kept.
    let outcome = ops::executor::run_due_graph(&ctx, &graph_a)
        .await
        .expect("run A queue");
    assert_eq!(outcome.done, 0);
    assert_eq!(outcome.failed, 0);
    assert!(
        ctx.db
            .read({
                let id = "remote-old-a".to_string();
                move |db| repo::messages::get(db, &id)
            })
            .await
            .expect("read old draft")
            .is_some(),
        "the old remote draft projection is untouched"
    );
}

/// A Graph MIME create is a send with no cancellation: when the draft is
/// discarded while the POST is on the wire, the executor must finish the
/// create, then delete the exact copy it returned.
#[tokio::test]
async fn graph_append_in_flight_discard_deletes_the_created_copy() {
    let mock = MockGraph::start_with_create_gate(0).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    {
        let drafts = Folder::new("acc-drafts", "acc", "Drafts", FolderRole::Drafts);
        ctx.db
            .write(move |db| repo::folders::insert(db, &drafts))
            .await
            .expect("insert drafts folder");
    }
    let service = MailService::new(ctx.clone());
    seed_graph_drafts_mapping(&ctx).await;
    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Graph in-flight discard".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let worker = {
        let ctx = ctx.clone();
        let account = graph.clone();
        let client = client.clone();
        tokio::spawn(
            async move { ops::executor::run_due_graph_with(&ctx, &account, &client).await },
        )
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), mock.create_received.acquire())
        .await
        .expect("the create must reach the mock")
        .expect("permit");
    let pending = append_op_for(&ctx, "acc", &draft_id)
        .await
        .expect("append op");
    assert_eq!(
        pending.state,
        nuncio_core::models::PendingOpState::InProgress
    );
    assert_eq!(
        pending.payload.append_state,
        ops::AppendState::Unknown,
        "the phase must be persisted before the request leaves"
    );

    service.compose_discard(&draft_id).await.expect("discard");
    mock.release_create.add_permits(1);
    let outcome = worker.await.expect("worker task").expect("graph pass");
    assert_eq!(outcome.done, 1);

    let created = mock.created_ids();
    assert_eq!(created.len(), 1, "exactly one create");
    assert!(
        mock.deleted_ids().contains(&created[0]),
        "the created copy must be deleted by its exact id"
    );
    assert!(
        !mock.ids().contains(&created[0]),
        "the copy is gone from the mailbox"
    );
    assert_eq!(
        append_op_for(&ctx, "acc", &draft_id)
            .await
            .expect("settled append")
            .state,
        nuncio_core::models::PendingOpState::Done
    );
    assert!(ctx
        .db
        .read({
            let id = draft_id.clone();
            move |db| repo::drafts::get(db, &id)
        })
        .await
        .expect("read draft")
        .is_none());
    assert!(
        ctx.db
            .read({
                let id = nuncio_mail::util::draft_message_id(&draft_id);
                move |db| repo::messages::get(db, &id)
            })
            .await
            .expect("read projection")
            .is_none(),
        "the local projection must be cleaned up with the remote copy"
    );
}

/// Sending a Graph draft while its MIME create is in flight records the same
/// cleanup intent as discard; the create still completes and is then deleted.
#[tokio::test]
async fn graph_append_in_flight_send_deletes_the_created_copy() {
    let mock = MockGraph::start_with_create_gate(0).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    {
        let drafts = Folder::new("acc-drafts", "acc", "Drafts", FolderRole::Drafts);
        ctx.db
            .write(move |db| repo::folders::insert(db, &drafts))
            .await
            .expect("insert drafts folder");
    }
    let service = MailService::new(ctx.clone());
    seed_graph_drafts_mapping(&ctx).await;
    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Graph in-flight send".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let worker = {
        let ctx = ctx.clone();
        let account = graph.clone();
        let client = client.clone();
        tokio::spawn(
            async move { ops::executor::run_due_graph_with(&ctx, &account, &client).await },
        )
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), mock.create_received.acquire())
        .await
        .expect("the create must reach the mock")
        .expect("permit");

    let draft = service.compose_get_draft(&draft_id).await.expect("reload");
    service
        .compose_send_graph_with(&graph, &draft, &client)
        .await
        .expect("send through the mock");
    let pending = append_op_for(&ctx, "acc", &draft_id)
        .await
        .expect("append op");
    assert!(
        pending.payload.discard_requested,
        "the send must hand the in-flight cleanup to the executor"
    );

    mock.release_create.add_permits(1);
    let outcome = worker.await.expect("worker task").expect("graph pass");
    assert_eq!(outcome.done, 1);
    let created = mock.created_ids();
    assert_eq!(created.len(), 1);
    assert!(
        mock.deleted_ids().contains(&created[0]),
        "the uploaded draft must not survive the send"
    );
    assert!(!mock.ids().contains(&created[0]));
    assert_eq!(
        append_op_for(&ctx, "acc", &draft_id)
            .await
            .expect("settled append")
            .state,
        nuncio_core::models::PendingOpState::Done
    );
    assert!(ctx
        .db
        .read({
            let id = draft_id.clone();
            move |db| repo::drafts::get(db, &id)
        })
        .await
        .expect("read draft")
        .is_none());
}

/// A create that succeeds but whose cleanup delete fails ambiguously must keep
/// both the record and the projection. The retry verifies the exact id with a
/// targeted GET before deleting, and only then drops the projection.
#[tokio::test]
async fn graph_append_cleanup_unknown_keeps_the_projection_until_verified() {
    let mock = MockGraph::start_with_create_gate(0).await;
    mock.set_permanent_delete_failure(true);
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    {
        let drafts = Folder::new("acc-drafts", "acc", "Drafts", FolderRole::Drafts);
        ctx.db
            .write(move |db| repo::folders::insert(db, &drafts))
            .await
            .expect("insert drafts folder");
    }
    let service = MailService::new(ctx.clone());
    seed_graph_drafts_mapping(&ctx).await;
    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Graph cleanup unknown".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let projection_id = nuncio_mail::util::draft_message_id(&draft_id);
    // Make the projection addressable so the discard keeps it while the upload
    // is in flight (a projection with no UID is a local-only echo).
    {
        let id = projection_id.clone();
        ctx.db
            .write(move |db| repo::messages::set_uid(db, &id, Some(7)))
            .await
            .expect("bind projection");
    }

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let worker = {
        let ctx = ctx.clone();
        let account = graph.clone();
        let client = client.clone();
        tokio::spawn(
            async move { ops::executor::run_due_graph_with(&ctx, &account, &client).await },
        )
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), mock.create_received.acquire())
        .await
        .expect("the create must reach the mock")
        .expect("permit");
    service.compose_discard(&draft_id).await.expect("discard");
    mock.release_create.add_permits(1);
    let outcome = worker.await.expect("worker task").expect("graph pass");
    assert_eq!(outcome.retried, 1, "the ambiguous delete is retried");

    let created = mock.created_ids();
    assert_eq!(created.len(), 1);
    let failed = append_op_for(&ctx, "acc", &draft_id)
        .await
        .expect("append op");
    assert_eq!(
        failed.payload.append_state,
        ops::AppendState::Acked,
        "the create receipt survives the cleanup failure"
    );
    assert_eq!(
        failed.payload.appended_remote_id.as_deref(),
        Some(created[0].as_str())
    );
    assert!(
        failed.payload.outcome_unknown,
        "the uncertain delete outcome must be recorded"
    );
    assert!(!failed.payload.cleanup_done);
    let projection_still_there = ctx
        .db
        .read({
            let id = projection_id.clone();
            move |db| repo::messages::get(db, &id)
        })
        .await
        .expect("read projection");
    assert!(
        projection_still_there.is_some(),
        "an unconfirmed delete must not erase the projection"
    );

    // The server recovers; the retry verifies the id first and finishes.
    mock.set_permanent_delete_failure(false);
    ctx.db
        .write(|db| {
            db.execute(
                "UPDATE pending_op SET next_retry_at = NULL
                  WHERE kind = 'appendDraft'",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("make the retry due");
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("retry pass");
    assert_eq!(outcome.done, 1);
    assert!(
        mock.deleted_ids().contains(&created[0]),
        "the verified copy is deleted on the retry"
    );
    assert!(!mock.ids().contains(&created[0]));
    let settled = append_op_for(&ctx, "acc", &draft_id)
        .await
        .expect("settled append");
    assert!(settled.payload.cleanup_done);
    assert!(!settled.payload.outcome_unknown);
    assert_eq!(settled.state, nuncio_core::models::PendingOpState::Done);
    assert!(
        ctx.db
            .read({
                let id = projection_id.clone();
                move |db| repo::messages::get(db, &id)
            })
            .await
            .expect("read projection")
            .is_none(),
        "the confirmed delete drops the projection"
    );
}

/// A Graph create answered 201 without a usable id may still have created a
/// remote draft. The phase must stay `Unknown`, and neither a retry nor a
/// later autosave (even after the operation exhausted its retries) may POST a
/// second copy.
#[tokio::test]
async fn graph_create_without_an_id_is_unknown_and_never_resent() {
    let mock = MockGraph::start(0).await;
    mock.set_create_response_without_id(true);
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    {
        let drafts = Folder::new("acc-drafts", "acc", "Drafts", FolderRole::Drafts);
        ctx.db
            .write(move |db| repo::folders::insert(db, &drafts))
            .await
            .expect("insert drafts folder");
    }
    let service = MailService::new(ctx.clone());
    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Graph missing id".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("first graph pass");
    assert_eq!(outcome.retried, 1, "an ambiguous create is retried");
    assert_eq!(mock.created_ids().len(), 1);
    let failed = append_op_for(&ctx, "acc", &draft_id)
        .await
        .expect("append op");
    assert_eq!(
        failed.payload.append_state,
        ops::AppendState::Unknown,
        "a 201 without an id must never reset to Idle"
    );
    assert!(failed.payload.outcome_unknown);
    assert!(failed.payload.appended_remote_id.is_none());

    // The Unknown phase verifies instead of re-POSTing.
    ctx.db
        .write(|db| {
            db.execute("UPDATE pending_op SET next_retry_at = NULL", [])?;
            Ok(())
        })
        .await
        .expect("make retry due");
    let requests_before = mock.requests().len();
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("verification pass");
    assert_eq!(outcome.done, 0);
    assert!(outcome.deferred >= 1);
    assert_eq!(mock.created_ids().len(), 1, "the create is never repeated");

    // Model the retry budget being exhausted. The durable unknown outcome
    // must still keep a later autosave from uploading a second copy.
    ctx.db
        .write(|db| {
            db.execute("UPDATE pending_op SET state = 'failed'", [])?;
            Ok(())
        })
        .await
        .expect("mark the operation failed");
    let mut draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    draft.subject = "Edited after an unknown create".into();
    service
        .compose_save(&draft)
        .await
        .expect("save after the failed create");
    let append_ops = ctx
        .db
        .read({
            let draft_id = draft_id.clone();
            move |db| {
                Ok(repo::pending_ops::list(db, Some("acc"))?
                    .into_iter()
                    .filter(|op| op.kind == ops::KIND_APPEND_DRAFT)
                    .filter(|op| {
                        repo::pending_ops::payload::<ops::AppendDraftOp>(op)
                            .map(|payload| payload.draft_id == draft_id)
                            .unwrap_or(false)
                    })
                    .count())
            }
        })
        .await
        .expect("count append ops");
    assert_eq!(
        append_ops, 1,
        "an unresolved create must block another autosave upload"
    );
    mock.set_create_response_without_id(false);
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("post-autosave pass");
    assert_eq!(outcome.done, 0);
    assert_eq!(mock.created_ids().len(), 1, "no second create");
    let creates_after: Vec<String> = mock
        .requests()
        .into_iter()
        .skip(requests_before)
        .filter(|request| request == "/me/messages")
        .collect();
    assert!(
        creates_after.is_empty(),
        "no create request may follow the unknown outcome: {creates_after:?}"
    );
}

/// A targeted GET that answers 200 without `parentFolderId` is not deletion
/// evidence. The cleanup must stay pending and keep the local projection.
#[tokio::test]
async fn graph_cleanup_without_parent_folder_does_not_delete_locally() {
    let mock = MockGraph::start_with_create_gate(0).await;
    mock.set_permanent_delete_failure(true);
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    {
        let drafts = Folder::new("acc-drafts", "acc", "Drafts", FolderRole::Drafts);
        ctx.db
            .write(move |db| repo::folders::insert(db, &drafts))
            .await
            .expect("insert drafts folder");
    }
    let service = MailService::new(ctx.clone());
    seed_graph_drafts_mapping(&ctx).await;
    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Graph unverified cleanup".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let projection_id = nuncio_mail::util::draft_message_id(&draft_id);
    {
        let id = projection_id.clone();
        ctx.db
            .write(move |db| repo::messages::set_uid(db, &id, Some(7)))
            .await
            .expect("bind projection");
    }

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let worker = {
        let ctx = ctx.clone();
        let account = graph.clone();
        let client = client.clone();
        tokio::spawn(
            async move { ops::executor::run_due_graph_with(&ctx, &account, &client).await },
        )
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), mock.create_received.acquire())
        .await
        .expect("the create must reach the mock")
        .expect("permit");
    service.compose_discard(&draft_id).await.expect("discard");
    mock.release_create.add_permits(1);
    let outcome = worker.await.expect("worker task").expect("graph pass");
    assert_eq!(outcome.retried, 1);
    assert!(mock.deleted_ids().len() == 1, "first delete was attempted");

    // The retry cannot prove anything: answer 200 with no parentFolderId.
    mock.set_permanent_delete_failure(false);
    mock.set_parent_folder_response(None);
    let deletes_before = mock.deleted_ids().len();
    ctx.db
        .write(|db| {
            db.execute("UPDATE pending_op SET next_retry_at = NULL", [])?;
            Ok(())
        })
        .await
        .expect("make retry due");
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("verification pass");
    assert_eq!(outcome.done, 0, "an unverified cleanup must not settle");
    assert!(outcome.deferred >= 1);
    assert_eq!(
        mock.deleted_ids().len(),
        deletes_before,
        "an inconclusive 200 must not authorize another delete"
    );
    let pending = append_op_for(&ctx, "acc", &draft_id)
        .await
        .expect("append op");
    assert!(pending.payload.outcome_unknown);
    assert!(!pending.payload.cleanup_done);
    assert!(
        ctx.db
            .read({
                let id = projection_id.clone();
                move |db| repo::messages::get(db, &id)
            })
            .await
            .expect("read projection")
            .is_some(),
        "the local projection must survive an unverified cleanup"
    );

    // Even if the recorded failure marker were lost (a crash after the claim
    // but before its result was persisted), the durable claim alone must force
    // verification before any resend.
    ctx.db
        .write({
            let draft_id = draft_id.clone();
            move |db| {
                for op in repo::pending_ops::list(db, Some("acc"))? {
                    if op.kind != ops::KIND_APPEND_DRAFT {
                        continue;
                    }
                    let mut payload: ops::AppendDraftOp = repo::pending_ops::payload(&op)?;
                    if payload.draft_id != draft_id {
                        continue;
                    }
                    assert!(payload.cleanup_claimed && !payload.cleanup_acked);
                    payload.outcome_unknown = false;
                    let value = serde_json::to_value(payload)
                        .map_err(|error| nuncio_core::AppError::internal(error.to_string()))?;
                    repo::pending_ops::update_payload(db, &op.id, &value)?;
                    db.execute(
                        "UPDATE pending_op SET next_retry_at = NULL WHERE id = ?1",
                        [&op.id],
                    )?;
                }
                Ok(())
            }
        })
        .await
        .expect("drop the failure marker");
    let deletes_before = mock.deleted_ids().len();
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("claimed-only pass");
    assert_eq!(outcome.done, 0);
    assert_eq!(
        mock.deleted_ids().len(),
        deletes_before,
        "a claimed-but-unacked delete must be verified, not repeated"
    );
}

/// Once the provider accepted a permanent delete, a failed local commit may
/// only retry that settlement. The delete itself must not be sent again.
#[tokio::test]
async fn graph_permanent_delete_receipt_survives_a_failed_local_commit() {
    let mock = MockGraph::start(1).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    let inbox = graph_inbox(&ctx).await;
    insert_stored(&ctx, "acc", "m1", &inbox, false).await;
    set_graph_id(&ctx, "m1").await;
    let service = MailService::new(ctx.clone());

    service
        .message_act(
            &["m1".to_string()],
            &MessageAction::Delete { permanent: true },
        )
        .await
        .expect("delete");
    // Fail only the local settlement, after the provider accepted the delete.
    ctx.db
        .write(|db| {
            db.execute(
                "CREATE TRIGGER test_block_local_delete
                   BEFORE DELETE ON message
                   WHEN old.id = 'm1'
                 BEGIN
                   SELECT RAISE(ABORT, 'local commit failed');
                 END",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("install local failure trigger");

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("first pass");
    assert_eq!(outcome.retried, 1, "the local commit failure retries");
    assert_eq!(mock.deleted_ids(), vec!["m1".to_string()]);
    let op = ctx
        .db
        .read(|db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops")
        .into_iter()
        .next()
        .expect("delete op");
    assert_eq!(op.state, nuncio_core::models::PendingOpState::Pending);
    let payload = repo::pending_ops::payload::<ops::DeleteOp>(&op).expect("payload");
    assert_eq!(
        payload.outcomes[0].state,
        ops::OpMessageState::Confirmed,
        "the provider receipt is durable before local settlement"
    );

    // The retry settles locally without another permanentDelete.
    ctx.db
        .write(|db| {
            db.execute("DROP TRIGGER test_block_local_delete", [])?;
            db.execute("UPDATE pending_op SET next_retry_at = NULL", [])?;
            Ok(())
        })
        .await
        .expect("remove the failure and make retry due");
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("settlement pass");
    assert_eq!(outcome.done, 1);
    assert_eq!(
        mock.deleted_ids(),
        vec!["m1".to_string()],
        "the retry must not repeat the accepted delete"
    );
    assert!(
        ctx.db
            .read(|db| repo::messages::get(db, "m1"))
            .await
            .expect("read row")
            .is_none(),
        "the local row is settled"
    );
}

/// A Graph row with no provider identity cannot prove which server copy the
/// user selected. A local row id is not a provider-issued id, so the action
/// must be refused before it can ever reach the executor.
#[tokio::test]
async fn graph_delete_recovery_does_not_hard_delete_on_an_unattributed_404() {
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    let inbox = graph_inbox(&ctx).await;
    insert_stored(&ctx, "acc", "local-id", &inbox, false).await;
    let service = MailService::new(ctx.clone());
    let error = service
        .message_act(
            &["local-id".to_string()],
            &MessageAction::Delete { permanent: true },
        )
        .await
        .expect_err("an unpinned Graph delete must be refused");
    assert_eq!(error.code(), "conflict");
    assert!(
        ctx.db
            .read(|db| repo::messages::get(db, "local-id"))
            .await
            .expect("read row")
            .is_some(),
        "the local row must stay untouched"
    );
    let pending = ctx
        .db
        .read(|db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops");
    assert!(
        pending.is_empty(),
        "no command may be queued without a pinned provider id"
    );
}

/// A soft delete must never be upgraded to a permanent one when no Trash
/// folder is known yet: the queued operation waits for the folder sync.
#[tokio::test]
async fn graph_soft_delete_without_trash_never_becomes_permanent() {
    let mock = MockGraph::start(1).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    // No Trash folder is known yet.
    let inbox = graph_inbox(&ctx).await;
    insert_stored(&ctx, "acc", "m1", &inbox, false).await;
    set_graph_id(&ctx, "m1").await;
    let service = MailService::new(ctx.clone());

    service
        .message_act(
            &["m1".to_string()],
            &MessageAction::Delete { permanent: false },
        )
        .await
        .expect("delete");

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("graph pass");
    assert_eq!(outcome.retried, 1, "the operation waits for a Trash folder");
    assert!(
        mock.deleted_ids().is_empty(),
        "a soft delete must not send permanentDelete"
    );
    assert!(
        mock.ids().contains(&"m1".to_string()),
        "the message is still on the server"
    );
    let op = ctx
        .db
        .read(|db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops")
        .into_iter()
        .next()
        .expect("delete op");
    assert_eq!(op.state, nuncio_core::models::PendingOpState::Pending);
}

/// A `permanentDelete` 404 only proves the id is unreachable. It must not be
/// recorded as a receipt or erase the local row: the message may have moved to
/// a new Graph id.
#[tokio::test]
async fn graph_permanent_delete_404_is_not_a_receipt() {
    let mock = MockGraph::start(1).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    let inbox = graph_inbox(&ctx).await;
    insert_stored(&ctx, "acc", "m1", &inbox, false).await;
    set_graph_id(&ctx, "m1").await;
    let service = MailService::new(ctx.clone());

    service
        .message_act(
            &["m1".to_string()],
            &MessageAction::Delete { permanent: true },
        )
        .await
        .expect("delete");
    mock.fail_permanent_delete_with_status_for("m1", 404);

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("graph pass");
    assert_eq!(outcome.retried, 1, "the 404 outcome stays unverified");
    let op = ctx
        .db
        .read(|db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops")
        .into_iter()
        .next()
        .expect("delete op");
    assert_eq!(op.state, nuncio_core::models::PendingOpState::Pending);
    let payload = repo::pending_ops::payload::<ops::DeleteOp>(&op).expect("payload");
    assert_eq!(
        payload.outcomes[0].state,
        ops::OpMessageState::Unknown,
        "a 404 is evidence of unreachability, not of deletion"
    );
    assert!(
        ctx.db
            .read(|db| repo::messages::get(db, "m1"))
            .await
            .expect("read row")
            .is_some(),
        "the local row must not be erased on a 404"
    );
}

/// A targeted GET that still finds the message cannot overrule an earlier
/// request whose outcome is unknown: that request may still be in flight, so
/// the row must stay hidden rather than be released as if the delete failed.
#[tokio::test]
async fn graph_still_present_does_not_release_an_unknown_permanent_delete() {
    let mock = MockGraph::start(1).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    let inbox = graph_inbox(&ctx).await;
    insert_stored(&ctx, "acc", "m1", &inbox, false).await;
    // Make the row's id a verifiable provider identity and teach the recovery
    // which remote folder is "still the expected one".
    {
        ctx.db
            .write(move |db| {
                let mut stored = repo::messages::get_required(db, "m1")?;
                stored.gm_msgid = Some("m1".into());
                repo::messages::upsert(db, &stored)?;
                repo::sync_state::set(db, "acc", "mail", "graph_folder:Inbox", "inbox-remote")
            })
            .await
            .expect("seed identity");
    }
    mock.set_parent_folder_response(Some("inbox-remote"));
    let service = MailService::new(ctx.clone());

    service
        .message_act(
            &["m1".to_string()],
            &MessageAction::Delete { permanent: true },
        )
        .await
        .expect("delete");
    // Make this pass terminal with an unknown outcome.
    mock.set_permanent_delete_failure(true);
    ctx.db
        .write(|db| {
            db.execute("UPDATE pending_op SET attempts = 8", [])?;
            Ok(())
        })
        .await
        .expect("exhaust retries");

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("graph pass");
    assert_eq!(outcome.failed, 1);
    let op = ctx
        .db
        .read(|db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops")
        .into_iter()
        .next()
        .expect("delete op");
    let payload = repo::pending_ops::payload::<ops::DeleteOp>(&op).expect("payload");
    assert_eq!(payload.outcomes[0].state, ops::OpMessageState::Unknown);
    assert!(
        ctx.db
            .read(|db| repo::messages::get(db, "m1"))
            .await
            .expect("read row")
            .is_some(),
        "the row must not be erased while the request may still apply"
    );
    let still_hidden = {
        let inbox = inbox.clone();
        ctx.db
            .read(move |db| repo::messages::pending_delete_ids_in_folder(db, &inbox))
            .await
            .expect("pending deletes")
    };
    assert!(
        still_hidden.contains(&"m1".to_string()),
        "an unknown request keeps the row protected even when a GET still sees it"
    );
}

/// The create receipt on the operation is enough to authorize a delete even if
/// the pinned identity was lost before the discard.
#[tokio::test]
async fn graph_discard_deletes_the_operation_receipt_after_identity_loss() {
    let mock = MockGraph::start(0).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let graph = graph_draft_account(&ctx).await;
    let service = MailService::new(ctx.clone());
    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Receipt only".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("upload pass");
    let created = mock.created_ids();
    assert_eq!(created.len(), 1);

    // Lose the pinned identity but keep the operation's durable receipt.
    let mut stored = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    stored.remote_draft = None;
    stored.remote_draft_message_id = None;
    ctx.db
        .write(move |db| repo::drafts::save(db, &stored))
        .await
        .expect("clear the pin");

    service.compose_discard(&draft_id).await.expect("discard");
    let deletes = delete_ops_for(&ctx, "acc", &draft_id).await;
    assert_eq!(deletes.len(), 1);
    assert_eq!(
        deletes[0].payload.remote_message_id.as_deref(),
        Some(created[0].as_str()),
        "the operation's own receipt authorizes the delete"
    );
    ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("cleanup pass");
    assert!(mock.deleted_ids().contains(&created[0]));
    assert!(!mock.ids().contains(&created[0]));
}

/// Queue the graph drafts folder and an external copy carrying the draft's
/// stable Message-ID. No receipt exists for that copy.
async fn seed_external_graph_copy(ctx: &CoreContext, draft_id: &str) {
    let mut summary = MessageSummary::new(
        "external-copy",
        "acc",
        "acc-drafts",
        "thread-external",
        chrono::Utc::now(),
    );
    summary.subject = "External copy".into();
    summary.is_draft = true;
    let mut stored = StoredMessage::new(summary);
    stored.uid = Some(900);
    stored.uidvalidity = Some(1);
    stored.gm_msgid = Some("external-copy".into());
    stored.message_id_header = Some(nuncio_mail::util::draft_message_id(draft_id));
    ctx.db
        .write(move |db| repo::messages::upsert(db, &stored))
        .await
        .expect("insert external copy");
}

/// The targeted cleanup GET must find the expected Drafts folder; the mock
/// answers `drafts-parent` by default.
async fn seed_graph_drafts_mapping(ctx: &CoreContext) {
    ctx.db
        .write(|db| {
            repo::sync_state::set(db, "acc", "mail", "graph_folder:Drafts", "drafts-parent")
        })
        .await
        .expect("seed graph drafts mapping");
}

async fn graph_draft_account(ctx: &CoreContext) -> Account {
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(ctx, &graph).await;
    let drafts = Folder::new("acc-drafts", "acc", "Drafts", FolderRole::Drafts);
    ctx.db
        .write(move |db| repo::folders::insert(db, &drafts))
        .await
        .expect("insert drafts folder");
    graph
}

/// Discarding a draft must not delete an external same-header copy: only an
/// upload receipt or the explicitly opened row may authorize a delete.
#[tokio::test]
async fn graph_discard_keeps_an_external_copy_without_a_receipt() {
    let mock = MockGraph::start(0).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let graph = graph_draft_account(&ctx).await;
    let service = MailService::new(ctx.clone());
    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "External copy".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    seed_external_graph_copy(&ctx, &draft_id).await;

    service.compose_discard(&draft_id).await.expect("discard");
    assert!(
        delete_ops_for(&ctx, "acc", &draft_id).await.is_empty(),
        "a same-header copy must not produce a remote delete"
    );
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("graph pass");
    assert!(
        mock.deleted_ids().is_empty(),
        "the external copy must be kept: {:?}",
        mock.deleted_ids()
    );
    assert!(ctx
        .db
        .read(|db| repo::messages::get(db, "external-copy"))
        .await
        .expect("read external copy")
        .is_some());
}

/// The send path must make the same decision as discard: no receipt and no
/// explicit open means the external copy is kept.
#[tokio::test]
async fn graph_send_keeps_an_external_copy_without_a_receipt() {
    let mock = MockGraph::start(0).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let graph = graph_draft_account(&ctx).await;
    let service = MailService::new(ctx.clone());
    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "External copy send".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    seed_external_graph_copy(&ctx, &draft_id).await;

    let draft = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    service
        .compose_send_graph_with(&graph, &draft, &client)
        .await
        .expect("send");
    let deletes = delete_ops_for(&ctx, "acc", &draft_id).await;
    assert!(
        deletes.is_empty(),
        "a same-header copy must not produce a send cleanup delete"
    );
    ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("graph pass");
    assert!(mock.deleted_ids().is_empty());
    assert!(ctx
        .db
        .read(|db| repo::messages::get(db, "external-copy"))
        .await
        .expect("read external copy")
        .is_some());
}

/// A kept-local Graph draft must not be uploaded even when an APPEND row is
/// already queued: the executor's send-right claim checks suppression.
#[tokio::test]
async fn graph_keep_local_blocks_a_queued_append() {
    let mock = MockGraph::start(0).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    {
        let drafts = Folder::new("acc-drafts", "acc", "Drafts", FolderRole::Drafts);
        ctx.db
            .write(move |db| repo::folders::insert(db, &drafts))
            .await
            .expect("insert drafts folder");
    }
    let service = MailService::new(ctx.clone());
    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Graph kept local".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    {
        let id = draft_id.clone();
        ctx.db
            .write(move |db| repo::drafts::set_sync_suppressed(db, &id, true))
            .await
            .expect("suppress");
    }

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("graph pass");
    assert_eq!(outcome.done, 1, "the queued row settles without a request");
    assert!(
        mock.created_ids().is_empty(),
        "a suppressed draft must never be uploaded: {:?}",
        mock.requests()
    );
    assert!(
        !mock
            .requests()
            .iter()
            .any(|request| request == "/me/messages"),
        "no create request may be sent for a kept-local draft"
    );
}

/// A successful Graph create persists the returned message id as the draft's
/// remote identity. A later discard can therefore target that exact copy even
/// though no Drafts sync has run and the projection has no server UID yet.
#[tokio::test]
async fn graph_append_receipt_is_persisted_and_used_by_a_later_discard() {
    let mock = MockGraph::start(0).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    {
        let drafts = Folder::new("acc-drafts", "acc", "Drafts", FolderRole::Drafts);
        ctx.db
            .write(move |db| repo::folders::insert(db, &drafts))
            .await
            .expect("insert drafts folder");
    }
    let service = MailService::new(ctx.clone());
    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Graph receipt".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let projection_id = nuncio_mail::util::draft_message_id(&draft_id);

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("graph pass");
    assert_eq!(outcome.done, 1);
    let created = mock.created_ids();
    assert_eq!(created.len(), 1);
    let identity = draft_identity(&ctx, &draft_id).await;
    assert_eq!(
        identity.attribution,
        nuncio_core::models::RemoteDraftAttribution::Confirmed
    );
    assert_eq!(identity.remote_id.as_deref(), Some(created[0].as_str()));
    let pending = append_op_for(&ctx, "acc", &draft_id)
        .await
        .expect("append op");
    assert_eq!(pending.payload.append_state, ops::AppendState::Acked);
    assert_eq!(
        pending.payload.appended_remote_id.as_deref(),
        Some(created[0].as_str())
    );
    let projection = ctx
        .db
        .read({
            let id = projection_id.clone();
            move |db| repo::messages::get(db, &id)
        })
        .await
        .expect("read projection")
        .expect("projection");
    assert_eq!(projection.gm_msgid.as_deref(), Some(created[0].as_str()));
    assert!(projection.uid.is_some(), "the synthetic uid is bound too");

    // Discard now, before any Drafts sync: the pinned receipt is the only
    // address, and it must be used directly.
    service.compose_discard(&draft_id).await.expect("discard");
    let deletes = delete_ops_for(&ctx, "acc", &draft_id).await;
    assert_eq!(deletes.len(), 1, "one directed cleanup is queued");
    assert_eq!(
        deletes[0].payload.remote_message_id.as_deref(),
        Some(created[0].as_str())
    );
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("cleanup pass");
    assert_eq!(outcome.done, 1);
    assert!(mock.deleted_ids().contains(&created[0]));
    assert!(!mock.ids().contains(&created[0]));
    assert!(
        ctx.db
            .read({
                let id = projection_id.clone();
                move |db| repo::messages::get(db, &id)
            })
            .await
            .expect("read projection")
            .is_none(),
        "the confirmed delete drops the projection"
    );
}

async fn graph_inbox(ctx: &CoreContext) -> String {
    let inbox = Folder::new("graph-inbox", "acc", "Inbox", FolderRole::Inbox);
    let id = inbox.id.clone();
    ctx.db
        .write(move |db| repo::folders::insert(db, &inbox))
        .await
        .expect("insert inbox");
    id
}

async fn run_graph_until_settled(
    ctx: &CoreContext,
    graph: &Account,
    client: &nuncio_mail::graph::GraphMailClient,
) {
    for _ in 0..3 {
        ops::executor::run_due_graph_with(ctx, graph, client)
            .await
            .expect("graph pass");
        ctx.db
            .write(|db| {
                db.execute("UPDATE pending_op SET next_retry_at = NULL", [])?;
                Ok(())
            })
            .await
            .expect("make retries due");
    }
}

/// Graph deletes a batch one message at a time. When the first succeeds and the
/// second is refused, rollback and undo must settle per message: the deleted
/// one cannot be restored, the refused one must stay visible and undoable.
#[tokio::test]
async fn graph_partial_permanent_delete_settles_per_message() {
    let mock = MockGraph::start(2).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    let inbox = graph_inbox(&ctx).await;
    insert_stored(&ctx, "acc", "m1", &inbox, false).await;
    insert_stored(&ctx, "acc", "m2", &inbox, false).await;
    set_graph_id(&ctx, "m1").await;
    set_graph_id(&ctx, "m2").await;
    let service = MailService::new(ctx.clone());

    let undo_id = service
        .message_act(
            &["m1".to_string(), "m2".to_string()],
            &MessageAction::Delete { permanent: true },
        )
        .await
        .expect("delete");
    mock.fail_permanent_delete_for("m2");
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    run_graph_until_settled(&ctx, &graph, &client).await;

    assert!(!mock.ids().contains(&"m1".to_string()));
    assert!(
        mock.ids().contains(&"m2".to_string()),
        "the refused message is untouched on the server"
    );

    let op = ctx
        .db
        .read(move |db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops")
        .into_iter()
        .next()
        .expect("delete op");
    assert_eq!(op.state, nuncio_core::models::PendingOpState::Failed);
    let payload = repo::pending_ops::payload::<ops::DeleteOp>(&op).expect("payload");
    assert_eq!(
        payload.outcomes[0].state,
        ops::OpMessageState::Confirmed,
        "m1 was deleted by the server"
    );
    assert_eq!(
        payload.outcomes[1].state,
        ops::OpMessageState::NotExecuted,
        "m2 was definitely refused"
    );

    // The confirmed row is gone; the refused row is visible again.
    assert!(ctx
        .db
        .read(|db| repo::messages::get(db, "m1"))
        .await
        .expect("m1")
        .is_none());
    let m2 = ctx
        .db
        .read(|db| repo::messages::get_required(db, "m2"))
        .await
        .expect("m2");
    assert!(!m2.is_deleted_local, "the refused delete is rolled back");

    // Undo can only fail the confirmed message; the refused one is already
    // back. It must not use the op's aggregate `failed` state.
    let error = service
        .message_undo(&undo_id)
        .await
        .expect_err("one message is permanently gone");
    assert!(error.to_string().contains("已删除"), "unexpected: {error}");
    let entries = ctx
        .db
        .read({
            let id = undo_id.clone();
            move |db| repo::undo::list_entries(db, &id)
        })
        .await
        .expect("entries");
    assert_eq!(
        entries[0].result,
        nuncio_core::models::UndoEntryResult::Failed
    );
    assert_eq!(
        entries[1].result,
        nuncio_core::models::UndoEntryResult::Done
    );
}

/// Undoing a Graph permanent delete *before* the command reaches the wire must
/// restore the snapshot's immutable provider identity and clear the pending
/// projection. Leaving the row `uid IS NULL, gm_msgid IS NULL,
/// target_pending = 1` made the next delta import a second row for the same
/// message (and the original stayed unbound forever).
#[tokio::test]
async fn graph_undo_of_an_unsent_permanent_delete_restores_identity_without_duplicates() {
    let mock = MockGraph::start(1).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    let inbox = graph_inbox(&ctx).await;
    ctx.db
        .write(|db| repo::sync_state::set(db, "acc", "mail", "graph_folder:Inbox", "inbox-remote"))
        .await
        .expect("seed graph folder mapping");
    let folder = ctx
        .db
        .read({
            let inbox = inbox.clone();
            move |db| repo::folders::get(db, &inbox)
        })
        .await
        .expect("folder")
        .expect("inbox folder");
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &graph, &folder, &client)
        .await
        .expect("initial graph sync");
    let rows = summaries(&ctx, &inbox).await;
    assert_eq!(rows.len(), 1, "one message was imported");
    let id = rows[0].id.clone();

    let service = MailService::new(ctx.clone());
    let undo_id = service
        .message_act(
            std::slice::from_ref(&id),
            &MessageAction::Delete { permanent: true },
        )
        .await
        .expect("permanent delete");

    // Nothing was sent yet: undo cancels the queued operation. The source copy
    // still carries the identity captured in the snapshot.
    assert!(service.message_undo(&undo_id).await.expect("undo"));

    let restored = ctx
        .db
        .read({
            let id = id.clone();
            move |db| repo::messages::get_required(db, &id)
        })
        .await
        .expect("restored row");
    assert!(!restored.is_deleted_local);
    assert_eq!(
        restored.gm_msgid.as_deref(),
        Some(id.as_str()),
        "the Graph provider id must come back from the snapshot"
    );
    assert_eq!(
        restored.uid,
        Some(nuncio_mail::sync::messages::synthetic_uid(&id)),
        "the synthetic UID is derived from the immutable row id"
    );
    let (pending_delete, target_pending, reconcile_owner): (i64, i64, Option<String>) = ctx
        .db
        .read({
            let id = id.clone();
            move |db| {
                Ok(db.query_row(
                    "SELECT pending_delete, target_pending, reconcile_op_id
                       FROM message WHERE id = ?1",
                    rusqlite::params![id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?)
            }
        })
        .await
        .expect("projection state");
    assert_eq!(pending_delete, 0);
    assert_eq!(
        target_pending, 0,
        "the restored row is bound, not a pending projection"
    );
    assert_eq!(
        reconcile_owner, None,
        "no empty reconcile token may be written by the undo"
    );

    // The next delta carries the same message: it must update the restored row
    // instead of importing a duplicate.
    nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &graph, &folder, &client)
        .await
        .expect("delta after undo");
    let after = summaries(&ctx, &inbox).await;
    assert_eq!(
        after.len(),
        1,
        "the restored identity must absorb the delta update"
    );
    assert_eq!(after[0].id, id);
}

/// A confirmed permanent delete in a batch must not force the whole request to
/// `Failed` while another entry still waits for an unsettled operation; that
/// would hide retryable work behind a terminal state.
#[tokio::test]
async fn permanent_gone_does_not_fail_a_request_with_pending_entries() {
    let mock = MockGraph::start(2).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    let inbox = graph_inbox(&ctx).await;
    insert_stored(&ctx, "acc", "m1", &inbox, false).await;
    insert_stored(&ctx, "acc", "m2", &inbox, false).await;
    set_graph_id(&ctx, "m1").await;
    set_graph_id(&ctx, "m2").await;
    let service = MailService::new(ctx.clone());

    let undo_id = service
        .message_act(
            &["m1".to_string(), "m2".to_string()],
            &MessageAction::Delete { permanent: true },
        )
        .await
        .expect("delete");
    // m1 succeeds; m2's request leaves the outcome unknown and the operation
    // remains queued for a retry.
    mock.fail_permanent_delete_with_status_for("m2", 500);
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("graph pass");

    let _ = service.message_undo(&undo_id).await;
    let request = ctx
        .db
        .read({
            let id = undo_id.clone();
            move |db| repo::undo::get_request(db, &id)
        })
        .await
        .expect("request")
        .expect("request exists");
    assert_eq!(
        request.state,
        nuncio_core::models::UndoState::Deferred,
        "the unknown message still has retryable work; the request is not Failed"
    );
    let entries = ctx
        .db
        .read({
            let id = undo_id.clone();
            move |db| repo::undo::list_entries(db, &id)
        })
        .await
        .expect("entries");
    assert_eq!(
        entries[0].result,
        nuncio_core::models::UndoEntryResult::Failed
    );
    assert_eq!(
        entries[1].result,
        nuncio_core::models::UndoEntryResult::Pending
    );
}

/// A DELETE queued by an older build names only folder+UID (no `draftId`). When
/// the draft still carries its pinned identity, that identity precisely names
/// the copy the replacement superseded, so the switch must cancel the legacy
/// delete just like a new payload.
#[tokio::test]
async fn switching_account_cancels_a_legacy_delete_by_pinned_identity() {
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account_a = account("acc-a", "a@example.com", Some(server(9)), None, None);
    let account_b = account("acc-b", "b@example.com", Some(server(9)), None, None);
    insert_account(&ctx, &account_a).await;
    insert_account(&ctx, &account_b).await;
    for folder in [
        Folder::new("a-drafts", "acc-a", "Drafts", FolderRole::Drafts),
        Folder::new("b-drafts", "acc-b", "Drafts", FolderRole::Drafts),
    ] {
        ctx.db
            .write(move |db| repo::folders::insert(db, &folder))
            .await
            .expect("insert drafts folder");
    }
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc-a")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Legacy payload".into();
    draft.html = "<p>body</p>".into();
    draft.remote_draft = Some(nuncio_core::models::RemoteDraftIdentity::confirmed(
        "a-drafts",
        7,
        Some(1),
        Some("old-projection".into()),
    ));
    let draft = service.compose_save(&draft).await.expect("save in A");
    let draft_id = draft.id.clone().expect("draft id");
    assert!(draft.remote_draft.is_some(), "identity is pinned");

    let legacy = enqueue_legacy_draft_delete(&ctx, "acc-a", "a-drafts", 7).await;

    let mut switched = service.compose_get_draft(&draft_id).await.expect("reload");
    switched.account_id = "acc-b".into();
    service
        .compose_save(&switched)
        .await
        .expect("save after switch");

    assert_eq!(
        pending_op_state(&ctx, &legacy).await,
        nuncio_core::models::PendingOpState::Cancelled,
        "the legacy DELETE naming the pinned copy must be cancelled with its APPEND"
    );
}

/// Without a pinned identity the switch resolves the exact server copy from
/// the local Drafts projection (the row carrying this draft's stable
/// Message-ID). Only the legacy DELETE naming that folder+UID is cancelled;
/// another draft's legacy DELETE in the same Drafts folder must stay queued.
#[tokio::test]
async fn switching_account_cancels_only_the_matching_legacy_delete() {
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account_a = account("acc-a", "a@example.com", Some(server(9)), None, None);
    let account_b = account("acc-b", "b@example.com", Some(server(9)), None, None);
    insert_account(&ctx, &account_a).await;
    insert_account(&ctx, &account_b).await;
    for folder in [
        Folder::new("a-drafts", "acc-a", "Drafts", FolderRole::Drafts),
        Folder::new("a-archive", "acc-a", "Archive", FolderRole::Archive),
        Folder::new("b-drafts", "acc-b", "Drafts", FolderRole::Drafts),
    ] {
        ctx.db
            .write(move |db| repo::folders::insert(db, &folder))
            .await
            .expect("insert folder");
    }
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc-a")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Projection identity".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save in A");
    let draft_id = draft.id.clone().expect("draft id");
    assert!(draft.remote_draft.is_none(), "nothing is pinned");

    // A sync bound this draft's projection to server UID 8. The legacy DELETE
    // for exactly that copy is this draft's cleanup; UID 9 belongs to another
    // draft in the same folder.
    let projection_id = nuncio_mail::util::draft_message_id(&draft_id);
    ctx.db
        .write(move |db| {
            db.execute(
                "UPDATE message SET uid = 8, uidvalidity = 1 WHERE id = ?1",
                rusqlite::params![projection_id],
            )?;
            Ok(())
        })
        .await
        .expect("bind projection uid");

    let drafts_delete = enqueue_legacy_draft_delete(&ctx, "acc-a", "a-drafts", 8).await;
    let other_draft_delete = enqueue_legacy_draft_delete(&ctx, "acc-a", "a-drafts", 9).await;
    let archive_delete = enqueue_legacy_draft_delete(&ctx, "acc-a", "a-archive", 10).await;
    let other_account_delete = enqueue_legacy_draft_delete(&ctx, "acc-b", "b-drafts", 11).await;

    let mut switched = service.compose_get_draft(&draft_id).await.expect("reload");
    switched.account_id = "acc-b".into();
    service
        .compose_save(&switched)
        .await
        .expect("save after switch");

    assert_eq!(
        pending_op_state(&ctx, &drafts_delete).await,
        nuncio_core::models::PendingOpState::Cancelled,
        "the legacy DELETE naming this draft's projected copy is cancelled"
    );
    assert_eq!(
        pending_op_state(&ctx, &other_draft_delete).await,
        nuncio_core::models::PendingOpState::Cancelled,
        "an unattributable legacy DELETE must be isolated, never executed"
    );
    assert_eq!(
        pending_op_state(&ctx, &archive_delete).await,
        nuncio_core::models::PendingOpState::Cancelled,
        "a delete in another folder cannot be tied to the draft, so it is isolated too"
    );
    assert_eq!(
        pending_op_state(&ctx, &other_account_delete).await,
        nuncio_core::models::PendingOpState::Pending,
        "another account's queue is never touched"
    );
}

/// Regression for the real upgrade shape: an old build saved an *external*
/// Graph draft by clearing `remote_draft_message_id`, keeping the remote row
/// (with its original Message-ID) and adding a UID-less local projection, while
/// the queue holds a legacy DELETE (no `draftId`) and an APPEND.
///
/// When the draft then switches accounts before the queue runs, the DELETE
/// cannot be associated with it. Leaving it queued let the old account's worker
/// resolve folder+UID to the Graph id and delete a remote draft the switch
/// deliberately kept. The switch must isolate it instead: no request on the
/// wire, the APPEND cancelled, and an auditable quarantined row retained.
#[tokio::test]
async fn switching_account_quarantines_an_unattributable_legacy_delete() {
    let mock = MockGraph::start(1).await; // the old remote draft, id "m1"
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut account_a = account("acc-a", "a@example.com", Some(server(9)), None, None);
    account_a.provider = ProviderKind::Outlook;
    account_a.oauth_provider = Some(OAuthProvider::Microsoft);
    let mut account_b = account("acc-b", "b@example.com", Some(server(9)), None, None);
    account_b.provider = ProviderKind::Outlook;
    account_b.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &account_a).await;
    insert_account(&ctx, &account_b).await;
    for folder in [
        Folder::new("a-drafts", "acc-a", "Drafts", FolderRole::Drafts),
        Folder::new("b-drafts", "acc-b", "Drafts", FolderRole::Drafts),
    ] {
        ctx.db
            .write(move |db| repo::folders::insert(db, &folder))
            .await
            .expect("insert folder");
    }
    let service = MailService::new(ctx.clone());

    // The draft row: no pinned identity and no `remoteDraftMessageId`, exactly
    // what the old external-draft save left behind.
    let mut draft = service.compose_create(Some("acc-a")).await.expect("draft");
    draft.id = Some("legacy-switch".into());
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "External draft".into();
    draft.html = "<p>body</p>".into();
    draft.remote_draft = None;
    draft.remote_draft_message_id = None;
    {
        let draft = draft.clone();
        ctx.db
            .write(move |db| repo::drafts::save(db, &draft))
            .await
            .expect("seed draft row");
    }
    let draft_id = "legacy-switch".to_string();

    // The old remote row keeps the *external* Message-ID and the folder+UID the
    // legacy DELETE names; the mapping lets the executor reach the Graph id.
    let old_projection = "remote-old-external";
    {
        let cursor_key = nuncio_mail::graph::messages::delta_cursor_key("a-drafts");
        let header = nuncio_mail::util::draft_message_id(&draft_id);
        ctx.db
            .write(move |db| {
                let mut summary = MessageSummary::new(
                    old_projection,
                    "acc-a",
                    "a-drafts",
                    "thread-old-external",
                    chrono::Utc::now(),
                );
                summary.subject = "Old external draft".into();
                summary.is_draft = true;
                let mut stored = StoredMessage::new(summary);
                stored.uid = Some(7);
                stored.uidvalidity = Some(1);
                stored.gm_msgid = Some("m1".into());
                stored.message_id_header = Some("<external-draft@example.com>".into());
                repo::messages::upsert(db, &stored)?;
                repo::sync_state::set(db, "acc-a", "mail", "graph_remote:a-drafts:7", "m1")?;
                repo::sync_state::set(db, "acc-a", "mail", &cursor_key, "cursor-a")?;
                // The new UID-less projection the old build wrote for this
                // draft. It cannot identify the copy above.
                let mut summary = MessageSummary::new(
                    "legacy-new-projection",
                    "acc-a",
                    "a-drafts",
                    "thread-legacy-switch",
                    chrono::Utc::now(),
                );
                summary.subject = "External draft".into();
                summary.is_draft = true;
                let mut projection = StoredMessage::new(summary);
                projection.uid = None;
                projection.message_id_header = Some(header);
                repo::messages::upsert(db, &projection)?;
                Ok(())
            })
            .await
            .expect("seed old-library shape");
    }
    let legacy = enqueue_legacy_draft_delete(&ctx, "acc-a", "a-drafts", 7).await;
    let append = enqueue_legacy_draft_append(&ctx, "acc-a", "a-drafts", &draft_id).await;

    let mut switched = service.compose_get_draft(&draft_id).await.expect("reload");
    switched.account_id = "acc-b".into();
    service
        .compose_save(&switched)
        .await
        .expect("save after switch");

    // The intent survives, isolated: not executed and not silently cancelled.
    let op = {
        let id = legacy.clone();
        ctx.db
            .read(move |db| repo::pending_ops::get_required(db, &id))
            .await
            .expect("quarantined op")
    };
    assert_eq!(
        op.state,
        nuncio_core::models::PendingOpState::Cancelled,
        "an unattributable legacy DELETE must be isolated as cancelled so every build ignores it"
    );
    let payload: ops::DeleteRemoteDraftOp =
        repo::pending_ops::payload(&op).expect("delete payload");
    assert!(payload.quarantined, "the payload must record the isolation");
    assert!(
        payload
            .quarantine_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("could not be attributed")),
        "the auditable reason must stay on the row"
    );
    assert_eq!(
        pending_op_state(&ctx, &append).await,
        nuncio_core::models::PendingOpState::Cancelled,
        "the paired APPEND is cancelled with the switch"
    );

    // The old account's worker must send nothing. The mock starts with the
    // remote draft present; it must still be there afterwards.
    assert_eq!(mock.ids(), vec!["m1".to_string()], "precondition");
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome = ops::executor::run_due_graph_with(&ctx, &account_a, &client)
        .await
        .expect("run A queue");
    assert_eq!(outcome.done, 0);
    assert_eq!(outcome.failed, 0);
    assert!(
        mock.deleted_ids().is_empty(),
        "no DELETE may be sent for an unattributed legacy op: {:?}",
        mock.deleted_ids()
    );
    assert_eq!(
        mock.ids(),
        vec!["m1".to_string()],
        "the old account's remote draft is kept"
    );
    // The switch queued reconciliation instead of guessing at the outcome.
    let cursor = ctx
        .db
        .read({
            let key = nuncio_mail::graph::messages::delta_cursor_key("a-drafts");
            move |db| repo::sync_state::get(db, "acc-a", "mail", &key)
        })
        .await
        .expect("read cursor");
    assert_eq!(cursor, None, "the isolated folder is queued for a re-sync");
}

/// An isolated legacy DELETE is stored as `cancelled` with the `quarantined`
/// payload marker. The executor selects only `pending` rows, so neither the
/// due scan nor a Graph pass may touch it (no DELETE on the wire), while the
/// row and its reason survive pruning as the audit record.
#[tokio::test]
async fn cancelled_quarantined_delete_is_never_executed() {
    let mock = MockGraph::start(1).await; // id "m1" is still live on the server
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc-a", "a@example.com", None, None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;

    let folder_id = "a-drafts".to_string();
    {
        let folder_id = folder_id.clone();
        ctx.db
            .write(move |db| {
                repo::folders::insert(
                    db,
                    &Folder::new(&folder_id, "acc-a", "Drafts", FolderRole::Drafts),
                )?;
                repo::sync_state::set(db, "acc-a", "mail", "graph_remote:a-drafts:7", "m1")?;
                let mut summary = MessageSummary::new(
                    "projection-7",
                    "acc-a",
                    folder_id.as_str(),
                    "thread-7",
                    chrono::Utc::now(),
                );
                summary.is_draft = true;
                let mut stored = StoredMessage::new(summary);
                stored.uid = Some(7);
                stored.uidvalidity = Some(1);
                stored.gm_msgid = Some("m1".into());
                repo::messages::upsert(db, &stored)?;
                Ok(())
            })
            .await
            .expect("seed projection");
    }
    let op_id = enqueue_legacy_draft_delete(&ctx, "acc-a", "a-drafts", 7).await;
    {
        // Simulate the persisted shape after isolation.
        let id = op_id.clone();
        ctx.db
            .write(move |db| {
                let op = repo::pending_ops::get_required(db, &id)?;
                let mut payload: ops::DeleteRemoteDraftOp = repo::pending_ops::payload(&op)?;
                payload.quarantined = true;
                payload.quarantine_reason = Some("legacy test isolation".into());
                let value = serde_json::to_value(&payload).expect("json");
                repo::pending_ops::update_payload(db, &id, &value)?;
                repo::pending_ops::quarantine(db, &id)
            })
            .await
            .expect("isolate legacy delete");
    }

    let due = {
        let now = chrono::Utc::now() + chrono::Duration::days(1);
        ctx.db
            .read(move |db| {
                Ok(repo::pending_ops::next_due(db, now, 10)?
                    .into_iter()
                    .map(|op| op.id)
                    .collect::<Vec<_>>())
            })
            .await
            .expect("due scan")
    };
    assert!(
        due.is_empty(),
        "a cancelled isolation must never be selected as due: {due:?}"
    );

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("graph pass");
    assert_eq!(outcome.done, 0);
    assert_eq!(outcome.failed, 0);
    assert!(
        mock.deleted_ids().is_empty(),
        "no DELETE may be sent for an isolated op: {:?}",
        mock.deleted_ids()
    );
    assert_eq!(
        mock.ids(),
        vec!["m1".to_string()],
        "the remote draft must be kept"
    );

    let op = {
        let id = op_id.clone();
        ctx.db
            .read(move |db| repo::pending_ops::get_required(db, &id))
            .await
            .expect("auditable row")
    };
    assert_eq!(
        op.state,
        nuncio_core::models::PendingOpState::Cancelled,
        "the row uses a state every released build understands"
    );
    let payload: ops::DeleteRemoteDraftOp =
        repo::pending_ops::payload(&op).expect("delete payload");
    assert!(payload.quarantined, "the payload keeps the audit marker");
    assert_eq!(
        payload.quarantine_reason.as_deref(),
        Some("legacy test isolation")
    );

    // Pruning treats it as auditable evidence, not as an ordinary settled row.
    let pruned = {
        let now = chrono::Utc::now() + chrono::Duration::days(1);
        ctx.db
            .write(move |db| repo::pending_ops::prune_finished(db, now))
            .await
            .expect("prune")
    };
    assert_eq!(pruned, 0, "the isolated row is retained");
    let retained = {
        let id = op_id.clone();
        ctx.db
            .read(move |db| repo::pending_ops::get(db, &id))
            .await
            .expect("read retained row")
    };
    assert!(retained.is_some(), "the audit record survives pruning");
}

async fn enqueue_legacy_draft_append(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    draft_id: &str,
) -> String {
    let op = nuncio_core::models::NewPendingOp::new(
        account_id,
        ops::KIND_APPEND_DRAFT,
        serde_json::json!({
            "draftId": draft_id,
            "folderId": folder_id,
            "rfc822Path": "/nonexistent/legacy-draft.eml",
            "appendState": "idle",
        }),
        "Save draft to server",
    );
    ctx.db
        .write(move |db| repo::pending_ops::enqueue(db, op))
        .await
        .expect("enqueue legacy append")
        .id
}

/// Cancelling an in-flight legacy DELETE cannot stop a request already on the
/// wire. The operation is still stopped from retrying, but its payload records
/// that the outcome is unknown and the folder is marked for a full re-sync.
#[tokio::test]
async fn switching_account_marks_an_in_flight_delete_outcome_unknown() {
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account_a = account("acc-a", "a@example.com", Some(server(9)), None, None);
    let account_b = account("acc-b", "b@example.com", Some(server(9)), None, None);
    insert_account(&ctx, &account_a).await;
    insert_account(&ctx, &account_b).await;
    for folder in [
        Folder::new("a-drafts", "acc-a", "Drafts", FolderRole::Drafts),
        Folder::new("b-drafts", "acc-b", "Drafts", FolderRole::Drafts),
    ] {
        ctx.db
            .write(move |db| repo::folders::insert(db, &folder))
            .await
            .expect("insert drafts folder");
    }
    ctx.db
        .write(|db| repo::folders::set_sync_flags(db, "a-drafts", Some(1), Some(55), None))
        .await
        .expect("seed folder sync flags");
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc-a")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "In flight".into();
    draft.html = "<p>body</p>".into();
    draft.remote_draft = Some(nuncio_core::models::RemoteDraftIdentity::confirmed(
        "a-drafts",
        7,
        Some(1),
        Some("old-projection".into()),
    ));
    let draft = service.compose_save(&draft).await.expect("save in A");
    let draft_id = draft.id.clone().expect("draft id");

    let legacy = enqueue_legacy_draft_delete(&ctx, "acc-a", "a-drafts", 7).await;
    {
        let id = legacy.clone();
        ctx.db
            .write(move |db| repo::pending_ops::mark_in_progress(db, &id))
            .await
            .expect("claim delete");
    }

    let mut switched = service.compose_get_draft(&draft_id).await.expect("reload");
    switched.account_id = "acc-b".into();
    service
        .compose_save(&switched)
        .await
        .expect("save after switch");

    let op = {
        let id = legacy.clone();
        ctx.db
            .read(move |db| repo::pending_ops::get_required(db, &id))
            .await
            .expect("op row")
    };
    assert_eq!(op.state, nuncio_core::models::PendingOpState::Cancelled);
    let payload: ops::DeleteRemoteDraftOp =
        repo::pending_ops::payload(&op).expect("delete payload");
    assert!(
        payload.outcome_unknown,
        "an in-flight delete must record that it may already have executed"
    );
    let (_, uidnext, _) = ctx
        .db
        .read(|db| repo::folders::sync_flags(db, "a-drafts"))
        .await
        .expect("folder sync flags");
    assert!(
        uidnext.is_none(),
        "the folder is queued for a full re-sync to reconcile the unknown outcome"
    );
}

/// Queue a draft delete in the pre-`draftId` payload shape.
async fn enqueue_legacy_draft_delete(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    uid: i64,
) -> String {
    let op = nuncio_core::models::NewPendingOp::new(
        account_id,
        ops::KIND_DELETE_REMOTE_DRAFT,
        serde_json::json!({ "folderId": folder_id, "uid": uid }),
        "Legacy remove remote draft",
    );
    ctx.db
        .write(move |db| repo::pending_ops::enqueue(db, op))
        .await
        .expect("enqueue legacy delete")
        .id
}

async fn pending_op_state(ctx: &CoreContext, id: &str) -> nuncio_core::models::PendingOpState {
    let id = id.to_string();
    ctx.db
        .read(move |db| repo::pending_ops::get_required(db, &id))
        .await
        .expect("read pending op")
        .state
}

/// A Graph remote-draft DELETE can succeed after the switch cancelled it (the
/// cancellation cannot stop a request already on the wire). The executor must
/// then queue the folder for a post-op Graph sync and drop the delta cursor,
/// so the sync is a full scan that reconciles the projection instead of
/// leaving it stale behind a cursor that never mentions the change.
#[tokio::test]
async fn graph_delete_completing_after_cancellation_resyncs_the_projection() {
    let mock = MockGraph::start_with_delete_gate(0).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc-a", "a@example.com", None, None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;

    let folder_id = "a-drafts".to_string();
    let projection_id = "remote-old-draft".to_string();
    let cursor_key = nuncio_mail::graph::messages::delta_cursor_key(&folder_id);
    {
        let folder_id = folder_id.clone();
        let projection_id = projection_id.clone();
        let cursor_key = cursor_key.clone();
        ctx.db
            .write(move |db| {
                repo::folders::insert(
                    db,
                    &Folder::new(&folder_id, "acc-a", "Drafts", FolderRole::Drafts),
                )?;
                // UIDNEXT makes the "only UIDNEXT was cleared" failure visible.
                repo::folders::set_sync_flags(db, &folder_id, Some(1), Some(1), None)?;
                repo::sync_state::set(db, "acc-a", "mail", "graph_folder:Drafts", "AAMk-drafts")?;
                repo::sync_state::set(
                    db,
                    "acc-a",
                    "mail",
                    &format!("graph_remote:{folder_id}:7"),
                    "graph-remote-old",
                )?;
                repo::sync_state::set(
                    db,
                    "acc-a",
                    "mail",
                    &cursor_key,
                    "https://graph.test/delta-cursor",
                )?;
                let mut summary = MessageSummary::new(
                    projection_id.as_str(),
                    "acc-a",
                    folder_id.as_str(),
                    format!("thread-{projection_id}"),
                    chrono::Utc::now(),
                );
                summary.subject = "Old draft".into();
                summary.is_draft = true;
                let mut stored = StoredMessage::new(summary);
                stored.uid = Some(7);
                stored.uidvalidity = Some(1);
                stored.gm_msgid = Some("graph-remote-old".into());
                stored.message_id_header = Some(nuncio_mail::util::draft_message_id("old-draft"));
                repo::messages::upsert(db, &stored)?;
                Ok(())
            })
            .await
            .expect("seed remote draft");
    }

    let op_id = {
        let payload = ops::DeleteRemoteDraftOp {
            folder_id: folder_id.clone(),
            uid: 7,
            remote_message_id: None,
            draft_id: Some("old-draft".into()),
            source_uidvalidity: Some(1),
            message_id: None,
            outcome_unknown: false,
            quarantined: false,
            quarantine_reason: None,
        };
        let value = serde_json::to_value(payload).expect("json");
        let op = nuncio_core::models::NewPendingOp::new(
            "acc-a",
            ops::KIND_DELETE_REMOTE_DRAFT,
            value,
            "Remove remote draft",
        );
        ctx.db
            .write(move |db| repo::pending_ops::enqueue(db, op))
            .await
            .expect("queue delete")
            .id
    };

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let worker = {
        let ctx = ctx.clone();
        let account = graph.clone();
        let client = client.clone();
        tokio::spawn(
            async move { ops::executor::run_due_graph_with(&ctx, &account, &client).await },
        )
    };
    // The mock withholds the DELETE response, so the request is provably on
    // the wire while the test cancels the operation.
    let _ = tokio::time::timeout(Duration::from_secs(5), mock.delete_received.acquire())
        .await
        .expect("DELETE must reach the mock")
        .expect("permit");
    assert_eq!(
        pending_op_state(&ctx, &op_id).await,
        nuncio_core::models::PendingOpState::InProgress,
        "executor must be mid-flight"
    );
    {
        let id = op_id.clone();
        ctx.db
            .write(move |db| {
                let op = repo::pending_ops::get_required(db, &id)?;
                let mut payload: ops::DeleteRemoteDraftOp = repo::pending_ops::payload(&op)?;
                payload.outcome_unknown = true;
                let value = serde_json::to_value(&payload).expect("json");
                repo::pending_ops::update_payload(db, &id, &value)?;
                repo::pending_ops::cancel(db, &id)
            })
            .await
            .expect("cancel while in flight");
    }
    mock.release_delete.add_permits(1);

    let outcome = worker.await.expect("worker task").expect("graph pass");
    assert_eq!(outcome.done, 0, "the cancelled row is not counted as done");
    assert!(
        outcome.resync_folders.contains(&folder_id),
        "the deleted folder must be queued for a post-op Graph sync"
    );
    let cursor = ctx
        .db
        .read({
            let cursor_key = cursor_key.clone();
            move |db| repo::sync_state::get(db, "acc-a", "mail", &cursor_key)
        })
        .await
        .expect("read cursor");
    assert_eq!(
        cursor, None,
        "the unknown outcome must clear the Graph delta cursor"
    );

    // The account actor drains `resync_folders` with this call. A full scan
    // (the mock now reports no messages) must drop the deleted projection.
    let folder = ctx
        .db
        .read({
            let id = folder_id.clone();
            move |db| repo::folders::get_required(db, &id)
        })
        .await
        .expect("folder");
    nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &graph, &folder, &client)
        .await
        .expect("post-op Graph sync");
    let projection = ctx
        .db
        .read(move |db| repo::messages::get(db, &projection_id))
        .await
        .expect("read projection");
    assert!(
        projection.is_none(),
        "the delta pass must converge the projection to the server state"
    );
}

/// A successful Graph DELETE confirms one exact Graph id. The local projection
/// must be removed with that receipt even when the account has a non-zero sync
/// window and the draft is older than it: a full scan drops out-of-window rows
/// before they reach the store, so it can never return (and therefore never
/// prove the absence of) a row this old. Absence from the window is not
/// evidence of deletion.
#[tokio::test]
async fn graph_delete_success_removes_a_projection_outside_the_sync_window() {
    let mock = MockGraph::start(1).await; // one live draft, id "m1"
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc-a", "a@example.com", None, None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    graph.sync_window_days = 30;
    insert_account(&ctx, &graph).await;

    let folder_id = "a-drafts".to_string();
    let projection_id = "old-out-of-window-draft".to_string();
    let mapping_key = "graph_remote:a-drafts:7".to_string();
    {
        let folder_id = folder_id.clone();
        let projection_id = projection_id.clone();
        let mapping_key = mapping_key.clone();
        ctx.db
            .write(move |db| {
                repo::folders::insert(
                    db,
                    &Folder::new(&folder_id, "acc-a", "Drafts", FolderRole::Drafts),
                )?;
                repo::sync_state::set(db, "acc-a", "mail", &mapping_key, "m1")?;
                let mut summary = MessageSummary::new(
                    projection_id.as_str(),
                    "acc-a",
                    folder_id.as_str(),
                    format!("thread-{projection_id}"),
                    // Well outside the 30-day window.
                    chrono::Utc::now() - chrono::Duration::days(400),
                );
                summary.subject = "Very old draft".into();
                summary.is_draft = true;
                let mut stored = StoredMessage::new(summary);
                stored.uid = Some(7);
                stored.uidvalidity = Some(1);
                stored.gm_msgid = Some("m1".into());
                stored.message_id_header = Some(nuncio_mail::util::draft_message_id("old-draft"));
                repo::messages::upsert(db, &stored)?;
                Ok(())
            })
            .await
            .expect("seed out-of-window projection");
    }
    let op_id = {
        let payload = ops::DeleteRemoteDraftOp {
            folder_id: folder_id.clone(),
            uid: 7,
            remote_message_id: None,
            draft_id: Some("old-draft".into()),
            source_uidvalidity: Some(1),
            message_id: Some(projection_id.clone()),
            outcome_unknown: false,
            quarantined: false,
            quarantine_reason: None,
        };
        let value = serde_json::to_value(payload).expect("json");
        let op = nuncio_core::models::NewPendingOp::new(
            "acc-a",
            ops::KIND_DELETE_REMOTE_DRAFT,
            value,
            "Remove remote draft",
        );
        ctx.db
            .write(move |db| repo::pending_ops::enqueue(db, op))
            .await
            .expect("queue delete")
            .id
    };

    assert_eq!(mock.ids(), vec!["m1".to_string()], "precondition");
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("graph pass");
    assert_eq!(outcome.done, 1);
    assert_eq!(
        mock.deleted_ids(),
        vec!["m1".to_string()],
        "the exact Graph id is sent"
    );
    assert!(
        mock.ids().is_empty(),
        "the mock must model a real deletion, not a no-op 204"
    );

    let projection = {
        let id = projection_id.clone();
        ctx.db
            .read(move |db| repo::messages::get(db, &id))
            .await
            .expect("read projection")
    };
    assert!(
        projection.is_none(),
        "the successful receipt must remove the out-of-window projection directly"
    );
    let mapping = {
        let key = mapping_key.clone();
        ctx.db
            .read(move |db| repo::sync_state::get(db, "acc-a", "mail", &key))
            .await
            .expect("read mapping")
    };
    assert_eq!(mapping, None, "the retired Graph id mapping is dropped");
    assert_eq!(
        pending_op_state(&ctx, &op_id).await,
        nuncio_core::models::PendingOpState::Done
    );
}

/// A Graph DELETE answering 404 means the remote item is already gone, which is
/// success. The projection and every `graph_remote:*` mapping to that id must
/// still be retired; mappings for other ids and non-`graph_remote` keys that
/// merely share the value must survive.
#[tokio::test]
async fn graph_delete_404_removes_projection_and_only_remote_mappings() {
    let mock = MockGraph::start_with_missing_remote(0).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc-a", "a@example.com", None, None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;

    let folder_id = "a-drafts".to_string();
    let projection_id = "gone-draft-projection".to_string();
    {
        let folder_id = folder_id.clone();
        let projection_id = projection_id.clone();
        ctx.db
            .write(move |db| {
                repo::folders::insert(
                    db,
                    &Folder::new(&folder_id, "acc-a", "Drafts", FolderRole::Drafts),
                )?;
                repo::sync_state::set(db, "acc-a", "mail", "graph_remote:a-drafts:7", "m-gone")?;
                // A move may leave the same Graph id under another folder.
                repo::sync_state::set(db, "acc-a", "mail", "graph_remote:a-archive:7", "m-gone")?;
                // Different Graph id: must remain resolvable.
                repo::sync_state::set(db, "acc-a", "mail", "graph_remote:a-drafts:8", "m-keep")?;
                // Same value but not a `graph_remote:` key: never touched.
                repo::sync_state::set(db, "acc-a", "mail", "anchor:a-drafts", "m-gone")?;
                repo::sync_state::set(db, "acc-a", "mail", "graph_folder:Drafts", "m-gone")?;

                let mut summary = MessageSummary::new(
                    projection_id.as_str(),
                    "acc-a",
                    folder_id.as_str(),
                    "thread-gone",
                    chrono::Utc::now(),
                );
                summary.is_draft = true;
                let mut stored = StoredMessage::new(summary);
                stored.uid = Some(7);
                stored.uidvalidity = Some(1);
                stored.gm_msgid = Some("m-gone".into());
                repo::messages::upsert(db, &stored)?;
                Ok(())
            })
            .await
            .expect("seed projection and mappings");
    }
    let op_id = {
        let payload = ops::DeleteRemoteDraftOp {
            folder_id: folder_id.clone(),
            uid: 7,
            remote_message_id: None,
            draft_id: Some("gone-draft".into()),
            source_uidvalidity: Some(1),
            message_id: Some(projection_id.clone()),
            outcome_unknown: false,
            quarantined: false,
            quarantine_reason: None,
        };
        let value = serde_json::to_value(payload).expect("json");
        let op = nuncio_core::models::NewPendingOp::new(
            "acc-a",
            ops::KIND_DELETE_REMOTE_DRAFT,
            value,
            "Remove remote draft",
        );
        ctx.db
            .write(move |db| repo::pending_ops::enqueue(db, op))
            .await
            .expect("queue delete")
            .id
    };

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("graph pass");
    assert_eq!(outcome.done, 1, "a 404 is an idempotent success");
    assert_eq!(outcome.failed, 0);
    assert_eq!(mock.deleted_ids(), vec!["m-gone".to_string()]);

    let projection = {
        let id = projection_id.clone();
        ctx.db
            .read(move |db| repo::messages::get(db, &id))
            .await
            .expect("read projection")
    };
    assert!(
        projection.is_none(),
        "the 404 receipt still proves this projection is gone"
    );
    for key in ["graph_remote:a-drafts:7", "graph_remote:a-archive:7"] {
        let mapping = {
            let key = key.to_string();
            ctx.db
                .read(move |db| repo::sync_state::get(db, "acc-a", "mail", &key))
                .await
                .expect("read mapping")
        };
        assert_eq!(
            mapping, None,
            "every graph_remote mapping to m-gone is retired"
        );
    }
    for key in [
        "graph_remote:a-drafts:8",
        "anchor:a-drafts",
        "graph_folder:Drafts",
    ] {
        let mapping = {
            let key = key.to_string();
            ctx.db
                .read(move |db| repo::sync_state::get(db, "acc-a", "mail", &key))
                .await
                .expect("read unrelated entry")
        };
        assert!(
            mapping.is_some(),
            "unrelated sync_state key {key} must survive"
        );
    }
    assert_eq!(
        pending_op_state(&ctx, &op_id).await,
        nuncio_core::models::PendingOpState::Done
    );
}

/// Mapping cleanup must not depend on finding a local projection row: the
/// receipt names a Graph id, and a leftover mapping would let a later queued
/// operation resolve an id the server already confirmed as gone.
#[tokio::test]
async fn graph_delete_without_a_projection_still_retires_mappings() {
    let mock = MockGraph::start_with_missing_remote(0).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc-a", "a@example.com", None, None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;

    let folder_id = "a-drafts".to_string();
    {
        let folder_id = folder_id.clone();
        ctx.db
            .write(move |db| {
                repo::folders::insert(
                    db,
                    &Folder::new(&folder_id, "acc-a", "Drafts", FolderRole::Drafts),
                )?;
                repo::sync_state::set(db, "acc-a", "mail", "graph_remote:a-drafts:7", "m-gone")?;
                Ok(())
            })
            .await
            .expect("seed mapping only");
    }
    let op_id = {
        // No local row projects this draft; only the mapping identifies it.
        let payload = ops::DeleteRemoteDraftOp {
            folder_id: folder_id.clone(),
            uid: 7,
            remote_message_id: None,
            draft_id: None,
            source_uidvalidity: None,
            message_id: None,
            outcome_unknown: false,
            quarantined: false,
            quarantine_reason: None,
        };
        let value = serde_json::to_value(payload).expect("json");
        let op = nuncio_core::models::NewPendingOp::new(
            "acc-a",
            ops::KIND_DELETE_REMOTE_DRAFT,
            value,
            "Remove remote draft",
        );
        ctx.db
            .write(move |db| repo::pending_ops::enqueue(db, op))
            .await
            .expect("queue delete")
            .id
    };

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("graph pass");
    assert_eq!(outcome.done, 1);
    assert_eq!(mock.deleted_ids(), vec!["m-gone".to_string()]);

    let mapping = ctx
        .db
        .read(|db| repo::sync_state::get(db, "acc-a", "mail", "graph_remote:a-drafts:7"))
        .await
        .expect("read mapping");
    assert_eq!(
        mapping, None,
        "the retired mapping is dropped even without a projection row"
    );
    assert_eq!(
        pending_op_state(&ctx, &op_id).await,
        nuncio_core::models::PendingOpState::Done
    );
}

/// A receipt from the old account must not be pinned to a draft that moved to
/// another account between the receipt and the identity write. The write is
/// refused and the draft's current Drafts folder is queued for a resync.
#[tokio::test]
async fn receipt_after_account_switch_cannot_pin_the_old_identity() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let (ctx, account_a) = drafts_account(&mock).await;
    let service = MailService::new(ctx.clone());

    // A second account with its own Drafts folder: that is where the switched
    // draft (and the resync) belongs.
    let account_b = account("acc-b", "b@example.com", None, None, None);
    insert_account(&ctx, &account_b).await;
    let drafts_b = Folder::new("b-drafts", "acc-b", "Drafts", FolderRole::Drafts);
    {
        let drafts_b = drafts_b.clone();
        ctx.db
            .write(move |db| repo::folders::insert(db, &drafts_b))
            .await
            .expect("insert B drafts");
    }
    // Give B's Drafts a sync marker so the forced resync is observable.
    ctx.db
        .write(|db| repo::folders::set_sync_flags(db, "b-drafts", Some(1), Some(77), None))
        .await
        .expect("seed B sync flags");

    let mut draft = service
        .compose_create(Some(account_a.id.as_str()))
        .await
        .expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Receipt race".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");

    // The switch commits immediately after the APPENDUID receipt is recorded,
    // i.e. after the receipt exists durably but before the identity is pinned.
    ctx.db
        .write(|db| {
            db.execute(
                "CREATE TRIGGER switch_draft_mid_receipt
                   AFTER UPDATE OF payload_json ON pending_op
                   WHEN NEW.kind = 'appendDraft'
                     AND json_extract(NEW.payload_json, '$.appendedUid') IS NOT NULL
                 BEGIN
                   UPDATE draft
                      SET account_id = 'acc-b', remote_draft_json = NULL
                    WHERE id = json_extract(NEW.payload_json, '$.draftId');
                 END",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("install switch trigger");

    let mut connection = connect(&account_a).await;
    ops::executor::run_due(&ctx, &mut connection, &account_a)
        .await
        .expect("append draft");

    let row = ctx
        .db
        .read({
            let draft_id = draft_id.clone();
            move |db| repo::drafts::get_required(db, &draft_id)
        })
        .await
        .expect("draft row");
    assert_eq!(row.account_id, "acc-b", "the draft moved accounts");
    assert!(
        row.remote_draft.is_none(),
        "an old-account receipt must not be pinned to the switched draft: {:?}",
        row.remote_draft
    );
    let (_, uidnext, _) = ctx
        .db
        .read(|db| repo::folders::sync_flags(db, "b-drafts"))
        .await
        .expect("B sync flags");
    assert!(
        uidnext.is_none(),
        "an untrusted identity queues a full resync of the draft's current folder"
    );
    assert_eq!(mock.state.message_count("Drafts"), 1);
}

/// A refused receipt for an account that has no Drafts folder yet cannot queue
/// a folder resync directly. The request is kept as a sync-state marker and
/// consumed by the first folder reconciliation that sees a Drafts folder.
#[tokio::test]
async fn receipt_after_switch_without_drafts_folder_defers_the_resync() {
    let mock_a = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock_a.state.set_append_uid_plus(true);
    let mock_b = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    let (ctx, account_a) = drafts_account(&mock_a).await;
    let account_b = account(
        "acc-b",
        "b@example.com",
        Some(server(mock_b.addr.port())),
        None,
        None,
    );
    insert_account(&ctx, &account_b).await;
    // B has no local Drafts folder yet: that is the deferred case.
    let service = MailService::new(ctx.clone());

    let mut draft = service
        .compose_create(Some(account_a.id.as_str()))
        .await
        .expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Deferred resync".into();
    draft.html = "<p>body</p>".into();
    service.compose_save(&draft).await.expect("save draft");

    ctx.db
        .write(|db| {
            db.execute(
                "CREATE TRIGGER switch_draft_mid_receipt
                   AFTER UPDATE OF payload_json ON pending_op
                   WHEN NEW.kind = 'appendDraft'
                     AND json_extract(NEW.payload_json, '$.appendedUid') IS NOT NULL
                 BEGIN
                   UPDATE draft
                      SET account_id = 'acc-b', remote_draft_json = NULL
                    WHERE id = json_extract(NEW.payload_json, '$.draftId');
                 END",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("install switch trigger");

    let mut connection = connect(&account_a).await;
    ops::executor::run_due(&ctx, &mut connection, &account_a)
        .await
        .expect("append draft");

    let marker_key = nuncio_mail::sync::folders::DRAFTS_RESYNC_KEY;
    let marker = ctx
        .db
        .read(move |db| repo::sync_state::get(db, "acc-b", "mail", marker_key))
        .await
        .expect("read resync marker");
    assert!(
        marker.is_some(),
        "an unresolvable Drafts folder records the resync for the next folder sync"
    );

    let mut connection_b = connect(&account_b).await;
    nuncio_mail::sync::folders::sync(&ctx, &mut connection_b, "acc-b")
        .await
        .expect("B folder sync creates Drafts");

    let marker_key = nuncio_mail::sync::folders::DRAFTS_RESYNC_KEY;
    let marker = ctx
        .db
        .read(move |db| repo::sync_state::get(db, "acc-b", "mail", marker_key))
        .await
        .expect("read resync marker");
    assert!(
        marker.is_none(),
        "the first folder sync that sees Drafts consumes the deferred resync"
    );
    let drafts = ctx
        .db
        .read(|db| repo::folders::by_role(db, "acc-b", FolderRole::Drafts))
        .await
        .expect("B drafts folder")
        .expect("B now has a Drafts folder");
    assert_eq!(drafts.remote_path, "Drafts");
}

/// An APPEND that completes between the send's pre-transaction read and the
/// outbox commit must be cleaned up by its *new* UID, not the stale plan's old
/// UID (which would leave the new copy behind and delete the wrong one).
#[tokio::test]
async fn send_uses_the_attribution_visible_at_commit() {
    let mock = MockImap::start_with_folders(
        &["CONDSTORE", "UIDPLUS"],
        &[("INBOX", &["\\Inbox"]), ("Drafts", &["\\Drafts"])],
    )
    .await;
    mock.state.set_append_uid_plus(true);
    let smtp = MockSmtp::start().await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = account(
        "acc",
        "test@example.com",
        Some(server(mock.addr.port())),
        Some(server(smtp.addr.port())),
        None,
    );
    insert_account(&ctx, &account).await;
    sync_once(&ctx, &account).await;
    let drafts_folder = folder_id(&ctx, "acc", "Drafts").await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "First".into();
    draft.html = "<p>first</p>".into();
    let draft = service.compose_save(&draft).await.expect("save");
    let draft_id = draft.id.clone().expect("draft id");
    let mut connection = connect(&account).await;
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("first append");
    nuncio_mail::sync::sync_folder_by_id(&ctx, &mut connection, &account, &drafts_folder)
        .await
        .expect("drafts sync");
    assert_eq!(mock.state.message_count("Drafts"), 1);

    let mut draft = service.compose_get_draft(&draft_id).await.expect("reload");
    draft.subject = "Second".into();
    draft.html = "<p>second</p>".into();
    service.compose_save(&draft).await.expect("second save");

    // The second APPEND completes between the service's reads and the outbox
    // transaction: the server now holds UID 2, and the operation settles as
    // done before the transaction recomputes its plan.
    mock.state.add_message(
        "Drafts",
        &message_raw(
            "test@example.com",
            "bob@example.com",
            "Second",
            "second",
            &nuncio_mail::util::draft_message_id(&draft_id),
        ),
    );
    let trigger = format!(
        "CREATE TRIGGER simulate_completed_append
           AFTER UPDATE OF sync_suppressed ON draft
           WHEN NEW.id = '{draft_id}' AND NEW.sync_suppressed = 1
         BEGIN
           UPDATE pending_op
              SET state = 'done',
                  payload_json = json_set(payload_json,
                      '$.appendState', 'acked',
                      '$.appendedUid', 2,
                      '$.appendedUidvalidity', 1)
            WHERE kind = 'appendDraft'
              AND json_extract(payload_json, '$.draftId') = '{draft_id}'
              AND state IN ('pending', 'inProgress');
         END"
    );
    ctx.db
        .write(move |db| {
            db.execute(&trigger, [])?;
            Ok(())
        })
        .await
        .expect("install append-completion trigger");

    let send_draft = service.compose_get_draft(&draft_id).await.expect("draft");
    service.compose_send(&send_draft).await.expect("send");
    let mut deletes = delete_ops_for(&ctx, "acc", &draft_id).await;
    assert_eq!(deletes.len(), 1, "the completed APPEND is cleaned up");
    assert_eq!(
        deletes.remove(0).payload.uid,
        2,
        "the plan must name the UID that completed at commit, not the stale one"
    );

    let before = mock.state.commands().len();
    ops::executor::run_due(&ctx, &mut connection, &account)
        .await
        .expect("cleanup");
    let commands: Vec<String> = mock.state.commands().into_iter().skip(before).collect();
    assert!(
        commands.iter().any(|command| {
            let command = command.trim().to_ascii_uppercase();
            command.starts_with("UID EXPUNGE 2") || command == "UID EXPUNGE 2"
        }),
        "the newly completed copy is the one removed: {commands:?}"
    );
    assert!(
        !commands.iter().any(|command| {
            let command = command.trim().to_ascii_uppercase();
            command.starts_with("UID EXPUNGE 1") || command == "UID EXPUNGE 1"
        }),
        "the stale UID must not be deleted again: {commands:?}"
    );
    assert_eq!(
        mock.state.message_count("Drafts"),
        1,
        "the old copy remains; only the completed one is removed"
    );
}

/* ─────────── deletion evidence: unreachable id vs. delete receipt ─────────── */

/// Seed a Graph message's provider id and the Inbox folder mapping so a
/// targeted GET can report it as still in the expected folder.
async fn seed_graph_identity(
    ctx: &CoreContext,
    id: &str,
    inbox: &str,
    remote_id: &str,
    folder: &str,
) {
    let id = id.to_string();
    let inbox = inbox.to_string();
    let remote_id = remote_id.to_string();
    let folder = folder.to_string();
    ctx.db
        .write(move |db| {
            let mut stored = repo::messages::get_required(db, &id)?;
            stored.gm_msgid = Some(remote_id);
            repo::messages::upsert(db, &stored)?;
            repo::sync_state::set(db, "acc", "mail", &format!("graph_folder:{folder}"), &inbox)
        })
        .await
        .expect("seed graph identity");
}

async fn graph_single_op(ctx: &CoreContext) -> nuncio_core::models::PendingOp {
    ctx.db
        .read(|db| repo::pending_ops::list(db, Some("acc")))
        .await
        .expect("ops")
        .into_iter()
        .next()
        .expect("one queued operation")
}

async fn graph_make_due(ctx: &CoreContext) {
    ctx.db
        .write(|db| {
            db.execute("UPDATE pending_op SET next_retry_at = NULL", [])?;
            Ok(())
        })
        .await
        .expect("make retries due");
}

async fn graph_force_terminal(ctx: &CoreContext) {
    ctx.db
        .write(|db| {
            db.execute(
                "UPDATE pending_op SET attempts = 8, next_retry_at = NULL",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("exhaust retries");
}

/// A targeted GET answering 404 is not a delete receipt: the id may have
/// moved. A retry must keep the outcome unknown, and the terminal recovery
/// must not erase the local row either.
#[tokio::test]
async fn graph_delete_verification_404_is_not_a_deletion_receipt() {
    let mock = MockGraph::start(1).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    let inbox = graph_inbox(&ctx).await;
    insert_stored(&ctx, "acc", "m1", &inbox, false).await;
    seed_graph_identity(&ctx, "m1", "inbox-remote", "m1", "Inbox").await;
    mock.set_parent_folder_response(Some("inbox-remote"));
    mock.fail_permanent_delete_with_status_for("m1", 404);
    let service = MailService::new(ctx.clone());

    service
        .message_act(
            &["m1".to_string()],
            &MessageAction::Delete { permanent: true },
        )
        .await
        .expect("delete");

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("first pass");
    assert_eq!(outcome.retried, 1, "a 404 stays unverified");
    let op = graph_single_op(&ctx).await;
    let payload = repo::pending_ops::payload::<ops::DeleteOp>(&op).expect("payload");
    assert_eq!(payload.outcomes[0].state, ops::OpMessageState::Unknown);
    assert_eq!(
        payload.outcomes[0].attempt_remote_id.as_deref(),
        Some("m1"),
        "the id actually sent is persisted with the unknown outcome"
    );

    // The old id is now unreachable without any recorded delete request (a
    // move would do the same). A GET 404 must not settle the operation.
    mock.remove_message("m1");
    graph_make_due(&ctx).await;
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("verification pass");
    assert_eq!(outcome.retried, 1);
    let op = graph_single_op(&ctx).await;
    let payload = repo::pending_ops::payload::<ops::DeleteOp>(&op).expect("payload");
    assert_eq!(
        payload.outcomes[0].state,
        ops::OpMessageState::Unknown,
        "an unreachable id must not be recorded as a deletion"
    );
    assert_eq!(
        mock.deleted_ids(),
        vec!["m1".to_string()],
        "the 404 verification must not resend the delete"
    );

    // Terminal recovery sees the same 404 and must keep the row hidden.
    graph_force_terminal(&ctx).await;
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("terminal pass");
    assert_eq!(outcome.failed, 1);
    assert!(
        ctx.db
            .read(|db| repo::messages::get(db, "m1"))
            .await
            .expect("read row")
            .is_some(),
        "a 404 only proves the id is unreachable, not that the message was deleted"
    );
    let hidden = ctx
        .db
        .read({
            let inbox = inbox.clone();
            move |db| repo::messages::pending_delete_ids_in_folder(db, &inbox)
        })
        .await
        .expect("pending deletes");
    assert!(hidden.contains(&"m1".to_string()));
}

/// An uncertain Graph draft cleanup whose targeted GET answers 404 must not be
/// recorded as a provider receipt or drop the projection.
#[tokio::test]
async fn graph_draft_cleanup_get_404_stays_unknown() {
    let mock = MockGraph::start_with_create_gate(0).await;
    mock.set_permanent_delete_failure(true);
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let graph = graph_draft_account(&ctx).await;
    seed_graph_drafts_mapping(&ctx).await;
    let service = MailService::new(ctx.clone());
    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Graph cleanup 404".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let projection_id = nuncio_mail::util::draft_message_id(&draft_id);
    ctx.db
        .write({
            let id = projection_id.clone();
            move |db| repo::messages::set_uid(db, &id, Some(7))
        })
        .await
        .expect("bind projection");

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let worker = {
        let ctx = ctx.clone();
        let account = graph.clone();
        let client = client.clone();
        tokio::spawn(
            async move { ops::executor::run_due_graph_with(&ctx, &account, &client).await },
        )
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), mock.create_received.acquire())
        .await
        .expect("the create must reach the mock")
        .expect("permit");
    service.compose_discard(&draft_id).await.expect("discard");
    mock.release_create.add_permits(1);
    let outcome = worker.await.expect("worker task").expect("graph pass");
    assert_eq!(outcome.retried, 1);
    let created = mock.created_ids();
    assert_eq!(created.len(), 1);

    // The old id disappears without a recorded delete; the GET answers 404.
    mock.set_permanent_delete_failure(false);
    mock.remove_message(&created[0]);
    graph_make_due(&ctx).await;
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("verification pass");
    assert_eq!(outcome.done, 0, "a 404 must not settle the cleanup");
    assert!(outcome.deferred >= 1);
    let pending = append_op_for(&ctx, "acc", &draft_id)
        .await
        .expect("append op");
    assert!(!pending.payload.cleanup_acked, "a 404 is not a receipt");
    assert!(!pending.payload.cleanup_done);
    assert!(pending.payload.outcome_unknown);
    assert_eq!(
        mock.deleted_ids(),
        created,
        "the verification must not send another delete"
    );
    assert!(
        ctx.db
            .read({
                let id = projection_id.clone();
                move |db| repo::messages::get(db, &id)
            })
            .await
            .expect("read projection")
            .is_some(),
        "the projection must survive an unreachable id"
    );
}

/// A Graph draft cleanup may only delete a copy that a targeted GET confirms
/// is still a draft in the expected Drafts folder. This applies to the first
/// attempt and to every retry.
#[tokio::test]
async fn graph_draft_cleanup_requires_a_draft_in_the_expected_folder() {
    let mock = MockGraph::start_with_create_gate(0).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let graph = graph_draft_account(&ctx).await;
    seed_graph_drafts_mapping(&ctx).await;
    let service = MailService::new(ctx.clone());
    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Graph not a draft".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let projection_id = nuncio_mail::util::draft_message_id(&draft_id);
    ctx.db
        .write({
            let id = projection_id.clone();
            move |db| repo::messages::set_uid(db, &id, Some(7))
        })
        .await
        .expect("bind projection");

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let worker = {
        let ctx = ctx.clone();
        let account = graph.clone();
        let client = client.clone();
        tokio::spawn(
            async move { ops::executor::run_due_graph_with(&ctx, &account, &client).await },
        )
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), mock.create_received.acquire())
        .await
        .expect("the create must reach the mock")
        .expect("permit");
    // First attempt: the copy is no longer a draft.
    mock.set_is_draft_response(Some(false));
    service.compose_discard(&draft_id).await.expect("discard");
    mock.release_create.add_permits(1);
    let outcome = worker.await.expect("worker task").expect("graph pass");
    assert_eq!(outcome.done, 0, "a non-draft must not be hard-deleted");
    assert!(outcome.deferred >= 1);
    assert!(mock.deleted_ids().is_empty(), "no delete may be sent");

    // Second attempt: still the wrong folder.
    mock.set_is_draft_response(Some(true));
    mock.set_parent_folder_response(Some("other-folder"));
    graph_make_due(&ctx).await;
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("folder verification pass");
    assert_eq!(outcome.done, 0);
    assert!(
        mock.deleted_ids().is_empty(),
        "a copy outside the expected Drafts folder must not be deleted"
    );
    let pending = append_op_for(&ctx, "acc", &draft_id)
        .await
        .expect("append op");
    assert!(!pending.payload.cleanup_claimed);
    assert!(!pending.payload.cleanup_done);
}

/// The retry verification path must compare the folder too; `InFolder(_)` is
/// not enough.
#[tokio::test]
async fn graph_draft_cleanup_retry_compares_the_expected_folder() {
    let mock = MockGraph::start_with_create_gate(0).await;
    mock.set_permanent_delete_failure(true);
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let graph = graph_draft_account(&ctx).await;
    seed_graph_drafts_mapping(&ctx).await;
    let service = MailService::new(ctx.clone());
    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Graph retry folder".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    ctx.db
        .write({
            let id = nuncio_mail::util::draft_message_id(&draft_id);
            move |db| repo::messages::set_uid(db, &id, Some(7))
        })
        .await
        .expect("bind projection");

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let worker = {
        let ctx = ctx.clone();
        let account = graph.clone();
        let client = client.clone();
        tokio::spawn(
            async move { ops::executor::run_due_graph_with(&ctx, &account, &client).await },
        )
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), mock.create_received.acquire())
        .await
        .expect("the create must reach the mock")
        .expect("permit");
    service.compose_discard(&draft_id).await.expect("discard");
    mock.release_create.add_permits(1);
    let outcome = worker.await.expect("worker task").expect("graph pass");
    assert_eq!(outcome.retried, 1);
    let deletes = mock.deleted_ids().len();
    assert_eq!(deletes, 1);

    // The retry sees the id in another folder: not evidence about this draft.
    mock.set_permanent_delete_failure(false);
    mock.set_parent_folder_response(Some("other-folder"));
    graph_make_due(&ctx).await;
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("verification pass");
    assert_eq!(outcome.done, 0);
    assert_eq!(
        mock.deleted_ids().len(),
        deletes,
        "a retry must not delete a copy outside the expected Drafts folder"
    );
    let pending = append_op_for(&ctx, "acc", &draft_id)
        .await
        .expect("append op");
    assert!(pending.payload.outcome_unknown);
    assert!(!pending.payload.cleanup_acked);
}

/// A retry verifies the id a previous attempt actually sent. A mapping that a
/// sync rebound (or removed) must never redirect it to another message.
#[tokio::test]
async fn graph_delete_retry_verifies_the_id_it_actually_sent() {
    let mock = MockGraph::start(2).await; // m1 and m2 both exist
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    let inbox = graph_inbox(&ctx).await;
    insert_stored(&ctx, "acc", "m1", &inbox, false).await;
    let stored = ctx
        .db
        .read(|db| repo::messages::get_required(db, "m1"))
        .await
        .expect("stored");
    let uid = stored.uid.expect("uid");
    seed_graph_identity(&ctx, "m1", "inbox-remote", "m1", "Inbox").await;
    ctx.db
        .write({
            let inbox = inbox.clone();
            move |db| {
                repo::sync_state::set(
                    db,
                    "acc",
                    "mail",
                    &format!("graph_remote:{inbox}:{uid}"),
                    "m1",
                )
            }
        })
        .await
        .expect("seed remote mapping");
    mock.set_parent_folder_response(Some("inbox-remote"));
    let service = MailService::new(ctx.clone());
    service
        .message_act(
            &["m1".to_string()],
            &MessageAction::Delete { permanent: true },
        )
        .await
        .expect("delete");

    mock.set_permanent_delete_failure(true);
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("first pass");
    assert_eq!(outcome.retried, 1);

    // A sync rebinds the row and its mapping to the other message. The retry
    // must still verify and address the id it sent.
    ctx.db
        .write({
            let inbox = inbox.clone();
            move |db| {
                let mut stored = repo::messages::get_required(db, "m1")?;
                stored.gm_msgid = Some("m2".into());
                repo::messages::upsert(db, &stored)?;
                repo::sync_state::set(
                    db,
                    "acc",
                    "mail",
                    &format!("graph_remote:{inbox}:{uid}"),
                    "m2",
                )
            }
        })
        .await
        .expect("rebind identity");
    mock.set_permanent_delete_failure(false);
    graph_make_due(&ctx).await;
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("retry pass");
    assert_eq!(outcome.done, 1);
    assert_eq!(
        mock.deleted_ids(),
        vec!["m1".to_string(), "m1".to_string()],
        "the retry must address the originally sent id, never the rebound one"
    );
    assert!(
        mock.ids().contains(&"m2".to_string()),
        "the rebound message must be untouched"
    );
}

/// A missing mapping is not evidence that the earlier send did not happen.
#[tokio::test]
async fn graph_delete_retry_survives_a_missing_mapping() {
    let mock = MockGraph::start(1).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    let inbox = graph_inbox(&ctx).await;
    insert_stored(&ctx, "acc", "m1", &inbox, false).await;
    seed_graph_identity(&ctx, "m1", "inbox-remote", "m1", "Inbox").await;
    mock.set_parent_folder_response(Some("inbox-remote"));
    let service = MailService::new(ctx.clone());
    service
        .message_act(
            &["m1".to_string()],
            &MessageAction::Delete { permanent: true },
        )
        .await
        .expect("delete");

    mock.set_permanent_delete_failure(true);
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("first pass");

    // Lose every mutable identity except the attempt recorded on the outcome.
    ctx.db
        .write({
            let inbox = inbox.clone();
            move |db| {
                let mut stored = repo::messages::get_required(db, "m1")?;
                stored.gm_msgid = None;
                stored.uid = None;
                repo::messages::upsert(db, &stored)?;
                db.execute(
                    "DELETE FROM sync_state WHERE account_id = 'acc' AND module = 'mail'
                       AND key GLOB 'graph_remote:*'",
                    [],
                )?;
                let _ = inbox;
                Ok(())
            }
        })
        .await
        .expect("clear identities");
    mock.set_permanent_delete_failure(false);
    graph_make_due(&ctx).await;
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("retry pass");
    assert_eq!(outcome.done, 1, "the persisted attempt id still settles");
    assert_eq!(
        mock.deleted_ids(),
        vec!["m1".to_string(), "m1".to_string()],
        "the missing mapping must not cancel the earlier request"
    );
}

/// A GET that still sees the message cannot clear an earlier unknown outcome:
/// the first request may still land. A retry refused in the meantime stays
/// unknown, and the row stays hidden.
#[tokio::test]
async fn graph_present_verification_keeps_an_unknown_delete_unknown() {
    let mock = MockGraph::start(1).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    let inbox = graph_inbox(&ctx).await;
    insert_stored(&ctx, "acc", "m1", &inbox, false).await;
    seed_graph_identity(&ctx, "m1", "inbox-remote", "m1", "Inbox").await;
    mock.set_parent_folder_response(Some("inbox-remote"));
    let service = MailService::new(ctx.clone());
    service
        .message_act(
            &["m1".to_string()],
            &MessageAction::Delete { permanent: true },
        )
        .await
        .expect("delete");

    // First attempt: ambiguous 500, outcome unknown.
    mock.set_permanent_delete_failure(true);
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("first pass");
    // Retry: the GET still sees the message, then the provider refuses this
    // attempt. The earlier unknown request may still apply, so it stays
    // unknown instead of becoming not-executed.
    mock.set_permanent_delete_failure(false);
    mock.fail_permanent_delete_with_status_for("m1", 400);
    graph_make_due(&ctx).await;
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("refused retry");
    assert_eq!(outcome.retried, 1);
    let op = graph_single_op(&ctx).await;
    let payload = repo::pending_ops::payload::<ops::DeleteOp>(&op).expect("payload");
    assert_eq!(
        payload.outcomes[0].state,
        ops::OpMessageState::Unknown,
        "a refusal for the retry cannot retract the earlier unknown request"
    );
    let hidden = ctx
        .db
        .read({
            let inbox = inbox.clone();
            move |db| repo::messages::pending_delete_ids_in_folder(db, &inbox)
        })
        .await
        .expect("pending deletes");
    assert!(
        hidden.contains(&"m1".to_string()),
        "the row must stay hidden and protected"
    );

    // The first request lands late; terminal recovery must keep the row hidden.
    mock.remove_message("m1");
    graph_force_terminal(&ctx).await;
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("terminal pass");
    assert_eq!(outcome.failed, 1);
    assert!(ctx
        .db
        .read(|db| repo::messages::get(db, "m1"))
        .await
        .expect("read row")
        .is_some());
    let hidden = ctx
        .db
        .read({
            let inbox = inbox.clone();
            move |db| repo::messages::pending_delete_ids_in_folder(db, &inbox)
        })
        .await
        .expect("pending deletes");
    assert!(hidden.contains(&"m1".to_string()));
}

/// A Graph move whose per-message outcome is unknown is never re-POSTed, even
/// on the operation's own automatic retry.
#[tokio::test]
async fn graph_move_unknown_outcome_is_not_resent() {
    let mock = MockGraph::start(8).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let mut graph = account("acc", "graph@example.com", Some(server(9)), None, None);
    graph.provider = ProviderKind::Outlook;
    graph.oauth_provider = Some(OAuthProvider::Microsoft);
    insert_account(&ctx, &graph).await;
    let inbox = graph_inbox(&ctx).await;
    let archive = Folder::new("graph-archive", "acc", "Archive", FolderRole::Archive);
    let archive_id = archive.id.clone();
    {
        let archive = archive.clone();
        ctx.db
            .write(move |db| repo::folders::insert(db, &archive))
            .await
            .expect("insert archive");
    }
    insert_stored(&ctx, "acc", "m1", &inbox, false).await;
    ctx.db
        .write(move |db| {
            repo::sync_state::set(db, "acc", "mail", "graph_folder:Inbox", "inbox-remote")?;
            repo::sync_state::set(db, "acc", "mail", "graph_folder:Archive", "archive-remote")
        })
        .await
        .expect("seed folder mappings");

    let payload = ops::MoveOp {
        folder_id: inbox.clone(),
        target_folder_id: archive_id.clone(),
        uids: vec![1],
        message_ids: vec!["m1".into()],
        source_uidvalidity: None,
        outcomes: vec![ops::OpMessageOutcome {
            message_id: "m1".into(),
            source_uid: Some(1),
            state: ops::OpMessageState::Unknown,
            target_folder_id: None,
            target_uid: None,
            target_uidvalidity: None,
            target_remote_id: None,
            attempt_remote_id: None,
            copy_phase: Default::default(),
            delete_flag_phase: Default::default(),
            expunge_phase: Default::default(),
        }],
        remote_ids: vec!["m1".into()],
    };
    ops::enqueue_operation(
        &ctx,
        "acc",
        ops::KIND_MOVE,
        serde_json::to_value(payload).expect("json"),
        "Move message",
    )
    .await
    .expect("enqueue move");

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome = ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("graph pass");
    assert_eq!(outcome.retried, 1);
    assert!(
        mock.requests()
            .iter()
            .all(|request| !request.contains("/move")),
        "an unknown move must not be sent again: {:?}",
        mock.requests()
    );
    let op = graph_single_op(&ctx).await;
    let payload = repo::pending_ops::payload::<ops::MoveOp>(&op).expect("payload");
    assert_eq!(payload.outcomes[0].state, ops::OpMessageState::Unknown);
}

/// A locally edited draft whose projection row a sync rebound to an external
/// same-header copy must not turn that external copy into a delete target.
#[tokio::test]
async fn graph_rebound_local_projection_never_authorizes_a_delete() {
    let mock = MockGraph::start(0).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let graph = graph_draft_account(&ctx).await;
    let service = MailService::new(ctx.clone());
    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Rebound projection".into();
    draft.html = "<p>body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let projection_id = nuncio_mail::util::draft_message_id(&draft_id);
    // The first APPEND is not the subject here; let a later save queue freely.
    let append_id = {
        let draft_id = draft_id.clone();
        ctx.db
            .read(move |db| {
                Ok(repo::pending_ops::list(db, Some("acc"))?
                    .into_iter()
                    .find(|op| {
                        op.kind == ops::KIND_APPEND_DRAFT
                            && repo::pending_ops::payload::<ops::AppendDraftOp>(op)
                                .map(|payload| payload.draft_id == draft_id)
                                .unwrap_or(false)
                    })
                    .map(|op| op.id))
            })
            .await
            .expect("append op")
            .expect("append id")
    };
    ctx.db
        .write(move |db| repo::pending_ops::mark_done(db, &append_id))
        .await
        .expect("settle the append");

    // A Drafts sync binds the row id to an external copy's Graph id.
    ctx.db
        .write({
            let id = projection_id.clone();
            move |db| {
                let mut stored = repo::messages::get_required(db, &id)?;
                stored.gm_msgid = Some("external-copy".into());
                stored.uid = Some(900);
                repo::messages::upsert(db, &stored)
            }
        })
        .await
        .expect("rebind projection");

    let opened = service
        .compose_open_draft(&projection_id)
        .await
        .expect("open draft");
    assert!(
        opened.remote_draft_message_id.is_none(),
        "opening a local projection must not record its mutable row id"
    );
    let mut edited = opened;
    edited.subject = "Rebound projection edited".into();
    service.compose_save(&edited).await.expect("save edit");
    assert!(
        delete_ops_for(&ctx, "acc", &draft_id).await.is_empty(),
        "a rebound projection must not authorize deleting the external copy"
    );

    service.compose_discard(&draft_id).await.expect("discard");
    assert!(
        delete_ops_for(&ctx, "acc", &draft_id).await.is_empty(),
        "discarding must not delete the external copy either"
    );
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    ops::executor::run_due_graph_with(&ctx, &graph, &client)
        .await
        .expect("graph pass");
    assert!(mock.deleted_ids().is_empty());
}

/// The locally generated `draft.eml` is never evidence about a server copy.
///
/// A sync can rebind the projection to an external same-header draft while
/// `body_path` still points at our bytes. An autosave after that rebind must
/// not stamp the external copy's identity on those bytes: if the local draft is
/// then discarded (its projection and staged file surviving the cleanup
/// window), reopening the projection would otherwise read local content and
/// still grant the external copy's `Explicit` delete right.
#[tokio::test]
async fn local_staged_body_never_authorizes_the_rebound_copy() {
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let _graph = graph_draft_account(&ctx).await;
    let service = MailService::new(ctx.clone());

    let mut draft = service.compose_create(Some("acc")).await.expect("draft");
    draft.to = vec![Address::new("bob@example.com")];
    draft.subject = "Local bytes".into();
    draft.html = "<p>local body</p>".into();
    let draft = service.compose_save(&draft).await.expect("save draft");
    let draft_id = draft.id.clone().expect("draft id");
    let projection_id = nuncio_mail::util::draft_message_id(&draft_id);

    // A Drafts sync rebinds the projection row to an external copy.
    ctx.db
        .write({
            let id = projection_id.clone();
            move |db| {
                let mut stored = repo::messages::get_required(db, &id)?;
                stored.gm_msgid = Some("external-copy".into());
                stored.uid = Some(900);
                repo::messages::upsert(db, &stored)
            }
        })
        .await
        .expect("rebind projection");

    // Autosave now rewrites the local draft (and its projection file).
    let mut edited = service
        .compose_get_draft(&draft_id)
        .await
        .expect("reload draft");
    edited.subject = "Local bytes edited".into();
    service.compose_save(&edited).await.expect("save again");

    let projection = ctx
        .db
        .read({
            let id = projection_id.clone();
            move |db| repo::messages::get_required(db, &id)
        })
        .await
        .expect("projection row");
    assert_eq!(
        projection.gm_msgid.as_deref(),
        Some("external-copy"),
        "the row still names the external copy"
    );
    assert_eq!(
        projection.body_identity, None,
        "the local staged bytes must not be attributed to the external copy"
    );
    let body_path = projection.body_path.clone().expect("staged body path");
    assert!(
        std::path::Path::new(&body_path).exists(),
        "the staged file outlives the committed discard in this window"
    );

    // Discard commits (the local draft row is gone) but the projection and its
    // staged file are still there, as they are until cleanup runs.
    ctx.db
        .write({
            let id = draft_id.clone();
            move |db| repo::drafts::delete(db, &id)
        })
        .await
        .expect("delete local draft");
    // Simulate a row written by a pre-fix build: the provider identity was
    // stamped on the local file. The staging path alone must still refuse it.
    ctx.db
        .write({
            let id = projection_id.clone();
            move |db| {
                db.execute(
                    "UPDATE message SET body_identity = 'graph:external-copy' WHERE id = ?1",
                    rusqlite::params![id],
                )?;
                Ok(())
            }
        })
        .await
        .expect("simulate a stale identity");

    let opened = service
        .compose_open_draft(&projection_id)
        .await
        .expect("open retained projection");
    assert!(
        opened.remote_draft_message_id.is_none(),
        "local bytes cannot prove they are the external copy"
    );
    assert_eq!(
        opened
            .remote_draft
            .as_ref()
            .expect("remote identity record")
            .attribution,
        nuncio_core::models::RemoteDraftAttribution::Candidate,
        "a local body may only downgrade the copy to a Candidate"
    );
}

/* ───────────────────── Graph delta cursor correctness ───────────────────── */

/// Graph caps a `$filter`ed `messages/delta` query at 5,000 messages, and the
/// truncated round can still end with a normal-looking `@odata.deltaLink`.
/// The sync must therefore never filter delta requests server-side: it pages
/// through the whole folder and applies every change before persisting the
/// final deltaLink as the next cursor.
#[tokio::test]
async fn graph_delta_pages_past_the_filtered_cap_and_advances_the_cursor() {
    // More than the documented 5,000-message cap, over 51 pages of 100.
    let mock = MockGraph::start(5_001).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let folder = Folder::new("fld", "acc", "Inbox", FolderRole::Inbox);
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    {
        let folder = folder.clone();
        ctx.db
            .write(move |db| repo::folders::insert(db, &folder))
            .await
            .expect("insert folder");
    }
    // `sync_folder_graph_with` resolves the delta path through this mapping.
    ctx.db
        .write(|db| repo::sync_state::set(db, "acc", "mail", "graph_folder:Inbox", "Inbox"))
        .await
        .expect("seed folder mapping");

    // Run the whole pipeline, not just the delta fetcher: every fetched change
    // must reach the local store.
    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &folder, &client)
            .await
            .expect("graph sync");

    assert_eq!(
        outcome.new_messages.len(),
        5_001,
        "every change past the 5,000-message cap must be applied"
    );
    assert!(outcome.initial, "this pass started without a cursor");
    let stored = ctx
        .db
        .read({
            let key = nuncio_mail::graph::messages::delta_cursor_key("fld");
            move |db| repo::sync_state::get(db, "acc", "mail", &key)
        })
        .await
        .expect("read cursor");
    let link = stored.expect("the completed pass persists its deltaLink");
    assert!(link.contains("%24deltatoken=delta-5001"), "got {link}");
    let rows: i64 = ctx
        .db
        .read(|db| {
            let rows = db.query_row(
                "SELECT COUNT(*) FROM message WHERE folder_id = 'fld'",
                [],
                |row| row.get(0),
            )?;
            Ok(rows)
        })
        .await
        .expect("count rows");
    assert_eq!(rows, 5_001, "every change must be stored locally");

    let requests = mock.requests();
    assert_eq!(requests.len(), 51, "5001 messages at 100 per page");
    assert!(
        requests
            .iter()
            .all(|url| !url.contains("%24filter") && !url.contains("$filter")),
        "a filtered delta round is silently capped at 5,000: {requests:?}"
    );
}

/// A response that ends with neither `@odata.nextLink` nor
/// `@odata.deltaLink` is truncated, not "no changes". The pass must fail and
/// leave both the delta cursor and any pending page exactly as they were.
#[tokio::test]
async fn graph_delta_without_a_delta_link_does_not_advance_the_cursor() {
    let mock = MockGraph::start(3).await;
    mock.set_delta_link_in_response(false);
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let folder = Folder::new("fld", "acc", "Inbox", FolderRole::Inbox);
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    seed_graph_folder(&ctx, "acc", &folder).await;

    let old_cursor = format!(
        "{}/me/mailFolders/Inbox/messages/delta?%24deltatoken=old",
        mock.base
    );
    let cursor_key = nuncio_mail::graph::messages::delta_cursor_key("fld");
    {
        let value = old_cursor.clone();
        let key = cursor_key.clone();
        ctx.db
            .write(move |db| repo::sync_state::set(db, "acc", "mail", &key, &value))
            .await
            .expect("seed cursor");
    }

    // Run the real pipeline: the fetcher must fail, and the caller must not
    // persist anything on its behalf.
    let error =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &folder, &client)
            .await
            .expect_err("an unterminated delta response must not count as success");
    assert!(
        error.message().contains("deltaLink"),
        "the error must name the missing cursor: {error:?}"
    );

    let stored = ctx
        .db
        .read({
            let key = cursor_key.clone();
            move |db| repo::sync_state::get(db, "acc", "mail", &key)
        })
        .await
        .expect("read cursor");
    assert_eq!(
        stored.as_deref(),
        Some(old_cursor.as_str()),
        "a truncated pass must keep the old cursor"
    );
    let pending = ctx
        .db
        .read(move |db| repo::sync_state::get(db, "acc", "mail", "graph_pending_v2:fld"))
        .await
        .expect("read pending page");
    assert_eq!(pending, None, "nothing is resumable from a nil response");

    // The same holds for a first-ever scan: failing must not persist a cursor
    // that would make the next run skip the folder's changes.
    {
        let key = cursor_key.clone();
        ctx.db
            .write(move |db| repo::sync_state::delete_key(db, "acc", "mail", &key))
            .await
            .expect("clear cursor");
    }
    nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &folder, &client)
        .await
        .expect_err("a truncated full scan is not success");
    let stored = ctx
        .db
        .read(move |db| repo::sync_state::get(db, "acc", "mail", &cursor_key))
        .await
        .expect("read cursor");
    assert_eq!(stored, None, "a failed full scan must not persist a cursor");
}

/// The ordinary case is unchanged: one page carrying a deltaLink completes the
/// pass, applies its messages and advances the cursor.
#[tokio::test]
async fn graph_delta_single_page_advances_the_cursor() {
    let mock = MockGraph::start(3).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let folder = Folder::new("fld", "acc", "Inbox", FolderRole::Inbox);
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    seed_graph_folder(&ctx, "acc", &folder).await;

    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &folder, &client)
            .await
            .expect("delta pass");

    assert_eq!(outcome.new_messages.len(), 3);
    assert!(outcome.initial);
    assert_eq!(mock.requests().len(), 1, "one page is enough");
    let link = {
        let cursor_key = nuncio_mail::graph::messages::delta_cursor_key("fld");
        ctx.db
            .read(move |db| repo::sync_state::get(db, "acc", "mail", &cursor_key))
            .await
            .expect("read cursor")
            .expect("the completed pass persists its deltaLink")
    };

    let stored = ctx
        .db
        .read({
            let key = nuncio_mail::graph::messages::delta_cursor_key("fld");
            move |db| repo::sync_state::get(db, "acc", "mail", &key)
        })
        .await
        .expect("read cursor");
    assert_eq!(stored.as_deref(), Some(link.as_str()));
}

/// Insert a folder and the `graph_folder:{remote_path}` mapping
/// `sync_folder_graph_with` resolves the opaque Graph folder id through.
async fn seed_graph_folder(ctx: &CoreContext, account_id: &str, folder: &Folder) {
    let stored_folder = folder.clone();
    ctx.db
        .write(move |db| repo::folders::insert(db, &stored_folder))
        .await
        .expect("insert folder");
    let key = format!("graph_folder:{}", folder.remote_path);
    let remote_path = folder.remote_path.clone();
    let account_id = account_id.to_string();
    ctx.db
        .write(move |db| repo::sync_state::set(db, &account_id, "mail", &key, &remote_path))
        .await
        .expect("seed folder mapping");
}

/// A message write that fails must roll the delta cursor back with it.
///
/// If the cursor committed first, the retry would resume *after* the messages
/// that never reached the store and silently skip every one of them.
#[tokio::test]
async fn graph_delta_message_write_failure_rolls_back_the_cursor_and_pending_page() {
    let mock = MockGraph::start(3).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let folder = Folder::new("fld", "acc", "Inbox", FolderRole::Inbox);
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    seed_graph_folder(&ctx, "acc", &folder).await;

    // Inject a failure in the message write transaction. The cursor and the
    // pending page are written after the rows in that same transaction, so
    // aborting here must roll all three back.
    ctx.db
        .write(|db| {
            db.execute_batch(
                "CREATE TRIGGER inject_message_write_failure
                 BEFORE INSERT ON message
                 BEGIN
                     SELECT RAISE(ABORT, 'injected message write failure');
                 END;",
            )?;
            Ok(())
        })
        .await
        .expect("install failure trigger");

    nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &folder, &client)
        .await
        .expect_err("the injected write failure must fail the pass");

    let rows: i64 = ctx
        .db
        .read(|db| {
            let rows = db.query_row(
                "SELECT COUNT(*) FROM message WHERE folder_id = 'fld'",
                [],
                |row| row.get(0),
            )?;
            Ok(rows)
        })
        .await
        .expect("count rows");
    assert_eq!(rows, 0, "nothing from the failed transaction may commit");

    let cursor = ctx
        .db
        .read(|db| {
            repo::sync_state::get(
                db,
                "acc",
                "mail",
                &nuncio_mail::graph::messages::delta_cursor_key("fld"),
            )
        })
        .await
        .expect("read cursor");
    assert_eq!(
        cursor, None,
        "the cursor must not advance past messages that were never stored"
    );
    let pending = ctx
        .db
        .read(|db| repo::sync_state::get(db, "acc", "mail", "graph_pending_v2:fld"))
        .await
        .expect("read pending page");
    assert_eq!(
        pending, None,
        "a failed write must not leave a pending page behind"
    );

    // The retry starts from the same state and must replay every change.
    ctx.db
        .write(|db| {
            db.execute_batch("DROP TRIGGER inject_message_write_failure")?;
            Ok(())
        })
        .await
        .expect("drop failure trigger");
    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &folder, &client)
            .await
            .expect("retry");
    assert_eq!(
        outcome.new_messages.len(),
        3,
        "the retry must apply every change the failed pass saw"
    );
    let rows: i64 = ctx
        .db
        .read(|db| {
            let rows = db.query_row(
                "SELECT COUNT(*) FROM message WHERE folder_id = 'fld'",
                [],
                |row| row.get(0),
            )?;
            Ok(rows)
        })
        .await
        .expect("count rows");
    assert_eq!(rows, 3);
    let cursor = ctx
        .db
        .read(|db| {
            repo::sync_state::get(
                db,
                "acc",
                "mail",
                &nuncio_mail::graph::messages::delta_cursor_key("fld"),
            )
        })
        .await
        .expect("read cursor");
    assert!(
        cursor.is_some(),
        "a successful retry advances the cursor again"
    );
}

/// The sync window only gates new imports. A message the user already has
/// locally must keep receiving read/flag updates after it falls outside the
/// window (for example after "load older mail"), otherwise those rows would
/// stop syncing forever.
#[tokio::test]
async fn graph_delta_updates_cached_messages_outside_the_sync_window() {
    let mock = MockGraph::start(0).await;
    // 2020 is outside any 30-day window regardless of when the test runs.
    mock.add_message("cached-old", "2020-01-01T00:00:00Z", true);
    mock.add_message("uncached-old", "2020-01-02T00:00:00Z", false);
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 30);
    insert_account(&ctx, &account).await;
    let folder = Folder::new("fld", "acc", "Inbox", FolderRole::Inbox);
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    seed_graph_folder(&ctx, "acc", &folder).await;

    // The row a previous load-older would have stored.
    ctx.db
        .write(|db| {
            let date = chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
                .expect("date")
                .with_timezone(&chrono::Utc);
            let mut summary =
                MessageSummary::new("cached-old", "acc", "fld", "thread-cached", date);
            summary.subject = "Cached old message".into();
            summary.seen = false;
            repo::messages::upsert(db, &StoredMessage::new(summary))?;
            Ok(())
        })
        .await
        .expect("seed cached old message");

    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &folder, &client)
            .await
            .expect("graph sync");

    assert!(
        outcome.new_messages.is_empty(),
        "an out-of-window update to a cached row is not a new import"
    );
    let cached = ctx
        .db
        .read(|db| repo::messages::get(db, "cached-old"))
        .await
        .expect("read cached row")
        .expect("the cached row must survive");
    assert!(
        cached.summary.seen,
        "the read flag of a cached out-of-window message must be applied"
    );
    let uncached: i64 = ctx
        .db
        .read(|db| {
            let rows = db.query_row(
                "SELECT COUNT(*) FROM message WHERE id = 'uncached-old'",
                [],
                |row| row.get(0),
            )?;
            Ok(rows)
        })
        .await
        .expect("count uncached");
    assert_eq!(
        uncached, 0,
        "an uncached out-of-window message must not be imported"
    );
}

/// The pre-existing window behaviour is unchanged: an ordinary message that was
/// never cached and predates the window is not imported.
#[tokio::test]
async fn graph_delta_does_not_import_uncached_messages_outside_the_sync_window() {
    let mock = MockGraph::start(0).await;
    mock.add_message("uncached-old", "2020-01-01T00:00:00Z", false);
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 30);
    insert_account(&ctx, &account).await;
    let folder = Folder::new("fld", "acc", "Inbox", FolderRole::Inbox);
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    seed_graph_folder(&ctx, "acc", &folder).await;

    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &folder, &client)
            .await
            .expect("graph sync");

    assert!(outcome.new_messages.is_empty());
    let rows: i64 = ctx
        .db
        .read(|db| {
            let rows = db.query_row(
                "SELECT COUNT(*) FROM message WHERE folder_id = 'fld'",
                [],
                |row| row.get(0),
            )?;
            Ok(rows)
        })
        .await
        .expect("count rows");
    assert_eq!(rows, 0, "out-of-window messages are still not imported");
}

/// An optimistic Graph move whose destination identity was confirmed by the
/// queued operation must still be applied even though the moved message is
/// older than the sync window.
///
/// The optimistic move clears the row's `uid`/`gm_msgid`, so the delta entry
/// (which carries only the *new* Graph id) has no row to match by provider id.
/// The confirmed receipt is the only bridge back to that row; without it the
/// entry is mistaken for a new import, dropped by the window, and the cursor
/// advances past the only event that can bind the row.
#[tokio::test]
async fn graph_delta_binds_a_confirmed_move_target_outside_the_sync_window() {
    let (mock, ctx, account, archive) =
        graph_move_fixture("2020-01-01T00:00:00Z", true).await;

    // The executor's confirmed receipt for the move: `m-old` became `m-new`.
    ctx.db
        .write(|db| {
            let source_uid = nuncio_mail::sync::messages::synthetic_uid("m-old");
            let payload = ops::MoveOp {
                folder_id: "inbox".into(),
                target_folder_id: "archive".into(),
                uids: vec![source_uid],
                source_uidvalidity: None,
                message_ids: vec!["m-old".into()],
                outcomes: vec![ops::OpMessageOutcome {
                    message_id: "m-old".into(),
                    source_uid: Some(source_uid),
                    state: ops::OpMessageState::Confirmed,
                    target_folder_id: Some("archive".into()),
                    target_uid: Some(nuncio_mail::sync::messages::synthetic_uid("m-new")),
                    target_uidvalidity: None,
                    target_remote_id: Some("m-new".into()),
                    attempt_remote_id: None,
                    copy_phase: Default::default(),
                    delete_flag_phase: Default::default(),
                    expunge_phase: Default::default(),
                }],
                remote_ids: vec!["m-old".into()],
            };
            repo::pending_ops::enqueue(
                db,
                nuncio_core::models::NewPendingOp::new(
                    "acc",
                    ops::KIND_MOVE,
                    serde_json::to_value(payload).expect("move payload"),
                    "Move message",
                ),
            )?;
            Ok(())
        })
        .await
        .expect("confirmed move receipt");

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &archive, &client)
            .await
            .expect("graph sync");

    assert!(
        outcome.new_messages.is_empty(),
        "binding a confirmed target updates the projection; it is not a new import"
    );
    assert_moved_message_bound(&ctx, &archive).await;
}

/// The durable target-folder binding must keep working after the operation row
/// that produced it is pruned: `graph_remote:{folder}:{source uid}` is written
/// when the move is confirmed and is independent of the pending-op retention.
#[tokio::test]
async fn graph_delta_binds_a_confirmed_move_mapping_outside_the_sync_window() {
    let (mock, ctx, account, archive) =
        graph_move_fixture("2020-01-01T00:00:00Z", true).await;

    // What `record_graph_move_outcome`/`remap_moved_ids` leave behind, with the
    // operation row already pruned.
    let key = format!(
        "graph_remote:archive:{}",
        nuncio_mail::sync::messages::synthetic_uid("m-old")
    );
    ctx.db
        .write(move |db| repo::sync_state::set(db, "acc", "mail", &key, "m-new"))
        .await
        .expect("seed confirmed target mapping");

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &archive, &client)
            .await
            .expect("graph sync");

    assert!(outcome.new_messages.is_empty());
    assert_moved_message_bound(&ctx, &archive).await;
}

/// Seed the state a confirmed Graph binding leaves behind: the local primary
/// key and the provider id deliberately differ (`id=m-old, gm_msgid=m-new`),
/// and the stored Message-ID is not the one Graph reports for `m-new`, so only
/// a `gm_msgid` match can recognise the row.
async fn seed_bound_graph_row(
    ctx: &CoreContext,
    folder: &Folder,
    local_id: &str,
    graph_id: &str,
    header: &str,
    seen: bool,
) {
    let account_id = folder.account_id.clone();
    let folder_id = folder.id.clone();
    let local_id = local_id.to_string();
    let graph_id = graph_id.to_string();
    let header = header.to_string();
    ctx.db
        .write(move |db| {
            let mut summary = MessageSummary::new(
                local_id.as_str(),
                account_id.as_str(),
                folder_id.as_str(),
                format!("thread-{local_id}"),
                chrono::Utc::now(),
            );
            summary.subject = "Bound old message".into();
            summary.seen = seen;
            let mut stored = StoredMessage::new(summary);
            // The delta writer derives the synthetic UID from the local row id
            // the delta is applied to, not from the provider id.
            stored.uid = Some(nuncio_mail::sync::messages::synthetic_uid(&local_id));
            stored.gm_msgid = Some(graph_id);
            stored.message_id_header = Some(header);
            repo::messages::upsert(db, &stored)
        })
        .await
        .expect("seed bound row");
}

/// Seed a delta cursor so the next Graph pass is incremental. Without it the
/// pass would be a full scan, whose authoritative id set can prune local rows
/// for unrelated reasons and hide the behaviour under test.
async fn seed_graph_delta_cursor(ctx: &CoreContext, base: &str, account_id: &str, folder: &Folder) {
    let key = nuncio_mail::graph::messages::delta_cursor_key(&folder.id);
    // A stored deltaLink is followed verbatim, so it must point at the mock.
    let cursor = format!(
        "{base}/me/mailFolders/{}/messages/delta?%24deltatoken=previous",
        folder.remote_path
    );
    let account_id = account_id.to_string();
    ctx.db
        .write(move |db| repo::sync_state::set(db, &account_id, "mail", &key, &cursor))
        .await
        .expect("seed delta cursor");
}

async fn graph_delta_cursor(ctx: &CoreContext, folder: &Folder) -> Option<String> {
    let key = nuncio_mail::graph::messages::delta_cursor_key(&folder.id);
    ctx.db
        .read(move |db| repo::sync_state::get(db, "acc", "mail", &key))
        .await
        .expect("read delta cursor")
}

/// A repeated delivery of the same Graph id must land on the row already bound
/// to it, even when the stored Message-ID no longer matches. Matching by
/// `(account, folder, gm_msgid)` comes first; otherwise the id becomes a second
/// row while the cursor advances past the only event that carries the update.
#[tokio::test]
async fn graph_delta_repeated_delivery_updates_the_row_bound_to_the_graph_id() {
    let mock = MockGraph::start(0).await;
    // isRead = true so the second delivery carries an observable content change.
    mock.add_message("m-new", "2026-01-01T00:00:00Z", true);
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let archive = Folder::new("archive", "acc", "Archive", FolderRole::Archive);
    seed_graph_folder(&ctx, "acc", &archive).await;
    seed_bound_graph_row(
        &ctx,
        &archive,
        "m-old",
        "m-new",
        "stale-header@graph.test",
        false,
    )
    .await;
    seed_graph_delta_cursor(&ctx, &mock.base, "acc", &archive).await;

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &archive, &client)
            .await
            .expect("graph sync");

    assert!(
        outcome.new_messages.is_empty(),
        "an already-bound Graph id is an update, not a new import"
    );
    let stored = ctx
        .db
        .read(|db| repo::messages::get(db, "m-old"))
        .await
        .expect("read bound row")
        .expect("the bound row must survive a repeated delivery");
    assert!(
        stored.summary.seen,
        "the delta content must be applied to the bound row"
    );
    assert_eq!(
        stored.summary.subject, "Subject m-new",
        "the delta content must update the bound row"
    );
    assert_eq!(stored.gm_msgid.as_deref(), Some("m-new"));
    let (rows, duplicate): (i64, i64) = ctx
        .db
        .read(|db| {
            let rows = db.query_row(
                "SELECT COUNT(*) FROM message WHERE account_id = 'acc' AND folder_id = 'archive'",
                [],
                |row| row.get(0),
            )?;
            let duplicate = db.query_row(
                "SELECT COUNT(*) FROM message WHERE id = 'm-new'",
                [],
                |row| row.get(0),
            )?;
            Ok((rows, duplicate))
        })
        .await
        .expect("count rows");
    assert_eq!(rows, 1, "the Graph id must not become a second row");
    assert_eq!(duplicate, 0);
    assert!(
        graph_delta_cursor(&ctx, &archive)
            .await
            .as_deref()
            .is_some_and(|cursor| cursor.contains("deltatoken")),
        "the completed pass advances the cursor"
    );
}

/// `@removed` names a Graph id, but a bound row is keyed by its local primary
/// key. The removal must resolve `(account, folder, gm_msgid)` first; deleting
/// by the Graph id alone misses the row while still advancing the cursor.
#[tokio::test]
async fn graph_delta_removal_deletes_the_row_bound_to_the_graph_id() {
    let mock = MockGraph::start(0).await;
    mock.push_delta_removed("m-new");
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let archive = Folder::new("archive", "acc", "Archive", FolderRole::Archive);
    seed_graph_folder(&ctx, "acc", &archive).await;
    seed_bound_graph_row(
        &ctx,
        &archive,
        "m-old",
        "m-new",
        "stale-header@graph.test",
        false,
    )
    .await;
    seed_graph_delta_cursor(&ctx, &mock.base, "acc", &archive).await;

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &archive, &client)
            .await
            .expect("graph sync");

    assert_eq!(
        outcome.deleted_ids,
        vec!["m-old".to_string()],
        "the removal must resolve to the bound local row"
    );
    let stored = ctx
        .db
        .read(|db| repo::messages::get(db, "m-old"))
        .await
        .expect("read bound row");
    assert!(stored.is_none(), "the bound row must be removed");
    let rows: i64 = ctx
        .db
        .read(|db| {
            Ok(db.query_row(
                "SELECT COUNT(*) FROM message WHERE account_id = 'acc' AND folder_id = 'archive'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .expect("count rows");
    assert_eq!(rows, 0);
    assert!(
        graph_delta_cursor(&ctx, &archive)
            .await
            .as_deref()
            .is_some_and(|cursor| cursor.contains("deltatoken")),
        "the cursor advances after the removal is applied"
    );
}

/// An `@removed` id with no local row is not a failure: the rest of the batch
/// still applies, unrelated rows stay, and the cursor still advances.
#[tokio::test]
async fn graph_delta_removal_of_an_unknown_id_keeps_other_rows() {
    let mock = MockGraph::start(0).await;
    mock.push_delta_removed("ghost-id");
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let archive = Folder::new("archive", "acc", "Archive", FolderRole::Archive);
    seed_graph_folder(&ctx, "acc", &archive).await;
    seed_bound_graph_row(&ctx, &archive, "m-old", "m-new", "m-new@graph.test", false).await;
    seed_graph_delta_cursor(&ctx, &mock.base, "acc", &archive).await;

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &archive, &client)
            .await
            .expect("an unknown removal must not fail the pass");

    assert!(outcome.deleted_ids.is_empty());
    let stored = ctx
        .db
        .read(|db| repo::messages::get(db, "m-old"))
        .await
        .expect("read bound row");
    assert!(
        stored.is_some(),
        "an unknown removal must not touch other rows"
    );
    assert!(
        graph_delta_cursor(&ctx, &archive)
            .await
            .as_deref()
            .is_some_and(|cursor| cursor.contains("deltatoken")),
        "an unknown removal is recorded but still lets the cursor advance"
    );
}

/// A late `@removed` naming the *old* Graph id must not delete a row that has
/// already been re-bound to a new id (`id=m-old, gm_msgid=m-new`). The local
/// primary key fallback only applies to never-rebound rows.
#[tokio::test]
async fn graph_delta_removal_of_a_superseded_id_keeps_the_rebound_row() {
    let mock = MockGraph::start(0).await;
    mock.push_delta_removed("m-old");
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let archive = Folder::new("archive", "acc", "Archive", FolderRole::Archive);
    seed_graph_folder(&ctx, "acc", &archive).await;
    seed_bound_graph_row(&ctx, &archive, "m-old", "m-new", "m-new@graph.test", false).await;
    seed_graph_delta_cursor(&ctx, &mock.base, "acc", &archive).await;

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &archive, &client)
            .await
            .expect("graph sync");

    assert!(
        outcome.deleted_ids.is_empty(),
        "the old id no longer names the re-bound row"
    );
    let stored = ctx
        .db
        .read(|db| repo::messages::get(db, "m-old"))
        .await
        .expect("read rebound row")
        .expect("the live message must survive a stale removal");
    assert_eq!(
        stored.gm_msgid.as_deref(),
        Some("m-new"),
        "the current binding is untouched"
    );
    assert_eq!(stored.summary.folder_id, "archive");
    assert!(
        graph_delta_cursor(&ctx, &archive)
            .await
            .as_deref()
            .is_some_and(|cursor| cursor.contains("deltatoken")),
        "the ignored removal still lets the cursor advance"
    );
}

/// A single batch can report a message and its removal for the same Graph id
/// (the removal is resolved before the local row exists). The removal must
/// win: no row is created and the cursor still advances.
#[tokio::test]
async fn graph_delta_removal_in_the_same_batch_does_not_resurrect_the_message() {
    let mock = MockGraph::start(0).await;
    mock.add_message("m-new", "2026-01-01T00:00:00Z", false);
    mock.push_delta_removed("m-new");
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let archive = Folder::new("archive", "acc", "Archive", FolderRole::Archive);
    seed_graph_folder(&ctx, "acc", &archive).await;
    seed_graph_delta_cursor(&ctx, &mock.base, "acc", &archive).await;

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &archive, &client)
            .await
            .expect("graph sync");

    assert!(
        outcome.new_messages.is_empty(),
        "the removal must suppress the same-batch upsert"
    );
    let rows: i64 = ctx
        .db
        .read(|db| {
            Ok(db.query_row(
                "SELECT COUNT(*) FROM message
                  WHERE account_id = 'acc' AND (id = 'm-new' OR gm_msgid = 'm-new')",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .expect("count rows");
    assert_eq!(rows, 0, "the deleted Graph id must not come back");
    assert!(
        graph_delta_cursor(&ctx, &archive)
            .await
            .as_deref()
            .is_some_and(|cursor| cursor.contains("deltatoken")),
        "the cursor advances past the add-then-remove pair"
    );
}

/// A stale entry that still names the superseded Graph id must not upsert by
/// primary key onto the row an earlier delta re-bound to the new id. The
/// `ON CONFLICT(id)` path would overwrite `gm_msgid=m-new` with `m-old`,
/// silently re-pointing later operations at an id Graph no longer uses.
#[tokio::test]
async fn graph_delta_stale_id_does_not_rebind_the_row_bound_to_the_new_id() {
    let mock = MockGraph::start(0).await;
    // The server still reports the old id, but with a different Message-ID, so
    // only the primary-key fallback could associate it with the local row.
    mock.add_message("m-old", "2026-01-01T00:00:00Z", true);
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let archive = Folder::new("archive", "acc", "Archive", FolderRole::Archive);
    seed_graph_folder(&ctx, "acc", &archive).await;
    seed_bound_graph_row(&ctx, &archive, "m-old", "m-new", "m-new@graph.test", false).await;
    seed_graph_delta_cursor(&ctx, &mock.base, "acc", &archive).await;

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &archive, &client)
            .await
            .expect("graph sync");

    assert!(
        outcome.new_messages.is_empty(),
        "the stale id is an update, not a new import"
    );
    let stored = ctx
        .db
        .read(|db| repo::messages::get(db, "m-old"))
        .await
        .expect("read bound row")
        .expect("the re-bound row must survive a stale entry");
    assert_eq!(
        stored.gm_msgid.as_deref(),
        Some("m-new"),
        "the live binding must not be overwritten by the superseded id"
    );
    assert_eq!(
        stored.summary.subject, "Bound old message",
        "the stale entry's content must not overwrite the row"
    );
    assert!(!stored.summary.seen, "the stale read flag is ignored");
    let (rows, duplicate): (i64, i64) = ctx
        .db
        .read(|db| {
            let rows = db.query_row(
                "SELECT COUNT(*) FROM message WHERE account_id = 'acc' AND folder_id = 'archive'",
                [],
                |row| row.get(0),
            )?;
            let duplicate = db.query_row(
                "SELECT COUNT(*) FROM message WHERE id = 'm-new'",
                [],
                |row| row.get(0),
            )?;
            Ok((rows, duplicate))
        })
        .await
        .expect("count rows");
    assert_eq!(rows, 1, "the stale id must not create a second row");
    assert_eq!(duplicate, 0);
    assert!(
        graph_delta_cursor(&ctx, &archive)
            .await
            .as_deref()
            .is_some_and(|cursor| cursor.contains("deltatoken")),
        "the ignored entry still lets the cursor advance"
    );
}

/// The same stale entry can carry the Message-ID the row was stored with. The
/// heuristic must not adopt a row already bound to another live Graph id just
/// because the headers match: that would rebind `gm_msgid` to the superseded
/// id exactly like the primary-key path did.
#[tokio::test]
async fn graph_delta_stale_id_with_a_matching_header_does_not_rebind() {
    let mock = MockGraph::start(0).await;
    mock.add_message("m-old", "2026-01-01T00:00:00Z", true);
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let archive = Folder::new("archive", "acc", "Archive", FolderRole::Archive);
    seed_graph_folder(&ctx, "acc", &archive).await;
    seed_bound_graph_row(&ctx, &archive, "m-old", "m-new", "m-old@graph.test", false).await;
    seed_graph_delta_cursor(&ctx, &mock.base, "acc", &archive).await;

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &archive, &client)
            .await
            .expect("graph sync");

    assert!(outcome.new_messages.is_empty());
    let stored = ctx
        .db
        .read(|db| repo::messages::get(db, "m-old"))
        .await
        .expect("read bound row")
        .expect("the re-bound row must survive");
    assert_eq!(
        stored.gm_msgid.as_deref(),
        Some("m-new"),
        "a matching Message-ID must not reintroduce the superseded id"
    );
    assert_eq!(stored.summary.subject, "Bound old message");
    assert!(
        graph_delta_cursor(&ctx, &archive)
            .await
            .as_deref()
            .is_some_and(|cursor| cursor.contains("deltatoken")),
        "the ignored entry still lets the cursor advance"
    );
}

/// A removal resolved before the batch's upserts must be re-checked at apply
/// time. Here the batch removes `m-old` and reports `m-new`, which reuses the
/// same local row by Message-ID. The replacement is live on the server; the
/// stale local-id capture must not delete it.
#[tokio::test]
async fn graph_delta_removal_in_the_same_batch_keeps_the_replacement_that_reused_the_row() {
    let mock = MockGraph::start(0).await;
    mock.add_message("m-new", "2026-01-01T00:00:00Z", true);
    mock.push_delta_removed("m-old");
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let archive = Folder::new("archive", "acc", "Archive", FolderRole::Archive);
    seed_graph_folder(&ctx, "acc", &archive).await;
    // The row still bound to the removed id; `m-new` reuses it through the
    // Message-ID the provider preserves across the replacement.
    seed_bound_graph_row(&ctx, &archive, "m-old", "m-old", "m-new@graph.test", false).await;
    seed_graph_delta_cursor(&ctx, &mock.base, "acc", &archive).await;

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &archive, &client)
            .await
            .expect("graph sync");

    assert!(
        outcome.deleted_ids.is_empty(),
        "the row now names the live replacement, so the stale removal must be skipped"
    );
    let stored = ctx
        .db
        .read(|db| repo::messages::get(db, "m-old"))
        .await
        .expect("read replacement")
        .expect("the replacement must not be deleted");
    assert_eq!(
        stored.gm_msgid.as_deref(),
        Some("m-new"),
        "the replacement is bound to its own Graph id"
    );
    assert!(stored.summary.seen, "the replacement's content is applied");
    let rows: i64 = ctx
        .db
        .read(|db| {
            Ok(db.query_row(
                "SELECT COUNT(*) FROM message WHERE account_id = 'acc' AND folder_id = 'archive'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .expect("count rows");
    assert_eq!(rows, 1, "the replacement reuses the row, not a duplicate");
    assert!(
        mock.ids().iter().any(|id| id == "m-new"),
        "the server still holds the replacement"
    );
    assert!(
        graph_delta_cursor(&ctx, &archive)
            .await
            .as_deref()
            .is_some_and(|cursor| cursor.contains("deltatoken")),
        "the completed pass advances the cursor"
    );
}

/// A confirmed move destination can be reported as `@removed` before any delta
/// entry ever bound it. The projection has no `gm_msgid`, so the removal must
/// use the confirmed destination identity to find it; otherwise the row is
/// orphaned while the cursor advances past the only event that named it.
#[tokio::test]
async fn graph_delta_removal_of_a_confirmed_move_projection_deletes_it() {
    let mock = MockGraph::start(0).await;
    // No live message: the only event for the confirmed destination is its
    // removal.
    mock.push_delta_removed("m-new");
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 30);
    insert_account(&ctx, &account).await;
    let inbox = Folder::new("inbox", "acc", "Inbox", FolderRole::Inbox);
    let archive = Folder::new("archive", "acc", "Archive", FolderRole::Archive);
    seed_graph_folder(&ctx, "acc", &inbox).await;
    seed_graph_folder(&ctx, "acc", &archive).await;

    let date = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .expect("fixture date")
        .with_timezone(&chrono::Utc);
    ctx.db
        .write(move |db| {
            let id = "m-old".to_string();
            let mut summary = MessageSummary::new(&id, "acc", "inbox", "thread-moved", date);
            summary.subject = "Moved old message".into();
            let mut stored = StoredMessage::new(summary);
            stored.uid = Some(nuncio_mail::sync::messages::synthetic_uid(&id));
            stored.gm_msgid = Some(id.clone());
            stored.message_id_header = Some("m-old@graph.test".into());
            repo::messages::upsert(db, &stored)?;
            // The optimistic projection an in-flight server MOVE writes.
            repo::messages::move_to_folder_pending(db, std::slice::from_ref(&id), "archive")?;
            Ok(())
        })
        .await
        .expect("seed optimistic move");

    // The executor's confirmed receipt: the destination id is `m-new`.
    ctx.db
        .write(|db| {
            let source_uid = nuncio_mail::sync::messages::synthetic_uid("m-old");
            let payload = ops::MoveOp {
                folder_id: "inbox".into(),
                target_folder_id: "archive".into(),
                uids: vec![source_uid],
                source_uidvalidity: None,
                message_ids: vec!["m-old".into()],
                outcomes: vec![ops::OpMessageOutcome {
                    message_id: "m-old".into(),
                    source_uid: Some(source_uid),
                    state: ops::OpMessageState::Confirmed,
                    target_folder_id: Some("archive".into()),
                    target_uid: Some(nuncio_mail::sync::messages::synthetic_uid("m-new")),
                    target_uidvalidity: None,
                    target_remote_id: Some("m-new".into()),
                    attempt_remote_id: None,
                    copy_phase: Default::default(),
                    delete_flag_phase: Default::default(),
                    expunge_phase: Default::default(),
                }],
                remote_ids: vec!["m-old".into()],
            };
            repo::pending_ops::enqueue(
                db,
                nuncio_core::models::NewPendingOp::new(
                    "acc",
                    ops::KIND_MOVE,
                    serde_json::to_value(payload).expect("move payload"),
                    "Move message",
                ),
            )?;
            Ok(())
        })
        .await
        .expect("confirmed move receipt");
    seed_graph_delta_cursor(&ctx, &mock.base, "acc", &archive).await;

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &archive, &client)
            .await
            .expect("graph sync");

    assert_eq!(
        outcome.deleted_ids,
        vec!["m-old".to_string()],
        "the confirmed destination identity must resolve the projection"
    );
    let stored = ctx
        .db
        .read(|db| repo::messages::get(db, "m-old"))
        .await
        .expect("read projection");
    assert!(
        stored.is_none(),
        "the removed destination's projection must be deleted, not orphaned"
    );
    let rows: i64 = ctx
        .db
        .read(|db| {
            Ok(db.query_row(
                "SELECT COUNT(*) FROM message WHERE account_id = 'acc' AND folder_id = 'archive'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .expect("count rows");
    assert_eq!(rows, 0);
    assert!(
        graph_delta_cursor(&ctx, &archive)
            .await
            .as_deref()
            .is_some_and(|cursor| cursor.contains("deltatoken")),
        "the deletion is applied, so the cursor may advance"
    );
}

/// A legitimate second copy shares the Message-ID of a cached message but has
/// its own Graph id. Declining to *reuse* the cached row must not decline the
/// *import*: the copy is a distinct message, so it becomes its own row and the
/// cursor advances. Skipping the entry (the stale-rebinding guard) made the
/// copy disappear forever.
#[tokio::test]
async fn graph_delta_imports_a_new_copy_that_shares_a_message_id() {
    let mock = MockGraph::start(0).await;
    mock.add_message("m-new", "2026-01-01T00:00:00Z", false);
    // Distinct Graph ids with one RFC 5322 Message-ID: what a copy looks like
    // on the server.
    mock.set_internet_message_id("m-new", "<shared@graph.test>");
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let archive = Folder::new("archive", "acc", "Archive", FolderRole::Archive);
    seed_graph_folder(&ctx, "acc", &archive).await;
    // The cached original, still bound to its own live Graph id.
    seed_bound_graph_row(&ctx, &archive, "m-old", "m-old", "shared@graph.test", false).await;
    seed_graph_delta_cursor(&ctx, &mock.base, "acc", &archive).await;

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &archive, &client)
            .await
            .expect("graph sync");

    assert_eq!(
        outcome.new_messages.len(),
        1,
        "the copy is a new import, not a reused row"
    );
    assert_eq!(outcome.new_messages[0].id, "m-new");

    let (rows, original, copy) = ctx
        .db
        .read(|db| {
            let rows = db.query_row(
                "SELECT COUNT(*) FROM message WHERE account_id = 'acc' AND folder_id = 'archive'",
                [],
                |row| row.get::<_, i64>(0),
            )?;
            let original = repo::messages::get(db, "m-old")?;
            let copy = repo::messages::get(db, "m-new")?;
            Ok((rows, original, copy))
        })
        .await
        .expect("read rows");
    assert_eq!(
        rows, 2,
        "the cached message and the new copy must both be stored"
    );
    let original = original.expect("the cached row must survive");
    assert_eq!(
        original.gm_msgid.as_deref(),
        Some("m-old"),
        "the cached row keeps its own provider binding"
    );
    let copy = copy.expect("the copy must be imported as its own row");
    assert_eq!(
        copy.message_id_header.as_deref(),
        Some("shared@graph.test"),
        "the copy keeps the shared Message-ID"
    );
    assert_eq!(copy.gm_msgid.as_deref(), Some("m-new"));
    assert!(
        graph_delta_cursor(&ctx, &archive)
            .await
            .as_deref()
            .is_some_and(|cursor| cursor.contains("deltatoken")),
        "the completed pass advances the cursor"
    );
}

/// A retained confirmed-MOVE receipt only describes the trip that produced it.
/// When the same row leaves the folder and moves back, the previous trip's
/// destination id is reported as `@removed`; it must not resolve (and delete)
/// the new trip's pending projection. Ownership is decided by the newest
/// operation that named the row for this folder, not by "some receipt exists".
#[tokio::test]
async fn graph_delta_ignores_a_previous_trips_move_receipt_for_a_new_projection() {
    let mock = MockGraph::start(0).await;
    // The destination identity of the *previous* trip. Moving out of Archive
    // reports it as removed.
    mock.push_delta_removed("m-prev");
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let inbox = Folder::new("inbox", "acc", "Inbox", FolderRole::Inbox);
    let archive = Folder::new("archive", "acc", "Archive", FolderRole::Archive);
    let trash = Folder::new("trash", "acc", "Trash", FolderRole::Trash);
    seed_graph_folder(&ctx, "acc", &inbox).await;
    seed_graph_folder(&ctx, "acc", &archive).await;
    seed_graph_folder(&ctx, "acc", &trash).await;

    // The row was moved into Archive on an earlier trip and bound to m-prev.
    // Its operation row is retained with the confirmed receipt.
    seed_bound_graph_row(&ctx, &archive, "m-old", "m-prev", "m-old@graph.test", false).await;
    let source_uid = nuncio_mail::sync::messages::synthetic_uid("m-old");
    ctx.db
        .write(move |db| {
            let payload = ops::MoveOp {
                folder_id: "inbox".into(),
                target_folder_id: "archive".into(),
                uids: vec![source_uid],
                source_uidvalidity: None,
                message_ids: vec!["m-old".into()],
                outcomes: vec![ops::OpMessageOutcome {
                    message_id: "m-old".into(),
                    source_uid: Some(source_uid),
                    state: ops::OpMessageState::Confirmed,
                    target_folder_id: Some("archive".into()),
                    target_uid: Some(nuncio_mail::sync::messages::synthetic_uid("m-prev")),
                    target_uidvalidity: None,
                    target_remote_id: Some("m-prev".into()),
                    attempt_remote_id: None,
                    copy_phase: Default::default(),
                    delete_flag_phase: Default::default(),
                    expunge_phase: Default::default(),
                }],
                remote_ids: vec!["m-old".into()],
            };
            let op = repo::pending_ops::enqueue(
                db,
                nuncio_core::models::NewPendingOp::new(
                    "acc",
                    ops::KIND_MOVE,
                    serde_json::to_value(payload).expect("move payload"),
                    "Move message",
                ),
            )?;
            repo::pending_ops::mark_done(db, &op.id)?;
            Ok(())
        })
        .await
        .expect("seed the previous trip's receipt");

    // The two MOVEs of this trip: leave Archive, then move back into it. Each
    // one optimistically projects the row into the destination, exactly like
    // the service's queue transaction.
    ctx.db
        .write(move |db| {
            let message_ids = vec!["m-old".to_string()];
            repo::messages::move_to_folder_pending(db, &message_ids, "trash")?;
            let outbound = ops::MoveOp {
                folder_id: "archive".into(),
                target_folder_id: "trash".into(),
                uids: vec![source_uid],
                source_uidvalidity: None,
                message_ids: message_ids.clone(),
                outcomes: Vec::new(),
                remote_ids: vec!["m-prev".into()],
            };
            repo::pending_ops::enqueue(
                db,
                nuncio_core::models::NewPendingOp::new(
                    "acc",
                    ops::KIND_MOVE,
                    serde_json::to_value(outbound).expect("outbound payload"),
                    "Move message",
                ),
            )?;
            repo::messages::move_to_folder_pending(db, &message_ids, "archive")?;
            let returning = ops::MoveOp {
                folder_id: "trash".into(),
                target_folder_id: "archive".into(),
                uids: vec![source_uid],
                source_uidvalidity: None,
                message_ids,
                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            repo::pending_ops::enqueue(
                db,
                nuncio_core::models::NewPendingOp::new(
                    "acc",
                    ops::KIND_MOVE,
                    serde_json::to_value(returning).expect("returning payload"),
                    "Move message",
                ),
            )?;
            Ok(())
        })
        .await
        .expect("seed the out-and-back moves");
    seed_graph_delta_cursor(&ctx, &mock.base, "acc", &archive).await;

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &archive, &client)
            .await
            .expect("graph sync");

    assert!(
        outcome.deleted_ids.is_empty(),
        "the previous trip's removal must not delete the new projection"
    );
    let stored = ctx
        .db
        .read(|db| repo::messages::get(db, "m-old"))
        .await
        .expect("read projection")
        .expect("the new trip's pending projection must survive");
    assert_eq!(stored.summary.folder_id, "archive");
    assert_eq!(stored.uid, None, "it is still waiting for its new identity");
    assert_eq!(
        stored.gm_msgid, None,
        "the previous trip's id must not be bound to the new projection"
    );
    let target_pending: i64 = ctx
        .db
        .read(|db| {
            Ok(db.query_row(
                "SELECT target_pending FROM message WHERE id = 'm-old'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .expect("read target_pending");
    assert_eq!(
        target_pending, 1,
        "the new projection is still waiting for its own move to be confirmed"
    );
}

/// A late update in the *source* folder still carries the old Graph id. It
/// must not upsert by primary key and pull an optimistic move projection back
/// out of its destination (or clear `target_pending`).
#[tokio::test]
async fn graph_source_folder_update_does_not_rewind_an_optimistic_move() {
    let mock = MockGraph::start(0).await;
    // The source folder keeps reporting the pre-move id/content.
    mock.add_message("m-old", "2026-01-01T00:00:00Z", true);
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 30);
    insert_account(&ctx, &account).await;
    let inbox = Folder::new("inbox", "acc", "Inbox", FolderRole::Inbox);
    let archive = Folder::new("archive", "acc", "Archive", FolderRole::Archive);
    seed_graph_folder(&ctx, "acc", &inbox).await;
    seed_graph_folder(&ctx, "acc", &archive).await;

    let date = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .expect("fixture date")
        .with_timezone(&chrono::Utc);
    ctx.db
        .write(move |db| {
            let id = "m-old".to_string();
            let mut summary = MessageSummary::new(&id, "acc", "inbox", "thread-moved", date);
            summary.subject = "Moved old message".into();
            let mut stored = StoredMessage::new(summary);
            stored.uid = Some(nuncio_mail::sync::messages::synthetic_uid(&id));
            stored.gm_msgid = Some(id.clone());
            stored.message_id_header = Some("m-old@graph.test".into());
            repo::messages::upsert(db, &stored)?;
            // The optimistic projection an in-flight server MOVE writes: the
            // row keeps its id but the UID and Graph binding are cleared.
            repo::messages::move_to_folder_pending(db, std::slice::from_ref(&id), "archive")?;
            Ok(())
        })
        .await
        .expect("seed optimistic move");
    seed_graph_delta_cursor(&ctx, &mock.base, "acc", &inbox).await;

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &inbox, &client)
            .await
            .expect("source folder sync");

    assert!(
        outcome.new_messages.is_empty(),
        "the projection is not a new import"
    );
    let stored = ctx
        .db
        .read(|db| repo::messages::get(db, "m-old"))
        .await
        .expect("read projection")
        .expect("the projection survives the source update");
    assert_eq!(
        stored.summary.folder_id, "archive",
        "the projection stays in the destination folder"
    );
    assert_eq!(stored.uid, None, "the target identity is not bound yet");
    assert_eq!(
        stored.gm_msgid, None,
        "the source id must not be re-bound to the projection"
    );
    assert_eq!(
        stored.summary.subject, "Moved old message",
        "stale source content does not overwrite the projection"
    );
    let target_pending: i64 = ctx
        .db
        .read(|db| {
            Ok(db.query_row(
                "SELECT target_pending FROM message WHERE id = 'm-old'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .expect("read target_pending");
    assert_eq!(
        target_pending, 1,
        "the move is still waiting for its target"
    );
}

/// A Graph account with an Inbox/Archive pair and one old message that the
/// service optimistically moved to Archive, exactly like an in-flight MOVE.
async fn graph_move_fixture(
    received: &str,
    is_read: bool,
) -> (MockGraph, CoreContext, Account, Folder) {
    let mock = MockGraph::start(0).await;
    // The server copy has its new Graph id and preserves the (old) received
    // time; the mock derives the Message-ID from the id, so the row cannot be
    // adopted by header alone.
    mock.add_message("m-new", received, is_read);
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 30);
    insert_account(&ctx, &account).await;
    let inbox = Folder::new("inbox", "acc", "Inbox", FolderRole::Inbox);
    let archive = Folder::new("archive", "acc", "Archive", FolderRole::Archive);
    seed_graph_folder(&ctx, "acc", &inbox).await;
    seed_graph_folder(&ctx, "acc", &archive).await;

    let date = chrono::DateTime::parse_from_rfc3339(received)
        .expect("fixture date")
        .with_timezone(&chrono::Utc);
    ctx.db
        .write(move |db| {
            let id = "m-old".to_string();
            let mut summary = MessageSummary::new(&id, "acc", "inbox", "thread-moved", date);
            summary.subject = "Moved old message".into();
            summary.seen = false;
            let mut stored = StoredMessage::new(summary);
            stored.uid = Some(nuncio_mail::sync::messages::synthetic_uid(&id));
            stored.gm_msgid = Some(id.clone());
            // The pre-move Message-ID; Graph keeps it across the move.
            stored.message_id_header = Some("m-old@graph.test".into());
            repo::messages::upsert(db, &stored)?;
            // Same optimistic projection the service writes for a remote move.
            repo::messages::move_to_folder_pending(db, std::slice::from_ref(&id), "archive")?;
            Ok(())
        })
        .await
        .expect("seed optimistic move");

    (mock, ctx, account, archive)
}

async fn assert_moved_message_bound(ctx: &CoreContext, archive: &Folder) {
    let stored = ctx
        .db
        .read(|db| repo::messages::get(db, "m-old"))
        .await
        .expect("read moved row")
        .expect("the moved row must survive the delta");
    assert_eq!(
        stored.summary.folder_id, archive.id,
        "the row stays in the destination folder"
    );
    assert!(
        stored.summary.seen,
        "the delta update must be applied to the projection"
    );
    assert_eq!(
        stored.gm_msgid.as_deref(),
        Some("m-new"),
        "the confirmed target identity is bound to the row"
    );
    assert!(stored.uid.is_some(), "the row is synced again");

    let (target_pending, duplicate): (i64, i64) = ctx
        .db
        .read(|db| {
            let pending = db.query_row(
                "SELECT target_pending FROM message WHERE id = 'm-old'",
                [],
                |row| row.get(0),
            )?;
            let duplicate = db.query_row(
                "SELECT COUNT(*) FROM message WHERE id = 'm-new'",
                [],
                |row| row.get(0),
            )?;
            Ok((pending, duplicate))
        })
        .await
        .expect("read binding state");
    assert_eq!(target_pending, 0, "the projection is no longer pending");
    assert_eq!(
        duplicate, 0,
        "the new Graph id must not become a second row"
    );

    let cursor_key = nuncio_mail::graph::messages::delta_cursor_key(&archive.id);
    let cursor = ctx
        .db
        .read(move |db| repo::sync_state::get(db, "acc", "mail", &cursor_key))
        .await
        .expect("read cursor");
    assert!(
        cursor.is_some(),
        "the completed pass advances the cursor without losing the message"
    );
}

/// A Microsoft Graph account with the given sync window.
fn graph_account(id: &str, sync_window_days: u32) -> Account {
    let mut account = account(id, &format!("{id}@example.com"), None, None, None);
    account.provider = ProviderKind::Outlook;
    account.oauth_provider = Some(OAuthProvider::Microsoft);
    account.sync_window_days = sync_window_days;
    account
}

/// Regression: a real Graph 403 refuses a move (`NotExecuted`), and the
/// verification GET cannot prove the folder (no `parentFolderId`). Undo then
/// restores the projection without a compensating inverse. The restored row
/// must keep the provable source identity, so a retained confirmation for an
/// older copy (`m-old`) is not re-adopted and its `@removed` cannot delete the
/// live message.
#[tokio::test]
async fn review_graph_refused_move_undo_preserves_current_trip_after_old_removal() {
    async fn confirm_latest(ctx: &CoreContext, remote: &str) {
        let remote = remote.to_string();
        ctx.db
            .write(move |db| {
                let op = repo::pending_ops::list(db, Some("acc"))?
                    .into_iter()
                    .max_by_key(|op| op.sequence)
                    .unwrap();
                let mut payload: ops::MoveOp = repo::pending_ops::payload(&op)?;
                payload.outcomes.push(ops::OpMessageOutcome {
                    message_id: "proj".into(),
                    source_uid: Some(payload.uids[0]),
                    state: ops::OpMessageState::Confirmed,
                    target_folder_id: Some(payload.target_folder_id.clone()),
                    target_uid: Some(nuncio_mail::sync::messages::synthetic_uid(&remote)),
                    target_uidvalidity: None,
                    target_remote_id: Some(remote.clone()),
                    attempt_remote_id: None,
                    copy_phase: Default::default(),
                    delete_flag_phase: Default::default(),
                    expunge_phase: Default::default(),
                });
                repo::pending_ops::update_payload(
                    db,
                    &op.id,
                    &serde_json::to_value(&payload).unwrap(),
                )?;
                repo::pending_ops::mark_done(db, &op.id)?;
                repo::messages::set_move_target_identity(
                    db,
                    "acc",
                    "proj",
                    &payload.target_folder_id,
                    &remote,
                    &op.id,
                    op.sequence,
                )?;
                let uid = nuncio_mail::sync::messages::synthetic_uid("proj");
                repo::messages::set_graph_identity(db, "proj", uid, &remote)?;
                repo::sync_state::set(
                    db,
                    "acc",
                    "mail",
                    &format!("graph_remote:{}:{uid}", payload.target_folder_id),
                    &remote,
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }
    let mock = MockGraph::start(0).await;
    mock.add_message("m-current", "2026-01-01T00:00:00Z", false);
    mock.set_parent_folder_response(None); // The verification GET proves no folder.
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let inbox = Folder::new("inbox", "acc", "Inbox", FolderRole::Inbox);
    let archive = Folder::new("archive", "acc", "Archive", FolderRole::Archive);
    seed_graph_folder(&ctx, "acc", &inbox).await;
    seed_graph_folder(&ctx, "acc", &archive).await;
    seed_bound_graph_row(&ctx, &inbox, "proj", "m-old", "shared@graph.test", false).await;
    let older = ctx
        .db
        .write(|db| {
            let uid = nuncio_mail::sync::messages::synthetic_uid("proj");
            let payload = ops::MoveOp {
                folder_id: "archive".into(),
                target_folder_id: "inbox".into(),
                uids: vec![uid],
                source_uidvalidity: None,
                message_ids: vec!["proj".into()],
                remote_ids: vec!["old-source".into()],
                outcomes: vec![ops::OpMessageOutcome {
                    message_id: "proj".into(),
                    source_uid: Some(uid),
                    state: ops::OpMessageState::Confirmed,
                    target_folder_id: Some("inbox".into()),
                    target_uid: Some(nuncio_mail::sync::messages::synthetic_uid("m-old")),
                    target_uidvalidity: None,
                    target_remote_id: Some("m-old".into()),
                    attempt_remote_id: None,
                    copy_phase: Default::default(),
                    delete_flag_phase: Default::default(),
                    expunge_phase: Default::default(),
                }],
            };
            repo::pending_ops::enqueue(
                db,
                nuncio_core::models::NewPendingOp::new(
                    "acc",
                    ops::KIND_MOVE,
                    serde_json::to_value(payload).unwrap(),
                    "Retained partial batch",
                ),
            )
        })
        .await
        .unwrap();
    let service = MailService::new(ctx.clone());
    service
        .message_act(
            &["proj".into()],
            &MessageAction::Move {
                folder_id: "archive".into(),
            },
        )
        .await
        .unwrap();
    confirm_latest(&ctx, "m-out").await;
    service
        .message_act(
            &["proj".into()],
            &MessageAction::Move {
                folder_id: "inbox".into(),
            },
        )
        .await
        .unwrap();
    confirm_latest(&ctx, "m-current").await;
    ctx.db
        .write(|db| {
            repo::pending_ops::prune_finished(db, chrono::Utc::now() + chrono::Duration::days(1))?;
            Ok(())
        })
        .await
        .unwrap();
    let undo = service
        .message_act(
            &["proj".into()],
            &MessageAction::Move {
                folder_id: "archive".into(),
            },
        )
        .await
        .unwrap();
    ctx.db
        .write(move |db| {
            db.execute(
                "UPDATE pending_op SET next_retry_at = '2099-01-01T00:00:00Z' WHERE id = ?1",
                rusqlite::params![older.id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let execution = ops::executor::run_due_graph_with(&ctx, &account, &client)
        .await
        .unwrap();
    assert_eq!(
        execution.failed, 1,
        "the real Graph transport refused this trip"
    );
    assert!(
        mock.requests()
            .iter()
            .any(|request| request.contains("m-current/move")),
        "POST addressed the current copy"
    );
    assert!(service.message_undo(&undo).await.unwrap());
    let projection = ctx
        .db
        .read(|db| repo::messages::get_required(db, "proj"))
        .await
        .unwrap();
    assert_eq!(projection.summary.folder_id, "inbox");
    assert_eq!(
        projection.uid,
        Some(nuncio_mail::sync::messages::synthetic_uid("proj")),
        "the refusal restores the provable source uid"
    );
    assert_eq!(
        projection.gm_msgid.as_deref(),
        Some("m-current"),
        "the restored row must own the current trip's Graph id"
    );
    mock.push_delta_removed("m-old");
    seed_graph_delta_cursor(&ctx, &mock.base, "acc", &inbox).await;
    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &inbox, &client)
            .await
            .unwrap();
    assert!(
        outcome.deleted_ids.is_empty(),
        "the historical ID deleted the current message: {:?}",
        outcome.deleted_ids
    );
    let projection = ctx
        .db
        .read(|db| repo::messages::get_required(db, "proj"))
        .await
        .unwrap();
    assert_eq!(projection.summary.folder_id, "inbox");
    assert_eq!(projection.gm_msgid.as_deref(), Some("m-current"));
}

/// Regression: a Graph batch that fails *before* the transport (missing target
/// folder mapping) settles with no `send_intent`, no `outcome_unknown` and no
/// per-message outcome. Undo must treat that as provably unsent, restore the
/// current trip's Graph identity, and therefore keep a retained `@removed` for
/// the previous trip from deleting the restored row.
#[tokio::test]
async fn graph_pre_send_failure_undo_preserves_current_trip_after_old_removal() {
    async fn confirm_latest(ctx: &CoreContext, remote: &str) {
        let remote = remote.to_string();
        ctx.db
            .write(move |db| {
                let op = repo::pending_ops::list(db, Some("acc"))?
                    .into_iter()
                    .max_by_key(|op| op.sequence)
                    .unwrap();
                let mut payload: ops::MoveOp = repo::pending_ops::payload(&op)?;
                payload.outcomes.push(ops::OpMessageOutcome {
                    message_id: "proj".into(),
                    source_uid: Some(payload.uids[0]),
                    state: ops::OpMessageState::Confirmed,
                    target_folder_id: Some(payload.target_folder_id.clone()),
                    target_uid: Some(nuncio_mail::sync::messages::synthetic_uid(&remote)),
                    target_uidvalidity: None,
                    target_remote_id: Some(remote.clone()),
                    attempt_remote_id: None,
                    copy_phase: Default::default(),
                    delete_flag_phase: Default::default(),
                    expunge_phase: Default::default(),
                });
                repo::pending_ops::update_payload(
                    db,
                    &op.id,
                    &serde_json::to_value(&payload).unwrap(),
                )?;
                repo::pending_ops::mark_done(db, &op.id)?;
                repo::messages::set_move_target_identity(
                    db,
                    "acc",
                    "proj",
                    &payload.target_folder_id,
                    &remote,
                    &op.id,
                    op.sequence,
                )?;
                let uid = nuncio_mail::sync::messages::synthetic_uid("proj");
                repo::messages::set_graph_identity(db, "proj", uid, &remote)?;
                repo::sync_state::set(
                    db,
                    "acc",
                    "mail",
                    &format!("graph_remote:{}:{uid}", payload.target_folder_id),
                    &remote,
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    let mock = MockGraph::start(0).await;
    mock.add_message("m-current", "2026-01-01T00:00:00Z", false);
    mock.set_parent_folder_response(None); // The verification GET proves no folder.
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let inbox = Folder::new("inbox", "acc", "Inbox", FolderRole::Inbox);
    let archive = Folder::new("archive", "acc", "Archive", FolderRole::Archive);
    seed_graph_folder(&ctx, "acc", &inbox).await;
    seed_graph_folder(&ctx, "acc", &archive).await;
    seed_bound_graph_row(&ctx, &inbox, "proj", "m-old", "shared@graph.test", false).await;
    // A historical partial batch is retained: it confirmed this row on an
    // earlier trip into `inbox` as `m-old`. It stays pending on the server side
    // (its target mapping disappeared), so it remains in the queue.
    let older = ctx
        .db
        .write(|db| {
            let uid = nuncio_mail::sync::messages::synthetic_uid("proj");
            let payload = ops::MoveOp {
                folder_id: "archive".into(),
                target_folder_id: "inbox".into(),
                uids: vec![uid],
                source_uidvalidity: None,
                message_ids: vec!["proj".into()],
                remote_ids: vec!["old-source".into()],
                outcomes: vec![ops::OpMessageOutcome {
                    message_id: "proj".into(),
                    source_uid: Some(uid),
                    state: ops::OpMessageState::Confirmed,
                    target_folder_id: Some("inbox".into()),
                    target_uid: Some(nuncio_mail::sync::messages::synthetic_uid("m-old")),
                    target_uidvalidity: None,
                    target_remote_id: Some("m-old".into()),
                    attempt_remote_id: None,
                    copy_phase: Default::default(),
                    delete_flag_phase: Default::default(),
                    expunge_phase: Default::default(),
                }],
            };
            repo::pending_ops::enqueue(
                db,
                nuncio_core::models::NewPendingOp::new(
                    "acc",
                    ops::KIND_MOVE,
                    serde_json::to_value(payload).unwrap(),
                    "Retained partial batch",
                ),
            )
        })
        .await
        .unwrap();
    let service = MailService::new(ctx.clone());
    service
        .message_act(
            &["proj".into()],
            &MessageAction::Move {
                folder_id: "archive".into(),
            },
        )
        .await
        .unwrap();
    confirm_latest(&ctx, "m-out").await;
    service
        .message_act(
            &["proj".into()],
            &MessageAction::Move {
                folder_id: "inbox".into(),
            },
        )
        .await
        .unwrap();
    confirm_latest(&ctx, "m-current").await;
    ctx.db
        .write(|db| {
            repo::pending_ops::prune_finished(db, chrono::Utc::now() + chrono::Duration::days(1))?;
            Ok(())
        })
        .await
        .unwrap();
    let undo = service
        .message_act(
            &["proj".into()],
            &MessageAction::Move {
                folder_id: "archive".into(),
            },
        )
        .await
        .unwrap();
    ctx.db
        .write(move |db| {
            db.execute(
                "UPDATE pending_op SET next_retry_at = '2099-01-01T00:00:00Z' WHERE id = ?1",
                rusqlite::params![older.id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    ctx.db
        .write(|db| {
            repo::sync_state::delete_key(db, "acc", "mail", "graph_folder:Archive")?;
            db.execute(
                "UPDATE pending_op SET attempts = 7 WHERE state = 'pending' AND next_retry_at IS NULL",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let execution = ops::executor::run_due_graph_with(&ctx, &account, &client)
        .await
        .unwrap();
    assert_eq!(
        execution.failed, 1,
        "the real Graph transport refused this trip"
    );
    assert!(
        !mock
            .requests()
            .iter()
            .any(|request| request.contains("/move")),
        "the missing target mapping prevented every POST"
    );
    let failed = ctx
        .db
        .read(|db| {
            Ok(repo::pending_ops::list(db, Some("acc"))?
                .into_iter()
                .max_by_key(|op| op.sequence)
                .unwrap())
        })
        .await
        .unwrap();
    assert!(
        !failed.send_intent && !failed.outcome_unknown,
        "no attempt reached the transport"
    );
    let failed_payload: ops::MoveOp = repo::pending_ops::payload(&failed).unwrap();
    assert!(
        failed_payload.outcomes.is_empty(),
        "the pre-send failure wrote no per-message outcome"
    );
    assert!(service.message_undo(&undo).await.unwrap());
    let projection = ctx
        .db
        .read(|db| repo::messages::get_required(db, "proj"))
        .await
        .unwrap();
    assert_eq!(projection.summary.folder_id, "inbox");
    mock.push_delta_removed("m-old");
    seed_graph_delta_cursor(&ctx, &mock.base, "acc", &inbox).await;
    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &inbox, &client)
            .await
            .unwrap();
    assert!(
        outcome.deleted_ids.is_empty(),
        "the historical ID deleted the current message: {:?}",
        outcome.deleted_ids
    );
    let projection = ctx
        .db
        .read(|db| repo::messages::get_required(db, "proj"))
        .await
        .unwrap();
    assert_eq!(projection.summary.folder_id, "inbox");
    assert_eq!(projection.gm_msgid.as_deref(), Some("m-current"));
}

/* ───────────────── full-sync marker lifecycle ───────────────── */

/// ① A folder pass interrupted while its delta request is in flight must not
/// leave the user-visible `fullSync` marker set. `AccountRuntime::abort`
/// drops the whole sync future, so only a `Drop`-based cleanup can run.
#[tokio::test]
async fn cancelled_graph_full_sync_clears_the_visible_marker() {
    use nuncio_core::models::SyncModule;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind hanging graph server");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let inbox = Folder::new("inbox", "acc", "Inbox", FolderRole::Inbox);
    seed_graph_folder(&ctx, "acc", &inbox).await;
    ctx.sync_status.begin("acc", SyncModule::Mail, None);

    let running = tokio::spawn({
        let ctx = ctx.clone();
        async move {
            let client = nuncio_mail::graph::GraphMailClient::with_base(base);
            nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &inbox, &client)
                .await
        }
    });
    // The marker is set before the real HTTP request. Keep the response
    // pending, then abort exactly as `AccountRuntime::abort` does on stop.
    let (_connection, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("delta request reached the server")
        .expect("accept");
    assert!(
        ctx.sync_status
            .get("acc", SyncModule::Mail)
            .expect("status")
            .full_sync,
        "the pass marks itself as a full sync before awaiting the network"
    );

    running.abort();
    assert!(running.await.expect_err("aborted task").is_cancelled());
    assert!(
        !ctx.sync_status
            .get("acc", SyncModule::Mail)
            .expect("status")
            .full_sync,
        "a cancelled folder pass must not leave the full-sync marker set"
    );
}

/// ② A pass that returns normally clears the marker and leaves the rest of
/// the sync status exactly as the caller began it.
#[tokio::test]
async fn completed_graph_full_sync_clears_the_marker_without_touching_state() {
    use nuncio_core::models::SyncModule;

    let mock = MockGraph::start(3).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let inbox = Folder::new("inbox", "acc", "Inbox", FolderRole::Inbox);
    seed_graph_folder(&ctx, "acc", &inbox).await;
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let begun = ctx.sync_status.begin("acc", SyncModule::Mail, None);

    let outcome =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &inbox, &client)
            .await
            .expect("graph sync");
    assert!(outcome.initial, "no cursor means a first full pass");
    let status = ctx
        .sync_status
        .get("acc", SyncModule::Mail)
        .expect("status");
    assert!(!status.full_sync, "a completed pass clears the marker");
    assert_eq!(
        status.state, begun.state,
        "the folder pass owns no state transition"
    );
}

/// An older Graph batch that still has retry work must not overwrite the
/// destination a newer move trip recorded on the same projection. Only the
/// durable row carries that evidence once the newer receipt has been pruned,
/// so a stale failure (or its recovery GET) that clears it lets the old
/// `@removed` tombstone delete the new projection.
#[tokio::test]
async fn an_old_graph_batch_failure_keeps_a_newer_trip_ownership() {
    let mock = MockGraph::start(0).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let inbox = Folder::new("inbox", "acc", "Inbox", FolderRole::Inbox);
    let archive = Folder::new("archive", "acc", "Archive", FolderRole::Archive);
    seed_graph_folder(&ctx, "acc", &inbox).await;
    seed_graph_folder(&ctx, "acc", &archive).await;
    seed_bound_graph_row(
        &ctx,
        &archive,
        "original-id",
        "m-earlier",
        "shared@graph.test",
        false,
    )
    .await;

    ctx.db
        .write(|db| {
            let ids = vec!["original-id".to_string()];
            let uid = nuncio_mail::sync::messages::synthetic_uid(&ids[0]);
            let make_payload = |src: &str, dst: &str, attempt: &str, confirmed: &str| ops::MoveOp {
                folder_id: src.into(),
                target_folder_id: dst.into(),
                uids: vec![uid],
                source_uidvalidity: None,
                message_ids: ids.clone(),
                remote_ids: vec![attempt.into()],
                outcomes: vec![ops::OpMessageOutcome {
                    message_id: ids[0].clone(),
                    source_uid: Some(uid),
                    state: ops::OpMessageState::Confirmed,
                    target_folder_id: Some(dst.into()),
                    target_uid: Some(nuncio_mail::sync::messages::synthetic_uid(confirmed)),
                    target_uidvalidity: None,
                    target_remote_id: Some(confirmed.into()),
                    attempt_remote_id: None,
                    copy_phase: Default::default(),
                    delete_flag_phase: Default::default(),
                    expunge_phase: Default::default(),
                }],
            };
            let enqueue = |db: &rusqlite::Connection, payload| {
                repo::pending_ops::enqueue(
                    db,
                    nuncio_core::models::NewPendingOp::new(
                        "acc",
                        ops::KIND_MOVE,
                        serde_json::to_value(payload).unwrap(),
                        "Move message",
                    ),
                )
            };
            // An earlier multi-message operation confirmed this row, but still
            // has retry work for another message.
            let mut earlier = make_payload("inbox", "archive", "original-id", "m-earlier");
            earlier.message_ids.push("second-message".into());
            earlier.uids.push(123);
            earlier.remote_ids.push("second-provider-id".into());
            enqueue(db, earlier)?;

            // The row makes a round trip, then a newer trip claims it and
            // records a destination whose receipt is pruned afterwards.
            let outbound = enqueue(db, make_payload("archive", "inbox", "m-earlier", "m-out"))?;
            repo::pending_ops::mark_done(db, &outbound.id)?;
            repo::messages::move_to_folder_pending(db, &ids, "inbox")?;
            repo::messages::set_graph_identity(db, &ids[0], uid, "m-out")?;

            repo::messages::move_to_folder_pending(db, &ids, "archive")?;
            let returning = enqueue(db, make_payload("inbox", "archive", "m-out", "m-current"))?;
            repo::pending_ops::mark_done(db, &returning.id)?;
            assert_eq!(
                repo::messages::set_move_target_identity(
                    db,
                    "acc",
                    &ids[0],
                    "archive",
                    "m-current",
                    &returning.id,
                    returning.sequence,
                )?,
                1
            );
            repo::pending_ops::prune_finished(db, chrono::Utc::now() + chrono::Duration::days(1))?;
            assert!(repo::pending_ops::get(db, &returning.id)?.is_none());
            Ok(())
        })
        .await
        .expect("seed trips");

    // The newer receipt is the durable evidence before the old batch fails.
    let seed_owner: Option<String> = ctx
        .db
        .read(|db| {
            Ok(db.query_row(
                "SELECT move_target_op_id FROM message WHERE id = 'original-id'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .expect("seed owner");
    assert!(seed_owner.is_some());

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let execution = ops::executor::run_due_graph_with(&ctx, &account, &client)
        .await
        .expect("run due operations");
    assert_eq!(
        execution.failed, 1,
        "the earlier multi-message operation really failed on its remaining row"
    );

    let (remote, owner, reconcile): (Option<String>, Option<String>, Option<String>) = ctx
        .db
        .read(|db| {
            Ok(db.query_row(
                "SELECT move_target_remote_id, move_target_op_id, reconcile_op_id
                   FROM message WHERE id = 'original-id'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?)
        })
        .await
        .expect("row evidence");
    assert_eq!(
        remote.as_deref(),
        Some("m-current"),
        "the old failure erased the newer destination"
    );
    assert_eq!(
        owner, seed_owner,
        "the old failure erased the current owner"
    );
    assert_eq!(reconcile, None, "the row was marked for the wrong rollback");

    // The old removal now names a copy that is no longer the projection's
    // evidence: it must not delete the row or advance past it as a deletion.
    mock.push_delta_removed("m-earlier");
    seed_graph_delta_cursor(&ctx, &mock.base, "acc", &archive).await;
    let result =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &archive, &client)
            .await
            .expect("sync after an earlier attempt's retry");
    assert!(
        result.deleted_ids.is_empty(),
        "the old tombstone deleted the new pending projection: {:?}",
        result.deleted_ids
    );
    let targets = ctx
        .db
        .read(|db| repo::messages::confirmed_move_projection_targets(db, "acc", "archive"))
        .await
        .expect("projection targets");
    assert_eq!(
        targets.get("m-current").map(String::as_str),
        Some("original-id")
    );
}

/// Regression: a trip that reached a terminal ambiguous failure leaves its
/// ownership in `reconcile_op_id` (its move slot is cleared). A retry of an
/// older partial batch must not fill the now-empty move slot with its stale
/// destination, and the delta must not resolve that old id to the current
/// reconciliation projection.
#[tokio::test]
async fn old_receipt_cannot_take_a_newer_failed_trips_reconcile_slot() {
    let mock = MockGraph::start(0).await;
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let inbox = Folder::new("inbox", "acc", "Inbox", FolderRole::Inbox);
    let archive = Folder::new("archive", "acc", "Archive", FolderRole::Archive);
    seed_graph_folder(&ctx, "acc", &inbox).await;
    seed_graph_folder(&ctx, "acc", &archive).await;
    seed_bound_graph_row(
        &ctx,
        &archive,
        "original-id",
        "m-earlier",
        "shared@graph.test",
        false,
    )
    .await;

    ctx.db
        .write(|db| {
            let ids = vec!["original-id".to_string()];
            let uid = nuncio_mail::sync::messages::synthetic_uid(&ids[0]);
            let make_payload = |src: &str, dst: &str, attempt: &str, confirmed: &str| ops::MoveOp {
                folder_id: src.into(),
                target_folder_id: dst.into(),
                uids: vec![uid],
                source_uidvalidity: None,
                message_ids: ids.clone(),
                remote_ids: vec![attempt.into()],
                outcomes: vec![ops::OpMessageOutcome {
                    message_id: ids[0].clone(),
                    source_uid: Some(uid),
                    state: ops::OpMessageState::Confirmed,
                    target_folder_id: Some(dst.into()),
                    target_uid: Some(nuncio_mail::sync::messages::synthetic_uid(confirmed)),
                    target_uidvalidity: None,
                    target_remote_id: Some(confirmed.into()),
                    attempt_remote_id: None,
                    copy_phase: Default::default(),
                    delete_flag_phase: Default::default(),
                    expunge_phase: Default::default(),
                }],
            };
            let enqueue = |db: &rusqlite::Connection, payload| {
                repo::pending_ops::enqueue(
                    db,
                    nuncio_core::models::NewPendingOp::new(
                        "acc",
                        ops::KIND_MOVE,
                        serde_json::to_value(payload).unwrap(),
                        "Move message",
                    ),
                )
            };
            // An earlier multi-message operation confirmed this row, but still
            // has retry work. The row was bound again and may now move.
            let mut earlier = make_payload("inbox", "archive", "original-id", "m-earlier");
            earlier.message_ids.push("second-message".into());
            earlier.uids.push(123);
            earlier.remote_ids.push("second-provider-id".into());
            let older = enqueue(db, earlier)?;
            db.execute(
                "UPDATE pending_op SET next_retry_at='2099-01-01T00:00:00Z' WHERE id=?1",
                rusqlite::params![older.id],
            )?;

            let outbound = enqueue(db, make_payload("archive", "inbox", "m-earlier", "m-out"))?;
            repo::pending_ops::mark_done(db, &outbound.id)?;
            repo::messages::move_to_folder_pending(db, &ids, "inbox")?;
            repo::messages::set_graph_identity(db, &ids[0], uid, "m-out")?;

            repo::messages::move_to_folder_pending(db, &ids, "archive")?;
            let mut returning_payload = make_payload("inbox", "archive", "m-out", "m-current");
            returning_payload.outcomes[0].state = ops::OpMessageState::Unknown;
            returning_payload.outcomes[0].target_remote_id = None;
            returning_payload.outcomes[0].target_uid = None;
            let returning = enqueue(db, returning_payload)?;
            repo::messages::claim_move_target_owner(
                db,
                "acc",
                &ids,
                "archive",
                &returning.id,
                returning.sequence,
            )?;
            repo::pending_ops::mark_send_intent(db, &returning.id)?;
            repo::pending_ops::mark_outcome_unknown(db, &returning.id)?;
            db.execute(
                "UPDATE pending_op SET attempts=7 WHERE id=?1",
                rusqlite::params![returning.id],
            )?;
            Ok(())
        })
        .await
        .expect("seed trips");

    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let current_execution = ops::executor::run_due_graph_with(&ctx, &account, &client)
        .await
        .expect("run due operations");
    assert_eq!(
        current_execution.failed, 1,
        "the new trip reaches terminal ambiguous failure"
    );
    let newer = ctx
        .db
        .read(|db| {
            let (owner, reconcile): (Option<String>, Option<String>) = db.query_row(
                "SELECT move_target_op_id, reconcile_op_id FROM message WHERE id='original-id'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert!(
                owner.is_none() && reconcile.is_some(),
                "failed new trip owns reconciliation but not the move slot"
            );
            db.query_row(
                "SELECT reconcile_op_id FROM message WHERE id='original-id'",
                [],
                |row| row.get::<_, String>(0),
            )
            .map_err(Into::into)
        })
        .await
        .expect("reconcile owner");

    ctx.db
        .write(|db| {
            db.execute("UPDATE pending_op SET next_retry_at=NULL WHERE state='pending'", [])?;
            Ok(())
        })
        .await
        .expect("wake older batch");
    let older_execution = ops::executor::run_due_graph_with(&ctx, &account, &client)
        .await
        .expect("run older batch");
    assert_eq!(older_execution.failed, 1, "the old partial batch retried");

    let evidence = ctx
        .db
        .read(|db| {
            Ok(db.query_row(
                "SELECT move_target_remote_id, move_target_op_id, reconcile_op_id
                   FROM message WHERE id='original-id'",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )?)
        })
        .await
        .expect("row evidence");
    assert_eq!(
        evidence.2.as_deref(),
        Some(newer.as_str()),
        "newer reconcile owner should remain recorded"
    );
    assert_eq!(
        evidence.0, None,
        "the old batch re-installed its stale destination"
    );

    mock.push_delta_removed("m-earlier");
    seed_graph_delta_cursor(&ctx, &mock.base, "acc", &archive).await;
    let result =
        nuncio_mail::sync::messages::sync_folder_graph_with(&ctx, &account, &archive, &client)
            .await
            .expect("sync after an earlier attempt's retry");
    assert!(
        result.deleted_ids.is_empty(),
        "the old tombstone deleted the new pending projection: {:?}",
        result.deleted_ids
    );
}

/// Regression: undoing an old NotExecuted move must not revert a newer move
/// that has already claimed the row's identity slot. Folder equality alone is
/// not ownership.
#[tokio::test]
async fn undo_of_an_old_refused_move_does_not_revert_a_new_trip() {
    let mock = MockGraph::start(0).await;
    mock.add_message("m-current", "2026-01-01T00:00:00Z", false);
    let ctx = context(
        StaticCreds::password("test@example.com", "secret"),
        Arc::new(RecordingNotifier::default()),
    )
    .await;
    let account = graph_account("acc", 0);
    insert_account(&ctx, &account).await;
    let inbox = Folder::new("inbox", "acc", "Inbox", FolderRole::Inbox);
    let archive = Folder::new("archive", "acc", "Archive", FolderRole::Archive);
    seed_graph_folder(&ctx, "acc", &inbox).await;
    seed_graph_folder(&ctx, "acc", &archive).await;
    seed_bound_graph_row(
        &ctx,
        &inbox,
        "proj",
        "m-current",
        "shared@graph.test",
        false,
    )
    .await;
    ctx.db
        .write(|db| {
            repo::sync_state::set(
                db,
                "acc",
                "mail",
                &format!(
                    "graph_remote:inbox:{}",
                    nuncio_mail::sync::messages::synthetic_uid("proj")
                ),
                "m-current",
            )
        })
        .await
        .expect("seed alias");
    let graph_inbox = ctx
        .db
        .read(|db| repo::sync_state::get(db, "acc", "mail", "graph_folder:Inbox"))
        .await
        .expect("graph folder")
        .expect("graph inbox id");
    mock.set_parent_folder_response(Some(&graph_inbox));

    let service = MailService::new(ctx.clone());
    let old = service
        .message_act(
            &["proj".into()],
            &MessageAction::Move {
                folder_id: "archive".into(),
            },
        )
        .await
        .expect("queue first move");
    let client = nuncio_mail::graph::GraphMailClient::with_base(mock.base.clone());
    let execution = ops::executor::run_due_graph_with(&ctx, &account, &client)
        .await
        .expect("run refused move");
    assert_eq!(execution.failed, 1);
    let restored = ctx
        .db
        .read(|db| repo::messages::get_required(db, "proj"))
        .await
        .expect("restored row");
    assert_eq!(restored.summary.folder_id, "inbox");
    assert!(
        restored.uid.is_some(),
        "the definitive refusal was recovered"
    );

    service
        .message_act(
            &["proj".into()],
            &MessageAction::Move {
                folder_id: "archive".into(),
            },
        )
        .await
        .expect("queue second move");
    let owner_before = ctx
        .db
        .read(|db| repo::messages::move_target_owner(db, "proj"))
        .await
        .expect("owner")
        .expect("the new trip claimed the slot");
    assert!(service.message_undo(&old).await.expect("undo old move"));

    let (row, owner) = ctx
        .db
        .read(|db| {
            Ok((
                repo::messages::get_required(db, "proj")?,
                repo::messages::move_target_owner(db, "proj")?,
            ))
        })
        .await
        .expect("row and owner");
    assert_eq!(
        (row.summary.folder_id, owner),
        ("archive".into(), Some(owner_before)),
        "NotExecuted restore trusted folder equality and let an old Undo replace the newer move's projection"
    );
}
