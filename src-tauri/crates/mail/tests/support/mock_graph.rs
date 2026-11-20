//! Minimal loopback Microsoft Graph mock for `/me/mailFolders/{id}/messages`.
#![allow(dead_code)]
//!
//! Supports only what "load older mail" sends: `$filter=receivedDateTime le X`,
//! `$orderby=receivedDateTime desc`, `$top=N`, and an `@odata.nextLink` (a
//! `$skip` token) when more matches remain. Every request URL is recorded.

use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub struct MockGraph {
    pub base: String,
    pub requests: Arc<Mutex<Vec<String>>>,
}

impl MockGraph {
    /// `count` messages, ids `m1..mN`, `mN` newest, one minute apart.
    pub async fn start(count: usize) -> Self {
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
        let requests = Arc::new(Mutex::new(Vec::new()));
        let served = messages.clone();
        let log = requests.clone();
        let origin = base.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let served = served.clone();
                let log = log.clone();
                let origin = origin.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buffer).to_string();
                    let target = head
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    log.lock().expect("log").push(target.clone());
                    let body = respond(&served, &origin, &target);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self { base, requests }
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("log").clone()
    }
}

fn respond(messages: &Mutex<Vec<(String, String)>>, origin: &str, target: &str) -> String {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut filter_le: Option<String> = None;
    let mut top = 10usize;
    let mut skip = 0usize;
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
            "$skip" => skip = value.parse().unwrap_or(0),
            _ => {}
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
    let value: Vec<_> = page
        .iter()
        .map(|(id, at)| {
            json!({
                "id": id,
                "subject": format!("Subject {id}"),
                "bodyPreview": "preview",
                "receivedDateTime": at,
                "sentDateTime": at,
                "isRead": false,
                "isDraft": false,
                "hasAttachments": false,
                "from": {"emailAddress": {"name": "A", "address": "a@example.com"}},
                "toRecipients": [{"emailAddress": {"name": "B", "address": "b@example.com"}}],
                "internetMessageId": format!("<{id}@graph.test>"),
                "conversationId": format!("c-{id}"),
            })
        })
        .collect();
    let mut body = json!({ "value": value });
    if skip + top < matching.len() {
        body["@odata.nextLink"] = json!(format!(
            "{origin}{path}?%24skip={}&%24top={top}",
            skip + top
        ));
    }
    body.to_string()
}
