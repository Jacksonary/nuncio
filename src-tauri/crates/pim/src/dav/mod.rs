//! CalDAV / WebDAV client (RFC 4791, RFC 6578).
//!
//! Discovery follows `/.well-known/caldav` → `current-user-principal` →
//! `calendar-home-set` → `PROPFIND Depth: 1`.  Incremental sync prefers
//! `sync-collection` and falls back to `getctag` + per-resource ETag
//! comparison.  All XML handling is generic so stage 2 can reuse it for
//! CardDAV (`addressbook-multiget`, `addressbook-home-set`).

pub mod map;
pub mod xml;

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use nuncio_core::models::{
    Account, AttendeeStatus, AuthKind, CalendarEvent, RecurrenceEditScope, StoredEvent,
};
use nuncio_core::providers::{self, DavService};
use reqwest::{Method, Response, StatusCode};
use tracing::{debug, warn};
use url::Url;

use crate::error::{invalid, protocol, Result};
use crate::http::{request_error, status_error, HttpClient, USER_AGENT};
use crate::provider::{CalendarCursor, CalendarProvider, EventDelta, EventWindow, RemoteCalendar};

use self::map::{
    apply_master_update, apply_override_update, compose_ics, events_from_ics, filter_after_split,
    set_response,
};
use self::xml::{
    collections_propfind, etags_propfind, homes_propfind, mkcalendar_body,
    mkcalendar_body_with_component, multiget_body, parse_multistatus, principal_propfind,
    proppatch_body, sync_collection_body, Multistatus,
};

/// Which DAV collection family is being addressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DavKind {
    Calendar,
    AddressBook,
}

impl DavKind {
    fn home_kind(self) -> &'static str {
        match self {
            Self::Calendar => "calendar",
            Self::AddressBook => "addressbook",
        }
    }
}

#[derive(Clone)]
enum DavAuth {
    Bearer(String),
    Basic { user: String, password: String },
}

impl std::fmt::Debug for DavAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bearer(_) => f.write_str("DavAuth::Bearer(<redacted>)"),
            Self::Basic { user, .. } => f
                .debug_struct("DavAuth::Basic")
                .field("user", user)
                .field("password", &"<redacted>")
                .finish(),
        }
    }
}

/// One fetched DAV resource.
#[derive(Debug, Clone, Default)]
pub struct DavResource {
    pub href: String,
    pub etag: Option<String>,
    pub data: String,
}

/// A batch of changes for a non-calendar DAV collection (CardDAV, VTODO).
#[derive(Debug, Clone, Default)]
pub struct ResourceDelta {
    pub changed: Vec<DavResource>,
    pub deleted_hrefs: Vec<String>,
    pub sync_token: Option<String>,
    pub ctag: Option<String>,
    pub full_sync: bool,
}

/// CalDAV/CardDAV client bound to one account and one DAV service.
///
/// The service selects which optional password is used: providers such as
/// Yandex issue a separate app password for calendar and contacts.
#[derive(Clone)]
pub struct DavClient {
    http: HttpClient,
    account: Account,
    service: DavService,
}

impl std::fmt::Debug for DavClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DavClient")
            .field("account", &self.account.id)
            .field("service", &self.service.as_str())
            .finish_non_exhaustive()
    }
}

impl DavClient {
    /// Calendar-backed client (CalDAV events and VTODO task lists).
    pub fn new(http: HttpClient, account: Account) -> Self {
        Self::for_service(http, account, DavService::Calendar)
    }

    pub fn for_service(http: HttpClient, account: Account, service: DavService) -> Self {
        Self {
            http,
            account,
            service,
        }
    }

    pub fn account(&self) -> &Account {
        &self.account
    }

    async fn auth(&self) -> Result<DavAuth> {
        if self.account.auth_kind == AuthKind::Oauth {
            let token = self
                .http
                .creds()
                .api_token(&self.account.id, nuncio_core::traits::TokenResource::Dav)
                .await?;
            return Ok(DavAuth::Bearer(token));
        }
        let (user, password) = self
            .http
            .creds()
            .dav_auth(&self.account, self.service)
            .await?;
        Ok(DavAuth::Basic { user, password })
    }

    fn build(
        &self,
        method: &Method,
        url: &str,
        headers: &[(String, String)],
        body: Option<&str>,
        auth: &DavAuth,
    ) -> reqwest::RequestBuilder {
        let mut builder = self
            .http
            .raw()
            .request(method.clone(), url)
            .header("User-Agent", USER_AGENT);
        builder = match auth {
            DavAuth::Bearer(token) => builder.bearer_auth(token),
            DavAuth::Basic { user, password } => builder.basic_auth(user, Some(password)),
        };
        for (name, value) in headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if let Some(body) = body {
            builder = builder.body(body.to_string());
        }
        builder
    }

    async fn send(
        &self,
        method: &Method,
        url: &str,
        headers: &[(String, String)],
        body: Option<&str>,
    ) -> Result<Response> {
        let mut auth = self.auth().await?;
        let mut response = self
            .build(method, url, headers, body, &auth)
            .send()
            .await
            .map_err(|error| request_error("DAV request", error))?;
        if response.status() == StatusCode::UNAUTHORIZED {
            debug!(account_id = %self.account.id, "DAV credential rejected; retrying once");
            self.http
                .creds()
                .report_auth_failure(&self.account.id, nuncio_core::traits::TokenResource::Dav)
                .await;
            auth = self.auth().await?;
            response = self
                .build(method, url, headers, body, &auth)
                .send()
                .await
                .map_err(|error| request_error("DAV request after reauth", error))?;
        }
        Ok(response)
    }

