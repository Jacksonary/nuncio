//! Loopback OAuth callback server, browser opener and cancellation.
//!
//! The flow binds an ephemeral loopback port, opens the system browser and
//! waits (up to five minutes) for the provider to redirect back with
//! `?code=...&state=...`.  Every request is answered with a small bilingual
//! HTML page so the browser tab has a clear end state.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

use nuncio_core::error::AppError;
use nuncio_core::Result;

/// Total time the user has to finish signing in.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Opens a URL with the OS default browser.
///
/// Implemented by the app crate (which may use `tauri-plugin-opener`); a
/// process-based default is provided by [`SystemUrlOpener`].
pub trait UrlOpener: Send + Sync {
    fn open(&self, url: &str) -> Result<()>;
}

/// Default opener: `open` on macOS, `rundll32` on Windows, `xdg-open` elsewhere.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemUrlOpener;

impl UrlOpener for SystemUrlOpener {
    fn open(&self, url: &str) -> Result<()> {
        let mut command = browser_command(url);
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|error| AppError::io(format!("cannot open the system browser: {error}")))
    }
}

fn browser_command(url: &str) -> std::process::Command {
    #[cfg(target_os = "windows")]
    {
        let mut command = std::process::Command::new("rundll32");
        command.arg("url.dll,FileProtocolHandler").arg(url);
        command
    }
    #[cfg(target_os = "macos")]
    {
        let mut command = std::process::Command::new("open");
        command.arg(url);
        command
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        command
    }
}

/// Shared cancellation flag for an in-flight interactive login.
#[derive(Debug, Default)]
pub struct Cancellation {
    cancelled: AtomicBool,
    notify: Notify,
}

impl Cancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        // `notify_one` stores a permit when nobody is waiting, so a cancel that
        // races with the accept loop cannot be lost.
        self.notify.notify_one();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// A loopback listener bound to an ephemeral port.
pub struct LoopbackListener {
    listener4: TcpListener,
    /// Best-effort IPv6 listener on the same port (`localhost` may resolve to
    /// `::1` first in some browsers).
    listener6: Option<TcpListener>,
    port: u16,
}

