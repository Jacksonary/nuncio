//! Low-level Microsoft Graph client for mail.
//!
//! Microsoft personal accounts (MSA) are regularly refused an IMAP session by
//! Exchange with "User is authenticated but not connected." even when the
//! token, audience and scopes are all correct.  Graph serves the same mailbox
//! over HTTPS for those accounts, so Microsoft accounts sync through this
//! module instead of IMAP (mirroring Wino Mail, which only uses IMAP for
//! iCloud/Yahoo/custom servers).
//!
//! This file deliberately contains no provider business logic: it is the
//! shared transport that folder listing, message sync and message operations
//! build on.  It mirrors the shape of `nuncio_pim::graph` (auth injection,
//! one-shot reauth on 401, error mapping) but cannot import it, because the
//! mail crate must not depend on the PIM crate.

pub mod folders;
pub mod messages;
pub mod ops;

use std::sync::OnceLock;
use std::time::Duration;

use nuncio_core::models::{Account, OAuthProvider};
use nuncio_core::traits::TokenResource;
use nuncio_core::{AppError, CoreContext, Result};
use reqwest::header::{HeaderValue, AUTHORIZATION};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use tracing::{debug, warn};

/// Graph v1.0 root, including the API version.
pub const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

/// Whether this account's mailbox must be synchronised through Microsoft Graph
/// instead of IMAP.
///
/// Exchange refuses IMAP AUTHENTICATE for personal Microsoft accounts (MSA)
/// with "User is authenticated but not connected." even when the token, the
/// audience and the scopes are all correct, while Graph serves the same
/// mailbox over HTTPS.  Every protocol branch in the mail crate calls this one
/// predicate, so the decision cannot drift between folder sync, message sync,
/// body fetch, the operation queue and sending.
///
/// `mail_enabled` is part of the condition on purpose: a Microsoft account
/// whose mail was disabled must keep the IMAP default (and, more importantly,
/// must not start talking to Graph).
pub fn uses_graph(account: &Account) -> bool {
    account.mail_enabled && account.oauth_provider == Some(OAuthProvider::Microsoft)
}

/// Every request is bounded: a hung Graph call must not block an account
/// actor indefinitely.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of response-body characters quoted in an error.  The IMAP
/// bug hid the real server message, so errors keep the body -- but a multi-MB
/// HTML error page must not end up in logs or the UI.
const BODY_SNIPPET_LEN: usize = 400;

/// Process-wide client so keep-alive and TLS sessions are reused across
/// accounts.  `GraphMailClient` itself stays a cheap value type (just the
/// base URL), which keeps it trivially cloneable and testable.
static CLIENT: OnceLock<Client> = OnceLock::new();

fn http() -> &'static Client {
    CLIENT.get_or_init(|| {
        crate::util::ensure_crypto_provider();
        Client::builder()
            .gzip(true)
            .connect_timeout(Duration::from_secs(15))
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("Nuncio/", env!("CARGO_PKG_VERSION")))
            .pool_max_idle_per_host(4)
            .build()
            // Building a client only fails when the TLS backend cannot be
            // initialised; falling back to the default client gives a
            // meaningful per-request network error instead of a panic inside
            // `new()`, which cannot return `Result`.
            .unwrap_or_else(|error| {
                warn!(%error, "cannot build the shared Graph HTTP client; using defaults");
                Client::new()
            })
    })
}

/// Low-level Microsoft Graph client for mail.
#[derive(Clone, Debug)]
pub struct GraphMailClient {
    base: String,
}

impl GraphMailClient {
    /// Create a client pointed at the public Graph v1.0 endpoint.
    pub fn new() -> Self {
        Self {
            base: GRAPH_BASE.to_string(),
        }
    }

    /// Join the configured base URL with a Graph-relative path.
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// Percent-encode a value for use as a single Graph path segment or as a
    /// query parameter (`$select`, OData filters, message ids).
    pub fn encode(value: &str) -> String {
        urlencoding::encode(value).into_owned()
    }