    async fn propfind(&self, url: &str, body: &str, depth: &str) -> Result<Multistatus> {
        let headers = vec![
            ("Depth".to_string(), depth.to_string()),
            (
                "Content-Type".to_string(),
                "application/xml; charset=utf-8".to_string(),
            ),
        ];
        let method = method("PROPFIND")?;
        let response = self.send(&method, url, &headers, Some(body)).await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(status_error(status, &text));
        }
        parse_multistatus(&text)
    }

    /* ─────────────────────────── discovery ─────────────────────────── */

    fn fallback_home(&self, kind: DavKind) -> String {
        home_url(&self.account, kind).unwrap_or_default()
    }

    /// Discover the calendar (or address-book) home collection URL.
    pub async fn discover_home(&self, kind: DavKind) -> Result<String> {
        let start = self.fallback_home(kind);
        if start.is_empty() {
            return Err(invalid("account has no DAV server URL"));
        }
        let headers = vec![
            ("Depth".to_string(), "0".to_string()),
            (
                "Content-Type".to_string(),
                "application/xml; charset=utf-8".to_string(),
            ),
        ];
        let method = method("PROPFIND")?;
        let response = self
            .send(&method, &start, &headers, Some(&principal_propfind()))
            .await?;
        let final_url = response.url().to_string();
        let status = response.status();
        if !status.is_success() && status != StatusCode::MULTI_STATUS {
            // Some servers do not implement .well-known; try the origin root.
            debug!(%final_url, status = status.as_u16(), "DAV discovery failed; trying origin");
            return self.discover_from_origin(kind, &start).await;
        }
        let text = response.text().await.unwrap_or_default();
        let multistatus = parse_multistatus(&text).unwrap_or_default();
        if let Some(principal) = multistatus.current_user_principal {
            let principal_url = self.resolve_url(&final_url, &principal);
            if let Some(home) = self.home_from_principal(kind, &principal_url).await? {
                return Ok(home);
            }
        }
        // The discovery URL may already be the home collection.
        Ok(ensure_trailing_slash(&final_url))
    }

    async fn discover_from_origin(&self, kind: DavKind, start: &str) -> Result<String> {
        let origin = Url::parse(start)
            .ok()
            .map(|url| {
                let mut origin = url.clone();
                origin.set_path("/");
                origin.set_query(None);
                origin.set_fragment(None);
                origin.to_string()
            })
            .unwrap_or_else(|| start.to_string());
        let headers = vec![
            ("Depth".to_string(), "0".to_string()),
            (
                "Content-Type".to_string(),
                "application/xml; charset=utf-8".to_string(),
            ),
        ];
        let method = method("PROPFIND")?;
        let response = self
            .send(&method, &origin, &headers, Some(&principal_propfind()))
            .await?;
        if !response.status().is_success() {
            return Err(status_error(
                response.status(),
                &response.text().await.unwrap_or_default(),
            ));
        }
        let final_url = response.url().to_string();
        let text = response.text().await.unwrap_or_default();
        let multistatus = parse_multistatus(&text).unwrap_or_default();
        if let Some(principal) = multistatus.current_user_principal {
            let principal_url = self.resolve_url(&final_url, &principal);
            if let Some(home) = self.home_from_principal(kind, &principal_url).await? {
                return Ok(home);
            }
        }
        Ok(ensure_trailing_slash(&final_url))
    }

    async fn home_from_principal(
        &self,
        kind: DavKind,
        principal_url: &str,
    ) -> Result<Option<String>> {
        let headers = vec![
            ("Depth".to_string(), "0".to_string()),
            (
                "Content-Type".to_string(),
                "application/xml; charset=utf-8".to_string(),
            ),
        ];
        let method = method("PROPFIND")?;
        let response = self
            .send(&method, principal_url, &headers, Some(&homes_propfind()))
            .await?;
        if !response.status().is_success() {
            return Ok(None);
        }
        let final_url = response.url().to_string();
        let text = response.text().await.unwrap_or_default();
        let multistatus = parse_multistatus(&text).unwrap_or_default();
        let home = match kind {
            DavKind::Calendar => multistatus.calendar_home_set,
            DavKind::AddressBook => multistatus.addressbook_home_set,
        };
        Ok(home.map(|home| ensure_trailing_slash(&self.resolve_url(&final_url, &home))))
    }

    /* ─────────────────────── collection listing ─────────────────────── */

    pub async fn list_collections(&self, kind: DavKind) -> Result<Vec<RemoteCalendar>> {
        let home = self.discover_home(kind).await?;
        let multistatus = self
            .propfind(&home, &collections_propfind(kind.home_kind()), "1")
            .await?;
        let mut out = Vec::new();
        for response in &multistatus.responses {
            let is_collection = match kind {
                DavKind::Calendar => response.prop.is_calendar,
                DavKind::AddressBook => response.prop.is_address_book,
            };
            if !is_collection {
                continue;
            }
            let href = ensure_trailing_slash(&self.resolve_url(&home, &response.href));
            let name = response
                .prop
                .display_name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| last_segment(&href));
            let read_only = response.prop.read_only;
            out.push(RemoteCalendar {
                local_id: String::new(),
                remote_id: href.clone(),
                href: Some(href),
                name,
                color: response.prop.color.clone(),
                read_only,
                sync_token: response.prop.sync_token.clone(),
                ctag: response.prop.ctag.clone(),
                supports_sync_collection: response.prop.supports_sync_collection,
            });
        }
        Ok(out)
    }

    pub async fn list_calendars(&self) -> Result<Vec<RemoteCalendar>> {
        self.list_collections(DavKind::Calendar).await
    }

    /// Calendar collections that advertise `component` (`VEVENT`, `VTODO`).
    pub async fn list_calendars_with_component(
        &self,
        component: &str,
    ) -> Result<Vec<RemoteCalendar>> {
        let home = self.discover_home(DavKind::Calendar).await?;
        let multistatus = self
            .propfind(
                &home,
                &collections_propfind(DavKind::Calendar.home_kind()),
                "1",
            )
            .await?;
        let component = component.to_ascii_uppercase();
        let mut out = Vec::new();
        for response in &multistatus.responses {
            if !response.prop.is_calendar {
                continue;
            }
            if !response.prop.components.is_empty()
                && !response.prop.components.contains(&component)
            {
                continue;
            }
            let href = ensure_trailing_slash(&self.resolve_url(&home, &response.href));
            let name = response
                .prop
                .display_name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| last_segment(&href));
            out.push(RemoteCalendar {
                local_id: String::new(),
                remote_id: href.clone(),
                href: Some(href),
                name,
                color: response.prop.color.clone(),
                read_only: response.prop.read_only,
                sync_token: response.prop.sync_token.clone(),
                ctag: response.prop.ctag.clone(),
                supports_sync_collection: response.prop.supports_sync_collection,
            });
        }
        Ok(out)
    }

    /// Reusable by stage 2 (CardDAV).
    pub async fn list_address_books(&self) -> Result<Vec<RemoteCalendar>> {
        self.list_collections(DavKind::AddressBook).await
    }

    pub async fn create_calendar(&self, name: &str, color: Option<&str>) -> Result<RemoteCalendar> {
        let home = self.discover_home(DavKind::Calendar).await?;
        let href = format!("{}{}/", home, urlencoding::encode(name.trim()));
        let headers = vec![(
            "Content-Type".to_string(),
            "application/xml; charset=utf-8".to_string(),
        )];
        let method = method("MKCALENDAR")?;
        let response = self
            .send(
                &method,
                &href,
                &headers,
                Some(&mkcalendar_body(name, color)),
            )
            .await?;
        let status = response.status();
        if !matches!(status.as_u16(), 200 | 201 | 204) {
            return Err(status_error(
                status,
                &response.text().await.unwrap_or_default(),
            ));
        }
        Ok(RemoteCalendar {
            local_id: String::new(),
            remote_id: href.clone(),
            href: Some(href),
            name: name.to_string(),
            color: color.and_then(crate::util::normalize_color),
            read_only: false,
            sync_token: None,
            ctag: None,
            supports_sync_collection: false,
        })
    }

    /// Create a VTODO collection (CalDAV task list).
    pub async fn create_task_list(
        &self,
        name: &str,
        color: Option<&str>,
    ) -> Result<RemoteCalendar> {
        let home = self.discover_home(DavKind::Calendar).await?;
        let href = format!("{}{}/", home, urlencoding::encode(name.trim()));
        let headers = vec![(
            "Content-Type".to_string(),
            "application/xml; charset=utf-8".to_string(),
        )];
        let method = method("MKCALENDAR")?;
        let response = self
            .send(
                &method,
                &href,
                &headers,
                Some(&mkcalendar_body_with_component(name, color, "VTODO")),
            )
            .await?;
        let status = response.status();
        if !matches!(status.as_u16(), 200 | 201 | 204) {
            return Err(status_error(
                status,
                &response.text().await.unwrap_or_default(),
            ));
        }
        Ok(RemoteCalendar {
            local_id: String::new(),
            remote_id: href.clone(),
            href: Some(href),
            name: name.to_string(),
            color: color.and_then(crate::util::normalize_color),
            read_only: false,
            sync_token: None,
            ctag: None,
            supports_sync_collection: false,
        })
    }

    pub async fn update_calendar(
        &self,
        remote_id: &str,
        name: Option<&str>,
        color: Option<&str>,
    ) -> Result<()> {
        if name.is_none() && color.is_none() {
            return Ok(());
        }
        let headers = vec![(
            "Content-Type".to_string(),
            "application/xml; charset=utf-8".to_string(),
        )];
        let method = method("PROPPATCH")?;
        let response = self
            .send(
                &method,
                remote_id,
                &headers,
                Some(&proppatch_body(name, color)),
            )
            .await?;
        ensure_ok(response).await
    }

    pub async fn delete_calendar(&self, remote_id: &str) -> Result<()> {
        let response = self.send(&Method::DELETE, remote_id, &[], None).await?;
        let status = response.status();
        if !status.is_success() && status != StatusCode::NOT_FOUND {
            return Err(status_error(
                status,
                &response.text().await.unwrap_or_default(),
            ));
        }
        Ok(())
    }

    /* ───────────────────────────── sync ───────────────────────────── */

    pub async fn sync_events(
        &self,
        calendar: &RemoteCalendar,
        cursor: &CalendarCursor,
        _window: EventWindow,
    ) -> Result<EventDelta> {
        let collection = calendar.remote_id.trim_end_matches('/').to_string();
        let mut delta = EventDelta::default();

        if let Some(token) = cursor.sync_token.clone() {
            let result = self.sync_collection(&collection, &token, cursor).await;
            match result {
                Ok(Some(mut part)) => {
                    part.full_sync = false;
                    return Ok(part);
                }
                Ok(None) => {
                    debug!(
                        collection,
                        "sync-collection unavailable; falling back to ctag"
                    );
                }
                Err(error) if is_fallback_error(&error) => {
                    debug!(collection, %error, "sync-collection failed; falling back to ctag");
                }
                Err(error) => return Err(error),
            }
        }

        let multistatus = self.propfind(&collection, &etags_propfind(), "1").await?;
        let collection_url = ensure_trailing_slash(&collection);
        let collection_response = multistatus
            .responses
            .iter()
            .find(|response| {
                same_href(
                    &self.resolve_url(&collection_url, &response.href),
                    &collection_url,
                )
            })
            .or_else(|| multistatus.responses.first());
        let new_ctag = collection_response.and_then(|response| response.prop.ctag.clone());
        let new_sync_token = collection_response
            .and_then(|response| response.prop.sync_token.clone())
            .or_else(|| cursor.sync_token.clone());

        if cursor.ctag.is_some() && cursor.ctag == new_ctag {
            delta.ctag = new_ctag;
            delta.sync_token = new_sync_token;
            delta.full_sync = false;
            return Ok(delta);
        }

        let local: HashMap<String, String> = cursor.etags.iter().cloned().collect();
        let mut changed = Vec::new();
        let mut present: HashSet<String> = HashSet::new();
        for response in &multistatus.responses {
            let Some(etag) = response.prop.etag.clone() else {
                continue;
            };
            if response.prop.is_collection && !response.prop.is_calendar {
                continue;
            }
            let href = self.resolve_url(&collection_url, &response.href);
            if same_href(&href, &collection_url) {
                continue;
            }
            present.insert(href.clone());
            if local.get(&href) != Some(&etag) {
                changed.push(href);
            }
        }
        for href in local.keys() {
            if !present.contains(href) {
                delta.deleted_hrefs.push(href.clone());
            }
        }
        if !changed.is_empty() {
            let fetched = self
                .multiget(DavKind::Calendar, &collection_url, &changed)
                .await?;
            for (href, etag, data) in fetched {
                match events_from_ics(
                    &data,
                    &href,
                    etag.as_deref(),
                    &calendar.local_id,
                    &self.account.id,
                    &self.account.email,
                ) {
                    Ok(events) => delta.events.extend(events),
                    Err(error) => warn!(%href, %error, "skipping invalid CalDAV resource"),
                }
            }
        }
        delta.ctag = new_ctag;
        delta.sync_token = new_sync_token;
        delta.full_sync = false;
        Ok(delta)
    }

    async fn sync_collection(
        &self,
        collection: &str,
        token: &str,
        cursor: &CalendarCursor,
    ) -> Result<Option<EventDelta>> {
        let headers = vec![(
            "Content-Type".to_string(),
            "application/xml; charset=utf-8".to_string(),
        )];
        let method = method("REPORT")?;
        let response = self
            .send(
                &method,
                collection,
                &headers,
                Some(&sync_collection_body(Some(token))),
            )
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let error = status_error(status, &body);
            return if matches!(status.as_u16(), 400 | 403 | 404 | 409 | 412 | 501) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        let header_token = response
            .headers()
            .get("sync-token")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let text = response.text().await.unwrap_or_default();
        let multistatus = parse_multistatus(&text)?;
        let mut delta = EventDelta::default();
        let mut changed = Vec::new();
        for item in &multistatus.responses {
            let href = self.resolve_url(collection, &item.href);
            if item.not_found || item.status == Some(404) {
                delta.deleted_hrefs.push(href);
                continue;
            }
            if item.prop.etag.is_some() {
                changed.push(href);
            }
        }
        if !changed.is_empty() {
            let collection_url = ensure_trailing_slash(collection);
            let fetched = self
                .multiget(DavKind::Calendar, &collection_url, &changed)
                .await?;
            for (href, etag, data) in fetched {
                match events_from_ics(
                    &data,
                    &href,
                    etag.as_deref(),
                    &cursor.local_calendar_id,
                    &self.account.id,
                    &self.account.email,
                ) {
                    Ok(events) => delta.events.extend(events),
                    Err(error) => warn!(%href, %error, "skipping invalid CalDAV resource"),
                }
            }
        }
        delta.sync_token = multistatus
            .sync_token
            .or(header_token)
            .or_else(|| Some(token.to_string()));
        delta.ctag = cursor.ctag.clone();
        Ok(Some(delta))
    }

    /// Fetch one or more resources.  `kind` selects `calendar-multiget` or
    /// `addressbook-multiget` (stage 2).
    pub async fn multiget(
        &self,
        kind: DavKind,
        collection: &str,
        hrefs: &[String],
    ) -> Result<Vec<(String, Option<String>, String)>> {
        let mut out = Vec::new();
        for batch in hrefs.chunks(50) {
            let headers = vec![(
                "Content-Type".to_string(),
                "application/xml; charset=utf-8".to_string(),
            )];
            let method = method("REPORT")?;
            let response = self
                .send(
                    &method,
                    collection,
                    &headers,
                    Some(&multiget_body(kind.home_kind(), batch)),
                )
                .await?;
            if !response.status().is_success() {
                return Err(status_error(
                    response.status(),
                    &response.text().await.unwrap_or_default(),
                ));
            }
            let text = response.text().await.unwrap_or_default();
            let multistatus = parse_multistatus(&text)?;
            for item in multistatus.responses {
                if item.not_found {
                    continue;
                }
                let href = self.resolve_url(collection, &item.href);
                let Some(data) = item.prop.data else {
                    continue;
                };
                out.push((href, item.prop.etag, data));
            }
        }
        Ok(out)
    }

    /* ────────────────── generic resource sync (CardDAV/VTODO) ───────── */

    /// Fetch the current collection state (ctag + per-resource etags).
    pub async fn collection_etags(
        &self,
        _kind: DavKind,
        collection: &str,
    ) -> Result<(Option<String>, Option<String>, Vec<(String, String)>)> {
        let multistatus = self.propfind(collection, &etags_propfind(), "1").await?;
        let collection_url = ensure_trailing_slash(collection);
        let collection_response = multistatus
            .responses
            .iter()
            .find(|response| {
                same_href(
                    &self.resolve_url(&collection_url, &response.href),
                    &collection_url,
                )
            })
            .or_else(|| multistatus.responses.first());
        let ctag = collection_response.and_then(|response| response.prop.ctag.clone());
        let sync_token = collection_response.and_then(|response| response.prop.sync_token.clone());
        let mut etags = Vec::new();
        for response in &multistatus.responses {
            let Some(etag) = response.prop.etag.clone() else {
                continue;
            };
            let href = self.resolve_url(&collection_url, &response.href);
            if same_href(&href, &collection_url) {
                continue;
            }
            if response.prop.is_collection {
                continue;
            }
            etags.push((href, etag));
        }
        Ok((ctag, sync_token, etags))
    }

    /// Incremental sync for non-calendar DAV collections.
    pub async fn sync_resources(
        &self,
        kind: DavKind,
        collection: &str,
        cursor: &CalendarCursor,
    ) -> Result<ResourceDelta> {
        let collection = collection.trim_end_matches('/').to_string();
        let collection_url = ensure_trailing_slash(&collection);
        let mut delta = ResourceDelta::default();

        if let Some(token) = cursor.sync_token.clone() {
            let headers = vec![(
                "Content-Type".to_string(),
                "application/xml; charset=utf-8".to_string(),
            )];
            let method = method("REPORT")?;
            let response = self
                .send(
                    &method,
                    &collection,
                    &headers,
                    Some(&sync_collection_body(Some(&token))),
                )
                .await?;
            if response.status().is_success() {
                let header_token = response
                    .headers()
                    .get("sync-token")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                let text = response.text().await.unwrap_or_default();
                let multistatus = parse_multistatus(&text)?;
                let mut changed = Vec::new();
                for item in &multistatus.responses {
                    let href = self.resolve_url(&collection_url, &item.href);
                    if item.not_found || item.status == Some(404) {
                        delta.deleted_hrefs.push(href);
                        continue;
                    }
                    if item.prop.etag.is_some() {
                        changed.push(href);
                    }
                }
                if !changed.is_empty() {
                    let fetched = self.multiget(kind, &collection_url, &changed).await?;
                    for (href, etag, data) in fetched {
                        delta.changed.push(DavResource { href, etag, data });
                    }
                }
                delta.sync_token = multistatus.sync_token.or(header_token).or(Some(token));
                delta.ctag = cursor.ctag.clone();
                delta.full_sync = false;
                return Ok(delta);
            }
            // Any 4xx/5xx: fall back to the ctag/etag comparison below.  A
            // server that no longer understands the token simply needs it
            // replaced, which the next successful pass does.
            debug!(
                collection,
                status = response.status().as_u16(),
                "sync-collection unsupported for this collection; using ETag diff"
            );
        }

        let (new_ctag, new_sync_token, etags) = self.collection_etags(kind, &collection).await?;
        if cursor.ctag.is_some() && cursor.ctag == new_ctag {
            delta.ctag = new_ctag;
            delta.sync_token = new_sync_token.or_else(|| cursor.sync_token.clone());
            delta.full_sync = false;
            return Ok(delta);
        }
        let local: HashMap<String, String> = cursor.etags.iter().cloned().collect();
        let mut present: HashSet<String> = HashSet::new();
        let mut changed = Vec::new();
        for (href, etag) in &etags {
            present.insert(href.clone());
            if local.get(href) != Some(etag) {
                changed.push(href.clone());
            }
        }
        for href in local.keys() {
            if !present.contains(href) {
                delta.deleted_hrefs.push(href.clone());
            }
        }
        if !changed.is_empty() {
            let fetched = self.multiget(kind, &collection_url, &changed).await?;
            for (href, etag, data) in fetched {
                delta.changed.push(DavResource { href, etag, data });
            }
        }
        delta.ctag = new_ctag;
        delta.sync_token = new_sync_token;
        delta.full_sync = cursor.ctag.is_none() && cursor.sync_token.is_none();
        Ok(delta)
    }

    /// Absolute resource URL for a new CardDAV/VTODO object.
    pub fn resource_url_for(collection: &str, name: &str, extension: &str) -> String {
        let collection = collection.trim_end_matches('/');
        format!(
            "{collection}/{}.{extension}",
            urlencoding::encode(name.trim())
        )
    }

    /// PUT a vCard/VTODO resource.  Returns the ETag reported by the server.
    pub async fn put_resource(
        &self,
        url: &str,
        body: &str,
        content_type: &str,
        etag: Option<&str>,
        create: bool,
    ) -> Result<Option<String>> {
        let mut headers = vec![("Content-Type".to_string(), content_type.to_string())];
        if create {
            headers.push(("If-None-Match".to_string(), "*".to_string()));
        } else if let Some(etag) = etag {
            headers.push(("If-Match".to_string(), if_match(etag)));
        } else {
            headers.push(("If-Match".to_string(), "*".to_string()));
        }
        let response = self.send(&Method::PUT, url, &headers, Some(body)).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(status_error(
                status,
                &response.text().await.unwrap_or_default(),
            ));
        }
        Ok(response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .map(|value| value.trim().trim_matches('"').to_string()))
    }

    /// DELETE a DAV resource guarded by its ETag.
    pub async fn delete_resource(&self, url: &str, etag: Option<&str>) -> Result<()> {
        let headers = match etag {
            Some(etag) => vec![("If-Match".to_string(), if_match(etag))],
            None => Vec::new(),
        };
        let response = self.send(&Method::DELETE, url, &headers, None).await?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(());
        }
        if !status.is_success() {
            return Err(status_error(
                status,
                &response.text().await.unwrap_or_default(),
            ));
        }
        Ok(())
    }

    /* ─────────────────────────── write ─────────────────────────── */

    fn resource_url(collection: &str, uid: &str) -> String {
        let collection = collection.trim_end_matches('/');
        let encoded = urlencoding::encode(uid.trim());
        format!("{collection}/{encoded}.ics")
    }

    async fn put(&self, url: &str, body: &str, etag: Option<&str>, create: bool) -> Result<()> {
        let mut headers = vec![(
            "Content-Type".to_string(),
            "text/calendar; charset=utf-8".to_string(),
        )];
        if create {
            headers.push(("If-None-Match".to_string(), "*".to_string()));
        } else if let Some(etag) = etag {
            headers.push(("If-Match".to_string(), if_match(etag)));
        } else {
            headers.push(("If-Match".to_string(), "*".to_string()));
        }
        let response = self.send(&Method::PUT, url, &headers, Some(body)).await?;
        ensure_ok(response).await
    }

    pub async fn create_event(
        &self,
        calendar: &RemoteCalendar,
        event: &CalendarEvent,
    ) -> Result<()> {
        let url = Self::resource_url(&calendar.remote_id, &event.uid);
        let body = nuncio_ical::ics::write_event(event, None);
        self.put(&url, &body, None, true).await
    }

    /// Create a recurring series (master + detached occurrences) as a single
    /// CalDAV resource.  A resource PUT replaces the whole object, so the
    /// master and its overrides must be composed together; separate PUTs to
    /// the same UID-derived URL would conflict with `If-None-Match: *`.
    pub async fn create_series(
        &self,
        calendar: &RemoteCalendar,
        series: &[StoredEvent],
    ) -> Result<()> {
        let master = series
            .iter()
            .find(|stored| stored.event.series_id.is_none())
            .ok_or_else(|| invalid("series has no master event"))?;
        let url = master
            .href
            .clone()
            .unwrap_or_else(|| Self::resource_url(&calendar.remote_id, &master.event.uid));
        self.put(&url, &compose_ics(series), None, true).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update_event(
        &self,
        calendar: &RemoteCalendar,
        current: &StoredEvent,
        series: &[StoredEvent],
        updated: &CalendarEvent,
        scope: RecurrenceEditScope,
        instance_start: Option<&str>,
    ) -> Result<()> {
        let master = series
            .iter()
            .find(|stored| stored.event.series_id.is_none())
            .unwrap_or(current);
        let master_url = master
            .href
            .clone()
            .unwrap_or_else(|| Self::resource_url(&calendar.remote_id, &master.event.uid));

        match scope {
            RecurrenceEditScope::All => {
                let mut list: Vec<StoredEvent> = series
                    .iter()
                    .filter(|stored| {
                        updated.recurrence.is_some() || stored.event.id == master.event.id
                    })
                    .map(|stored| {
                        if stored.event.id == master.event.id {
                            apply_master_update(stored, updated)
                        } else {
                            stored.clone()
                        }
                    })
                    .collect();
                if list.is_empty() {
                    list.push(apply_master_update(master, updated));
                }
                self.put(
                    &master_url,
                    &compose_ics(&list),
                    master.etag.as_deref(),
                    false,
                )
                .await
            }
            RecurrenceEditScope::This => {
                let instance_start =
                    instance_start.ok_or_else(|| invalid("instance start is required"))?;
                let existing = series.iter().find(|stored| {
                    stored
                        .event
                        .recurrence_id
                        .as_deref()
                        .is_some_and(|value| crate::util::same_instant(value, instance_start))
                });
                let override_event =
                    apply_override_update(master, existing, updated, instance_start);
                let mut list: Vec<StoredEvent> =
                    series
                        .iter()
                        .filter(|stored| {
                            !stored.event.recurrence_id.as_deref().is_some_and(|value| {
                                crate::util::same_instant(value, instance_start)
                            })
                        })
                        .cloned()
                        .collect();
                if list.is_empty() {
                    list.push(master.clone());
                }
                list.push(override_event);
                self.put(
                    &master_url,
                    &compose_ics(&list),
                    master.etag.as_deref(),
                    false,
                )
                .await
            }
            RecurrenceEditScope::ThisAndFollowing => {
                let instance_start =
                    instance_start.ok_or_else(|| invalid("instance start is required"))?;
                let split =
                    nuncio_ical::recurrence::split_series_at(&master.event, instance_start)?;
                let mut truncated = master.clone();
                truncated.event = split.master;
                truncated.raw_ics = None;
                truncated.rrule = truncated
                    .event
                    .recurrence
                    .as_ref()
                    .map(nuncio_ical::recurrence::to_rrule_string);
                let mut list = filter_after_split(series, &master.event.id, instance_start);
                for item in &mut list {
                    if item.event.id == master.event.id {
                        *item = truncated.clone();
                    }
                }
                if list.is_empty() {
                    list.push(truncated.clone());
                }
                self.put(
                    &master_url,
                    &compose_ics(&list),
                    master.etag.as_deref(),
                    false,
                )
                .await?;
                // The caller assigned a fresh UID to the new series.
                let following_url = Self::resource_url(&calendar.remote_id, &updated.uid);
                self.put(
                    &following_url,
                    &nuncio_ical::ics::write_event(updated, None),
                    None,
                    true,
                )
                .await
            }
        }
    }

    pub async fn delete_event(
        &self,
        calendar: &RemoteCalendar,
        event: &StoredEvent,
        series: &[StoredEvent],
        scope: RecurrenceEditScope,
        instance_start: Option<&str>,
    ) -> Result<()> {
        let master = series
            .iter()
            .find(|stored| stored.event.series_id.is_none())
            .unwrap_or(event);
        let master_url = master
            .href
            .clone()
            .unwrap_or_else(|| Self::resource_url(&calendar.remote_id, &master.event.uid));

        match scope {
            RecurrenceEditScope::All => {
                let mut headers = Vec::new();
                if let Some(etag) = master.etag.as_deref() {
                    headers.push(("If-Match".to_string(), if_match(etag)));
                }
                let response = self
                    .send(&Method::DELETE, &master_url, &headers, None)
                    .await?;
                let status = response.status();
                if !status.is_success() && status != StatusCode::NOT_FOUND {
                    return Err(status_error(
                        status,
                        &response.text().await.unwrap_or_default(),
                    ));
                }
                Ok(())
            }
            RecurrenceEditScope::This => {
                let instance_start = instance_start
                    .or(event.event.recurrence_id.as_deref())
                    .ok_or_else(|| {
                        invalid("instance start is required for deleting one occurrence")
                    })?;
                let mut updated = master.clone();
                updated.exdates = master.exdates.clone();
                nuncio_ical::recurrence::add_exdate(&mut updated.exdates, instance_start);
                let list: Vec<StoredEvent> =
                    series
                        .iter()
                        .filter(|stored| {
                            !stored.event.recurrence_id.as_deref().is_some_and(|value| {
                                crate::util::same_instant(value, instance_start)
                            })
                        })
                        .map(|stored| {
                            if stored.event.id == master.event.id {
                                updated.clone()
                            } else {
                                stored.clone()
                            }
                        })
                        .collect();
                let composed = if list.is_empty() {
                    compose_ics(&[updated])
                } else {
                    compose_ics(&list)
                };
                self.put(&master_url, &composed, master.etag.as_deref(), false)
                    .await
            }
            RecurrenceEditScope::ThisAndFollowing => {
                let instance_start =
                    instance_start.ok_or_else(|| invalid("instance start is required"))?;
                let split =
                    nuncio_ical::recurrence::split_series_at(&master.event, instance_start)?;
                let mut truncated = master.clone();
                truncated.event = split.master;
                truncated.raw_ics = None;
                truncated.rrule = truncated
                    .event
                    .recurrence
                    .as_ref()
                    .map(nuncio_ical::recurrence::to_rrule_string);
                let mut list = filter_after_split(series, &master.event.id, instance_start);
                for item in &mut list {
                    if item.event.id == master.event.id {
                        *item = truncated.clone();
                    }
                }
                if list.is_empty() {
                    list.push(truncated.clone());
                }
                self.put(
                    &master_url,
                    &compose_ics(&list),
                    master.etag.as_deref(),
                    false,
                )
                .await
            }
        }
    }

    pub async fn respond_event(
        &self,
        _calendar: &RemoteCalendar,
        event: &StoredEvent,
        series: &[StoredEvent],
        response: AttendeeStatus,
        email: &str,
    ) -> Result<()> {
        let master = series
            .iter()
            .find(|stored| stored.event.series_id.is_none())
            .unwrap_or(event);
        let master_url = master
            .href
            .clone()
            .ok_or_else(|| invalid("CalDAV event has no resource URL"))?;
        let mut updated_master = master.clone();
        set_response(&mut updated_master.event, email, response);
        let list: Vec<StoredEvent> = series
            .iter()
            .map(|stored| {
                if stored.event.id == master.event.id {
                    updated_master.clone()
                } else {
                    stored.clone()
                }
            })
            .collect();
        let composed = if list.is_empty() {
            compose_ics(&[updated_master])
        } else {
            compose_ics(&list)
        };
        self.put(&master_url, &composed, master.etag.as_deref(), false)
            .await
    }

    /// Resolve a server-provided href against `base`.
    ///
    /// Absolute hrefs are only honoured when their scheme/host/port match one
    /// of the configured DAV origins.  This prevents a malicious or
    /// compromised server from redirecting authenticated PROPFIND/PUT requests
    /// (which carry Basic/Bearer credentials) to another origin or the
    /// intranet.  Cross-origin hrefs are ignored and the base URL is kept.
    fn resolve_url(&self, base: &str, href: &str) -> String {
        resolve_href(base, href, &self.allowed_origins())
    }

    /// Origins the account is allowed to talk to: the resolved calendar and
    /// address-book URLs.
    ///
    /// The directory defaults count because [`home_url`] sends the first
    /// PROPFIND to them for accounts with no user-entered URL.  The mail
    /// domain is deliberately NOT an allowed origin: it was only needed by the
    /// removed `.well-known` guess.
    fn allowed_origins(&self) -> Vec<Url> {
        let mut allowed: Vec<Url> = Vec::new();
        for service in [DavService::Calendar, DavService::Contacts] {
            if let Some(value) = providers::dav_url(&self.account, service) {
                if let Ok(url) = Url::parse(&value) {
                    allowed.push(url);
                }
            }
        }
        allowed
    }
}

