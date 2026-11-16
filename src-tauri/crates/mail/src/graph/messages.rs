//! Microsoft Graph message synchronization for one folder.
//!
//! Microsoft personal accounts (MSA) are refused an IMAP session by Exchange
//! ("User is authenticated but not connected.") even with a valid token, while
//! Graph serves the same mailbox over HTTPS. This module therefore mirrors the
//! IMAP metadata path in [`crate::sync::messages`] on top of Graph's
//! change-tracking delta endpoint:
//!
//! * the first sync without a cursor does a full delta scan, bounded by the
//!   account's sync window (`$filter=receivedDateTime ge ...`), mirroring the
//!   IMAP `SINCE` search;
//! * later syncs resume from the persisted `@odata.deltaLink`, so only changes
//!   are transferred;
//! * a pass truncated by [`MAX_PAGES`] persists its `@odata.nextLink` in
//!   `sync_state`; the next run resumes from that page instead of restarting a
//!   large backfill from the top;
//! * `410 Gone` means the delta token expired and the scan restarts from a
//!   full sync (the same recovery `nuncio_pim::graph` uses for calendars);
//! * entries carrying `@removed` are reported rather than mapped.

use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use nuncio_core::models::{Address, Folder, Importance, MessageSummary, StoredMessage};
use nuncio_core::{repo, AppError, CoreContext, Result};
use reqwest::StatusCode;
use tracing::{debug, warn};

use super::GraphMailClient;

/// `sync_state` module under which Graph mail cursors are stored.
const SYNC_MODULE: &str = "mail";

/// Property list for delta scans.
///
/// Keep this in sync with [`map_message`]: every mapped field must be selected,
/// otherwise Graph omits it and the row silently loses data. `body` is
/// deliberately absent — bodies are fetched lazily by [`fetch_body`] — and
/// `internetMessageHeaders` is only returned when explicitly selected.
/// `size` is deliberately NOT in this list.  Outlook.com (MSA) rejects it with
/// `RequestBroker--ParseUri: Could not find a property named 'size' on type
/// 'microsoft.graph.message'`, and Graph evaluates `$select` as a unit: one
/// unknown name fails the whole request, so a single stray property takes down
/// every folder of the account.  Message size is therefore reported as 0 for
/// Graph accounts; attachment sizes come from the attachment collection, which
/// does declare `size`.
const DELTA_SELECT: &str = "id,subject,bodyPreview,from,toRecipients,ccRecipients,\
bccRecipients,replyTo,receivedDateTime,sentDateTime,isRead,isDraft,flag,hasAttachments,\
categories,conversationId,internetMessageId,internetMessageHeaders,importance";

/// Property list for the reader body fetch.
const BODY_SELECT: &str = "body,bodyPreview,internetMessageHeaders,internetMessageId,from,\
toRecipients,ccRecipients,bccRecipients,subject,receivedDateTime,sentDateTime,isRead,isDraft,\
flag,hasAttachments,categories,conversationId,parentFolderId";

/// Upper bound on pages in one delta pass. A malformed or endlessly paging
/// response must not pin a background worker forever; truncation is reported
/// so the caller never stale-deletes from an incomplete scan.
const MAX_PAGES: usize = 500;

/// Page size requested from Graph.
const PAGE_SIZE: usize = 100;

/// Result of syncing one folder's messages.
#[derive(Debug, Default)]
pub struct MessageDelta {
    /// New or updated messages, ready for `repo::messages::upsert`.
    pub messages: Vec<StoredMessage>,
    /// `@odata.deltaLink` to persist as the folder cursor. A truncated pass
    /// returns the previous cursor unchanged (the pending page is stored in
    /// `sync_state`, not here).
    pub delta_link: Option<String>,
    /// `true` only when this pass was a complete full scan that started from a
    /// cursor-less request. A resumed backfill returns `false` even on its last
    /// page: it saw only the tail of the folder, so callers must not use its id
    /// set to delete local rows.
    pub full_sync: bool,
    /// Graph ids of messages deleted on the server.
    pub removed_remote_ids: Vec<String>,
}

