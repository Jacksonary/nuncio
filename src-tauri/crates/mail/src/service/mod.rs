//! Command-facing mail service.
//!
//! [`MailService`] is the public API used by the Tauri command layer. It owns a
//! [`Engine`] (IMAP/SMTP/POP3 synchronization and the persistent operation
//! queue) and implements every mail command from `src/ipc/api.ts`: folders,
//! message listing/search, reading (MIME sanitization, remote-content policy,
//! cid resolution, invitations), optimistic message actions with undo,
//! attachments, composing/sending and mail rules.
//!
//! All methods take and return `nuncio_core` contract types and report failures
//! as `AppError`. Local state is updated immediately; server work is persisted
//! in `pending_op` / `outbox` so the UI stays responsive while offline.

mod attachments;
mod compose;
mod detail;
mod folders;
mod messages;
pub(crate) mod outbox;
mod undo;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nuncio_core::models::{
    Account, ConnectionTestResult, Folder, Identity, NewAccountInput, StoredMessage,
};
use nuncio_core::traits::{MailAuth, MailSender};
use nuncio_core::{events, repo, AppError, CoreContext, Result};
use tracing::{debug, info, warn};

use crate::Engine;

/// Maximum number of externally opened `.eml` files kept for cid rendering.
const EXTERNAL_LIMIT: usize = 12;
/// How long an externally opened `.eml` stays addressable.
const EXTERNAL_TTL: Duration = Duration::from_secs(30 * 60);

pub(crate) struct ExternalAttachment {
    pub attachment_id: String,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    pub content_id: Option<String>,
    pub part: String,
    pub is_inline: bool,
}

pub(crate) struct ExternalMessage {
    pub raw: Vec<u8>,
    pub inserted: Instant,
    pub attachments: Vec<ExternalAttachment>,
}

#[derive(Default)]
pub(crate) struct ExternalStore {
    entries: HashMap<String, ExternalMessage>,
    /// attachment id -> owning synthetic message id.
    by_attachment: HashMap<String, String>,
    order: VecDeque<String>,
}

impl ExternalStore {
    fn insert(&mut self, message_id: String, message: ExternalMessage) {
        self.remove(&message_id);
        for attachment in &message.attachments {
            self.by_attachment
                .insert(attachment.attachment_id.clone(), message_id.clone());
        }
        self.order.push_back(message_id.clone());
        self.entries.insert(message_id, message);
        self.prune();
    }

    fn remove(&mut self, message_id: &str) {
        if let Some(entry) = self.entries.remove(message_id) {
            for attachment in entry.attachments {
                self.by_attachment.remove(&attachment.attachment_id);
            }
        }
        self.order.retain(|id| id != message_id);
    }

    fn prune(&mut self) {
        let now = Instant::now();
        let stale: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, entry)| now.duration_since(entry.inserted) > EXTERNAL_TTL)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.remove(&id);
        }
        while self.order.len() > EXTERNAL_LIMIT {
            if let Some(oldest) = self.order.pop_front() {
                if let Some(entry) = self.entries.remove(&oldest) {
                    for attachment in entry.attachments {
                        self.by_attachment.remove(&attachment.attachment_id);
                    }
                }
            } else {
                break;
            }
        }
    }

    fn get(&self, message_id: &str) -> Option<&ExternalMessage> {
        self.entries.get(message_id)
    }
}

pub(crate) struct MailServiceInner {
    pub ctx: CoreContext,
    pub engine: Engine,
    pub external: Mutex<ExternalStore>,
}

/// Cloneable handle to the mail command service.
#[derive(Clone)]
pub struct MailService {
    inner: Arc<MailServiceInner>,
}

impl std::fmt::Debug for MailService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailService").finish_non_exhaustive()
    }
}

impl MailService {
    pub fn new(ctx: CoreContext) -> Self {
        let engine = Engine::new(ctx.clone());
        Self {
            inner: Arc::new(MailServiceInner {
                ctx,
                engine,
                external: Mutex::new(ExternalStore::default()),
            }),
        }
    }

    /// Access the underlying engine (used by the app for lifecycle wiring).
    pub fn engine(&self) -> &Engine {
        &self.inner.engine
    }

    /// Access the shared core context.
    pub fn context(&self) -> &CoreContext {
        &self.inner.ctx
    }

    /// Spawn background workers for every enabled mail account.
    pub async fn start(&self) -> Result<()> {
        self.inner.engine.start().await?;
        let accounts = self
            .inner
            .ctx
            .db
            .read(repo::accounts::list_mail_accounts)
            .await?;
        for account in accounts {
            if let Err(error) = self.ensure_default_identity(&account.id).await {
                warn!(account = %account.id, %error, "could not ensure a default identity");
            }
            if let Err(error) = outbox::refresh_outbox_folder(&self.inner.ctx, &account.id).await {
                debug!(account = %account.id, %error, "could not refresh the Outbox folder");
            }
        }
        Ok(())
    }

