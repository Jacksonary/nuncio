//! Fixture-driven integration tests covering the formats Nuncio meets in the
//! wild (Outlook, Google) plus round-trips and DST behaviour.

use chrono::{DateTime, Utc};
use nuncio_core::models::{
    Attendee, AttendeeRole, AttendeeStatus, CalendarEvent, ContactFieldType, EventStatus,
    Importance, RecurrenceFreq, RecurrenceRule, ShowAs, Task, TaskStatus, TaskStep, Weekday,
};
use nuncio_ical::ics::{parse_calendar, parse_todos, write_calendar, write_event, write_task};
use nuncio_ical::recurrence::{expand, expand_stored};
use nuncio_ical::vcard::{parse_vcards, write_vcard};

fn utc(year: i32, month: u32, day: u32) -> DateTime<Utc> {
    chrono::TimeZone::with_ymd_and_hms(&Utc, year, month, day, 0, 0, 0).unwrap()
}

const OUTLOOK_INVITE: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Microsoft Corporation//Outlook 16.0 MIMEDIR//EN\r\n\
METHOD:REQUEST\r\n\
BEGIN:VTIMEZONE\r\n\
TZID:Pacific Standard Time\r\n\
BEGIN:STANDARD\r\n\
DTSTART:16010101T020000\r\n\
TZOFFSETFROM:-0700\r\n\
TZOFFSETTO:-0800\r\n\
RRULE:FREQ=YEARLY;BYDAY=1SU;BYMONTH=11\r\n\
END:STANDARD\r\n\
BEGIN:DAYLIGHT\r\n\
DTSTART:16010101T020000\r\n\
TZOFFSETFROM:-0800\r\n\
TZOFFSETTO:-0700\r\n\
RRULE:FREQ=YEARLY;BYDAY=2SU;BYMONTH=3\r\n\
END:DAYLIGHT\r\n\
END:VTIMEZONE\r\n\
BEGIN:VEVENT\r\n\
UID:outlook-1@example.com\r\n\
DTSTART;TZID=\"Pacific Standard Time\":20260715T090000\r\n\
DTEND;TZID=\"Pacific Standard Time\":20260715T100000\r\n\
SUMMARY:Quarterly planning\\, Q3\r\n\
LOCATION:Seattle HQ\\; Floor 3\r\n\
DESCRIPTION:Agenda:\\n1. Roadmap\\n2. Hiring\r\n\
ORGANIZER;CN=Jane Doe:mailto:jane@example.com\r\n\
ATTENDEE;CN=Bob;ROLE=OPT-PARTICIPANT;PARTSTAT=TENTATIVE:mailto:bob@example.com\r\n\
ATTENDEE;CN=Alice;ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED:mailto:alice@example.com\r\n\
X-MICROSOFT-CDO-BUSYSTATUS:BUSY\r\n\
CLASS:PRIVATE\r\n\
BEGIN:VALARM\r\n\
ACTION:DISPLAY\r\n\
TRIGGER:-PT30M\r\n\
END:VALARM\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

#[test]
fn outlook_invite_with_windows_timezone() {
    let calendar = parse_calendar(OUTLOOK_INVITE).unwrap();
    assert_eq!(calendar.method.as_deref(), Some("REQUEST"));
    assert_eq!(calendar.events.len(), 1);
    let event = &calendar.events[0];
    assert_eq!(event.uid, "outlook-1@example.com");
    assert_eq!(event.title, "Quarterly planning, Q3");
    assert_eq!(event.location, "Seattle HQ; Floor 3");
    assert_eq!(event.description, "Agenda:\n1. Roadmap\n2. Hiring");
    assert_eq!(event.time_zone, "America/Los_Angeles");
    assert_eq!(event.start, "2026-07-15T09:00:00-07:00");
    assert_eq!(event.end, "2026-07-15T10:00:00-07:00");
    assert!(!event.all_day);
    assert!(event.is_private);
    assert_eq!(event.show_as, ShowAs::Busy);
    assert_eq!(event.reminders, vec![30]);
    assert_eq!(event.sequence, 0);

    let organizer = event.organizer.as_ref().unwrap();
    assert_eq!(organizer.email, "jane@example.com");
    assert_eq!(organizer.name.as_deref(), Some("Jane Doe"));
    assert_eq!(organizer.role, AttendeeRole::Chair);

    assert_eq!(event.attendees.len(), 2);
    let alice = event
        .attendees
        .iter()
        .find(|attendee| attendee.email == "alice@example.com")
        .unwrap();
    assert_eq!(alice.status, AttendeeStatus::Accepted);
    assert_eq!(alice.role, AttendeeRole::Required);
    let bob = event
        .attendees
        .iter()
        .find(|attendee| attendee.email == "bob@example.com")
        .unwrap();
    assert_eq!(bob.role, AttendeeRole::Optional);

    let model = event.clone().into_event("e1", "cal1", "acct1");
    assert_eq!(model.time_zone, "America/Los_Angeles");
    assert!(model.organizer.is_some());
}

