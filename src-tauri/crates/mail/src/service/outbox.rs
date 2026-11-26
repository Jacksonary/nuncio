//! Outbox projection: the virtual `Outbox` folder, synthetic message
//! summaries for queued/failed mail and retry/discard helpers.
//!
//! `outbox` rows are not `message` rows, so the list query (`repo::messages`)
//! can never return them. The sidebar advertises an `outbox` role and a count
//! from the `outbox` table, therefore the service maps those rows to
//! `MessageSummary` values with a synthetic `outbox:<id>` id.

use nuncio_core::models::{
    Account, Draft, Folder, FolderRole, MessageSummary, OutboxItem, OutboxState,
};
use nuncio_core::{events, repo, AppError, CoreContext, Result};
use tracing::debug;

use super::MailService;

/// Remote path of the virtual per-account Outbox folder.
pub const OUTBOX_REMOTE_PATH: &str = "local:outbox";

/// Prefix of synthetic message ids returned for outbox rows.
pub const OUTBOX_ID_PREFIX: &str = "outbox:";

pub(crate) fn synthetic_id(item_id: &str) -> String {
    format!("{OUTBOX_ID_PREFIX}{item_id}")
}

pub(crate) fn item_id_from_message_id(message_id: &str) -> Option<&str> {
    message_id
        .strip_prefix(OUTBOX_ID_PREFIX)
        .filter(|value| !value.is_empty())
}

/// Accept both the raw outbox row id and the synthetic `outbox:<id>` message id.
fn normalize_outbox_id(id: &str) -> &str {
    item_id_from_message_id(id).unwrap_or(id)
}

/// Map an outbox row to the list contract. `folder_id` is the account's
/// virtual Outbox folder (or the literal `"outbox"` when it does not exist).
pub(crate) fn summary_from_item(
    item: &OutboxItem,
    account: &Account,
    folder_id: &str,
) -> MessageSummary {
    let mut summary = MessageSummary::new(
        synthetic_id(&item.id),
        item.account_id.clone(),
        folder_id.to_string(),
        format!("outbox:{}", item.id),
        item.created_at,
    );
    summary.subject = item.subject.clone();
    summary.r#from = Some(account.address());
    summary.to = item.recipients.clone();
    summary.preview = outbox_preview(item);
    summary.seen = true;
    summary.thread_count = 1;
    summary.thread_unread = 0;
    summary.size = std::fs::metadata(&item.rfc822_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    summary
}

/// Human-readable state line shown in the list preview.
fn outbox_preview(item: &OutboxItem) -> String {
    let recipients = item
        .recipients
        .iter()
        .map(|address| {
            address
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| address.email.clone())
        })
        .collect::<Vec<_>>()
        .join(", ");
    match item.state {
        OutboxState::Failed => format!(
            "Failed: {}",
            item.last_error.as_deref().unwrap_or("delivery failed")
        ),
        OutboxState::Sending => format!("Sending to {recipients}"),
        OutboxState::Pending => {
            if item.attempts > 0 {
                format!(
                    "Waiting to retry ({} attempt(s)) · {recipients}",
                    item.attempts
                )
            } else {
                format!("Queued · {recipients}")
            }
        }
        OutboxState::Sent => format!("Sent to {recipients}"),
    }
}

