//! iCalendar writing (VEVENT / VTODO).

use chrono::Utc;
use nuncio_core::models::{
    Attendee, AttendeeRole, AttendeeStatus, CalendarEvent, EventStatus, ShowAs, StoredEvent, Task,
    TaskStatus,
};

use crate::recurrence::to_rrule_string;
use crate::tz;
use crate::value::{
    escape_text, format_compact_date, format_compact_local, format_date, format_utc_compact,
    parse_date_value, parse_instant, push_line, TzSpec,
};

const PRODID: &str = "-//Nuncio//EN";

/// Write a single event wrapped in a VCALENDAR.
pub fn write_event(event: &CalendarEvent, method: Option<&str>) -> String {
    write_event_with_exdates(event, &[], method)
}

/// Write a single event with its EXDATE list.
pub fn write_event_with_exdates(
    event: &CalendarEvent,
    exdates: &[String],
    method: Option<&str>,
) -> String {
    let mut out = String::new();
    push_line(&mut out, "BEGIN:VCALENDAR");
    push_line(&mut out, "VERSION:2.0");
    push_line(&mut out, &format!("PRODID:{PRODID}"));
    push_line(&mut out, "CALSCALE:GREGORIAN");
    if let Some(method) = method {
        push_line(
            &mut out,
            &format!("METHOD:{}", method.trim().to_ascii_uppercase()),
        );
    }
    write_vevent(&mut out, event, exdates, &[], None, 0, true, None);
    push_line(&mut out, "END:VCALENDAR");
    out
}

/// Write a stored event (event + rrule/exdates/rdates metadata).
pub fn write_stored_event(stored: &StoredEvent, method: Option<&str>) -> String {
    let exdates = stored.exdates.clone();
    let rdates = rdates_from_stored(stored);
    let rrule = stored_rrule_text(stored);
    let mut out = String::new();
    push_line(&mut out, "BEGIN:VCALENDAR");
    push_line(&mut out, "VERSION:2.0");
    push_line(&mut out, &format!("PRODID:{PRODID}"));
    push_line(&mut out, "CALSCALE:GREGORIAN");
    if let Some(method) = method {
        push_line(
            &mut out,
            &format!("METHOD:{}", method.trim().to_ascii_uppercase()),
        );
    }
    write_vevent(
        &mut out,
        &stored.event,
        &exdates,
        &rdates,
        rrule.as_deref(),
        0,
        true,
        None,
    );
    push_line(&mut out, "END:VCALENDAR");
    out
}

/// Write several events into one VCALENDAR.
pub fn write_calendar(events: &[CalendarEvent]) -> String {
    let mut out = String::new();
    push_line(&mut out, "BEGIN:VCALENDAR");
    push_line(&mut out, "VERSION:2.0");
    push_line(&mut out, &format!("PRODID:{PRODID}"));
    push_line(&mut out, "CALSCALE:GREGORIAN");
    for event in events {
        let rrule = event
            .recurrence
            .as_ref()
            .map(|rule| rrule_for_event(rule, event.all_day, event_zone(event)));
        write_vevent(&mut out, event, &[], &[], rrule.as_deref(), 0, true, None);
    }
    push_line(&mut out, "END:VCALENDAR");
    out
}

/// Write several stored events into one VCALENDAR.
pub fn write_stored_calendar(events: &[StoredEvent]) -> String {
    let mut out = String::new();
    push_line(&mut out, "BEGIN:VCALENDAR");
    push_line(&mut out, "VERSION:2.0");
    push_line(&mut out, &format!("PRODID:{PRODID}"));
    push_line(&mut out, "CALSCALE:GREGORIAN");
    for stored in events {
        let rdates = rdates_from_stored(stored);
        let rrule = stored_rrule_text(stored);
        write_vevent(
            &mut out,
            &stored.event,
            &stored.exdates,
            &rdates,
            rrule.as_deref(),
            0,
            true,
            None,
        );
    }
    push_line(&mut out, "END:VCALENDAR");
    out
}

