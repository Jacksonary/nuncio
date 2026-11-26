//! IMAP connection establishment, capability probing and command timeouts.
//!
//! One `ImapConnection` wraps a single authenticated `async-imap` session over
//! either a plain TCP stream or a rustls TLS stream. Account actors create at
//! most two of these per account: a serialized worker session and an IDLE
//! session (see `engine::account_actor`).
//!
//! Every command is bounded twice:
//!
//! * [`ImapConnection::with_session`] applies the command budget to the whole
//!   write + read-to-tagged-completion sequence;
//! * [`TimeoutStream`] bounds socket-read and socket-write *inactivity*, so a
//!   half-open socket cannot leave a poll pending forever.
//!
//! Either expiry poisons the connection (the response stream is left
//! half-consumed and cannot be resynchronised). The worker therefore drops and
//! rebuilds a poisoned connection instead of issuing another command on it.
//! IDLE is the one waiting command: it suspends the read watchdog and is
//! instead bounded by its own 25 minute re-issue interval (see
//! `run_idle_session`), with only its start/stop handshake under the command
//! budget.

use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_imap::imap_proto::rfc4315::UidSetMember;
use async_imap::imap_proto::{MailboxDatum, Response, ResponseCode, Status};
use async_imap::types::{Capabilities, Fetch, Mailbox, Name};
use async_imap::{Authenticator, Client, Session};
use futures::stream::{FuturesUnordered, StreamExt, TryStreamExt};
use nuncio_core::models::{Account, ProviderCapabilities, Security, ServerConfig};
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
    /// Budget for one interactive command: writing the command and reading its
    /// tagged completion. An expired budget invalidates the whole connection.
    pub command: Duration,
    /// Budget for one metadata/body `FETCH`/`SEARCH`, which may legitimately
    /// take longer than an interactive command on slow servers.
    pub fetch: Duration,
    /// Per-poll inactivity limit for socket reads. This is a backstop for
    /// paths that are not wrapped in a command budget (and for a half-open
    /// socket that never wakes the command future). IDLE suspends it.
    pub io_read: Duration,
    /// Per-poll inactivity limit for socket writes.
    pub io_write: Duration,
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
            command: COMMAND_TIMEOUT,
            fetch: FETCH_TIMEOUT,
            io_read: IO_READ_TIMEOUT,
            io_write: IO_WRITE_TIMEOUT,
        }
    }
}

/// Command timeout for interactive operations (select, list, store, ...).
///
/// Covers the command write *and* reading its tagged completion. A timeout
/// poisons the connection: a half-read response can never be resynchronised
/// with the next command, so the worker has to reconnect.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// Metadata/body fetches and searches may legitimately take a while on slow
/// servers.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(180);
/// Socket read inactivity limit. Backstop for reads that no command budget
/// wraps (IDLE deliberately suspends it and uses its own 25 minute wait).
pub const IO_READ_TIMEOUT: Duration = Duration::from_secs(180);
/// Socket write inactivity limit.
pub const IO_WRITE_TIMEOUT: Duration = Duration::from_secs(60);

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

fn timeout_millis(timeout: Duration) -> u64 {
    if timeout.is_zero() {
        0
    } else {
        timeout.as_millis().max(1) as u64
    }
}

/// Shared state between a connection's stream wrapper and its owner.
///
/// The stream uses the read/write limits to bound a single `poll_read` /
/// `poll_write` (an inactivity watchdog, not a total transfer deadline). The
/// owner uses [`Self::poison`] to record that a command timed out and the
/// response stream can no longer be trusted; a poisoned connection must be
/// dropped and rebuilt, never reused.
#[derive(Debug)]
pub(crate) struct TransportControl {
    read_timeout_ms: AtomicU64,
    write_timeout_ms: AtomicU64,
    /// Configured read limit, restored when a suspended read timeout resumes.
    armed_read_ms: AtomicU64,
    poisoned: AtomicBool,
}

impl TransportControl {
    fn new(read: Duration, write: Duration) -> Arc<Self> {
        let read_ms = timeout_millis(read);
        let write_ms = timeout_millis(write);
        Arc::new(Self {
            read_timeout_ms: AtomicU64::new(read_ms),
            write_timeout_ms: AtomicU64::new(write_ms),
            armed_read_ms: AtomicU64::new(read_ms),
            poisoned: AtomicBool::new(false),
        })
    }

    fn read_timeout(&self) -> Option<Duration> {
        match self.read_timeout_ms.load(Ordering::SeqCst) {
            0 => None,
            millis => Some(Duration::from_millis(millis)),
        }
    }

    fn write_timeout(&self) -> Option<Duration> {
        match self.write_timeout_ms.load(Ordering::SeqCst) {
            0 => None,
            millis => Some(Duration::from_millis(millis)),
        }
    }

    /// Stop applying the read watchdog. Used by IDLE, whose read legitimately
    /// waits for up to 25 minutes without a byte arriving.
    pub(crate) fn suspend_read_timeout(&self) {
        self.read_timeout_ms.store(0, Ordering::SeqCst);
    }

    /// Restore the read watchdog configured at connect time.
    pub(crate) fn resume_read_timeout(&self) {
        let armed = self.armed_read_ms.load(Ordering::SeqCst);
        self.read_timeout_ms.store(armed, Ordering::SeqCst);
    }

    pub(crate) fn poison(&self) {
        self.poisoned.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::SeqCst)
    }
}

