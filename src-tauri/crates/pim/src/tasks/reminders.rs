//! Task reminders for the shared reminder loop.
//!
//! The engine lives in [`crate::reminders`]; this source queries tasks whose
//! `reminderAt` falls into the 48-hour window and skips already-delivered ones
//! through the `pim_task_reminder_fired` marker.  Dismissed and snoozed
//! reminders are persisted in the shared `reminder_state` table (kind =
//! `task`); expired snoozes are re-evaluated even when the original reminder
//! time has left the catch-up window.

use async_trait::async_trait;
use chrono::{DateTime, Duration, NaiveTime, Utc};
use nuncio_core::models::{Task, TaskStatus};
use nuncio_core::repo::{reminder_state, tasks as tasks_repo};
use nuncio_core::CoreContext;

use crate::error::Result;
use crate::reminders::{ReminderHit, ReminderKind, ReminderSource};

/// How late a reminder may fire after its trigger (mirrors the calendar source).
const GRACE: Duration = Duration::minutes(15);

/// Upper bound on snooze rows examined per pass.
const SNOOZE_LIMIT: usize = 500;

#[derive(Debug, Default, Clone, Copy)]
pub struct TaskReminderSource;

/// Stable reminder key for a task.  It follows the task's `reminder_at` so a
/// later edit naturally produces a new occurrence; tasks without a reminder
/// use an empty key (only reachable by dismiss/snooze calls).
pub fn task_reminder_key(task: &Task) -> String {
    task.reminder_at
        .map(|reminder| reminder.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

/// True when the reminder only carries a due date (midnight), e.g. a task
/// imported from a provider that stores date-only reminders at 00:00 UTC.
pub fn task_is_all_day(task: &Task) -> bool {
    task.due_date.is_some()
        && task
            .reminder_at
            .is_some_and(|reminder| reminder.time() == NaiveTime::MIN)
}

#[async_trait]
impl ReminderSource for TaskReminderSource {
    fn name(&self) -> &'static str {
        "tasks"
    }

    async fn due(
        &self,
        ctx: &CoreContext,
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<ReminderHit>> {
        let db = ctx.db.clone();
        let from = now - GRACE;
        let hits = db
            .read(move |conn| {
                let mut tasks: Vec<Task> = tasks_repo::reminder_candidates(conn, from, until)?
                    .into_iter()
                    .filter(|task| task.status != TaskStatus::Completed)
                    .collect();

                // Re-evaluate tasks whose snooze expired after the original
                // reminder left the catch-up window.
                let mut known: std::collections::HashSet<String> =
                    tasks.iter().map(|task| task.id.clone()).collect();
                for (task_id, _key) in reminder_state::due_task_snoozes(conn, now, SNOOZE_LIMIT)? {
                    if !known.insert(task_id.clone()) {
                        continue;
                    }
                    if let Some(task) = tasks_repo::get(conn, &task_id)? {
                        if task.status != TaskStatus::Completed && task.reminder_at.is_some() {
                            tasks.push(task);
                        }
                    }
                }

                let mut hits = Vec::new();
                for task in &tasks {
                    let Some(reminder) = task.reminder_at else {
                        continue;
                    };
                    let key = task_reminder_key(task);
                    let mut snoozed_due = false;
                    if let Some(state) =
                        reminder_state::get_for(conn, reminder_state::KIND_TASK, &task.id, &key)?
                    {
                        if state.dismissed_at.is_some() {
                            continue;
                        }
                        if let Some(until) = state.snoozed_until {
                            if until > now {
                                continue;
                            }
                            snoozed_due = true;
                        }
                    }
                    if !snoozed_due {
                        // Only fire once the reminder time has arrived; the
                        // query window provides the catch-up grace period.
                        if reminder > now {
                            continue;
                        }
                        if crate::store::task_reminder_fired(conn, &task.id, &key)? {
                            continue;
                        }
                    }
                    hits.push(ReminderHit {
                        kind: ReminderKind::Task,
                        id: task.id.clone(),
                        instance_start: Some(key),
                        title: task.title.clone(),
                        start: task.due_date.clone(),
                        all_day: task_is_all_day(task),
                        location: None,
                    });
                }
                Ok(hits)
            })
            .await?;
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use nuncio_core::models::{new_id, Importance, TaskInput};
    use nuncio_core::{CoreContext, Paths};

    async fn ctx() -> CoreContext {
        let dir = std::env::temp_dir().join(format!("nuncio-task-rem-{}", new_id()));
        let ctx = CoreContext::for_test(Paths::new(dir)).await.expect("ctx");
        ctx.migrate(crate::store::PIM_NAMESPACE, crate::store::PIM_MIGRATIONS)
            .expect("schema");
        ctx
    }

    #[tokio::test]
    async fn collects_due_task_reminders() {
        let ctx = ctx().await;
        let now = Utc.with_ymd_and_hms(2026, 11, 12, 9, 50, 0).unwrap();
        ctx.db
            .write(move |conn| {
                tasks_repo::create(
                    conn,
                    "local-tasks",
                    &TaskInput {
                        title: "Call back".into(),
                        notes: String::new(),
                        importance: Importance::Normal,
                        due_date: Some("2026-11-12".into()),
                        reminder_at: Some(now - Duration::minutes(5)),
                        recurrence: None,
                        my_day_date: None,
                        steps: Vec::new(),
                    },
                )?;
                Ok(())
            })
            .await
            .expect("task");
        let source = TaskReminderSource;
        let hits = source
            .due(&ctx, now, now + Duration::hours(48))
            .await
            .expect("hits");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, ReminderKind::Task);
        assert_eq!(hits[0].title, "Call back");
        assert!(!hits[0].all_day);
    }

    #[tokio::test]
    async fn skips_future_and_fired() {
        let ctx = ctx().await;
        let now = Utc.with_ymd_and_hms(2026, 11, 12, 9, 0, 0).unwrap();
        let task = ctx
            .db
            .write(move |conn| {
                tasks_repo::create(
                    conn,
                    "local-tasks",
                    &TaskInput {
                        title: "Later".into(),
                        reminder_at: Some(now + Duration::hours(3)),
                        ..TaskInput::default()
                    },
                )
            })
            .await
            .expect("task");
        let source = TaskReminderSource;
        let hits = source
            .due(&ctx, now, now + Duration::hours(1))
            .await
            .expect("hits");
        assert!(hits.is_empty());

        let key = task_reminder_key(&task);
        ctx.db
            .write(move |conn| crate::store::mark_task_reminder_fired(conn, &task.id, &key))
            .await
            .expect("mark");
        let hits = source
            .due(&ctx, now, now + Duration::hours(48))
            .await
            .expect("hits");
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn respects_dismiss_and_snooze_and_refires_after_snooze() {
        let ctx = ctx().await;
        let now = Utc.with_ymd_and_hms(2026, 11, 12, 9, 0, 0).unwrap();
        let task = ctx
            .db
            .write(move |conn| {
                tasks_repo::create(
                    conn,
                    "local-tasks",
                    &TaskInput {
                        title: "Pay bill".into(),
                        due_date: Some("2026-11-12".into()),
                        reminder_at: Some(now - Duration::minutes(2)),
                        ..TaskInput::default()
                    },
                )
            })
            .await
            .expect("task");
        let key = task_reminder_key(&task);
        let task_id = task.id.clone();

        // Dismissed reminders never fire.
        ctx.db
            .write({
                let task_id = task_id.clone();
                let key = key.clone();
                move |conn| {
                    reminder_state::dismiss_for(conn, reminder_state::KIND_TASK, &task_id, &key)
                }
            })
            .await
            .expect("dismiss");
        let hits = TaskReminderSource
            .due(&ctx, now, now + Duration::hours(48))
            .await
            .expect("hits");
        assert!(hits.is_empty());

        // Snoozed until the future -> suppressed.
        ctx.db
            .write({
                let task_id = task_id.clone();
                let key = key.clone();
                move |conn| {
                    reminder_state::snooze_for(
                        conn,
                        reminder_state::KIND_TASK,
                        &task_id,
                        &key,
                        10,
                        now,
                    )
                    .map(|_| ())
                }
            })
            .await
            .expect("snooze");
        let hits = TaskReminderSource
            .due(&ctx, now, now + Duration::hours(48))
            .await
            .expect("hits");
        assert!(hits.is_empty());

        // After the snooze deadline it fires again even though the original
        // reminder time is outside the 15 minute catch-up window.
        let later = now + Duration::minutes(30);
        let hits = TaskReminderSource
            .due(&ctx, later, later + Duration::hours(48))
            .await
            .expect("hits");
        assert_eq!(hits.len(), 1, "expired snooze re-fires the reminder");
        assert_eq!(hits[0].id, task_id);
        // The task's reminder is at 08:58 with a due date, but not midnight,
        // so it is a timed reminder.
        assert!(!hits[0].all_day);
    }

    #[tokio::test]
    async fn date_only_reminder_is_all_day() {
        let ctx = ctx().await;
        let now = Utc.with_ymd_and_hms(2026, 11, 12, 0, 5, 0).unwrap();
        ctx.db
            .write(move |conn| {
                tasks_repo::create(
                    conn,
                    "local-tasks",
                    &TaskInput {
                        title: "All day".into(),
                        due_date: Some("2026-11-12".into()),
                        reminder_at: Some(now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc()),
                        ..TaskInput::default()
                    },
                )?;
                Ok(())
            })
            .await
            .expect("task");
        let hits = TaskReminderSource
            .due(&ctx, now, now + Duration::hours(48))
            .await
            .expect("hits");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].all_day);
    }
}