/// Internal writer with sequence / alarm / status control (used by iTIP).
pub(crate) fn write_vevent_ext(
    event: &CalendarEvent,
    exdates: &[String],
    method: &str,
    sequence: u32,
    include_alarms: bool,
    force_status: Option<EventStatus>,
) -> String {
    let mut out = String::new();
    push_line(&mut out, "BEGIN:VCALENDAR");
    push_line(&mut out, "VERSION:2.0");
    push_line(&mut out, &format!("PRODID:{PRODID}"));
    push_line(&mut out, "CALSCALE:GREGORIAN");
    push_line(&mut out, &format!("METHOD:{}", method.to_ascii_uppercase()));
    write_vevent(
        &mut out,
        event,
        exdates,
        &[],
        None,
        sequence,
        include_alarms,
        force_status,
    );
    push_line(&mut out, "END:VCALENDAR");
    out
}

fn write_vevent(
    out: &mut String,
    event: &CalendarEvent,
    exdates: &[String],
    rdates: &[String],
    rrule_text: Option<&str>,
    sequence: u32,
    include_alarms: bool,
    force_status: Option<EventStatus>,
) {
    push_line(out, "BEGIN:VEVENT");
    let uid = if event.uid.trim().is_empty() {
        event.id.clone()
    } else {
        event.uid.clone()
    };
    push_line(out, &format!("UID:{}", escape_text(&uid)));
    push_line(
        out,
        &format!(
            "DTSTAMP:{}",
            format_utc_compact(event.updated_at.unwrap_or_else(Utc::now))
        ),
    );
    if let Some(updated) = event.updated_at {
        push_line(
            out,
            &format!("LAST-MODIFIED:{}", format_utc_compact(updated)),
        );
        push_line(out, &format!("CREATED:{}", format_utc_compact(updated)));
    }
    push_line(out, &format!("SEQUENCE:{sequence}"));

    let tz = event_zone(event);
    write_dt_property(out, "DTSTART", &event.start, event.all_day, tz);
    write_dt_property(out, "DTEND", &event.end, event.all_day, tz);

    if let Some(rrule) = rrule_text {
        push_line(out, &format!("RRULE:{rrule}"));
    } else if let Some(rule) = &event.recurrence {
        push_line(
            out,
            &format!("RRULE:{}", rrule_for_event(rule, event.all_day, tz)),
        );
    }
    write_exdates(out, exdates, event.all_day, tz);
    write_rdates(out, rdates, event.all_day, tz);
    if let Some(recurrence_id) = &event.recurrence_id {
        write_dt_property(out, "RECURRENCE-ID", recurrence_id, event.all_day, tz);
    }

    if !event.title.is_empty() {
        push_line(out, &format!("SUMMARY:{}", escape_text(&event.title)));
    }
    if !event.location.is_empty() {
        push_line(out, &format!("LOCATION:{}", escape_text(&event.location)));
    }
    if !event.description.is_empty() {
        push_line(
            out,
            &format!("DESCRIPTION:{}", escape_text(&event.description)),
        );
    }
    if event.is_private {
        push_line(out, "CLASS:PRIVATE");
    }

    let status = force_status.unwrap_or(event.status);
    push_line(
        out,
        &format!(
            "STATUS:{}",
            match status {
                EventStatus::Confirmed => "CONFIRMED",
                EventStatus::Tentative => "TENTATIVE",
                EventStatus::Cancelled => "CANCELLED",
            }
        ),
    );
    let (transp, busy) = match event.show_as {
        ShowAs::Busy => ("OPAQUE", "BUSY"),
        ShowAs::Free => ("TRANSPARENT", "FREE"),
        ShowAs::Tentative => ("OPAQUE", "TENTATIVE"),
        ShowAs::Oof => ("OPAQUE", "OOF"),
        ShowAs::WorkingElsewhere => ("OPAQUE", "WORKINGELSEWHERE"),
    };
    push_line(out, &format!("TRANSP:{transp}"));
    push_line(out, &format!("X-MICROSOFT-CDO-BUSYSTATUS:{busy}"));

    if let Some(url) = &event.online_meeting_url {
        if !url.trim().is_empty() {
            push_line(out, &format!("URL:{url}"));
            if url.contains("meet.google.com") {
                push_line(out, &format!("X-GOOGLE-CONFERENCE:{}", escape_text(url)));
            }
            if url.contains("teams.microsoft.com") {
                push_line(
                    out,
                    &format!("X-MICROSOFT-SKYPETEAMSMEETINGURL:{}", escape_text(url)),
                );
            }
        }
    }

    if let Some(organizer) = &event.organizer {
        write_organizer(out, organizer);
    }
    for attendee in &event.attendees {
        write_attendee(out, attendee);
    }

    if include_alarms {
        for minutes in &event.reminders {
            if *minutes < 0 {
                continue;
            }
            push_line(out, "BEGIN:VALARM");
            push_line(out, "ACTION:DISPLAY");
            push_line(
                out,
                &format!(
                    "DESCRIPTION:{}",
                    escape_text(if event.title.is_empty() {
                        "Reminder"
                    } else {
                        &event.title
                    })
                ),
            );
            push_line(
                out,
                &format!(
                    "TRIGGER:{}",
                    crate::value::format_duration_minutes(*minutes as i64)
                ),
            );
            push_line(out, "END:VALARM");
        }
    }

    push_line(out, "END:VEVENT");
}