const GOOGLE_INVITE: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Google Inc//Google Calendar 70.9054//EN\r\n\
CALSCALE:GREGORIAN\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
DTSTART;TZID=Asia/Shanghai:20260112T090000\r\n\
DTEND;TZID=Asia/Shanghai:20260112T093000\r\n\
RRULE:FREQ=WEEKLY;BYDAY=MO,WE;COUNT=6\r\n\
EXDATE;TZID=Asia/Shanghai:20260114T090000,20260119T090000\r\n\
DTSTAMP:20260101T000000Z\r\n\
ORGANIZER;CN=Manager:mailto:manager@example.com\r\n\
UID:google-1@google.com\r\n\
ATTENDEE;CUTYPE=INDIVIDUAL;ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED;CN=Me:mailto:me@example.com\r\n\
SUMMARY:Standup\r\n\
LOCATION:https://meet.google.com/abc-defg-hij\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
DTSTART;TZID=Asia/Shanghai:20260121T100000\r\n\
DTEND;TZID=Asia/Shanghai:20260121T103000\r\n\
RECURRENCE-ID;TZID=Asia/Shanghai:20260121T090000\r\n\
DTSTAMP:20260101T000000Z\r\n\
ORGANIZER;CN=Manager:mailto:manager@example.com\r\n\
UID:google-1@google.com\r\n\
ATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED;CN=Me:mailto:me@example.com\r\n\
SUMMARY:Standup (moved)\r\n\
STATUS:TENTATIVE\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

#[test]
fn google_invite_weekly_exdate_and_override() {
    let calendar = parse_calendar(GOOGLE_INVITE).unwrap();
    assert_eq!(calendar.events.len(), 2);
    let master = &calendar.events[0];
    assert_eq!(master.time_zone, "Asia/Shanghai");
    assert_eq!(master.start, "2026-01-12T09:00:00+08:00");
    assert_eq!(master.recurrence.as_ref().unwrap().count, Some(6));
    assert_eq!(
        master.recurrence.as_ref().unwrap().by_weekday,
        vec![Weekday::Mo, Weekday::We]
    );
    assert_eq!(master.exdates.len(), 2);
    assert_eq!(master.exdates[0], "2026-01-14T09:00:00+08:00");
    assert_eq!(
        master.online_meeting_url.as_deref(),
        Some("https://meet.google.com/abc-defg-hij")
    );

    let exception = &calendar.events[1];
    assert_eq!(exception.uid, master.uid);
    assert_eq!(
        exception.recurrence_id.as_deref(),
        Some("2026-01-21T09:00:00+08:00")
    );
    assert_eq!(exception.status, EventStatus::Tentative);

    let stored = master.clone().into_stored("master", "cal", "acct");
    let overrides = vec![exception.clone().into_event("override", "cal", "acct")];
    let occurrences = expand_stored(&stored, &overrides, utc(2026, 1, 1), utc(2026, 2, 1), 100);
    let starts: Vec<&str> = occurrences.iter().map(|o| o.start.as_str()).collect();
    assert_eq!(
        starts,
        vec![
            "2026-01-12T09:00:00+08:00",
            "2026-01-21T10:00:00+08:00",
            "2026-01-26T09:00:00+08:00",
            "2026-01-28T09:00:00+08:00",
        ]
    );
    let moved = &occurrences[1];
    assert_eq!(moved.original_start, "2026-01-21T09:00:00+08:00");
}

