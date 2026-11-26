//! CalDAV client tests against a tiny in-process HTTP/1.1 mock.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nuncio_core::models::{
    new_id, Account, AuthKind, CalendarEvent, RecurrenceEditScope, StoredEvent,
};
use nuncio_core::providers::DavService;
use nuncio_core::traits::{CredentialProvider, MailAuth, TokenResource};
use nuncio_core::{AppError, CoreContext, Paths, Result};
use nuncio_pim::dav::DavClient;
use nuncio_pim::provider::{CalendarCursor, EventWindow, RemoteCalendar};
use nuncio_pim::HttpClient;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

struct TestCreds;

#[async_trait]
impl CredentialProvider for TestCreds {
    async fn mail_auth(&self, _account: &Account) -> Result<MailAuth> {
        Err(AppError::unsupported("no mail"))
    }

    async fn api_token(&self, _account_id: &str, _resource: TokenResource) -> Result<String> {
        Err(AppError::unsupported("no oauth"))
    }

    async fn dav_auth(&self, _account: &Account, _service: DavService) -> Result<(String, String)> {
        Ok(("user".into(), "password".into()))
    }

    async fn has_dav_password(&self, _account_id: &str, _service: DavService) -> bool {
        false
    }

    async fn report_auth_failure(&self, _account_id: &str, _resource: TokenResource) {}
}

struct MockResponse {
    status: u16,
    reason: &'static str,
    body: String,
}

