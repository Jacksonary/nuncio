//! Message body and attachment retrieval, MIME cache management.
//!
//! Bodies are stored as `.eml` under `Paths::mime/<account>/<xx>/<hash>.eml`.
//! The DB keeps only metadata plus the parsed text used for search, so opening
//! a message does not require re-downloading it. The cache is trimmed LRU-style
//! to `settings.mail.bodyCacheLimitMb`.

use std::path::PathBuf;

use async_imap::imap_proto::SectionPath;
use chrono::Utc;
use nuncio_core::models::{Account, Attachment, ProviderKind, StoredMessage};
use nuncio_core::{repo, AppError, CoreContext, Result};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::graph::GraphMailClient;
use crate::imap::conn::ImapConnection;
use crate::util::attachment_id;

const MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;

/// Version token for one immutable cache file.
///
/// It covers both the provider copy the bytes were read from and the bytes
/// themselves, so two different copies (or two different bodies of the same
/// copy) can never share a filename. That is what makes a cached `.eml`
/// immutable: a racing fetch writes a different file instead of overwriting
/// the one a reader may already hold.
pub(crate) fn body_cache_version(identity: Option<&str>, raw: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(identity.unwrap_or("unknown").as_bytes());
    hasher.update(b"\0");
    hasher.update(raw);
    hex::encode(&hasher.finalize()[..8])
}

/// Version token encoded in a versioned cache file name.
///
/// Only the exact `<hash>-<16 lowercase hex>.eml` shape is accepted. Legacy
/// files, draft staging paths and anything else return `None`: a reader then
/// has no proof of which bytes it holds and must not grant a delete right.
pub(crate) fn body_cache_version_from_path(path: &std::path::Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let (_, version) = stem.rsplit_once('-')?;
    let versioned = version.len() == 16
        && version
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    versioned.then(|| version.to_string())
}

/// Write `raw` to a new versioned cache file and return its path.
///
/// The write goes through a temporary file in the same directory and is
/// renamed into place, so a reader never observes a partially written cache.
/// An existing file with the same version is identical by construction and is
/// reused.
pub(crate) async fn cache_body_file(
    ctx: &CoreContext,
    account_id: &str,
    message_id: &str,
    identity: Option<&str>,
    raw: &[u8],
) -> Result<PathBuf> {
    let version = body_cache_version(identity, raw);
    let path = ctx
        .paths
        .mime_versioned_path(account_id, message_id, &version);
    ctx.paths.ensure_parent(&path)?;
    if tokio::fs::metadata(&path).await.is_ok() {
        return Ok(path);
    }
    let temporary = path.with_file_name(format!(
        ".{}-{}.tmp",
        nuncio_core::paths::sanitize_component(message_id),
        nuncio_core::models::new_id()
    ));
    tokio::fs::write(&temporary, raw)
        .await
        .map_err(|error| AppError::io(format!("cannot write message cache: {error}")))?;
    if let Err(error) = tokio::fs::rename(&temporary, &path).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        // A concurrent writer may have won the rename with identical bytes.
        if tokio::fs::metadata(&path).await.is_ok() {
            return Ok(path);
        }
        return Err(AppError::io(format!("cannot write message cache: {error}")));
    }
    Ok(path)
}

/// The copy a body write is allowed to publish under.
///
/// Captured when the body request starts, before the bytes are fetched. The
/// identity key names the provider copy; account and folder are included
/// because an upsert also writes those columns, and a row without an identity
/// (a local/Graph row in a transient state) must still be pinned to the place
/// it was captured in.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PublishTarget {
    account_id: String,
    folder_id: String,
    identity: Option<String>,
}

impl PublishTarget {
    fn of(stored: &StoredMessage) -> Self {
        Self {
            account_id: stored.summary.account_id.clone(),
            folder_id: stored.summary.folder_id.clone(),
            identity: repo::messages::body_identity_key(stored),
        }
    }
}

