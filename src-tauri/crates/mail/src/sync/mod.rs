//! Mail synchronization: folder discovery, message metadata, bodies, notify.

pub mod body;
pub mod folders;
pub mod messages;
pub mod notify;

use std::collections::HashSet;

use nuncio_core::models::{Folder, FolderRole};
use nuncio_core::models::{SyncModule, SyncState};
use nuncio_core::{repo, AppError, CoreContext, Result};
use tracing::{debug, info, warn};

use crate::imap::conn::ImapConnection;

/// Report a sync failure, or keep the indicator calm while the worker is
/// still inside its reconnect budget. Non-connection failures are always
/// reported immediately.
fn note_sync_error(
    ctx: &CoreContext,
    account_id: &str,
    error: &AppError,
    report_network_errors: bool,
) {
    if error.code() == "network" && !report_network_errors {
        debug!(
            account = account_id,
            code = error.code(),
            "transient sync failure; not reporting an error status yet"
        );
        settle_sync_idle(ctx, account_id);
    } else {
        ctx.sync_status
            .fail(account_id, SyncModule::Mail, error.message());
    }
}

/// Return the mail sync indicator to idle without touching `last_sync_at`.
/// Used when a transient failure is suppressed and when a later command
/// succeeds after a reported outage. `needsReauth` is left alone because only
/// fresh credentials can fix it.
pub fn settle_sync_idle(ctx: &CoreContext, account_id: &str) {
    let Some(mut status) = ctx.sync_status.get(account_id, SyncModule::Mail) else {
        return;
    };
    if status.state == SyncState::NeedsReauth {
        return;
    }
    if status.state != SyncState::Idle || status.progress.is_some() || status.message.is_some() {
        status.state = SyncState::Idle;
        status.progress = None;
        status.message = None;
        ctx.sync_status.set(status);
    }
}

/// Full account sync: reconcile folders, then fetch the inbox and every
/// sync-enabled/favourite folder. Individual folder failures do not abort the
/// rest of the account; the first error is returned at the end so the caller
/// can surface it in `sync://status`.
pub async fn sync_account(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
) -> Result<()> {
    sync_account_with(ctx, conn, account, true).await
}

