//! Minimal loopback Microsoft Graph mock.
//!
//! Supports what the mail paths send:
//!
//! * `GET /me/mailFolders/{id}/messages` with `$filter`, `$orderby`, `$top`
//!   and an `@odata.nextLink` (`$skip` token) for load-older and sync tests;
//! * `GET /me/mailFolders/{id}/messages/delta` with `$top` pages
//!   (`$skiptoken` continuations) ending in `@odata.deltaLink`, optionally
//!   omitted to model a truncated response;
//! * `DELETE /me/messages/{id}` (204, or 404 in "already gone" mode);
//! * `POST /me/messages` for MIME draft creation, optionally gated so a test
//!   can act while the create is still on the wire;
//! * `POST /me/messages/{id}/permanentDelete`, optionally failing so an
//!   uncertain cleanup outcome can be exercised;
//! * `POST /me/sendMail` (202);
//! * `GET /me/messages/{id}?$select=parentFolderId` for targeted verification.
//!
//! Every request URL is recorded.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

pub struct MockGraph {
    pub base: String,
    pub requests: Arc<Mutex<Vec<String>>>,
    /// Graph ids named by incoming DELETE/permanentDelete requests.
    deleted: Arc<Mutex<Vec<String>>>,
    /// The live message set; a delete removes its id, so tests can assert the
    /// server state before and after a request.
    messages: Arc<Mutex<Vec<(String, String)>>>,
    /// `isRead` overrides for messages added by [`MockGraph::add_message`];
    /// ids not present default to unread, matching [`MockGraph::start`].
    read_state: Arc<Mutex<HashMap<String, bool>>>,
    /// `internetMessageId` overrides for messages whose id is present. Real
    /// Graph ids differ for copies that share one Message-ID; the default
    /// derives the header from the id.
    internet_message_ids: Arc<Mutex<HashMap<String, String>>>,
    /// One permit per DELETE request received.
    pub delete_received: Arc<Semaphore>,
    /// A DELETE response is withheld until a permit is released here, so a
    /// test can act while the request is still on the wire.
    pub release_delete: Arc<Semaphore>,
    /// Ids created by `POST /me/messages`, in arrival order.
    created: Arc<Mutex<Vec<String>>>,
    /// One permit per create request received; the response is withheld until
    /// [`MockGraph::release_create`] receives a permit.
    pub create_received: Arc<Semaphore>,
    pub release_create: Arc<Semaphore>,
    /// When set, `permanentDelete` answers 500 without touching the live set.
    permanent_delete_failure: Arc<AtomicBool>,
    /// Specific ids whose `permanentDelete` answers 400 without deleting. Lets
    /// a test model a batch that succeeded for one message and was refused for
    /// another.
    permanent_delete_fail_ids: Arc<Mutex<Vec<(String, u16)>>>,
    /// When set, a draft create answers 201 with `{}` (the request succeeded
    /// but the body carries no id).
    create_without_id: Arc<AtomicBool>,
    /// `parentFolderId` returned by the targeted GET. `None` answers 200 with
    /// an empty object, modelling a response that omits the field.
    parent_folder: Arc<Mutex<Option<String>>>,
    /// `isDraft` returned by the targeted GET. Defaults to `true`; `None`
    /// models a response that omits the field.
    is_draft: Arc<Mutex<Option<bool>>>,
    /// One permit per gated PATCH request received.
    pub patch_received: Arc<Semaphore>,
    /// The first gated PATCH response is withheld until a permit is released
    /// here, so a test can mutate the projection while the request is in
    /// flight. Only the first PATCH is gated, so a regression that sends the
    /// next one cannot deadlock the test.
    pub release_patch: Arc<Semaphore>,
    /// One permit per first-401 PATCH received. The 401 response is withheld
    /// until [`MockGraph::release_unauthorized`] is released, so a test can
    /// change the writer while the client is waiting to refresh its token.
    pub unauthorized_received: Arc<Semaphore>,
    pub release_unauthorized: Arc<Semaphore>,
    /// Specific PATCH ids that answer the configured status without applying
    /// anything. Models a batch whose first message succeeded and whose second
    /// one was refused.
    patch_fail_ids: Arc<Mutex<Vec<(String, u16)>>>,
    /// Whether the delta endpoint returns `@odata.deltaLink` on its final page.
    /// `false` models a truncated/malformed response that stops without a
    /// cursor.
    delta_link_in_response: Arc<AtomicBool>,
    /// Graph ids the delta endpoint reports as `@removed` on its first page.
    delta_removed: Arc<Mutex<Vec<String>>>,
}

