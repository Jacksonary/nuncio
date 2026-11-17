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
                            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
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

fn ics() -> String {
    "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Test//EN\r\nBEGIN:VEVENT\r\nUID:event-1\r\nSUMMARY:Dav event\r\nDTSTART:20261112T090000Z\r\nDTEND:20261112T100000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".to_string()
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