/// Like [`sync_account`], but lets an account actor defer the user-visible
/// error status while it is still reconnecting (see `engine::account_actor`).
pub async fn sync_account_with(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    report_network_errors: bool,
) -> Result<()> {
    ctx.sync_status.begin(&account.id, SyncModule::Mail, None);
    let all_folders = match folders::sync(ctx, conn, &account.id).await {
        Ok(folders) => folders,
        Err(error) => {
            note_sync_error(ctx, &account.id, &error, report_network_errors);
            return Err(error);
        }
    };

    let mut targets: Vec<Folder> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut push = |folder: &Folder, targets: &mut Vec<Folder>| {
        if folder.is_selectable && seen.insert(folder.id.clone()) {
            targets.push(folder.clone());
        }
    };
    if let Some(inbox) = all_folders.iter().find(|f| f.role == FolderRole::Inbox) {
        push(inbox, &mut targets);
    }
    for folder in all_folders.iter().filter(|f| f.is_favorite) {
        push(folder, &mut targets);
    }
    for folder in all_folders.iter().filter(|f| f.sync_enabled) {
        push(folder, &mut targets);
    }

    let mut first_error: Option<AppError> = None;
    for folder in &targets {
        match messages::sync_folder(ctx, conn, account, folder).await {
            Ok(outcome) => {
                if let Err(error) = notify::after_new_mail(
                    ctx,
                    account,
                    folder,
                    &outcome.new_messages,
                    outcome.initial,
                )
                .await
                {
                    warn!(%error, folder = %folder.remote_path, "notification step failed");
                }
                if folder.role == FolderRole::Inbox {
                    let limit = ctx.settings.current().mail.prefetch_unread_bodies;
                    if limit > 0 {
                        if let Err(error) =
                            body::prefetch_unread(ctx, conn, &account.id, folder, limit).await
                        {
                            debug!(%error, "unread body prefetch failed");
                        }
                    }
                }
            }
            Err(error) => {
                warn!(%error, folder = %folder.remote_path, "folder sync failed");
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }

    let settings = ctx.settings.current();
    if let Err(error) = body::trim_cache(ctx, settings.mail.body_cache_limit_mb).await {
        debug!(%error, "body cache trim failed");
    }

    let now = nuncio_core::models::now();
    let account_id = account.id.clone();
    ctx.db
        .write(move |db| repo::accounts::touch_synced_at(db, &account_id, now))
        .await?;

    match first_error {
        Some(error) => {
            note_sync_error(ctx, &account.id, &error, report_network_errors);
            Err(error)
        }
        None => {
            ctx.sync_status.finish(&account.id, SyncModule::Mail);
            info!(account = %account.id, folders = targets.len(), "mail sync finished");
            Ok(())
        }
    }
}

/// Sync a single folder by local id (open folder / "sync now" on a folder).
pub async fn sync_folder_by_id(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder_id: &str,
) -> Result<()> {
    sync_folder_by_id_with(ctx, conn, account, folder_id, true).await
}

/// Like [`sync_folder_by_id`] with control over transient error reporting.
pub async fn sync_folder_by_id_with(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder_id: &str,
    report_network_errors: bool,
) -> Result<()> {
    let folder_id = folder_id.to_string();
    let folder = ctx
        .db
        .read(move |db| repo::folders::get_required(db, &folder_id))
        .await?;
    if folder.account_id != account.id {
        return Err(AppError::invalid_input("folder belongs to another account"));
    }
    ctx.sync_status.begin(&account.id, SyncModule::Mail, None);
    match messages::sync_folder(ctx, conn, account, &folder).await {
        Ok(outcome) => {
            notify::after_new_mail(
                ctx,
                account,
                &folder,
                &outcome.new_messages,
                outcome.initial,
            )
            .await?;
            if folder.role == FolderRole::Inbox {
                let limit = ctx.settings.current().mail.prefetch_unread_bodies;
                if limit > 0 {
                    let _ = body::prefetch_unread(ctx, conn, &account.id, &folder, limit).await;
                }
            }
            ctx.sync_status.finish(&account.id, SyncModule::Mail);
            Ok(())
        }
        Err(error) => {
            note_sync_error(ctx, &account.id, &error, report_network_errors);
            Err(error)
        }
    }
}

/// List-only folder refresh (used after folder operations).
pub async fn refresh_folders(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account_id: &str,
) -> Result<Vec<Folder>> {
    folders::sync(ctx, conn, account_id).await
}

/// All folders in a stable order.
pub async fn load_folders(ctx: &CoreContext, account_id: &str) -> Result<Vec<Folder>> {
    let account_id = account_id.to_string();
    ctx.db
        .read(move |db| repo::folders::list(db, &account_id))
        .await
}

/// Poll every sync-enabled folder except the Inbox (the IDLE connection
/// handles that one).
pub async fn sync_other_folders(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
) -> Result<()> {
    sync_other_folders_with(ctx, conn, account, true).await
}

/// Like [`sync_other_folders`] with control over transient error reporting.
pub async fn sync_other_folders_with(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    report_network_errors: bool,
) -> Result<()> {
    let folders = load_folders(ctx, &account.id).await?;
    let targets: Vec<Folder> = folders
        .into_iter()
        .filter(|folder| {
            folder.sync_enabled && folder.is_selectable && folder.role != FolderRole::Inbox
        })
        .collect();
    if targets.is_empty() {
        return Ok(());
    }
    ctx.sync_status.begin(&account.id, SyncModule::Mail, None);
    let mut first_error: Option<AppError> = None;
    for folder in targets {
        match messages::sync_folder(ctx, conn, account, &folder).await {
            Ok(outcome) => {
                if let Err(error) = notify::after_new_mail(
                    ctx,
                    account,
                    &folder,
                    &outcome.new_messages,
                    outcome.initial,
                )
                .await
                {
                    debug!(%error, folder = %folder.remote_path, "notify failed");
                }
            }
            Err(error) => {
                warn!(%error, folder = %folder.remote_path, "folder poll failed");
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }
    match first_error {
        Some(error) => {
            note_sync_error(ctx, &account.id, &error, report_network_errors);
        }
        None => {
            ctx.sync_status.finish(&account.id, SyncModule::Mail);
        }
    }
    Ok(())
}
