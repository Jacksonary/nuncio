#![allow(dead_code)]
//! Minimal POP3 server for UIDL/differential download tests.

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

pub struct MockPop3 {
    pub addr: std::net::SocketAddr,
    pub messages: Arc<Mutex<Vec<Pop3Message>>>,
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
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock POP3");
        let addr = listener.local_addr().expect("mock address");
        let messages = Arc::new(Mutex::new(Vec::new()));
        let cancel = CancellationToken::new();
        let state = messages.clone();
        let accept_cancel = cancel.clone();
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
            messages,
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
}

async fn handle(
    stream: tokio::net::TcpStream,
    messages: Arc<Mutex<Vec<Pop3Message>>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);
    write(&mut reader, "+OK mock POP3 ready\r\n").await?;
    let mut deleted = Vec::new();
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
                let count = messages.lock().expect("pop3 messages").len();
                write(&mut reader, &format!("+OK {count} messages\r\n")).await?;
            }
            "LIST" => {
                let lines: Vec<String> =
                    {
                        let messages = messages.lock().expect("pop3 messages");
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
                    let messages = messages.lock().expect("pop3 messages");
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
            "RETR" => {
                let index: usize = parts
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                let raw = {
                    let messages = messages.lock().expect("pop3 messages");
                    messages
                        .get(index.saturating_sub(1))
                        .map(|message| message.raw.clone())
                };
                let Some(raw) = raw else {
                    write(&mut reader, "-ERR no such message\r\n").await?;
                    continue;
                };
                write(&mut reader, &format!("+OK {} octets\r\n", raw.len())).await?;
                for line in raw.split_inclusive(|byte| *byte == b'\n') {
                    if line.starts_with(b".") {
                        write_bytes(&mut reader, b".").await?;
                    }
                    write_bytes(&mut reader, line).await?;
                    if !line.ends_with(b"\n") {
                        write_bytes(&mut reader, b"\r\n").await?;
                    }
                }
                write_bytes(&mut reader, b".\r\n").await?;
            }
            "DELE" => {
                if let Some(index) = parts.next().and_then(|value| value.parse::<usize>().ok()) {
                    deleted.push(index);
                }
                write(&mut reader, "+OK deleted\r\n").await?;
            }
            "NOOP" | "RSET" => write(&mut reader, "+OK\r\n").await?,
            "QUIT" => {
                if !deleted.is_empty() {
                    let mut messages = messages.lock().expect("pop3 messages");
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