/// URL that [`DavClient::discover_home`] starts from.
///
/// It is the user's own value when set, otherwise the verified directory
/// entry.  `None` means the account has no DAV service; we must never derive a
/// host from the mail domain.  Yandex is the cautionary example: mail lives on
/// `*.yandex.com` but the DAV servers are `*.yandex.ru`, and
/// `https://yandex.com/.well-known/caldav` answers 405.
fn home_url(account: &Account, kind: DavKind) -> Option<String> {
    let service = match kind {
        DavKind::Calendar => DavService::Calendar,
        DavKind::AddressBook => DavService::Contacts,
    };
    providers::dav_url(account, service).map(|url| ensure_trailing_slash(&url))
}

/// Resolve a server-provided href against `base`, ignoring absolute hrefs that
/// point outside the allowed origins.
fn resolve_href(base: &str, href: &str, allowed: &[Url]) -> String {
    let href = href.trim();
    if href.starts_with("http://") || href.starts_with("https://") {
        return match Url::parse(href) {
            Ok(url) if origin_allowed(allowed, &url) => url.to_string(),
            Ok(url) => {
                warn!(
                    origin = %url.origin().ascii_serialization(),
                    "ignoring cross-origin DAV href"
                );
                base.to_string()
            }
            Err(_) => base.to_string(),
        };
    }
    match Url::parse(base).and_then(|base| base.join(href)) {
        Ok(url) if origin_allowed(allowed, &url) => url.to_string(),
        Ok(url) => {
            // Protocol-relative hrefs (`//host/path`) can change the origin.
            warn!(
                origin = %url.origin().ascii_serialization(),
                "ignoring cross-origin DAV href"
            );
            base.to_string()
        }
        Err(_) => href.to_string(),
    }
}

