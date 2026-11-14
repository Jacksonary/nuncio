//! VCALENDAR / VEVENT / VTODO parsing.

use std::collections::HashMap;

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use nuncio_core::models::{
    new_id, Attendee, AttendeeRole, AttendeeStatus, EventStatus, Id, Importance, ShowAs,
    TaskStatus, TaskStep,
};
use nuncio_core::{AppError, Result};

use crate::ics::lines::{Component, ContentLine};
use crate::ics::types::{ParsedCalendar, ParsedEvent, ParsedTodo};
use crate::recurrence::{from_rrule_string, from_rrule_string_with_zone};
use crate::tz::{self, VTimezone};
use crate::value::{
    format_date, format_in_zone, parse_date_value, parse_duration, unescape_text, TzSpec,
};

/// Everything parsing needs to resolve TZIDs.
struct TzContext {
    zones: tz::TimezoneMap,
    default_tz: Option<String>,
}

/// Result of resolving a TZID.
struct Resolved {
    spec: TzSpec,
    /// IANA name when the event should keep a named zone.
    iana: Option<String>,
}

impl TzContext {
    fn new(zones: tz::TimezoneMap, default_tz: Option<String>) -> Self {
        Self { zones, default_tz }
    }

    fn resolve(&self, tzid: Option<&str>, date: NaiveDate) -> Resolved {
        let tzid = tzid
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or(self.default_tz.as_deref());
        let Some(tzid) = tzid else {
            return Resolved {
                spec: TzSpec::utc(),
                iana: Some("UTC".into()),
            };
        };
        if tzid.eq_ignore_ascii_case("UTC")
            || tzid.eq_ignore_ascii_case("GMT")
            || tzid.eq_ignore_ascii_case("Z")
        {
            return Resolved {
                spec: TzSpec::Named(chrono_tz::Tz::UTC),
                iana: Some("UTC".into()),
            };
        }
        if let Some(tz) = tz::resolve_iana(tzid) {
            return Resolved {
                spec: TzSpec::Named(tz),
                iana: Some(tz.name().to_string()),
            };
        }
        if let Some(vtz) = tz::lookup(&self.zones, tzid) {
            if let Some(offset) = vtz
                .offset_at(date)
                .or(vtz.standard_offset)
                .or(vtz.daylight_offset)
            {
                return Resolved {
                    spec: TzSpec::Fixed(offset),
                    iana: None,
                };
            }
        }
        tracing::debug!(tzid, "unknown TZID; falling back to UTC");
        Resolved {
            spec: TzSpec::utc(),
            iana: Some("UTC".into()),
        }
    }
}

