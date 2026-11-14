//! Temporary audit probes: each assertion describes required behavior.
use std::sync::Arc;
use chrono::Utc;
use nuncio_core::models::{new_id, RecurrenceFreq, RecurrenceRule, TaskInput};
use nuncio_core::traits::testing::NoopMailSender;
use nuncio_core::{CoreContext, Paths};
use nuncio_pim::PimService;

#[test]
fn every_subscription_master_must_preserve_rdates() {
    let ics = concat!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n",
        "BEGIN:VEVENT\r\nUID:first\r\nSUMMARY:First\r\nDTSTART:20261113T090000Z\r\nDTEND:20261113T100000Z\r\nRDATE:20261114T090000Z\r\nEND:VEVENT\r\n",
        "BEGIN:VEVENT\r\nUID:second\r\nSUMMARY:Second\r\nDTSTART:20261113T110000Z\r\nDTEND:20261113T120000Z\r\nRDATE:20261114T110000Z\r\nEND:VEVENT\r\n",
        "END:VCALENDAR\r\n"
    );
    let events = nuncio_pim::webcal::parse_subscription(ics, "cal", "local", "me@example.com").unwrap();
    assert_eq!(events.len(), 2);
    assert!(events[0].raw_ics.as_deref().unwrap_or_default().contains("RDATE:"));
    assert!(events[1].raw_ics.as_deref().unwrap_or_default().contains("RDATE:"), "the second master lost its RDATE occurrences");
}

#[tokio::test]
async fn recurring_task_count_two_must_stop_after_second_completion() {
    let ctx = CoreContext::for_test(Paths::new(std::env::temp_dir().join(format!("nuncio-audit-pim-{}", new_id())))).await.unwrap();
    let service = PimService::new(ctx.clone(), Arc::new(NoopMailSender)).unwrap();
    let task = service.task_create("local-tasks", TaskInput {
        title: "Repeat twice".into(),
        due_date: Some(Utc::now().date_naive().format("%Y-%m-%d").to_string()),
        recurrence: Some(RecurrenceRule { freq: RecurrenceFreq::Daily, interval: 1, count: Some(2), ..Default::default() }),
        ..Default::default()
    }).await.unwrap();
    service.task_set_completed(&task.id, true).await.unwrap();
    let next_id: String = ctx.db.read(|db| Ok(db.query_row("SELECT id FROM task WHERE status <> 'completed'", [], |row| row.get(0))?)).await.unwrap();
    service.task_set_completed(&next_id, true).await.unwrap();
    let active: i64 = ctx.db.read(|db| Ok(db.query_row("SELECT COUNT(*) FROM task WHERE status <> 'completed'", [], |row| row.get(0))?)).await.unwrap();
    assert_eq!(active, 0, "COUNT=2 created a third active occurrence");
}