struct MockServer {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    handle: JoinHandle<()>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl MockServer {
    async fn start(responses: Vec<MockResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let handle = tokio::spawn(async move {
            for response in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = Vec::new();
                loop {
                    let mut chunk = [0u8; 4096];
                    match socket.read(&mut chunk).await {
                        Ok(0) => break,
                        Ok(read) => {
                            buffer.extend_from_slice(&chunk[..read]);
                            // Read the whole body, not just the headers: the
                            // tests assert on the sync-token a page request
                            // carried.
                            if let Some(position) = find_headers_end(&buffer) {
                                let head = String::from_utf8_lossy(&buffer[..position]).to_string();
                                let length = content_length(&head).unwrap_or(0);
                                if buffer.len() >= position + 4 + length {
                                    break;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                log.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(String::from_utf8_lossy(&buffer).into_owned());
                let bytes = response.body.as_bytes();
                let head = format!(
                    "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nContent-Type: application/xml; charset=utf-8\r\nConnection: close\r\n\r\n",
                    response.status,
                    response.reason,
                    bytes.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(bytes).await;
                let _ = socket.shutdown().await;
            }
        });
        Self {
            base: format!("http://{addr}"),
            requests,
            handle,
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

fn find_headers_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn content_length(head: &str) -> Option<usize> {
    head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            value.trim().parse().ok()
        } else {
            None
        }
    })
}

async fn test_account(server: &MockServer) -> Account {
    let dir = std::env::temp_dir().join(format!("nuncio-pim-dav-{}", new_id()));
    let ctx = CoreContext::for_test(Paths::new(dir)).await.expect("ctx");
    let mut account = ctx
        .db
        .read(nuncio_core::repo::accounts::local)
        .await
        .expect("local account");
    account.email = "me@example.com".into();
    account.auth_kind = AuthKind::Password;
    account.caldav_url = Some(format!("{}/cal/", server.base));
    account
}

async fn client(server: &MockServer) -> (DavClient, RemoteCalendar) {
    let account = test_account(server).await;
    let http = HttpClient::new(Arc::new(TestCreds)).expect("http");
    let client = DavClient::new(http, account);
    let remote = RemoteCalendar {
        local_id: "local-calendar".into(),
        remote_id: format!("{}/cal/", server.base),
        href: Some(format!("{}/cal/", server.base)),
        name: "Calendar".into(),
        color: Some("#0078D4".into()),
        read_only: false,
        sync_token: None,
        ctag: None,
        supports_sync_collection: true,
    };
    (client, remote)
}

fn ics_with(uid: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Test//EN\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nSUMMARY:Dav event\r\nDTSTART:20261112T090000Z\r\nDTEND:20261112T100000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

fn ics() -> String {
    ics_with("event-1")
}

#[tokio::test]
async fn sync_collection_then_multiget() {
    let sync_body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/cal/event-1.ics</d:href>
    <d:propstat><d:prop><d:getetag>"e2"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/cal/deleted.ics</d:href>
    <d:status>HTTP/1.1 404 Not Found</d:status>
  </d:response>
  <d:sync-token>token-2</d:sync-token>
</d:multistatus>"#;
    let multiget_body = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/cal/event-1.ics</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e2"</d:getetag><cal:calendar-data>{}</cal:calendar-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#,
        ics()
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    );
    let server = MockServer::start(vec![
        MockResponse {
            status: 207,
            reason: "Multi-Status",
            body: sync_body.to_string(),
        },
        MockResponse {
            status: 207,
            reason: "Multi-Status",
            body: multiget_body,
        },
    ])
    .await;
    let (client, remote) = client(&server).await;
    let cursor = CalendarCursor {
        local_calendar_id: "local-calendar".into(),
        sync_token: Some("token-1".into()),
        ctag: None,
        etags: Vec::new(),
    };
    let delta = client
        .sync_events(
            &remote,
            &cursor,
            EventWindow::default_window(chrono::Utc::now()),
        )
        .await
        .expect("sync");
    assert_eq!(delta.sync_token.as_deref(), Some("token-2"));
    assert_eq!(delta.deleted_hrefs.len(), 1);
    assert_eq!(delta.events.len(), 1);
    assert_eq!(delta.events[0].event.title, "Dav event");
    assert_eq!(delta.events[0].etag.as_deref(), Some("e2"));
    assert!(delta.events[0]
        .remote_id
        .as_deref()
        .unwrap_or_default()
        .ends_with("/cal/event-1.ics"));

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("REPORT "));
    assert!(requests[0].contains("sync-collection"));
    assert!(requests[1].contains("calendar-multiget"));
}

/// RFC 6578 §3.6: a `507` on the collection's own response means the result
/// set was truncated, not that the report failed. The sync-token returned with
/// the page acknowledges exactly its members, so the client must save it and
/// request the next page. Treating the `507` as a failed round kept the old
/// token and re-requested the same page forever.
#[tokio::test]
async fn sync_collection_507_pages_until_complete() {
    let page_one = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/cal/</d:href>
    <d:status>HTTP/1.1 507 Insufficient Storage</d:status>
  </d:response>
  <d:response>
    <d:href>/cal/event-1.ics</d:href>
    <d:propstat><d:prop><d:getetag>"e1"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:sync-token>token-2</d:sync-token>
</d:multistatus>"#;
    let page_two = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/cal/event-2.ics</d:href>
    <d:propstat><d:prop><d:getetag>"e2"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:sync-token>token-3</d:sync-token>
</d:multistatus>"#;
    let multiget_one = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/cal/event-1.ics</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e1"</d:getetag><cal:calendar-data>{}</cal:calendar-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#,
        ics_with("event-1")
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    );
    let multiget_two = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/cal/event-2.ics</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e2"</d:getetag><cal:calendar-data>{}</cal:calendar-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#,
        ics_with("event-2")
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    );
    let server = MockServer::start(vec![
        MockResponse {
            status: 207,
            reason: "Multi-Status",
            body: page_one.to_string(),
        },
        MockResponse {
            status: 207,
            reason: "Multi-Status",
            body: multiget_one,
        },
        MockResponse {
            status: 207,
            reason: "Multi-Status",
            body: page_two.to_string(),
        },
        MockResponse {
            status: 207,
            reason: "Multi-Status",
            body: multiget_two,
        },
    ])
    .await;
    let (client, remote) = client(&server).await;
    let cursor = CalendarCursor {
        local_calendar_id: "local-calendar".into(),
        sync_token: Some("token-1".into()),
        ctag: None,
        etags: Vec::new(),
    };
    let delta = client
        .sync_events(
            &remote,
            &cursor,
            EventWindow::default_window(chrono::Utc::now()),
        )
        .await
        .expect("sync");
    assert!(!delta.incomplete, "a truncated page is not a failed round");
    assert!(
        delta.deleted_hrefs.is_empty(),
        "a truncated page carries no deletions"
    );
    assert_eq!(delta.events.len(), 2, "both pages must be applied");
    assert_eq!(
        delta.sync_token.as_deref(),
        Some("token-3"),
        "the cursor must end on the last page's token"
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    assert!(requests[0].contains("sync-collection"));
    assert!(requests[1].contains("calendar-multiget"));
    assert!(
        requests[2].contains("token-2"),
        "the second page must resume from the truncated page's token"
    );
    assert!(requests[3].contains("calendar-multiget"));
}

#[tokio::test]
async fn ctag_fallback_detects_changes() {
    let listing = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/">
  <d:response>
    <d:href>/cal/</d:href>
    <d:propstat><d:prop><cs:getctag>ctag-2</cs:getctag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/cal/event-1.ics</d:href>
    <d:propstat><d:prop><d:getetag>"e2"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;
    let multiget = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/cal/event-1.ics</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e2"</d:getetag><cal:calendar-data>{}</cal:calendar-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#,
        ics()
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    );
    let server = MockServer::start(vec![
        MockResponse {
            status: 207,
            reason: "Multi-Status",
            body: listing.to_string(),
        },
        MockResponse {
            status: 207,
            reason: "Multi-Status",
            body: multiget,
        },
    ])
    .await;
    let (client, remote) = client(&server).await;
    let cursor = CalendarCursor {
        local_calendar_id: "local-calendar".into(),
        sync_token: None,
        ctag: Some("ctag-1".into()),
        etags: Vec::new(),
    };
    let delta = client
        .sync_events(
            &remote,
            &cursor,
            EventWindow::default_window(chrono::Utc::now()),
        )
        .await
        .expect("sync");
    assert_eq!(delta.ctag.as_deref(), Some("ctag-2"));
    assert_eq!(delta.events.len(), 1);
    assert!(server.requests()[0].starts_with("PROPFIND "));
}

/// A `207` listing whose own response for the collection is a `503` is not an
/// empty calendar. Deriving deletions from its missing members would delete
/// every cached event. The round must fail and the cursor must not advance.
#[tokio::test]
async fn collection_level_503_fails_the_round_instead_of_deleting_events() {
    let listing = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/cal/</d:href>
    <d:status>HTTP/1.1 503 Service Unavailable</d:status>
  </d:response>
</d:multistatus>"#;
    let server = MockServer::start(vec![MockResponse {
        status: 207,
        reason: "Multi-Status",
        body: listing.to_string(),
    }])
    .await;
    let (client, remote) = client(&server).await;
    let cursor = CalendarCursor {
        local_calendar_id: "local-calendar".into(),
        sync_token: None,
        ctag: Some("ctag-1".into()),
        etags: vec![(format!("{}/cal/kept.ics", server.base), "e-kept".into())],
    };
    let result = client
        .sync_events(
            &remote,
            &cursor,
            EventWindow::default_window(chrono::Utc::now()),
        )
        .await;
    assert!(
        result.is_err(),
        "a failed collection response must fail the round, not look like an empty calendar"
    );
    assert_eq!(
        server.requests().len(),
        1,
        "no follow-up request may run after a failed listing"
    );
}

/// The ETag fallback is a full listing. A listed object whose `getetag` could
/// not be read (`503` + empty element) must not be treated as absent: doing so
/// put its href in `deleted_hrefs` and deleted the local event. The round is
/// incomplete, so the cursor stays before the unread identity.
#[tokio::test]
async fn ctag_fallback_keeps_listed_event_with_unreadable_etag() {
    let listing = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/">
  <d:response>
    <d:href>/cal/</d:href>
    <d:propstat><d:prop><cs:getctag>ctag-2</cs:getctag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/cal/event-1.ics</d:href>
    <d:propstat>
      <d:prop><d:getetag/></d:prop>
      <d:status>HTTP/1.1 503 Service Unavailable</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
    let server = MockServer::start(vec![MockResponse {
        status: 207,
        reason: "Multi-Status",
        body: listing.to_string(),
    }])
    .await;
    let (client, remote) = client(&server).await;
    let cursor = CalendarCursor {
        local_calendar_id: "local-calendar".into(),
        sync_token: None,
        ctag: Some("ctag-1".into()),
        etags: vec![(format!("{}/cal/event-1.ics", server.base), "e-old".into())],
    };
    let delta = client
        .sync_events(
            &remote,
            &cursor,
            EventWindow::default_window(chrono::Utc::now()),
        )
        .await
        .expect("sync");
    assert!(delta.incomplete, "the round must be marked uncertain");
    assert!(
        delta.deleted_hrefs.is_empty(),
        "a listed but unreadable resource is not a deletion"
    );
    assert!(delta.events.is_empty(), "no body could be read");
    assert_eq!(
        delta.ctag.as_deref(),
        Some("ctag-1"),
        "the old cursor must be retained"
    );
    assert_eq!(
        server.requests().len(),
        1,
        "no multiget should be issued without a readable ETag"
    );
}

/// `sync-collection` can name a changed member whose `getetag` fails (`503`
/// with an empty element). The change was not read, so the new sync token must
/// not be acknowledged.
#[tokio::test]
async fn sync_collection_unreadable_member_is_not_acknowledged() {
    let sync_body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/cal/event-1.ics</d:href>
    <d:propstat>
      <d:prop><d:getetag/></d:prop>
      <d:status>HTTP/1.1 503 Service Unavailable</d:status>
    </d:propstat>
  </d:response>
  <d:sync-token>token-2</d:sync-token>
</d:multistatus>"#;
    let server = MockServer::start(vec![MockResponse {
        status: 207,
        reason: "Multi-Status",
        body: sync_body.to_string(),
    }])
    .await;
    let (client, remote) = client(&server).await;
    let cursor = CalendarCursor {
        local_calendar_id: "local-calendar".into(),
        sync_token: Some("token-1".into()),
        ctag: None,
        etags: Vec::new(),
    };
    let delta = client
        .sync_events(
            &remote,
            &cursor,
            EventWindow::default_window(chrono::Utc::now()),
        )
        .await
        .expect("sync");
    assert!(delta.incomplete, "the round must be marked uncertain");
    assert!(delta.events.is_empty());
    assert!(delta.deleted_hrefs.is_empty());
    assert_eq!(
        delta.sync_token.as_deref(),
        Some("token-1"),
        "the unread change must stay ahead of the cursor"
    );
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn put_and_delete_use_preconditions() {
    let server = MockServer::start(vec![
        MockResponse {
            status: 201,
            reason: "Created",
            body: String::new(),
        },
        MockResponse {
            status: 204,
            reason: "No Content",
            body: String::new(),
        },
        MockResponse {
            status: 204,
            reason: "No Content",
            body: String::new(),
        },
    ])
    .await;
    let (client, remote) = client(&server).await;
    let event = CalendarEvent::new(
        "local-event",
        "local-calendar",
        "local",
        "uid@example.com",
        "Title",
        "2026-11-12T09:00:00Z",
        "2026-11-12T10:00:00Z",
    );
    client.create_event(&remote, &event).await.expect("create");

    let current = StoredEvent {
        event: event.clone(),
        raw_ics: None,
        etag: Some("e1".into()),
        remote_id: Some(format!("{}/cal/uid%40example.com.ics", server.base)),
        href: Some(format!("{}/cal/uid%40example.com.ics", server.base)),
        rrule: None,
        exdates: Vec::new(),
    };
    let mut updated = event.clone();
    updated.title = "Renamed".into();
    client
        .update_event(
            &remote,
            &current,
            std::slice::from_ref(&current),
            &updated,
            RecurrenceEditScope::All,
            None,
        )
        .await
        .expect("update");
    client
        .delete_event(
            &remote,
            &current,
            std::slice::from_ref(&current),
            RecurrenceEditScope::All,
            None,
        )
        .await
        .expect("delete");

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let first = requests[0].to_ascii_lowercase();
    assert!(first.starts_with("put "));
    assert!(first.contains("if-none-match: *"), "{first}");
    let second = requests[1].to_ascii_lowercase();
    assert!(second.contains("if-match:"), "{second}");
    assert!(second.contains("\"e1\""), "{second}");
    assert!(requests[2].starts_with("DELETE "));
}

#[tokio::test]
async fn creates_imported_series_with_a_single_put() {
    let server = MockServer::start(vec![MockResponse {
        status: 201,
        reason: "Created",
        body: String::new(),
    }])
    .await;
    let (client, remote) = client(&server).await;
    let mut master = CalendarEvent::new(
        "master",
        "local-calendar",
        "local",
        "series@example.com",
        "Weekly",
        "2026-11-12T09:00:00Z",
        "2026-11-12T10:00:00Z",
    );
    master.recurrence = Some(nuncio_core::models::RecurrenceRule {
        freq: nuncio_core::models::RecurrenceFreq::Weekly,
        interval: 1,
        ..Default::default()
    });
    let mut exception = CalendarEvent::new(
        "exception",
        "local-calendar",
        "local",
        "series@example.com",
        "Weekly moved",
        "2026-11-19T11:00:00Z",
        "2026-11-19T12:00:00Z",
    );
    exception.series_id = Some("master".into());
    exception.recurrence_id = Some("2026-11-19T09:00:00Z".into());
    let series = vec![StoredEvent::new(master), StoredEvent::new(exception)];
    client
        .create_series(&remote, &series)
        .await
        .expect("create series");

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "master and exception share one resource");
    let request = requests[0].to_ascii_lowercase();
    assert!(request.starts_with("put "), "{request}");
    assert!(request.contains("if-none-match: *"), "{request}");
}

/// A truncated `sync-collection` round is one ordered change stream. Page two
/// can report the same href as `404` after page one already returned its body.
/// Emitting both made consumers delete the event and then re-import the stale
/// body from page one. The newest page must win.
#[tokio::test]
async fn later_page_404_erases_an_earlier_fetched_event() {
    let page_one = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/cal/</d:href>
    <d:status>HTTP/1.1 507 Insufficient Storage</d:status>
  </d:response>
  <d:response>
    <d:href>/cal/event-1.ics</d:href>
    <d:propstat><d:prop><d:getetag>"e1"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:sync-token>token-2</d:sync-token>
</d:multistatus>"#;
    let multiget_one = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/cal/event-1.ics</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e1"</d:getetag><cal:calendar-data>{}</cal:calendar-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#,
        ics_with("event-1")
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    );
    let page_two = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/cal/event-1.ics</d:href>
    <d:status>HTTP/1.1 404 Not Found</d:status>
  </d:response>
  <d:sync-token>token-3</d:sync-token>
</d:multistatus>"#;
    let server = MockServer::start(vec![
        MockResponse {
            status: 207,
            reason: "Multi-Status",
            body: page_one.to_string(),
        },
        MockResponse {
            status: 207,
            reason: "Multi-Status",
            body: multiget_one,
        },
        MockResponse {
            status: 207,
            reason: "Multi-Status",
            body: page_two.to_string(),
        },
    ])
    .await;
    let (client, remote) = client(&server).await;
    let cursor = CalendarCursor {
        local_calendar_id: "local-calendar".into(),
        sync_token: Some("token-1".into()),
        ctag: None,
        etags: Vec::new(),
    };
    let delta = client
        .sync_events(
            &remote,
            &cursor,
            EventWindow::default_window(chrono::Utc::now()),
        )
        .await
        .expect("sync");
    let href = format!("{}/cal/event-1.ics", server.base);
    assert!(
        delta.events.is_empty(),
        "the later page's deletion must erase the earlier body"
    );
    assert_eq!(
        delta.deleted_hrefs,
        vec![href],
        "the event is deleted exactly once"
    );
    assert!(!delta.incomplete);
    assert_eq!(
        delta.sync_token.as_deref(),
        Some("token-3"),
        "the cursor reaches the last page"
    );
    assert_eq!(server.requests().len(), 3);
}

/// RFC 3986 §6.2.2: `%41` and `A` are the same resource. A later page's `404`
/// must erase a body fetched for that resource on an earlier page even when the
/// two pages spell it differently. Keeping raw strings as the merge key meant
/// the delta kept the stale body *and* acknowledged the new token.
async fn assert_equivalent_spelling_merges(body_href: &str, deletion_href: &str) {
    let page_one = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/cal/</d:href>
    <d:status>HTTP/1.1 507 Insufficient Storage</d:status>
  </d:response>
  <d:response>
    <d:href>{body_href}</d:href>
    <d:propstat><d:prop><d:getetag>"e1"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:sync-token>token-2</d:sync-token>
</d:multistatus>"#
    );
    let multiget_one = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>{body_href}</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e1"</d:getetag><cal:calendar-data>{}</cal:calendar-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#,
        ics_with("event-1")
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    );
    let page_two = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>{deletion_href}</d:href>
    <d:status>HTTP/1.1 404 Not Found</d:status>
  </d:response>
  <d:sync-token>token-3</d:sync-token>
</d:multistatus>"#
    );
    let server = MockServer::start(vec![
        MockResponse {
            status: 207,
            reason: "Multi-Status",
            body: page_one,
        },
        MockResponse {
            status: 207,
            reason: "Multi-Status",
            body: multiget_one,
        },
        MockResponse {
            status: 207,
            reason: "Multi-Status",
            body: page_two,
        },
    ])
    .await;
    let (client, remote) = client(&server).await;
    let cursor = CalendarCursor {
        local_calendar_id: "local-calendar".into(),
        sync_token: Some("token-1".into()),
        ctag: None,
        etags: Vec::new(),
    };
    let delta = client
        .sync_events(
            &remote,
            &cursor,
            EventWindow::default_window(chrono::Utc::now()),
        )
        .await
        .expect("sync");
    assert!(
        delta.events.is_empty(),
        "the later page's deletion must erase the earlier body even across spellings"
    );
    assert_eq!(
        delta.deleted_hrefs,
        vec![format!("{}{}", server.base, deletion_href)],
        "the deletion is emitted once, in the newest raw spelling"
    );
    assert!(!delta.incomplete);
    assert_eq!(
        delta.sync_token.as_deref(),
        Some("token-3"),
        "the cursor reaches the last page"
    );
    assert_eq!(server.requests().len(), 3);
}

