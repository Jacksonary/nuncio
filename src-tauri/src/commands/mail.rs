//! Mail commands: folders, messages, attachments, compose and rules.

use tauri::State;
use tracing::debug;

use nuncio_core::models::{
    Address, Attachment, Draft, DraftAttachment, Folder, Identity, MailRule, MessageAction,
    MessageDetail, MessageListQuery, MessagePage, MessageSummary, ReplyMode, UnifiedCounts,
};
use nuncio_core::{repo, AppError};

use crate::state::AppState;

/* ───────────────────────────── folders ───────────────────────────── */

#[tauri::command]
pub async fn folder_list(
    state: State<'_, AppState>,
    account_id: String,
) -> nuncio_core::Result<Vec<Folder>> {
    state.mail.folder_list(&account_id).await
}

#[tauri::command]
pub async fn folder_unified_counts(
    state: State<'_, AppState>,
) -> nuncio_core::Result<UnifiedCounts> {
    state.mail.folder_unified_counts().await
}

#[tauri::command]
pub async fn folder_create(
    state: State<'_, AppState>,
    account_id: String,
    parent_id: Option<String>,
    name: String,
) -> nuncio_core::Result<Folder> {
    state
        .mail
        .folder_create(&account_id, parent_id.as_deref(), &name)
        .await
}

#[tauri::command]
pub async fn folder_rename(
    state: State<'_, AppState>,
    id: String,
    name: String,
) -> nuncio_core::Result<Folder> {
    state.mail.folder_rename(&id, &name).await
}

#[tauri::command]
pub async fn folder_move(
    state: State<'_, AppState>,
    id: String,
    new_parent_id: Option<String>,
) -> nuncio_core::Result<Folder> {
    state.mail.folder_move(&id, new_parent_id.as_deref()).await
}

#[tauri::command]
pub async fn folder_delete(state: State<'_, AppState>, id: String) -> nuncio_core::Result<()> {
    state.mail.folder_delete(&id).await
}

#[tauri::command]
pub async fn folder_set_favorite(
    state: State<'_, AppState>,
    id: String,
    value: bool,
) -> nuncio_core::Result<()> {
    state.mail.folder_set_favorite(&id, value).await
}

#[tauri::command]
pub async fn folder_set_sync_enabled(
    state: State<'_, AppState>,
    id: String,
    value: bool,
) -> nuncio_core::Result<()> {
    state.mail.folder_set_sync_enabled(&id, value).await
}

#[tauri::command]
pub async fn folder_mark_all_read(
    state: State<'_, AppState>,
    id: String,
) -> nuncio_core::Result<()> {
    state.mail.folder_mark_all_read(&id).await
}

#[tauri::command]
pub async fn folder_empty(state: State<'_, AppState>, id: String) -> nuncio_core::Result<()> {
    state.mail.folder_empty(&id).await
}

#[tauri::command]
pub async fn folder_sync(state: State<'_, AppState>, id: String) -> nuncio_core::Result<()> {
    state.mail.folder_sync(&id).await
}

/* ───────────────────────────── messages ──────────────────────────── */

#[tauri::command]
pub async fn message_list(
    state: State<'_, AppState>,
    query: MessageListQuery,
) -> nuncio_core::Result<MessagePage> {
    state.mail.message_list(&query).await
}

#[tauri::command]
pub async fn message_thread(
    state: State<'_, AppState>,
    account_id: String,
    thread_id: String,
) -> nuncio_core::Result<Vec<MessageSummary>> {
    state.mail.message_thread(&account_id, &thread_id).await
}

#[tauri::command]
pub async fn message_get(
    state: State<'_, AppState>,
    id: String,
    allow_remote: bool,
) -> nuncio_core::Result<MessageDetail> {
    state.mail.message_get(&id, allow_remote).await
}

#[tauri::command]
pub async fn message_act(
    state: State<'_, AppState>,
    ids: Vec<String>,
    action: MessageAction,
) -> nuncio_core::Result<String> {
    state.mail.message_act(&ids, &action).await
}

#[tauri::command]
pub async fn message_act_threads(
    state: State<'_, AppState>,
    account_id: String,
    thread_ids: Vec<String>,
    action: MessageAction,
) -> nuncio_core::Result<String> {
    state
        .mail
        .message_act_threads(&account_id, &thread_ids, &action)
        .await
}

