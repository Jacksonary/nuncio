//! `AuthService`: interactive OAuth login, token cache/refresh and the
//! [`CredentialProvider`] implementation used by the mail and PIM engines.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use tokio::sync::Mutex as AsyncMutex;

use nuncio_core::error::AppError;
use nuncio_core::events;
use nuncio_core::models::{
    Account, AccountState, AdvancedSettingsPatch, AppSettings, AuthKind, DiscoveryResult,
    NewAccountInput, OAuthProvider, OAuthResult, ProviderKind, SettingsPatch,
};
use nuncio_core::repo;
use nuncio_core::traits::{CredentialProvider, EventSink, MailAuth, TokenResource};
use nuncio_core::{Db, Paths, Result, SettingsService};

use crate::discovery::dav::DavUrls;
use crate::discovery::{self, presets};
use crate::http;
use crate::oauth::flow::{Cancellation, LoopbackListener, UrlOpener, LOGIN_TIMEOUT};
use crate::oauth::token::{
    self, AccessToken, GoogleUserInfo, OAuthIdentity, PendingTokens, TokenError, TokenResponse,
};
use crate::oauth::{
    scope_string, AuthorizationRequest, OAuthConfig, ResolvedClient, GOOGLE_USERINFO_ENDPOINT,
    MICROSOFT_GRAPH_SCOPES, MICROSOFT_MAIL_SCOPES,
};
use crate::secrets::{self, SecretStore};

/// Interactive login results must be claimed within this window.
const SESSION_TTL: Duration = Duration::minutes(10);

// ─────────────────────────────────────────────────────────────────────────────
// Pending OAuth sessions
// ─────────────────────────────────────────────────────────────────────────────

struct PendingSession {
    provider: OAuthProvider,
    tokens: PendingTokens,
    created_at: DateTime<Utc>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Token cache
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct AccountTokens {
    /// Microsoft Outlook resource token (also used for SMTP).
    mail: Option<AccessToken>,
    /// Google multi-scope token (mail + calendar + contacts + tasks).
    google: Option<AccessToken>,
    /// Microsoft Graph resource token.
    graph: Option<AccessToken>,
}

impl AccountTokens {
    fn get(&self, provider: OAuthProvider, resource: TokenResource) -> Option<&AccessToken> {
        match (provider, resource) {
            (OAuthProvider::Google, TokenResource::Mail | TokenResource::GoogleApis) => {
                self.google.as_ref()
            }
            (OAuthProvider::Microsoft, TokenResource::Mail) => self.mail.as_ref(),
            (OAuthProvider::Microsoft, TokenResource::MicrosoftGraph) => self.graph.as_ref(),
            _ => None,
        }
    }

    fn set(&mut self, provider: OAuthProvider, resource: TokenResource, token: AccessToken) {
        match (provider, resource) {
            (OAuthProvider::Google, TokenResource::Mail | TokenResource::GoogleApis) => {
                self.google = Some(token);
            }
            (OAuthProvider::Microsoft, TokenResource::Mail) => self.mail = Some(token),
            (OAuthProvider::Microsoft, TokenResource::MicrosoftGraph) => self.graph = Some(token),
            _ => {}
        }
    }