/// Refuse to attach cached bytes unless the row still names the copy (and the
/// account/folder) the bytes came from.
///
/// The identity comparison includes the "no identity" case: a snapshot without
/// a key must not write over a row a sync has since bound, because the upsert
/// would clear that newer `uid`/`gm_msgid`. An owned or pending projection is
/// refused as well, so the upsert cannot rewrite the folder or markers of a
/// newer move/reconcile trip.
///
/// Returning an error instead of falling through is what keeps a body fetch
/// from recreating a row a delete/move removed while the download was in
/// flight.
fn ensure_publish_target(
    conn: &rusqlite::Connection,
    id: &str,
    expected: &PublishTarget,
) -> Result<()> {
    let current = repo::messages::get(conn, id)?;
    let Some(current) = current else {
        return Err(AppError::conflict(
            "message disappeared while its body was being downloaded",
        ));
    };
    let same_copy = current.summary.account_id == expected.account_id
        && current.summary.folder_id == expected.folder_id
        && repo::messages::body_identity_key(&current) == expected.identity;
    if !same_copy {
        return Err(AppError::conflict(
            "message was rebound while its body was being downloaded; retry after the next sync",
        ));
    }
    // A row waiting for a move target/reconciliation, or one claimed by a trip,
    // belongs to that trip: a stale body snapshot must not write over it even
    // when both sides have no identity key at all.
    let owner = repo::messages::projection_owner(conn, id)?;
    let pending = repo::messages::is_pending_projection(conn, id)?;
    if owner.is_some() || pending {
        return Err(AppError::conflict(
            "message is being moved while its body was downloaded; retry after the next sync",
        ));
    }
    Ok(())
}

/// Hard limit for one RFC822 message. Bodies larger than this are not
/// downloaded, because the whole message is buffered by the IMAP client and
/// then parsed in memory (the release build aborts on OOM).
pub(crate) const MAX_MESSAGE_BYTES: u64 = 50 * 1024 * 1024;