/// Parse an iCalendar document.
pub fn parse_calendar(text: &str) -> Result<ParsedCalendar> {
    let components = crate::ics::lines::parse_components(text);
    if components.is_empty() {
        return Err(AppError::invalid_input("no iCalendar data found"));
    }

    let mut calendar = ParsedCalendar::default();
    let mut recognised = false;

    for component in &components {
        match component.name.as_str() {
            "VCALENDAR" => {
                recognised = true;
                let method = component
                    .value("METHOD")
                    .map(|value| value.trim().to_ascii_uppercase())
                    .filter(|value| !value.is_empty());
                if calendar.method.is_none() {
                    calendar.method = method;
                }

                let mut zones: HashMap<String, VTimezone> = HashMap::new();
                for child in &component.children {
                    if child.name == "VTIMEZONE" {
                        if let Some(vtz) = parse_vtimezone(child) {
                            zones.insert(vtz.tzid.clone(), vtz);
                        }
                    }
                }
                let default_tz = component
                    .value("X-WR-TIMEZONE")
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty());
                let ctx = TzContext::new(zones, default_tz);

                for child in &component.children {
                    match child.name.as_str() {
                        "VEVENT" => {
                            if let Some(event) = parse_event(child, &ctx) {
                                calendar.events.push(event);
                            }
                        }
                        "VTODO" => {
                            if let Some(todo) = parse_todo(child, &ctx) {
                                calendar.todos.push(todo);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "VEVENT" | "VTODO" => {
                recognised = true;
                let ctx = TzContext::new(HashMap::new(), None);
                if component.name == "VEVENT" {
                    if let Some(event) = parse_event(component, &ctx) {
                        calendar.events.push(event);
                    }
                } else if let Some(todo) = parse_todo(component, &ctx) {
                    calendar.todos.push(todo);
                }
            }
            _ => {}
        }
    }

    if !recognised {
        return Err(AppError::invalid_input(
            "no VCALENDAR, VEVENT or VTODO component found",
        ));
    }
    Ok(calendar)
}

/// Parse only VTODO components (convenience for CalDAV task collections).
pub fn parse_todos(text: &str) -> Result<Vec<ParsedTodo>> {
    Ok(parse_calendar(text)?.todos)
}

// ─────────────────────────────── VEVENT ────────────────────────────────

fn parse_event(component: &Component, ctx: &TzContext) -> Option<ParsedEvent> {
    let start_line = component.prop("DTSTART")?;
    let parsed_start = parse_date_value(&start_line.value)?;
    let all_day = is_all_day(start_line, parsed_start.time.is_none());
    let resolved = ctx.resolve(start_line.param("TZID"), parsed_start.date);
    let start_tz = event_tz(all_day, &resolved);

    let start_local = parsed_start.resolve(resolved.spec);
    let start_utc = start_local.with_timezone(&Utc);
    let start = if all_day {
        format_date(parsed_start.date)
    } else {
        format_in_zone(start_utc, start_tz)
    };

    // End: DTEND, else DURATION, else a sensible default.
    let (end, end_utc) = if let Some(line) = component.prop("DTEND") {
        if let Some(parsed_end) = parse_date_value(&line.value) {
            let end_spec = if line.param("TZID").is_some() {
                ctx.resolve(line.param("TZID"), parsed_end.date).spec
            } else {
                resolved.spec
            };
            let local = parsed_end.resolve(end_spec);
            let utc = local.with_timezone(&Utc);
            let text = if all_day {
                format_date(parsed_end.date)
            } else {
                format_in_zone(utc, start_tz)
            };
            (text, utc)
        } else {
            default_end(parsed_start, start_utc, start_tz, all_day)
        }
    } else if let Some(line) = component.prop("DURATION") {
        match parse_duration(&line.value) {
            Some(duration) if all_day => {
                let days = duration.num_days();
                let date = parsed_start.date + chrono::Duration::days(days);
                (format_date(date), start_utc + duration)
            }
            Some(duration) => {
                let utc = start_utc + duration;
                (format_in_zone(utc, start_tz), utc)
            }
            None => default_end(parsed_start, start_utc, start_tz, all_day),
        }
    } else {
        default_end(parsed_start, start_utc, start_tz, all_day)
    };
    let end = if all_day && end == start {
        format_date(parsed_start.date.succ_opt().unwrap_or(parsed_start.date))
    } else {
        end
    };

    let uid = component
        .value("UID")
        .map(|value| unescape_text(value).trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(new_id);

    let title = component
        .value("SUMMARY")
        .map(unescape_text)
        .unwrap_or_default();
    let location = component
        .value("LOCATION")
        .map(unescape_text)
        .unwrap_or_default();
    let description = component
        .value("DESCRIPTION")
        .map(unescape_text)
        .unwrap_or_default();

    let rrule_raw = component
        .value("RRULE")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let recurrence = rrule_raw.as_deref().and_then(|raw| {
        from_rrule_string_with_zone(raw, if all_day { None } else { Some(start_tz) })
    });

    let recurrence_id = component
        .prop("RECURRENCE-ID")
        .and_then(|line| parse_date_value(&line.value).map(|parsed| (line, parsed)))
        .map(|(line, parsed)| {
            let resolved = ctx.resolve(line.param("TZID"), parsed.date);
            let tz = event_tz(all_day, &resolved);
            if all_day {
                format_date(parsed.date)
            } else {
                format_in_zone(parsed.resolve(resolved.spec).with_timezone(&Utc), tz)
            }
        });

    let exdates = collect_date_list(component, "EXDATE", ctx, all_day, start_tz);
    let rdates = collect_date_list(component, "RDATE", ctx, all_day, start_tz);

    let mut status = component
        .value("STATUS")
        .map(parse_event_status)
        .unwrap_or(EventStatus::Confirmed);

    let busy = component
        .value("X-MICROSOFT-CDO-BUSYSTATUS")
        .map(|value| value.trim().to_ascii_uppercase());
    let transp = component
        .value("TRANSP")
        .map(|value| value.trim().to_ascii_uppercase());
    let show_as = match busy.as_deref() {
        Some("FREE") => ShowAs::Free,
        Some("TENTATIVE") => ShowAs::Tentative,
        Some("BUSY") => ShowAs::Busy,
        Some("OOF") => ShowAs::Oof,
        Some("WORKINGELSEWHERE") => ShowAs::WorkingElsewhere,
        _ => match transp.as_deref() {
            Some("TRANSPARENT") => ShowAs::Free,
            _ if status == EventStatus::Tentative => ShowAs::Tentative,
            _ => ShowAs::Busy,
        },
    };
    if component
        .value("X-MICROSOFT-CDO-INTENDEDSTATUS")
        .map(|value| value.trim().eq_ignore_ascii_case("TENTATIVE"))
        .unwrap_or(false)
        && show_as == ShowAs::Busy
    {
        status = EventStatus::Tentative;
    }

    let class = component
        .value("CLASS")
        .map(|value| value.trim().to_ascii_uppercase());
    let is_private = matches!(class.as_deref(), Some("PRIVATE") | Some("CONFIDENTIAL"));

    let organizer = component.prop("ORGANIZER").and_then(parse_organizer);
    let attendees = component
        .props_named("ATTENDEE")
        .filter_map(parse_attendee)
        .collect::<Vec<_>>();

    let raw_alarms = parse_alarms(component);
    let mut reminders = resolve_reminders(&raw_alarms, start_utc, end_utc);

    let online_meeting_url = online_meeting_url(component, &description, &location);
    let sequence = component
        .value("SEQUENCE")
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let updated_at = component
        .value("LAST-MODIFIED")
        .and_then(parse_compact_utc)
        .or_else(|| component.value("DTSTAMP").and_then(parse_compact_utc))
        .or_else(|| component.value("CREATED").and_then(parse_compact_utc));

    reminders.sort_unstable();
    reminders.dedup();
    reminders.retain(|minutes| *minutes >= 0);

    Some(ParsedEvent {
        uid,
        title,
        location,
        description,
        start,
        end,
        all_day,
        time_zone: start_tz.name(),
        recurrence,
        recurrence_id,
        status,
        show_as,
        is_private,
        organizer,
        attendees,
        reminders,
        online_meeting_url,
        sequence,
        updated_at,
        rrule: rrule_raw,
        exdates,
        rdates,
    })
}

fn is_all_day(line: &ContentLine, no_time: bool) -> bool {
    no_time
        || line
            .param("VALUE")
            .map(|value| value.eq_ignore_ascii_case("DATE"))
            .unwrap_or(false)
}

fn event_tz(all_day: bool, resolved: &Resolved) -> TzSpec {
    if all_day {
        TzSpec::utc()
    } else {
        match &resolved.iana {
            Some(name) => tz::resolve_iana(name)
                .map(TzSpec::Named)
                .unwrap_or_else(TzSpec::utc),
            None => TzSpec::utc(),
        }
    }
}

fn default_end(
    parsed_start: crate::value::ParsedDate,
    start_utc: DateTime<Utc>,
    start_tz: TzSpec,
    all_day: bool,
) -> (String, DateTime<Utc>) {
    if all_day {
        (
            format_date(parsed_start.date.succ_opt().unwrap_or(parsed_start.date)),
            start_utc + chrono::Duration::days(1),
        )
    } else {
        (format_in_zone(start_utc, start_tz), start_utc)
    }
}

fn parse_event_status(value: &str) -> EventStatus {
    match value.trim().to_ascii_uppercase().as_str() {
        "TENTATIVE" => EventStatus::Tentative,
        "CANCELLED" | "CANCELED" => EventStatus::Cancelled,
        _ => EventStatus::Confirmed,
    }
}

fn collect_date_list(
    component: &Component,
    property: &str,
    ctx: &TzContext,
    all_day: bool,
    event_tz: TzSpec,
) -> Vec<String> {
    let mut out = Vec::new();
    for line in component.props_named(property) {
        let tzid = line.param("TZID");
        let is_date = line
            .param("VALUE")
            .map(|value| value.eq_ignore_ascii_case("DATE"))
            .unwrap_or(false);
        for raw in line.value.split(',') {
            let Some(parsed) = parse_date_value(raw.trim()) else {
                continue;
            };
            let resolved = ctx.resolve(tzid, parsed.date);
            let value = if all_day || is_date || parsed.time.is_none() {
                format_date(parsed.date)
            } else {
                format_in_zone(parsed.resolve(resolved.spec).with_timezone(&Utc), event_tz)
            };
            if !out.contains(&value) {
                out.push(value);
            }
        }
    }
    out
}

fn parse_compact_utc(value: &str) -> Option<DateTime<Utc>> {
    let parsed = parse_date_value(value)?;
    Some(parsed.resolve(TzSpec::utc()).with_timezone(&Utc))
}

fn parse_organizer(line: &ContentLine) -> Option<Attendee> {
    let email = normalize_email(&line.value)?;
    let name = line
        .param("CN")
        .map(decode_param_text)
        .filter(|n| !n.trim().is_empty());
    Some(Attendee {
        email,
        name,
        role: AttendeeRole::Chair,
        status: AttendeeStatus::NeedsAction,
    })
}

fn parse_attendee(line: &ContentLine) -> Option<Attendee> {
    let email = normalize_email(&line.value)?;
    let name = line
        .param("CN")
        .map(decode_param_text)
        .filter(|n| !n.trim().is_empty());
    let role = match line
        .param("ROLE")
        .unwrap_or("REQ-PARTICIPANT")
        .trim()
        .to_ascii_uppercase()
        .as_str()
    {
        "OPT-PARTICIPANT" | "NON-PARTICIPANT" => AttendeeRole::Optional,
        "RESOURCE" | "ROOM" => AttendeeRole::Resource,
        "CHAIR" => AttendeeRole::Chair,
        _ => AttendeeRole::Required,
    };
    let status = match line
        .param("PARTSTAT")
        .unwrap_or("NEEDS-ACTION")
        .trim()
        .to_ascii_uppercase()
        .as_str()
    {
        "ACCEPTED" => AttendeeStatus::Accepted,
        "DECLINED" => AttendeeStatus::Declined,
        "TENTATIVE" => AttendeeStatus::Tentative,
        "DELEGATED" => AttendeeStatus::Delegated,
        _ => AttendeeStatus::NeedsAction,
    };
    Some(Attendee {
        email,
        name,
        role,
        status,
    })
}

fn decode_param_text(value: &str) -> String {
    crate::value::decode_caret(value)
}

fn normalize_email(value: &str) -> Option<String> {
    let email = crate::value::normalize_mailto(value);
    if email.is_empty() || !email.contains('@') {
        None
    } else {
        Some(email)
    }
}

fn online_meeting_url(component: &Component, description: &str, location: &str) -> Option<String> {
    for property in [
        "URL",
        "X-GOOGLE-CONFERENCE",
        "X-MICROSOFT-SKYPETEAMSMEETINGURL",
        "X-MICROSOFT-ONLINEMEETINGCONFLINK",
    ] {
        if let Some(value) = component.value(property) {
            let value = unescape_text(value.trim());
            if value.is_empty() {
                continue;
            }
            if value.starts_with("http://") || value.starts_with("https://") {
                return Some(value);
            }
            if property == "X-GOOGLE-CONFERENCE" {
                return Some(format!("https://meet.google.com/{value}"));
            }
        }
    }
    find_meeting_url(&format!("{location}\n{description}"))
}

fn find_meeting_url(text: &str) -> Option<String> {
    for token in text.split_whitespace() {
        let candidate = token.trim_matches(|ch: char| "()[]<>,.;'\"".contains(ch));
        if !(candidate.starts_with("http://") || candidate.starts_with("https://")) {
            continue;
        }
        let lower = candidate.to_ascii_lowercase();
        for host in [
            "teams.microsoft.com",
            "teams.live.com",
            "zoom.us",
            "meet.google.com",
            "webex.com",
            "whereby.com",
            "skype.com",
            "gotomeeting.com",
            "bluejeans.com",
        ] {
            if lower.contains(host) {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

// ─────────────────────────────── VALARM ────────────────────────────────

#[derive(Debug, Clone)]
struct RawAlarm {
    trigger: Option<String>,
    related_end: bool,
    absolute: Option<String>,
}

fn parse_alarms(component: &Component) -> Vec<RawAlarm> {
    let mut out = Vec::new();
    for child in &component.children {
        if child.name != "VALARM" {
            continue;
        }
        let action = child
            .value("ACTION")
            .map(|value| value.trim().to_ascii_uppercase())
            .unwrap_or_else(|| "DISPLAY".into());
        if action != "DISPLAY" && action != "EMAIL" && action != "AUDIO" {
            continue;
        }
        let Some(trigger) = child.prop("TRIGGER") else {
            continue;
        };
        let value_type = trigger
            .param("VALUE")
            .map(|value| value.trim().to_ascii_uppercase());
        let trimmed = trigger.value.trim();
        let looks_like_duration = trimmed
            .trim_start_matches(['+', '-'])
            .starts_with(['P', 'p']);
        let absolute = if value_type.as_deref() == Some("DATE-TIME")
            || (!looks_like_duration && trimmed.contains('T'))
        {
            Some(trigger.value.clone())
        } else {
            None
        };
        let related_end = trigger
            .param("RELATED")
            .map(|value| value.eq_ignore_ascii_case("END"))
            .unwrap_or(false);
        out.push(RawAlarm {
            trigger: if absolute.is_none() {
                Some(trigger.value.clone())
            } else {
                None
            },
            related_end,
            absolute,
        });
    }
    out
}

fn resolve_reminders(alarms: &[RawAlarm], start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<i32> {
    let mut out = Vec::new();
    for alarm in alarms {
        if let Some(raw) = &alarm.absolute {
            if let Some(parsed) = parse_date_value(raw) {
                let instant = parsed.resolve(TzSpec::utc()).with_timezone(&Utc);
                let minutes = (start - instant).num_minutes();
                if minutes >= 0 && minutes <= i32::MAX as i64 {
                    out.push(minutes as i32);
                }
            }
            continue;
        }
        let Some(raw) = &alarm.trigger else { continue };
        let Some(duration) = parse_duration(raw) else {
            continue;
        };
        let minutes_before = if alarm.related_end {
            duration.num_minutes() + (end - start).num_minutes()
        } else {
            -duration.num_minutes()
        };
        let minutes_before = if minutes_before < 0 {
            0
        } else {
            minutes_before
        };
        if minutes_before <= i32::MAX as i64 {
            out.push(minutes_before as i32);
        }
    }
    out
}

// ─────────────────────────────── VTODO ─────────────────────────────────

/// Resolve VTODO alarms to a single reminder instant.
///
/// Absolute triggers are used verbatim (this is what Nuncio writes, so the
/// round-trip is lossless). Relative triggers are applied to the due instant
/// (or DTSTART, or 09:00 UTC on the due date as a last resort) using the
/// trigger's own sign, so `-PT15M` means "15 minutes before due".
fn resolve_todo_reminders(
    alarms: &[RawAlarm],
    anchor: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    let mut best: Option<DateTime<Utc>> = None;
    for alarm in alarms {
        let candidate = if let Some(raw) = &alarm.absolute {
            parse_date_value(raw).map(|parsed| parsed.resolve(TzSpec::utc()).with_timezone(&Utc))
        } else if let (Some(raw), Some(base)) = (&alarm.trigger, anchor) {
            parse_duration(raw).map(|duration| base + duration)
        } else {
            None
        };
        if let Some(candidate) = candidate {
            best = Some(match best {
                Some(current) if current <= candidate => current,
                _ => candidate,
            });
        }
    }
    best
}

fn parse_todo(component: &Component, ctx: &TzContext) -> Option<ParsedTodo> {
    let uid = component
        .value("UID")
        .map(|value| unescape_text(value).trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(new_id);

    let title = component
        .value("SUMMARY")
        .map(unescape_text)
        .unwrap_or_default();
    let notes = component
        .value("DESCRIPTION")
        .map(unescape_text)
        .unwrap_or_default();

    let status_text = component
        .value("STATUS")
        .map(|value| value.trim().to_ascii_uppercase())
        .unwrap_or_default();
    let status = if status_text == "COMPLETED" {
        TaskStatus::Completed
    } else {
        TaskStatus::NotStarted
    };

    let importance = component
        .value("PRIORITY")
        .and_then(|value| value.trim().parse::<u8>().ok())
        .map(crate::ics::types::priority_to_importance)
        .unwrap_or(Importance::Normal);

    // DUE: date or date-time. Keep both the date and (when present) the instant
    // so VALARM can be anchored precisely.
    let mut due_date = None;
    let mut due_instant = None;
    if let Some(line) = component.prop("DUE") {
        if let Some(parsed) = parse_date_value(&line.value) {
            due_date = Some(format_date(parsed.date));
            if !parsed.is_date_only() {
                let resolved = ctx.resolve(line.param("TZID"), parsed.date);
                due_instant = Some(parsed.resolve(resolved.spec).with_timezone(&Utc));
            }
        }
    }

    let dtstart_instant = component.prop("DTSTART").and_then(|line| {
        let parsed = parse_date_value(&line.value)?;
        let resolved = ctx.resolve(line.param("TZID"), parsed.date);
        Some(parsed.resolve(resolved.spec).with_timezone(&Utc))
    });

    let completed_at = component
        .value("COMPLETED")
        .and_then(parse_compact_utc)
        .or_else(|| {
            if status == TaskStatus::Completed {
                component.value("LAST-MODIFIED").and_then(parse_compact_utc)
            } else {
                None
            }
        });

    let raw_alarms = parse_alarms(component);
    let anchor = due_instant.or(dtstart_instant).or_else(|| {
        due_date
            .as_deref()
            .and_then(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d").ok())
            .map(|date| Utc.from_utc_datetime(&date.and_hms_opt(9, 0, 0).unwrap_or_default()))
    });
    let reminder_at = resolve_todo_reminders(&raw_alarms, anchor);

    let recurrence = component.value("RRULE").and_then(from_rrule_string);

    let my_day_date = component
        .value("X-NUNCIO-MYDAY")
        .and_then(|value| parse_date_value(value.trim()).map(|parsed| format_date(parsed.date)));

    let steps = parse_steps(component);
    let sequence = component
        .value("SEQUENCE")
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let updated_at = component
        .value("LAST-MODIFIED")
        .and_then(parse_compact_utc)
        .or_else(|| component.value("DTSTAMP").and_then(parse_compact_utc))
        .or_else(|| component.value("CREATED").and_then(parse_compact_utc));

    Some(ParsedTodo {
        uid,
        title,
        notes,
        status,
        importance,
        due_date,
        reminder_at,
        recurrence,
        my_day_date,
        completed_at,
        steps,
        sequence,
        updated_at,
    })
}

fn parse_steps(component: &Component) -> Vec<TaskStep> {
    let mut steps = Vec::new();
    for line in component.props_named("X-NUNCIO-STEP") {
        let parts: Vec<&str> = line.value.split(';').collect();
        if parts.len() < 2 {
            continue;
        }
        let is_completed = parts[0].trim() == "1" || parts[0].trim().eq_ignore_ascii_case("true");
        let sort_order = parts[1].trim().parse::<i64>().unwrap_or(steps.len() as i64);
        let id = parts
            .get(2)
            .map(|value| crate::value::percent_decode(value))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(new_id);
        let title = parts
            .get(3)
            .map(|value| crate::value::percent_decode(value))
            .unwrap_or_default();
        steps.push(TaskStep {
            id,
            title,
            is_completed,
            sort_order,
        });
    }
    steps.sort_by_key(|step| step.sort_order);
    steps
}

// ───────────────────────────── VTIMEZONE ───────────────────────────────

fn parse_vtimezone(component: &Component) -> Option<VTimezone> {
    let tzid = component.value("TZID")?.trim().to_string();
    let mut vtz = VTimezone {
        tzid,
        ..Default::default()
    };
    for child in &component.children {
        let is_standard = child.name == "STANDARD";
        let is_daylight = child.name == "DAYLIGHT";
        if !is_standard && !is_daylight {
            continue;
        }
        let offset = child
            .value("TZOFFSETTO")
            .and_then(tz::parse_offset_to)
            .or_else(|| child.value("TZOFFSETFROM").and_then(tz::parse_offset_to));
        let transition = transition_month_day(child);
        if is_standard {
            if vtz.standard_offset.is_none() {
                vtz.standard_offset = offset;
            }
            if vtz.standard_start.is_none() {
                vtz.standard_start = transition;
            }
        } else {
            if vtz.daylight_offset.is_none() {
                vtz.daylight_offset = offset;
            }
            if vtz.daylight_start.is_none() {
                vtz.daylight_start = transition;
            }
        }
    }
    if vtz.standard_offset.is_none() && vtz.daylight_offset.is_none() {
        return None;
    }
    Some(vtz)
}

fn transition_month_day(component: &Component) -> Option<(u32, u32)> {
    let dtstart = component
        .value("DTSTART")
        .and_then(parse_date_value)
        .map(|parsed| parsed.date);
    let rrule = component.value("RRULE").unwrap_or("");

    let mut by_month: Option<u32> = None;
    let mut by_month_day: Option<u32> = None;
    let mut by_day: Option<(i32, chrono::Weekday)> = None;
    for part in rrule.split(';') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key.trim().to_ascii_uppercase().as_str() {
            "BYMONTH" => {
                by_month = value.split(',').next().and_then(|v| v.trim().parse().ok());
            }
            "BYMONTHDAY" => {
                by_month_day = value.split(',').next().and_then(|v| v.trim().parse().ok());
            }
            "BYDAY" => {
                let value = value.split(',').next().unwrap_or("").trim();
                by_day = parse_byday_transition(value);
            }
            _ => {}
        }
    }

    if let (Some(month), Some(day)) = (by_month, by_month_day) {
        return Some((month, day));
    }
    if let (Some(month), Some((ordinal, weekday))) = (by_month, by_day) {
        let year = dtstart.map(|date| date.year()).unwrap_or(2000);
        let date = crate::value::nth_weekday_of_month(year, month, weekday, ordinal)?;
        return Some((date.month(), date.day()));
    }
    if let Some(month) = by_month {
        return Some((month, 1));
    }
    dtstart.map(|date| (date.month(), date.day()))
}

fn parse_byday_transition(value: &str) -> Option<(i32, chrono::Weekday)> {
    let value = value.trim();
    if value.len() < 2 {
        return None;
    }
    let (number, weekday) = value.split_at(value.len() - 2);
    let weekday = match weekday.to_ascii_uppercase().as_str() {
        "MO" => chrono::Weekday::Mon,
        "TU" => chrono::Weekday::Tue,
        "WE" => chrono::Weekday::Wed,
        "TH" => chrono::Weekday::Thu,
        "FR" => chrono::Weekday::Fri,
        "SA" => chrono::Weekday::Sat,
        "SU" => chrono::Weekday::Sun,
        _ => return None,
    };
    let ordinal = number.trim().parse::<i32>().unwrap_or(1);
    Some((if ordinal == 0 { 1 } else { ordinal }, weekday))
}

/// Parse a single event (utility used by tests and the PIM layer).
pub fn parse_first_event(text: &str) -> Result<ParsedEvent> {
    let calendar = parse_calendar(text)?;
    calendar
        .events
        .into_iter()
        .next()
        .ok_or_else(|| AppError::not_found("no VEVENT in input"))
}

/// Convert a list of parsed events into core events.
pub fn events_to_models(
    events: Vec<ParsedEvent>,
    calendar_id: &str,
    account_id: &str,
) -> Vec<nuncio_core::models::CalendarEvent> {
    events
        .into_iter()
        .map(|event| {
            let id: Id = new_id();
            event.into_event(id, calendar_id.to_string(), account_id.to_string())
        })
        .collect()
}
