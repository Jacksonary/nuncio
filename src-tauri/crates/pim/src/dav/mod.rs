//! CalDAV / WebDAV client (RFC 4791, RFC 6578).
//!
//! Discovery follows `/.well-known/caldav` → `current-user-principal` →
//! `calendar-home-set` → `PROPFIND Depth: 1`.  Incremental sync prefers
//! `sync-collection` and falls back to `getctag` + per-resource ETag
//! comparison.  All XML handling is generic so stage 2 can reuse it for
//! CardDAV (`addressbook-multiget`, `addressbook-home-set`).

pub mod map;
pub mod xml;

use std::collections::{BTreeMap, HashMap, HashSet};

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
    proppatch_body, sync_collection_body, DavResponse, Multistatus,
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

/// Result of one `calendar-multiget`/`addressbook-multiget` REPORT.
#[derive(Debug, Clone, Default)]
pub struct MultigetResult {
    /// Resources the server returned with a body.
    pub resources: Vec<DavResource>,
    /// Hrefs the server explicitly reported as `404`.
    ///
    /// This is newer evidence than the listing that requested the multiget: an
    /// object can disappear between the PROPFIND and the REPORT, so a `404`
    /// here overrides "the listing named it".
    pub missing: Vec<String>,
}

/// A batch of changes for a non-calendar DAV collection (CardDAV, VTODO).
#[derive(Debug, Clone, Default)]
pub struct ResourceDelta {
    pub changed: Vec<DavResource>,
    pub deleted_hrefs: Vec<String>,
    /// Every href the server listed in this pass, including objects whose ETag
    /// matched the local baseline and were therefore not fetched into
    /// [`Self::changed`], and objects whose ETag could not be read.
    ///
    /// Only a full collection listing can fill this in (the ETag fallback).
    /// An incremental `sync-collection` response is not a listing, so it stays
    /// empty. Consumers pruning from a [`Self::full_sync`] batch must use this
    /// set to tell "listed but unchanged" apart from "no longer on the server".
    pub listed_hrefs: Vec<String>,
    /// The listing named at least one object whose identity could not be read
    /// this round (for example a per-resource `503` with no ETag).
    ///
    /// Such an object is *present* (it must not be pruned) but its ETag was not
    /// refreshed, so consumers must not advance the sync cursor: the next pass
    /// has to retry instead of trusting a stale identity.
    pub incomplete: bool,
    pub sync_token: Option<String>,
    pub ctag: Option<String>,
    pub full_sync: bool,
}

/// Upper bound on RFC 6578 §3.6 pages consumed by one incremental sync.
///
/// A truncating server is required to return a new token that moves forward,
/// and a repeated token is detected separately; this cap only protects against
/// a server that manufactures an endless chain of distinct tokens.
const MAX_SYNC_PAGES: usize = 10_000;

/// One page of a `sync-collection` REPORT.
struct SyncPage {
    members: Vec<SyncMember>,
    /// The token that acknowledges this page (body token wins over the
    /// header). `None` means the server sent no token at all.
    next_token: Option<String>,
    /// RFC 6578 §3.6: the collection's own response carried `507`, so the
    /// result set was truncated and another page must be fetched with
    /// [`Self::next_token`].
    truncated: bool,
    /// Some response could not be read this round (for example a per-resource
    /// `503` with an empty `getetag`); the token must not be acknowledged.
    incomplete: bool,
}

struct SyncMember {
    href: String,
    missing: bool,
    is_collection: bool,
    etag: Option<String>,
}

/// The newest thing a truncated `sync-collection` round learned about one href.
///
/// RFC 6578 pages are a single ordered stream: a later page's evidence is
/// newer than an earlier page's. Keeping the evidence per href and letting a
/// later entry overwrite an earlier one is what stops a deletion on page N from
/// being resurrected by a body fetched for the same href on page N-1.
enum HrefEvidence {
    /// The newest evidence says the resource is gone.
    Missing,
    /// The newest evidence named the resource but could not read its identity.
    /// Withhold both the earlier body and the earlier deletion: the round is
    /// incomplete and will be retried from the old cursor.
    Unreadable,
    /// The newest evidence is a fetched body.
    Fetched(DavResource),
}

/// Href-keyed merge of page evidence that preserves the order in which hrefs
/// were first seen, so the applied batch stays deterministic.
///
/// Keys are RFC 3986 §6.2.2 normal forms ([`href_key`]) but the stored value
/// keeps the newest raw href, because that is what requests and the database
/// use. A server may spell the same resource `%41.ics` on one page and
/// `A.ics` on the next; both must address one merge entry.
#[derive(Default)]
struct HrefMerge {
    /// Normalized keys in first-seen order.
    order: Vec<String>,
    /// Normalized key -> (newest raw href, newest evidence).
    entries: HashMap<String, (String, HrefEvidence)>,
}

impl HrefMerge {
    fn record(&mut self, href: String, evidence: HrefEvidence) {
        let key = href_key(&href);
        if !self.entries.contains_key(&key) {
            self.order.push(key.clone());
        }
        self.entries.insert(key, (href, evidence));
    }
}

/// Apply merged page evidence to a calendar delta. Only the newest evidence for
/// each href is emitted, so a later `404` removes an earlier body.
fn materialize_event_evidence(
    merged: HrefMerge,
    account: &Account,
    cursor: &CalendarCursor,
    delta: &mut EventDelta,
) {
    let HrefMerge { order, mut entries } = merged;
    for key in order {
        match entries.remove(&key) {
            // Emit the newest raw form the server used; normalization is only
            // an identity key.
            Some((href, HrefEvidence::Missing)) => delta.deleted_hrefs.push(href),
            Some((_, HrefEvidence::Fetched(resource))) => match events_from_ics(
                &resource.data,
                &resource.href,
                resource.etag.as_deref(),
                &cursor.local_calendar_id,
                &account.id,
                &account.email,
            ) {
                Ok(events) => delta.events.extend(events),
                Err(error) => {
                    warn!(href = %resource.href, %error, "skipping invalid CalDAV resource")
                }
            },
            Some((_, HrefEvidence::Unreadable)) | None => {}
        }
    }
}

