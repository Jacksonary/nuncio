//! iTIP (RFC 5546) payloads and calendar e-mails.
//!
//! The writer emits the VCALENDAR/VEVENT body; [`itip_email`] wraps it in a
//! `multipart/mixed` message whose first part is a
//! `multipart/alternative` (plain text + `text/calendar; method=…`) and whose
//! second part is an `invite.ics` attachment, which is what Outlook/Gmail
//! expect for a portable invitation.

use mail_builder::headers::address::Address as MailAddress;
use mail_builder::headers::content_type::ContentType;
use mail_builder::headers::text::Text as MailText;
use mail_builder::mime::MimePart;
use mail_builder::MessageBuilder;
use nuncio_core::models::{
    Address, Attendee, AttendeeRole, AttendeeStatus, CalendarEvent, EventStatus,
};

use crate::ics::write_vevent_ext;
use crate::value::{escape_text, fold_line};

/// Build a `METHOD:REPLY` VCALENDAR for one attendee.
pub fn build_reply(
    event: &CalendarEvent,
    attendee_email: &str,
    status: AttendeeStatus,
    comment: Option<&str>,
) -> String {
    build_reply_with_sequence(event, attendee_email, status, comment, 0)
}

/// Build a REPLY with an explicit `SEQUENCE`.
pub fn build_reply_with_sequence(
    event: &CalendarEvent,
    attendee_email: &str,
    status: AttendeeStatus,
    comment: Option<&str>,
    sequence: u32,
) -> String {
    let mut reply = event.clone();
    reply.attendees = filter_attendee(event, attendee_email, status);
    reply.is_organizer = false;

    let mut ics = write_vevent_ext(&reply, &[], "REPLY", sequence, false, None);
    if let Some(comment) = comment.filter(|text| !text.trim().is_empty()) {
        insert_before_end(&mut ics, &format!("COMMENT:{}", escape_text(comment)));
    }
    ics
}

/// Build a `METHOD:REQUEST` VCALENDAR.
pub fn build_request(event: &CalendarEvent) -> String {
    build_request_with_sequence(event, 0)
}

/// Build a REQUEST with an explicit `SEQUENCE`.
pub fn build_request_with_sequence(event: &CalendarEvent, sequence: u32) -> String {
    write_vevent_ext(event, &[], "REQUEST", sequence, true, None)
}

/// Build a `METHOD:CANCEL` VCALENDAR. `SEQUENCE` is bumped to 1 because the
/// core event model does not persist the counter; call
/// [`build_cancel_with_sequence`] when the previous value is known.
pub fn build_cancel(event: &CalendarEvent) -> String {
    build_cancel_with_sequence(event, 1)
}

/// Build a CANCEL with an explicit (already bumped) `SEQUENCE`.
pub fn build_cancel_with_sequence(event: &CalendarEvent, sequence: u32) -> String {
    write_vevent_ext(
        event,
        &[],
        "CANCEL",
        sequence,
        false,
        Some(EventStatus::Cancelled),
    )
}

fn filter_attendee(
    event: &CalendarEvent,
    attendee_email: &str,
    status: AttendeeStatus,
) -> Vec<Attendee> {
    let needle = attendee_email.trim();
    let mut attendees = Vec::new();
    let mut found = false;
    for attendee in &event.attendees {
        if attendee.email.eq_ignore_ascii_case(needle) {
            let mut attendee = attendee.clone();
            attendee.status = status;
            attendees.push(attendee);
            found = true;
        }
    }
    if !found && !needle.is_empty() {
        attendees.push(Attendee {
            email: needle.to_string(),
            name: None,
            role: AttendeeRole::Required,
            status,
        });
    }
    attendees
}

fn insert_before_end(document: &mut String, line: &str) {
    let Some(index) = document.rfind("END:VEVENT") else {
        return;
    };
    let folded = fold_line(line);
    document.insert_str(index, &folded);
}