fn origin_allowed(allowed: &[Url], candidate: &Url) -> bool {
    if allowed.is_empty() {
        // No configured origin to compare against: fail closed.
        return false;
    }
    allowed.iter().any(|origin| {
        origin.scheme().eq_ignore_ascii_case(candidate.scheme())
            && origin.port_or_known_default() == candidate.port_or_known_default()
            && host_matches(origin.host_str(), candidate.host_str())
    })
}

/// Host equality or same registrable domain, so sharded hosts such as
/// `p42-caldav.icloud.com` are accepted for a configured `caldav.icloud.com`
/// while `evil.example.net` is not accepted for `example.com`.
fn host_matches(allowed: Option<&str>, candidate: Option<&str>) -> bool {
    let (Some(allowed), Some(candidate)) = (allowed, candidate) else {
        return false;
    };
    if allowed.eq_ignore_ascii_case(candidate) {
        return true;
    }
    match (registrable_domain(allowed), registrable_domain(candidate)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// Approximate registrable domain (last two labels, or three for common
/// two-level suffixes such as `co.uk`).  IP addresses compare verbatim.
fn registrable_domain(host: &str) -> Option<String> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Some(host);
    }
    let labels: Vec<&str> = host.split('.').filter(|label| !label.is_empty()).collect();
    if labels.len() <= 2 {
        return Some(labels.join("."));
    }
    let second_last = labels[labels.len() - 2];
    let take = if second_last.len() == 2
        && matches!(
            second_last,
            "co" | "com" | "net" | "org" | "gov" | "ac" | "edu"
        ) {
        3
    } else {
        2
    };
    Some(labels[labels.len() - take..].join("."))
}