/// Undo by the id returned from `message_act`; without one, the most recent
/// action (keyboard shortcut path) is reverted.
#[tauri::command]
pub async fn message_undo_last(
    state: State<'_, AppState>,
    undo_id: Option<String>,
) -> nuncio_core::Result<bool> {
    match undo_id {
        Some(id) if !id.is_empty() => state.mail.message_undo(&id).await,
        _ => state.mail.message_undo_last().await,
    }
}

#[tauri::command]
pub async fn message_save_eml(
    state: State<'_, AppState>,
    id: String,
    path: String,
) -> nuncio_core::Result<()> {
    state.mail.message_save_eml(&id, &path).await
}

#[tauri::command]
pub async fn message_open_eml(
    state: State<'_, AppState>,
    path: String,
) -> nuncio_core::Result<MessageDetail> {
    state.mail.message_open_eml(&path).await
}

#[tauri::command]
pub async fn message_trust_sender(
    state: State<'_, AppState>,
    value: String,
) -> nuncio_core::Result<()> {
    state.mail.message_trust_sender(&value).await
}

#[tauri::command]
pub async fn message_unsubscribe(
    state: State<'_, AppState>,
    id: String,
) -> nuncio_core::Result<()> {
    state.mail.message_unsubscribe(&id).await
}

/* ─────────────────────────── attachments ─────────────────────────── */

#[tauri::command]
pub async fn attachment_open(state: State<'_, AppState>, id: String) -> nuncio_core::Result<()> {
    use tauri_plugin_opener::OpenerExt;
    let path = state.mail.attachment_open(&id).await?;
    state
        .app
        .opener()
        .open_path(path.to_string_lossy().into_owned(), None::<String>)
        .map_err(|error| AppError::io(format!("cannot open the attachment: {error}")))?;
    Ok(())
}

#[tauri::command]
pub async fn attachment_save(
    state: State<'_, AppState>,
    id: String,
    path: String,
) -> nuncio_core::Result<()> {
    state.mail.attachment_save(&id, &path).await
}

#[tauri::command]
pub async fn attachment_save_all(
    state: State<'_, AppState>,
    message_id: String,
    dir: String,
) -> nuncio_core::Result<()> {
    state.mail.attachment_save_all(&message_id, &dir).await?;
    Ok(())
}

#[tauri::command]
pub async fn attachment_is_dangerous(
    state: State<'_, AppState>,
    filename: String,
) -> nuncio_core::Result<bool> {
    Ok(state.mail.attachment_is_dangerous(&filename))
}

/// Used by the `cid:` protocol handler and internal helpers.
pub async fn attachment_by_id(
    state: &AppState,
    id: &str,
) -> nuncio_core::Result<Option<Attachment>> {
    let id = id.to_string();
    state
        .ctx
        .db
        .read(move |conn| repo::attachments::get(conn, &id))
        .await
}

/* ───────────────────────────── compose ───────────────────────────── */

#[tauri::command]
pub async fn compose_create(
    state: State<'_, AppState>,
    account_id: Option<String>,
) -> nuncio_core::Result<Draft> {
    state.mail.compose_create(account_id.as_deref()).await
}

/// Prefilled `mailto:` drafts are persisted so the compose window (which only
/// receives a draft id through the route) can load the content.
#[tauri::command]
pub async fn compose_from_mailto(
    state: State<'_, AppState>,
    url: String,
) -> nuncio_core::Result<Draft> {
    let draft = state.mail.compose_from_mailto(&url).await?;
    if draft.id.is_none() {
        state.mail.compose_save(&draft).await
    } else {
        Ok(draft)
    }
}

#[tauri::command]
pub async fn compose_reply(
    state: State<'_, AppState>,
    message_id: String,
    mode: ReplyMode,
) -> nuncio_core::Result<Draft> {
    state.mail.compose_reply(&message_id, mode).await
}

#[tauri::command]
pub async fn compose_open_draft(
    state: State<'_, AppState>,
    message_id: String,
) -> nuncio_core::Result<Draft> {
    state.mail.compose_open_draft(&message_id).await
}

#[tauri::command]
pub async fn compose_get_draft(
    state: State<'_, AppState>,
    draft_id: String,
) -> nuncio_core::Result<Draft> {
    state.mail.compose_get_draft(&draft_id).await
}

