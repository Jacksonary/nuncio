//! Google Calendar API v3 provider.
//!
//! Incremental sync uses `events.list` with `syncToken`; a `410 Gone` response
//! triggers the documented full resync over `[now - 1y, now + 2y]`.
//! `singleEvents=false` keeps recurring masters and detached exceptions in the
//! feed, matching the database model (expansion happens locally).

pub mod map;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use nuncio_core::models::{
    Account, Attendee, AttendeeStatus, CalendarEvent, EventStatus, RecurrenceEditScope, StoredEvent,
};
use reqwest::StatusCode;
use tracing::{debug, warn};

use crate::error::{invalid, AppError, Result};
use crate::http::{status_error, HttpClient};
use crate::provider::{CalendarCursor, CalendarProvider, EventDelta, EventWindow, RemoteCalendar};
use crate::util::{format_instant, normalize_color, parse_instant};

use self::map::{
    attendee_status_name, event_body, is_cancelled_master, is_cancelled_override, map_calendar,
    map_event, GCalendarList, GEvent, GEventList,
};

pub const GOOGLE_BASE: &str = "https://www.googleapis.com/calendar/v3";

fn encode(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

/// Low-level Google Calendar client.  Reusable by stage 2 (People/Tasks APIs
/// share the same bearer-token infrastructure).
#[derive(Clone)]
pub struct GoogleClient {
    http: HttpClient,
    base: String,
}

impl std::fmt::Debug for GoogleClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleClient")
            .field("base", &self.base)
            .finish()
    }
}

impl GoogleClient {
    pub fn new(http: HttpClient) -> Self {
        Self {
            http,
            base: GOOGLE_BASE.to_string(),
        }
    }