/// Test helper: compare scheme, host and effective port.
#[cfg(test)]
mod origin_tests {
    use super::*;

    fn allowed() -> Vec<Url> {
        vec![
            Url::parse("https://dav.example.com/cal/").expect("url"),
            Url::parse("https://example.com/").expect("url"),
        ]
    }

    #[test]
    fn accepts_same_origin_and_relative_hrefs() {
        let base = "https://dav.example.com/cal/";
        assert_eq!(
            resolve_href(base, "https://dav.example.com/cal/other/", &allowed()),
            "https://dav.example.com/cal/other/"
        );
        assert_eq!(
            resolve_href(base, "/cal/other/", &allowed()),
            "https://dav.example.com/cal/other/"
        );
        assert_eq!(
            resolve_href(base, "sub/", &allowed()),
            "https://dav.example.com/cal/sub/"
        );
    }

    #[test]
    fn rejects_cross_origin_absolute_hrefs() {
        let base = "https://dav.example.com/cal/";
        // A hostile calendar-home-set must not receive the account credentials.
        assert_eq!(
            resolve_href(base, "https://evil.example.net/steal/", &allowed()),
            base
        );
        // Scheme downgrade is also rejected.
        assert_eq!(
            resolve_href(base, "http://dav.example.com/cal/", &allowed()),
            base
        );
        // Protocol-relative hrefs must not escape the allowed origin either.
        assert_eq!(
            resolve_href(base, "//evil.example.net/steal/", &allowed()),
            base
        );
        // Unconfigured accounts fail closed.
        assert_eq!(
            resolve_href(base, "https://dav.example.com/cal/", &[]),
            base
        );
    }

