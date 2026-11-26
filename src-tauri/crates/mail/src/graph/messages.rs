//! Microsoft Graph message synchronization for one folder.
//!
//! Microsoft personal accounts (MSA) are refused an IMAP session by Exchange
//! ("User is authenticated but not connected.") even with a valid token, while
//! Graph serves the same mailbox over HTTPS. This module therefore mirrors the
//! IMAP metadata path in [`crate::sync::messages`] on top of Graph's
//! change-tracking delta endpoint:
//!
//! * the first sync without a cursor does a full delta scan; the account's
//!   sync window is applied **locally** instead of as a `$filter`. Graph caps
//!   a `$filter`ed `messages/delta` query at 5,000 messages (and can still
//!   return a `@odata.deltaLink` after that cap), so a server-side window can
//!   silently skip older changes forever. Without the filter the whole folder
//!   is paged, which is the only way to see every change; out-of-window
//!   messages are then dropped before they reach the local store, mirroring
//!   the IMAP `SINCE` search. That drop only applies to new imports: an
//!   already-cached message keeps receiving read/flag updates, because history
//!   loaded with "load older mail" must stay in sync;
//! * later syncs resume from the persisted `@odata.deltaLink`, so only changes
//!   are transferred;
//! * a pass truncated by [`MAX_PAGES`] reports its `@odata.nextLink` so the
//!   caller can persist it in `sync_state`; the next run resumes from that page
//!   instead of restarting a large backfill from the top. The cursor and the
//!   pending page are handed back with the messages and must be written in the
//!   **same transaction** as those messages: advancing a cursor whose rows did
//!   not commit would make the retry skip exactly those changes;
//! * a pass only completes when the final page carries `@odata.deltaLink`.
//!   `@odata.nextLink` is always followed, even when a page is empty; a
//!   response with neither link is treated as truncation: the call fails and
//!   the cursor is left untouched, so an incomplete scan is never mistaken for
//!   "no changes";
//! * `410 Gone` means the delta token expired and the scan restarts from a
//!   full sync (the same recovery `nuncio_pim::graph` uses for calendars);
//! * entries carrying `@removed` are reported rather than mapped.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use nuncio_core::models::{Address, Folder, Importance, MessageSummary, StoredMessage};
use nuncio_core::{repo, AppError, CoreContext, Result};
use reqwest::StatusCode;
use tracing::{debug, warn};

use super::GraphMailClient;
use crate::ops::{
    ArchiveOp, DeleteOp, MarkJunkOp, MoveOp, OpMessageOutcome, OpMessageState, KIND_ARCHIVE,
    KIND_DELETE, KIND_MARK_JUNK, KIND_MOVE,
};

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
/// response must not pin a background worker forever. Reaching it is reported
/// as a resumable truncation (the pending page is returned for the caller to
/// store with the messages), so the caller never stale-deletes from an
/// incomplete scan.
const MAX_PAGES: usize = 500;

/// Page size requested from Graph.
const PAGE_SIZE: usize = 100;

/// Pagination marker a delta pass wants the caller to persist.
///
/// It is carried out of [`sync_messages`] instead of being written there so
/// the caller can commit it in the same database transaction as the messages
/// the pass returned. Any other ordering lets a committed cursor describe rows
/// that were never stored, and the next run then skips those changes forever.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PendingCursor {
    /// The pass learned nothing about pagination; leave the stored marker as
    /// it is.
    #[default]
    Keep,
    /// Store this `@odata.nextLink`: the pass was truncated by [`MAX_PAGES`]
    /// and the next run resumes here.
    Set(String),
    /// The pass completed (or the marker belongs to an expired scan); remove
    /// the stored marker.
    Clear,
}

/// Result of syncing one folder's messages.
#[derive(Debug, Default)]
pub struct MessageDelta {
    /// New or updated messages, ready for `repo::messages::upsert`.
    pub messages: Vec<StoredMessage>,
    /// `@odata.deltaLink` to persist as the folder cursor. `None` leaves the
    /// stored cursor untouched. A pass truncated by [`MAX_PAGES`] returns the
    /// previous cursor unchanged (the pending page is returned separately). A
    /// response that ends without a deltaLink is an error, not a successful
    /// empty result.
    pub delta_link: Option<String>,
    /// Page marker to persist together with `delta_link` and `messages`.
    pub pending_cursor: PendingCursor,
    /// Incoming Graph id -> local row id for a message whose destination
    /// identity was already confirmed by a queued move but whose provider id
    /// has not yet been bound to the row. The caller must upsert onto that row
    /// instead of inserting a second copy under the new id.
    ///
    /// A removal entry for such an id is carried here too: the projection has
    /// no `gm_msgid` yet, so this confirmed identity is the only way the
    /// writer can resolve (and delete) it. Without that the removal matches
    /// nothing while the cursor still advances past the only event naming the
    /// row.
    ///
    /// Only receipts belonging to the operation that owns the row's *current*
    /// pending projection are included. Operation rows are retained after they
    /// finish, so a receipt from an earlier trip into the folder must not
    /// resolve a later projection (its destination may be reported as
    /// `@removed` when the row left on that earlier trip).
    pub adopted_local_ids: HashMap<String, String>,
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
/// the IMAP initial-sync window and bounds which **newly seen** messages are
/// stored locally (`0` means "everything"); it is deliberately not sent as a
/// `$filter` because Graph caps a filtered delta round at 5,000 messages.
/// Messages already present locally are never windowed: their read/flag
/// updates must still apply after the user loaded history older than the
/// window.
///
/// This function does **not** write the cursor or the pending page: they are
/// returned in [`MessageDelta`] and the caller must persist them with
/// [`persist_cursor_update`] inside the same database transaction that stores
/// `messages`. Keeping them in one transaction is what makes a failed write
/// replayable instead of silently skipping the unstored changes.
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
    // The sync window is enforced locally for the same reason the request no
    // longer carries `$filter`: filtering a delta query server-side silently
    // stops at 5,000 messages (see the module docs).
    let cutoff = sync_window_cutoff(sync_window_days, Utc::now());
    // The window gates *new imports* only. A message that is already cached
    // (for example because the user loaded older mail) must still receive its
    // read/flag updates even though its date predates the window; otherwise
    // those rows would stop reacting to server changes as soon as they fall
    // out of the window. This set also carries the identities a confirmed
    // optimistic move already bound to the destination folder: an out-of-window
    // moved message must still be applied, never skipped as a new import.
    // Preload once per pass rather than querying per delta entry.
    let mut cached = cached_identities(ctx, account_id, &folder.id).await?;
    'restart: loop {
        let started_full = incoming.is_none();
        let mut messages: Vec<StoredMessage> = Vec::new();
        let mut adopted_local_ids: HashMap<String, String> = HashMap::new();
        let mut removed_remote_ids: Vec<String> = Vec::new();
        let mut next: Option<String> = Some(delta_request_url(
            client,
            &folder.remote_path,
            incoming.as_deref(),
        ));
        // Graph normally accepts `$top` on this delta endpoint; a tenant that
        // rejects it with `ErrorInvalidUrlQuery` gets the supported `Prefer:
        // odata.maxpagesize` fallback instead.
        let mut top_rejected = false;
        let mut first_request = true;
        let mut page_delta: Option<String> = None;
        let mut guard = 0usize;
        let mut completed = false;
        let mut truncated = false;