    fn remove(&mut self, provider: OAuthProvider, resource: TokenResource) {
        match (provider, resource) {
            (OAuthProvider::Google, TokenResource::Mail | TokenResource::GoogleApis) => {
                self.google = None;
            }
            (OAuthProvider::Microsoft, TokenResource::Mail) => self.mail = None,
            (OAuthProvider::Microsoft, TokenResource::MicrosoftGraph) => self.graph = None,
            _ => {}
        }
    }
}

#[derive(Default)]
struct TokenCache {
    tokens: StdMutex<HashMap<String, AccountTokens>>,
    /// One async lock per account so concurrent callers share a single refresh.
    locks: StdMutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl TokenCache {
    fn tokens(&self) -> std::sync::MutexGuard<'_, HashMap<String, AccountTokens>> {
        self.tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn cached(
        &self,
        account_id: &str,
        provider: OAuthProvider,
        resource: TokenResource,
    ) -> Option<AccessToken> {
        self.tokens()
            .get(account_id)
            .and_then(|tokens| tokens.get(provider, resource))
            .cloned()
    }

    fn store(
        &self,
        account_id: &str,
        provider: OAuthProvider,
        resource: TokenResource,
        token: AccessToken,
    ) {
        self.tokens()
            .entry(account_id.to_string())
            .or_default()
            .set(provider, resource, token);
    }

    fn invalidate(&self, account_id: &str, provider: OAuthProvider, resource: TokenResource) {
        if let Some(tokens) = self.tokens().get_mut(account_id) {
            tokens.remove(provider, resource);
        }
    }

    fn invalidate_account(&self, account_id: &str) {
        self.tokens().remove(account_id);
    }

    fn lock_for(&self, account_id: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self
            .locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        locks
            .entry(account_id.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// AuthService
// ─────────────────────────────────────────────────────────────────────────────

/// Owns OAuth clients, pending sign-ins, cached tokens and secret storage.
///
/// Clone it freely: every clone shares the same cache and pending sessions.
#[derive(Clone)]
pub struct AuthService {
    db: Db,
    paths: Paths,
    events: Arc<dyn EventSink>,
    secrets: Arc<SecretStore>,
    config: OAuthConfig,
    opener: Arc<dyn UrlOpener>,
    http: reqwest::Client,
    settings: Option<SettingsService>,
    sessions: Arc<StdMutex<HashMap<String, PendingSession>>>,
    current_login: Arc<AsyncMutex<Option<Arc<Cancellation>>>>,
    tokens: Arc<TokenCache>,
}

impl AuthService {
    /// Build the service with the default secret store (`<data>/secrets`).
    pub fn new(
        db: Db,
        paths: Paths,
        events: Arc<dyn EventSink>,
        config: OAuthConfig,
        opener: Arc<dyn UrlOpener>,
    ) -> Result<Self> {
        let http = http::http_client()?;
        let secrets = Arc::new(SecretStore::new(&paths));
        Ok(Self {
            db,
            paths,
            events,
            secrets,
            config,
            opener,
            http,
            settings: None,
            sessions: Arc::new(StdMutex::new(HashMap::new())),
            current_login: Arc::new(AsyncMutex::new(None)),
            tokens: Arc::new(TokenCache::default()),
        })
    }

    /// Replace the secret store (tests, portable stores).
    pub fn with_secret_store(mut self, secrets: Arc<SecretStore>) -> Self {
        self.secrets = secrets;
        self
    }

    /// Read the ISP-DB privacy toggle and advanced OAuth client overrides live.
    ///
    /// The advanced Google client *id* is read from settings; the matching
    /// client *secret* is only ever read from the [`SecretStore`]
    /// (architecture §7).  Call [`AuthService::migrate_settings_secret`] (or
    /// [`AuthService::start`]) to move a legacy plaintext copy out of the
    /// settings table.
    pub fn with_settings(mut self, settings: SettingsService) -> Self {
        self.settings = Some(settings);
        self
    }

    /// Replace the HTTP client (tests with a mock transport).
    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    pub fn secrets(&self) -> &Arc<SecretStore> {
        &self.secrets
    }

    pub fn http_client(&self) -> &reqwest::Client {
        &self.http
    }

    pub fn config(&self) -> &OAuthConfig {
        &self.config
    }

    pub fn set_config(&mut self, config: OAuthConfig) {
        self.config = config;
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    fn resolved_client(&self, provider: OAuthProvider) -> Result<ResolvedClient> {
        let current = self.settings.as_ref().map(|settings| settings.current());
        let mut client = match current.as_ref() {
            Some(settings) => self.config.clone().with_settings(settings),
            None => self.config.clone(),
        };

        // Legacy builds persisted the advanced Google client secret in the
        // settings table.  Move it into the SecretStore before using it and
        // schedule removal of the plaintext copy.
        if let Some(settings) = current.as_ref() {
            if let Some(legacy) =
                non_empty_secret(settings.advanced.google_client_secret.as_deref())
            {
                match self.adopt_legacy_google_secret(legacy) {
                    Ok(()) => {}
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            "cannot move the OAuth client secret into the secret store; using the settings value for this request"
                        );
                    }
                }
                // The legacy value is still the newest user-provided secret
                // until the background scrub finishes.
                client.google_client_secret = Some(legacy.to_string());
            }
        }

        let mut resolved = client.resolved(provider)?;

        // The SecretStore is the source of truth for the Google client secret.
        if provider == OAuthProvider::Google {
            match self.secrets.get(secrets::SETTINGS_GOOGLE_SECRET_KEY) {
                Ok(Some(secret)) => {
                    let secret = secret.trim();
                    if !secret.is_empty() {
                        resolved.client_secret = Some(secret.to_string());
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "cannot read the OAuth client secret from the secret store");
                }
            }
        }

        Ok(resolved)
    }

    /// Whether an advanced Google OAuth client secret is stored.
    ///
    /// Used by the settings UI to show a "secret configured" hint without ever
    /// returning the secret itself.  A secret-store read failure is logged and
    /// reported as "not configured".
    pub fn has_google_client_secret(&self) -> bool {
        match self.secrets.get(secrets::SETTINGS_GOOGLE_SECRET_KEY) {
            Ok(value) => value.map(|value| !value.trim().is_empty()).unwrap_or(false),
            Err(error) => {
                tracing::warn!(%error, "cannot read the OAuth client secret from the secret store");
                false
            }
        }
    }

    /// Store (or clear) the advanced Google OAuth client secret.
    ///
    /// The value goes to the [`SecretStore`] only; a legacy plaintext copy in
    /// the settings table is removed, so `settings_get` never returns it.
    pub async fn set_google_client_secret(&self, secret: Option<String>) -> Result<()> {
        match non_empty_secret(secret.as_deref()) {
            Some(secret) => self
                .secrets
                .set(secrets::SETTINGS_GOOGLE_SECRET_KEY, secret)?,
            None => self.secrets.delete(secrets::SETTINGS_GOOGLE_SECRET_KEY)?,
        }
        self.clear_settings_secret().await
    }

    /// Move a plaintext `advanced.googleClientSecret` from the settings table
    /// into the [`SecretStore`] and scrub the plaintext copy.
    ///
    /// Idempotent and safe to call at startup; when no settings service is
    /// wired up it does nothing.
    pub async fn migrate_settings_secret(&self) -> Result<()> {
        let Some(settings) = self.settings.as_ref() else {
            return Ok(());
        };

        let legacy = non_empty_secret(settings.current().advanced.google_client_secret.as_deref())
            .map(str::to_string);
        if let Some(secret) = legacy {
            let existing = self.secrets.get(secrets::SETTINGS_GOOGLE_SECRET_KEY)?;
            if existing.as_deref() != Some(secret.as_str()) {
                self.secrets
                    .set(secrets::SETTINGS_GOOGLE_SECRET_KEY, &secret)?;
            }
            tracing::info!(
                "moved the advanced Google OAuth client secret from settings into the secret store"
            );
        }

        self.clear_settings_secret().await
    }

    /// Write a legacy settings secret into the SecretStore when it is not there
    /// yet, then schedule an asynchronous scrub of the settings row.
    fn adopt_legacy_google_secret(&self, legacy: &str) -> Result<()> {
        let existing = self.secrets.get(secrets::SETTINGS_GOOGLE_SECRET_KEY)?;
        if existing.as_deref() != Some(legacy) {
            self.secrets
                .set(secrets::SETTINGS_GOOGLE_SECRET_KEY, legacy)?;
        }
        self.schedule_settings_secret_scrub();
        Ok(())
    }

    /// Remove `advanced.googleClientSecret` from the settings table.
    ///
    /// The settings service emits `settings://changed`, so the UI clears the
    /// corresponding field as well.
    async fn clear_settings_secret(&self) -> Result<()> {
        let Some(settings) = self.settings.as_ref() else {
            return Ok(());
        };
        if settings.current().advanced.google_client_secret.is_none() {
            return Ok(());
        }

        let patch = SettingsPatch {
            advanced: Some(AdvancedSettingsPatch {
                google_client_secret: Some(None),
                ..Default::default()
            }),
            ..Default::default()
        };
        settings.update(patch).await.map(|_| ())
    }

    /// Scrub the plaintext secret in the background; failures are logged and
    /// retried on the next service start.
    fn schedule_settings_secret_scrub(&self) {
        if self.settings.is_none() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::debug!(
                "no async runtime available; the plaintext OAuth client secret is scrubbed on next start"
            );
            return;
        };
        let service = self.clone();
        handle.spawn(async move {
            if let Err(error) = service.clear_settings_secret().await {
                tracing::warn!(
                    %error,
                    "cannot remove the plaintext OAuth client secret from settings"
                );
            }
        });
    }

    async fn load_account(&self, account_id: &str) -> Result<Account> {
        let id = account_id.to_string();
        let account = self
            .db
            .read(move |conn| repo::accounts::get(conn, &id))
            .await?;
        AppError::require(account, "account")
    }

    // ── Discovery ────────────────────────────────────────────────────────

    /// Discovery honoring the "allow ISPDB" privacy setting.
    pub async fn discover(&self, email: &str) -> Result<DiscoveryResult> {
        let allow_ispdb = self
            .settings
            .as_ref()
            .map(|settings| settings.current().privacy.allow_ispdb_lookup)
            .unwrap_or(true);
        self.discover_with(email, allow_ispdb).await
    }

    /// Discovery with an explicit ISPDB toggle.
    pub async fn discover_with(&self, email: &str, allow_ispdb: bool) -> Result<DiscoveryResult> {
        discovery::discover_with_client(&self.http, email, allow_ispdb).await
    }

    /// CalDAV/CardDAV endpoints for an address or domain.
    pub async fn discover_dav(&self, email_or_domain: &str) -> DavUrls {
        discovery::discover_dav_with_client(&self.http, email_or_domain).await
    }

    // ── Interactive OAuth ────────────────────────────────────────────────

    /// Run the full authorization-code + PKCE flow in the system browser.
    ///
    /// Rejects with [`AppError::Cancelled`] when the user cancels or the
    /// five-minute window expires.
    pub async fn oauth_login(
        &self,
        provider: OAuthProvider,
        login_hint: Option<String>,
    ) -> Result<OAuthResult> {
        let client = self.resolved_client(provider)?;
        self.prune_sessions();

        let cancel = Arc::new(Cancellation::new());
        {
            let mut current = self.current_login.lock().await;
            if current.is_some() {
                return Err(AppError::conflict(
                    "another sign-in is already in progress; finish or cancel it first",
                ));
            }
            *current = Some(cancel.clone());
        }

        let result = self.run_login(&client, login_hint.as_deref(), cancel).await;
        self.current_login.lock().await.take();
        result
    }

    /// Abort the in-flight login, if any.
    pub async fn oauth_cancel(&self) -> Result<()> {
        let cancel = self.current_login.lock().await.clone();
        if let Some(cancel) = cancel {
            tracing::info!("cancelling the in-flight OAuth sign-in");
            cancel.cancel();
        }
        Ok(())
    }

    async fn run_login(
        &self,
        client: &ResolvedClient,
        login_hint: Option<&str>,
        cancel: Arc<Cancellation>,
    ) -> Result<OAuthResult> {
        let listener = LoopbackListener::bind().await?;
        let redirect_uri = listener.redirect_uri(client.provider);
        let request = AuthorizationRequest::build(client, redirect_uri, login_hint)?;

        tracing::info!(
            provider = client.provider.as_str(),
            port = listener.port(),
            "opening the system browser for OAuth sign-in"
        );
        self.opener.open(&request.url)?;

        let params =
            crate::oauth::flow::wait_for_callback(listener, &request.state, cancel, LOGIN_TIMEOUT)
                .await?;
        let code = params.code.unwrap_or_default();

        let exchanged = self
            .exchange_code(
                client,
                &code,
                &request.redirect_uri,
                request.pkce.verifier(),
            )
            .await?;
        let identity = self
            .resolve_identity(
                client.provider,
                &exchanged.access_token.value,
                exchanged.id_token.as_deref(),
                login_hint,
            )
            .await?;

        let mut tokens = PendingTokens {
            access: exchanged.access_token,
            refresh: exchanged.refresh_token,
            graph_access: None,
            graph_refresh: None,
        };

        if client.provider == OAuthProvider::Microsoft {
            // Single-resource token calls: redeem the refresh token for Graph.
            match self
                .request_refresh(client, &tokens.refresh, MICROSOFT_GRAPH_SCOPES)
                .await
            {
                Ok(graph) => {
                    tokens.graph_access = Some(graph.cache_token(Utc::now()));
                    tokens.graph_refresh = Some(
                        graph
                            .refresh_token_value()
                            .map(str::to_string)
                            .unwrap_or_else(|| tokens.refresh.clone()),
                    );
                    tracing::debug!("obtained a Microsoft Graph token during sign-in");
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "could not obtain a Microsoft Graph token during sign-in; calendar, contacts and tasks will retry with the refresh token"
                    );
                    tokens.graph_refresh = Some(tokens.refresh.clone());
                }
            }
        }

        let session_id = uuid::Uuid::new_v4().to_string();
        let result = OAuthResult {
            session_id: session_id.clone(),
            provider: client.provider,
            email: identity.email.clone(),
            display_name: identity.display_name.clone(),
        };
        let session = PendingSession {
            provider: client.provider,
            tokens,
            created_at: Utc::now(),
        };
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id, session);

        tracing::info!(
            provider = client.provider.as_str(),
            email_domain = %result.email.rsplit('@').next().unwrap_or(""),
            "OAuth sign-in completed"
        );
        Ok(result)
    }

