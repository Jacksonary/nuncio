//! OAuth2 provider configuration, scopes and authorization-URL building.
//!
//! Only the authorization-code + PKCE flow is implemented (public desktop
//! clients).  Google is treated as a confidential-ish client because its
//! installed-app flow expects the client secret at the token endpoint; Microsoft
//! is a public client and never gets a secret.
//!
//! Client ids come from build-time environment variables or the (non-secret)
//! advanced settings.  The Google client **secret** is only read from the
//! [`crate::secrets::SecretStore`] by [`crate::AuthService`] — never from the
//! settings table (architecture §7).

pub mod flow;
pub mod pkce;
pub mod token;

use nuncio_core::error::AppError;
use nuncio_core::models::{AppSettings, OAuthProvider};
use nuncio_core::Result;

use crate::oauth::pkce::Pkce;

/// Google authorization endpoint.
pub const GOOGLE_AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
/// Google token endpoint.
pub const GOOGLE_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
/// Google OpenID userinfo endpoint.
pub const GOOGLE_USERINFO_ENDPOINT: &str = "https://openidconnect.googleapis.com/v1/userinfo";

/// Microsoft (multi-tenant + personal accounts) authorization endpoint.
pub const MICROSOFT_AUTH_ENDPOINT: &str =
    "https://login.microsoftonline.com/common/oauth2/v2.0/authorize";
/// Microsoft token endpoint.
pub const MICROSOFT_TOKEN_ENDPOINT: &str =
    "https://login.microsoftonline.com/common/oauth2/v2.0/token";

/// Scopes requested from Google: identity, full mail (IMAP/SMTP), calendar,
/// contacts, other contacts and tasks.
pub const GOOGLE_SCOPES: &[&str] = &[
    "openid",
    "email",
    "profile",
    "https://mail.google.com/",
    "https://www.googleapis.com/auth/calendar",
    "https://www.googleapis.com/auth/contacts",
    "https://www.googleapis.com/auth/contacts.other.readonly",
    "https://www.googleapis.com/auth/tasks",
];

/// Scopes for the Microsoft *mail* resource (Outlook IMAP + SMTP).
pub const MICROSOFT_MAIL_SCOPES: &[&str] = &[
    "openid",
    "profile",
    "email",
    "offline_access",
    "https://outlook.office.com/IMAP.AccessAsUser.All",
    "https://outlook.office.com/SMTP.Send",
];

/// Scopes for the Microsoft *Graph* resource (mail, calendar, contacts, tasks).
///
/// The mail scopes are here as well as on the IMAP resource: some accounts are
/// refused an IMAP session by Exchange (`User is authenticated but not
/// connected`) even though every prerequisite checks out, and Graph is the only
/// other way to reach the mailbox. They are requested so a token that carries
/// them exists at all — OAuth only ever issues scopes that were asked for.
pub const MICROSOFT_GRAPH_SCOPES: &[&str] = &[
    "openid",
    "offline_access",
    "https://graph.microsoft.com/Mail.ReadWrite",
    "https://graph.microsoft.com/Mail.Send",
    "https://graph.microsoft.com/Calendars.ReadWrite",
    "https://graph.microsoft.com/Contacts.ReadWrite",
    "https://graph.microsoft.com/Tasks.ReadWrite",
    "https://graph.microsoft.com/User.Read",
    "https://graph.microsoft.com/MailboxSettings.Read",
];

/// All scopes requested once, at authorization time.  Entra lets the authorize
/// request span resources; the token endpoint does not, which is why the code
/// is redeemed for mail scopes first and then the refresh token is used for
/// Graph scopes (see `AuthService`).
pub const MICROSOFT_AUTHORIZE_SCOPES: &[&str] = &[
    "openid",
    "profile",
    "email",
    "offline_access",
    "https://outlook.office.com/IMAP.AccessAsUser.All",
    "https://outlook.office.com/SMTP.Send",
    "https://graph.microsoft.com/Mail.ReadWrite",
    "https://graph.microsoft.com/Mail.Send",
    "https://graph.microsoft.com/Calendars.ReadWrite",
    "https://graph.microsoft.com/Contacts.ReadWrite",
    "https://graph.microsoft.com/Tasks.ReadWrite",
    "https://graph.microsoft.com/User.Read",
    "https://graph.microsoft.com/MailboxSettings.Read",
];

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// OAuth client identifiers.
///
/// Values normally come from build-time environment variables
/// (`NUNCIO_GOOGLE_CLIENT_ID`, `NUNCIO_GOOGLE_CLIENT_SECRET`,
/// `NUNCIO_MS_CLIENT_ID`); non-empty values in
/// `settings.advanced` override them at runtime.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct OAuthConfig {
    pub google_client_id: Option<String>,
    pub google_client_secret: Option<String>,
    pub microsoft_client_id: Option<String>,
}