        while guard < MAX_PAGES {
            // Check the budget before taking the URL so a truncated pass still
            // knows the exact page the next run must resume from.
            let Some(url) = next.take() else {
                // Defensive: the page handler below always either queues the
                // next link, records a deltaLink, or marks the pass truncated.
                truncated = true;
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
                    // retry it. The restarting pass returns its own pending
                    // intent (Clear on completion, Set/Clear when truncated),
                    // so that write replaces the dead marker atomically with
                    // the messages.
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
                    // opaque server URLs and must be followed verbatim.
                    next = Some(initial_delta_url(client, &folder.remote_path, false));
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
                    // Removals are applied even for messages outside the sync
                    // window: they are folder-level events, not dated content.
                    let remote_id = message.id.trim();
                    if !remote_id.is_empty() {
                        // A confirmed move destination can be reported as
                        // removed before any delta entry ever bound it: the
                        // projection still has no `gm_msgid`, so only the
                        // confirmed mapping names it. Carry that identity to
                        // the writer; otherwise the removal matches nothing
                        // and the cursor advances past the only evidence that
                        // can delete (or rebind) the projection.
                        if let Some(local_id) = cached.adopted_local_ids.get(remote_id) {
                            adopted_local_ids.insert(remote_id.to_string(), local_id.clone());
                        }
                        removed_remote_ids.push(remote_id.to_string());
                    }
                    continue;
                }
                // The window used to be a server `$filter`, which Graph caps
                // at 5,000 results. Drop out-of-window *new* content here
                // instead; an entry Graph did not date is kept rather than
                // dropped. A message already in the local store is an update
                // (read/flag/importance, possibly after a load-older), not a
                // new import, so the window must not discard it.
                let remote_id = message.id.trim();
                // A confirmed move destination is an update to the optimistic
                // projection already sitting in this folder, even though that
                // row no longer carries the old provider id. Never let the
                // window discard it: doing so advanced the cursor past the
                // only event that can bind the row, losing the message from
                // the destination folder.
                let adopted_local_id = cached.adopted_local_ids.get(remote_id).cloned();
                if adopted_local_id.is_none()
                    && !cached.remote_ids.contains(remote_id)
                    && !message_within_window(message, cutoff)
                {
                    continue;
                }
                match map_message(message, account_id, &folder.id) {
                    Some(stored) => {
                        cached.remote_ids.insert(remote_id.to_string());
                        if let Some(local_id) = adopted_local_id {
                            adopted_local_ids.insert(remote_id.to_string(), local_id);
                        }
                        messages.push(stored);
                    }
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
                    // A round is complete only when Graph supplies the
                    // deltaLink. Without it the response is truncated (or
                    // malformed), even if it is empty: never persist a cursor
                    // from an incomplete scan.
                    if page.delta_link.is_some() {
                        page_delta = page.delta_link;
                        completed = true;
                    } else {
                        truncated = true;
                    }
                    break;
                }
            }
        }

        removed_remote_ids.sort();
        removed_remote_ids.dedup();

        if truncated {
            // Neither a continuation URL nor a deltaLink: the response ended
            // early. Do not touch the stored cursor or the pending page, so
            // the next run retries from exactly the same place instead of
            // treating this partial pass as "no changes" and skipping ahead.
            return Err(AppError::protocol(format!(
                "Graph delta for {} ended without an @odata.deltaLink; \
                 the pass is incomplete and the cursor was not advanced",
                folder.remote_path
            )));
        }

        if !completed {
            // Mid-backfill: keep the delta cursor unchanged (it is a real
            // deltaLink, not this continuation) and hand back the page URL so
            // the caller can persist it together with the messages. The
            // partial page set is still returned and applied: progress must be
            // visible even when a folder is larger than the budget.
            let pending_cursor = match next.as_deref() {
                Some(link) => PendingCursor::Set(link.to_string()),
                // Defensive: nothing to resume from, so drop any stale marker
                // and let the next run use the stored delta cursor.
                None => PendingCursor::Clear,
            };
            warn!(
                account = account_id,
                folder = %folder.remote_path,
                pages = guard,
                "Graph delta pagination limit reached; the next run resumes from the returned page"
            );
            return Ok(MessageDelta {
                messages,
                delta_link: fallback,
                pending_cursor,
                adopted_local_ids,
                // A resumed backfill saw only the tail of the folder, so its
                // id set is not exhaustive and must not drive stale-row
                // deletion in the caller.
                full_sync: false,
                removed_remote_ids,
            });
        }

        let Some(resolved) = page_delta else {
            // `completed` is only set together with a deltaLink; reaching
            // this arm would mean the invariant broke. Keep the cursor rather
            // than fabricating a successful empty pass.
            return Err(AppError::protocol(format!(
                "Graph delta for {} completed without an @odata.deltaLink",
                folder.remote_path
            )));
        };
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
            delta_link: Some(resolved),
            // The scan reached a deltaLink; the page marker is no longer a
            // valid starting point and a stale one would shadow the new
            // cursor.
            pending_cursor: PendingCursor::Clear,
            adopted_local_ids,
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
///
/// The `v2` prefix deliberately orphans cursors written by earlier builds:
/// those tokens encode a `$filter` that Graph caps at 5,000 messages, so
/// resuming them would keep skipping changes the truncated scan never saw.
/// Ignoring them forces one unfiltered full scan, which is what repairs the
/// local store.
pub fn delta_cursor_key(folder_id: &str) -> String {
    format!("graph_delta_v2:{folder_id}")
}