/// Apply merged page evidence to a generic (CardDAV/VTODO) delta.
fn materialize_resource_evidence(merged: HrefMerge, delta: &mut ResourceDelta) {
    let HrefMerge { order, mut entries } = merged;
    for key in order {
        match entries.remove(&key) {
            Some((href, HrefEvidence::Missing)) => delta.deleted_hrefs.push(href),
            Some((_, HrefEvidence::Fetched(resource))) => delta.changed.push(resource),
            Some((_, HrefEvidence::Unreadable)) | None => {}
        }
    }
}

/// How one `multistatus` response relates to the collection being synced.
enum ResponseRole {
    /// The collection's own response.
    Collection {
        /// `507` for the request-URI: a truncated page, not a failure.
        truncated: bool,
        /// A real failure status for the collection; the page is not
        /// authoritative.
        failed: bool,
    },
    /// A member resource, resolved to an absolute href.
    Member(String),
}

/// Result of one `sync-collection` REPORT.
enum PageOutcome {
    Page(SyncPage),
    /// The HTTP request itself was not successful; the body is kept for
    /// [`status_error`].
    Status(StatusCode, String),
}

/// One authoritative `Depth: 1` ETag listing of a collection.
#[derive(Debug, Clone, Default)]
pub struct CollectionState {
    pub ctag: Option<String>,
    pub sync_token: Option<String>,
    /// Member resources whose ETag could be read.
    pub etags: Vec<(String, String)>,
    /// Every member href the server listed, including resources whose ETag
    /// was unavailable this round.
    pub listed_hrefs: Vec<String>,
    /// At least one listed resource had no readable ETag.
    pub incomplete: bool,
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

        // The fallback is a complete `Depth: 1` listing. Presence must come
        // from the listed hrefs, not from "its ETag was readable": a
        // per-resource `503` still proves the object exists, and treating it
        // as absent used to push the href into `deleted_hrefs`. The shared
        // `CollectionState` gives us the listing plus the `incomplete` flag so
        // the cursor is retried instead of acknowledged past an unread
        // identity (same semantics as CardDAV's `sync_resources`).
        let state = self
            .collection_etags(DavKind::Calendar, &collection)
            .await?;
        let collection_url = ensure_trailing_slash(&collection);
        let new_ctag = state.ctag.clone();
        let new_sync_token = state
            .sync_token
            .clone()
            .or_else(|| cursor.sync_token.clone());

        if cursor.ctag.is_some() && cursor.ctag == new_ctag {
            if state.incomplete {
                delta.ctag = cursor.ctag.clone();
                delta.sync_token = cursor.sync_token.clone();
            } else {
                delta.ctag = new_ctag;
                delta.sync_token = new_sync_token;
            }
            delta.incomplete = state.incomplete;
            delta.full_sync = false;
            return Ok(delta);
        }

