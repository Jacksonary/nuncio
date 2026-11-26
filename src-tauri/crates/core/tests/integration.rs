//! End-to-end tests for the core repositories against an in-memory database.

use chrono::{DateTime, TimeZone, Utc};
use nuncio_core::models::*;
use nuncio_core::{repo, Db};

fn ts(day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, day, hour, 0, 0).unwrap()
}

fn setup() -> Db {
    let db = Db::open_in_memory().expect("in-memory db");
    db.migrate_core().expect("migrate core");
    db.migrate_core().expect("migrate core again");
    db
}

fn add_account(db: &Db, id: &str, email: &str, unified: bool) -> Account {
    let account = Account {
        id: id.into(),
        provider: ProviderKind::Imap,
        email: email.into(),
        display_name: email.split('@').next().unwrap_or(email).into(),
        color: "#123456".into(),
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        imap: Some(ServerConfig::new(
            "imap.example.com",
            993,
            Security::Tls,
            email,
        )),
        smtp: Some(ServerConfig::new(
            "smtp.example.com",
            587,
            Security::Starttls,
            email,
        )),
        pop3: None,
        caldav_url: None,
        carddav_url: None,
        sent_copy: None,
        sync_window_days: 90,
        mail_enabled: true,
        calendar_enabled: true,
        contacts_enabled: true,
        tasks_enabled: true,
        notifications_enabled: true,
        include_in_unified: unified,
        sort_order: 1,
        state: AccountState::Ok,
        state_message: None,
        capabilities: ProviderCapabilities::defaults_for(ProviderKind::Imap),
        created_at: ts(1, 0),
    };
    db.write_sync(|conn| repo::accounts::insert(conn, &account))
        .expect("insert account");
    account
}

fn add_folder(db: &Db, id: &str, account_id: &str, path: &str, role: FolderRole) -> Folder {
    let folder = Folder {
        id: id.into(),
        account_id: account_id.into(),
        remote_path: path.into(),
        ..Folder::new(id, account_id, path, role)
    };
    db.write_sync(|conn| repo::folders::upsert_remote(conn, &folder))
        .expect("upsert folder")
}

#[allow(clippy::too_many_arguments)]
fn make_message(
    id: &str,
    account_id: &str,
    folder_id: &str,
    thread_id: &str,
    date: DateTime<Utc>,
    subject: &str,
    seen: bool,
    flagged: bool,
) -> StoredMessage {
    let mut summary = MessageSummary::new(id, account_id, folder_id, thread_id, date);
    summary.subject = subject.into();
    summary.preview = format!("preview of {subject}");
    summary.seen = seen;
    summary.flagged = flagged;
    summary.r#from = Some(Address::with_name("Alice", "alice@example.com"));
    summary.to = vec![Address::with_name("Me", "me@example.com")];
    summary.size = 1024;
    StoredMessage::new(summary)
}

fn add_message(db: &Db, message: &StoredMessage) {
    db.write_sync(|conn| repo::messages::upsert(conn, message))
        .expect("upsert message");
}

#[test]
fn migrations_are_idempotent_and_seed_builtins() {
    let db = setup();
    db.read_sync(|conn| {
        let account = repo::accounts::get_required(conn, "local")?;
        assert_eq!(account.provider, ProviderKind::Local);
        assert!(!account.mail_enabled);
        assert!(account.calendar_enabled && account.contacts_enabled && account.tasks_enabled);

        let calendars = repo::calendars::list(conn)?;
        assert!(calendars
            .iter()
            .any(|calendar| calendar.id == "local-calendar" && calendar.name == "日历"));

        let books = repo::address_books::list(conn, Some("local"))?;
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].name, "联系人");

        let lists = repo::task_lists::list(conn)?;
        assert!(lists
            .iter()
            .any(|list| list.id == "local-tasks" && list.name == "任务"));

        let settings = repo::settings::load(conn)?.expect("settings row");
        assert_eq!(settings, AppSettings::default());

        assert!(repo::accounts::count(conn)? >= 1);
        Ok(())
    })
    .expect("seed checks");
}

#[test]
fn accounts_identity_signature_and_patch() {
    let db = setup();
    add_account(&db, "acc", "me@example.com", true);
    db.write_sync(|conn| {
        let mut identity = Identity {
            id: String::new(),
            account_id: "acc".into(),
            name: "Me".into(),
            email: "me@example.com".into(),
            reply_to: None,
            signature_id: None,
            is_default: true,
        };
        let saved = repo::identities::upsert(conn, &mut identity)?;
        assert!(!saved.id.is_empty());
        assert_eq!(repo::identities::list(conn, Some("acc"))?.len(), 1);
        assert_eq!(
            repo::identities::default_for_account(conn, "acc")?
                .expect("default")
                .name,
            "Me"
        );

        let mut signature = Signature {
            id: String::new(),
            account_id: None,
            name: "Main".into(),
            html: "<p>hi</p>".into(),
            use_for_new: true,
            use_for_reply: false,
        };
        let saved = repo::signatures::upsert(conn, &mut signature)?;
        assert!(repo::signatures::default_for_new(conn, "acc")?.is_some());
        assert!(repo::signatures::default_for_reply(conn, "acc")?.is_none());
        repo::signatures::delete(conn, &saved.id)?;

        let patch: AccountPatch = serde_json::from_str(
            r#"{"displayName":"New name","includeInUnified":false,"imap":null}"#,
        )
        .expect("patch");
        let account = repo::accounts::update_patch(conn, "acc", &patch)?;
        assert_eq!(account.display_name, "New name");
        assert!(!account.include_in_unified);
        assert!(account.imap.is_none());
        // untouched field
        assert_eq!(account.smtp.as_ref().map(|s| s.port), Some(587));

        // sent_copy round-trips through the column and serializes as camelCase.
        let patch: AccountPatch =
            serde_json::from_str(r#"{"sentCopy":"server"}"#).expect("sent copy patch");
        let account = repo::accounts::update_patch(conn, "acc", &patch)?;
        assert_eq!(account.sent_copy, Some(SentCopyMode::Server));
        let persisted = repo::accounts::get_required(conn, "acc")?;
        assert_eq!(persisted.sent_copy, Some(SentCopyMode::Server));

        let mut value = serde_json::to_value(&persisted).expect("account json");
        assert_eq!(value["sentCopy"], serde_json::json!("server"));
        // An older payload without the field means "automatic".
        value.as_object_mut().expect("object").remove("sentCopy");
        let old: Account = serde_json::from_value(value).expect("account without sentCopy");
        assert_eq!(old.sent_copy, None);

        // A value written by a newer build must not break the row mapping.
        conn.execute(
            "UPDATE account SET sent_copy = 'bogus' WHERE id = 'acc'",
            [],
        )?;
        assert_eq!(repo::accounts::get_required(conn, "acc")?.sent_copy, None);

        // null resets the setting to automatic.
        let patch: AccountPatch =
            serde_json::from_str(r#"{"sentCopy":null}"#).expect("clear sent copy");
        let account = repo::accounts::update_patch(conn, "acc", &patch)?;
        assert_eq!(account.sent_copy, None);

        repo::accounts::reorder(conn, &["acc".to_string(), "local".to_string()])?;
        let accounts = repo::accounts::list(conn)?;
        assert_eq!(accounts.first().map(|a| a.id.as_str()), Some("acc"));
        Ok(())
    })
    .expect("accounts");
}

#[test]
fn message_pagination_filter_and_cursor() {
    let db = setup();
    add_account(&db, "acc", "me@example.com", true);
    let inbox = add_folder(&db, "inbox", "acc", "INBOX", FolderRole::Inbox);

    for (index, day) in [1u32, 2, 3, 4, 5].iter().enumerate() {
        let mut message = make_message(
            &format!("m{index}"),
            "acc",
            &inbox.id,
            &format!("t{}", index % 2),
            ts(*day, 9),
            &format!("Message {index}"),
            index % 2 == 0,
            index == 4,
        );
        message.summary.has_attachments = index == 3;
        add_message(&db, &message);
    }

    db.read_sync(|conn| {
        let first = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::folder("inbox"))
                .with_conversation(false)
                .with_limit(2),
        )?;
        assert_eq!(first.total, 5);
        assert_eq!(first.items.len(), 2);
        assert!(first.next_cursor.is_some());
        // newest first
        assert_eq!(first.items[0].id, "m4");
        assert_eq!(first.items[1].id, "m3");

        let second = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::folder("inbox"))
                .with_conversation(false)
                .with_limit(2)
                .with_cursor(first.next_cursor.clone()),
        )?;
        assert_eq!(second.items.len(), 2);
        assert_eq!(second.items[0].id, "m2");
        assert_eq!(second.items[1].id, "m1");

        let third = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::folder("inbox"))
                .with_conversation(false)
                .with_limit(2)
                .with_cursor(second.next_cursor.clone()),
        )?;
        assert_eq!(third.items.len(), 1);
        assert_eq!(third.items[0].id, "m0");
        assert!(third.next_cursor.is_none());

        let unread = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::folder("inbox"))
                .with_conversation(false)
                .with_filter(MailFilter::Unread)
                .with_limit(50),
        )?;
        assert_eq!(unread.total, 2); // m1, m3

        let attachments = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::folder("inbox"))
                .with_conversation(false)
                .with_filter(MailFilter::Attachments)
                .with_limit(50),
        )?;
        assert_eq!(attachments.total, 1);
        assert_eq!(attachments.items[0].id, "m3");

        let mentioned = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::folder("inbox"))
                .with_conversation(false)
                .with_filter(MailFilter::MentionsMe)
                .with_limit(50),
        )?;
        assert_eq!(mentioned.total, 5);

        // Invalid cursor is rejected.
        let bad = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::folder("inbox"))
                .with_conversation(false)
                .with_cursor(Some("not-a-cursor".into())),
        );
        assert!(bad.is_err());
        Ok(())
    })
    .expect("pagination");
}

