//! Per-account actors: one serialized worker connection, one IDLE connection,
//! pollers for other folders, and the operation/outbox tickers.

use std::time::{Duration, Instant};

use nuncio_core::models::{Account, AccountState, FolderRole, ProviderKind};
use nuncio_core::{events, repo, AppError, CoreContext, Result};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{sleep, sleep_until, Instant as TokioInstant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::engine::EngineConfig;
use crate::imap::conn::ImapConnection;
use crate::ops::executor::{expire, store};
use crate::sync::body;
use crate::util::{is_command_timeout, is_connection_error};
use crate::{ops, pop3, smtp, sync};

/// Commands serialized through an account's worker connection.
pub(crate) enum AccountCommand {
    SyncAll(oneshot::Sender<Result<()>>),
    SyncFolder(Option<String>, oneshot::Sender<Result<()>>),
    SyncOtherFolders,
    FetchBody {
        message_id: String,
        reply: oneshot::Sender<Result<std::path::PathBuf>>,
    },
    FetchPart {
        message_id: String,
        part: String,
        reply: oneshot::Sender<Result<Vec<u8>>>,
    },
    Search {
        folder_id: Option<String>,
        criteria: String,
        reply: oneshot::Sender<Result<Vec<String>>>,
    },
    /// Header-scan fallback for servers whose content SEARCH is broken.
    Scan {
        folder_id: String,
        query: nuncio_core::models::SearchQuery,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
        reply: oneshot::Sender<Result<sync::messages::ScanOutcome>>,
    },
    Append {
        folder_id: String,
        bytes: Vec<u8>,
        flags: Option<String>,
        reply: oneshot::Sender<Result<()>>,
    },
    ListFolders(oneshot::Sender<Result<Vec<nuncio_core::models::Folder>>>),
    FolderCreate {
        parent_id: Option<String>,
        name: String,
        reply: oneshot::Sender<Result<()>>,
    },
    FolderRename {
        folder_id: String,
        name: String,
        reply: oneshot::Sender<Result<()>>,
    },
    FolderMove {
        folder_id: String,
        new_parent_id: Option<String>,
        reply: oneshot::Sender<Result<()>>,
    },
    FolderDelete {
        folder_id: String,
        reply: oneshot::Sender<Result<()>>,
    },
    FolderSubscribe {
        folder_id: String,
        subscribe: bool,
        reply: oneshot::Sender<Result<()>>,
    },
    Expunge {
        folder_id: String,
        /// `true` when the messages were removed from the server, `false` when
        /// the server has no UIDPLUS and they keep their `\Deleted` flag.
        reply: oneshot::Sender<Result<bool>>,
    },
    ExecuteOps {
        reply: Option<oneshot::Sender<Result<()>>>,
    },
    /// User-driven "load older mail" batch for one folder.
    LoadOlder {
        folder_id: String,
        reply: oneshot::Sender<Result<nuncio_core::models::LoadOlderResult>>,
    },
    Reconnect,
    /// NOOP keepalive for accounts whose server has no IDLE support.
    KeepAlive,
}

pub(crate) struct AccountRuntime {
    pub tx: mpsc::Sender<AccountCommand>,
    pub cancel: CancellationToken,
    pub tasks: Vec<JoinHandle<()>>,
}

impl AccountRuntime {
    pub fn abort(&self) {
        self.cancel.cancel();
        for task in &self.tasks {
            task.abort();
        }
    }
}

pub(crate) async fn spawn(
    ctx: CoreContext,
    account: Account,
    parent_cancel: &CancellationToken,
    config: EngineConfig,
) -> Result<AccountRuntime> {
    let cancel = parent_cancel.child_token();
    let (tx, rx) = mpsc::channel(64);
    let mut tasks = Vec::new();
    let is_pop3 = account.provider == ProviderKind::Pop3
        || (account.imap.is_none() && account.pop3.is_some());
    // Third transport: Microsoft OAuth accounts are served by Graph.  This is
    // the ONE predicate every branch in the crate calls, so a mailbox cannot
    // end up half on IMAP and half on Graph.
    let is_graph = crate::graph::uses_graph(&account);

    if is_pop3 {
        tasks.push(tokio::spawn(pop3_loop(
            ctx.clone(),
            account.id.clone(),
            cancel.clone(),
        )));
        tasks.push(tokio::spawn(ops_ticker(
            ctx.clone(),
            account.id.clone(),
            None,
            cancel.clone(),
        )));
    } else if is_graph {
        // Graph has no IDLE and no IMAP worker: one serialized loop polls on
        // the existing no-IDLE cadence, handles commands (body fetch, sync
        // now) and drains the operation queue.  IDLE/keepalive stay IMAP-only.
        tasks.push(tokio::spawn(graph_loop(
            ctx.clone(),
            account.id.clone(),
            rx,
            cancel.clone(),
            config.clone(),
        )));
        tasks.push(tokio::spawn(ops_ticker(
            ctx.clone(),
            account.id.clone(),
            Some(tx.clone()),
            cancel.clone(),
        )));
    } else {
        tasks.push(tokio::spawn(worker_loop(
            ctx.clone(),
            account.id.clone(),
            rx,
            cancel.clone(),
            config.clone(),
        )));
        tasks.push(tokio::spawn(idle_loop(
            ctx.clone(),
            account.id.clone(),
            tx.clone(),
            cancel.clone(),
            config.clone(),
        )));
        tasks.push(tokio::spawn(keepalive_loop(
            account.id.clone(),
            tx.clone(),
            cancel.clone(),
            config.keepalive_interval,
        )));
        tasks.push(tokio::spawn(poll_loop(
            ctx.clone(),
            account.id.clone(),
            tx.clone(),
            cancel.clone(),
        )));
        tasks.push(tokio::spawn(ops_ticker(
            ctx.clone(),
            account.id.clone(),
            Some(tx.clone()),
            cancel.clone(),
        )));
    }
    if !is_graph {
        // Graph sends happen synchronously through `sendMail`; there is no
        // SMTP outbox to drain (and an SMTP attempt for an MSA mailbox would
        // only fail).  The outbox loop stays IMAP/POP3-only.
        tasks.push(tokio::spawn(outbox_loop(
            ctx,
            account.id.clone(),
            cancel.clone(),
        )));
    }

    info!(
        account = %account.id,
        pop3 = is_pop3,
        graph = is_graph,
        "mail account actor started"
    );
    Ok(AccountRuntime { tx, cancel, tasks })
}

/* ────────────────────────────── worker ─────────────────────────────── */

/// Exponential reconnect backoff: 5 s → 10 → 20 … capped at five minutes,
/// reset only after a connection actually succeeds.
#[derive(Debug, Clone)]
struct Backoff {
    next: Duration,
    initial: Duration,
    max: Duration,
}

impl Backoff {
    fn new(initial: Duration, max: Duration) -> Self {
        Self {
            next: initial,
            initial,
            max,
        }
    }

    fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(self.max);
        delay
    }

    fn reset(&mut self) {
        self.next = self.initial;
    }
}