        // Compare baselines by identity key, not by spelling: the same
        // resource may be listed as `%41.ics` in one round and `A.ics` in the
        // next without being a change or a deletion.
        let local: HashMap<String, (String, String)> = cursor
            .etags
            .iter()
            .map(|(href, etag)| (href_key(href), (href.clone(), etag.clone())))
            .collect();
        let mut changed = Vec::new();
        for (href, etag) in &state.etags {
            if local.get(&href_key(href)).map(|(_, local_etag)| local_etag) != Some(etag) {
                changed.push(href.clone());
            }
        }
        // Only an href missing from the listing is a deletion. A listed object
        // whose ETag could not be read keeps its local row untouched.
        let listed: HashSet<String> = state
            .listed_hrefs
            .iter()
            .map(|href| href_key(href))
            .collect();
        for (key, (href, _)) in &local {
            if !listed.contains(key) {
                delta.deleted_hrefs.push(href.clone());
            }
        }
        if !changed.is_empty() {
            let fetched = self
                .multiget(DavKind::Calendar, &collection_url, &changed)
                .await?;
            // A multiget 404 is newer evidence than this listing.
            delta.deleted_hrefs.extend(fetched.missing);
            for resource in fetched.resources {
                match events_from_ics(
                    &resource.data,
                    &resource.href,
                    resource.etag.as_deref(),
                    &calendar.local_id,
                    &self.account.id,
                    &self.account.email,
                ) {
                    Ok(events) => delta.events.extend(events),
                    Err(error) => {
                        warn!(href = %resource.href, %error, "skipping invalid CalDAV resource")
                    }
                }
            }
        }
        delta.incomplete = state.incomplete;
        if state.incomplete {
            // Never acknowledge a listing that left an identity unread.
            delta.ctag = cursor.ctag.clone();
            delta.sync_token = cursor.sync_token.clone();
        } else {
            delta.ctag = new_ctag;
            delta.sync_token = new_sync_token;
        }
        delta.full_sync = false;
        Ok(delta)
    }

    /// Fetch and classify one `sync-collection` page.
    async fn request_sync_page(&self, collection: &str, token: &str) -> Result<PageOutcome> {
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
        let status = response.status();
        if !status.is_success() {
            return Ok(PageOutcome::Status(
                status,
                response.text().await.unwrap_or_default(),
            ));
        }
        let header_token = response
            .headers()
            .get("sync-token")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let text = response.text().await.unwrap_or_default();
        let multistatus = parse_multistatus(&text)?;
        let collection_url = ensure_trailing_slash(collection);
        let collection_href = self.collection_identity(&collection_url)?;
        let mut page = SyncPage {
            members: Vec::new(),
            next_token: multistatus
                .sync_token
                .or(header_token)
                .filter(|token| !token.trim().is_empty()),
            truncated: false,
            incomplete: false,
        };
        for item in &multistatus.responses {
            match self.classify_response(&collection_url, &collection_href, item)? {
                ResponseRole::Collection { truncated, failed } => {
                    // RFC 6578 §3.6: `507` for the collection means this page
                    // was truncated, not that the report failed.
                    page.truncated |= truncated;
                    page.incomplete |= failed;
                }
                ResponseRole::Member(href) => page.members.push(SyncMember {
                    href,
                    missing: item.is_missing(),
                    is_collection: item.prop.is_collection,
                    etag: item.prop.etag.clone(),
                }),
            }
        }
        Ok(PageOutcome::Page(page))
    }

    /// Identity of the collection itself, with no fallback to a guessed href.
    ///
    /// If the configured collection URL cannot be confirmed against the
    /// account's allowed origins, no member href can be attributed either, so
    /// the caller must fail the round instead of diffing against a guess.
    fn collection_identity(&self, collection_url: &str) -> Result<String> {
        self.try_resolve_url(collection_url, collection_url)
            .ok_or_else(|| {
                protocol(format!(
                    "cannot confirm the identity of DAV collection {collection_url}; refusing to diff its members"
                ))
            })
    }

    /// Classify one `multistatus` response against the collection identity.
    ///
    /// The href must resolve without guessing. An empty href, a malformed href
    /// or an href outside the account's allowed origins cannot be attributed
    /// to the collection; treating one as the collection made a listing
    /// without a real collection response look empty and deleted every cached
    /// member.
    fn classify_response(
        &self,
        collection_url: &str,
        collection_href: &str,
        item: &DavResponse,
    ) -> Result<ResponseRole> {
        let href = self
            .try_resolve_url(collection_url, &item.href)
            .ok_or_else(|| {
                protocol(format!(
                    "DAV response href {:?} cannot be attributed to collection {collection_url}; the listing is not authoritative",
                    item.href
                ))
            })?;
        if same_href(&href, collection_href) {
            let truncated = item.status == Some(507);
            Ok(ResponseRole::Collection {
                truncated,
                failed: !truncated && collection_response_failed(item),
            })
        } else if href_in_collection(collection_href, &href) {
            Ok(ResponseRole::Member(href))
        } else {
            // Being on an allow-listed origin is not membership. A response
            // for `/other-book/x.vcf` must not be diffed as a member of
            // `/ab/`: importing it would cross address books, and pruning
            // against it could delete a local row that belongs elsewhere.
            Err(protocol(format!(
                "DAV response href {href} is outside collection {collection_url}; the listing is not authoritative"
            )))
        }
    }

    async fn sync_collection(
        &self,
        collection: &str,
        token: &str,
        cursor: &CalendarCursor,
    ) -> Result<Option<EventDelta>> {
        let collection_url = ensure_trailing_slash(collection);
        let mut delta = EventDelta::default();
        // Pages are one ordered change stream. Merge per href so the newest
        // page wins: a page-2 `404` must erase a body fetched for the same
        // href from page 1 instead of emitting both a body and a deletion.
        let mut merged = HrefMerge::default();
        let mut page_token = token.to_string();
        let mut seen: HashSet<String> = HashSet::new();
        while seen.len() < MAX_SYNC_PAGES {
            seen.insert(page_token.clone());
            let page = match self.request_sync_page(collection, &page_token).await? {
                PageOutcome::Status(status, body) => {
                    return if matches!(status.as_u16(), 400 | 403 | 404 | 409 | 412 | 501) {
                        Ok(None)
                    } else {
                        Err(status_error(status, &body))
                    };
                }
                PageOutcome::Page(page) => page,
            };
            let mut changed = Vec::new();
            for member in page.members {
                if member.missing {
                    merged.record(member.href, HrefEvidence::Missing);
                    continue;
                }
                if member.is_collection {
                    continue;
                }
                match member.etag {
                    // The member changed; its body has to be fetched.
                    Some(_) => changed.push(member.href),
                    // The server named a change but did not let us read its
                    // identity (for example a per-resource `503`). It is not a
                    // deletion, but it is also not safe to acknowledge: keep
                    // the cursor before this batch so the change is delivered
                    // again. It also supersedes any older evidence for this
                    // href, so neither an old body nor an old deletion is
                    // applied.
                    None => {
                        merged.record(member.href, HrefEvidence::Unreadable);
                        delta.incomplete = true;
                    }
                }
            }
            if !changed.is_empty() {
                let fetched = self
                    .multiget(DavKind::Calendar, &collection_url, &changed)
                    .await?;
                // A multiget 404 is newer evidence than the sync-collection
                // report; a fetched body is equally newer than an earlier
                // page's deletion.
                for href in fetched.missing {
                    merged.record(href, HrefEvidence::Missing);
                }
                for resource in fetched.resources {
                    merged.record(resource.href.clone(), HrefEvidence::Fetched(resource));
                }
            }
            delta.incomplete |= page.incomplete;
            // An incomplete page, or a truncation with no token to continue
            // from, must not acknowledge anything past the cursor we had: the
            // unread change would never be requested again.
            if delta.incomplete || (page.truncated && page.next_token.is_none()) {
                delta.incomplete = true;
                materialize_event_evidence(merged, &self.account, cursor, &mut delta);
                delta.sync_token = Some(token.to_string());
                delta.ctag = cursor.ctag.clone();
                return Ok(Some(delta));
            }
            let next = page.next_token.unwrap_or_else(|| page_token.clone());
            if !page.truncated {
                materialize_event_evidence(merged, &self.account, cursor, &mut delta);
                delta.sync_token = Some(next);
                delta.ctag = cursor.ctag.clone();
                return Ok(Some(delta));
            }
            if seen.contains(&next) {
                // A truncation that does not move the token forward would
                // request the same page forever.
                delta.incomplete = true;
                materialize_event_evidence(merged, &self.account, cursor, &mut delta);
                delta.sync_token = Some(token.to_string());
                delta.ctag = cursor.ctag.clone();
                return Ok(Some(delta));
            }
            page_token = next;
        }
        Err(protocol(format!(
            "sync-collection on {collection_url} exceeded {MAX_SYNC_PAGES} truncated pages"
        )))
    }

    /// Fetch one or more resources.  `kind` selects `calendar-multiget` or
    /// `addressbook-multiget` (stage 2).
    ///
    /// The result distinguishes an explicit `404` (the object is gone) from
    /// every other failure. A `5xx`, a timeout, a malformed response or a
    /// requested href that the server did not answer at all leaves the state
    /// *unknown*; the whole call fails so the caller retries instead of
    /// treating the object as either present or deleted.
    pub async fn multiget(
        &self,
        kind: DavKind,
        collection: &str,
        hrefs: &[String],
    ) -> Result<MultigetResult> {
        let mut out = MultigetResult::default();
        for batch in hrefs.chunks(50) {
            let headers = vec![(
                "Content-Type".to_string(),
                "application/xml; charset=utf-8".to_string(),
            )];
            let method = method("REPORT")?;
            // Normalize the requested hrefs once. Every response is compared
            // against this exact set of *identity keys* (RFC 3986 §6.2.2), so
            // a body or a `404` for an href we did not ask about can never
            // become sync evidence for this collection, while a server that
            // answers `%41.ics` for a requested `A.ics` (or vice versa) is
            // still understood as answering the same resource.
            let mut requested = Vec::with_capacity(batch.len());
            let mut requested_keys: HashSet<String> = HashSet::new();
            for href in batch {
                let resolved = self.try_resolve_url(collection, href).ok_or_else(|| {
                    protocol(format!(
                        "multiget href {href} cannot be attributed to collection {collection}"
                    ))
                })?;
                let key = href_key(&resolved);
                // Two spellings of one resource must be requested once only.
                if requested_keys.insert(key) {
                    requested.push(resolved);
                }
            }
            let references: Vec<String> = requested
                .iter()
                .map(|href| request_href(collection, href))
                .collect();
            let response = self
                .send(
                    &method,
                    collection,
                    &headers,
                    Some(&multiget_body(kind.home_kind(), &references)),
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
            let mut answered: HashSet<String> = HashSet::new();
            // The last response for an href wins, just like pages: a later
            // `404` erases an earlier body and a later body erases an earlier
            // `404`. Keyed by identity so equivalent spellings collapse.
            let mut outcomes: BTreeMap<String, (String, Option<DavResource>)> = BTreeMap::new();
            for item in multistatus.responses {
                if item.href.trim().is_empty() {
                    return Err(protocol(
                        "multiget returned a resource without an href; the batch state is uncertain",
                    ));
                }
                let Some(href) = self.try_resolve_url(collection, &item.href) else {
                    // A malformed or cross-origin href cannot be one of the
                    // requested resources; it carries no evidence.
                    warn!(
                        href = %item.href,
                        "ignoring a multiget response that is not attributed to this collection"
                    );
                    continue;
                };
                let key = href_key(&href);
                if !requested_keys.contains(&key) {
                    // The server answered a resource outside this batch. It
                    // must not be imported, and its `404` must not delete a
                    // local object.
                    warn!(
                        %href,
                        "ignoring a multiget response for an unrequested href"
                    );
                    continue;
                }
                answered.insert(key.clone());
                if item
                    .status
                    .is_some_and(|status| !(200..300).contains(&status) && status != 404)
                {
                    // A response-level 5xx (or similar) is not proof of
                    // deletion and the body cannot be trusted either.
                    return Err(protocol(format!(
                        "multiget returned status {} for {href}; refusing an incomplete batch",
                        item.status.unwrap_or_default()
                    )));
                }
                // An empty element is not a body. A server can answer a
                // requested property with a failed `propstat` (or a bare
                // `<address-data/>`), and treating that as delivered data made
                // the sync apply "zero objects" and then advance its cursor.
                let missing = item.is_missing();
                let etag = item.prop.etag;
                let outcome = match item.prop.data.filter(|data| !data.trim().is_empty()) {
                    Some(data) => Some(DavResource {
                        href: href.clone(),
                        etag,
                        data,
                    }),
                    // Only an explicit 404 is proof that the object is gone.
                    None if missing => None,
                    None => {
                        return Err(protocol(format!(
                            "multiget returned no data for {href}; the resource state is uncertain"
                        )))
                    }
                };
                outcomes.insert(key, (href, outcome));
            }
            // A server that silently omits a requested href leaves us unable
            // to decide; do not advance past it. Responses for unrequested
            // hrefs do not count as answers.
            for href in &requested {
                if !answered.contains(&href_key(href)) {
                    return Err(protocol(format!(
                        "multiget omitted {href}; the resource state is uncertain"
                    )));
                }
            }
            for (_, (href, outcome)) in outcomes {
                match outcome {
                    Some(resource) => out.resources.push(resource),
                    None => out.missing.push(href),
                }
            }
        }
        // A server that answers every reference with "not found" is not telling
        // us the resources are gone, it is refusing the request form. Returning
        // an empty list here made the caller record the collection tag as if the
        // sync had worked, and nothing was ever fetched again.
        if out.resources.is_empty() && !hrefs.is_empty() {
            return Err(protocol(format!(
                "the server returned no data for any of the {} requested resources ({} reported missing)",
                hrefs.len(),
                out.missing.len()
            )));
        }
        Ok(out)
    }

    /* ────────────────── generic resource sync (CardDAV/VTODO) ───────── */

    /// Fetch the current collection state (ctag + per-resource etags).
    ///
    /// The returned [`CollectionState::listed_hrefs`] is the complete member
    /// set, not just the members whose ETag could be read. Callers must use it
    /// to distinguish "not listed" (deleted) from "listed but unreadable"
    /// (present, identity unrefreshed). The latter also sets
    /// [`CollectionState::incomplete`].
    pub async fn collection_etags(
        &self,
        _kind: DavKind,
        collection: &str,
    ) -> Result<CollectionState> {
        let multistatus = self.propfind(collection, &etags_propfind(), "1").await?;
        let collection_url = ensure_trailing_slash(collection);
        // Compare against the normalized form so a configured default port or
        // an escaped path does not hide the collection's own response. The
        // resolution must not guess: an empty or cross-origin href is not the
        // collection's identity.
        let collection_href = self.collection_identity(&collection_url)?;
        let mut collection_response = None;
        for response in &multistatus.responses {
            if let ResponseRole::Collection { .. } =
                self.classify_response(&collection_url, &collection_href, response)?
            {
                collection_response = Some(response);
                break;
            }
        }
        // The collection's own response anchors the listing: it carries the
        // ctag/sync-token and proves the server actually served the collection.
        // A `207` can still contain a failed response element for the collection
        // itself (for example a `503` with no members). Treating that as an
        // empty-but-successful listing made every locally cached object look
        // deleted. Fail the whole round instead; the caller must not derive
        // deletions from, or advance the cursor past, a failed listing.
        let Some(collection_response) = collection_response else {
            return Err(protocol(format!(
                "PROPFIND on {collection_url} did not include the collection itself; the listing is not authoritative"
            )));
        };
        if collection_response_failed(collection_response) {
            return Err(protocol(format!(
                "PROPFIND on {collection_url} reported status {} for the collection itself; refusing to diff its members",
                collection_response
                    .status
                    .map_or_else(|| "non-2xx".to_string(), |code| code.to_string())
            )));
        }
        let mut state = CollectionState {
            ctag: collection_response.prop.ctag.clone(),
            sync_token: collection_response
                .prop
                .sync_token
                .clone()
                .filter(|token| !token.trim().is_empty()),
            ..CollectionState::default()
        };
        for response in &multistatus.responses {
            let href = match self.classify_response(&collection_url, &collection_href, response)? {
                ResponseRole::Collection { .. } => continue,
                ResponseRole::Member(href) => href,
            };
            if response.prop.is_collection {
                continue;
            }
            // A response that proves the resource itself is gone is *not* part
            // of the listing; its absence drives `deleted_hrefs`.
            if response.is_missing() {
                continue;
            }
            state.listed_hrefs.push(href.clone());
            match response.prop.etag.clone() {
                Some(etag) => state.etags.push((href, etag)),
                None => {
                    // The server named this object but did not let us read its
                    // identity (for example a per-resource 503). Treat it as
                    // present so nothing is pruned, flag the round, and leave
                    // the identity unrefreshed.
                    state.incomplete = true;
                }
            }
        }
        Ok(state)
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

        if let Some(token) = cursor.sync_token.clone().filter(|token| !token.is_empty()) {
            let mut page_token = token.clone();
            let mut seen: HashSet<String> = HashSet::new();
            let mut fell_back = false;
            // Pages are one ordered change stream; merge per href so the
            // newest page wins and a later `404` erases an earlier body.
            let mut merged = HrefMerge::default();
            while seen.len() < MAX_SYNC_PAGES {
                seen.insert(page_token.clone());
                let page = match self.request_sync_page(&collection, &page_token).await? {
                    PageOutcome::Status(status, _) => {
                        // Any 4xx/5xx: fall back to the ctag/etag comparison
                        // below. A server that no longer understands the token
                        // simply needs it replaced, which the next successful
                        // pass does.
                        debug!(
                            collection,
                            status = status.as_u16(),
                            "sync-collection unsupported for this collection; using ETag diff"
                        );
                        fell_back = true;
                        break;
                    }
                    PageOutcome::Page(page) => page,
                };
                let mut changed = Vec::new();
                for member in page.members {
                    if member.missing {
                        merged.record(member.href, HrefEvidence::Missing);
                        continue;
                    }
                    if member.is_collection {
                        continue;
                    }
                    match member.etag {
                        // The member changed; its body has to be fetched.
                        Some(_) => changed.push(member.href),
                        // The server named a change but its identity could not
                        // be read (for example a per-resource `503` with an
                        // empty `getetag`). Not a deletion, and not safe to
                        // acknowledge: keep the cursor before this batch so
                        // the change is requested again. It supersedes any
                        // older body or deletion for the same href.
                        None => {
                            merged.record(member.href, HrefEvidence::Unreadable);
                            delta.incomplete = true;
                        }
                    }
                }
                if !changed.is_empty() {
                    let fetched = self.multiget(kind, &collection_url, &changed).await?;
                    // Multiget evidence is newer than the page that requested
                    // it: a 404 is a deletion, a body is present.
                    for href in fetched.missing {
                        merged.record(href, HrefEvidence::Missing);
                    }
                    for resource in fetched.resources {
                        merged.record(resource.href.clone(), HrefEvidence::Fetched(resource));
                    }
                }
                delta.incomplete |= page.incomplete;
                // An incomplete page, or a truncation with no token to
                // continue from, must not acknowledge anything past the
                // cursor we had.
                if delta.incomplete || (page.truncated && page.next_token.is_none()) {
                    delta.incomplete = true;
                    materialize_resource_evidence(merged, &mut delta);
                    delta.sync_token = Some(token);
                    delta.ctag = cursor.ctag.clone();
                    delta.full_sync = false;
                    return Ok(delta);
                }
                let next = page.next_token.unwrap_or_else(|| page_token.clone());
                if !page.truncated {
                    materialize_resource_evidence(merged, &mut delta);
                    delta.sync_token = Some(next);
                    delta.ctag = cursor.ctag.clone();
                    delta.full_sync = false;
                    return Ok(delta);
                }
                if seen.contains(&next) {
                    // A truncation that does not move the token forward would
                    // request the same page forever.
                    delta.incomplete = true;
                    materialize_resource_evidence(merged, &mut delta);
                    delta.sync_token = Some(token);
                    delta.ctag = cursor.ctag.clone();
                    delta.full_sync = false;
                    return Ok(delta);
                }
                page_token = next;
            }
            if fell_back {
                // Discard the partial pages: the ETag listing below is
                // authoritative and rebuilds `delta` from the baseline cursor.
                delta = ResourceDelta::default();
            } else {
                return Err(protocol(format!(
                    "sync-collection on {collection_url} exceeded {MAX_SYNC_PAGES} truncated pages"
                )));
            }
        }

        let state = self.collection_etags(kind, &collection).await?;
        if ctag_proves_unchanged(cursor, state.ctag.as_deref()) {
            delta.full_sync = false;
            // An unreadable resource must keep the cursor where it is; the
            // next pass has to retry its identity.
            delta.incomplete = state.incomplete;
            if state.incomplete {
                delta.ctag = cursor.ctag.clone();
                delta.sync_token = cursor.sync_token.clone();
            } else {
                delta.ctag = state.ctag;
                delta.sync_token = state.sync_token.or_else(|| cursor.sync_token.clone());
            }
            return Ok(delta);
        }
        // Compare baselines by identity key, not by spelling (RFC 3986
        // §6.2.2). Otherwise a resource listed as `%41.ics` after a round that
        // stored `A.ics` would look both changed and deleted.
        let local: HashMap<String, (String, String)> = cursor
            .etags
            .iter()
            .map(|(href, etag)| (href_key(href), (href.clone(), etag.clone())))
            .collect();
        // Presence comes from the complete listing, not from the resources
        // whose ETag happened to be readable. A member that is listed but
        // returned a per-resource error is present with an unrefreshed
        // identity; only an href missing from the listing is a deletion.
        // Keys are identity keys; values keep the raw listed href.
        let mut present: BTreeMap<String, String> = BTreeMap::new();
        for href in &state.listed_hrefs {
            present
                .entry(href_key(href))
                .or_insert_with(|| href.clone());
        }
        let mut changed = Vec::new();
        for (href, etag) in &state.etags {
            if local.get(&href_key(href)).map(|(_, local_etag)| local_etag) != Some(etag) {
                changed.push(href.clone());
            }
        }
        for (key, (href, _)) in &local {
            if !present.contains_key(key) {
                delta.deleted_hrefs.push(href.clone());
            }
        }
        // This PROPFIND is the authoritative listing. Preserve it even for
        // unchanged objects: they are absent from `changed`/`delta.changed`,
        // but a `full_sync` consumer must not prune them as deleted.
        if !changed.is_empty() {
            let fetched = self.multiget(kind, &collection_url, &changed).await?;
            // The multiget is newer than the listing: an explicit 404 proves
            // the object is gone even though the listing named it.
            for href in fetched.missing {
                present.remove(&href_key(&href));
                delta.deleted_hrefs.push(href);
            }
            delta.changed.extend(fetched.resources);
        }
        delta.listed_hrefs = present.into_values().collect();
        delta.incomplete = state.incomplete;
        if state.incomplete {
            // Do not acknowledge a listing that left an identity unread; the
            // consumer's retry must start from the cursor it already had.
            delta.ctag = cursor.ctag.clone();
            delta.sync_token = cursor.sync_token.clone();
        } else {
            delta.ctag = state.ctag;
            delta.sync_token = state.sync_token;
        }
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

    /// Resolve an href that must identify a real resource or collection.
    ///
    /// Unlike [`Self::resolve_url`] this never falls back to `base`: an empty,
    /// malformed or cross-origin href yields `None`. Callers that use the
    /// result for identity checks must treat `None` as "cannot be attributed",
    /// never as the collection itself.
    fn try_resolve_url(&self, base: &str, href: &str) -> Option<String> {
        resolve_href_strict(base, href, &self.allowed_origins())
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

/// Strict variant of [`resolve_href`] used for identity checks.
///
/// Returns `None` instead of guessing when the reference is empty, cannot be
/// parsed, or points outside the allowed origins. An empty href must not be
/// resolved to `base`: that turned a response with no href into the
/// collection's own response.
fn resolve_href_strict(base: &str, href: &str, allowed: &[Url]) -> Option<String> {
    let href = href.trim();
    if href.is_empty() {
        return None;
    }
    let url = if href.starts_with("http://") || href.starts_with("https://") {
        Url::parse(href).ok()?
    } else {
        Url::parse(base).ok()?.join(href).ok()?
    };
    origin_allowed(allowed, &url).then(|| url.to_string())
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
    fn multiget_references_use_the_path_form_for_same_origin_urls() {
        let collection = "https://contacts.163.com/carddav/abc/contacts/";
        assert_eq!(
            request_href(
                collection,
                "https://contacts.163.com/carddav/abc/contacts/one.vcf"
            ),
            "/carddav/abc/contacts/one.vcf"
        );
        // A path the server gave us stays exactly as it is.
        assert_eq!(
            request_href(collection, "/carddav/abc/contacts/one.vcf"),
            "/carddav/abc/contacts/one.vcf"
        );
        // Query strings are part of the reference.
        assert_eq!(
            request_href(collection, "https://contacts.163.com/carddav/x.vcf?a=1"),
            "/carddav/x.vcf?a=1"
        );
    }

    #[test]
    fn multiget_references_to_other_origins_are_left_alone() {
        let collection = "https://contacts.163.com/carddav/abc/contacts/";
        let other_host = "https://evil.example.com/carddav/abc/contacts/one.vcf";
        assert_eq!(request_href(collection, other_host), other_host);
        let other_scheme = "http://contacts.163.com/carddav/abc/contacts/one.vcf";
        assert_eq!(request_href(collection, other_scheme), other_scheme);
        let other_port = "https://contacts.163.com:8443/carddav/abc/contacts/one.vcf";
        assert_eq!(request_href(collection, other_port), other_port);
    }

    fn cursor(ctag: Option<&str>, etags: &[(&str, &str)]) -> CalendarCursor {
        CalendarCursor {
            local_calendar_id: "book".into(),
            sync_token: None,
            ctag: ctag.map(str::to_string),
            etags: etags
                .iter()
                .map(|(href, etag)| (href.to_string(), etag.to_string()))
                .collect(),
        }
    }

    #[test]
    fn an_unchanged_tag_only_proves_anything_when_we_hold_the_resources() {
        let held = cursor(Some("42"), &[("/a.vcf", "\"1\"")]);
        assert!(ctag_proves_unchanged(&held, Some("42")));
        assert!(!ctag_proves_unchanged(&held, Some("43")));
        assert!(!ctag_proves_unchanged(&held, None));
    }

    #[test]
    fn an_unchanged_tag_with_nothing_stored_does_not_short_circuit() {
        // The state a failed first sync leaves behind: tag recorded, no resources.
        let stuck = cursor(Some("-1889622814"), &[]);
        assert!(!ctag_proves_unchanged(&stuck, Some("-1889622814")));
        // A first sync has no tag at all.
        assert!(!ctag_proves_unchanged(&cursor(None, &[]), Some("1")));
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
    fn href_keys_follow_rfc3986_percent_encoding_equivalence() {
        // Unreserved octets decode to the character they encode.
        assert_eq!(href_key("/cal/%41.ics"), "/cal/A.ics");
        assert_eq!(href_key("/cal/%61%62.ics"), "/cal/ab.ics");
        assert_eq!(href_key("/cal/%7Euser.vcf"), "/cal/~user.vcf");
        assert_eq!(href_key("/cal/a%2Db%2Ec%5Fd%7E"), "/cal/a-b.c_d~");
        // Everything else is only uppercased, never decoded.
        assert_eq!(href_key("/cal/a%2fb%3Ac"), "/cal/a%2Fb%3Ac");
        assert_eq!(href_key("/cal/a%2Fb"), "/cal/a%2Fb");
        // Malformed escapes and non-ASCII input stay byte-for-byte stable.
        assert_eq!(href_key("/cal/%zz.ics"), "/cal/%zz.ics");
        assert_eq!(href_key("/cal/%4.ics"), "/cal/%4.ics");
        assert_eq!(href_key("/cal/%C3%A9.ics"), "/cal/%C3%A9.ics");
        assert_eq!(href_key(""), "");
        // Equivalent spellings collapse to one key.
        assert_eq!(
            href_key("https://dav/cal/%41.ics"),
            href_key("https://dav/cal/A.ics")
        );
        assert_eq!(
            href_key("https://dav/cal/%7e"),
            href_key("https://dav/cal/~")
        );
    }

    #[test]
    fn membership_uses_normalized_paths() {
        let collection = "https://dav.example.com/ab/";
        assert!(href_in_collection(
            collection,
            "https://dav.example.com/%61b/card.vcf"
        ));
        assert!(href_in_collection(
            "https://dav.example.com/%61b/",
            "https://dav.example.com/ab/card.vcf"
        ));
        // Equivalent spellings of the collection itself are still not members.
        assert!(!href_in_collection(
            collection,
            "https://dav.example.com/%61b/"
        ));
        // An encoded slash is reserved and must not create a member.
        assert!(!href_in_collection(
            collection,
            "https://dav.example.com/ab%2Fcard.vcf"
        ));
    }

    #[test]
    fn membership_requires_the_collection_path_prefix() {
        let collection = "https://dav.example.com/ab/";
        assert!(href_in_collection(
            collection,
            "https://dav.example.com/ab/card.vcf"
        ));
        assert!(href_in_collection(
            collection,
            "https://dav.example.com/ab/sub/card.vcf"
        ));
        // Same origin but a sibling collection.
        assert!(!href_in_collection(
            collection,
            "https://dav.example.com/other-book/card.vcf"
        ));
        // A shared prefix that is not a path prefix.
        assert!(!href_in_collection(
            collection,
            "https://dav.example.com/ab-other/card.vcf"
        ));
        // The collection itself is not one of its members.
        assert!(!href_in_collection(
            collection,
            "https://dav.example.com/ab/"
        ));
        assert!(!href_in_collection(
            collection,
            "https://dav.example.com/ab"
        ));
        // Different origin, even when the path matches.
        assert!(!href_in_collection(
            collection,
            "https://evil.example.net/ab/card.vcf"
        ));
        assert!(!href_in_collection(
            collection,
            "http://dav.example.com/ab/card.vcf"
        ));
        assert!(!href_in_collection(
            collection,
            "https://dav.example.com:8443/ab/card.vcf"
        ));
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

/// The form of a resource reference to put in a `*-multiget` request.
///
/// Servers list members as path references (`/carddav/…/x.vcf`). We resolve
/// those to absolute URLs internally, but some servers answer a multiget for an
/// absolute URL with 404 for every resource: NetEase's `contacts.163.com`
/// returns 200 for the path and 404 for the very same resource as a full URL.
/// Same-origin absolute URLs are therefore sent as their path (and query);
/// anything else is passed through untouched.
fn request_href(collection: &str, href: &str) -> String {
    let (Ok(base), Ok(target)) = (Url::parse(collection), Url::parse(href)) else {
        return href.to_string();
    };
    if base.scheme() != target.scheme()
        || base.host_str() != target.host_str()
        || base.port_or_known_default() != target.port_or_known_default()
    {
        return href.to_string();
    }
    match target.query() {
        Some(query) => format!("{}?{}", target.path(), query),
        None => target.path().to_string(),
    }
}

/// A matching collection tag proves "nothing changed" only if we actually hold
/// what we saw. An unchanged tag with an empty local set is the signature of an
/// earlier sync that fetched nothing and still recorded the tag; trusting it
/// would leave the collection empty forever.
fn ctag_proves_unchanged(cursor: &CalendarCursor, new_ctag: Option<&str>) -> bool {
    cursor.ctag.is_some() && cursor.ctag.as_deref() == new_ctag && !cursor.etags.is_empty()
}

/// Identity key for href comparison (RFC 3986 §6.2.2).
///
/// Percent-encoded octets that correspond to an unreserved character
/// (`ALPHA / DIGIT / "-" / "." / "_" / "~"`) are decoded to that character;
/// every other percent-encoded octet is rewritten with uppercase hex. Both
/// rules together make equivalent spellings (`%41.ics` and `A.ics`,
/// `%7e` and `~`, `%2f` and `%2F`) compare equal.
///
/// This is an *identity* key only. Requests and persisted hrefs keep the
/// server's original spelling, so [`same_href`], merge maps and member lookups
/// normalize both sides instead of rewriting stored values.
pub(crate) fn href_key(href: &str) -> String {
    fn hex_value(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    fn hex_digit(value: u8) -> char {
        match value {
            0..=9 => (b'0' + value) as char,
            _ => (b'A' + value - 10) as char,
        }
    }
    /// RFC 3986 §2.3 unreserved characters.
    fn is_unreserved(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
    }

    let bytes = href.as_bytes();
    let mut out = String::with_capacity(href.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if let (Some(high), Some(low)) = (
                bytes.get(index + 1).copied().and_then(hex_value),
                bytes.get(index + 2).copied().and_then(hex_value),
            ) {
                let byte = high * 16 + low;
                if is_unreserved(byte) {
                    out.push(byte as char);
                } else {
                    out.push('%');
                    out.push(hex_digit(high));
                    out.push(hex_digit(low));
                }
                index += 3;
                continue;
            }
        }
        let Some(character) = href[index..].chars().next() else {
            break;
        };
        out.push(character);
        index += character.len_utf8();
    }
    out
}

fn same_href(left: &str, right: &str) -> bool {
    let left = href_key(left);
    let right = href_key(right);
    left.trim_end_matches('/') == right.trim_end_matches('/')
}

/// Whether `member` is really inside the collection identified by `collection`.
///
/// An allow-listed origin only proves which server we are talking to; it says
/// nothing about which collection a response belongs to. Membership is the
/// collection's own origin plus its exact path prefix, with the trailing slash
/// enforced so `/ab/` cannot claim `/ab-other/card.vcf` or `/other-book/x.vcf`.
fn href_in_collection(collection: &str, member: &str) -> bool {
    let (Ok(collection), Ok(member)) = (Url::parse(collection), Url::parse(member)) else {
        return false;
    };
    if collection.scheme() != member.scheme()
        || collection.host_str() != member.host_str()
        || collection.port_or_known_default() != member.port_or_known_default()
    {
        return false;
    }
    // Compare normalized paths so `/ab/` still owns `/%61b/card.vcf`. Only
    // unreserved octets are decoded, so a `%2F` can never manufacture a new
    // segment boundary and turn a sibling into a member.
    let mut prefix = href_key(collection.path());
    if !prefix.ends_with('/') {
        prefix.push('/');
    }
    let member_path = href_key(member.path());
    member_path.len() > prefix.len() && member_path.starts_with(&prefix)
}

/// Whether a collection's own `response` element reports failure.
///
/// A response-level status (`<response><status>HTTP/1.1 503 …</status>`) is
/// authoritative; when there is none, `ok == false` means every `propstat`
/// carried a non-2xx status. A response with no status at all counts as `200`,
/// so a server that omits statuses is not rejected. Either case means the
/// collection listing cannot be trusted as complete.
fn collection_response_failed(response: &DavResponse) -> bool {
    response
        .status
        .is_some_and(|code| !(200..300).contains(&code))
        || !response.ok
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
