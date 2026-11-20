#![allow(dead_code)]
//! Minimal POP3 server for UIDL/differential download tests.
//!
//! Supports `UIDL`, `LIST`, `TOP`, `RETR`, `DELE` and `QUIT`. `TOP` answers
//! `-ERR` when constructed with [`MockPop3::without_top`], which exercises the
//! `RETR` fallback of the header-first listing. Request counters let a test
//! assert that listing never downloads a body.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Pop3Message {
    pub uidl: String,
    pub raw: Vec<u8>,
}

#[derive(Default)]
pub struct Pop3Counters {
    pub top: AtomicUsize,
    pub retr: AtomicUsize,
    pub dele: AtomicUsize,
}

impl Pop3Counters {
    pub fn top(&self) -> usize {
        self.top.load(Ordering::SeqCst)
    }

    pub fn retr(&self) -> usize {
        self.retr.load(Ordering::SeqCst)
    }

    pub fn dele(&self) -> usize {
        self.dele.load(Ordering::SeqCst)
    }

    fn reset(&self) {
        self.top.store(0, Ordering::SeqCst);
        self.retr.store(0, Ordering::SeqCst);
        self.dele.store(0, Ordering::SeqCst);
    }
}

struct Shared {
    messages: Arc<Mutex<Vec<Pop3Message>>>,
    support_top: bool,
    counters: Arc<Pop3Counters>,
}

pub struct MockPop3 {
    pub addr: std::net::SocketAddr,
    pub messages: Arc<Mutex<Vec<Pop3Message>>>,
    pub counters: Arc<Pop3Counters>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl Drop for MockPop3 {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

impl MockPop3 {
    pub async fn start() -> Self {
        Self::spawn(true).await
    }

    /// A server that rejects `TOP`, forcing the `RETR` fallback.
    pub async fn without_top() -> Self {
        Self::spawn(false).await
    }

    async fn spawn(support_top: bool) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock POP3");
        let addr = listener.local_addr().expect("mock address");
        let shared = Arc::new(Shared {
            messages: Arc::new(Mutex::new(Vec::new())),
            support_top,
            counters: Arc::new(Pop3Counters::default()),
        });
        let cancel = CancellationToken::new();
        let accept_cancel = cancel.clone();
        let state = shared.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = accept_cancel.cancelled() => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let state = state.clone();
                        tokio::spawn(async move {
                            let _ = handle(stream, state).await;
                        });
                    }
                }
            }
        });
        Self {
            addr,
            messages: shared.messages.clone(),
            counters: shared.counters.clone(),
            cancel,
            task,
        }
    }

    pub fn add(&self, uidl: &str, raw: &[u8]) {
        self.messages
            .lock()
            .expect("pop3 messages")
            .push(Pop3Message {
                uidl: uidl.to_string(),
                raw: raw.to_vec(),
            });
    }

    pub fn remove(&self, uidl: &str) {
        self.messages
            .lock()
            .expect("pop3 messages")
            .retain(|message| message.uidl != uidl);
    }

    /// Clear the request counters between sessions.
    pub fn reset_counters(&self) {
        self.counters.reset();
    }
}

/// Header block of a raw RFC822 message, including the blank separator line.
fn split_headers(raw: &[u8]) -> (&[u8], &[u8]) {
    if let Some(index) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
        return (&raw[..index + 4], &raw[index + 4..]);
    }
    if let Some(index) = raw.windows(2).position(|window| window == b"\n\n") {
        return (&raw[..index + 2], &raw[index + 2..]);
    }
    (raw, &[])
}