    /// GET with bearer auth and JSON decode.  Non-2xx becomes `AppError`.
    pub async fn get<T: serde::de::DeserializeOwned>(
        &self,
        ctx: &CoreContext,
        account_id: &str,
        url: &str,
    ) -> Result<T> {
        let response = self
            .send_authed(ctx, account_id, |http| http.get(url))
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Self::failure(status, &body));
        }
        response
            .json::<T>()
            .await
            .map_err(|error| AppError::protocol(format!("invalid Graph JSON: {error}")))
    }

    /// Authenticated request built by the caller; used for POST/PATCH/DELETE.
    ///
    /// The closure receives the shared [`reqwest::Client`] and returns a fully
    /// configured request, mirroring `nuncio_pim::graph`'s client:
    ///
    /// ```ignore
    /// let url = client.url(&format!("/me/messages/{}", GraphMailClient::encode(id)));
    /// let response = client
    ///     .send_authed(ctx, account_id, |http| http.patch(&url).json(&body))
    ///     .await?;
    /// ```
    ///
    /// A 401 is retried exactly once with a freshly read token; the rejected
    /// token is reported so the credential provider can refresh it or mark the
    /// account as needing re-authentication.
    pub async fn send_authed<F>(
        &self,
        ctx: &CoreContext,
        account_id: &str,
        build: F,
    ) -> Result<Response>
    where
        F: FnOnce(&Client) -> RequestBuilder,
    {
        let mut token = ctx
            .creds
            .api_token(account_id, TokenResource::MicrosoftGraph)
            .await?;
        // Build the request once: the closure is `FnOnce` and cannot be
        // replayed.  Bodies produced by `json`/`body` are byte buffers, so the
        // built request stays cloneable for the single retry.
        let request = build(http())
            .bearer_auth(&token)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| AppError::protocol(format!("cannot build Graph request: {error}")))?;
        let retry = request.try_clone();
        let mut response = http()
            .execute(request)
            .await
            .map_err(|error| request_error("Graph request", error))?;

        if response.status() == StatusCode::UNAUTHORIZED {
            match retry {
                Some(mut retry) => {
                    debug!(
                        account = account_id,
                        "Graph rejected the token; retrying once with a fresh token"
                    );
                    ctx.creds
                        .report_auth_failure(account_id, TokenResource::MicrosoftGraph)
                        .await;
                    token = ctx
                        .creds
                        .api_token(account_id, TokenResource::MicrosoftGraph)
                        .await?;
                    let header =
                        HeaderValue::from_str(&format!("Bearer {token}")).map_err(|error| {
                            AppError::auth(format!("invalid Graph access token: {error}"))
                        })?;
                    retry.headers_mut().insert(AUTHORIZATION, header);
                    response = http()
                        .execute(retry)
                        .await
                        .map_err(|error| request_error("Graph request after reauth", error))?;
                }
                None => {
                    // A streaming body cannot be replayed.  Still report the
                    // failure so the next call reads a refreshed token.
                    warn!(
                        account = account_id,
                        "Graph rejected the token and the request body is not replayable"
                    );
                    ctx.creds
                        .report_auth_failure(account_id, TokenResource::MicrosoftGraph)
                        .await;
                }
            }
        }

        Ok(response)
    }

    /// Map a non-success Graph response to the closest `AppError` variant.
    ///
    /// Kept as a pure function so classification is unit-testable without a
    /// network.  The body snippet is essential diagnostics: Graph explains
    /// permission and mailbox problems there, not in the status code.
    ///
    /// 401 and 403 are deliberately different: a 401 means the credential is
    /// bad (or expired), so it maps to [`AppError::Auth`] and the credential
    /// provider may refresh or mark the account as needing re-authentication.
    /// A 403 is a policy/permission decision on an otherwise valid token;
    /// refreshing the token cannot change it, so it maps to a terminal
    /// [`AppError::Unsupported`] instead.  Mapping it to `Auth` made the
    /// executor refresh the token on every retry until the attempt limit.
    pub fn failure(status: StatusCode, body: &str) -> AppError {
        let snippet = truncate(body, BODY_SNIPPET_LEN);
        match status.as_u16() {
            401 => AppError::auth(format!("Graph authentication failed: {snippet}")),
            403 => AppError::unsupported(format!("Graph denied access: {snippet}")),
            404 => AppError::not_found(format!("Graph resource missing: {snippet}")),
            429 => AppError::network(format!("Graph rate limited: {snippet}")),
            _ if status.is_server_error() => {
                AppError::network(format!("Graph error {status}: {snippet}"))
            }
            _ => AppError::protocol(format!("Graph returned {status}: {snippet}")),
        }
    }
}

impl Default for GraphMailClient {
    fn default() -> Self {
        Self::new()
    }
}