/// Incrementally synchronize one folder's messages through Graph.
///
/// `delta_link` is the cursor returned by the previous call (or read from
/// [`delta_cursor_key`]); `None` starts a full scan. `sync_window_days` mirrors
/// the IMAP initial-sync window and bounds the first request of a full scan
/// (`0` means "everything"). The returned cursor and the pending-page marker
/// are also written to `sync_state`, so a caller that does not persist them
/// itself still resumes correctly on the next run.
pub async fn sync_messages(
    client: &GraphMailClient,
    ctx: &CoreContext,
    account_id: &str,
    folder: &Folder,
    delta_link: Option<&str>,
    sync_window_days: u32,
) -> Result<MessageDelta> {
    // A previous pass may have stopped at `MAX_PAGES`; that page URL is the
    // only state that lets a large folder make progress. Prefer it over the
    // delta cursor: the cursor still points at the start of the backfill.
    let pending = read_pending_cursor(ctx, account_id, &folder.id).await?;
    // An empty token is equivalent to no token; treating it as a cursor would
    // request a Graph URL that cannot work.
    let stored_cursor: Option<String> = delta_link
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let mut incoming = pending
        .filter(|value| !value.trim().is_empty())
        .or_else(|| stored_cursor.clone());
    // The cursor to keep when a pass cannot produce a new deltaLink. A `410`
    // invalidates it, so it is dropped together with the restart.
    let mut fallback = stored_cursor;

    'restart: loop {
        let started_full = incoming.is_none();
        let mut messages: Vec<StoredMessage> = Vec::new();
        let mut removed_remote_ids: Vec<String> = Vec::new();
        let mut next: Option<String> = Some(delta_request_url(
            client,
            &folder.remote_path,
            incoming.as_deref(),
            sync_window_days,
        ));
        // Graph normally accepts `$top` on this delta endpoint; a tenant that
        // rejects it with `ErrorInvalidUrlQuery` gets the supported `Prefer:
        // odata.maxpagesize` fallback instead.
        let mut top_rejected = false;
        let mut first_request = true;
        let mut page_delta: Option<String> = None;
        let mut guard = 0usize;
        let mut completed = false;

        while guard < MAX_PAGES {
            // Check the budget before taking the URL so a truncated pass still
            // knows the exact page the next run must resume from.
            let Some(url) = next.take() else {
                completed = true;
                break;
            };
            guard += 1;

            let response = client
                .send_authed(ctx, account_id, |http| {
                    http.get(&url)
                        // Also used as the page-size hint when `$top` was
                        // rejected; harmless when it was accepted.
                        .header("Prefer", format!("odata.maxpagesize={PAGE_SIZE}"))
                })
                .await?;
            let status = response.status();

            if status == StatusCode::GONE {
                if incoming.is_some() {
                    debug!(
                        account = account_id,
                        folder = %folder.remote_path,
                        "Graph delta token expired; restarting with a full scan"
                    );
                    incoming = None;
                    fallback = None;
                    // A pending page from the expired scan is dead too; never
                    // retry it or the next run would fail before it starts.
                    persist_pending_cursor(ctx, account_id, &folder.id, None).await?;
                    continue 'restart;
                }
                let body = response.text().await.unwrap_or_default();
                return Err(GraphMailClient::failure(status, &body));
            }

            // Only our own initial URL carries `$top` and `$filter`; a
            // server-provided deltaLink/nextLink must never be rewritten into
            // a full-scan URL.
            if status == StatusCode::BAD_REQUEST && first_request && started_full && !top_rejected {
                let body = response.text().await.unwrap_or_default();
                if body.contains("ErrorInvalidUrlQuery") {
                    top_rejected = true;
                    debug!(
                        account = account_id,
                        folder = %folder.remote_path,
                        "Graph rejected $top on delta; retrying with odata.maxpagesize"
                    );
                    // `$top` was only ever on the initial URL; page links are
                    // opaque server URLs and must be followed verbatim. The
                    // sync-window filter stays: this is still a fresh scan.
                    next = Some(initial_delta_url(
                        client,
                        &folder.remote_path,
                        false,
                        sync_window_days,
                    ));
                    continue;
                }
                return Err(GraphMailClient::failure(status, &body));
            }

            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(GraphMailClient::failure(status, &body));
            }

            let page: GraphMessagePage = response.json().await.map_err(|error| {
                AppError::protocol(format!("invalid Graph messages JSON: {error}"))
            })?;
            first_request = false;

            for message in &page.value {
                if is_removed(message) {
                    if !message.id.trim().is_empty() {
                        removed_remote_ids.push(message.id.clone());
                    }
                    continue;
                }
                match map_message(message, account_id, &folder.id) {
                    Some(stored) => messages.push(stored),
                    None => warn!(
                        account = account_id,
                        folder = %folder.remote_path,
                        "skipping Graph message without an id"
                    ),
                }
            }

            match page.next_link {
                Some(link) => next = Some(link),
                None => {
                    page_delta = page.delta_link;
                    completed = true;
                    break;
                }
            }
        }

        removed_remote_ids.sort();
        removed_remote_ids.dedup();

        if !completed {
            // Mid-backfill: keep the delta cursor unchanged (it is a real
            // deltaLink, not this continuation) and persist the page URL so
            // the next run continues instead of re-downloading from the top.
            // The partial page set is still returned and applied: progress
            // must be visible even when a folder is larger than the budget.
            if let Some(link) = next.as_deref() {
                persist_pending_cursor(ctx, account_id, &folder.id, Some(link)).await?;
            } else {
                // Defensive: nothing to resume from, so drop any stale marker
                // and let the next run use the stored delta cursor.
                persist_pending_cursor(ctx, account_id, &folder.id, None).await?;
            }
            warn!(
                account = account_id,
                folder = %folder.remote_path,
                pages = guard,
                "Graph delta pagination limit reached; the next run resumes from the stored page"
            );
            return Ok(MessageDelta {
                messages,
                delta_link: fallback,
                // A resumed backfill saw only the tail of the folder, so its
                // id set is not exhaustive and must not drive stale-row
                // deletion in the caller.
                full_sync: false,
                removed_remote_ids,
            });
        }

        let resolved = resolve_delta_link(fallback.as_deref(), page_delta);
        persist_delta_cursor(ctx, account_id, &folder.id, resolved.as_deref()).await?;
        // The scan reached a deltaLink; the page marker is no longer a valid
        // starting point and a stale one would shadow the new cursor.
        persist_pending_cursor(ctx, account_id, &folder.id, None).await?;

        debug!(
            account = account_id,
            folder = %folder.remote_path,
            messages = messages.len(),
            removed = removed_remote_ids.len(),
            full_sync = started_full,
            "Graph message delta applied"
        );

        return Ok(MessageDelta {
            messages,
            delta_link: resolved,
            full_sync: started_full,
            removed_remote_ids,
        });
    }
}

