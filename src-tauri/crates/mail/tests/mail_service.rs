//! Integration tests for `nuncio_mail::MailService` against the in-process
//! mock IMAP / SMTP servers and an in-memory database.

mod support;

use std::sync::Arc;
use std::time::Duration;

use nuncio_core::models::{
    Account, Address, MailFilter, MailRule, MailScope, MailSettingsPatch, MessageAction,
    MessageListQuery, ReplyMode, RuleAction, RuleActionKind, RuleCondition, RuleField, RuleOp,
    SearchQuery, ServerConfig, SettingsPatch, Signature,
};
use nuncio_core::traits::MailAuth;
use nuncio_core::{repo, CoreContext};
use nuncio_mail::{ops, MailService};
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
    assert!(
        detail.summary.seen,
        "markReadMode onOpen marks the message read"
    );
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
    let payload: ops::AppendToSentOp =
        repo::pending_ops::payload(&append).expect("append payload");
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
    std::fs::write(&path, b"Subject: retry\r\nTo: bob@example.com\r\n\r\nbody\r\n")
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
    service.folder_empty(&junk).await.expect("junk can be emptied");
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
