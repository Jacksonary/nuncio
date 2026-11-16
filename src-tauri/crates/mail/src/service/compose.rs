//! Composer commands: new messages, replies/forwards, drafts (local + IMAP
//! Drafts folder), attachments, recipient suggestions and sending.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use nuncio_core::events::MailChangeKind;
use nuncio_core::models::{
    Account, Address, Draft, DraftAttachment, Folder, FolderRole, Identity, Importance, Language,
    MessageSummary, PendingOpState, ProviderKind, ReplyMode, Signature, StoredMessage,
};
use nuncio_core::{events, repo, AppError, Result};
use tracing::{debug, warn};

use crate::ops::{
    self, AppendDraftOp, DeleteRemoteDraftOp, SetFlagsOp, KIND_APPEND_DRAFT,
    KIND_DELETE_REMOTE_DRAFT, KIND_SET_FLAGS,
};

use super::MailService;

const MAX_INLINE_BYTES: usize = 25 * 1024 * 1024;
const MAX_DRAFT_BYTES: u64 = 100 * 1024 * 1024;
/// Remote path of the local Drafts folder used when the server has none.
const LOCAL_DRAFTS_PATH: &str = "local:drafts";

impl MailService {
    /* ───────────────────────── creation ───────────────────────── */

    /// New blank draft with the default identity and signature.
    pub async fn compose_create(&self, account_id: Option<&str>) -> Result<Draft> {
        let account = match account_id {
            Some(id) => self.account(id).await?,
            None => self.default_mail_account().await?,
        };
        let identity = self.identity_for(&account, &[]).await?;
        let signature = self
            .signature_for_identity(&account.id, identity.signature_id.as_deref(), false)
            .await?;
        let html = format!("<p><br></p>{}", signature_block(signature.as_ref()));
        Ok(Draft {
            id: None,
            account_id: account.id.clone(),
            identity_id: Some(identity.id.clone()),
            from: identity_address(&identity),
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: String::new(),
            html,
            attachments: Vec::new(),
            reply_mode: None,
            source_message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            importance: Importance::Normal,
            request_read_receipt: false,
            remote_draft_message_id: None,
            updated_at: None,
        })
    }

    /// New draft pre-filled from a `mailto:` URL.
    pub async fn compose_from_mailto(&self, url: &str) -> Result<Draft> {
        let data = nuncio_mime::parse_mailto(url)?;
        let mut draft = self.compose_create(None).await?;
        draft.to = data.to;
        draft.cc = data.cc;
        draft.bcc = data.bcc;
        draft.subject = data.subject;
        if !data.body.trim().is_empty() {
            let signature_tail = draft
                .html
                .strip_prefix("<p><br></p>")
                .unwrap_or_default()
                .to_string();
            draft.html = format!(
                "{}{}{}",
                nuncio_mime::plain_text_to_html(&data.body),
                "<p><br></p>",
                signature_tail
            );
        }
        Ok(draft)
    }

