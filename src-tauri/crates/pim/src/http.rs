//! Shared HTTP plumbing for the Google, Graph and WebDAV providers.
//!
//! * the process-wide rustls **ring** provider is installed exactly once;
//! * requests are built by a closure so a 401 can be retried with a fresh
//!   access token;
//! * `429`/`503` responses honour `Retry-After` (seconds or HTTP date) with
//!   bounded exponential backoff;
//! * provider error bodies are truncated before they reach logs or the UI.

use std::sync::{Arc, Once};
use std::time::Duration;

use chrono::{DateTime, Utc};
use nuncio_core::traits::{CredentialProvider, TokenResource};
use reqwest::header::{HeaderValue, RETRY_AFTER};
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;
use tracing::{debug, warn};

use crate::error::{network, AppError, Result};

static INSTALL_CRYPTO: Once = Once::new();

/// User agent sent with every request.
pub const USER_AGENT: &str = concat!("Nuncio/", env!("CARGO_PKG_VERSION"));

/// Maximum number of rate-limit retries inside one logical request.
const MAX_RATE_LIMIT_RETRIES: u32 = 3;

/// Install the rustls ring provider once per process (idempotent).
pub fn install_crypto_provider() {
    INSTALL_CRYPTO.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Cloneable HTTP client that can attach provider credentials.
#[derive(Clone)]
pub struct HttpClient {
    client: reqwest::Client,
    creds: Arc<dyn CredentialProvider>,
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClient").finish_non_exhaustive()
    }
}

impl HttpClient {
    pub fn new(creds: Arc<dyn CredentialProvider>) -> Result<Self> {
        install_crypto_provider();
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .gzip(true)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .user_agent(USER_AGENT)
            .pool_max_idle_per_host(4)
            .build()
            .map_err(|error| network(format!("cannot create HTTP client: {error}")))?;
        Ok(Self { client, creds })
    }

    /// Raw reqwest client (webcal downloads, tests).
    pub fn raw(&self) -> &reqwest::Client {
        &self.client
    }

    pub fn creds(&self) -> &Arc<dyn CredentialProvider> {
        &self.creds
    }

    /// Send a request authenticated with an OAuth bearer token.
    ///
    /// `build` is invoked again after a 401 so a refreshed token can be used.
    pub async fn send_authed<F>(
        &self,
        account_id: &str,
        resource: TokenResource,
        build: F,
    ) -> Result<Response>
    where
        F: Fn(&str) -> RequestBuilder,
    {
        let mut token = self.creds.api_token(account_id, resource).await?;
        let mut response = build(&token)
            .send()
            .await
            .map_err(|error| request_error("provider request", error))?;

        if response.status() == StatusCode::UNAUTHORIZED {
            debug!(
                account_id,
                resource = resource.as_str(),
                "provider rejected token; retrying once"
            );
            self.creds.report_auth_failure(account_id, resource).await;
            token = self.creds.api_token(account_id, resource).await?;
            response = build(&token)
                .send()
                .await
                .map_err(|error| request_error("provider request after reauth", error))?;
        }

        let mut attempt = 0;
        while is_rate_limited(response.status()) && attempt < MAX_RATE_LIMIT_RETRIES {
            let delay = retry_delay(&response, attempt);
            debug!(
                account_id,
                status = response.status().as_u16(),
                delay_ms = delay.as_millis() as u64,
                "provider rate limited; backing off"
            );
            response = {
                drop(response);
                tokio::time::sleep(delay).await;
                build(&token)
                    .send()
                    .await
                    .map_err(|error| request_error("provider request after backoff", error))?
            };
            if response.status() == StatusCode::UNAUTHORIZED {
                self.creds.report_auth_failure(account_id, resource).await;
                token = self.creds.api_token(account_id, resource).await?;
                response = build(&token)
                    .send()
                    .await
                    .map_err(|error| request_error("provider request after reauth", error))?;
            }
            attempt += 1;
        }
        Ok(response)
    }

    /// Decode a successful JSON response; map error statuses to `AppError`.
    pub async fn json<T: DeserializeOwned>(response: Response) -> Result<T> {
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(status_error(status, &body));
        }
        response
            .json::<T>()
            .await
            .map_err(|error| AppError::protocol(format!("invalid provider JSON: {error}")))
    }

    /// Read a successful response body as text.
    pub async fn text(response: Response) -> Result<String> {
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| request_error("read response body", error))?;
        if !status.is_success() {
            return Err(status_error(status, &body));
        }
        Ok(body)
    }

    /// Ensure a response is successful, returning the body for diagnostics.
    pub async fn ensure_success(response: Response) -> Result<()> {
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        Err(status_error(status, &body))
    }
}