#[tauri::command]
pub async fn compose_save(state: State<'_, AppState>, draft: Draft) -> nuncio_core::Result<Draft> {
    state.mail.compose_save(&draft).await
}

#[tauri::command]
pub async fn compose_discard(
    state: State<'_, AppState>,
    draft_id: String,
) -> nuncio_core::Result<()> {
    state.mail.compose_discard(&draft_id).await
}

#[tauri::command]
pub async fn compose_send(state: State<'_, AppState>, draft: Draft) -> nuncio_core::Result<()> {
    state.mail.compose_send(&draft).await
}

#[tauri::command]
pub async fn compose_add_attachments(
    state: State<'_, AppState>,
    paths: Vec<String>,
) -> nuncio_core::Result<Vec<DraftAttachment>> {
    state.mail.compose_add_attachments(&paths).await
}

#[tauri::command]
pub async fn compose_add_inline_image(
    state: State<'_, AppState>,
    filename: String,
    mime_type: String,
    base64_data: String,
) -> nuncio_core::Result<DraftAttachment> {
    state
        .mail
        .compose_add_inline_image(&filename, &mime_type, &base64_data)
        .await
}

#[tauri::command]
pub async fn compose_suggest_recipients(
    state: State<'_, AppState>,
    query: String,
    limit: u32,
) -> nuncio_core::Result<Vec<Address>> {
    state.mail.compose_suggest_recipients(&query, limit).await
}

/// Acknowledge `app://flush-drafts`: the main window finished saving, so the
/// close-to-tray path may destroy the webview.
///
/// `seq` is the id that arrived with the `app://flush-drafts` event, echoed
/// back. It has to be the caller's own id and not "whatever is live now": a
/// composer that was still saving for an earlier close would otherwise release
/// a newer handshake that has not finished writing yet, and the window would be
/// destroyed mid-save. An ack for a handshake that has already been cancelled
/// matches nothing and is ignored.
#[tauri::command]
pub async fn compose_flush_done(state: State<'_, AppState>, seq: u64) -> nuncio_core::Result<()> {
    let ordering = std::sync::atomic::Ordering::SeqCst;
    if seq != 0 && state.flush_seq.load(ordering) == seq {
        state.flush_ack.store(seq, ordering);
    }
    Ok(())
}

/* ────────────────────────────── outbox ───────────────────────────── */

#[tauri::command]
pub async fn outbox_retry(
    state: State<'_, AppState>,
    outbox_id: Option<String>,
) -> nuncio_core::Result<()> {
    state.mail.outbox_retry(outbox_id.as_deref()).await
}

/// Stop sending and turn the item back into an editable draft.
#[tauri::command]
pub async fn outbox_cancel(
    state: State<'_, AppState>,
    outbox_id: String,
) -> nuncio_core::Result<Draft> {
    state.mail.outbox_cancel(&outbox_id).await
}

/* ────────────────────────────── rules ────────────────────────────── */

#[tauri::command]
pub async fn rule_list(state: State<'_, AppState>) -> nuncio_core::Result<Vec<MailRule>> {
    state.ctx.db.read(repo::rules::list).await
}

#[tauri::command]
pub async fn rule_upsert(
    state: State<'_, AppState>,
    rule: MailRule,
) -> nuncio_core::Result<MailRule> {
    state
        .ctx
        .db
        .write(move |conn| repo::rules::upsert(conn, &rule))
        .await
}

#[tauri::command]
pub async fn rule_delete(state: State<'_, AppState>, id: String) -> nuncio_core::Result<()> {
    state
        .ctx
        .db
        .write(move |conn| repo::rules::delete(conn, &id))
        .await
}

#[tauri::command]
pub async fn rule_run_now(
    state: State<'_, AppState>,
    id: String,
    folder_id: String,
) -> nuncio_core::Result<usize> {
    state.mail.rule_run_now(&id, &folder_id).await
}

/* ────────────────────────────── helpers ──────────────────────────── */

/// Resolve the sender's default identity for "send later" flows.
pub async fn ensure_default_identity(
    state: &AppState,
    account_id: &str,
) -> nuncio_core::Result<Identity> {
    let identity = state.mail.ensure_default_identity(account_id).await?;
    debug!(account = account_id, identity = %identity.id, "default identity ready");
    Ok(identity)
}