struct Worker {
    ctx: CoreContext,
    account_id: String,
    conn: Option<ImapConnection>,
    backoff: Backoff,
    next_attempt: Instant,
    short_circuited: bool,
    config: EngineConfig,
    /// Reconnect/command failures since the last successful round trip.
    consecutive_failures: u32,
    /// First failure of the current unhealthy streak, for the time-based gate.
    unhealthy_since: Option<Instant>,
    /// Whether the current outage has already been surfaced to the user, so we
    /// do not emit a WARN + offline state on every retry.
    reported_failure: bool,
}

impl Worker {
    fn new(ctx: CoreContext, account_id: String, config: EngineConfig) -> Self {
        Self {
            ctx,
            account_id,
            conn: None,
            backoff: Backoff::new(Duration::from_secs(5), Duration::from_secs(300)),
            next_attempt: Instant::now(),
            short_circuited: false,
            config,
            consecutive_failures: 0,
            unhealthy_since: None,
            reported_failure: false,
        }
    }

    async fn load_account(&self) -> Result<Account> {
        let id = self.account_id.clone();
        self.ctx
            .db
            .read(move |db| repo::accounts::get_required(db, &id))
            .await
    }

    /// Whether a failure occurring now should be shown to the user. Used by
    /// sync paths before running so a single transient flap stays invisible.
    fn report_network_errors(&self) -> bool {
        self.consecutive_failures + 1 >= self.config.error_after_failures
            || self
                .unhealthy_since
                .map(|since| since.elapsed() >= self.config.error_after)
                .unwrap_or(false)
    }

    fn should_report_failure(&self) -> bool {
        self.consecutive_failures >= self.config.error_after_failures
            || self
                .unhealthy_since
                .map(|since| since.elapsed() >= self.config.error_after)
                .unwrap_or(false)
    }

    async fn connection(&mut self, account: &Account) -> Result<&mut ImapConnection> {
        // A timed-out command poisons the session: its response stream cannot
        // be resynchronised, so it must never be handed to another command.
        // `finish` drops it on the error path; this is the defensive guard for
        // paths that swallowed a timeout inside a larger command.
        if self
            .conn
            .as_ref()
            .is_some_and(|connection| connection.is_poisoned())
        {
            debug!(
                account = %self.account_id,
                "discarding an IMAP connection poisoned by a timeout"
            );
            self.conn = None;
            self.next_attempt = Instant::now();
        }
        if self.conn.is_none() {
            let now = Instant::now();
            if now < self.next_attempt {
                self.short_circuited = true;
                return Err(AppError::network("IMAP is reconnecting"));
            }
            let timeouts = crate::imap::conn::ConnectTimeouts {
                command: self.config.command_timeout,
                fetch: self.config.fetch_timeout,
                io_read: self.config.io_read_timeout,
                io_write: self.config.io_write_timeout,
                ..crate::imap::conn::ConnectTimeouts::default()
            };
            let connection =
                crate::imap::conn::connect_account_with_timeouts(&self.ctx, account, timeouts)
                    .await?;
            if connection.is_poisoned() {
                return Err(AppError::network(
                    "IMAP handshake ended with an unusable connection",
                ));
            }
            let capabilities = connection.caps.merge_into(account);
            persist_capabilities(&self.ctx, account, capabilities).await?;
            self.conn = Some(connection);
            self.backoff.reset();
            self.next_attempt = Instant::now();
            self.set_account_state(AccountState::Ok, None).await?;
        }
        self.conn
            .as_mut()
            .ok_or_else(|| AppError::internal("IMAP connection was not stored"))
    }

    /// Get a session ready for a command: a connection that has been idle for
    /// a while is probed with NOOP first so a server-side close is detected
    /// before the real command hits EPIPE.
    async fn session(&mut self, account: &Account) -> Result<&mut ImapConnection> {
        let stale = self
            .conn
            .as_ref()
            .map(|conn| conn.idle_for() >= self.config.liveness_probe_after)
            .unwrap_or(false);
        if stale {
            let failure = match self.conn.as_mut() {
                Some(conn) => conn.noop().await.err(),
                None => None,
            };
            if let Some(error) = failure {
                let poisoned = self
                    .conn
                    .as_ref()
                    .is_some_and(|connection| connection.is_poisoned());
                if is_connection_error(&error) || is_command_timeout(&error) || poisoned {
                    debug!(
                        account = %self.account_id,
                        %error,
                        "stale or timed-out IMAP session detected; reconnecting before use"
                    );
                    self.conn = None;
                    self.next_attempt = Instant::now();
                } else {
                    return Err(error);
                }
            }
        }
        self.connection(account).await
    }

    async fn set_account_state(&self, state: AccountState, message: Option<String>) -> Result<()> {
        let account_id = self.account_id.clone();
        self.ctx
            .db
            .write({
                let message = message.clone();
                move |db| repo::accounts::update_state(db, &account_id, state, message.as_deref())
            })
            .await?;
        events::emit_accounts_changed(self.ctx.events.as_ref(), vec![self.account_id.clone()]);
        Ok(())
    }

    async fn record_failure(&mut self, error: &AppError) {
        self.conn = None;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.unhealthy_since.is_none() {
            self.unhealthy_since = Some(Instant::now());
        }
        match error {
            AppError::Auth(_) => {
                let _ = self
                    .set_account_state(AccountState::NeedsReauth, Some(error.message()))
                    .await;
                self.next_attempt = Instant::now() + Duration::from_secs(60);
                self.reported_failure = true;
            }
            AppError::Network(_) => {
                let delay = self.backoff.next_delay();
                self.next_attempt = Instant::now() + delay;
                if self.should_report_failure() {
                    if self.reported_failure {
                        debug!(
                            account = %self.account_id,
                            %error,
                            retry_seconds = delay.as_secs(),
                            "IMAP reconnect failed"
                        );
                    } else {
                        warn!(
                            account = %self.account_id,
                            %error,
                            retry_seconds = delay.as_secs(),
                            "IMAP connection failed"
                        );
                    }
                    let _ = self
                        .set_account_state(AccountState::Offline, Some(error.message()))
                        .await;
                    self.reported_failure = true;
                } else {
                    debug!(
                        account = %self.account_id,
                        %error,
                        retry_seconds = delay.as_secs(),
                        "transient IMAP failure; retrying"
                    );
                }
            }
            _ => {
                let _ = self
                    .set_account_state(AccountState::Error, Some(error.message()))
                    .await;
                self.reported_failure = true;
            }
        }
    }

    /// Common success/failure bookkeeping for a command attempt.
    async fn finish<T>(&mut self, account: &Account, result: Result<T>) -> Result<T> {
        match &result {
            Ok(_) => {
                if let Some(conn) = self.conn.as_mut() {
                    conn.touch();
                }
                let had_reported = self.reported_failure;
                self.consecutive_failures = 0;
                self.unhealthy_since = None;
                self.reported_failure = false;
                if account.state != AccountState::Ok {
                    let _ = self.set_account_state(AccountState::Ok, None).await;
                }
                if had_reported {
                    // A previously surfaced outage is over: clear the banner.
                    crate::sync::settle_sync_idle(&self.ctx, &self.account_id);
                }
            }
            Err(error) => {
                if !self.short_circuited {
                    self.record_failure(error).await;
                }
            }
        }
        result
    }