/// Build the RFC 5322 message that transports an iTIP payload.
///
/// The body is:
/// ```text
/// multipart/mixed
///   multipart/alternative
///     text/plain
///     text/calendar; method=<METHOD>
///   text/calendar; method=<METHOD>; name=invite.ics (attachment)
/// ```
pub fn itip_email(
    from: &Address,
    to: &[Address],
    subject: &str,
    body_text: &str,
    ics: &str,
    method: &str,
) -> Vec<u8> {
    let method_upper = method.trim().to_ascii_uppercase();
    let text_part = MimePart::new("text/plain", body_text.to_string());
    let calendar_part = MimePart::new(
        ContentType::new("text/calendar")
            .attribute("method", method_upper.clone())
            .attribute("charset", "utf-8"),
        ics.to_string(),
    )
    .header("Content-Transfer-Encoding", MailText::new("8bit"));
    let alternative = MimePart::new("multipart/alternative", vec![text_part, calendar_part]);
    let attachment = MimePart::new(
        ContentType::new("application/ics")
            .attribute("method", method_upper.clone())
            .attribute("name", "invite.ics"),
        ics.to_string(),
    )
    .attachment("invite.ics");
    let body = MimePart::new("multipart/mixed", vec![alternative, attachment]);

    let from_address = MailAddress::new_address(from.name.clone(), from.email.clone());
    let to_address = MailAddress::new_list(
        to.iter()
            .map(|address| MailAddress::new_address(address.name.clone(), address.email.clone()))
            .collect(),
    );

    let message = MessageBuilder::new()
        .from(from_address)
        .to(to_address)
        .subject(subject.to_string())
        .header(
            "Content-Class",
            MailText::new("urn:content-classes:calendarmessage"),
        )
        .body(body);

    match message.write_to_vec() {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::error!(%error, "failed to serialise iTIP e-mail");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::models::CalendarEvent;

    fn event() -> CalendarEvent {
        let mut event = CalendarEvent::new(
            "e1",
            "c1",
            "a1",
            "uid-1",
            "Design review",
            "2026-11-12T10:00:00+01:00",
            "2026-11-12T11:00:00+01:00",
        );
        event.time_zone = "Europe/Berlin".into();
        event.organizer = Some(Attendee {
            email: "organizer@example.com".into(),
            name: Some("Organizer".into()),
            role: AttendeeRole::Chair,
            status: AttendeeStatus::Accepted,
        });
        event.attendees = vec![
            Attendee {
                email: "alice@example.com".into(),
                name: Some("Alice".into()),
                role: AttendeeRole::Required,
                status: AttendeeStatus::NeedsAction,
            },
            Attendee {
                email: "bob@example.com".into(),
                name: Some("Bob".into()),
                role: AttendeeRole::Optional,
                status: AttendeeStatus::Tentative,
            },
        ];
        event.reminders = vec![15];
        event
    }

    #[test]
    fn reply_contains_only_responder_and_comment() {
        let reply = build_reply(
            &event(),
            "alice@example.com",
            AttendeeStatus::Accepted,
            Some("Works for me"),
        );
        assert!(reply.contains("METHOD:REPLY"));
        assert!(reply.contains("ATTENDEE;CN=Alice;ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED"));
        assert!(!reply.contains("bob@example.com"));
        assert!(reply.contains("ORGANIZER;CN=Organizer:mailto:organizer@example.com"));
        assert!(reply.contains("COMMENT:Works for me"));
        assert!(!reply.contains("BEGIN:VALARM"));
        assert!(reply.ends_with("END:VCALENDAR\r\n"));
    }

    #[test]
    fn cancel_marks_status_and_sequence() {
        let cancel = build_cancel_with_sequence(&event(), 4);
        assert!(cancel.contains("METHOD:CANCEL"));
        assert!(cancel.contains("STATUS:CANCELLED"));
        assert!(cancel.contains("SEQUENCE:4"));
    }

    #[test]
    fn request_has_method_and_alarm() {
        let request = build_request(&event());
        assert!(request.contains("METHOD:REQUEST"));
        assert!(request.contains("BEGIN:VALARM"));
        assert!(request.contains("TRIGGER:-PT15M"));
    }

    #[test]
    fn email_is_multipart_with_calendar_part() {
        let from = Address::new("organizer@example.com");
        let to = vec![Address::with_name("Alice", "alice@example.com")];
        let bytes = itip_email(
            &from,
            &to,
            "Design review",
            "Please find the invite attached.",
            &build_request(&event()),
            "REQUEST",
        );
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("multipart/mixed"));
        assert!(text.contains("multipart/alternative"));
        assert!(text.contains("text/calendar"));
        assert!(text.to_ascii_lowercase().contains("method=\"request\""));
        assert!(text.contains("filename=\"invite.ics\""));
    }
}