/// Fetch one message body and convert it to the HTML the reader renders.
///
/// Graph returns either HTML (pass through) or plain text (wrapped with the
/// same `nuncio-plain` helper the compose/reading path uses, so both protocols
/// render text bodies identically).
pub async fn fetch_body(
    client: &GraphMailClient,
    ctx: &CoreContext,
    account_id: &str,
    remote_id: &str,
) -> Result<String> {
    let url = client.url(&format!(
        "/me/messages/{}?%24select={}",
        GraphMailClient::encode(remote_id),
        GraphMailClient::encode(BODY_SELECT)
    ));
    let message: GraphMessageBody = client.get(ctx, account_id, &url).await?;
    let (content_type, content) = match &message.body {
        Some(body) => (body.content_type.as_deref(), body.content.as_deref()),
        None => (None, None),
    };
    Ok(body_to_html(
        content_type,
        content,
        message.body_preview.as_deref(),
    ))
}

/// `sync_state` key under which the delta cursor of `folder_id` is stored.
///
/// Callers that want to drive [`sync_messages`] themselves read this key with
/// `repo::sync_state::get(db, account_id, "mail", &delta_cursor_key(folder_id))`
/// and pass the value as `delta_link`.
pub fn delta_cursor_key(folder_id: &str) -> String {
    format!("graph_delta:{folder_id}")
}

/* ────────────────────────────── URL building ───────────────────────────── */

/// Request URL for the first page of a pass.
///
/// A full scan starts from our own delta URL and therefore carries the sync
/// window. A stored `@odata.deltaLink`/`@odata.nextLink` is a server-issued
/// continuation and must be followed verbatim: Graph rejects `$filter` on a
/// delta continuation URL because the token already encodes the original
/// query, so appending one would break every incremental sync.
fn delta_request_url(
    client: &GraphMailClient,
    folder_id: &str,
    resume: Option<&str>,
    sync_window_days: u32,
) -> String {
    match resume {
        Some(link) => link.to_string(),
        None => initial_delta_url(client, folder_id, true, sync_window_days),
    }
}

/// Build the initial delta URL. Split out so the `$select`/`$filter`/`$top`
/// shape is unit-testable without a network.
fn initial_delta_url(
    client: &GraphMailClient,
    folder_id: &str,
    with_top: bool,
    sync_window_days: u32,
) -> String {
    let path = format!(
        "/me/mailFolders/{}/messages/delta?%24select={}",
        GraphMailClient::encode(folder_id),
        GraphMailClient::encode(DELTA_SELECT)
    );
    let mut url = client.url(&path);
    if let Some(filter) = sync_window_filter(sync_window_days, Utc::now()) {
        url.push_str("&%24filter=");
        url.push_str(&GraphMailClient::encode(&filter));
    }
    if with_top {
        url.push_str(&format!("&%24top={PAGE_SIZE}"));
    }
    url
}

/// OData `$filter` clause bounding a full scan to the account's sync window.
///
/// Mirrors `SINCE <date>` on the IMAP initial sync: without it the first Graph
/// scan of a large mailbox downloads the entire history. `0` means no window,
/// exactly like the IMAP `ALL` criteria.
fn sync_window_filter(window_days: u32, now: DateTime<Utc>) -> Option<String> {
    if window_days == 0 {
        return None;
    }
    let since = now - ChronoDuration::days(i64::from(window_days));
    Some(format!(
        "receivedDateTime ge {}",
        since.to_rfc3339_opts(SecondsFormat::Secs, true)
    ))
}

/* ─────────────────────────────── mapping ───────────────────────────────── */

