//! Task mapping helpers shared by the Google, Graph and DAV providers.

use chrono::{Duration, NaiveDate, TimeZone, Utc};
use nuncio_core::models::{
    new_id, CalendarEvent, Importance, Task, TaskInput, TaskStatus, TaskStep,
};

use crate::tasks::provider::TaskCapabilities;

/// "Today" for My Day / Planned defaults and recurring-task advancement.
///
/// Uses the machine's local date: the UI renders dates in local time, so a UTC
/// date would advance a task to the wrong day for users east of Greenwich
/// during the first hours after midnight.
pub fn today() -> String {
    chrono::Local::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string()
}

pub fn date_part(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Some(date.format("%Y-%m-%d").to_string());
    }
    let date = value.split('T').next().unwrap_or(value);
    NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()
        .map(|date| date.format("%Y-%m-%d").to_string())
}

/// Parse `2026-11-12T00:00:00.0000000` (Graph) or RFC 3339 into UTC.
pub fn parse_naive_utc(value: &str) -> Option<chrono::DateTime<Utc>> {
    let value = value.trim();
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(parsed.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(value, format) {
            return Some(Utc.from_utc_datetime(&naive));
        }
    }
    None
}

/// Format an instant the way Graph expects it (UTC naive, 7 fractional digits).
pub fn graph_datetime(value: chrono::DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%S.0000000").to_string()
}

/// Graph date-time at midnight for a task due date.
pub fn graph_due(date: &str) -> Option<String> {
    date_part(date).map(|date| format!("{date}T00:00:00.0000000"))
}

pub fn importance_name(importance: Importance) -> &'static str {
    importance.as_str()
}

pub fn importance_from(value: &str) -> Importance {
    match value.trim().to_ascii_lowercase().as_str() {
        "high" => Importance::High,
        "low" => Importance::Low,
        _ => Importance::Normal,
    }
}

/// Parse Google's zero-padded `position` into a sortable number.
pub fn position_to_order(position: Option<&str>) -> i64 {
    let Some(position) = position else {
        return 0;
    };
    let digits: String = position.chars().filter(|ch| ch.is_ascii_digit()).collect();
    if digits.is_empty() {
        return 0;
    }
    // Google positions are 20 zero-padded digits; the last 18 fit in i64 and
    // preserve ordering because the leading digits are always zero.
    let start = digits.len().saturating_sub(18);
    digits[start..].parse::<i64>().unwrap_or(0)
}

/// Preserve locally-owned fields that the provider cannot store.
pub fn preserve_local_fields(remote: &mut Task, existing: &Task, caps: TaskCapabilities) {
    if !caps.importance {
        remote.importance = existing.importance;
    }
    if !caps.reminders {
        remote.reminder_at = existing.reminder_at;
    }
    if !caps.recurrence {
        remote.recurrence = existing.recurrence.clone();
    }
    if !caps.steps {
        remote.steps = existing.steps.clone();
    }
    if !caps.ordering {
        // Providers without a server-side order must not reset the local
        // drag-and-drop order to 0 on every sync.
        remote.sort_order = existing.sort_order;
    }
    // Always local-only:
    remote.my_day_date = existing.my_day_date.clone();
    remote.linked_message_id = existing.linked_message_id.clone();
    if remote.title.trim().is_empty() {
        remote.title = existing.title.clone();
    }
}

