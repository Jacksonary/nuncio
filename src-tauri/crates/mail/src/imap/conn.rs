//! IMAP connection establishment and capability probing.
//!
//! One `ImapConnection` wraps a single authenticated `async-imap` session over
//! either a plain TCP stream or a rustls TLS stream. Account actors create at
//! most two of these per account: a serialized worker session and an IDLE
//! session (see `engine::account_actor`).

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_imap::types::Capabilities;
use async_imap::{Authenticator, Client, Session};
use futures::stream::{FuturesUnordered, StreamExt};
use nuncio_core::models::{Account, ProviderCapabilities, ProviderKind, Security, ServerConfig};
use nuncio_core::traits::MailAuth;
use nuncio_core::{AppError, CoreContext, Result};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;
use tracing::{debug, info, warn};

use crate::util::{ensure_crypto_provider, imap_error};

// Bringing up a connection has four distinct phases, and each one fails for
// its own reason: DNS, a TCP connect to one resolved address, the TLS
// handshake, the server greeting, then authentication. They used to share a
// single 30 s budget that was wrapped around `TcpStream::connect` alone, which
// dials the resolved addresses *sequentially* with no per-address limit. One
// blackholed address therefore consumed the entire budget, and the message the
// user saw ("IMAP connect ... timed out", or "TLS handshake timed out" when a
// socket was accepted but never handshook) named whichever phase happened to
// expire first rather than the address that was dead.

/// Budget for resolving the server host name.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);
/// Budget for one TCP connect attempt to a single resolved address.
pub const ADDRESS_TIMEOUT: Duration = Duration::from_secs(5);
/// Budget for the TLS handshake once an address has accepted TCP.
pub const TLS_TIMEOUT: Duration = Duration::from_secs(10);
/// Budget for reading the server greeting (and answering STARTTLS).
pub const GREETING_TIMEOUT: Duration = Duration::from_secs(15);
/// Upper bound for sweeping every resolved address.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(40);
/// Budget for LOGIN / AUTHENTICATE.
pub const AUTH_TIMEOUT: Duration = Duration::from_secs(30);
/// How many resolved addresses are dialled at the same time.
const DIAL_PARALLELISM: usize = 4;

/// Per-phase connection budgets. Each phase owns its timeout so that a failure
/// (and the message the user sees) names the phase that actually stalled.
#[derive(Debug, Clone, Copy)]
pub struct ConnectTimeouts {
    pub resolve: Duration,
    pub address: Duration,
    pub tls: Duration,
    pub greeting: Duration,
    pub dial: Duration,
    pub auth: Duration,
}

impl Default for ConnectTimeouts {
    fn default() -> Self {
        Self {
            resolve: RESOLVE_TIMEOUT,
            address: ADDRESS_TIMEOUT,
            tls: TLS_TIMEOUT,
            greeting: GREETING_TIMEOUT,
            dial: DIAL_TIMEOUT,
            auth: AUTH_TIMEOUT,
        }
    }
}

/// Command timeout for interactive operations (select, list, store, ...).
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// Metadata/body fetches may legitimately take a while on slow servers.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(180);

/// Plain or TLS transport. Kept as a concrete enum so `Session<T>` is nameable
/// and the STARTTLS upgrade path can reuse the raw socket.
pub enum ImapTransport {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl fmt::Debug for ImapTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plain(_) => f.write_str("ImapTransport::Plain"),
            Self::Tls(_) => f.write_str("ImapTransport::Tls"),
        }
    }
}

impl AsyncRead for ImapTransport {
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

impl AsyncWrite for ImapTransport {
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

/// Server extensions we actually branch on, derived from `CAPABILITY`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerCapabilities {
    pub idle: bool,
    pub condstore: bool,
    pub qresync: bool,
    pub r#move: bool,
    pub uidplus: bool,
    pub gmail: bool,
    pub special_use: bool,
    pub xlist: bool,
    pub starttls: bool,
    pub raw: Vec<String>,
}

impl ServerCapabilities {
    fn from_capabilities(caps: &Capabilities) -> Self {
        let mut raw = Vec::new();
        for capability in caps.iter() {
            let name = match capability {
                async_imap::types::Capability::Imap4rev1 => "IMAP4rev1".to_string(),
                async_imap::types::Capability::Auth(mechanism) => format!("AUTH={mechanism}"),
                async_imap::types::Capability::Atom(atom) => atom.clone(),
            };
            raw.push(name);
        }
        raw.sort();
        raw.dedup();
        let has = |name: &str| caps.has_str(name);
        Self {
            idle: has("IDLE"),
            condstore: has("CONDSTORE"),
            qresync: has("QRESYNC"),
            r#move: has("MOVE"),
            uidplus: has("UIDPLUS"),
            gmail: has("X-GM-EXT-1"),
            special_use: has("SPECIAL-USE"),
            xlist: has("XLIST"),
            starttls: has("STARTTLS"),
            raw,
        }
    }