/// Download (or return the cached) raw RFC822 message and store it on disk.
pub async fn fetch_body(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account_id: &str,
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

    // Clear early dispatch: a Microsoft account is refused an IMAP session, so
    // its body comes from Graph.  The IMAP branch below stays untouched.
    let account = {
        let account_id = account_id.to_string();
        ctx.db
            .read(move |db| repo::accounts::get_required(db, &account_id))
            .await?
    };
    if crate::graph::uses_graph(&account) {
        return fetch_body_graph_stored(ctx, &account, stored).await;
    }

    if let Some(path) = stored.body_path.as_deref() {
        let path = PathBuf::from(path);
        if tokio::fs::metadata(&path).await.is_ok() {
            return Ok(path);
        }
        debug!(
            message = message_id,
            "cached body file is missing; re-downloading"
        );
    }

    let uid = stored
        .uid
        .ok_or_else(|| AppError::unsupported("message has no IMAP UID"))?;
    let folder = ctx
        .db
        .read({
            let folder_id = stored.summary.folder_id.clone();
            move |db| repo::folders::get(db, &folder_id)
        })
        .await?
        .ok_or_else(|| AppError::not_found("message folder no longer exists"))?;
    if !folder.is_selectable {
        return Err(AppError::unsupported("message folder is not selectable"));
    }

    let mailbox = conn.select(&folder.remote_path).await?;
    // A UID only names the same message inside the UIDVALIDITY generation it
    // was synced in. After a mailbox rebuild the stored number may address an
    // unrelated message, so refuse to download (and cache) its bytes under
    // this row. Without a recorded generation the source cannot be proven;
    // the fetch is kept as best effort but the bytes are never attributed to
    // a copy (see `body_identity` below).
    let live_uidvalidity = mailbox.uid_validity.map(|value| value as i64);
    // Legacy rows may lack their own generation; the folder's last synced
    // generation is the next best evidence that the cached UID still belongs
    // to this mailbox.
    let folder_uidvalidity = {
        let folder_id = folder.id.clone();
        ctx.db
            .read(move |db| repo::folders::uidvalidity(db, &folder_id))
            .await?
    };
    if let Some(expected) = stored.uidvalidity.or(folder_uidvalidity) {
        if live_uidvalidity != Some(expected) {
            warn!(
                message = %stored.summary.id,
                folder = %folder.remote_path,
                expected,
                live = ?live_uidvalidity,
                "body fetch skipped: the folder was rebuilt since this message was synced"
            );
            return Err(AppError::conflict(format!(
                "message body skipped: {} was rebuilt since the message was synced",
                folder.remote_path
            )));
        }
    }

    // Check the size before downloading so an oversized message never reaches
    // the process heap. Metadata sync stores RFC822.SIZE; probe it when unknown.
    let declared_size = if stored.summary.size > 0 {
        stored.summary.size
    } else {
        fetch_declared_size(conn, uid).await.unwrap_or(0)
    };
    if declared_size > MAX_MESSAGE_BYTES {
        return Err(AppError::invalid_input(format!(
            "message is too large to open ({} MB, limit {} MB)",
            declared_size / (1024 * 1024),
            MAX_MESSAGE_BYTES / (1024 * 1024)
        )));
    }

    let set = uid.to_string();
    let fetches = conn.uid_fetch_all(&set, "(UID BODY.PEEK[])").await?;

    let raw = fetches
        .iter()
        .find_map(|fetch| fetch.body())
        .ok_or_else(|| AppError::protocol("server returned no message body"))?;
    if raw.len() as u64 > MAX_MESSAGE_BYTES {
        return Err(AppError::invalid_input(
            "message body exceeds the size limit; only metadata was stored",
        ));
    }

    // Re-read the row before attaching the bytes. A sync may have rebound it
    // while the FETCH was in flight; caching the old copy's MIME under the new
    // row would pair B's headers with A's body and could authorize a delete of
    // the wrong copy later.
    let source_key = repo::messages::body_identity_key(&stored);
    let current = ctx
        .db
        .read({
            let message_id = stored.summary.id.clone();
            move |db| repo::messages::get(db, &message_id)
        })
        .await?;
    match current {
        Some(current) => {
            if repo::messages::body_identity_key(&current) != source_key {
                return Err(AppError::conflict(
                    "message was rebound while its body was being downloaded; retry after the next sync",
                ));
            }
        }
        None => {
            return Err(AppError::not_found(format!(
                "message {} disappeared while its body was being downloaded",
                stored.summary.id
            )))
        }
    }
    // Only a generation the SELECT above verified proves these bytes belong to
    // this row's copy. Without one the bytes are still cached (best effort)
    // but the source stays unknown, so they can never authorize a delete.
    let body_identity = if stored.uidvalidity.is_some() {
        source_key
    } else {
        None
    };

    // The file name carries the source identity and a digest of these exact
    // bytes, so publishing it under another row (or after a rebind) is a
    // different path and can never rewrite a cache a reader still uses.
    let stored_id = stored.summary.id.clone();
    let path = cache_body_file(ctx, account_id, &stored_id, body_identity.as_deref(), raw).await?;

    if let Err(error) = store_parsed_body(ctx, stored, path.clone(), raw, body_identity).await {
        // Never leave an unpublished copy: no row names it, so nobody could
        // read it and it would only occupy the cache budget.
        let _ = tokio::fs::remove_file(&path).await;
        return Err(error);
    }
    Ok(path)
}

/// Download one message body through Microsoft Graph and cache it as `.eml`.
///
/// Graph returns rendered HTML, not raw RFC822.  Rebuilding a minimal RFC822
/// document with the stored headers keeps the rest of the reading path
/// unchanged: the reader parses `.eml`, `raw_message` returns bytes, and the
/// cid/attachment cache logic sees a normal MIME document.  Attachment bytes
/// are still server-side (Graph exposes them through a separate endpoint), so
/// a Graph message's attachments are only listed, not downloadable, until that
/// endpoint is wired.
pub async fn fetch_body_graph(
    ctx: &CoreContext,
    account: &Account,
    message_id: &str,
) -> Result<PathBuf> {
    if !crate::graph::uses_graph(account) {
        return Err(AppError::unsupported(
            "message body fetch through Graph is only available for Microsoft accounts",
        ));
    }
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
    fetch_body_graph_stored(ctx, account, stored).await
}

