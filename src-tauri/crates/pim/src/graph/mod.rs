//! Microsoft Graph v1.0 calendar provider.
//!
//! Incremental sync uses `GET /me/calendars/{id}/events/delta` and follows
//! `@odata.nextLink` until `@odata.deltaLink` is returned.  We deliberately use
//! the **events** delta (not `calendarView/delta`): it returns recurring
//! masters plus detached exceptions, which matches the local model.  Expansion
//! into occurrences happens locally with `nuncio-ical`.  A `410 Gone` delta
//! token triggers a full resync.

pub mod map;

use async_trait::async_trait;
use chrono::Duration;
use nuncio_core::models::{
    Account, AttendeeStatus, CalendarEvent, EventStatus, RecurrenceEditScope, StoredEvent,
};
use reqwest::StatusCode;
use tracing::{debug, warn};

use crate::error::{invalid, AppError, Result};
use crate::http::{status_error, HttpClient};
use crate::provider::{CalendarCursor, CalendarProvider, EventDelta, EventWindow, RemoteCalendar};
use crate::util::{format_instant, parse_instant};

use self::map::{
    event_body, is_cancelled, is_exception, is_removed, map_calendar, map_event, response_body,
    series_update_body, GraphCalendar, GraphCalendarPage, GraphEvent, GraphEventPage,
};

pub const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

const EVENT_SELECT: &str = "id,iCalUId,subject,body,bodyPreview,start,end,isAllDay,location,locations,recurrence,seriesMasterId,originalStart,originalStartTimeZone,type,isCancelled,showAs,sensitivity,attendees,organizer,isOnlineMeeting,onlineMeeting,reminderMinutesBeforeStart,isReminderOn,responseStatus,lastModifiedDateTime,changeKey";

fn encode(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

fn odata_quote(value: &str) -> String {
    value.replace('\'', "''")
}

/// Low-level Graph client.
#[derive(Clone)]
pub struct GraphClient {
    http: HttpClient,
    base: String,
}

impl std::fmt::Debug for GraphClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphClient")
            .field("base", &self.base)
            .finish()
    }
}

impl GraphClient {
    pub fn new(http: HttpClient) -> Self {
        Self {
            http,
            base: GRAPH_BASE.to_string(),
        }
    }

    pub fn with_base(http: HttpClient, base: impl Into<String>) -> Self {
        Self {
            http,
            base: base.into().trim_end_matches('/').to_string(),
        }
    }