    /// Reply / reply-all / forward / forward-as-attachment / edit-as-new.
    pub async fn compose_reply(&self, message_id: &str, mode: ReplyMode) -> Result<Draft> {
        let mut stored = self.stored(message_id).await?;
        let raw = self.raw_message(&stored).await?;
        stored = self.stored(message_id).await?;
        let parsed = nuncio_mime::parse(&raw)?;
        let account = self.account(&stored.summary.account_id).await?;

        let mut preferred = parsed.headers.to.clone();
        preferred.extend(parsed.headers.cc.clone());
        preferred.extend(parsed.headers.bcc.clone());
        let identity = self.identity_for(&account, &preferred).await?;

        let mut me: Vec<String> = {
            let account_id = account.id.clone();
            self.inner
                .ctx
                .db
                .read(move |db| repo::identities::list(db, Some(&account_id)))
                .await?
                .into_iter()
                .map(|identity| identity.email)
                .collect()
        };
        if me.is_empty() {
            me.push(account.email.clone());
        }
        let settings = self.inner.ctx.settings.current();
        let lang = match settings.language {
            Language::EnUs => "en-US",
            _ => "zh-CN",
        };
        let quote = settings.mail.quote_original;
        let parts = nuncio_mime::build_reply(&parsed, mode, &me, lang, quote);

        let signature = self
            .signature_for_identity(
                &account.id,
                identity.signature_id.as_deref(),
                !matches!(mode, ReplyMode::EditAsNew),
            )
            .await?;
        let mut html = match mode {
            ReplyMode::EditAsNew => parts.html.clone(),
            _ => insert_signature(&parts.html, signature.as_ref()),
        };
        if html.trim().is_empty() {
            html = format!("<p><br></p>{}", signature_block(signature.as_ref()));
        }

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
        let mut attachments = Vec::new();
        let mut used_names: HashSet<String> = HashSet::new();

        if matches!(mode, ReplyMode::ForwardAsAttachment) {
            let filename = format!("{}.eml", safe_stem(&parsed.headers.subject));
            let dest = staging.join(&filename);
            tokio::fs::write(&dest, &raw).await.map_err(|error| {
                AppError::io(format!("cannot stage forwarded message: {error}"))
            })?;
            attachments.push(DraftAttachment {
                id: nuncio_core::models::new_id(),
                filename,
                mime_type: "message/rfc822".to_string(),
                size: raw.len() as u64,
                path: dest.to_string_lossy().to_string(),
                content_id: None,
            });
        } else {
            for attachment in &parts.attachments_to_forward {
                let bytes = match nuncio_mime::extract_part(&raw, &attachment.part) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        warn!(%error, filename = %attachment.filename, "could not stage forwarded attachment");
                        continue;
                    }
                };
                let filename = unique_name(&staging, &attachment.filename, &mut used_names).await;
                let dest = staging.join(&filename);
                tokio::fs::write(&dest, &bytes).await.map_err(|error| {
                    AppError::io(format!("cannot stage forwarded attachment: {error}"))
                })?;
                let keep_inline = quote || matches!(mode, ReplyMode::EditAsNew);
                let content_id = if attachment.is_inline && keep_inline {
                    Some(
                        attachment
                            .content_id
                            .clone()
                            .unwrap_or_else(|| nuncio_mime::attachments::content_id_for(&filename)),
                    )
                } else {
                    None
                };
                attachments.push(DraftAttachment {
                    id: nuncio_core::models::new_id(),
                    filename,
                    mime_type: attachment.mime_type.clone(),
                    size: bytes.len() as u64,
                    path: dest.to_string_lossy().to_string(),
                    content_id,
                });
            }
        }

        Ok(Draft {
            id: None,
            account_id: account.id.clone(),
            identity_id: Some(identity.id.clone()),
            from: identity_address(&identity),
            to: parts.to,
            cc: parts.cc,
            bcc: Vec::new(),
            subject: parts.subject,
            html,
            attachments,
            reply_mode: Some(mode),
            source_message_id: Some(message_id.to_string()),
            in_reply_to: parts.in_reply_to,
            references: parts.references,
            importance: parsed.headers.importance,
            request_read_receipt: false,
            remote_draft_message_id: None,
            updated_at: None,
        })
    }

    /// Open a message from the Drafts folder as an editable draft.
    pub async fn compose_open_draft(&self, message_id: &str) -> Result<Draft> {
        let mut stored = self.stored(message_id).await?;
        let raw = self.raw_message(&stored).await?;
        stored = self.stored(message_id).await?;
        let parsed = nuncio_mime::parse(&raw)?;
        let account = self.account(&stored.summary.account_id).await?;
        let preferred = parsed.headers.from.iter().cloned().collect::<Vec<_>>();
        let identity = self.identity_for(&account, &preferred).await?;

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
        let mut used_names: HashSet<String> = HashSet::new();
        let mut attachments = Vec::new();
        for attachment in &parsed.attachments {
            let bytes = match nuncio_mime::extract_part(&raw, &attachment.part) {
                Ok(bytes) => bytes,
                Err(error) => {
                    warn!(%error, filename = %attachment.filename, "could not stage draft attachment");
                    continue;
                }
            };
            let filename = unique_name(&staging, &attachment.filename, &mut used_names).await;
            let dest = staging.join(&filename);
            tokio::fs::write(&dest, &bytes)
                .await
                .map_err(|error| AppError::io(format!("cannot stage draft attachment: {error}")))?;
            attachments.push(DraftAttachment {
                id: nuncio_core::models::new_id(),
                filename,
                mime_type: attachment.mime_type.clone(),
                size: bytes.len() as u64,
                path: dest.to_string_lossy().to_string(),
                content_id: if attachment.is_inline {
                    attachment.content_id.clone()
                } else {
                    None
                },
            });
        }
        let html = parsed
            .html
            .clone()
            .or_else(|| parsed.text.as_deref().map(nuncio_mime::plain_text_to_html))
            .unwrap_or_else(|| "<p><br></p>".to_string());
        // A row projected by `compose_save` carries its local draft id in the
        // Message-ID, so reopening edits the same draft instead of cloning it.
        let local_draft_id = stored
            .message_id_header
            .as_deref()
            .and_then(crate::util::draft_id_from_header)
            .map(str::to_string);

        Ok(Draft {
            id: local_draft_id,
            account_id: account.id.clone(),
            identity_id: Some(identity.id.clone()),
            from: identity_address(&identity),
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
            remote_draft_message_id: Some(message_id.to_string()),
            updated_at: None,
        })
    }

    pub async fn compose_get_draft(&self, draft_id: &str) -> Result<Draft> {
        let id = draft_id.to_string();
        self.inner
            .ctx
            .db
            .read(move |db| repo::drafts::get_required(db, &id))
            .await
    }

    /* ───────────────────────── saving ───────────────────────── */

    /// Persist the draft locally and queue a debounced IMAP Drafts APPEND.
    pub async fn compose_save(&self, draft: &Draft) -> Result<Draft> {
        let account = self.account(&draft.account_id).await?;
        let draft_id = draft
            .id
            .clone()
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(nuncio_core::models::new_id);
        let mut draft = draft.clone();
        draft.id = Some(draft_id.clone());

        // Move staged files into the draft directory so they survive rebuilds.
        let dir = self.inner.ctx.paths.draft_staging_dir(&draft_id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|error| AppError::io(format!("cannot create draft dir: {error}")))?;
        let mut used_names: HashSet<String> = HashSet::new();
        let mut attachments = Vec::new();
        for attachment in &draft.attachments {
            let source = PathBuf::from(&attachment.path);
            let metadata = tokio::fs::metadata(&source).await.map_err(|error| {
                AppError::io(format!(
                    "draft attachment {} is missing: {error}",
                    attachment.filename
                ))
            })?;
            if !metadata.is_file() {
                return Err(AppError::invalid_input(format!(
                    "draft attachment {} is not a file",
                    attachment.filename
                )));
            }
            let already_staged = source.parent() == Some(dir.as_path())
                && !used_names.contains(&attachment.filename.to_lowercase())
                && tokio::fs::metadata(&source).await.is_ok();
            let filename = if already_staged {
                used_names.insert(attachment.filename.to_lowercase());
                attachment.filename.clone()
            } else {
                unique_name(&dir, &attachment.filename, &mut used_names).await
            };
            let dest = dir.join(&filename);
            if source != dest {
                tokio::fs::copy(&source, &dest)
                    .await
                    .map_err(|error| AppError::io(format!("cannot stage draft file: {error}")))?;
            }
            attachments.push(DraftAttachment {
                id: if attachment.id.trim().is_empty() {
                    nuncio_core::models::new_id()
                } else {
                    attachment.id.clone()
                },
                filename,
                mime_type: if attachment.mime_type.trim().is_empty() {
                    mime_guess::from_path(&dest)
                        .first_or_octet_stream()
                        .to_string()
                } else {
                    attachment.mime_type.clone()
                },
                size: metadata.len(),
                path: dest.to_string_lossy().to_string(),
                content_id: attachment.content_id.clone(),
            });
        }
        draft.attachments = attachments;

        // Build the RFC822 message with a deterministic Message-ID so an
        // uploaded draft can be matched back to this local row.
        let identity = self
            .identity_for(&account, std::slice::from_ref(&draft.from))
            .await?;
        let header_id = draft_message_id(&draft_id);
        let outgoing = self.outgoing_from_draft(&draft, &identity, Some(header_id.clone()))?;
        // Drafts keep the Bcc header locally and in the server Drafts copy so
        // reopening a draft still shows every recipient.
        let (bytes, _) = nuncio_mime::build_message_with_bcc(outgoing)?;
        if bytes.len() as u64 > MAX_DRAFT_BYTES {
            return Err(AppError::invalid_input("draft exceeds 100 MB"));
        }
        let temp = dir.join("draft.eml.tmp");
        let final_path = dir.join("draft.eml");
        tokio::fs::write(&temp, &bytes)
            .await
            .map_err(|error| AppError::io(format!("cannot write draft message: {error}")))?;
        tokio::fs::rename(&temp, &final_path)
            .await
            .map_err(|error| AppError::io(format!("cannot finalize draft message: {error}")))?;

        // Find an old remote copy (previous autosave) that must be removed.
        let drafts_folder = self.draft_folder(&account).await?;
        let server_drafts_folder = drafts_folder.as_ref().filter(|folder| {
            !folder
                .remote_path
                .starts_with(crate::sync::folders::LOCAL_FOLDER_PREFIX)
        });
        let mut remote_uid: Option<(String, i64)> = None;
        let mut remote_local_id: Option<String> = None;
        if let Some(folder) = server_drafts_folder {
            let old = self
                .find_remote_draft(&account, folder, &draft, &header_id)
                .await?;
            if let Some(old) = old {
                if let Some(uid) = old.uid {
                    remote_uid = Some((old.summary.folder_id.clone(), uid));
                }
                remote_local_id = Some(old.summary.id.clone());
            }
        }
        let pending_append = self
            .pending_draft_operation(&account.id, KIND_APPEND_DRAFT, &draft_id)
            .await?;
        if remote_local_id.is_some() || remote_uid.is_some() {
            // The new APPEND replaces the old remote draft.
            draft.remote_draft_message_id = None;
        }

        let saved = self
            .inner
            .ctx
            .db
            .write(move |db| repo::drafts::save(db, &draft))
            .await?;

        if account.imap.is_some() {
            let mut deleted_folder: Option<String> = None;
            if let Some((folder_id, uid)) = remote_uid {
                let pending_delete = self
                    .pending_delete_remote_draft(&account.id, &folder_id, uid)
                    .await?;
                if !pending_delete {
                    let payload = DeleteRemoteDraftOp {
                        folder_id: folder_id.clone(),
                        uid,
                    };
                    ops::enqueue_operation(
                        &self.inner.ctx,
                        &account.id,
                        KIND_DELETE_REMOTE_DRAFT,
                        serde_json::to_value(payload).map_err(|error| {
                            AppError::internal(format!("cannot encode deleteRemoteDraft: {error}"))
                        })?,
                        "Remove old remote draft",
                    )
                    .await?;
                }
                deleted_folder = Some(folder_id);
            }
            if let Some(local_id) = remote_local_id.take() {
                // For Graph the queued deleteRemoteDraft op resolves the
                // server id from this row (the payload only carries the
                // synthetic UID), so it must survive until the op runs.  The
                // next delta sync removes it once the server copy is gone.
                if !crate::graph::uses_graph(&account) {
                    let id_for_db = local_id.clone();
                    self.inner
                        .ctx
                        .db
                        .write(move |db| {
                            repo::messages::delete_local(db, &[id_for_db], true)?;
                            Ok(())
                        })
                        .await?;
                    events::emit_mail_changed(
                        self.inner.ctx.events.as_ref(),
                        account.id.clone(),
                        deleted_folder.into_iter().collect(),
                        vec![local_id],
                        MailChangeKind::Deleted,
                    );
                }
            }
            if pending_append.is_none() {
                if let Some(folder) = server_drafts_folder {
                    let payload = AppendDraftOp {
                        draft_id: draft_id.clone(),
                        folder_id: folder.id.clone(),
                        rfc822_path: final_path.to_string_lossy().to_string(),
                    };
                    ops::enqueue_operation(
                        &self.inner.ctx,
                        &account.id,
                        KIND_APPEND_DRAFT,
                        serde_json::to_value(payload).map_err(|error| {
                            AppError::internal(format!("cannot encode appendDraft: {error}"))
                        })?,
                        "Save draft to server",
                    )
                    .await?;
                    events::emit_folders_changed(
                        self.inner.ctx.events.as_ref(),
                        account.id.clone(),
                        vec![folder.id.clone()],
                    );
                }
            }
        }

        // Local Drafts projection: the composer stays usable offline and the
        // row becomes the synced APPEND copy once the server echoes it back.
        if let Some(folder) = drafts_folder.as_ref() {
            if let Err(error) = self
                .store_draft_message(
                    &account,
                    &saved,
                    folder,
                    &header_id,
                    final_path.to_string_lossy().as_ref(),
                    bytes.len() as u64,
                )
                .await
            {
                warn!(%error, draft = %draft_id, "could not project the draft into Drafts");
            }
        }
        debug!(draft = %draft_id, bytes = bytes.len(), "saved draft");
        Ok(saved)
    }

    /// Delete a draft locally, its staged files and its remote copy.
    pub async fn compose_discard(&self, draft_id: &str) -> Result<()> {
        let draft = self.compose_get_draft(draft_id).await?;
        let account = self.account(&draft.account_id).await?;
        self.cancel_pending_draft_appends(&draft.account_id, draft_id)
            .await?;
        let mut folders: Vec<String> = Vec::new();
        if let Some(message_id) = draft.remote_draft_message_id.as_deref() {
            if let Ok(stored) = self.stored(message_id).await {
                if stored.summary.account_id == draft.account_id {
                    if let Some(uid) = stored.uid {
                        let payload = DeleteRemoteDraftOp {
                            folder_id: stored.summary.folder_id.clone(),
                            uid,
                        };
                        if let Err(error) = ops::enqueue_operation(
                            &self.inner.ctx,
                            &draft.account_id,
                            KIND_DELETE_REMOTE_DRAFT,
                            serde_json::to_value(payload).map_err(|error| {
                                AppError::internal(format!(
                                    "cannot encode deleteRemoteDraft: {error}"
                                ))
                            })?,
                            "Remove remote draft",
                        )
                        .await
                        {
                            debug!(%error, "could not queue remote draft deletion");
                        }
                    }
                    folders.push(stored.summary.folder_id.clone());
                    // Keep the row for Graph until the queued delete can
                    // resolve its server id (see compose_save); the delta sync
                    // removes it afterwards.
                    if !crate::graph::uses_graph(&account) {
                        let id = stored.summary.id.clone();
                        let _ = self
                            .inner
                            .ctx
                            .db
                            .write(move |db| repo::messages::delete_local(db, &[id], true))
                            .await;
                    }
                }
            }
        }
        let id = draft_id.to_string();
        self.inner
            .ctx
            .db
            .write(move |db| repo::drafts::delete(db, &id))
            .await?;
        match self.remove_draft_message(draft_id).await {
            Ok(Some(folder_id)) => folders.push(folder_id),
            Ok(None) => {}
            Err(error) => {
                debug!(%error, draft = %draft_id, "could not remove the draft projection")
            }
        }
        if let Some(folder_id) = self.drafts_folder_id(&draft.account_id).await? {
            folders.push(folder_id);
        }
        let dir = self.inner.ctx.paths.draft_staging_dir(draft_id);
        let _ = tokio::fs::remove_dir_all(dir).await;
        events::emit_mail_changed(
            self.inner.ctx.events.as_ref(),
            draft.account_id.clone(),
            folders,
            Vec::new(),
            MailChangeKind::Updated,
        );
        self.update_badge().await;
        Ok(())
    }

    /// Validate, build MIME, queue to the outbox and remove the draft.
    pub async fn compose_send(&self, draft: &Draft) -> Result<()> {
        let account = self.account(&draft.account_id).await?;
        // Clear early dispatch: Microsoft accounts send through Graph, which
        // also files the Sent copy, so the SMTP/outbox path is skipped.
        if crate::graph::uses_graph(&account) {
            return self.compose_send_graph(&account, draft).await;
        }
        let recipients = all_recipients(draft)?;
        if account.smtp.is_none() {
            return Err(AppError::unsupported(
                "this account has no outgoing (SMTP) server configured",
            ));
        }
        let identity = self
            .identity_for(&account, std::slice::from_ref(&draft.from))
            .await?;
        let outgoing = self.outgoing_from_draft(draft, &identity, None)?;
        let (bytes, _) = nuncio_mime::build_message(outgoing)?;
        if bytes.len() as u64 > MAX_DRAFT_BYTES {
            return Err(AppError::invalid_input("message exceeds 100 MB"));
        }

        let draft_id = draft.id.clone();
        if let Some(draft_id) = draft_id.as_deref() {
            self.cancel_pending_draft_appends(&account.id, draft_id)
                .await?;
        }
        // Remove the remote draft copy that this send supersedes.
        self.queue_remote_draft_deletion(&account, draft).await;
        let sent_now = self
            .queue_outbox(
                &account,
                draft_id.clone(),
                draft.subject.clone(),
                recipients,
                bytes,
            )
            .await?;
        // The outbox applies source flags only when it can still read the
        // draft row; cover the offline and unsaved-draft cases ourselves.
        let handled_by_outbox = sent_now && draft_id.is_some();
        if !handled_by_outbox {
            if let Err(error) = self.apply_source_flags(&account, draft).await {
                warn!(%error, "could not mark the source message as replied/forwarded");
            }
        }

        // `queue_outbox` above is the point of no return: the message is durable
        // and the sender will be driven from the outbox. Everything below is
        // cleanup, and a failure here must NOT surface as "the send failed" —
        // the client would re-submit a message that is already queued, and the
        // draft it recreates would be a second copy of mail that is on its way.
        let mut affected_folders: Vec<String> = Vec::new();
        if let Some(draft_id) = draft_id {
            let id = draft_id.clone();
            if let Err(error) = self
                .inner
                .ctx
                .db
                .write(move |db| repo::drafts::delete(db, &id))
                .await
            {
                warn!(%error, draft = %draft_id, "could not delete the sent draft");
            }
            match self.remove_draft_message(&draft_id).await {
                Ok(Some(folder_id)) => affected_folders.push(folder_id),
                Ok(None) => {}
                Err(error) => {
                    debug!(%error, draft = %draft_id, "could not remove the draft projection")
                }
            }
            let dir = self.inner.ctx.paths.draft_staging_dir(&draft_id);
            let _ = tokio::fs::remove_dir_all(dir).await;
        }
        events::emit_mail_changed(
            self.inner.ctx.events.as_ref(),
            account.id.clone(),
            affected_folders,
            Vec::new(),
            MailChangeKind::Updated,
        );
        self.update_badge().await;
        Ok(())
    }

    /// Send a draft through Microsoft Graph.
    ///
    /// Graph's `sendMail` accepts a base64 MIME payload and reads the
    /// recipients from its headers, so the staged copy must keep the Bcc
    /// header (`build_message_with_bcc`) -- unlike SMTP there is no envelope
    /// to carry Bcc separately.  Graph files the Sent copy itself, so no
    /// `appendToSent` operation is queued.  A failed send leaves the draft and
    /// its local projection intact so the user can retry.
    async fn compose_send_graph(&self, account: &Account, draft: &Draft) -> Result<()> {
        all_recipients(draft)?;
        let identity = self
            .identity_for(account, std::slice::from_ref(&draft.from))
            .await?;
        let outgoing = self.outgoing_from_draft(draft, &identity, None)?;
        let (bytes, _) = nuncio_mime::build_message_with_bcc(outgoing)?;
        if bytes.len() as u64 > MAX_DRAFT_BYTES {
            return Err(AppError::invalid_input("message exceeds 100 MB"));
        }

        let draft_id = draft.id.clone();
        if let Some(draft_id) = draft_id.as_deref() {
            self.cancel_pending_draft_appends(&account.id, draft_id)
                .await?;
        }

        crate::graph::ops::send_raw(
            &crate::graph::GraphMailClient::new(),
            &self.inner.ctx,
            &account.id,
            &bytes,
        )
        .await?;

        // The message is sent; everything below is best-effort cleanup and must
        // not turn a delivered message into a reported failure.
        self.queue_graph_remote_draft_removal(account, draft).await;
        if let Err(error) = self.apply_source_flags(account, draft).await {
            warn!(%error, "could not mark the source message as replied/forwarded");
        }

        let mut affected_folders: Vec<String> = Vec::new();
        if let Some(draft_id) = draft_id {
            let id = draft_id.clone();
            if let Err(error) = self
                .inner
                .ctx
                .db
                .write(move |db| repo::drafts::delete(db, &id))
                .await
            {
                warn!(%error, draft = %draft_id, "could not delete the sent draft");
            }
            match self.remove_draft_message(&draft_id).await {
                Ok(Some(folder_id)) => affected_folders.push(folder_id),
                Ok(None) => {}
                Err(error) => {
                    debug!(%error, draft = %draft_id, "could not remove the draft projection")
                }
            }
            let dir = self.inner.ctx.paths.draft_staging_dir(&draft_id);
            let _ = tokio::fs::remove_dir_all(dir).await;
        }
        events::emit_mail_changed(
            self.inner.ctx.events.as_ref(),
            account.id.clone(),
            affected_folders,
            Vec::new(),
            MailChangeKind::Updated,
        );
        self.update_badge().await;
        Ok(())
    }

    /// Queue deletion of the server copy of a draft, keeping the local row.
    ///
    /// The queued payload only carries the synthetic UID; the Graph executor
    /// resolves the real message id from the row.  Deleting the row here (as
    /// the IMAP path does) would make the operation unresolvable, so it is
    /// left for the next delta sync to remove.
    async fn queue_graph_remote_draft_removal(&self, account: &Account, draft: &Draft) {
        let mut remote: Option<StoredMessage> = None;
        if let Some(message_id) = draft.remote_draft_message_id.as_deref() {
            if let Ok(stored) = self.stored(message_id).await {
                if stored.summary.account_id == account.id {
                    remote = Some(stored);
                }
            }
        }
        if remote.is_none() {
            if let (Some(draft_id), Some(folder)) = (
                draft.id.as_deref(),
                self.folder_by_role(&account.id, FolderRole::Drafts)
                    .await
                    .ok()
                    .flatten(),
            ) {
                let header = draft_message_id(draft_id);
                remote = self
                    .find_remote_draft(account, &folder, draft, &header)
                    .await
                    .ok()
                    .flatten();
            }
        }
        let Some(stored) = remote else { return };
        let Some(uid) = stored.uid else { return };
        let payload = DeleteRemoteDraftOp {
            folder_id: stored.summary.folder_id.clone(),
            uid,
        };
        let Ok(value) = serde_json::to_value(payload) else {
            return;
        };
        if let Err(error) = ops::enqueue_operation(
            &self.inner.ctx,
            &account.id,
            KIND_DELETE_REMOTE_DRAFT,
            value,
            "Remove remote draft",
        )
        .await
        {
            debug!(%error, "could not queue remote draft deletion");
        }
    }

    /* ───────────────────── attachments / suggestions ───────────────────── */

    /// Copy files into the compose staging area.
    pub async fn compose_add_attachments(&self, paths: &[String]) -> Result<Vec<DraftAttachment>> {
        let dir = self
            .inner
            .ctx
            .paths
            .staging_dir()
            .join("compose")
            .join(nuncio_core::models::new_id());
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|error| AppError::io(format!("cannot create staging dir: {error}")))?;
        let mut used_names: HashSet<String> = HashSet::new();
        let mut result = Vec::new();
        for path in paths {
            let source = PathBuf::from(path);
            let metadata = tokio::fs::metadata(&source)
                .await
                .map_err(|error| AppError::io(format!("cannot read attachment {path}: {error}")))?;
            if !metadata.is_file() {
                return Err(AppError::invalid_input(format!("{path} is not a file")));
            }
            let filename = source
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("attachment")
                .to_string();
            let filename = unique_name(&dir, &filename, &mut used_names).await;
            let dest = dir.join(&filename);
            tokio::fs::copy(&source, &dest)
                .await
                .map_err(|error| AppError::io(format!("cannot stage attachment: {error}")))?;
            let mime_type = mime_guess::from_path(&dest)
                .first_or_octet_stream()
                .to_string();
            result.push(DraftAttachment {
                id: nuncio_core::models::new_id(),
                filename: filename.clone(),
                mime_type,
                size: metadata.len(),
                path: dest.to_string_lossy().to_string(),
                content_id: None,
            });
        }
        Ok(result)
    }

    /// Stage a pasted image and give it a `cid:` content id.
    pub async fn compose_add_inline_image(
        &self,
        filename: &str,
        mime_type: &str,
        base64_data: &str,
    ) -> Result<DraftAttachment> {
        let payload = base64_data
            .split_once(',')
            .filter(|(prefix, _)| prefix.starts_with("data:"))
            .map(|(_, data)| data)
            .unwrap_or(base64_data)
            .trim();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
            .map_err(|error| AppError::invalid_input(format!("invalid base64 image: {error}")))?;
        if bytes.len() > MAX_INLINE_BYTES {
            return Err(AppError::invalid_input("inline image exceeds 25 MB"));
        }
        let dir = self
            .inner
            .ctx
            .paths
            .staging_dir()
            .join("compose")
            .join(nuncio_core::models::new_id());
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|error| AppError::io(format!("cannot create staging dir: {error}")))?;
        let mut used_names = HashSet::new();
        let filename = unique_name(&dir, filename, &mut used_names).await;
        let dest = dir.join(&filename);
        tokio::fs::write(&dest, &bytes)
            .await
            .map_err(|error| AppError::io(format!("cannot stage inline image: {error}")))?;
        let mime_type = if mime_type.trim().is_empty() {
            mime_guess::from_path(&dest)
                .first_or_octet_stream()
                .to_string()
        } else {
            mime_type.to_string()
        };
        let content_id = nuncio_mime::attachments::content_id_for(&filename);
        Ok(DraftAttachment {
            id: nuncio_core::models::new_id(),
            filename,
            mime_type,
            size: bytes.len() as u64,
            path: dest.to_string_lossy().to_string(),
            content_id: Some(content_id),
        })
    }

    /// Recipient autocomplete: contacts first, then recently used addresses.
    pub async fn compose_suggest_recipients(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Vec<Address>> {
        let limit = limit.clamp(1, 50);
        let query_owned = query.trim().to_string();
        let contacts = self
            .inner
            .ctx
            .db
            .read(move |db| {
                let summaries = repo::contacts::search(db, &query_owned, limit)?;
                let mut addresses = Vec::new();
                for summary in summaries {
                    if let Some(contact) = repo::contacts::get(db, &summary.id)? {
                        if let Some(address) = repo::contacts::address_for(&contact) {
                            addresses.push(address);
                        }
                    }
                }
                Ok(addresses)
            })
            .await?;
        let query_owned = query.trim().to_string();
        let history = self
            .inner
            .ctx
            .db
            .read(move |db| repo::recipient_history::suggest(db, &query_owned, limit))
            .await?;
        let mut seen: HashSet<String> = HashSet::new();
        let mut result = Vec::new();
        for address in contacts.into_iter().chain(history) {
            let key = nuncio_core::text::normalize_email(&address.email);
            if key.is_empty() || !seen.insert(key) {
                continue;
            }
            result.push(address);
            if result.len() >= limit as usize {
                break;
            }
        }
        Ok(result)
    }

    /* ───────────────────────── helpers ───────────────────────── */

    async fn default_mail_account(&self) -> Result<nuncio_core::models::Account> {
        let accounts = self
            .inner
            .ctx
            .db
            .read(repo::accounts::list_mail_accounts)
            .await?;
        accounts
            .into_iter()
            .find(|account| {
                account.mail_enabled && !matches!(account.provider, ProviderKind::Local)
            })
            .ok_or_else(|| AppError::not_found("no mail account is configured"))
    }

    pub(crate) async fn identity_for(
        &self,
        account: &nuncio_core::models::Account,
        preferred: &[Address],
    ) -> Result<Identity> {
        let account_id = account.id.clone();
        let mut identities = self
            .inner
            .ctx
            .db
            .read(move |db| repo::identities::list(db, Some(&account_id)))
            .await?;
        if identities.is_empty() {
            return self.ensure_default_identity(&account.id).await;
        }
        let preferred_keys: Vec<String> = preferred
            .iter()
            .map(|address| nuncio_core::text::normalize_email(&address.email))
            .filter(|email| !email.is_empty())
            .collect();
        if let Some(identity) = identities.iter().find(|identity| {
            preferred_keys.contains(&nuncio_core::text::normalize_email(&identity.email))
        }) {
            return Ok(identity.clone());
        }
        if let Some(identity) = identities.iter().find(|identity| identity.is_default) {
            return Ok(identity.clone());
        }
        Ok(identities.remove(0))
    }

    /// Signature configured on the selected identity, falling back to the
    /// account/global `useForNew` / `useForReply` default.
    async fn signature_for_identity(
        &self,
        account_id: &str,
        signature_id: Option<&str>,
        for_reply: bool,
    ) -> Result<Option<Signature>> {
        if let Some(signature_id) = signature_id.filter(|value| !value.trim().is_empty()) {
            let id = signature_id.to_string();
            let account_id_owned = account_id.to_string();
            let configured = self
                .inner
                .ctx
                .db
                .read(move |db| repo::signatures::get(db, &id))
                .await?;
            if let Some(signature) = configured {
                let usable = signature
                    .account_id
                    .as_deref()
                    .map(|owner| owner == account_id_owned)
                    .unwrap_or(true);
                if usable {
                    return Ok(Some(signature));
                }
            }
        }
        self.signature_for(account_id, for_reply).await
    }

    /// Signature selected for the account (kept for the fallback path).
    async fn signature_for(&self, account_id: &str, for_reply: bool) -> Result<Option<Signature>> {
        let account_id = account_id.to_string();
        self.inner
            .ctx
            .db
            .read(move |db| {
                if for_reply {
                    repo::signatures::default_for_reply(db, &account_id)
                } else {
                    repo::signatures::default_for_new(db, &account_id)
                }
            })
            .await
    }

    /// Translate a `Draft` into [`nuncio_mime::OutgoingMessage`].
    fn outgoing_from_draft(
        &self,
        draft: &Draft,
        identity: &Identity,
        message_id: Option<String>,
    ) -> Result<nuncio_mime::OutgoingMessage> {
        let mut message = nuncio_mime::OutgoingMessage::new(
            Address {
                name: if draft.from.name.is_some() {
                    draft.from.name.clone()
                } else if identity.name.trim().is_empty() {
                    None
                } else {
                    Some(identity.name.clone())
                },
                email: if draft.from.email.trim().is_empty() {
                    identity.email.clone()
                } else {
                    draft.from.email.clone()
                },
            },
            draft.subject.clone(),
            draft.html.clone(),
        );
        if draft.from.email.trim().is_empty() && identity.email.trim().is_empty() {
            return Err(AppError::invalid_input("draft has no From address"));
        }
        message.to = draft.to.clone();
        message.cc = draft.cc.clone();
        message.bcc = draft.bcc.clone();
        if let Some(reply_to) = identity
            .reply_to
            .as_deref()
            .and_then(nuncio_mime::parse_one_address)
        {
            message.reply_to = vec![reply_to];
        }
        message.in_reply_to = draft.in_reply_to.clone();
        message.references = draft.references.clone();
        message.importance = draft.importance;
        message.request_read_receipt = draft.request_read_receipt;
        message.message_id = message_id;
        message.date = Some(nuncio_core::models::now());
        for attachment in &draft.attachments {
            let path = PathBuf::from(&attachment.path);
            let mime_type = if attachment.mime_type.trim().is_empty() {
                mime_guess::from_path(&path)
                    .first_or_octet_stream()
                    .to_string()
            } else {
                attachment.mime_type.clone()
            };
            match attachment.content_id.as_deref() {
                Some(content_id) if !content_id.trim().is_empty() => {
                    message
                        .inline_images
                        .push((content_id.to_string(), mime_type, path));
                }
                _ => {
                    message
                        .attachments
                        .push((attachment.filename.clone(), mime_type, path));
                }
            }
        }
        Ok(message)
    }

    /// Queue deletion of the server copy of a draft (best effort).
    async fn queue_remote_draft_deletion(
        &self,
        account: &nuncio_core::models::Account,
        draft: &Draft,
    ) {
        let mut remote: Option<StoredMessage> = None;
        if let Some(message_id) = draft.remote_draft_message_id.as_deref() {
            if let Ok(stored) = self.stored(message_id).await {
                if stored.summary.account_id == account.id {
                    remote = Some(stored);
                }
            }
        }
        if remote.is_none() {
            if let (Some(draft_id), Some(folder)) = (
                draft.id.as_deref(),
                self.folder_by_role(&account.id, FolderRole::Drafts)
                    .await
                    .ok()
                    .flatten(),
            ) {
                let header = draft_message_id(draft_id);
                remote = self
                    .find_remote_draft(account, &folder, draft, &header)
                    .await
                    .ok()
                    .flatten();
            }
        }
        let Some(stored) = remote else { return };
        let Some(uid) = stored.uid else { return };
        let payload = DeleteRemoteDraftOp {
            folder_id: stored.summary.folder_id.clone(),
            uid,
        };
        let Ok(value) = serde_json::to_value(payload) else {
            return;
        };
        if ops::enqueue_operation(
            &self.inner.ctx,
            &account.id,
            KIND_DELETE_REMOTE_DRAFT,
            value,
            "Remove remote draft",
        )
        .await
        .is_ok()
        {
            let folder_id = stored.summary.folder_id.clone();
            let message_id = stored.summary.id.clone();
            let id_for_db = message_id.clone();
            let _ = self
                .inner
                .ctx
                .db
                .write(move |db| {
                    repo::messages::delete_local(db, &[id_for_db], true)?;
                    Ok(())
                })
                .await;
            events::emit_mail_changed(
                self.inner.ctx.events.as_ref(),
                account.id.clone(),
                vec![folder_id],
                vec![message_id],
                MailChangeKind::Deleted,
            );
        }
    }

    /// Mark the original message as answered/forwarded (local + queued).
    pub(crate) async fn apply_source_flags(
        &self,
        account: &nuncio_core::models::Account,
        draft: &Draft,
    ) -> Result<()> {
        let Some(source_id) = draft.source_message_id.clone() else {
            return Ok(());
        };
        let stored = match self.stored(&source_id).await {
            Ok(stored) => stored,
            Err(error) if error.is_not_found() => return Ok(()),
            Err(error) => return Err(error),
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
        let id = stored.summary.id.clone();
        self.inner
            .ctx
            .db
            .write(move |db| {
                repo::messages::set_flags(
                    db,
                    &[id],
                    None,
                    None,
                    answered.then_some(true),
                    forwarded.then_some(true),
                )?;
                Ok(())
            })
            .await?;
        if is_local_only_account(account.provider) {
            return Ok(());
        }
        if let Some(uid) = stored.uid {
            let payload = SetFlagsOp {
                folder_id: stored.summary.folder_id.clone(),
                uids: vec![uid],
                seen: None,
                flagged: None,
                answered: answered.then_some(true),
                forwarded: forwarded.then_some(true),
                previous: vec![crate::ops::FlagRollback {
                    uid,
                    seen: stored.summary.seen,
                    flagged: stored.summary.flagged,
                    answered: stored.summary.answered,
                    forwarded: stored.summary.forwarded,
                }],
            };
            ops::enqueue_operation(
                &self.inner.ctx,
                &account.id,
                KIND_SET_FLAGS,
                serde_json::to_value(payload).map_err(|error| {
                    AppError::internal(format!("cannot encode setFlags: {error}"))
                })?,
                "Mark original as replied/forwarded",
            )
            .await?;
        }
        Ok(())
    }

    /// Drafts folder used by the composer: the server folder when it exists,
    /// otherwise a local folder so POP3/local/IMAP-without-Drafts accounts can
    /// still list and reopen closed drafts offline.
    async fn draft_folder(&self, account: &nuncio_core::models::Account) -> Result<Option<Folder>> {
        let account_id = account.id.clone();
        let folders = self
            .inner
            .ctx
            .db
            .read({
                let account_id = account_id.clone();
                move |db| repo::folders::list(db, &account_id)
            })
            .await?;
        if let Some(folder) = folders.iter().find(|folder| {
            folder.role == FolderRole::Drafts
                && !folder
                    .remote_path
                    .starts_with(crate::sync::folders::LOCAL_FOLDER_PREFIX)
        }) {
            return Ok(Some(folder.clone()));
        }
        if let Some(folder) = folders.iter().find(|folder| {
            folder.role == FolderRole::Drafts
                && folder
                    .remote_path
                    .starts_with(crate::sync::folders::LOCAL_FOLDER_PREFIX)
        }) {
            return Ok(Some(folder.clone()));
        }
        let folder = Folder {
            id: nuncio_core::models::new_id(),
            account_id,
            parent_id: None,
            remote_path: LOCAL_DRAFTS_PATH.to_string(),
            name: "Drafts".to_string(),
            role: FolderRole::Drafts,
            unread_count: 0,
            total_count: 0,
            is_favorite: false,
            sync_enabled: false,
            is_selectable: true,
            sort_order: 2,
            last_sync_at: None,
        };
        let folder_for_db = folder.clone();
        self.inner
            .ctx
            .db
            .write(move |db| repo::folders::insert(db, &folder_for_db))
            .await?;
        events::emit_folders_changed(
            self.inner.ctx.events.as_ref(),
            account.id.clone(),
            vec![folder.id.clone()],
        );
        debug!(account = %account.id, folder = %folder.id, "created local Drafts folder");
        Ok(Some(folder))
    }

    /// Project a composer draft into the Drafts folder as an `is_draft` message
    /// row. This makes offline drafts visible/reopenable before (or without)
    /// a server APPEND.
    async fn store_draft_message(
        &self,
        account: &nuncio_core::models::Account,
        draft: &Draft,
        folder: &Folder,
        header_id: &str,
        rfc822_path: &str,
        size: u64,
    ) -> Result<String> {
        let draft_id = draft.id.clone().ok_or_else(|| {
            AppError::internal("draft id missing while storing the draft message")
        })?;
        let message_id = crate::util::draft_message_id(&draft_id);
        let now = draft.updated_at.unwrap_or_else(nuncio_core::models::now);
        let text = nuncio_mime::html_to_text(&draft.html);
        let mut summary = MessageSummary::new(
            message_id.clone(),
            account.id.clone(),
            folder.id.clone(),
            format!("draft:{draft_id}"),
            now,
        );
        summary.subject = draft.subject.clone();
        summary.r#from = Some(Address {
            name: draft
                .from
                .name
                .clone()
                .filter(|name| !name.trim().is_empty()),
            email: if draft.from.email.trim().is_empty() {
                account.email.clone()
            } else {
                draft.from.email.clone()
            },
        });
        summary.to = draft.to.clone();
        summary.cc = draft.cc.clone();
        summary.preview = nuncio_core::text::trim_preview(&text, 200);
        summary.seen = true;
        summary.is_draft = true;
        summary.has_attachments = !draft.attachments.is_empty();
        summary.importance = draft.importance;
        summary.size = size;

        let mut stored = StoredMessage::new(summary);
        stored.bcc = draft.bcc.clone();
        stored.message_id_header = Some(header_id.to_string());
        stored.in_reply_to = draft.in_reply_to.clone();
        stored.references = draft.references.clone();
        stored.body_path = Some(rfc822_path.to_string());
        stored.body_cached_at = Some(nuncio_core::models::now());
        stored.internal_date = Some(now);

        let message_id_for_db = message_id.clone();
        let folder_id = folder.id.clone();
        let path_for_db = rfc822_path.to_string();
        let text_for_fts = text;
        let preview = stored.summary.preview.clone();
        self.inner
            .ctx
            .db
            .write(move |db| {
                let mut stored = stored;
                if let Some(existing) = repo::messages::get(db, &message_id_for_db)? {
                    // Keep server-side identity/flags when this row is also
                    // the synced APPEND copy.
                    stored.uid = existing.uid;
                    stored.uidvalidity = existing.uidvalidity;
                    stored.gm_msgid = existing.gm_msgid;
                    stored.gm_thrid = existing.gm_thrid;
                    stored.labels = existing.labels;
                    stored.raw_headers = existing.raw_headers;
                }
                repo::messages::upsert(db, &stored)?;
                repo::messages::set_body(
                    db,
                    &message_id_for_db,
                    Some(&path_for_db),
                    Some(&text_for_fts),
                )?;
                repo::messages::update_preview(db, &message_id_for_db, &preview)?;
                repo::folders::recompute_counts(db, &folder_id)?;
                Ok(())
            })
            .await?;

        events::emit_mail_changed(
            self.inner.ctx.events.as_ref(),
            account.id.clone(),
            vec![folder.id.clone()],
            vec![message_id.clone()],
            MailChangeKind::Updated,
        );
        Ok(message_id)
    }

    /// Remove the Drafts projection of a local draft (send/discard).
    async fn remove_draft_message(&self, draft_id: &str) -> Result<Option<String>> {
        let message_id = crate::util::draft_message_id(draft_id);
        let stored = {
            let id = message_id.clone();
            self.inner
                .ctx
                .db
                .read(move |db| repo::messages::get(db, &id))
                .await?
        };
        let Some(stored) = stored else {
            return Ok(None);
        };
        let folder_id = stored.summary.folder_id.clone();
        let id = message_id.clone();
        self.inner
            .ctx
            .db
            .write(move |db| repo::messages::delete_local(db, &[id], true))
            .await?;
        events::emit_mail_changed(
            self.inner.ctx.events.as_ref(),
            stored.summary.account_id.clone(),
            vec![folder_id.clone()],
            vec![message_id],
            MailChangeKind::Deleted,
        );
        Ok(Some(folder_id))
    }

    async fn drafts_folder_id(&self, account_id: &str) -> Result<Option<String>> {
        Ok(self
            .folder_by_role(account_id, FolderRole::Drafts)
            .await?
            .map(|folder| folder.id))
    }

    /// A pending (or running) operation of the given kind for this draft.
    async fn pending_draft_operation(
        &self,
        account_id: &str,
        kind: &str,
        draft_id: &str,
    ) -> Result<Option<String>> {
        let account_id = account_id.to_string();
        let kind = kind.to_string();
        let draft_id = draft_id.to_string();
        self.inner
            .ctx
            .db
            .read(move |db| {
                for op in repo::pending_ops::list(db, Some(&account_id))? {
                    if op.kind != kind
                        || !matches!(
                            op.state,
                            PendingOpState::Pending | PendingOpState::InProgress
                        )
                    {
                        continue;
                    }
                    let payload: AppendDraftOp = match repo::pending_ops::payload(&op) {
                        Ok(payload) => payload,
                        Err(_) => continue,
                    };
                    if payload.draft_id == draft_id {
                        return Ok(Some(op.id));
                    }
                }
                Ok(None)
            })
            .await
    }

    async fn pending_delete_remote_draft(
        &self,
        account_id: &str,
        folder_id: &str,
        uid: i64,
    ) -> Result<bool> {
        let account_id = account_id.to_string();
        let folder_id = folder_id.to_string();
        self.inner
            .ctx
            .db
            .read(move |db| {
                for op in repo::pending_ops::list(db, Some(&account_id))? {
                    if op.kind != KIND_DELETE_REMOTE_DRAFT
                        || !matches!(
                            op.state,
                            PendingOpState::Pending | PendingOpState::InProgress
                        )
                    {
                        continue;
                    }
                    let Ok(payload) = repo::pending_ops::payload::<DeleteRemoteDraftOp>(&op) else {
                        continue;
                    };
                    if payload.folder_id == folder_id && payload.uid == uid {
                        return Ok(true);
                    }
                }
                Ok(false)
            })
            .await
    }

    async fn cancel_pending_draft_appends(&self, account_id: &str, draft_id: &str) -> Result<()> {
        let account_id = account_id.to_string();
        let draft_id = draft_id.to_string();
        let ids = self
            .inner
            .ctx
            .db
            .read(move |db| {
                let mut ids = Vec::new();
                for op in repo::pending_ops::list(db, Some(&account_id))? {
                    if op.kind != KIND_APPEND_DRAFT
                        || !matches!(
                            op.state,
                            PendingOpState::Pending | PendingOpState::InProgress
                        )
                    {
                        continue;
                    }
                    if repo::pending_ops::payload::<AppendDraftOp>(&op)
                        .map(|payload| payload.draft_id == draft_id)
                        .unwrap_or(false)
                    {
                        ids.push(op.id);
                    }
                }
                Ok(ids)
            })
            .await?;
        for id in ids {
            self.inner
                .ctx
                .db
                .write(move |db| repo::pending_ops::cancel(db, &id))
                .await?;
        }
        Ok(())
    }

    async fn find_remote_draft(
        &self,
        account: &nuncio_core::models::Account,
        drafts_folder: &nuncio_core::models::Folder,
        draft: &Draft,
        header_id: &str,
    ) -> Result<Option<StoredMessage>> {
        if let Some(message_id) = draft.remote_draft_message_id.as_deref() {
            if let Ok(stored) = self.stored(message_id).await {
                if stored.summary.account_id == account.id
                    && stored.summary.folder_id == drafts_folder.id
                {
                    return Ok(Some(stored));
                }
            }
        }
        let account_id = account.id.clone();
        let header = header_id.to_string();
        let folder_id = drafts_folder.id.clone();
        let found = self
            .inner
            .ctx
            .db
            .read(move |db| {
                let rows = repo::messages::by_message_id(db, &header, Some(&account_id))?;
                Ok(rows
                    .into_iter()
                    .find(|row| row.summary.folder_id == folder_id))
            })
            .await?;
        Ok(found)
    }
}

