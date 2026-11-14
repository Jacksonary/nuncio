//! Public mail engine.
//!
//! `Engine` owns one actor per mail account. The command-facing `MailService`
//! (next stage) is expected to build on this API; all methods take and return
//! core contract types.
//!
//! ```text
//! Engine::new(ctx)
//!   .start().await?                       // spawn actors for every active account
//!   .sync_now(None, None).await?           // sync all accounts
//! ```
//!
//! The engine never mutates folder/message rows in ways the UI cannot observe:
//! every batch emits `mail://changed` and `sync://status` through the core event
//! sink, and every user-visible failure is reported as `op://failed`.

mod account_actor;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use nuncio_core::models::{Account, Folder, ProviderKind, ServerConfig};
use nuncio_core::traits::{MailAuth, MailSender};
use nuncio_core::{repo, AppError, CoreContext, Result};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::smtp;
use crate::sync;
use account_actor::{AccountCommand, AccountRuntime};

/// Tunables for connection liveness and transient-failure reporting.
///
/// The defaults match what real providers need: NetEase 163 silently closes
/// sockets that stay idle for under two minutes, so a session is probed with
/// NOOP well before that and a NOOP keepalive runs while the account has no
/// IDLE support. Tests shorten the intervals through [`Engine::with_config`].
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Probe (NOOP) a session idle for at least this long before using it.
    pub liveness_probe_after: Duration,
    /// NOOP cadence for accounts without IDLE support.
    pub keepalive_interval: Duration,
    /// Poll cadence for INBOX when IDLE is unavailable.
    pub no_idle_poll_interval: Duration,
    /// Consecutive failed reconnects before the UI shows an error.
    pub error_after_failures: u32,
    /// Time without a working session before the UI shows an error.
    pub error_after: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            liveness_probe_after: Duration::from_secs(45),
            keepalive_interval: Duration::from_secs(60),
            no_idle_poll_interval: Duration::from_secs(120),
            error_after_failures: 3,
            error_after: Duration::from_secs(120),
        }
    }
}

struct EngineInner {
    ctx: CoreContext,
    accounts: Arc<Mutex<HashMap<String, AccountRuntime>>>,
    cancel: Mutex<CancellationToken>,
    started: Arc<AtomicBool>,
    config: EngineConfig,
}