    /// Run a non-idempotent operation (or one whose retry is owned by the
    /// persistent op queue). No transparent retry.
    async fn run_conn<T, F>(&mut self, f: F) -> Result<T>
    where
        F: for<'a> FnOnce(
            &'a mut ImapConnection,
            &'a Account,
            CoreContext,
        ) -> futures::future::BoxFuture<'a, Result<T>>,
    {
        self.short_circuited = false;
        let account = self.load_account().await?;
        let ctx = self.ctx.clone();
        let result = match self.session(&account).await {
            Ok(conn) => f(conn, &account, ctx).await,
            Err(error) => Err(error),
        };
        self.finish(&account, result).await
    }

    /// Run an idempotent command (SELECT/EXAMINE/FETCH/SEARCH/NOOP/LIST/
    /// STATUS). If the socket died, reconnect once and retry exactly once.
    async fn run_conn_read<T, F>(&mut self, mut f: F) -> Result<T>
    where
        F: for<'a> FnMut(
            &'a mut ImapConnection,
            &'a Account,
            CoreContext,
        ) -> futures::future::BoxFuture<'a, Result<T>>,
    {
        self.short_circuited = false;
        let account = self.load_account().await?;
        let ctx = self.ctx.clone();
        let mut result = match self.session(&account).await {
            Ok(conn) => f(conn, &account, ctx.clone()).await,
            Err(error) => Err(error),
        };
        let retry = match &result {
            Err(error)
                if !self.short_circuited
                    && (is_connection_error(error) || is_command_timeout(error)) =>
            {
                debug!(
                    account = %self.account_id,
                    %error,
                    "IMAP command lost its connection or timed out; reconnecting and retrying once"
                );
                true
            }
            _ => false,
        };
        if retry {
            self.conn = None;
            self.next_attempt = Instant::now();
            result = match self.session(&account).await {
                Ok(conn) => f(conn, &account, ctx).await,
                Err(error) => Err(error),
            };
        }
        self.finish(&account, result).await
    }
}

async fn worker_loop(
    ctx: CoreContext,
    account_id: String,
    mut rx: mpsc::Receiver<AccountCommand>,
    cancel: CancellationToken,
    config: EngineConfig,
) {
    let mut worker = Worker::new(ctx, account_id.clone(), config);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            command = rx.recv() => {
                let Some(command) = command else { break };
                handle_command(&mut worker, command).await;
            }
        }
    }
    if let Some(connection) = worker.conn.take() {
        connection.logout().await;
    }
    debug!(account = %account_id, "worker loop stopped");
}