/* ────────────────────────────── URL building ───────────────────────────── */

/// Request URL for the first page of a pass.
///
/// A full scan starts from our own delta URL. A stored
/// `@odata.deltaLink`/`@odata.nextLink` is a server-issued continuation and
/// must be followed verbatim: the token encodes the original query, and Graph
/// rejects query options added to it.
///
/// The request deliberately carries **no `$filter`**: Microsoft documents that
/// filtering a delta query returns only up to 5,000 messages, and that cap can
/// be signalled by a perfectly normal-looking `@odata.deltaLink`, which would
/// silently advance the cursor past unseen changes. The sync window is applied
/// locally in [`sync_messages`] instead.
fn delta_request_url(client: &GraphMailClient, folder_id: &str, resume: Option<&str>) -> String {
    match resume {
        Some(link) => link.to_string(),
        None => initial_delta_url(client, folder_id, true),
    }
}

/// Build the initial delta URL. Split out so the `$select`/`$top` shape stays
/// unit-testable without a network.
fn initial_delta_url(client: &GraphMailClient, folder_id: &str, with_top: bool) -> String {
    let path = format!(
        "/me/mailFolders/{}/messages/delta?%24select={}",
        GraphMailClient::encode(folder_id),
        GraphMailClient::encode(DELTA_SELECT)
    );
    let mut url = client.url(&path);
    if with_top {
        url.push_str(&format!("&%24top={PAGE_SIZE}"));
    }
    url
}

/// Cutoff below which a full delta scan stops storing messages locally.
///
/// This replaces the old server-side `$filter=receivedDateTime ge ...`, which
/// Graph caps at 5,000 messages. `0` means no window, exactly like the IMAP
/// `ALL` criteria. One extra day of slack absorbs clock/query skew, so a
/// message right at the boundary is kept (over-inclusion is harmless; dropping
/// it would lose a row the previous build stored).
fn sync_window_cutoff(window_days: u32, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    (window_days > 0).then(|| now - ChronoDuration::days(i64::from(window_days).saturating_add(1)))
}

/// Whether a **new** non-removal delta entry belongs in the local store.
///
/// This only gates imports. Callers must not use it to discard an entry whose
/// message is already cached: [`sync_messages`] checks the cached remote id
/// first, so read/flag updates for older history still apply.
///
/// A payload that omits `receivedDateTime` (Graph does so on some updates) is
/// kept rather than silently dropped: only a date that is definitively older
/// than the window is filtered out.
fn message_within_window(message: &GraphMessage, cutoff: Option<DateTime<Utc>>) -> bool {
    let Some(cutoff) = cutoff else {
        return true;
    };
    message
        .received_date_time
        .as_deref()
        .and_then(parse_graph_datetime)
        .map(|date| date >= cutoff)
        .unwrap_or(true)
}

/// Messages requested per "load older mail" click.
const OLDER_BATCH: usize = 100;

/// One batch of older messages for [`list_older`].
#[derive(Debug, Default)]
pub struct OlderPage {
    pub messages: Vec<StoredMessage>,
    /// `true` while Graph reports a further page below this batch.
    pub has_more: bool,
    /// Incoming Graph id -> local row id for a confirmed move destination (see
    /// [`MessageDelta::adopted_local_ids`]).
    pub adopted_local_ids: HashMap<String, String>,
}

/// Request URL for one "load older" batch: newest first, bounded above by the
/// oldest locally stored `receivedDateTime` (`le`, so equal-second neighbours
/// are never skipped; the known boundary row is simply upserted again).
fn older_url(client: &GraphMailClient, folder_id: &str, before: Option<DateTime<Utc>>) -> String {
    let mut url = client.url(&format!(
        "/me/mailFolders/{}/messages?%24select={}",
        GraphMailClient::encode(folder_id),
        GraphMailClient::encode(DELTA_SELECT)
    ));
    if let Some(before) = before {
        // Graph requires every `$orderby` property to appear first in `$filter`.
        let filter = format!(
            "receivedDateTime le {}",
            before.to_rfc3339_opts(SecondsFormat::Secs, true)
        );
        url.push_str("&%24filter=");
        url.push_str(&GraphMailClient::encode(&filter));
    }
    url.push_str("&%24orderby=");
    url.push_str(&GraphMailClient::encode("receivedDateTime desc"));
    url.push_str(&format!("&%24top={OLDER_BATCH}"));
    url
}

