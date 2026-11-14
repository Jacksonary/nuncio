//! Integration tests for the calendar command facade (local account).

use std::sync::Arc;

use nuncio_core::models::{
    new_id, Attendee, AttendeeStatus, EventInput, RecurrenceEditScope, RecurrenceFreq,
    RecurrenceRule, ShowAs, Weekday,
};
use nuncio_core::repo;
use nuncio_core::traits::testing::NoopMailSender;
use nuncio_core::{CoreContext, Paths};
use nuncio_pim::PimService;

async fn service() -> (CoreContext, PimService) {
    let dir = std::env::temp_dir().join(format!("nuncio-pim-service-{}", new_id()));
    let ctx = CoreContext::for_test(Paths::new(dir)).await.expect("ctx");
    let service = PimService::new(ctx.clone(), Arc::new(NoopMailSender)).expect("service");
    (ctx, service)
}

fn weekly_input(title: &str, start: &str, end: &str) -> EventInput {
    EventInput {
        calendar_id: "local-calendar".into(),
        title: title.into(),
        location: String::new(),
        description: String::new(),
        start: start.into(),
        end: end.into(),
        all_day: false,
        time_zone: "UTC".into(),
        recurrence: Some(RecurrenceRule {
            freq: RecurrenceFreq::Weekly,
            interval: 1,
            by_weekday: vec![Weekday::Th],
            ..Default::default()
        }),
        show_as: ShowAs::Busy,
        is_private: false,
        attendees: Vec::new(),
        reminders: vec![15],
        online_meeting_url: None,
        color: None,
        notify_attendees: false,
    }
}

fn daily_input(title: &str, start: &str, end: &str) -> EventInput {
    EventInput {
        recurrence: Some(RecurrenceRule {
            freq: RecurrenceFreq::Daily,
            interval: 1,
            ..Default::default()
        }),
        ..weekly_input(title, start, end)
    }
}

#[tokio::test]
async fn expands_recurring_events_and_applies_single_occurrence_edits() {
    let (_ctx, service) = service().await;
    let master = service
        .event_create(weekly_input(
            "Standup",
            "2026-11-12T09:00:00Z",
            "2026-11-12T09:30:00Z",
        ))
        .await
        .expect("create");

    let range = service
        .event_range("2026-11-12T00:00:00Z", "2026-11-30T00:00:00Z", None)
        .await
        .expect("range");
    assert_eq!(range.len(), 3, "three Thursdays");
    assert_eq!(range[0].start, "2026-11-12T09:00:00+00:00");
    assert!(range[0].is_recurring);

    // Move only 19 Nov to 10:00.
    let mut edit = weekly_input(
        "Standup moved",
        "2026-11-19T10:00:00Z",
        "2026-11-19T10:30:00Z",
    );
    edit.recurrence = None;
    service
        .event_update(
            &master.id,
            edit,
            RecurrenceEditScope::This,
            Some("2026-11-19T09:00:00Z"),
        )
        .await
        .expect("edit occurrence");

    let range = service
        .event_range("2026-11-12T00:00:00Z", "2026-11-30T00:00:00Z", None)
        .await
        .expect("range");
    assert_eq!(range.len(), 3);
    let moved = range
        .iter()
        .find(|instance| instance.start.starts_with("2026-11-19"))
        .expect("moved instance");
    assert_eq!(moved.start, "2026-11-19T10:00:00Z");
    assert_eq!(moved.title, "Standup moved");

    // Delete the moved occurrence; it disappears from the range.
    service
        .event_delete(
            &master.id,
            RecurrenceEditScope::This,
            Some("2026-11-19T09:00:00Z"),
            false,
        )
        .await
        .expect("delete occurrence");
    let range = service
        .event_range("2026-11-12T00:00:00Z", "2026-11-30T00:00:00Z", None)
        .await
        .expect("range");
    assert_eq!(range.len(), 2);
}