#[test]
fn conversation_grouping_and_thread_view() {
    let db = setup();
    add_account(&db, "acc", "me@example.com", true);
    let inbox = add_folder(&db, "inbox", "acc", "INBOX", FolderRole::Inbox);

    let dates = [(1u32, "a"), (2, "b"), (3, "c")];
    for (day, id) in dates {
        add_message(
            &db,
            &make_message(
                id,
                "acc",
                &inbox.id,
                "thread-1",
                ts(day, 10),
                &format!("Re: Project {id}"),
                false,
                false,
            ),
        );
    }
    add_message(
        &db,
        &make_message(
            "d",
            "acc",
            &inbox.id,
            "thread-2",
            ts(2, 11),
            "Other",
            true,
            false,
        ),
    );

    db.read_sync(|conn| {
        let page = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::folder("inbox"))
                .with_conversation(true)
                .with_limit(10),
        )?;
        assert_eq!(page.total, 2, "two conversations");
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].thread_id, "thread-1");
        assert_eq!(page.items[0].id, "c", "latest message of the conversation");
        assert_eq!(page.items[0].thread_count, 3);
        assert_eq!(page.items[0].thread_unread, 3);
        assert_eq!(page.items[1].thread_count, 1);

        // Opening the thread returns oldest -> newest with aggregate counts.
        let members = repo::messages::thread(conn, "acc", "thread-1")?;
        assert_eq!(members.len(), 3);
        assert_eq!(members[0].id, "a");
        assert_eq!(members[2].id, "c");
        assert!(members.iter().all(|member| member.thread_count == 3));

        // Paging one conversation at a time.
        let first = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::folder("inbox"))
                .with_conversation(true)
                .with_limit(1),
        )?;
        assert_eq!(first.items.len(), 1);
        let second = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::folder("inbox"))
                .with_conversation(true)
                .with_limit(1)
                .with_cursor(first.next_cursor),
        )?;
        assert_eq!(second.items.len(), 1);
        assert_eq!(second.items[0].thread_id, "thread-2");
        Ok(())
    })
    .expect("conversations");
}

#[test]
fn unified_scope_respects_account_setting() {
    let db = setup();
    add_account(&db, "a1", "one@example.com", true);
    add_account(&db, "a2", "two@example.com", false);
    let inbox1 = add_folder(&db, "i1", "a1", "INBOX", FolderRole::Inbox);
    let inbox2 = add_folder(&db, "i2", "a2", "INBOX", FolderRole::Inbox);

    add_message(
        &db,
        &make_message("m1", "a1", &inbox1.id, "t1", ts(3, 9), "One", false, true),
    );
    add_message(
        &db,
        &make_message("m2", "a2", &inbox2.id, "t2", ts(4, 9), "Two", false, true),
    );

    db.read_sync(|conn| {
        let inbox = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::unified(FolderRole::Inbox))
                .with_conversation(false)
                .with_limit(50),
        )?;
        assert_eq!(inbox.total, 1);
        assert_eq!(inbox.items[0].id, "m1");

        let flagged = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::unified(FolderRole::Flagged))
                .with_conversation(false)
                .with_limit(50),
        )?;
        assert_eq!(flagged.total, 1);
        assert_eq!(flagged.items[0].id, "m1");

        let counts = repo::folders::unified_counts(conn)?;
        assert_eq!(
            counts.inbox, 1,
            "only unread inbox messages of unified accounts"
        );
        assert_eq!(counts.flagged, 1);

        // Non-unified listings still see both accounts.
        let all = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::unified(FolderRole::All))
                .with_conversation(false)
                .with_limit(50),
        )?;
        assert_eq!(all.total, 1);
        Ok(())
    })
    .expect("unified");
}

#[test]
fn unified_outbox_scope_lists_queued_items() {
    let db = setup();
    add_account(&db, "a1", "one@example.com", true);
    add_account(&db, "a2", "two@example.com", false);

    db.write_sync(|conn| {
        let mut newer = OutboxItem::new(
            "a1",
            None,
            vec![Address::with_name("You", "you@example.com")],
            "Queued one",
            "/tmp/one.eml",
        );
        newer.created_at = ts(5, 10);
        repo::outbox::enqueue(conn, &newer)?;

        let mut older = OutboxItem::new(
            "a1",
            None,
            vec![Address::new("old@example.com")],
            "Queued older",
            "/tmp/older.eml",
        );
        older.created_at = ts(4, 10);
        repo::outbox::enqueue(conn, &older)?;

        // Not part of the unified inbox and already sent: never listed.
        let mut other = OutboxItem::new(
            "a2",
            None,
            vec![Address::new("x@example.com")],
            "Other account",
            "/tmp/other.eml",
        );
        other.created_at = ts(6, 10);
        repo::outbox::enqueue(conn, &other)?;
        let mut sent = OutboxItem::new(
            "a1",
            None,
            vec![Address::new("x@example.com")],
            "Already sent",
            "/tmp/sent.eml",
        );
        sent.created_at = ts(7, 10);
        let sent = repo::outbox::enqueue(conn, &sent)?;
        repo::outbox::mark_sent(conn, &sent.id)?;
        Ok(())
    })
    .expect("queue outbox items");

    db.read_sync(|conn| {
        let page = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::unified(FolderRole::Outbox))
                .with_conversation(false)
                .with_limit(50),
        )?;
        assert_eq!(page.total, 2);
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].subject, "Queued one");
        assert_eq!(page.items[1].subject, "Queued older");
        let first = &page.items[0];
        assert_eq!(first.account_id, "a1");
        assert_eq!(first.to[0].email, "you@example.com");
        assert_eq!(
            first.r#from.as_ref().map(|from| from.email.as_str()),
            Some("one@example.com")
        );
        assert!(first.seen, "queued mail is not unread");
        assert_eq!(first.thread_count, 1);
        assert_eq!(first.thread_unread, 0);
        assert!(first.folder_id.starts_with("outbox:a1"));
        assert_eq!(page.next_cursor, None);

        // Cursor pagination follows the created-at order.
        let first_page = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::unified(FolderRole::Outbox))
                .with_conversation(false)
                .with_limit(1),
        )?;
        assert_eq!(first_page.items.len(), 1);
        assert_eq!(first_page.items[0].subject, "Queued one");
        assert_eq!(first_page.total, 2);
        let cursor = first_page.next_cursor.expect("next cursor");
        let second_page = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::unified(FolderRole::Outbox))
                .with_conversation(false)
                .with_limit(1)
                .with_cursor(Some(cursor)),
        )?;
        assert_eq!(second_page.items.len(), 1);
        assert_eq!(second_page.items[0].subject, "Queued older");
        assert_eq!(second_page.next_cursor, None);
        Ok(())
    })
    .expect("outbox scope");
}

#[test]
fn search_cjk_body_and_structured_filters() {
    let db = setup();
    add_account(&db, "acc", "me@example.com", true);
    let inbox = add_folder(&db, "inbox", "acc", "INBOX", FolderRole::Inbox);

    let mut first = make_message(
        "m1",
        "acc",
        &inbox.id,
        "t1",
        ts(2, 9),
        "项目会议安排",
        false,
        false,
    );
    first.summary.preview = "你好世界，请准时参加".into();
    add_message(&db, &first);

    add_message(
        &db,
        &make_message(
            "m2",
            "acc",
            &inbox.id,
            "t2",
            ts(10, 9),
            "Weekly report",
            true,
            true,
        ),
    );

    let mut third = make_message(
        "m3",
        "acc",
        &inbox.id,
        "t3",
        ts(20, 9),
        "Another 中文 subject",
        false,
        false,
    );
    third.summary.r#from = Some(Address::with_name("Bob", "bob@other.com"));
    third.summary.has_attachments = true;
    add_message(&db, &third);

    db.write_sync(|conn| {
        repo::messages::set_body(
            conn,
            "m1",
            Some("/tmp/m1.eml"),
            Some("这是深度学习笔记"),
            None,
        )?;
        Ok(())
    })
    .expect("body");

    db.read_sync(|conn| {
        let by_subject_cjk = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::search(SearchQuery::text("项目")))
                .with_conversation(false)
                .with_limit(50),
        )?;
        assert_eq!(by_subject_cjk.total, 1);
        assert_eq!(by_subject_cjk.items[0].id, "m1");

        let by_preview_cjk = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::search(SearchQuery::text("你好")))
                .with_conversation(false)
                .with_limit(50),
        )?;
        assert_eq!(by_preview_cjk.total, 1);

        let by_body = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::search(SearchQuery::text("深度")))
                .with_conversation(false)
                .with_limit(50),
        )?;
        assert_eq!(by_body.total, 1, "FTS indexes cached body text");

        let latin = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::search(SearchQuery::text("weekly")))
                .with_conversation(false)
                .with_limit(50),
        )?;
        assert_eq!(latin.total, 1);
        assert_eq!(latin.items[0].id, "m2");

        let structured = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::search(SearchQuery {
                text: String::new(),
                account_id: Some("acc".into()),
                folder_id: Some(inbox.id.clone()),
                from: Some("bob@".into()),
                to: None,
                subject: None,
                has_attachment: Some(true),
                unread: Some(true),
                flagged: None,
                date_from: Some("2026-11-01".into()),
                date_to: Some("2026-11-30".into()),
                include_server: false,
            }))
            .with_conversation(false)
            .with_limit(50),
        )?;
        assert_eq!(structured.total, 1);
        assert_eq!(structured.items[0].id, "m3");

        let date_window = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::search(SearchQuery {
                date_from: Some("2026-11-05".into()),
                date_to: Some("2026-11-15".into()),
                ..SearchQuery::default()
            }))
            .with_conversation(false)
            .with_limit(50),
        )?;
        assert_eq!(date_window.total, 1);
        assert_eq!(date_window.items[0].id, "m2");

        // Search + conversation grouping still groups threads.
        let grouped = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::search(SearchQuery::text("项目")))
                .with_conversation(true)
                .with_limit(50),
        )?;
        assert_eq!(grouped.total, 1);
        Ok(())
    })
    .expect("search");
}