/// Cloneable handle to the mail engine.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<EngineInner>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("accounts", &self.account_ids())
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Create the engine with the production defaults. Call
    /// [`Engine::start`] once the app is ready.
    pub fn new(ctx: CoreContext) -> Self {
        Self::with_config(ctx, EngineConfig::default())
    }

    /// Create the engine with explicit liveness/retry tunables.
    pub fn with_config(ctx: CoreContext, config: EngineConfig) -> Self {
        Self {
            inner: Arc::new(EngineInner {
                ctx,
                accounts: Arc::new(Mutex::new(HashMap::new())),
                cancel: Mutex::new(CancellationToken::new()),
                started: Arc::new(AtomicBool::new(false)),
                config,
            }),
        }
    }

    /// Access the shared core context (database, paths, event sink, ...).
    pub fn context(&self) -> &CoreContext {
        &self.inner.ctx
    }

    /// Spawn actors for all enabled mail accounts and the housekeeping task.
    /// Safe to call more than once.
    pub async fn start(&self) -> Result<()> {
        if self.inner.started.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        {
            let mut cancel = self
                .inner
                .cancel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if cancel.is_cancelled() {
                *cancel = CancellationToken::new();
            }
        }
        let accounts = self
            .inner
            .ctx
            .db
            .read(repo::accounts::list_mail_accounts)
            .await?;
        for account in accounts {
            if account.is_mail_active() {
                if let Err(error) = self.add_account(&account.id).await {
                    warn!(account = %account.id, %error, "could not start mail account");
                }
            }
        }
        self.spawn_housekeeping();
        info!("mail engine started");
        Ok(())
    }

    /// Start (or restart) one account. No-op for unknown/disabled accounts.
    pub async fn add_account(&self, account_id: &str) -> Result<()> {
        let account = self.load_account(account_id).await?;
        if !account.is_mail_active() {
            return Ok(());
        }
        self.spawn_account(&account).await?;
        if account.provider != ProviderKind::Pop3 {
            self.send_silent(account_id, AccountCommand::SyncAll(oneshot::channel().0));
        }
        Ok(())
    }

    /// Stop one account and drop its queued server operations.
    pub async fn remove_account(&self, account_id: &str) -> Result<()> {
        self.stop_runtime(account_id);
        let account_id_owned = account_id.to_string();
        self.inner
            .ctx
            .db
            .write(move |db| repo::pending_ops::cancel_for_account(db, &account_id_owned))
            .await?;
        self.inner.ctx.sync_status.remove_account(account_id);
        info!(account = account_id, "mail account stopped");
        Ok(())
    }

    /// React to account settings/capability changes. Queued user operations
    /// survive the restart.
    pub async fn account_changed(&self, account_id: &str) -> Result<()> {
        self.stop_runtime(account_id);
        self.add_account(account_id).await
    }

    fn stop_runtime(&self, account_id: &str) {
        let runtime = self
            .inner
            .accounts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(account_id);
        if let Some(runtime) = runtime {
            runtime.abort();
        }
    }

    /// Stop all actors. The engine can be started again afterwards.
    pub async fn shutdown(&self) {
        self.inner
            .cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .cancel();
        let runtimes: Vec<AccountRuntime> = {
            let mut accounts = self
                .inner
                .accounts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            accounts.drain().map(|(_, runtime)| runtime).collect()
        };
        for runtime in runtimes {
            runtime.abort();
        }
        self.inner.started.store(false, Ordering::SeqCst);
        info!("mail engine stopped");
    }

    /// Sync one account (or all), optionally only one folder.
    pub async fn sync_now(&self, account_id: Option<&str>, folder_id: Option<&str>) -> Result<()> {
        let accounts = match account_id {
            Some(id) => vec![self.load_account(id).await?],
            None => {
                self.inner
                    .ctx
                    .db
                    .read(repo::accounts::list_mail_accounts)
                    .await?
            }
        };
        let mut first_error = None;
        for account in accounts
            .into_iter()
            .filter(|account| account.is_mail_active())
        {
            let result = if account.provider == ProviderKind::Pop3 {
                pop3_sync(&self.inner.ctx, &account).await
            } else {
                self.call_sync(&account.id, folder_id).await
            };
            if let Err(error) = result {
                warn!(account = %account.id, %error, "sync request failed");
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Sync a single local folder id.
    pub async fn sync_folder(&self, folder_id: &str) -> Result<()> {
        let folder = self.load_folder(folder_id).await?;
        self.sync_now(Some(&folder.account_id), Some(folder_id))
            .await
    }

    /// Download (or reuse) the raw RFC822 body; returns its local path.
    pub async fn fetch_body(&self, message_id: &str) -> Result<std::path::PathBuf> {
        match self.fetch_body_once(message_id).await {
            Ok(path) => Ok(path),
            Err(error) if error.code() == "protocol" || error.code() == "notFound" => {
                // The UID may be stale after a server-side move; refresh the
                // folder once and retry with the new local row.
                let stored = self
                    .inner
                    .ctx
                    .db
                    .read({
                        let message_id = message_id.to_string();
                        move |db| repo::messages::get(db, &message_id)
                    })
                    .await?
                    .ok_or_else(|| {
                        AppError::not_found(format!("message {message_id} not found"))
                    })?;
                self.sync_folder(&stored.summary.folder_id).await?;
                self.fetch_body_once(message_id).await
            }
            Err(error) => Err(error),
        }
    }

    async fn fetch_body_once(&self, message_id: &str) -> Result<std::path::PathBuf> {
        let stored = self
            .inner
            .ctx
            .db
            .read({
                let message_id = message_id.to_string();
                move |db| repo::messages::get(db, &message_id)
            })
            .await?
            .ok_or_else(|| AppError::not_found(format!("message {message_id} not found")))?;
        if let Some(path) = stored.body_path.as_deref() {
            let path = std::path::PathBuf::from(path);
            if tokio::fs::metadata(&path).await.is_ok() {
                return Ok(path);
            }
        }
        let account = self.load_account(&stored.summary.account_id).await?;
        if account.provider == ProviderKind::Pop3 {
            return Err(AppError::not_found(
                "POP3 message body is not cached locally",
            ));
        }
        let (sender, receiver) = oneshot::channel();
        let sent = self
            .sender(&account.id)
            .await?
            .send(AccountCommand::FetchBody {
                message_id: message_id.to_string(),
                reply: sender,
            })
            .await;
        if sent.is_err() {
            return Err(AppError::internal("mail worker stopped"));
        }
        receiver
            .await
            .map_err(|_| AppError::internal("mail worker dropped the reply"))?
    }

    /// Fetch attachment bytes by IMAP part path or flattened index.
    ///
    /// Numeric indexes are resolved against the cached `.eml` (downloaded on
    /// demand); dotted part paths can be fetched directly from the server when
    /// the body is not cached.
    pub async fn fetch_attachment(&self, message_id: &str, part: &str) -> Result<Vec<u8>> {
        let mut stored = self
            .inner
            .ctx
            .db
            .read({
                let message_id = message_id.to_string();
                move |db| repo::messages::get_required(db, &message_id)
            })
            .await?;
        let wants_index = part.parse::<u32>().is_ok();
        let cached_file = match stored.body_path.as_deref() {
            Some(path) => tokio::fs::metadata(path).await.is_ok(),
            None => false,
        };
        if wants_index && !cached_file {
            self.fetch_body(message_id).await?;
            stored = self
                .inner
                .ctx
                .db
                .read({
                    let message_id = message_id.to_string();
                    move |db| repo::messages::get_required(db, &message_id)
                })
                .await?;
        }
        if let Some(path) = stored.body_path.as_deref() {
            if let Ok(raw) = tokio::fs::read(path).await {
                if let Some(part_path) = nuncio_mime::PartPath::parse(part) {
                    return nuncio_mime::extract_part(&raw, &part_path);
                }
                if let Ok(index) = part.parse::<u32>() {
                    return nuncio_mime::extract_part_by_index(&raw, index);
                }
            }
        }
        let (sender, receiver) = oneshot::channel();
        let sender_tx = self.sender_for_message(message_id).await?;
        sender_tx
            .send(AccountCommand::FetchPart {
                message_id: message_id.to_string(),
                part: part.to_string(),
                reply: sender,
            })
            .await
            .map_err(|_| AppError::internal("mail worker stopped"))?;
        receiver
            .await
            .map_err(|_| AppError::internal("mail worker dropped the reply"))?
    }

    /// Describe attachments of a message (downloading the body if needed).
    pub async fn attachment_parts(
        &self,
        message_id: &str,
    ) -> Result<Vec<sync::body::AttachmentPart>> {
        // Ensure the body is cached before parsing it.
        let stored = self
            .inner
            .ctx
            .db
            .read({
                let message_id = message_id.to_string();
                move |db| repo::messages::get(db, &message_id)
            })
            .await?
            .ok_or_else(|| AppError::not_found(format!("message {message_id} not found")))?;
        let has_body = stored
            .body_path
            .as_deref()
            .map(|path| std::path::Path::new(path).exists())
            .unwrap_or(false);
        if !has_body {
            let _ = self.fetch_body(message_id).await?;
        }
        sync::body::attachment_parts(&self.inner.ctx, message_id).await
    }

    /// Server-side IMAP search fallback; matching metadata is cached locally.
    pub async fn server_search(
        &self,
        account_id: &str,
        folder_id: Option<&str>,
        criteria: &str,
    ) -> Result<Vec<String>> {
        let account = self.load_account(account_id).await?;
        if account.provider == ProviderKind::Pop3 {
            return Ok(Vec::new());
        }
        let (sender, receiver) = oneshot::channel();
        self.sender(account_id)
            .await?
            .send(AccountCommand::Search {
                folder_id: folder_id.map(str::to_string),
                criteria: criteria.to_string(),
                reply: sender,
            })
            .await
            .map_err(|_| AppError::internal("mail worker stopped"))?;
        receiver
            .await
            .map_err(|_| AppError::internal("mail worker dropped the reply"))?
    }

    /// APPEND raw bytes to a folder.
    pub async fn append(
        &self,
        account_id: &str,
        folder_id: &str,
        bytes: Vec<u8>,
        flags: Vec<String>,
    ) -> Result<()> {
        let flag_string = if flags.is_empty() {
            None
        } else {
            Some(format!("({})", flags.join(" ")))
        };
        let (sender, receiver) = oneshot::channel();
        self.sender(account_id)
            .await?
            .send(AccountCommand::Append {
                folder_id: folder_id.to_string(),
                bytes,
                flags: flag_string,
                reply: sender,
            })
            .await
            .map_err(|_| AppError::internal("mail worker stopped"))?;
        receiver
            .await
            .map_err(|_| AppError::internal("mail worker dropped the reply"))?
    }

    /// Refresh the folder list from the server.
    pub async fn list_folders(&self, account_id: &str) -> Result<Vec<Folder>> {
        let account = self.load_account(account_id).await?;
        if account.provider == ProviderKind::Pop3 {
            return sync::load_folders(&self.inner.ctx, account_id).await;
        }
        let (sender, receiver) = oneshot::channel();
        self.sender(account_id)
            .await?
            .send(AccountCommand::ListFolders(sender))
            .await
            .map_err(|_| AppError::internal("mail worker stopped"))?;
        receiver
            .await
            .map_err(|_| AppError::internal("mail worker dropped the reply"))?
    }

    /// Create a folder (parent null = top level).
    pub async fn create_folder(
        &self,
        account_id: &str,
        parent_id: Option<&str>,
        name: &str,
    ) -> Result<()> {
        let (sender, receiver) = oneshot::channel();
        self.sender(account_id)
            .await?
            .send(AccountCommand::FolderCreate {
                parent_id: parent_id.map(str::to_string),
                name: name.to_string(),
                reply: sender,
            })
            .await
            .map_err(|_| AppError::internal("mail worker stopped"))?;
        receiver
            .await
            .map_err(|_| AppError::internal("mail worker dropped the reply"))?
    }

    pub async fn rename_folder(&self, account_id: &str, folder_id: &str, name: &str) -> Result<()> {
        let (sender, receiver) = oneshot::channel();
        self.sender(account_id)
            .await?
            .send(AccountCommand::FolderRename {
                folder_id: folder_id.to_string(),
                name: name.to_string(),
                reply: sender,
            })
            .await
            .map_err(|_| AppError::internal("mail worker stopped"))?;
        receiver
            .await
            .map_err(|_| AppError::internal("mail worker dropped the reply"))?
    }

    /// Move a folder to a different parent (IMAP `RENAME` to the new path).
    pub async fn move_folder(
        &self,
        account_id: &str,
        folder_id: &str,
        new_parent_id: Option<&str>,
    ) -> Result<()> {
        let (sender, receiver) = oneshot::channel();
        self.sender(account_id)
            .await?
            .send(AccountCommand::FolderMove {
                folder_id: folder_id.to_string(),
                new_parent_id: new_parent_id.map(str::to_string),
                reply: sender,
            })
            .await
            .map_err(|_| AppError::internal("mail worker stopped"))?;
        receiver
            .await
            .map_err(|_| AppError::internal("mail worker dropped the reply"))?
    }

    pub async fn delete_folder(&self, account_id: &str, folder_id: &str) -> Result<()> {
        let (sender, receiver) = oneshot::channel();
        self.sender(account_id)
            .await?
            .send(AccountCommand::FolderDelete {
                folder_id: folder_id.to_string(),
                reply: sender,
            })
            .await
            .map_err(|_| AppError::internal("mail worker stopped"))?;
        receiver
            .await
            .map_err(|_| AppError::internal("mail worker dropped the reply"))?
    }

    pub async fn subscribe_folder(
        &self,
        account_id: &str,
        folder_id: &str,
        subscribe: bool,
    ) -> Result<()> {
        let (sender, receiver) = oneshot::channel();
        self.sender(account_id)
            .await?
            .send(AccountCommand::FolderSubscribe {
                folder_id: folder_id.to_string(),
                subscribe,
                reply: sender,
            })
            .await
            .map_err(|_| AppError::internal("mail worker stopped"))?;
        receiver
            .await
            .map_err(|_| AppError::internal("mail worker dropped the reply"))?
    }

    /// Permanently empty a folder (UID STORE \Deleted + EXPUNGE).
    pub async fn expunge_folder(&self, account_id: &str, folder_id: &str) -> Result<()> {
        let (sender, receiver) = oneshot::channel();
        self.sender(account_id)
            .await?
            .send(AccountCommand::Expunge {
                folder_id: folder_id.to_string(),
                reply: sender,
            })
            .await
            .map_err(|_| AppError::internal("mail worker stopped"))?;
        receiver
            .await
            .map_err(|_| AppError::internal("mail worker dropped the reply"))?
    }

    /// Run the pending operation queue immediately and wait for completion.
    pub async fn flush_ops(&self, account_id: &str) -> Result<()> {
        let account = self.load_account(account_id).await?;
        if account.provider == ProviderKind::Pop3 {
            crate::ops::executor::run_due_local(&self.inner.ctx, &account).await?;
            return Ok(());
        }
        let (sender, receiver) = oneshot::channel();
        self.sender(account_id)
            .await?
            .send(AccountCommand::ExecuteOps {
                reply: Some(sender),
            })
            .await
            .map_err(|_| AppError::internal("mail worker stopped"))?;
        receiver
            .await
            .map_err(|_| AppError::internal("mail worker dropped the reply"))?
    }

    /// Process due outbox items immediately.
    pub async fn process_outbox(&self, account_id: &str) -> Result<usize> {
        let account = self.load_account(account_id).await?;
        smtp::outbox::process_due(&self.inner.ctx, &account).await
    }

    /// POP3: read the "leave messages on server" preference (default true).
    pub async fn pop3_leave_on_server(&self, account_id: &str) -> Result<bool> {
        let account_id = account_id.to_string();
        let value = self
            .inner
            .ctx
            .db
            .read(move |db| repo::sync_state::get(db, &account_id, "pop3", "leave_on_server"))
            .await?;
        Ok(value.map(|value| value != "false").unwrap_or(true))
    }

    /// POP3: keep downloaded messages on the server (default) or delete them
    /// after a successful download.
    pub async fn set_pop3_leave_on_server(&self, account_id: &str, leave: bool) -> Result<()> {
        let account_id = account_id.to_string();
        let value = if leave { "true" } else { "false" };
        self.inner
            .ctx
            .db
            .write(move |db| {
                repo::sync_state::set(db, &account_id, "pop3", "leave_on_server", value)
            })
            .await
    }

    /// Force a reconnect/catch-up after sleep or network recovery.
    pub async fn reconnect_all(&self) {
        let senders: Vec<_> = {
            self.inner
                .accounts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .values()
                .map(|runtime| runtime.tx.clone())
                .collect()
        };
        for sender in senders {
            let _ = sender.send(AccountCommand::Reconnect).await;
            let _ = sender
                .send(AccountCommand::SyncAll(oneshot::channel().0))
                .await;
        }
    }

    /// Test an IMAP configuration without creating an account.
    pub async fn test_imap(config: &ServerConfig, auth: &MailAuth) -> Result<()> {
        crate::imap::conn::test_config(config, auth)
            .await
            .map(|_| ())
    }

    /// Test an SMTP configuration without creating an account.
    pub async fn test_smtp(config: &ServerConfig, auth: &MailAuth) -> Result<()> {
        smtp::send::test_smtp(config, auth).await
    }

    /// Test a POP3 configuration without creating an account.
    pub async fn test_pop3(config: &ServerConfig, auth: &MailAuth) -> Result<()> {
        let mut client = crate::pop3::Pop3Client::connect(config, auth).await?;
        client.noop().await?;
        client.quit().await;
        Ok(())
    }

    /// Send raw RFC822 bytes for an account. Used by `compose_send` and iTIP.
    pub async fn send_message(
        &self,
        account_id: &str,
        rfc822: Vec<u8>,
        save_to_sent: bool,
    ) -> Result<()> {
        let account = self.load_account(account_id).await?;
        smtp::send::send_account_message(&self.inner.ctx, &account, &rfc822, None).await?;
        if save_to_sent && !account.capabilities.auto_saves_sent {
            self.stage_sent_copy(&account, &rfc822).await?;
        }
        Ok(())
    }

    async fn stage_sent_copy(&self, account: &Account, bytes: &[u8]) -> Result<()> {
        let account_id = account.id.clone();
        let sent = self
            .inner
            .ctx
            .db
            .read(move |db| {
                repo::folders::by_role(db, &account_id, nuncio_core::models::FolderRole::Sent)
            })
            .await?;
        let Some(sent) = sent else {
            debug!(account = %account.id, "no Sent folder; not staging a copy");
            return Ok(());
        };
        let path = self
            .inner
            .ctx
            .paths
            .staging_dir()
            .join(format!("sent-{}.eml", nuncio_core::models::new_id()));
        self.inner.ctx.paths.ensure_parent(&path)?;
        let write_path = path.clone();
        let bytes = bytes.to_vec();
        tokio::task::spawn_blocking(move || std::fs::write(&write_path, &bytes))
            .await
            .map_err(|error| AppError::internal(format!("staging task failed: {error}")))?
            .map_err(|error| AppError::io(format!("cannot stage sent copy: {error}")))?;
        let path_string = path.to_string_lossy().to_string();
        crate::ops::enqueue_append_to_sent(&self.inner.ctx, &account.id, &sent.id, &path_string)
            .await
    }

    fn account_ids(&self) -> Vec<String> {
        self.inner
            .accounts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    async fn spawn_account(&self, account: &Account) -> Result<()> {
        {
            let accounts = self
                .inner
                .accounts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if accounts.contains_key(&account.id) {
                return Ok(());
            }
        }
        let cancel = self
            .inner
            .cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let runtime = account_actor::spawn(
            self.inner.ctx.clone(),
            account.clone(),
            &cancel,
            self.inner.config.clone(),
        )
        .await?;
        self.inner
            .accounts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(account.id.clone(), runtime);
        Ok(())
    }

    fn spawn_housekeeping(&self) {
        let inner = self.inner.clone();
        let token = {
            let cancel = inner
                .cancel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            cancel.clone()
        };
        tokio::spawn(async move {
            let mut last_monotonic = Instant::now();
            let mut last_wall = Utc::now();
            let mut last_prune = Instant::now();
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                }
                let monotonic = last_monotonic.elapsed();
                let wall = Utc::now() - last_wall;
                last_monotonic = Instant::now();
                last_wall = Utc::now();
                let wall = wall.to_std().unwrap_or_default();
                let drift = wall.abs_diff(monotonic);
                if drift > Duration::from_secs(90) {
                    warn!(
                        monotonic = monotonic.as_secs(),
                        wall = wall.as_secs(),
                        "clock jump detected; reconnecting accounts"
                    );
                    let senders: Vec<_> = {
                        inner
                            .accounts
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .values()
                            .map(|runtime| runtime.tx.clone())
                            .collect()
                    };
                    for sender in senders {
                        let _ = sender.send(AccountCommand::Reconnect).await;
                        let _ = sender
                            .send(AccountCommand::SyncAll(oneshot::channel().0))
                            .await;
                    }
                }
                if last_prune.elapsed() > Duration::from_secs(6 * 3600) {
                    last_prune = Instant::now();
                    let cutoff = Utc::now() - chrono::Duration::days(7);
                    let _ = inner
                        .ctx
                        .db
                        .write(move |db| {
                            repo::pending_ops::prune_finished(db, cutoff)?;
                            repo::outbox::prune_sent(db, cutoff)?;
                            Ok(())
                        })
                        .await;
                }
            }
        });
    }

    async fn load_account(&self, account_id: &str) -> Result<Account> {
        let id = account_id.to_string();
        self.inner
            .ctx
            .db
            .read(move |db| repo::accounts::get_required(db, &id))
            .await
    }

    async fn load_folder(&self, folder_id: &str) -> Result<Folder> {
        let id = folder_id.to_string();
        self.inner
            .ctx
            .db
            .read(move |db| repo::folders::get_required(db, &id))
            .await
    }

    async fn sender(&self, account_id: &str) -> Result<tokio::sync::mpsc::Sender<AccountCommand>> {
        let sender = self
            .inner
            .accounts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(account_id)
            .map(|runtime| runtime.tx.clone());
        sender
            .ok_or_else(|| AppError::not_found(format!("mail account {account_id} is not running")))
    }

    async fn sender_for_message(
        &self,
        message_id: &str,
    ) -> Result<tokio::sync::mpsc::Sender<AccountCommand>> {
        let stored = self
            .inner
            .ctx
            .db
            .read({
                let message_id = message_id.to_string();
                move |db| repo::messages::get_required(db, &message_id)
            })
            .await?;
        self.sender(&stored.summary.account_id).await
    }

    fn send_silent(&self, account_id: &str, command: AccountCommand) {
        let sender = self
            .inner
            .accounts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(account_id)
            .map(|runtime| runtime.tx.clone());
        if let Some(sender) = sender {
            tokio::spawn(async move {
                let _ = sender.send(command).await;
            });
        }
    }

    async fn call_sync(&self, account_id: &str, folder_id: Option<&str>) -> Result<()> {
        let sender = self.sender(account_id).await?;
        let (reply_tx, reply_rx) = oneshot::channel();
        let command = match folder_id {
            Some(folder_id) => AccountCommand::SyncFolder(Some(folder_id.to_string()), reply_tx),
            None => AccountCommand::SyncAll(reply_tx),
        };
        sender
            .send(command)
            .await
            .map_err(|_| AppError::internal("mail worker stopped"))?;
        reply_rx
            .await
            .map_err(|_| AppError::internal("mail worker dropped the reply"))?
    }
}

async fn pop3_sync(ctx: &CoreContext, account: &Account) -> Result<()> {
    crate::pop3::sync::sync_account(ctx, account).await
}

#[async_trait]
impl MailSender for Engine {
    async fn send_raw(&self, account_id: &str, rfc822: Vec<u8>, save_to_sent: bool) -> Result<()> {
        self.send_message(account_id, rfc822, save_to_sent).await
    }
}
