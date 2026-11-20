//! Integration tests for the contacts/tasks commands exposed by `PimService`
//! on the built-in local account (no network).

use std::sync::Arc;

use chrono::Utc;
use nuncio_core::models::{
    new_id, Contact, ContactField, ContactFieldType, ContactListQuery, Importance, MessageSummary,
    NewTaskFromMessage, StoredMessage, TaskInput, TaskQuery,
};
use nuncio_core::repo;
use nuncio_core::traits::testing::NoopMailSender;
use nuncio_core::{CoreContext, Paths};
use nuncio_pim::PimService;

async fn service() -> (CoreContext, PimService) {
    let dir = std::env::temp_dir().join(format!("nuncio-pim-ct-{}", new_id()));
    let ctx = CoreContext::for_test(Paths::new(dir)).await.expect("ctx");
    let service = PimService::new(ctx.clone(), Arc::new(NoopMailSender)).expect("service");
    (ctx, service)
}

fn contact(name: &str, email: &str) -> Contact {
    let mut contact = Contact::new("", "local-contacts", "local");
    contact.display_name = name.into();
    contact.emails = vec![ContactField {
        r#type: ContactFieldType::Work,
        value: email.into(),
        is_primary: true,
    }];
    contact
}

#[tokio::test]
async fn contact_commands_round_trip() {
    let (_ctx, service) = service().await;
    let books = service.addressbook_list().await.expect("books");
    assert!(books.iter().any(|book| book.id == "local-contacts"));

    let created = service
        .contact_create(contact("Katherine Johnson", "katherine@example.com"))
        .await
        .expect("create");
    assert_eq!(created.account_id, "local");

    let mut updated = created.clone();
    updated.company = "NASA".into();
    updated.job_title = "Mathematician".into();
    let saved = service.contact_update(updated).await.expect("update");
    assert_eq!(saved.company, "NASA");

    let group = service
        .group_create("local-contacts", "Team")
        .await
        .expect("group");
    service
        .group_set_members(&group.id, vec![saved.id.clone()])
        .await
        .expect("members");
    service
        .group_rename(&group.id, "Flight Team")
        .await
        .expect("rename");

    let page = service
        .contact_list(ContactListQuery {
            address_book_id: None,
            group_id: Some(group.id.clone()),
            query: None,
            favorites_only: false,
            cursor: None,
            limit: 20,
        })
        .await
        .expect("list");
    assert_eq!(page.items.len(), 1);

    service
        .contact_set_favorite(&saved.id, true)
        .await
        .expect("favorite");
    let found = service
        .contact_find_by_email("katherine@example.com")
        .await
        .expect("find")
        .expect("contact");
    assert!(found.is_favorite);

    service
        .contact_delete(vec![saved.id.clone()])
        .await
        .expect("delete");
    assert!(service.contact_get(&saved.id).await.is_err());

    service.group_delete(&group.id).await.expect("delete group");
    assert!(service
        .group_list(Some("local-contacts".into()))
        .await
        .expect("groups")
        .iter()
        .all(|group| group.name != "Flight Team"));
}

#[tokio::test]
async fn task_commands_round_trip() {
    let (_ctx, service) = service().await;
    let lists = service.tasklist_list().await.expect("lists");
    assert!(lists.iter().any(|list| list.id == "local-tasks"));

    let list = service
        .tasklist_create("local", "Personal", "#00AA00")
        .await
        .expect("list");
    let task = service
        .task_create(
            &list.id,
            TaskInput {
                title: "Buy milk".into(),
                notes: "2 litres".into(),
                importance: Importance::High,
                due_date: Some("2026-11-12".into()),
                ..TaskInput::default()
            },
        )
        .await
        .expect("create");
    assert_eq!(task.list_id, list.id);

    let updated = service
        .task_update(
            &task.id,
            TaskInput {
                title: "Buy oat milk".into(),
                importance: Importance::High,
                ..TaskInput::default()
            },
        )
        .await
        .expect("update");
    assert_eq!(updated.title, "Buy oat milk");

    service
        .task_set_my_day(&task.id, Some("2026-11-12"))
        .await
        .expect("my day");
    let counts = service
        .task_smart_counts("2026-11-12".into())
        .await
        .expect("counts");
    assert!(counts.my_day >= 1);

    let open = service
        .task_query(TaskQuery::smart(
            nuncio_core::models::SmartList::All,
            "2026-11-12",
        ))
        .await
        .expect("query");
    assert!(open.iter().any(|item| item.id == task.id));

    service
        .task_move(&task.id, "local-tasks")
        .await
        .expect("move");
    let moved = service.task_get(&task.id).await.expect("get");
    assert_eq!(moved.list_id, "local-tasks");

    service
        .task_set_completed(&task.id, true)
        .await
        .expect("complete");
    service.task_delete(&task.id).await.expect("delete");
    service
        .tasklist_delete(&list.id)
        .await
        .expect("delete list");
}