#[tokio::test]
async fn this_and_following_splits_the_series() {
    let (_ctx, service) = service().await;
    let master = service
        .event_create(daily_input(
            "Old",
            "2026-12-01T09:00:00Z",
            "2026-12-01T09:30:00Z",
        ))
        .await
        .expect("create");

    let mut edit = daily_input("New", "2026-12-03T10:00:00Z", "2026-12-03T10:30:00Z");
    edit.reminders = vec![5];
    service
        .event_update(
            &master.id,
            edit,
            RecurrenceEditScope::ThisAndFollowing,
            Some("2026-12-03T09:00:00Z"),
        )
        .await
        .expect("split series");

    let range = service
        .event_range("2026-12-01T00:00:00Z", "2026-12-06T00:00:00Z", None)
        .await
        .expect("range");
    assert_eq!(range.len(), 5);
    let old: Vec<_> = range
        .iter()
        .filter(|instance| instance.title == "Old")
        .collect();
    let new: Vec<_> = range
        .iter()
        .filter(|instance| instance.title == "New")
        .collect();
    assert_eq!(old.len(), 2);
    assert_eq!(new.len(), 3);
    assert_eq!(new[0].start, "2026-12-03T10:00:00+00:00");

    // The occurrence view returns the concrete start for the requested date.
    let view = service
        .event_get(&master.id, Some("2026-12-01T09:00:00Z"))
        .await
        .expect("view");
    assert_eq!(view.title, "Old");
    assert!(view.start.starts_with("2026-12-01T09:00:00"));
}

#[tokio::test]
async fn moving_an_occurrence_is_visible_to_event_get_and_keeps_edits() {
    let (_ctx, service) = service().await;
    let master = service
        .event_create(weekly_input(
            "Standup",
            "2026-11-12T09:00:00Z",
            "2026-11-12T09:30:00Z",
        ))
        .await
        .expect("create");

    let mut edit = weekly_input(
        "Standup moved",
        "2026-11-19T10:00:00Z",
        "2026-11-19T10:30:00Z",
    );
    edit.recurrence = None;
    service
        .event_update(
            &master.id,
            edit,
            RecurrenceEditScope::This,
            Some("2026-11-19T09:00:00Z"),
        )
        .await
        .expect("move occurrence");

    let range = service
        .event_range("2026-11-12T00:00:00Z", "2026-11-30T00:00:00Z", None)
        .await
        .expect("range");
    let moved_start = range
        .iter()
        .find(|instance| instance.title == "Standup moved")
        .expect("moved instance")
        .start
        .clone();

    // The peek opens the occurrence with its *new* start, not the original
    // RECURRENCE-ID.  It must still return the override, not master fields.
    let peek = service
        .event_get(&master.id, Some(&moved_start))
        .await
        .expect("get moved occurrence");
    assert_eq!(peek.title, "Standup moved");
    assert_eq!(peek.recurrence_id.as_deref(), Some("2026-11-19T09:00:00Z"));

    // The original start keeps resolving to the same override.
    let by_original = service
        .event_get(&master.id, Some("2026-11-19T09:00:00Z"))
        .await
        .expect("get by original start");
    assert_eq!(by_original.title, "Standup moved");

    // Editing "this event" from the moved view updates the same exception
    // instead of creating another one or overwriting it with master fields.
    let mut second = weekly_input(
        "Standup moved again",
        "2026-11-19T11:00:00Z",
        "2026-11-19T11:30:00Z",
    );
    second.recurrence = None;
    service
        .event_update(
            &master.id,
            second,
            RecurrenceEditScope::This,
            Some(&peek.start),
        )
        .await
        .expect("edit moved occurrence");
    let range = service
        .event_range("2026-11-12T00:00:00Z", "2026-11-30T00:00:00Z", None)
        .await
        .expect("range");
    let matches: Vec<_> = range
        .iter()
        .filter(|instance| instance.title.starts_with("Standup"))
        .collect();
    assert_eq!(matches.len(), 3, "no duplicate exception was created");
    assert!(matches
        .iter()
        .any(|instance| instance.title == "Standup moved again"));
}