/// Moving a row locally and moving it optimistically are different states.
///
/// Only a move whose server operation was queued may clear the folder-local
/// identity and set `target_pending`; a plain local move (Local/POP3, folder
/// migration, rollback) must keep both, or every later action refuses it.
#[test]
fn plain_and_pending_moves_keep_distinct_identity_state() {
    let db = setup();
    add_account(&db, "acc", "me@example.com", true);
    let inbox = add_folder(&db, "inbox", "acc", "INBOX", FolderRole::Inbox);
    let archive = add_folder(&db, "archive", "acc", "Archive", FolderRole::Archive);
    let mut message = make_message("m1", "acc", &inbox.id, "t1", ts(1, 9), "One", false, false);
    message.uid = Some(42);
    message.uidvalidity = Some(7);
    add_message(&db, &message);

    db.write_sync(|conn| {
        repo::messages::move_to_folder(conn, &["m1".to_string()], &archive.id)?;
        let moved = repo::messages::get_required(conn, "m1")?;
        assert_eq!(moved.summary.folder_id, archive.id);
        assert_eq!(moved.uid, Some(42), "a plain move keeps the source UID");
        assert_eq!(moved.uidvalidity, Some(7));
        assert!(
            repo::messages::target_pending_ids(conn, &["m1".to_string()])?.is_empty(),
            "a plain move must not wait for a target sync"
        );

        repo::messages::move_to_folder_pending(conn, &["m1".to_string()], &inbox.id)?;
        let moved = repo::messages::get_required(conn, "m1")?;
        assert_eq!(moved.summary.folder_id, inbox.id);
        assert!(moved.uid.is_none(), "an optimistic move clears the UID");
        assert!(moved.uidvalidity.is_none());
        assert_eq!(
            repo::messages::target_pending_ids(conn, &["m1".to_string()])?,
            vec!["m1".to_string()]
        );
        Ok(())
    })
    .expect("plain and pending moves");
}

/// A UIDVALIDITY reset drops cached metadata, but a row waiting for the server
/// to confirm a permanent delete is not cached metadata: dropping it would lose
/// its attachment records and FTS document and leave a refused operation with
/// nothing to restore. Its old UID must be cleared, though, because it belongs
/// to the replaced generation.
#[test]
fn uidvalidity_reset_preserves_pending_permanent_delete_rows() {
    let db = setup();
    add_account(&db, "acc", "me@example.com", true);
    let inbox = add_folder(&db, "inbox", "acc", "INBOX", FolderRole::Inbox);

    let mut target = make_message(
        "m1",
        "acc",
        "inbox",
        "t1",
        ts(1, 9),
        "Keep me",
        false,
        false,
    );
    target.uid = Some(7);
    target.uidvalidity = Some(3);
    target.summary.has_attachments = true;
    add_message(&db, &target);
    let mut other = make_message(
        "m2",
        "acc",
        "inbox",
        "t2",
        ts(1, 10),
        "Drop me",
        false,
        false,
    );
    other.uid = Some(8);
    other.uidvalidity = Some(3);
    add_message(&db, &other);

    db.write_sync(|conn| {
        let mut attachment = Attachment {
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
        repo::messages::set_body(
            conn,
            "m1",
            Some("/tmp/nuncio-m1.eml"),
            Some("pending delete body token"),
            None,
        )?;
        repo::folders::set_sync_flags(conn, &inbox.id, Some(3), Some(9), None)?;
        repo::messages::hide_for_permanent_delete(conn, &["m1".to_string()])?;

        repo::messages::clear_folder(conn, &inbox.id)?;

        let kept = repo::messages::get_required(conn, "m1")?;
        assert!(kept.is_deleted_local, "the row stays hidden, not deleted");
        assert!(kept.uid.is_none(), "the old generation's UID is cleared");
        assert!(kept.uidvalidity.is_none());
        assert_eq!(
            repo::messages::target_pending_ids(conn, &["m1".to_string()])?,
            vec!["m1".to_string()],
            "no UID action may run until a sync rebinds the row"
        );
        assert_eq!(
            repo::attachments::list(conn, "m1")?.len(),
            1,
            "attachment records survive the reset"
        );
        let body: String = conn.query_row(
            "SELECT body FROM fts_mail WHERE message_id = 'm1'",
            [],
            |row| row.get(0),
        )?;
        assert!(
            body.contains("pending delete body token"),
            "the FTS document survives the reset"
        );
        // A row that is not waiting for a delete is still dropped.
        assert!(repo::messages::get(conn, "m2")?.is_none());
        Ok(())
    })
    .expect("uidvalidity reset");
}

/// A row left waiting for a targeted reconciliation (a permanent-delete
/// rollback that could not trust its UID) must survive the UIDVALIDITY reset
/// too. Otherwise the reset erases the very row the rollback just restored,
/// together with its attachments and FTS document.
#[test]
fn uidvalidity_reset_preserves_rows_awaiting_reconciliation() {
    let db = setup();
    add_account(&db, "acc", "me@example.com", true);
    let inbox = add_folder(&db, "inbox", "acc", "INBOX", FolderRole::Inbox);

    let mut target = make_message(
        "m1",
        "acc",
        "inbox",
        "t1",
        ts(1, 9),
        "Restore me",
        false,
        false,
    );
    target.uid = Some(7);
    target.uidvalidity = Some(3);
    target.summary.has_attachments = true;
    add_message(&db, &target);

    db.write_sync(|conn| {
        let mut attachment = Attachment {
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
        repo::messages::set_body(conn, "m1", None, Some("reconcile token"), None)?;
        repo::folders::set_sync_flags(conn, &inbox.id, Some(3), Some(9), None)?;
        repo::messages::mark_reconcile_owned(
            conn,
            &["m1".to_string()],
            repo::messages::RECONCILE_REBIND,
            "op-rebind",
            1,
            Some(3),
            Some(3),
        )?;

        repo::messages::clear_folder(conn, &inbox.id)?;

        let kept = repo::messages::get_required(conn, "m1")?;
        assert!(kept.uid.is_none(), "the old generation's UID is cleared");
        assert!(kept.uidvalidity.is_none());
        assert_eq!(
            repo::messages::pending_identity_rows(conn, &inbox.id)?.len(),
            1,
            "the row still waits for its targeted reconciliation"
        );
        assert_eq!(repo::attachments::list(conn, "m1")?.len(), 1);
        let body: String = conn.query_row(
            "SELECT body FROM fts_mail WHERE message_id = 'm1'",
            [],
            |row| row.get(0),
        )?;
        assert!(body.contains("reconcile token"));
        Ok(())
    })
    .expect("uidvalidity reset with awaiting row");
}

#[test]
fn flags_move_delete_and_counts() {
    let db = setup();
    add_account(&db, "acc", "me@example.com", true);
    let inbox = add_folder(&db, "inbox", "acc", "INBOX", FolderRole::Inbox);
    let archive = add_folder(&db, "archive", "acc", "Archive", FolderRole::Archive);

    add_message(
        &db,
        &make_message("m1", "acc", &inbox.id, "t1", ts(1, 9), "One", false, false),
    );
    add_message(
        &db,
        &make_message("m2", "acc", &inbox.id, "t2", ts(2, 9), "Two", false, false),
    );

    db.write_sync(|conn| {
        repo::folders::recompute_counts(conn, &inbox.id)?;
        // flags
        repo::messages::set_flags(
            conn,
            &["m1".to_string()],
            Some(true),
            Some(true),
            None,
            None,
        )?;
        let stored = repo::messages::get_required(conn, "m1")?;
        assert!(stored.summary.seen && stored.summary.flagged);
        let inbox_folder = repo::folders::get_required(conn, &inbox.id)?;
        assert_eq!(inbox_folder.unread_count, 1);
        assert_eq!(inbox_folder.total_count, 2);

        // move
        repo::messages::move_to_folder(conn, &["m2".to_string()], &archive.id)?;
        assert_eq!(repo::folders::get_required(conn, &inbox.id)?.total_count, 1);
        assert_eq!(
            repo::folders::get_required(conn, &archive.id)?.total_count,
            1
        );
        assert!(repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::folder("archive")).with_conversation(false)
        )?
        .items
        .iter()
        .any(|m| m.id == "m2"));

        // mark all read
        repo::messages::mark_all_read(conn, &archive.id)?;
        assert_eq!(
            repo::folders::get_required(conn, &archive.id)?.unread_count,
            0
        );

        // soft delete hides the message from lists
        repo::messages::delete_local(conn, &["m2".to_string()], false)?;
        let page = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::folder("archive")).with_conversation(false),
        )?;
        assert!(page.items.is_empty());

        // permanent delete removes the row and its search doc
        repo::messages::delete_local(conn, &["m1".to_string()], true)?;
        assert!(repo::messages::get(conn, "m1")?.is_none());
        assert_eq!(repo::folders::get_required(conn, &inbox.id)?.total_count, 0);
        Ok(())
    })
    .expect("mutations");
}

#[test]
fn attachments_drafts_and_outbox() {
    let db = setup();
    add_account(&db, "acc", "me@example.com", true);
    let inbox = add_folder(&db, "inbox", "acc", "INBOX", FolderRole::Inbox);
    add_message(
        &db,
        &make_message("m1", "acc", &inbox.id, "t1", ts(1, 9), "One", false, false),
    );

    db.write_sync(|conn| {
        let mut attachment = Attachment {
            id: String::new(),
            message_id: "m1".into(),
            filename: "report.pdf".into(),
            mime_type: "application/pdf".into(),
            size: 42,
            content_id: None,
            is_inline: false,
            is_downloaded: false,
        };
        let saved = repo::attachments::insert(conn, &mut attachment)?;
        assert_eq!(repo::attachments::list(conn, "m1")?.len(), 1);
        repo::attachments::set_downloaded(conn, &saved.id, "/tmp/report.pdf")?;
        assert_eq!(
            repo::attachments::local_path(conn, &saved.id)?.as_deref(),
            Some("/tmp/report.pdf")
        );

        let draft = Draft {
            id: None,
            account_id: "acc".into(),
            identity_id: None,
            from: Address::new("me@example.com"),
            to: vec![Address::new("you@example.com")],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Hello".into(),
            html: "<p>Hi</p>".into(),
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
        };
        let saved = repo::drafts::save(conn, &draft)?;
        assert!(saved.id.is_some());
        assert!(saved.updated_at.is_some());
        let loaded = repo::drafts::get_required(conn, saved.id.as_deref().unwrap())?;
        assert_eq!(loaded.subject, "Hello");
        assert_eq!(loaded.to.len(), 1);
        repo::drafts::set_remote_id(conn, saved.id.as_deref().unwrap(), Some("remote-1"))?;
        assert!(repo::drafts::find_by_remote_id(conn, "remote-1")?.is_some());
        assert_eq!(repo::drafts::count(conn)?, 1);
        repo::drafts::delete(conn, saved.id.as_deref().unwrap())?;
        assert_eq!(repo::drafts::count(conn)?, 0);

        let item = OutboxItem::new(
            "acc",
            None,
            vec![Address::new("you@example.com")],
            "Queued",
            "/tmp/queued.eml",
        );
        let item = repo::outbox::enqueue(conn, &item)?;
        assert_eq!(repo::outbox::pending_count(conn, Some("acc"))?, 1);
        assert_eq!(repo::outbox::next_due(conn, ts(1, 10), 10)?.len(), 1);
        let state = repo::outbox::mark_failed(conn, &item.id, "smtp down", ts(1, 10), 3)?;
        assert_eq!(state, OutboxState::Pending);
        let retried = repo::outbox::get_required(conn, &item.id)?;
        assert_eq!(retried.attempts, 1);
        assert!(retried.next_retry_at.is_some());
        // not due yet
        assert!(repo::outbox::next_due(conn, ts(1, 10), 10)?.is_empty());
        repo::outbox::mark_sent(conn, &item.id)?;
        assert_eq!(repo::outbox::pending_count(conn, None)?, 0);
        Ok(())
    })
    .expect("attachments/drafts/outbox");
}