fn is_local_only_account(provider: ProviderKind) -> bool {
    matches!(provider, ProviderKind::Local | ProviderKind::Pop3)
}

pub(crate) fn identity_address(identity: &Identity) -> Address {
    Address {
        name: if identity.name.trim().is_empty() {
            None
        } else {
            Some(identity.name.clone())
        },
        email: identity.email.clone(),
    }
}

fn signature_block(signature: Option<&Signature>) -> String {
    match signature {
        Some(signature) if !signature.html.trim().is_empty() => {
            // Signatures are HTML supplied by the user (or restored from a
            // backup); sanitize before inserting into the composer.
            let sanitized = nuncio_mime::sanitize_fragment(&signature.html);
            if sanitized.trim().is_empty() {
                return String::new();
            }
            format!("<div class=\"nuncio-signature\"><br>{sanitized}</div>")
        }
        _ => String::new(),
    }
}

fn insert_signature(body: &str, signature: Option<&Signature>) -> String {
    let block = signature_block(signature);
    if block.is_empty() {
        return body.to_string();
    }
    if let Some(rest) = body.strip_prefix("<p><br></p>") {
        format!("<p><br></p>{block}{rest}")
    } else {
        format!("{block}{body}")
    }
}

fn draft_message_id(draft_id: &str) -> String {
    format!("nuncio-draft-{draft_id}@nuncio.local")
}