fn request_error(what: &str, error: reqwest::Error) -> AppError {
    if error.is_timeout() {
        return AppError::network(format!("{what} timed out"));
    }
    if error.is_connect() {
        return AppError::network(format!("{what} could not connect: {error}"));
    }
    AppError::network(format!("{what} failed: {error}"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::models::{AccountState, AuthKind, ProviderCapabilities, ProviderKind};

    fn account(provider: Option<OAuthProvider>, mail_enabled: bool) -> Account {
        Account {
            id: "acc".into(),
            provider: ProviderKind::Outlook,
            email: "user@example.com".into(),
            display_name: "User".into(),
            color: "#000000".into(),
            auth_kind: AuthKind::Oauth,
            oauth_provider: provider,
            imap: None,
            smtp: None,
            pop3: None,
            caldav_url: None,
            carddav_url: None,
            sent_copy: None,
            sync_window_days: 90,
            mail_enabled,
            calendar_enabled: false,
            contacts_enabled: false,
            tasks_enabled: false,
            notifications_enabled: false,
            include_in_unified: true,
            sort_order: 0,
            state: AccountState::Ok,
            state_message: None,
            capabilities: ProviderCapabilities::defaults_for(ProviderKind::Outlook),
            created_at: nuncio_core::models::now(),
        }
    }

    #[test]
    fn only_microsoft_accounts_with_mail_enabled_use_graph() {
        assert!(uses_graph(&account(Some(OAuthProvider::Microsoft), true)));
        // A disabled Microsoft account keeps the IMAP default.
        assert!(!uses_graph(&account(Some(OAuthProvider::Microsoft), false)));
        // Gmail OAuth keeps IMAP.
        assert!(!uses_graph(&account(Some(OAuthProvider::Google), true)));
        // Password accounts keep IMAP.
        assert!(!uses_graph(&account(None, true)));
    }

    #[test]
    fn url_joins_base_and_path() {
        let client = GraphMailClient::new();
        assert_eq!(
            client.url("/me/messages"),
            "https://graph.microsoft.com/v1.0/me/messages"
        );
        assert_eq!(
            client.url("/me/mailFolders/inbox/messages?$top=10"),
            "https://graph.microsoft.com/v1.0/me/mailFolders/inbox/messages?$top=10"
        );
    }

    #[test]
    fn encode_escapes_path_and_query_metacharacters() {
        assert_eq!(
            GraphMailClient::encode("AAMkAD/abc== +x"),
            "AAMkAD%2Fabc%3D%3D%20%2Bx"
        );
        assert_eq!(GraphMailClient::encode("$select"), "%24select");
        assert_eq!(GraphMailClient::encode("a#b%c"), "a%23b%25c");
        // Non-ASCII is UTF-8 percent-encoded, never left raw in the URL.
        assert_eq!(GraphMailClient::encode("邮件"), "%E9%82%AE%E4%BB%B6");
    }

    #[test]
    fn failure_classifies_status() {
        // 401 is a bad credential: refreshable, so it must stay Auth.
        assert!(matches!(
            GraphMailClient::failure(StatusCode::UNAUTHORIZED, "nope"),
            AppError::Auth(_)
        ));
        // 403 is a policy decision on a valid credential.  It must NOT be Auth,
        // or the executor would keep refreshing the token and retrying.
        let denied = GraphMailClient::failure(StatusCode::FORBIDDEN, "denied");
        assert!(matches!(denied, AppError::Unsupported(_)));
        assert_eq!(denied.code(), "unsupported");
        assert!(matches!(
            GraphMailClient::failure(StatusCode::NOT_FOUND, "gone"),
            AppError::NotFound(_)
        ));
        assert!(matches!(
            GraphMailClient::failure(StatusCode::TOO_MANY_REQUESTS, "slow down"),
            AppError::Network(_)
        ));
        assert!(matches!(
            GraphMailClient::failure(StatusCode::INTERNAL_SERVER_ERROR, "boom"),
            AppError::Network(_)
        ));
        assert!(matches!(
            GraphMailClient::failure(StatusCode::BAD_REQUEST, "bad"),
            AppError::Protocol(_)
        ));
    }

    #[test]
    fn failure_keeps_the_body_but_bounds_it() {
        let error = GraphMailClient::failure(StatusCode::FORBIDDEN, "  denied by policy  ");
        // The policy decision stays visible even though the variant changed.
        assert_eq!(error.message(), "Graph denied access: denied by policy");

        let long = "x".repeat(1000);
        let error = GraphMailClient::failure(StatusCode::BAD_REQUEST, &long);
        let message = error.message();
        // 400 body characters, then the appended ellipsis.
        assert!(message.ends_with('…'));
        assert_eq!(message.chars().filter(|c| *c == 'x').count(), 400);
    }
}
