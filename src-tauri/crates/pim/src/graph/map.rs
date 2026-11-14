//! Microsoft Graph (v1.0) event ⇄ core model mapping (pure, unit-tested).

use chrono::{DateTime, NaiveDate, Utc};
use nuncio_core::models::{
    new_id, Attendee, AttendeeRole, AttendeeStatus, CalendarEvent, EventStatus, RecurrenceFreq,
    RecurrenceRule, ShowAs, StoredEvent, Weekday,
};
use serde::Deserialize;
use tracing::warn;

use crate::error::Result;
use crate::provider::RemoteEvent;
use crate::util::{
    date_part, graph_to_instant, instant_in_zone, instant_to_graph_in_zone, normalize_zone,
    parse_instant, resolve_zone,
};

/* ─────────────────────────── API response types ─────────────────────────── */

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphDateTime {
    #[serde(default)]
    pub date_time: Option<String>,
    #[serde(default)]
    pub time_zone: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphEmailAddress {
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphResponseStatus {
    #[serde(default)]
    pub response: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphAttendee {
    #[serde(default)]
    pub email_address: Option<GraphEmailAddress>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub status: Option<GraphResponseStatus>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphOrganizer {
    #[serde(default)]
    pub email_address: Option<GraphEmailAddress>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphLocation {
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphBody {
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphPattern {
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub interval: Option<u32>,
    #[serde(default)]
    pub days_of_week: Option<Vec<String>>,
    #[serde(default)]
    pub day_of_month: Option<i32>,
    #[serde(default)]
    pub month: Option<u32>,
    #[serde(default)]
    pub index: Option<String>,
    #[serde(default)]
    pub first_day_of_week: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphRange {
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub start_date: Option<String>,
    #[serde(default)]
    pub end_date: Option<String>,
    #[serde(default)]
    pub number_of_occurrences: Option<u32>,
    #[serde(default)]
    pub recurrence_time_zone: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphRecurrence {
    #[serde(default)]
    pub pattern: Option<GraphPattern>,
    #[serde(default)]
    pub range: Option<GraphRange>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphOnlineMeeting {
    #[serde(default)]
    pub join_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphRemoved {
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphEvent {
    pub id: String,
    #[serde(default, rename = "iCalUId")]
    pub i_cal_uid: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub body: Option<GraphBody>,
    #[serde(default)]
    pub body_preview: Option<String>,
    #[serde(default)]
    pub start: Option<GraphDateTime>,
    #[serde(default)]
    pub end: Option<GraphDateTime>,
    #[serde(default)]
    pub is_all_day: Option<bool>,
    #[serde(default)]
    pub location: Option<GraphLocation>,
    #[serde(default)]
    pub locations: Option<Vec<GraphLocation>>,
    #[serde(default)]
    pub recurrence: Option<GraphRecurrence>,
    #[serde(default)]
    pub series_master_id: Option<String>,
    #[serde(default)]
    pub original_start: Option<String>,
    #[serde(default)]
    pub original_start_time_zone: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub is_cancelled: Option<bool>,
    #[serde(default)]
    pub show_as: Option<String>,
    #[serde(default)]
    pub sensitivity: Option<String>,
    #[serde(default)]
    pub attendees: Option<Vec<GraphAttendee>>,
    #[serde(default)]
    pub organizer: Option<GraphOrganizer>,
    #[serde(default)]
    pub is_online_meeting: Option<bool>,
    #[serde(default)]
    pub online_meeting: Option<GraphOnlineMeeting>,
    #[serde(default)]
    pub reminder_minutes_before_start: Option<i32>,
    #[serde(default)]
    pub is_reminder_on: Option<bool>,
    #[serde(default)]
    pub response_status: Option<GraphResponseStatus>,
    #[serde(default)]
    pub last_modified_date_time: Option<String>,
    #[serde(default)]
    pub change_key: Option<String>,
    #[serde(default, rename = "@removed")]
    pub removed: Option<GraphRemoved>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GraphEventPage {
    #[serde(default)]
    pub value: Vec<GraphEvent>,
    #[serde(default, rename = "@odata.nextLink")]
    pub next_link: Option<String>,
    #[serde(default, rename = "@odata.deltaLink")]
    pub delta_link: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphCalendar {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub hex_color: Option<String>,
    #[serde(default)]
    pub can_edit: Option<bool>,
    #[serde(default)]
    pub is_default_calendar: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GraphCalendarPage {
    #[serde(default)]
    pub value: Vec<GraphCalendar>,
    #[serde(default, rename = "@odata.nextLink")]
    pub next_link: Option<String>,
}

/* ───────────────────────────── calendar mapping ──────────────────────────── */

pub fn map_calendar(calendar: &GraphCalendar) -> crate::provider::RemoteCalendar {
    let name = calendar
        .name
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| calendar.id.clone());
    let color = calendar
        .hex_color
        .as_deref()
        .and_then(crate::util::normalize_color)
        .or_else(|| {
            calendar.color.as_deref().and_then(|color| {
                crate::util::normalize_color(color).or_else(|| preset_color_hex(color))
            })
        });
    crate::provider::RemoteCalendar {
        local_id: String::new(),
        remote_id: calendar.id.clone(),
        href: None,
        name,
        color,
        read_only: calendar.can_edit == Some(false),
        sync_token: None,
        ctag: None,
        supports_sync_collection: false,
    }
}

/* ─────────────────────────────── event mapping ───────────────────────────── */

pub fn attendee_status_name(status: AttendeeStatus) -> &'static str {
    match status {
        AttendeeStatus::NeedsAction => "none",
        AttendeeStatus::Accepted => "accepted",
        AttendeeStatus::Declined => "declined",
        AttendeeStatus::Tentative => "tentativelyAccepted",
        AttendeeStatus::Delegated => "none",
    }
}

fn attendee_status(value: Option<&str>) -> AttendeeStatus {
    match value.unwrap_or("none") {
        "accepted" => AttendeeStatus::Accepted,
        "declined" => AttendeeStatus::Declined,
        "tentativelyAccepted" => AttendeeStatus::Tentative,
        _ => AttendeeStatus::NeedsAction,
    }
}

fn weekday(value: &str) -> Option<Weekday> {
    match value.trim().to_ascii_lowercase().as_str() {
        "monday" => Some(Weekday::Mo),
        "tuesday" => Some(Weekday::Tu),
        "wednesday" => Some(Weekday::We),
        "thursday" => Some(Weekday::Th),
        "friday" => Some(Weekday::Fr),
        "saturday" => Some(Weekday::Sa),
        "sunday" => Some(Weekday::Su),
        _ => None,
    }
}

fn weekday_name(day: Weekday) -> &'static str {
    match day {
        Weekday::Mo => "monday",
        Weekday::Tu => "tuesday",
        Weekday::We => "wednesday",
        Weekday::Th => "thursday",
        Weekday::Fr => "friday",
        Weekday::Sa => "saturday",
        Weekday::Su => "sunday",
    }
}

fn set_pos(index: Option<&str>) -> Option<i32> {
    match index?.trim().to_ascii_lowercase().as_str() {
        "first" => Some(1),
        "second" => Some(2),
        "third" => Some(3),
        "fourth" => Some(4),
        "last" => Some(-1),
        _ => None,
    }
}

fn index_name(position: i32) -> &'static str {
    match position {
        1 => "first",
        2 => "second",
        3 => "third",
        4 => "fourth",
        -1 => "last",
        _ => "first",
    }
}

/// Convert Graph `pattern` + `range` into the core recurrence model.
pub fn map_recurrence(recurrence: &GraphRecurrence) -> Option<RecurrenceRule> {
    let pattern = recurrence.pattern.clone()?;
    let range = recurrence.range.clone().unwrap_or_default();
    let (freq, by_weekday, by_month_day, by_month, by_set_pos) =
        match pattern.kind.as_deref().unwrap_or("weekly") {
            "daily" => (
                RecurrenceFreq::Daily,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ),
            "weekly" => (
                RecurrenceFreq::Weekly,
                pattern
                    .days_of_week
                    .clone()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|day| weekday(day))
                    .collect(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ),
            "absoluteMonthly" => (
                RecurrenceFreq::Monthly,
                Vec::new(),
                pattern.day_of_month.into_iter().collect(),
                Vec::new(),
                Vec::new(),
            ),
            "relativeMonthly" => (
                RecurrenceFreq::Monthly,
                pattern
                    .days_of_week
                    .clone()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|day| weekday(day))
                    .collect(),
                Vec::new(),
                Vec::new(),
                set_pos(pattern.index.as_deref()).into_iter().collect(),
            ),
            "absoluteYearly" => (
                RecurrenceFreq::Yearly,
                Vec::new(),
                pattern.day_of_month.into_iter().collect(),
                pattern
                    .month
                    .map(|month| month as i32)
                    .into_iter()
                    .collect(),
                Vec::new(),
            ),
            "relativeYearly" => (
                RecurrenceFreq::Yearly,
                pattern
                    .days_of_week
                    .clone()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|day| weekday(day))
                    .collect(),
                Vec::new(),
                pattern
                    .month
                    .map(|month| month as i32)
                    .into_iter()
                    .collect(),
                set_pos(pattern.index.as_deref()).into_iter().collect(),
            ),
            _ => return None,
        };
    let count = match range.kind.as_deref() {
        Some("numbered") => range.number_of_occurrences,
        _ => None,
    };
    let until = match range.kind.as_deref() {
        Some("endDate") => range
            .end_date
            .as_deref()
            .and_then(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d").ok())
            .map(|date| date.format("%Y-%m-%d").to_string()),
        _ => None,
    };
    Some(RecurrenceRule {
        freq,
        interval: pattern.interval.unwrap_or(1).max(1),
        by_weekday,
        by_set_pos,
        by_month_day,
        by_month,
        count,
        until,
    })
}

/// Convert a Graph event into the provider-neutral remote event.
pub fn map_event(
    ge: &GraphEvent,
    account_email: &str,
    calendar_id: &str,
    account_id: &str,
    default_reminder_minutes: i32,
) -> Result<RemoteEvent> {
    let uid = ge
        .i_cal_uid
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| ge.id.clone());
    let start_part = ge.start.clone().unwrap_or_default();
    let end_part = ge.end.clone().unwrap_or_default();
    // `Prefer: outlook.timezone="UTC"` makes Graph return UTC wall-clock values.
    // For recurring events the series time zone is available as
    // `recurrence.range.recurrenceTimeZone`; use it (never treat the UTC
    // dateTime as that zone's wall clock) so DST transitions stay stable.
    let recurrence_zone = ge
        .recurrence
        .as_ref()
        .and_then(|recurrence| recurrence.range.as_ref())
        .and_then(|range| range.recurrence_time_zone.as_deref())
        .filter(|zone| !zone.trim().is_empty());
    let original_zone = ge
        .original_start_time_zone
        .as_deref()
        .filter(|zone| !zone.trim().is_empty());
    let zone = normalize_zone(
        recurrence_zone
            .or(original_zone)
            .or(start_part.time_zone.as_deref())
            .unwrap_or("UTC"),
    );
    let all_day = ge.is_all_day == Some(true);

    let (start, end) = if all_day {
        let start = start_part
            .date_time
            .as_deref()
            .and_then(graph_date)
            .unwrap_or_else(|| "1970-01-01".to_string());
        let end = end_part
            .date_time
            .as_deref()
            .and_then(graph_date)
            .unwrap_or_else(|| start.clone());
        (start, end)
    } else {
        // Graph returns the requested (UTC) instant; convert it into the
        // event's own zone before storing the wall-clock string.
        let start = start_part
            .date_time
            .as_deref()
            .and_then(|value| graph_to_instant(value, "UTC"))
            .and_then(|value| parse_instant(&value).ok())
            .map(|instant| instant_in_zone(instant, &zone))
            .unwrap_or_else(|| start_part.date_time.clone().unwrap_or_default());
        let end = end_part
            .date_time
            .as_deref()
            .and_then(|value| graph_to_instant(value, "UTC"))
            .and_then(|value| parse_instant(&value).ok())
            .map(|instant| instant_in_zone(instant, &zone))
            .unwrap_or_else(|| end_part.date_time.clone().unwrap_or_default());
        (start, end)
    };

    let recurrence = ge.recurrence.as_ref().and_then(map_recurrence);
    // Baseline RRULE text for recurring series.  Graph only exposes a single
    // `index` per pattern, so a rule our structured model cannot express (for
    // example `BYDAY=1MO,3WE`) would be silently degraded if it were rebuilt
    // from `recurrence`.  Keeping the derived text lets updates detect an
    // unchanged rule and leave the server's stored pattern alone.
    let rrule = recurrence
        .as_ref()
        .map(nuncio_ical::recurrence::to_rrule_string);

    let recurrence_id = ge.original_start.as_deref().and_then(|value| {
        let rid_zone = normalize_zone(original_zone.or(recurrence_zone).unwrap_or(zone.as_str()));
        if all_day {
            graph_date(value)
        } else {
            graph_to_instant(value, "UTC")
                .and_then(|value| parse_instant(&value).ok())
                .map(|instant| instant_in_zone(instant, &rid_zone))
                .or_else(|| graph_date(value))
        }
    });

    let organizer = ge.organizer.as_ref().and_then(|organizer| {
        let email = organizer.email_address.as_ref()?.address.clone()?;
        Some(Attendee {
            email,
            name: organizer
                .email_address
                .as_ref()
                .and_then(|address| address.name.clone()),
            role: AttendeeRole::Chair,
            status: AttendeeStatus::Accepted,
        })
    });

    let attendees: Vec<Attendee> = ge
        .attendees
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|attendee| {
            let address = attendee.email_address?;
            let email = address.address?;
            let role = match attendee.kind.as_deref() {
                Some("optional") => AttendeeRole::Optional,
                Some("resource") => AttendeeRole::Resource,
                _ => AttendeeRole::Required,
            };
            Some(Attendee {
                email,
                name: address.name,
                role,
                status: attendee_status(
                    attendee
                        .status
                        .as_ref()
                        .and_then(|status| status.response.as_deref()),
                ),
            })
        })
        .collect();

    let my_response = ge
        .attendees
        .as_ref()
        .and_then(|list| {
            list.iter().find(|attendee| {
                attendee
                    .email_address
                    .as_ref()
                    .and_then(|address| address.address.as_deref())
                    .is_some_and(|email| email.eq_ignore_ascii_case(account_email))
            })
        })
        .map(|attendee| {
            attendee_status(
                attendee
                    .status
                    .as_ref()
                    .and_then(|status| status.response.as_deref()),
            )
        })
        .or_else(|| {
            ge.response_status
                .as_ref()
                .and_then(|status| status.response.as_deref())
                .map(|value| attendee_status(Some(value)))
        });

    let is_organizer = ge.organizer.as_ref().is_some_and(|organizer| {
        organizer
            .email_address
            .as_ref()
            .and_then(|address| address.address.as_deref())
            .is_some_and(|email| email.eq_ignore_ascii_case(account_email))
    });

    let reminders = if ge.is_reminder_on == Some(false) {
        Vec::new()
    } else {
        match ge.reminder_minutes_before_start {
            Some(minutes) if minutes >= 0 => vec![minutes],
            _ if default_reminder_minutes > 0 => vec![default_reminder_minutes],
            _ => Vec::new(),
        }
    };

    let show_as = match ge.show_as.as_deref() {
        Some("free") => ShowAs::Free,
        Some("tentative") => ShowAs::Tentative,
        Some("oof") => ShowAs::Oof,
        Some("workingElsewhere") => ShowAs::WorkingElsewhere,
        _ => ShowAs::Busy,
    };

    let is_private = matches!(
        ge.sensitivity.as_deref(),
        Some("private") | Some("confidential")
    );
    let title = ge.subject.clone().unwrap_or_default();
    let location = ge
        .location
        .as_ref()
        .and_then(|location| location.display_name.clone())
        .or_else(|| {
            ge.locations
                .as_ref()
                .and_then(|list| list.first())
                .and_then(|location| location.display_name.clone())
        })
        .unwrap_or_default();
    let description = ge
        .body
        .as_ref()
        .and_then(|body| body.content.clone())
        .or_else(|| ge.body_preview.clone())
        .unwrap_or_default();
    let online_meeting_url = ge
        .online_meeting
        .as_ref()
        .and_then(|meeting| meeting.join_url.clone())
        .filter(|value| !value.trim().is_empty());
    let updated_at = ge
        .last_modified_date_time
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
        .or_else(|| {
            ge.last_modified_date_time
                .as_deref()
                .and_then(|value| parse_instant(value).ok())
        });

    let event = CalendarEvent {
        id: new_id(),
        calendar_id: calendar_id.to_string(),
        account_id: account_id.to_string(),
        uid,
        title,
        location,
        description,
        start,
        end,
        all_day,
        time_zone: zone,
        recurrence,
        series_id: None,
        recurrence_id,
        status: if ge.is_cancelled == Some(true) {
            EventStatus::Cancelled
        } else {
            EventStatus::Confirmed
        },
        show_as,
        is_private,
        organizer,
        attendees,
        reminders,
        is_organizer,
        my_response,
        online_meeting_url,
        color: None,
        updated_at,
    };

    Ok(RemoteEvent {
        event,
        remote_id: Some(ge.id.clone()),
        href: None,
        etag: ge.change_key.clone(),
        rrule,
        exdates: Vec::new(),
        raw_ics: None,
        series_remote_id: ge.series_master_id.clone(),
    })
}

fn graph_date(value: &str) -> Option<String> {
    crate::util::parse_graph_datetime(value)
        .map(|naive| naive.date().format("%Y-%m-%d").to_string())
        .or_else(|| date_part(value))
}

/* ─────────────────────────────── outbound JSON ───────────────────────────── */

pub fn recurrence_body(rule: &RecurrenceRule, start: &str, zone: &str) -> serde_json::Value {
    let start_date = date_part(start).unwrap_or_else(|| "1970-01-01".to_string());
    let mut pattern = serde_json::Map::new();
    let kind = match rule.freq {
        RecurrenceFreq::Daily => "daily",
        RecurrenceFreq::Weekly => "weekly",
        RecurrenceFreq::Monthly => {
            if !rule.by_weekday.is_empty() {
                "relativeMonthly"
            } else {
                "absoluteMonthly"
            }
        }
        RecurrenceFreq::Yearly => {
            if !rule.by_weekday.is_empty() && !rule.by_month.is_empty() {
                "relativeYearly"
            } else {
                "absoluteYearly"
            }
        }
    };
    pattern.insert("type".into(), serde_json::json!(kind));
    pattern.insert("interval".into(), serde_json::json!(rule.interval.max(1)));
    if !rule.by_weekday.is_empty() {
        pattern.insert(
            "daysOfWeek".into(),
            serde_json::json!(rule
                .by_weekday
                .iter()
                .map(|day| weekday_name(*day))
                .collect::<Vec<_>>()),
        );
    }
    if let Some(position) = rule.by_set_pos.first() {
        pattern.insert("index".into(), serde_json::json!(index_name(*position)));
    }
    if let Some(day) = rule.by_month_day.first() {
        pattern.insert("dayOfMonth".into(), serde_json::json!(day));
    }
    if let Some(month) = rule.by_month.first() {
        pattern.insert("month".into(), serde_json::json!(month));
    }
    pattern.insert("firstDayOfWeek".into(), serde_json::json!("sunday"));

    let mut range = serde_json::Map::new();
    if let Some(count) = rule.count {
        range.insert("type".into(), serde_json::json!("numbered"));
        range.insert("numberOfOccurrences".into(), serde_json::json!(count));
    } else if let Some(until) = &rule.until {
        range.insert("type".into(), serde_json::json!("endDate"));
        range.insert("endDate".into(), serde_json::json!(until));
    } else {
        range.insert("type".into(), serde_json::json!("noEnd"));
    }
    range.insert("startDate".into(), serde_json::json!(start_date));
    range.insert(
        "recurrenceTimeZone".into(),
        serde_json::json!(normalize_zone(zone)),
    );

    serde_json::json!({ "pattern": pattern, "range": range })
}

/// Build the Graph event body for `POST /events` and `PATCH /events/{id}`.
pub fn event_body(event: &CalendarEvent, for_create: bool) -> serde_json::Value {
    // Send the event's own wall-clock time together with its time zone.  The
    // previous implementation always sent UTC, which made every round-tripped
    // event live in UTC and shifted recurring series across DST.
    let zone = normalize_zone(&event.time_zone);
    let start = if event.all_day {
        let date = date_part(&event.start).unwrap_or_else(|| event.start.clone());
        serde_json::json!({ "dateTime": format!("{date}T00:00:00.0000000"), "timeZone": "UTC" })
    } else {
        serde_json::json!({
            "dateTime": instant_to_graph_in_zone(&event.start, &zone)
                .unwrap_or_else(|_| event.start.clone()),
            "timeZone": zone,
        })
    };
    let end = if event.all_day {
        let date = date_part(&event.end).unwrap_or_else(|| event.end.clone());
        serde_json::json!({ "dateTime": format!("{date}T00:00:00.0000000"), "timeZone": "UTC" })
    } else {
        serde_json::json!({
            "dateTime": instant_to_graph_in_zone(&event.end, &zone)
                .unwrap_or_else(|_| event.end.clone()),
            "timeZone": zone,
        })
    };

    let attendees: Vec<serde_json::Value> = event
        .attendees
        .iter()
        .map(|attendee| {
            let kind = match attendee.role {
                AttendeeRole::Optional => "optional",
                AttendeeRole::Resource => "resource",
                _ => "required",
            };
            serde_json::json!({
                "emailAddress": { "address": attendee.email, "name": attendee.name },
                "type": kind,
                "status": { "response": attendee_status_name(attendee.status) },
            })
        })
        .collect();

    let mut body = serde_json::json!({
        "subject": event.title,
        "body": { "contentType": "text", "content": event.description },
        "start": start,
        "end": end,
        "isAllDay": event.all_day,
        "location": { "displayName": event.location },
        "attendees": attendees,
        "showAs": match event.show_as {
            ShowAs::Free => "free",
            ShowAs::Tentative => "tentative",
            ShowAs::Oof => "oof",
            ShowAs::WorkingElsewhere => "workingElsewhere",
            ShowAs::Busy => "busy",
        },
        "sensitivity": if event.is_private { "private" } else { "normal" },
        "isReminderOn": !event.reminders.is_empty(),
        "reminderMinutesBeforeStart": event.reminders.first().copied().unwrap_or(0),
    });

    if let Some(rule) = &event.recurrence {
        body["recurrence"] = recurrence_body(rule, &event.start, &event.time_zone);
    } else if !for_create {
        body["recurrence"] = serde_json::Value::Null;
    }
    if for_create {
        body["iCalUId"] = serde_json::json!(event.uid);
    }
    body
}

/// Does the structured rule still describe the stored series?
///
/// Prefer the original RRULE text (it distinguishes patterns our model
/// approximates); fall back to the structured rule for rows synced before the
/// baseline was persisted.
pub fn recurrence_matches_stored(current: &StoredEvent, updated: Option<&RecurrenceRule>) -> bool {
    let stored = current
        .rrule
        .as_deref()
        .and_then(nuncio_ical::recurrence::from_rrule_string)
        .or_else(|| current.event.recurrence.clone());
    stored.as_ref() == updated
}

/// A structured rule that Graph cannot represent faithfully.  Graph's
/// `pattern` accepts a single `index`, so multiple BYSETPOS positions would be
/// truncated to the first one.
pub fn recurrence_is_lossy(rule: &RecurrenceRule) -> bool {
    if rule.by_set_pos.len() > 1 {
        return true;
    }
    nuncio_ical::recurrence::from_rrule_string_detailed(&nuncio_ical::recurrence::to_rrule_string(
        rule,
    ))
    .is_some_and(|parsed| parsed.lossy)
}

/// Build the PATCH body for a whole-series edit.
///
/// When the incoming recurrence still matches what the server stores the
/// `recurrence` key is omitted: Graph then keeps its stored pattern, which
/// preserves RRULEs (multi-ordinal BYDAY, multiple BYSETPOS, ...) that our
/// structured model only approximates.  A rule that actually changed but is
/// lossy is not written either; writing the truncated single-index form would
/// silently rewrite the series.
pub fn series_update_body(current: &StoredEvent, updated: &CalendarEvent) -> serde_json::Value {
    let mut body = event_body(updated, false);
    if recurrence_matches_stored(current, updated.recurrence.as_ref()) {
        if let Some(object) = body.as_object_mut() {
            object.remove("recurrence");
        }
    } else if let Some(rule) = updated.recurrence.as_ref() {
        if recurrence_is_lossy(rule) {
            warn!(
                event_uid = %current.event.uid,
                "Graph cannot represent this recurrence exactly; keeping the stored series pattern"
            );
            if let Some(object) = body.as_object_mut() {
                object.remove("recurrence");
            }
        }
    }
    body
}

/// Graph sometimes returns `isCancelled` masters; this helper mirrors the
/// Google logic used by the sync engine.
pub fn is_cancelled(ge: &GraphEvent) -> bool {
    ge.is_cancelled == Some(true)
}

pub fn is_removed(ge: &GraphEvent) -> bool {
    ge.removed.is_some()
}

/// Graph event kind: master, exception, occurrence, singleInstance.
pub fn is_exception(ge: &GraphEvent) -> bool {
    ge.kind.as_deref() == Some("exception")
        || (ge.series_master_id.is_some() && ge.original_start.is_some())
}

/// Build the response body for accept/decline endpoints.
pub fn response_body(comment: Option<&str>, send_response: bool) -> serde_json::Value {
    serde_json::json!({
        "comment": comment.unwrap_or_default(),
        "sendResponse": send_response,
    })
}

fn preset_color_hex(name: &str) -> Option<String> {
    let hex = match name.trim().to_ascii_lowercase().as_str() {
        "lightblue" => "#0078D4",
        "lightgreen" => "#00B294",
        "lightorange" => "#FF8C00",
        "lightgray" => "#7A7574",
        "lightyellow" => "#FFB900",
        "lightteal" => "#00B7C3",
        "lightpink" => "#E3008C",
        "lightbrown" => "#8E562E",
        "lightred" => "#D13438",
        "maxcolor" => "#0078D4",
        _ => return None,
    };
    Some(hex.to_string())
}

/// Map a hex color to the Graph calendar `color` preset (Graph does not accept
/// arbitrary hex values on write).
pub fn color_name(hex: &str) -> Option<&'static str> {
    let normalized = crate::util::normalize_color(hex)?;
    let name = match normalized.as_str() {
        "#0078D4" | "#0F6CBD" | "#3F51B5" => "lightBlue",
        "#33B679" | "#0B8043" | "#00B294" => "lightGreen",
        "#F6BF26" | "#FFB900" => "lightYellow",
        "#F4511E" | "#D50000" | "#E67C73" => "lightRed",
        "#8E24AA" => "lightPurple",
        "#616161" => "lightGray",
        "#039BE5" => "lightTeal",
        _ => return None,
    };
    Some(name)
}

/// Graph zone names may be Windows names; this helper is used by graph/mod.
pub fn zone_or_utc(value: &str) -> String {
    resolve_zone(value)
        .map(|zone| zone.name().to_string())
        .unwrap_or_else(|| "UTC".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph_event() -> serde_json::Value {
        serde_json::json!({
            "id": "AAMkAGI2",
            "iCalUId": "graph-uid@example.com",
            "subject": "Design review",
            "body": { "contentType": "text", "content": "Agenda" },
            "start": { "dateTime": "2026-11-12T01:00:00.0000000", "timeZone": "UTC" },
            "end": { "dateTime": "2026-11-12T02:00:00.0000000", "timeZone": "UTC" },
            "isAllDay": false,
            "location": { "displayName": "Studio" },
            "recurrence": {
                "pattern": {
                    "type": "weekly",
                    "interval": 1,
                    "daysOfWeek": ["thursday"],
                    "firstDayOfWeek": "sunday"
                },
                "range": {
                    "type": "endDate",
                    "startDate": "2026-11-12",
                    "endDate": "2026-12-31",
                    "recurrenceTimeZone": "UTC"
                }
            },
            "attendees": [
                { "emailAddress": { "address": "me@example.com", "name": "Me" }, "type": "required",
                  "status": { "response": "accepted" } },
                { "emailAddress": { "address": "other@example.com", "name": "Other" }, "type": "optional",
                  "status": { "response": "none" } }
            ],
            "organizer": { "emailAddress": { "address": "boss@example.com", "name": "Boss" } },
            "showAs": "busy",
            "sensitivity": "normal",
            "isReminderOn": true,
            "reminderMinutesBeforeStart": 10,
            "changeKey": "ck-1",
            "lastModifiedDateTime": "2026-11-01T08:00:00Z"
        })
    }

    #[test]
    fn maps_graph_json_to_core() {
        let ge: GraphEvent = serde_json::from_value(graph_event()).expect("fixture");
        let remote = map_event(&ge, "me@example.com", "cal-1", "acc-1", 30).expect("map");
        let event = &remote.event;
        assert_eq!(event.title, "Design review");
        assert_eq!(event.uid, "graph-uid@example.com");
        assert_eq!(event.start, "2026-11-12T01:00:00Z");
        assert_eq!(remote.remote_id.as_deref(), Some("AAMkAGI2"));
        assert_eq!(remote.etag.as_deref(), Some("ck-1"));
        let rule = event.recurrence.as_ref().expect("rule");
        assert_eq!(rule.freq, RecurrenceFreq::Weekly);
        assert_eq!(rule.by_weekday, vec![Weekday::Th]);
        assert_eq!(rule.until.as_deref(), Some("2026-12-31"));
        assert_eq!(
            remote.rrule.as_deref(),
            Some("FREQ=WEEKLY;BYDAY=TH;UNTIL=20261231")
        );
        assert_eq!(event.reminders, vec![10]);
        assert_eq!(event.attendees.len(), 2);
        assert_eq!(event.attendees[1].role, AttendeeRole::Optional);
        assert_eq!(event.my_response, Some(AttendeeStatus::Accepted));
    }

    #[test]
    fn maps_relative_monthly_recurrence() {
        let mut value = graph_event();
        value["recurrence"]["pattern"] = serde_json::json!({
            "type": "relativeMonthly",
            "interval": 2,
            "daysOfWeek": ["tuesday"],
            "index": "last"
        });
        value["recurrence"]["range"] = serde_json::json!({
            "type": "numbered",
            "startDate": "2026-11-12",
            "numberOfOccurrences": 6
        });
        let ge: GraphEvent = serde_json::from_value(value).expect("fixture");
        let remote = map_event(&ge, "me@example.com", "cal", "acc", 0).expect("map");
        let rule = remote.event.recurrence.expect("rule");
        assert_eq!(rule.freq, RecurrenceFreq::Monthly);
        assert_eq!(rule.interval, 2);
        assert_eq!(rule.by_set_pos, vec![-1]);
        assert_eq!(rule.by_weekday, vec![Weekday::Tu]);
        assert_eq!(rule.count, Some(6));
    }

    #[test]
    fn maps_recurring_event_in_series_time_zone() {
        let mut value = graph_event();
        // Graph is asked for UTC and returns UTC wall clock for a 09:00 New
        // York series.
        value["start"] = serde_json::json!({
            "dateTime": "2026-11-12T14:00:00.0000000",
            "timeZone": "UTC"
        });
        value["end"] = serde_json::json!({
            "dateTime": "2026-11-12T15:00:00.0000000",
            "timeZone": "UTC"
        });
        value["recurrence"]["range"]["recurrenceTimeZone"] = serde_json::json!("America/New_York");
        let ge: GraphEvent = serde_json::from_value(value).expect("fixture");
        let remote = map_event(&ge, "me@example.com", "cal", "acc", 0).expect("map");
        assert_eq!(remote.event.time_zone, "America/New_York");
        assert_eq!(remote.event.start, "2026-11-12T09:00:00-05:00");
        assert_eq!(remote.event.end, "2026-11-12T10:00:00-05:00");

        // The same series after the DST change still maps to 09:00 local.
        let mut summer = graph_event();
        summer["start"] = serde_json::json!({
            "dateTime": "2027-07-01T13:00:00.0000000",
            "timeZone": "UTC"
        });
        summer["end"] = serde_json::json!({
            "dateTime": "2027-07-01T14:00:00.0000000",
            "timeZone": "UTC"
        });
        summer["recurrence"]["range"]["recurrenceTimeZone"] = serde_json::json!("America/New_York");
        let ge: GraphEvent = serde_json::from_value(summer).expect("fixture");
        let remote = map_event(&ge, "me@example.com", "cal", "acc", 0).expect("map");
        assert_eq!(remote.event.start, "2027-07-01T09:00:00-04:00");
        assert_eq!(remote.event.end, "2027-07-01T10:00:00-04:00");

        let body = event_body(&remote.event, false);
        assert_eq!(body["start"]["timeZone"], "America/New_York");
        assert_eq!(body["start"]["dateTime"], "2027-07-01T09:00:00.0000000");
    }

    #[test]
    fn emits_graph_event_body() {
        let ge: GraphEvent = serde_json::from_value(graph_event()).expect("fixture");
        let remote = map_event(&ge, "me@example.com", "cal", "acc", 30).expect("map");
        let body = event_body(&remote.event, true);
        assert_eq!(body["subject"], "Design review");
        assert_eq!(body["start"]["timeZone"], "UTC");
        assert_eq!(body["start"]["dateTime"], "2026-11-12T01:00:00.0000000");
        assert_eq!(body["iCalUId"], "graph-uid@example.com");
        assert_eq!(body["recurrence"]["pattern"]["type"], "weekly");
        assert_eq!(body["recurrence"]["pattern"]["daysOfWeek"][0], "thursday");
        assert_eq!(body["recurrence"]["range"]["type"], "endDate");
    }

    #[test]
    fn maps_all_day_graph_event() {
        let mut value = graph_event();
        value["isAllDay"] = serde_json::json!(true);
        value["start"] =
            serde_json::json!({ "dateTime": "2026-12-01T00:00:00.0000000", "timeZone": "UTC" });
        value["end"] =
            serde_json::json!({ "dateTime": "2026-12-02T00:00:00.0000000", "timeZone": "UTC" });
        let ge: GraphEvent = serde_json::from_value(value).expect("fixture");
        let remote = map_event(&ge, "me@example.com", "cal", "acc", 0).expect("map");
        assert!(remote.event.all_day);
        assert_eq!(remote.event.start, "2026-12-01");
        assert_eq!(remote.event.end, "2026-12-02");
    }

    #[test]
    fn title_only_edit_keeps_stored_multi_ordinal_recurrence() {
        // `BYDAY=1MO,3WE` cannot be expressed by the structured model; the
        // stored RRULE text must survive a title-only edit untouched.
        let text = "FREQ=MONTHLY;BYDAY=1MO,3WE";
        let parsed = nuncio_ical::recurrence::from_rrule_string_detailed(text).expect("rule");
        assert!(parsed.lossy);
        let mut master = CalendarEvent::new(
            "evt-1",
            "cal-1",
            "acc-1",
            "uid-1",
            "Planning",
            "2026-11-02T09:00:00Z",
            "2026-11-02T10:00:00Z",
        );
        master.recurrence = Some(parsed.rule.clone());
        let mut current = StoredEvent::new(master.clone());
        current.rrule = Some(text.to_string());

        let mut edited = master;
        edited.title = "Planning (new room)".to_string();
        let body = series_update_body(&current, &edited);
        assert_eq!(body["subject"], "Planning (new room)");
        assert!(
            body.get("recurrence").is_none(),
            "unchanged recurrence must be omitted so Graph keeps its pattern: {body}"
        );

        // A genuinely changed (representable) rule is still written.
        let mut changed = edited;
        changed.recurrence = Some(RecurrenceRule {
            freq: RecurrenceFreq::Weekly,
            interval: 1,
            by_weekday: vec![Weekday::Tu],
            by_set_pos: Vec::new(),
            by_month_day: Vec::new(),
            by_month: Vec::new(),
            count: None,
            until: None,
        });
        let body = series_update_body(&current, &changed);
        assert_eq!(body["recurrence"]["pattern"]["type"], "weekly");
        assert_eq!(body["recurrence"]["pattern"]["daysOfWeek"][0], "tuesday");
    }

    #[test]
    fn lossy_changed_recurrence_is_not_written() {
        let mut master = CalendarEvent::new(
            "evt-1",
            "cal-1",
            "acc-1",
            "uid-1",
            "Planning",
            "2026-11-02T09:00:00Z",
            "2026-11-02T10:00:00Z",
        );
        master.recurrence = Some(RecurrenceRule {
            freq: RecurrenceFreq::Monthly,
            interval: 1,
            by_weekday: vec![Weekday::Mo],
            by_set_pos: vec![1],
            by_month_day: Vec::new(),
            by_month: Vec::new(),
            count: None,
            until: None,
        });
        let mut current = StoredEvent::new(master.clone());
        current.rrule = Some("FREQ=MONTHLY;BYDAY=1MO".to_string());

        let mut changed = master;
        changed.recurrence = Some(RecurrenceRule {
            freq: RecurrenceFreq::Monthly,
            interval: 1,
            by_weekday: vec![Weekday::Mo, Weekday::We],
            by_set_pos: vec![1, 3],
            by_month_day: Vec::new(),
            by_month: Vec::new(),
            count: None,
            until: None,
        });
        let body = series_update_body(&current, &changed);
        assert!(
            body.get("recurrence").is_none(),
            "a lossy rule must not be truncated to a single index: {body}"
        );
    }

    #[test]
    fn detects_removed_and_exception() {
        let mut value = graph_event();
        value["@removed"] = serde_json::json!({ "reason": "deleted" });
        value["seriesMasterId"] = serde_json::json!("master");
        value["originalStart"] = serde_json::json!("2026-11-12T01:00:00.0000000");
        value["type"] = serde_json::json!("exception");
        let ge: GraphEvent = serde_json::from_value(value).expect("fixture");
        assert!(is_removed(&ge));
        assert!(is_exception(&ge));
    }
}
