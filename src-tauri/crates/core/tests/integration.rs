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
        repo::messages::set_body(conn, "m1", Some("/tmp/m1.eml"), Some("这是深度学习笔记"))?;
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
            repo::tasks::query(conn, &TaskQuery::smart(SmartList::FlaggedEmail, today.clone()))?
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
        repo::messages::set_body(conn, "m1", Some("/tmp/m1.eml"), Some("body-token-xyz"))?;
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
