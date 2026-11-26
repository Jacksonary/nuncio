//! Filesystem layout for database, MIME cache, attachments, staging, logs.

use std::path::{Path, PathBuf};

use crate::error::AppError;
use crate::Result;

/// Rooted at the Tauri `app_local_data_dir`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `nuncio.db` inside the data directory.
    pub fn db_file(&self) -> PathBuf {
        self.root.join("nuncio.db")
    }

    /// Alias for [`Paths::db_file`] used by startup code.
    pub fn db_path(&self) -> PathBuf {
        self.db_file()
    }

    /// `mime/<account>/<2-char>/<hash>.eml`
    pub fn mime_path(&self, account_id: &str, hash: &str) -> PathBuf {
        self.mime_dir(account_id, hash)
            .join(format!("{}.eml", sanitize_component(hash)))
    }

    /// `mime/<account>/<2-char>/<hash>-<version>.eml`
    ///
    /// The version names the exact bytes and the provider copy they came from,
    /// so a fetch for another version never overwrites a file a reader may
    /// still be using. Only the row whose `body_path` names this file may treat
    /// its bytes as its own.
    pub fn mime_versioned_path(&self, account_id: &str, hash: &str, version: &str) -> PathBuf {
        let hash = sanitize_component(hash);
        let version = sanitize_component(version);
        self.mime_dir(account_id, &hash)
            .join(format!("{hash}-{version}.eml"))
    }

    fn mime_dir(&self, account_id: &str, hash: &str) -> PathBuf {
        let account = sanitize_component(account_id);
        let hash = sanitize_component(hash);
        let prefix: String = hash.chars().take(2).collect::<String>().to_lowercase();
        let prefix = if prefix.is_empty() {
            "xx".to_string()
        } else {
            prefix
        };
        self.root.join("mime").join(account).join(prefix)
    }

    pub fn mime_root(&self) -> PathBuf {
        self.root.join("mime")
    }

    pub fn attachments_dir(&self) -> PathBuf {
        self.root.join("attachments")
    }

    pub fn attachment_dir(&self, message_id: &str) -> PathBuf {
        self.attachments_dir().join(sanitize_component(message_id))
    }

    /// Path for a persisted attachment (filename sanitized to a basename).
    pub fn attachment_path(&self, message_id: &str, filename: &str) -> PathBuf {
        self.attachment_dir(message_id)
            .join(sanitize_filename(filename))
    }

    pub fn staging_dir(&self) -> PathBuf {
        self.root.join("staging")
    }

    pub fn draft_staging_dir(&self, draft_id: &str) -> PathBuf {
        self.staging_dir().join(sanitize_component(draft_id))
    }

    pub fn draft_attachment_path(&self, draft_id: &str, filename: &str) -> PathBuf {
        self.draft_staging_dir(draft_id)
            .join(sanitize_filename(filename))
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// Create every directory the app writes to (idempotent).
    pub fn ensure_dirs(&self) -> Result<()> {
        for directory in [
            self.root.clone(),
            self.mime_root(),
            self.attachments_dir(),
            self.staging_dir(),
            self.logs_dir(),
            self.tmp_dir(),
        ] {
            std::fs::create_dir_all(&directory).map_err(|error| {
                AppError::Io(format!("cannot create {}: {error}", directory.display()))
            })?;
        }
        Ok(())
    }

    /// Ensure the parent directory of a MIME cache file exists.
    pub fn ensure_parent(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                AppError::Io(format!("cannot create {}: {error}", parent.display()))
            })?;
        }
        Ok(())
    }
}

/// Keep only safe characters in an id used as a directory/file component.
pub fn sanitize_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches('.').to_string();
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed
    }
}

/// Strip directories and dangerous characters from an attachment filename.
pub fn sanitize_filename(value: &str) -> String {
    let basename = value
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(value)
        .trim()
        .to_string();
    let cleaned: String = basename
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(character, ':' | '*' | '?' | '"' | '<' | '>' | '|')
            {
                '_'
            } else {
                character
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('.').to_string();
    if cleaned.is_empty() {
        "attachment".to_string()
    } else {
        cleaned
    }
}

/// Recursive directory size helper (used by `app_storage_usage`).
pub fn directory_size(path: &Path) -> u64 {
    let Ok(metadata) = std::fs::metadata(path) else {
        return 0;
    };
    if metadata.is_file() {
        return metadata.len();
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| directory_size(&entry.path()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_paths() {
        let paths = Paths::new("/data/nuncio");
        assert_eq!(paths.db_file(), PathBuf::from("/data/nuncio/nuncio.db"));
        assert_eq!(
            paths.mime_path("acc-1", "AB12cd"),
            PathBuf::from("/data/nuncio/mime/acc-1/ab/AB12cd.eml")
        );
        // A versioned cache file is a distinct path per content/source, so a
        // fetch for another copy can never overwrite the one a reader holds.
        assert_eq!(
            paths.mime_versioned_path("acc-1", "AB12cd", "deadbeefdeadbeef"),
            PathBuf::from("/data/nuncio/mime/acc-1/ab/AB12cd-deadbeefdeadbeef.eml")
        );
        assert_ne!(
            paths.mime_versioned_path("acc-1", "AB12cd", "aaaaaaaaaaaaaaaa"),
            paths.mime_versioned_path("acc-1", "AB12cd", "bbbbbbbbbbbbbbbb")
        );
        assert_eq!(
            paths.attachment_path("msg-1", "../../evil.exe"),
            PathBuf::from("/data/nuncio/attachments/msg-1/evil.exe")
        );
        assert_eq!(
            paths.draft_staging_dir("draft 1"),
            PathBuf::from("/data/nuncio/staging/draft_1")
        );
    }

    #[test]
    fn sanitization_blocks_traversal() {
        assert_eq!(sanitize_component("../etc"), "_etc");
        assert_eq!(sanitize_component("..."), "unknown");
        assert_eq!(sanitize_filename("a/b\\c.txt"), "c.txt");
        assert_eq!(sanitize_filename("..."), "attachment");
        assert_eq!(sanitize_filename("invoice:2026.pdf"), "invoice_2026.pdf");
    }

    #[test]
    fn ensure_dirs_creates_layout() {
        let root = std::env::temp_dir().join(format!("nuncio-paths-{}", uuid::Uuid::new_v4()));
        let paths = Paths::new(&root);
        paths.ensure_dirs().expect("dirs");
        assert!(paths.logs_dir().is_dir());
        assert!(paths.mime_root().is_dir());
        assert!(paths.staging_dir().is_dir());
        assert!(paths.tmp_dir().is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn directory_size_sums_files() {
        let root = std::env::temp_dir().join(format!("nuncio-size-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("sub")).expect("dir");
        std::fs::write(root.join("a.bin"), vec![0u8; 10]).expect("write");
        std::fs::write(root.join("sub/b.bin"), vec![0u8; 5]).expect("write");
        assert_eq!(directory_size(&root), 15);
        let _ = std::fs::remove_dir_all(&root);
    }
}