    pub fn http(&self) -> &HttpClient {
        &self.http
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    async fn failure(response: reqwest::Response) -> crate::error::AppError {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        status_error(status, &body)
    }

    /* ─────────────────────────── calendars ─────────────────────────── */

    pub async fn list_calendars(&self, account_id: &str) -> Result<Vec<RemoteCalendar>> {
        let mut calendars = Vec::new();
        let mut next: Option<String> = Some(self.url("/me/calendars?$top=100"));
        let mut guard = 0;
        while let Some(url) = next.take() {
            guard += 1;
            if guard > 50 {
                break;
            }
            let response = self
                .http
                .send_authed(
                    account_id,
                    nuncio_core::traits::TokenResource::MicrosoftGraph,
                    |token| self.http.raw().get(&url).bearer_auth(token),
                )
                .await?;
            if !response.status().is_success() {
                return Err(Self::failure(response).await);
            }
            let page: GraphCalendarPage = response
                .json()
                .await
                .map_err(|error| invalid(format!("invalid Graph calendars JSON: {error}")))?;
            calendars.extend(page.value.iter().map(map_calendar));
            next = page.next_link;
        }
        Ok(calendars)
    }

    pub async fn create_calendar(
        &self,
        account_id: &str,
        name: &str,
        color: &str,
    ) -> Result<RemoteCalendar> {
        let url = self.url("/me/calendars");
        let mut body = serde_json::json!({ "name": name });
        if let Some(color) = map::color_name(color) {
            body["color"] = serde_json::json!(color);
        }
        let response = self
            .http
            .send_authed(
                account_id,
                nuncio_core::traits::TokenResource::MicrosoftGraph,
                |token| self.http.raw().post(&url).bearer_auth(token).json(&body),
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        let created: GraphCalendar = response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid Graph calendar JSON: {error}")))?;
        Ok(map_calendar(&created))
    }

    pub async fn update_calendar(
        &self,
        account_id: &str,
        calendar_id: &str,
        name: Option<&str>,
        color: Option<&str>,
    ) -> Result<()> {
        let url = self.url(&format!("/me/calendars/{}", encode(calendar_id)));
        let mut body = serde_json::Map::new();
        if let Some(name) = name {
            body.insert("name".into(), serde_json::json!(name));
        }
        if let Some(color) = color.and_then(map::color_name) {
            body.insert("color".into(), serde_json::json!(color));
        }
        if body.is_empty() {
            return Ok(());
        }
        let body = serde_json::Value::Object(body);
        let response = self
            .http
            .send_authed(
                account_id,
                nuncio_core::traits::TokenResource::MicrosoftGraph,
                |token| self.http.raw().patch(&url).bearer_auth(token).json(&body),
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        Ok(())
    }

    pub async fn delete_calendar(&self, account_id: &str, calendar_id: &str) -> Result<()> {
        let url = self.url(&format!("/me/calendars/{}", encode(calendar_id)));
        let response = self
            .http
            .send_authed(
                account_id,
                nuncio_core::traits::TokenResource::MicrosoftGraph,
                |token| self.http.raw().delete(&url).bearer_auth(token),
            )
            .await?;
        if !response.status().is_success() && response.status() != StatusCode::NOT_FOUND {
            return Err(Self::failure(response).await);
        }
        Ok(())
    }

    /* ───────────────────────────── events ───────────────────────────── */

    #[allow(clippy::too_many_arguments)]
    pub async fn sync_events(
        &self,
        account_id: &str,
        email: &str,
        calendar_remote_id: &str,
        calendar_local_id: &str,
        cursor: &CalendarCursor,
        _window: EventWindow,
        default_reminder_minutes: i32,
    ) -> Result<EventDelta> {
        let mut delta_url = cursor.sync_token.clone();
        'restart: loop {
            let full_sync = delta_url.is_none();
            let mut delta = EventDelta::default();
            let mut next: Option<String> = Some(match &delta_url {
                Some(url) => url.clone(),
                None => self.url(&format!(
                    // No `$top`: change-tracking endpoints reject it —
                    // "The '$top' parameter is not supported with change tracking
                    // over the 'SyncEvents' resource as page size cannot be
                    // guaranteed." Page size is requested with `Prefer:
                    // odata.maxpagesize` instead (see the request below).
                    "/me/calendars/{}/events/delta?%24select={}",
                    encode(calendar_remote_id),
                    encode(EVENT_SELECT)
                )),
            });
            let mut next_delta: Option<String> = None;
            let mut guard = 0;
            let mut truncated = false;
            while let Some(url) = next.take() {
                guard += 1;
                if guard > 500 {
                    warn!(calendar_remote_id, "Graph delta pagination limit reached");
                    truncated = true;
                    break;
                }
                let response = self
                    .http
                    .send_authed(
                        account_id,
                        nuncio_core::traits::TokenResource::MicrosoftGraph,
                        |token| {
                            self.http
                                .raw()
                                .get(&url)
                                .bearer_auth(token)
                                // One header, comma separated: the timezone and
                                // the page size are both `Prefer` directives.
                                // Delta endpoints refuse `$top` — this is the
                                // supported way to influence page size.
                                .header("Prefer", "outlook.timezone=\"UTC\", odata.maxpagesize=100")
                        },
                    )
                    .await?;
                let status = response.status();
                if status == StatusCode::GONE && delta_url.is_some() {
                    debug!(
                        calendar_remote_id,
                        "Graph delta token expired; doing full resync"
                    );
                    delta_url = None;
                    continue 'restart;
                }
                if !status.is_success() {
                    return Err(Self::failure(response).await);
                }
                let page: GraphEventPage = response
                    .json()
                    .await
                    .map_err(|error| invalid(format!("invalid Graph events JSON: {error}")))?;
                for ge in &page.value {
                    if is_removed(ge) {
                        delta.deleted_remote_ids.push(ge.id.clone());
                        continue;
                    }
                    if is_cancelled(ge) && !is_exception(ge) {
                        delta.deleted_remote_ids.push(ge.id.clone());
                        continue;
                    }
                    match map_event(
                        ge,
                        email,
                        calendar_local_id,
                        account_id,
                        default_reminder_minutes,
                    ) {
                        Ok(remote) => delta.events.push(remote),
                        Err(error) => {
                            warn!(event_id = %ge.id, %error, "skipping unmappable Graph event");
                        }
                    }
                    if is_cancelled(ge) {
                        if let Some(last) = delta.events.last_mut() {
                            last.event.status = EventStatus::Cancelled;
                        }
                    }
                }
                if let Some(link) = page.next_link {
                    next = Some(link);
                } else {
                    next_delta = page.delta_link;
                }
            }
            delta.sync_token = next_delta.or(delta_url);
            // Never stale-delete from a truncated full scan.
            delta.full_sync = full_sync && !truncated;
            return Ok(delta);
        }
    }

    pub async fn create_event(
        &self,
        account_id: &str,
        calendar_remote_id: &str,
        event: &CalendarEvent,
    ) -> Result<()> {
        let url = self.url(&format!(
            "/me/calendars/{}/events",
            encode(calendar_remote_id)
        ));
        let body = event_body(event, true);
        let response = self
            .http
            .send_authed(
                account_id,
                nuncio_core::traits::TokenResource::MicrosoftGraph,
                |token| self.http.raw().post(&url).bearer_auth(token).json(&body),
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update_event(
        &self,
        account_id: &str,
        calendar_remote_id: &str,
        current: &StoredEvent,
        series: &[StoredEvent],
        updated: &CalendarEvent,
        scope: RecurrenceEditScope,
        instance_start: Option<&str>,
    ) -> Result<()> {
        match scope {
            RecurrenceEditScope::All => {
                let Some(remote_id) = current.remote_id.as_deref() else {
                    return Err(AppError::conflict("Graph event has no remote id"));
                };
                // Omit `recurrence` when the user did not change it so Graph
                // keeps the exact stored pattern instead of rebuilding a
                // lossy approximation from the structured model.
                self.patch_event(account_id, remote_id, &series_update_body(current, updated))
                    .await
            }
            RecurrenceEditScope::This => {
                let master_remote = master_remote_id(series, current)?;
                let instance_start =
                    instance_start.ok_or_else(|| invalid("instance start is required"))?;
                let instance_id = self
                    .find_instance_id(account_id, master_remote, instance_start)
                    .await?;
                match instance_id {
                    Some(instance_id) => {
                        let mut body = event_body(updated, false);
                        // An exception must not carry the master recurrence.
                        if let Some(object) = body.as_object_mut() {
                            object.remove("recurrence");
                        }
                        self.patch_event(account_id, &instance_id, &body).await
                    }
                    None => {
                        warn!(
                            master_remote,
                            instance_start, "Graph occurrence not found; patching master"
                        );
                        // An instance edit must not rewrite the master series
                        // pattern (event_body would send `recurrence: null`).
                        let mut body = event_body(updated, false);
                        if let Some(object) = body.as_object_mut() {
                            object.remove("recurrence");
                        }
                        self.patch_event(account_id, master_remote, &body).await
                    }
                }
            }
            RecurrenceEditScope::ThisAndFollowing => {
                let Some(master_remote) = current.remote_id.as_deref() else {
                    return Err(AppError::conflict("Graph recurring event has no remote id"));
                };
                let instance_start =
                    instance_start.ok_or_else(|| invalid("instance start is required"))?;
                let split =
                    nuncio_ical::recurrence::split_series_at(&current.event, instance_start)?;
                let mut body = serde_json::json!({});
                match &split.master.recurrence {
                    Some(rule) => {
                        body["recurrence"] = map::recurrence_body(
                            rule,
                            &split.master.start,
                            &split.master.time_zone,
                        );
                    }
                    None => body["recurrence"] = serde_json::Value::Null,
                }
                self.patch_event(account_id, master_remote, &body).await?;
                self.create_event(account_id, calendar_remote_id, updated)
                    .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn delete_event(
        &self,
        account_id: &str,
        _calendar_remote_id: &str,
        event: &StoredEvent,
        series: &[StoredEvent],
        scope: RecurrenceEditScope,
        instance_start: Option<&str>,
    ) -> Result<()> {
        match scope {
            RecurrenceEditScope::All => {
                let Some(remote_id) = event.remote_id.as_deref() else {
                    return Ok(());
                };
                self.delete_event_by_id(account_id, remote_id).await
            }
            RecurrenceEditScope::This => {
                let master_remote = master_remote_id(series, event)?;
                let instance_start = instance_start
                    .or(event.event.recurrence_id.as_deref())
                    .ok_or_else(|| invalid("instance start is required"))?;
                match self
                    .find_instance_id(account_id, master_remote, instance_start)
                    .await?
                {
                    Some(instance_id) => self.delete_event_by_id(account_id, &instance_id).await,
                    None => Ok(()),
                }
            }
            RecurrenceEditScope::ThisAndFollowing => {
                let Some(master_remote) = event.remote_id.as_deref() else {
                    return Ok(());
                };
                let instance_start = instance_start
                    .ok_or_else(|| invalid("instance start is required for series delete"))?;
                let split = nuncio_ical::recurrence::split_series_at(&event.event, instance_start)?;
                let mut body = serde_json::json!({});
                match &split.master.recurrence {
                    Some(rule) => {
                        body["recurrence"] = map::recurrence_body(
                            rule,
                            &split.master.start,
                            &split.master.time_zone,
                        );
                    }
                    None => body["recurrence"] = serde_json::Value::Null,
                }
                self.patch_event(account_id, master_remote, &body).await
            }
        }
    }

    pub async fn respond_event(
        &self,
        account_id: &str,
        event: &StoredEvent,
        response: AttendeeStatus,
        comment: Option<&str>,
        send_reply: bool,
    ) -> Result<()> {
        let Some(remote_id) = event.remote_id.as_deref() else {
            return Err(AppError::conflict("Graph event has no remote id"));
        };
        let action = match response {
            AttendeeStatus::Accepted => "accept",
            AttendeeStatus::Declined => "decline",
            _ => "tentativelyAccept",
        };
        let url = self.url(&format!("/me/events/{}/{}", encode(remote_id), action));
        let body = response_body(comment, send_reply);
        let response = self
            .http
            .send_authed(
                account_id,
                nuncio_core::traits::TokenResource::MicrosoftGraph,
                |token| self.http.raw().post(&url).bearer_auth(token).json(&body),
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        Ok(())
    }

    /// Find an event by iCalendar UID (meeting invitation flow).
    pub async fn find_by_ical_uid(
        &self,
        account_id: &str,
        uid: &str,
    ) -> Result<Option<GraphEvent>> {
        let filter = format!("iCalUId eq '{}'", odata_quote(uid));
        let url = format!(
            "{}/me/events?%24filter={}&%24top=5&%24select={}",
            self.base,
            encode(&filter),
            encode(EVENT_SELECT)
        );
        let response = self
            .http
            .send_authed(
                account_id,
                nuncio_core::traits::TokenResource::MicrosoftGraph,
                |token| {
                    self.http
                        .raw()
                        .get(&url)
                        .bearer_auth(token)
                        .header("Prefer", "outlook.timezone=\"UTC\"")
                },
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        let page: GraphEventPage = response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid Graph events JSON: {error}")))?;
        Ok(page.value.into_iter().next())
    }

    async fn find_instance_id(
        &self,
        account_id: &str,
        master_remote_id: &str,
        instance_start: &str,
    ) -> Result<Option<String>> {
        let instant = parse_instant(instance_start)?;
        let (start, end) = (
            format_instant(instant - Duration::days(1)),
            format_instant(instant + Duration::days(2)),
        );
        let url = format!(
            "{}/me/events/{}/instances?startDateTime={}&endDateTime={}&%24select=id,originalStart,start,end,type,seriesMasterId&%24top=50",
            self.base,
            encode(master_remote_id),
            encode(&start),
            encode(&end)
        );
        let response = self
            .http
            .send_authed(
                account_id,
                nuncio_core::traits::TokenResource::MicrosoftGraph,
                |token| {
                    self.http
                        .raw()
                        .get(&url)
                        .bearer_auth(token)
                        .header("Prefer", "outlook.timezone=\"UTC\"")
                },
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        let page: GraphEventPage = response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid Graph instances JSON: {error}")))?;
        let target = parse_instant(instance_start).ok();
        let matched = page.value.into_iter().find(|event| {
            let Some(original) = event.original_start.as_deref() else {
                return false;
            };
            match (parse_instant(original), target) {
                (Ok(original), Some(target)) => original == target,
                _ => original == instance_start,
            }
        });
        Ok(matched.map(|event| event.id))
    }

    async fn patch_event(
        &self,
        account_id: &str,
        event_id: &str,
        body: &serde_json::Value,
    ) -> Result<()> {
        let url = self.url(&format!("/me/events/{}", encode(event_id)));
        let response = self
            .http
            .send_authed(
                account_id,
                nuncio_core::traits::TokenResource::MicrosoftGraph,
                |token| self.http.raw().patch(&url).bearer_auth(token).json(body),
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        Ok(())
    }

    async fn delete_event_by_id(&self, account_id: &str, event_id: &str) -> Result<()> {
        let url = self.url(&format!("/me/events/{}", encode(event_id)));
        let response = self
            .http
            .send_authed(
                account_id,
                nuncio_core::traits::TokenResource::MicrosoftGraph,
                |token| self.http.raw().delete(&url).bearer_auth(token),
            )
            .await?;
        if !response.status().is_success() && response.status() != StatusCode::NOT_FOUND {
            return Err(Self::failure(response).await);
        }
        Ok(())
    }
}

fn master_remote_id<'a>(series: &'a [StoredEvent], current: &'a StoredEvent) -> Result<&'a str> {
    series
        .iter()
        .find(|stored| stored.event.series_id.is_none())
        .and_then(|stored| stored.remote_id.as_deref())
        .or(current.remote_id.as_deref())
        .ok_or_else(|| AppError::conflict("Graph recurring event has no remote id"))
}

/// `CalendarProvider` implementation.
pub struct GraphProvider {
    client: GraphClient,
    default_reminder_minutes: i32,
}

impl std::fmt::Debug for GraphProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphProvider").finish_non_exhaustive()
    }
}

impl GraphProvider {
    pub fn new(http: HttpClient, default_reminder_minutes: i32) -> Self {
        Self {
            client: GraphClient::new(http),
            default_reminder_minutes,
        }
    }

    pub fn with_base(
        http: HttpClient,
        base: impl Into<String>,
        default_reminder_minutes: i32,
    ) -> Self {
        Self {
            client: GraphClient::with_base(http, base),
            default_reminder_minutes,
        }
    }

    pub fn client(&self) -> &GraphClient {
        &self.client
    }
}

#[async_trait]
impl CalendarProvider for GraphProvider {
    fn name(&self) -> &'static str {
        "graph"
    }

    async fn list_calendars(&self, account: &Account) -> Result<Vec<RemoteCalendar>> {
        self.client.list_calendars(&account.id).await
    }

    async fn create_calendar(
        &self,
        account: &Account,
        name: &str,
        color: &str,
        _time_zone: &str,
    ) -> Result<RemoteCalendar> {
        self.client.create_calendar(&account.id, name, color).await
    }

    async fn update_calendar(
        &self,
        account: &Account,
        remote: &RemoteCalendar,
        name: Option<&str>,
        color: Option<&str>,
    ) -> Result<()> {
        self.client
            .update_calendar(&account.id, &remote.remote_id, name, color)
            .await
    }

    async fn delete_calendar(&self, account: &Account, remote: &RemoteCalendar) -> Result<()> {
        self.client
            .delete_calendar(&account.id, &remote.remote_id)
            .await
    }

    async fn sync_events(
        &self,
        account: &Account,
        remote: &RemoteCalendar,
        cursor: &CalendarCursor,
        window: EventWindow,
    ) -> Result<EventDelta> {
        self.client
            .sync_events(
                &account.id,
                &account.email,
                &remote.remote_id,
                &remote.local_id,
                cursor,
                window,
                self.default_reminder_minutes,
            )
            .await
    }

    async fn create_event(
        &self,
        account: &Account,
        remote: &RemoteCalendar,
        event: &CalendarEvent,
        _notify_attendees: bool,
    ) -> Result<()> {
        self.client
            .create_event(&account.id, &remote.remote_id, event)
            .await
    }

    async fn update_event(
        &self,
        account: &Account,
        remote: &RemoteCalendar,
        current: &StoredEvent,
        series: &[StoredEvent],
        updated: &CalendarEvent,
        scope: RecurrenceEditScope,
        instance_start: Option<&str>,
        _notify_attendees: bool,
    ) -> Result<()> {
        self.client
            .update_event(
                &account.id,
                &remote.remote_id,
                current,
                series,
                updated,
                scope,
                instance_start,
            )
            .await
    }

    async fn delete_event(
        &self,
        account: &Account,
        remote: &RemoteCalendar,
        event: &StoredEvent,
        series: &[StoredEvent],
        scope: RecurrenceEditScope,
        instance_start: Option<&str>,
        _notify_attendees: bool,
    ) -> Result<()> {
        self.client
            .delete_event(
                &account.id,
                &remote.remote_id,
                event,
                series,
                scope,
                instance_start,
            )
            .await
    }

    async fn respond_event(
        &self,
        account: &Account,
        _remote: &RemoteCalendar,
        event: &StoredEvent,
        _series: &[StoredEvent],
        response: AttendeeStatus,
        comment: Option<&str>,
        send_reply: bool,
    ) -> Result<()> {
        self.client
            .respond_event(&account.id, event, response, comment, send_reply)
            .await
    }
}

/// Exposed for tests: the exact event search URL shape.
pub fn ical_uid_filter(uid: &str) -> String {
    format!("iCalUId eq '{}'", odata_quote(uid))
}