/// Ensure the virtual Outbox folder exists (when something is queued) and keep
/// its counts in sync with the outbox table. Called after enqueue/send/failure
/// so the sidebar badge and the folder row agree with the list.
pub(crate) async fn refresh_outbox_folder(ctx: &CoreContext, account_id: &str) -> Result<()> {
    let account_id = account_id.to_string();
    let account_for_events = account_id.clone();
    let outcome = ctx
        .db
        .write(move |db| {
            let Some(account) = repo::accounts::get(db, &account_id)? else {
                return Ok(OutboxFolderOutcome::default());
            };
            if !account.mail_enabled {
                return Ok(OutboxFolderOutcome::default());
            }
            let count = repo::outbox::pending_count(db, Some(&account_id))?;
            let existing = repo::folders::by_role(db, &account_id, FolderRole::Outbox)?
                .or(repo::folders::by_remote_path(db, &account_id, OUTBOX_REMOTE_PATH)?);
            let existed = existing.is_some();
            let folder = match existing {
                Some(folder) => {
                    db.execute(
                        "UPDATE folder SET total_count = ?2, unread_count = 0, updated_at = ?3 WHERE id = ?1",
                        rusqlite::params![folder.id, count, nuncio_core::repo::now_string()],
                    )?;
                    folder
                }
                None if count > 0 => {
                    let folder = Folder {
                        id: nuncio_core::models::new_id(),
                        account_id: account_id.clone(),
                        parent_id: None,
                        remote_path: OUTBOX_REMOTE_PATH.to_string(),
                        name: "Outbox".to_string(),
                        role: FolderRole::Outbox,
                        unread_count: 0,
                        total_count: count,
                        is_favorite: false,
                        sync_enabled: false,
                        is_selectable: true,
                        sort_order: 9,
                        last_sync_at: None,
                    };
                    repo::folders::insert(db, &folder)?;
                    folder
                }
                None => return Ok(OutboxFolderOutcome::default()),
            };
            Ok(OutboxFolderOutcome {
                folder_id: Some(folder.id),
                created: !existed,
            })
        })
        .await?;

    if let Some(folder_id) = outcome.folder_id {
        if outcome.created {
            events::emit_folders_changed(
                ctx.events.as_ref(),
                account_for_events.clone(),
                vec![folder_id.clone()],
            );
            debug!(account = %account_for_events, folder = %folder_id, "created virtual Outbox folder");
        }
        events::emit_mail_changed(
            ctx.events.as_ref(),
            account_for_events,
            vec![folder_id],
            Vec::new(),
            events::MailChangeKind::Folders,
        );
    }
    Ok(())
}

#[derive(Debug, Default)]
struct OutboxFolderOutcome {
    folder_id: Option<String>,
    created: bool,
}

impl MailService {
    /// Synthetic message id (`outbox:<id>`) or `None`.
    pub(crate) fn outbox_item_id(message_id: &str) -> Option<&str> {
        item_id_from_message_id(message_id)
    }

    /// Read one outbox row by its synthetic message id.
    pub(crate) async fn outbox_item(&self, message_id: &str) -> Result<Option<OutboxItem>> {
        let Some(item_id) = item_id_from_message_id(message_id) else {
            return Ok(None);
        };
        let item_id = item_id.to_string();
        self.inner
            .ctx
            .db
            .read(move |db| repo::outbox::get(db, &item_id))
            .await
    }

    /// Queued/failed items for the given accounts, newest first.
    pub(crate) async fn outbox_items(&self, account_ids: Vec<String>) -> Result<Vec<OutboxItem>> {
        self.inner
            .ctx
            .db
            .read(move |db| {
                let mut items = Vec::new();
                for account_id in &account_ids {
                    items.extend(repo::outbox::list(db, Some(account_id))?);
                }
                items.sort_by(|a, b| {
                    b.created_at
                        .cmp(&a.created_at)
                        .then_with(|| b.id.cmp(&a.id))
                });
                Ok(items)
            })
            .await
    }

    /// Id of the account's virtual Outbox folder, when it exists.
    pub(crate) async fn outbox_folder_id(&self, account_id: &str) -> Result<Option<String>> {
        let account_id = account_id.to_string();
        self.inner
            .ctx
            .db
            .read(move |db| {
                Ok(repo::folders::by_role(db, &account_id, FolderRole::Outbox)?
                    .map(|folder| folder.id))
            })
            .await
    }