async fn handle(stream: tokio::net::TcpStream, shared: Arc<Shared>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);
    write(&mut reader, "+OK mock POP3 ready\r\n").await?;
    let mut deleted: Vec<usize> = Vec::new();
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            return Ok(());
        }
        let trimmed = line.trim_end();
        let mut parts = trimmed.split_whitespace();
        let command = parts.next().unwrap_or_default().to_ascii_uppercase();
        match command.as_str() {
            "USER" | "PASS" | "APOP" => write(&mut reader, "+OK accepted\r\n").await?,
            "STAT" => {
                let count = shared.messages.lock().expect("pop3 messages").len();
                write(&mut reader, &format!("+OK {count} messages\r\n")).await?;
            }
            "LIST" => {
                let lines: Vec<String> =
                    {
                        let messages = shared.messages.lock().expect("pop3 messages");
                        let mut lines = vec![format!("+OK {} messages", messages.len())];
                        lines.extend(messages.iter().enumerate().map(|(index, message)| {
                            format!("{} {}", index + 1, message.raw.len())
                        }));
                        lines
                    };
                for line in lines {
                    write(&mut reader, &format!("{line}\r\n")).await?;
                }
                write(&mut reader, ".\r\n").await?;
            }
            "UIDL" => {
                let lines: Vec<String> = {
                    let messages = shared.messages.lock().expect("pop3 messages");
                    let mut lines = vec![format!("+OK {} messages", messages.len())];
                    lines.extend(
                        messages
                            .iter()
                            .enumerate()
                            .map(|(index, message)| format!("{} {}", index + 1, message.uidl)),
                    );
                    lines
                };
                for line in lines {
                    write(&mut reader, &format!("{line}\r\n")).await?;
                }
                write(&mut reader, ".\r\n").await?;
            }
            "TOP" => {
                if !shared.support_top {
                    write(&mut reader, "-ERR TOP not supported\r\n").await?;
                    continue;
                }
                shared.counters.top.fetch_add(1, Ordering::SeqCst);
                let index: usize = parts
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                let lines: u32 = parts
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                let raw = {
                    let messages = shared.messages.lock().expect("pop3 messages");
                    messages
                        .get(index.saturating_sub(1))
                        .map(|message| message.raw.clone())
                };
                let Some(raw) = raw else {
                    write(&mut reader, "-ERR no such message\r\n").await?;
                    continue;
                };
                let (headers, body) = split_headers(&raw);
                write(&mut reader, &format!("+OK {} octets\r\n", headers.len())).await?;
                write_lines(&mut reader, headers).await?;
                if lines > 0 {
                    let body_lines: Vec<&[u8]> = body
                        .split_inclusive(|byte| *byte == b'\n')
                        .take(lines as usize)
                        .collect();
                    for line in body_lines {
                        write_lines(&mut reader, line).await?;
                    }
                }
                write_bytes(&mut reader, b".\r\n").await?;
            }
            "RETR" => {
                shared.counters.retr.fetch_add(1, Ordering::SeqCst);
                let index: usize = parts
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                let raw = {
                    let messages = shared.messages.lock().expect("pop3 messages");
                    messages
                        .get(index.saturating_sub(1))
                        .map(|message| message.raw.clone())
                };
                let Some(raw) = raw else {
                    write(&mut reader, "-ERR no such message\r\n").await?;
                    continue;
                };
                write(&mut reader, &format!("+OK {} octets\r\n", raw.len())).await?;
                write_lines(&mut reader, &raw).await?;
                write_bytes(&mut reader, b".\r\n").await?;
            }
            "DELE" => {
                shared.counters.dele.fetch_add(1, Ordering::SeqCst);
                if let Some(index) = parts.next().and_then(|value| value.parse::<usize>().ok()) {
                    deleted.push(index);
                }
                write(&mut reader, "+OK deleted\r\n").await?;
            }
            "NOOP" | "RSET" => write(&mut reader, "+OK\r\n").await?,
            "QUIT" => {
                if !deleted.is_empty() {
                    let mut messages = shared.messages.lock().expect("pop3 messages");
                    deleted.sort_unstable();
                    for index in deleted.into_iter().rev() {
                        if index > 0 && index <= messages.len() {
                            messages.remove(index - 1);
                        }
                    }
                }
                write(&mut reader, "+OK bye\r\n").await?;
                return Ok(());
            }
            _ => write(&mut reader, "-ERR unsupported\r\n").await?,
        }
    }
}

/// Write a block line by line with POP3 dot-stuffing.
async fn write_lines(
    reader: &mut BufReader<tokio::net::TcpStream>,
    raw: &[u8],
) -> std::io::Result<()> {
    for line in raw.split_inclusive(|byte| *byte == b'\n') {
        if line.starts_with(b".") {
            write_bytes(reader, b".").await?;
        }
        write_bytes(reader, line).await?;
        if !line.ends_with(b"\n") {
            write_bytes(reader, b"\r\n").await?;
        }
    }
    Ok(())
}

async fn write(reader: &mut BufReader<tokio::net::TcpStream>, value: &str) -> std::io::Result<()> {
    write_bytes(reader, value.as_bytes()).await
}

async fn write_bytes(
    reader: &mut BufReader<tokio::net::TcpStream>,
    value: &[u8],
) -> std::io::Result<()> {
    reader.get_mut().write_all(value).await?;
    reader.get_mut().flush().await
}