    /// Persist an interactive session's tokens under an account id.
    pub fn attach_session(&self, session_id: &str, account_id: &str) -> Result<()> {
        let session = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id)
            .ok_or_else(|| {
                AppError::not_found("OAuth sign-in session not found or already used")
            })?;

        if Utc::now() - session.created_at > SESSION_TTL {
            return Err(AppError::auth(
                "the OAuth sign-in session expired; sign in again",
            ));
        }

        match session.provider {
            OAuthProvider::Google => {
                self.secrets
                    .set(&secrets::refresh_key(account_id), &session.tokens.refresh)?;
                self.tokens.store(
                    account_id,
                    OAuthProvider::Google,
                    TokenResource::Mail,
                    session.tokens.access.clone(),
                );
            }
            OAuthProvider::Microsoft => {
                self.secrets.set(
                    &secrets::account_key(account_id, "refresh_token:outlook"),
                    &session.tokens.refresh,
                )?;
                let graph_refresh = session
                    .tokens
                    .graph_refresh
                    .as_deref()
                    .unwrap_or(&session.tokens.refresh);
                self.secrets.set(
                    &secrets::account_key(account_id, "refresh_token:graph"),
                    graph_refresh,
                )?;
                self.tokens.store(
                    account_id,
                    OAuthProvider::Microsoft,
                    TokenResource::Mail,
                    session.tokens.access.clone(),
                );
                if let Some(graph_access) = session.tokens.graph_access.clone() {
                    self.tokens.store(
                        account_id,
                        OAuthProvider::Microsoft,
                        TokenResource::MicrosoftGraph,
                        graph_access,
                    );
                }
            }
        }

        tracing::info!(
            account_id,
            provider = session.provider.as_str(),
            "OAuth session attached to account"
        );
        Ok(())
    }