    #[test]
    fn host_matching_only_accepts_related_domains() {
        assert!(host_matches(
            Some("example.com"),
            Some("caldav.example.com")
        ));
        assert!(host_matches(
            Some("caldav.example.com"),
            Some("example.com")
        ));
        assert!(host_matches(Some("EXAMPLE.com"), Some("example.com")));
        // iCloud shards live on sibling hosts under the same registrable domain.
        assert!(host_matches(
            Some("caldav.icloud.com"),
            Some("p42-caldav.icloud.com")
        ));
        assert!(host_matches(
            Some("example.co.uk"),
            Some("dav.example.co.uk")
        ));
        assert!(!host_matches(Some("example.com"), Some("evilexample.com")));
        assert!(!host_matches(Some("example.com"), Some("example.net")));
        assert!(!host_matches(None, Some("example.com")));
    }

    fn same_origin(left: &Url, right: &Url) -> bool {
        left.scheme().eq_ignore_ascii_case(right.scheme())
            && left.host_str().map(str::to_ascii_lowercase)
                == right.host_str().map(str::to_ascii_lowercase)
            && left.port_or_known_default() == right.port_or_known_default()
    }

    #[test]
    fn same_origin_compares_scheme_host_port() {
        assert!(same_origin(
            &Url::parse("https://a.example.com/x").expect("url"),
            &Url::parse("https://a.example.com:443/y").expect("url")
        ));
        assert!(!same_origin(
            &Url::parse("https://a.example.com/x").expect("url"),
            &Url::parse("http://a.example.com/y").expect("url")
        ));
    }
}