#[test]
fn smart_task_lists_and_counts() {
    let db = setup();
    db.write_sync(|conn| {
        let today = "2026-11-12".to_string();
        let input = |title: &str,
                     importance: Importance,
                     due: Option<&str>,
                     my_day: Option<&str>| TaskInput {
            title: title.into(),
            notes: String::new(),
            importance,
            due_date: due.map(str::to_string),
            reminder_at: None,
            recurrence: None,
            my_day_date: my_day.map(str::to_string),
            steps: vec![TaskStep {
                id: "step-1".into(),
                title: "first".into(),
                is_completed: false,
                sort_order: 0,
            }],
        };
        let first = repo::tasks::create(
            conn,
            "local-tasks",
            &input(
                "Important today",
                Importance::High,
                Some("2026-11-12"),
                Some("2026-11-12"),
            ),
        )?;
        let second = repo::tasks::create(
            conn,
            "local-tasks",
            &input(
                "Planned later",
                Importance::Normal,
                Some("2026-11-20"),
                None,
            ),
        )?;
        repo::tasks::create(
            conn,
            "local-tasks",
            &input("Someday", Importance::Normal, None, None),
        )?;
        assert_eq!(first.steps.len(), 1);

        // Mark one complete.
        repo::tasks::set_completed(conn, &second.id, true)?;

        let counts = repo::tasks::smart_counts(conn, &today)?;
        assert_eq!(counts.my_day, 1);
        assert_eq!(counts.important, 1);
        assert_eq!(counts.planned, 1, "only incomplete planned tasks");
        assert_eq!(counts.all, 2);
        assert_eq!(counts.completed, 1);
        assert_eq!(counts.flagged_email, 0);

        let my_day = repo::tasks::query(conn, &TaskQuery::smart(SmartList::MyDay, today.clone()))?;
        assert_eq!(my_day.len(), 1);
        assert_eq!(my_day[0].title, "Important today");

        let important =
            repo::tasks::query(conn, &TaskQuery::smart(SmartList::Important, today.clone()))?;
        assert_eq!(important.len(), 1);

        let all = repo::tasks::query(conn, &TaskQuery::smart(SmartList::All, today.clone()))?;
        assert_eq!(all.len(), 2, "completed excluded unless requested");
        let mut include_completed = TaskQuery::smart(SmartList::All, today.clone());
        include_completed.include_completed = true;
        assert_eq!(repo::tasks::query(conn, &include_completed)?.len(), 3);

        let completed =
            repo::tasks::query(conn, &TaskQuery::smart(SmartList::Completed, today.clone()))?;
        assert_eq!(completed.len(), 1);
        assert!(completed[0].completed_at.is_some());

        // Flagged email smart list. The link only counts while the message
        // row exists and is not in the trash.
        let folder = repo::folders::create(conn, "local", None, "Temp")?;
        let summary = MessageSummary::new("m1", "local", folder.id, "thread-m1", Utc::now());
        repo::messages::upsert(conn, &StoredMessage::new(summary))?;
        repo::tasks::set_linked_message(conn, &first.id, Some("m1"))?;
        let flagged = repo::tasks::query(
            conn,
            &TaskQuery::smart(SmartList::FlaggedEmail, today.clone()),
        )?;
        assert_eq!(flagged.len(), 1);
        assert_eq!(repo::tasks::smart_counts(conn, &today)?.flagged_email, 1);

        // A message in the trash drops out of the smart list immediately.
        repo::messages::delete_local(conn, &["m1".to_string()], false)?;
        assert_eq!(
            repo::tasks::query(
                conn,
                &TaskQuery::smart(SmartList::FlaggedEmail, today.clone())
            )?
            .len(),
            0
        );
        assert_eq!(repo::tasks::smart_counts(conn, &today)?.flagged_email, 0);

        // My Day setter + list incomplete count.
        repo::tasks::set_my_day(conn, &second.id, Some("2026-11-12"))?;
        let list = repo::task_lists::get_required(conn, "local-tasks")?;
        assert_eq!(list.incomplete_count, 2);

        repo::tasks::move_task(conn, &first.id, "local-tasks")?;
        assert_eq!(repo::tasks::count_in_list(conn, "local-tasks")?, 3);
        repo::tasks::set_importance(conn, &first.id, Importance::Low)?;
        assert_eq!(
            repo::tasks::get_required(conn, &first.id)?.importance,
            Importance::Low
        );

        let found = repo::tasks::search(conn, "someday")?;
        assert_eq!(found.len(), 1);

        repo::tasks::delete(conn, &first.id)?;
        assert_eq!(repo::tasks::count(conn)?, 2);
        Ok(())
    })
    .expect("tasks");
}

#[test]
fn calendars_events_range_and_reminders() {
    let db = setup();
    db.write_sync(|conn| {
        let calendar = repo::calendars::writable_default(conn)?.expect("local calendar");
        assert_eq!(calendar.id, "local-calendar");

        let mut master = CalendarEvent::new(
            "ev-1",
            &calendar.id,
            "local",
            "uid-1",
            "Weekly standup",
            "2026-11-02T09:00:00+08:00",
            "2026-11-02T09:30:00+08:00",
        );
        master.all_day = false;
        master.recurrence = Some(RecurrenceRule {
            freq: RecurrenceFreq::Weekly,
            interval: 1,
            by_weekday: vec![Weekday::Mo],
            ..RecurrenceRule::default()
        });
        let stored = StoredEvent {
            rrule: Some("FREQ=WEEKLY;BYDAY=MO".into()),
            ..StoredEvent::new(master)
        };
        repo::events::upsert(conn, &stored)?;

        let mut one_off = CalendarEvent::new(
            "ev-2",
            &calendar.id,
            "local",
            "uid-2",
            "Dentist",
            "2026-11-12T15:00:00Z",
            "2026-11-12T16:00:00Z",
        );
        one_off.reminders = vec![15];
        repo::events::upsert(conn, &StoredEvent::new(one_off))?;

        let outside = CalendarEvent::new(
            "ev-3",
            &calendar.id,
            "local",
            "uid-3",
            "Old",
            "2026-09-01T10:00:00Z",
            "2026-09-01T11:00:00Z",
        );
        repo::events::upsert(conn, &StoredEvent::new(outside))?;

        // Range includes the occurrence and all recurring masters.
        let range =
            repo::events::range(conn, "2026-11-10T00:00:00Z", "2026-11-17T00:00:00Z", None)?;
        let ids: Vec<&str> = range.iter().map(|event| event.event.id.as_str()).collect();
        assert!(
            ids.contains(&"ev-1"),
            "recurring master returned for expansion"
        );
        assert!(ids.contains(&"ev-2"));
        assert!(!ids.contains(&"ev-3"));

        assert!(repo::events::by_uid(conn, &calendar.id, "uid-1")?.is_some());
        assert!(repo::events::search(conn, "dentist", 10)?.len() == 1);
        assert_eq!(repo::events::count(conn)?, 3);

        // Detached exception inside the window is returned.
        let mut exception = repo::events::get_required(conn, "ev-1")?.event;
        exception.id = "ev-1-ex".into();
        exception.series_id = Some("ev-1".into());
        exception.recurrence_id = Some("2026-11-09T01:00:00Z".into());
        exception.recurrence = None;
        exception.start = "2026-11-09T10:00:00Z".into();
        exception.end = "2026-11-09T10:30:00Z".into();
        repo::events::upsert(conn, &StoredEvent::new(exception))?;
        let range =
            repo::events::range(conn, "2026-11-09T00:00:00Z", "2026-11-10T00:00:00Z", None)?;
        assert!(range.iter().any(|event| event.event.id == "ev-1-ex"));

        // A detached exception moved outside the window must still be returned:
        // the expander uses it to suppress the original occurrence (otherwise a
        // ghost occurrence remains at the old time).
        let mut moved = repo::events::get_required(conn, "ev-1")?.event;
        moved.id = "ev-1-moved".into();
        moved.series_id = Some("ev-1".into());
        moved.recurrence_id = Some("2026-11-16T01:00:00Z".into());
        moved.recurrence = None;
        moved.start = "2026-12-24T10:00:00Z".into();
        moved.end = "2026-12-24T10:30:00Z".into();
        repo::events::upsert(conn, &StoredEvent::new(moved))?;
        let range =
            repo::events::range(conn, "2026-11-10T00:00:00Z", "2026-11-17T00:00:00Z", None)?;
        assert!(
            range.iter().any(|event| event.event.id == "ev-1-moved"),
            "detached exception moved out of the window must still be returned"
        );

        // Reminder dismiss / snooze.
        repo::reminder_state::dismiss(conn, "ev-2", "2026-11-12T15:00:00Z")?;
        assert!(repo::reminder_state::is_suppressed(
            conn,
            "ev-2",
            "2026-11-12T15:00:00Z",
            ts(12, 14)
        )?);
        let until =
            repo::reminder_state::snooze(conn, "ev-2", "2026-11-12T15:00:00Z", 10, ts(12, 14))?;
        assert_eq!(until, ts(12, 14) + chrono::Duration::minutes(10));
        assert!(repo::reminder_state::is_suppressed(
            conn,
            "ev-2",
            "2026-11-12T15:00:00Z",
            ts(12, 14) + chrono::Duration::minutes(5)
        )?);
        assert!(!repo::reminder_state::is_suppressed(
            conn,
            "ev-2",
            "2026-11-12T15:00:00Z",
            ts(12, 14) + chrono::Duration::minutes(11)
        )?);

        // Delete series removes the master and exceptions.
        repo::events::delete_series(conn, "ev-1")?;
        assert!(repo::events::get(conn, "ev-1")?.is_none());
        assert!(repo::events::get(conn, "ev-1-ex")?.is_none());
        Ok(())
    })
    .expect("events");
}