    /// Merge protocol capabilities into the account's persisted flags while
    /// preserving the PIM-side feature flags owned by other crates.
    pub fn merge_into(&self, account: &Account) -> ProviderCapabilities {
        if self.raw.is_empty() {
            return account.capabilities.clone();
        }
        let mut next = account.capabilities.clone();
        next.idle = self.idle;
        next.condstore = self.condstore;
        next.qresync = self.qresync;
        next.r#move = self.r#move;
        next.uidplus = self.uidplus;
        next.gmail_extensions = self.gmail;
        next.special_use = self.special_use;
        next.server_search = true;
        // Gmail always files sent mail automatically; APPENDing again would
        // produce a duplicate in "[Gmail]/Sent Mail".
        next.auto_saves_sent = self.gmail || account.provider == ProviderKind::Gmail;
        next
    }
}

/// An authenticated IMAP session plus its capability snapshot.
pub struct ImapConnection {
    pub session: Session<ImapTransport>,
    pub caps: ServerCapabilities,
    pub host: String,
    /// Last time a command completed (or the session was created). Used to
    /// probe a session that may have been silently closed by the server while
    /// it sat idle (NetEase 163 and some other providers do this well before
    /// two minutes).
    last_activity: Instant,
}

impl fmt::Debug for ImapConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImapConnection")
            .field("host", &self.host)
            .field("caps", &self.caps)
            .field("idle_for_ms", &self.last_activity.elapsed().as_millis())
            .finish()
    }
}

impl ImapConnection {
    /// Record that the session just completed a round trip.
    pub fn touch(&mut self) {
        self.last_activity = Instant::now();
    }

    /// How long the session has been idle since the last completed command.
    pub fn idle_for(&self) -> Duration {
        self.last_activity.elapsed()
    }

    /// Send a cheap NOOP. Used both as an idle probe and as a keepalive on
    /// accounts whose server has no IDLE support.
    pub async fn noop(&mut self) -> Result<()> {
        let session = &mut self.session;
        crate::util::with_timeout(COMMAND_TIMEOUT, "NOOP", async move {
            session.noop().await.map_err(imap_error)
        })
        .await?;
        self.touch();
        Ok(())
    }

    /// Probe a session that has been idle for a while before trusting it with
    /// a real command. A stale socket is detected here (the NOOP fails) so the
    /// caller can reconnect transparently instead of failing the command.
    pub async fn ensure_alive(&mut self, max_idle: Duration) -> Result<()> {
        if self.last_activity.elapsed() >= max_idle {
            self.noop().await?;
        }
        Ok(())
    }

    /// Select a mailbox with a bounded timeout.
    pub async fn select(&mut self, mailbox: &str) -> Result<async_imap::types::Mailbox> {
        let session = &mut self.session;
        crate::util::with_timeout(COMMAND_TIMEOUT, "SELECT", async move {
            session.select(mailbox).await.map_err(imap_error)
        })
        .await
    }

    /// Select a mailbox while requesting the CONDSTORE extension.
    pub async fn select_condstore(&mut self, mailbox: &str) -> Result<async_imap::types::Mailbox> {
        let session = &mut self.session;
        crate::util::with_timeout(COMMAND_TIMEOUT, "SELECT (CONDSTORE)", async move {
            session.select_condstore(mailbox).await.map_err(imap_error)
        })
        .await
    }