fn event_zone(event: &CalendarEvent) -> TzSpec {
    if event.all_day {
        return TzSpec::utc();
    }
    tz::resolve_iana(&event.time_zone)
        .map(TzSpec::Named)
        .unwrap_or_else(TzSpec::utc)
}

fn write_dt_property(out: &mut String, property: &str, value: &str, all_day: bool, tz: TzSpec) {
    if all_day {
        if let Some(date) = parse_date_value(value).map(|parsed| parsed.date) {
            push_line(
                out,
                &format!("{property};VALUE=DATE:{}", format_compact_date(date)),
            );
        }
        return;
    }
    let Some(instant) = parse_instant(value) else {
        tracing::warn!(property, value, "cannot format date-time property");
        return;
    };
    match tz {
        TzSpec::Named(zone) if !zone.name().eq_ignore_ascii_case("UTC") => {
            let local = instant.with_timezone(&zone);
            push_line(
                out,
                &format!(
                    "{property};TZID={}:{}",
                    param_value(zone.name()),
                    format_compact_local(local.fixed_offset())
                ),
            );
        }
        _ => {
            push_line(out, &format!("{property}:{}", format_utc_compact(instant)));
        }
    }
}

/// RRULE serialisation for events. A timed DTSTART requires a UTC DATE-TIME
/// `UNTIL` (RFC 5545 §3.3.10); we use the last second of the until date in the
/// event's zone so every occurrence on that date is still included.
fn rrule_for_event(
    rule: &nuncio_core::models::RecurrenceRule,
    all_day: bool,
    tz: TzSpec,
) -> String {
    let mut parts: Vec<String> = to_rrule_string(rule)
        .split(';')
        .filter(|part| !part.starts_with("UNTIL="))
        .map(str::to_string)
        .collect();
    if rule.count.is_none() {
        if let Some(until) = rule.until.as_deref().and_then(parse_date_value) {
            let value = if all_day {
                format_compact_date(until.date)
            } else {
                let Some(local) = until.date.and_hms_opt(23, 59, 59) else {
                    return parts.join(";");
                };
                let instant = tz.resolve_local(local).with_timezone(&Utc);
                format_utc_compact(instant)
            };
            parts.push(format!("UNTIL={value}"));
        }
    }
    parts.join(";")
}

fn write_exdates(out: &mut String, exdates: &[String], all_day: bool, tz: TzSpec) {
    write_date_list(out, "EXDATE", exdates, all_day, tz);
}

fn write_rdates(out: &mut String, rdates: &[String], all_day: bool, tz: TzSpec) {
    write_date_list(out, "RDATE", rdates, all_day, tz);
}