impl LoopbackListener {
    pub async fn bind() -> Result<Self> {
        let listener4 = TcpListener::bind(std::net::SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .map_err(|error| {
                AppError::network(format!("cannot bind OAuth loopback port: {error}"))
            })?;
        let port = listener4
            .local_addr()
            .map_err(|error| AppError::network(format!("cannot read loopback port: {error}")))?
            .port();
        let listener6 = TcpListener::bind(std::net::SocketAddr::from((Ipv6Addr::LOCALHOST, port)))
            .await
            .ok();
        tracing::debug!(
            port,
            ipv6 = listener6.is_some(),
            "OAuth loopback listener ready"
        );
        Ok(Self {
            listener4,
            listener6,
            port,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `http://127.0.0.1:<port>` or `http://localhost:<port>`.
    pub fn redirect_uri(&self, provider: nuncio_core::models::OAuthProvider) -> String {
        use nuncio_core::models::OAuthProvider;
        let base = match provider {
            OAuthProvider::Google => "http://127.0.0.1",
            OAuthProvider::Microsoft => "http://localhost",
        };
        format!("{base}:{}", self.port)
    }
}

/// Parsed query parameters of the provider redirect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// Parse an HTTP callback request.  Returns `Ok(None)` for unrelated requests
/// (favicons, probes) which should be ignored while waiting.
pub fn parse_callback_request(request: &str) -> Result<Option<CallbackParams>> {
    let first_line = request.lines().next().unwrap_or_default().trim();
    if first_line.is_empty() {
        return Err(AppError::protocol("empty OAuth callback request"));
    }

    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts
        .next()
        .ok_or_else(|| AppError::protocol("malformed OAuth callback request line"))?;

    if !method.eq_ignore_ascii_case("GET") {
        return Ok(None);
    }

    let url = if target.starts_with("http://") || target.starts_with("https://") {
        url::Url::parse(target)
    } else if target.starts_with('/') {
        url::Url::parse(&format!("http://127.0.0.1{target}"))
    } else {
        return Ok(None);
    }
    .map_err(|error| AppError::protocol(format!("malformed OAuth callback URL: {error}")))?;

    if url.path() != "/" {
        return Ok(None);
    }

    let mut params = CallbackParams::default();
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => params.code = Some(value.into_owned()),
            "state" => params.state = Some(value.into_owned()),
            "error" => params.error = Some(value.into_owned()),
            "error_description" => params.error_description = Some(value.into_owned()),
            _ => {}
        }
    }
    Ok(Some(params))
}

/// Wait for a valid callback and answer the browser.
pub async fn wait_for_callback(
    listener: LoopbackListener,
    expected_state: &str,
    cancel: Arc<Cancellation>,
    timeout: Duration,
) -> Result<CallbackParams> {
    let expected_state = expected_state.to_string();
    let work = async move {
        loop {
            let notified = cancel.notify.notified();
            if cancel.is_cancelled() {
                return Err(AppError::Cancelled);
            }

            let accept = async {
                match &listener.listener6 {
                    Some(listener6) => tokio::select! {
                        result = listener.listener4.accept() => result.map(|(stream, _)| stream),
                        result = listener6.accept() => result.map(|(stream, _)| stream),
                    },
                    None => listener.listener4.accept().await.map(|(stream, _)| stream),
                }
            };

            let stream = tokio::select! {
                biased;
                _ = notified => return Err(AppError::Cancelled),
                result = accept => result
                    .map_err(|error| AppError::network(format!("OAuth callback accept failed: {error}")))?,
            };

            if let Some(params) = handle_connection(stream, &expected_state).await? {
                return Ok(params);
            }
        }
    };

    match tokio::time::timeout(timeout, work).await {
        Ok(result) => result,
        Err(_) => {
            tracing::info!("OAuth login timed out");
            Err(AppError::Cancelled)
        }
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    expected_state: &str,
) -> Result<Option<CallbackParams>> {
    let request = match read_request(&mut stream).await {
        Ok(request) => request,
        Err(error) => {
            let _ = write_page(
                &mut stream,
                400,
                "Bad Request",
                &failure_page(&error.message()),
            )
            .await;
            return Err(error);
        }
    };

    let params = match parse_callback_request(&request) {
        Ok(Some(params)) => params,
        Ok(None) => {
            let _ = write_page(
                &mut stream,
                404,
                "Not Found",
                "<!doctype html><meta charset=\"utf-8\"><title>Not Found</title><p>Not Found</p>",
            )
            .await;
            return Ok(None);
        }
        Err(error) => {
            let _ = write_page(
                &mut stream,
                400,
                "Bad Request",
                &failure_page(&error.message()),
            )
            .await;
            return Err(error);
        }
    };

    // Provider-side error (user cancelled, consent denied, ...).
    if let Some(error) = params.error.clone().filter(|value| !value.is_empty()) {
        let description = params.error_description.clone().unwrap_or_default();
        let message = if description.is_empty() {
            error.clone()
        } else {
            format!("{error}: {description}")
        };
        let _ = write_page(&mut stream, 200, "OK", &failure_page(&message)).await;
        if error.eq_ignore_ascii_case("access_denied") {
            // The user closed/cancelled the consent screen.
            return Err(AppError::Cancelled);
        }
        return Err(AppError::auth(format!(
            "authorization was not granted ({message})"
        )));
    }

    if params.state.as_deref() != Some(expected_state) {
        let _ = write_page(
            &mut stream,
            400,
            "Bad Request",
            &failure_page("state mismatch; please start the sign-in again"),
        )
        .await;
        return Err(AppError::auth(
            "OAuth state mismatch; the response did not come from the sign-in this app started",
        ));
    }

    if params
        .code
        .as_deref()
        .map(str::trim)
        .filter(|code| !code.is_empty())
        .is_none()
    {
        let _ = write_page(
            &mut stream,
            400,
            "Bad Request",
            &failure_page("the provider did not return an authorization code"),
        )
        .await;
        return Err(AppError::auth(
            "the authorization callback did not include a code",
        ));
    }

    let _ = write_page(&mut stream, 200, "OK", &success_page()).await;
    Ok(Some(params))
}

async fn read_request(stream: &mut TcpStream) -> Result<String> {
    const MAX_REQUEST: usize = 16 * 1024;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    loop {
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() > MAX_REQUEST {
            return Err(AppError::protocol("OAuth callback request is too large"));
        }
        let read = tokio::time::timeout_at(deadline, stream.read(&mut chunk))
            .await
            .map_err(|_| AppError::network("timed out reading the OAuth callback"))?
            .map_err(|error| {
                AppError::network(format!("cannot read the OAuth callback: {error}"))
            })?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }

    String::from_utf8(buffer)
        .map_err(|_| AppError::protocol("OAuth callback request is not valid UTF-8"))
}

async fn write_page(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &str,
) -> io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

/// Escape provider-controlled text before embedding it in the HTML page.
pub fn html_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn page(title: &str, heading: &str, message: &str, accent: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="referrer" content="no-referrer">
<title>{title}</title>
<style>
:root {{ color-scheme: light dark; }}
body {{ margin: 0; min-height: 100vh; display: flex; align-items: center; justify-content: center;
       font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", "Microsoft YaHei", sans-serif;
       background: #f3f3f3; color: #1b1b1b; }}
.card {{ background: #ffffff; border: 1px solid #e0e0e0; border-radius: 12px; padding: 36px 40px;
         max-width: 460px; box-shadow: 0 8px 30px rgba(0,0,0,.08); text-align: center; }}
.badge {{ width: 56px; height: 56px; margin: 0 auto 18px; border-radius: 50%; display: flex;
          align-items: center; justify-content: center; font-size: 28px; color: #fff; background: {accent}; }}
h1 {{ font-size: 20px; margin: 0 0 10px; }}
p {{ margin: 0; line-height: 1.55; color: #555; font-size: 14px; white-space: pre-wrap; word-break: break-word; }}
.en {{ margin-top: 8px; font-size: 13px; color: #777; }}
@media (prefers-color-scheme: dark) {{
  body {{ background: #1f1f1f; color: #f3f3f3; }}
  .card {{ background: #2b2b2b; border-color: #3a3a3a; box-shadow: none; }}
  p {{ color: #c8c8c8; }} .en {{ color: #9a9a9a; }}
}}
</style>
</head>
<body>
<div class="card">
<div class="badge">{badge}</div>
<h1>{heading}</h1>
<p>{message}</p>
</div>
</body>
</html>"#,
        title = html_escape(title),
        heading = html_escape(heading),
        message = html_escape(message),
        accent = accent,
        badge = if accent == "#107c10" { "✓" } else { "!" },
    )
}

/// Bilingual success page.
pub fn success_page() -> String {
    page(
        "登录成功 · Sign-in successful",
        "登录成功",
        "可以关闭此页面并返回 Nuncio。\nSign-in successful. You can close this page and return to Nuncio.",
        "#107c10",
    )
}

/// Bilingual failure page with an escaped reason.
pub fn failure_page(reason: &str) -> String {
    page(
        "登录失败 · Sign-in failed",
        "登录失败",
        &format!("{reason}\n\nSign-in failed. Close this page and return to Nuncio to try again."),
        "#c42b1c",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_code_and_state() {
        let request =
            "GET /?code=abc123&state=xyz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
        let params = parse_callback_request(request)
            .expect("parse")
            .expect("callback");
        assert_eq!(params.code.as_deref(), Some("abc123"));
        assert_eq!(params.state.as_deref(), Some("xyz"));
        assert!(params.error.is_none());
    }

    #[test]
    fn parses_provider_error() {
        let request =
            "GET /?error=access_denied&error_description=User%20cancelled&state=s HTTP/1.1\r\n\r\n";
        let params = parse_callback_request(request)
            .expect("parse")
            .expect("callback");
        assert_eq!(params.error.as_deref(), Some("access_denied"));
        assert_eq!(params.error_description.as_deref(), Some("User cancelled"));
    }

    #[test]
    fn ignores_non_get_and_other_paths() {
        assert!(parse_callback_request("POST /?code=x HTTP/1.1\r\n\r\n")
            .expect("parse")
            .is_none());
        assert!(parse_callback_request("GET /favicon.ico HTTP/1.1\r\n\r\n")
            .expect("parse")
            .is_none());
    }

    #[test]
    fn malformed_request_line_is_an_error() {
        assert!(parse_callback_request("").is_err());
        assert!(parse_callback_request("GET").is_err());
    }

    #[test]
    fn pages_are_bilingual_and_escape_reason() {
        assert!(success_page().contains("登录成功"));
        assert!(success_page().contains("Sign-in successful"));
        let failure = failure_page("<script>alert(1)</script>");
        assert!(failure.contains("登录失败"));
        assert!(!failure.contains("<script>"));
        assert!(failure.contains("&lt;script&gt;"));
    }

    #[test]
    fn html_escape_handles_quotes() {
        assert_eq!(
            html_escape("a&b<c>\"d\"'e'"),
            "a&amp;b&lt;c&gt;&quot;d&quot;&#39;e&#39;"
        );
    }

    #[tokio::test]
    async fn loopback_captures_callback_and_answers_browser() {
        let listener = LoopbackListener::bind().await.expect("bind");
        let port = listener.port();
        let cancel = Arc::new(Cancellation::new());
        let task = tokio::spawn(async move {
            wait_for_callback(listener, "state-123", cancel, Duration::from_secs(10)).await
        });

        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        stream
            .write_all(
                b"GET /?code=auth-code&state=state-123 HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("read response");

        let params = task.await.expect("join").expect("callback");
        assert_eq!(params.code.as_deref(), Some("auth-code"));
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("登录成功"));
    }

    #[tokio::test]
    async fn state_mismatch_is_rejected() {
        let listener = LoopbackListener::bind().await.expect("bind");
        let port = listener.port();
        let cancel = Arc::new(Cancellation::new());
        let task = tokio::spawn(async move {
            wait_for_callback(listener, "expected", cancel, Duration::from_secs(10)).await
        });

        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        stream
            .write_all(b"GET /?code=code&state=wrong HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("write");
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response).await;

        let error = task.await.expect("join").expect_err("must fail");
        assert!(error.message().contains("state mismatch"));
        assert!(response.contains("登录失败"));
    }

    #[tokio::test]
    async fn cancellation_aborts_waiting() {
        let listener = LoopbackListener::bind().await.expect("bind");
        let cancel = Arc::new(Cancellation::new());
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            wait_for_callback(listener, "state", cancel, Duration::from_secs(30)).await
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        task_cancel.cancel();
        let error = task.await.expect("join").expect_err("must be cancelled");
        assert!(error.is_cancelled());
    }

    #[tokio::test]
    async fn provider_access_denied_maps_to_cancelled() {
        let listener = LoopbackListener::bind().await.expect("bind");
        let port = listener.port();
        let cancel = Arc::new(Cancellation::new());
        let task = tokio::spawn(async move {
            wait_for_callback(listener, "st", cancel, Duration::from_secs(10)).await
        });

        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        stream
            .write_all(
                b"GET /?error=access_denied&error_description=User%20cancelled&state=st HTTP/1.1\r\nHost: x\r\n\r\n",
            )
            .await
            .expect("write");
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response).await;

        let error = task.await.expect("join").expect_err("must be cancelled");
        assert!(error.is_cancelled());
        assert!(response.contains("登录失败"));
    }

    #[tokio::test]
    async fn unrelated_request_is_ignored_then_real_callback_arrives() {
        let listener = LoopbackListener::bind().await.expect("bind");
        let port = listener.port();
        let cancel = Arc::new(Cancellation::new());
        let task = tokio::spawn(async move {
            wait_for_callback(listener, "st", cancel, Duration::from_secs(10)).await
        });

        let mut first = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        first
            .write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("write");
        let mut response = String::new();
        let _ = first.read_to_string(&mut response).await;
        assert!(response.starts_with("HTTP/1.1 404"));

        let mut second = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        second
            .write_all(b"GET /?code=c&state=st HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("write");
        let mut response = String::new();
        let _ = second.read_to_string(&mut response).await;
        assert!(response.contains("登录成功"));

        let params = task.await.expect("join").expect("callback");
        assert_eq!(params.code.as_deref(), Some("c"));
    }
}