    /// Execute a command we do not model with typed helpers and require `OK`.
    ///
    /// Used for extension commands (`ENABLE`, `XLIST`, ...) and for the rare
    /// `UID STORE` with Gmail label syntax. Untagged responses of such a
    /// command are intentionally discarded; callers must use the typed APIs
    /// when they need returned data.
    pub async fn raw_command_ok(&mut self, command: &str) -> Result<()> {
        let session = &mut self.session;
        crate::util::with_timeout(COMMAND_TIMEOUT, "IMAP command", async move {
            session
                .run_command_and_check_ok(command)
                .await
                .map_err(imap_error)
        })
        .await
    }

    /// Append raw RFC822 bytes to a mailbox.
    pub async fn append(&mut self, mailbox: &str, flags: Option<&str>, bytes: &[u8]) -> Result<()> {
        let session = &mut self.session;
        crate::util::with_timeout(FETCH_TIMEOUT, "APPEND", async move {
            session
                .append(mailbox, flags, None, bytes)
                .await
                .map_err(imap_error)
        })
        .await
    }

    /// Orderly LOGOUT; errors are ignored because callers are usually already
    /// tearing a broken connection down.
    pub async fn logout(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(5), self.session.logout()).await;
    }
}

/// SASL PLAIN / XOAUTH2 authenticators. async-imap base64-encodes the returned
/// string for us, so `process` returns the raw SASL response.
struct SaslAuthenticator {
    response: String,
}

impl Authenticator for SaslAuthenticator {
    type Response = String;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        self.response.clone()
    }
}

fn xoauth2_response(username: &str, access_token: &str) -> String {
    format!("user={username}\x01auth=Bearer {access_token}\x01\x01")
}

/// Connect and authenticate using explicit credentials (used by connection
/// tests and by callers that already refreshed the token).
pub async fn connect(config: &ServerConfig, auth: &MailAuth) -> Result<ImapConnection> {
    connect_with_timeouts(config, auth, ConnectTimeouts::default()).await
}