#[test]
fn all_day_multi_day_event() {
    let text = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:holiday\r\nDTSTART;VALUE=DATE:20261112\r\nDTEND;VALUE=DATE:20261115\r\nSUMMARY:Offsite\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let calendar = parse_calendar(text).unwrap();
    let event = &calendar.events[0];
    assert!(event.all_day);
    assert_eq!(event.start, "2026-11-12");
    assert_eq!(event.end, "2026-11-15");
    assert_eq!(event.time_zone, "UTC");

    let model = event.clone().into_event("e", "c", "a");
    let written = write_event(&model, None);
    let reparsed = parse_calendar(&written).unwrap();
    assert!(reparsed.events[0].all_day);
    assert_eq!(reparsed.events[0].start, "2026-11-12");
    assert_eq!(reparsed.events[0].end, "2026-11-15");
}

#[test]
fn monthly_second_tuesday_from_ics() {
    let text = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:review\r\nDTSTART;TZID=Europe/Berlin:20260113T140000\r\nDTEND;TZID=Europe/Berlin:20260113T150000\r\nRRULE:FREQ=MONTHLY;BYDAY=2TU;COUNT=4\r\nSUMMARY:Review\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let calendar = parse_calendar(text).unwrap();
    let rule = calendar.events[0].recurrence.as_ref().unwrap();
    assert_eq!(rule.freq, RecurrenceFreq::Monthly);
    assert_eq!(rule.by_weekday, vec![Weekday::Tu]);
    assert_eq!(rule.by_set_pos, vec![2]);

    let event = calendar.events[0].clone().into_event("e", "c", "a");
    let occurrences = expand(&event, &[], utc(2026, 1, 1), utc(2026, 6, 1), 100);
    assert_eq!(occurrences.len(), 4);
    assert_eq!(occurrences[0].start, "2026-01-13T14:00:00+01:00");
    assert_eq!(occurrences[1].start, "2026-02-10T14:00:00+01:00");
    assert_eq!(occurrences[2].start, "2026-03-10T14:00:00+01:00");
    assert_eq!(occurrences[3].start, "2026-04-14T14:00:00+02:00");
}

#[test]
fn yearly_count_and_until() {
    let count_text = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:y1\r\nDTSTART;TZID=America/New_York:20240501T100000\r\nDTEND;TZID=America/New_York:20240501T110000\r\nRRULE:FREQ=YEARLY;COUNT=3\r\nSUMMARY:Anniversary\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let calendar = parse_calendar(count_text).unwrap();
    let event = calendar.events[0].clone().into_event("e", "c", "a");
    let occurrences = expand(&event, &[], utc(2024, 1, 1), utc(2030, 1, 1), 100);
    assert_eq!(occurrences.len(), 3);
    assert_eq!(occurrences[0].start, "2024-05-01T10:00:00-04:00");
    assert_eq!(occurrences[2].start, "2026-05-01T10:00:00-04:00");

    let until_text = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:y2\r\nDTSTART:20240101T100000Z\r\nDTEND:20240101T110000Z\r\nRRULE:FREQ=YEARLY;UNTIL=20260101\r\nSUMMARY:New year\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let calendar = parse_calendar(until_text).unwrap();
    assert_eq!(
        calendar.events[0]
            .recurrence
            .as_ref()
            .unwrap()
            .until
            .as_deref(),
        Some("2026-01-01")
    );
    let event = calendar.events[0].clone().into_event("e", "c", "a");
    let occurrences = expand(&event, &[], utc(2024, 1, 1), utc(2030, 1, 1), 100);
    assert_eq!(occurrences.len(), 3);
}

#[test]
fn dst_crossing_keeps_0900_in_berlin() {
    let text = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:dst\r\nDTSTART;TZID=Europe/Berlin:20260322T090000\r\nDTEND;TZID=Europe/Berlin:20260322T100000\r\nRRULE:FREQ=WEEKLY;BYDAY=SU;COUNT=3\r\nSUMMARY:DST\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let calendar = parse_calendar(text).unwrap();
    let event = calendar.events[0].clone().into_event("e", "c", "a");
    let occurrences = expand(&event, &[], utc(2026, 3, 1), utc(2026, 5, 1), 100);
    assert_eq!(occurrences.len(), 3);
    assert_eq!(occurrences[0].start, "2026-03-22T09:00:00+01:00");
    assert_eq!(occurrences[1].start, "2026-03-29T09:00:00+02:00");
    assert_eq!(occurrences[2].start, "2026-04-05T09:00:00+02:00");
}

