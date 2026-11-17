//! CardDAV contact provider tests against a tiny in-process HTTP/1.1 mock.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nuncio_core::models::{new_id, Account, AuthKind, Contact, ContactField, ContactFieldType};
use nuncio_core::providers::DavService;
use nuncio_core::traits::{CredentialProvider, MailAuth, TokenResource};
use nuncio_core::{AppError, CoreContext, Paths, Result};
use nuncio_pim::contacts::provider::{
    ContactCursor, ContactProvider, RemoteAddressBook, RemoteContactRef,
};
use nuncio_pim::contacts::CardDavProvider;
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
    };
    provider
        .update_contact(&account, &book(&server), &current, &contact, &[])
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