impl MockGraph {
    /// `count` messages, ids `m1..mN`, `mN` newest, one minute apart.
    pub async fn start(count: usize) -> Self {
        Self::start_inner(count, false, false, false, false, false).await
    }

    /// Same as [`MockGraph::start`], but DELETE answers only after
    /// [`MockGraph::release_delete`] receives a permit. Each arrival adds a
    /// permit to [`MockGraph::delete_received`].
    pub async fn start_with_delete_gate(count: usize) -> Self {
        Self::start_inner(count, true, false, false, false, false).await
    }

    /// Same as [`MockGraph::start`], but DELETE answers 404: the remote item is
    /// already gone. The request id is still recorded and the live set is left
    /// untouched, mirroring Graph's idempotent delete contract.
    pub async fn start_with_missing_remote(count: usize) -> Self {
        Self::start_inner(count, false, true, false, false, false).await
    }

    /// Same as [`MockGraph::start`], but draft creation is gated: each
    /// `POST /me/messages` adds a permit to [`MockGraph::create_received`] and
    /// waits for [`MockGraph::release_create`] before answering.
    pub async fn start_with_create_gate(count: usize) -> Self {
        Self::start_inner(count, false, false, true, false, false).await
    }

    /// Same as [`MockGraph::start`], but the first `PATCH /me/messages/{id}`
    /// adds a permit to [`MockGraph::patch_received`] and waits for
    /// [`MockGraph::release_patch`] before answering. Later PATCHes answer
    /// immediately, so a test that expects the next one to be skipped fails
    /// without hanging.
    pub async fn start_with_patch_gate(count: usize) -> Self {
        Self::start_inner(count, false, false, false, true, false).await
    }

    /// Same as [`MockGraph::start`], but the first `PATCH /me/messages/{id}`
    /// answers 401 *after* [`MockGraph::release_unauthorized`] is released.
    /// This holds the client at the token-refresh boundary, so a test can
    /// change the writer between the rejected request and the retry.
    pub async fn start_with_unauthorized_gate(count: usize) -> Self {
        Self::start_inner(count, false, false, false, false, true).await
    }

    /// Wait until the gated PATCH has arrived.
    pub async fn wait_for_patch(&self) {
        let _ = self.patch_received.acquire().await;
    }

    /// Let the gated PATCH answer.
    pub fn release_patch(&self) {
        self.release_patch.add_permits(1);
    }

    /// Wait until the first PATCH has been refused with 401.
    pub async fn wait_for_unauthorized(&self) {
        let _ = self.unauthorized_received.acquire().await;
    }

    /// Let the gated 401 answer.
    pub fn release_unauthorized(&self) {
        self.release_unauthorized.add_permits(1);
    }

    /// Answer PATCHes for `id` with `status` without applying anything.
    pub fn fail_patch_for(&self, id: &str, status: u16) {
        self.patch_fail_ids
            .lock()
            .expect("patch fail ids")
            .push((id.to_string(), status));
    }

    /// Stop refusing PATCHes configured by [`MockGraph::fail_patch_for`].
    pub fn clear_patch_failures(&self) {
        self.patch_fail_ids.lock().expect("patch fail ids").clear();
    }