/// Fetch the next batch of older messages of a folder (one GET, no delta
/// state touched). The cursor is derived by the caller from the local
/// database (`before`), so it survives deletions and never goes stale like a
/// stored `$skip` continuation would.
pub async fn list_older(
    client: &GraphMailClient,
    ctx: &CoreContext,
    account_id: &str,
    folder: &Folder,
    before: Option<DateTime<Utc>>,
) -> Result<OlderPage> {
    let url = older_url(client, &folder.remote_path, before);
    let page: GraphMessagePage = client.get(ctx, account_id, &url).await?;
    // A moved message older than the local boundary can appear in this page
    // with its new Graph id while its local projection (optimistically moved
    // into the same folder) still has no provider identity. Resolve the
    // confirmed destination so the page updates that row instead of inserting
    // a second copy.
    let cached = cached_identities(ctx, account_id, &folder.id).await?;
    let mut messages = Vec::new();
    let mut adopted_local_ids = HashMap::new();
    for message in &page.value {
        if is_removed(message) {
            continue;
        }
        if let Some(stored) = map_message(message, account_id, &folder.id) {
            if let Some(local_id) = cached.adopted_local_ids.get(&stored.summary.id) {
                adopted_local_ids.insert(stored.summary.id.clone(), local_id.clone());
            }
            messages.push(stored);
        }
    }
    Ok(OlderPage {
        messages,
        has_more: page.next_link.is_some(),
        adopted_local_ids,
    })
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

    let headers = message.internet_message_headers.as_deref().unwrap_or(&[]);
    let raw_headers = render_raw_headers(headers);
    // Graph sometimes omits `subject`, `from` and the dates from a
    // messages/delta payload while still returning `internetMessageHeaders`
    // for the same message. Those headers are authoritative, so fill the gaps
    // from them instead of persisting an "unknown sender / no subject" row.
    let header_fallback = raw_headers
        .as_deref()
        .and_then(|block| nuncio_mime::parse_headers_only(block.as_bytes()).ok());

    let mut subject = message.subject.clone().unwrap_or_default();
    if subject.trim().is_empty() {
        if let Some(parsed) = &header_fallback {
            subject = parsed.subject.clone();
        }
    }
    let from = message
        .from
        .as_ref()
        .and_then(recipient_to_address)
        .or_else(|| {
            header_fallback
                .as_ref()
                .and_then(|parsed| parsed.from.clone())
        });
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
        .or_else(|| header_fallback.as_ref().and_then(|parsed| parsed.date))
        .unwrap_or_else(nuncio_core::models::now);
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
    summary.r#from = from;
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
    stored.raw_headers = raw_headers;
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
/// Repair Graph rows that were stored without a subject or sender even though
/// their raw headers carry both (see the fallback in [`map_message`]).
///
/// Runs at the end of a Graph delta pass. The query is capped so a large
/// mailbox cannot stall the sync; the next pass continues where this one
/// stopped.
pub(crate) fn heal_missing_metadata(
    db: &rusqlite::Connection,
    account_id: &str,
) -> nuncio_core::Result<usize> {
    let mut statement = db.prepare(
        "SELECT id, raw_headers FROM message
         WHERE account_id = ?1 AND raw_headers IS NOT NULL
           AND (subject IS NULL OR subject = '' OR from_json IS NULL OR from_json = '')
         LIMIT 200",
    )?;
    let rows: Vec<(String, String)> = statement
        .query_map(rusqlite::params![account_id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);

    let mut healed = 0;
    for (id, raw) in rows {
        let Ok(parsed) = nuncio_mime::parse_headers_only(raw.as_bytes()) else {
            continue;
        };
        let from_json = parsed
            .from
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| {
                nuncio_core::AppError::internal(format!("cannot encode a repaired sender: {error}"))
            })?;
        let changed = db.execute(
            "UPDATE message SET
                 subject = CASE WHEN subject IS NULL OR subject = '' THEN ?2 ELSE subject END,
                 from_json = CASE WHEN from_json IS NULL OR from_json = '' THEN ?3 ELSE from_json END,
                 updated_at = ?4
             WHERE id = ?1",
            rusqlite::params![id, parsed.subject, from_json, nuncio_core::repo::now_string()],
        )?;
        healed += changed;
    }
    if healed > 0 {
        debug!(
            account = account_id,
            healed, "repaired Graph message metadata from raw headers"
        );
    }
    Ok(healed)
}

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

/// Local identities that must not be mistaken for a new import.
///
/// `remote_ids` covers every message already stored for the account: both the
/// row id and `gm_msgid` can carry a Graph id (a draft re-APPEND reuses a row
/// by Message-ID and only mirrors the provider id into `gm_msgid`). It also
/// carries the confirmed destination id of an optimistic move, which no message
/// row carries yet, plus the ids a queued operation pinned while the folder
/// still holds the row they belong to.
///
/// `adopted_local_ids` maps such a confirmed destination id to the local row it
/// belongs to. The row cannot be found by id/`gm_msgid` because the optimistic
/// move cleared both, so the delta entry must be applied to that row directly;
/// otherwise the new id would be treated as a new message (and, outside the
/// window, dropped after the cursor advanced past it).
#[derive(Debug, Default)]
pub(crate) struct CachedIdentities {
    pub(crate) remote_ids: HashSet<String>,
    pub(crate) adopted_local_ids: HashMap<String, String>,
}

/// One queued move-style operation, reduced to the fields identity resolution
/// needs.
#[derive(Debug)]
pub(crate) struct OpScope {
    /// Queue row id. This is recorded on the projection row as the owner of
    /// its confirmed destination, so receipts can be matched against it.
    pub(crate) id: String,
    /// Explicit queue order. The newest operation that names a row for a
    /// folder owns that row's current pending projection.
    pub(crate) sequence: i64,
    /// Source folder recorded in the payload.
    pub(crate) folder_id: String,
    /// Destination folder of the operation as a whole (the payload's
    /// `targetFolderId`). `None` for operations that do not move rows.
    pub(crate) target_folder_id: Option<String>,
    /// Local rows the operation moves.
    pub(crate) message_ids: Vec<String>,
    /// Provider ids pinned when the operation was queued.
    pub(crate) remote_ids: Vec<String>,
    /// Per-message results recorded as attempts complete.
    pub(crate) outcomes: Vec<OpMessageOutcome>,
}

/// Every queued move-style operation that can contribute identity evidence.
///
/// Operation rows are retained after they settle precisely so that a sync
/// starting *after* the command landed can still attribute the confirmed
/// destination. The queue state is therefore not a filter: a `done` row with a
/// confirmed outcome is the normal case by the time the destination folder is
/// synchronized. What keeps a retained receipt from attaching to a *new*
/// projection is [`latest_owner`] plus a re-check that the named row is still
/// `target_pending` in the folder in question (done by the callers).
///
/// A malformed payload is skipped rather than failing the sync.
pub(crate) fn move_operation_scopes(
    db: &rusqlite::Connection,
    account_id: &str,
) -> Result<Vec<OpScope>> {
    let mut scopes: Vec<OpScope> = Vec::new();
    for op in repo::pending_ops::list(db, Some(account_id))? {
        let scope = match op.kind.as_str() {
            KIND_MOVE => match repo::pending_ops::payload::<MoveOp>(&op) {
                Ok(payload) => OpScope {
                    id: op.id.clone(),
                    sequence: op.sequence,
                    folder_id: payload.folder_id,
                    target_folder_id: Some(payload.target_folder_id),
                    message_ids: payload.message_ids,
                    remote_ids: payload.remote_ids,
                    outcomes: payload.outcomes,
                },
                Err(_) => continue,
            },
            KIND_ARCHIVE => match repo::pending_ops::payload::<ArchiveOp>(&op) {
                Ok(payload) => OpScope {
                    id: op.id.clone(),
                    sequence: op.sequence,
                    folder_id: payload.folder_id,
                    target_folder_id: payload.target_folder_id,
                    message_ids: payload.message_ids,
                    remote_ids: payload.remote_ids,
                    outcomes: payload.outcomes,
                },
                Err(_) => continue,
            },
            KIND_MARK_JUNK => match repo::pending_ops::payload::<MarkJunkOp>(&op) {
                Ok(payload) => OpScope {
                    id: op.id.clone(),
                    sequence: op.sequence,
                    folder_id: payload.folder_id,
                    target_folder_id: payload.target_folder_id,
                    message_ids: payload.message_ids,
                    remote_ids: payload.remote_ids,
                    outcomes: payload.outcomes,
                },
                Err(_) => continue,
            },
            KIND_DELETE => match repo::pending_ops::payload::<DeleteOp>(&op) {
                Ok(payload) => OpScope {
                    id: op.id.clone(),
                    sequence: op.sequence,
                    folder_id: payload.folder_id,
                    target_folder_id: payload.target_folder_id,
                    message_ids: payload.message_ids,
                    remote_ids: payload.remote_ids,
                    outcomes: payload.outcomes,
                },
                Err(_) => continue,
            },
            _ => continue,
        };
        scopes.push(scope);
    }
    Ok(scopes)
}

/// Map a local row to the newest operation that moved it into `folder_id`.
///
/// A row may be named by several retained operations (it can leave a folder
/// and later return). Only the newest one owns its current pending projection;
/// receipts from an earlier trip must not resolve or delete it. Queue
/// `sequence` is the explicit order, and rows are attributed only through the
/// operation that named them.
pub(crate) fn latest_owner(scopes: &[OpScope], folder_id: &str) -> HashMap<String, i64> {
    let mut latest_owner: HashMap<String, i64> = HashMap::new();
    for scope in scopes {
        let payload_targets = scope.target_folder_id.as_deref() == Some(folder_id);
        let mut named: Vec<&str> = Vec::new();
        if payload_targets {
            named.extend(scope.message_ids.iter().map(String::as_str));
        }
        for outcome in &scope.outcomes {
            if outcome.target_folder_id.as_deref() == Some(folder_id) {
                named.push(outcome.message_id.as_str());
            }
        }
        for message_id in named {
            let owner = latest_owner
                .entry(message_id.to_string())
                .or_insert(scope.sequence);
            *owner = (*owner).max(scope.sequence);
        }
    }
    latest_owner
}

/// Preload the identities of every message already known in `folder_id`.
///
/// Used by [`sync_messages`] to tell an out-of-window **update** to a cached or
/// confirmed-move message (which must be applied) from an out-of-window **new
/// import** (which stays filtered). Only the latter is discarded by the window.
///
/// Confirmed move receipts are attributed by queue order: the newest operation
/// that named a row for this folder owns its pending projection. Receipts from
/// an earlier trip (the row left and came back) are ignored, and the durable
/// `graph_remote` mapping is only trusted when that owning operation confirmed
/// exactly that destination.
pub(crate) async fn cached_identities(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
) -> Result<CachedIdentities> {
    let account_id = account_id.to_string();
    let folder_id = folder_id.to_string();
    ctx.db
        .read(move |db| {
            let mut identities = CachedIdentities::default();
            {
                let mut statement = db.prepare(
                    "SELECT id FROM message WHERE account_id = ?1
                     UNION
                     SELECT gm_msgid FROM message
                      WHERE account_id = ?1 AND gm_msgid IS NOT NULL",
                )?;
                let mut rows = statement.query(rusqlite::params![&account_id])?;
                while let Some(row) = rows.next()? {
                    identities.remote_ids.insert(row.get::<_, String>(0)?);
                }
            }

            // The projection row itself records the confirmed destination of
            // its move *and the operation that owns that identity* (the
            // executor writes both at settlement and binding clears them).
            // This is the primary source: unlike the operation payload it
            // cannot be pruned by housekeeping. Seed both the id set and the
            // adopted mapping from it before the queue is read.
            let row_identities =
                repo::messages::move_target_identities(db, &account_id, &folder_id)?;
            let mut row_target_by_message: HashMap<String, String> = HashMap::new();
            let mut row_owner_by_message: HashMap<String, (String, Option<i64>)> = HashMap::new();
            // One local row may be adopted for only one destination id. When
            // the row carries an identity, that identity owns the row
            // exclusively: a stale receipt must never be added next to it.
            let mut adopted_by_message: HashMap<String, String> = HashMap::new();
            for identity in &row_identities {
                identities.remote_ids.insert(identity.remote_id.clone());
                identities
                    .adopted_local_ids
                    .entry(identity.remote_id.clone())
                    .or_insert_with(|| identity.local_id.clone());
                adopted_by_message
                    .entry(identity.local_id.clone())
                    .or_insert_with(|| identity.remote_id.clone());
                row_target_by_message
                    .insert(identity.local_id.clone(), identity.remote_id.clone());
                if let Some(owner) = identity.owner_op_id.clone() {
                    row_owner_by_message
                        .insert(identity.local_id.clone(), (owner, identity.owner_sequence));
                }
            }
            // A row may have claimed the slot without a destination yet. The
            // claim is still authoritative: a historical receipt must not
            // endorse an alias for that row in the meantime.
            for (message_id, owner) in
                repo::messages::move_target_owners(db, &account_id, &folder_id)?
            {
                row_owner_by_message.insert(message_id, owner);
            }

            // Operation results carry the same evidence durably in the payload:
            // a confirmed move records its destination id, while `remote_ids`
            // and `attempt_remote_id` pin ids this folder's rows already had.
            let scopes = move_operation_scopes(db, &account_id)?;
            // A projection may only be resolved by the operation that owns it.
            // Operation rows are retained after they finish, so a receipt from
            // an *earlier* trip is still in `outcomes` when the same row leaves
            // and later returns. That stale destination id must never resolve
            // the new projection: the previous trip's copy can be reported as
            // `@removed`, and deleting the row here would lose the new copy
            // while the cursor advanced. Queue `sequence` remains the fallback
            // order for rows written before ownership was recorded.
            let latest_owner = latest_owner(&scopes, &folder_id);

            // The confirmed destination a scope recorded for one row in this
            // folder, if any.
            let confirmed_target = |scope: &OpScope, message_id: &str| -> Option<String> {
                scope
                    .outcomes
                    .iter()
                    .find(|outcome| {
                        outcome.message_id == message_id
                            && outcome.state == OpMessageState::Confirmed
                            && outcome.target_folder_id.as_deref() == Some(folder_id.as_str())
                    })
                    .and_then(|outcome| outcome.target_remote_id.as_deref())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            };

            // An optimistic move clears the row's uid/gm_msgid and marks it
            // `target_pending`, but the executor records the confirmed Graph id
            // under `graph_remote:{folder}:{source uid}` (its alias key) before
            // it can be bound. Resolve those bindings back to the projection so
            // the delta update lands on it instead of becoming a second row.
            //
            // That mapping carries no operation id, so it must agree with the
            // identity the row itself carries. If the row has no destination
            // yet (only a claim, or nothing), the alias is trusted only when
            // the owning operation — never a historical one — confirmed it;
            // otherwise it would keep an old copy's id alive next to the new
            // target and an `@removed` for it could delete the projection.
            {
                let mut statement = db.prepare(
                    "SELECT id FROM message
                      WHERE account_id = ?1 AND folder_id = ?2
                        AND uid IS NULL AND target_pending = 1",
                )?;
                let mut rows = statement.query(rusqlite::params![&account_id, &folder_id])?;
                while let Some(row) = rows.next()? {
                    let id: String = row.get(0)?;
                    let key = format!(
                        "graph_remote:{folder_id}:{}",
                        crate::sync::messages::synthetic_uid(&id)
                    );
                    let Some(remote) = repo::sync_state::get(db, &account_id, SYNC_MODULE, &key)?
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty())
                    else {
                        continue;
                    };
                    let accepted = if let Some(expected) = row_target_by_message.get(&id) {
                        // The row identity is authoritative: the alias may
                        // only name the same copy.
                        expected == &remote
                    } else if let Some((owner_id, _)) = row_owner_by_message.get(&id) {
                        // The owner claimed the slot but has not written its
                        // destination yet. Only that operation's own receipt
                        // may endorse the alias; a historical receipt cannot.
                        scopes
                            .iter()
                            .find(|scope| &scope.id == owner_id)
                            .and_then(|scope| confirmed_target(scope, &id))
                            .as_deref()
                            == Some(remote.as_str())
                    } else if let Some(owner) = latest_owner.get(&id) {
                        // No owner recorded (a row written before migration
                        // 0024): the newest operation that named it for this
                        // folder is the fallback authority.
                        scopes
                            .iter()
                            .find(|scope| scope.sequence == *owner)
                            .and_then(|scope| confirmed_target(scope, &id))
                            .as_deref()
                            == Some(remote.as_str())
                    } else {
                        // No operation and no row identity: the mapping is the
                        // only durable evidence, as before.
                        true
                    };
                    if !accepted {
                        debug!(
                            message = %id,
                            remote = %remote,
                            folder = %folder_id,
                            "Graph durable move mapping is not owned by the projection's current identity; ignored"
                        );
                        continue;
                    }
                    if adopted_by_message
                        .get(&id)
                        .is_some_and(|existing| existing != &remote)
                    {
                        debug!(
                            message = %id,
                            remote = %remote,
                            expected = ?adopted_by_message.get(&id),
                            folder = %folder_id,
                            "Graph durable move mapping conflicts with the row's identity; ignored"
                        );
                        continue;
                    }
                    identities.remote_ids.insert(remote.clone());
                    identities
                        .adopted_local_ids
                        .entry(remote.clone())
                        .or_insert(id.clone());
                    adopted_by_message.entry(id).or_insert(remote);
                }
            }

            // A pinned id only proves a cached copy when the folder still holds
            // the row it belongs to. Without that anchor an id the operation
            // names (but which now lives elsewhere) would let an out-of-window
            // entry through as a phantom duplicate.
            let known_in_folder = |remote: &str| -> Result<bool> {
                Ok(db.query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM message
                          WHERE account_id = ?1 AND folder_id = ?2
                            AND (id = ?3 OR gm_msgid = ?3))",
                    rusqlite::params![&account_id, &folder_id, remote],
                    |row| row.get(0),
                )?)
            };

            for scope in &scopes {
                if scope.folder_id == folder_id {
                    for remote in &scope.remote_ids {
                        let remote = remote.trim();
                        if !remote.is_empty() && known_in_folder(remote)? {
                            identities.remote_ids.insert(remote.to_string());
                        }
                    }
                }
                for outcome in &scope.outcomes {
                    if scope.folder_id == folder_id {
                        if let Some(attempt) = outcome
                            .attempt_remote_id
                            .as_deref()
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                        {
                            if known_in_folder(attempt)? {
                                identities.remote_ids.insert(attempt.to_string());
                            }
                        }
                    }
                    if outcome.state != OpMessageState::Confirmed
                        || outcome.target_folder_id.as_deref() != Some(folder_id.as_str())
                    {
                        continue;
                    }
                    let Some(remote) = outcome
                        .target_remote_id
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                    else {
                        continue;
                    };
                    // The receipt must belong to the operation that owns the
                    // row's *current* projection. A retained receipt from an
                    // earlier trip into this folder names a copy that already
                    // left; applying it to the new projection would delete the
                    // message when the old destination is reported as
                    // `@removed`.
                    //
                    // The owner recorded on the row is authoritative. Without
                    // one (a row written before migration 0024) fall back to
                    // the newest operation that named it, but only while the
                    // row carries no conflicting identity of its own.
                    if let Some((owner_id, _)) = row_owner_by_message.get(&outcome.message_id) {
                        if owner_id != &scope.id {
                            debug!(
                                message = %outcome.message_id,
                                remote = %remote,
                                sequence = scope.sequence,
                                owner = %owner_id,
                                folder = %folder_id,
                                "Graph confirmed move receipt is not the row identity's owner; not applied"
                            );
                            continue;
                        }
                    } else if !row_target_by_message.contains_key(&outcome.message_id)
                        && latest_owner.get(&outcome.message_id).copied() != Some(scope.sequence)
                    {
                        debug!(
                            message = %outcome.message_id,
                            remote = %remote,
                            sequence = scope.sequence,
                            owner = ?latest_owner.get(&outcome.message_id),
                            folder = %folder_id,
                            "Graph confirmed move receipt belongs to an earlier trip; not applied to the current projection"
                        );
                        continue;
                    }
                    // Bind only while the projection is still waiting for its
                    // target identity in this folder. A row already rebound to
                    // another message (or moved away) is not this operation's
                    // destination any more.
                    let still_pending: bool = db.query_row(
                        "SELECT EXISTS(
                            SELECT 1 FROM message
                             WHERE id = ?1 AND account_id = ?2 AND folder_id = ?3
                               AND uid IS NULL AND target_pending = 1)",
                        rusqlite::params![outcome.message_id, &account_id, &folder_id],
                        |row| row.get(0),
                    )?;
                    if still_pending {
                        // A row-level receipt belongs to the row's *current*
                        // projection. When it disagrees, this operation result
                        // describes an earlier trip and must not add another
                        // destination for the same row.
                        if let Some(expected) = row_target_by_message.get(&outcome.message_id) {
                            if expected != remote {
                                continue;
                            }
                        }
                        // Never stack a second destination for one local row.
                        if adopted_by_message
                            .get(&outcome.message_id)
                            .is_some_and(|existing| existing != remote)
                        {
                            continue;
                        }
                        identities.remote_ids.insert(remote.to_string());
                        identities
                            .adopted_local_ids
                            .entry(remote.to_string())
                            .or_insert_with(|| outcome.message_id.clone());
                        adopted_by_message
                            .entry(outcome.message_id.clone())
                            .or_insert_with(|| remote.to_string());
                    }
                }
            }
            Ok(identities)
        })
        .await
}

