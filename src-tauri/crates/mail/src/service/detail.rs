//! Reading messages: MIME parsing, sanitized rendering, remote-content policy,
//! cid resolution, invitations, `.eml` files and unsubscribing.

use std::path::PathBuf;

use nuncio_core::events::MailChangeKind;
use nuncio_core::models::{
    Account, Address, Attachment, CalendarEvent, InvitePreview, MarkReadMode, MessageDetail,
    MessageSummary,
};
use nuncio_core::{events, repo, AppError, Result};
use nuncio_mime::{
    html_to_text, parse, parse_headers_only, ParsedAttachment, ParsedMessage, RenderInput,
};
use tracing::{debug, warn};

use super::{ExternalAttachment, MailService};

const MAX_EML_BYTES: u64 = 256 * 1024 * 1024;

impl MailService {
    /// Full message detail. Downloads the body when needed, applies the
    /// remote-content policy, renders sanitized HTML and detects invitations.
    pub async fn message_get(&self, id: &str, allow_remote: bool) -> Result<MessageDetail> {
        if let Some(item) = self.outbox_item(id).await? {
            return self.outbox_detail(&item, allow_remote).await;
        }
        let mut stored = self.stored(id).await?;
        let raw = self.raw_message(&stored).await?;
        // A body download refreshes headers/attachments/preview in the DB.
        stored = self.stored(id).await?;

        if self.inner.ctx.settings.current().mail.mark_read_mode == MarkReadMode::OnOpen {
            self.mark_seen_on_open(&mut stored).await?;
        }

        let parsed = parse(&raw)?;
        let account = self.account(&stored.summary.account_id).await?;
        let allow = self
            .remote_content_allowed(allow_remote, stored.summary.r#from.as_ref())
            .await?;

        let mut summary = stored.summary.clone();
        let mut thread = self
            .message_thread(&summary.account_id, &summary.thread_id)
            .await?;
        if thread.is_empty() {
            thread.push(summary.clone());
        }
        summary.thread_count = thread.len() as i64;
        summary.thread_unread = thread.iter().filter(|message| !message.seen).count() as i64;

        let attachments = self
            .inner
            .ctx
            .db
            .read({
                let message_id = summary.id.clone();
                move |db| repo::attachments::list(db, &message_id)
            })
            .await?;
        let invite = self
            .invite_preview(&parsed, &account)
            .await
            .unwrap_or_else(|error| {
                warn!(%error, message = %summary.id, "could not build invitation preview");
                None
            });

        let settings = self.inner.ctx.settings.current();
        let rendered = nuncio_mime::render_document(RenderInput {
            html: parsed.html.clone(),
            text: parsed.text.clone(),
            message_id: summary.id.clone(),
            allow_remote: allow,
            cid_base: self.cid_base().to_string(),
            light_background: settings.mail.light_message_background,
        });
        let text = parsed
            .text
            .clone()
            .or_else(|| parsed.html.as_deref().map(html_to_text))
            .filter(|text| !text.trim().is_empty());
        let raw_headers = stored
            .raw_headers
            .clone()
            .filter(|headers| !headers.trim().is_empty())
            .unwrap_or_else(|| parsed.headers.raw.clone());

        Ok(MessageDetail {
            summary,
            to: parsed.headers.to.clone(),
            cc: parsed.headers.cc.clone(),
            bcc: parsed.headers.bcc.clone(),
            reply_to: parsed.headers.reply_to.clone(),
            message_id_header: parsed.headers.message_id.clone(),
            in_reply_to: parsed.headers.in_reply_to.clone(),
            references: parsed.headers.references.clone(),
            html: rendered.document,
            text,
            has_remote_content: rendered.has_remote_content,
            remote_content_allowed: allow,
            attachments,
            invite,
            list_unsubscribe: parsed.headers.list_unsubscribe.clone(),
            raw_headers,
            external_path: None,
        })
    }

    /// Read-only detail for a queued/failed outbox message. The RFC822 file is
    /// served from the staging directory and its attachments are exposed
    /// through the external registry.
    async fn outbox_detail(
        &self,
        item: &nuncio_core::models::OutboxItem,
        allow_remote: bool,
    ) -> Result<MessageDetail> {
        let metadata = tokio::fs::metadata(&item.rfc822_path)
            .await
            .map_err(|error| AppError::io(format!("cannot read queued message: {error}")))?;
        if metadata.len() > MAX_EML_BYTES {
            return Err(AppError::invalid_input("queued message is too large"));
        }
        let raw = tokio::fs::read(&item.rfc822_path)
            .await
            .map_err(|error| AppError::io(format!("cannot read queued message: {error}")))?;
        let parsed = parse(&raw)?;
        let account = self.account(&item.account_id).await?;
        let folder_id = self
            .outbox_folder_id(&item.account_id)
            .await?
            .unwrap_or_else(|| "outbox".to_string());
        let summary = super::outbox::summary_from_item(item, &account, &folder_id);
        let message_id = summary.id.clone();

        let records: Vec<ExternalAttachment> = parsed
            .attachments
            .iter()
            .enumerate()
            .map(|(index, attachment)| ExternalAttachment {
                attachment_id: crate::util::attachment_id(
                    &message_id,
                    index,
                    attachment.content_id.as_deref(),
                ),
                filename: attachment.filename.clone(),
                mime_type: attachment.mime_type.clone(),
                size: attachment.size,
                content_id: attachment.content_id.clone(),
                part: attachment.part.as_string(),
                is_inline: attachment.is_inline,
            })
            .collect();
        let attachments: Vec<Attachment> = records
            .iter()
            .map(|record| Attachment {
                id: record.attachment_id.clone(),
                message_id: message_id.clone(),
                filename: record.filename.clone(),
                mime_type: record.mime_type.clone(),
                size: record.size,
                content_id: record.content_id.clone(),
                is_inline: record.is_inline,
                is_downloaded: false,
            })
            .collect();
        self.external_insert(message_id.clone(), raw, records);

        let allow = self
            .remote_content_allowed(allow_remote, summary.r#from.as_ref())
            .await?;
        let settings = self.inner.ctx.settings.current();
        let rendered = nuncio_mime::render_document(RenderInput {
            html: parsed.html.clone(),
            text: parsed.text.clone(),
            message_id: message_id.clone(),
            allow_remote: allow,
            cid_base: self.cid_base().to_string(),
            light_background: settings.mail.light_message_background,
        });
        let text = parsed
            .text
            .clone()
            .or_else(|| parsed.html.as_deref().map(html_to_text))
            .filter(|text| !text.trim().is_empty());

        Ok(MessageDetail {
            summary,
            to: parsed.headers.to.clone(),
            cc: parsed.headers.cc.clone(),
            bcc: parsed.headers.bcc.clone(),
            reply_to: parsed.headers.reply_to.clone(),
            message_id_header: parsed.headers.message_id.clone(),
            in_reply_to: parsed.headers.in_reply_to.clone(),
            references: parsed.headers.references.clone(),
            html: rendered.document,
            text,
            has_remote_content: rendered.has_remote_content,
            remote_content_allowed: allow,
            attachments,
            invite: None,
            list_unsubscribe: parsed.headers.list_unsubscribe.clone(),
            raw_headers: parsed.headers.raw.clone(),
            external_path: None,
        })
    }

    /// Parse a `.eml` file without touching the database. Attachments stay
    /// addressable through the in-memory registry so cid images render.
    pub async fn message_open_eml(&self, path: &str) -> Result<MessageDetail> {
        let path_buf = PathBuf::from(path);
        let metadata = tokio::fs::metadata(&path_buf)
            .await
            .map_err(|error| AppError::io(format!("cannot open {path}: {error}")))?;
        if metadata.len() > MAX_EML_BYTES {
            return Err(AppError::invalid_input("message file is too large"));
        }
        let raw = tokio::fs::read(&path_buf)
            .await
            .map_err(|error| AppError::io(format!("cannot read {path}: {error}")))?;
        let parsed = parse(&raw)?;

        let modified = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|value| value.as_secs())
            .unwrap_or(0);
        let message_id = crate::util::stable_id(
            "external-eml",
            &format!("{}|{}|{}", path_buf.display(), raw.len(), modified),
        );
        let date = parsed.headers.date.unwrap_or_else(nuncio_core::models::now);
        let thread_id = crate::util::thread_id(
            parsed.headers.message_id.as_deref(),
            parsed.headers.in_reply_to.as_deref(),
            &parsed.headers.references,
            &parsed.headers.subject,
            None,
        );
        let mut summary =
            MessageSummary::new(message_id.clone(), "local", "external", thread_id, date);
        summary.subject = parsed.headers.subject.clone();
        summary.r#from = parsed.headers.from.clone();
        summary.to = parsed.headers.to.clone();
        summary.cc = parsed.headers.cc.clone();
        summary.date = date;
        summary.preview = nuncio_core::text::trim_preview(&parsed.preview, 300);
        summary.seen = true;
        summary.has_attachments = !parsed.attachments.is_empty();
        summary.importance = parsed.headers.importance;
        summary.size = raw.len() as u64;

        let records: Vec<ExternalAttachment> = parsed
            .attachments
            .iter()
            .enumerate()
            .map(|(index, attachment)| ExternalAttachment {
                attachment_id: crate::util::attachment_id(
                    &message_id,
                    index,
                    attachment.content_id.as_deref(),
                ),
                filename: attachment.filename.clone(),
                mime_type: attachment.mime_type.clone(),
                size: attachment.size,
                content_id: attachment.content_id.clone(),
                part: attachment.part.as_string(),
                is_inline: attachment.is_inline,
            })
            .collect();
        let attachments: Vec<Attachment> = records
            .iter()
            .map(|record| Attachment {
                id: record.attachment_id.clone(),
                message_id: message_id.clone(),
                filename: record.filename.clone(),
                mime_type: record.mime_type.clone(),
                size: record.size,
                content_id: record.content_id.clone(),
                is_inline: record.is_inline,
                is_downloaded: false,
            })
            .collect();
        self.external_insert(message_id.clone(), raw, records);

        let allow = self
            .remote_content_allowed(false, summary.r#from.as_ref())
            .await?;
        let settings = self.inner.ctx.settings.current();
        let rendered = nuncio_mime::render_document(RenderInput {
            html: parsed.html.clone(),
            text: parsed.text.clone(),
            message_id: message_id.clone(),
            allow_remote: allow,
            cid_base: self.cid_base().to_string(),
            light_background: settings.mail.light_message_background,
        });
        let text = parsed
            .text
            .clone()
            .or_else(|| parsed.html.as_deref().map(html_to_text))
            .filter(|text| !text.trim().is_empty());

        Ok(MessageDetail {
            summary,
            to: parsed.headers.to.clone(),
            cc: parsed.headers.cc.clone(),
            bcc: parsed.headers.bcc.clone(),
            reply_to: parsed.headers.reply_to.clone(),
            message_id_header: parsed.headers.message_id.clone(),
            in_reply_to: parsed.headers.in_reply_to.clone(),
            references: parsed.headers.references.clone(),
            html: rendered.document,
            text,
            has_remote_content: rendered.has_remote_content,
            remote_content_allowed: allow,
            attachments,
            invite: None,
            list_unsubscribe: parsed.headers.list_unsubscribe.clone(),
            raw_headers: parsed.headers.raw.clone(),
            external_path: Some(path.to_string()),
        })
    }

    /// Save a message as `.eml` (also works for externally opened files).
    pub async fn message_save_eml(&self, id: &str, path: &str) -> Result<()> {
        let raw = self.raw_message_by_id(id).await?;
        let target = PathBuf::from(path);
        crate::service::attachments::mark_of_the_web(&target);
        tokio::fs::write(&target, &raw)
            .await
            .map_err(|error| AppError::io(format!("cannot save message: {error}")))?;
        Ok(())
    }

    /// Build an invitation preview from the first `text/calendar` part.
    async fn invite_preview(
        &self,
        parsed: &ParsedMessage,
        account: &Account,
    ) -> Result<Option<InvitePreview>> {
        let Some(calendar_part) = parsed.calendar_parts.first() else {
            return Ok(None);
        };
        let parsed_calendar = match nuncio_ical::ics::parse_calendar(&calendar_part.text) {
            Ok(value) => value,
            Err(error) => {
                debug!(%error, "could not parse the calendar part");
                return Ok(None);
            }
        };
        let Some(parsed_event) = parsed_calendar.events.into_iter().next() else {
            return Ok(None);
        };
        let method = parsed_calendar
            .method
            .unwrap_or_else(|| "REQUEST".to_string())
            .to_ascii_uppercase();

        let identities = {
            let account_id = account.id.clone();
            self.inner
                .ctx
                .db
                .read(move |db| repo::identities::list(db, Some(&account_id)))
                .await?
        };
        let mut my_emails: Vec<String> = identities
            .iter()
            .map(|identity| nuncio_core::text::normalize_email(&identity.email))
            .filter(|email| !email.is_empty())
            .collect();
        if my_emails.is_empty() {
            my_emails.push(nuncio_core::text::normalize_email(&account.email));
        }

        // Match an event already stored locally by UID (any calendar).
        let calendars = self.inner.ctx.db.read(repo::calendars::list).await?;
        let uid = parsed_event.uid.clone();
        let existing = {
            let calendars = calendars.clone();
            let uid = uid.clone();
            self.inner
                .ctx
                .db
                .read(move |db| {
                    for calendar in &calendars {
                        if let Some(stored) = repo::events::by_uid(db, &calendar.id, &uid)? {
                            return Ok(Some(stored));
                        }
                    }
                    Ok(None)
                })
                .await?
        };
        let default_calendar = match &existing {
            Some(stored) => stored.event.calendar_id.clone(),
            None => {
                let account_id = account.id.clone();
                match self
                    .inner
                    .ctx
                    .db
                    .read(move |db| repo::calendars::default_for_account(db, &account_id))
                    .await?
                {
                    Some(calendar) => calendar.id,
                    None => calendars
                        .first()
                        .map(|calendar| calendar.id.clone())
                        .unwrap_or_else(|| nuncio_core::models::LOCAL_CALENDAR_ID.to_string()),
                }
            }
        };
        let mut event: CalendarEvent = parsed_event.into_event(
            nuncio_core::models::new_id(),
            default_calendar,
            account.id.clone(),
        );
        let matches_me = |address: &Address| {
            my_emails.contains(&nuncio_core::text::normalize_email(&address.email))
        };
        let my_response = event
            .attendees
            .iter()
            .find(|attendee| {
                my_emails.contains(&nuncio_core::text::normalize_email(&attendee.email))
            })
            .map(|attendee| attendee.status)
            .or_else(|| {
                existing
                    .as_ref()
                    .and_then(|stored| stored.event.my_response)
            });
        let is_organizer = event
            .organizer
            .as_ref()
            .map(|attendee| {
                matches_me(&Address {
                    name: attendee.name.clone(),
                    email: attendee.email.clone(),
                })
            })
            .unwrap_or(false)
            || existing
                .as_ref()
                .map(|stored| stored.event.is_organizer)
                .unwrap_or(false);
        event.my_response = my_response;
        event.is_organizer = is_organizer;

        let can_respond = method == "REQUEST"
            && !is_organizer
            && my_response.is_some()
            && event.status != nuncio_core::models::EventStatus::Cancelled;

        Ok(Some(InvitePreview {
            method,
            event,
            existing_event_id: existing.map(|stored| stored.event.id),
            can_respond,
            my_response,
        }))
    }

    /// Serve a `cid:` image for the app's custom URI protocol handler.
    /// Errors are logged and reported as `None` (the protocol returns 404).
    pub async fn resolve_cid(
        &self,
        message_id: &str,
        content_id: &str,
    ) -> Option<(String, Vec<u8>)> {
        let content_id = normalize_content_id(content_id);
        // External .eml files live only in memory.
        if let Some(raw) = self.external_raw(message_id) {
            if let Ok(parsed) = parse(&raw) {
                if let Some(attachment) = find_parsed_attachment(&parsed, &content_id) {
                    match nuncio_mime::extract_part(&raw, &attachment.part) {
                        Ok(bytes) => return Some((attachment.mime_type.clone(), bytes)),
                        Err(error) => {
                            debug!(%error, "could not extract external cid attachment");
                        }
                    }
                }
            }
        }
        // Stored messages: try the downloaded cache first, then the MIME body.
        let cached = {
            let message_id = message_id.to_string();
            let content_id = content_id.clone();
            self.inner
                .ctx
                .db
                .read(move |db| {
                    let attachment =
                        repo::attachments::by_content_id(db, &message_id, &content_id)?;
                    match attachment {
                        Some(attachment) => Ok(Some(attachment)),
                        None => Ok(None),
                    }
                })
                .await
                .ok()
                .flatten()
        };
        if let Some(attachment) = cached {
            if let Ok(Some(path)) = self
                .inner
                .ctx
                .db
                .read({
                    let id = attachment.id.clone();
                    move |db| repo::attachments::local_path(db, &id)
                })
                .await
            {
                if let Ok(bytes) = tokio::fs::read(&path).await {
                    return Some((attachment.mime_type, bytes));
                }
            }
        }
        let raw = self.raw_message_by_id(message_id).await.ok()?;
        let parsed = parse(&raw).ok()?;
        let attachment = find_parsed_attachment(&parsed, &content_id)?;
        match nuncio_mime::extract_part(&raw, &attachment.part) {
            Ok(bytes) => Some((attachment.mime_type.clone(), bytes)),
            Err(error) => {
                debug!(%error, %message_id, "could not extract cid attachment");
                None
            }
        }
    }

    /// Unsubscribe from a mailing list:
    /// * `mailto:` → send an unsubscribe e-mail through the outbox;
    /// * RFC 8058 one-click HTTPS → POST;
    /// * otherwise → `Unsupported` whose message contains the URL for the UI.
    pub async fn message_unsubscribe(&self, id: &str) -> Result<()> {
        let stored = self.stored(id).await?;
        let headers = match stored.raw_headers.as_deref() {
            Some(raw) if !raw.trim().is_empty() => parse_headers_only(raw.as_bytes()).ok(),
            _ => None,
        };
        let headers = match headers {
            Some(headers) => headers,
            None => {
                let raw = self.raw_message(&stored).await?;
                parse(&raw)?.headers
            }
        };
        let Some(header) = headers.list_unsubscribe.clone() else {
            return Err(AppError::unsupported(
                "message has no List-Unsubscribe header",
            ));
        };
        let one_click = headers
            .raw
            .to_ascii_lowercase()
            .contains("list-unsubscribe-post")
            && headers.raw.to_ascii_lowercase().contains("one-click");

        let mut urls = subscribe_urls(&header);
        // Prefer HTTPS (one-click), then mailto.
        urls.sort_by_key(|url| {
            if url.starts_with("https://") {
                0
            } else if url.starts_with("http://") {
                1
            } else if url.starts_with("mailto:") {
                2
            } else {
                3
            }
        });
        let Some(url) = urls.into_iter().next() else {
            return Err(AppError::unsupported(
                "List-Unsubscribe contains no usable URL",
            ));
        };

        if url.starts_with("http://") {
            // Never POST to plain HTTP; let the UI open the page instead.
            return Err(AppError::unsupported(format!("open {url} to unsubscribe")));
        }
        if url.starts_with("https://") {
            if !one_click {
                return Err(AppError::unsupported(format!("open {url} to unsubscribe")));
            }
            crate::util::ensure_crypto_provider();
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .map_err(|error| AppError::internal(format!("HTTP client: {error}")))?;
            let response = client
                .post(url.as_str())
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body("List-Unsubscribe=One-Click")
                .send()
                .await
                .map_err(|error| {
                    AppError::network(format!("unsubscribe request failed: {error}"))
                })?;
            if !response.status().is_success() {
                return Err(AppError::network(format!(
                    "unsubscribe endpoint returned HTTP {}",
                    response.status()
                )));
            }
            return Ok(());
        }

        let mailto = if url.starts_with("mailto:") {
            url
        } else {
            format!("mailto:{url}")
        };
        let data = nuncio_mime::parse_mailto(&mailto)?;
        let account = self.account(&stored.summary.account_id).await?;
        let identity = self.ensure_default_identity(&account.id).await?;
        let subject = if data.subject.trim().is_empty() {
            "unsubscribe".to_string()
        } else {
            data.subject.clone()
        };
        let body = if data.body.trim().is_empty() {
            "unsubscribe".to_string()
        } else {
            data.body.clone()
        };
        let mut message = nuncio_mime::OutgoingMessage::new(
            Address {
                name: if identity.name.trim().is_empty() {
                    None
                } else {
                    Some(identity.name)
                },
                email: identity.email,
            },
            subject.clone(),
            nuncio_mime::plain_text_to_html(&body),
        );
        message.to = if data.to.is_empty() {
            vec![Address::new(
                stored
                    .summary
                    .r#from
                    .as_ref()
                    .map(|from| from.email.clone())
                    .unwrap_or_default(),
            )]
        } else {
            data.to
        };
        message.cc = data.cc;
        let (raw, _) = nuncio_mime::build_message(message)?;
        let _ = self
            .queue_outbox(&account, None, subject, message_to_recipients(&raw)?, raw)
            .await?;
        Ok(())
    }

    /// Queue already-built RFC822 bytes in the outbox and try to send now.
    pub(crate) async fn queue_outbox(
        &self,
        account: &Account,
        draft_id: Option<String>,
        subject: String,
        recipients: Vec<Address>,
        raw: Vec<u8>,
    ) -> Result<bool> {
        if recipients.is_empty() {
            return Err(AppError::invalid_input("message has no recipients"));
        }
        let path = self
            .inner
            .ctx
            .paths
            .staging_dir()
            .join(format!("outbox-{}.eml", nuncio_core::models::new_id()));
        self.inner.ctx.paths.ensure_parent(&path)?;
        tokio::fs::write(&path, &raw)
            .await
            .map_err(|error| AppError::io(format!("cannot stage outgoing message: {error}")))?;
        let item = nuncio_core::models::OutboxItem::new(
            account.id.clone(),
            draft_id,
            recipients.clone(),
            subject,
            path.to_string_lossy().to_string(),
        );
        let item = self
            .inner
            .ctx
            .db
            .write(move |db| repo::outbox::enqueue(db, &item))
            .await?;
        if let Err(error) =
            crate::service::outbox::refresh_outbox_folder(&self.inner.ctx, &account.id).await
        {
            debug!(%error, "could not refresh the Outbox folder");
        }
        if let Err(error) = self
            .inner
            .ctx
            .db
            .write({
                let recipients = recipients.clone();
                move |db| repo::recipient_history::record(db, &recipients)
            })
            .await
        {
            debug!(%error, "could not record recipient history");
        }
        match self.inner.engine.process_outbox(&account.id).await {
            Ok(_) => {}
            Err(error) => {
                warn!(account = %account.id, %error, "outbox processing failed; message stays queued");
            }
        }
        if let Err(error) =
            crate::service::outbox::refresh_outbox_folder(&self.inner.ctx, &account.id).await
        {
            debug!(%error, "could not refresh the Outbox folder");
        }
        events::emit_mail_changed(
            self.inner.ctx.events.as_ref(),
            account.id.clone(),
            Vec::new(),
            Vec::new(),
            MailChangeKind::Updated,
        );
        let sent = self
            .inner
            .ctx
            .db
            .read(move |db| repo::outbox::get_required(db, &item.id))
            .await
            .map(|item| item.state == nuncio_core::models::OutboxState::Sent)
            .unwrap_or(false);
        Ok(sent)
    }
}

fn normalize_content_id(value: &str) -> String {
    let trimmed = value.trim().trim_matches('<').trim_matches('>').trim();
    if let Ok(decoded) = urlencoding::decode(trimmed) {
        if decoded != trimmed && !decoded.is_empty() {
            return decoded.to_string();
        }
    }
    trimmed.to_string()
}

fn find_parsed_attachment<'a>(
    parsed: &'a ParsedMessage,
    content_id: &str,
) -> Option<&'a ParsedAttachment> {
    let target = normalize_content_id(content_id);
    parsed.attachments.iter().find(|attachment| {
        attachment
            .content_id
            .as_deref()
            .map(normalize_content_id)
            .as_deref()
            == Some(target.as_str())
    })
}

/// Parse `<mailto:...>, <https://...>` style headers.
fn subscribe_urls(header: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rest = header;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('>') else { break };
        let url = after[..close].trim();
        if !url.is_empty() {
            urls.push(url.to_string());
        }
        rest = &after[close + 1..];
    }
    if urls.is_empty() {
        for part in header.split(',') {
            let part = part.trim();
            if !part.is_empty() {
                urls.push(part.to_string());
            }
        }
    }
    urls
}

/// Envelope recipients of a built message (To + Cc + Bcc).
fn message_to_recipients(raw: &[u8]) -> Result<Vec<Address>> {
    let parsed = parse(raw)?;
    let mut recipients = parsed.headers.to.clone();
    recipients.extend(parsed.headers.cc.clone());
    recipients.extend(parsed.headers.bcc.clone());
    Ok(nuncio_mime::dedupe_addresses(recipients))
}