async fn handle_command(worker: &mut Worker, command: AccountCommand) {
    match command {
        AccountCommand::SyncAll(reply) => {
            let report_errors = worker.report_network_errors();
            let result = worker
                .run_conn_read(|conn, account, ctx| {
                    Box::pin(async move {
                        sync::sync_account_with(&ctx, conn, account, report_errors).await
                    })
                })
                .await;
            let _ = reply.send(result);
        }
        AccountCommand::SyncFolder(folder_id, reply) => {
            let report_errors = worker.report_network_errors();
            let result = match folder_id {
                Some(folder_id) => {
                    worker
                        .run_conn_read(move |conn, account, ctx| {
                            let folder_id = folder_id.clone();
                            Box::pin(async move {
                                sync::sync_folder_by_id_with(
                                    &ctx,
                                    conn,
                                    account,
                                    &folder_id,
                                    report_errors,
                                )
                                .await
                            })
                        })
                        .await
                }
                None => {
                    worker
                        .run_conn_read(|conn, account, ctx| {
                            Box::pin(async move {
                                let folders = sync::load_folders(&ctx, &account.id).await?;
                                if let Some(inbox) =
                                    folders.iter().find(|f| f.role == FolderRole::Inbox)
                                {
                                    sync::sync_folder_by_id_with(
                                        &ctx,
                                        conn,
                                        account,
                                        &inbox.id,
                                        report_errors,
                                    )
                                    .await
                                } else {
                                    sync::sync_account_with(&ctx, conn, account, report_errors)
                                        .await
                                }
                            })
                        })
                        .await
                }
            };
            let _ = reply.send(result);
        }
        AccountCommand::SyncOtherFolders => {
            let report_errors = worker.report_network_errors();
            let result = worker
                .run_conn_read(|conn, account, ctx| {
                    Box::pin(async move {
                        sync::sync_other_folders_with(&ctx, conn, account, report_errors).await
                    })
                })
                .await;
            if let Err(error) = result {
                debug!(%error, "other-folder poll failed");
            }
        }
        AccountCommand::FetchBody { message_id, reply } => {
            let result = worker
                .run_conn_read(move |conn, account, ctx| {
                    let message_id = message_id.clone();
                    Box::pin(
                        async move { body::fetch_body(&ctx, conn, &account.id, &message_id).await },
                    )
                })
                .await;
            let _ = reply.send(result);
        }
        AccountCommand::FetchPart {
            message_id,
            part,
            reply,
        } => {
            let result = worker
                .run_conn_read(move |conn, _account, ctx| {
                    let message_id = message_id.clone();
                    let part = part.clone();
                    Box::pin(
                        async move { body::fetch_attachment(&ctx, conn, &message_id, &part).await },
                    )
                })
                .await;
            let _ = reply.send(result);
        }
        AccountCommand::Search {
            folder_id,
            criteria,
            reply,
        } => {
            let result = worker
                .run_conn_read(move |conn, account, ctx| {
                    let folder_id = folder_id.clone();
                    let criteria = criteria.clone();
                    Box::pin(async move {
                        let folder = match folder_id {
                            Some(id) => {
                                let id = id.to_string();
                                ctx.db
                                    .read(move |db| repo::folders::get_required(db, &id))
                                    .await?
                            }
                            None => {
                                let folders = sync::load_folders(&ctx, &account.id).await?;
                                folders
                                    .into_iter()
                                    .find(|f| f.role == FolderRole::Inbox)
                                    .ok_or_else(|| {
                                        AppError::not_found("account has no Inbox folder")
                                    })?
                            }
                        };
                        sync::messages::server_search(&ctx, conn, account, &folder, &criteria).await
                    })
                })
                .await;
            let _ = reply.send(result);
        }
        AccountCommand::Scan {
            folder_id,
            query,
            cancel,
            reply,
        } => {
            let result = worker
                .run_conn_read(move |conn, account, ctx| {
                    let folder_id = folder_id.clone();
                    let query = query.clone();
                    let cancel = cancel.clone();
                    Box::pin(async move {
                        let folder = ctx
                            .db
                            .read(move |db| repo::folders::get_required(db, &folder_id))
                            .await?;
                        sync::messages::server_scan(&ctx, conn, account, &folder, &query, &cancel)
                            .await
                    })
                })
                .await;
            let _ = reply.send(result);
        }
        AccountCommand::Append {
            folder_id,
            bytes,
            flags,
            reply,
        } => {
            let result = worker
                .run_conn(move |conn, _account, ctx| {
                    Box::pin(async move {
                        let id = folder_id.clone();
                        let folder: nuncio_core::models::Folder = ctx
                            .db
                            .read(move |db| repo::folders::get_required(db, &id))
                            .await?;
                        conn.append(&folder.remote_path, flags.as_deref(), &bytes)
                            .await
                    })
                })
                .await;
            let _ = reply.send(result);
        }
        AccountCommand::ListFolders(reply) => {
            let result = worker
                .run_conn_read(|conn, account, ctx| {
                    Box::pin(async move { sync::refresh_folders(&ctx, conn, &account.id).await })
                })
                .await;
            let _ = reply.send(result);
        }
        AccountCommand::FolderCreate {
            parent_id,
            name,
            reply,
        } => {
            let result = worker
                .run_conn(move |conn, account, ctx| {
                    Box::pin(async move {
                        let op = ops::executor::enqueue_folder_create_now(
                            &ctx,
                            &account.id,
                            parent_id.as_deref(),
                            &name,
                        )
                        .await?;
                        ops::executor::run_one_now(&ctx, conn, account, &op).await
                    })
                })
                .await;
            let _ = reply.send(result);
        }
        AccountCommand::FolderRename {
            folder_id,
            name,
            reply,
        } => {
            let result = worker
                .run_conn(move |conn, account, ctx| {
                    Box::pin(async move {
                        let op = ops::executor::enqueue_folder_rename_now(
                            &ctx,
                            &account.id,
                            &folder_id,
                            &name,
                        )
                        .await?;
                        ops::executor::run_one_now(&ctx, conn, account, &op).await
                    })
                })
                .await;
            let _ = reply.send(result);
        }
        AccountCommand::FolderMove {
            folder_id,
            new_parent_id,
            reply,
        } => {
            let result = worker
                .run_conn(move |conn, account, ctx| {
                    Box::pin(async move {
                        let folder: nuncio_core::models::Folder = ctx
                            .db
                            .read({
                                let folder_id = folder_id.clone();
                                move |db| repo::folders::get_required(db, &folder_id)
                            })
                            .await?;
                        let delimiter = if folder.remote_path.contains('/') {
                            '/'
                        } else if folder.remote_path.contains('.') {
                            '.'
                        } else {
                            '/'
                        };
                        let new_remote = match new_parent_id.as_deref() {
                            Some(parent_id) => {
                                let parent: nuncio_core::models::Folder = ctx
                                    .db
                                    .read({
                                        let parent_id = parent_id.to_string();
                                        move |db| repo::folders::get_required(db, &parent_id)
                                    })
                                    .await?;
                                if parent.account_id != folder.account_id {
                                    return Err(AppError::invalid_input(
                                        "parent folder belongs to another account",
                                    ));
                                }
                                if parent.id == folder.id {
                                    return Err(AppError::invalid_input(
                                        "a folder cannot be its own parent",
                                    ));
                                }
                                format!(
                                    "{}{delimiter}{}",
                                    parent.remote_path.trim_end_matches(delimiter),
                                    folder.name
                                )
                            }
                            None => folder.name.clone(),
                        };
                        conn.rename(&folder.remote_path, &new_remote).await?;
                        let folder_id = folder.id.clone();
                        let parent_for_db = new_parent_id.clone();
                        let updated_remote = new_remote.clone();
                        ctx.db
                            .write(move |db| {
                                db.execute(
                                    "UPDATE folder SET remote_path = ?2, parent_id = ?3, updated_at = ?4 WHERE id = ?1",
                                    rusqlite::params![
                                        folder_id,
                                        updated_remote,
                                        parent_for_db,
                                        nuncio_core::repo::now_string()
                                    ],
                                )?;
                                Ok(())
                            })
                            .await?;
                        crate::sync::folders::sync(&ctx, conn, &account.id).await?;
                        Ok(())
                    })
                })
                .await;
            let _ = reply.send(result);
        }
        AccountCommand::FolderDelete { folder_id, reply } => {
            let result = worker
                .run_conn(move |conn, account, ctx| {
                    Box::pin(async move {
                        let op =
                            ops::executor::enqueue_folder_delete_now(&ctx, &account.id, &folder_id)
                                .await?;
                        ops::executor::run_one_now(&ctx, conn, account, &op).await
                    })
                })
                .await;
            let _ = reply.send(result);
        }
        AccountCommand::FolderSubscribe {
            folder_id,
            subscribe,
            reply,
        } => {
            let result = worker
                .run_conn(move |conn, account, ctx| {
                    Box::pin(async move {
                        let op = ops::executor::enqueue_folder_subscribe_now(
                            &ctx,
                            &account.id,
                            &folder_id,
                            subscribe,
                        )
                        .await?;
                        ops::executor::run_one_now(&ctx, conn, account, &op).await
                    })
                })
                .await;
            let _ = reply.send(result);
        }
        AccountCommand::Expunge { folder_id, reply } => {
            let result = worker
                .run_conn(move |conn, _account, ctx| {
                    Box::pin(async move {
                        let id = folder_id.clone();
                        // "Do it, or do not do it": without a directed
                        // expunge the only possible command is a whole-folder
                        // EXPUNGE, which would also erase mail other clients
                        // flagged. Refuse before SELECT/STORE and leave both
                        // the server and the local rows untouched.
                        if !conn.caps.directed_expunge() {
                            return Ok(false);
                        }
                        let folder: nuncio_core::models::Folder = ctx
                            .db
                            .read(move |db| repo::folders::get_required(db, &id))
                            .await?;
                        conn.select(&folder.remote_path).await?;
                        store(conn, "1:*", "+FLAGS.SILENT (\\Deleted)").await?;
                        expire(conn, "1:*").await?;
                        let folder_id = folder.id.clone();
                        ctx.db
                            .write(move |db| {
                                repo::messages::delete_all_in_folder(db, &folder_id)?;
                                Ok(())
                            })
                            .await?;
                        Ok(true)
                    })
                })
                .await;
            let _ = reply.send(result);
        }
        AccountCommand::LoadOlder { folder_id, reply } => {
            let result = worker
                .run_conn_read(move |conn, account, ctx| {
                    let folder_id = folder_id.clone();
                    Box::pin(async move {
                        let id = folder_id.clone();
                        let folder = ctx
                            .db
                            .read(move |db| repo::folders::get_required(db, &id))
                            .await?;
                        if folder.account_id != account.id {
                            return Err(AppError::invalid_input(
                                "folder belongs to another account",
                            ));
                        }
                        sync::messages::load_older_imap(&ctx, conn, account, &folder).await
                    })
                })
                .await;
            let _ = reply.send(result);
        }
        AccountCommand::ExecuteOps { reply } => {
            let result = worker
                .run_conn(|conn, account, ctx| {
                    Box::pin(async move {
                        let outcome = ops::executor::run_due(&ctx, conn, account).await?;
                        for folder_id in outcome.resync_folders {
                            if let Err(error) =
                                sync::sync_folder_by_id(&ctx, conn, account, &folder_id).await
                            {
                                debug!(%error, "post-op folder resync failed");
                            }
                        }
                        Ok(())
                    })
                })
                .await;
            if let Err(error) = &result {
                debug!(%error, "op queue pass failed");
            }
            if let Some(reply) = reply {
                let _ = reply.send(result);
            }
        }
        AccountCommand::KeepAlive => {
            let account = match worker.load_account().await {
                Ok(account) => account,
                Err(error) => {
                    debug!(%error, "keepalive account load failed");
                    return;
                }
            };
            if account.capabilities.idle {
                // The IDLE session keeps the server happy; the worker may sit
                // idle without a keepalive.
                return;
            }
            let result = worker
                .run_conn_read(|conn, _account, _ctx| Box::pin(async move { conn.noop().await }))
                .await;
            if let Err(error) = result {
                debug!(%error, "IMAP keepalive NOOP failed");
            }
        }
        AccountCommand::Reconnect => {
            worker.conn = None;
            worker.next_attempt = Instant::now();
        }
    }
}

