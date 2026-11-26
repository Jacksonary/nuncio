//! POP3 sync: header-first UIDL listing, bodies fetched on demand.
//!
//! POP3 has no folders, flags or server-side search, and `UIDL` values are
//! opaque strings, so the message number from `LIST`/`UIDL` and the `Date`
//! header are the only ordering inputs. A sync therefore:
//!
//! 1. reads `UIDL` + `LIST` to learn every message on the server,
//! 2. fetches only the header block (`TOP n 0`, falling back to a size-guarded
//!    `RETR` when the server rejects TOP) of messages that are not stored yet
//!    and stores them as listable rows,
//! 3. leaves the body to [`crate::pop3::body::fetch_body`], which downloads and
//!    caches one message when it is actually opened,
//! 4. drops local rows whose UIDL disappeared from the server.
//!
//! Listing, ordering (newest first) and cursor paging are served entirely from
//! the local database; no server round trip is needed to reach older mail
//! because every UIDL is already known locally.
//!
//! `sync_window_days` is deliberately ignored here: POP3 exposes the whole
//! mailbox and has no date search, so applying the configured window would only
//! hide messages that can never be paged back into view. IMAP and Graph keep
//! honouring the setting.

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

pub(crate) const STATE_KEY: &str = "uidl_state";
pub(crate) const MODULE: &str = "pop3";
/// Upper bound for the `RETR` fallback (and for one on-demand body).
pub(crate) const MAX_MESSAGE_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Pop3State {
    /// UIDL -> local message id.
    #[serde(default)]
    pub seen: HashMap<String, String>,
    /// UIDLs the client asked the server to delete (leave-on-server = false).
    /// Their local rows stay: the body was never downloaded and no longer can
    /// be, so the entry also tells the body path not to re-download.
    #[serde(default)]
    pub deleted: Vec<String>,
}

/// Read the persisted UIDL state for one account.
pub(crate) async fn load_state(ctx: &CoreContext, account_id: &str) -> Result<Pop3State> {
    let account_id = account_id.to_string();
    ctx.db
        .read(move |db| {
            repo::sync_state::get_json::<Pop3State>(db, &account_id, MODULE, STATE_KEY)
                .map(Option::unwrap_or_default)
        })
        .await
}