/// Map one Graph message onto the local row model.
///
/// Returns `None` for entries Graph should never omit but occasionally does
/// (an empty id cannot key a row).
fn map_message(message: &GraphMessage, account_id: &str, folder_id: &str) -> Option<StoredMessage> {
    let id = message.id.trim();
    if id.is_empty() {
        return None;
    }

    let date = message
        .received_date_time
        .as_deref()
        .and_then(parse_graph_datetime)
        .or_else(|| {
            message
                .sent_date_time
                .as_deref()
                .and_then(parse_graph_datetime)
        })
        .unwrap_or_else(nuncio_core::models::now);

    let subject = message.subject.clone().unwrap_or_default();
    let headers = message.internet_message_headers.as_deref().unwrap_or(&[]);
    // Graph returns `internetMessageId` exactly as the server stored it, i.e.
    // wrapped in angle brackets (`<abc@example.com>`). Every other writer of
    // `message_id_header` stores the bare id: the IMAP/POP3 parse path strips
    // the brackets and the composer's draft projection uses
    // `nuncio-draft-<uuid>@nuncio.local`. The sync reuses an existing row by
    // `WHERE message_id_header = ?`, so an unnormalized value can never match
    // -- the APPEND echo of a draft would become a second row and
    // `draft_id_from_header` would always return `None`. Normalize here so all
    // providers agree on one representation.
    let message_id_header = message
        .internet_message_id
        .as_deref()
        .map(nuncio_mime::normalize_message_id)
        .filter(|value| !value.is_empty());
    // In-Reply-To is a real RFC 5322 header, so it carries brackets too.
    let in_reply_to = header_value(headers, "in-reply-to")
        .map(nuncio_mime::normalize_message_id)
        .filter(|value| !value.is_empty());
    // `normalize_references` splits the whitespace-separated list, strips
    // brackets, drops empties and de-duplicates while keeping order -- exactly
    // what the stored form needs. Do not re-implement it locally.
    let references = header_value(headers, "references")
        .map(|value| nuncio_mime::threading::normalize_references(&[value.to_string()]))
        .unwrap_or_default();

    // Graph's conversationId is the direct analogue of Gmail's X-GM-THRID:
    // it groups a reply chain even when References is missing. It must not be
    // written to `gm_thrid` (that column is consumed as a Gmail-specific id),
    // but the local thread key is a plain string and can carry it.
    let thread_id = message
        .conversation_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|conversation| format!("graph:{conversation}"))
        .unwrap_or_else(|| {
            crate::util::thread_id(
                message_id_header.as_deref(),
                in_reply_to.as_deref(),
                &references,
                &subject,
                None,
            )
        });

    let mut summary = MessageSummary::new(id.to_string(), account_id, folder_id, thread_id, date);
    summary.subject = subject;
    summary.r#from = message.from.as_ref().and_then(recipient_to_address);
    summary.to = recipients_to_addresses(message.to_recipients.as_deref());
    summary.cc = recipients_to_addresses(message.cc_recipients.as_deref());
    summary.date = date;
    summary.preview =
        nuncio_core::text::trim_preview(message.body_preview.as_deref().unwrap_or_default(), 300);
    summary.seen = message.is_read.unwrap_or(false);
    summary.flagged = matches!(
        message
            .flag
            .as_ref()
            .and_then(|flag| flag.flag_status.as_deref()),
        Some("flagged") | Some("complete")
    );
    summary.is_draft = message.is_draft.unwrap_or(false);
    summary.has_attachments = message.has_attachments.unwrap_or(false);
    summary.importance = message
        .importance
        .as_deref()
        .and_then(|value| value.parse::<Importance>().ok())
        .unwrap_or(Importance::Normal);
    // Graph does not expose a message size on this resource for Outlook.com
    // accounts (see `DELTA_SELECT`), so there is nothing to map.  Attachment
    // sizes still come through the attachment collection.
    summary.size = 0;

    let mut stored = StoredMessage::new(summary);
    stored.bcc = recipients_to_addresses(message.bcc_recipients.as_deref());
    // Reply-To is a real header on the message resource; without it the reader
    // would reply to the From address and bypass e.g. a mailing list.
    stored.reply_to = recipients_to_addresses(message.reply_to.as_deref());
    stored.message_id_header = message_id_header;
    stored.in_reply_to = in_reply_to;
    stored.references = references;
    stored.labels = message.categories.clone().unwrap_or_default();
    stored.raw_headers = render_raw_headers(headers);
    // `uid`/`uidvalidity` are IMAP concepts and stay `None`. The Graph id is
    // the provider id: it keys `summary.id` (so a delta removal id can delete
    // the row directly) and is also mirrored into `gm_msgid`, the only string
    // provider-id column, for operations that need to find the message on the
    // server.
    stored.gm_msgid = Some(id.to_string());
    // Outlook's received time is the closest equivalent of IMAP INTERNALDATE.
    stored.internal_date = Some(date);
    Some(stored)
}