/* ─────────────────────────────── IDLE ─────────────────────────────── */

async fn idle_loop(
    ctx: CoreContext,
    account_id: String,
    tx: mpsc::Sender<AccountCommand>,
    cancel: CancellationToken,
    config: EngineConfig,
) {
    let mut backoff = Backoff::new(Duration::from_secs(5), Duration::from_secs(300));
    loop {
        if cancel.is_cancelled() {
            break;
        }
        let account = match load_account(&ctx, &account_id).await {
            Ok(account) => account,
            Err(error) => {
                debug!(%error, account = %account_id, "IDLE account load failed");
                sleep(Duration::from_secs(30)).await;
                continue;
            }
        };
        if !account.capabilities.idle {
            // No IDLE: poll INBOX on the configured cadence. The worker
            // connection is kept alive with a separate NOOP ticker.
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = sleep(crate::engine::poll_interval(config.no_idle_poll_interval)) => {}
            }
            let (reply, _rx) = oneshot::channel();
            let _ = tx.send(AccountCommand::SyncFolder(None, reply)).await;
            continue;
        }

        let timeouts = crate::imap::conn::ConnectTimeouts {
            command: config.command_timeout,
            fetch: config.fetch_timeout,
            io_read: config.io_read_timeout,
            io_write: config.io_write_timeout,
            ..crate::imap::conn::ConnectTimeouts::default()
        };
        let connection =
            match crate::imap::conn::connect_account_with_timeouts(&ctx, &account, timeouts).await {
                Ok(connection) => connection,
            Err(error) => {
                debug!(%error, account = %account_id, "IDLE connection failed");
                let delay = backoff.next_delay();
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = sleep(delay) => {}
                }
                continue;
            }
        };
        backoff.reset();
        // The cached capabilities may be stale (an account added before its
        // first sync). Trust the live CAPABILITY response.
        let capabilities = connection.caps.merge_into(&account);
        if let Err(error) = persist_capabilities(&ctx, &account, capabilities.clone()).await {
            debug!(%error, account = %account_id, "could not persist IMAP capabilities");
        }
        if !capabilities.idle {
            debug!(
                account = %account_id,
                "server does not advertise IDLE; polling INBOX instead"
            );
            connection.logout().await;
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = sleep(config.no_idle_poll_interval) => {}
            }
            let (reply, _rx) = oneshot::channel();
            let _ = tx.send(AccountCommand::SyncFolder(None, reply)).await;
            continue;
        }
        if let Err(error) = run_idle_session(&tx, connection).await {
            if cancel.is_cancelled() {
                break;
            }
            debug!(%error, account = %account_id, "IDLE session ended");
            if idle_unsupported(&error) {
                // Some servers advertise IDLE but reject the command (NetEase
                // 163 answers "command not support"). Poll from now on.
                debug!(account = %account_id, "IDLE rejected; switching to polling");
                let mut caps = account.capabilities.clone();
                caps.idle = false;
                let _ = persist_capabilities(&ctx, &account, caps).await;
            }
            let delay = backoff.next_delay();
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = sleep(delay) => {}
            }
        }
    }
    debug!(account = %account_id, "idle loop stopped");
}

/// How long one IDLE wait may last before it is re-issued (RFC 2177 requires
/// re-issuing before 29 minutes).
const IDLE_WAIT: Duration = Duration::from_secs(25 * 60);

async fn run_idle_session(
    tx: &mpsc::Sender<AccountCommand>,
    mut connection: ImapConnection,
) -> Result<()> {
    connection.select("INBOX").await?;
    connection.touch();
    let control = connection.timeout_control();
    let command_timeout = connection.command_timeout();
    let mut session = connection.session;
    loop {
        // IDLE is a waiting command, not a request/response command: the
        // server may stay silent for the whole wait, so the per-read socket
        // watchdog must not apply. `init`/`done` themselves are still bounded
        // by the ordinary command budget below.
        control.suspend_read_timeout();
        let mut handle = session.idle();
        match tokio::time::timeout(command_timeout, handle.init()).await {
            Ok(result) => result.map_err(crate::util::imap_error)?,
            Err(_) => {
                control.poison();
                return Err(AppError::network(
                    "IMAP command timeout: IDLE did not start in time",
                ));
            }
        }
        let (future, stop) = handle.wait_with_timeout(IDLE_WAIT);
        let outcome = future.await.map_err(crate::util::imap_error)?;
        drop(stop);
        // The wait is over; from here on reads are ordinary command reads.
        control.resume_read_timeout();
        session = match tokio::time::timeout(command_timeout, handle.done()).await {
            Ok(result) => result.map_err(crate::util::imap_error)?,
            Err(_) => {
                control.poison();
                return Err(AppError::network(
                    "IMAP command timeout: IDLE did not terminate in time",
                ));
            }
        };
        match outcome {
            async_imap::extensions::idle::IdleResponse::NewData(_) => {
                let (reply, _rx) = oneshot::channel();
                let _ = tx.send(AccountCommand::SyncFolder(None, reply)).await;
            }
            async_imap::extensions::idle::IdleResponse::Timeout => {
                // Re-issue IDLE (RFC 2177 requires < 29 minutes). Probe the
                // socket first: a server may have dropped it during the long
                // wait without telling us.
                match tokio::time::timeout(command_timeout, session.noop()).await {
                    Ok(result) => result.map_err(crate::util::imap_error)?,
                    Err(_) => {
                        control.poison();
                        return Err(AppError::network(
                            "IMAP command timeout: IDLE keepalive NOOP timed out",
                        ));
                    }
                }
            }
            async_imap::extensions::idle::IdleResponse::ManualInterrupt => {
                return Err(AppError::network("IDLE interrupted"));
            }
        }
    }
}