/// Shared Graph body materialisation; `stored` is already loaded.
async fn fetch_body_graph_stored(
    ctx: &CoreContext,
    account: &Account,
    stored: StoredMessage,
) -> Result<PathBuf> {
    if let Some(path) = stored.body_path.as_deref() {
        let path = PathBuf::from(path);
        if tokio::fs::metadata(&path).await.is_ok() {
            return Ok(path);
        }
        debug!(
            message = %stored.summary.id,
            "cached Graph body file is missing; re-downloading"
        );
    }

    // `gm_msgid` holds the Graph id; the message id itself is the same value
    // for rows written by the Graph sync, but older rows may differ.
    let remote_id = stored
        .gm_msgid
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(&stored.summary.id);
    let html =
        crate::graph::messages::fetch_body(&GraphMailClient::new(), ctx, &account.id, remote_id)
            .await?;

    let mut message = nuncio_mime::OutgoingMessage::new(
        stored
            .summary
            .r#from
            .clone()
            .unwrap_or_else(|| account.address()),
        stored.summary.subject.clone(),
        html.clone(),
    );
    message.to = stored.summary.to.clone();
    message.cc = stored.summary.cc.clone();
    message.bcc = stored.bcc.clone();
    message.reply_to = stored.reply_to.clone();
    message.in_reply_to = stored.in_reply_to.clone();
    message.references = stored.references.clone();
    message.importance = stored.summary.importance;
    message.message_id = stored.message_id_header.clone();
    message.date = Some(stored.summary.date);
    // Graph's HTML already carries any read-receipt request; do not invent one.
    message.request_read_receipt = false;
    let (raw, _) = nuncio_mime::build_message_with_bcc(message)?;
    if raw.len() as u64 > MAX_MESSAGE_BYTES {
        return Err(AppError::invalid_input(
            "message body exceeds the size limit; only metadata was stored",
        ));
    }

    // Captured before the body was fetched: a sync that rebinds the row while
    // the request is in flight must leave the cached MIME attributed to the
    // copy it actually came from.
    let body_identity = repo::messages::body_identity_key(&stored);
    let stored_id = stored.summary.id.clone();
    let path =
        cache_body_file(ctx, &account.id, &stored_id, body_identity.as_deref(), &raw).await?;

    // Graph gives HTML; store the same plain-text projection the IMAP parser
    // would produce so search and previews behave identically.
    let text = nuncio_mime::html_to_text(&html);
    let preview = nuncio_core::text::trim_preview(&text, 300);
    let path_string = path.to_string_lossy().to_string();
    let message_id = stored.summary.id.clone();
    let text_for_db = text;
    let expected = PublishTarget::of(&stored);
    let published = ctx
        .db
        .write(move |db| {
            ensure_publish_target(db, &message_id, &expected)?;
            repo::messages::set_body(
                db,
                &message_id,
                Some(&path_string),
                Some(&text_for_db),
                body_identity.as_deref(),
            )?;
            repo::messages::update_preview(db, &message_id, &preview)
        })
        .await;
    if let Err(error) = published {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(error);
    }

    debug!(
        message = %stored.summary.id,
        bytes = raw.len(),
        "Graph message body cached"
    );
    Ok(path)
}

/// Ask the server for `RFC822.SIZE` without downloading the body.
async fn fetch_declared_size(conn: &mut ImapConnection, uid: i64) -> Result<u64> {
    let fetches = conn
        .uid_fetch_all(&uid.to_string(), "(UID RFC822.SIZE)")
        .await?;
    Ok(fetches
        .iter()
        .find_map(|fetch| fetch.size)
        .map(u64::from)
        .unwrap_or(0))
}

