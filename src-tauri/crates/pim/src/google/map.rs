//! Google Calendar v3 ⇄ core model mapping (pure, unit-tested).

use chrono::{DateTime, Utc};
use nuncio_core::models::{
    new_id, Attendee, AttendeeRole, AttendeeStatus, CalendarEvent, EventStatus, RecurrenceRule,
    ShowAs, StoredEvent,
};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::provider::{RemoteCalendar, RemoteEvent};
use crate::util::{
    date_part, format_instant, ical_date_value, instant_in_zone, normalize_ical_date,
    normalize_zone, parse_instant, resolve_zone,
};

/* ─────────────────────────── API response types ─────────────────────────── */

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GCalendarListEntry {
    pub id: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub summary_override: Option<String>,
    #[serde(default)]
    pub background_color: Option<String>,
    #[serde(default)]
    pub foreground_color: Option<String>,
    #[serde(default)]
    pub access_role: Option<String>,
    #[serde(default)]
    pub primary: Option<bool>,
    #[serde(default)]
    pub selected: Option<bool>,
    #[serde(default)]
    pub deleted: Option<bool>,
    #[serde(default)]
    pub time_zone: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GCalendarList {
    #[serde(default)]
    pub items: Vec<GCalendarListEntry>,
    #[serde(default)]
    pub next_page_token: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GEventDateTime {
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub date_time: Option<String>,
    #[serde(default)]
    pub time_zone: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GPerson {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default, rename = "self")]
    pub is_self: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GAttendee {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default, rename = "self")]
    pub is_self: Option<bool>,
    #[serde(default)]
    pub organizer: Option<bool>,
    #[serde(default)]
    pub resource: Option<bool>,
    #[serde(default)]
    pub optional: Option<bool>,
    #[serde(default)]
    pub response_status: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GReminder {
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub minutes: Option<i32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GReminders {
    #[serde(default)]
    pub use_default: Option<bool>,
    #[serde(default)]
    pub overrides: Option<Vec<GReminder>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GEntryPoint {
    #[serde(default)]
    pub entry_point_type: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GConferenceData {
    #[serde(default)]
    pub entry_points: Option<Vec<GEntryPoint>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GEvent {
    pub id: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub start: Option<GEventDateTime>,
    #[serde(default)]
    pub end: Option<GEventDateTime>,
    #[serde(default)]
    pub recurrence: Option<Vec<String>>,
    #[serde(default)]
    pub recurring_event_id: Option<String>,
    #[serde(default)]
    pub original_start_time: Option<GEventDateTime>,
    #[serde(default)]
    pub transparency: Option<String>,
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default, rename = "iCalUID")]
    pub ical_uid: Option<String>,
    #[serde(default)]
    pub attendees: Option<Vec<GAttendee>>,
    #[serde(default)]
    pub organizer: Option<GPerson>,
    #[serde(default)]
    pub hangout_link: Option<String>,
    #[serde(default)]
    pub conference_data: Option<GConferenceData>,
    #[serde(default)]
    pub reminders: Option<GReminders>,
    #[serde(default)]
    pub updated: Option<String>,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub color_id: Option<String>,
    #[serde(default)]
    pub html_link: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GEventList {
    #[serde(default)]
    pub items: Vec<GEvent>,
    #[serde(default)]
    pub next_page_token: Option<String>,
    #[serde(default)]
    pub next_sync_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GErrorEnvelope {
    #[serde(default)]
    pub error: Option<serde_json::Value>,
}

/* ───────────────────────────── calendar mapping ──────────────────────────── */

pub fn map_calendar(entry: &GCalendarListEntry) -> RemoteCalendar {
    let name = entry
        .summary_override
        .clone()
        .or_else(|| entry.summary.clone())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| entry.id.clone());
    let color = entry
        .background_color
        .as_deref()
        .and_then(crate::util::normalize_color);
    let read_only = !matches!(entry.access_role.as_deref(), Some("owner") | Some("writer"));
    RemoteCalendar {
        local_id: String::new(),
        remote_id: entry.id.clone(),
        href: None,
        name,
        color,
        read_only,
        sync_token: None,
        ctag: None,
        supports_sync_collection: false,
    }
}

/* ─────────────────────────────── event mapping ───────────────────────────── */

fn status_of(value: Option<&str>) -> EventStatus {
    match value.unwrap_or("confirmed") {
        "cancelled" => EventStatus::Cancelled,
        "tentative" => EventStatus::Tentative,
        _ => EventStatus::Confirmed,
    }
}

fn attendee_status(value: Option<&str>) -> AttendeeStatus {
    match value.unwrap_or("needsAction") {
        "accepted" => AttendeeStatus::Accepted,
        "declined" => AttendeeStatus::Declined,
        "tentative" => AttendeeStatus::Tentative,
        "delegated" => AttendeeStatus::Delegated,
        _ => AttendeeStatus::NeedsAction,
    }
}

pub fn attendee_status_name(status: AttendeeStatus) -> &'static str {
    match status {
        AttendeeStatus::NeedsAction => "needsAction",
        AttendeeStatus::Accepted => "accepted",
        AttendeeStatus::Declined => "declined",
        AttendeeStatus::Tentative => "tentative",
        AttendeeStatus::Delegated => "delegated",
    }
}

/// Parse Google `recurrence` lines into the core rule + EXDATE list.
pub fn parse_recurrence_lines(
    lines: &[String],
    time_zone: &str,
    event_id: &str,
) -> (Option<RecurrenceRule>, Option<String>, Vec<String>) {
    let mut rule = None;
    let mut raw_rule = None;
    let mut exdates = Vec::new();
    for line in lines {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("RRULE:") {
            raw_rule = Some(value.to_string());
            rule = nuncio_ical::recurrence::from_rrule_string(value);
        } else if let Some(rest) = line.strip_prefix("EXDATE") {
            if let Some((_, values)) = rest.split_once(':') {
                for value in values.split(',') {
                    if let Some(normalized) = normalize_ical_date(value, time_zone) {
                        exdates.push(normalized);
                    }
                }
            }
        } else if line.starts_with("RDATE") {
            // RDATE is not represented in the core model; providers normally
            // emit EXDATE/RRULE only.  Keep the event importable.
            tracing::debug!(event_id, "ignoring RDATE from Google recurrence");
        }
    }
    (rule, raw_rule, exdates)
}

/// Convert a Google event into the provider-neutral remote event.
///
/// `default_reminder_minutes` is used when the event inherits the calendar
/// default (`reminders.useDefault`).
pub fn map_event(
    ge: &GEvent,
    account_email: &str,
    calendar_id: &str,
    account_id: &str,
    default_reminder_minutes: i32,
) -> Result<RemoteEvent> {
    let uid = ge
        .ical_uid
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| ge.id.clone());

    let start_part = ge.start.clone().unwrap_or_default();
    let end_part = ge.end.clone().unwrap_or_default();
    let all_day = start_part.date.is_some();
    let zone = normalize_zone(
        start_part
            .time_zone
            .as_deref()
            .or(end_part.time_zone.as_deref())
            .unwrap_or("UTC"),
    );

    let (start, end) = if all_day {
        let start = start_part
            .date
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "1970-01-01".to_string());
        let end = end_part
            .date
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| start.clone());
        (start, end)
    } else {
        let start =
            google_datetime_to_core(start_part.date_time.as_deref().unwrap_or_default(), &zone);
        let end = google_datetime_to_core(end_part.date_time.as_deref().unwrap_or_default(), &zone);
        (start, end)
    };

    let recurrence_lines = ge.recurrence.clone().unwrap_or_default();
    let (recurrence, rrule, exdates) = parse_recurrence_lines(&recurrence_lines, &zone, &ge.id);

    let recurrence_id = ge.original_start_time.as_ref().and_then(|value| {
        if let Some(date) = value.date.as_deref() {
            date_part(date)
        } else {
            value
                .date_time
                .as_deref()
                .map(|date_time| google_datetime_to_core(date_time, &zone))
        }
    });

    let organizer = ge.organizer.as_ref().map(|person| Attendee {
        email: person.email.clone().unwrap_or_default(),
        name: person.display_name.clone(),
        role: AttendeeRole::Chair,
        status: AttendeeStatus::Accepted,
    });

    let attendees: Vec<Attendee> = ge
        .attendees
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|attendee| {
            let email = attendee.email?;
            let role = if attendee.resource == Some(true) {
                AttendeeRole::Resource
            } else if attendee.optional == Some(true) {
                AttendeeRole::Optional
            } else if attendee.organizer == Some(true) {
                AttendeeRole::Chair
            } else {
                AttendeeRole::Required
            };
            Some(Attendee {
                email,
                name: attendee.display_name,
                role,
                status: attendee_status(attendee.response_status.as_deref()),
            })
        })
        .collect();

    let my_response = ge
        .attendees
        .as_ref()
        .and_then(|list| {
            list.iter().find(|attendee| {
                attendee.is_self == Some(true)
                    || attendee
                        .email
                        .as_deref()
                        .is_some_and(|email| email.eq_ignore_ascii_case(account_email))
            })
        })
        .map(|attendee| attendee_status(attendee.response_status.as_deref()));

    let is_organizer = ge.organizer.as_ref().is_some_and(|person| {
        person.is_self == Some(true)
            || person
                .email
                .as_deref()
                .is_some_and(|email| email.eq_ignore_ascii_case(account_email))
    });

    let reminders = map_reminders(ge.reminders.as_ref(), default_reminder_minutes);

    let show_as = match ge.transparency.as_deref() {
        Some("transparent") => ShowAs::Free,
        Some("opaque") => {
            if ge.status.as_deref() == Some("tentative") {
                ShowAs::Tentative
            } else {
                ShowAs::Busy
            }
        }
        _ => ShowAs::Busy,
    };

    let is_private = matches!(
        ge.visibility.as_deref(),
        Some("private") | Some("confidential")
    );

    let online_meeting_url = ge
        .hangout_link
        .clone()
        .or_else(|| {
            ge.conference_data.as_ref().and_then(|conference| {
                conference
                    .entry_points
                    .as_ref()
                    .and_then(|points| {
                        points
                            .iter()
                            .find(|point| {
                                matches!(
                                    point.entry_point_type.as_deref(),
                                    Some("video") | Some("more")
                                )
                            })
                            .or_else(|| points.first())
                    })
                    .and_then(|point| point.uri.clone())
            })
        })
        .filter(|value| !value.trim().is_empty());

    let updated_at = ge
        .updated
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc));

    let event = CalendarEvent {
        id: new_id(),
        calendar_id: calendar_id.to_string(),
        account_id: account_id.to_string(),
        uid,
        title: ge.summary.clone().unwrap_or_default(),
        location: ge.location.clone().unwrap_or_default(),
        description: ge.description.clone().unwrap_or_default(),
        start,
        end,
        all_day,
        time_zone: zone,
        recurrence,
        series_id: None,
        recurrence_id,
        status: status_of(ge.status.as_deref()),
        show_as,
        is_private,
        organizer,
        attendees,
        reminders,
        is_organizer,
        my_response,
        online_meeting_url,
        color: color_from_id(ge.color_id.as_deref()),
        updated_at,
    };

    Ok(RemoteEvent {
        event,
        remote_id: Some(ge.id.clone()),
        href: None,
        etag: ge.etag.clone(),
        rrule,
        exdates,
        raw_ics: None,
        series_remote_id: ge.recurring_event_id.clone(),
    })
}

