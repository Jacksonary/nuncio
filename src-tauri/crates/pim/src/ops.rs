//! Serializable payloads for the `pim.*` pending operations.
//!
//! Payloads reference local ids and embed a snapshot of the rows before the
//! optimistic change, so a permanently failed remote write can be rolled back
//! without guessing.

use nuncio_core::models::{
    Calendar, CalendarEvent, PendingOp as CorePendingOp, RecurrenceEditScope, StoredEvent,
};
use nuncio_core::repo::now_string;
use serde::{Deserialize, Serialize};

use crate::error::{internal, Result};
use crate::store::CalendarRemote;

/// Serialisable copy of [`StoredEvent`] (which deliberately has no serde impl).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventSnapshot {
    pub event: CalendarEvent,
    pub raw_ics: Option<String>,
    pub etag: Option<String>,
    pub remote_id: Option<String>,
    pub href: Option<String>,
    pub rrule: Option<String>,
    pub exdates: Vec<String>,
}

impl From<&StoredEvent> for EventSnapshot {
    fn from(stored: &StoredEvent) -> Self {
        Self {
            event: stored.event.clone(),
            raw_ics: stored.raw_ics.clone(),
            etag: stored.etag.clone(),
            remote_id: stored.remote_id.clone(),
            href: stored.href.clone(),
            rrule: stored.rrule.clone(),
            exdates: stored.exdates.clone(),
        }
    }
}

impl EventSnapshot {
    pub fn to_stored(&self) -> StoredEvent {
        StoredEvent {
            event: self.event.clone(),
            raw_ics: self.raw_ics.clone(),
            etag: self.etag.clone(),
            remote_id: self.remote_id.clone(),
            href: self.href.clone(),
            rrule: self.rrule.clone(),
            exdates: self.exdates.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarSnapshot {
    pub calendar: Calendar,
    pub remote: Option<CalendarRemote>,
    pub events: Vec<EventSnapshot>,
}

impl CalendarSnapshot {
    pub fn new(
        calendar: Calendar,
        remote: Option<CalendarRemote>,
        events: Vec<StoredEvent>,
    ) -> Self {
        Self {
            calendar,
            remote,
            events: events.iter().map(EventSnapshot::from).collect(),
        }
    }
}

/// A queued PIM write.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum PimOp {
    EventCreate {
        event_id: String,
        notify_attendees: bool,
    },
    EventUpdate {
        event_id: String,
        scope: RecurrenceEditScope,
        instance_start: Option<String>,
        notify_attendees: bool,
        snapshot: Vec<EventSnapshot>,
        /// The new tail series created by a `ThisAndFollowing` edit.
        #[serde(default)]
        following_id: Option<String>,
        /// The override created (or reused) by a `This` edit. It is a new local
        /// row when the occurrence had no override yet, so it must be part of
        /// the pending-protection set even though it is absent from the
        /// pre-edit snapshot.
        #[serde(default)]
        exception_id: Option<String>,
    },
    EventDelete {
        event_id: String,
        scope: RecurrenceEditScope,
        instance_start: Option<String>,
        notify_attendees: bool,
        snapshot: Vec<EventSnapshot>,
    },
    EventRespond {
        event_id: String,
        response: nuncio_core::models::AttendeeStatus,
        comment: Option<String>,
        send_reply: bool,
        snapshot: Vec<EventSnapshot>,
    },
    CalendarCreate {
        calendar_id: String,
    },
    CalendarUpdate {
        calendar_id: String,
        previous_name: String,
        previous_color: String,
    },
    CalendarDelete {
        calendar_id: String,
        snapshot: CalendarSnapshot,
    },
}

pub const KIND_PREFIX: &str = "pim.";

/// `true` for calendar/event operations handled by the calendar worker.
/// Contacts and tasks share the `pim.` namespace but have their own workers.
pub fn is_calendar_kind(kind: &str) -> bool {
    kind.starts_with("pim.event.") || kind.starts_with("pim.calendar.")
}

pub const KIND_EVENT_CREATE: &str = "pim.event.create";
pub const KIND_EVENT_UPDATE: &str = "pim.event.update";
pub const KIND_EVENT_DELETE: &str = "pim.event.delete";
pub const KIND_EVENT_RESPOND: &str = "pim.event.respond";
pub const KIND_CALENDAR_CREATE: &str = "pim.calendar.create";
pub const KIND_CALENDAR_UPDATE: &str = "pim.calendar.update";
pub const KIND_CALENDAR_DELETE: &str = "pim.calendar.delete";

impl PimOp {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::EventCreate { .. } => KIND_EVENT_CREATE,
            Self::EventUpdate { .. } => KIND_EVENT_UPDATE,
            Self::EventDelete { .. } => KIND_EVENT_DELETE,
            Self::EventRespond { .. } => KIND_EVENT_RESPOND,
            Self::CalendarCreate { .. } => KIND_CALENDAR_CREATE,
            Self::CalendarUpdate { .. } => KIND_CALENDAR_UPDATE,
            Self::CalendarDelete { .. } => KIND_CALENDAR_DELETE,
        }
    }