/// NOOP keepalive for providers without IDLE support (NetEase 163 closes idle
/// sockets in well under two minutes). Runs until the account actor shuts down.
async fn keepalive_loop(
    account_id: String,
    tx: mpsc::Sender<AccountCommand>,
    cancel: CancellationToken,
    interval: Duration,
) {
    if interval.is_zero() {
        return;
    }
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = sleep(interval) => {}
        }
        if tx.send(AccountCommand::KeepAlive).await.is_err() {
            break;
        }
    }
    debug!(account = %account_id, "keepalive loop stopped");
}

/* ───────────────────────────── pollers ────────────────────────────── */

async fn poll_loop(
    ctx: CoreContext,
    _account_id: String,
    tx: mpsc::Sender<AccountCommand>,
    cancel: CancellationToken,
) {
    loop {
        let minutes = ctx
            .settings
            .current()
            .mail
            .other_folders_poll_minutes
            .max(1);
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = sleep(Duration::from_secs(u64::from(minutes) * 60)) => {}
        }
        if tx.send(AccountCommand::SyncOtherFolders).await.is_err() {
            break;
        }
    }
}

async fn ops_ticker(
    ctx: CoreContext,
    account_id: String,
    tx: Option<mpsc::Sender<AccountCommand>>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = sleep(Duration::from_secs(10)) => {}
        }
        match &tx {
            Some(tx) => {
                if tx
                    .send(AccountCommand::ExecuteOps { reply: None })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            None => {
                if let Ok(account) = load_account(&ctx, &account_id).await {
                    if let Err(error) = ops::executor::run_due_local(&ctx, &account).await {
                        debug!(%error, "local op queue pass failed");
                    }
                }
            }
        }
    }
}

async fn outbox_loop(ctx: CoreContext, account_id: String, cancel: CancellationToken) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = sleep(Duration::from_secs(20)) => {}
        }
        match load_account(&ctx, &account_id).await {
            Ok(account) => {
                if let Err(error) = smtp::outbox::process_due(&ctx, &account).await {
                    debug!(%error, "outbox pass failed");
                }
            }
            Err(error) => debug!(%error, "outbox account load failed"),
        }
    }
}

async fn pop3_loop(ctx: CoreContext, account_id: String, cancel: CancellationToken) {
    let mut first = true;
    loop {
        if !first {
            let minutes = ctx
                .settings
                .current()
                .mail
                .other_folders_poll_minutes
                .max(1);
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = sleep(Duration::from_secs(u64::from(minutes) * 60)) => {}
            }
        }
        first = false;
        if let Ok(account) = load_account(&ctx, &account_id).await {
            if let Err(error) = pop3::sync::sync_account(&ctx, &account).await {
                debug!(%error, account = %account_id, "POP3 sync failed");
            }
        }
    }
}

/* ────────────────────────────── Graph ─────────────────────────────── */

/// Per-account worker for Microsoft Graph accounts.
///
/// Commands arrive on the same channel as the IMAP worker, so `sync_now`,
/// `fetch_body` and `flush_ops` keep working unchanged.  When idle the loop
/// performs a periodic full refresh on the existing no-IDLE poll cadence; a
/// failure backs off exponentially with the shared [`Backoff`].  There is no
/// IDLE and no keepalive: Graph is request/response, so a long-poll session
/// would only hold a socket open.
async fn graph_loop(
    ctx: CoreContext,
    account_id: String,
    mut rx: mpsc::Receiver<AccountCommand>,
    cancel: CancellationToken,
    config: EngineConfig,
) {
    let mut backoff = Backoff::new(Duration::from_secs(5), Duration::from_secs(300));
    // Absolute deadline, not a per-iteration delay. The op-queue ticker sends a
    // command every 10 seconds; resetting a 120-second sleep on every command
    // starved the poll timer forever, so automatic Graph sync never ran and new
    // mail only appeared after a manual sync.
    let mut next_poll =
        TokioInstant::now() + crate::engine::poll_interval(config.no_idle_poll_interval);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            command = rx.recv() => {
                let Some(command) = command else { break };
                let explicit_sync = matches!(
                    command,
                    AccountCommand::SyncAll(_) | AccountCommand::SyncFolder(_, _)
                );
                handle_graph_command(&ctx, &account_id, command).await;
                backoff.reset();
                if explicit_sync {
                    // The user asked for a sync; the timer restarts from now.
                    next_poll = TokioInstant::now()
                        + crate::engine::poll_interval(config.no_idle_poll_interval);
                }
            }
            _ = sleep_until(next_poll) => {
                debug!(account = %account_id, "Graph poll tick");
                match graph_sync_account(&ctx, &account_id).await {
                    Ok(()) => {
                        backoff.reset();
                        next_poll = TokioInstant::now()
                            + crate::engine::poll_interval(config.no_idle_poll_interval);
                    }
                    Err(error) => {
                        let mut next = backoff.next_delay();
                        // A rate limit or server error means the service
                        // answered and asked us to slow down; retrying five
                        // seconds later would hammer it. Never come back
                        // sooner than the normal poll cadence, and honour a
                        // `Retry-After` hint plus a small margin.
                        if matches!(error, nuncio_core::AppError::Service(_)) {
                            next = next.max(crate::engine::poll_interval(
                                config.no_idle_poll_interval,
                            ));
                        }
                        if let Some(seconds) = crate::graph::take_retry_after_secs() {
                            next = next.max(Duration::from_secs(seconds + 5));
                        }
                        next_poll = TokioInstant::now() + next;
                        // Warn, not debug: a release build logs at info, and a
                        // silently failing Graph loop looks exactly like "new
                        // mail never arrives".
                        warn!(
                            account = %account_id,
                            %error,
                            retry_seconds = next.as_secs(),
                            "Graph sync failed; backing off"
                        );
                    }
                }
            }
        }
    }
    debug!(account = %account_id, "Graph loop stopped");
}