#[test]
fn contacts_groups_and_recipient_suggestions() {
    let db = setup();
    db.write_sync(|conn| {
        let mut contact = Contact::new("", "local-contacts", "local");
        contact.display_name = "Alice Zhang".into();
        contact.given_name = "Alice".into();
        contact.family_name = "Zhang".into();
        contact.company = "Example".into();
        contact.emails = vec![ContactField {
            r#type: ContactFieldType::Work,
            value: "alice@example.com".into(),
            is_primary: true,
        }];
        contact.phones = vec![ContactField {
            r#type: ContactFieldType::Mobile,
            value: "+8613800000000".into(),
            is_primary: true,
        }];
        let saved = repo::contacts::create(conn, &contact)?;
        assert!(!saved.id.is_empty());

        let mut bob = Contact::new("", "local-contacts", "local");
        bob.display_name = "Bob Li".into();
        bob.emails = vec![ContactField {
            r#type: ContactFieldType::Home,
            value: "bob@example.com".into(),
            is_primary: true,
        }];
        let bob = repo::contacts::create(conn, &bob)?;

        let page = repo::contacts::list(conn, &ContactListQuery::default())?;
        assert_eq!(page.total, 2);
        assert_eq!(page.items[0].display_name, "Alice Zhang");
        assert_eq!(
            page.items[0].primary_email.as_deref(),
            Some("alice@example.com")
        );

        let filtered = repo::contacts::list(
            conn,
            &ContactListQuery {
                query: Some("bob".into()),
                ..ContactListQuery::default()
            },
        )?;
        assert_eq!(filtered.total, 1);

        assert_eq!(
            repo::contacts::find_by_email(conn, "ALICE@example.com")?.map(|contact| contact.id),
            Some(saved.id.clone())
        );
        assert!(repo::contacts::find_by_email(conn, "nobody@example.com")?.is_none());

        let group = repo::contacts::create_group(conn, "local-contacts", "Team")?;
        repo::contacts::set_group_members(conn, &group.id, &[saved.id.clone(), bob.id.clone()])?;
        let groups = repo::contacts::list_groups(conn, Some("local-contacts"))?;
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].member_count, 2);
        assert_eq!(repo::contacts::members(conn, &group.id)?.len(), 2);

        let in_group = repo::contacts::list(
            conn,
            &ContactListQuery {
                group_id: Some(group.id.clone()),
                ..ContactListQuery::default()
            },
        )?;
        assert_eq!(in_group.total, 2);

        repo::contacts::set_favorite(conn, &saved.id, true)?;
        let favorites = repo::contacts::list(
            conn,
            &ContactListQuery {
                favorites_only: true,
                ..ContactListQuery::default()
            },
        )?;
        assert_eq!(favorites.total, 1);

        // Suggestions merge contacts and recipient history.
        repo::recipient_history::record(
            conn,
            &[
                Address::with_name("Alice", "alice@example.com"),
                Address::with_name("Carol", "carol@example.com"),
            ],
        )?;
        let suggestions = repo::recipient_history::suggest(conn, "ali", 5)?;
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].email, "alice@example.com");
        let all = repo::recipient_history::suggest(conn, "", 10)?;
        assert!(all
            .iter()
            .any(|address| address.email == "carol@example.com"));
        assert!(all
            .iter()
            .any(|address| address.email == "alice@example.com"));

        let renamed = repo::contacts::rename_group(conn, &group.id, "Friends")?;
        assert_eq!(renamed.name, "Friends");
        repo::contacts::delete_group(conn, &group.id)?;
        assert_eq!(repo::contacts::list_groups(conn, None)?.len(), 0);

        repo::contacts::delete_many(conn, &[saved.id, bob.id])?;
        assert_eq!(repo::contacts::count(conn)?, 0);
        Ok(())
    })
    .expect("contacts");
}

#[test]
fn pending_ops_undo_rules_and_trusted_senders() {
    let db = setup();
    add_account(&db, "acc", "me@example.com", true);

    db.write_sync(|conn| {
        let op = repo::pending_ops::enqueue(
            conn,
            NewPendingOp::new(
                "acc",
                "imap.store",
                serde_json::json!({"ids": ["m1"], "seen": true}),
                "Mark as read",
            ),
        )?;
        assert_eq!(op.state, PendingOpState::Pending);
        assert_eq!(op.sequence, 1, "the first operation gets sequence 1");
        assert_eq!(repo::pending_ops::next_due(conn, ts(1, 10), 10)?.len(), 1);
        assert_eq!(repo::pending_ops::pending_count(conn, None)?, 1);

        let retried = repo::pending_ops::mark_retry(conn, &op.id, "timeout", ts(1, 10))?;
        assert_eq!(retried.attempts, 1);
        assert!(retried.next_retry_at.is_some());
        assert!(repo::pending_ops::next_due(conn, ts(1, 10), 10)?.is_empty());

        let payload: serde_json::Value = repo::pending_ops::payload(&retried)?;
        assert_eq!(payload["seen"], true);

        repo::pending_ops::mark_done(conn, &op.id)?;
        assert_eq!(repo::pending_ops::pending_count(conn, None)?, 0);

        let second = repo::pending_ops::enqueue(
            conn,
            NewPendingOp::new("acc", "imap.move", serde_json::json!({}), "Move"),
        )?;
        assert!(
            second.sequence > op.sequence,
            "the explicit sequence follows insertion order"
        );
        // Even when `created_at` collides (second precision), listing follows
        // the explicit sequence, never the implicit rowid.
        let listed: Vec<String> = repo::pending_ops::list(conn, Some("acc"))?
            .into_iter()
            .map(|op| op.id)
            .collect();
        assert_eq!(listed, vec![op.id.clone(), second.id.clone()]);
        repo::pending_ops::mark_failed(conn, &second.id, "permanent")?;
        assert_eq!(
            repo::pending_ops::get_required(conn, &second.id)?.state,
            PendingOpState::Failed
        );
        repo::pending_ops::cancel_for_account(conn, "acc")?;

        // Undo log
        repo::undo::push(
            conn,
            "acc",
            "setSeen",
            "Marked read",
            &serde_json::json!({"ids": ["m1"]}),
        )?;
        repo::undo::push(
            conn,
            "acc",
            "move",
            "Moved",
            &serde_json::json!({"folderId": "archive"}),
        )?;
        let last = repo::undo::pop_last(conn, Some("acc"))?.expect("undo entry");
        assert_eq!(last.kind, "move");
        assert_eq!(repo::undo::count(conn)?, 1);
        assert!(repo::undo::pop_last(conn, None)?.is_some());
        assert!(repo::undo::pop_last(conn, None)?.is_none());

        // Rules
        let rule = repo::rules::upsert(
            conn,
            &MailRule {
                id: String::new(),
                account_id: None,
                name: "Newsletters".into(),
                enabled: true,
                match_all: true,
                conditions: vec![RuleCondition {
                    field: RuleField::From,
                    op: RuleOp::Contains,
                    value: "news@".into(),
                }],
                actions: vec![RuleAction {
                    kind: RuleActionKind::Move,
                    folder_id: Some("inbox".into()),
                }],
                stop_processing: false,
                sort_order: 0,
            },
        )?;
        assert_eq!(repo::rules::list_enabled(conn, "acc")?.len(), 1);
        assert_eq!(
            repo::rules::get_required(conn, &rule.id)?.conditions.len(),
            1
        );
        repo::rules::set_enabled(conn, &rule.id, false)?;
        assert!(repo::rules::list_enabled(conn, "acc")?.is_empty());
        repo::rules::delete(conn, &rule.id)?;

        // Trusted senders
        repo::trusted_senders::add(conn, "Friend@Example.com", None)?;
        assert!(repo::trusted_senders::is_trusted(
            conn,
            "friend@example.com",
            "example.com"
        )?);
        repo::trusted_senders::add(conn, "@trusted.org", None)?;
        assert!(repo::trusted_senders::is_trusted(
            conn,
            "someone@trusted.org",
            "trusted.org"
        )?);
        assert!(!repo::trusted_senders::is_trusted(
            conn,
            "evil@other.org",
            "other.org"
        )?);
        assert_eq!(repo::trusted_senders::list(conn)?.len(), 2);
        repo::trusted_senders::remove(conn, "friend@example.com")?;
        assert!(!repo::trusted_senders::is_trusted(
            conn,
            "friend@example.com",
            "example.com"
        )?);
        Ok(())
    })
    .expect("ops");
}