    async fn start_inner(
        count: usize,
        gate_delete: bool,
        delete_not_found: bool,
        gate_create: bool,
        gate_patch: bool,
        gate_unauthorized: bool,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let mut all: Vec<(String, String)> = (1..=count)
            .map(|n| {
                let at = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                    .expect("date")
                    .with_timezone(&chrono::Utc)
                    + chrono::Duration::minutes(n as i64);
                (
                    format!("m{n}"),
                    at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                )
            })
            .collect();
        all.reverse();
        let messages = Arc::new(Mutex::new(all));
        let read_state = Arc::new(Mutex::new(HashMap::new()));
        let internet_message_ids = Arc::new(Mutex::new(HashMap::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let deleted = Arc::new(Mutex::new(Vec::new()));
        let created = Arc::new(Mutex::new(Vec::new()));
        let delete_received = Arc::new(Semaphore::new(0));
        let release_delete = Arc::new(Semaphore::new(0));
        let create_received = Arc::new(Semaphore::new(0));
        let release_create = Arc::new(Semaphore::new(0));
        let patch_received = Arc::new(Semaphore::new(0));
        let release_patch = Arc::new(Semaphore::new(0));
        let unauthorized_received = Arc::new(Semaphore::new(0));
        let release_unauthorized = Arc::new(Semaphore::new(0));
        // Only the first PATCH is gated. A regression that still sends the
        // message a test expects to be skipped must answer normally, otherwise
        // the worker would wait forever and the test would time out instead of
        // reporting the unexpected request.
        let patch_armed = Arc::new(AtomicBool::new(gate_patch));
        let unauthorized_armed = Arc::new(AtomicBool::new(gate_unauthorized));
        let patch_fail_ids: Arc<Mutex<Vec<(String, u16)>>> = Arc::new(Mutex::new(Vec::new()));
        let permanent_delete_failure = Arc::new(AtomicBool::new(false));
        let permanent_delete_fail_ids: Arc<Mutex<Vec<(String, u16)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let create_without_id = Arc::new(AtomicBool::new(false));
        let delta_link_in_response = Arc::new(AtomicBool::new(true));
        let delta_removed = Arc::new(Mutex::new(Vec::new()));
        let parent_folder = Arc::new(Mutex::new(Some("drafts-parent".to_string())));
        let is_draft = Arc::new(Mutex::new(Some(true)));
        let next_created = Arc::new(AtomicUsize::new(1));
        let served = messages.clone();
        let reads = read_state.clone();
        let log = requests.clone();
        let origin = base.clone();
        let received = delete_received.clone();
        let release = release_delete.clone();
        let deleted_log = deleted.clone();
        let created_log = created.clone();
        let create_in = create_received.clone();
        let create_out = release_create.clone();
        let patch_in_outer = patch_received.clone();
        let patch_out_outer = release_patch.clone();
        let patch_armed_outer = patch_armed.clone();
        let unauthorized_in_outer = unauthorized_received.clone();
        let unauthorized_out_outer = release_unauthorized.clone();
        let unauthorized_armed_outer = unauthorized_armed.clone();
        let patch_fail_ids_outer = patch_fail_ids.clone();
        let perm_fail = permanent_delete_failure.clone();
        let fail_ids = permanent_delete_fail_ids.clone();
        let no_create_id = create_without_id.clone();
        let delta_links_outer = delta_link_in_response.clone();
        let delta_removed_outer = delta_removed.clone();
        let message_headers_outer = internet_message_ids.clone();
        let folder_for_get = parent_folder.clone();
        let draft_for_get = is_draft.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let served = served.clone();
                let reads = reads.clone();
                let log = log.clone();
                let origin = origin.clone();
                let received = received.clone();
                let release = release.clone();
                let deleted_log = deleted_log.clone();
                let created_log = created_log.clone();
                let create_in = create_in.clone();
                let create_out = create_out.clone();
                let patch_in = patch_in_outer.clone();
                let patch_out = patch_out_outer.clone();
                let patch_armed = patch_armed_outer.clone();
                let unauthorized_in = unauthorized_in_outer.clone();
                let unauthorized_out = unauthorized_out_outer.clone();
                let unauthorized_armed = unauthorized_armed_outer.clone();
                let patch_fail_ids = patch_fail_ids_outer.clone();
                let perm_fail = perm_fail.clone();
                let fail_ids = fail_ids.clone();
                let no_create_id = no_create_id.clone();
                let delta_links = delta_links_outer.clone();
                let delta_removed = delta_removed_outer.clone();
                let message_headers = message_headers_outer.clone();
                let folder_for_get = folder_for_get.clone();
                let draft_for_get = draft_for_get.clone();
                let next_created = next_created.clone();
                tokio::spawn(async move {
                    let Some(head) = read_head(&mut stream).await else {
                        return;
                    };
                    let request_line = head.lines().next().unwrap_or_default();
                    let method = request_line
                        .split_whitespace()
                        .next()
                        .unwrap_or("GET")
                        .to_string();
                    let target = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_string();
                    log.lock().expect("log").push(target.clone());
                    let path = target.split('?').next().unwrap_or(&target);
                    if method == "POST" && path.ends_with("/move") {
                        // A policy refusal: definitive, so the executor records
                        // the move as `NotExecuted` (never `Unknown`) and undo
                        // restores the source projection.
                        drain_body(&mut stream, &head).await;
                        respond(
                            &mut stream,
                            "403 Forbidden",
                            r#"{"error":{"code":"ErrorAccessDenied","message":"move refused"}}"#,
                        )
                        .await;
                        return;
                    }
                    if method == "POST" && path == "/me/messages" {
                        // Draft create. Drain the MIME body first: the client
                        // is still writing it while the gate holds the response.
                        drain_body(&mut stream, &head).await;
                        let id = format!("created-{}", next_created.fetch_add(1, Ordering::SeqCst));
                        created_log.lock().expect("created").push(id.clone());
                        served.lock().expect("messages").push((
                            id.clone(),
                            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                        ));
                        if gate_create {
                            create_in.add_permits(1);
                            let _ = create_out.acquire().await;
                        }
                        let body = if no_create_id.load(Ordering::SeqCst) {
                            json!({}).to_string()
                        } else {
                            json!({ "id": id }).to_string()
                        };
                        respond(&mut stream, "201 Created", &body).await;
                        return;
                    }
                    if method == "POST" && path == "/me/sendMail" {
                        drain_body(&mut stream, &head).await;
                        respond(&mut stream, "202 Accepted", "").await;
                        return;
                    }
                    if method == "POST" && path.ends_with("/permanentDelete") {
                        drain_body(&mut stream, &head).await;
                        let remote_id = path
                            .strip_prefix("/me/messages/")
                            .and_then(|rest| rest.strip_suffix("/permanentDelete"))
                            .map(decode_id);
                        if let Some(remote_id) = remote_id {
                            deleted_log.lock().expect("deleted").push(remote_id.clone());
                            if perm_fail.load(Ordering::SeqCst) {
                                let body = r#"{"error":{"code":"ErrorInternalServerError","message":"boom"}}"#;
                                respond(&mut stream, "500 Internal Server Error", body).await;
                                return;
                            }
                            let configured = fail_ids
                                .lock()
                                .expect("permanent delete fail ids")
                                .iter()
                                .find(|(id, _)| id == &remote_id)
                                .map(|(_, status)| *status);
                            if let Some(status) = configured {
                                let body = r#"{"error":{"code":"ErrorInvalidRequest","message":"refused"}}"#;
                                let line = if status == 404 {
                                    "404 Not Found"
                                } else if status >= 500 {
                                    "500 Internal Server Error"
                                } else {
                                    "400 Bad Request"
                                };
                                respond(&mut stream, line, body).await;
                                return;
                            }
                            served
                                .lock()
                                .expect("messages")
                                .retain(|(id, _)| id != &remote_id);
                        }
                        respond(&mut stream, "204 No Content", "").await;
                        return;
                    }
                    if method == "DELETE" {
                        let remote_id = path.strip_prefix("/me/messages/").map(decode_id);
                        if let Some(remote_id) = remote_id {
                            deleted_log.lock().expect("deleted").push(remote_id.clone());
                            // A real DELETE mutates server state; tests assert
                            // the message is gone afterwards. A 404 models an
                            // item that was already absent, so nothing changes.
                            if !delete_not_found {
                                served
                                    .lock()
                                    .expect("messages")
                                    .retain(|(id, _)| id != &remote_id);
                            }
                        }
                        if gate_delete {
                            received.add_permits(1);
                            let _ = release.acquire().await;
                        }
                        if delete_not_found {
                            let body = r#"{"error":{"code":"ErrorItemNotFound","message":"gone"}}"#;
                            respond(&mut stream, "404 Not Found", body).await;
                        } else {
                            respond(&mut stream, "204 No Content", "").await;
                        }
                        return;
                    }
                    if method == "PATCH" && path.starts_with("/me/messages/") {
                        drain_body(&mut stream, &head).await;
                        let remote_id = path
                            .strip_prefix("/me/messages/")
                            .map(decode_id)
                            .unwrap_or_default();
                        // Hold the first 401 until the test has mutated the
                        // writer. The client is then waiting to refresh its
                        // token, exactly where a retry could bypass the gate.
                        if unauthorized_armed.swap(false, Ordering::SeqCst) {
                            unauthorized_in.add_permits(1);
                            let _ = unauthorized_out.acquire().await;
                            let body = r#"{"error":{"code":"InvalidAuthenticationToken","message":"expired"}}"#;
                            respond(&mut stream, "401 Unauthorized", body).await;
                            return;
                        }
                        let failure = patch_fail_ids
                            .lock()
                            .expect("patch fail ids")
                            .iter()
                            .find(|(id, _)| id == &remote_id)
                            .map(|(_, status)| *status);
                        if let Some(status) = failure {
                            let line = if status == 404 {
                                "404 Not Found"
                            } else if status >= 500 {
                                "500 Internal Server Error"
                            } else {
                                "400 Bad Request"
                            };
                            let body = r#"{"error":{"code":"ErrorInternalServerError","message":"refused"}}"#;
                            respond(&mut stream, line, body).await;
                            return;
                        }
                        // Gate only the first PATCH so a test can mutate the
                        // next message's writer while this one is on the wire.
                        if patch_armed.swap(false, Ordering::SeqCst) {
                            patch_in.add_permits(1);
                            let _ = patch_out.acquire().await;
                        }
                        respond(&mut stream, "200 OK", "{}").await;
                        return;
                    }
                    if method == "GET" && path.starts_with("/me/messages/") {
                        // Targeted verification of one message's folder.
                        let remote_id = path
                            .strip_prefix("/me/messages/")
                            .map(decode_id)
                            .unwrap_or_default();
                        let exists = served
                            .lock()
                            .expect("messages")
                            .iter()
                            .any(|(id, _)| id == &remote_id);
                        if exists {
                            let parent = folder_for_get.lock().expect("parent folder").clone();
                            let is_draft = *draft_for_get.lock().expect("draft flag");
                            let body = match parent {
                                Some(parent) => json!({
                                    "parentFolderId": parent,
                                    "isDraft": is_draft,
                                })
                                .to_string(),
                                // A 200 that omits the field must not look like
                                // a 404.
                                None => json!({ "isDraft": is_draft }).to_string(),
                            };
                            respond(&mut stream, "200 OK", &body).await;
                        } else {
                            let body = r#"{"error":{"code":"ErrorItemNotFound","message":"gone"}}"#;
                            respond(&mut stream, "404 Not Found", body).await;
                        }
                        return;
                    }
                    let body = respond_listing(
                        &served,
                        &reads,
                        &message_headers,
                        &delta_removed,
                        &origin,
                        &target,
                        delta_links.load(Ordering::SeqCst),
                    );
                    respond(&mut stream, "200 OK", &body).await;
                });
            }
        });
        Self {
            base,
            requests,
            deleted,
            messages,
            read_state,
            internet_message_ids,
            delete_received,
            release_delete,
            created,
            create_received,
            release_create,
            permanent_delete_failure,
            permanent_delete_fail_ids,
            create_without_id,
            parent_folder,
            is_draft,
            patch_received,
            release_patch,
            unauthorized_received,
            release_unauthorized,
            patch_fail_ids,
            delta_link_in_response,
            delta_removed,
        }
    }

    /// Report `id` as `@removed` on the first page of the next delta pass.
    pub fn push_delta_removed(&self, id: &str) {
        self.delta_removed
            .lock()
            .expect("delta removed")
            .push(id.to_string());
    }

    /// Override the `internetMessageId` the listing/delta endpoints report for
    /// `id`. Distinct Graph ids can legitimately share one Message-ID (the
    /// server hands out a new id for a copy), which the default derived value
    /// (`<{id}@graph.test>`) cannot express.
    pub fn set_internet_message_id(&self, id: &str, message_id: &str) {
        self.internet_message_ids
            .lock()
            .expect("internet message ids")
            .insert(id.to_string(), message_id.to_string());
    }

    /// Control whether the delta endpoint ends its page chain with
    /// `@odata.deltaLink`. `false` models a truncated response that carries
    /// neither a continuation nor a cursor.
    pub fn set_delta_link_in_response(&self, enabled: bool) {
        self.delta_link_in_response.store(enabled, Ordering::SeqCst);
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("log").clone()
    }

    /// Graph ids still present on the server.
    pub fn ids(&self) -> Vec<String> {
        self.messages
            .lock()
            .expect("messages")
            .iter()
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Graph ids named by DELETE/permanentDelete requests, in arrival order.
    pub fn deleted_ids(&self) -> Vec<String> {
        self.deleted.lock().expect("deleted").clone()
    }

    /// Ids assigned by `POST /me/messages`, in arrival order.
    pub fn created_ids(&self) -> Vec<String> {
        self.created.lock().expect("created").clone()
    }

    /// Toggle `permanentDelete` failure between passes.
    pub fn set_permanent_delete_failure(&self, fail: bool) {
        self.permanent_delete_failure.store(fail, Ordering::SeqCst);
    }

    /// Refuse `permanentDelete` for this id (400, message untouched).
    pub fn fail_permanent_delete_for(&self, id: &str) {
        self.fail_permanent_delete_with_status_for(id, 400);
    }

    /// Refuse `permanentDelete` for this id with `status`. A 5xx leaves the
    /// outcome unknown; a 4xx is a definite refusal.
    pub fn fail_permanent_delete_with_status_for(&self, id: &str, status: u16) {
        self.permanent_delete_fail_ids
            .lock()
            .expect("permanent delete fail ids")
            .push((id.to_string(), status));
    }

    /// Answer `POST /me/messages` with 201 and an empty body. The create
    /// succeeds on the server but the client cannot learn the new id.
    pub fn set_create_response_without_id(&self, enabled: bool) {
        self.create_without_id.store(enabled, Ordering::SeqCst);
    }

    /// Choose the `parentFolderId` returned by the targeted GET. `None` answers
    /// 200 with an empty object (the field is absent).
    pub fn set_parent_folder_response(&self, parent: Option<&str>) {
        *self.parent_folder.lock().expect("parent folder") = parent.map(str::to_string);
    }

    /// Choose the `isDraft` returned by the targeted GET. `None` omits it.
    pub fn set_is_draft_response(&self, is_draft: Option<bool>) {
        *self.is_draft.lock().expect("draft flag") = is_draft;
    }

    /// Add (or replace) one message with an explicit received time and read
    /// flag.
    ///
    /// Lets a test model an old message that appears in a delta response: a
    /// cached one whose read/flag update must still apply, or an uncached one
    /// that must stay outside the local sync window.
    pub fn add_message(&self, id: &str, received: &str, is_read: bool) {
        {
            let mut messages = self.messages.lock().expect("messages");
            messages.retain(|(remote, _)| remote != id);
            messages.push((id.to_string(), received.to_string()));
        }
        self.read_state
            .lock()
            .expect("read state")
            .insert(id.to_string(), is_read);
    }

    /// Make a message disappear from the live set without recording a delete
    /// request, modelling a move that invalidated the old id.
    pub fn remove_message(&self, id: &str) {
        self.messages
            .lock()
            .expect("messages")
            .retain(|(remote, _)| remote != id);
    }
}