/// Transport wrapper that bounds socket inactivity.
///
/// Each `poll_read`/`poll_write` arms a fresh watchdog; a completed poll
/// disarms it. A poll that stays pending past the limit poisons the
/// connection and returns `TimedOut`, so a half-open socket can never hang a
/// worker forever. The watchdog measures inactivity, so a slow but progressing
/// transfer is not interrupted.
pub struct TimeoutStream {
    inner: ImapTransport,
    control: Arc<TransportControl>,
    read_deadline: Pin<Box<tokio::time::Sleep>>,
    read_armed: bool,
    write_deadline: Pin<Box<tokio::time::Sleep>>,
    write_armed: bool,
}

impl fmt::Debug for TimeoutStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TimeoutStream")
            .field("inner", &self.inner)
            .field("poisoned", &self.control.is_poisoned())
            .finish()
    }
}

impl TimeoutStream {
    fn new(inner: ImapTransport, control: Arc<TransportControl>) -> Self {
        Self {
            inner,
            control,
            read_deadline: Box::pin(tokio::time::sleep(Duration::ZERO)),
            read_armed: false,
            write_deadline: Box::pin(tokio::time::sleep(Duration::ZERO)),
            write_armed: false,
        }
    }

    fn into_inner(self) -> ImapTransport {
        self.inner
    }
}

impl AsyncRead for TimeoutStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.control.read_timeout() {
            None => this.read_armed = false,
            Some(timeout) if !this.read_armed => {
                this.read_deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + timeout);
                this.read_armed = true;
            }
            Some(_) => {}
        }
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(result) => {
                this.read_armed = false;
                Poll::Ready(result)
            }
            Poll::Pending if this.read_armed => {
                if this.read_deadline.as_mut().poll(cx).is_ready() {
                    this.read_armed = false;
                    this.control.poison();
                    let timeout = this.control.read_timeout().unwrap_or_default();
                    Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "IMAP read timeout: no data for {}s; connection abandoned",
                            timeout.as_secs()
                        ),
                    )))
                } else {
                    Poll::Pending
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for TimeoutStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match this.control.write_timeout() {
            None => this.write_armed = false,
            Some(timeout) if !this.write_armed => {
                this.write_deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + timeout);
                this.write_armed = true;
            }
            Some(_) => {}
        }
        match Pin::new(&mut this.inner).poll_write(cx, buf) {
            Poll::Ready(result) => {
                this.write_armed = false;
                Poll::Ready(result)
            }
            Poll::Pending if this.write_armed => {
                if this.write_deadline.as_mut().poll(cx).is_ready() {
                    this.write_armed = false;
                    this.control.poison();
                    let timeout = this.control.write_timeout().unwrap_or_default();
                    Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "IMAP write timeout: no progress for {}s; connection abandoned",
                            timeout.as_secs()
                        ),
                    )))
                } else {
                    Poll::Pending
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.control.write_timeout() {
            None => this.write_armed = false,
            Some(timeout) if !this.write_armed => {
                this.write_deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + timeout);
                this.write_armed = true;
            }
            Some(_) => {}
        }
        match Pin::new(&mut this.inner).poll_flush(cx) {
            Poll::Ready(result) => {
                this.write_armed = false;
                Poll::Ready(result)
            }
            Poll::Pending if this.write_armed => {
                if this.write_deadline.as_mut().poll(cx).is_ready() {
                    this.write_armed = false;
                    this.control.poison();
                    let timeout = this.control.write_timeout().unwrap_or_default();
                    Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "IMAP flush timeout: no progress for {}s; connection abandoned",
                            timeout.as_secs()
                        ),
                    )))
                } else {
                    Poll::Pending
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

/// The authenticated session type used by [`ImapConnection`].
pub type ImapSession = Session<TimeoutStream>;

/// Server extensions we actually branch on, derived from `CAPABILITY`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerCapabilities {
    pub idle: bool,
    pub condstore: bool,
    pub qresync: bool,
    pub r#move: bool,
    pub uidplus: bool,
    /// IMAP4rev2 (RFC 9051) advertised by the server.
    ///
    /// Recorded for diagnostics only. This client never sends
    /// `ENABLE IMAP4rev2` and always keeps a dual-revision session in rev1:
    /// RFC 9051 §6.4.4 replaces `SEARCH` with `ESEARCH`, which
    /// `imap-proto`/`async-imap` 0.12 do not parse, and the sync engine relies
    /// on `UID SEARCH ALL`. Enabling rev2 would therefore make search fail.
    /// Consequently rev2 supplies no capability this build can use.
    pub imap4rev2: bool,
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
        Self::from_raw(raw)
    }

    /// Snapshot the capability atoms. `async-imap` has no dedicated variant
    /// for rev2, and capability atoms are case-insensitive (RFC 9051
    /// §6.1.2), so matching is done on the raw names.
    fn from_raw(raw: Vec<String>) -> Self {
        let has = |name: &str| raw.iter().any(|item| item.eq_ignore_ascii_case(name));
        let imap4rev2 = raw
            .iter()
            .any(|name| name.eq_ignore_ascii_case("IMAP4rev2"));
        Self {
            idle: has("IDLE"),
            condstore: has("CONDSTORE"),
            qresync: has("QRESYNC"),
            r#move: has("MOVE"),
            uidplus: has("UIDPLUS"),
            imap4rev2,
            gmail: has("X-GM-EXT-1"),
            special_use: has("SPECIAL-USE"),
            xlist: has("XLIST"),
            starttls: has("STARTTLS"),
            raw,
        }
    }

    /// Whether the server can remove exactly the selected messages.
    ///
    /// Only the UIDPLUS extension (RFC 4315) is trusted: it is advertised
    /// independently of the protocol revision and provides `UID EXPUNGE`.
    /// Without it there is no safe way to act on a `\Deleted` marker, because a
    /// plain `EXPUNGE` removes every flagged message in the mailbox, including
    /// ones another client flagged but did not intend to remove.
    ///
    /// IMAP4rev2 also includes `UID EXPUNGE` in its base protocol (RFC 9051
    /// §6.4.9), but this client never enables rev2 (see
    /// [`Self::imap4rev2`]), so advertising it is not evidence that a
    /// rev2-only command is available.
    pub fn directed_expunge(&self) -> bool {
        self.uidplus
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
        // A successful CAPABILITY exchange says nothing about whether text
        // SEARCH works: QQ and NetEase accept the command but return no hits,
        // and 139 ignores the criteria (it returns every message). The
        // directory is the only source for that, so never advertise search
        // just because the server answered.
        next.server_search = nuncio_core::providers::server_text_search(account);
        // Yahoo and AOL file sent mail themselves too; APPENDing as well would
        // put two copies in Sent. `server_files_sent` also honours the
        // per-account `sent_copy` choice, so this must not overwrite it with a
        // provider-only guess. Compute against the freshly probed flags: the
        // X-GM-EXT-1 capability is live evidence, while the persisted account
        // may predate this connection.
        let probed = Account {
            capabilities: next.clone(),
            ..account.clone()
        };
        next.auto_saves_sent = nuncio_core::providers::server_files_sent(&probed);
        next
    }
}