fn all_recipients(draft: &Draft) -> Result<Vec<Address>> {
    let mut recipients = draft.to.clone();
    recipients.extend(draft.cc.clone());
    recipients.extend(draft.bcc.clone());
    let recipients = nuncio_mime::dedupe_addresses(recipients);
    if recipients.is_empty() {
        return Err(AppError::invalid_input(
            "add at least one recipient before sending",
        ));
    }
    for address in &recipients {
        let email = address.email.trim();
        if !email.contains('@') || email.starts_with('@') || email.ends_with('@') {
            return Err(AppError::invalid_input(format!(
                "invalid recipient address: {email}"
            )));
        }
    }
    Ok(recipients)
}

pub(crate) async fn unique_name(dir: &Path, filename: &str, used: &mut HashSet<String>) -> String {
    let sanitized = nuncio_core::paths::sanitize_filename(filename);
    let base = if sanitized.is_empty() {
        "attachment".to_string()
    } else {
        sanitized
    };
    let (stem, extension) = match base.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => {
            (stem.to_string(), Some(extension.to_string()))
        }
        _ => (base.clone(), None),
    };
    let mut index = 0u32;
    loop {
        let candidate = if index == 0 {
            base.clone()
        } else {
            match &extension {
                Some(extension) => format!("{stem} ({index}).{extension}"),
                None => format!("{stem} ({index})"),
            }
        };
        let key = candidate.to_lowercase();
        let exists = used.contains(&key) || tokio::fs::metadata(dir.join(&candidate)).await.is_ok();
        if !exists {
            used.insert(key);
            return candidate;
        }
        index += 1;
        if index > 10_000 {
            return format!("{}-{}", nuncio_core::models::new_id(), base);
        }
    }
}