/// [`connect`] with explicit per-phase budgets (tests, and callers that need a
/// tighter deadline than the defaults).
pub async fn connect_with_timeouts(
    config: &ServerConfig,
    auth: &MailAuth,
    timeouts: ConnectTimeouts,
) -> Result<ImapConnection> {
    ensure_crypto_provider();
    // PLAIN/LOGIN and XOAUTH2 must never cross the network unencrypted: a
    // passive attacker could capture passwords or bearer tokens. Plaintext is
    // only accepted for a loopback relay (local development / localhost MTA).
    if config.security == Security::None && !crate::util::is_loopback_host(&config.host) {
        return Err(AppError::invalid_input(
            "IMAP without TLS is only allowed for a local (loopback) relay; \
             choose TLS or STARTTLS for this server",
        ));
    }
    debug!(host = %config.host, port = config.port, security = ?config.security, "connecting to IMAP");

    let addresses = resolve(&config.host, config.port, timeouts.resolve).await?;
    debug!(
        host = %config.host,
        addresses = %format_addresses(&addresses),
        count = addresses.len(),
        "IMAP host resolved"
    );

    let tls_config = tls_client_config()?;
    let host = config.host.clone();
    let security = config.security;
    let dialled = tokio::time::timeout(
        timeouts.dial,
        first_success(&addresses, |address| {
            dial(address, host.clone(), security, tls_config.clone(), timeouts)
        }),
    )
    .await
    .map_err(|_| {
        AppError::network(format!(
            "IMAP connect to {} timed out after {}s across {} resolved addresses",
            config.host,
            timeouts.dial.as_secs(),
            addresses.len()
        ))
    })??;
    let (address, transport) = dialled;
    debug!(host = %config.host, %address, "IMAP transport established");

    let client = match config.security {
        Security::Tls => {
            let mut client = Client::new(transport);
            read_greeting(&mut client, timeouts.greeting).await?;
            client
        }
        Security::Starttls => {
            let tcp = match transport {
                ImapTransport::Plain(tcp) => tcp,
                ImapTransport::Tls(_) => {
                    return Err(AppError::internal(
                        "STARTTLS connection was already encrypted",
                    ))
                }
            };
            let tcp = raw_starttls(tcp, timeouts.greeting).await?;
            let transport = wrap_tls(tcp, &config.host, tls_config.clone(), timeouts).await?;
            // STARTTLS does not emit a second greeting.
            Client::new(transport)
        }
        Security::None => {
            let mut client = Client::new(transport);
            read_greeting(&mut client, timeouts.greeting).await?;
            client
        }
    };

    // Some servers only advertise STARTTLS in the plain greeting; the account
    // configuration is authoritative, and TLS is mandatory for credentials.
    if config.security == Security::None {
        warn!(host = %config.host, "IMAP connection uses no transport security; credentials are sent in clear text");
    }

    let (mut session, login_caps) = match auth {
        MailAuth::Password {
            username: user,
            password,
        } => tokio::time::timeout(timeouts.auth, client.login_with_capabilities(user, password))
            .await
            .map_err(|_| AppError::network("IMAP LOGIN timed out"))?
            .map_err(|(error, _)| imap_error(error))?,
        MailAuth::XOAuth2 {
            username: user,
            access_token,
        } => {
            let session = tokio::time::timeout(
                timeouts.auth,
                client.authenticate(
                    "XOAUTH2",
                    SaslAuthenticator {
                        response: xoauth2_response(user, access_token),
                    },
                ),
            )
            .await
            .map_err(|_| AppError::network("IMAP AUTHENTICATE timed out"))?
            .map_err(|(error, _)| imap_error(error))?;
            (session, None)
        }
    };

    let caps = match session.capabilities().await {
        Ok(caps) => ServerCapabilities::from_capabilities(&caps),
        Err(error) => {
            debug!(%error, "post-login CAPABILITY probe failed");
            login_caps
                .as_ref()
                .map(ServerCapabilities::from_capabilities)
                .unwrap_or_default()
        }
    };

    let mut connection = ImapConnection {
        session,
        caps,
        host: config.host.clone(),
        last_activity: Instant::now(),
    };

    if connection.caps.raw.iter().any(|cap| cap == "ID") {
        let id = [
            ("name", Some("Nuncio")),
            ("version", Some(env!("CARGO_PKG_VERSION"))),
            ("vendor", Some("Nuncio")),
        ];
        match tokio::time::timeout(COMMAND_TIMEOUT, connection.session.id(id)).await {
            Ok(Ok(_)) => debug!("IMAP ID exchanged"),
            Ok(Err(error)) => debug!(%error, "IMAP ID command not accepted"),
            Err(_) => debug!("IMAP ID command timed out"),
        }
    }

    if connection.caps.condstore {
        if let Err(error) = connection.raw_command_ok("ENABLE CONDSTORE").await {
            debug!(%error, "ENABLE CONDSTORE not accepted");
        }
    }

    debug!(
        host = %connection.host,
        idle = connection.caps.idle,
        condstore = connection.caps.condstore,
        gmail = connection.caps.gmail,
        "IMAP session ready"
    );
    info!(host = %connection.host, "IMAP connected");
    Ok(connection)
}

/// Resolve credentials through the core provider and retry once after a
/// reported auth failure (token refresh / password update). The second failure
/// is surfaced as `AppError::Auth`, which callers translate to `needsReauth`.
pub async fn connect_account(ctx: &CoreContext, account: &Account) -> Result<ImapConnection> {
    let config = account
        .imap
        .as_ref()
        .ok_or_else(|| AppError::invalid_input("account has no IMAP server configured"))?;
    let auth = ctx.creds.mail_auth(account).await?;
    match connect(config, &auth).await {
        Ok(connection) => Ok(connection),
        Err(error) if error.code() == "auth" => {
            // The server's own wording is the only thing that distinguishes
            // "this token has no IMAP scope" from "this mailbox is blocked" or
            // "the audience is wrong"; dropping it leaves nothing to diagnose
            // with. The message carries no credential (servers quote the
            // username at most), so it is safe to keep in the log.
            warn!(
                account = %account.id,
                host = %config.host,
                error = %error.message(),
                "IMAP authentication failed; requesting fresh credentials"
            );
            log_token_claims(&account.id, &auth);
            ctx.creds
                .report_auth_failure(&account.id, nuncio_core::traits::TokenResource::Mail)
                .await;
            let refreshed = ctx.creds.mail_auth(account).await?;
            connect(config, &refreshed).await.map_err(|second| {
                if second.code() == "auth" {
                    warn!(
                        account = %account.id,
                        host = %config.host,
                        error = %second.message(),
                        "IMAP rejected the refreshed token"
                    );
                    log_token_claims(&account.id, &refreshed);
                    AppError::auth("IMAP authentication failed after token refresh")
                } else {
                    second
                }
            })
        }
        Err(error) => Err(error),
    }
}

