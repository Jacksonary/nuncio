//! POP3 incremental download (UIDL diff) into a local Inbox folder.

use std::collections::{HashMap, HashSet};

use chrono::Utc;
use nuncio_core::models::SyncModule;
use nuncio_core::models::{new_id, Attachment, Folder, FolderRole, MessageSummary, StoredMessage};
use nuncio_core::{events, repo, AppError, CoreContext, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::pop3::Pop3Client;
use crate::sync::notify;
use crate::util::{attachment_id, pop3_message_id};

const STATE_KEY: &str = "uidl_state";
const MODULE: &str = "pop3";
const MAX_MESSAGE_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Pop3State {
    /// UIDL -> local message id.
    #[serde(default)]
    seen: HashMap<String, String>,
    /// UIDLs the client asked the server to delete (leave-on-server = false).
    #[serde(default)]
    deleted: Vec<String>,
}

/// Download new messages and reconcile deletions using UIDL.
pub async fn sync_account(ctx: &CoreContext, account: &nuncio_core::models::Account) -> Result<()> {
    let config = account
        .pop3
        .as_ref()
        .ok_or_else(|| AppError::invalid_input("account has no POP3 server configured"))?;
    ctx.sync_status.begin(&account.id, SyncModule::Mail, None);
    let result = sync_inner(ctx, account, config).await;
    match &result {
        Ok(()) => {
            ctx.sync_status.finish(&account.id, SyncModule::Mail);
            info!(account = %account.id, "POP3 sync finished");
        }
        Err(error) => {
            ctx.sync_status
                .fail(&account.id, SyncModule::Mail, error.message());
        }
    }
    result
}

async fn sync_inner(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    config: &nuncio_core::models::ServerConfig,
) -> Result<()> {
    let auth = ctx.creds.mail_auth(account).await?;
    let mut client = Pop3Client::connect(config, &auth).await?;
    let remote = client.uidl().await?;
    let remote_uids: HashSet<String> = remote.iter().map(|(_, uidl)| uidl.clone()).collect();
    let sizes: HashMap<u32, u64> = match client.list().await {
        Ok(entries) => entries.into_iter().collect(),
        Err(error) => {
            // Some POP3 servers implement UIDL but not LIST; size limits are
            // only an optimization, so keep going.
            debug!(%error, "POP3 LIST failed; continuing without sizes");
            HashMap::new()
        }
    };
    debug!(account = %account.id, messages = remote.len(), "POP3 UIDL listing");

    let folder = ensure_inbox(ctx, account).await?;

    let account_id = account.id.clone();
    let state: Pop3State = ctx
        .db
        .read({
            let account_id = account_id.clone();
            move |db| {
                repo::sync_state::get_json(db, &account_id, MODULE, STATE_KEY)
                    .map(Option::unwrap_or_default)
            }
        })
        .await?;
    let was_initial = state.seen.is_empty();
    let mut state = state;

    let leave_on_server = ctx
        .db
        .read({
            let account_id = account_id.clone();
            move |db| repo::sync_state::get(db, &account_id, MODULE, "leave_on_server")
        })
        .await?
        .map(|value| value != "false")
        .unwrap_or(true);

    let start_uid = ctx
        .db
        .read({
            let folder_id = folder.id.clone();
            move |db| {
                db.query_row(
                    "SELECT COALESCE(MAX(uid), 0) FROM message WHERE folder_id = ?1",
                    rusqlite::params![folder_id],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(AppError::from)
            }
        })
        .await?;

    let mut new_messages: Vec<MessageSummary> = Vec::new();
    let mut next_uid = start_uid + 1;
    for (number, uidl) in &remote {
        if state.seen.contains_key(uidl) {
            continue;
        }
        let size = sizes.get(number).copied().unwrap_or(0);
        if size > MAX_MESSAGE_BYTES {
            warn!(uidl, size, "POP3 message too large; skipping");
            continue;
        }
        let raw = client.retr(*number).await?;
        if raw.len() as u64 > MAX_MESSAGE_BYTES {
            warn!(
                uidl,
                size = raw.len(),
                "POP3 message too large after download; skipping"
            );
            continue;
        }
        match store_message(ctx, account, &folder, uidl, next_uid, &raw).await {
            Ok(summary) => {
                state.seen.insert(uidl.clone(), summary.id.clone());
                new_messages.push(summary);
                next_uid += 1;
                if !leave_on_server {
                    if let Err(error) = client.dele(*number).await {
                        warn!(%error, uidl, "POP3 DELE failed");
                    } else {
                        state.deleted.push(uidl.clone());
                    }
                }
            }
            Err(error) => {
                warn!(%error, uidl, "POP3 message could not be stored");
            }
        }
    }

    // Messages deleted on the server (by another client) disappear locally.
    let mut removed = Vec::new();
    let deleted_set: HashSet<&String> = state.deleted.iter().collect();
    state.seen.retain(|uidl, id| {
        if remote_uids.contains(uidl) || deleted_set.contains(uidl) {
            true
        } else {
            removed.push(id.clone());
            false
        }
    });
    if !removed.is_empty() {
        let ids = removed.clone();
        ctx.db
            .write(move |db| repo::messages::delete_local(db, &ids, true))
            .await?;
        events::emit_mail_changed(
            ctx.events.as_ref(),
            account.id.clone(),
            vec![folder.id.clone()],
            removed,
            events::MailChangeKind::Deleted,
        );
    }

    let state_for_db = state.clone();
    ctx.db
        .write({
            let account_id = account_id.clone();
            move |db| repo::sync_state::set_json(db, &account_id, MODULE, STATE_KEY, &state_for_db)
        })
        .await?;

    if !new_messages.is_empty() {
        let ids: Vec<String> = new_messages.iter().map(|item| item.id.clone()).collect();
        events::emit_mail_new(
            ctx.events.as_ref(),
            account.id.clone(),
            folder.id.clone(),
            ids,
        );
        notify::after_new_mail(ctx, account, &folder, &new_messages, was_initial).await?;
    } else {
        notify::update_badge(ctx).await?;
    }

    client.quit().await;
    Ok(())
}

async fn ensure_inbox(ctx: &CoreContext, account: &nuncio_core::models::Account) -> Result<Folder> {
    let account_id = account.id.clone();
    if let Some(folder) = ctx
        .db
        .read({
            let account_id = account_id.clone();
            move |db| repo::folders::by_role(db, &account_id, FolderRole::Inbox)
        })
        .await?
    {
        return Ok(folder);
    }
    let folder = Folder {
        id: new_id(),
        account_id: account_id.clone(),
        parent_id: None,
        remote_path: "INBOX".into(),
        name: "Inbox".into(),
        role: FolderRole::Inbox,
        unread_count: 0,
        total_count: 0,
        is_favorite: true,
        sync_enabled: true,
        is_selectable: true,
        sort_order: 0,
        last_sync_at: None,
    };
    let stored = ctx
        .db
        .write(move |db| repo::folders::upsert_remote(db, &folder))
        .await?;
    events::emit_folders_changed(ctx.events.as_ref(), account_id, vec![stored.id.clone()]);
    Ok(stored)
}

async fn store_message(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    uidl: &str,
    uid: i64,
    raw: &[u8],
) -> Result<MessageSummary> {
    let parsed = nuncio_mime::parse(raw)?;
    let headers = &parsed.headers;
    let id = pop3_message_id(&account.id, uidl);
    let date = headers.date.unwrap_or_else(Utc::now);
    let thread_id = crate::util::thread_id(
        headers.message_id.as_deref(),
        headers.in_reply_to.as_deref(),
        &headers.references,
        &headers.subject,
        None,
    );
    let mut summary = MessageSummary::new(
        id.clone(),
        account.id.clone(),
        folder.id.clone(),
        thread_id,
        date,
    );
    summary.subject = headers.subject.clone();
    summary.r#from = headers.from.clone();
    summary.to = headers.to.clone();
    summary.cc = headers.cc.clone();
    summary.preview = nuncio_core::text::trim_preview(&parsed.preview, 300);
    summary.has_attachments = !parsed.attachments.is_empty();
    summary.importance = headers.importance;
    summary.size = raw.len() as u64;

    let mut stored = StoredMessage::new(summary.clone());
    stored.bcc = headers.bcc.clone();
    stored.reply_to = headers.reply_to.clone();
    stored.message_id_header = headers.message_id.clone();
    stored.in_reply_to = headers.in_reply_to.clone();
    stored.references = headers.references.clone();
    stored.uid = Some(uid);
    stored.uidvalidity = None;
    stored.raw_headers = Some(headers.raw.clone());
    stored.internal_date = headers.date;

    let path = ctx.paths.mime_path(&account.id, &id);
    ctx.paths.ensure_parent(&path)?;
    let write_path = path.clone();
    let bytes = raw.to_vec();
    tokio::task::spawn_blocking(move || std::fs::write(&write_path, &bytes))
        .await
        .map_err(|error| AppError::internal(format!("POP3 body write task failed: {error}")))?
        .map_err(|error| AppError::io(format!("cannot cache POP3 message: {error}")))?;
    stored.body_path = Some(path.to_string_lossy().to_string());
    stored.body_cached_at = Some(Utc::now());

    let body_text = parsed
        .text
        .clone()
        .or_else(|| parsed.html.as_deref().map(nuncio_mime::html_to_text))
        .unwrap_or_default();
    let attachments: Vec<Attachment> = parsed
        .attachments
        .iter()
        .enumerate()
        .map(|(index, attachment)| Attachment {
            id: attachment_id(&id, index, attachment.content_id.as_deref()),
            message_id: id.clone(),
            filename: attachment.filename.clone(),
            mime_type: attachment.mime_type.clone(),
            size: attachment.size,
            content_id: attachment.content_id.clone(),
            is_inline: attachment.is_inline,
            is_downloaded: false,
        })
        .collect();

    let folder_id = folder.id.clone();
    let preview = summary.preview.clone();
    let message_id = id.clone();
    let path_string = stored.body_path.clone().unwrap_or_default();
    ctx.db
        .write(move |db| {
            repo::messages::upsert(db, &stored)?;
            repo::messages::set_body(db, &message_id, Some(&path_string), Some(&body_text))?;
            repo::messages::update_preview(db, &message_id, &preview)?;
            repo::attachments::replace_for_message(db, &message_id, &attachments)?;
            repo::folders::recompute_counts(db, &folder_id)
        })
        .await?;
    Ok(summary)
}