/// An authenticated IMAP session plus its capability snapshot.
pub struct ImapConnection {
    pub(crate) session: ImapSession,
    pub caps: ServerCapabilities,
    pub host: String,
    /// Last time a command completed (or the session was created). Used to
    /// probe a session that may have been silently closed by the server while
    /// it sat idle (NetEase 163 and some other providers do this well before
    /// two minutes).
    last_activity: Instant,
    /// Shared with the stream wrapper: poisoned after any command or socket
    /// timeout so the connection can never be reused mid-response.
    control: Arc<TransportControl>,
    command_timeout: Duration,
    fetch_timeout: Duration,
}

impl fmt::Debug for ImapConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImapConnection")
            .field("host", &self.host)
            .field("caps", &self.caps)
            .field("idle_for_ms", &self.last_activity.elapsed().as_millis())
            .field("poisoned", &self.is_poisoned())
            .finish()
    }
}

/// A command timeout invalidates the connection: the response stream is in an
/// unknown position after the future was dropped, so the next command would
/// read a stale tail as its own response.
fn command_timeout_error(what: &str, budget: Duration) -> AppError {
    AppError::network(format!(
        "{}: {what} timed out after {:.1}s; connection marked unusable",
        crate::util::COMMAND_TIMEOUT_MARKER,
        budget.as_secs_f64()
    ))
}

fn poisoned_reuse_error(what: &str) -> AppError {
    AppError::network(format!(
        "{}: {what} refused: the connection was left unusable by an earlier timeout",
        crate::util::COMMAND_TIMEOUT_MARKER
    ))
}

/// Return `future`'s result, poisoning `control` if the budget expires.
async fn timeout_or_poison<T, F>(
    control: &TransportControl,
    budget: Duration,
    what: &str,
    future: F,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    timeout_at_or_poison(control, Instant::now() + budget, budget, what, future).await
}

/// Return `future`'s result, poisoning `control` once `deadline` has passed.
///
/// Unlike [`timeout_or_poison`], the deadline is absolute: a caller that loops
/// over responses passes the same deadline on every iteration, so unsolicited
/// traffic that arrives promptly can never renew the total budget. The
/// deadline check is explicit because `tokio::time::timeout` with a zero
/// duration still polls its future once, which would let a buffered response
/// satisfy an already-expired stage.
async fn timeout_at_or_poison<T, F>(
    control: &TransportControl,
    deadline: Instant,
    budget: Duration,
    what: &str,
    future: F,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        control.poison();
        return Err(command_timeout_error(what, budget));
    }
    match tokio::time::timeout(remaining, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => {
            if control.is_poisoned() && !crate::util::is_command_timeout(&error) {
                Err(AppError::network(format!(
                    "{}: {what} aborted by a socket timeout: {}",
                    crate::util::COMMAND_TIMEOUT_MARKER,
                    error.message()
                )))
            } else {
                Err(error)
            }
        }
        Err(_) => {
            control.poison();
            Err(command_timeout_error(what, budget))
        }
    }
}

/// UID assigned by the server to an APPEND (RFC 4315 `APPENDUID`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendedUid {
    pub uid: i64,
    pub uidvalidity: i64,
}