/// Log the claims that decide whether a provider will accept a token.
///
/// Only the *claims* are logged, never the credential itself: `scp` tells us
/// which scopes were actually granted, `aud` and `tid` tell us which resource
/// and tenant the token was issued for. When IMAP rejects a token that Graph
/// accepts, one of those three is the reason — and without them the failure is
/// indistinguishable from a dozen unrelated causes.
fn log_token_claims(account_id: &str, auth: &nuncio_core::traits::MailAuth) {
    let nuncio_core::traits::MailAuth::XOAuth2 { access_token, .. } = auth else {
        return;
    };
    let Some(claims) = decode_jwt_claims(access_token) else {
        debug!(account = %account_id, "the access token is not a JWT; no claims to inspect");
        return;
    };
    let read = |key: &str| {
        claims
            .get(key)
            .and_then(|value| value.as_str())
            .unwrap_or("<absent>")
            .to_string()
    };
    warn!(
        account = %account_id,
        scp = %read("scp"),
        aud = %read("aud"),
        tid = %read("tid"),
        // `ver` distinguishes the v1 and v2 token formats; v1 tokens carry the
        // scopes in a space-separated `scp` too, but some resources only accept
        // a specific generation.
        ver = %read("ver"),
        "access token claims"
    );
}

/// Decode a JWT payload without verifying it.
///
/// The token came from the provider's token endpoint over TLS in response to
/// our own request, so the signature adds nothing here; we only want to read
/// back what was issued. Returns `None` for opaque (non-JWT) tokens.
fn decode_jwt_claims(token: &str) -> Option<serde_json::Value> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Resolve `host:port`, preferring IPv4.
///
/// IPv4 goes first because a dual-stack host whose IPv6 is unroutable (a
/// common home/office network condition: the records resolve, but there is no
/// working default route) would otherwise spend the whole dial budget on
/// addresses that cannot ever work. Every address is still tried, so an
/// IPv6-only host still connects.
pub async fn resolve(host: &str, port: u16, timeout: Duration) -> Result<Vec<SocketAddr>> {
    let (host, bare) = strip_brackets(host);
    // A literal address needs no name resolution.
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let resolved = tokio::time::timeout(timeout, tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| {
            AppError::network(format!(
                "IMAP host name lookup for {host} timed out after {}s",
                timeout.as_secs()
            ))
        })?
        .map_err(|error| AppError::network(format!("cannot resolve IMAP host {host}: {error}")))?;
    Ok(order_addresses(resolved.collect()))
}

fn strip_brackets(host: &str) -> (&str, &str) {
    let trimmed = host.trim();
    match trimmed.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
        Some(bare) => (bare, bare),
        None => (trimmed, trimmed),
    }
}

/// Stable order for a resolved address list: IPv4 first, then IPv6, each
/// keeping the resolver's own preference order and de-duplicated.
pub(crate) fn order_addresses(addresses: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let mut ordered = Vec::with_capacity(addresses.len());
    for v4 in addresses.iter().filter(|address| address.is_ipv4()) {
        if !ordered.contains(v4) {
            ordered.push(*v4);
        }
    }
    for v6 in addresses.iter().filter(|address| address.is_ipv6()) {
        if !ordered.contains(v6) {
            ordered.push(*v6);
        }
    }
    ordered
}