/// Full Graph sync: reconcile folders, then every Inbox/favourite/
/// sync-enabled folder.
///
/// This mirrors `sync::sync_account_with`, which cannot be reused directly
/// because it requires an `ImapConnection`.  The per-folder work and the
/// notification step are the same shared functions.
async fn graph_sync_account(ctx: &CoreContext, account_id: &str) -> Result<()> {
    debug!(account = %account_id, "Graph sync start");
    let account = load_account(ctx, account_id).await?;
    if !crate::graph::uses_graph(&account) {
        debug!(account = %account_id, "Graph sync skipped: account is not a Graph account");
        return Ok(());
    }
    ctx.sync_status
        .begin(&account.id, nuncio_core::models::SyncModule::Mail, None);

    let all_folders = match sync::folders::sync_graph(ctx, &account).await {
        Ok(folders) => folders,
        Err(error) => {
            ctx.sync_status.fail(
                &account.id,
                nuncio_core::models::SyncModule::Mail,
                error.message(),
            );
            return Err(error);
        }
    };

    let mut targets: Vec<nuncio_core::models::Folder> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |folder: &nuncio_core::models::Folder, targets: &mut Vec<_>| {
        if folder.is_selectable && seen.insert(folder.id.clone()) {
            targets.push(folder.clone());
        }
    };
    if let Some(inbox) = all_folders
        .iter()
        .find(|folder| folder.role == FolderRole::Inbox)
    {
        push(inbox, &mut targets);
    }
    for folder in all_folders.iter().filter(|folder| folder.is_favorite) {
        push(folder, &mut targets);
    }
    for folder in all_folders.iter().filter(|folder| folder.sync_enabled) {
        push(folder, &mut targets);
    }

    let mut first_error: Option<AppError> = None;
    for folder in &targets {
        match sync::messages::sync_folder_graph(ctx, &account, folder).await {
            Ok(outcome) => {
                if let Err(error) = sync::notify::after_new_mail(
                    ctx,
                    &account,
                    folder,
                    &outcome.new_messages,
                    outcome.initial,
                )
                .await
                {
                    warn!(%error, folder = %folder.remote_path, "notification step failed");
                }
            }
            Err(error) => {
                warn!(%error, folder = %folder.remote_path, "Graph folder sync failed");
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }

    let now = nuncio_core::models::now();
    let id = account.id.clone();
    ctx.db
        .write(move |db| repo::accounts::touch_synced_at(db, &id, now))
        .await?;

    match first_error {
        Some(error) => {
            ctx.sync_status.fail(
                &account.id,
                nuncio_core::models::SyncModule::Mail,
                error.message(),
            );
            Err(error)
        }
        None => {
            ctx.sync_status
                .finish(&account.id, nuncio_core::models::SyncModule::Mail);
            // A completed Graph sync proves the credential works, so clear any
            // stale error left on the account row.  Without this, an IMAP
            // failure recorded before the mailbox moved to Graph (or a single
            // transient 401) kept the sidebar showing "sign-in expired"
            // forever: only the IMAP worker ever reset the state, and a Graph
            // account never runs one.
            if account.state != AccountState::Ok {
                let id = account.id.clone();
                if let Err(error) = ctx
                    .db
                    .write(move |db| repo::accounts::update_state(db, &id, AccountState::Ok, None))
                    .await
                {
                    debug!(%error, account = %account.id, "cannot clear the account state");
                } else {
                    events::emit_accounts_changed(ctx.events.as_ref(), vec![account.id.clone()]);
                }
            }
            info!(
                account = %account.id,
                folders = targets.len(),
                "Graph mail sync finished"
            );
            Ok(())
        }
    }
}

/// Handle one actor command on the Graph path.
async fn handle_graph_command(ctx: &CoreContext, account_id: &str, command: AccountCommand) {
    let account = match load_account(ctx, account_id).await {
        Ok(account) => account,
        Err(error) => {
            respond_graph_error(command, &error);
            return;
        }
    };
    match command {
        AccountCommand::SyncAll(reply) => {
            let _ = reply.send(graph_sync_account(ctx, account_id).await);
        }
        AccountCommand::SyncFolder(folder_id, reply) => {
            let result = async {
                let folder = match folder_id {
                    Some(folder_id) => {
                        let id = folder_id;
                        ctx.db
                            .read(move |db| repo::folders::get_required(db, &id))
                            .await?
                    }
                    None => {
                        let folders = sync::load_folders(ctx, account_id).await?;
                        folders
                            .into_iter()
                            .find(|folder| folder.role == FolderRole::Inbox)
                            .ok_or_else(|| AppError::not_found("account has no Inbox folder"))?
                    }
                };
                ctx.sync_status
                    .begin(&account.id, nuncio_core::models::SyncModule::Mail, None);
                match sync::messages::sync_folder_graph(ctx, &account, &folder).await {
                    Ok(outcome) => {
                        // Notifications are best-effort and must not leave the
                        // sync indicator stuck in progress.
                        if let Err(error) = sync::notify::after_new_mail(
                            ctx,
                            &account,
                            &folder,
                            &outcome.new_messages,
                            outcome.initial,
                        )
                        .await
                        {
                            warn!(%error, folder = %folder.remote_path, "notification step failed");
                        }
                        ctx.sync_status
                            .finish(&account.id, nuncio_core::models::SyncModule::Mail);
                        Ok(())
                    }
                    Err(error) => {
                        ctx.sync_status.fail(
                            &account.id,
                            nuncio_core::models::SyncModule::Mail,
                            error.message(),
                        );
                        Err(error)
                    }
                }
            }
            .await;
            let _ = reply.send(result);
        }
        AccountCommand::SyncOtherFolders => {
            let result: Result<()> = async {
                let folders = sync::load_folders(ctx, account_id).await?;
                for folder in folders.into_iter().filter(|folder| {
                    folder.sync_enabled && folder.is_selectable && folder.role != FolderRole::Inbox
                }) {
                    if let Err(error) =
                        sync::messages::sync_folder_graph(ctx, &account, &folder).await
                    {
                        debug!(
                            %error,
                            folder = %folder.remote_path,
                            "Graph other-folder sync failed"
                        );
                    }
                }
                Ok(())
            }
            .await;
            if let Err(error) = result {
                debug!(%error, "Graph other-folder poll failed");
            }
        }
        AccountCommand::FetchBody { message_id, reply } => {
            let result = body::fetch_body_graph(ctx, &account, &message_id).await;
            let _ = reply.send(result);
        }
        AccountCommand::FetchPart { reply, .. } => {
            // Graph attachments need the separate `/attachments/{id}/$value`
            // endpoint, which is not wired yet; fail loudly rather than
            // returning an empty file.
            let _ = reply.send(Err(AppError::unsupported(
                "attachment download is not yet implemented for Microsoft Graph accounts",
            )));
        }
        AccountCommand::Scan { reply, .. } => {
            let _ = reply.send(Err(AppError::unsupported(
                "header scan is not available for Microsoft Graph accounts",
            )));
        }
        AccountCommand::Search { reply, .. } => {
            // Graph `$search` is not wired; the caller already falls back to
            // the local full-text index.
            let _ = reply.send(Err(AppError::unsupported(
                "server-side search is not implemented for Microsoft Graph accounts",
            )));
        }
        AccountCommand::Append { reply, .. } => {
            let _ = reply.send(Err(AppError::unsupported(
                "APPEND is not available for Microsoft Graph accounts",
            )));
        }
        AccountCommand::ListFolders(reply) => {
            let result = sync::folders::sync_graph(ctx, &account).await;
            let _ = reply.send(result);
        }
        AccountCommand::FolderCreate {
            parent_id,
            name,
            reply,
        } => {
            let result = async {
                let op = ops::executor::enqueue_folder_create_now(
                    ctx,
                    &account.id,
                    parent_id.as_deref(),
                    &name,
                )
                .await?;
                ops::executor::run_one_graph_now(ctx, &account, &op).await
            }
            .await;
            let _ = reply.send(result);
        }
        AccountCommand::FolderRename {
            folder_id,
            name,
            reply,
        } => {
            let result = async {
                let op =
                    ops::executor::enqueue_folder_rename_now(ctx, &account.id, &folder_id, &name)
                        .await?;
                ops::executor::run_one_graph_now(ctx, &account, &op).await
            }
            .await;
            let _ = reply.send(result);
        }
        AccountCommand::FolderDelete { folder_id, reply } => {
            let result = async {
                let op =
                    ops::executor::enqueue_folder_delete_now(ctx, &account.id, &folder_id).await?;
                ops::executor::run_one_graph_now(ctx, &account, &op).await
            }
            .await;
            let _ = reply.send(result);
        }
        AccountCommand::FolderSubscribe {
            folder_id,
            subscribe,
            reply,
        } => {
            let result = async {
                let op = ops::executor::enqueue_folder_subscribe_now(
                    ctx,
                    &account.id,
                    &folder_id,
                    subscribe,
                )
                .await?;
                ops::executor::run_one_graph_now(ctx, &account, &op).await
            }
            .await;
            let _ = reply.send(result);
        }
        AccountCommand::FolderMove { reply, .. } => {
            // Graph cannot reparent a folder in place; the UI would need to
            // create/copy.  Refuse instead of silently doing nothing.
            let _ = reply.send(Err(AppError::unsupported(
                "moving a folder is not supported for Microsoft Graph accounts",
            )));
        }
        AccountCommand::Expunge { reply, .. } => {
            let _ = reply.send(Err(AppError::unsupported(
                "emptying a folder is not supported for Microsoft Graph accounts",
            )));
        }
        AccountCommand::LoadOlder { folder_id, reply } => {
            let result = async {
                let folder = ctx
                    .db
                    .read(move |db| repo::folders::get_required(db, &folder_id))
                    .await?;
                if folder.account_id != account.id {
                    return Err(AppError::invalid_input("folder belongs to another account"));
                }
                sync::messages::load_older_graph(ctx, &account, &folder).await
            }
            .await;
            let _ = reply.send(result);
        }
        AccountCommand::ExecuteOps { reply } => {
            let result = async {
                let outcome = ops::executor::run_due_graph(ctx, &account).await?;
                for folder_id in outcome.resync_folders {
                    let folder = {
                        let id = folder_id.clone();
                        ctx.db
                            .read(move |db| repo::folders::get_required(db, &id))
                            .await
                    };
                    match folder {
                        Ok(folder) => {
                            if let Err(error) =
                                sync::messages::sync_folder_graph(ctx, &account, &folder).await
                            {
                                debug!(%error, "post-op Graph folder resync failed");
                            }
                        }
                        Err(error) => {
                            debug!(%error, folder = %folder_id, "resync folder vanished");
                        }
                    }
                }
                Ok(())
            }
            .await;
            if let Err(error) = &result {
                debug!(%error, "Graph op queue pass failed");
            }
            if let Some(reply) = reply {
                let _ = reply.send(result);
            }
        }
        AccountCommand::KeepAlive => {
            // Graph needs no keepalive; the periodic refresh is the heartbeat.
        }
        AccountCommand::Reconnect => {
            // The next periodic tick re-syncs; an explicit command has no
            // socket to drop.
        }
    }
}

/// Answer a command with an account-load error where the variant has a reply
/// channel; commands without one simply fail silently (they are background
/// triggers).
fn respond_graph_error(command: AccountCommand, error: &AppError) {
    let message = error.message();
    match command {
        AccountCommand::SyncAll(reply) => {
            let _ = reply.send(Err(AppError::internal(message)));
        }
        AccountCommand::SyncFolder(_, reply) => {
            let _ = reply.send(Err(AppError::internal(message)));
        }
        AccountCommand::FetchBody { reply, .. } => {
            let _ = reply.send(Err(AppError::internal(message)));
        }
        AccountCommand::FetchPart { reply, .. } => {
            let _ = reply.send(Err(AppError::internal(message)));
        }
        AccountCommand::Search { reply, .. } => {
            let _ = reply.send(Err(AppError::internal(message)));
        }
        AccountCommand::Scan { reply, .. } => {
            let _ = reply.send(Err(AppError::internal(message)));
        }
        AccountCommand::Append { reply, .. } => {
            let _ = reply.send(Err(AppError::internal(message)));
        }
        AccountCommand::ListFolders(reply) => {
            let _ = reply.send(Err(AppError::internal(message)));
        }
        AccountCommand::FolderCreate { reply, .. } => {
            let _ = reply.send(Err(AppError::internal(message)));
        }
        AccountCommand::FolderRename { reply, .. } => {
            let _ = reply.send(Err(AppError::internal(message)));
        }
        AccountCommand::FolderMove { reply, .. } => {
            let _ = reply.send(Err(AppError::internal(message)));
        }
        AccountCommand::FolderDelete { reply, .. } => {
            let _ = reply.send(Err(AppError::internal(message)));
        }
        AccountCommand::FolderSubscribe { reply, .. } => {
            let _ = reply.send(Err(AppError::internal(message)));
        }
        AccountCommand::Expunge { reply, .. } => {
            let _ = reply.send(Err(AppError::internal(message)));
        }
        AccountCommand::LoadOlder { reply, .. } => {
            let _ = reply.send(Err(AppError::internal(message)));
        }
        AccountCommand::ExecuteOps { reply } => {
            if let Some(reply) = reply {
                let _ = reply.send(Err(AppError::internal(message)));
            }
        }
        AccountCommand::SyncOtherFolders
        | AccountCommand::KeepAlive
        | AccountCommand::Reconnect => {}
    }
}

/// Persist the IMAP capability snapshot when it changed (the PIM-side flags
/// in it are preserved by `ServerCapabilities::merge_into`).
async fn persist_capabilities(
    ctx: &CoreContext,
    account: &Account,
    capabilities: nuncio_core::models::ProviderCapabilities,
) -> Result<()> {
    if capabilities == account.capabilities {
        return Ok(());
    }
    let account_id = account.id.clone();
    ctx.db
        .write(move |db| repo::accounts::set_capabilities(db, &account_id, &capabilities))
        .await
}

/// Servers that advertise IDLE but answer with a "not supported" style error.
fn idle_unsupported(error: &AppError) -> bool {
    let message = error.message().to_ascii_lowercase();
    message.contains("not support")
        || message.contains("unsupported")
        || message.contains("unknown command")
        || message.contains("invalid command")
}

async fn load_account(ctx: &CoreContext, account_id: &str) -> Result<Account> {
    let id = account_id.to_string();
    ctx.db
        .read(move |db| repo::accounts::get_required(db, &id))
        .await
}

#[cfg(test)]
mod tests {
    use super::Backoff;
    use std::time::Duration;

    #[test]
    fn backoff_grows_and_caps_across_consecutive_failures() {
        let mut backoff = Backoff::new(Duration::from_secs(5), Duration::from_secs(300));
        let delays: Vec<u64> = (0..8).map(|_| backoff.next_delay().as_secs()).collect();
        assert_eq!(delays, vec![5, 10, 20, 40, 80, 160, 300, 300]);
    }

    #[test]
    fn backoff_resets_only_on_success() {
        let mut backoff = Backoff::new(Duration::from_secs(5), Duration::from_secs(300));
        assert_eq!(backoff.next_delay(), Duration::from_secs(5));
        assert_eq!(backoff.next_delay(), Duration::from_secs(10));
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_secs(5));
    }
}