fn is_rate_limited(status: StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 503)
}

fn retry_delay(response: &Response, attempt: u32) -> Duration {
    if let Some(value) = response.headers().get(RETRY_AFTER) {
        if let Some(delay) = parse_retry_after(value) {
            return delay.min(Duration::from_secs(60));
        }
    }
    let seconds = 1u64 << attempt.min(4);
    Duration::from_secs(seconds.min(30))
}

fn parse_retry_after(value: &HeaderValue) -> Option<Duration> {
    let text = value.to_str().ok()?.trim();
    if let Ok(seconds) = text.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    if let Ok(date) = DateTime::parse_from_rfc2822(text) {
        let delta = date.with_timezone(&Utc) - Utc::now();
        if delta.num_seconds() > 0 {
            return Some(Duration::from_secs(delta.num_seconds() as u64));
        }
    }
    None
}

pub fn request_error(what: &str, error: reqwest::Error) -> AppError {
    if error.is_timeout() {
        return AppError::network(format!("{what} timed out"));
    }
    if error.is_connect() {
        return AppError::network(format!("{what} could not connect: {error}"));
    }
    AppError::network(format!("{what} failed: {error}"))
}

/// Map a non-2xx provider response to the closest `AppError` variant.
pub fn status_error(status: StatusCode, body: &str) -> AppError {
    let snippet = truncate(body, 400);
    match status.as_u16() {
        401 => AppError::auth(format!("provider authentication failed: {snippet}")),
        403 => AppError::auth(format!("provider denied access: {snippet}")),
        404 | 410 => AppError::not_found(format!("provider resource missing: {snippet}")),
        409 | 412 => AppError::conflict(format!("provider conflict: {snippet}")),
        429 => AppError::service(format!("provider rate limited: {snippet}")),
        400 | 422 => AppError::invalid_input(format!("provider rejected request: {snippet}")),
        405 | 501 => AppError::unsupported(format!(
            "provider does not support this operation: {snippet}"
        )),
        _ if status.is_server_error() => {
            AppError::service(format!("provider error {status}: {snippet}"))
        }
        _ => AppError::protocol(format!("provider returned {status}: {snippet}")),
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// Log a provider response failure without leaking an unbounded body.
pub fn log_status_error(context: &str, error: &AppError) {
    match error {
        AppError::Auth(message) => warn!(%context, %message, "provider authentication error"),
        _ => debug!(%context, %error, "provider request failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_seconds() {
        let value = HeaderValue::from_static("7");
        assert_eq!(parse_retry_after(&value), Some(Duration::from_secs(7)));
    }

    #[test]
    fn retry_after_http_date_in_the_past_is_ignored() {
        let value = HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT");
        assert_eq!(parse_retry_after(&value), None);
    }

    #[test]
    fn status_mapping() {
        assert!(matches!(
            status_error(StatusCode::TOO_MANY_REQUESTS, "slow down"),
            AppError::Service(_)
        ));
        assert!(matches!(
            status_error(StatusCode::SERVICE_UNAVAILABLE, "maintenance"),
            AppError::Service(_)
        ));
        assert!(matches!(
            status_error(StatusCode::BAD_REQUEST, "bad"),
            AppError::InvalidInput(_)
        ));
        assert!(matches!(
            status_error(StatusCode::UNAUTHORIZED, "nope"),
            AppError::Auth(_)
        ));
        assert!(matches!(
            status_error(StatusCode::METHOD_NOT_ALLOWED, "nope"),
            AppError::Unsupported(_)
        ));
        assert!(matches!(
            status_error(StatusCode::CONFLICT, "etag"),
            AppError::Conflict(_)
        ));
        assert!(matches!(
            status_error(StatusCode::BAD_GATEWAY, "bad"),
            AppError::Service(_)
        ));
    }

    #[test]
    fn bodies_are_truncated() {
        let long = "x".repeat(1000);
        let text = truncate(&long, 20);
        assert_eq!(text.chars().count(), 21);
    }
}