#[tokio::test]
async fn later_page_404_erases_a_body_spelled_with_an_equivalent_encoding() {
    assert_equivalent_spelling_merges("/cal/%41.ics", "/cal/A.ics").await;
}

#[tokio::test]
async fn later_page_404_spelled_with_an_equivalent_encoding_erases_a_plain_body() {
    assert_equivalent_spelling_merges("/cal/A.ics", "/cal/%41.ics").await;
}

/// The response set of a multiget is compared by identity too: a server that
/// answers `A.ics` for a requested `%41.ics` is answering the same resource,
/// not an unrequested href that must be ignored.
#[tokio::test]
async fn multiget_accepts_an_equivalent_spelling_of_the_requested_href() {
    let sync_body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/cal/%41.ics</d:href>
    <d:propstat><d:prop><d:getetag>"e1"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:sync-token>token-2</d:sync-token>
</d:multistatus>"#;
    let multiget_body = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/cal/A.ics</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e1"</d:getetag><cal:calendar-data>{}</cal:calendar-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#,
        ics_with("event-1")
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    );
    let server = MockServer::start(vec![
        MockResponse {
            status: 207,
            reason: "Multi-Status",
            body: sync_body.to_string(),
        },
        MockResponse {
            status: 207,
            reason: "Multi-Status",
            body: multiget_body,
        },
    ])
    .await;
    let (client, remote) = client(&server).await;
    let cursor = CalendarCursor {
        local_calendar_id: "local-calendar".into(),
        sync_token: Some("token-1".into()),
        ctag: None,
        etags: Vec::new(),
    };
    let delta = client
        .sync_events(
            &remote,
            &cursor,
            EventWindow::default_window(chrono::Utc::now()),
        )
        .await
        .expect("the equivalent response must be accepted");
    assert_eq!(delta.events.len(), 1);
    let expected_href = format!("{}/cal/A.ics", server.base);
    assert_eq!(
        delta.events[0].href.as_deref(),
        Some(expected_href.as_str()),
        "the raw response spelling is kept"
    );
    assert!(delta.deleted_hrefs.is_empty());
    assert_eq!(delta.sync_token.as_deref(), Some("token-2"));
}