/// The UID of a single-message `APPENDUID`.
///
/// A single-message APPEND must yield exactly one UID, spelled either as a
/// plain number or a one-element range. Anything else (several members, a
/// multi-value range, or zero) is not a valid binding for the projection, so
/// it is refused and the caller falls back to "appended, not yet bound"
/// instead of guessing which UID belongs to this copy.
fn first_uid(uidvalidity: u32, members: &[UidSetMember]) -> Option<AppendedUid> {
    let single = match members {
        [member] => member,
        _ => return None,
    };
    let uid = match single {
        UidSetMember::Uid(uid) => i64::from(*uid),
        UidSetMember::UidRange(range) if range.start() == range.end() => i64::from(*range.start()),
        UidSetMember::UidRange(_) => return None,
    };
    if uid <= 0 {
        return None;
    }
    Some(AppendedUid {
        uid,
        uidvalidity: i64::from(uidvalidity),
    })
}

/// Failure from [`ImapConnection::append_with_uid`], split by whether the
/// message literal was already handed to the transport.
///
/// A caller that persists a retry marker must know this: once `sent` is true
/// the server may have committed the APPEND even though the tagged response
/// was lost, so the next attempt has to verify before appending again.
#[derive(Debug)]
pub struct AppendError {
    /// The RFC822 literal (or part of it) was written to the socket.
    pub sent: bool,
    /// The server answered the APPEND with a tagged `NO`/`BAD` *after*
    /// receiving the literal. Unlike a lost completion this proves the server
    /// did not commit the message, so a retry may APPEND again.
    pub rejected: bool,
    pub error: AppError,
}

impl AppendError {
    fn new(sent: bool, error: AppError) -> Self {
        Self {
            sent,
            rejected: false,
            error,
        }
    }

    fn rejected(sent: bool, error: AppError) -> Self {
        Self {
            sent,
            rejected: true,
            error,
        }
    }