/// Parse a downloaded message and update preview, FTS text and attachments.
///
/// `body_identity` names the provider copy the bytes were actually read from
/// (verified by the caller against the live UIDVALIDITY). It is persisted with
/// `body_path` so a later reader can tell whether the cache still belongs to
/// the copy the row names now.
async fn store_parsed_body(
    ctx: &CoreContext,
    mut stored: StoredMessage,
    path: PathBuf,
    raw: &[u8],
    body_identity: Option<String>,
) -> Result<()> {
    let expected = PublishTarget::of(&stored);
    let parsed = match nuncio_mime::parse(raw) {
        Ok(parsed) => parsed,
        Err(error) => {
            warn!(%error, message = %stored.summary.id, "failed to parse downloaded MIME body");
            let path_string = path.to_string_lossy().to_string();
            let message_id = stored.summary.id.clone();
            let body_identity = body_identity.clone();
            let expected = expected.clone();
            ctx.db
                .write(move |db| {
                    ensure_publish_target(db, &message_id, &expected)?;
                    repo::messages::set_body(
                        db,
                        &message_id,
                        Some(&path_string),
                        None,
                        body_identity.as_deref(),
                    )
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
    stored.summary.thread_id = crate::util::thread_id(
        stored.message_id_header.as_deref(),
        stored.in_reply_to.as_deref(),
        &stored.references,
        &stored.summary.subject,
        stored.gm_thrid.as_deref(),
    );
    stored.raw_headers = Some(headers.raw.clone());
    stored.body_path = Some(path.to_string_lossy().to_string());
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
    let path_string = path.to_string_lossy().to_string();
    let text_for_fts = body_text.clone();
    let preview = stored.summary.preview.clone();
    let mut parsed_stored = stored.clone();
    ctx.db
        .write(move |db| {
            // The body was fetched under the snapshot above; a sync may have
            // rebound the row while it was in flight. These bytes then belong
            // to another copy, so neither the parsed headers nor the body may
            // be written under this row. A row that disappeared must not be
            // recreated by the upsert below: the delete/move already ran.
            ensure_publish_target(db, &message_id, &expected)?;
            let current = repo::messages::get(db, &message_id)?
                .expect("ensure_publish_target proved the row exists");
            // State that does not come from the body belongs to the live row:
            // the user may have read/flagged (or moved) the message while its
            // body was downloading.
            parsed_stored.summary.seen = current.summary.seen;
            parsed_stored.summary.flagged = current.summary.flagged;
            parsed_stored.summary.answered = current.summary.answered;
            parsed_stored.summary.forwarded = current.summary.forwarded;
            parsed_stored.summary.is_draft = current.summary.is_draft;
            parsed_stored.is_deleted_local = current.is_deleted_local;
            // The guard proved account/folder/identity are unchanged, so the
            // upsert writes the same values back instead of moving the row.
            parsed_stored.summary.account_id = current.summary.account_id.clone();
            parsed_stored.summary.folder_id = current.summary.folder_id.clone();
            parsed_stored.uid = current.uid;
            parsed_stored.uidvalidity = current.uidvalidity;
            parsed_stored.gm_msgid = current.gm_msgid.clone();
            let account_id = parsed_stored.summary.account_id.clone();
            crate::sync::messages::resolve_thread_id(db, &account_id, &mut parsed_stored)?;
            repo::messages::upsert(db, &parsed_stored)?;
            let thread_id = parsed_stored.summary.thread_id.clone();
            crate::sync::messages::reconcile_threads(
                db,
                &account_id,
                &thread_id,
                parsed_stored.message_id_header.as_deref(),
            )?;
            repo::messages::set_body(
                db,
                &message_id,
                Some(&path_string),
                Some(&text_for_fts),
                body_identity.as_deref(),
            )?;
            repo::messages::update_preview(db, &message_id, &preview)?;
            repo::attachments::replace_for_message(db, &message_id, &attachments)?;
            repo::folders::recompute_counts(db, &parsed_stored.summary.folder_id)
        })
        .await?;

    debug!(message = %stored.summary.id, bytes = raw.len(), "message body cached");
    Ok(())
}

/// One attachment's addressable location inside the cached `.eml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentPart {
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    pub content_id: Option<String>,
    pub is_inline: bool,
    /// IMAP section path / MIME part path (`"2"` or `"1.2"`).
    pub part: String,
    /// Flattened parser index accepted by [`fetch_attachment`].
    pub index: u32,
}

/// Parse the cached body (downloading it first if needed) and describe every
/// attachment. The command layer uses this to map database attachment ids to
/// IMAP parts.
pub async fn attachment_parts(ctx: &CoreContext, message_id: &str) -> Result<Vec<AttachmentPart>> {
    let stored = ctx
        .db
        .read({
            let message_id = message_id.to_string();
            move |db| repo::messages::get(db, &message_id)
        })
        .await?
        .ok_or_else(|| AppError::not_found(format!("message {message_id} not found")))?;
    let Some(path) = stored.body_path.as_deref() else {
        return Err(AppError::not_found("message body is not cached"));
    };
    let raw = read_cached_body(path).await?;
    let parsed = nuncio_mime::parse(&raw)?;
    Ok(parsed
        .attachments
        .iter()
        .map(|attachment| AttachmentPart {
            filename: attachment.filename.clone(),
            mime_type: attachment.mime_type.clone(),
            size: attachment.size,
            content_id: attachment.content_id.clone(),
            is_inline: attachment.is_inline,
            part: attachment.part.as_string(),
            index: attachment.index,
        })
        .collect())
}

/// Fetch one attachment's decoded bytes.
///
/// `part` may be an IMAP section path (`"2"`, `"1.2"`) or a flattened
/// attachment index (`"0"`, `"1"`, ...). When the message body is already
/// cached the index form is resolved from the local `.eml`; otherwise the
/// numeric value is treated as an IMAP part path.
pub async fn fetch_attachment(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    message_id: &str,
    part: &str,
) -> Result<Vec<u8>> {
    let stored = ctx
        .db
        .read({
            let message_id = message_id.to_string();
            move |db| repo::messages::get(db, &message_id)
        })
        .await?
        .ok_or_else(|| AppError::not_found(format!("message {message_id} not found")))?;

    if let Some(path) = stored.body_path.as_deref() {
        match read_cached_body(path).await {
            Ok(raw) => {
                if let Some(path_parts) = nuncio_mime::PartPath::parse(part) {
                    return nuncio_mime::extract_part(&raw, &path_parts);
                }
                if let Ok(index) = part.parse::<u32>() {
                    return nuncio_mime::extract_part_by_index(&raw, index);
                }
                return Err(AppError::invalid_input(format!(
                    "invalid attachment part: {part}"
                )));
            }
            Err(error) if error.code() == "invalidInput" => return Err(error),
            Err(error) => debug!(%error, "cached body unreadable; fetching the part"),
        }
    }

    let uid = stored
        .uid
        .ok_or_else(|| AppError::unsupported("message has no IMAP UID"))?;
    let folder = ctx
        .db
        .read({
            let folder_id = stored.summary.folder_id.clone();
            move |db| repo::folders::get(db, &folder_id)
        })
        .await?
        .ok_or_else(|| AppError::not_found("message folder no longer exists"))?;
    let mailbox = conn.select(&folder.remote_path).await?;
    // Same generation rule as the full body: a UID from a rebuilt mailbox may
    // address another message, so refuse instead of returning its bytes.
    if let Some(expected) = stored.uidvalidity {
        if mailbox.uid_validity.map(|value| value as i64) != Some(expected) {
            return Err(AppError::conflict(format!(
                "attachment fetch skipped: {} was rebuilt since the message was synced",
                folder.remote_path
            )));
        }
    }

    let query = format!("(UID BODY.PEEK[{part}])");
    let fetches = conn.uid_fetch_all(&uid.to_string(), &query).await?;

    let section = attachment_section(part);
    let bytes = fetches
        .iter()
        .find_map(|fetch| match &section {
            Some(section) => fetch.section(section),
            None => None,
        })
        .ok_or_else(|| AppError::protocol("server returned no attachment data"))?;
    if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(AppError::invalid_input(
            "attachment exceeds the 50 MB download limit",
        ));
    }
    Ok(bytes.to_vec())
}

/// Read a cached `.eml`, refusing files above the message size limit so a large
/// file cannot be pulled into memory just to extract one part.
async fn read_cached_body(path: &str) -> Result<Vec<u8>> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| AppError::io(format!("cannot read cached body: {error}")))?;
    if metadata.len() > MAX_MESSAGE_BYTES {
        return Err(AppError::invalid_input(
            "cached message exceeds the size limit",
        ));
    }
    tokio::fs::read(path)
        .await
        .map_err(|error| AppError::io(format!("cannot read cached body: {error}")))
}