    /// Test / staging base URL override.
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
        let mut items = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut query: Vec<(String, String)> = vec![("maxResults".into(), "250".into())];
            if let Some(token) = &page_token {
                query.push(("pageToken".into(), token.clone()));
            }
            let url = self.url("/users/me/calendarList");
            let response = self
                .http
                .send_authed(
                    account_id,
                    nuncio_core::traits::TokenResource::GoogleApis,
                    |token| self.http.raw().get(&url).bearer_auth(token).query(&query),
                )
                .await?;
            if !response.status().is_success() {
                return Err(Self::failure(response).await);
            }
            let page: GCalendarList = response
                .json()
                .await
                .map_err(|error| invalid(format!("invalid Google calendarList JSON: {error}")))?;
            for entry in page
                .items
                .iter()
                .filter(|entry| entry.deleted != Some(true))
            {
                items.push(map_calendar(entry));
            }
            match page.next_page_token {
                Some(token) if !token.is_empty() => page_token = Some(token),
                _ => break,
            }
        }
        Ok(items)
    }

    pub async fn create_calendar(
        &self,
        account_id: &str,
        name: &str,
        color: &str,
        time_zone: &str,
    ) -> Result<RemoteCalendar> {
        let url = self.url("/calendars");
        let body = serde_json::json!({
            "summary": name,
            "timeZone": crate::util::normalize_zone(time_zone),
        });
        let response = self
            .http
            .send_authed(
                account_id,
                nuncio_core::traits::TokenResource::GoogleApis,
                |token| self.http.raw().post(&url).bearer_auth(token).json(&body),
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        let created: GEventLikeCalendar = response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid Google calendar JSON: {error}")))?;
        let color = normalize_color(color).unwrap_or_else(|| "#0078D4".to_string());
        // `backgroundColor` only exists on the calendarList entry.
        if let Err(error) = self
            .patch_calendar_color(account_id, &created.id, &color)
            .await
        {
            warn!(%error, "could not set Google calendar color");
        }
        Ok(RemoteCalendar {
            local_id: String::new(),
            remote_id: created.id,
            href: None,
            name: created.summary.unwrap_or_else(|| name.to_string()),
            color: Some(color),
            read_only: false,
            sync_token: None,
            ctag: None,
            supports_sync_collection: false,
        })
    }

    async fn patch_calendar_color(
        &self,
        account_id: &str,
        calendar_id: &str,
        color: &str,
    ) -> Result<()> {
        let url = self.url(&format!("/users/me/calendarList/{}", encode(calendar_id)));
        let body = serde_json::json!({
            "backgroundColor": color,
            "foregroundColor": contrast_color(color),
        });
        let response = self
            .http
            .send_authed(
                account_id,
                nuncio_core::traits::TokenResource::GoogleApis,
                |token| self.http.raw().patch(&url).bearer_auth(token).json(&body),
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        Ok(())
    }

    pub async fn update_calendar(
        &self,
        account_id: &str,
        calendar_id: &str,
        name: Option<&str>,
        color: Option<&str>,
    ) -> Result<()> {
        if let Some(name) = name {
            let url = self.url(&format!("/calendars/{}", encode(calendar_id)));
            let body = serde_json::json!({ "summary": name });
            let response = self
                .http
                .send_authed(
                    account_id,
                    nuncio_core::traits::TokenResource::GoogleApis,
                    |token| self.http.raw().patch(&url).bearer_auth(token).json(&body),
                )
                .await?;
            if !response.status().is_success() {
                return Err(Self::failure(response).await);
            }
        }
        if let Some(color) = color {
            if let Some(color) = normalize_color(color) {
                self.patch_calendar_color(account_id, calendar_id, &color)
                    .await?;
            }
        }
        Ok(())
    }

    pub async fn delete_calendar(
        &self,
        account_id: &str,
        calendar_id: &str,
        unsubscribe_only: bool,
    ) -> Result<()> {
        let path = if unsubscribe_only {
            format!("/users/me/calendarList/{}", encode(calendar_id))
        } else {
            format!("/calendars/{}", encode(calendar_id))
        };
        let url = self.url(&path);
        let response = self
            .http
            .send_authed(
                account_id,
                nuncio_core::traits::TokenResource::GoogleApis,
                |token| self.http.raw().delete(&url).bearer_auth(token),
            )
            .await?;
        if !response.status().is_success() && response.status() != StatusCode::NOT_FOUND {
            return Err(Self::failure(response).await);
        }
        Ok(())
    }

    /* ───────────────────────────── events ───────────────────────────── */

    /// Incremental event sync.  Returns masters + exceptions with a fresh
    /// `syncToken`; a 410 response transparently falls back to a full sync.
    #[allow(clippy::too_many_arguments)]
    pub async fn sync_events(
        &self,
        account_id: &str,
        email: &str,
        calendar_remote_id: &str,
        calendar_local_id: &str,
        cursor: &CalendarCursor,
        window: EventWindow,
        default_reminder_minutes: i32,
    ) -> Result<EventDelta> {
        let mut sync_token = cursor.sync_token.clone();
        'restart: loop {
            let full_sync = sync_token.is_none();
            let mut delta = EventDelta::default();
            let mut page_token: Option<String> = None;
            let next_sync_token: Option<String> = loop {
                // Google forbids combining `syncToken` with most filters;
                // `singleEvents` defaults to false, which is what we want.
                let mut query: Vec<(String, String)> = vec![
                    ("showDeleted".into(), "true".into()),
                    ("maxResults".into(), "250".into()),
                ];
                if let Some(token) = &sync_token {
                    query.push(("syncToken".into(), token.clone()));
                } else {
                    query.push(("singleEvents".into(), "false".into()));
                    query.push(("timeMin".into(), format_instant(window.start)));
                    query.push(("timeMax".into(), format_instant(window.end)));
                }
                if let Some(token) = &page_token {
                    query.push(("pageToken".into(), token.clone()));
                }
                let url = self.url(&format!("/calendars/{}/events", encode(calendar_remote_id)));
                let response = self
                    .http
                    .send_authed(
                        account_id,
                        nuncio_core::traits::TokenResource::GoogleApis,
                        |token| self.http.raw().get(&url).bearer_auth(token).query(&query),
                    )
                    .await?;
                let status = response.status();
                if status == StatusCode::GONE && sync_token.is_some() {
                    debug!(
                        calendar_remote_id,
                        "Google sync token expired; doing full resync"
                    );
                    sync_token = None;
                    continue 'restart;
                }
                if !status.is_success() {
                    return Err(Self::failure(response).await);
                }
                let page: GEventList = response
                    .json()
                    .await
                    .map_err(|error| invalid(format!("invalid Google events JSON: {error}")))?;
                for ge in &page.items {
                    if is_cancelled_master(ge) {
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
                            warn!(event_id = %ge.id, %error, "skipping unmappable Google event");
                        }
                    }
                    if is_cancelled_override(ge) {
                        // The event is kept with status = cancelled so local
                        // recurrence expansion drops that occurrence.
                        if let Some(last) = delta.events.last_mut() {
                            last.event.status = EventStatus::Cancelled;
                        }
                    }
                }
                match page.next_page_token {
                    Some(token) if !token.is_empty() => page_token = Some(token),
                    _ => break page.next_sync_token,
                }
            };
            delta.sync_token = next_sync_token.or(sync_token);
            delta.full_sync = full_sync;
            return Ok(delta);
        }
    }

    pub async fn create_event(
        &self,
        account_id: &str,
        calendar_remote_id: &str,
        event: &CalendarEvent,
        notify_attendees: bool,
    ) -> Result<()> {
        let url = self.url(&format!("/calendars/{}/events", encode(calendar_remote_id)));
        let body = event_body(event, &[], None, true);
        let send_updates = send_updates(notify_attendees, event);
        let response = self
            .http
            .send_authed(
                account_id,
                nuncio_core::traits::TokenResource::GoogleApis,
                |token| {
                    self.http
                        .raw()
                        .post(&url)
                        .bearer_auth(token)
                        .query(&[("sendUpdates", send_updates)])
                        .json(&body)
                },
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
        notify_attendees: bool,
    ) -> Result<()> {
        let send_updates = send_updates(notify_attendees, updated);
        match scope {
            RecurrenceEditScope::All => {
                let Some(master_remote) = current.remote_id.as_deref() else {
                    return Err(AppError::conflict("Google event has no remote id"));
                };
                // Emit the original RRULE text when the structured rule still
                // matches it; otherwise the user changed the recurrence.
                let original_rrule = match (&current.rrule, &updated.recurrence) {
                    (Some(text), Some(rule))
                        if nuncio_ical::recurrence::from_rrule_string(text).as_ref()
                            == Some(rule) =>
                    {
                        Some(text.as_str())
                    }
                    _ => None,
                };
                let mut body = event_body(updated, &current.exdates, original_rrule, false);
                if updated.recurrence.is_none() {
                    // Google keeps the old RRULE unless an empty list is sent.
                    body["recurrence"] = serde_json::json!([]);
                }
                self.patch_event(
                    account_id,
                    calendar_remote_id,
                    master_remote,
                    &body,
                    send_updates,
                )
                .await
            }
            RecurrenceEditScope::This => {
                let master_remote = master_remote_id(series, current)?;
                let instance_start =
                    instance_start.ok_or_else(|| invalid("instance start is required"))?;
                let instance_id = self
                    .find_instance_id(
                        account_id,
                        calendar_remote_id,
                        master_remote,
                        instance_start,
                        updated.all_day,
                    )
                    .await?;
                let Some(instance_id) = instance_id else {
                    warn!(
                        master_remote,
                        instance_start, "Google occurrence not found; patching master"
                    );
                    return self
                        .patch_event(
                            account_id,
                            calendar_remote_id,
                            master_remote,
                            &event_body(updated, &[], None, false),
                            send_updates,
                        )
                        .await;
                };
                self.patch_event(
                    account_id,
                    calendar_remote_id,
                    &instance_id,
                    &event_body(updated, &[], None, false),
                    send_updates,
                )
                .await
            }
            RecurrenceEditScope::ThisAndFollowing => {
                let Some(master_remote) = current.remote_id.as_deref() else {
                    return Err(AppError::conflict(
                        "Google recurring event has no remote id",
                    ));
                };
                let instance_start =
                    instance_start.ok_or_else(|| invalid("instance start is required"))?;
                let split =
                    nuncio_ical::recurrence::split_series_at(&current.event, instance_start)?;
                let mut body = serde_json::json!({});
                if let Some(rule) = split.master.recurrence {
                    body["recurrence"] = serde_json::json!([format!(
                        "RRULE:{}",
                        nuncio_ical::recurrence::to_rrule_string(&rule)
                    )]);
                }
                self.patch_event(
                    account_id,
                    calendar_remote_id,
                    master_remote,
                    &body,
                    send_updates,
                )
                .await?;
                // The new series starts at the split instance.  Google requires
                // a unique iCalUID, so the caller assigned one to `updated`.
                self.create_event(account_id, calendar_remote_id, updated, notify_attendees)
                    .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn delete_event(
        &self,
        account_id: &str,
        calendar_remote_id: &str,
        event: &StoredEvent,
        series: &[StoredEvent],
        scope: RecurrenceEditScope,
        instance_start: Option<&str>,
        notify_attendees: bool,
    ) -> Result<()> {
        let send_updates = if notify_attendees { "all" } else { "none" };
        match scope {
            RecurrenceEditScope::All => {
                let Some(remote_id) = event.remote_id.as_deref() else {
                    return Ok(());
                };
                self.delete_event_by_id(account_id, calendar_remote_id, remote_id, send_updates)
                    .await
            }
            RecurrenceEditScope::This => {
                let master_remote = master_remote_id(series, event)?;
                let instance_start = instance_start
                    .or(event.event.recurrence_id.as_deref())
                    .ok_or_else(|| {
                        invalid("instance start is required for deleting one occurrence")
                    })?;
                let instance_id = self
                    .find_instance_id(
                        account_id,
                        calendar_remote_id,
                        master_remote,
                        instance_start,
                        event.event.all_day,
                    )
                    .await?;
                match instance_id {
                    Some(instance_id) => {
                        self.delete_event_by_id(
                            account_id,
                            calendar_remote_id,
                            &instance_id,
                            send_updates,
                        )
                        .await
                    }
                    None => Ok(()),
                }
            }
            RecurrenceEditScope::ThisAndFollowing => {
                let Some(master_remote) = event.remote_id.as_deref() else {
                    return Ok(());
                };
                let instance_start = instance_start.ok_or_else(|| {
                    invalid("instance start is required for this-and-following delete")
                })?;
                let split = nuncio_ical::recurrence::split_series_at(&event.event, instance_start)?;
                let mut body = serde_json::json!({});
                if let Some(rule) = split.master.recurrence {
                    body["recurrence"] = serde_json::json!([format!(
                        "RRULE:{}",
                        nuncio_ical::recurrence::to_rrule_string(&rule)
                    )]);
                }
                self.patch_event(
                    account_id,
                    calendar_remote_id,
                    master_remote,
                    &body,
                    send_updates,
                )
                .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn respond_event(
        &self,
        account_id: &str,
        email: &str,
        calendar_remote_id: &str,
        event: &StoredEvent,
        response: AttendeeStatus,
        _comment: Option<&str>,
        send_reply: bool,
    ) -> Result<()> {
        let Some(remote_id) = event.remote_id.as_deref() else {
            return Err(AppError::conflict("Google event has no remote id"));
        };
        let body = GoogleClient::response_body(&event.event, email, response);
        self.patch_event(
            account_id,
            calendar_remote_id,
            remote_id,
            &body,
            if send_reply { "all" } else { "none" },
        )
        .await
    }

    /// Set the response status of `email` and return the JSON patch body.
    pub fn response_body(
        event: &CalendarEvent,
        email: &str,
        response: AttendeeStatus,
    ) -> serde_json::Value {
        let mut attendees = event.attendees.clone();
        let mut found = false;
        for attendee in &mut attendees {
            if attendee.email.eq_ignore_ascii_case(email) {
                attendee.status = response;
                found = true;
            }
        }
        if !found {
            attendees.push(Attendee {
                email: email.to_string(),
                name: None,
                role: nuncio_core::models::AttendeeRole::Required,
                status: response,
            });
        }
        serde_json::json!({
            "attendees": attendees.iter().map(|attendee| serde_json::json!({
                "email": attendee.email,
                "displayName": attendee.name,
                "optional": attendee.role == nuncio_core::models::AttendeeRole::Optional,
                "resource": attendee.role == nuncio_core::models::AttendeeRole::Resource,
                "responseStatus": attendee_status_name(attendee.status),
            })).collect::<Vec<_>>(),
        })
    }

    /// Find an event by its iCalendar UID (used for meeting invitations).
    pub async fn find_by_ical_uid(
        &self,
        account_id: &str,
        calendar_remote_id: &str,
        uid: &str,
    ) -> Result<Option<GEvent>> {
        let url = self.url(&format!("/calendars/{}/events", encode(calendar_remote_id)));
        let query = vec![
            ("iCalUID".to_string(), uid.to_string()),
            ("singleEvents".to_string(), "false".to_string()),
            ("showDeleted".to_string(), "false".to_string()),
            ("maxResults".to_string(), "5".to_string()),
        ];
        let response = self
            .http
            .send_authed(
                account_id,
                nuncio_core::traits::TokenResource::GoogleApis,
                |token| self.http.raw().get(&url).bearer_auth(token).query(&query),
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        let page: GEventList = response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid Google events JSON: {error}")))?;
        Ok(page.items.into_iter().next())
    }

    async fn find_instance_id(
        &self,
        account_id: &str,
        calendar_remote_id: &str,
        master_remote_id: &str,
        instance_start: &str,
        all_day: bool,
    ) -> Result<Option<String>> {
        let url = self.url(&format!(
            "/calendars/{}/events/{}/instances",
            encode(calendar_remote_id),
            encode(master_remote_id)
        ));
        let mut query: Vec<(String, String)> = vec![
            ("maxResults".into(), "20".into()),
            ("singleEvents".into(), "true".into()),
        ];
        if all_day {
            query.push(("originalStart".into(), instance_start.to_string()));
        } else if let Ok(instant) = parse_instant(instance_start) {
            query.push(("originalStart".into(), format_instant(instant)));
            query.push((
                "timeMin".into(),
                format_instant(instant - Duration::hours(2)),
            ));
            query.push((
                "timeMax".into(),
                format_instant(instant + Duration::hours(2)),
            ));
        }
        let response = self
            .http
            .send_authed(
                account_id,
                nuncio_core::traits::TokenResource::GoogleApis,
                |token| self.http.raw().get(&url).bearer_auth(token).query(&query),
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        let page: GEventList = response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid Google instances JSON: {error}")))?;
        Ok(page.items.first().map(|event| event.id.clone()))
    }

    async fn patch_event(
        &self,
        account_id: &str,
        calendar_remote_id: &str,
        event_id: &str,
        body: &serde_json::Value,
        send_updates: &str,
    ) -> Result<()> {
        let url = self.url(&format!(
            "/calendars/{}/events/{}",
            encode(calendar_remote_id),
            encode(event_id)
        ));
        let response = self
            .http
            .send_authed(
                account_id,
                nuncio_core::traits::TokenResource::GoogleApis,
                |token| {
                    self.http
                        .raw()
                        .patch(&url)
                        .bearer_auth(token)
                        .query(&[("sendUpdates", send_updates)])
                        .json(body)
                },
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        Ok(())
    }

    async fn delete_event_by_id(
        &self,
        account_id: &str,
        calendar_remote_id: &str,
        event_id: &str,
        send_updates: &str,
    ) -> Result<()> {
        let url = self.url(&format!(
            "/calendars/{}/events/{}",
            encode(calendar_remote_id),
            encode(event_id)
        ));
        let response = self
            .http
            .send_authed(
                account_id,
                nuncio_core::traits::TokenResource::GoogleApis,
                |token| {
                    self.http
                        .raw()
                        .delete(&url)
                        .bearer_auth(token)
                        .query(&[("sendUpdates", send_updates)])
                },
            )
            .await?;
        if !response.status().is_success() && response.status() != StatusCode::NOT_FOUND {
            return Err(Self::failure(response).await);
        }
        Ok(())
    }
}

fn send_updates(notify: bool, event: &CalendarEvent) -> &'static str {
    if notify && !event.attendees.is_empty() {
        "all"
    } else {
        "none"
    }
}

fn master_remote_id<'a>(series: &'a [StoredEvent], current: &'a StoredEvent) -> Result<&'a str> {
    series
        .iter()
        .find(|stored| stored.event.series_id.is_none())
        .and_then(|stored| stored.remote_id.as_deref())
        .or(current.remote_id.as_deref())
        .ok_or_else(|| AppError::conflict("Google recurring event has no remote id"))
}

fn contrast_color(color: &str) -> &'static str {
    let hex = color.trim_start_matches('#');
    if hex.len() < 6 {
        return "#FFFFFF";
    }
    let rgb = u32::from_str_radix(&hex[..6], 16).unwrap_or(0x0078D4);
    let r = (rgb >> 16) & 0xff;
    let g = (rgb >> 8) & 0xff;
    let b = rgb & 0xff;
    let luminance = (r * 299 + g * 587 + b * 114) / 1000;
    if luminance > 160 {
        "#000000"
    } else {
        "#FFFFFF"
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GEventLikeCalendar {
    id: String,
    #[serde(default)]
    summary: Option<String>,
}

/// `CalendarProvider` implementation used by `PimService`.
pub struct GoogleProvider {
    client: GoogleClient,
    default_reminder_minutes: i32,
}

impl std::fmt::Debug for GoogleProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleProvider").finish_non_exhaustive()
    }
}

impl GoogleProvider {
    pub fn new(http: HttpClient, default_reminder_minutes: i32) -> Self {
        Self {
            client: GoogleClient::new(http),
            default_reminder_minutes,
        }
    }

    pub fn with_base(
        http: HttpClient,
        base: impl Into<String>,
        default_reminder_minutes: i32,
    ) -> Self {
        Self {
            client: GoogleClient::with_base(http, base),
            default_reminder_minutes,
        }
    }

    pub fn client(&self) -> &GoogleClient {
        &self.client
    }
}

#[async_trait]
impl CalendarProvider for GoogleProvider {
    fn name(&self) -> &'static str {
        "google"
    }

    async fn list_calendars(&self, account: &Account) -> Result<Vec<RemoteCalendar>> {
        self.client.list_calendars(&account.id).await
    }

    async fn create_calendar(
        &self,
        account: &Account,
        name: &str,
        color: &str,
        time_zone: &str,
    ) -> Result<RemoteCalendar> {
        self.client
            .create_calendar(&account.id, name, color, time_zone)
            .await
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
            .delete_calendar(&account.id, &remote.remote_id, remote.read_only)
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
        notify_attendees: bool,
    ) -> Result<()> {
        self.client
            .create_event(&account.id, &remote.remote_id, event, notify_attendees)
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
        notify_attendees: bool,
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
                notify_attendees,
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
        notify_attendees: bool,
    ) -> Result<()> {
        self.client
            .delete_event(
                &account.id,
                &remote.remote_id,
                event,
                series,
                scope,
                instance_start,
                notify_attendees,
            )
            .await
    }

    async fn respond_event(
        &self,
        account: &Account,
        remote: &RemoteCalendar,
        event: &StoredEvent,
        _series: &[StoredEvent],
        response: AttendeeStatus,
        comment: Option<&str>,
        send_reply: bool,
    ) -> Result<()> {
        self.client
            .respond_event(
                &account.id,
                &account.email,
                &remote.remote_id,
                event,
                response,
                comment,
                send_reply,
            )
            .await
    }
}

/// Microsoft Graph-shaped helper: compute the UTC instant for a Google time.
pub fn google_instant(value: &str, zone: &str) -> String {
    parse_instant(value)
        .map(|instant| crate::util::instant_in_zone(instant, zone))
        .unwrap_or_else(|_| Utc::now().to_rfc3339())
}