async fn ensure_ok(response: Response) -> Result<()> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    Err(status_error(status, &body))
}

fn method(name: &str) -> Result<Method> {
    Method::from_bytes(name.as_bytes())
        .map_err(|error| protocol(format!("invalid HTTP method {name}: {error}")))
}

fn ensure_trailing_slash(value: &str) -> String {
    if value.ends_with('/') {
        value.to_string()
    } else {
        format!("{value}/")
    }
}

fn same_href(left: &str, right: &str) -> bool {
    left.trim_end_matches('/') == right.trim_end_matches('/')
}

fn last_segment(href: &str) -> String {
    let trimmed = href.trim_end_matches('/');
    let segment = trimmed.rsplit('/').next().unwrap_or(trimmed);
    urlencoding::decode(segment)
        .map(|value| value.into_owned())
        .unwrap_or_else(|_| segment.to_string())
}

fn if_match(etag: &str) -> String {
    let cleaned = etag.trim().trim_start_matches("W/").trim_matches('"');
    format!("\"{cleaned}\"")
}

fn is_fallback_error(error: &crate::error::AppError) -> bool {
    matches!(
        error,
        crate::error::AppError::Unsupported(_)
            | crate::error::AppError::Protocol(_)
            | crate::error::AppError::InvalidInput(_)
    )
}

