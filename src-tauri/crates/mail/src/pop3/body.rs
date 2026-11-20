//! POP3 body-on-demand: download one message with `RETR` and cache it the way
//! IMAP bodies are cached (`Paths::mime/<account>/<xx>/<hash>.eml` plus the
//! `body_path` / `body_cached_at` columns, parsed text and FTS index).
//!
//! Listing only stores headers, so a body is materialised the first time it is
//! really needed: opening a message, resolving an attachment, or anything that
//! asks for the raw RFC822 document. A cached body is never downloaded twice.
//! A message that has already left the server (deleted here under
//! leave-on-server = false, or deleted elsewhere) fails with a clear
//! `notFound` instead of retrying forever: the header row stays listable, only
//! its body is gone.

use std::path::PathBuf;

use chrono::Utc;
use nuncio_core::models::{Attachment, StoredMessage};
use nuncio_core::{repo, AppError, CoreContext, Result};
use tracing::{debug, warn};

use crate::pop3::sync::{load_state, MAX_MESSAGE_BYTES};
use crate::pop3::Pop3Client;
use crate::util::attachment_id;

/// Remember that this UIDL can no longer be downloaded from the server.
///
/// The same `deleted` list the sync already maintains for leave-on-server =
/// false is reused, so the next open reports [`BODY_GONE`] immediately instead
/// of reconnecting every time. The message row is untouched: it stays listable
/// and searchable by its header.
async fn mark_unreachable(ctx: &CoreContext, account_id: &str, uidl: &str) -> Result<()> {
    warn!(uidl, "POP3 body is no longer reachable on the server");
    let mut state = load_state(ctx, account_id).await?;
    if state.deleted.iter().any(|entry| entry == uidl) {
        return Ok(());
    }
    state.deleted.push(uidl.to_string());
    let state_for_db = state;
    let account_id = account_id.to_string();
    ctx.db
        .write(move |db| {
            repo::sync_state::set_json(
                db,
                &account_id,
                crate::pop3::sync::MODULE,
                crate::pop3::sync::STATE_KEY,
                &state_for_db,
            )
        })
        .await?;
    Ok(())
}

/// Download (or return the cached) raw RFC822 body of a POP3 message.
pub async fn fetch_body(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    message_id: &str,
) -> Result<PathBuf> {
    let stored = ctx
        .db
        .read({
            let message_id = message_id.to_string();
            move |db| repo::messages::get(db, &message_id)
        })
        .await?
        .ok_or_else(|| AppError::not_found(format!("message {message_id} not found")))?;
    if stored.summary.account_id != account.id {
        return Err(AppError::invalid_input(
            "message belongs to another account",
        ));
    }

    if let Some(path) = stored.body_path.as_deref() {
        let path = PathBuf::from(path);
        if tokio::fs::metadata(&path).await.is_ok() {
            return Ok(path);
        }
        debug!(
            message = %stored.summary.id,
            "cached POP3 body file is missing; re-downloading"
        );
    }

    let config = account
        .pop3
        .as_ref()
        .ok_or_else(|| AppError::invalid_input("account has no POP3 server configured"))?;
    // One UIDL lookup per attempt is enough: a UIDL that already left the
    // server is remembered in the sync state, so a reopen skips the round trip.
    let state = load_state(ctx, &account.id).await?;
    let uidl = match state
        .seen
        .into_iter()
        .find_map(|(uidl, id)| (id == stored.summary.id).then_some(uidl))
    {
        Some(uidl) => uidl,
        None => return Err(AppError::not_found(BODY_GONE)),
    };
    if state.deleted.iter().any(|entry| entry == &uidl) {
        return Err(AppError::not_found(BODY_GONE));
    }

    let auth = ctx.creds.mail_auth(account).await?;
    let mut client = Pop3Client::connect(config, &auth).await?;

    // Resolve UIDL -> current message number. Numbers shift whenever another
    // client deletes mail, so they are never stored.
    let remote = match client.uidl().await {
        Ok(remote) => remote,
        Err(error) => {
            client.quit().await;
            return Err(error);
        }
    };
    let Some((number, _)) = remote
        .into_iter()
        .find(|(_, remote_uidl)| remote_uidl == &uidl)
    else {
        client.quit().await;
        mark_unreachable(ctx, &account.id, &uidl).await?;
        return Err(AppError::not_found(BODY_GONE));
    };

    // Probe the declared size first so an oversized message never reaches the
    // process heap. `LIST` is optional on the server; unknown means "try".
    let declared = match client.list().await {
        Ok(entries) => entries
            .into_iter()
            .find(|(entry, _)| *entry == number)
            .map(|(_, size)| size)
            .unwrap_or(0),
        Err(error) => {
            debug!(%error, "POP3 LIST failed during body fetch; continuing without a size");
            0
        }
    };
    if declared > MAX_MESSAGE_BYTES {
        client.quit().await;
        return Err(AppError::invalid_input(format!(
            "message is too large to open ({} MB, limit {} MB)",
            declared / (1024 * 1024),
            MAX_MESSAGE_BYTES / (1024 * 1024)
        )));
    }

    let raw = match client.retr(number).await {
        Ok(raw) => raw,
        Err(error) => {
            client.quit().await;
            if error.code() == "protocol" {
                // The message vanished between UIDL and RETR.
                mark_unreachable(ctx, &account.id, &uidl).await?;
                return Err(AppError::not_found(BODY_GONE));
            }
            return Err(error);
        }
    };
    client.quit().await;

    if raw.len() as u64 > MAX_MESSAGE_BYTES {
        return Err(AppError::invalid_input(
            "message body exceeds the size limit; only headers were stored",
        ));
    }

    let path = ctx.paths.mime_path(&account.id, &stored.summary.id);
    ctx.paths.ensure_parent(&path)?;
    tokio::fs::write(&path, &raw)
        .await
        .map_err(|error| AppError::io(format!("cannot write message cache: {error}")))?;

    store_parsed_body(ctx, stored, path.clone(), &raw).await?;
    debug!(
        message = %path.display(),
        bytes = raw.len(),
        "POP3 message body cached"
    );
    Ok(path)
}