    /// Drop a pending session without persisting anything.
    pub fn discard_session(&self, session_id: &str) -> Result<()> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id);
        Ok(())
    }

    fn prune_sessions(&self) {
        let now = Utc::now();
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|_, session| now - session.created_at <= SESSION_TTL);
    }

    // ── HTTP helpers ─────────────────────────────────────────────────────

    async fn exchange_code(
        &self,
        client: &ResolvedClient,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<CodeExchangeResult> {
        // Microsoft's v2 token endpoint requires the resource scopes; Google
        // derives them from the authorization request and ignores a `scope`
        // parameter on code exchange.
        let scope = (client.provider == OAuthProvider::Microsoft)
            .then(|| scope_string(client.code_exchange_scopes()));
        let form = CodeExchangeRequest {
            grant_type: "authorization_code",
            code,
            redirect_uri,
            client_id: &client.client_id,
            code_verifier: verifier,
            client_secret: client.client_secret.as_deref(),
            scope: scope.as_deref(),
        };

        let response = self
            .post_token(client.token_endpoint(), &form)
            .await
            .map_err(|failure| failure.into_app_error("token exchange failed"))?;

        let access_token = response.cache_token(Utc::now());
        let refresh_token = response.refresh_token_value().map(str::to_string).ok_or_else(|| {
            AppError::auth(
                "the provider did not return a refresh token; make sure offline access is allowed and try again",
            )
        })?;

        Ok(CodeExchangeResult {
            access_token,
            refresh_token,
            id_token: response.id_token,
        })
    }

    async fn resolve_identity(
        &self,
        provider: OAuthProvider,
        access_token: &str,
        id_token: Option<&str>,
        login_hint: Option<&str>,
    ) -> Result<OAuthIdentity> {
        match provider {
            OAuthProvider::Google => {
                let response = self
                    .http
                    .get(GOOGLE_USERINFO_ENDPOINT)
                    .bearer_auth(access_token)
                    .timeout(StdDuration::from_secs(20))
                    .send()
                    .await
                    .map_err(|error| {
                        AppError::network(format!("cannot read the Google profile: {error}"))
                    })?;
                let status = response.status();
                if !status.is_success() {
                    return Err(AppError::auth(format!(
                        "Google profile request returned HTTP {}",
                        status.as_u16()
                    )));
                }
                let info: GoogleUserInfo = response.json().await.map_err(|error| {
                    AppError::protocol(format!("cannot decode the Google profile: {error}"))
                })?;
                token::google_identity(&info)
            }
            OAuthProvider::Microsoft => {
                let id_token = id_token.ok_or_else(|| {
                    AppError::auth("Microsoft did not return an ID token; sign in again")
                })?;
                let claims = token::decode_id_token(id_token)?;
                token::microsoft_identity(&claims, login_hint)
            }
        }
    }

    async fn request_refresh(
        &self,
        client: &ResolvedClient,
        refresh_token: &str,
        scopes: &[&str],
    ) -> Result<TokenResponse> {
        let scope = scope_string(scopes);
        let form = RefreshRequest {
            grant_type: "refresh_token",
            refresh_token,
            client_id: &client.client_id,
            client_secret: client.client_secret.as_deref(),
            scope: Some(scope.as_str()),
        };
        self.post_token(client.token_endpoint(), &form)
            .await
            .map_err(|failure| failure.into_app_error("token refresh failed"))
    }

    async fn post_token<T>(
        &self,
        url: &str,
        form: &T,
    ) -> std::result::Result<TokenResponse, RefreshFailure>
    where
        T: serde::Serialize + ?Sized,
    {
        let response = self
            .http
            .post(url)
            .form(form)
            .timeout(StdDuration::from_secs(20))
            .send()
            .await
            .map_err(|error| RefreshFailure::Network(format!("token request failed: {error}")))?;
        let status = response.status().as_u16();
        let body = response.text().await.map_err(|error| {
            RefreshFailure::Network(format!("cannot read token response: {error}"))
        })?;
        token::decode_token_response(status, &body).map_err(RefreshFailure::Token)
    }

    // ── Token cache / refresh ───────────────────────────────────────────

    async fn access_token_for(
        &self,
        account: &Account,
        resource: TokenResource,
    ) -> Result<AccessToken> {
        if account.auth_kind != AuthKind::Oauth {
            return Err(AppError::unsupported(format!(
                "account {} uses password authentication",
                account.email
            )));
        }
        let provider = account.oauth_provider.ok_or_else(|| {
            AppError::auth(format!(
                "account {} is marked as OAuth but has no provider",
                account.email
            ))
        })?;

        let supported = matches!(
            (provider, resource),
            (
                OAuthProvider::Google,
                TokenResource::Mail | TokenResource::GoogleApis
            ) | (
                OAuthProvider::Microsoft,
                TokenResource::Mail | TokenResource::MicrosoftGraph
            )
        );
        if !supported {
            return Err(AppError::unsupported(format!(
                "resource '{}' is not available for {} accounts",
                resource.as_str(),
                provider.as_str()
            )));
        }

        if let Some(cached) = self.tokens.cached(&account.id, provider, resource) {
            if cached.is_valid(Utc::now()) {
                return Ok(cached);
            }
        }

        // Serialise refreshes per account so concurrent readers share one call.
        let lock = self.tokens.lock_for(&account.id);
        let _guard = lock.lock().await;
        if let Some(cached) = self.tokens.cached(&account.id, provider, resource) {
            if cached.is_valid(Utc::now()) {
                return Ok(cached);
            }
        }
        self.refresh_locked(account, provider, resource).await
    }

    async fn refresh_locked(
        &self,
        account: &Account,
        provider: OAuthProvider,
        resource: TokenResource,
    ) -> Result<AccessToken> {
        let client = self.resolved_client(provider)?;
        let (key, scopes): (String, &[&str]) = match (provider, resource) {
            (OAuthProvider::Google, TokenResource::Mail | TokenResource::GoogleApis) => {
                (secrets::refresh_key(&account.id), &[])
            }
            (OAuthProvider::Microsoft, TokenResource::Mail) => (
                secrets::account_key(&account.id, "refresh_token:outlook"),
                MICROSOFT_MAIL_SCOPES,
            ),
            (OAuthProvider::Microsoft, TokenResource::MicrosoftGraph) => (
                secrets::account_key(&account.id, "refresh_token:graph"),
                MICROSOFT_GRAPH_SCOPES,
            ),
            _ => {
                return Err(AppError::unsupported(format!(
                    "resource '{}' cannot be refreshed for this provider",
                    resource.as_str()
                )))
            }
        };

        let refresh_token = self
            .secrets
            .get(&key)?
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        let refresh_token = match refresh_token {
            Some(token) => token,
            None => {
                self.mark_needs_reauth(
                    &account.id,
                    "no OAuth refresh token is stored for this account",
                )
                .await;
                return Err(AppError::auth(
                    "no stored OAuth refresh token; sign in to this account again",
                ));
            }
        };

        let scope = if scopes.is_empty() {
            None
        } else {
            Some(scope_string(scopes))
        };
        let form = RefreshRequest {
            grant_type: "refresh_token",
            refresh_token: &refresh_token,
            client_id: &client.client_id,
            client_secret: client.client_secret.as_deref(),
            scope: scope.as_deref(),
        };

        let response = match self.post_token(client.token_endpoint(), &form).await {
            Ok(response) => response,
            Err(failure) => {
                if failure.is_invalid_grant() {
                    self.mark_needs_reauth(
                        &account.id,
                        "the OAuth sign-in is no longer valid; sign in again",
                    )
                    .await;
                }
                return Err(failure.into_app_error("token refresh failed"));
            }
        };

        // Providers rotate refresh tokens; keep the newest one.
        if let Some(rotated) = response.refresh_token_value() {
            if let Err(error) = self.secrets.set(&key, rotated) {
                tracing::warn!(%error, account_id = %account.id, "cannot persist the rotated refresh token");
            }
        }

        let token = response.cache_token(Utc::now());
        self.tokens
            .store(&account.id, provider, resource, token.clone());
        tracing::debug!(
            account_id = %account.id,
            provider = provider.as_str(),
            resource = resource.as_str(),
            "refreshed OAuth access token"
        );
        Ok(token)
    }

    async fn mark_needs_reauth(&self, account_id: &str, reason: &str) {
        let id = account_id.to_string();
        let reason = reason.to_string();
        let update = self
            .db
            .write(move |conn| {
                repo::accounts::update_state(
                    conn,
                    &id,
                    AccountState::NeedsReauth,
                    Some(reason.as_str()),
                )
            })
            .await;
        match update {
            Ok(()) => {
                tracing::info!(account_id, "account marked as needing re-authentication");
                events::emit_accounts_changed(self.events.as_ref(), vec![account_id.to_string()]);
            }
            Err(error) => {
                tracing::warn!(%error, account_id, "cannot mark the account as needsReauth");
            }
        }
    }

    // ── Secret management ───────────────────────────────────────────────

    /// Store the IMAP/SMTP/POP3 password (or app password / authorization code).
    pub fn set_password(&self, account_id: &str, password: &str) -> Result<()> {
        if password.is_empty() {
            return Err(AppError::invalid_input("password cannot be empty"));
        }
        self.secrets
            .set(&secrets::password_key(account_id), password)
    }

    /// Remove every secret belonging to an account and drop cached tokens.
    pub fn delete_account_secrets(&self, account_id: &str) -> Result<()> {
        self.secrets.delete_prefix(account_id)?;
        self.tokens.invalidate_account(account_id);
        Ok(())
    }

    /// Export account secrets for an encrypted backup.
    pub fn export_account_secrets(
        &self,
        account_ids: &[String],
    ) -> Result<HashMap<String, String>> {
        let mut exported = HashMap::new();
        for account_id in account_ids {
            for suffix in secrets::ACCOUNT_KEY_SUFFIXES {
                let key = secrets::account_key(account_id, suffix);
                if let Some(value) = self.secrets.get(&key)? {
                    exported.insert(key, value);
                }
            }
        }
        Ok(exported)
    }

    /// Import secrets produced by [`AuthService::export_account_secrets`].
    pub fn import_account_secrets(&self, exported: &HashMap<String, String>) -> Result<()> {
        for (key, value) in exported {
            if !key.starts_with("account:") {
                return Err(AppError::invalid_input(format!(
                    "unexpected secret key in backup: {key}"
                )));
            }
            self.secrets.set(key, value)?;
        }
        Ok(())
    }

    /// Apply a newly created account's credential input.
    pub fn apply_new_account(&self, account_id: &str, input: &NewAccountInput) -> Result<()> {
        match input.auth_kind {
            AuthKind::Password => {
                let password = input
                    .password
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                match password {
                    Some(password) => self.set_password(account_id, password),
                    None if input.provider == ProviderKind::Local => Ok(()),
                    None => Err(AppError::invalid_input(
                        "a password, app password or authorization code is required",
                    )),
                }
            }
            AuthKind::Oauth => {
                let session_id = input.oauth_session_id.as_deref().ok_or_else(|| {
                    AppError::invalid_input("the OAuth sign-in session is missing; sign in again")
                })?;
                self.attach_session(session_id, account_id)
            }
        }
    }

    // ── Lifecycle hooks (architecture §5) ───────────────────────────────

    pub fn start(&self) {
        tracing::debug!("nuncio-auth started");

        // Move a legacy plaintext OAuth client secret out of the settings
        // table as soon as possible (architecture §7: secrets only in the
        // SecretStore).  The call is idempotent and safe when no settings
        // service is wired up.
        if self.settings.is_none() {
            return;
        }
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let service = self.clone();
                handle.spawn(async move {
                    if let Err(error) = service.migrate_settings_secret().await {
                        tracing::warn!(
                            %error,
                            "cannot migrate the advanced OAuth client secret into the secret store"
                        );
                    }
                });
            }
            Err(_) => {
                tracing::debug!(
                    "no async runtime at startup; the OAuth client secret migration is deferred"
                );
            }
        }
    }

    pub fn on_account_added(&self, account: &Account) {
        tracing::debug!(account_id = %account.id, "auth service saw a new account");
    }

    pub fn on_account_changed(&self, account: &Account) {
        // `account_update` never changes stored credentials; keep the cache.
        tracing::debug!(account_id = %account.id, "auth service saw an account change");
    }

    pub fn on_account_removed(&self, account_id: &str) {
        self.tokens.invalidate_account(account_id);
    }

    pub async fn shutdown(&self) {
        if let Err(error) = self.oauth_cancel().await {
            tracing::debug!(%error, "cannot cancel OAuth sign-in during shutdown");
        }
    }
}