fn write_date_list(out: &mut String, property: &str, values: &[String], all_day: bool, tz: TzSpec) {
    if values.is_empty() {
        return;
    }
    let mut formatted = Vec::new();
    for value in values {
        if all_day {
            if let Some(date) = parse_date_value(value).map(|parsed| parsed.date) {
                formatted.push(format_compact_date(date));
            }
            continue;
        }
        let Some(instant) = parse_instant(value) else {
            continue;
        };
        match tz {
            TzSpec::Named(zone) if !zone.name().eq_ignore_ascii_case("UTC") => {
                let local = instant.with_timezone(&zone);
                formatted.push(format_compact_local(local.fixed_offset()));
            }
            _ => formatted.push(format_utc_compact(instant)),
        }
    }
    if formatted.is_empty() {
        return;
    }
    let line = if all_day {
        format!("{property};VALUE=DATE:{}", formatted.join(","))
    } else {
        match tz {
            TzSpec::Named(zone) if !zone.name().eq_ignore_ascii_case("UTC") => {
                format!(
                    "{property};TZID={}:{}",
                    param_value(zone.name()),
                    formatted.join(",")
                )
            }
            _ => format!("{property}:{}", formatted.join(",")),
        }
    };
    push_line(out, &line);
}

/// The RRULE text to emit for a stored event.
///
/// The original provider text is preferred so rules that the structured model
/// cannot express (`BYDAY=1MO,3WE`) are not rewritten.  `UNTIL` is normalised
/// for the event's all-day/timezone semantics so both original and regenerated
/// text stays valid.
fn stored_rrule_text(stored: &StoredEvent) -> Option<String> {
    let rule = stored.event.recurrence.as_ref()?;
    let tz = event_zone(&stored.event);
    match stored
        .rrule
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(text) => Some(normalize_rrule_text(text, stored.event.all_day, tz)),
        None => Some(rrule_for_event(rule, stored.event.all_day, tz)),
    }
}

fn normalize_rrule_text(value: &str, all_day: bool, tz: TzSpec) -> String {
    let raw = value
        .trim()
        .strip_prefix("RRULE:")
        .or_else(|| value.trim().strip_prefix("rrule:"))
        .unwrap_or_else(|| value.trim());
    let mut parts: Vec<String> = Vec::new();
    for part in raw.split(';') {
        if let Some(until) = part.strip_prefix("UNTIL=") {
            if all_day {
                if let Some(parsed) = parse_date_value(until) {
                    parts.push(format!("UNTIL={}", format_compact_date(parsed.date)));
                    continue;
                }
            } else if !until.contains('T') {
                if let Some(parsed) = parse_date_value(until) {
                    if let Some(local) = parsed.date.and_hms_opt(23, 59, 59) {
                        let instant = tz.resolve_local(local).with_timezone(&Utc);
                        parts.push(format!("UNTIL={}", format_utc_compact(instant)));
                        continue;
                    }
                }
            }
        }
        parts.push(part.to_string());
    }
    parts.join(";")
}

/// Recover RDATE values from the stored raw iCalendar resource (the database
/// keeps the resource, or a compact RDATE-only document, for this purpose).
fn rdates_from_stored(stored: &StoredEvent) -> Vec<String> {
    if stored.event.recurrence_id.is_some() {
        return Vec::new();
    }
    let Some(raw) = stored.raw_ics.as_deref() else {
        return Vec::new();
    };
    let Ok(calendar) = crate::ics::parse_calendar(raw) else {
        return Vec::new();
    };
    let master = calendar
        .events
        .iter()
        .find(|event| event.uid == stored.event.uid && event.recurrence_id.is_none())
        .or_else(|| {
            calendar
                .events
                .iter()
                .find(|event| event.uid == stored.event.uid)
        });
    master.map(|event| event.rdates.clone()).unwrap_or_default()
}

fn write_organizer(out: &mut String, attendee: &Attendee) {
    let mut head = String::from("ORGANIZER");
    if let Some(name) = &attendee.name {
        if !name.trim().is_empty() {
            head.push_str(";CN=");
            head.push_str(&param_value(name));
        }
    }
    push_line(out, &format!("{head}:mailto:{}", attendee.email));
}