#[tokio::test]
async fn message_create_task_links_without_flagging() {
    let (ctx, service) = service().await;
    let (message_id, folder_id) = ctx
        .db
        .write(|conn| {
            let folder = repo::folders::create(conn, "local", None, "Inbox")?;
            let summary =
                MessageSummary::new(new_id(), "local", folder.id.clone(), "thread-1", Utc::now());
            let mut stored = StoredMessage::new(summary);
            stored.summary.subject = "Quarterly report".into();
            stored.summary.r#from = Some(nuncio_core::models::Address::with_name(
                "Boss",
                "boss@example.com",
            ));
            stored.uid = Some(42);
            repo::messages::upsert(conn, &stored)?;
            Ok((stored.summary.id.clone(), folder.id))
        })
        .await
        .expect("message");

    let options = |mark_flagged: bool| NewTaskFromMessage {
        title: "Quarterly report".into(),
        notes: String::new(),
        list_id: "local-tasks".into(),
        due_date: Some("2026-11-12".into()),
        reminder_at: None,
        my_day_date: Some("2026-11-12".into()),
        mark_flagged,
    };

    let task = service
        .message_create_task(&message_id, options(false))
        .await
        .expect("task");
    assert_eq!(task.title, "Quarterly report");
    assert!(task.linked_message_id.as_deref() == Some(message_id.as_str()));
    assert!(task.notes.contains("boss@example.com"));
    assert!(task.due_date.is_some());

    let message = ctx
        .db
        .read({
            let id = message_id.clone();
            move |conn| repo::messages::get_required(conn, &id)
        })
        .await
        .expect("message");
    // Flagging is opt-in: creating a task must not change the mail unless the
    // dialog's checkbox was ticked.
    assert!(!message.summary.flagged);

    let ops = ctx
        .db
        .read(move |conn| repo::pending_ops::list(conn, Some("local")))
        .await
        .expect("ops");
    // Local accounts have no mail server, so no setFlags op is queued.
    assert!(ops.iter().all(|op| op.kind != "setFlags"));

    let tasks = service
        .task_query(TaskQuery::smart(
            nuncio_core::models::SmartList::FlaggedEmail,
            "2026-11-12",
        ))
        .await
        .expect("query");
    assert!(tasks.iter().any(|item| item.id == task.id));
    let _ = folder_id;
}

#[tokio::test]
async fn message_create_task_flags_only_when_asked() {
    let (ctx, service) = service().await;
    let message_id = ctx
        .db
        .write(|conn| {
            let folder = repo::folders::create(conn, "local", None, "Inbox")?;
            let summary =
                MessageSummary::new(new_id(), "local", folder.id.clone(), "thread-f", Utc::now());
            let mut stored = StoredMessage::new(summary);
            stored.summary.subject = "Please flag".into();
            repo::messages::upsert(conn, &stored)?;
            Ok(stored.summary.id.clone())
        })
        .await
        .expect("message");

    service
        .message_create_task(
            &message_id,
            NewTaskFromMessage {
                title: String::new(),
                notes: String::new(),
                list_id: "local-tasks".into(),
                due_date: None,
                reminder_at: None,
                my_day_date: None,
                mark_flagged: true,
            },
        )
        .await
        .expect("task");

    let message = ctx
        .db
        .read({
            let id = message_id.clone();
            move |conn| repo::messages::get_required(conn, &id)
        })
        .await
        .expect("message");
    assert!(message.summary.flagged);
}

#[tokio::test]
async fn soft_deleted_message_drops_the_task_link() {
    let (ctx, service) = service().await;
    let message_id = ctx
        .db
        .write(|conn| {
            let folder = repo::folders::create(conn, "local", None, "Inbox")?;
            let summary =
                MessageSummary::new(new_id(), "local", folder.id.clone(), "thread-d", Utc::now());
            let mut stored = StoredMessage::new(summary);
            stored.summary.subject = "Deleted later".into();
            repo::messages::upsert(conn, &stored)?;
            Ok(stored.summary.id.clone())
        })
        .await
        .expect("message");

    let task = service
        .message_create_task(
            &message_id,
            NewTaskFromMessage {
                title: "Keep the task".into(),
                notes: String::new(),
                list_id: "local-tasks".into(),
                due_date: None,
                reminder_at: None,
                my_day_date: None,
                mark_flagged: false,
            },
        )
        .await
        .expect("task");

    // Deleting the mail only hides the row; the task must stop pointing at it.
    ctx.db
        .write({
            let id = message_id.clone();
            move |conn| repo::messages::delete_local(conn, &[id], false)
        })
        .await
        .expect("delete");

    let tasks = service
        .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
        .await
        .expect("query");
    let stored = tasks.iter().find(|item| item.id == task.id).expect("task");
    assert!(stored.linked_message_id.is_none(), "stale link must be cleared");

    let counts = service
        .task_smart_counts("2026-11-12".into())
        .await
        .expect("counts");
    assert_eq!(counts.flagged_email, 0, "deleted mail must not count");
}