    /// Stop background workers.
    pub async fn shutdown(&self) {
        self.inner.engine.shutdown().await;
    }

    /// Called after an account is created: seed the default identity and start
    /// its mail workers.
    pub async fn on_account_added(&self, account_id: &str) -> Result<()> {
        self.ensure_default_identity(account_id).await?;
        outbox::refresh_outbox_folder(&self.inner.ctx, account_id).await?;
        self.inner.engine.add_account(account_id).await
    }

    /// Called after account settings changed.
    pub async fn on_account_changed(&self, account_id: &str) -> Result<()> {
        self.ensure_default_identity(account_id).await?;
        self.inner.engine.account_changed(account_id).await
    }

    /// Called after an account is deleted.
    pub async fn on_account_removed(&self, account_id: &str) -> Result<()> {
        self.inner.engine.remove_account(account_id).await
    }

    /// Sync one account (or all mail accounts). Background workers are
    /// preferred; when they are not running a direct connection is used so the
    /// command still works during startup/shutdown edge cases.
    pub async fn sync_now(&self, account_id: Option<&str>) -> Result<()> {
        match self.inner.engine.sync_now(account_id, None).await {
            Ok(()) => Ok(()),
            Err(error) => {
                if !matches!(error.code(), "notFound" | "internal") {
                    return Err(error);
                }
                self.sync_direct(account_id).await
            }
        }
    }