impl std::fmt::Debug for OAuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthConfig")
            .field("google_client_id", &self.google_client_id)
            .field(
                "google_client_secret",
                &self.google_client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("microsoft_client_id", &self.microsoft_client_id)
            .finish()
    }
}

impl OAuthConfig {
    pub fn new(
        google_client_id: Option<String>,
        google_client_secret: Option<String>,
        microsoft_client_id: Option<String>,
    ) -> Self {
        Self {
            google_client_id: non_empty(google_client_id.as_deref()),
            google_client_secret: non_empty(google_client_secret.as_deref()),
            microsoft_client_id: non_empty(microsoft_client_id.as_deref()),
        }
    }

    /// Read build-time values.  `option_env!` covers build scripts that inject
    /// them into this crate; `std::env::var` covers a runtime-provided value.
    pub fn from_env() -> Self {
        let google_client_id = std::env::var("NUNCIO_GOOGLE_CLIENT_ID")
            .ok()
            .or_else(|| option_env!("NUNCIO_GOOGLE_CLIENT_ID").map(str::to_string));
        let google_client_secret = std::env::var("NUNCIO_GOOGLE_CLIENT_SECRET")
            .ok()
            .or_else(|| option_env!("NUNCIO_GOOGLE_CLIENT_SECRET").map(str::to_string));
        let microsoft_client_id = std::env::var("NUNCIO_MS_CLIENT_ID")
            .ok()
            .or_else(|| option_env!("NUNCIO_MS_CLIENT_ID").map(str::to_string));
        Self::new(google_client_id, google_client_secret, microsoft_client_id)
    }

    /// Return a copy where non-empty `settings.advanced` client ids win.
    ///
    /// The Google client *secret* is deliberately ignored here: it is a secret
    /// and therefore only ever read from the [`SecretStore`] by `AuthService`
    /// (architecture §7).  Any legacy plaintext copy in the settings table is
    /// migrated into the secret store and scrubbed by `AuthService`.
    pub fn with_settings(mut self, settings: &AppSettings) -> Self {
        let advanced = &settings.advanced;
        if let Some(value) = non_empty(advanced.google_client_id.as_deref()) {
            self.google_client_id = Some(value);
        }
        if let Some(value) = non_empty(advanced.microsoft_client_id.as_deref()) {
            self.microsoft_client_id = Some(value);
        }
        self
    }

    /// Resolve the effective client for a provider or fail with a user-actionable
    /// message.
    pub fn resolved(&self, provider: OAuthProvider) -> Result<ResolvedClient> {
        match provider {
            OAuthProvider::Google => {
                let client_id = self.google_client_id.clone().ok_or_else(|| {
                    AppError::auth(
                        "Google OAuth is not configured: set the client ID in Settings > Advanced or build with NUNCIO_GOOGLE_CLIENT_ID",
                    )
                })?;
                Ok(ResolvedClient {
                    provider,
                    client_id,
                    client_secret: self.google_client_secret.clone(),
                })
            }
            OAuthProvider::Microsoft => {
                let client_id = self.microsoft_client_id.clone().ok_or_else(|| {
                    AppError::auth(
                        "Microsoft OAuth is not configured: set the client ID in Settings > Advanced or build with NUNCIO_MS_CLIENT_ID",
                    )
                })?;
                Ok(ResolvedClient {
                    provider,
                    client_id,
                    client_secret: None,
                })
            }
        }
    }
}

/// Provider-specific values needed to start a login or refresh a token.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedClient {
    pub provider: OAuthProvider,
    pub client_id: String,
    /// Google only; Microsoft is a public client.
    pub client_secret: Option<String>,
}