fn format_addresses(addresses: &[SocketAddr]) -> String {
    addresses
        .iter()
        .map(SocketAddr::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Dial every address with bounded parallelism and return the first one that
/// completes the whole phase chain (TCP + TLS) successfully.
///
/// Racing the addresses is what makes a partially blackholed host work: on
/// `outlook.office365.com` this machine resolves eight addresses of which two
/// silently drop TCP SYNs, so a strictly sequential dial burns its per-address
/// budget on the dead ones before ever reaching a live one. Failures are
/// remembered per address so the error the user sees names the addresses that
/// actually failed and why.
async fn first_success<T, F, Fut>(addresses: &[SocketAddr], dial: F) -> Result<(SocketAddr, T)>
where
    F: Fn(SocketAddr) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    if addresses.is_empty() {
        return Err(AppError::network("IMAP host did not resolve to any address"));
    }
    let mut pending = FuturesUnordered::new();
    let mut next = 0usize;
    let mut failures: Vec<String> = Vec::new();

    // Fill the window with address attempts. `dial` is only borrowed while
    // building each future, so the closure itself stays callable.
    let fill = |pending: &mut FuturesUnordered<_>, next: &mut usize| {
        while pending.len() < DIAL_PARALLELISM && *next < addresses.len() {
            let address = addresses[*next];
            *next += 1;
            let attempt = dial(address);
            pending.push(async move { (address, attempt.await) });
        }
    };
    fill(&mut pending, &mut next);

    while let Some((address, outcome)) = pending.next().await {
        match outcome {
            Ok(value) => return Ok((address, value)),
            Err(error) => {
                debug!(%address, %error, "IMAP address attempt failed");
                failures.push(format!("{address}: {}", error.message()));
            }
        }
        fill(&mut pending, &mut next);
    }

    Err(AppError::network(format!(
        "IMAP connect failed for every resolved address ({})",
        failures.join("; ")
    )))
}

/// Establish a transport to one address: TCP connect, then (for TLS) the
/// handshake.
async fn dial(
    address: SocketAddr,
    host: String,
    security: Security,
    tls_config: Arc<rustls::ClientConfig>,
    timeouts: ConnectTimeouts,
) -> Result<ImapTransport> {
    let tcp = connect_address(address, timeouts.address).await?;
    let transport = match security {
        Security::Tls => wrap_tls(tcp, &host, tls_config, timeouts).await?,
        Security::Starttls | Security::None => ImapTransport::Plain(tcp),
    };
    Ok(transport)
}

/// TCP connect to exactly one address with its own budget.
async fn connect_address(address: SocketAddr, timeout: Duration) -> Result<TcpStream> {
    let stream = tokio::time::timeout(timeout, TcpStream::connect(address))
        .await
        .map_err(|_| {
            AppError::network(format!(
                "TCP connect to {address} timed out after {}s",
                timeout.as_secs()
            ))
        })?
        .map_err(|error| AppError::network(format!("TCP connect to {address} failed: {error}")))?;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

/// Perform the plaintext part of STARTTLS directly on the socket: read the
/// greeting, issue `STARTTLS`, and consume responses until the tagged OK.
/// async-imap does not expose a typed STARTTLS upgrade for `Client`.
async fn raw_starttls(tcp: TcpStream, timeout: Duration) -> Result<TcpStream> {
    let mut reader = BufReader::new(tcp);
    let greeting = read_socket_line(&mut reader, "greeting", timeout).await?;
    if !greeting.starts_with("* OK") && !greeting.starts_with("+OK") {
        return Err(AppError::protocol(format!(
            "IMAP server rejected the connection: {greeting}"
        )));
    }
    let mut tcp = reader.into_inner();
    tcp.write_all(b"nuncio-starttls STARTTLS\r\n")
        .await
        .map_err(|error| AppError::network(format!("STARTTLS write failed: {error}")))?;
    tcp.flush()
        .await
        .map_err(|error| AppError::network(format!("STARTTLS flush failed: {error}")))?;
    let mut reader = BufReader::new(tcp);
    loop {
        let line = read_socket_line(&mut reader, "STARTTLS response", timeout).await?;
        if line.starts_with("nuncio-starttls ") {
            return if line.contains(" OK") {
                Ok(reader.into_inner())
            } else {
                Err(AppError::protocol(format!(
                    "IMAP server rejected STARTTLS: {line}"
                )))
            };
        }
    }
}

async fn read_socket_line<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    what: &str,
    timeout: Duration,
) -> Result<String> {
    let mut line = String::new();
    tokio::time::timeout(timeout, reader.read_line(&mut line))
        .await
        .map_err(|_| {
            AppError::network(format!(
                "IMAP {what} timed out after {}s",
                timeout.as_secs()
            ))
        })?
        .map_err(|error| AppError::network(format!("IMAP {what} read failed: {error}")))?;
    if line.is_empty() {
        return Err(AppError::network("IMAP server closed the connection"));
    }
    Ok(line.trim_end().to_string())
}

async fn read_greeting(client: &mut Client<ImapTransport>, timeout: Duration) -> Result<()> {
    match tokio::time::timeout(timeout, client.read_response()).await {
        Ok(Ok(Some(_))) => Ok(()),
        Ok(Ok(None)) => Err(AppError::network("IMAP server closed the connection")),
        Ok(Err(error)) => Err(AppError::network(format!("IMAP greeting failed: {error}"))),
        Err(_) => Err(AppError::network(format!(
            "IMAP greeting timed out after {}s",
            timeout.as_secs()
        ))),
    }
}

async fn wrap_tls(
    tcp: TcpStream,
    host: &str,
    config: Arc<rustls::ClientConfig>,
    timeouts: ConnectTimeouts,
) -> Result<ImapTransport> {
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| AppError::invalid_input(format!("invalid IMAP host name: {host}")))?;
    let connector = TlsConnector::from(config);
    let stream = tokio::time::timeout(timeouts.tls, connector.connect(server_name, tcp))
        .await
        .map_err(|_| {
            AppError::network(format!(
                "TLS handshake with {host} timed out after {}s",
                timeouts.tls.as_secs()
            ))
        })?
        .map_err(|error| AppError::network(format!("TLS handshake failed: {error}")))?;
    Ok(ImapTransport::Tls(Box::new(stream)))
}