/// The ETag fallback is a full listing and its baseline is compared by
/// identity. A resource spelled `%41.ics` after a round that stored `A.ics`
/// is neither changed nor deleted.
#[tokio::test]
async fn etag_fallback_compares_equivalent_spellings_by_identity() {
    let listing = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/">
  <d:response>
    <d:href>/cal/</d:href>
    <d:propstat><d:prop><cs:getctag>ctag-2</cs:getctag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/cal/%41.ics</d:href>
    <d:propstat><d:prop><d:getetag>"e1"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;
    let server = MockServer::start(vec![MockResponse {
        status: 207,
        reason: "Multi-Status",
        body: listing.to_string(),
    }])
    .await;
    let (client, remote) = client(&server).await;
    let cursor = CalendarCursor {
        local_calendar_id: "local-calendar".into(),
        sync_token: None,
        ctag: Some("ctag-1".into()),
        etags: vec![(format!("{}/cal/A.ics", server.base), "e1".into())],
    };
    let delta = client
        .sync_events(
            &remote,
            &cursor,
            EventWindow::default_window(chrono::Utc::now()),
        )
        .await
        .expect("sync");
    assert!(
        delta.events.is_empty(),
        "an unchanged resource must not be re-fetched"
    );
    assert!(
        delta.deleted_hrefs.is_empty(),
        "an equivalent spelling is not a deletion"
    );
    assert_eq!(delta.ctag.as_deref(), Some("ctag-2"));
    assert_eq!(
        server.requests().len(),
        1,
        "no multiget is needed for an unchanged resource"
    );
}

