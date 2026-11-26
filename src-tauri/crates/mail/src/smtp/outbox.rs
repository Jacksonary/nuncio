//! Outbox processor: sends queued RFC822 messages and performs post-send
//! housekeeping (Answered/$Forwarded flags, Drafts cleanup, Sent APPEND,
//! recipient history).

use chrono::Utc;
use nuncio_core::models::{
    Address, Draft, FolderRole, OutboxDraftCleanup, OutboxItem, OutboxState, ProviderKind,
    ReplyMode,
};
use nuncio_core::{events, repo, AppError, CoreContext, Result};
use tracing::{debug, info, warn};

const MAX_ATTEMPTS: u32 = 8;

/// How long a send may stay in `sending` before it is assumed abandoned.
///
/// `send_raw` cancels a message after `SEND_DEADLINE` (5 min), so any row still
/// in `sending` past 8 minutes belongs to a task that is gone: crashed, killed,
/// or cut off mid-send. The two constants are tied together deliberately —
/// shortening this below the send deadline would let a live transfer be
/// reclaimed and delivered twice.
const SENDING_LEASE: chrono::Duration = chrono::Duration::minutes(8);

/// Send every due outbox item for one account.
pub async fn process_due(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
) -> Result<usize> {
    recover_stuck(ctx, &account.id).await?;
    let now = Utc::now();
    let account_id = account.id.clone();
    let items = ctx
        .db
        .read(move |db| repo::outbox::list(db, Some(&account_id)))
        .await?;
    let mut sent = 0;
    for item in items.into_iter().filter(|item| {
        item.state == OutboxState::Pending && item.next_retry_at.map(|at| at <= now).unwrap_or(true)
    }) {
        match process_one(ctx, account, &item.id).await {
            Ok(()) => sent += 1,
            Err(error) => warn!(%error, outbox = %item.id, "outbox item failed"),
        }
    }
    Ok(sent)
}

/// Send one outbox item. Retry bookkeeping is persisted on the row, so this
/// returns `Ok` even when delivery failed (the item stays pending/failed).
pub async fn process_one(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    outbox_id: &str,
) -> Result<()> {
    let item = ctx
        .db
        .read({
            let outbox_id = outbox_id.to_string();
            move |db| repo::outbox::get(db, &outbox_id)
        })
        .await?
        .ok_or_else(|| AppError::not_found(format!("outbox item {outbox_id} not found")))?;
    if item.state == OutboxState::Sent {
        return Ok(());
    }

    // Conditional transition avoids two outbox passes sending the same item.
    let item_id = item.id.clone();
    let claimed = ctx
        .db
        .write(move |db| {
            let changed = db.execute(
                "UPDATE outbox SET state = 'sending', sending_started_at = ?2
                 WHERE id = ?1 AND state IN ('pending', 'failed')",
                rusqlite::params![item_id, nuncio_core::repo::dt_string(Utc::now())],
            )?;
            Ok(changed)
        })
        .await?;
    if claimed == 0 {
        debug!(outbox = %item.id, "outbox item claimed by another worker");
        return Ok(());
    }

    // The budget starts at the claim, not after the file read: reading a large
    // staged message, refreshing the credential and talking SMTP must all fit
    // inside one window that stays below `SENDING_LEASE`.
    let deadline = tokio::time::Instant::now() + crate::smtp::send::SEND_DEADLINE;

    let bytes = match tokio::time::timeout_at(deadline, tokio::fs::read(&item.rfc822_path)).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => {
            let message = format!("staged message is missing: {error}");
            fail_permanently(ctx, &item, &message).await?;
            return Ok(());
        }
        Err(_) => {
            let message = "reading the staged message exceeded the send deadline".to_string();
            fail_permanently(ctx, &item, &message).await?;
            return Ok(());
        }
    };

    let send_result = crate::smtp::send::send_account_message_within(
        ctx,
        account,
        &bytes,
        Some(&item.recipients),
        deadline,
    )
    .await;
    match send_result {
        Ok(()) => {
            let item_id = item.id.clone();
            ctx.db
                .write(move |db| repo::outbox::mark_sent(db, &item_id))
                .await?;
            post_send(ctx, account, &item).await?;
            if let Err(error) =
                crate::service::outbox::refresh_outbox_folder(ctx, &account.id).await
            {
                debug!(%error, "could not refresh the Outbox folder");
            }
            info!(outbox = %item.id, "outbox item sent");
        }
        Err(error) => {
            // `send_account_message_within` already reported an auth failure
            // inside its own deadline; reporting again here was an unbounded
            // tail that could outlive the lease. Keep the state transition but
            // never block on the report.
            if error.code() == "auth" {
                debug!(outbox = %item.id, "auth failure reported by the send path");
            }
            let message = error.message();
            let state = ctx
                .db
                .write({
                    let item_id = item.id.clone();
                    let message = message.clone();
                    move |db| {
                        repo::outbox::mark_failed(db, &item_id, &message, Utc::now(), MAX_ATTEMPTS)
                    }
                })
                .await?;
            if state == OutboxState::Failed {
                events::emit_op_failed(
                    ctx.events.as_ref(),
                    account.id.clone(),
                    &format!("Send \"{}\"", item.subject),
                    &message,
                );
                warn!(outbox = %item.id, error = %message, "outbox item failed permanently");
            } else {
                debug!(outbox = %item.id, error = %message, "outbox item scheduled for retry");
            }
        }
    }
    Ok(())
}