impl std::fmt::Debug for ResolvedClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedClient")
            .field("provider", &self.provider)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl ResolvedClient {
    pub fn auth_endpoint(&self) -> &'static str {
        match self.provider {
            OAuthProvider::Google => GOOGLE_AUTH_ENDPOINT,
            OAuthProvider::Microsoft => MICROSOFT_AUTH_ENDPOINT,
        }
    }

    pub fn token_endpoint(&self) -> &'static str {
        match self.provider {
            OAuthProvider::Google => GOOGLE_TOKEN_ENDPOINT,
            OAuthProvider::Microsoft => MICROSOFT_TOKEN_ENDPOINT,
        }
    }

    /// Scopes sent to the authorization endpoint.
    pub fn authorize_scopes(&self) -> &'static [&'static str] {
        match self.provider {
            OAuthProvider::Google => GOOGLE_SCOPES,
            OAuthProvider::Microsoft => MICROSOFT_AUTHORIZE_SCOPES,
        }
    }

    /// Scopes used when redeeming the authorization code.  Microsoft's token
    /// endpoint rejects mixed resources, so only Outlook scopes are requested;
    /// the Graph token is obtained later with the refresh token.
    pub fn code_exchange_scopes(&self) -> &'static [&'static str] {
        match self.provider {
            OAuthProvider::Google => GOOGLE_SCOPES,
            OAuthProvider::Microsoft => MICROSOFT_MAIL_SCOPES,
        }
    }

    /// Redirect URI base.  Google accepts literal `127.0.0.1`; Entra ignores
    /// the port but requires the `localhost` form.
    pub fn redirect_base(&self) -> &'static str {
        match self.provider {
            OAuthProvider::Google => "http://127.0.0.1",
            OAuthProvider::Microsoft => "http://localhost",
        }
    }
}

/// Render a space-separated scope list.
pub fn scope_string(scopes: &[&str]) -> String {
    scopes.join(" ")
}

/// Build the provider authorization URL.
pub fn build_auth_url(
    client: &ResolvedClient,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
    login_hint: Option<&str>,
) -> Result<String> {
    let mut url = url::Url::parse(client.auth_endpoint())
        .map_err(|error| AppError::internal(format!("invalid OAuth endpoint: {error}")))?;

    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("client_id", &client.client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", &scope_string(client.authorize_scopes()))
            .append_pair("state", state)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256");

        match client.provider {
            OAuthProvider::Google => {
                query
                    .append_pair("access_type", "offline")
                    .append_pair("prompt", "consent")
                    .append_pair("include_granted_scopes", "true");
            }
            OAuthProvider::Microsoft => {
                // `consent`, not `select_account`: the account picker alone lets
                // Entra answer from a previous grant, and a grant recorded before
                // a scope was added to the registration does not carry it. The
                // token then authenticates fine against Graph but is rejected by
                // IMAP — which looks like "sign-in succeeded, then immediately
                // expired". Asking for consent every time makes the issued token
                // match the scopes this build actually requests.
                query
                    .append_pair("prompt", "consent")
                    .append_pair("response_mode", "query");
            }
        }

        if let Some(hint) = login_hint.map(str::trim).filter(|hint| !hint.is_empty()) {
            query.append_pair("login_hint", hint);
        }
    }

    Ok(url.to_string())
}

/// Everything needed to run one interactive login.
pub struct AuthorizationRequest {
    pub url: String,
    pub state: String,
    pub redirect_uri: String,
    pub pkce: Pkce,
}

