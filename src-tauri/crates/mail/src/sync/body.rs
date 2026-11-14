//! Message body and attachment retrieval, MIME cache management.
//!
//! Bodies are stored as `.eml` under `Paths::mime/<account>/<xx>/<hash>.eml`.
//! The DB keeps only metadata plus the parsed text used for search, so opening
//! a message does not require re-downloading it. The cache is trimmed LRU-style
//! to `settings.mail.bodyCacheLimitMb`.

use std::path::PathBuf;

use async_imap::imap_proto::SectionPath;
use async_imap::types::Fetch;
use chrono::Utc;
use futures::TryStreamExt;
use nuncio_core::models::{Attachment, Folder, ProviderKind, StoredMessage};
use nuncio_core::{repo, AppError, CoreContext, Result};
use tracing::{debug, warn};

use crate::imap::conn::{ImapConnection, FETCH_TIMEOUT};
use crate::util::{attachment_id, imap_error};

const MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;

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

    conn.select(&folder.remote_path).await?;

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
    let session = &mut conn.session;
    let fetches = tokio::time::timeout(FETCH_TIMEOUT, async {
        let stream = session
            .uid_fetch(set, "(UID BODY.PEEK[])")
            .await
            .map_err(imap_error)?;
        stream.try_collect::<Vec<Fetch>>().await.map_err(imap_error)
    })
    .await
    .map_err(|_| AppError::network("body fetch timed out"))??;

    let raw = fetches
        .iter()
        .find_map(|fetch| fetch.body())
        .ok_or_else(|| AppError::protocol("server returned no message body"))?;
    if raw.len() as u64 > MAX_MESSAGE_BYTES {
        return Err(AppError::invalid_input(
            "message body exceeds the size limit; only metadata was stored",
        ));
    }

    let path = ctx.paths.mime_path(account_id, &stored.summary.id);
    ctx.paths.ensure_parent(&path)?;
    // Write straight from the fetched slice: no second full-message copy.
    tokio::fs::write(&path, raw)
        .await
        .map_err(|error| AppError::io(format!("cannot write message cache: {error}")))?;

    store_parsed_body(ctx, stored, path.clone(), raw).await?;
    Ok(path)
}

/// Ask the server for `RFC822.SIZE` without downloading the body.
async fn fetch_declared_size(conn: &mut ImapConnection, uid: i64) -> Result<u64> {
    let session = &mut conn.session;
    let fetches = tokio::time::timeout(FETCH_TIMEOUT, async {
        let stream = session
            .uid_fetch(uid.to_string(), "(UID RFC822.SIZE)")
            .await
            .map_err(imap_error)?;
        stream.try_collect::<Vec<Fetch>>().await.map_err(imap_error)
    })
    .await
    .map_err(|_| AppError::network("size probe timed out"))??;
    Ok(fetches
        .iter()
        .find_map(|fetch| fetch.size)
        .map(u64::from)
        .unwrap_or(0))
}

/// Parse a downloaded message and update preview, FTS text and attachments.
async fn store_parsed_body(
    ctx: &CoreContext,
    mut stored: StoredMessage,
    path: PathBuf,
    raw: &[u8],
) -> Result<()> {
    let parsed = match nuncio_mime::parse(raw) {
        Ok(parsed) => parsed,
        Err(error) => {
            warn!(%error, message = %stored.summary.id, "failed to parse downloaded MIME body");
            let path_string = path.to_string_lossy().to_string();
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
            repo::messages::set_body(db, &message_id, Some(&path_string), Some(&text_for_fts))?;
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
    conn.select(&folder.remote_path).await?;

    let query = format!("(UID BODY.PEEK[{part}])");
    let session = &mut conn.session;
    let fetches = tokio::time::timeout(FETCH_TIMEOUT, async {
        let stream = session
            .uid_fetch(uid.to_string(), query)
            .await
            .map_err(imap_error)?;
        stream.try_collect::<Vec<Fetch>>().await.map_err(imap_error)
    })
    .await
    .map_err(|_| AppError::network("attachment fetch timed out"))??;

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

/// Prefetch the newest `limit` unread bodies of a folder.
pub async fn prefetch_unread(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account_id: &str,
    folder: &Folder,
    limit: u32,
) -> Result<usize> {
    if limit == 0 {
        return Ok(0);
    }
    let folder_id = folder.id.clone();
    let ids = ctx
        .db
        .read(move |db| {
            let mut statement = db.prepare(
                "SELECT id FROM message
                 WHERE folder_id = ?1 AND seen = 0 AND is_deleted_local = 0 AND body_path IS NULL
                 ORDER BY date DESC LIMIT ?2",
            )?;
            let mut rows = statement.query(rusqlite::params![folder_id, limit as i64])?;
            let mut ids = Vec::new();
            while let Some(row) = rows.next()? {
                ids.push(row.get::<_, String>(0)?);
            }
            Ok(ids)
        })
        .await?;

    let mut fetched = 0;
    for id in ids {
        match fetch_body(ctx, conn, account_id, &id).await {
            Ok(_) => fetched += 1,
            Err(error) => debug!(%error, message = %id, "body prefetch skipped"),
        }
    }
    Ok(fetched)
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
            .write(move |db| repo::messages::set_body(db, &id, None, None))
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
}
