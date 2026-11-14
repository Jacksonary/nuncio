//! Per-account actors: one serialized worker connection, one IDLE connection,
//! pollers for other folders, and the operation/outbox tickers.

use std::time::{Duration, Instant};

use nuncio_core::models::{Account, AccountState, FolderRole, ProviderKind};
use nuncio_core::{events, repo, AppError, CoreContext, Result};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::engine::EngineConfig;
use crate::imap::conn::ImapConnection;
use crate::ops::executor::{expire, store};
use crate::sync::body;
use crate::util::{imap_error, is_connection_error};
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
        reply: oneshot::Sender<Result<()>>,
    },
    ExecuteOps {
        reply: Option<oneshot::Sender<Result<()>>>,
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
    tasks.push(tokio::spawn(outbox_loop(
        ctx,
        account.id.clone(),
        cancel.clone(),
    )));

    info!(account = %account.id, pop3 = is_pop3, "mail account actor started");
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
        if self.conn.is_none() {
            let now = Instant::now();
            if now < self.next_attempt {
                self.short_circuited = true;
                return Err(AppError::network("IMAP is reconnecting"));
            }
            let connection = crate::imap::conn::connect_account(&self.ctx, account).await?;
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
                if is_connection_error(&error) {
                    debug!(
                        account = %self.account_id,
                        %error,
                        "stale IMAP session detected; reconnecting before use"
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
            Err(error) if !self.short_circuited && is_connection_error(error) => {
                debug!(
                    account = %self.account_id,
                    %error,
                    "IMAP command lost its connection; reconnecting and retrying once"
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
                        conn.session
                            .rename(&folder.remote_path, &new_remote)
                            .await
                            .map_err(imap_error)?;
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
                            .await
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
                _ = sleep(config.no_idle_poll_interval) => {}
            }
            let (reply, _rx) = oneshot::channel();
            let _ = tx.send(AccountCommand::SyncFolder(None, reply)).await;
            continue;
        }

        let connection = match crate::imap::conn::connect_account(&ctx, &account).await {
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

async fn run_idle_session(
    tx: &mpsc::Sender<AccountCommand>,
    mut connection: ImapConnection,
) -> Result<()> {
    connection.select("INBOX").await?;
    connection.touch();
    let mut session = connection.session;
    loop {
        let mut handle = session.idle();
        handle.init().await.map_err(crate::util::imap_error)?;
        let (future, stop) = handle.wait_with_timeout(Duration::from_secs(25 * 60));
        let outcome = future.await.map_err(crate::util::imap_error)?;
        drop(stop);
        session = handle.done().await.map_err(crate::util::imap_error)?;
        match outcome {
            async_imap::extensions::idle::IdleResponse::NewData(_) => {
                let (reply, _rx) = oneshot::channel();
                let _ = tx.send(AccountCommand::SyncFolder(None, reply)).await;
            }
            async_imap::extensions::idle::IdleResponse::Timeout => {
                // Re-issue IDLE (RFC 2177 requires < 29 minutes). Probe the
                // socket first: a server may have dropped it during the long
                // wait without telling us.
                session.noop().await.map_err(crate::util::imap_error)?;
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