fn write_attendee(out: &mut String, attendee: &Attendee) {
    let mut head = String::from("ATTENDEE");
    if let Some(name) = &attendee.name {
        if !name.trim().is_empty() {
            head.push_str(";CN=");
            head.push_str(&param_value(name));
        }
    }
    let role = match attendee.role {
        AttendeeRole::Required => "REQ-PARTICIPANT",
        AttendeeRole::Optional => "OPT-PARTICIPANT",
        AttendeeRole::Resource => "RESOURCE",
        AttendeeRole::Chair => "CHAIR",
    };
    let status = match attendee.status {
        AttendeeStatus::NeedsAction => "NEEDS-ACTION",
        AttendeeStatus::Accepted => "ACCEPTED",
        AttendeeStatus::Declined => "DECLINED",
        AttendeeStatus::Tentative => "TENTATIVE",
        AttendeeStatus::Delegated => "DELEGATED",
    };
    head.push_str(&format!(";ROLE={role};PARTSTAT={status};RSVP=TRUE"));
    push_line(out, &format!("{head}:mailto:{}", attendee.email));
}

fn param_value(value: &str) -> String {
    let needs_quotes = value
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, ';' | ':' | ',' | '"'));
    if !needs_quotes {
        return value.to_string();
    }
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

// ─────────────────────────────── VTODO ─────────────────────────────────

/// Write one task as a VCALENDAR/VTODO.
pub fn write_task(task: &Task) -> String {
    write_tasks(std::slice::from_ref(task))
}

/// Write several tasks into one VCALENDAR.
pub fn write_tasks(tasks: &[Task]) -> String {
    let mut out = String::new();
    push_line(&mut out, "BEGIN:VCALENDAR");
    push_line(&mut out, "VERSION:2.0");
    push_line(&mut out, &format!("PRODID:{PRODID}"));
    push_line(&mut out, "CALSCALE:GREGORIAN");
    for task in tasks {
        write_vtodo(&mut out, task);
    }
    push_line(&mut out, "END:VCALENDAR");
    out
}