/// `CalendarProvider` implementation.
#[derive(Clone)]
pub struct DavProvider {
    http: HttpClient,
}

impl std::fmt::Debug for DavProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DavProvider").finish_non_exhaustive()
    }
}

impl DavProvider {
    pub fn new(http: HttpClient) -> Self {
        Self { http }
    }

    fn client(&self, account: &Account) -> DavClient {
        DavClient::new(self.http.clone(), account.clone())
    }
}

#[async_trait]
impl CalendarProvider for DavProvider {
    fn name(&self) -> &'static str {
        "caldav"
    }

    async fn list_calendars(&self, account: &Account) -> Result<Vec<RemoteCalendar>> {
        self.client(account).list_calendars().await
    }

    async fn create_calendar(
        &self,
        account: &Account,
        name: &str,
        color: &str,
        _time_zone: &str,
    ) -> Result<RemoteCalendar> {
        self.client(account)
            .create_calendar(name, Some(color))
            .await
    }

    async fn update_calendar(
        &self,
        account: &Account,
        remote: &RemoteCalendar,
        name: Option<&str>,
        color: Option<&str>,
    ) -> Result<()> {
        self.client(account)
            .update_calendar(&remote.remote_id, name, color)
            .await
    }

    async fn delete_calendar(&self, account: &Account, remote: &RemoteCalendar) -> Result<()> {
        self.client(account)
            .delete_calendar(&remote.remote_id)
            .await
    }

    async fn sync_events(
        &self,
        account: &Account,
        remote: &RemoteCalendar,
        cursor: &CalendarCursor,
        window: EventWindow,
    ) -> Result<EventDelta> {
        self.client(account)
            .sync_events(remote, cursor, window)
            .await
    }

    async fn create_event(
        &self,
        account: &Account,
        remote: &RemoteCalendar,
        event: &CalendarEvent,
        _notify_attendees: bool,
    ) -> Result<()> {
        self.client(account).create_event(remote, event).await
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
        self.client(account)
            .update_event(remote, current, series, updated, scope, instance_start)
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
        self.client(account)
            .delete_event(remote, event, series, scope, instance_start)
            .await
    }

    async fn respond_event(
        &self,
        account: &Account,
        remote: &RemoteCalendar,
        event: &StoredEvent,
        series: &[StoredEvent],
        response: AttendeeStatus,
        _comment: Option<&str>,
        _send_reply: bool,
    ) -> Result<()> {
        self.client(account)
            .respond_event(remote, event, series, response, &account.email)
            .await
    }
}

/// Test helper: chosen HTTP method name for a scope (documentation aid).
pub fn scope_name(scope: RecurrenceEditScope) -> &'static str {
    match scope {
        RecurrenceEditScope::This => "this",
        RecurrenceEditScope::ThisAndFollowing => "thisAndFollowing",
        RecurrenceEditScope::All => "all",
    }
}

#[cfg(test)]
mod home_url_tests {
    use super::*;
    use nuncio_core::models::{AccountState, ProviderCapabilities, ProviderKind};

    fn account(email: &str) -> Account {
        Account {
            id: "test".into(),
            provider: ProviderKind::Imap,
            email: email.into(),
            display_name: "Test".into(),
            color: "#000000".into(),
            auth_kind: AuthKind::Password,
            oauth_provider: None,
            imap: None,
            smtp: None,
            pop3: None,
            caldav_url: None,
            carddav_url: None,
            sent_copy: None,
            sync_window_days: 90,
            mail_enabled: true,
            calendar_enabled: true,
            contacts_enabled: true,
            tasks_enabled: true,
            notifications_enabled: true,
            include_in_unified: true,
            sort_order: 0,
            state: AccountState::Ok,
            state_message: None,
            capabilities: ProviderCapabilities::default(),
            created_at: nuncio_core::models::now(),
        }
    }

    #[test]
    fn yandex_without_a_stored_url_starts_at_the_ru_dav_host() {
        let yandex = account("me@yandex.com");
        let start = home_url(&yandex, DavKind::Calendar).expect("calendar URL");
        assert_eq!(start, "https://caldav.yandex.ru/");
        // The removed domain guess produced https://yandex.com/... (405).
        assert!(!start.contains("yandex.com"));
        assert_eq!(
            home_url(&yandex, DavKind::AddressBook).as_deref(),
            Some("https://carddav.yandex.ru/")
        );
    }

    #[test]
    fn user_url_wins_and_a_missing_url_yields_nothing() {
        let mut custom = account("me@yandex.com");
        custom.caldav_url = Some("https://dav.example.com/cal".into());
        assert_eq!(
            home_url(&custom, DavKind::Calendar).as_deref(),
            Some("https://dav.example.com/cal/")
        );

        // An unknown domain with no URL has no DAV service at all; the client
        // must report "account has no DAV server URL" rather than guess.
        let unknown = account("me@unknown.example");
        assert_eq!(home_url(&unknown, DavKind::Calendar), None);
        assert_eq!(home_url(&unknown, DavKind::AddressBook), None);
    }
}