/// Map one raw Graph message payload through [`map_message`].
///
/// Exposed to the crate's tests so `sync::messages` can feed a Graph-shaped
/// payload (angle-bracketed ids included) through the real mapper instead of
/// re-deriving the normalized header itself, which would leave the
/// normalization untested from that side.
#[cfg(test)]
pub(crate) fn map_graph_json(
    json: serde_json::Value,
    account_id: &str,
    folder_id: &str,
) -> Option<StoredMessage> {
    let message: GraphMessage = serde_json::from_value(json).ok()?;
    map_message(&message, account_id, folder_id)
}

/// Map a Graph recipient list; blank addresses are dropped because a
/// nameless, address-less recipient cannot be rendered or replied to.
fn recipients_to_addresses(recipients: Option<&[GraphRecipient]>) -> Vec<Address> {
    recipients
        .unwrap_or_default()
        .iter()
        .filter_map(recipient_to_address)
        .collect()
}

fn recipient_to_address(recipient: &GraphRecipient) -> Option<Address> {
    let email = recipient.email_address.address.as_deref()?.trim();
    if email.is_empty() {
        return None;
    }
    let name = recipient
        .email_address
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    Some(Address {
        name,
        email: email.to_string(),
    })
}

/// Case-insensitive lookup in `internetMessageHeaders`.
fn header_value<'a>(headers: &'a [GraphHeader], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value.as_str())
}

/// Render `internetMessageHeaders` back into an RFC 822 header block, which is
/// what the reader and the unsubscribe path expect in `raw_headers`.
fn render_raw_headers(headers: &[GraphHeader]) -> Option<String> {
    let mut rendered = String::new();
    for header in headers {
        let name = header.name.trim();
        if name.is_empty() {
            continue;
        }
        // Header values are single-line by definition; unfold CRLF+WSP so the
        // stored block stays a valid header section, and flatten any stray
        // newline that would otherwise split the header.
        let value = header.value.replace("\r\n", "").replace(['\r', '\n'], " ");
        rendered.push_str(name);
        rendered.push_str(": ");
        rendered.push_str(value.trim());
        rendered.push_str("\r\n");
    }
    if rendered.is_empty() {
        None
    } else {
        Some(rendered)
    }
}

/// Convert a Graph body into reader HTML.
///
/// HTML passes through untouched; text is wrapped by the shared plain-text
/// helper so text messages render as paragraphs and links like every other
/// text body in the app.
fn body_to_html(
    content_type: Option<&str>,
    content: Option<&str>,
    preview: Option<&str>,
) -> String {
    if let Some(content) = content.filter(|value| !value.trim().is_empty()) {
        if content_type
            .map(|value| value.eq_ignore_ascii_case("html"))
            .unwrap_or(false)
        {
            return content.to_string();
        }
        if content_type
            .map(|value| value.eq_ignore_ascii_case("text"))
            .unwrap_or(false)
        {
            return nuncio_mime::plain_text_to_html(content);
        }
        // An unknown content type is still better shown verbatim than dropped.
        return content.to_string();
    }
    preview
        .filter(|value| !value.trim().is_empty())
        .map(nuncio_mime::plain_text_to_html)
        .unwrap_or_default()
}

/// Choose the delta cursor to persist after a completed pass.
///
/// A completed pass normally stores the fresh `@odata.deltaLink`. A response
/// that omits it keeps the cursor that was valid before the pass, so changes
/// are re-read instead of skipped. Truncation never reaches this helper: it
/// keeps the cursor untouched and stores a pending page instead.
fn resolve_delta_link(fallback: Option<&str>, page_delta: Option<String>) -> Option<String> {
    page_delta.or_else(|| fallback.map(str::to_string))
}

/// Persist (or clear) the folder cursor. A full sync that produced no delta
/// link clears a stale cursor so the next run cannot resume from a token the
/// server no longer honours.
async fn persist_delta_cursor(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    link: Option<&str>,
) -> Result<()> {
    let account_id = account_id.to_string();
    let key = delta_cursor_key(folder_id);
    match link {
        Some(link) => {
            let value = link.to_string();
            ctx.db
                .write(move |db| repo::sync_state::set(db, &account_id, SYNC_MODULE, &key, &value))
                .await
        }
        None => {
            ctx.db
                .write(move |db| repo::sync_state::delete_key(db, &account_id, SYNC_MODULE, &key))
                .await
        }
    }
}

/// `sync_state` key for the page a truncated backfill stopped at.
///
/// Kept separate from [`delta_cursor_key`] because an `@odata.nextLink` is a
/// pagination continuation, not a change cursor: storing it as the cursor
/// would make `sync_messages` resume an expired scan as if it were incremental.
fn pending_cursor_key(folder_id: &str) -> String {
    format!("graph_pending:{folder_id}")
}

async fn read_pending_cursor(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
) -> Result<Option<String>> {
    let account_id = account_id.to_string();
    let key = pending_cursor_key(folder_id);
    ctx.db
        .read(move |db| repo::sync_state::get(db, &account_id, SYNC_MODULE, &key))
        .await
}

