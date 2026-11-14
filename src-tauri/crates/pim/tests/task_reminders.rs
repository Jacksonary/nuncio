//! Integration tests for persisted task reminder dismiss/snooze state.

use std::sync::Arc;

use chrono::{Duration, Utc};
use nuncio_core::models::{new_id, TaskInput};
use nuncio_core::repo::reminder_state;
use nuncio_core::traits::testing::NoopMailSender;
use nuncio_core::{CoreContext, Paths};
use nuncio_pim::{store, tasks::task_reminder_key, PimService};

async fn service() -> (CoreContext, PimService) {
    let dir = std::env::temp_dir().join(format!("nuncio-pim-task-rem-{}", new_id()));
    let ctx = CoreContext::for_test(Paths::new(dir)).await.expect("ctx");
    let service = PimService::new(ctx.clone(), Arc::new(NoopMailSender)).expect("service");
    (ctx, service)
}

#[tokio::test]
async fn dismiss_and_snooze_are_persisted_for_tasks() {
    let (ctx, service) = service().await;
    let reminder_at = Utc::now() - Duration::minutes(5);
    let task = service
        .task_create(
            "local-tasks",
            TaskInput {
                title: "Pay bill".into(),
                due_date: Some(reminder_at.format("%Y-%m-%d").to_string()),
                reminder_at: Some(reminder_at),
                ..TaskInput::default()
            },
        )
        .await
        .expect("task");
    let key = task_reminder_key(&task);

    service
        .task_dismiss_reminder(&task.id)
        .await
        .expect("dismiss");
    let state = ctx
        .db
        .read({
            let task_id = task.id.clone();
            let key = key.clone();
            move |conn| reminder_state::get_for(conn, reminder_state::KIND_TASK, &task_id, &key)
        })
        .await
        .expect("state")
        .expect("row");
    assert!(state.dismissed_at.is_some());
    assert!(state.snoozed_until.is_none());

    // Marking it fired and then snoozing must clear the marker so the engine
    // can deliver the snoozed reminder again.
    ctx.db
        .write({
            let task_id = task.id.clone();
            let key = key.clone();
            move |conn| store::mark_task_reminder_fired(conn, &task_id, &key)
        })
        .await
        .expect("mark fired");

    let before = Utc::now();
    service
        .task_snooze_reminder(&task.id, 10)
        .await
        .expect("snooze");
    let state = ctx
        .db
        .read({
            let task_id = task.id.clone();
            let key = key.clone();
            move |conn| reminder_state::get_for(conn, reminder_state::KIND_TASK, &task_id, &key)
        })
        .await
        .expect("state")
        .expect("row");
    assert!(state.dismissed_at.is_none());
    let until = state.snoozed_until.expect("snooze deadline");
    assert!(until > before + Duration::minutes(5));

    let fired = ctx
        .db
        .read({
            let task_id = task.id.clone();
            let key = key.clone();
            move |conn| store::task_reminder_fired(conn, &task_id, &key)
        })
        .await
        .expect("fired marker");
    assert!(!fired, "snoozing must allow the reminder to fire again");
}