#[test]
fn unknown_vtimezone_tzid_uses_offsets() {
    let text = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTIMEZONE\r\nTZID:Customized Time Zone\r\nBEGIN:STANDARD\r\nDTSTART:16010101T020000\r\nTZOFFSETFROM:-0700\r\nTZOFFSETTO:-0800\r\nRRULE:FREQ=YEARLY;BYDAY=1SU;BYMONTH=11\r\nEND:STANDARD\r\nBEGIN:DAYLIGHT\r\nDTSTART:16010101T020000\r\nTZOFFSETFROM:-0800\r\nTZOFFSETTO:-0700\r\nRRULE:FREQ=YEARLY;BYDAY=2SU;BYMONTH=3\r\nEND:DAYLIGHT\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:custom\r\nDTSTART;TZID=Customized Time Zone:20260715T090000\r\nDTEND;TZID=Customized Time Zone:20260715T100000\r\nSUMMARY:Custom TZ\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let calendar = parse_calendar(text).unwrap();
    let event = &calendar.events[0];
    // July is daylight time (-07:00) -> 16:00 UTC; the model falls back to UTC.
    assert_eq!(event.time_zone, "UTC");
    assert_eq!(event.start, "2026-07-15T16:00:00+00:00");
    assert_eq!(event.end, "2026-07-15T17:00:00+00:00");
}

#[test]
fn parse_and_write_round_trip_preserves_fields() {
    let mut event = CalendarEvent::new(
        "e1",
        "cal1",
        "acct1",
        "uid-roundtrip",
        "周会 / Weekly sync",
        "2026-11-12T09:00:00+08:00",
        "2026-11-12T10:00:00+08:00",
    );
    event.time_zone = "Asia/Shanghai".into();
    event.location = "会议室 A".into();
    event.description = "第一项\n第二项; 带分号, 带逗号".into();
    event.recurrence = Some(RecurrenceRule {
        freq: RecurrenceFreq::Weekly,
        interval: 1,
        by_weekday: vec![Weekday::Th],
        by_set_pos: vec![],
        by_month_day: vec![],
        by_month: vec![],
        count: Some(5),
        until: None,
    });
    event.reminders = vec![10, 30];
    event.attendees = vec![Attendee {
        email: "a@example.com".into(),
        name: Some("张三".into()),
        role: AttendeeRole::Required,
        status: AttendeeStatus::Accepted,
    }];
    event.organizer = Some(Attendee {
        email: "owner@example.com".into(),
        name: Some("Owner".into()),
        role: AttendeeRole::Chair,
        status: AttendeeStatus::NeedsAction,
    });
    event.online_meeting_url = Some("https://teams.microsoft.com/l/meetup-join/1".into());

    let written = write_calendar(std::slice::from_ref(&event));
    assert!(written.contains("PRODID:-//Nuncio//EN"));
    assert!(written.contains("TZID=Asia/Shanghai"));
    assert!(written.contains("RRULE:FREQ=WEEKLY;BYDAY=TH;COUNT=5"));
    for line in written.split("\r\n") {
        assert!(line.len() <= 75, "line exceeds 75 octets: {line}");
    }

    let parsed = parse_calendar(&written).unwrap();
    let back = &parsed.events[0];
    assert_eq!(back.uid, "uid-roundtrip");
    assert_eq!(back.title, "周会 / Weekly sync");
    assert_eq!(back.start, "2026-11-12T09:00:00+08:00");
    assert_eq!(back.end, "2026-11-12T10:00:00+08:00");
    assert_eq!(back.description, event.description);
    assert_eq!(back.reminders, vec![10, 30]);
    assert_eq!(back.attendees[0].name.as_deref(), Some("张三"));
    assert_eq!(back.online_meeting_url, event.online_meeting_url);
}