/// Persist (or clear with `None`) the page a truncated backfill stopped at.
async fn persist_pending_cursor(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    link: Option<&str>,
) -> Result<()> {
    let account_id = account_id.to_string();
    let key = pending_cursor_key(folder_id);
    match link {
        Some(link) => {
            let value = link.to_string();
            ctx.db
                .write(move |db| repo::sync_state::set(db, &account_id, SYNC_MODULE, &key, &value))
                .await
        }
        None => {
            ctx.db
                .write(move |db| repo::sync_state::delete_key(db, &account_id, SYNC_MODULE, &key))
                .await
        }
    }
}

fn parse_graph_datetime(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|date| date.with_timezone(&Utc))
}

/// `true` for delta entries describing a removal (`"@removed"`).
fn is_removed(message: &GraphMessage) -> bool {
    message.removed.is_some()
}

/* ────────────────────────────── Graph DTOs ─────────────────────────────── */

/// One page of a message delta response.
#[derive(Debug, Clone, serde::Deserialize)]
struct GraphMessagePage {
    #[serde(default)]
    value: Vec<GraphMessage>,
    #[serde(rename = "@odata.nextLink", default)]
    next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink", default)]
    delta_link: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphMessage {
    #[serde(default)]
    id: String,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body_preview: Option<String>,
    #[serde(default)]
    from: Option<GraphRecipient>,
    #[serde(default)]
    to_recipients: Option<Vec<GraphRecipient>>,
    #[serde(default)]
    cc_recipients: Option<Vec<GraphRecipient>>,
    #[serde(default)]
    bcc_recipients: Option<Vec<GraphRecipient>>,
    /// Becomes `replyTo` under `rename_all = "camelCase"`.
    #[serde(default)]
    reply_to: Option<Vec<GraphRecipient>>,
    #[serde(default)]
    received_date_time: Option<String>,
    #[serde(default)]
    sent_date_time: Option<String>,
    #[serde(default)]
    is_read: Option<bool>,
    #[serde(default)]
    is_draft: Option<bool>,
    #[serde(default)]
    flag: Option<GraphFlag>,
    #[serde(default)]
    has_attachments: Option<bool>,
    #[serde(default)]
    categories: Option<Vec<String>>,
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    internet_message_id: Option<String>,
    #[serde(default)]
    internet_message_headers: Option<Vec<GraphHeader>>,
    #[serde(default)]
    importance: Option<String>,
    /// Present only for delta entries describing a deletion.
    #[serde(rename = "@removed", default)]
    removed: Option<serde_json::Value>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphRecipient {
    #[serde(default)]
    email_address: GraphEmailAddress,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct GraphEmailAddress {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    address: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphFlag {
    #[serde(default)]
    flag_status: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct GraphHeader {
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: String,
}

/// Body response for [`fetch_body`]. The select list carries more fields than
/// the reader needs; only the body is deserialized and the rest is ignored by
/// serde.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphMessageBody {
    #[serde(default)]
    body: Option<GraphBodyContent>,
    #[serde(default)]
    body_preview: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphBodyContent {
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_message(json: serde_json::Value) -> GraphMessage {
        serde_json::from_value(json).expect("representative Graph message")
    }

    fn representative() -> GraphMessage {
        parse_message(serde_json::json!({
            "id": "AAMkAD/abc== +x",
            "subject": "Quarterly report",
            "bodyPreview": "Hello team, please review the attached numbers.",
            "from": { "emailAddress": { "name": "Alice", "address": "alice@example.com" } },
            "toRecipients": [
                { "emailAddress": { "name": "", "address": "me@example.com" } }
            ],
            "ccRecipients": [
                { "emailAddress": { "address": "bob@example.com" } }
            ],
            "bccRecipients": [
                { "emailAddress": { "name": "Carol", "address": "carol@example.com" } }
            ],
            "replyTo": [
                { "emailAddress": { "name": "Support", "address": "support@example.com" } }
            ],
            "receivedDateTime": "2026-11-16T08:09:10Z",
            "sentDateTime": "2026-11-16T08:09:09Z",
            "isRead": false,
            "isDraft": false,
            "flag": { "flagStatus": "flagged" },
            "hasAttachments": true,
            "categories": ["Work", "Urgent"],
            "conversationId": "conv-1",
            "internetMessageId": "<abc@example.com>",
            "internetMessageHeaders": [
                { "name": "In-Reply-To", "value": "<root@example.com>" },
                { "name": "References", "value": "<root@example.com> <mid@example.com>" }
            ],
            "importance": "high",
            "parentFolderId": "inbox-id"
        }))
    }

    #[test]
    fn maps_a_representative_message() {
        let stored = map_message(&representative(), "acc", "fld").expect("mapped");
        let summary = &stored.summary;

        assert_eq!(summary.id, "AAMkAD/abc== +x");
        assert_eq!(summary.account_id, "acc");
        assert_eq!(summary.folder_id, "fld");
        assert_eq!(summary.subject, "Quarterly report");
        assert_eq!(
            summary.preview,
            "Hello team, please review the attached numbers."
        );
        assert!(!summary.seen);
        assert!(summary.flagged);
        assert!(!summary.is_draft);
        assert!(summary.has_attachments);
        assert_eq!(summary.importance, Importance::High);
        // Graph reports no message size for Outlook.com accounts; see
        // `DELTA_SELECT`.  Nothing in the UI reads it, so 0 is the honest value.
        assert_eq!(summary.size, 0);
        assert_eq!(summary.thread_id, "graph:conv-1");
        assert_eq!(
            summary.r#from,
            Some(Address::with_name("Alice", "alice@example.com"))
        );
        assert_eq!(summary.to, vec![Address::new("me@example.com")]);
        assert_eq!(summary.cc, vec![Address::new("bob@example.com")]);
        assert_eq!(
            stored.bcc,
            vec![Address::with_name("Carol", "carol@example.com")]
        );
        assert_eq!(
            stored.reply_to,
            vec![Address::with_name("Support", "support@example.com")]
        );
        assert_eq!(stored.labels, vec!["Work", "Urgent"]);
        // `message_id_header`/`in_reply_to`/`references` are stored bare: the
        // IMAP parse path strips the brackets too, so the two protocols must
        // agree for the message-id based row reuse to work.
        assert_eq!(stored.message_id_header.as_deref(), Some("abc@example.com"));
        assert_eq!(stored.in_reply_to.as_deref(), Some("root@example.com"));
        assert_eq!(
            stored.references,
            vec!["root@example.com", "mid@example.com"]
        );
        // `raw_headers` is the verbatim server header block, brackets included:
        // it is shown to the user / fed to the unsubscribe path, not compared.
        assert_eq!(
            stored.raw_headers.as_deref(),
            Some("In-Reply-To: <root@example.com>\r\nReferences: <root@example.com> <mid@example.com>\r\n")
        );
        assert_eq!(stored.internal_date, Some(summary.date));

        // IMAP-only columns stay empty; the Graph id is the provider id.
        assert!(stored.uid.is_none());
        assert!(stored.uidvalidity.is_none());
        assert!(stored.gm_thrid.is_none());
        assert_eq!(stored.gm_msgid.as_deref(), Some("AAMkAD/abc== +x"));
    }

    #[test]
    fn message_ids_are_normalized_without_angle_brackets() {
        // Graph hands out `<abc@example.com>` while every other writer of
        // `message_id_header` stores the bare id. Without the normalization
        // here the draft row reuse query never matches and a re-APPENDed draft
        // ends up as two rows.
        let stored = map_message(&representative(), "acc", "fld").expect("mapped");
        assert_eq!(stored.message_id_header.as_deref(), Some("abc@example.com"));
        assert_eq!(stored.in_reply_to.as_deref(), Some("root@example.com"));
        assert_eq!(
            stored.references,
            vec!["root@example.com", "mid@example.com"]
        );

        // References is whitespace separated, may repeat an id and may mix
        // bracketed and bare forms; the shared helper splits, normalizes and
        // de-duplicates in one pass.
        let messy = parse_message(serde_json::json!({
            "id": "messy",
            "internetMessageId": "  <own@example.com>  ",
            "internetMessageHeaders": [
                { "name": "References", "value": "<a@example.com> b@example.com  <a@example.com> <c@example.com>" },
                { "name": "In-Reply-To", "value": "<b@example.com>" }
            ]
        }));
        let stored = map_message(&messy, "acc", "fld").expect("mapped");
        assert_eq!(stored.message_id_header.as_deref(), Some("own@example.com"));
        assert_eq!(stored.in_reply_to.as_deref(), Some("b@example.com"));
        assert_eq!(
            stored.references,
            vec!["a@example.com", "b@example.com", "c@example.com"]
        );

        // An empty id must not produce an empty header row (which would match
        // every other blank row in the reuse lookup).
        let blank = parse_message(serde_json::json!({
            "id": "blank",
            "internetMessageId": "  ",
            "internetMessageHeaders": [{ "name": "In-Reply-To", "value": "<>" }]
        }));
        let stored = map_message(&blank, "acc", "fld").expect("mapped");
        assert_eq!(stored.message_id_header, None);
        assert_eq!(stored.in_reply_to, None);
        assert!(stored.references.is_empty());
    }

    #[test]
    fn maps_read_flagged_and_draft_variants() {
        let read = parse_message(serde_json::json!({
            "id": "read",
            "isRead": true,
            "isDraft": true,
            "flag": { "flagStatus": "complete" },
            "importance": "low"
        }));
        let stored = map_message(&read, "acc", "fld").expect("mapped");
        assert!(stored.summary.seen);
        assert!(stored.summary.is_draft);
        assert!(stored.summary.flagged);
        assert_eq!(stored.summary.importance, Importance::Low);
        assert!(stored.labels.is_empty());
        assert!(stored.raw_headers.is_none());
        // A message without Reply-To maps to an empty list, not to a bogus
        // address derived from From.
        assert!(stored.reply_to.is_empty());

        let unflagged = parse_message(serde_json::json!({
            "id": "plain",
            "flag": { "flagStatus": "notFlagged" }
        }));
        let stored = map_message(&unflagged, "acc", "fld").expect("mapped");
        assert!(!stored.summary.flagged);
        assert_eq!(stored.summary.importance, Importance::Normal);
    }

    #[test]
    fn detects_removed_entries() {
        let removed = parse_message(serde_json::json!({
            "id": "gone",
            "@removed": { "reason": "deleted" }
        }));
        assert!(is_removed(&removed));
        let alive = parse_message(serde_json::json!({ "id": "alive" }));
        assert!(!is_removed(&alive));
    }

    #[test]
    fn empty_ids_are_not_mapped() {
        let blank = parse_message(serde_json::json!({ "id": "   " }));
        assert!(map_message(&blank, "acc", "fld").is_none());
    }

    #[test]
    fn delta_urls_encode_folder_and_select() {
        let client = GraphMailClient::new();
        let url = initial_delta_url(&client, "Inbox/Sub", true, 0);
        assert!(url.starts_with(
            "https://graph.microsoft.com/v1.0/me/mailFolders/Inbox%2FSub/messages/delta?"
        ));
        assert!(url.contains("%24select="));
        assert!(url.contains("bodyPreview"));
        assert!(url.contains("internetMessageHeaders"));
        assert!(url.contains("replyTo"));
        assert!(url.ends_with("&%24top=100"));

        let fallback = initial_delta_url(&client, "Inbox", false, 0);
        assert!(!fallback.contains("%24top"));
    }

    #[test]
    fn first_request_carries_the_sync_window_but_a_delta_link_does_not() {
        let client = GraphMailClient::new();

        // A full scan is bounded like the IMAP SINCE search.
        let first = delta_request_url(&client, "Inbox", None, 90);
        assert!(first.contains("%24filter=receivedDateTime%20ge%20"));
        // 0 days means "no window", matching IMAP's ALL criteria.
        let unbounded = delta_request_url(&client, "Inbox", None, 0);
        assert!(!unbounded.contains("%24filter"));

        // A server-issued continuation is followed verbatim: a filter appended
        // to a delta token breaks the query (the token encodes it already).
        let delta =
            "https://graph.microsoft.com/v1.0/me/mailFolders/Inbox/messages/delta?$deltatoken=abc";
        assert_eq!(delta_request_url(&client, "Inbox", Some(delta), 90), delta);
        assert!(!delta_request_url(&client, "Inbox", Some(delta), 90).contains("filter"));
    }

    #[test]
    fn sync_window_filter_formats_an_iso8601_instant() {
        let now = DateTime::parse_from_rfc3339("2026-11-16T00:00:00Z")
            .expect("valid instant")
            .with_timezone(&Utc);
        assert_eq!(
            sync_window_filter(30, now).as_deref(),
            Some("receivedDateTime ge 2026-10-17T00:00:00Z")
        );
        assert_eq!(sync_window_filter(0, now), None);
    }

    #[test]
    fn delta_cursor_prefers_a_fresh_link_and_keeps_the_old_one_otherwise() {
        // Completed: prefer the fresh delta link.
        assert_eq!(
            resolve_delta_link(None, Some("new".into())),
            Some("new".to_string())
        );
        // Completed without a link: keep the previous cursor so changes are
        // re-read instead of skipped.
        assert_eq!(
            resolve_delta_link(Some("old"), None),
            Some("old".to_string())
        );
        assert_eq!(resolve_delta_link(None, None), None);
    }

    #[test]
    fn bodies_pass_html_and_wrap_text() {
        assert_eq!(
            body_to_html(Some("html"), Some("<p>Hi</p>"), None),
            "<p>Hi</p>"
        );
        let text = body_to_html(Some("text"), Some("a < b\nc"), None);
        assert!(text.starts_with("<div class=\"nuncio-plain\">"));
        assert!(text.contains("a &lt; b<br>c"));
        // Missing body: fall back to the preview as plain text.
        let preview = body_to_html(None, None, Some("preview <text>"));
        assert!(preview.contains("preview &lt;text&gt;"));
        assert!(body_to_html(None, None, None).is_empty());
    }

    #[test]
    fn header_lookup_is_case_insensitive_and_strips_folding() {
        let headers = vec![
            GraphHeader {
                name: "REFERENCES".into(),
                value: "<a@x> <b@x>".into(),
            },
            GraphHeader {
                name: "X-Folded".into(),
                value: "line1\r\n line2".into(),
            },
        ];
        assert_eq!(header_value(&headers, "references"), Some("<a@x> <b@x>"));
        assert_eq!(
            render_raw_headers(&headers).as_deref(),
            Some("REFERENCES: <a@x> <b@x>\r\nX-Folded: line1 line2\r\n")
        );
    }
}