/// Persist the delta cursor and pending-page marker of one Graph pass.
///
/// Must be called inside the same transaction that stores that pass's
/// messages. `delta_link: None` leaves the stored cursor untouched (a
/// truncated backfill or a load-older page carries no new change cursor).
/// Committing either cursor before the rows it describes would make a retry
/// resume past changes that were never stored.
pub(crate) fn persist_cursor_update(
    db: &rusqlite::Connection,
    account_id: &str,
    folder_id: &str,
    delta_link: Option<&str>,
    pending: &PendingCursor,
) -> Result<()> {
    if let Some(link) = delta_link {
        repo::sync_state::set(
            db,
            account_id,
            SYNC_MODULE,
            &delta_cursor_key(folder_id),
            link,
        )?;
    }
    match pending {
        PendingCursor::Keep => {}
        PendingCursor::Set(link) => repo::sync_state::set(
            db,
            account_id,
            SYNC_MODULE,
            &pending_cursor_key(folder_id),
            link,
        )?,
        PendingCursor::Clear => repo::sync_state::delete_key(
            db,
            account_id,
            SYNC_MODULE,
            &pending_cursor_key(folder_id),
        )?,
    }
    Ok(())
}

/// `sync_state` key for the page a truncated backfill stopped at.
///
/// Kept separate from [`delta_cursor_key`] because an `@odata.nextLink` is a
/// pagination continuation, not a change cursor: storing it as the cursor
/// would make `sync_messages` resume an expired scan as if it were incremental.
/// The `v2` prefix orphans continuations of the old `$filter`ed scan for the
/// same reason as [`delta_cursor_key`].
fn pending_cursor_key(folder_id: &str) -> String {
    format!("graph_pending_v2:{folder_id}")
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
        let url = initial_delta_url(&client, "Inbox/Sub", true);
        assert!(url.starts_with(
            "https://graph.microsoft.com/v1.0/me/mailFolders/Inbox%2FSub/messages/delta?"
        ));
        assert!(url.contains("%24select="));
        assert!(url.contains("bodyPreview"));
        assert!(url.contains("internetMessageHeaders"));
        assert!(url.contains("replyTo"));
        assert!(url.ends_with("&%24top=100"));

        let fallback = initial_delta_url(&client, "Inbox", false);
        assert!(!fallback.contains("%24top"));
    }

    #[test]
    fn delta_requests_never_carry_a_filter() {
        let client = GraphMailClient::new();

        // Filtering a messages delta query caps the result at 5,000 messages
        // and can still return a deltaLink, so a filtered cursor silently
        // skips changes. The sync window must be applied locally instead.
        let first = delta_request_url(&client, "Inbox", None);
        assert!(!first.contains("%24filter"));
        assert!(!first.contains("receivedDateTime%20ge"));

        // A server-issued continuation is followed verbatim, with nothing
        // appended: the token already encodes the original query.
        let delta =
            "https://graph.microsoft.com/v1.0/me/mailFolders/Inbox/messages/delta?$deltatoken=abc";
        assert_eq!(delta_request_url(&client, "Inbox", Some(delta)), delta);
    }

    #[test]
    fn sync_window_cutoff_and_membership_bound_storage_locally() {
        let now = DateTime::parse_from_rfc3339("2026-11-16T00:00:00Z")
            .expect("valid instant")
            .with_timezone(&Utc);
        // One extra day of slack so boundary messages are kept.
        assert_eq!(
            sync_window_cutoff(30, now),
            Some(now - ChronoDuration::days(31))
        );
        assert_eq!(sync_window_cutoff(0, now), None);

        let cutoff = sync_window_cutoff(30, now);
        let recent = parse_message(serde_json::json!({
            "id": "recent",
            "receivedDateTime": "2026-11-01T00:00:00Z"
        }));
        let old = parse_message(serde_json::json!({
            "id": "old",
            "receivedDateTime": "2026-01-01T00:00:00Z"
        }));
        // A payload without a date is kept rather than silently dropped.
        let undated = parse_message(serde_json::json!({ "id": "undated" }));
        assert!(message_within_window(&recent, cutoff));
        assert!(!message_within_window(&old, cutoff));
        assert!(message_within_window(&undated, cutoff));
        // No window accepts everything.
        assert!(message_within_window(&old, None));
    }

    #[test]
    fn cursor_keys_are_versioned_to_orphan_filtered_cursors() {
        // Tokens written by earlier builds encode a `$filter`; resuming one
        // would keep the 5,000-message cap. The v2 key makes them invisible.
        assert_eq!(delta_cursor_key("fld"), "graph_delta_v2:fld");
        assert_eq!(pending_cursor_key("fld"), "graph_pending_v2:fld");
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

    #[test]
    fn delta_message_without_subject_falls_back_to_headers() {
        // Graph can omit subject/from from a delta payload while still
        // returning the headers; storing an empty row made the list show
        // "unknown sender / no subject".
        let raw = serde_json::json!({
            "id": "m1",
            "internetMessageId": "<m1@example.com>",
            "internetMessageHeaders": [
                {"name": "Subject", "value": "Weekly report"},
                {"name": "From", "value": "Boss <boss@example.com>"},
                {"name": "Date", "value": "Mon, 23 Nov 2026 08:00:00 +0000"}
            ]
        });
        let message: GraphMessage = serde_json::from_value(raw).expect("message");
        let stored = map_message(&message, "acc", "folder").expect("mapped");
        assert_eq!(stored.summary.subject, "Weekly report");
        assert_eq!(
            stored
                .summary
                .r#from
                .as_ref()
                .map(|address| address.email.as_str()),
            Some("boss@example.com")
        );
        assert_eq!(
            stored.summary.date.to_rfc3339(),
            "2026-11-23T08:00:00+00:00"
        );
    }
}