struct CodeExchangeResult {
    access_token: AccessToken,
    refresh_token: String,
    id_token: Option<String>,
}

#[derive(serde::Serialize)]
struct CodeExchangeRequest<'a> {
    grant_type: &'static str,
    code: &'a str,
    redirect_uri: &'a str,
    client_id: &'a str,
    code_verifier: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_secret: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'a str>,
}

#[derive(serde::Serialize)]
struct RefreshRequest<'a> {
    grant_type: &'static str,
    refresh_token: &'a str,
    client_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_secret: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'a str>,
}

enum RefreshFailure {
    Network(String),
    Token(TokenError),
}

impl RefreshFailure {
    fn is_invalid_grant(&self) -> bool {
        matches!(self, Self::Token(error) if error.is_invalid_grant())
    }

    fn into_app_error(self, context: &str) -> AppError {
        match self {
            Self::Network(message) => AppError::network(message),
            Self::Token(error) => error.into_app_error(context),
        }
    }
}

/// Trim a candidate secret and treat whitespace-only values as absent.
fn non_empty_secret(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn username_for(account: &Account) -> String {
    account
        .imap
        .as_ref()
        .or(account.pop3.as_ref())
        .map(|server| server.username.trim())
        .filter(|username| !username.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| account.email.clone())
}

#[async_trait]
impl CredentialProvider for AuthService {
    async fn mail_auth(&self, account: &Account) -> Result<MailAuth> {
        if account.provider.is_local() {
            return Err(AppError::unsupported(
                "local accounts do not have mail credentials",
            ));
        }

        match account.auth_kind {
            AuthKind::Password => {
                let password = self
                    .secrets
                    .get(&secrets::password_key(&account.id))?
                    .filter(|password| !password.is_empty())
                    .ok_or_else(|| {
                        AppError::auth(format!(
                            "no password is stored for {}; enter it again",
                            account.email
                        ))
                    })?;
                Ok(MailAuth::Password {
                    username: username_for(account),
                    password,
                })
            }
            AuthKind::Oauth => {
                let token = self.access_token_for(account, TokenResource::Mail).await?;
                Ok(MailAuth::XOAuth2 {
                    username: username_for(account),
                    access_token: token.value,
                })
            }
        }
    }

    async fn api_token(&self, account_id: &str, resource: TokenResource) -> Result<String> {
        let account = self.load_account(account_id).await?;
        let token = self.access_token_for(&account, resource).await?;
        Ok(token.value)
    }

    async fn dav_auth(&self, account: &Account) -> Result<(String, String)> {
        match self.secrets.get(&secrets::password_key(&account.id))? {
            Some(password) if !password.is_empty() => Ok((username_for(account), password)),
            _ if account.auth_kind == AuthKind::Oauth => Err(AppError::unsupported(
                "CalDAV/CardDAV basic auth is not available for OAuth accounts",
            )),
            _ => Err(AppError::auth(format!(
                "no password is stored for {}; enter it again",
                account.email
            ))),
        }
    }

    async fn report_auth_failure(&self, account_id: &str, resource: TokenResource) {
        let account = match self.load_account(account_id).await {
            Ok(account) => account,
            Err(error) => {
                tracing::debug!(%error, account_id, "cannot handle auth failure for an unknown account");
                return;
            }
        };

        let provider = account.oauth_provider;
        match provider {
            Some(provider) => self.tokens.invalidate(account_id, provider, resource),
            None => self.tokens.invalidate_account(account_id),
        }

        if account.auth_kind == AuthKind::Password {
            self.mark_needs_reauth(account_id, "the server rejected the stored password")
                .await;
            return;
        }

        let Some(provider) = provider else {
            self.mark_needs_reauth(account_id, "the account has no OAuth provider")
                .await;
            return;
        };

        let lock = self.tokens.lock_for(account_id);
        let _guard = lock.lock().await;
        if let Err(error) = self.refresh_locked(&account, provider, resource).await {
            tracing::warn!(
                account_id,
                resource = resource.as_str(),
                %error,
                "token refresh failed after the server rejected the credential"
            );
        }
    }
}

/// Built-in presets are public so the settings UI can show hints before login.
pub fn preset_for_domain(domain: &str) -> Option<nuncio_core::models::DiscoveryResult> {
    presets::lookup(domain).map(|preset| preset.into_result(&format!("user@{domain}")))
}

/// Re-export used by the app when constructing `OAuthConfig` from settings.
pub fn oauth_config_from_settings(settings: &AppSettings) -> OAuthConfig {
    OAuthConfig::from_env().with_settings(settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::models::Security;
    use nuncio_core::models::ServerConfig;
    use nuncio_core::traits::testing::RecordingEventSink;
    use std::path::Path;

    struct NoopOpener;

    impl UrlOpener for NoopOpener {
        fn open(&self, _url: &str) -> Result<()> {
            Ok(())
        }
    }

    fn test_account(
        id: &str,
        auth_kind: AuthKind,
        oauth_provider: Option<OAuthProvider>,
    ) -> Account {
        let provider = match oauth_provider {
            Some(OAuthProvider::Google) => ProviderKind::Gmail,
            Some(OAuthProvider::Microsoft) => ProviderKind::Outlook,
            None => ProviderKind::Imap,
        };
        Account {
            id: id.to_string(),
            provider,
            email: format!("{id}@example.com"),
            display_name: "Test User".to_string(),
            color: "#0078d4".to_string(),
            auth_kind,
            oauth_provider,
            imap: Some(ServerConfig::new(
                "imap.example.com",
                993,
                Security::Tls,
                format!("{id}@example.com"),
            )),
            smtp: Some(ServerConfig::new(
                "smtp.example.com",
                465,
                Security::Tls,
                format!("{id}@example.com"),
            )),
            pop3: None,
            caldav_url: None,
            carddav_url: None,
            sync_window_days: 90,
            mail_enabled: true,
            calendar_enabled: true,
            contacts_enabled: true,
            tasks_enabled: true,
            notifications_enabled: true,
            include_in_unified: true,
            sort_order: 0,
            state: AccountState::Ok,
            state_message: None,
            capabilities: Default::default(),
            created_at: nuncio_core::models::now(),
        }
    }

    fn service(dir: &Path) -> (AuthService, Arc<RecordingEventSink>) {
        let db = Db::open_in_memory().expect("in-memory db");
        db.migrate_core().expect("migrate");
        let paths = Paths::new(dir);
        paths.ensure_dirs().expect("dirs");
        let events = Arc::new(RecordingEventSink::new());
        let config = OAuthConfig::new(
            Some("google-client".into()),
            Some("google-secret".into()),
            Some("ms-client".into()),
        );
        let service = AuthService::new(db, paths, events.clone(), config, Arc::new(NoopOpener))
            .expect("service")
            .with_secret_store(Arc::new(SecretStore::file_only(dir.join("secrets"))));
        (service, events)
    }

    /// Service wired to a live settings service sharing the same in-memory DB.
    fn service_with_settings(
        dir: &Path,
    ) -> (AuthService, SettingsService, Arc<RecordingEventSink>) {
        let db = Db::open_in_memory().expect("in-memory db");
        db.migrate_core().expect("migrate");
        let paths = Paths::new(dir);
        paths.ensure_dirs().expect("dirs");
        let events = Arc::new(RecordingEventSink::new());
        // No build-time secret: tests must observe only the SecretStore.
        let config = OAuthConfig::new(Some("google-client".into()), None, Some("ms-client".into()));
        let service = AuthService::new(
            db.clone(),
            paths,
            events.clone(),
            config,
            Arc::new(NoopOpener),
        )
        .expect("service")
        .with_secret_store(Arc::new(SecretStore::file_only(dir.join("secrets"))));
        let settings = SettingsService::from_settings(db, events.clone(), AppSettings::default());
        (service.with_settings(settings.clone()), settings, events)
    }

    fn legacy_secret_patch(secret: &str) -> SettingsPatch {
        SettingsPatch {
            advanced: Some(AdvancedSettingsPatch {
                google_client_secret: Some(Some(secret.to_string())),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    async fn insert_account(service: &AuthService, account: &Account) {
        let account = account.clone();
        service
            .db
            .write(move |conn| repo::accounts::insert(conn, &account))
            .await
            .expect("insert account");
    }

    fn pending_session(provider: OAuthProvider, tokens: PendingTokens) -> PendingSession {
        PendingSession {
            provider,
            tokens,
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn password_account_round_trip_and_dav_auth() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (service, _events) = service(dir.path());
        let account = test_account("pw", AuthKind::Password, None);
        insert_account(&service, &account).await;

        service.set_password("pw", "s3cret").expect("set password");

        let auth = service.mail_auth(&account).await.expect("mail auth");
        match auth {
            MailAuth::Password { username, password } => {
                assert_eq!(username, "pw@example.com");
                assert_eq!(password, "s3cret");
            }
            other => panic!("unexpected auth: {other:?}"),
        }

        let (username, password) = service.dav_auth(&account).await.expect("dav auth");
        assert_eq!(username, "pw@example.com");
        assert_eq!(password, "s3cret");

        service
            .delete_account_secrets("pw")
            .expect("delete secrets");
        assert!(service.mail_auth(&account).await.is_err());
        assert!(service.dav_auth(&account).await.is_err());
    }

    #[tokio::test]
    async fn empty_password_is_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (service, _) = service(dir.path());
        assert!(service.set_password("x", "").is_err());
    }

    #[tokio::test]
    async fn google_session_attach_serves_mail_and_api_tokens() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (service, _events) = service(dir.path());
        let account = test_account("g", AuthKind::Oauth, Some(OAuthProvider::Google));
        insert_account(&service, &account).await;

        let tokens = PendingTokens {
            access: AccessToken::new("google-access", Utc::now() + Duration::hours(1)),
            refresh: "google-refresh".to_string(),
            graph_access: None,
            graph_refresh: None,
        };
        service.sessions.lock().expect("sessions").insert(
            "session-g".to_string(),
            pending_session(OAuthProvider::Google, tokens),
        );

        service
            .attach_session("session-g", "g")
            .expect("attach session");

        let auth = service.mail_auth(&account).await.expect("mail auth");
        match auth {
            MailAuth::XOAuth2 {
                username,
                access_token,
            } => {
                assert_eq!(username, "g@example.com");
                assert_eq!(access_token, "google-access");
            }
            other => panic!("unexpected auth: {other:?}"),
        }

        // Google uses one multi-scope token for mail and all APIs.
        assert_eq!(
            service
                .api_token("g", TokenResource::GoogleApis)
                .await
                .expect("api token"),
            "google-access"
        );

        let exported = service
            .export_account_secrets(&["g".to_string()])
            .expect("export");
        assert_eq!(
            exported.get("account:g:refresh_token").map(String::as_str),
            Some("google-refresh")
        );
    }

    #[tokio::test]
    async fn microsoft_session_attach_keeps_outlook_and_graph_tokens_separate() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (service, _events) = service(dir.path());
        let account = test_account("m", AuthKind::Oauth, Some(OAuthProvider::Microsoft));
        insert_account(&service, &account).await;

        let tokens = PendingTokens {
            access: AccessToken::new("outlook-access", Utc::now() + Duration::hours(1)),
            refresh: "outlook-refresh".to_string(),
            graph_access: Some(AccessToken::new(
                "graph-access",
                Utc::now() + Duration::hours(1),
            )),
            graph_refresh: Some("graph-refresh".to_string()),
        };
        service.sessions.lock().expect("sessions").insert(
            "session-m".to_string(),
            pending_session(OAuthProvider::Microsoft, tokens),
        );
        service
            .attach_session("session-m", "m")
            .expect("attach session");

        match service.mail_auth(&account).await.expect("mail auth") {
            MailAuth::XOAuth2 { access_token, .. } => assert_eq!(access_token, "outlook-access"),
            other => panic!("unexpected auth: {other:?}"),
        }
        assert_eq!(
            service
                .api_token("m", TokenResource::MicrosoftGraph)
                .await
                .expect("graph token"),
            "graph-access"
        );

        let exported = service
            .export_account_secrets(&["m".to_string()])
            .expect("export");
        assert_eq!(
            exported
                .get("account:m:refresh_token:outlook")
                .map(String::as_str),
            Some("outlook-refresh")
        );
        assert_eq!(
            exported
                .get("account:m:refresh_token:graph")
                .map(String::as_str),
            Some("graph-refresh")
        );
    }

    #[tokio::test]
    async fn missing_session_is_reported() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (service, _) = service(dir.path());
        assert!(service.attach_session("nope", "acct").is_err());
        assert!(service.discard_session("nope").is_ok());
    }

    #[tokio::test]
    async fn report_auth_failure_marks_password_account_needs_reauth() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (service, events) = service(dir.path());
        let account = test_account("bad", AuthKind::Password, None);
        insert_account(&service, &account).await;
        service.set_password("bad", "wrong").expect("set password");

        service
            .report_auth_failure("bad", TokenResource::Mail)
            .await;

        let stored = service.load_account("bad").await.expect("account");
        assert_eq!(stored.state, AccountState::NeedsReauth);
        assert!(stored
            .state_message
            .as_deref()
            .unwrap_or_default()
            .contains("password"));

        let emitted = events.take();
        assert!(emitted.iter().any(|(name, _)| name == events_name()));
    }

    fn events_name() -> &'static str {
        nuncio_core::events::ACCOUNTS_CHANGED
    }

    #[tokio::test]
    async fn report_auth_failure_for_unknown_account_is_ignored() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (service, events) = service(dir.path());
        service
            .report_auth_failure("missing", TokenResource::Mail)
            .await;
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn oauth_cancel_without_login_is_ok() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (service, _) = service(dir.path());
        service.oauth_cancel().await.expect("cancel");
    }

    #[tokio::test]
    async fn apply_new_account_stores_password_and_attaches_oauth() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (service, _) = service(dir.path());

        let mut input = NewAccountInput {
            provider: ProviderKind::Imap,
            email: "new@example.com".to_string(),
            display_name: "New".to_string(),
            auth_kind: AuthKind::Password,
            password: Some("pw".to_string()),
            oauth_session_id: None,
            imap: None,
            smtp: None,
            pop3: None,
            caldav_url: None,
            carddav_url: None,
            sync_window_days: 90,
            mail_enabled: true,
            calendar_enabled: false,
            contacts_enabled: false,
            tasks_enabled: false,
        };
        service
            .apply_new_account("new", &input)
            .expect("apply password");
        assert_eq!(
            service
                .secrets()
                .get(&secrets::password_key("new"))
                .expect("get"),
            Some("pw".to_string())
        );

        input.auth_kind = AuthKind::Oauth;
        input.password = None;
        input.oauth_session_id = Some("missing-session".to_string());
        assert!(service.apply_new_account("new2", &input).is_err());
    }

    #[tokio::test]
    async fn import_rejects_foreign_keys() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (service, _) = service(dir.path());
        let mut imported = HashMap::new();
        imported.insert("not-an-account-key".to_string(), "v".to_string());
        assert!(service.import_account_secrets(&imported).is_err());
    }

    #[tokio::test]
    async fn api_token_for_password_account_is_unsupported() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (service, _) = service(dir.path());
        let account = test_account("pw2", AuthKind::Password, None);
        insert_account(&service, &account).await;
        let error = service
            .api_token("pw2", TokenResource::GoogleApis)
            .await
            .expect_err("must fail");
        assert!(error.message().contains("password"));
    }

    #[tokio::test]
    async fn preset_lookup_helper_formats_a_result() {
        let result = preset_for_domain("qq.com").expect("preset");
        assert_eq!(result.provider, ProviderKind::Qq);
        assert_eq!(result.imap.expect("imap").username, "user@qq.com");
    }

    #[tokio::test]
    async fn discovery_uses_preset_offline() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (service, _) = service(dir.path());
        let result = service
            .discover_with("user@icloud.com", false)
            .await
            .expect("discover");
        assert_eq!(result.provider, ProviderKind::Icloud);
        assert_eq!(result.source, "preset");
    }

    #[tokio::test]
    async fn secret_store_supplies_the_google_client_secret() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (service, settings, _) = service_with_settings(dir.path());

        service
            .secrets()
            .set(secrets::SETTINGS_GOOGLE_SECRET_KEY, "store-secret")
            .expect("set secret");

        let client = service
            .resolved_client(OAuthProvider::Google)
            .expect("client");
        assert_eq!(client.client_id, "google-client");
        assert_eq!(client.client_secret.as_deref(), Some("store-secret"));
        // The secret never lands in the settings value.
        assert_eq!(settings.current().advanced.google_client_secret, None);
    }

    #[tokio::test]
    async fn legacy_settings_secret_is_migrated_and_scrubbed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (service, settings, _) = service_with_settings(dir.path());

        settings
            .update(legacy_secret_patch("legacy-secret"))
            .await
            .expect("seed legacy secret");
        assert_eq!(
            settings.current().advanced.google_client_secret.as_deref(),
            Some("legacy-secret")
        );

        service
            .migrate_settings_secret()
            .await
            .expect("migrate secret");

        assert_eq!(settings.current().advanced.google_client_secret, None);
        assert_eq!(
            service
                .secrets()
                .get(secrets::SETTINGS_GOOGLE_SECRET_KEY)
                .expect("get")
                .as_deref(),
            Some("legacy-secret")
        );
        let client = service
            .resolved_client(OAuthProvider::Google)
            .expect("client");
        assert_eq!(client.client_secret.as_deref(), Some("legacy-secret"));

        // Idempotent: a second run keeps the stored secret.
        service
            .migrate_settings_secret()
            .await
            .expect("second migrate");
        assert_eq!(
            service
                .secrets()
                .get(secrets::SETTINGS_GOOGLE_SECRET_KEY)
                .expect("get")
                .as_deref(),
            Some("legacy-secret")
        );
    }

    #[tokio::test]
    async fn resolved_client_adopts_a_legacy_settings_secret() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (service, settings, _) = service_with_settings(dir.path());
        settings
            .update(legacy_secret_patch("runtime-legacy"))
            .await
            .expect("seed legacy secret");

        let client = service
            .resolved_client(OAuthProvider::Google)
            .expect("client");
        assert_eq!(client.client_secret.as_deref(), Some("runtime-legacy"));
        assert_eq!(
            service
                .secrets()
                .get(secrets::SETTINGS_GOOGLE_SECRET_KEY)
                .expect("get")
                .as_deref(),
            Some("runtime-legacy")
        );

        // The plaintext copy is scrubbed in the background.
        for _ in 0..100 {
            if settings.current().advanced.google_client_secret.is_none() {
                break;
            }
            tokio::time::sleep(StdDuration::from_millis(5)).await;
        }
        assert_eq!(settings.current().advanced.google_client_secret, None);
    }

    #[tokio::test]
    async fn setting_and_clearing_the_google_client_secret_stays_in_the_store() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (service, settings, _) = service_with_settings(dir.path());

        service
            .set_google_client_secret(Some("  new-secret  ".to_string()))
            .await
            .expect("set secret");
        assert!(service.has_google_client_secret());
        assert_eq!(
            service
                .secrets()
                .get(secrets::SETTINGS_GOOGLE_SECRET_KEY)
                .expect("get")
                .as_deref(),
            Some("new-secret")
        );
        assert_eq!(settings.current().advanced.google_client_secret, None);
        assert_eq!(
            service
                .resolved_client(OAuthProvider::Google)
                .expect("client")
                .client_secret
                .as_deref(),
            Some("new-secret")
        );

        service
            .set_google_client_secret(Some("   ".to_string()))
            .await
            .expect("clear secret");
        assert!(!service.has_google_client_secret());
        assert_eq!(
            service
                .resolved_client(OAuthProvider::Google)
                .expect("client")
                .client_secret,
            None
        );
    }

    #[tokio::test]
    async fn start_migrates_a_legacy_settings_secret() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (service, settings, _) = service_with_settings(dir.path());
        settings
            .update(legacy_secret_patch("startup-secret"))
            .await
            .expect("seed legacy secret");

        service.start();

        for _ in 0..100 {
            if service
                .secrets()
                .get(secrets::SETTINGS_GOOGLE_SECRET_KEY)
                .expect("get")
                .is_some()
                && settings.current().advanced.google_client_secret.is_none()
            {
                break;
            }
            tokio::time::sleep(StdDuration::from_millis(5)).await;
        }
        assert_eq!(
            service
                .secrets()
                .get(secrets::SETTINGS_GOOGLE_SECRET_KEY)
                .expect("get")
                .as_deref(),
            Some("startup-secret")
        );
        assert_eq!(settings.current().advanced.google_client_secret, None);
    }
}
