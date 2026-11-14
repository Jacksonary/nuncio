//! Attachment download, caching, saving and OS hand-off helpers.

use std::path::{Path, PathBuf};

use nuncio_core::models::Attachment;
use nuncio_core::{repo, AppError, Result};
use tracing::debug;

use super::MailService;

impl MailService {
    /// True for executable/script types that need a confirmation prompt.
    pub fn attachment_is_dangerous(&self, filename: &str) -> bool {
        nuncio_mime::is_dangerous(filename)
    }

    /// Ensure the attachment is cached locally and return its path. The app
    /// crate opens the returned path with the OS default application.
    pub async fn attachment_open(&self, id: &str) -> Result<PathBuf> {
        if let Some((_mime_type, bytes, filename, message_id)) = self.external_attachment(id) {
            let dest = self.external_cache_path(&message_id, id, &filename);
            self.inner.ctx.paths.ensure_parent(&dest)?;
            tokio::fs::write(&dest, &bytes)
                .await
                .map_err(|error| AppError::io(format!("cannot cache attachment: {error}")))?;
            mark_of_the_web(&dest);
            return Ok(dest);
        }
        let attachment = self.attachment(id).await?;
        self.ensure_cached(&attachment).await
    }

    /// Save an attachment to a user-chosen path.
    pub async fn attachment_save(&self, id: &str, path: &str) -> Result<()> {
        let target = PathBuf::from(path);
        if let Some((_, bytes, _, _)) = self.external_attachment(id) {
            tokio::fs::write(&target, &bytes)
                .await
                .map_err(|error| AppError::io(format!("cannot save attachment: {error}")))?;
            mark_of_the_web(&target);
            return Ok(());
        }
        let attachment = self.attachment(id).await?;
        let cached = self.ensure_cached(&attachment).await?;
        if cached != target {
            tokio::fs::copy(&cached, &target)
                .await
                .map_err(|error| AppError::io(format!("cannot save attachment: {error}")))?;
        }
        mark_of_the_web(&target);
        Ok(())
    }

    /// Save every attachment of a message into a directory.
    pub async fn attachment_save_all(&self, message_id: &str, dir: &str) -> Result<Vec<PathBuf>> {
        let target_dir = PathBuf::from(dir);
        let mut saved = Vec::new();
        // Externally opened messages live only in the registry.
        let external = self.external_attachments_of(message_id);
        if !external.is_empty() {
            for record in external {
                let Some((_, bytes, filename, _)) = self.external_attachment(&record.attachment_id)
                else {
                    continue;
                };
                let dest = unique_existing(&target_dir, &filename).await;
                tokio::fs::write(&dest, &bytes)
                    .await
                    .map_err(|error| AppError::io(format!("cannot save attachment: {error}")))?;
                mark_of_the_web(&dest);
                saved.push(dest);
            }
            return Ok(saved);
        }
        let attachments = {
            let message_id = message_id.to_string();
            self.inner
                .ctx
                .db
                .read(move |db| repo::attachments::list(db, &message_id))
                .await?
        };
        for attachment in attachments {
            let cached = self.ensure_cached(&attachment).await?;
            let dest = unique_existing(&target_dir, &attachment.filename).await;
            if cached != dest {
                tokio::fs::copy(&cached, &dest)
                    .await
                    .map_err(|error| AppError::io(format!("cannot save attachment: {error}")))?;
            }
            mark_of_the_web(&dest);
            saved.push(dest);
        }
        Ok(saved)
    }

    /// Local cache path of an attachment, if it has been downloaded.
    pub async fn attachment_path(&self, id: &str) -> Result<Option<PathBuf>> {
        if let Some((_, _, filename, message_id)) = self.external_attachment(id) {
            return Ok(Some(self.external_cache_path(&message_id, id, &filename)));
        }
        let id_owned = id.to_string();
        let path = self
            .inner
            .ctx
            .db
            .read(move |db| repo::attachments::local_path(db, &id_owned))
            .await?;
        Ok(path.map(PathBuf::from).filter(|path| path.exists()))
    }