fn decode_id(raw: &str) -> String {
    let raw = raw.split('?').next().unwrap_or(raw);
    urlencoding::decode(raw)
        .map(|value| value.to_string())
        .unwrap_or_else(|_| raw.to_string())
}

/// Read request headers (through the blank line).
async fn read_head(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    }
    Some(String::from_utf8_lossy(&buffer).to_string())
}

/// Read any remaining request body so responding cannot truncate the upload.
async fn drain_body(stream: &mut tokio::net::TcpStream, head: &str) {
    let length = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    });
    let Some(length) = length else {
        return;
    };
    let Some(body_start) = head.find("\r\n\r\n").map(|index| index + 4) else {
        return;
    };
    let mut received = head.len().saturating_sub(body_start);
    let mut chunk = [0u8; 8192];
    while received < length {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => received += read,
        }
    }
}

async fn respond(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn respond_listing(
    messages: &Mutex<Vec<(String, String)>>,
    read_state: &Mutex<HashMap<String, bool>>,
    internet_message_ids: &Mutex<HashMap<String, String>>,
    removed: &Mutex<Vec<String>>,
    origin: &str,
    target: &str,
    delta_link: bool,
) -> String {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    // The delta endpoint pages with `$skiptoken` and ends with a `deltaLink`;
    // the plain listing endpoint pages with `$skip`/`$top` (load-older).
    let is_delta = path.ends_with("/messages/delta");
    let mut filter_le: Option<String> = None;
    let mut top = 10usize;
    let mut skip = 0usize;
    let mut skip_token: Option<usize> = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = urlencoding::decode(key).unwrap_or_default().to_string();
        let value = urlencoding::decode(value).unwrap_or_default().to_string();
        match key.as_str() {
            "$filter" => {
                filter_le = value
                    .strip_prefix("receivedDateTime le ")
                    .map(str::to_string);
            }
            "$top" => top = value.parse().unwrap_or(top),
            "$skip" => skip = value.parse().unwrap_or(skip),
            "$skiptoken" => skip_token = value.parse().ok(),
            _ => {}
        }
    }
    if is_delta {
        if let Some(token) = skip_token {
            skip = token;
        }
    }
    let matching: Vec<(String, String)> = messages
        .lock()
        .expect("messages")
        .iter()
        .filter(|(_, at)| filter_le.as_ref().map(|max| at <= max).unwrap_or(true))
        .cloned()
        .collect();
    let page: Vec<_> = matching.iter().skip(skip).take(top).collect();
    let reads = read_state.lock().expect("read state");
    let message_ids = internet_message_ids.lock().expect("internet message ids");
    let mut value: Vec<_> = page
        .iter()
        .map(|(id, at)| {
            json!({
                "id": id,
                "subject": format!("Subject {id}"),
                "bodyPreview": "preview",
                "receivedDateTime": at,
                "sentDateTime": at,
                "isRead": reads.get(id).copied().unwrap_or(false),
                "isDraft": false,
                "hasAttachments": false,
                "from": {"emailAddress": {"name": "A", "address": "a@example.com"}},
                "toRecipients": [{"emailAddress": {"name": "B", "address": "b@example.com"}}],
                "internetMessageId": message_ids
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| format!("<{id}@graph.test>")),
                "conversationId": format!("c-{id}"),
            })
        })
        .collect();
    // Injected removals belong to the first delta page only, so a paged pass
    // does not report the same deletion once per continuation.
    if is_delta && skip == 0 {
        for id in removed.lock().expect("delta removed").iter() {
            value.push(json!({
                "id": id,
                "@removed": {"reason": "deleted"},
            }));
        }
    }
    let mut body = json!({ "value": value });
    let next = skip + page.len();
    if next < matching.len() {
        let link = if is_delta {
            // Like Graph, the continuation token keeps the requested page size.
            format!("{origin}{path}?%24skiptoken={next}&%24top={top}")
        } else {
            format!("{origin}{path}?%24skip={next}&%24top={top}")
        };
        body["@odata.nextLink"] = json!(link);
    } else if is_delta && delta_link {
        body["@odata.deltaLink"] = json!(format!(
            "{origin}{path}?%24deltatoken=delta-{}",
            matching.len()
        ));
    }
    body.to_string()
}