impl AuthorizationRequest {
    pub fn build(
        client: &ResolvedClient,
        redirect_uri: String,
        login_hint: Option<&str>,
    ) -> Result<Self> {
        let pkce = Pkce::generate();
        let state = pkce::random_state();
        let url = build_auth_url(client, &redirect_uri, &state, pkce.challenge(), login_hint)?;
        Ok(Self {
            url,
            state,
            redirect_uri,
            pkce,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    fn google() -> ResolvedClient {
        OAuthConfig::new(
            Some("google-client".into()),
            Some("google-secret".into()),
            Some("ms-client".into()),
        )
        .resolved(OAuthProvider::Google)
        .expect("google client")
    }

    fn microsoft() -> ResolvedClient {
        OAuthConfig::new(None, None, Some("ms-client".into()))
            .resolved(OAuthProvider::Microsoft)
            .expect("microsoft client")
    }

    fn query(url: &str) -> std::collections::HashMap<String, String> {
        Url::parse(url)
            .expect("url")
            .query_pairs()
            .into_owned()
            .collect()
    }

    #[test]
    fn builds_google_authorization_url() {
        let url = build_auth_url(
            &google(),
            "http://127.0.0.1:4321",
            "state-1",
            "challenge-1",
            Some("me@example.com"),
        )
        .expect("url");
        assert!(url.starts_with(GOOGLE_AUTH_ENDPOINT));
        let params = query(&url);
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some("google-client")
        );
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some("http://127.0.0.1:4321")
        );
        assert_eq!(params.get("state").map(String::as_str), Some("state-1"));
        assert_eq!(
            params.get("code_challenge").map(String::as_str),
            Some("challenge-1")
        );
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            params.get("access_type").map(String::as_str),
            Some("offline")
        );
        assert_eq!(params.get("prompt").map(String::as_str), Some("consent"));
        assert_eq!(
            params.get("include_granted_scopes").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            params.get("login_hint").map(String::as_str),
            Some("me@example.com")
        );
        let scope = params.get("scope").expect("scope");
        assert!(scope.contains("https://mail.google.com/"));
        assert!(scope.contains("https://www.googleapis.com/auth/tasks"));
    }

    #[test]
    fn builds_microsoft_authorization_url() {
        let url = build_auth_url(
            &microsoft(),
            "http://localhost:5000",
            "state-2",
            "challenge-2",
            None,
        )
        .expect("url");
        assert!(url.starts_with(MICROSOFT_AUTH_ENDPOINT));
        let params = query(&url);
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some("ms-client")
        );
        assert_eq!(params.get("prompt").map(String::as_str), Some("consent"));
        assert_eq!(
            params.get("response_mode").map(String::as_str),
            Some("query")
        );
        assert!(!params.contains_key("client_secret"));
        assert!(!params.contains_key("login_hint"));
        let scope = params.get("scope").expect("scope");
        assert!(scope.contains("https://outlook.office.com/IMAP.AccessAsUser.All"));
        assert!(scope.contains("https://graph.microsoft.com/Calendars.ReadWrite"));
        assert!(scope.contains("offline_access"));
    }

    #[test]
    fn microsoft_token_scopes_are_single_resource() {
        let mail = scope_string(MICROSOFT_MAIL_SCOPES);
        assert!(mail.contains("outlook.office.com"));
        assert!(!mail.contains("graph.microsoft.com"));
        let graph = scope_string(MICROSOFT_GRAPH_SCOPES);
        assert!(graph.contains("graph.microsoft.com"));
        assert!(!graph.contains("outlook.office.com"));
    }

    #[test]
    fn missing_client_ids_are_reported() {
        let config = OAuthConfig::default();
        let error = config
            .resolved(OAuthProvider::Google)
            .expect_err("must fail");
        assert!(error.message().contains("NUNCIO_GOOGLE_CLIENT_ID"));
        let error = config
            .resolved(OAuthProvider::Microsoft)
            .expect_err("must fail");
        assert!(error.message().contains("NUNCIO_MS_CLIENT_ID"));
    }

    #[test]
    fn settings_advanced_overrides_build_time_values() {
        let mut settings = AppSettings::default();
        settings.advanced.google_client_id = Some("from-settings".into());
        settings.advanced.google_client_secret = Some("from-settings-secret".into());
        settings.advanced.microsoft_client_id = Some("ms-settings".into());

        let config = OAuthConfig::new(Some("build".into()), Some("build-secret".into()), None)
            .with_settings(&settings);
        assert_eq!(config.google_client_id.as_deref(), Some("from-settings"));
        // The client secret is not read from settings; it lives in SecretStore.
        assert_eq!(config.google_client_secret.as_deref(), Some("build-secret"));
        assert_eq!(config.microsoft_client_id.as_deref(), Some("ms-settings"));
    }

    #[test]
    fn settings_do_not_set_a_client_secret() {
        let mut settings = AppSettings::default();
        settings.advanced.google_client_secret = Some("only-in-settings".into());

        let config = OAuthConfig::default().with_settings(&settings);
        assert_eq!(config.google_client_secret, None);
    }

    #[test]
    fn debug_redacts_client_secret() {
        let rendered = format!("{:?}", google());
        assert!(!rendered.contains("google-secret"));
        assert!(rendered.contains("redacted"));
    }
}