    /// Retry queued/failed items. `outbox_id` selects one item; `None` retries
    /// every failed/pending item across all accounts.
    pub async fn outbox_retry(&self, outbox_id: Option<&str>) -> Result<()> {
        let outbox_id = outbox_id.map(normalize_outbox_id);
        let items: Vec<OutboxItem> = match outbox_id {
            Some(id) => {
                let id = id.to_string();
                let item = self
                    .inner
                    .ctx
                    .db
                    .read(move |db| repo::outbox::get_required(db, &id))
                    .await?;
                vec![item]
            }
            None => self
                .inner
                .ctx
                .db
                .read(move |db| repo::outbox::list(db, None))
                .await?
                .into_iter()
                .filter(|item| matches!(item.state, OutboxState::Failed | OutboxState::Pending))
                .collect(),
        };
        let mut accounts: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for item in items {
            match item.state {
                OutboxState::Sending => {
                    if outbox_id.is_some() {
                        return Err(AppError::conflict(
                            "the message is being sent right now; try again in a moment",
                        ));
                    }
                    continue;
                }
                OutboxState::Sent => {
                    if outbox_id.is_some() {
                        return Err(AppError::invalid_input("the message was already sent"));
                    }
                    continue;
                }
                OutboxState::Failed | OutboxState::Pending => {}
            }
            let item_id = item.id.clone();
            self.inner
                .ctx
                .db
                .write(move |db| {
                    repo::outbox::retry_now(db, &item_id)?;
                    // A manual retry starts a fresh attempt budget.
                    db.execute(
                        "UPDATE outbox SET attempts = 0, last_error = NULL WHERE id = ?1",
                        rusqlite::params![item_id],
                    )?;
                    Ok(())
                })
                .await?;
            accounts.insert(item.account_id.clone());
        }
        for account_id in accounts {
            if let Err(error) = self.inner.engine.process_outbox(&account_id).await {
                debug!(%error, account = %account_id, "outbox retry left the item queued");
            }
            refresh_outbox_folder(&self.inner.ctx, &account_id).await?;
        }
        Ok(())
    }

    /// Stop sending an item and turn it back into an editable draft.
    ///
    /// Refuses while the row is in the `sending` state. The outbox row and the
    /// staged RFC822 file are deleted; the draft row is returned when it still
    /// exists, otherwise a new draft is reconstructed from the staged message
    /// (including its attachments, re-staged into the compose directory).
    pub async fn outbox_cancel(&self, outbox_id: &str) -> Result<Draft> {
        let outbox_id = normalize_outbox_id(outbox_id).to_string();
        let item = {
            self.inner
                .ctx
                .db
                .read(move |db| repo::outbox::get_required(db, &outbox_id))
                .await?
        };
        if item.state == OutboxState::Sending {
            return Err(AppError::conflict(
                "the message is being sent right now; cancel after it completes",
            ));
        }
        if item.state == OutboxState::Sent {
            return Err(AppError::invalid_input("the message was already sent"));
        }
        // Delete conditionally so a worker that claimed the row between the
        // read and this write wins the race instead of us cancelling a send.
        let item_id = item.id.clone();
        let deleted = self
            .inner
            .ctx
            .db
            .write(move |db| {
                db.execute(
                    "DELETE FROM outbox WHERE id = ?1 AND state <> 'sending'",
                    rusqlite::params![item_id],
                )
                .map_err(AppError::from)
            })
            .await?;
        if deleted == 0 {
            return Err(AppError::conflict(
                "the message started sending; try again after it completes",
            ));
        }

        let existing = match item.draft_id.as_deref() {
            Some(draft_id) => {
                let draft_id = draft_id.to_string();
                self.inner
                    .ctx
                    .db
                    .read(move |db| repo::drafts::get(db, &draft_id))
                    .await?
            }
            None => None,
        };
        let draft = match existing {
            Some(draft) => draft,
            None => self.rebuild_draft_from_message(&item).await?,
        };
        let _ = tokio::fs::remove_file(&item.rfc822_path).await;
        refresh_outbox_folder(&self.inner.ctx, &item.account_id).await?;
        events::emit_mail_changed(
            self.inner.ctx.events.as_ref(),
            item.account_id.clone(),
            Vec::new(),
            Vec::new(),
            events::MailChangeKind::Updated,
        );
        Ok(draft)
    }

