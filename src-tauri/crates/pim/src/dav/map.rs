//! CalDAV resource ⇄ core model helpers.
//!
//! A CalDAV resource normally contains one `VCALENDAR` with a master `VEVENT`
//! and zero or more overrides (same UID, different `RECURRENCE-ID`).  The
//! parser preserves the raw resource so recurrence expansion can recover RDATE
//! values.

use nuncio_core::models::{new_id, Attendee, AttendeeStatus, CalendarEvent, StoredEvent};

use crate::error::{protocol, Result};
use crate::provider::RemoteEvent;
use crate::util::{find_self_index, is_email, parse_instant};

/// Parse one iCalendar resource into remote events.
///
/// `href` is the absolute resource URL; the master gets it as `remote_id`,
/// overrides use `href|RECURRENCE-ID` so they stay addressable.
pub fn events_from_ics(
    raw: &str,
    href: &str,
    etag: Option<&str>,
    calendar_id: &str,
    account_id: &str,
    account_email: &str,
) -> Result<Vec<RemoteEvent>> {
    let calendar = nuncio_ical::ics::parse_calendar(raw)
        .map_err(|error| protocol(format!("invalid iCalendar resource {href}: {error}")))?;
    let mut parsed_events = calendar.events;
    // Put the master first: servers are not required to order VEVENTs.
    if let Some(index) = parsed_events
        .iter()
        .position(|parsed| parsed.recurrence_id.is_none())
    {
        let master = parsed_events.remove(index);
        parsed_events.insert(0, master);
    }
    let mut remote_events = Vec::new();
    let mut master_seen = false;
    for parsed in parsed_events {
        let recurrence_id = parsed.recurrence_id.clone();
        let is_master = recurrence_id.is_none() && !master_seen;
        if is_master {
            master_seen = true;
        }
        let remote_id = if is_master {
            href.to_string()
        } else {
            match &recurrence_id {
                Some(value) => format!("{href}|{value}"),
                None => format!("{href}|{}", parsed.uid),
            }
        };
        let rrule = parsed.rrule.clone();
        let exdates = parsed.exdates.clone();
        let mut stored = parsed.into_stored(new_id(), calendar_id, account_id);
        stored.raw_ics = Some(raw.to_string());
        stored.etag = etag.map(str::to_string);
        stored.remote_id = Some(remote_id);
        stored.href = Some(href.to_string());
        stored.rrule = rrule;
        stored.exdates = exdates;
        enrich_identity(&mut stored.event, account_email);
        remote_events.push(RemoteEvent::from_stored(&stored, None));
    }
    Ok(remote_events)
}

/// Fill `is_organizer` / `my_response` from the account address.
pub fn enrich_identity(event: &mut CalendarEvent, account_email: &str) {
    event.is_organizer = event
        .organizer
        .as_ref()
        .is_some_and(|organizer| is_email(&organizer.email, account_email));
    if event.attendees.is_empty() {
        return;
    }
    if let Some(index) = find_self_index(event, account_email) {
        event.my_response = Some(event.attendees[index].status);
    }
}

/// Serialise a single event as an iCalendar resource.
pub fn event_to_ics(stored: &StoredEvent) -> String {
    nuncio_ical::ics::write_stored_event(stored, None)
}

/// Serialise a complete series (master + overrides) as one resource.
pub fn compose_ics(series: &[StoredEvent]) -> String {
    nuncio_ical::ics::write_stored_calendar(series)
}