/// A member href must belong to the collection being synced, not merely to an
/// allow-listed origin. `/other-calendar/event.ics` used to be accepted as a
/// member of `/cal/` and imported across calendars.
#[tokio::test]
async fn href_from_another_calendar_is_not_a_member() {
    let listing = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/">
  <d:response>
    <d:href>/cal/</d:href>
    <d:propstat><d:prop><cs:getctag>ctag-2</cs:getctag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/cal/kept.ics</d:href>
    <d:propstat><d:prop><d:getetag>"e-kept"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/other-calendar/event.ics</d:href>
    <d:propstat><d:prop><d:getetag>"e-other"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;
    let server = MockServer::start(vec![MockResponse {
        status: 207,
        reason: "Multi-Status",
        body: listing.to_string(),
    }])
    .await;
    let (client, remote) = client(&server).await;
    let cursor = CalendarCursor {
        local_calendar_id: "local-calendar".into(),
        sync_token: None,
        ctag: Some("ctag-1".into()),
        etags: vec![(format!("{}/cal/kept.ics", server.base), "e-kept".into())],
    };
    let result = client
        .sync_events(
            &remote,
            &cursor,
            EventWindow::default_window(chrono::Utc::now()),
        )
        .await;
    assert!(
        result.is_err(),
        "a response outside the collection is not authoritative"
    );
    assert_eq!(
        server.requests().len(),
        1,
        "no multiget may follow a listing with a foreign member"
    );
}