#[tokio::test]
async fn editing_all_from_a_later_occurrence_keeps_earlier_occurrences() {
    let (_ctx, service) = service().await;
    let master = service
        .event_create(weekly_input(
            "Sync",
            "2026-11-12T09:00:00Z",
            "2026-11-12T09:30:00Z",
        ))
        .await
        .expect("create");

    // The editor opened the third occurrence (26 Nov) and chose "all events".
    let edit = weekly_input(
        "Sync changed",
        "2026-11-26T10:00:00Z",
        "2026-11-26T10:30:00Z",
    );
    service
        .event_update(&master.id, edit, RecurrenceEditScope::All, None)
        .await
        .expect("update all");

    let range = service
        .event_range("2026-11-12T00:00:00Z", "2026-11-30T00:00:00Z", None)
        .await
        .expect("range");
    assert_eq!(range.len(), 3, "series start must not move");
    assert_eq!(range[0].start, "2026-11-12T10:00:00+00:00");
    assert_eq!(range[1].start, "2026-11-19T10:00:00+00:00");
    assert_eq!(range[2].start, "2026-11-26T10:00:00+00:00");
    assert!(range
        .iter()
        .all(|instance| instance.title == "Sync changed"));
}

#[tokio::test]
async fn all_day_events_and_search() {
    let (_ctx, service) = service().await;
    let mut input = weekly_input("Company holiday", "2026-12-25", "2026-12-26");
    input.all_day = true;
    input.recurrence = None;
    input.reminders = Vec::new();
    service.event_create(input).await.expect("create");

    let range = service
        .event_range("2026-12-24T00:00:00Z", "2026-12-27T00:00:00Z", None)
        .await
        .expect("range");
    assert_eq!(range.len(), 1);
    assert!(range[0].all_day);

    let found = service.event_search("holiday", 10).await.expect("search");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].title, "Company holiday");
}

#[tokio::test]
async fn responding_updates_my_status() {
    let (_ctx, service) = service().await;
    let organizer = Attendee {
        email: "boss@example.com".into(),
        name: Some("Boss".into()),
        role: nuncio_core::models::AttendeeRole::Chair,
        status: AttendeeStatus::Accepted,
    };
    let me = Attendee {
        email: "local".into(),
        name: None,
        role: nuncio_core::models::AttendeeRole::Required,
        status: AttendeeStatus::NeedsAction,
    };
    let mut input = weekly_input("Review", "2026-11-12T09:00:00Z", "2026-11-12T10:00:00Z");
    input.recurrence = None;
    input.attendees = vec![organizer, me];
    let event = service.event_create(input).await.expect("create");

    service
        .event_respond(&event.id, AttendeeStatus::Accepted, Some("ok"), false)
        .await
        .expect("respond");
    let updated = service.event_get(&event.id, None).await.expect("get");
    assert_eq!(updated.my_response, Some(AttendeeStatus::Accepted));
    // The local account has no e-mail address, so only `myResponse` changes.
    assert_eq!(updated.attendees.len(), 2);
}

#[tokio::test]
async fn calendar_crud_is_local() {
    let (ctx, service) = service().await;
    let calendar = service
        .calendar_create("local", "Work", "#112233")
        .await
        .expect("create calendar");
    assert_eq!(calendar.color, "#112233");
    service
        .calendar_set_visible(&calendar.id, false)
        .await
        .expect("hide");
    let listed = service.calendar_list().await.expect("list");
    let found = listed
        .iter()
        .find(|item| item.id == calendar.id)
        .expect("calendar");
    assert!(!found.is_visible);

    service
        .calendar_update(&calendar.id, Some("Work 2"), Some("#AABBCC"))
        .await
        .expect("update");
    service.calendar_delete(&calendar.id).await.expect("delete");
    assert!(
        repo::calendars::get(&ctx.db.connection().expect("conn"), &calendar.id)
            .expect("query")
            .is_none()
    );
}