async fn post_send(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    item: &OutboxItem,
) -> Result<()> {
    // The Sent copy is independent of draft bookkeeping: a draft that was never
    // auto-saved (no id yet) still has to be filed away, otherwise providers
    // that do not save sent mail themselves would lose the record entirely.
    record_recipients(ctx, &item.recipients).await?;
    if let Err(error) = append_to_sent(ctx, account, item).await {
        warn!(outbox = %item.id, %error, "could not append the message to Sent");
    }

    let Some(draft_id) = item.draft_id.clone() else {
        return Ok(());
    };
    let draft = ctx
        .db
        .read({
            let draft_id = draft_id.clone();
            move |db| repo::drafts::get(db, &draft_id)
        })
        .await?;

    let mut affected_folders = Vec::new();
    if let Some(draft) = &draft {
        apply_source_flags(ctx, account, draft).await?;
        // Consume the cleanup decision persisted when the message was queued.
        // This path must never enqueue a delete from `remote_draft_message_id`:
        // a queued delete or an in-flight APPEND already owns the remote copy,
        // and a candidate must be kept. Only a local projection with no server
        // UID (a ghost of the now-deleted draft) is reclaimed here.
        if item.draft_cleanup == OutboxDraftCleanup::None {
            if let Some(folder_id) = reclaim_unaddressable_projection(ctx, account, &draft_id).await
            {
                affected_folders.push(folder_id);
            }
        }
        affected_folders.extend(
            ctx.db
                .read({
                    let account_id = account.id.clone();
                    move |db| repo::folders::by_role(db, &account_id, FolderRole::Drafts)
                })
                .await?
                .map(|folder| folder.id),
        );
        let staging = ctx.paths.draft_staging_dir(&draft_id);
        if let Err(error) = tokio::fs::remove_dir_all(&staging).await {
            debug!(%error, path = %staging.display(), "draft staging cleanup skipped");
        }
        let draft_id = draft_id.clone();
        ctx.db
            .write(move |db| repo::drafts::delete(db, &draft_id))
            .await?;
    }

    // Notify the UI that drafts/outbox changed.
    events::emit_mail_changed(
        ctx.events.as_ref(),
        account.id.clone(),
        affected_folders,
        Vec::new(),
        events::MailChangeKind::Updated,
    );
    Ok(())
}

async fn apply_source_flags(
    ctx: &CoreContext,
    _account: &nuncio_core::models::Account,
    draft: &Draft,
) -> Result<()> {
    let Some(source_id) = draft.source_message_id.clone() else {
        return Ok(());
    };
    let stored = ctx
        .db
        .read({
            let source_id = source_id.clone();
            move |db| repo::messages::get(db, &source_id)
        })
        .await?;
    let Some(stored) = stored else {
        return Ok(());
    };
    let answered = matches!(
        draft.reply_mode,
        Some(ReplyMode::Reply) | Some(ReplyMode::ReplyAll)
    );
    let forwarded = matches!(
        draft.reply_mode,
        Some(ReplyMode::Forward) | Some(ReplyMode::ForwardAsAttachment)
    );
    if !answered && !forwarded {
        return Ok(());
    }
    // Captured outside the transaction and re-checked inside it. The send may
    // have taken long enough for a sync to delete/rebind the source projection;
    // the queue row and its writer must not be created for a copy that no
    // longer carries this identity.
    let identity = repo::messages::MessageIdentity::of(&stored);
    // A reply/forward may be sent from a different account than the one that
    // holds the original message. Folder/UID only identify a copy inside the
    // *source* mailbox, so the queued operation must belong to that account;
    // executing it under the sender would SELECT the same folder/UID on the
    // wrong server and mark an unrelated message.
    let source_account_id = stored.summary.account_id.clone();
    let source_local_only = ctx
        .db
        .read({
            let source_account_id = source_account_id.clone();
            move |db| repo::accounts::get(db, &source_account_id)
        })
        .await?
        .map(|source| matches!(source.provider, ProviderKind::Local | ProviderKind::Pop3))
        // A vanished source account cannot receive server mail; never create
        // an orphan queue row under it.
        .unwrap_or(true);
    let queued = if source_local_only {
        None
    } else {
        stored
            .uid
            .map(|uid| {
                let payload = crate::ops::SetFlagsOp {
                    folder_id: stored.summary.folder_id.clone(),
                    uids: vec![uid],
                    seen: None,
                    flagged: None,
                    answered: answered.then_some(true),
                    forwarded: forwarded.then_some(true),
                    source_uidvalidity: stored.uidvalidity,
                    previous: vec![crate::ops::FlagRollback {
                        uid,
                        seen: stored.summary.seen,
                        flagged: stored.summary.flagged,
                        answered: stored.summary.answered,
                        forwarded: stored.summary.forwarded,
                    }],
                    message_ids: vec![stored.summary.id.clone()],
                    outcomes: Vec::new(),
                    remote_ids: stored
                        .gm_msgid
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(|value| vec![value.to_string()])
                        .unwrap_or_default(),
                };
                serde_json::to_value(payload)
                    .map(|payload| (source_account_id.clone(), payload))
                    .map_err(|error| AppError::internal(format!("cannot encode setFlags: {error}")))
            })
            .transpose()?
    };
    ctx.db
        .write(move |db| {
            crate::ops::apply_source_flags_in_db(
                db,
                &identity,
                answered,
                forwarded,
                queued.as_ref(),
            )
        })
        .await?;
    Ok(())
}