/// A short-lived build persisted `quarantined` as a state string. Older
/// binaries do not know it and fall back to `pending`, which would send the
/// legacy DELETE. Reading must map it to `cancelled` (never selected), and the
/// payload marker must keep the row through pruning as the audit record.
#[test]
fn legacy_quarantined_state_reads_as_cancelled_and_is_never_due() {
    let db = setup();
    add_account(&db, "acc", "me@example.com", true);
    db.write_sync(|conn| {
        let payload = serde_json::json!({
            "folderId": "drafts",
            "uid": 7,
            "quarantined": true,
            "quarantineReason": "legacy row",
        });
        conn.execute(
            "INSERT INTO pending_op
                 (id, account_id, kind, payload_json, state, attempts, next_retry_at,
                  description, last_error, created_at, updated_at, seq)
             VALUES ('legacy-op', 'acc', 'deleteRemoteDraft', ?1, 'quarantined', 0,
                     NULL, 'legacy delete', NULL, ?2, ?2, 1)",
            rusqlite::params![
                serde_json::to_string(&payload).expect("payload"),
                repo::dt_string(ts(1, 1))
            ],
        )?;

        let op = repo::pending_ops::get_required(conn, "legacy-op")?;
        assert_eq!(op.state, PendingOpState::Cancelled);
        assert_eq!(op.state.as_str(), "cancelled");
        assert!(
            repo::pending_ops::next_due(conn, ts(1, 10), 10)?.is_empty(),
            "a legacy quarantined row must never be selected for execution"
        );

        // Downgrade safety: the row must be rewritten on disk, so an OLDER
        // binary (which maps unknown strings to pending) can never execute it.
        let rewritten = repo::pending_ops::normalize_quarantined_state(conn)?;
        assert_eq!(rewritten, 1, "the legacy string is normalized exactly once");
        let raw: String = conn.query_row(
            "SELECT state FROM pending_op WHERE id = 'legacy-op'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(raw, "cancelled", "no `quarantined` string may survive");
        assert_eq!(repo::pending_ops::normalize_quarantined_state(conn)?, 0);

        // Pruned only AFTER normalization: the row is now `cancelled` and the
        // audit intent lives solely in the payload marker, so this is the
        // state the retention rule actually has to protect.
        repo::pending_ops::prune_finished(conn, ts(1, 10))?;
        assert!(
            repo::pending_ops::get(conn, "legacy-op")?.is_some(),
            "the normalized cancelled+quarantined row survives pruning"
        );
        Ok(())
    })
    .expect("legacy quarantined state");
}

/// A failed APPEND whose phase is still `unknown` and that has no receipt may
/// describe a remote create that already happened. Pruning it would let a
/// later autosave upload a second copy, so the row must survive housekeeping.
#[test]
fn pruning_retains_an_unknown_append_without_a_receipt() {
    let db = setup();
    add_account(&db, "acc", "a@example.com", false);
    db.write_sync(|conn| {
        let unknown = repo::pending_ops::enqueue(
            conn,
            NewPendingOp::new(
                "acc",
                "appendDraft",
                serde_json::json!({
                    "draftId": "d-unknown",
                    "folderId": "drafts",
                    "rfc822Path": "/tmp/unknown.eml",
                    "appendState": "unknown",
                }),
                "Save draft to server",
            ),
        )?;
        repo::pending_ops::mark_failed(conn, &unknown.id, "lost completion")?;

        let acked = repo::pending_ops::enqueue(
            conn,
            NewPendingOp::new(
                "acc",
                "appendDraft",
                serde_json::json!({
                    "draftId": "d-acked",
                    "folderId": "drafts",
                    "rfc822Path": "/tmp/acked.eml",
                    "appendState": "acked",
                    "appendedRemoteId": "remote-1",
                }),
                "Save draft to server",
            ),
        )?;
        repo::pending_ops::mark_failed(conn, &acked.id, "local commit failed")?;

        repo::pending_ops::prune_finished(conn, Utc::now() + chrono::Duration::days(1))?;
        assert!(
            repo::pending_ops::get(conn, &unknown.id)?.is_some(),
            "the unknown create is kept as the guard against a duplicate upload"
        );
        assert!(
            repo::pending_ops::get(conn, &acked.id)?.is_none(),
            "a row with an explicit receipt may be pruned"
        );
        Ok(())
    })
    .expect("prune unknown append");
}

/// Housekeeping must use the settlement time, not the enqueue time: an
/// operation that waited longer than the retention window in the queue but
/// succeeded just now is the only evidence that the object changed to the
/// uploaded value. Pruning it immediately would let a delta requested before
/// the upload overwrite the row with the older server copy.
#[test]
fn pruning_uses_settlement_time_not_enqueue_time() {
    let db = setup();
    add_account(&db, "acc", "a@example.com", false);
    db.write_sync(|conn| {
        let now = Utc::now();
        let long_queued = repo::pending_ops::enqueue(
            conn,
            NewPendingOp::new(
                "acc",
                "pim.contact.save",
                serde_json::json!({"contactId": "c-1"}),
                "Old but just settled",
            ),
        )?;
        // It sat in the queue for nine days before finally succeeding now.
        conn.execute(
            "UPDATE pending_op SET created_at = ?2, updated_at = ?2 WHERE id = ?1",
            rusqlite::params![
                long_queued.id,
                repo::dt_string(now - chrono::Duration::days(9))
            ],
        )?;
        repo::pending_ops::mark_done(conn, &long_queued.id)?;

        // A control row that settled long ago as well, so it may go.
        let stale = repo::pending_ops::enqueue(
            conn,
            NewPendingOp::new(
                "acc",
                "pim.contact.save",
                serde_json::json!({"contactId": "c-2"}),
                "Settled long ago",
            ),
        )?;
        conn.execute(
            "UPDATE pending_op SET state = 'done', created_at = ?2, updated_at = ?2 WHERE id = ?1",
            rusqlite::params![stale.id, repo::dt_string(now - chrono::Duration::days(9))],
        )?;

        repo::pending_ops::prune_finished(conn, now - chrono::Duration::days(7))?;
        assert!(
            repo::pending_ops::get(conn, &long_queued.id)?.is_some(),
            "an operation settled within the window must survive pruning"
        );
        assert!(
            repo::pending_ops::get(conn, &stale.id)?.is_none(),
            "an operation settled before the window may be pruned"
        );
        Ok(())
    })
    .expect("prune by settlement time");
}

/// A confirmed move's destination lives on the projection row, so it must be
/// written only while the row is still unbound and disappear with the binding.
#[test]
fn move_target_identity_is_bound_to_the_projection_row() {
    let db = setup();
    add_account(&db, "acc", "a@example.com", false);
    add_folder(&db, "fld", "acc", "Inbox", FolderRole::Inbox);
    add_message(
        &db,
        &make_message("proj", "acc", "fld", "t", ts(2, 1), "m", true, false),
    );
    db.write_sync(|conn| {
        // The optimistic move's projection state.
        conn.execute(
            "UPDATE message SET uid = NULL, gm_msgid = NULL, target_pending = 1
              WHERE id = 'proj'",
            [],
        )?;

        assert_eq!(
            repo::messages::set_move_target_identity(
                conn, "acc", "proj", "fld", "m-new", "op-1", 1
            )?,
            1,
            "the unbound projection accepts its confirmed destination"
        );
        assert_eq!(
            repo::messages::confirmed_move_projection_targets(conn, "acc", "fld")?,
            std::collections::HashMap::from([("m-new".to_string(), "proj".to_string())])
        );

        // A second confirmation (for example a retry) is harmless.
        assert_eq!(
            repo::messages::set_move_target_identity(
                conn, "acc", "proj", "fld", "m-new", "op-1", 1
            )?,
            1
        );

        // Binding the row clears the temporary identity in the same write.
        repo::messages::set_uid_identity(conn, "proj", Some(42), Some(1))?;
        let (uid, target_pending, remote, folder): (
            Option<i64>,
            i64,
            Option<String>,
            Option<String>,
        ) = conn.query_row(
            "SELECT uid, target_pending, move_target_remote_id, move_target_folder_id
                   FROM message WHERE id = 'proj'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        assert_eq!((uid, target_pending), (Some(42), 0));
        assert_eq!((remote, folder), (None, None), "the identity is cleared");
        assert!(repo::messages::confirmed_move_projection_targets(conn, "acc", "fld")?.is_empty());

        // A bound row is not a projection any more; no identity is revived.
        assert_eq!(
            repo::messages::set_move_target_identity(
                conn, "acc", "proj", "fld", "m-new", "op-1", 1
            )?,
            0
        );
        assert!(repo::messages::confirmed_move_projection_targets(conn, "acc", "fld")?.is_empty());
        Ok(())
    })
    .expect("move target identity");
}

/// A later trip's identity must not be replaced by the retry of an older
/// operation, while a newer operation may still take the slot over (which is
/// what keeps a legitimate confirmation from being blocked by stale evidence).
#[test]
fn move_target_identity_is_owned_by_the_newest_trip() {
    let db = setup();
    add_account(&db, "acc", "a@example.com", false);
    add_folder(&db, "fld", "acc", "Inbox", FolderRole::Inbox);
    add_message(
        &db,
        &make_message("proj", "acc", "fld", "t", ts(2, 1), "m", true, false),
    );
    db.write_sync(|conn| {
        conn.execute(
            "UPDATE message SET uid = NULL, gm_msgid = NULL, target_pending = 1
              WHERE id = 'proj'",
            [],
        )?;

        // The newer trip claims the slot and records its destination.
        repo::messages::claim_move_target_owner(
            conn,
            "acc",
            &["proj".to_string()],
            "fld",
            "op-new",
            2,
        )?;
        assert_eq!(
            repo::messages::set_move_target_identity(
                conn, "acc", "proj", "fld", "m-new2", "op-new", 2
            )?,
            1
        );

        // The older batch retries with its own stale destination.
        assert_eq!(
            repo::messages::set_move_target_identity(
                conn, "acc", "proj", "fld", "m-new1", "op-old", 1
            )?,
            0,
            "an older operation must not overwrite the newer trip"
        );
        assert_eq!(
            repo::messages::confirmed_move_projection_targets(conn, "acc", "fld")?,
            std::collections::HashMap::from([("m-new2".to_string(), "proj".to_string())])
        );
        let (remote, owner, sequence): (Option<String>, Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT move_target_remote_id, move_target_op_id, move_target_seq
                   FROM message WHERE id = 'proj'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        assert_eq!(
            (remote.as_deref(), owner.as_deref(), sequence),
            (Some("m-new2"), Some("op-new"), Some(2))
        );

        // A strictly newer trip may take the slot over.
        assert_eq!(
            repo::messages::set_move_target_identity(
                conn, "acc", "proj", "fld", "m-new3", "op-newer", 3
            )?,
            1
        );

        // Binding the row clears the identity and its owner together.
        repo::messages::set_uid_identity(conn, "proj", Some(42), Some(1))?;
        let (remote, owner, sequence): (Option<String>, Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT move_target_remote_id, move_target_op_id, move_target_seq
                   FROM message WHERE id = 'proj'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        assert_eq!((remote, owner, sequence), (None, None, None));
        Ok(())
    })
    .expect("move target ownership");
}

/// A failed batch's recovery writes are conditional on the row's current move
/// owner. An older operation must not clear a newer trip's destination or pull
/// its projection back to the source folder, while the owner itself can still
/// record a reconciliation.
#[test]
fn move_failure_recovery_is_owned_by_the_current_trip() {
    let db = setup();
    add_account(&db, "acc", "a@example.com", false);
    add_folder(&db, "src", "acc", "Inbox", FolderRole::Inbox);
    add_folder(&db, "fld", "acc", "Archive", FolderRole::Archive);
    add_message(
        &db,
        &make_message("proj", "acc", "fld", "t", ts(2, 1), "m", true, false),
    );
    db.write_sync(|conn| {
        conn.execute(
            "UPDATE message SET uid = NULL, gm_msgid = NULL, target_pending = 1
              WHERE id = 'proj'",
            [],
        )?;

        repo::messages::claim_move_target_owner(
            conn,
            "acc",
            &["proj".to_string()],
            "fld",
            "op-new",
            2,
        )?;
        assert_eq!(
            repo::messages::set_move_target_identity(
                conn,
                "acc",
                "proj",
                "fld",
                "m-current",
                "op-new",
                2
            )?,
            1
        );

        // The older batch's failure must leave the newer trip's evidence.
        repo::messages::mark_reconcile_owned(
            conn,
            &["proj".to_string()],
            repo::messages::RECONCILE_MOVE_ROLLBACK,
            "op-old",
            1,
            Some(7),
            Some(1),
        )?;
        assert_eq!(
            repo::messages::move_target_owner(conn, "proj")?.as_deref(),
            Some("op-new"),
            "an older recovery must not take the newer owner's slot"
        );
        let (remote, folder, reconcile): (Option<String>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT move_target_remote_id, move_target_folder_id, reconcile_op_id
                   FROM message WHERE id = 'proj'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        assert_eq!(remote.as_deref(), Some("m-current"));
        assert_eq!(folder.as_deref(), Some("fld"));
        assert_eq!(reconcile, None);

        // Nor may it move the newer projection back into the source folder.
        assert!(repo::messages::move_to_folder_rollback(
            conn,
            &["proj".to_string()],
            "src",
            "fld",
            "acc",
            "op-old",
        )?
        .is_empty());
        assert_eq!(
            repo::messages::get_required(conn, "proj")?
                .summary
                .folder_id,
            "fld"
        );

        // The operation that owns the slot may still record the failure.
        repo::messages::mark_reconcile_owned(
            conn,
            &["proj".to_string()],
            repo::messages::RECONCILE_MOVE_ROLLBACK,
            "op-new",
            2,
            Some(7),
            Some(1),
        )?;
        assert_eq!(repo::messages::move_target_owner(conn, "proj")?, None);
        let reconcile_owner: Option<String> = conn.query_row(
            "SELECT reconcile_op_id FROM message WHERE id = 'proj'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(reconcile_owner.as_deref(), Some("op-new"));
        Ok(())
    })
    .expect("owner-checked move recovery");
}

/// A legacy permanent-delete rollback wrote an empty reconcile token where it
/// meant "no trip owns this row". An empty (or `"0"`) token is not a trip and
/// must not look like a foreign owner to the binding guards: the row stays
/// legitimately bindable and does not get stuck as a pending duplicate.
#[test]
fn blank_owner_token_is_an_unowned_projection() {
    let db = setup();
    add_account(&db, "acc", "a@example.com", false);
    add_folder(&db, "fld", "acc", "Inbox", FolderRole::Inbox);
    add_message(
        &db,
        &make_message("proj", "acc", "fld", "t", ts(2, 1), "m", true, false),
    );
    db.write_sync(|conn| {
        conn.execute(
            "UPDATE message SET uid = NULL, gm_msgid = NULL, target_pending = 1,
                    awaiting_reconcile = 1,
                    reconcile_reason = ?1,
                    reconcile_op_id = ''
              WHERE id = 'proj'",
            rusqlite::params![repo::messages::RECONCILE_PENDING_DELETE],
        )?;

        assert_eq!(
            repo::messages::projection_owner(conn, "proj")?,
            None,
            "an empty token is not an owner"
        );
        assert!(
            repo::messages::move_target_owners(conn, "acc", "fld")?.is_empty(),
            "an empty token must not claim the row's identity slot"
        );

        // The legacy `"0"` placeholder is the same "no owner" marker.
        conn.execute(
            "UPDATE message SET reconcile_op_id = '0' WHERE id = 'proj'",
            [],
        )?;
        assert_eq!(repo::messages::projection_owner(conn, "proj")?, None);

        // A legitimate binding must succeed despite the leftover token.
        conn.execute(
            "UPDATE message SET reconcile_op_id = '' WHERE id = 'proj'",
            [],
        )?;
        assert_eq!(
            repo::messages::set_graph_identity_if_pending(
                conn, "proj", "acc", "fld", 42, "graph-1", "undo-op"
            )?,
            1,
            "an empty token must not block the row's own binding"
        );
        let restored = repo::messages::get_required(conn, "proj")?;
        assert_eq!(restored.uid, Some(42));
        assert_eq!(restored.gm_msgid.as_deref(), Some("graph-1"));
        assert!(
            repo::messages::target_pending_ids(conn, &["proj".to_string()])?.is_empty(),
            "a confirmed binding ends the pending projection"
        );
        assert_eq!(repo::messages::projection_owner(conn, "proj")?, None);
        Ok(())
    })
    .expect("blank owner token");
}

/// Releasing a permanent-delete projection is owner-checked: a row owned by a
/// newer trip stays hidden with its markers, while an unowned row (or one owned
/// by the releasing operation) is released exactly as before.
#[test]
fn releasing_a_permanent_delete_respects_the_projection_owner() {
    let db = setup();
    add_account(&db, "acc", "a@example.com", false);
    add_folder(&db, "inbox", "acc", "INBOX", FolderRole::Inbox);
    for (index, id) in ["m1", "m2", "m3"].into_iter().enumerate() {
        let mut row = make_message(id, "acc", "inbox", "t", ts(2, 1), "m", true, false);
        row.uid = Some(7 + index as i64);
        add_message(&db, &row);
    }

    db.write_sync(|conn| {
        // m1/m2 were hidden by an older delete but a newer trip then claimed
        // their identity. m3 is unowned.
        conn.execute(
            "UPDATE message
                SET uid = NULL, gm_msgid = NULL, target_pending = 1,
                    is_deleted_local = 1, pending_delete = 1,
                    reconcile_op_id = CASE WHEN id = 'm3' THEN NULL ELSE 'op-new' END,
                    reconcile_seq = CASE WHEN id = 'm3' THEN NULL ELSE 5 END,
                    reconcile_reason = 'pendingDelete'
              WHERE id IN ('m1', 'm2', 'm3')",
            [],
        )?;

        let ids = |id: &str| vec![id.to_string()];
        // A newer trip's row must not be unhidden, and its markers stay.
        assert!(repo::messages::release_pending_delete_preserving_identity(
            conn,
            "acc",
            "inbox",
            &ids("m1"),
            "op-old"
        )?
        .is_empty());
        assert!(repo::messages::release_pending_delete_with_trust(
            conn,
            "acc",
            "inbox",
            &ids("m2"),
            &std::collections::HashSet::new(),
            &[],
            None,
            "op-old"
        )?
        .is_empty());
        for id in ["m1", "m2"] {
            let row = repo::messages::get_required(conn, id)?;
            assert!(row.is_deleted_local, "{id} must stay hidden");
            let (pending_delete, owner): (i64, Option<String>) = conn.query_row(
                "SELECT pending_delete, reconcile_op_id FROM message WHERE id = ?1",
                rusqlite::params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert_eq!(pending_delete, 1);
            assert_eq!(owner.as_deref(), Some("op-new"));
        }

        // The owning operation (or an unowned row) is released normally.
        assert_eq!(
            repo::messages::release_pending_delete_preserving_identity(
                conn,
                "acc",
                "inbox",
                &ids("m1"),
                "op-new"
            )?,
            ids("m1")
        );
        assert_eq!(
            repo::messages::release_pending_delete_with_trust(
                conn,
                "acc",
                "inbox",
                &ids("m2"),
                &std::collections::HashSet::new(),
                &[8],
                None,
                "op-new"
            )?,
            ids("m2")
        );
        assert_eq!(
            repo::messages::release_pending_delete_with_trust(
                conn,
                "acc",
                "inbox",
                &ids("m3"),
                &std::collections::HashSet::new(),
                &[],
                None,
                "op-old"
            )?,
            ids("m3")
        );
        for id in ["m1", "m2", "m3"] {
            let row = repo::messages::get_required(conn, id)?;
            assert!(!row.is_deleted_local, "{id} must be visible again");
            let pending_delete: i64 = conn.query_row(
                "SELECT pending_delete FROM message WHERE id = ?1",
                rusqlite::params![id],
                |row| row.get(0),
            )?;
            assert_eq!(pending_delete, 0);
        }
        Ok(())
    })
    .expect("owner-checked permanent-delete release");
}

/// Housekeeping must not drop the confirmed move receipt while it is the only
/// evidence for an unbound projection. Once the row carries that identity, the
/// operation may be pruned.
#[test]
fn pruning_retains_a_confirmed_move_until_the_row_carries_the_identity() {
    let db = setup();
    add_account(&db, "acc", "a@example.com", false);
    add_folder(&db, "src", "acc", "Inbox", FolderRole::Inbox);
    add_folder(&db, "fld", "acc", "Archive", FolderRole::Archive);
    add_message(
        &db,
        &make_message("proj", "acc", "fld", "t", ts(2, 1), "m", true, false),
    );
    db.write_sync(|conn| {
        conn.execute(
            "UPDATE message SET uid = NULL, gm_msgid = NULL, target_pending = 1
              WHERE id = 'proj'",
            [],
        )?;
        let op = repo::pending_ops::enqueue(
            conn,
            NewPendingOp::new(
                "acc",
                "move",
                serde_json::json!({
                    "folderId": "src",
                    "targetFolderId": "fld",
                    "uids": [7],
                    "messageIds": ["proj"],
                    "outcomes": [{
                        "messageId": "proj",
                        "state": "confirmed",
                        "targetFolderId": "fld",
                        "targetRemoteId": "m-new"
                    }]
                }),
                "Move message",
            ),
        )?;
        let old = Utc::now() - chrono::Duration::days(9);
        conn.execute(
            "UPDATE pending_op SET state = 'done', updated_at = ?2 WHERE id = ?1",
            rusqlite::params![op.id, repo::dt_string(old)],
        )?;

        repo::pending_ops::prune_finished(conn, Utc::now() - chrono::Duration::days(7))?;
        assert!(
            repo::pending_ops::get(conn, &op.id)?.is_some(),
            "the receipt is the projection's only identity and must survive"
        );

        // The executor mirrored the receipt onto the row; the queue row is now
        // redundant and may be housekept.
        repo::messages::set_move_target_identity(conn, "acc", "proj", "fld", "m-new", "op-1", 1)?;
        repo::pending_ops::prune_finished(conn, Utc::now() - chrono::Duration::days(7))?;
        assert!(
            repo::pending_ops::get(conn, &op.id)?.is_none(),
            "the durable row identity replaces the operation receipt"
        );
        Ok(())
    })
    .expect("prune confirmed move");
}

#[test]
fn sync_state_and_settings_repo() {
    let db = setup();
    db.write_sync(|conn| {
        repo::sync_state::set(conn, "acc", "mail", "inbox.uidnext", "42")?;
        assert_eq!(
            repo::sync_state::get(conn, "acc", "mail", "inbox.uidnext")?.as_deref(),
            Some("42")
        );
        repo::sync_state::set_json(conn, "acc", "calendar", "syncToken", &"token-1")?;
        let token: Option<String> =
            repo::sync_state::get_json(conn, "acc", "calendar", "syncToken")?;
        assert_eq!(token.as_deref(), Some("token-1"));

        let cursor =
            repo::sync_state::get_cursor(conn, "acc", "mail", "inbox.uidnext")?.expect("cursor");
        assert_eq!(cursor.value, "42");
        assert_eq!(repo::sync_state::list_module(conn, "acc", "mail")?.len(), 1);
        repo::sync_state::delete_module(conn, "acc", "mail")?;
        assert!(repo::sync_state::get(conn, "acc", "mail", "inbox.uidnext")?.is_none());
        Ok(())
    })
    .expect("sync state");
}

#[test]
fn folder_create_move_validation_and_account_delete_cleanup() {
    let db = setup();
    add_account(&db, "acc", "me@example.com", true);
    db.write_sync(|conn| {
        let parent = repo::folders::create(conn, "acc", None, "Projects")?;
        let child = repo::folders::create(conn, "acc", Some(&parent.id), "2026")?;
        assert_eq!(child.parent_id.as_deref(), Some(parent.id.as_str()));
        // Duplicate remote name is rejected.
        assert!(repo::folders::create(conn, "acc", None, "Projects").is_err());
        // Moving a folder into its own subtree is rejected.
        assert!(repo::folders::move_folder(conn, &parent.id, Some(&child.id)).is_err());

        // Messages + cached body + attachment, then delete the account.
        let inbox = repo::folders::create(conn, "acc", None, "INBOX")?;
        let mut message = make_message(
            "m1",
            "acc",
            &inbox.id,
            "t1",
            ts(1, 9),
            "Searchable",
            false,
            false,
        );
        message.summary.preview = "unique-preview-token".into();
        repo::messages::upsert(conn, &message)?;
        repo::messages::set_body(
            conn,
            "m1",
            Some("/tmp/m1.eml"),
            Some("body-token-xyz"),
            None,
        )?;
        let mut attachment = Attachment {
            id: String::new(),
            message_id: "m1".into(),
            filename: "a.txt".into(),
            mime_type: "text/plain".into(),
            size: 1,
            content_id: None,
            is_inline: false,
            is_downloaded: false,
        };
        repo::attachments::insert(conn, &mut attachment)?;
        repo::sync_state::set(conn, "acc", "mail", "cursor", "1")?;

        repo::accounts::delete(conn, "acc")?;
        assert!(repo::accounts::get(conn, "acc")?.is_none());
        assert_eq!(repo::folders::list(conn, "acc")?.len(), 0);
        assert_eq!(repo::messages::cached_body_count(conn)?, 0);
        assert!(repo::sync_state::get(conn, "acc", "mail", "cursor")?.is_none());
        let hits = repo::messages::list(
            conn,
            &MessageListQuery::new(MailScope::search(SearchQuery::text("body-token")))
                .with_conversation(false),
        )?;
        assert_eq!(
            hits.total, 0,
            "FTS rows must be cleaned up with the account"
        );
        // The built-in local account is protected.
        assert!(repo::accounts::delete(conn, "local").is_err());
        Ok(())
    })
    .expect("folder lifecycle");
}

#[test]
fn serialized_contract_shapes() {
    let message = make_message("m1", "a1", "f1", "t1", ts(1, 9), "Hi", false, false);
    let value = serde_json::to_value(&message.summary).expect("serialize");
    assert_eq!(value["accountId"], "a1");
    assert_eq!(value["threadId"], "t1");
    assert!(
        value.get("from").is_some(),
        "Rust raw field r#from must serialize as `from`"
    );
    assert_eq!(value["threadCount"], 1);
    assert_eq!(value["hasAttachments"], false);
    // Optional fields serialize as null (never omitted).
    assert_eq!(value["from"]["name"], "Alice");
    assert!(value.get("replyTo").is_none());

    let address = serde_json::to_value(Address::new("x@y")).expect("address");
    assert_eq!(address["name"], serde_json::Value::Null);
    assert_eq!(address["email"], "x@y");

    let event = CalendarEvent::new(
        "e",
        "c",
        "a",
        "uid",
        "t",
        "2026-11-12T09:00:00+08:00",
        "2026-11-12T10:00:00+08:00",
    );
    let value = serde_json::to_value(&event).expect("event");
    assert_eq!(value["calendarId"], "c");
    assert_eq!(value["recurrence"], serde_json::Value::Null);
    assert_eq!(value["attendees"], serde_json::json!([]));
    assert!(value.get("isOrganizer").is_some());
}

/// Clearing a writer with no predecessor deletes its row. The message-level
/// revision must still move, or a reader that observed `None` before cannot
/// tell "nothing changed" from "an undo restored the value and removed the
/// writer".
#[test]
fn clearing_a_flag_writer_advances_the_message_revision() {
    let db = setup();
    add_account(&db, "a1", "a1@example.com", true);
    add_folder(&db, "f1", "a1", "INBOX", FolderRole::Inbox);
    let message = make_message("m1", "a1", "f1", "t1", ts(1, 0), "revision", false, false);
    add_message(&db, &message);

    db.write_sync(|conn| {
        let observed = repo::messages::flag_revision(conn, "m1")?.expect("message row");
        assert_eq!(observed, 0);

        // A user action records its writer: the revision moves.
        repo::messages::record_flag_writer(conn, "m1", "seen", "op-1", Some(true), "setSeen")?;
        let after_write = repo::messages::flag_revision(conn, "m1")?.expect("message row");
        assert!(after_write > observed);

        // Undo with no predecessor clears the writer row entirely.
        repo::messages::reassign_flag_writer(conn, "m1", "seen", "op-1", None, None, None)?;
        assert!(repo::messages::flag_writer(conn, "m1", "seen")?.is_none());
        let after_clear = repo::messages::flag_revision(conn, "m1")?.expect("message row");
        assert!(
            after_clear > after_write,
            "removing the last writer must still advance the observed revision"
        );
        Ok(())
    })
    .expect("flag revision on clear");
}

#[test]
fn flag_writer_revisions_are_not_reused_after_a_writer_is_deleted() {
    let db = setup();
    db.write_sync(|conn| {
        repo::messages::record_flag_writer(conn, "m1", "seen", "first", Some(false), "unread")?;
        let first = repo::messages::flag_writer(conn, "m1", "seen")?
            .expect("first writer")
            .seq;
        // Undoing an action with no predecessor removes its writer row.
        repo::messages::reassign_flag_writer(conn, "m1", "seen", "first", None, None, None)?;
        assert!(repo::messages::flag_writer(conn, "m1", "seen")?.is_none());

        repo::messages::record_flag_writer(conn, "m1", "seen", "second", Some(false), "unread")?;
        let second = repo::messages::flag_writer(conn, "m1", "seen")?
            .expect("second writer")
            .seq;
        assert!(
            second > first,
            "a revision freed by deleting the writer row must never be handed out again"
        );

        repo::messages::reassign_flag_writer(
            conn, "m1", "seen", "second", Some("third"), Some(false), Some("unread"),
        )?;
        let third = repo::messages::flag_writer(conn, "m1", "seen")?
            .expect("third writer")
            .seq;
        assert!(
            third > second,
            "a hand-off revision must also advance past every deleted value"
        );
        Ok(())
    })
    .expect("flag writer revisions");
}

/// Message-ID reuse is identity-aware: a row bound to a different UID (or a
/// different UIDVALIDITY generation) is a *different provider copy* and must
/// never be overwritten. Only a single identity-less projection may be bound
/// by header alone. This is the page-boundary / split-pass case in miniature.
#[test]
fn message_id_reuse_respects_the_provider_identity_generation() {
    let db = setup();
    add_account(&db, "acc", "me@example.com", true);
    add_folder(&db, "sent", "acc", "Sent", FolderRole::Sent);

    for (id, uid) in [("copy-a", 10), ("copy-b", 11)] {
        let mut message = make_message(id, "acc", "sent", "t1", ts(6, 9), "Pair", true, false);
        message.message_id_header = Some("pair@example.com".into());
        message.uid = Some(uid);
        message.uidvalidity = Some(1);
        add_message(&db, &message);
    }
    let mut projection = make_message(
        "projection",
        "acc",
        "sent",
        "t1",
        ts(6, 9),
        "Moving",
        true,
        false,
    );
    projection.message_id_header = Some("move@example.com".into());
    add_message(&db, &projection);

    db.read_sync(|conn| {
        // Same UID + generation: the row is the incoming copy.
        assert_eq!(
            repo::messages::unambiguous_message_id_row_for_identity(
                conn,
                "acc",
                "sent",
                "pair@example.com",
                1,
                Some(10),
                Some(1),
            )?,
            Some("copy-a".to_string())
        );
        // A different UID is a second provider copy, never a reuse target.
        assert_eq!(
            repo::messages::unambiguous_message_id_row_for_identity(
                conn,
                "acc",
                "sent",
                "pair@example.com",
                1,
                Some(12),
                Some(1),
            )?,
            None
        );
        // The same UID in another generation is not the same message.
        assert_eq!(
            repo::messages::unambiguous_message_id_row_for_identity(
                conn,
                "acc",
                "sent",
                "pair@example.com",
                1,
                Some(10),
                Some(2),
            )?,
            None
        );
        // An identity-less projection may be bound when it is the only
        // candidate for the header (the reconciliation path relies on this).
        assert_eq!(
            repo::messages::unambiguous_message_id_row_for_identity(
                conn,
                "acc",
                "sent",
                "move@example.com",
                1,
                Some(12),
                Some(1),
            )?,
            Some("projection".to_string())
        );
        Ok(())
    })
    .expect("identity-aware lookup");
}
