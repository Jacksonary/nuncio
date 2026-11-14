//! Reminder engine.
//!
//! A 30-second loop expands calendar occurrences into a 48-hour window and
//! fires one system notification per occurrence.  Delivered occurrences are
//! recorded in `pim_reminder_fired` so a restart does not repeat them.
//!
//! Stage 2 adds task reminders by implementing [`ReminderSource`] and calling
//! [`ReminderEngine::add_source`]; no engine changes are required.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use nuncio_core::events::{emit_reminder_due, ReminderDue, ReminderKind as CoreReminderKind};
use nuncio_core::models::{AttendeeStatus, EventStatus, NavigateTarget, StoredEvent};
use nuncio_core::repo::{events, reminder_state};
use nuncio_core::CoreContext;
use tokio::sync::Notify;
use tracing::{debug, warn};

use crate::error::Result;
use crate::store;
use crate::util::MAX_OCCURRENCES_PER_SERIES;

/// How late a reminder may fire after its trigger (app was closed/asleep).
const GRACE: Duration = Duration::minutes(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReminderKind {
    Event,
    Task,
}

impl ReminderKind {
    pub fn as_core(self) -> CoreReminderKind {
        match self {
            Self::Event => CoreReminderKind::Event,
            Self::Task => CoreReminderKind::Task,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Event => "event",
            Self::Task => "task",
        }
    }
}

/// One reminder that should be delivered now.
#[derive(Debug, Clone)]
pub struct ReminderHit {
    pub kind: ReminderKind,
    pub id: String,
    pub instance_start: Option<String>,
    pub title: String,
    pub start: Option<String>,
    /// All-day events and date-only task reminders carry no clock time.
    pub all_day: bool,
    pub location: Option<String>,
}

/// Extensible source of due reminders.
#[async_trait]
pub trait ReminderSource: Send + Sync {
    fn name(&self) -> &'static str;
    async fn due(
        &self,
        ctx: &CoreContext,
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<ReminderHit>>;
}

/// Calendar occurrence reminders.
#[derive(Debug, Default, Clone, Copy)]
pub struct CalendarReminderSource;

#[async_trait]
impl ReminderSource for CalendarReminderSource {
    fn name(&self) -> &'static str {
        "calendar"
    }

    async fn due(
        &self,
        ctx: &CoreContext,
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<ReminderHit>> {
        let db = ctx.db.clone();
        let from = now - GRACE;
        // Visibility is a view toggle, not a sync/reminder filter: expand every
        // calendar so hidden calendars still produce their reminders.
        let calendar_ids = db
            .read(|conn| {
                Ok(nuncio_core::repo::calendars::list(conn)?
                    .into_iter()
                    .map(|calendar| calendar.id)
                    .collect::<Vec<_>>())
            })
            .await?;
        let stored = db
            .read(move |conn| {
                events::range(
                    conn,
                    &crate::util::format_instant(from),
                    &crate::util::format_instant(until),
                    Some(calendar_ids.as_slice()),
                )
            })
            .await?;
        let hits = db
            .read(move |conn| compute_calendar_hits(conn, &stored, now, until))
            .await?;
        Ok(hits)
    }
}

/// Pure-ish computation against a connection (used by the source and tests).
pub fn compute_calendar_hits(
    conn: &rusqlite::Connection,
    stored: &[StoredEvent],
    now: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<ReminderHit>> {
    let mut overrides: std::collections::HashMap<String, Vec<nuncio_core::models::CalendarEvent>> =
        std::collections::HashMap::new();
    let mut masters: Vec<&StoredEvent> = Vec::new();
    for item in stored {
        match item.event.series_id.as_ref() {
            Some(series_id) => {
                overrides
                    .entry(series_id.clone())
                    .or_default()
                    .push(item.event.clone());
            }
            None => masters.push(item),
        }
    }

    let mut hits = Vec::new();
    for master in masters {
        let event = &master.event;
        if event.status == EventStatus::Cancelled {
            continue;
        }
        if event.reminders.is_empty() {
            continue;
        }
        let overrides_for_series = overrides.remove(&event.id).unwrap_or_default();
        let occurrences: Vec<(String, String)> = if event.recurrence.is_some() {
            nuncio_ical::recurrence::expand_stored(
                master,
                &overrides_for_series,
                now - Duration::days(1),
                until,
                MAX_OCCURRENCES_PER_SERIES,
            )
            .into_iter()
            .map(|occurrence| (occurrence.start, occurrence.original_start))
            .collect()
        } else {
            let start = crate::util::event_start_instant(event, &event.start)
                .ok_or_else(|| crate::error::invalid("invalid event start"))?;
            if start > until
                || crate::util::event_end_instant(event, &event.end).unwrap_or(start) < now - GRACE
            {
                Vec::new()
            } else {
                vec![(event.start.clone(), event.start.clone())]
            }
        };

        for (start, original_start) in occurrences {
            let source = overrides_for_series
                .iter()
                .find(|over| {
                    over.recurrence_id
                        .as_deref()
                        .is_some_and(|value| crate::util::same_instant(value, &original_start))
                })
                .unwrap_or(event);
            let Some(start_instant) = crate::util::event_start_instant(source, &start) else {
                continue;
            };
            let end_value =
                crate::util::occurrence_end(event, &start).unwrap_or_else(|_| start.clone());
            let end_instant =
                crate::util::event_end_instant(source, &end_value).unwrap_or(start_instant);
            if end_instant < now - GRACE {
                continue;
            }
            if source.reminders.is_empty() {
                continue;
            }

            // Suppression: dismissed always wins; a future snooze suppresses.
            let state = reminder_state::get(conn, &event.id, &original_start)?;
            let mut snoozed_due = false;
            if let Some(state) = &state {
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
            if !snoozed_due && store::reminder_fired(conn, &event.id, &original_start)? {
                continue;
            }

            let due = snoozed_due
                || source.reminders.iter().any(|minutes| {
                    let trigger = start_instant - Duration::minutes(i64::from(*minutes));
                    // Fire when due, or catch up while the event has not started.
                    trigger <= now && (trigger >= now - GRACE || start_instant > now)
                });
            if !due {
                continue;
            }

            hits.push(ReminderHit {
                kind: ReminderKind::Event,
                id: event.id.clone(),
                instance_start: Some(original_start),
                title: source.title.clone(),
                start: Some(start),
                all_day: source.all_day,
                location: if source.location.trim().is_empty() {
                    None
                } else {
                    Some(source.location.clone())
                },
            });
        }
    }
    hits.sort_by(|left, right| left.start.cmp(&right.start));
    Ok(hits)
}

/// Runs the reminder loop and owns the extensible source list.
#[derive(Clone)]
pub struct ReminderEngine {
    ctx: CoreContext,
    sources: Arc<Mutex<Vec<Arc<dyn ReminderSource>>>>,
    changed: Arc<Notify>,
    interval: std::time::Duration,
}

impl std::fmt::Debug for ReminderEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self
            .sources
            .lock()
            .map(|sources| sources.len())
            .unwrap_or(0);
        f.debug_struct("ReminderEngine")
            .field("sources", &count)
            .finish()
    }
}

impl ReminderEngine {
    pub fn new(ctx: CoreContext) -> Self {
        Self {
            ctx,
            sources: Arc::new(Mutex::new(vec![Arc::new(CalendarReminderSource)])),
            changed: Arc::new(Notify::new()),
            interval: std::time::Duration::from_secs(30),
        }
    }

    /// Register another source (task reminders in stage 2).  Safe to call
    /// after the loop started.
    pub fn add_source(&self, source: Arc<dyn ReminderSource>) {
        if let Ok(mut sources) = self.sources.lock() {
            sources.push(source);
        }
    }

    /// Wake the loop early after event/task changes.
    pub fn notify_changed(&self) {
        self.changed.notify_waiters();
    }

    /// Run until the returned future is aborted by the service.
    pub async fn run(self) {
        loop {
            let tick = self.tick().await;
            if let Err(error) = tick {
                warn!(%error, "reminder tick failed");
            }
            tokio::select! {
                _ = tokio::time::sleep(self.interval) => {}
                _ = self.changed.notified() => {}
            }
        }
    }

    /// One pass over every source.
    pub async fn tick(&self) -> Result<()> {
        let now = Utc::now();
        let until = now + Duration::hours(48);
        let sources: Vec<Arc<dyn ReminderSource>> = match self.sources.lock() {
            Ok(sources) => sources.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        for source in sources.iter() {
            match source.due(&self.ctx, now, until).await {
                Ok(hits) => {
                    for hit in hits {
                        if let Err(error) = self.fire(&hit, now).await {
                            warn!(source = source.name(), id = %hit.id, %error, "could not deliver reminder");
                        }
                    }
                }
                Err(error) => warn!(source = source.name(), %error, "reminder source failed"),
            }
        }
        if let Err(error) = self.ctx.db.write_sync(store::prune_reminder_fired) {
            debug!(%error, "pruning fired reminder markers failed");
        }
        if let Err(error) = self.ctx.db.write_sync(reminder_state::prune_task_state) {
            debug!(%error, "pruning task reminder state failed");
        }
        Ok(())
    }

    async fn fire(&self, hit: &ReminderHit, now: DateTime<Utc>) -> Result<()> {
        let db = self.ctx.db.clone();
        let should_fire = {
            let hit = hit.clone();
            db.write(move |conn| {
                let instance = hit.instance_start.clone().unwrap_or_default();
                let state_kind = match hit.kind {
                    ReminderKind::Event => reminder_state::KIND_EVENT,
                    ReminderKind::Task => reminder_state::KIND_TASK,
                };
                let state = reminder_state::get_for(conn, state_kind, &hit.id, &instance)?;
                let mut released_snooze = false;
                if let Some(state) = &state {
                    if state.dismissed_at.is_some() {
                        return Ok(false);
                    }
                    if let Some(until) = state.snoozed_until {
                        if until > now {
                            return Ok(false);
                        }
                        // A snooze that has expired makes the reminder eligible
                        // again even when its marker says it already fired.
                        match hit.kind {
                            ReminderKind::Event => {
                                store::clear_reminder_fired(conn, &hit.id, &instance)?
                            }
                            ReminderKind::Task => {
                                store::clear_task_reminder_fired(conn, &hit.id, &instance)?
                            }
                        }
                        released_snooze = true;
                    }
                }
                match hit.kind {
                    ReminderKind::Event => {
                        if store::reminder_fired(conn, &hit.id, &instance)? {
                            return Ok(false);
                        }
                        store::mark_reminder_fired(conn, &hit.id, &instance)?;
                    }
                    ReminderKind::Task => {
                        if store::task_reminder_fired(conn, &hit.id, &instance)? {
                            return Ok(false);
                        }
                        store::mark_task_reminder_fired(conn, &hit.id, &instance)?;
                    }
                }
                // The snooze has been delivered; drop the expired snooze so
                // the fired marker prevents a 30-second re-fire loop.
                if released_snooze {
                    reminder_state::clear_for_occurrence_for(conn, state_kind, &hit.id, &instance)?;
                }
                Ok(true)
            })
            .await?
        };
        if !should_fire {
            return Ok(());
        }

        let body = match (&hit.start, &hit.location) {
            (Some(start), Some(location)) => format!("{start} · {location}"),
            (Some(start), None) => start.clone(),
            (None, Some(location)) => location.clone(),
            (None, None) => String::new(),
        };
        let target = match hit.kind {
            ReminderKind::Event => NavigateTarget::calendar(&hit.id),
            ReminderKind::Task => NavigateTarget::task(&hit.id),
        };
        self.ctx.notifier.reminder(&hit.title, &body, target);
        emit_reminder_due(
            self.ctx.events.as_ref(),
            ReminderDue {
                kind: hit.kind.as_core(),
                id: hit.id.clone(),
                instance_start: hit.instance_start.clone(),
                title: hit.title.clone(),
                start: hit.start.clone(),
                all_day: hit.all_day,
                location: hit.location.clone(),
            },
        );
        debug!(kind = hit.kind.as_str(), id = %hit.id, "reminder delivered");
        Ok(())
    }
}

/// Helper used by the service when an occurrence is dismissed.
pub fn snooze_allows_refire(event: &StoredEvent) -> bool {
    event.event.status != EventStatus::Cancelled
        && !event.event.reminders.is_empty()
        && event.event.my_response != Some(AttendeeStatus::Declined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use nuncio_core::models::{new_id, CalendarEvent};
    use nuncio_core::{CoreContext, Paths};

    async fn ctx() -> CoreContext {
        let dir = std::env::temp_dir().join(format!("nuncio-pim-rem-{}", new_id()));
        let ctx = CoreContext::for_test(Paths::new(dir)).await.expect("ctx");
        ctx.migrate(crate::store::PIM_NAMESPACE, crate::store::PIM_MIGRATIONS)
            .expect("pim schema");
        ctx
    }

    fn event(id: &str, start: &str, end: &str, reminders: Vec<i32>) -> CalendarEvent {
        let mut event = CalendarEvent::new(
            id,
            "local-calendar",
            "local",
            "uid-1",
            "Meeting",
            start,
            end,
        );
        event.reminders = reminders;
        event
    }

    #[tokio::test]
    async fn fires_reminder_in_window() {
        let ctx = ctx().await;
        let now = Utc.with_ymd_and_hms(2026, 11, 12, 9, 50, 0).unwrap();
        let event = event(
            "e1",
            "2026-11-12T10:00:00Z",
            "2026-11-12T11:00:00Z",
            vec![15],
        );
        let stored = vec![StoredEvent::new(event)];
        let stored_owned = stored.clone();
        ctx.db
            .write(move |conn| {
                for item in &stored_owned {
                    events::upsert(conn, item)?;
                }
                Ok(())
            })
            .await
            .expect("insert");
        let hits = ctx
            .db
            .read(move |conn| compute_calendar_hits(conn, &stored, now, now + Duration::hours(48)))
            .await
            .expect("hits");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "e1");
        assert_eq!(
            hits[0].instance_start.as_deref(),
            Some("2026-11-12T10:00:00Z")
        );
    }

    #[tokio::test]
    async fn all_day_reminder_uses_event_time_zone() {
        let ctx = ctx().await;
        let mut event = event("e-all-day", "2026-11-12", "2026-11-13", vec![0]);
        event.all_day = true;
        event.time_zone = "Asia/Shanghai".into();
        let stored = vec![StoredEvent::new(event)];
        let stored_owned = stored.clone();
        ctx.db
            .write(move |conn| {
                for item in &stored_owned {
                    events::upsert(conn, item)?;
                }
                Ok(())
            })
            .await
            .expect("insert");
        let stored_for_late = stored.clone();
        // 2026-11-11T16:00:00Z is midnight on 12 Nov in Asia/Shanghai.
        let now = Utc.with_ymd_and_hms(2026, 11, 11, 16, 0, 0).unwrap();
        let hits = ctx
            .db
            .read(move |conn| compute_calendar_hits(conn, &stored, now, now + Duration::hours(48)))
            .await
            .expect("hits");
        assert_eq!(hits.len(), 1, "all-day reminder fires at local midnight");
        assert_eq!(hits[0].id, "e-all-day");

        // Before local midnight it must not fire yet (UTC midnight handling was
        // the defect: 00:00 UTC is 08:00 in Shanghai).
        let before = now - Duration::minutes(30);
        let hits = ctx
            .db
            .read(move |conn| {
                compute_calendar_hits(conn, &stored_for_late, before, before + Duration::hours(48))
            })
            .await
            .expect("hits");
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn hidden_calendars_still_fire_reminders() {
        let ctx = ctx().await;
        let now = Utc.with_ymd_and_hms(2026, 11, 12, 9, 50, 0).unwrap();
        let event = event(
            "e-hidden",
            "2026-11-12T10:00:00Z",
            "2026-11-12T11:00:00Z",
            vec![15],
        );
        let stored = vec![StoredEvent::new(event)];
        ctx.db
            .write(move |conn| {
                for item in &stored {
                    events::upsert(conn, item)?;
                }
                nuncio_core::repo::calendars::set_visible(conn, "local-calendar", false)?;
                Ok(())
            })
            .await
            .expect("insert");
        // Calendar visibility is only a view toggle; reminders must still fire.
        let hits = CalendarReminderSource
            .due(&ctx, now, now + Duration::hours(48))
            .await
            .expect("hits");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "e-hidden");
    }

    #[tokio::test]
    async fn skips_dismissed_and_fired() {
        let ctx = ctx().await;
        let now = Utc.with_ymd_and_hms(2026, 11, 12, 9, 50, 0).unwrap();
        let event = event(
            "e2",
            "2026-11-12T10:00:00Z",
            "2026-11-12T11:00:00Z",
            vec![15],
        );
        let stored = vec![StoredEvent::new(event)];
        let stored_owned = stored.clone();
        ctx.db
            .write(move |conn| {
                for item in &stored_owned {
                    events::upsert(conn, item)?;
                }
                reminder_state::dismiss(conn, "e2", "2026-11-12T10:00:00Z")?;
                Ok(())
            })
            .await
            .expect("insert");
        let hits = ctx
            .db
            .read({
                let stored = stored.clone();
                move |conn| compute_calendar_hits(conn, &stored, now, now + Duration::hours(48))
            })
            .await
            .expect("hits");
        assert!(hits.is_empty());

        // Clear dismissal, mark fired -> still skipped.
        ctx.db
            .write(|conn| {
                reminder_state::clear_for_event(conn, "e2")?;
                store::mark_reminder_fired(conn, "e2", "2026-11-12T10:00:00Z")
            })
            .await
            .expect("fired");
        let hits = ctx
            .db
            .read({
                let stored = stored.clone();
                move |conn| compute_calendar_hits(conn, &stored, now, now + Duration::hours(48))
            })
            .await
            .expect("hits");
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn expands_recurring_occurrences() {
        let ctx = ctx().await;
        let now = Utc.with_ymd_and_hms(2026, 11, 12, 9, 50, 0).unwrap();
        let mut event = event(
            "e3",
            "2026-11-12T10:00:00Z",
            "2026-11-12T11:00:00Z",
            vec![15],
        );
        event.recurrence = Some(nuncio_core::models::RecurrenceRule {
            freq: nuncio_core::models::RecurrenceFreq::Daily,
            interval: 1,
            ..Default::default()
        });
        let stored = vec![StoredEvent::new(event)];
        let stored_owned = stored.clone();
        ctx.db
            .write(move |conn| {
                for item in &stored_owned {
                    events::upsert(conn, item)?;
                }
                Ok(())
            })
            .await
            .expect("insert");
        let hits = ctx
            .db
            .read(move |conn| compute_calendar_hits(conn, &stored, now, now + Duration::hours(48)))
            .await
            .expect("hits");
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].instance_start.as_deref(),
            Some("2026-11-12T10:00:00+00:00")
        );
    }
}