    /// Drop the phase information for callers that only need the error.
    pub fn into_error(self) -> AppError {
        self.error
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

    /// Whether a command or socket timeout has invalidated this connection.
    pub fn is_poisoned(&self) -> bool {
        self.control.is_poisoned()
    }

    /// Budget for one interactive command.
    pub fn command_timeout(&self) -> Duration {
        self.command_timeout
    }

    /// Budget for one metadata/body FETCH or SEARCH.
    pub fn fetch_timeout(&self) -> Duration {
        self.fetch_timeout
    }

    /// Override the interactive command budget (tests, or a caller that knows
    /// the server is slow).
    pub fn set_command_timeout(&mut self, timeout: Duration) {
        self.command_timeout = timeout;
    }

    /// Override the metadata/body budget.
    pub fn set_fetch_timeout(&mut self, timeout: Duration) {
        self.fetch_timeout = timeout;
    }

    pub(crate) fn timeout_control(&self) -> Arc<TransportControl> {
        self.control.clone()
    }

    /// Run one protocol command under a hard budget.
    ///
    /// The budget covers both writing the command and reading its tagged
    /// completion. Expiry poisons the connection because the response stream
    /// is left half-consumed; the caller (account worker) must rebuild it. A
    /// socket-level timeout raised by [`TimeoutStream`] also poisons the
    /// connection and is surfaced as the same retryable network error.
    pub async fn with_session<T, F>(&mut self, budget: Duration, what: &str, f: F) -> Result<T>
    where
        F: for<'a> FnOnce(&'a mut ImapSession) -> futures::future::BoxFuture<'a, Result<T>>,
    {
        if self.is_poisoned() {
            return Err(poisoned_reuse_error(what));
        }
        let control = self.control.clone();
        match tokio::time::timeout(budget, f(&mut self.session)).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => {
                if control.is_poisoned() && !crate::util::is_command_timeout(&error) {
                    // An IO watchdog fired but the caller mapped the io error to
                    // a different shape; keep the operation retryable and name
                    // the poisoned connection.
                    Err(AppError::network(format!(
                        "{}: {what} aborted by a socket timeout: {}",
                        crate::util::COMMAND_TIMEOUT_MARKER,
                        error.message()
                    )))
                } else {
                    Err(error)
                }
            }
            Err(_) => {
                control.poison();
                Err(command_timeout_error(what, budget))
            }
        }
    }

    /// Send a cheap NOOP. Used both as an idle probe and as a keepalive on
    /// accounts whose server has no IDLE support.
    pub async fn noop(&mut self) -> Result<()> {
        let budget = self.command_timeout;
        self.with_session(budget, "NOOP", |session| {
            Box::pin(async move { session.noop().await.map_err(imap_error) })
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
    pub async fn select(&mut self, mailbox: &str) -> Result<Mailbox> {
        let budget = self.command_timeout;
        let mailbox = mailbox.to_string();
        self.with_session(budget, "SELECT", move |session| {
            Box::pin(async move { session.select(&mailbox).await.map_err(imap_error) })
        })
        .await
    }

    /// Select a mailbox while requesting the CONDSTORE extension.
    pub async fn select_condstore(&mut self, mailbox: &str) -> Result<Mailbox> {
        let budget = self.command_timeout;
        let mailbox = mailbox.to_string();
        self.with_session(budget, "SELECT (CONDSTORE)", move |session| {
            Box::pin(async move { session.select_condstore(&mailbox).await.map_err(imap_error) })
        })
        .await
    }

    /// `EXAMINE` a mailbox without changing its `\Seen` flags.
    pub async fn examine(&mut self, mailbox: &str) -> Result<Mailbox> {
        let budget = self.command_timeout;
        let mailbox = mailbox.to_string();
        self.with_session(budget, "EXAMINE", move |session| {
            Box::pin(async move { session.examine(&mailbox).await.map_err(imap_error) })
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
        let budget = self.command_timeout;
        let command = command.to_string();
        self.with_session(budget, "IMAP command", move |session| {
            Box::pin(async move {
                session
                    .run_command_and_check_ok(&command)
                    .await
                    .map_err(imap_error)
            })
        })
        .await
    }

    /// Run a typed `UID SEARCH`, collecting every matching UID.
    pub async fn uid_search(&mut self, criteria: &str) -> Result<HashSet<u32>> {
        let budget = self.fetch_timeout;
        let criteria = criteria.to_string();
        self.with_session(budget, "UID SEARCH", move |session| {
            Box::pin(async move {
                session
                    .uid_search(criteria)
                    .await
                    .map(|set| set.into_iter().collect())
                    .map_err(imap_error)
            })
        })
        .await
    }

    /// Run one `UID FETCH` and collect its complete response.
    pub async fn uid_fetch_all(&mut self, set: &str, query: &str) -> Result<Vec<Fetch>> {
        let budget = self.fetch_timeout;
        let set = set.to_string();
        let query = query.to_string();
        self.with_session(budget, "UID FETCH", move |session| {
            Box::pin(async move {
                let stream = session.uid_fetch(&set, &query).await.map_err(imap_error)?;
                stream.try_collect::<Vec<Fetch>>().await.map_err(imap_error)
            })
        })
        .await
    }

    /// `LIST "" "*"` and collect every mailbox record.
    pub async fn list_all(&mut self) -> Result<Vec<Name>> {
        let budget = self.command_timeout;
        self.with_session(budget, "LIST", |session| {
            Box::pin(async move {
                let mut stream = session
                    .list(Some(""), Some("\"*\""))
                    .await
                    .map_err(imap_error)?;
                let mut names = Vec::new();
                while let Some(name) = stream.try_next().await.map_err(imap_error)? {
                    names.push(name);
                }
                Ok(names)
            })
        })
        .await
    }

    /// `CREATE` a mailbox.
    pub async fn create(&mut self, mailbox: &str) -> Result<()> {
        let budget = self.command_timeout;
        let mailbox = mailbox.to_string();
        self.with_session(budget, "CREATE", move |session| {
            Box::pin(async move { session.create(&mailbox).await.map_err(imap_error) })
        })
        .await
    }

    /// `DELETE` a mailbox.
    pub async fn delete(&mut self, mailbox: &str) -> Result<()> {
        let budget = self.command_timeout;
        let mailbox = mailbox.to_string();
        self.with_session(budget, "DELETE", move |session| {
            Box::pin(async move { session.delete(&mailbox).await.map_err(imap_error) })
        })
        .await
    }

    /// `RENAME` a mailbox.
    pub async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let budget = self.command_timeout;
        let from = from.to_string();
        let to = to.to_string();
        self.with_session(budget, "RENAME", move |session| {
            Box::pin(async move {
                session
                    .rename(&from, &to)
                    .await
                    .map_err(imap_error)
            })
        })
        .await
    }

    /// `SUBSCRIBE`/`UNSUBSCRIBE` a mailbox.
    pub async fn subscribe(&mut self, mailbox: &str, subscribe: bool) -> Result<()> {
        let budget = self.command_timeout;
        let mailbox = mailbox.to_string();
        self.with_session(budget, "SUBSCRIBE", move |session| {
            Box::pin(async move {
                if subscribe {
                    session.subscribe(&mailbox).await.map_err(imap_error)
                } else {
                    session.unsubscribe(&mailbox).await.map_err(imap_error)
                }
            })
        })
        .await
    }

    /// Run `UID SEARCH` and require a tagged `OK` completion.
    ///
    /// The typed `async-imap` helper stops consuming at the tagged response
    /// without checking its status, so a `NO`/`BAD` is indistinguishable from
    /// an empty result set. Destructive recovery paths must never read that as
    /// "the message is gone": collect every `SEARCH` response, then accept the
    /// result only when this command's own tagged completion is `OK`.
    ///
    /// `Err` on `NO`/`BAD`, a lost connection, or a timeout. Callers treat
    /// every error as "inconclusive", not as absence.
    pub async fn uid_search_checked(&mut self, criteria: &str) -> Result<HashSet<u32>> {
        if criteria.contains('\r') || criteria.contains('\n') {
            return Err(AppError::invalid_input(
                "refusing UID SEARCH criteria with a line break",
            ));
        }
        let command = format!("UID SEARCH {criteria}");
        // A checked SEARCH does exactly the same server-side work as the typed
        // `uid_search`; it must use the FETCH/SEARCH budget, not the shorter
        // interactive one, or a legitimately slow search poisons the session.
        let budget = self.fetch_timeout;
        let result = self
            .with_session(budget, "UID SEARCH", move |session| {
                Box::pin(async move {
                    let tag = session.run_command(&command).await.map_err(imap_error)?;
                    let mut found = HashSet::new();
                    loop {
                        let response = session
                            .read_response()
                            .await
                            .map_err(|error| {
                                AppError::network(format!("UID SEARCH read failed: {error}"))
                            })?;
                        let Some(data) = response else {
                            break Err(AppError::network(
                                "IMAP connection lost during UID SEARCH",
                            ));
                        };
                        match data.parsed() {
                            Response::MailboxData(MailboxDatum::Search(uids)) => {
                                found.extend(uids.iter().copied());
                            }
                            Response::Done {
                                tag: done, status, ..
                            } if done.0 == tag.0 => {
                                break match status {
                                    Status::Ok => Ok(found),
                                    other => Err(AppError::protocol(format!(
                                        "IMAP UID SEARCH rejected: {other:?}"
                                    ))),
                                };
                            }
                            _ => continue,
                        }
                    }
                })
            })
            .await?;
        self.touch();
        Ok(result)
    }

    /// Append raw RFC822 bytes to a mailbox.
    pub async fn append(&mut self, mailbox: &str, flags: Option<&str>, bytes: &[u8]) -> Result<()> {
        self.append_with_uid(mailbox, flags, bytes)
            .await
            .map(|_| ())
            .map_err(AppendError::into_error)
    }

    /// Append raw RFC822 bytes and return the server-assigned UID when the
    /// server answered with a UIDPLUS `APPENDUID`.
    ///
    /// This reimplements `async-imap`'s typed `append` because that method
    /// discards the tagged response, and with it the only authoritative
    /// binding between the new server copy and the local draft projection.
    /// The literal is written to the transport directly, exactly like
    /// `async-imap` does, and the tagged completion is checked here so a
    /// `NO`/`BAD` can never be mistaken for a successful upload.
    ///
    /// Failures carry [`AppendError::sent`]: a caller that persists a retry
    /// marker must not blindly repeat an APPEND whose literal already reached
    /// the socket.
    pub async fn append_with_uid(
        &mut self,
        mailbox: &str,
        flags: Option<&str>,
        bytes: &[u8],
    ) -> std::result::Result<Option<AppendedUid>, AppendError> {
        if mailbox.contains('\r') || mailbox.contains('\n') {
            return Err(AppendError::new(
                false,
                AppError::invalid_input("refusing a mailbox name with a line break"),
            ));
        }
        if flags.is_some_and(|flags| flags.contains('\r') || flags.contains('\n')) {
            return Err(AppendError::new(
                false,
                AppError::invalid_input("refusing APPEND flags with a line break"),
            ));
        }
        // `async-imap` quotes mailbox names the same way; this command is
        // assembled by hand because the typed API hides the completion.
        let quoted = format!("\"{}\"", mailbox.replace('\\', "\\\\").replace('"', "\\\""));
        let command = format!(
            "APPEND {quoted}{}{} {{{}}}",
            if flags.is_some() { " " } else { "" },
            flags.unwrap_or(""),
            bytes.len()
        );
        if self.is_poisoned() {
            return Err(AppendError::new(false, poisoned_reuse_error("APPEND")));
        }
        let control = self.control.clone();
        let command_budget = self.command_timeout;
        let fetch_budget = self.fetch_timeout;
        let session = &mut self.session;
        let tag = timeout_or_poison(&control, command_budget, "APPEND", async {
            session.run_command(&command).await.map_err(imap_error)
        })
        .await
        .map_err(|error| AppendError::new(false, error))?;

        // Every remaining phase shares one absolute deadline. A server that
        // keeps sending unsolicited `EXISTS` (or any other chatter) between
        // reads must not be able to renew the budget and pin the serialized
        // worker forever: the total time from a written APPEND to the tagged
        // completion is bounded by the FETCH/SEARCH budget.
        let deadline = Instant::now() + fetch_budget;

        // Wait for the continuation request; a server may reject the command
        // outright with a tagged NO/BAD, which must surface as an error.
        loop {
            let response =
                timeout_at_or_poison(&control, deadline, fetch_budget, "APPEND response", async {
                    session
                        .read_response()
                        .await
                        .map_err(|error| AppError::network(format!("APPEND read failed: {error}")))
                })
                .await
                .map_err(|error| AppendError::new(false, error))?;
            let Some(data) = response else {
                return Err(AppendError::new(
                    false,
                    AppError::network("IMAP connection lost during APPEND"),
                ));
            };
            match data.parsed() {
                Response::Continue(_) => break,
                Response::Done {
                    tag: done, status, ..
                } if done.0 == tag.0 => {
                    return Err(AppendError::new(
                        false,
                        AppError::protocol(format!("IMAP APPEND rejected: {status:?}")),
                    ))
                }
                _ => continue,
            }
        }

        {
            let transport = session.as_mut();
            timeout_at_or_poison(&control, deadline, fetch_budget, "APPEND upload", async {
                let upload = |error: std::io::Error| {
                    AppError::network(format!("APPEND upload failed: {error}"))
                };
                transport.write_all(bytes).await.map_err(upload)?;
                transport.write_all(b"\r\n").await.map_err(upload)?;
                transport.flush().await.map_err(upload)
            })
            .await
            .map_err(|error| AppendError::new(true, error))?;
        }

        let appended = loop {
            // The literal is already on the wire, so every failure from here
            // on is `sent`: the server may have committed the APPEND even
            // though its receipt was never read.
            let response = timeout_at_or_poison(
                &control,
                deadline,
                fetch_budget,
                "APPEND completion",
                async {
                    session.read_response().await.map_err(|error| {
                        AppError::network(format!("APPEND completion read failed: {error}"))
                    })
                },
            )
            .await
            .map_err(|error| AppendError::new(true, error))?;
            let Some(data) = response else {
                return Err(AppendError::new(
                    true,
                    AppError::network("IMAP connection lost after APPEND"),
                ));
            };
            let Response::Done {
                tag: done,
                status,
                outcome,
            } = data.parsed()
            else {
                continue;
            };
            if done.0 != tag.0 {
                continue;
            }
            if !matches!(status, Status::Ok) {
                // The server saw the literal and explicitly refused it, so the
                // copy does not exist and retrying the APPEND is safe.
                return Err(AppendError::rejected(
                    true,
                    AppError::protocol(format!("IMAP APPEND rejected: {status:?}")),
                ));
            }
            break match outcome.code.as_ref() {
                Some(ResponseCode::AppendUid(validity, uids)) => first_uid(*validity, uids),
                _ => None,
            };
        };
        self.touch();
        Ok(appended)
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
            dial(
                address,
                host.clone(),
                security,
                tls_config.clone(),
                timeouts,
            )
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

    let control = TransportControl::new(timeouts.io_read, timeouts.io_write);
    let transport = TimeoutStream::new(transport, control.clone());

    let client = match config.security {
        Security::Tls => {
            let mut client = Client::new(transport);
            read_greeting(&mut client, timeouts.greeting).await?;
            client
        }
        Security::Starttls => {
            let tcp = match transport.into_inner() {
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
            Client::new(TimeoutStream::new(transport, control.clone()))
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
        } => tokio::time::timeout(
            timeouts.auth,
            client.login_with_capabilities(user, password),
        )
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

    let caps = match timeout_or_poison(&control, timeouts.command, "CAPABILITY", async {
        session.capabilities().await.map_err(imap_error)
    })
    .await
    {
        Ok(caps) => ServerCapabilities::from_capabilities(&caps),
        Err(error) => {
            if crate::util::is_command_timeout(&error) {
                return Err(error);
            }
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
        control: control.clone(),
        command_timeout: timeouts.command,
        fetch_timeout: timeouts.fetch,
    };

    if connection.caps.raw.iter().any(|cap| cap == "ID") {
        let id = [
            ("name", Some("Nuncio")),
            ("version", Some(env!("CARGO_PKG_VERSION"))),
            ("vendor", Some("Nuncio")),
        ];
        match timeout_or_poison(&control, timeouts.command, "ID", async {
            connection.session.id(id).await.map_err(imap_error)
        })
        .await
        {
            Ok(_) => debug!("IMAP ID exchanged"),
            Err(error) if crate::util::is_command_timeout(&error) => return Err(error),
            Err(error) => debug!(%error, "IMAP ID command not accepted"),
        }
    }

    // The session deliberately stays IMAP4rev1 even when the server advertises
    // IMAP4rev2: rev2 requires `ESEARCH` for search (RFC 9051 §6.4.4), which
    // `imap-proto`/`async-imap` 0.12 do not parse, while the sync engine relies
    // on the legacy `UID SEARCH ALL`. `directed_expunge` therefore depends on
    // UIDPLUS alone.
    if connection.caps.condstore {
        if let Err(error) = connection.raw_command_ok("ENABLE CONDSTORE").await {
            if crate::util::is_command_timeout(&error) {
                return Err(error);
            }
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
    connect_account_with_timeouts(ctx, account, ConnectTimeouts::default()).await
}

/// [`connect_account`] with explicit command and socket budgets. The account
/// worker passes its configured values so a test (or a slow-server profile)
/// can tighten the command deadline without touching the connect phases.
pub async fn connect_account_with_timeouts(
    ctx: &CoreContext,
    account: &Account,
    timeouts: ConnectTimeouts,
) -> Result<ImapConnection> {
    let config = account
        .imap
        .as_ref()
        .ok_or_else(|| AppError::invalid_input("account has no IMAP server configured"))?;
    let auth = ctx.creds.mail_auth(account).await?;
    match connect_with_timeouts(config, &auth, timeouts).await {
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
            connect_with_timeouts(config, &refreshed, timeouts).await.map_err(|second| {
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
    match trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
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
        return Err(AppError::network(
            "IMAP host did not resolve to any address",
        ));
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

async fn read_greeting(client: &mut Client<TimeoutStream>, timeout: Duration) -> Result<()> {
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

    #[test]
    fn appenduid_parser_accepts_only_a_single_nonzero_uid() {
        assert_eq!(
            first_uid(1, &[UidSetMember::Uid(7)]),
            Some(AppendedUid {
                uid: 7,
                uidvalidity: 1
            })
        );
        assert_eq!(
            first_uid(1, &[UidSetMember::UidRange(7..=7)]),
            Some(AppendedUid {
                uid: 7,
                uidvalidity: 1
            })
        );
        // Ambiguous answers must fall back to "appended, not yet bound"
        // instead of guessing which UID belongs to this copy.
        assert_eq!(first_uid(1, &[]), None);
        assert_eq!(
            first_uid(1, &[UidSetMember::Uid(7), UidSetMember::Uid(8)]),
            None
        );
        assert_eq!(first_uid(1, &[UidSetMember::UidRange(7..=8)]), None);
        assert_eq!(first_uid(1, &[UidSetMember::Uid(0)]), None);
    }

    use nuncio_core::models::{
        AccountState, AuthKind, ProviderCapabilities, ProviderKind, SentCopyMode,
    };

    /// An account row as the merge sees it: e-mail domain, the user's
    /// sent-copy choice and the persisted capability flags.
    fn account(email: &str) -> Account {
        Account {
            id: "acc".into(),
            provider: ProviderKind::Imap,
            email: email.into(),
            display_name: "Test".into(),
            color: "#000000".into(),
            auth_kind: AuthKind::Password,
            oauth_provider: None,
            imap: None,
            smtp: None,
            pop3: None,
            caldav_url: None,
            carddav_url: None,
            sent_copy: None,
            sync_window_days: 0,
            mail_enabled: true,
            calendar_enabled: false,
            contacts_enabled: false,
            tasks_enabled: false,
            notifications_enabled: true,
            include_in_unified: true,
            sort_order: 0,
            state: AccountState::Ok,
            state_message: None,
            capabilities: ProviderCapabilities::defaults_for(ProviderKind::Imap),
            created_at: nuncio_core::models::now(),
        }
    }

    /// `merge_into` only needs a non-empty `raw` (evidence that a CAPABILITY
    /// exchange happened) plus the protocol flags, so a synthetic snapshot is
    /// enough.
    fn probe() -> ServerCapabilities {
        ServerCapabilities {
            raw: vec!["IMAP4rev1".into()],
            ..ServerCapabilities::default()
        }
    }

    #[test]
    fn dual_revision_capability_does_not_supply_directed_expunge() {
        // The session deliberately stays rev1 (ESEARCH is not implemented),
        // and rev2 is never enabled, so the advertised revision cannot vouch
        // for `UID EXPUNGE`.
        let caps = ServerCapabilities::from_raw(vec![
            "IMAP4rev1".into(),
            "IMAP4rev2".into(),
            "CONDSTORE".into(),
        ]);
        assert!(caps.imap4rev2);
        assert!(!caps.directed_expunge());
    }

    #[test]
    fn rev2_only_server_does_not_supply_directed_expunge() {
        let caps = ServerCapabilities::from_raw(vec!["IMAP4rev2".into()]);
        assert!(caps.imap4rev2);
        assert!(!caps.directed_expunge());
    }

    #[test]
    fn uidplus_is_trusted_on_a_rev1_session() {
        let caps = ServerCapabilities::from_raw(vec!["IMAP4rev1".into(), "UIDPLUS".into()]);
        assert!(caps.directed_expunge());
    }

    #[test]
    fn merge_does_not_append_sent_for_presets_that_file_it() {
        // Yahoo files sent mail itself; a client APPEND would duplicate it.
        assert!(probe().merge_into(&account("me@yahoo.com")).auto_saves_sent);
        assert!(probe().merge_into(&account("me@aol.com")).auto_saves_sent);
        assert!(
            !probe()
                .merge_into(&account("me@yandex.com"))
                .auto_saves_sent
        );

        // An explicit per-account choice survives the next connection instead
        // of being overwritten by the provider default.
        let mut yandex = account("me@yandex.com");
        yandex.sent_copy = Some(SentCopyMode::Server);
        assert!(probe().merge_into(&yandex).auto_saves_sent);

        let mut yahoo = account("me@yahoo.com");
        yahoo.sent_copy = Some(SentCopyMode::Client);
        assert!(!probe().merge_into(&yahoo).auto_saves_sent);
    }

    #[test]
    fn merge_uses_the_fresh_gmail_probe_as_positive_evidence() {
        // X-GM-EXT-1 is live evidence that the server files sent mail, even
        // though the persisted `gmail_extensions` flag is still false.
        let account = account("me@yandex.com");
        assert!(!account.capabilities.gmail_extensions);
        let probe = ServerCapabilities {
            gmail: true,
            ..probe()
        };
        assert!(probe.merge_into(&account).auto_saves_sent);
    }

    #[test]
    fn merge_gates_server_search_on_the_provider_directory() {
        // These servers accept UID SEARCH but the results are unusable: QQ and
        // NetEase return no hits, 139 ignores the criteria.
        assert!(!probe().merge_into(&account("me@qq.com")).server_search);
        assert!(!probe().merge_into(&account("me@163.com")).server_search);
        assert!(!probe().merge_into(&account("me@139.com")).server_search);
        // A server that answered CAPABILITY and is not known-broken keeps it.
        assert!(probe().merge_into(&account("me@gmail.com")).server_search);
        assert!(
            probe()
                .merge_into(&account("me@unknown.example"))
                .server_search
        );
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
            vec![SocketAddr::new(
                std::net::IpAddr::V4(Ipv4Addr::LOCALHOST),
                143
            )]
        );
        let bracketed = resolve("[::1]", 143, Duration::from_millis(1))
            .await
            .expect("bracketed literal resolves");
        assert_eq!(
            bracketed,
            vec![SocketAddr::new(
                std::net::IpAddr::V6(Ipv6Addr::LOCALHOST),
                143
            )]
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
            Err::<(), _>(AppError::network(format!(
                "TCP connect to {address} timed out"
            )))
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
        assert!(
            peak.load(Ordering::SeqCst) > 1,
            "dialer should race addresses"
        );
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
        assert!(
            error.message().contains("203.0.113.7"),
            "{}",
            error.message()
        );
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
        let control = TransportControl::new(Duration::ZERO, Duration::ZERO);
        let mut client = Client::new(TimeoutStream::new(
            ImapTransport::Plain(stream),
            control,
        ));
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
        let error = wrap_tls(
            stream,
            "localhost",
            tls_client_config().expect("tls config"),
            timeouts,
        )
        .await
        .expect_err("no handshake completes");
        assert!(
            error
                .message()
                .contains("TLS handshake with localhost timed out"),
            "{}",
            error.message()
        );
        silent.abort();
    }
}