fn google_datetime_to_core(value: &str, zone: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return format_instant(Utc::now());
    }
    match parse_instant(value) {
        Ok(instant) => instant_in_zone(instant, zone),
        Err(_) => value.to_string(),
    }
}

fn map_reminders(reminders: Option<&GReminders>, default_minutes: i32) -> Vec<i32> {
    let Some(reminders) = reminders else {
        return if default_minutes > 0 {
            vec![default_minutes]
        } else {
            Vec::new()
        };
    };
    let overrides = reminders.overrides.clone().unwrap_or_default();
    let minutes: Vec<i32> = overrides
        .iter()
        .filter(|reminder| reminder.method.as_deref() != Some("email"))
        .filter_map(|reminder| reminder.minutes)
        .filter(|minutes| *minutes >= 0)
        .collect();
    if minutes.is_empty() && reminders.use_default != Some(false) && default_minutes > 0 {
        return vec![default_minutes];
    }
    minutes
}

/* ─────────────────────────────── outbound JSON ───────────────────────────── */

/// Build the JSON body for `events.insert` / `events.patch`.
pub fn event_body(
    event: &CalendarEvent,
    exdates: &[String],
    original_rrule: Option<&str>,
    for_create: bool,
) -> serde_json::Value {
    let start = if event.all_day {
        serde_json::json!({ "date": date_part(&event.start).unwrap_or_else(|| event.start.clone()) })
    } else {
        serde_json::json!({
            "dateTime": event.start,
            "timeZone": normalize_zone(&event.time_zone),
        })
    };
    let end = if event.all_day {
        serde_json::json!({ "date": date_part(&event.end).unwrap_or_else(|| event.end.clone()) })
    } else {
        serde_json::json!({
            "dateTime": event.end,
            "timeZone": normalize_zone(&event.time_zone),
        })
    };

    let mut body = serde_json::json!({
        "summary": event.title,
        "location": event.location,
        "description": event.description,
        "start": start,
        "end": end,
        "transparency": if event.show_as == ShowAs::Free { "transparent" } else { "opaque" },
        "visibility": if event.is_private { "private" } else { "default" },
        "status": match event.status {
            EventStatus::Cancelled => "cancelled",
            EventStatus::Tentative => "tentative",
            EventStatus::Confirmed => "confirmed",
        },
        "attendees": event.attendees.iter().map(|attendee| {
            serde_json::json!({
                "email": attendee.email,
                "displayName": attendee.name,
                "optional": attendee.role == AttendeeRole::Optional,
                "resource": attendee.role == AttendeeRole::Resource,
                "responseStatus": attendee_status_name(attendee.status),
            })
        }).collect::<Vec<_>>(),
        "reminders": if event.reminders.is_empty() {
            serde_json::json!({ "useDefault": true })
        } else {
            serde_json::json!({
                "useDefault": false,
                "overrides": event.reminders.iter().map(|minutes| {
                    serde_json::json!({ "method": "popup", "minutes": minutes })
                }).collect::<Vec<_>>(),
            })
        },
    });

    if let Some(rule) = &event.recurrence {
        let rrule = original_rrule
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| {
                value
                    .strip_prefix("RRULE:")
                    .or_else(|| value.strip_prefix("rrule:"))
                    .unwrap_or(value)
                    .to_string()
            })
            .unwrap_or_else(|| nuncio_ical::recurrence::to_rrule_string(rule));
        let mut lines = vec![format!("RRULE:{rrule}")];
        if !exdates.is_empty() {
            let mut params = String::new();
            let zone = normalize_zone(&event.time_zone);
            if zone != "UTC" && resolve_zone(&zone).is_some() {
                params = format!(";TZID={zone}");
            }
            let values = exdates
                .iter()
                .map(|value| ical_date_value(value, &zone).1)
                .collect::<Vec<_>>()
                .join(",");
            lines.push(format!("EXDATE{params}:{values}"));
        }
        body["recurrence"] = serde_json::json!(lines);
    }

    if let Some(color_id) = color_id_from_hex(event.color.as_deref()) {
        body["colorId"] = serde_json::json!(color_id);
    }
    if for_create {
        body["iCalUID"] = serde_json::json!(event.uid);
    }
    body
}

