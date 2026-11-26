//! CardDAV contact provider tests against a tiny in-process HTTP/1.1 mock.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nuncio_core::models::{
    new_id, Account, AddressBook, AuthKind, Contact, ContactField, ContactFieldType, ProviderKind,
};
use nuncio_core::providers::DavService;
use nuncio_core::repo;
use nuncio_core::traits::{CredentialProvider, MailAuth, TokenResource};
use nuncio_core::{AppError, CoreContext, Paths, Result};
use nuncio_pim::contacts::provider::{
    ContactCursor, ContactProvider, RemoteAddressBook, RemoteContactRef,
};
use nuncio_pim::contacts::CardDavProvider;
use nuncio_pim::op_worker::PimOpExecutor;
use nuncio_pim::store::AddressBookRemote;
use nuncio_pim::{ContactsService, DavClient, HttpClient};
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
        Self::start_with_uids(responses, Vec::new()).await
    }

    /// Like [`MockServer::start`], but the address book already contains the
    /// given `(UID, href)` pairs. The mock enforces RFC 6352 §5.1 UID
    /// uniqueness against them (and across accepted PUTs), so a regression
    /// test cannot be masked by a canned `204`.
    async fn start_with_uids(
        responses: Vec<MockResponse>,
        existing_uids: Vec<(String, String)>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let mut known: KnownUids = existing_uids.into_iter().collect();
        let handle = tokio::spawn(async move {
            for response in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = Vec::new();
                loop {
                    let mut chunk = [0u8; 8192];
                    match socket.read(&mut chunk).await {
                        Ok(0) => break,
                        Ok(read) => {
                            buffer.extend_from_slice(&chunk[..read]);
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
                let request = String::from_utf8_lossy(&buffer).into_owned();
                log.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(request.clone());
                // A real CardDAV server validates the address object it is
                // asked to store. Enforcing the same rules here means a canned
                // `204` can no longer hide a PUT that omits the UID or reuses
                // another resource's UID.
                let response = match vcard_put_rejection(&request, &known) {
                    Some(rejection) => rejection,
                    None => {
                        if response.status < 400 {
                            if let Some((target, Some(uid))) = parse_vcard_put(&request) {
                                known.insert(uid, target);
                            }
                        }
                        response
                    }
                };
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

/// UID → resource target of every card the mock address book knows about.
type KnownUids = HashMap<String, String>;

/// Parse a vCard `PUT`: its request target and, when present, its non-empty
/// `UID`.
fn parse_vcard_put(request: &str) -> Option<(String, Option<String>)> {
    let (head, body) = request.split_once("\r\n\r\n")?;
    let mut parts = head.lines().next()?.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?.to_string();
    if method != "PUT" || !body.contains("BEGIN:VCARD") {
        return None;
    }
    let uid = body
        .lines()
        .find_map(|line| line.strip_prefix("UID:"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    Some((target, uid))
}

/// Reduce an absolute or origin-form request target to its path for identity
/// comparison.
fn resource_path(target: &str) -> &str {
    let rest = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("https://"));
    match rest {
        Some(rest) => rest.find('/').map(|index| &rest[index..]).unwrap_or("/"),
        None => target,
    }
}

fn valid_address_data() -> MockResponse {
    MockResponse {
        status: 403,
        reason: "Forbidden",
        body: "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<d:error xmlns:d=\"DAV:\" xmlns:card=\"urn:ietf:params:xml:ns:carddav\">\
<card:valid-address-data/></d:error>"
            .to_string(),
    }
}

fn no_uid_conflict() -> MockResponse {
    MockResponse {
        status: 403,
        reason: "Forbidden",
        body: "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<d:error xmlns:d=\"DAV:\" xmlns:card=\"urn:ietf:params:xml:ns:carddav\">\
<card:no-uid-conflict/></d:error>"
            .to_string(),
    }
}

/// RFC 6352 §5.1 / §6.3.2: a `PUT` to an address object resource must carry a
/// vCard with a non-empty `UID` (`403 valid-address-data`), and that UID must
/// be unique among the address book's resources (`403 no-uid-conflict`).
/// Reusing a UID on the *same* resource is an ordinary update and is allowed.
/// Returns the rejection for such a request so the mock can emit it instead of
/// the canned response.
fn vcard_put_rejection(request: &str, known: &KnownUids) -> Option<MockResponse> {
    let (target, uid) = parse_vcard_put(request)?;
    let Some(uid) = uid else {
        return Some(valid_address_data());
    };
    match known.get(&uid) {
        Some(existing) if resource_path(existing) != resource_path(&target) => {
            Some(no_uid_conflict())
        }
        _ => None,
    }
}

/// The vCard `UID` carried by a recorded `PUT`.
fn request_uid(request: &str) -> String {
    parse_vcard_put(request)
        .and_then(|(_, uid)| uid)
        .expect("PUT must carry a UID")
}

async fn account(server: &MockServer) -> Account {
    let dir = std::env::temp_dir().join(format!("nuncio-pim-carddav-{}", new_id()));
    let ctx = CoreContext::for_test(Paths::new(dir)).await.expect("ctx");
    let mut account = ctx
        .db
        .read(nuncio_core::repo::accounts::local)
        .await
        .expect("local account");
    account.email = "me@example.com".into();
    account.auth_kind = AuthKind::Password;
    account.carddav_url = Some(format!("{}/ab/", server.base));
    account
}

fn book(server: &MockServer) -> RemoteAddressBook {
    RemoteAddressBook {
        local_id: "local-contacts".into(),
        remote_id: format!("{}/ab/", server.base),
        href: Some(format!("{}/ab/", server.base)),
        name: "Contacts".into(),
        read_only: false,
    }
}

fn vcard(uid: &str, name: &str, email: &str, groups: &str) -> String {
    format!(
        "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:{uid}\r\nFN:{name}\r\nEMAIL;TYPE=WORK:{email}\r\nCATEGORIES:{groups}\r\nEND:VCARD\r\n"
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[tokio::test]
async fn syncs_address_book_with_sync_collection() {
    let first = xml_escape(&vcard(
        "card-1",
        "Ada Lovelace",
        "ada@example.com",
        "Friends",
    ));
    let second = xml_escape(&vcard(
        "card-2",
        "Alan Turing",
        "alan@example.com",
        "Friends,Mathematicians",
    ));
    let sync_body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/ab/card-1.vcf</d:href>
    <d:propstat><d:prop><d:getetag>"e1"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/ab/card-2.vcf</d:href>
    <d:propstat><d:prop><d:getetag>"e2"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/ab/gone.vcf</d:href>
    <d:status>HTTP/1.1 404 Not Found</d:status>
  </d:response>
  <d:sync-token>token-2</d:sync-token>
</d:multistatus>"#;
    let multiget = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:response>
    <d:href>/ab/card-1.vcf</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e1"</d:getetag><card:address-data>{first}</card:address-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/ab/card-2.vcf</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e2"</d:getetag><card:address-data>{second}</card:address-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
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
    let account = account(&server).await;
    let http = HttpClient::new(Arc::new(TestCreds)).expect("http");
    let provider = CardDavProvider::new(http);
    let cursor = ContactCursor {
        local_address_book_id: "local-contacts".into(),
        sync_token: Some("token-1".into()),
        ctag: None,
        etags: Vec::new(),
    };
    let delta = provider
        .sync_contacts(&account, &book(&server), &cursor)
        .await
        .expect("sync");
    assert_eq!(delta.sync_token.as_deref(), Some("token-2"));
    assert_eq!(delta.deleted_hrefs.len(), 1);
    assert_eq!(delta.contacts.len(), 2);
    let ada = delta
        .contacts
        .iter()
        .find(|contact| contact.remote_id.as_deref() == Some("card-1"))
        .expect("ada");
    assert_eq!(ada.contact.display_name, "Ada Lovelace");
    assert_eq!(ada.etag.as_deref(), Some("e1"));
    assert_eq!(ada.group_names, vec!["Friends".to_string()]);
    let alan = delta
        .contacts
        .iter()
        .find(|contact| contact.remote_id.as_deref() == Some("card-2"))
        .expect("alan");
    assert_eq!(alan.group_names.len(), 2);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("REPORT "));
    assert!(requests[0].contains("sync-collection"));
    assert!(requests[1].contains("addressbook-multiget"));
}

/// A vCard without a UID is identified by its resource href. The derived
/// remote id must use the normalized spelling so `%41.vcf` and `A.vcf` are one
/// contact (otherwise equivalent re-delivery bypasses its delete tombstone).
#[tokio::test]
async fn no_uid_contact_uses_normalized_href_as_remote_id() {
    let body = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:No Uid\r\nEMAIL;TYPE=WORK:no-uid@example.com\r\nEND:VCARD\r\n";
    let escaped = xml_escape(body);
    let sync_body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/ab/%41.vcf</d:href>
    <d:propstat><d:prop><d:getetag>"e1"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:sync-token>token-2</d:sync-token>
</d:multistatus>"#;
    let multiget = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:response>
    <d:href>/ab/%41.vcf</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e1"</d:getetag><card:address-data>{escaped}</card:address-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
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
    let account = account(&server).await;
    let http = HttpClient::new(Arc::new(TestCreds)).expect("http");
    let provider = CardDavProvider::new(http);
    let cursor = ContactCursor {
        local_address_book_id: "local-contacts".into(),
        sync_token: Some("token-1".into()),
        ctag: None,
        etags: Vec::new(),
    };
    let delta = provider
        .sync_contacts(&account, &book(&server), &cursor)
        .await
        .expect("sync");
    assert_eq!(delta.contacts.len(), 1);
    let contact = &delta.contacts[0];
    assert_eq!(
        contact.remote_id.as_deref(),
        Some("A.vcf"),
        "the remote id must be derived from the normalized href"
    );
    assert!(
        contact
            .href
            .as_deref()
            .is_some_and(|href| href.ends_with("/ab/%41.vcf")),
        "the raw spelling is kept for requests"
    );
}

/// A server with neither ctag nor sync-token always takes the ETag fallback
/// and marks the batch `full_sync`. Unchanged objects are not re-downloaded,
/// so the delta must expose the complete server listing separately; pruning
/// only by `contacts` would delete them locally.
#[tokio::test]
async fn etag_fallback_reports_full_listing_without_refetching_unchanged() {
    let listing = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/ab/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/ab/unchanged.vcf</d:href>
    <d:propstat><d:prop><d:getetag>"e-same"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/ab/changed.vcf</d:href>
    <d:propstat><d:prop><d:getetag>"e-new"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;
    let changed_vcard = xml_escape(&vcard("card-changed", "Changed", "changed@example.com", ""));
    let multiget = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:response>
    <d:href>/ab/changed.vcf</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e-new"</d:getetag><card:address-data>{changed_vcard}</card:address-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
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
    let account = account(&server).await;
    let http = HttpClient::new(Arc::new(TestCreds)).expect("http");
    let provider = CardDavProvider::new(http);
    let cursor = ContactCursor {
        local_address_book_id: "local-contacts".into(),
        sync_token: None,
        ctag: None,
        etags: vec![
            (format!("{}/ab/unchanged.vcf", server.base), "e-same".into()),
            (format!("{}/ab/changed.vcf", server.base), "e-old".into()),
        ],
    };
    let delta = provider
        .sync_contacts(&account, &book(&server), &cursor)
        .await
        .expect("sync");
    assert!(delta.full_sync, "the ETag fallback is a full sync");
    assert_eq!(delta.contacts.len(), 1, "only the moved ETag is fetched");
    assert_eq!(delta.deleted_hrefs.len(), 0);
    let mut listed = delta.listed_hrefs.clone();
    listed.sort();
    assert_eq!(
        listed,
        vec![
            format!("{}/ab/changed.vcf", server.base),
            format!("{}/ab/unchanged.vcf", server.base),
        ],
        "the delta must carry the whole authoritative listing"
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "no sync-collection REPORT expected");
    assert!(requests[0].starts_with("PROPFIND "));
    assert!(requests[1].contains("addressbook-multiget"));
    assert!(requests[1].contains("changed.vcf"));
    assert!(
        !requests[1].contains("unchanged.vcf"),
        "an unchanged object must not be re-downloaded"
    );
}

/// A resource can disappear between the PROPFIND listing and the multiget.
/// The newer `404` must override the older "the listing named it": the href is
/// a deletion, not a present object, so a full-sync consumer must neither keep
/// the local row nor replay its quarantine snapshot.
#[tokio::test]
async fn multiget_404_overrides_an_older_listing_entry() {
    let listing = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/ab/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/ab/gone.vcf</d:href>
    <d:propstat><d:prop><d:getetag>"e-gone-new"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/ab/kept.vcf</d:href>
    <d:propstat><d:prop><d:getetag>"e-kept-new"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;
    let kept_vcard = xml_escape(&vcard("card-kept", "Kept", "kept@example.com", ""));
    // The server names `gone.vcf` in the listing, then states explicitly that
    // the resource no longer exists.
    let multiget = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:response>
    <d:href>/ab/gone.vcf</d:href>
    <d:status>HTTP/1.1 404 Not Found</d:status>
  </d:response>
  <d:response>
    <d:href>/ab/kept.vcf</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e-kept-new"</d:getetag><card:address-data>{kept_vcard}</card:address-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
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
    let account = account(&server).await;
    let http = HttpClient::new(Arc::new(TestCreds)).expect("http");
    let provider = CardDavProvider::new(http);
    let gone = format!("{}/ab/gone.vcf", server.base);
    let kept = format!("{}/ab/kept.vcf", server.base);
    let cursor = ContactCursor {
        local_address_book_id: "local-contacts".into(),
        sync_token: None,
        ctag: None,
        etags: vec![
            (gone.clone(), "e-gone-old".into()),
            (kept.clone(), "e-kept-old".into()),
        ],
    };
    let delta = provider
        .sync_contacts(&account, &book(&server), &cursor)
        .await
        .expect("sync");
    assert!(delta.full_sync);
    assert_eq!(
        delta.contacts.len(),
        1,
        "only the surviving resource is fetched"
    );
    assert_eq!(delta.contacts[0].href.as_deref(), Some(kept.as_str()));
    assert!(
        delta.deleted_hrefs.contains(&gone),
        "a multiget 404 is definitive deletion evidence"
    );
    assert!(
        !delta.listed_hrefs.contains(&gone),
        "the older listing must not keep a deleted resource present"
    );
    assert!(
        delta.listed_hrefs.contains(&kept),
        "the authoritative listing keeps the surviving resource"
    );
}

/// A `207` listing whose own response for the collection is a `503` is not an
/// empty address book. Deriving deletions from its missing members would wipe
/// every cached contact. The round must fail, without deletion evidence and
/// without advancing the cursor.
#[tokio::test]
async fn collection_level_503_fails_the_round_instead_of_deleting_contacts() {
    let listing = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/ab/</d:href>
    <d:status>HTTP/1.1 503 Service Unavailable</d:status>
  </d:response>
</d:multistatus>"#;
    let server = MockServer::start(vec![MockResponse {
        status: 207,
        reason: "Multi-Status",
        body: listing.to_string(),
    }])
    .await;
    let account = account(&server).await;
    let http = HttpClient::new(Arc::new(TestCreds)).expect("http");
    let provider = CardDavProvider::new(http);
    let cursor = ContactCursor {
        local_address_book_id: "local-contacts".into(),
        sync_token: None,
        ctag: None,
        etags: vec![(format!("{}/ab/kept.vcf", server.base), "e-kept".into())],
    };
    let result = provider
        .sync_contacts(&account, &book(&server), &cursor)
        .await;
    assert!(
        result.is_err(),
        "a failed collection response must fail the round, not look like an empty address book"
    );
    assert_eq!(
        server.requests().len(),
        1,
        "no multiget (and therefore no deletion evidence) may follow a failed listing"
    );
}

/// A `207` listing can name an href while the per-resource properties fail
/// (here a `503` with no `getetag`). That is "listed but unreadable", not
/// "absent": it must not generate a deletion, its quarantine snapshot must be
/// retained, and the round must be marked incomplete so its identity is
/// retried instead of trusted.
#[tokio::test]
async fn listed_resource_with_503_is_not_treated_as_deleted() {
    let listing = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/ab/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/ab/stable.vcf</d:href>
    <d:propstat><d:prop><d:getetag>"e-stable"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/ab/temporarily-unreadable.vcf</d:href>
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
    let account = account(&server).await;
    let http = HttpClient::new(Arc::new(TestCreds)).expect("http");
    let provider = CardDavProvider::new(http);
    let stable = format!("{}/ab/stable.vcf", server.base);
    let unreadable = format!("{}/ab/temporarily-unreadable.vcf", server.base);
    let cursor = ContactCursor {
        local_address_book_id: "local-contacts".into(),
        sync_token: None,
        ctag: None,
        etags: vec![
            (stable.clone(), "e-stable".into()),
            (unreadable.clone(), "e-old".into()),
        ],
    };
    let delta = provider
        .sync_contacts(&account, &book(&server), &cursor)
        .await
        .expect("sync");
    assert!(delta.full_sync);
    assert!(delta.incomplete, "the round must be marked uncertain");
    assert!(
        delta.deleted_hrefs.is_empty(),
        "being listed but unreadable is not deletion evidence"
    );
    assert!(
        delta.listed_hrefs.contains(&unreadable),
        "the href stays present so its quarantine snapshot is retained"
    );
    assert_eq!(delta.contacts.len(), 0, "no identity was refreshed");
    assert_eq!(
        server.requests().len(),
        1,
        "no multiget should be issued without a readable ETag"
    );
}

/// A `sync-collection` response can name a changed member while its `getetag`
/// fails (`503` + an empty element). The change was not read, so the round must
/// be marked incomplete and keep the old cursor. Advancing the token here made
/// the next sync skip the object forever.
#[tokio::test]
async fn sync_collection_unreadable_member_etag_does_not_advance_cursor() {
    let sync_body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/ab/card-1.vcf</d:href>
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
    let account = account(&server).await;
    let http = HttpClient::new(Arc::new(TestCreds)).expect("http");
    let provider = CardDavProvider::new(http);
    let cursor = ContactCursor {
        local_address_book_id: "local-contacts".into(),
        sync_token: Some("token-1".into()),
        ctag: None,
        etags: Vec::new(),
    };
    let delta = provider
        .sync_contacts(&account, &book(&server), &cursor)
        .await
        .expect("sync");
    assert!(delta.incomplete, "the round must be marked uncertain");
    assert!(delta.contacts.is_empty(), "no body was read");
    assert!(
        delta.deleted_hrefs.is_empty(),
        "a named member is not a deletion"
    );
    assert_eq!(
        delta.sync_token.as_deref(),
        Some("token-1"),
        "the cursor must keep pointing before the unread change"
    );
    assert_eq!(
        server.requests().len(),
        1,
        "no multiget can be issued without an ETag"
    );
}

/// `addressbook-multiget` can answer `address-data` with a failed `propstat`
/// (`404`) and an empty element. That is not a delivered body: treating it as
/// one produced "zero contacts" with a fresh sync token. The batch must fail so
/// the cursor is not acknowledged.
#[tokio::test]
async fn multiget_property_level_404_is_not_a_successful_body() {
    let sync_body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/ab/card-1.vcf</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e1"</d:getetag></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:sync-token>token-2</d:sync-token>
</d:multistatus>"#;
    // `getetag` succeeds, but `address-data` is answered by a failed propstat
    // carrying an empty element.
    let multiget = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:response>
    <d:href>/ab/card-1.vcf</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e1"</d:getetag></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
    <d:propstat>
      <d:prop><card:address-data/></d:prop>
      <d:status>HTTP/1.1 404 Not Found</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
    let server = MockServer::start(vec![
        MockResponse {
            status: 207,
            reason: "Multi-Status",
            body: sync_body.to_string(),
        },
        MockResponse {
            status: 207,
            reason: "Multi-Status",
            body: multiget.to_string(),
        },
    ])
    .await;
    let account = account(&server).await;
    let http = HttpClient::new(Arc::new(TestCreds)).expect("http");
    let provider = CardDavProvider::new(http);
    let cursor = ContactCursor {
        local_address_book_id: "local-contacts".into(),
        sync_token: Some("token-1".into()),
        ctag: None,
        etags: Vec::new(),
    };
    let result = provider
        .sync_contacts(&account, &book(&server), &cursor)
        .await;
    assert!(
        result.is_err(),
        "an unread resource must not be acknowledged as a successful sync"
    );
}

#[tokio::test]
async fn writes_vcards_with_preconditions() {
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
    let account = account(&server).await;
    let http = HttpClient::new(Arc::new(TestCreds)).expect("http");
    let provider = CardDavProvider::new(http);
    let mut contact = Contact::new("local-id", "local-contacts", "local");
    contact.display_name = "Grace Hopper".into();
    contact.emails = vec![ContactField {
        r#type: ContactFieldType::Work,
        value: "grace@example.com".into(),
        is_primary: true,
    }];
    let created = provider
        .create_contact(&account, &book(&server), &contact, &["Navy".into()])
        .await
        .expect("create");
    assert_eq!(created.remote_id.as_deref(), Some("local-id"));
    assert!(created
        .href
        .as_deref()
        .unwrap_or_default()
        .ends_with("/ab/local-id.vcf"));

    let current = RemoteContactRef {
        remote_id: Some("local-id".into()),
        href: created.href.clone(),
        etag: Some("e1".into()),
        raw_vcard: None,
        uid_is_derived: false,
    };
    provider
        .update_contact(&account, &book(&server), &current, &contact, &[], None)
        .await
        .expect("update");
    provider
        .delete_contact(&account, &book(&server), &current)
        .await
        .expect("delete");

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].starts_with("PUT "));
    assert!(requests[0]
        .to_ascii_lowercase()
        .contains("if-none-match: *"));
    assert!(requests[0].contains("BEGIN:VCARD"));
    assert!(requests[1].starts_with("PUT "));
    assert!(requests[1].to_ascii_lowercase().contains("if-match:"));
    assert!(requests[2].starts_with("DELETE "));
    assert!(requests[2].to_ascii_lowercase().contains("if-match:"));
}

/// The mock itself enforces RFC 6352 §5.1 on address object data. This keeps
/// the derived-card regression honest: a UID-less PUT is answered with
/// `403 valid-address-data` instead of the canned `204`.
#[tokio::test]
async fn carddav_put_without_uid_is_rejected_by_the_mock_server() {
    let server = MockServer::start(vec![MockResponse {
        status: 204,
        reason: "No Content",
        body: String::new(),
    }])
    .await;
    let account = account(&server).await;
    let http = HttpClient::new(Arc::new(TestCreds)).expect("http");
    let client = DavClient::for_service(http, account, DavService::Contacts);
    let url = format!("{}/ab/no-uid.vcf", server.base);
    let body = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:No UID\r\nEND:VCARD\r\n";
    let error = client
        .put_resource(&url, body, "text/vcard; charset=utf-8", None, false)
        .await
        .expect_err("a UID-less vCard PUT must be rejected");
    assert!(
        error.to_string().contains("valid-address-data"),
        "the validation error must surface, got: {error}"
    );
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn mock_rejects_missing_or_empty_vcard_uid() {
    let known = KnownUids::new();
    let request = |body: &str| {
        format!(
            "PUT /ab/A.vcf HTTP/1.1\r\nContent-Type: text/vcard\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    };
    for body in [
        "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:No UID\r\nEND:VCARD\r\n",
        "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:\r\nFN:Empty UID\r\nEND:VCARD\r\n",
    ] {
        let rejected =
            vcard_put_rejection(&request(body), &known).expect("the mock must reject this PUT");
        assert_eq!(rejected.status, 403);
        assert!(rejected.body.contains("valid-address-data"));
    }
    let accepted = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:A.vcf\r\nFN:Has UID\r\nEND:VCARD\r\n";
    assert!(vcard_put_rejection(&request(accepted), &known).is_none());
}

/// The mock enforces RFC 6352 UID uniqueness per address book: the same UID
/// on another resource is `403 no-uid-conflict`, while an update of the
/// resource that already owns that UID is allowed.
#[test]
fn mock_rejects_duplicate_uid_on_another_resource() {
    let mut known = KnownUids::new();
    known.insert(
        "A.vcf".to_string(),
        "http://127.0.0.1:9/ab/other/A.vcf".to_string(),
    );
    let request = |target: &str, uid: &str| {
        format!(
            "PUT {target} HTTP/1.1\r\nContent-Type: text/vcard\r\n\r\nBEGIN:VCARD\r\nVERSION:3.0\r\nUID:{uid}\r\nFN:Card\r\nEND:VCARD\r\n"
        )
    };
    let rejected = vcard_put_rejection(&request("/ab/A.vcf", "A.vcf"), &known)
        .expect("a duplicate UID must be rejected");
    assert_eq!(rejected.status, 403);
    assert!(
        rejected.body.contains("no-uid-conflict"),
        "the server must name the uniqueness error, got: {}",
        rejected.body
    );
    assert!(
        vcard_put_rejection(&request("/ab/other/A.vcf", "A.vcf"), &known).is_none(),
        "updating the resource that owns the UID is not a conflict"
    );
    assert!(
        vcard_put_rejection(&request("/ab/A.vcf", "fresh-uid"), &known).is_none(),
        "a new UID is accepted"
    );
}

/// Seed an address book with a genuine `A.vcf` UID on another href and a
/// UID-less card whose local identity is derived from the `A.vcf` file name.
/// The server is assumed to already know the genuine UID (see
/// [`MockServer::start_with_uids`]).
async fn derived_identity_fixture(server: &MockServer) -> (CoreContext, Contact, Contact, String) {
    let dir = std::env::temp_dir().join(format!("nuncio-pim-carddav-{}", new_id()));
    let ctx = CoreContext::for_test(Paths::new(dir)).await.expect("ctx");
    ctx.migrate(
        nuncio_pim::store::PIM_NAMESPACE,
        nuncio_pim::store::PIM_MIGRATIONS,
    )
    .expect("pim migrations");
    let account_id = "dav-account";
    let mut account = ctx
        .db
        .read(repo::accounts::local)
        .await
        .expect("local account");
    account.id = account_id.into();
    account.provider = ProviderKind::Imap;
    account.auth_kind = AuthKind::Password;
    account.email = "me@example.com".into();
    account.oauth_provider = None;
    account.carddav_url = Some(format!("{}/ab/", server.base));
    account.contacts_enabled = true;
    ctx.db
        .write(move |conn| repo::accounts::insert(conn, &account))
        .await
        .expect("account");
    let book_id = "dav-book";
    let mut book = AddressBook {
        id: book_id.into(),
        account_id: account_id.into(),
        name: "Contacts".into(),
        is_read_only: false,
        is_default: true,
    };
    ctx.db
        .write(move |conn| repo::address_books::insert(conn, &mut book))
        .await
        .expect("book");
    let collection = format!("{}/ab/", server.base);
    ctx.db
        .write(move |conn| {
            nuncio_pim::store::set_book(
                conn,
                &AddressBookRemote::new(
                    book_id,
                    account_id,
                    "carddav",
                    collection.clone(),
                    Some(collection),
                ),
            )
        })
        .await
        .expect("book mapping");

    let derived_href = format!("{}/ab/A.vcf", server.base);
    let real_href = format!("{}/ab/other/A.vcf", server.base);
    let mut derived = Contact::new("uidless-local", book_id, account_id);
    derived.display_name = "No UID".into();
    derived.company = "Before".into();
    derived.emails = vec![ContactField {
        r#type: ContactFieldType::Work,
        value: "no-uid@example.com".into(),
        is_primary: true,
    }];
    let mut real = Contact::new("real-local", book_id, account_id);
    real.display_name = "Real UID".into();
    real.company = "Keep".into();
    ctx.db
        .write({
            let derived = derived.clone();
            let real = real.clone();
            let derived_href = derived_href.clone();
            let real_href = real_href.clone();
            move |conn| {
                repo::contacts::upsert_remote(
                    conn,
                    &derived,
                    Some("e-derived"),
                    Some("A.vcf"),
                    Some(&derived_href),
                    None,
                )?;
                // The imported card had no UID: its remote id is the href
                // placeholder, not a real provider UID.
                conn.execute(
                    "UPDATE contact SET uid_is_derived = 1 WHERE id = 'uidless-local'",
                    [],
                )?;
                repo::contacts::upsert_remote(
                    conn,
                    &real,
                    Some("e-real"),
                    Some("A.vcf"),
                    Some(&real_href),
                    None,
                )
            }
        })
        .await
        .expect("seed contacts");
    (ctx, derived, real, derived_href)
}

async fn queued_op(ctx: &CoreContext) -> nuncio_core::models::PendingOp {
    ctx.db
        .read(|conn| repo::pending_ops::list(conn, Some("dav-account")))
        .await
        .expect("ops")
        .into_iter()
        .next()
        .expect("queued save")
}

async fn claim(ctx: &CoreContext, op: &nuncio_core::models::PendingOp) {
    let op_id = op.id.clone();
    ctx.db
        .write(move |conn| repo::pending_ops::mark_in_progress(conn, &op_id))
        .await
        .expect("claim");
}

/// The UID persisted on the operation payload, if any.
async fn payload_assigned_uid(
    ctx: &CoreContext,
    op: &nuncio_core::models::PendingOp,
) -> Option<String> {
    let op_id = op.id.clone();
    ctx.db
        .read(move |conn| {
            conn.query_row(
                "SELECT json_extract(payload_json, '$.assigned_uid') FROM pending_op WHERE id = ?1",
                rusqlite::params![op_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(|error| AppError::db(format!("op payload: {error}")))
        })
        .await
        .expect("op payload")
}

/// Editing a card whose identity was derived from its href must assign a
/// **fresh** real UID (RFC 6352 requires UIDs to be unique per address book;
/// writing the file name back collides with a genuine `A.vcf`).
///
/// The PUT must succeed against a mock that enforces uniqueness, the local row
/// must be promoted to the real UID (`uid_is_derived = 0`), and the genuine
/// same-named card at another href must stay untouched.
#[tokio::test]
async fn editing_a_derived_identity_card_assigns_a_fresh_uid() {
    let server = MockServer::start_with_uids(
        vec![MockResponse {
            status: 204,
            reason: "No Content",
            body: String::new(),
        }],
        vec![("A.vcf".into(), "/ab/other/A.vcf".into())],
    )
    .await;
    let (ctx, derived, _real, derived_href) = derived_identity_fixture(&server).await;
    let http = HttpClient::new(Arc::new(TestCreds)).expect("http");
    let service = ContactsService::new(ctx.clone(), http);
    let mut edited = derived.clone();
    edited.company = "After".into();
    service.contact_update(edited).await.expect("update");
    let op = queued_op(&ctx).await;
    claim(&ctx, &op).await;
    service.execute(&op).await.expect("execute update");

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "one conditional PUT expected");
    assert!(requests[0].starts_with("PUT "));
    assert!(
        requests[0].to_ascii_lowercase().contains("if-match:"),
        "the edit must keep its ETag precondition"
    );
    let pushed_uid = request_uid(&requests[0]);
    assert_ne!(
        pushed_uid, "A.vcf",
        "the file name is not a usable UID: it belongs to another card"
    );
    assert_eq!(
        payload_assigned_uid(&ctx, &op).await.as_deref(),
        Some(pushed_uid.as_str()),
        "the assigned UID must be persisted on the operation before the PUT"
    );
    assert!(
        requests[0].contains("FN:No UID"),
        "the edited card is the one being written"
    );

    let (remote_id, href, uid_is_derived, company): (Option<String>, Option<String>, i64, String) =
        ctx.db
            .read(|conn| {
                conn.query_row(
                    "SELECT remote_id, href, uid_is_derived, company FROM contact
                 WHERE id = 'uidless-local'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .map_err(|error| AppError::db(format!("derived row: {error}")))
            })
            .await
            .expect("derived row");
    assert_eq!(
        remote_id.as_deref(),
        Some(pushed_uid.as_str()),
        "the local row must adopt the real UID"
    );
    assert_eq!(href.as_deref(), Some(derived_href.as_str()));
    assert_eq!(
        uid_is_derived, 0,
        "after a successful promote the row carries a real UID"
    );
    assert_eq!(company, "After");

    let (real_remote_id, stored_real_href, real_derived, real_name, real_company): (
        Option<String>,
        Option<String>,
        i64,
        String,
        String,
    ) = ctx
        .db
        .read(|conn| {
            conn.query_row(
                "SELECT remote_id, href, uid_is_derived, display_name, company FROM contact
                 WHERE id = 'real-local'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .map_err(|error| AppError::db(format!("real row: {error}")))
        })
        .await
        .expect("the real-UID card must still exist");
    assert_eq!(real_remote_id.as_deref(), Some("A.vcf"));
    let expected_real_href = format!("{}/ab/other/A.vcf", server.base);
    assert_eq!(
        stored_real_href.as_deref(),
        Some(expected_real_href.as_str())
    );
    assert_eq!(real_derived, 0);
    assert_eq!(real_name, "Real UID");
    assert_eq!(
        real_company, "Keep",
        "a same-named real UID at another href must not be merged or edited"
    );
    assert_eq!(
        ctx.db.read(repo::contacts::count).await.expect("count"),
        2,
        "the two identities must remain separate rows"
    );
}

/// A retry of the *same* operation must reuse the UID persisted on the
/// operation payload, even though the first attempt failed before the local
/// row was promoted.
#[tokio::test]
async fn retrying_a_derived_identity_edit_reuses_the_assigned_uid() {
    let server = MockServer::start_with_uids(
        vec![
            MockResponse {
                status: 500,
                reason: "Internal Server Error",
                body: "temporary failure".to_string(),
            },
            MockResponse {
                status: 204,
                reason: "No Content",
                body: String::new(),
            },
        ],
        vec![("A.vcf".into(), "/ab/other/A.vcf".into())],
    )
    .await;
    let (ctx, derived, _real, _derived_href) = derived_identity_fixture(&server).await;
    let http = HttpClient::new(Arc::new(TestCreds)).expect("http");
    let service = ContactsService::new(ctx.clone(), http);
    let mut edited = derived.clone();
    edited.company = "After".into();
    service.contact_update(edited).await.expect("update");
    let op = queued_op(&ctx).await;
    claim(&ctx, &op).await;

    service.execute(&op).await.expect_err("first attempt fails");
    let first_uid = request_uid(&server.requests()[0]);
    assert_ne!(first_uid, "A.vcf");
    let (stored_remote, still_derived): (Option<String>, i64) = ctx
        .db
        .read(|conn| {
            conn.query_row(
                "SELECT remote_id, uid_is_derived FROM contact WHERE id = 'uidless-local'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| AppError::db(format!("derived row: {error}")))
        })
        .await
        .expect("derived row");
    assert_eq!(stored_remote.as_deref(), Some("A.vcf"));
    assert_eq!(
        still_derived, 1,
        "a failed attempt must not promote the local row"
    );
    assert_eq!(
        payload_assigned_uid(&ctx, &op).await.as_deref(),
        Some(first_uid.as_str()),
        "the UID must be persisted before the first attempt"
    );

    service.execute(&op).await.expect("retry succeeds");
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "exactly one retry");
    assert_eq!(
        request_uid(&requests[1]),
        first_uid,
        "a retry must not generate a new UID"
    );

    let (remote_id, uid_is_derived): (Option<String>, i64) = ctx
        .db
        .read(|conn| {
            conn.query_row(
                "SELECT remote_id, uid_is_derived FROM contact WHERE id = 'uidless-local'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| AppError::db(format!("derived row: {error}")))
        })
        .await
        .expect("derived row");
    assert_eq!(remote_id.as_deref(), Some(first_uid.as_str()));
    assert_eq!(uid_is_derived, 0);
}

/// RFC 6578 §3.6: `507` on the collection's own response means the result set
/// was truncated and the returned sync-token acknowledges exactly this page.
/// The client must save it and fetch the next page; treating the `507` as a
/// failure kept the old token and re-requested the same page forever.
#[tokio::test]
async fn sync_collection_507_pages_until_complete() {
    let first = xml_escape(&vcard("card-1", "Ada Lovelace", "ada@example.com", ""));
    let second = xml_escape(&vcard("card-2", "Alan Turing", "alan@example.com", ""));
    let page_one = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/ab/</d:href>
    <d:status>HTTP/1.1 507 Insufficient Storage</d:status>
  </d:response>
  <d:response>
    <d:href>/ab/card-1.vcf</d:href>
    <d:propstat><d:prop><d:getetag>"e1"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:sync-token>token-2</d:sync-token>
</d:multistatus>"#;
    let multiget_one = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:response>
    <d:href>/ab/card-1.vcf</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e1"</d:getetag><card:address-data>{first}</card:address-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let page_two = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/ab/card-2.vcf</d:href>
    <d:propstat><d:prop><d:getetag>"e2"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:sync-token>token-3</d:sync-token>
</d:multistatus>"#;
    let multiget_two = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:response>
    <d:href>/ab/card-2.vcf</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e2"</d:getetag><card:address-data>{second}</card:address-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
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
    let account = account(&server).await;
    let http = HttpClient::new(Arc::new(TestCreds)).expect("http");
    let provider = CardDavProvider::new(http);
    let cursor = ContactCursor {
        local_address_book_id: "local-contacts".into(),
        sync_token: Some("token-1".into()),
        ctag: None,
        etags: Vec::new(),
    };
    let delta = provider
        .sync_contacts(&account, &book(&server), &cursor)
        .await
        .expect("sync");
    assert!(!delta.incomplete, "a truncated page is not a failed round");
    assert!(
        delta.deleted_hrefs.is_empty(),
        "a truncated page carries no deletions"
    );
    assert_eq!(delta.contacts.len(), 2, "both pages must be applied");
    assert_eq!(
        delta.sync_token.as_deref(),
        Some("token-3"),
        "the cursor must end on the last page's token"
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    assert!(requests[0].contains("sync-collection"));
    assert!(requests[1].contains("addressbook-multiget"));
    assert!(
        requests[2].contains("token-2"),
        "the second page must resume from the truncated page's token"
    );
    assert!(requests[3].contains("addressbook-multiget"));
}

/// A response with no href cannot be attributed. Resolving the empty href to
/// the collection URL made it look like the collection's own response, so the
/// ETag fallback believed in an empty address book and listed every cached
/// contact as deleted. The round must fail instead.
#[tokio::test]
async fn empty_href_response_is_not_the_collection_itself() {
    let listing = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href/>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;
    let server = MockServer::start(vec![MockResponse {
        status: 207,
        reason: "Multi-Status",
        body: listing.to_string(),
    }])
    .await;
    let account = account(&server).await;
    let http = HttpClient::new(Arc::new(TestCreds)).expect("http");
    let provider = CardDavProvider::new(http);
    let cursor = ContactCursor {
        local_address_book_id: "local-contacts".into(),
        sync_token: None,
        ctag: None,
        etags: vec![(format!("{}/ab/kept.vcf", server.base), "e-kept".into())],
    };
    let result = provider
        .sync_contacts(&account, &book(&server), &cursor)
        .await;
    assert!(
        result.is_err(),
        "an empty href must not stand in for the collection's own response"
    );
    assert_eq!(
        server.requests().len(),
        1,
        "a round without a verified collection identity must not proceed"
    );
}

/// A truncated `sync-collection` round is one ordered change stream. Page two
/// can report the same href as `404` after page one already returned its body.
/// Both pieces of evidence used to be emitted, and consumers delete before they
/// write, so the object was deleted and then re-imported from the stale body.
/// The newest page must win: no contact and a deletion for the same href.
#[tokio::test]
async fn later_page_404_erases_an_earlier_fetched_contact() {
    let first = xml_escape(&vcard("card-1", "Ada Lovelace", "ada@example.com", ""));
    let page_one = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/ab/</d:href>
    <d:status>HTTP/1.1 507 Insufficient Storage</d:status>
  </d:response>
  <d:response>
    <d:href>/ab/card-1.vcf</d:href>
    <d:propstat><d:prop><d:getetag>"e1"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:sync-token>token-2</d:sync-token>
</d:multistatus>"#;
    let multiget_one = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:response>
    <d:href>/ab/card-1.vcf</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e1"</d:getetag><card:address-data>{first}</card:address-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let page_two = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/ab/card-1.vcf</d:href>
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
    let account = account(&server).await;
    let http = HttpClient::new(Arc::new(TestCreds)).expect("http");
    let provider = CardDavProvider::new(http);
    let cursor = ContactCursor {
        local_address_book_id: "local-contacts".into(),
        sync_token: Some("token-1".into()),
        ctag: None,
        etags: Vec::new(),
    };
    let delta = provider
        .sync_contacts(&account, &book(&server), &cursor)
        .await
        .expect("sync");
    let href = format!("{}/ab/card-1.vcf", server.base);
    assert!(
        delta.contacts.is_empty(),
        "the later page's deletion must erase the earlier body"
    );
    assert_eq!(
        delta.deleted_hrefs,
        vec![href],
        "the object is deleted exactly once"
    );
    assert!(!delta.incomplete);
    assert_eq!(
        delta.sync_token.as_deref(),
        Some("token-3"),
        "the cursor reaches the last page"
    );
    assert_eq!(server.requests().len(), 3);
}

/// The member href must belong to the collection being synced, not merely to
/// an allow-listed origin. A response for `/other-book/card.vcf` used to be
/// treated as a member of `/ab/` and imported across address books.
#[tokio::test]
async fn href_from_another_address_book_is_not_a_member() {
    let listing = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/ab/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/ab/kept.vcf</d:href>
    <d:propstat><d:prop><d:getetag>"e-kept"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/other-book/card.vcf</d:href>
    <d:propstat><d:prop><d:getetag>"e-other"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;
    let server = MockServer::start(vec![MockResponse {
        status: 207,
        reason: "Multi-Status",
        body: listing.to_string(),
    }])
    .await;
    let account = account(&server).await;
    let http = HttpClient::new(Arc::new(TestCreds)).expect("http");
    let provider = CardDavProvider::new(http);
    let cursor = ContactCursor {
        local_address_book_id: "local-contacts".into(),
        sync_token: None,
        ctag: None,
        etags: vec![(format!("{}/ab/kept.vcf", server.base), "e-kept".into())],
    };
    let result = provider
        .sync_contacts(&account, &book(&server), &cursor)
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

/// `addressbook-multiget` responses are evidence only for the hrefs this batch
/// requested. A body for `/other-book/card.vcf` used to be imported even though
/// the request never named it.
#[tokio::test]
async fn multiget_response_for_an_unrequested_href_is_ignored() {
    let requested = xml_escape(&vcard("card-1", "Ada Lovelace", "ada@example.com", ""));
    let foreign = xml_escape(&vcard("card-2", "Other Book", "other@example.com", ""));
    let sync_body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/ab/card-1.vcf</d:href>
    <d:propstat><d:prop><d:getetag>"e1"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:sync-token>token-2</d:sync-token>
</d:multistatus>"#;
    let multiget = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:response>
    <d:href>/ab/card-1.vcf</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e1"</d:getetag><card:address-data>{requested}</card:address-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/other-book/card-2.vcf</d:href>
    <d:propstat>
      <d:prop><d:getetag>"e2"</d:getetag><card:address-data>{foreign}</card:address-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
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
    let account = account(&server).await;
    let http = HttpClient::new(Arc::new(TestCreds)).expect("http");
    let provider = CardDavProvider::new(http);
    let cursor = ContactCursor {
        local_address_book_id: "local-contacts".into(),
        sync_token: Some("token-1".into()),
        ctag: None,
        etags: Vec::new(),
    };
    let delta = provider
        .sync_contacts(&account, &book(&server), &cursor)
        .await
        .expect("sync");
    assert_eq!(
        delta.contacts.len(),
        1,
        "only the requested href is imported"
    );
    assert_eq!(delta.contacts[0].remote_id.as_deref(), Some("card-1"));
    assert!(
        !delta
            .contacts
            .iter()
            .any(|contact| contact.remote_id.as_deref() == Some("card-2")),
        "the unrequested response must not be imported"
    );
}

/// An absolute href outside the account's origins cannot be attributed either.
/// The lenient resolver fell back to the collection URL, which made a foreign
/// response look like the collection and deleted every cached contact.
#[tokio::test]
async fn cross_origin_href_response_is_not_the_collection_itself() {
    let listing = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>https://evil.example.net/ab/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;
    let server = MockServer::start(vec![MockResponse {
        status: 207,
        reason: "Multi-Status",
        body: listing.to_string(),
    }])
    .await;
    let account = account(&server).await;
    let http = HttpClient::new(Arc::new(TestCreds)).expect("http");
    let provider = CardDavProvider::new(http);
    let cursor = ContactCursor {
        local_address_book_id: "local-contacts".into(),
        sync_token: None,
        ctag: None,
        etags: vec![(format!("{}/ab/kept.vcf", server.base), "e-kept".into())],
    };
    let result = provider
        .sync_contacts(&account, &book(&server), &cursor)
        .await;
    assert!(
        result.is_err(),
        "an out-of-origin href must not stand in for the collection's own response"
    );
    assert_eq!(
        server.requests().len(),
        1,
        "a round without a verified collection identity must not proceed"
    );
}
