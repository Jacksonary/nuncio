//! Minimal POP3 client (RFC 1939) with optional STLS and implicit TLS.
//!
//! POP3 has no flags, labels or server search, so messages are downloaded into
//! a local `INBOX` folder and remain there. `UIDL` provides the incremental
//! key; downloaded messages are kept on the server by default.

pub mod sync;

use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use nuncio_core::models::{Security, ServerConfig};
use nuncio_core::traits::MailAuth;
use nuncio_core::{AppError, Result};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;
use tracing::debug;

use crate::util::ensure_crypto_provider;

const TIMEOUT: Duration = Duration::from_secs(60);

pub enum Pop3Transport {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl fmt::Debug for Pop3Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plain(_) => f.write_str("Pop3Transport::Plain"),
            Self::Tls(_) => f.write_str("Pop3Transport::Tls"),
        }
    }
}

impl AsyncRead for Pop3Transport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Pop3Transport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }
}

pub struct Pop3Client {
    reader: Option<BufReader<Pop3Transport>>,
}

impl Pop3Client {
    /// Connect, optionally upgrade with STLS, and authenticate.
    pub async fn connect(config: &ServerConfig, auth: &MailAuth) -> Result<Self> {
        if config.security == Security::None && !crate::util::is_loopback_host(&config.host) {
            return Err(AppError::invalid_input(
                "POP3 without TLS is only allowed for a local (loopback) relay; \
                 choose TLS or STARTTLS for this server",
            ));
        }
        ensure_crypto_provider();
        let tcp = tokio::time::timeout(
            TIMEOUT,
            TcpStream::connect((config.host.as_str(), config.port)),
        )
        .await
        .map_err(|_| AppError::network("POP3 connect timed out"))?
        .map_err(|error| AppError::network(format!("POP3 connect failed: {error}")))?;
        let _ = tcp.set_nodelay(true);

        let mut client = if config.security == Security::Tls {
            let transport = tls_wrap(tcp, &config.host).await?;
            let mut client = Pop3Client {
                reader: Some(BufReader::new(transport)),
            };
            client.read_ok("greeting").await?;
            client
        } else {
            let mut client = Pop3Client {
                reader: Some(BufReader::new(Pop3Transport::Plain(tcp))),
            };
            client.read_ok("greeting").await?;
            if config.security == Security::Starttls {
                client.command_ok("STLS").await?;
                let tcp = client.take_transport()?;
                let transport = tls_wrap(tcp, &config.host).await?;
                client.reader = Some(BufReader::new(transport));
            }
            client
        };

        match auth {
            MailAuth::Password { username, password } => {
                client.command_ok(&format!("USER {username}")).await?;
                client.command_ok(&format!("PASS {password}")).await?;
            }
            MailAuth::XOAuth2 { .. } => {
                return Err(AppError::unsupported(
                    "POP3 OAuth (XOAUTH2) is not supported; use an app password",
                ));
            }
        }
        debug!(host = %config.host, "POP3 session ready");
        Ok(client)
    }

    fn io(&mut self) -> Result<&mut BufReader<Pop3Transport>> {
        self.reader
            .as_mut()
            .ok_or_else(|| AppError::internal("POP3 connection is being upgraded"))
    }

    /// Take the plain socket out during a STARTTLS upgrade. A new reader is
    /// installed immediately after the TLS handshake.
    fn take_transport(&mut self) -> Result<TcpStream> {
        let reader = self
            .reader
            .take()
            .ok_or_else(|| AppError::internal("POP3 transport already taken"))?;
        match reader.into_inner() {
            Pop3Transport::Plain(tcp) => Ok(tcp),
            Pop3Transport::Tls(_) => Err(AppError::internal("POP3 transport already uses TLS")),
        }
    }

    async fn command(&mut self, command: &str) -> Result<String> {
        let line = format!("{command}\r\n");
        self.io()?
            .get_mut()
            .write_all(line.as_bytes())
            .await
            .map_err(|error| AppError::network(format!("POP3 write failed: {error}")))?;
        self.io()?
            .get_mut()
            .flush()
            .await
            .map_err(|error| AppError::network(format!("POP3 flush failed: {error}")))?;
        let mut response = String::new();
        tokio::time::timeout(TIMEOUT, self.io()?.read_line(&mut response))
            .await
            .map_err(|_| AppError::network("POP3 response timed out"))?
            .map_err(|error| AppError::network(format!("POP3 read failed: {error}")))?;
        if response.is_empty() {
            return Err(AppError::network("POP3 server closed the connection"));
        }
        let trimmed = response.trim_end().to_string();
        if trimmed.starts_with("-ERR") {
            let lower = trimmed.to_ascii_lowercase();
            if lower.contains("auth") || lower.contains("password") || lower.contains("login") {
                return Err(AppError::auth(format!(
                    "POP3 authentication failed: {trimmed}"
                )));
            }
            return Err(AppError::protocol(format!("POP3 error: {trimmed}")));
        }
        if !trimmed.starts_with("+OK") {
            return Err(AppError::protocol(format!(
                "unexpected POP3 response: {trimmed}"
            )));
        }
        Ok(trimmed)
    }