/// Accurate JSON for a Google recurrence-only patch.
pub fn recurrence_body(event: &CalendarEvent) -> Option<serde_json::Value> {
    event.recurrence.as_ref().map(|rule| {
        serde_json::json!({
            "recurrence": [format!("RRULE:{}", nuncio_ical::recurrence::to_rrule_string(rule))]
        })
    })
}

/* ───────────────────────────── color palette ─────────────────────────────── */

fn color_from_id(id: Option<&str>) -> Option<String> {
    let hex = match id? {
        "1" => "#7986CB",
        "2" => "#33B679",
        "3" => "#8E24AA",
        "4" => "#E67C73",
        "5" => "#F6BF26",
        "6" => "#F4511E",
        "7" => "#039BE5",
        "8" => "#616161",
        "9" => "#3F51B5",
        "10" => "#0B8043",
        "11" => "#D50000",
        _ => return None,
    };
    Some(hex.to_string())
}

fn color_id_from_hex(color: Option<&str>) -> Option<&'static str> {
    let color = color?.trim().to_ascii_uppercase();
    match color.as_str() {
        "#7986CB" => Some("1"),
        "#33B679" => Some("2"),
        "#8E24AA" => Some("3"),
        "#E67C73" => Some("4"),
        "#F6BF26" => Some("5"),
        "#F4511E" => Some("6"),
        "#039BE5" => Some("7"),
        "#616161" => Some("8"),
        "#3F51B5" => Some("9"),
        "#0B8043" => Some("10"),
        "#D50000" => Some("11"),
        _ => None,
    }
}