/// `calendar-multiget` responses are evidence only for the hrefs this batch
/// requested. A body for `/other-calendar/event.ics` used to be imported even
/// though the request never named it.
#[tokio::test]
async fn multiget_response_for_an_unrequested_href_is_ignored() {
    let sync_body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/cal/event-1.ics</d:href>
    <d:propstat><d:prop><d:getetag>"e1"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:sync-token>token-2</d:sync-token>
</d:multistatus>"#;
    let multiget = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/cal/event-1.ics</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e1"</d:getetag><cal:calendar-data>{}</cal:calendar-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/other-calendar/event-2.ics</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e2"</d:getetag><cal:calendar-data>{}</cal:calendar-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#,
        ics_with("event-1")
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;"),
        ics_with("event-2")
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    );
    let server = MockServer::start(vec![
        MockResponse {
            status: 207,
            reason: "Multi-Status",
            body: sync_body.to_string(),
        },
        MockResponse {
            status: 207,
            reason: "Multi-Status",
            body: multiget,
        },
    ])
    .await;
    let (client, remote) = client(&server).await;
    let cursor = CalendarCursor {
        local_calendar_id: "local-calendar".into(),
        sync_token: Some("token-1".into()),
        ctag: None,
        etags: Vec::new(),
    };
    let delta = client
        .sync_events(
            &remote,
            &cursor,
            EventWindow::default_window(chrono::Utc::now()),
        )
        .await
        .expect("sync");
    assert_eq!(delta.events.len(), 1, "only the requested href is imported");
    assert_eq!(delta.events[0].event.title, "Dav event");
    assert!(
        delta.events.iter().all(|event| event
            .remote_id
            .as_deref()
            .is_some_and(|id| id.ends_with("/cal/event-1.ics"))),
        "the unrequested response must not be imported"
    );
}