    pub fn description(&self, title: &str) -> String {
        match self {
            Self::EventCreate { .. } => format!("创建日程「{title}」"),
            Self::EventUpdate { scope, .. } => match scope {
                RecurrenceEditScope::All => format!("更新日程「{title}」"),
                RecurrenceEditScope::This => format!("更新日程「{title}」的本次事件"),
                RecurrenceEditScope::ThisAndFollowing => {
                    format!("更新日程「{title}」及后续事件")
                }
            },
            Self::EventDelete { scope, .. } => match scope {
                RecurrenceEditScope::All => format!("删除日程「{title}」"),
                RecurrenceEditScope::This => format!("删除日程「{title}」的本次事件"),
                RecurrenceEditScope::ThisAndFollowing => {
                    format!("删除日程「{title}」及后续事件")
                }
            },
            Self::EventRespond { .. } => format!("回复日程邀请「{title}」"),
            Self::CalendarCreate { .. } => "创建日历".to_string(),
            Self::CalendarUpdate { .. } => "更新日历".to_string(),
            Self::CalendarDelete { .. } => "删除日历".to_string(),
        }
    }
}

/// Decode a pending op row into a strongly typed [`PimOp`].
pub fn decode(op: &CorePendingOp) -> Result<PimOp> {
    serde_json::from_value(op.payload.clone())
        .map_err(|error| internal(format!("invalid pim pending op payload: {error}")))
}

/// Build a JSON payload.
pub fn to_value(op: &PimOp) -> Result<serde_json::Value> {
    serde_json::to_value(op)
        .map_err(|error| internal(format!("cannot encode pim pending op: {error}")))
}

/// Snapshot a series (master + detached exceptions) as payload data.
pub fn snapshots(events: &[StoredEvent]) -> Vec<EventSnapshot> {
    events.iter().map(EventSnapshot::from).collect()
}

/// Restorable stored events.
pub fn restore_events(snapshots: &[EventSnapshot]) -> Vec<StoredEvent> {
    snapshots.iter().map(EventSnapshot::to_stored).collect()
}

/// Helper for op creation timestamps in error text.
pub fn timestamp() -> String {
    now_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::models::new_id;

    #[test]
    fn calendar_worker_ignores_other_modules() {
        assert!(is_calendar_kind(KIND_EVENT_CREATE));
        assert!(is_calendar_kind(KIND_CALENDAR_DELETE));
        assert!(!is_calendar_kind("pim.contact.save"));
        assert!(!is_calendar_kind("pim.task.save"));
        assert!(!is_calendar_kind("setFlags"));
    }

    #[test]
    fn op_round_trip() {
        let event = CalendarEvent::new(
            new_id(),
            "cal",
            "acc",
            "uid",
            "Title",
            "2026-11-12T09:00:00Z",
            "2026-11-12T10:00:00Z",
        );
        let stored = StoredEvent::new(event);
        let op = PimOp::EventUpdate {
            event_id: stored.event.id.clone(),
            scope: RecurrenceEditScope::All,
            instance_start: None,
            notify_attendees: true,
            snapshot: snapshots(&[stored]),
            following_id: None,
            exception_id: None,
        };
        let value = to_value(&op).expect("encode");
        let decoded: PimOp = serde_json::from_value(value).expect("decode");
        assert_eq!(decoded.kind(), KIND_EVENT_UPDATE);
        match decoded {
            PimOp::EventUpdate { snapshot, .. } => assert_eq!(snapshot.len(), 1),
            _ => panic!("wrong variant"),
        }
    }
}