/// Remove the Drafts projection of a sent draft when it has no server UID.
///
/// A projection with a UID is a real server copy: its cleanup belongs to the
/// queued delete or in-flight APPEND, never to the SMTP post-send path. Only an
/// unaddressable local echo is reclaimed so Drafts does not keep an unopenable
/// row.
async fn reclaim_unaddressable_projection(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    draft_id: &str,
) -> Option<String> {
    let message_id = crate::util::draft_message_id(draft_id);
    let account_id = account.id.clone();
    ctx.db
        .write(move |db| {
            let Some(stored) = repo::messages::get(db, &message_id)? else {
                return Ok(None);
            };
            if stored.summary.account_id != account_id || stored.uid.is_some() {
                return Ok(None);
            }
            let folder_id = stored.summary.folder_id.clone();
            repo::messages::delete_local(db, &[message_id], true)?;
            Ok(Some(folder_id))
        })
        .await
        .ok()
        .flatten()
}

/// Whether this send must APPEND its own copy to Sent.
///
/// Reads the provider directory and the account's `sent_copy` override
/// directly instead of the persisted `capabilities.auto_saves_sent`: that flag
/// is only refreshed when the IMAP session reconnects, so using it here would
/// make a settings change wait for the next connection (or silently keep
/// duplicating Sent mail on a Yahoo/AOL account that stays connected).
fn client_appends_sent(account: &nuncio_core::models::Account) -> bool {
    !nuncio_core::providers::server_files_sent(account)
}

async fn append_to_sent(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    item: &OutboxItem,
) -> Result<()> {
    if !client_appends_sent(account) {
        return Ok(());
    }
    let account_id = account.id.clone();
    let sent = ctx
        .db
        .read(move |db| repo::folders::by_role(db, &account_id, FolderRole::Sent))
        .await?;
    let Some(sent) = sent else {
        debug!(account = %account.id, "no Sent folder; skipping APPEND");
        return Ok(());
    };
    // The transmitted bytes intentionally carry no Bcc header. The stored Sent
    // copy keeps it, like regular mail clients, so the sender can see who the
    // message went to.
    let with_bcc = match sent_copy_with_bcc(ctx, item).await {
        Ok(Some(path)) => Some(path),
        Ok(None) => None,
        Err(error) => {
            debug!(%error, outbox = %item.id, "could not prepare the Sent copy with Bcc");
            None
        }
    };
    let path = with_bcc.as_deref().unwrap_or(item.rfc822_path.as_str());
    crate::ops::enqueue_append_to_sent(ctx, &account.id, &sent.id, path).await
}

