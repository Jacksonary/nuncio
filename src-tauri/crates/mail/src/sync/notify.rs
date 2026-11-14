//! Notification + rule post-processing for freshly synced mail.

use chrono::{Duration, Utc};
use nuncio_core::models::{FolderRole, MessageSummary};
use nuncio_core::{CoreContext, Result};
use tracing::debug;

use crate::rules;

/// Apply local rules then notify about new inbox mail.
pub async fn after_new_mail(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    folder: &nuncio_core::models::Folder,
    new_messages: &[MessageSummary],
    initial: bool,
) -> Result<()> {
    if new_messages.is_empty() {
        update_badge(ctx).await?;
        return Ok(());
    }

    let effects = rules::apply_rules(ctx, account, new_messages).await?;
    let settings = ctx.settings.current();
    let should_notify = !initial
        && folder.role == FolderRole::Inbox
        && account.notifications_enabled
        && settings.notifications.enabled;
    if should_notify {
        let now = Utc::now();
        let cutoff = now - Duration::hours(24);
        let items: Vec<MessageSummary> = new_messages
            .iter()
            .filter(|message| !message.seen)
            .filter(|message| message.date >= cutoff)
            .filter(|message| !effects.skip_notify.contains(&message.id))
            .cloned()
            .collect();
        if !items.is_empty() {
            debug!(account = %account.id, count = items.len(), "notifying new mail");
            ctx.notifier.new_mail(account, &items);
        }
    }
    update_badge(ctx).await
}

/// Publish the unified inbox unread count to the OS badge.
pub async fn update_badge(ctx: &CoreContext) -> Result<()> {
    let count = ctx
        .db
        .read(|db| {
            db.query_row(
                "SELECT COUNT(*) FROM message m
                 JOIN folder f ON f.id = m.folder_id
                 JOIN account a ON a.id = m.account_id
                 WHERE f.role = 'inbox' AND a.include_in_unified = 1
                   AND m.seen = 0 AND m.is_deleted_local = 0",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(nuncio_core::AppError::from)
        })
        .await?;
    ctx.notifier.unread_badge(crate::util::clamp_count(count));
    Ok(())
}