/// Download new messages' headers and reconcile deletions using UIDL.
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
            // Some POP3 servers implement UIDL but not LIST; sizes are only an
            // optimization, so keep going.
            debug!(%error, "POP3 LIST failed; continuing without sizes");
            HashMap::new()
        }
    };
    debug!(account = %account.id, messages = remote.len(), "POP3 UIDL listing");

    let folder = ensure_inbox(ctx, account).await?;

    let account_id = account.id.clone();
    let mut state = load_state(ctx, &account_id).await?;
    let was_initial = state.seen.is_empty();

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
    // Some servers reject TOP entirely; probe once per session instead of
    // paying a failed round trip per message.
    let mut top = TopProbe::default();
    for (number, uidl) in &remote {
        if state.seen.contains_key(uidl) {
            continue;
        }
        let size = sizes.get(number).copied().unwrap_or(0);
        if size > MAX_MESSAGE_BYTES {
            warn!(uidl, size, "POP3 message too large; skipping");
            continue;
        }
        let fetched = match fetch_headers(&mut client, *number, &mut top).await {
            Ok(fetched) => fetched,
            Err(error) => {
                // A single unreadable message must not abort the whole sync;
                // it is retried on the next pass.
                warn!(%error, uidl, "POP3 headers could not be fetched");
                continue;
            }
        };
        let stored = match &fetched {
            FetchedHeaders::Headers(raw) => {
                match store_headers(ctx, account, &folder, uidl, next_uid, size, raw).await {
                    // A server that answers TOP with an unusable header block (for
                    // example an empty payload) would otherwise fail on every pass.
                    // One RETR retry settles it: either the full message stores, or
                    // the message is genuinely broken.
                    Err(error) if error.code() == "protocol" => {
                        debug!(%error, uidl, "TOP headers unusable; retrying with RETR");
                        match client.retr(*number).await {
                            Ok(full) if full.len() as u64 <= MAX_MESSAGE_BYTES => {
                                store_full_message(ctx, account, &folder, uidl, next_uid, &full)
                                    .await
                            }
                            Ok(_) => Err(AppError::invalid_input(
                                "POP3 message is larger than the 100 MB download limit",
                            )),
                            Err(error) => Err(error),
                        }
                    }
                    other => other,
                }
            }
            FetchedHeaders::Full(raw) => {
                store_full_message(ctx, account, &folder, uidl, next_uid, raw).await
            }
        };
        match stored {
            Ok(summary) => {
                state.seen.insert(uidl.clone(), summary.id.clone());
                new_messages.push(summary);
                next_uid += 1;
                // Deletion keeps the semantics it has always had: as soon as
                // the message is stored locally (here: its listable header
                // row) it is removed from the server when the user asked not
                // to leave mail there. Bulk cleaning a mailbox must not wait
                // for bodies that will never be opened.
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

/// What a per-message listing fetch produced.
enum FetchedHeaders {
    /// `TOP n 0` answered: the header block, no body.
    Headers(Vec<u8>),
    /// `RETR` fallback because the server rejects TOP: the whole message.
    Full(Vec<u8>),
}

/// How many TOP rejections in a row mean "this server has no TOP". One `-ERR`
/// can be a single message that is briefly unavailable; giving up on the first
/// would turn a header-first listing into a full download of the mailbox.
const TOP_REJECTIONS_BEFORE_GIVING_UP: u32 = 3;

/// Per-session record of whether the server answers `TOP`.
#[derive(Debug)]
struct TopProbe {
    works: bool,
    consecutive_rejections: u32,
}

impl Default for TopProbe {
    fn default() -> Self {
        Self {
            works: true,
            consecutive_rejections: 0,
        }
    }
}

impl TopProbe {
    fn succeeded(&mut self) {
        self.consecutive_rejections = 0;
    }

    /// Record one rejection; true once TOP should no longer be tried.
    fn rejected(&mut self) -> bool {
        self.consecutive_rejections += 1;
        if self.consecutive_rejections >= TOP_REJECTIONS_BEFORE_GIVING_UP {
            self.works = false;
        }
        !self.works
    }
}

/// Fetch only the headers of one message, falling back to a size-guarded
/// `RETR` for that message when TOP is rejected (and for the rest of the
/// session once it has been rejected several times in a row).
async fn fetch_headers(
    client: &mut Pop3Client,
    number: u32,
    top: &mut TopProbe,
) -> Result<FetchedHeaders> {
    if top.works {
        match client.top(number, 0).await {
            Ok(raw) => {
                top.succeeded();
                return Ok(FetchedHeaders::Headers(raw));
            }
            Err(error) if error.code() == "protocol" => {
                // `-ERR` is what a server without TOP answers. Anything else
                // (auth, network) is a real failure and is propagated below.
                let gave_up = top.rejected();
                debug!(%error, number, gave_up, "POP3 TOP rejected; falling back to RETR");
            }
            Err(error) => return Err(error),
        }
    }
    let raw = client.retr(number).await?;
    if raw.len() as u64 > MAX_MESSAGE_BYTES {
        return Err(AppError::invalid_input(
            "POP3 message is larger than the 100 MB download limit",
        ));
    }
    Ok(FetchedHeaders::Full(raw))
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

/// Build the local row for one server message from its headers.
///
/// `body_path` stays `None`: the row is listable, searchable by header and
/// ordered by its `Date` header, but the body is downloaded on demand. Keeping
/// this a pure function makes the header mapping and the ordering key testable
/// without a server.
fn header_row(
    account_id: &str,
    folder_id: &str,
    uidl: &str,
    uid: i64,
    size: u64,
    headers: &nuncio_mime::ParsedHeaders,
) -> StoredMessage {
    let id = pop3_message_id(account_id, uidl);
    // POP3 exposes no internal date; the Date header is the ordering key, and
    // a message without one sorts at the top rather than disappearing.
    let date = headers.date.unwrap_or_else(Utc::now);
    let thread_id = crate::util::thread_id(
        headers.message_id.as_deref(),
        headers.in_reply_to.as_deref(),
        &headers.references,
        &headers.subject,
        None,
    );
    let mut summary = MessageSummary::new(id, account_id, folder_id, thread_id, date);
    summary.subject = headers.subject.clone();
    summary.r#from = headers.from.clone();
    summary.to = headers.to.clone();
    summary.cc = headers.cc.clone();
    summary.importance = headers.importance;
    summary.size = size;

    let mut stored = StoredMessage::new(summary);
    stored.bcc = headers.bcc.clone();
    stored.reply_to = headers.reply_to.clone();
    stored.message_id_header = headers.message_id.clone();
    stored.in_reply_to = headers.in_reply_to.clone();
    stored.references = headers.references.clone();
    stored.uid = Some(uid);
    stored.uidvalidity = None;
    stored.raw_headers = Some(headers.raw.clone());
    stored.internal_date = headers.date;
    stored
}

/// Whether a header/body write may (re)use an existing row.
///
/// POP3 rows are identified by the deterministic id derived from their UIDL, so
/// the message number (`uid`) may legitimately change between sessions and is
/// not treated as an identity here. Everything that belongs to a *newer local
/// action* is: a different folder (the user moved the row), a soft/permanent
/// delete, or an in-flight move/reconcile projection. The upsert writes
/// `folder_id`, `uid`, `target_pending` and the owner columns, so writing over
/// any of those would revert that action. Returns `true` when the row is either
/// absent or still the untouched copy this sync stored.
fn may_publish_row(
    existing: Option<&StoredMessage>,
    incoming: &StoredMessage,
    owner: Option<repo::messages::ProjectionOwner>,
    pending: bool,
) -> bool {
    let Some(existing) = existing else {
        return true;
    };
    existing.summary.account_id == incoming.summary.account_id
        && existing.summary.folder_id == incoming.summary.folder_id
        && !existing.is_deleted_local
        && owner.is_none()
        && !pending
}

/// Re-check an existing POP3 row inside the caller's write transaction.
///
/// A sync may have been running while the user moved, deleted or re-queued the
/// row; the in-flight header/body write must then be refused instead of
/// reverting that newer local state.
fn check_publish_row(db: &rusqlite::Connection, incoming: &StoredMessage) -> Result<bool> {
    let existing = repo::messages::get(db, &incoming.summary.id)?;
    if existing.is_none() {
        return Ok(true);
    }
    let owner = repo::messages::projection_owner(db, &incoming.summary.id)?;
    let pending = repo::messages::is_pending_projection(db, &incoming.summary.id)?;
    if may_publish_row(existing.as_ref(), incoming, owner, pending) {
        return Ok(true);
    }
    warn!(
        message = %incoming.summary.id,
        "POP3 sync: row changed while it was being stored; refusing to overwrite it"
    );
    Ok(false)
}

/// Store a headers-only row (the normal listing path).
async fn store_headers(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    uidl: &str,
    uid: i64,
    size: u64,
    raw_headers: &[u8],
) -> Result<MessageSummary> {
    let headers = nuncio_mime::parse_headers_only(raw_headers)?;
    let stored = header_row(&account.id, &folder.id, uidl, uid, size, &headers);
    let summary = stored.summary.clone();
    let folder_id = folder.id.clone();
    ctx.db
        .write(move |db| {
            if check_publish_row(db, &stored)? {
                repo::messages::upsert(db, &stored)?;
            }
            repo::folders::recompute_counts(db, &folder_id)
        })
        .await?;
    Ok(summary)
}

/// Store a message whose body arrived with the header fetch (TOP fallback).
async fn store_full_message(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    uidl: &str,
    uid: i64,
    raw: &[u8],
) -> Result<MessageSummary> {
    let parsed = nuncio_mime::parse(raw)?;
    let mut stored = header_row(
        &account.id,
        &folder.id,
        uidl,
        uid,
        raw.len() as u64,
        &parsed.headers,
    );
    stored.summary.preview = nuncio_core::text::trim_preview(&parsed.preview, 300);
    stored.summary.has_attachments = !parsed.attachments.is_empty();
    stored.summary.size = raw.len() as u64;

    let id = stored.summary.id.clone();
    // The cache version is bound to the provider key recorded below, so the
    // same bytes read later can be recomputed and attributed to this copy.
    let body_identity = repo::messages::body_identity_key(&stored);
    // Versioned, immutable cache file: a later body refresh writes a different
    // path instead of overwriting this one.
    let path =
        crate::sync::body::cache_body_file(ctx, &account.id, &id, body_identity.as_deref(), raw)
            .await?;
    let path_string = path.to_string_lossy().to_string();
    stored.body_path = Some(path_string.clone());
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

    let summary = stored.summary.clone();
    let preview = summary.preview.clone();
    let folder_id = folder.id.clone();
    ctx.db
        .write(move |db| {
            if check_publish_row(db, &stored)? {
                repo::messages::upsert(db, &stored)?;
                repo::messages::set_body(
                    db,
                    &id,
                    Some(&path_string),
                    Some(&body_text),
                    body_identity.as_deref(),
                )?;
                repo::messages::update_preview(db, &id, &preview)?;
                repo::attachments::replace_for_message(db, &id, &attachments)?;
            }
            repo::folders::recompute_counts(db, &folder_id)
        })
        .await?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(raw: &str) -> nuncio_mime::ParsedHeaders {
        nuncio_mime::parse_headers_only(raw.as_bytes()).expect("headers parse")
    }

    #[test]
    fn top_is_only_given_up_after_consecutive_rejections() {
        let mut probe = TopProbe::default();
        // One bad message does not switch the whole session to full downloads.
        assert!(!probe.rejected());
        assert!(probe.works);
        // A success in between resets the streak.
        probe.succeeded();
        assert!(!probe.rejected());
        assert!(!probe.rejected());
        assert!(probe.works);
        // Three in a row means the server has no TOP.
        assert!(probe.rejected());
        assert!(!probe.works);
    }

    #[test]
    fn header_row_is_listable_without_a_body() {
        let parsed = headers(
            "From: Alice <alice@example.com>\r\n\
             To: bob@example.com\r\n\
             Subject: Header only\r\n\
             Date: Mon, 01 Jan 2024 10:00:00 +0800\r\n\
             Message-ID: <m-1@example.com>\r\n\
             Content-Type: text/plain\r\n",
        );
        let stored = header_row("acc", "folder", "uid-1", 7, 4096, &parsed);

        assert_eq!(stored.summary.folder_id, "folder");
        assert_eq!(stored.summary.subject, "Header only");
        assert_eq!(
            stored
                .summary
                .r#from
                .as_ref()
                .map(|from| from.email.as_str()),
            Some("alice@example.com")
        );
        assert_eq!(stored.uid, Some(7));
        assert_eq!(stored.summary.size, 4096);
        assert_eq!(stored.message_id_header.as_deref(), Some("m-1@example.com"));
        assert!(stored
            .raw_headers
            .as_deref()
            .unwrap_or("")
            .contains("Subject"));
        // The listing row has no body and claims no attachments until the body
        // is downloaded and parsed.
        assert!(stored.body_path.is_none());
        assert!(!stored.summary.has_attachments);
        assert!(stored.summary.preview.is_empty());
    }

    #[test]
    fn header_row_orders_by_the_date_header_instant() {
        // +0800 10:00 is 02:00 UTC; the later UTC stamp must sort as newer.
        let first = headers(
            "Subject: first\r\nDate: Mon, 01 Jan 2024 10:00:00 +0800\r\nMessage-ID: <a@x>\r\n",
        );
        let second = headers(
            "Subject: second\r\nDate: Mon, 01 Jan 2024 05:00:00 +0000\r\nMessage-ID: <b@x>\r\n",
        );
        let mut rows = [
            header_row("acc", "folder", "uid-a", 1, 10, &first).summary,
            header_row("acc", "folder", "uid-b", 2, 10, &second).summary,
        ];
        rows.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| b.id.cmp(&a.id)));
        assert_eq!(rows[0].subject, "second");
        assert_eq!(rows[1].subject, "first");
    }

    /// A re-sent header block must not revert a newer local action: a moved,
    /// deleted or claimed row keeps its folder/identity/markers.
    #[test]
    fn a_newer_local_action_blocks_a_header_write() {
        let parsed = headers("Subject: x\r\nMessage-ID: <x@y>\r\n\r\n");
        let incoming = header_row("acc", "folder", "uidl-1", 7, 10, &parsed);
        let untouched = incoming.clone();

        assert!(may_publish_row(None, &incoming, None, false));
        assert!(may_publish_row(Some(&untouched), &incoming, None, false));

        let mut moved = untouched.clone();
        moved.summary.folder_id = "other".into();
        assert!(!may_publish_row(Some(&moved), &incoming, None, false));

        let mut deleted = untouched.clone();
        deleted.is_deleted_local = true;
        assert!(!may_publish_row(Some(&deleted), &incoming, None, false));

        let owner = repo::messages::ProjectionOwner {
            op_id: "op-new".into(),
            sequence: Some(2),
        };
        assert!(!may_publish_row(
            Some(&untouched),
            &incoming,
            Some(owner),
            false
        ));
        assert!(!may_publish_row(Some(&untouched), &incoming, None, true));
    }

    #[test]
    fn header_row_falls_back_to_now_without_a_date_header() {
        let parsed = headers("Subject: undated\r\nMessage-ID: <c@x>\r\n");
        let before = Utc::now();
        let stored = header_row("acc", "folder", "uid-c", 3, 0, &parsed);
        assert!(stored.summary.date >= before);
        // `internal_date` mirrors the header: absent stays absent.
        assert!(stored.internal_date.is_none());
    }

    #[test]
    fn uidl_identity_is_stable_per_account() {
        let parsed = headers("Subject: s\r\nMessage-ID: <d@x>\r\n");
        let one = header_row("acc", "folder", "uid-d", 1, 0, &parsed);
        let other_account = header_row("other", "folder-2", "uid-d", 1, 0, &parsed);
        let same = header_row("acc", "folder", "uid-d", 9, 0, &parsed);
        assert_eq!(one.summary.id, same.summary.id);
        assert_ne!(one.summary.id, other_account.summary.id);
    }
}