/// Write a copy of the staged message that restores the Bcc header. The Bcc
/// recipients are the outbox recipients that are neither `To` nor `Cc` in the
/// built message. Returns `None` when there is no Bcc recipient.
async fn sent_copy_with_bcc(ctx: &CoreContext, item: &OutboxItem) -> Result<Option<String>> {
    let bytes = tokio::fs::read(&item.rfc822_path)
        .await
        .map_err(|error| AppError::io(format!("cannot read the staged message: {error}")))?;
    let parsed = match nuncio_mime::parse(&bytes) {
        Ok(parsed) => parsed,
        Err(error) => {
            debug!(%error, outbox = %item.id, "cannot parse the sent message for Bcc");
            return Ok(None);
        }
    };
    let mut visible: std::collections::HashSet<String> = parsed
        .headers
        .to
        .iter()
        .chain(parsed.headers.cc.iter())
        .map(|address| nuncio_core::text::normalize_email(&address.email))
        .filter(|email| !email.is_empty())
        .collect();
    visible.extend(
        parsed
            .headers
            .bcc
            .iter()
            .map(|address| nuncio_core::text::normalize_email(&address.email)),
    );
    let bcc: Vec<Address> = item
        .recipients
        .iter()
        .filter(|address| {
            let email = nuncio_core::text::normalize_email(&address.email);
            !email.is_empty() && !visible.contains(&email)
        })
        .cloned()
        .collect();
    let bcc = nuncio_mime::dedupe_addresses(bcc);
    if bcc.is_empty() {
        return Ok(None);
    }
    let with_bcc = nuncio_mime::add_bcc_header(&bytes, &bcc)?;
    let path = ctx
        .paths
        .staging_dir()
        .join(format!("sent-{}.eml", nuncio_core::models::new_id()));
    ctx.paths.ensure_parent(&path)?;
    tokio::fs::write(&path, &with_bcc)
        .await
        .map_err(|error| AppError::io(format!("cannot stage the Sent copy: {error}")))?;
    Ok(Some(path.to_string_lossy().into_owned()))
}

async fn record_recipients(ctx: &CoreContext, recipients: &[Address]) -> Result<()> {
    if recipients.is_empty() {
        return Ok(());
    }
    let recipients = recipients.to_vec();
    ctx.db
        .write(move |db| repo::recipient_history::record(db, &recipients))
        .await
}

async fn fail_permanently(ctx: &CoreContext, item: &OutboxItem, message: &str) -> Result<()> {
    let item_id = item.id.clone();
    let message_owned = message.to_string();
    ctx.db
        .write(move |db| repo::outbox::mark_failed(db, &item_id, &message_owned, Utc::now(), 1))
        .await?;
    events::emit_op_failed(
        ctx.events.as_ref(),
        item.account_id.clone(),
        &format!("Send \"{}\"", item.subject),
        message,
    );
    Ok(())
}

/// Reset `sending` rows abandoned by a crash or forced shutdown.
///
/// The cutoff is measured from when the send *started*, not when the item was
/// queued: an item can sit pending for hours behind an offline queue and still
/// be actively transmitting when another account's pass runs. Without this the
/// old check would knock a live send back to `pending`, letting a second worker
/// claim it and deliver the message twice.
async fn recover_stuck(ctx: &CoreContext, account_id: &str) -> Result<()> {
    // Every configured account polls on its own timer. Recovery is therefore
    // scoped to the row's own account: an expired row of account A must not be
    // revived by account B's pass, which is how a second worker could end up
    // sending a message whose first attempt was still running.
    let cutoff = Utc::now() - SENDING_LEASE;
    let account_id = account_id.to_string();
    ctx.db
        .write(move |db| {
            db.execute(
                "UPDATE outbox SET state = 'pending', sending_started_at = NULL
                 WHERE state = 'sending'
                   AND account_id = ?2
                   AND COALESCE(sending_started_at, created_at) < ?1",
                rusqlite::params![nuncio_core::repo::dt_string(cutoff), account_id],
            )?;
            Ok(())
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::models::{
        Account, AccountState, AuthKind, ProviderCapabilities, ProviderKind, SentCopyMode,
    };

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

    #[test]
    fn sent_copy_decision_follows_the_directory_and_the_override() {
        // Yahoo/AOL and Gmail file the sent copy themselves.
        assert!(!client_appends_sent(&account("me@yahoo.com")));
        assert!(!client_appends_sent(&account("me@aol.com")));
        assert!(!client_appends_sent(&account("me@gmail.com")));
        // Everyone else (including configurable ones like QQ) needs the client
        // to APPEND.
        assert!(client_appends_sent(&account("me@yandex.com")));
        assert!(client_appends_sent(&account("me@qq.com")));

        // The per-account choice is honoured on the next send, without waiting
        // for a reconnect to refresh `capabilities.auto_saves_sent`.
        let mut yahoo = account("me@yahoo.com");
        yahoo.sent_copy = Some(SentCopyMode::Client);
        assert!(client_appends_sent(&yahoo));

        let mut yandex = account("me@yandex.com");
        yandex.sent_copy = Some(SentCopyMode::Server);
        assert!(!client_appends_sent(&yandex));
    }

    #[test]
    fn gmail_capability_probe_skips_the_append_too() {
        let mut unknown = account("me@unknown.example");
        assert!(client_appends_sent(&unknown));
        // A live X-GM-EXT-1 probe is positive evidence even when the domain
        // is not in the directory.
        unknown.capabilities.gmail_extensions = true;
        assert!(!client_appends_sent(&unknown));
    }
}