#[test]
fn vtodo_round_trip() {
    let now = utc(2026, 11, 1);
    let task = Task {
        id: "task-1".into(),
        list_id: "list".into(),
        account_id: "acct".into(),
        title: "提交季度报告".into(),
        notes: "包含附录\n数据表".into(),
        status: TaskStatus::NotStarted,
        importance: Importance::High,
        due_date: Some("2026-11-12".into()),
        reminder_at: Some(utc(2026, 11, 12) + chrono::Duration::hours(8)),
        recurrence: Some(RecurrenceRule {
            freq: RecurrenceFreq::Monthly,
            interval: 1,
            by_weekday: vec![],
            by_set_pos: vec![],
            by_month_day: vec![12],
            by_month: vec![],
            count: None,
            until: Some("2027-12-31".into()),
        }),
        my_day_date: Some("2026-11-10".into()),
        completed_at: None,
        created_at: now,
        updated_at: now,
        steps: vec![
            TaskStep {
                id: "s1".into(),
                title: "收集数据".into(),
                is_completed: true,
                sort_order: 0,
            },
            TaskStep {
                id: "s2".into(),
                title: "撰写摘要".into(),
                is_completed: false,
                sort_order: 1,
            },
        ],
        sort_order: 0,
        linked_message_id: None,
    };

    let written = write_task(&task);
    assert!(written.contains("BEGIN:VTODO"));
    assert!(written.contains("PRIORITY:1"));
    assert!(written.contains("X-NUNCIO-MYDAY:2026-11-10"));

    let todos = parse_todos(&written).unwrap();
    assert_eq!(todos.len(), 1);
    let todo = &todos[0];
    assert_eq!(todo.uid, "task-1");
    assert_eq!(todo.title, "提交季度报告");
    assert_eq!(todo.notes, "包含附录\n数据表");
    assert_eq!(todo.importance, Importance::High);
    assert_eq!(todo.due_date.as_deref(), Some("2026-11-12"));
    assert_eq!(todo.my_day_date.as_deref(), Some("2026-11-10"));
    assert_eq!(todo.reminder_at, task.reminder_at);
    assert_eq!(todo.status, TaskStatus::NotStarted);
    assert_eq!(todo.steps.len(), 2);
    assert_eq!(todo.steps[0].title, "收集数据");
    assert!(todo.steps[0].is_completed);
    assert_eq!(todo.recurrence.as_ref().unwrap().by_month_day, vec![12]);

    let model = todo.clone().into_task("task-1", "list", "acct");
    assert_eq!(model.title, task.title);
    assert_eq!(model.importance, Importance::High);
}

