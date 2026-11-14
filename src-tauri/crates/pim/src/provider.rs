//! Reusable provider abstraction for calendar backends.
//!
//! Stage 1 of `nuncio-pim` implements Google Calendar, Microsoft Graph and
//! CalDAV.  Stage 2 adds CardDAV and task providers on top of the same
//! infrastructure: [`crate::http::HttpClient`], this trait's delta/cursor
//! shapes, and the DAV client's generic `multistatus` parsing.
//!
//! All methods take the core contract [`Account`] so the providers stay
//! stateless and cloneable; the PIM service constructs one per operation.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nuncio_core::models::{
    Account, AttendeeStatus, CalendarEvent, RecurrenceEditScope, StoredEvent,
};

use crate::error::Result;

/// A provider-side calendar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCalendar {
    /// Local `calendar.id`; filled in by the sync engine.
    pub local_id: String,
    /// Provider identifier (Google calendar id, Graph calendar id, absolute
    /// CalDAV collection URL).
    pub remote_id: String,
    /// Collection URL for DAV providers (same as `remote_id` there).
    pub href: Option<String>,
    pub name: String,
    pub color: Option<String>,
    pub read_only: bool,
    pub sync_token: Option<String>,
    pub ctag: Option<String>,
    pub supports_sync_collection: bool,
}

impl RemoteCalendar {
    pub fn new(remote_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            local_id: String::new(),
            remote_id: remote_id.into(),
            href: None,
            name: name.into(),
            color: None,
            read_only: false,
            sync_token: None,
            ctag: None,
            supports_sync_collection: false,
        }
    }
}

/// Incremental sync cursor for a calendar.
#[derive(Debug, Clone, Default)]
pub struct CalendarCursor {
    /// Local `calendar.id` (used when mapping fetched resources).
    pub local_calendar_id: String,
    /// Google `nextSyncToken`, Graph `@odata.deltaLink` or DAV sync token.
    pub sync_token: Option<String>,
    /// DAV `getctag` (fallback when sync-collection is unavailable).
    pub ctag: Option<String>,
    /// Local `(href, etag)` pairs used by the DAV ctag fallback.
    pub etags: Vec<(String, String)>,
}

/// Time window for the initial (full) sync.
#[derive(Debug, Clone, Copy)]
pub struct EventWindow {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl EventWindow {
    /// Default window used for full calendar syncs: one year back, two ahead.
    pub fn default_window(now: DateTime<Utc>) -> Self {
        Self {
            start: now - chrono::Duration::days(365),
            end: now + chrono::Duration::days(730),
        }
    }
}

/// One event returned by a provider, with sync metadata.
#[derive(Debug, Clone)]
pub struct RemoteEvent {
    pub event: CalendarEvent,
    pub remote_id: Option<String>,
    pub href: Option<String>,
    pub etag: Option<String>,
    /// Raw RRULE value (without the `RRULE:` prefix).
    pub rrule: Option<String>,
    pub exdates: Vec<String>,
    /// Raw iCalendar resource for DAV; `None` for JSON providers.
    pub raw_ics: Option<String>,
    /// Provider id of the recurring series master, for detached exceptions.
    pub series_remote_id: Option<String>,
}

impl RemoteEvent {
    pub fn from_stored(stored: &StoredEvent, series_remote_id: Option<String>) -> Self {
        Self {
            event: stored.event.clone(),
            remote_id: stored.remote_id.clone(),
            href: stored.href.clone(),
            etag: stored.etag.clone(),
            rrule: stored.rrule.clone(),
            exdates: stored.exdates.clone(),
            raw_ics: stored.raw_ics.clone(),
            series_remote_id,
        }
    }
}

/// A batch of remote changes.
#[derive(Debug, Clone, Default)]
pub struct EventDelta {
    pub events: Vec<RemoteEvent>,
    /// Provider ids that were deleted entirely.
    pub deleted_remote_ids: Vec<String>,
    /// DAV hrefs that were deleted.
    pub deleted_hrefs: Vec<String>,
    pub sync_token: Option<String>,
    pub ctag: Option<String>,
    /// `true` when the provider performed a full resync: the sync engine then
    /// removes local provider events that were not seen in this batch.
    pub full_sync: bool,
}

/// Calendar backend.  Implementations are cheap to construct.
#[async_trait]
pub trait CalendarProvider: Send + Sync {
    /// Stable provider key stored in `pim_calendar_map.provider`.
    fn name(&self) -> &'static str;

    async fn list_calendars(&self, account: &Account) -> Result<Vec<RemoteCalendar>>;

    async fn create_calendar(
        &self,
        account: &Account,
        name: &str,
        color: &str,
        time_zone: &str,
    ) -> Result<RemoteCalendar>;

    async fn update_calendar(
        &self,
        account: &Account,
        remote: &RemoteCalendar,
        name: Option<&str>,
        color: Option<&str>,
    ) -> Result<()>;

    async fn delete_calendar(&self, account: &Account, remote: &RemoteCalendar) -> Result<()>;

    async fn sync_events(
        &self,
        account: &Account,
        remote: &RemoteCalendar,
        cursor: &CalendarCursor,
        window: EventWindow,
    ) -> Result<EventDelta>;

    async fn create_event(
        &self,
        account: &Account,
        remote: &RemoteCalendar,
        event: &CalendarEvent,
        notify_attendees: bool,
    ) -> Result<()>;

    /// Update a master (`scope = All`), a single occurrence (`This`) or a
    /// series split (`ThisAndFollowing`).
    ///
    /// `current` is the stored master for `All`/`ThisAndFollowing` and the
    /// affected occurrence for `This`.  `series` contains every event with the
    /// same UID (master + detached exceptions) so DAV can rewrite one resource.
    #[allow(clippy::too_many_arguments)]
    async fn update_event(
        &self,
        account: &Account,
        remote: &RemoteCalendar,
        current: &StoredEvent,
        series: &[StoredEvent],
        updated: &CalendarEvent,
        scope: RecurrenceEditScope,
        instance_start: Option<&str>,
        notify_attendees: bool,
    ) -> Result<()>;

    #[allow(clippy::too_many_arguments)]
    async fn delete_event(
        &self,
        account: &Account,
        remote: &RemoteCalendar,
        event: &StoredEvent,
        series: &[StoredEvent],
        scope: RecurrenceEditScope,
        instance_start: Option<&str>,
        notify_attendees: bool,
    ) -> Result<()>;

    #[allow(clippy::too_many_arguments)]
    async fn respond_event(
        &self,
        account: &Account,
        remote: &RemoteCalendar,
        event: &StoredEvent,
        series: &[StoredEvent],
        response: AttendeeStatus,
        comment: Option<&str>,
        send_reply: bool,
    ) -> Result<()>;
}