/// Apply edited fields from `updated` onto the stored master.
pub fn apply_master_update(master: &StoredEvent, updated: &CalendarEvent) -> StoredEvent {
    let mut stored = master.clone();
    let event = &mut stored.event;
    event.title = updated.title.clone();
    event.location = updated.location.clone();
    event.description = updated.description.clone();
    event.start = updated.start.clone();
    event.end = updated.end.clone();
    event.all_day = updated.all_day;
    event.time_zone = updated.time_zone.clone();
    event.recurrence = updated.recurrence.clone();
    event.show_as = updated.show_as;
    event.is_private = updated.is_private;
    event.attendees = updated.attendees.clone();
    event.reminders = updated.reminders.clone();
    event.online_meeting_url = updated.online_meeting_url.clone();
    event.color = updated.color.clone();
    event.status = updated.status;
    if updated.organizer.is_some() {
        event.organizer = updated.organizer.clone();
    }
    let recurrence_changed = master.event.recurrence != updated.recurrence;
    stored.rrule =
        crate::util::rrule_for_update(master.rrule.as_deref(), updated.recurrence.as_ref());
    if recurrence_changed {
        // RDATEs belong to the old rule; drop the raw copy so a stale
        // occurrence is not re-emitted by the writer/expander.
        stored.raw_ics = None;
    }
    stored
}

/// Build the stored override for a "this occurrence" edit.
pub fn apply_override_update(
    master: &StoredEvent,
    existing: Option<&StoredEvent>,
    updated: &CalendarEvent,
    instance_start: &str,
) -> StoredEvent {
    let mut stored = existing.cloned().unwrap_or_else(|| StoredEvent {
        event: CalendarEvent::new(
            new_id(),
            master.event.calendar_id.clone(),
            master.event.account_id.clone(),
            master.event.uid.clone(),
            updated.title.clone(),
            updated.start.clone(),
            updated.end.clone(),
        ),
        raw_ics: None,
        etag: None,
        remote_id: None,
        href: master.href.clone(),
        rrule: None,
        exdates: Vec::new(),
    });
    let event = &mut stored.event;
    event.uid = master.event.uid.clone();
    event.series_id = Some(master.event.id.clone());
    event.recurrence_id = Some(instance_start.to_string());
    event.recurrence = None;
    event.title = updated.title.clone();
    event.location = updated.location.clone();
    event.description = updated.description.clone();
    event.start = updated.start.clone();
    event.end = updated.end.clone();
    event.all_day = updated.all_day;
    event.time_zone = updated.time_zone.clone();
    event.show_as = updated.show_as;
    event.is_private = updated.is_private;
    event.attendees = updated.attendees.clone();
    event.reminders = updated.reminders.clone();
    event.online_meeting_url = updated.online_meeting_url.clone();
    event.color = updated.color.clone();
    event.status = updated.status;
    stored.href = master.href.clone();
    stored.remote_id = Some(match &stored.event.recurrence_id {
        Some(value) => format!("{}|{}", master.href.clone().unwrap_or_default(), value),
        None => master.href.clone().unwrap_or_default(),
    });
    stored
}

/// Set the current user's `PARTSTAT` on an event (master or occurrence).
pub fn set_response(event: &mut CalendarEvent, email: &str, status: AttendeeStatus) {
    if let Some(index) = find_self_index(event, email) {
        event.attendees[index].status = status;
        event.my_response = Some(status);
        return;
    }
    event.attendees.push(Attendee {
        email: email.to_string(),
        name: None,
        role: nuncio_core::models::AttendeeRole::Required,
        status,
    });
    event.my_response = Some(status);
}

/// True when `value` is at or after `split`.
pub fn at_or_after(value: &str, split: &str) -> bool {
    match (parse_instant(value), parse_instant(split)) {
        (Ok(value), Ok(split)) => value >= split,
        _ => value >= split,
    }
}

/// Keep the events of a series that belong to the master resource after a
/// "this and following" split.
pub fn filter_after_split(
    series: &[StoredEvent],
    master_id: &str,
    split: &str,
) -> Vec<StoredEvent> {
    series
        .iter()
        .filter(|stored| {
            if stored.event.id == master_id {
                return true;
            }
            match &stored.event.recurrence_id {
                Some(value) => !at_or_after(value, split),
                None => true,
            }
        })
        .cloned()
        .collect()
}