fn attachment_section(part: &str) -> Option<SectionPath> {
    let parts: Option<Vec<u32>> = part
        .split('.')
        .map(|segment| segment.parse::<u32>().ok())
        .collect();
    parts.map(|parts| SectionPath::Part(parts, None))
}

/// Drop least-recently cached bodies until the MIME cache is under the limit.
pub async fn trim_cache(ctx: &CoreContext, limit_mb: u32) -> Result<usize> {
    let limit_bytes = u64::from(limit_mb).saturating_mul(1024 * 1024);
    if limit_bytes == 0 {
        return Ok(0);
    }
    let root = ctx.paths.mime_root();
    let root_for_size = root.clone();
    let mut total =
        tokio::task::spawn_blocking(move || nuncio_core::paths::directory_size(&root_for_size))
            .await
            .map_err(|error| AppError::internal(format!("cache size task failed: {error}")))?;
    if total <= limit_bytes {
        return Ok(0);
    }

    let candidates = ctx
        .db
        .read(move |db| repo::messages::body_lru_candidates(db, 500))
        .await?;
    // POP3 messages have no server-side copy we can re-fetch by UID; never
    // evict their bodies.
    let pop3_accounts: Vec<String> = ctx
        .db
        .read(|db| {
            Ok(repo::accounts::list(db)?
                .into_iter()
                .filter(|account| account.provider == ProviderKind::Pop3)
                .map(|account| account.id)
                .collect::<Vec<_>>())
        })
        .await?;
    let mut removed = 0;
    for entry in candidates {
        if total <= limit_bytes {
            break;
        }
        if !pop3_accounts.is_empty() {
            let id = entry.id.clone();
            let account_id = ctx
                .db
                .read(move |db| {
                    Ok(repo::messages::get(db, &id)?.map(|stored| stored.summary.account_id))
                })
                .await?;
            if account_id
                .as_deref()
                .map(|account| pop3_accounts.iter().any(|pop3| pop3 == account))
                .unwrap_or(false)
            {
                continue;
            }
        }
        let path = PathBuf::from(&entry.body_path);
        let size = tokio::fs::metadata(&path)
            .await
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if tokio::fs::remove_file(&path).await.is_ok() {
            total = total.saturating_sub(size);
            removed += 1;
        }
        let id = entry.id.clone();
        ctx.db
            .write(move |db| repo::messages::set_body(db, &id, None, None, None))
            .await?;
    }
    if removed > 0 {
        debug!(removed, remaining = total, "trimmed MIME cache");
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_sections() {
        assert_eq!(
            attachment_section("1.2"),
            Some(SectionPath::Part(vec![1, 2], None))
        );
        assert_eq!(attachment_section("x"), None);
    }

    #[test]
    fn message_size_limit_is_fifty_megabytes() {
        assert_eq!(MAX_MESSAGE_BYTES, 50 * 1024 * 1024);
        assert!(MAX_ATTACHMENT_BYTES <= MAX_MESSAGE_BYTES);
    }

    #[test]
    fn cache_version_binds_copy_and_content() {
        let first = body_cache_version(Some("graph:copy-a"), b"one");
        assert_eq!(first, body_cache_version(Some("graph:copy-a"), b"one"));
        assert_ne!(
            first,
            body_cache_version(Some("graph:copy-b"), b"one"),
            "the same bytes read from another copy must get another file"
        );
        assert_ne!(
            first,
            body_cache_version(Some("graph:copy-a"), b"two"),
            "different bytes from the same copy must get another file"
        );
        assert_eq!(first.len(), 16);
    }

    fn publish_fixture() -> (nuncio_core::Db, StoredMessage) {
        use nuncio_core::models::{Folder, FolderRole, MessageSummary};

        let db = nuncio_core::Db::open_in_memory().expect("db");
        db.migrate_core().expect("migrate");
        let folder = Folder::new("folder", "local", "INBOX", FolderRole::Inbox);
        let mut summary = MessageSummary::new(
            "row",
            "local",
            "folder",
            "thread",
            nuncio_core::models::now(),
        );
        summary.subject = "s".into();
        let mut row = StoredMessage::new(summary);
        row.uid = Some(7);
        row.uidvalidity = Some(1);
        let stored = row.clone();
        db.write_sync(move |conn| {
            repo::folders::insert(conn, &folder)?;
            repo::messages::upsert(conn, &row)
        })
        .expect("seed row");
        (db, stored)
    }

    #[test]
    fn publishing_requires_a_live_matching_row() {
        let (db, row) = publish_fixture();
        let key = PublishTarget::of(&row);
        db.read_sync(|conn| {
            assert!(ensure_publish_target(conn, "row", &key).is_ok());
            // The row was deleted while the body was in flight: no upsert may
            // recreate it.
            assert!(ensure_publish_target(conn, "missing", &key).is_err());
            Ok(())
        })
        .expect("live row");

        // The row was rebound to another copy in the same generation.
        db.write_sync(|conn| repo::messages::set_uid_identity(conn, "row", Some(8), Some(1)))
            .expect("rebind");
        db.read_sync(|conn| {
            assert!(ensure_publish_target(conn, "row", &key).is_err());
            Ok(())
        })
        .expect("rebound row");

        // A snapshot without an identity must not write over a newer binding:
        // the upsert would clear the uid a sync just bound.
        let (db, _) = publish_fixture();
        let unbound = PublishTarget {
            account_id: "local".into(),
            folder_id: "folder".into(),
            identity: None,
        };
        db.write_sync(|conn| repo::messages::set_uid_identity(conn, "row", Some(9), Some(1)))
            .expect("new binding");
        db.read_sync(|conn| {
            assert!(ensure_publish_target(conn, "row", &unbound).is_err());
            Ok(())
        })
        .expect("stale unbound snapshot");

        // A newer trip's ownership must block the write even when the identity
        // is still the same (nothing from the body may reset its markers).
        let (db, row) = publish_fixture();
        let key = PublishTarget::of(&row);
        db.write_sync(|conn| {
            conn.execute(
                "UPDATE message SET reconcile_op_id = 'op-new', reconcile_seq = 3
                  WHERE id = 'row'",
                [],
            )?;
            Ok(())
        })
        .expect("claim");
        db.read_sync(|conn| {
            assert!(ensure_publish_target(conn, "row", &key).is_err());
            Ok(())
        })
        .expect("owned row");

        // A pending projection is refused the same way.
        let (db, row) = publish_fixture();
        let key = PublishTarget::of(&row);
        db.write_sync(|conn| {
            conn.execute("UPDATE message SET target_pending = 1 WHERE id = 'row'", [])?;
            Ok(())
        })
        .expect("pending");
        db.read_sync(|conn| {
            assert!(ensure_publish_target(conn, "row", &key).is_err());
            Ok(())
        })
        .expect("pending row");
    }
}