fn write_vtodo(out: &mut String, task: &Task) {
    push_line(out, "BEGIN:VTODO");
    let uid = if task.id.trim().is_empty() {
        nuncio_core::models::new_id()
    } else {
        task.id.clone()
    };
    push_line(out, &format!("UID:{}", escape_text(&uid)));
    push_line(
        out,
        &format!("DTSTAMP:{}", format_utc_compact(task.updated_at)),
    );
    push_line(
        out,
        &format!("CREATED:{}", format_utc_compact(task.created_at)),
    );
    push_line(
        out,
        &format!("LAST-MODIFIED:{}", format_utc_compact(task.updated_at)),
    );
    push_line(out, &format!("SUMMARY:{}", escape_text(&task.title)));
    if !task.notes.is_empty() {
        push_line(out, &format!("DESCRIPTION:{}", escape_text(&task.notes)));
    }
    if let Some(due) = &task.due_date {
        if let Some(parsed) = parse_date_value(due) {
            push_line(
                out,
                &format!("DUE;VALUE=DATE:{}", format_compact_date(parsed.date)),
            );
        }
    }
    let status = match task.status {
        TaskStatus::Completed => "COMPLETED",
        TaskStatus::NotStarted => {
            if task.completed_at.is_some() {
                "COMPLETED"
            } else {
                "NEEDS-ACTION"
            }
        }
    };
    push_line(out, &format!("STATUS:{status}"));
    if let Some(completed) = task.completed_at {
        push_line(out, &format!("COMPLETED:{}", format_utc_compact(completed)));
    }
    push_line(
        out,
        &format!(
            "PRIORITY:{}",
            crate::ics::types::importance_to_priority(task.importance)
        ),
    );
    if let Some(rule) = &task.recurrence {
        push_line(out, &format!("RRULE:{}", to_rrule_string(rule)));
    }
    if let Some(reminder) = task.reminder_at {
        push_line(out, "BEGIN:VALARM");
        push_line(out, "ACTION:DISPLAY");
        push_line(out, &format!("DESCRIPTION:{}", escape_text(&task.title)));
        push_line(
            out,
            &format!("TRIGGER;VALUE=DATE-TIME:{}", format_utc_compact(reminder)),
        );
        push_line(out, "END:VALARM");
    }
    if let Some(my_day) = &task.my_day_date {
        if let Some(parsed) = parse_date_value(my_day) {
            push_line(out, &format!("X-NUNCIO-MYDAY:{}", format_date(parsed.date)));
        }
    }
    for step in &task.steps {
        push_line(
            out,
            &format!(
                "X-NUNCIO-STEP:{};{};{};{}",
                if step.is_completed { 1 } else { 0 },
                step.sort_order,
                crate::value::percent_encode(&step.id),
                crate::value::percent_encode(&step.title)
            ),
        );
    }
    push_line(out, "END:VTODO");
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::models::{new_id, RecurrenceFreq, RecurrenceRule, Weekday};

    fn stored_with_rule(rrule: &str, raw_ics: Option<&str>) -> StoredEvent {
        let mut event = CalendarEvent::new(
            new_id(),
            "cal",
            "acc",
            "uid-write",
            "Mixed rule",
            "2026-01-05T10:00:00Z",
            "2026-01-05T11:00:00Z",
        );
        event.time_zone = "UTC".into();
        event.recurrence = crate::recurrence::from_rrule_string(rrule);
        let mut stored = StoredEvent::new(event);
        stored.rrule = Some(rrule.to_string());
        stored.raw_ics = raw_ics.map(str::to_string);
        stored
    }

    #[test]
    fn stored_write_preserves_multi_ordinal_rrule_text() {
        let stored = stored_with_rule("FREQ=MONTHLY;BYDAY=1MO,3WE", None);
        let ics = write_stored_event(&stored, None);
        assert!(
            ics.contains("RRULE:FREQ=MONTHLY;BYDAY=1MO,3WE"),
            "original RRULE text must not be rewritten:\n{ics}"
        );
        assert!(!ics.contains("BYSETPOS"));
    }

    #[test]
    fn stored_write_keeps_rdate_from_raw_ics() {
        let raw = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Test//EN\r\nBEGIN:VEVENT\r\nUID:uid-write\r\nDTSTART:20260105T100000Z\r\nDTEND:20260105T110000Z\r\nRRULE:FREQ=MONTHLY;BYDAY=1MO,3WE\r\nRDATE:20260115T100000Z,20260220T100000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let stored = stored_with_rule("FREQ=MONTHLY;BYDAY=1MO,3WE", Some(raw));
        let ics = write_stored_event(&stored, None);
        assert!(
            ics.contains("RDATE:20260115T100000Z,20260220T100000Z"),
            "RDATE values must survive a DAV PUT/export:\n{ics}"
        );
        assert!(ics.contains("RRULE:FREQ=MONTHLY;BYDAY=1MO,3WE"));
    }

    #[test]
    fn regenerated_rrule_text_keeps_structured_semantics() {
        let mut event = CalendarEvent::new(
            new_id(),
            "cal",
            "acc",
            "uid-regenerated",
            "Weekly",
            "2026-01-05T10:00:00Z",
            "2026-01-05T11:00:00Z",
        );
        event.time_zone = "UTC".into();
        event.recurrence = Some(RecurrenceRule {
            freq: RecurrenceFreq::Monthly,
            interval: 1,
            by_weekday: vec![Weekday::Mo, Weekday::We],
            by_set_pos: vec![1],
            by_month_day: Vec::new(),
            by_month: Vec::new(),
            count: None,
            until: Some("2026-12-31".into()),
        });
        let stored = StoredEvent::new(event);
        let ics = write_stored_event(&stored, None);
        assert!(ics.contains("BYDAY=MO,WE"));
        assert!(ics.contains("BYSETPOS=1"));
        // A timed DTSTART requires a UTC DATE-TIME UNTIL.
        assert!(ics.contains("UNTIL=20261231T"));
        assert!(!ics.contains("UNTIL=20261231\n"));
    }
}
