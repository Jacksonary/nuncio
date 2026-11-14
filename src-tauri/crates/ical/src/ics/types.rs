//! Types produced by [`crate::ics::parse_calendar`].

use chrono::{DateTime, Utc};
use nuncio_core::models::{
    Attendee, CalendarEvent, EventStatus, Id, Importance, RecurrenceRule, ShowAs, StoredEvent,
    Task, TaskInput, TaskStatus, TaskStep,
};

use crate::value::parse_instant;

/// A parsed iCalendar document.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedCalendar {
    /// `METHOD` from the VCALENDAR, upper-cased.
    pub method: Option<String>,
    pub events: Vec<ParsedEvent>,
    pub todos: Vec<ParsedTodo>,
}

/// A parsed VEVENT, before ids are assigned.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedEvent {
    pub uid: String,
    pub title: String,
    pub location: String,
    pub description: String,
    /// `YYYY-MM-DD` for all-day events, otherwise RFC3339 with offset.
    pub start: String,
    /// Exclusive end. Same format as `start`.
    pub end: String,
    pub all_day: bool,
    /// IANA zone, or `UTC` when the source TZID could not be mapped.
    pub time_zone: String,
    pub recurrence: Option<RecurrenceRule>,
    /// Original start for a detached occurrence (`RECURRENCE-ID`).
    pub recurrence_id: Option<String>,
    pub status: EventStatus,
    pub show_as: ShowAs,
    pub is_private: bool,
    pub organizer: Option<Attendee>,
    pub attendees: Vec<Attendee>,
    /// Minutes before start.
    pub reminders: Vec<i32>,
    pub online_meeting_url: Option<String>,
    pub sequence: u32,
    pub updated_at: Option<DateTime<Utc>>,
    /// Raw `RRULE` value (without the `RRULE:` prefix).
    pub rrule: Option<String>,
    /// EXDATE values normalised to this event's time zone / date format.
    pub exdates: Vec<String>,
    /// RDATE values normalised to this event's time zone / date format.
    pub rdates: Vec<String>,
}

impl ParsedEvent {
    pub fn is_exception(&self) -> bool {
        self.recurrence_id.is_some()
    }

    pub fn is_cancelled(&self) -> bool {
        self.status == EventStatus::Cancelled
    }

    /// Convert into the IPC model. `id` is normally [`nuncio_core::models::new_id`].
    pub fn into_event(
        self,
        id: impl Into<Id>,
        calendar_id: impl Into<Id>,
        account_id: impl Into<Id>,
    ) -> CalendarEvent {
        CalendarEvent {
            id: id.into(),
            calendar_id: calendar_id.into(),
            account_id: account_id.into(),
            uid: self.uid,
            title: self.title,
            location: self.location,
            description: self.description,
            start: self.start,
            end: self.end,
            all_day: self.all_day,
            time_zone: self.time_zone,
            recurrence: self.recurrence,
            series_id: None,
            recurrence_id: self.recurrence_id,
            status: self.status,
            show_as: self.show_as,
            is_private: self.is_private,
            organizer: self.organizer,
            attendees: self.attendees,
            reminders: self.reminders,
            is_organizer: false,
            my_response: None,
            online_meeting_url: self.online_meeting_url,
            color: None,
            updated_at: self.updated_at,
        }
    }

    /// Convert into the persistence model, keeping sync metadata.
    pub fn into_stored(
        self,
        id: impl Into<Id>,
        calendar_id: impl Into<Id>,
        account_id: impl Into<Id>,
    ) -> StoredEvent {
        let rrule = self.rrule.clone();
        let exdates = self.exdates.clone();
        StoredEvent {
            event: self.into_event(id, calendar_id, account_id),
            raw_ics: None,
            etag: None,
            remote_id: None,
            href: None,
            rrule,
            exdates,
        }
    }

    /// Re-format `start` as an instant (used for sorting/merging).
    pub fn start_instant(&self) -> Option<DateTime<Utc>> {
        parse_instant(&self.start)
    }

    /// Re-format `end` as an instant (used for sorting/merging).
    pub fn end_instant(&self) -> Option<DateTime<Utc>> {
        parse_instant(&self.end)
    }
}

/// A parsed VTODO, before ids are assigned.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedTodo {
    pub uid: String,
    pub title: String,
    pub notes: String,
    pub status: TaskStatus,
    pub importance: Importance,
    /// `YYYY-MM-DD`
    pub due_date: Option<String>,
    pub reminder_at: Option<DateTime<Utc>>,
    pub recurrence: Option<RecurrenceRule>,
    /// `YYYY-MM-DD`
    pub my_day_date: Option<String>,
    pub completed_at: Option<DateTime<Utc>>,
    pub steps: Vec<TaskStep>,
    pub sequence: u32,
    pub updated_at: Option<DateTime<Utc>>,
}

impl ParsedTodo {
    pub fn into_task_input(&self) -> TaskInput {
        TaskInput {
            title: self.title.clone(),
            notes: self.notes.clone(),
            importance: self.importance,
            due_date: self.due_date.clone(),
            reminder_at: self.reminder_at,
            recurrence: self.recurrence.clone(),
            my_day_date: self.my_day_date.clone(),
            steps: self.steps.clone(),
        }
    }

    /// Convert into the IPC model. Timestamps default to `Utc::now()` when the
    /// VTODO carried no CREATED/LAST-MODIFIED information.
    pub fn into_task(
        self,
        id: impl Into<Id>,
        list_id: impl Into<Id>,
        account_id: impl Into<Id>,
    ) -> Task {
        let now = Utc::now();
        Task {
            id: id.into(),
            list_id: list_id.into(),
            account_id: account_id.into(),
            title: self.title,
            notes: self.notes,
            status: self.status,
            importance: self.importance,
            due_date: self.due_date,
            reminder_at: self.reminder_at,
            recurrence: self.recurrence,
            my_day_date: self.my_day_date,
            completed_at: self.completed_at,
            created_at: now,
            updated_at: self.updated_at.unwrap_or(now),
            steps: self.steps,
            sort_order: 0,
            linked_message_id: None,
        }
    }
}

/// Map core `Importance` to VTODO PRIORITY.
pub(crate) fn importance_to_priority(importance: Importance) -> u8 {
    match importance {
        Importance::High => 1,
        Importance::Normal => 5,
        Importance::Low => 9,
    }
}

/// Map VTODO PRIORITY to core `Importance`.
pub(crate) fn priority_to_importance(priority: u8) -> Importance {
    match priority {
        1..=4 => Importance::High,
        6..=9 => Importance::Low,
        _ => Importance::Normal,
    }
}