/// Error text for a body that can no longer be downloaded.
pub const BODY_GONE: &str =
    "message was removed from the POP3 server; its body is no longer available";

/// Parse a downloaded message and update preview, FTS text and attachments.
async fn store_parsed_body(
    ctx: &CoreContext,
    mut stored: StoredMessage,
    path: PathBuf,
    raw: &[u8],
) -> Result<()> {
    let path_string = path.to_string_lossy().to_string();
    let parsed = match nuncio_mime::parse(raw) {
        Ok(parsed) => parsed,
        Err(error) => {
            warn!(%error, message = %stored.summary.id, "failed to parse downloaded POP3 MIME body");
            let message_id = stored.summary.id.clone();
            ctx.db
                .write(move |db| {
                    repo::messages::set_body(db, &message_id, Some(&path_string), None)
                })
                .await?;
            return Ok(());
        }
    };

    let body_text = parsed
        .text
        .clone()
        .or_else(|| parsed.html.as_deref().map(nuncio_mime::html_to_text))
        .unwrap_or_default();
    let headers = &parsed.headers;
    stored.summary.subject = headers.subject.clone();
    if let Some(from) = headers.from.clone() {
        stored.summary.r#from = Some(from);
    }
    if !headers.to.is_empty() {
        stored.summary.to = headers.to.clone();
    }
    if !headers.cc.is_empty() {
        stored.summary.cc = headers.cc.clone();
    }
    stored.bcc = headers.bcc.clone();
    stored.reply_to = headers.reply_to.clone();
    if let Some(date) = headers.date {
        stored.summary.date = date;
    }
    stored.message_id_header = headers.message_id.clone();
    stored.in_reply_to = headers.in_reply_to.clone();
    stored.references = headers.references.clone();
    stored.summary.importance = headers.importance;
    stored.summary.has_attachments = !parsed.attachments.is_empty();
    stored.summary.preview = nuncio_core::text::trim_preview(&parsed.preview, 300);
    stored.summary.size = raw.len() as u64;
    stored.summary.thread_id = crate::util::thread_id(
        headers.message_id.as_deref(),
        headers.in_reply_to.as_deref(),
        &headers.references,
        &headers.subject,
        None,
    );
    stored.raw_headers = Some(headers.raw.clone());
    stored.body_path = Some(path_string.clone());
    stored.body_cached_at = Some(Utc::now());

    let attachments: Vec<Attachment> = parsed
        .attachments
        .iter()
        .enumerate()
        .map(|(index, attachment)| Attachment {
            id: attachment_id(&stored.summary.id, index, attachment.content_id.as_deref()),
            message_id: stored.summary.id.clone(),
            filename: attachment.filename.clone(),
            mime_type: attachment.mime_type.clone(),
            size: attachment.size,
            content_id: attachment.content_id.clone(),
            is_inline: attachment.is_inline,
            is_downloaded: false,
        })
        .collect();

    let message_id = stored.summary.id.clone();
    let text_for_fts = body_text;
    let preview = stored.summary.preview.clone();
    let folder_id = stored.summary.folder_id.clone();
    let mut parsed_stored = stored;
    ctx.db
        .write(move |db| {
            // `stored` was read before the download, which can take seconds. The
            // user may have flagged, read or moved the message meanwhile, so the
            // state that is not derived from the body is taken from the row as it
            // is now rather than written back from the old snapshot.
            if let Some(current) = repo::messages::get(db, &message_id)? {
                parsed_stored.summary.seen = current.summary.seen;
                parsed_stored.summary.flagged = current.summary.flagged;
                parsed_stored.summary.answered = current.summary.answered;
                parsed_stored.summary.forwarded = current.summary.forwarded;
                parsed_stored.summary.folder_id = current.summary.folder_id.clone();
                parsed_stored.summary.is_draft = current.summary.is_draft;
            }
            let account_id = parsed_stored.summary.account_id.clone();
            crate::sync::messages::resolve_thread_id(db, &account_id, &mut parsed_stored)?;
            repo::messages::upsert(db, &parsed_stored)?;
            crate::sync::messages::reconcile_threads(
                db,
                &account_id,
                &parsed_stored.summary.thread_id,
                parsed_stored.message_id_header.as_deref(),
            )?;
            repo::messages::set_body(db, &message_id, Some(&path_string), Some(&text_for_fts))?;
            repo::messages::update_preview(db, &message_id, &preview)?;
            repo::attachments::replace_for_message(db, &message_id, &attachments)?;
            repo::folders::recompute_counts(db, &folder_id)
        })
        .await?;
    Ok(())
}