/// Serialisable view used by tests and diagnostics.
#[derive(Debug, Clone, Serialize)]
pub struct MappedEventSummary {
    pub title: String,
    pub uid: String,
    pub start: String,
    pub end: String,
}

impl From<&CalendarEvent> for MappedEventSummary {
    fn from(event: &CalendarEvent) -> Self {
        Self {
            title: event.title.clone(),
            uid: event.uid.clone(),
            start: event.start.clone(),
            end: event.end.clone(),
        }
    }
}

/// Helper used by the sync layer to decide whether a Google item is a deleted
/// master/single event versus a cancelled override.
pub fn is_cancelled_master(ge: &GEvent) -> bool {
    ge.status.as_deref() == Some("cancelled")
        && (ge.recurring_event_id.is_none() || ge.original_start_time.is_none())
}

pub fn is_cancelled_override(ge: &GEvent) -> bool {
    ge.status.as_deref() == Some("cancelled")
        && ge.recurring_event_id.is_some()
        && ge.original_start_time.is_some()
}

/// Conversion helper used by tests.
pub fn stored_for(
    event: CalendarEvent,
    rrule: Option<String>,
    exdates: Vec<String>,
) -> StoredEvent {
    StoredEvent {
        event,
        raw_ics: None,
        etag: None,
        remote_id: None,
        href: None,
        rrule,
        exdates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::models::{RecurrenceFreq, Weekday};

    fn google_event() -> serde_json::Value {
        serde_json::json!({
            "id": "abc123",
            "iCalUID": "uid@example.com",
            "status": "confirmed",
            "summary": "Team sync",
            "description": "Weekly",
            "location": "Room 1",
            "start": { "dateTime": "2026-11-12T09:00:00+08:00", "timeZone": "Asia/Shanghai" },
            "end": { "dateTime": "2026-11-12T10:00:00+08:00", "timeZone": "Asia/Shanghai" },
            "recurrence": ["RRULE:FREQ=WEEKLY;BYDAY=TH;COUNT=10", "EXDATE;TZID=Asia/Shanghai:20261119T090000"],
            "attendees": [
                { "email": "me@example.com", "self": true, "responseStatus": "accepted" },
                { "email": "other@example.com", "responseStatus": "needsAction" }
            ],
            "organizer": { "email": "boss@example.com" },
            "reminders": { "useDefault": false, "overrides": [{ "method": "popup", "minutes": 15 }] },
            "transparency": "opaque",
            "visibility": "default",
            "etag": "\"etag-1\"",
            "updated": "2026-11-01T08:00:00Z"
        })
    }

    #[test]
    fn maps_google_json_to_core() {
        let ge: GEvent = serde_json::from_value(google_event()).expect("fixture");
        let remote = map_event(&ge, "me@example.com", "cal-1", "acc-1", 30).expect("map");
        let event = &remote.event;
        assert_eq!(event.title, "Team sync");
        assert_eq!(event.uid, "uid@example.com");
        assert_eq!(event.time_zone, "Asia/Shanghai");
        assert!(event.start.ends_with("+08:00"));
        assert_eq!(remote.remote_id.as_deref(), Some("abc123"));
        assert_eq!(remote.etag.as_deref(), Some("\"etag-1\""));
        let rule = event.recurrence.as_ref().expect("rule");
        assert_eq!(rule.freq, RecurrenceFreq::Weekly);
        assert_eq!(rule.by_weekday, vec![Weekday::Th]);
        assert_eq!(rule.count, Some(10));
        assert_eq!(remote.exdates.len(), 1);
        assert_eq!(event.reminders, vec![15]);
        assert_eq!(event.my_response, Some(AttendeeStatus::Accepted));
        assert_eq!(event.attendees.len(), 2);
        assert!(!event.is_organizer);
    }

    #[test]
    fn maps_all_day_google_event() {
        let mut geo = google_event();
        geo["start"] = serde_json::json!({ "date": "2026-12-01" });
        geo["end"] = serde_json::json!({ "date": "2026-12-02" });
        geo["recurrence"] = serde_json::json!([]);
        let ge: GEvent = serde_json::from_value(geo).expect("fixture");
        let remote = map_event(&ge, "me@example.com", "cal-1", "acc-1", 0).expect("map");
        assert!(remote.event.all_day);
        assert_eq!(remote.event.start, "2026-12-01");
        assert_eq!(remote.event.end, "2026-12-02");
    }

    #[test]
    fn emits_google_event_body() {
        let ge: GEvent = serde_json::from_value(google_event()).expect("fixture");
        let remote = map_event(&ge, "me@example.com", "cal-1", "acc-1", 30).expect("map");
        let body = event_body(&remote.event, &remote.exdates, None, true);
        assert_eq!(body["summary"], "Team sync");
        assert_eq!(body["start"]["timeZone"], "Asia/Shanghai");
        assert_eq!(body["iCalUID"], "uid@example.com");
        let recurrence = body["recurrence"].as_array().expect("recurrence");
        assert!(recurrence[0]
            .as_str()
            .unwrap_or_default()
            .starts_with("RRULE:FREQ=WEEKLY"));
        assert!(recurrence[1]
            .as_str()
            .unwrap_or_default()
            .starts_with("EXDATE"));
        assert_eq!(body["reminders"]["overrides"][0]["minutes"], 15);
    }

    #[test]
    fn detects_cancellations() {
        let mut value = google_event();
        value["status"] = serde_json::json!("cancelled");
        let ge: GEvent = serde_json::from_value(value.clone()).expect("fixture");
        assert!(is_cancelled_master(&ge));
        value["recurringEventId"] = serde_json::json!("master");
        value["originalStartTime"] = serde_json::json!({ "dateTime": "2026-11-12T09:00:00+08:00" });
        let ge: GEvent = serde_json::from_value(value).expect("fixture");
        assert!(is_cancelled_override(&ge));
    }
}