    async fn sync_direct(&self, account_id: Option<&str>) -> Result<()> {
        let accounts = match account_id {
            Some(id) => vec![self.account(id).await?],
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
            let result = if account.imap.is_some() {
                let connection = crate::imap::connect_account(&self.inner.ctx, &account).await;
                match connection {
                    Ok(mut connection) => {
                        crate::sync::sync_account(&self.inner.ctx, &mut connection, &account).await
                    }
                    Err(error) => Err(error),
                }
            } else {
                crate::pop3::sync::sync_account(&self.inner.ctx, &account).await
            };
            if let Err(error) = result {
                warn!(account = %account.id, %error, "direct sync failed");
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

    /// Unified inbox unread count (all accounts included in the unified view).
    pub async fn unread_total(&self) -> Result<i64> {
        self.inner
            .ctx
            .db
            .read(|db| repo::messages::unread_total(db, None))
            .await
    }

    /// Ensure the account has an identity so the composer always has a From
    /// address. Returns the default identity.
    pub async fn ensure_default_identity(&self, account_id: &str) -> Result<Identity> {
        let account = self.account(account_id).await?;
        let account_id_owned = account.id.clone();
        let identities = self
            .inner
            .ctx
            .db
            .read(move |db| repo::identities::list(db, Some(&account_id_owned)))
            .await?;
        if let Some(identity) = identities.iter().find(|identity| identity.is_default) {
            return Ok(identity.clone());
        }
        if let Some(mut identity) = identities.into_iter().next() {
            // An identity exists but none is marked default; promote the first.
            identity.is_default = true;
            let stored = self
                .inner
                .ctx
                .db
                .write(move |db| repo::identities::upsert(db, &mut identity))
                .await?;
            return Ok(stored);
        }
        let mut identity = Identity {
            id: String::new(),
            account_id: account.id.clone(),
            name: if account.display_name.trim().is_empty() {
                account.email.clone()
            } else {
                account.display_name.clone()
            },
            email: account.email.clone(),
            reply_to: None,
            signature_id: None,
            is_default: true,
        };
        let stored = self
            .inner
            .ctx
            .db
            .write(move |db| repo::identities::upsert(db, &mut identity))
            .await?;
        info!(account = %stored.account_id, identity = %stored.id, "created default identity");
        Ok(stored)
    }

    /// Minimal mail connection test for the add-account wizard. OAuth accounts
    /// are tested after creation (the token lives in the auth service), so the
    /// mail probes are left `null` for them.
    pub async fn test_connection(&self, input: &NewAccountInput) -> Result<ConnectionTestResult> {
        let mut result = ConnectionTestResult {
            imap_ok: None,
            smtp_ok: None,
            pop3_ok: None,
            dav_ok: None,
            error: None,
        };
        let password = match input.password.as_deref() {
            Some(password) if !password.is_empty() => password.to_string(),
            _ => {
                if input.auth_kind == nuncio_core::models::AuthKind::Password {
                    return Err(AppError::invalid_input(
                        "a password is required to test a password account",
                    ));
                }
                return Ok(result);
            }
        };
        let username = input
            .imap
            .as_ref()
            .or(input.smtp.as_ref())
            .or(input.pop3.as_ref())
            .map(|config| config.username.trim().to_string())
            .filter(|username| !username.is_empty())
            .unwrap_or_else(|| input.email.clone());
        let auth = MailAuth::Password { username, password };
        let mut errors: Vec<String> = Vec::new();
        if let Some(config) = input.imap.as_ref() {
            match Engine::test_imap(config, &auth).await {
                Ok(()) => result.imap_ok = Some(true),
                Err(error) => {
                    result.imap_ok = Some(false);
                    errors.push(format!("IMAP: {}", error.message()));
                }
            }
        }
        if let Some(config) = input.smtp.as_ref() {
            match Engine::test_smtp(config, &auth).await {
                Ok(()) => result.smtp_ok = Some(true),
                Err(error) => {
                    result.smtp_ok = Some(false);
                    errors.push(format!("SMTP: {}", error.message()));
                }
            }
        }
        if let Some(config) = input.pop3.as_ref() {
            match Engine::test_pop3(config, &auth).await {
                Ok(()) => result.pop3_ok = Some(true),
                Err(error) => {
                    result.pop3_ok = Some(false);
                    errors.push(format!("POP3: {}", error.message()));
                }
            }
        }
        if !errors.is_empty() {
            result.error = Some(errors.join("; "));
        }
        Ok(result)
    }

    /* ─────────────────────── shared internals ─────────────────────── */

    pub(crate) async fn account(&self, account_id: &str) -> Result<Account> {
        let id = account_id.to_string();
        self.inner
            .ctx
            .db
            .read(move |db| repo::accounts::get_required(db, &id))
            .await
    }

    pub(crate) async fn folder(&self, folder_id: &str) -> Result<Folder> {
        let id = folder_id.to_string();
        self.inner
            .ctx
            .db
            .read(move |db| repo::folders::get_required(db, &id))
            .await
    }

    pub(crate) async fn stored(&self, message_id: &str) -> Result<StoredMessage> {
        let id = message_id.to_string();
        self.inner
            .ctx
            .db
            .read(move |db| repo::messages::get_required(db, &id))
            .await
    }

    pub(crate) fn cid_base(&self) -> &'static str {
        if cfg!(target_os = "windows") {
            "http://nuncio.localhost/"
        } else {
            "nuncio://localhost/"
        }
    }

    /// Read the raw RFC822 message, downloading/caching it when needed.
    /// Works with POP3 (body cached during sync) and directly with IMAP when
    /// the background worker is not running.
    pub(crate) async fn raw_message(&self, stored: &StoredMessage) -> Result<Vec<u8>> {
        if let Some(path) = stored.body_path.as_deref() {
            match tokio::fs::metadata(path).await {
                Ok(metadata) if metadata.len() > crate::sync::body::MAX_MESSAGE_BYTES => {
                    return Err(AppError::invalid_input(
                        "cached message exceeds the size limit",
                    ));
                }
                Ok(_) => match tokio::fs::read(path).await {
                    Ok(bytes) => return Ok(bytes),
                    Err(error) => debug!(
                        message = %stored.summary.id,
                        %error,
                        "cached body is unreadable; re-downloading"
                    ),
                },
                Err(error) => debug!(
                    message = %stored.summary.id,
                    %error,
                    "cached body is unreadable; re-downloading"
                ),
            }
        }
        let account = self.account(&stored.summary.account_id).await?;
        if account.imap.is_none() {
            return Err(AppError::not_found(format!(
                "message {} has no cached body",
                stored.summary.id
            )));
        }
        let message_id = stored.summary.id.clone();
        let path = match self.inner.engine.fetch_body(&message_id).await {
            Ok(path) => path,
            Err(error) if matches!(error.code(), "notFound" | "internal") => {
                let mut connection =
                    crate::imap::connect_account(&self.inner.ctx, &account).await?;
                crate::sync::body::fetch_body(
                    &self.inner.ctx,
                    &mut connection,
                    &account.id,
                    &message_id,
                )
                .await?
            }
            Err(error) => return Err(error),
        };
        tokio::fs::read(&path)
            .await
            .map_err(|error| AppError::io(format!("cannot read cached message body: {error}")))
    }

    /// Read raw bytes by local message id.
    pub(crate) async fn raw_message_by_id(&self, message_id: &str) -> Result<Vec<u8>> {
        if let Some(raw) = self.external_raw(message_id) {
            return Ok(raw);
        }
        let stored = self.stored(message_id).await?;
        self.raw_message(&stored).await
    }

    /// Decide whether remote images may load.
    ///
    /// * `always`      - always load.
    /// * `ask`         - load only for trusted senders/domains, unless the user
    ///   explicitly allowed remote content for this view (`allow_override`),
    ///   which is the "show images once" path.
    /// * `trustedOnly` - load only for trusted senders; the per-view override
    ///   is deliberately ignored, so the only way to unlock an unknown sender
    ///   is to trust them.
    pub(crate) async fn remote_content_allowed(
        &self,
        allow_override: bool,
        from: Option<&nuncio_core::models::Address>,
    ) -> Result<bool> {
        let policy = self.inner.ctx.settings.current().mail.remote_images;
        match policy {
            nuncio_core::models::RemoteImagesPolicy::Always => return Ok(true),
            nuncio_core::models::RemoteImagesPolicy::Ask => {
                if allow_override {
                    return Ok(true);
                }
            }
            nuncio_core::models::RemoteImagesPolicy::TrustedOnly => {
                // Ignore `allow_override`: only trusting the sender unlocks.
            }
        }
        let Some(from) = from else {
            return Ok(false);
        };
        let email = nuncio_core::text::normalize_email(&from.email);
        let domain = nuncio_core::text::email_domain(&from.email).unwrap_or_default();
        if email.is_empty() {
            return Ok(false);
        }
        self.inner
            .ctx
            .db
            .read(move |db| repo::trusted_senders::is_trusted(db, &email, &domain))
            .await
    }

    pub(crate) fn external_insert(
        &self,
        message_id: String,
        raw: Vec<u8>,
        attachments: Vec<ExternalAttachment>,
    ) {
        if let Ok(mut store) = self.inner.external.lock() {
            store.insert(
                message_id,
                ExternalMessage {
                    raw,
                    inserted: Instant::now(),
                    attachments,
                },
            );
        }
    }

    pub(crate) fn external_raw(&self, message_id: &str) -> Option<Vec<u8>> {
        self.inner
            .external
            .lock()
            .ok()
            .and_then(|store| store.get(message_id).map(|entry| entry.raw.clone()))
    }

    /// Bytes + MIME type + filename for an attachment of an externally opened
    /// `.eml` file.
    pub(crate) fn external_attachment(
        &self,
        attachment_id: &str,
    ) -> Option<(String, Vec<u8>, String, String)> {
        let store = self.inner.external.lock().ok()?;
        let message_id = store.by_attachment.get(attachment_id)?.clone();
        let message = store.entries.get(&message_id)?;
        let attachment = message
            .attachments
            .iter()
            .find(|attachment| attachment.attachment_id == attachment_id)?;
        let part = nuncio_mime::PartPath::parse(&attachment.part)?;
        let bytes = nuncio_mime::extract_part(&message.raw, &part).ok()?;
        Some((
            attachment.mime_type.clone(),
            bytes,
            attachment.filename.clone(),
            message_id,
        ))
    }

    pub(crate) fn external_attachments_of(&self, message_id: &str) -> Vec<ExternalAttachment> {
        let store = match self.inner.external.lock() {
            Ok(store) => store,
            Err(poisoned) => poisoned.into_inner(),
        };
        store
            .get(message_id)
            .map(|entry| {
                entry
                    .attachments
                    .iter()
                    .map(|attachment| ExternalAttachment {
                        attachment_id: attachment.attachment_id.clone(),
                        filename: attachment.filename.clone(),
                        mime_type: attachment.mime_type.clone(),
                        size: attachment.size,
                        content_id: attachment.content_id.clone(),
                        part: attachment.part.clone(),
                        is_inline: attachment.is_inline,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) async fn update_badge(&self) {
        if let Err(error) = crate::sync::notify::update_badge(&self.inner.ctx).await {
            debug!(%error, "could not update unread badge");
        }
    }

    /// Emit `mail://changed` for a set of messages grouped by account.
    pub(crate) fn emit_changed(
        &self,
        account_id: &str,
        folder_ids: Vec<String>,
        message_ids: Vec<String>,
        kind: events::MailChangeKind,
    ) {
        events::emit_mail_changed(
            self.inner.ctx.events.as_ref(),
            account_id,
            folder_ids,
            message_ids,
            kind,
        );
    }
}

#[async_trait]
impl MailSender for MailService {
    async fn send_raw(&self, account_id: &str, rfc822: Vec<u8>, save_to_sent: bool) -> Result<()> {
        self.inner
            .engine
            .send_message(account_id, rfc822, save_to_sent)
            .await
    }
}