/// Compute the next occurrence of a recurring task after completion.
///
/// The next date is the first rule occurrence on or after
/// `max(due + 1 day, today)`, so completing a task that is weeks overdue
/// immediately produces the next *future* occurrence instead of another
/// overdue one (matching Microsoft To Do / Google Tasks behaviour).
/// `today` is passed in by the caller (`YYYY-MM-DD`) so the logic is testable.
///
/// Returns `None` when the task has no due date or no recurrence, or when the
/// rule produces no further date.
pub fn next_occurrence(task: &Task, today: &str) -> Option<TaskInput> {
    let rule = task.recurrence.clone()?;
    let due = task.due_date.as_deref().and_then(date_part)?;
    let date = NaiveDate::parse_from_str(&due, "%Y-%m-%d").ok()?;
    let next_day = date + Duration::days(1);
    let today = NaiveDate::parse_from_str(today.trim(), "%Y-%m-%d").ok();
    let from_date = match today {
        Some(today) => next_day.max(today),
        None => next_day,
    };
    let end_date = date + Duration::days(1);
    let mut event = CalendarEvent::new(
        "recurring-task",
        "local",
        "local",
        "recurring-task",
        task.title.clone(),
        date.format("%Y-%m-%d").to_string(),
        end_date.format("%Y-%m-%d").to_string(),
    );
    event.all_day = true;
    event.time_zone = "UTC".into();
    event.recurrence = Some(rule.clone());

    // List the occurrences that come *after* this task. A COUNT-bounded rule needs
    // all of them (they are what bounds the series); an open-ended rule only
    // needs the first.
    //
    // The window starts small and grows until it holds enough occurrences: a
    // fixed window truncates a long-interval rule ("every 20 years") and would
    // wrongly report the series as finished.
    // A bounded series is enumerated from the day after this task so the skipped
    // occurrences can be counted. An open-ended series only needs the next date,
    // so it is searched from `max(due + 1, today)` and costs one occurrence.
    let start_from = match rule.count {
        Some(_) => date + Duration::days(1),
        None => from_date,
    };
    let limit = rule.count.map(|count| count as usize).unwrap_or(1).max(1);
    const MAX_HORIZON_DAYS: i64 = 366 * 500;
    let mut horizon_days: i64 = 366;
    // Grow the search window until it holds enough occurrences: a fixed window
    // truncates long-interval rules ("every 20 years") and would wrongly report
    // the series as finished.
    let occurrences = loop {
        let from = Utc.from_utc_datetime(&start_from.and_hms_opt(0, 0, 0)?);
        let to = Utc
            .from_utc_datetime(&(start_from + Duration::days(horizon_days)).and_hms_opt(0, 0, 0)?);
        let found = nuncio_ical::recurrence::expand(&event, &[], from, to, limit);
        if found.len() >= limit || horizon_days >= MAX_HORIZON_DAYS {
            break found;
        }
        horizon_days = (horizon_days * 4).min(MAX_HORIZON_DAYS);
    };

    // Completing a task late skips the occurrences in between; those still count
    // against a bounded series. Open-ended series skip nothing that matters, so
    // the search already started at `today`.
    let today_str = from_date.format("%Y-%m-%d").to_string();
    let skipped = if rule.count.is_some() {
        occurrences
            .iter()
            .take_while(|occurrence| {
                date_part(&occurrence.start).is_some_and(|due| due < today_str)
            })
            .count()
    } else {
        0
    };
    let next = occurrences.get(skipped)?;
    let next_due = date_part(&next.start)?;
    let shift = NaiveDate::parse_from_str(&next_due, "%Y-%m-%d").ok()? - date;
    let reminder_at = task
        .reminder_at
        .map(|reminder| reminder + Duration::days(shift.num_days()));

    let steps = task
        .steps
        .iter()
        .map(|step| TaskStep {
            id: new_id(),
            title: step.title.clone(),
            is_completed: false,
            sort_order: step.sort_order,
        })
        .collect();

    // The follow-up carries what is left after this task and the skipped ones.
    // Zero would mean the series has no occurrence left to show.
    let mut next_rule = rule;
    if let Some(count) = next_rule.count {
        // `skipped` covers the periods passed over; the task being completed is
        // the one that just consumed a slot, so it comes off as well.
        let consumed = skipped as u32 + 1;
        let remaining = count.saturating_sub(consumed);
        if remaining == 0 {
            return None;
        }
        next_rule.count = Some(remaining);
    }

    Some(TaskInput {
        title: task.title.clone(),
        notes: task.notes.clone(),
        importance: task.importance,
        due_date: Some(next_due),
        reminder_at,
        recurrence: Some(next_rule),
        my_day_date: None,
        steps,
    })
}

/// Sort a custom list by `sort_order` (ascending).
///
/// New tasks receive `MIN(sort_order) - 1` when `newTasksOnTop` is on (they
/// land first) and `MAX(sort_order) + 1` when it is off (they land last), so a
/// single ascending sort implements both settings.
pub fn sort_for_list(tasks: &mut [Task]) {
    tasks.sort_by(|left, right| {
        left.sort_order
            .cmp(&right.sort_order)
            .then_with(|| left.created_at.cmp(&right.created_at))
    });
}