/// Helper used by tests: true when the resource carries an override.
pub fn has_override(events: &[RemoteEvent]) -> bool {
    events
        .iter()
        .any(|event| event.event.recurrence_id.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Test//EN\r\nBEGIN:VEVENT\r\nUID:series-1\r\nSUMMARY:Standup\r\nDTSTART:20261112T090000Z\r\nDTEND:20261112T091500Z\r\nRRULE:FREQ=WEEKLY;BYDAY=TH\r\nEXDATE:20261119T090000Z\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:me@example.com\r\nORGANIZER:mailto:boss@example.com\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:series-1\r\nRECURRENCE-ID:20261126T090000Z\r\nSUMMARY:Standup moved\r\nDTSTART:20261126T100000Z\r\nDTEND:20261126T101500Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn parses_master_and_override() {
        let events = events_from_ics(
            ICS,
            "https://dav/cal/x.ics",
            Some("etag-1"),
            "cal",
            "acc",
            "me@example.com",
        )
        .expect("parse");
        assert_eq!(events.len(), 2);
        let master = &events[0];
        assert_eq!(master.event.uid, "series-1");
        assert_eq!(master.exdates.len(), 1);
        assert!(master.event.recurrence.is_some());
        assert_eq!(master.event.my_response, Some(AttendeeStatus::Accepted));
        assert_eq!(
            master.event.organizer.as_ref().map(|a| a.email.as_str()),
            Some("boss@example.com")
        );
        let exception = &events[1];
        assert!(exception.event.recurrence_id.is_some());
        assert_eq!(
            exception.remote_id.as_deref(),
            Some("https://dav/cal/x.ics|2026-11-26T09:00:00+00:00")
        );
        assert_eq!(exception.event.title, "Standup moved");
    }

    #[test]
    fn composes_series_resource() {
        let events = events_from_ics(
            ICS,
            "https://dav/cal/x.ics",
            Some("etag-1"),
            "cal",
            "acc",
            "me@example.com",
        )
        .expect("parse");
        let stored: Vec<StoredEvent> = events
            .iter()
            .map(|remote| StoredEvent {
                event: remote.event.clone(),
                raw_ics: remote.raw_ics.clone(),
                etag: remote.etag.clone(),
                remote_id: remote.remote_id.clone(),
                href: remote.href.clone(),
                rrule: remote.rrule.clone(),
                exdates: remote.exdates.clone(),
            })
            .collect();
        let ics = compose_ics(&stored);
        assert!(ics.contains("BEGIN:VCALENDAR"));
        assert!(ics.contains("UID:series-1"));
        assert_eq!(ics.matches("BEGIN:VEVENT").count(), 2);
    }

    #[test]
    fn applies_master_edit() {
        let events = events_from_ics(
            ICS,
            "https://dav/cal/x.ics",
            None,
            "cal",
            "acc",
            "me@example.com",
        )
        .expect("parse");
        let master = StoredEvent {
            event: events[0].event.clone(),
            raw_ics: None,
            etag: None,
            remote_id: events[0].remote_id.clone(),
            href: events[0].href.clone(),
            rrule: events[0].rrule.clone(),
            exdates: events[0].exdates.clone(),
        };
        let mut updated = master.event.clone();
        updated.title = "Renamed".into();
        updated.start = "2026-11-12T09:30:00Z".into();
        let result = apply_master_update(&master, &updated);
        assert_eq!(result.event.title, "Renamed");
        assert_eq!(result.event.start, "2026-11-12T09:30:00Z");
        assert_eq!(result.exdates.len(), 1);
    }

    #[test]
    fn filters_exceptions_after_split() {
        let events = events_from_ics(
            ICS,
            "https://dav/cal/x.ics",
            None,
            "cal",
            "acc",
            "me@example.com",
        )
        .expect("parse");
        let stored: Vec<StoredEvent> = events
            .iter()
            .map(|remote| StoredEvent {
                event: remote.event.clone(),
                raw_ics: None,
                etag: None,
                remote_id: None,
                href: None,
                rrule: None,
                exdates: Vec::new(),
            })
            .collect();
        let master_id = stored[0].event.id.clone();
        let kept = filter_after_split(&stored, &master_id, "2026-11-20T00:00:00Z");
        assert_eq!(kept.len(), 1);
        let kept = filter_after_split(&stored, &master_id, "2026-11-27T00:00:00Z");
        assert_eq!(kept.len(), 2);
    }
}
