//! Minimal SMTP sink for outbox tests.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub struct MockSmtp {
    pub addr: std::net::SocketAddr,
    pub messages: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Envelope recipients seen in `RCPT TO` commands, in order.
    #[allow(dead_code)]
    pub recipients: Arc<Mutex<Vec<String>>>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl Drop for MockSmtp {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

impl MockSmtp {
    pub async fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock SMTP");
        let addr = listener.local_addr().expect("mock address");
        let messages = Arc::new(Mutex::new(Vec::new()));
        let recipients = Arc::new(Mutex::new(Vec::new()));
        let cancel = CancellationToken::new();
        let state = messages.clone();
        let accepted = recipients.clone();
        let accept_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = accept_cancel.cancelled() => break,
                    accepted_stream = listener.accept() => {
                        let Ok((stream, _)) = accepted_stream else { break };
                        let messages = state.clone();
                        let recipients = accepted.clone();
                        tokio::spawn(async move {
                            let _ = handle(stream, messages, recipients).await;
                        });
                    }
                }
            }
        });
        Self {
            addr,
            messages,
            recipients,
            cancel,
            task,
        }
    }
}

async fn handle(
    stream: tokio::net::TcpStream,
    messages: Arc<Mutex<Vec<Vec<u8>>>>,
    recipients: Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);
    write(&mut reader, "220 mock SMTP ready\r\n").await?;
    let mut data_mode = false;
    let mut current = Vec::new();
    loop {
        if data_mode {
            let mut line = Vec::new();
            let read = reader.read_until(b'\n', &mut line).await?;
            if read == 0 {
                return Ok(());
            }
            if line == b".\r\n" || line == b".\n" {
                messages
                    .lock()
                    .expect("smtp messages")
                    .push(std::mem::take(&mut current));
                data_mode = false;
                write(&mut reader, "250 queued\r\n").await?;
                continue;
            }
            if line.starts_with(b"..") {
                line.remove(0);
            }
            current.extend_from_slice(&line);
            continue;
        }
        let mut line = String::new();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            return Ok(());
        }
        let upper = line.trim().to_ascii_uppercase();
        if upper.starts_with("EHLO") || upper.starts_with("HELO") {
            write(
                &mut reader,
                "250-mock\r\n250-AUTH PLAIN LOGIN\r\n250 SIZE 10485760\r\n",
            )
            .await?;
        } else if upper.starts_with("AUTH PLAIN") {
            if line.split_whitespace().count() >= 3 {
                write(&mut reader, "235 2.7.0 authenticated\r\n").await?;
            } else {
                write(&mut reader, "334 \r\n").await?;
                let mut challenge = String::new();
                reader.read_line(&mut challenge).await?;
                write(&mut reader, "235 2.7.0 authenticated\r\n").await?;
            }
        } else if upper.starts_with("AUTH LOGIN") {
            write(&mut reader, "334 VXNlcm5hbWU6\r\n").await?;
            let mut user = String::new();
            reader.read_line(&mut user).await?;
            write(&mut reader, "334 UGFzc3dvcmQ6\r\n").await?;
            let mut password = String::new();
            reader.read_line(&mut password).await?;
            write(&mut reader, "235 2.7.0 authenticated\r\n").await?;
        } else if upper.starts_with("MAIL FROM")
            || upper.starts_with("RCPT TO")
            || upper.starts_with("RSET")
        {
            if upper.starts_with("RCPT TO") {
                if let Some(address) = extract_path(&line) {
                    recipients.lock().expect("smtp recipients").push(address);
                }
            }
            write(&mut reader, "250 ok\r\n").await?;
        } else if upper.starts_with("DATA") {
            data_mode = true;
            write(&mut reader, "354 end with .\r\n").await?;
        } else if upper.starts_with("QUIT") {
            write(&mut reader, "221 bye\r\n").await?;
            return Ok(());
        } else {
            write(&mut reader, "250 ok\r\n").await?;
        }
    }
}

async fn write(reader: &mut BufReader<tokio::net::TcpStream>, value: &str) -> std::io::Result<()> {
    reader.get_mut().write_all(value.as_bytes()).await?;
    reader.get_mut().flush().await
}

/// Extract the address from `RCPT TO:<user@example.com>` (or bare/bracketed
/// parameter forms) and normalize it to lower case.
fn extract_path(line: &str) -> Option<String> {
    let rest = line.split_once(':')?.1.trim();
    let rest = rest.split_whitespace().next().unwrap_or_default();
    let rest = rest.trim().trim_start_matches('<').trim_end_matches('>');
    let address = rest
        .rsplit_once("mailto:")
        .map(|(_, value)| value)
        .unwrap_or(rest)
        .trim();
    if address.is_empty() {
        None
    } else {
        Some(address.to_ascii_lowercase())
    }
}