#[test]
fn vtodo_completed_state() {
    let text = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:t2\r\nSUMMARY:Done\r\nSTATUS:COMPLETED\r\nCOMPLETED:20261110T120000Z\r\nPRIORITY:9\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
    let todos = parse_todos(text).unwrap();
    let todo = &todos[0];
    assert_eq!(todo.status, TaskStatus::Completed);
    assert_eq!(todo.importance, Importance::Low);
    assert_eq!(
        todo.completed_at,
        Some(
            DateTime::parse_from_rfc3339("2026-11-10T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        )
    );
}

#[test]
fn vcard_round_trip_and_types() {
    let text = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:王小明\r\nN:王;小明;;;\r\nORG:示例公司;产品部\r\nEMAIL;TYPE=HOME;PREF=1:wang@example.com\r\nEMAIL;TYPE=WORK:wang@work.example.com\r\nTEL;TYPE=FAX:010-00000000\r\nADR;TYPE=HOME:;;长安街 1 号;北京市;;100000;中国\r\nURL:https://example.com/~wang\r\nBDAY:--0305\r\nNOTE:喜欢\\, 咖啡\r\nCATEGORIES:客户,VIP\r\nEND:VCARD\r\n";
    let parsed = parse_vcards(text);
    assert_eq!(parsed.len(), 1);
    let groups = parsed[0].group_names.clone();
    let contact = parsed
        .into_iter()
        .next()
        .unwrap()
        .into_contact("c", "ab", "acct");
    assert_eq!(contact.display_name, "王小明");
    assert_eq!(contact.company, "示例公司");
    assert_eq!(contact.emails.len(), 2);
    assert_eq!(contact.emails[0].r#type, ContactFieldType::Home);
    assert!(contact.emails[0].is_primary);
    assert_eq!(contact.phones[0].r#type, ContactFieldType::Fax);
    assert_eq!(contact.birthday.as_deref(), Some("--03-05"));
    assert_eq!(contact.notes, "喜欢, 咖啡");

    let written = write_vcard(&contact, &groups);
    let reparsed = parse_vcards(&written);
    assert_eq!(reparsed[0].contact.display_name, "王小明");
    assert_eq!(reparsed[0].contact.emails.len(), 2);
    assert_eq!(reparsed[0].group_names, vec!["客户", "VIP"]);
    assert_eq!(reparsed[0].contact.birthday.as_deref(), Some("--03-05"));
}

#[test]
fn vcard_type_pref_marks_primary() {
    let text = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Pref\r\nEMAIL;TYPE=INTERNET,pref:first@example.com\r\nEMAIL;TYPE=WORK:second@example.com\r\nEND:VCARD\r\n";
    let cards = parse_vcards(text);
    assert!(cards[0].contact.emails[0].is_primary);
    assert!(!cards[0].contact.emails[1].is_primary);
}

#[test]
fn vtodo_relative_alarm() {
    let text = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:t3\r\nSUMMARY:Call\r\nDUE:2026-11-12T15:00:00Z\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
    let todos = parse_todos(text).unwrap();
    assert_eq!(
        todos[0].reminder_at,
        Some(
            DateTime::parse_from_rfc3339("2026-11-12T14:45:00Z")
                .unwrap()
                .with_timezone(&Utc)
        )
    );
}

#[test]
fn rdate_is_expanded() {
    let text = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:rdate\r\nDTSTART;TZID=Europe/Berlin:20260105T090000\r\nDTEND;TZID=Europe/Berlin:20260105T100000\r\nRRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=2\r\nRDATE;TZID=Europe/Berlin:20260108T090000,20260109T090000\r\nSUMMARY:Extra\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let calendar = parse_calendar(text).unwrap();
    let event = calendar.events[0].clone().into_event("e", "c", "a");
    let exdates = Vec::new();
    let rdates = calendar.events[0].rdates.clone();
    let occurrences = nuncio_ical::recurrence::expand_with_rdates(
        &event,
        &exdates,
        &rdates,
        &[],
        utc(2026, 1, 1),
        utc(2026, 2, 1),
        100,
    );
    let starts: Vec<&str> = occurrences.iter().map(|o| o.start.as_str()).collect();
    assert!(starts.contains(&"2026-01-05T09:00:00+01:00"));
    assert!(starts.contains(&"2026-01-08T09:00:00+01:00"));
    assert!(starts.contains(&"2026-01-09T09:00:00+01:00"));
    assert!(starts.contains(&"2026-01-12T09:00:00+01:00"));
}

#[test]
fn dtend_with_own_timezone() {
    let text = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:tzend\r\nDTSTART;TZID=Europe/Berlin:20260105T090000\r\nDTEND;TZID=America/New_York:20260105T040000\r\nSUMMARY:Cross TZ\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let calendar = parse_calendar(text).unwrap();
    // 04:00 New York (EST) == 10:00 Berlin (CET).
    assert_eq!(calendar.events[0].end, "2026-01-05T10:00:00+01:00");
}

#[test]
fn write_event_with_method() {
    let event = CalendarEvent::new(
        "e",
        "c",
        "a",
        "uid",
        "Title",
        "2026-01-01T09:00:00+00:00",
        "2026-01-01T10:00:00+00:00",
    );
    let written = write_event(&event, Some("request"));
    assert!(written.contains("METHOD:REQUEST"));
}

#[test]
fn invalid_calendar_reports_error() {
    assert!(parse_calendar("not a calendar").is_err());
}

#[test]
fn timed_until_is_utc_and_round_trips() {
    let mut event = CalendarEvent::new(
        "e",
        "c",
        "a",
        "uid-until",
        "Yearly",
        "2026-01-15T09:00:00+01:00",
        "2026-01-15T10:00:00+01:00",
    );
    event.time_zone = "Europe/Berlin".into();
    event.recurrence = Some(RecurrenceRule {
        freq: RecurrenceFreq::Yearly,
        interval: 1,
        by_weekday: vec![],
        by_set_pos: vec![],
        by_month_day: vec![],
        by_month: vec![],
        count: None,
        until: Some("2028-01-15".into()),
    });
    let written = write_event(&event, None);
    // 23:59:59 Berlin (CET, +01:00) == 22:59:59Z.
    assert!(written.contains("UNTIL=20280115T225959Z"));
    let parsed = parse_calendar(&written).unwrap();
    assert_eq!(
        parsed.events[0]
            .recurrence
            .as_ref()
            .unwrap()
            .until
            .as_deref(),
        Some("2028-01-15")
    );
}

#[test]
fn all_day_until_stays_date() {
    let mut event = CalendarEvent::new(
        "e",
        "c",
        "a",
        "uid-ad",
        "Holiday",
        "2026-01-01",
        "2026-01-02",
    );
    event.all_day = true;
    event.recurrence = Some(RecurrenceRule {
        freq: RecurrenceFreq::Yearly,
        interval: 1,
        by_weekday: vec![],
        by_set_pos: vec![],
        by_month_day: vec![],
        by_month: vec![],
        count: None,
        until: Some("2028-01-01".into()),
    });
    let written = write_event(&event, None);
    assert!(written.contains("UNTIL=20280101"));
    let parsed = parse_calendar(&written).unwrap();
    assert_eq!(
        parsed.events[0]
            .recurrence
            .as_ref()
            .unwrap()
            .until
            .as_deref(),
        Some("2028-01-01")
    );
}

#[test]
fn duration_end_and_free_transparency() {
    let text = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:dur\r\nDTSTART;TZID=Asia/Shanghai:20261112T140000\r\nDURATION:PT1H30M\r\nTRANSP:TRANSPARENT\r\nSEQUENCE:3\r\nLAST-MODIFIED:20261101T030405Z\r\nSUMMARY:Optional\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let calendar = parse_calendar(text).unwrap();
    let event = &calendar.events[0];
    assert_eq!(event.start, "2026-11-12T14:00:00+08:00");
    assert_eq!(event.end, "2026-11-12T15:30:00+08:00");
    assert_eq!(event.show_as, ShowAs::Free);
    assert_eq!(event.sequence, 3);
    assert_eq!(
        event.updated_at,
        Some(
            DateTime::parse_from_rfc3339("2026-11-01T03:04:05Z")
                .unwrap()
                .with_timezone(&Utc)
        )
    );
}

#[test]
fn all_day_duration() {
    let text = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:ad-dur\r\nDTSTART;VALUE=DATE:20261112\r\nDURATION:P3D\r\nSUMMARY:Trip\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let calendar = parse_calendar(text).unwrap();
    let event = &calendar.events[0];
    assert!(event.all_day);
    assert_eq!(event.start, "2026-11-12");
    assert_eq!(event.end, "2026-11-15");
}

#[test]
fn cancelled_override_removes_occurrence() {
    let master = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:cancel\r\nDTSTART;TZID=Asia/Shanghai:20260105T090000\r\nDTEND;TZID=Asia/Shanghai:20260105T100000\r\nRRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=3\r\nSUMMARY:Series\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let calendar = parse_calendar(master).unwrap();
    let event = calendar.events[0].clone().into_event("e", "c", "a");
    let mut cancelled = event.clone();
    cancelled.start = "2026-01-12T09:00:00+08:00".into();
    cancelled.end = "2026-01-12T10:00:00+08:00".into();
    cancelled.recurrence_id = Some("2026-01-12T09:00:00+08:00".into());
    cancelled.status = EventStatus::Cancelled;
    let occurrences = expand(&event, &[cancelled], utc(2026, 1, 1), utc(2026, 2, 1), 100);
    assert_eq!(occurrences.len(), 2);
    assert_eq!(occurrences[0].start, "2026-01-05T09:00:00+08:00");
    assert_eq!(occurrences[1].start, "2026-01-19T09:00:00+08:00");
}