/// Build a rustls client config using the OS trust store, falling back to the
/// compiled-in Mozilla roots when the platform verifier is unavailable.
fn tls_client_config() -> Result<Arc<rustls::ClientConfig>> {
    use rustls_platform_verifier::BuilderVerifierExt;

    let config = match rustls::ClientConfig::builder().with_platform_verifier() {
        Ok(builder) => builder.with_no_client_auth(),
        Err(error) => {
            warn!(%error, "platform certificate verifier unavailable; using webpki roots");
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth()
        }
    };
    Ok(Arc::new(config))
}

/// Best-effort connectivity probe used by `account_test`; returns the server
/// capability snapshot on success.
pub async fn test_config(config: &ServerConfig, auth: &MailAuth) -> Result<ServerCapabilities> {
    let connection = connect(config, auth).await?;
    let caps = connection.caps.clone();
    connection.logout().await;
    Ok(caps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn oauth2_sasl_string_is_rfc_shaped() {
        let value = xoauth2_response("me@example.com", "token");
        assert_eq!(value, "user=me@example.com\u{1}auth=Bearer token\u{1}\u{1}");
    }

    fn v4(last: u8) -> SocketAddr {
        SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(40, 104, 69, last)), 993)
    }

    fn v6(last: u16) -> SocketAddr {
        SocketAddr::new(
            std::net::IpAddr::V6(Ipv6Addr::new(0x2603, 0x1046, 0xc07, 0xd17, 0, 0, 0, last)),
            993,
        )
    }

    #[test]
    fn addresses_are_ordered_ipv4_first_and_deduplicated() {
        // Apple's resolver hands back IPv6 first; the dialer must not spend its
        // per-address budgets there when an IPv4 route exists.
        let ordered = order_addresses(vec![v6(1), v6(2), v4(10), v6(1), v4(11)]);
        assert_eq!(ordered, vec![v4(10), v4(11), v6(1), v6(2)]);
    }

    #[test]
    fn ipv6_only_hosts_keep_every_address() {
        let ordered = order_addresses(vec![v6(3), v6(1)]);
        assert_eq!(ordered, vec![v6(3), v6(1)]);
    }

    #[tokio::test]
    async fn literal_addresses_skip_name_resolution() {
        let resolved = resolve("127.0.0.1", 143, Duration::from_millis(1))
            .await
            .expect("literal host resolves");
        assert_eq!(
            resolved,
            vec![SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 143)]
        );
        let bracketed = resolve("[::1]", 143, Duration::from_millis(1))
            .await
            .expect("bracketed literal resolves");
        assert_eq!(
            bracketed,
            vec![SocketAddr::new(std::net::IpAddr::V6(Ipv6Addr::LOCALHOST), 143)]
        );
    }

    #[tokio::test]
    async fn first_success_skips_unreachable_addresses_in_order() {
        // The first address never answers, the second succeeds: the dialer must
        // return the second instead of burning its whole budget on the first.
        let addresses = vec![v4(1), v4(2), v4(3)];
        let (address, value) = first_success(&addresses, |address| async move {
            if address == v4(1) {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Err(AppError::network("TCP connect timed out"))
            } else {
                Ok(address.port())
            }
        })
        .await
        .expect("one address answers");
        assert_eq!(address, v4(2));
        assert_eq!(value, 993);
    }

    #[tokio::test]
    async fn first_success_reports_every_address_on_total_failure() {
        let addresses = vec![v4(1), v4(2)];
        let error = first_success(&addresses, |address| async move {
            Err::<(), _>(AppError::network(format!("TCP connect to {address} timed out")))
        })
        .await
        .expect_err("no address answers");
        let message = error.message();
        assert!(message.contains(&v4(1).to_string()), "{message}");
        assert!(message.contains(&v4(2).to_string()), "{message}");
        assert!(message.contains("timed out"), "{message}");
    }

    #[tokio::test]
    async fn first_success_rejects_an_empty_address_list() {
        let error = first_success(&[] as &[SocketAddr], |address| async move {
            Ok::<_, AppError>(address)
        })
        .await
        .expect_err("nothing to dial");
        assert_eq!(error.code(), "network");
    }

    #[tokio::test]
    async fn dial_window_never_exceeds_parallelism() {
        let live = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let addresses: Vec<SocketAddr> = (1..=16).map(v4).collect();
        let (address, _) = first_success(&addresses, |address| {
            let live = &live;
            let peak = &peak;
            async move {
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                live.fetch_sub(1, Ordering::SeqCst);
                if address == v4(16) {
                    Ok(())
                } else {
                    Err(AppError::network("nope"))
                }
            }
        })
        .await
        .expect("the last address answers");
        assert_eq!(address, v4(16));
        assert!(peak.load(Ordering::SeqCst) <= DIAL_PARALLELISM);
        assert!(peak.load(Ordering::SeqCst) > 1, "dialer should race addresses");
    }

    #[tokio::test]
    async fn tcp_phase_timeout_names_the_address() {
        // 203.0.113.0/24 is TEST-NET-3: guaranteed not to answer.
        let address: SocketAddr = "203.0.113.7:993".parse().expect("addr");
        let error = connect_address(address, Duration::from_millis(50))
            .await
            .expect_err("TEST-NET address cannot connect");
        assert_eq!(error.code(), "network");
        assert!(
            error.message().contains("TCP connect"),
            "expected a TCP-phase message, got {}",
            error.message()
        );
        assert!(error.message().contains("203.0.113.7"), "{}", error.message());
    }

    #[tokio::test]
    async fn greeting_phase_timeout_is_reported_as_greeting() {
        // A socket that accepts TCP but never sends anything stands in for a
        // TLS endpoint that accepts and then blackholes the handshake.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        let noisy = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                // Hold the connection open without writing a greeting.
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(stream);
            }
        });
        let stream = connect_address(address, Duration::from_secs(1))
            .await
            .expect("loopback connect");
        let mut client = Client::new(ImapTransport::Plain(stream));
        let error = read_greeting(&mut client, Duration::from_millis(100))
            .await
            .expect_err("no greeting arrives");
        assert!(
            error.message().contains("IMAP greeting timed out"),
            "{}",
            error.message()
        );
        noisy.abort();
    }

    #[tokio::test]
    async fn tls_phase_timeout_is_reported_as_tls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        let silent = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(stream);
            }
        });
        let stream = connect_address(address, Duration::from_secs(1))
            .await
            .expect("loopback connect");
        let timeouts = ConnectTimeouts {
            tls: Duration::from_millis(100),
            ..ConnectTimeouts::default()
        };
        let error = wrap_tls(stream, "localhost", tls_client_config().expect("tls config"), timeouts)
            .await
            .expect_err("no handshake completes");
        assert!(
            error.message().contains("TLS handshake with localhost timed out"),
            "{}",
            error.message()
        );
        silent.abort();
    }
}