    async fn attachment(&self, id: &str) -> Result<Attachment> {
        let id_owned = id.to_string();
        self.inner
            .ctx
            .db
            .read(move |db| repo::attachments::get_required(db, &id_owned))
            .await
    }

    /// Download (or reuse) an attachment into `attachments/<message-id>/`.
    async fn ensure_cached(&self, attachment: &Attachment) -> Result<PathBuf> {
        if let Some(path) = self.attachment_path(&attachment.id).await? {
            return Ok(path);
        }
        let bytes = self.stored_attachment_bytes(attachment).await?;
        let dir = self.inner.ctx.paths.attachment_dir(&attachment.message_id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|error| AppError::io(format!("cannot create attachment dir: {error}")))?;
        let dest = unique_existing(&dir, &attachment.filename).await;
        tokio::fs::write(&dest, &bytes)
            .await
            .map_err(|error| AppError::io(format!("cannot cache attachment: {error}")))?;
        let id = attachment.id.clone();
        let path_string = dest.to_string_lossy().to_string();
        self.inner
            .ctx
            .db
            .write(move |db| repo::attachments::set_downloaded(db, &id, &path_string))
            .await?;
        mark_of_the_web(&dest);
        debug!(attachment = %attachment.id, size = bytes.len(), "cached attachment");
        Ok(dest)
    }

    async fn stored_attachment_bytes(&self, attachment: &Attachment) -> Result<Vec<u8>> {
        let raw = self.raw_message_by_id(&attachment.message_id).await?;
        let parsed = nuncio_mime::parse(&raw)?;
        for (index, part) in parsed.attachments.iter().enumerate() {
            let expected = crate::util::attachment_id(
                &attachment.message_id,
                index,
                part.content_id.as_deref(),
            );
            if expected == attachment.id {
                return nuncio_mime::extract_part(&raw, &part.part);
            }
        }
        // Fallback for rows created by an older parse: match content id or name.
        let by_content = attachment.content_id.as_deref().and_then(|content_id| {
            parsed.attachments.iter().find(|part| {
                part.content_id
                    .as_deref()
                    .map(|value| value.eq_ignore_ascii_case(content_id))
                    .unwrap_or(false)
            })
        });
        let matched = by_content.or_else(|| {
            parsed
                .attachments
                .iter()
                .find(|part| part.filename == attachment.filename)
        });
        match matched {
            Some(part) => nuncio_mime::extract_part(&raw, &part.part),
            None => Err(AppError::not_found(format!(
                "attachment {} is not present in the cached message",
                attachment.filename
            ))),
        }
    }

    fn external_cache_path(
        &self,
        message_id: &str,
        attachment_id: &str,
        filename: &str,
    ) -> PathBuf {
        let prefix: String = attachment_id.chars().take(8).collect();
        let safe = nuncio_core::paths::sanitize_filename(filename);
        let name = if safe.is_empty() {
            format!("{prefix}-attachment")
        } else {
            format!("{prefix}-{safe}")
        };
        self.inner.ctx.paths.attachment_dir(message_id).join(name)
    }
}

/// Path inside `dir` for `filename`, avoiding overwrites with ` (n)` suffixes.
async fn unique_existing(dir: &Path, filename: &str) -> PathBuf {
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
    let mut candidate = dir.join(&base);
    let mut index = 1u32;
    while tokio::fs::metadata(&candidate).await.is_ok() {
        let name = match &extension {
            Some(extension) => format!("{stem} ({index}).{extension}"),
            None => format!("{stem} ({index})"),
        };
        candidate = dir.join(name);
        index += 1;
        if index > 10_000 {
            break;
        }
    }
    candidate
}

/// Best-effort Mark-of-the-Web so Windows SmartScreen can evaluate downloads.
pub(crate) fn mark_of_the_web(path: &Path) {
    #[cfg(windows)]
    {
        use std::io::Write;
        let zone_path = format!("{}:Zone.Identifier", path.display());
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&zone_path)
        {
            let _ = file.write_all(b"[ZoneTransfer]\r\nZoneId=3\r\n");
        }
    }
    #[cfg(not(windows))]
    {
        let _ = path;
    }
}