    /// Reconstruct an editable [`Draft`] from the staged RFC822 message.
    async fn rebuild_draft_from_message(&self, item: &OutboxItem) -> Result<Draft> {
        let raw = tokio::fs::read(&item.rfc822_path)
            .await
            .map_err(|error| AppError::io(format!("cannot read the queued message: {error}")))?;
        let parsed = nuncio_mime::parse(&raw)?;
        let account = self.account(&item.account_id).await?;
        let from = parsed
            .headers
            .from
            .clone()
            .unwrap_or_else(|| account.address());
        let identity = self
            .identity_for(&account, std::slice::from_ref(&from))
            .await?;

        // Re-stage attachments into a fresh compose directory so the returned
        // draft survives until the user saves it.
        let staging = self
            .inner
            .ctx
            .paths
            .staging_dir()
            .join("compose")
            .join(nuncio_core::models::new_id());
        tokio::fs::create_dir_all(&staging)
            .await
            .map_err(|error| AppError::io(format!("cannot create staging dir: {error}")))?;
        let mut used_names = std::collections::HashSet::new();
        let mut attachments = Vec::new();
        for attachment in &parsed.attachments {
            let bytes = match nuncio_mime::extract_part(&raw, &attachment.part) {
                Ok(bytes) => bytes,
                Err(error) => {
                    debug!(%error, filename = %attachment.filename, "could not stage queued attachment");
                    continue;
                }
            };
            let filename =
                super::compose::unique_name(&staging, &attachment.filename, &mut used_names).await;
            let dest = staging.join(&filename);
            if let Err(error) = tokio::fs::write(&dest, &bytes).await {
                debug!(%error, %filename, "could not write queued attachment");
                continue;
            }
            attachments.push(nuncio_core::models::DraftAttachment {
                id: nuncio_core::models::new_id(),
                filename,
                mime_type: attachment.mime_type.clone(),
                size: bytes.len() as u64,
                path: dest.to_string_lossy().into_owned(),
                content_id: attachment.content_id.clone(),
            });
        }

        let html = parsed
            .html
            .clone()
            .or_else(|| parsed.text.as_deref().map(nuncio_mime::plain_text_to_html))
            .unwrap_or_else(|| "<p><br></p>".to_string());
        Ok(Draft {
            id: None,
            account_id: account.id.clone(),
            identity_id: Some(identity.id.clone()),
            from: super::compose::identity_address(&identity),
            to: parsed.headers.to.clone(),
            cc: parsed.headers.cc.clone(),
            bcc: parsed.headers.bcc.clone(),
            subject: parsed.headers.subject.clone(),
            html,
            attachments,
            reply_mode: None,
            source_message_id: None,
            in_reply_to: parsed.headers.in_reply_to.clone(),
            references: parsed.headers.references.clone(),
            importance: parsed.headers.importance,
            request_read_receipt: !parsed.headers.disposition_notification_to.is_empty(),
            remote_draft_message_id: None,
            remote_draft: None,
            sync_suppressed: false,
            updated_at: None,
        })
    }

    /// Discard a queued/failed message (and its staged RFC822 file).
    pub async fn outbox_discard(&self, message_id: &str) -> Result<()> {
        let item = self
            .outbox_item(message_id)
            .await?
            .ok_or_else(|| AppError::not_found(format!("outbox item {message_id} not found")))?;
        // A message that is already on the wire cannot be unsent. Refuse so the
        // UI does not claim it was deleted while delivery may still complete.
        let item_id = item.id.clone();
        let removed = self
            .inner
            .ctx
            .db
            .write(move |db| repo::outbox::remove_when_not_sending(db, &item_id))
            .await?;
        if removed == 0 {
            return Err(AppError::conflict(
                "The message is being sent right now; wait for it to finish before deleting it.",
            ));
        }
        let _ = tokio::fs::remove_file(&item.rfc822_path).await;
        refresh_outbox_folder(&self.inner.ctx, &item.account_id).await?;
        Ok(())
    }
}