    async fn command_ok(&mut self, command: &str) -> Result<()> {
        self.command(command).await.map(|_| ())
    }

    async fn read_ok(&mut self, what: &str) -> Result<()> {
        let mut response = String::new();
        tokio::time::timeout(TIMEOUT, self.io()?.read_line(&mut response))
            .await
            .map_err(|_| AppError::network(format!("POP3 {what} timed out")))?
            .map_err(|error| AppError::network(format!("POP3 read failed: {error}")))?;
        if response.starts_with("+OK") {
            Ok(())
        } else {
            Err(AppError::protocol(format!(
                "POP3 server rejected the connection: {}",
                response.trim_end()
            )))
        }
    }

    async fn read_multiline(&mut self) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        loop {
            let mut line = Vec::new();
            let read = tokio::time::timeout(TIMEOUT, self.io()?.read_until(b'\n', &mut line))
                .await
                .map_err(|_| AppError::network("POP3 multiline read timed out"))?
                .map_err(|error| AppError::network(format!("POP3 read failed: {error}")))?;
            if read == 0 {
                return Err(AppError::network("POP3 connection closed mid-response"));
            }
            if line == b".\r\n" || line == b".\n" {
                break;
            }
            // Byte-stuffing: a line starting with ".." loses one dot.
            if line.starts_with(b"..") {
                line.remove(0);
            }
            if line.ends_with(b"\r\n") {
                line.truncate(line.len() - 2);
                line.extend_from_slice(b"\r\n");
            }
            data.extend_from_slice(&line);
        }
        Ok(data)
    }

    /// `UIDL` -> (message number, unique id).
    pub async fn uidl(&mut self) -> Result<Vec<(u32, String)>> {
        self.command_ok("UIDL").await?;
        let data = self.read_multiline().await?;
        let text = String::from_utf8_lossy(&data);
        let mut entries = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let mut parts = line.splitn(2, char::is_whitespace);
            let Some(number) = parts.next() else { continue };
            let Some(uidl) = parts.next() else { continue };
            if let Ok(number) = number.parse::<u32>() {
                entries.push((number, uidl.trim().to_string()));
            }
        }
        Ok(entries)
    }

    /// `LIST` -> (message number, size in bytes).
    pub async fn list(&mut self) -> Result<Vec<(u32, u64)>> {
        self.command_ok("LIST").await?;
        let data = self.read_multiline().await?;
        let text = String::from_utf8_lossy(&data);
        let mut entries = Vec::new();
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            if let (Some(number), Some(size)) = (parts.next(), parts.next()) {
                if let (Ok(number), Ok(size)) = (number.parse::<u32>(), size.parse::<u64>()) {
                    entries.push((number, size));
                }
            }
        }
        Ok(entries)
    }

    /// `RETR` a message; CRLF normalized and dot-unstuffed.
    pub async fn retr(&mut self, number: u32) -> Result<Vec<u8>> {
        self.command_ok(&format!("RETR {number}")).await?;
        self.read_multiline().await
    }

    pub async fn dele(&mut self, number: u32) -> Result<()> {
        self.command_ok(&format!("DELE {number}")).await
    }

    pub async fn noop(&mut self) -> Result<()> {
        self.command_ok("NOOP").await
    }

    pub async fn quit(mut self) {
        let _ = tokio::time::timeout(TIMEOUT, self.command("QUIT")).await;
    }
}

async fn tls_wrap(tcp: TcpStream, host: &str) -> Result<Pop3Transport> {
    use rustls_platform_verifier::BuilderVerifierExt;
    let config = match rustls::ClientConfig::builder().with_platform_verifier() {
        Ok(builder) => builder.with_no_client_auth(),
        Err(error) => {
            debug!(%error, "platform verifier unavailable for POP3; using webpki roots");
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth()
        }
    };
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| AppError::invalid_input(format!("invalid POP3 host: {host}")))?;
    let connector = TlsConnector::from(Arc::new(config));
    let stream = tokio::time::timeout(TIMEOUT, connector.connect(server_name, tcp))
        .await
        .map_err(|_| AppError::network("POP3 TLS handshake timed out"))?
        .map_err(|error| AppError::network(format!("POP3 TLS failed: {error}")))?;
    Ok(Pop3Transport::Tls(Box::new(stream)))
}