/// Task status helper: Graph reports several non-completed statuses.
pub fn status_from_graph(value: &str) -> TaskStatus {
    if value.eq_ignore_ascii_case("completed") {
        TaskStatus::Completed
    } else {
        TaskStatus::NotStarted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::models::{RecurrenceFreq, RecurrenceRule};

    fn task(due: &str, freq: RecurrenceFreq) -> Task {
        let mut task = Task {
            id: "t1".into(),
            list_id: "list".into(),
            account_id: "acc".into(),
            title: "Water plants".into(),
            notes: String::new(),
            status: TaskStatus::NotStarted,
            importance: Importance::Normal,
            due_date: Some(due.into()),
            reminder_at: None,
            recurrence: Some(RecurrenceRule {
                freq,
                interval: 1,
                ..Default::default()
            }),
            my_day_date: None,
            completed_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            steps: vec![TaskStep {
                id: "s1".into(),
                title: "Fill can".into(),
                is_completed: true,
                sort_order: 0,
            }],
            sort_order: 0,
            linked_message_id: None,
        };
        task.reminder_at = Utc.with_ymd_and_hms(2026, 11, 12, 8, 0, 0).single();
        task
    }

    #[test]
    fn advances_daily_task() {
        let next = next_occurrence(&task("2026-11-12", RecurrenceFreq::Daily), "2026-11-01")
            .expect("next");
        assert_eq!(next.due_date.as_deref(), Some("2026-11-13"));
        assert_eq!(next.steps.len(), 1);
        assert!(!next.steps[0].is_completed);
        assert_ne!(next.steps[0].id, "s1");
        assert!(next.reminder_at.is_some());
        assert!(next.my_day_date.is_none());
    }

    #[test]
    fn advances_weekly_task() {
        let next = next_occurrence(&task("2026-11-12", RecurrenceFreq::Weekly), "2026-11-01")
            .expect("next");
        assert_eq!(next.due_date.as_deref(), Some("2026-11-19"));
    }

    #[test]
    fn skips_overdue_periods_when_completing() {
        // Due 10 weeks ago: the next task must be in the future, not another
        // overdue period.
        let next = next_occurrence(&task("2026-09-03", RecurrenceFreq::Weekly), "2026-11-12")
            .expect("next");
        assert_eq!(next.due_date.as_deref(), Some("2026-11-12"));
        // Same rule, one day later -> next week.
        let next = next_occurrence(&task("2026-09-03", RecurrenceFreq::Weekly), "2026-11-13")
            .expect("next");
        assert_eq!(next.due_date.as_deref(), Some("2026-11-19"));
        // Daily overdue by 10 days -> today.
        let next = next_occurrence(&task("2026-11-02", RecurrenceFreq::Daily), "2026-11-12")
            .expect("next");
        assert_eq!(next.due_date.as_deref(), Some("2026-11-12"));
        assert!(next.reminder_at.is_some());
    }

    #[test]
    fn stops_at_until() {
        let mut original = task("2026-11-12", RecurrenceFreq::Daily);
        if let Some(rule) = original.recurrence.as_mut() {
            rule.until = Some("2026-11-12".into());
        }
        assert!(next_occurrence(&original, "2026-11-01").is_none());
    }

    #[test]
    fn parses_dates_and_positions() {
        assert_eq!(
            date_part("2026-11-12T00:00:00Z").as_deref(),
            Some("2026-11-12")
        );
        assert_eq!(date_part("nonsense"), None);
        assert_eq!(position_to_order(Some("000000000000001")), 1);
        assert_eq!(position_to_order(Some("garbage")), 0);
        assert_eq!(importance_from("HIGH"), Importance::High);
        assert_eq!(importance_from("critical"), Importance::Normal);
    }

    #[test]
    fn preserves_unsupported_fields() {
        let mut existing = task("2026-11-12", RecurrenceFreq::Daily);
        existing.importance = Importance::High;
        existing.my_day_date = Some("2026-11-12".into());
        let mut remote = existing.clone();
        remote.importance = Importance::Normal;
        remote.recurrence = None;
        remote.my_day_date = None;
        let caps = TaskCapabilities {
            importance: false,
            reminders: false,
            steps: false,
            recurrence: false,
            ordering: false,
            server_recurring_completion: false,
        };
        preserve_local_fields(&mut remote, &existing, caps);
        assert_eq!(remote.importance, Importance::High);
        assert!(remote.recurrence.is_some());
        assert_eq!(remote.my_day_date.as_deref(), Some("2026-11-12"));
    }
}