fn safe_stem(subject: &str) -> String {
    let stem: String = subject
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, ' ' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let stem = stem.trim().trim_matches('_').to_string();
    if stem.is_empty() {
        "message".to_string()
    } else {
        stem.chars().take(60).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_block_is_sanitized_and_marked_for_swapping() {
        let signature = Signature {
            id: "sig-1".into(),
            account_id: None,
            name: "Work".into(),
            html: concat!(
                "<p onclick=\"steal()\">Hi</p>",
                "<script>alert(1)</script>",
                "<a href=\"javascript:alert(1)\">click</a>",
                "<img src=\"https://example.com/a.png\">"
            )
            .into(),
            use_for_new: true,
            use_for_reply: true,
        };
        let block = signature_block(Some(&signature));
        assert!(block.starts_with("<div class=\"nuncio-signature\">"));
        assert!(block.ends_with("</div>"));
        assert!(block.contains("Hi"));
        assert!(!block.to_ascii_lowercase().contains("<script"));
        assert!(!block.to_ascii_lowercase().contains("onclick"));
        assert!(!block.to_ascii_lowercase().contains("javascript:"));
    }

    #[test]
    fn empty_signatures_produce_no_block() {
        assert_eq!(signature_block(None), "");
        let signature = Signature {
            id: "sig-2".into(),
            account_id: None,
            name: "Empty".into(),
            html: "   ".into(),
            use_for_new: false,
            use_for_reply: false,
        };
        assert_eq!(signature_block(Some(&signature)), "");
        assert_eq!(insert_signature("<p><br></p>x", None), "<p><br></p>x");
    }
}
