//! Secret storage: system keyring with an AES-256-GCM encrypted file fallback.
//!
//! Every secret is addressed by a string key such as
//! `account:<id>:password` or `account:<id>:refresh_token`.
//!
//! The primary backend is the OS keyring (macOS Keychain, Windows Credential
//! Manager, Linux Secret Service).  When the keyring cannot be used (for
//! example a Linux desktop without a Secret Service daemon) the store
//! transparently falls back to an encrypted JSON file in the application data
//! directory:
//!
//! ```text
//! <root>/secrets/secrets.key   32 random bytes (0600 on unix)
//! <root>/secrets/secrets.enc   magic || nonce(12) || AES-256-GCM ciphertext
//! ```
//!
//! The plaintext is a JSON object mapping key -> value.  Secrets never leave
//! the machine unless the caller explicitly exports them for a backup.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::rngs::OsRng;
use rand::RngCore;
use tracing::{debug, warn};

use nuncio_core::error::AppError;
use nuncio_core::paths::Paths;
use nuncio_core::Result;

/// Keyring service name shared by every Nuncio secret.
pub const KEYRING_SERVICE: &str = "app.nuncio.desktop";

/// SecretStore key for the user-supplied Google OAuth client secret
/// (Settings > Advanced).  It must never be persisted in the settings table:
/// `AppSettings` only keeps the non-secret client id.
pub const SETTINGS_GOOGLE_SECRET_KEY: &str = "settings:google_client_secret";

/// All account-scoped key suffixes.  Used for prefix deletion (the keyring API
/// cannot enumerate entries, so we delete the known keys explicitly) and for
/// backups.
pub const ACCOUNT_KEY_SUFFIXES: &[&str] = &[
    "password",
    "refresh_token",
    "refresh_token:outlook",
    "refresh_token:graph",
    "refresh_token:google",
];

const MAGIC: &[u8] = b"NUNCIO-SECRETS-1\n";
const KEY_FILE: &str = "secrets.key";
const DATA_FILE: &str = "secrets.enc";

/// Build the storage key for an account-scoped secret.
pub fn account_key(account_id: &str, suffix: &str) -> String {
    format!("account:{account_id}:{suffix}")
}

/// `account:<id>:password`
pub fn password_key(account_id: &str) -> String {
    account_key(account_id, "password")
}

/// `account:<id>:refresh_token`
pub fn refresh_key(account_id: &str) -> String {
    account_key(account_id, "refresh_token")
}

/// `settings:google_client_secret` — the advanced OAuth client secret.
pub fn google_client_secret_key() -> &'static str {
    SETTINGS_GOOGLE_SECRET_KEY
}

/// Keyring-backed secret store with an encrypted-file fallback.
///
/// All methods take `&self` and are safe to call concurrently.  File access is
/// serialised by an internal mutex; the keyring is thread-safe on its own.
pub struct SecretStore {
    service: String,
    dir: PathBuf,
    /// `false` after the keyring reported an unrecoverable error.  Secrets then
    /// go to the encrypted file for the rest of the process lifetime.
    keyring_enabled: AtomicBool,
    /// Whether this store was ever allowed to use the system keychain. `false`
    /// for `file_only()`, which is how tests opt out of touching real credentials.
    ever_keyring_enabled: bool,
    file_lock: Mutex<()>,
}

impl std::fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretStore")
            .field("service", &self.service)
            .field("dir", &self.dir)
            .field(
                "keyring_enabled",
                &self.keyring_enabled.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl SecretStore {
    /// Store rooted under `<app data dir>/secrets`, using the default service.
    pub fn new(paths: &Paths) -> Self {
        Self::with_dir(paths.root().join("secrets"))
    }

    /// Store rooted at an explicit directory, keyring first.
    pub fn with_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            service: KEYRING_SERVICE.to_string(),
            dir: dir.into(),
            keyring_enabled: AtomicBool::new(true),
            ever_keyring_enabled: true,
            file_lock: Mutex::new(()),
        }
    }

    /// Store that never touches the keyring.  Used by tests and by the
    /// "portable secrets" flow where the caller wants the encrypted file only.
    pub fn file_only(dir: impl Into<PathBuf>) -> Self {
        Self {
            service: KEYRING_SERVICE.to_string(),
            dir: dir.into(),
            keyring_enabled: AtomicBool::new(false),
            ever_keyring_enabled: false,
            file_lock: Mutex::new(()),
        }
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    pub fn data_dir(&self) -> &Path {
        &self.dir
    }

    /// Whether the keyring backend is still in use for this process.
    pub fn is_keyring_enabled(&self) -> bool {
        self.keyring_enabled.load(Ordering::SeqCst)
    }

    /// Read a secret.  `Ok(None)` means "not present".
    pub fn get(&self, key: &str) -> Result<Option<String>> {
        if self.is_keyring_enabled() {
            match keyring::Entry::new(&self.service, key) {
                Ok(entry) => match entry.get_password() {
                    Ok(value) => return Ok(Some(value)),
                    Err(keyring::Error::NoEntry) => {
                        // The value might have been written while the keyring
                        // was unavailable; check the file before reporting a miss.
                    }
                    Err(error) => {
                        warn!(
                            %error,
                            "keyring is unavailable; using the encrypted secret file"
                        );
                        self.disable_keyring();
                    }
                },
                Err(error) => {
                    warn!(
                        %error,
                        "keyring entry could not be created; using the encrypted secret file"
                    );
                    self.disable_keyring();
                }
            }
        }
        let map = self.read_file_map()?;
        Ok(map.get(key).cloned())
    }

    /// Create or replace a secret.
    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        if self.is_keyring_enabled() {
            match keyring::Entry::new(&self.service, key) {
                Ok(entry) => match entry.set_password(value) {
                    Ok(()) => {
                        // Drop a possible stale fallback value so a later
                        // keyring miss cannot resurrect an old secret.
                        self.remove_file_entry_if_present(key)?;
                        return Ok(());
                    }
                    Err(error) => {
                        warn!(
                            %error,
                            "keyring write failed; using the encrypted secret file"
                        );
                        self.disable_keyring();
                    }
                },
                Err(error) => {
                    warn!(
                        %error,
                        "keyring entry could not be created; using the encrypted secret file"
                    );
                    self.disable_keyring();
                }
            }
        }
        self.set_file_entry(key, value)
    }

    /// Remove a secret from both backends.  Missing entries are not an error.
    ///
    /// A genuine keyring failure is reported: reporting success while the value
    /// is still in the OS store means the secret reappears on the next launch,
    /// and the user was told it had been cleared.
    pub fn delete(&self, key: &str) -> Result<()> {
        // Attempt the keyring deletion even when reads were disabled earlier in
        // this process: a read failure does not imply the store cannot delete,
        // and skipping it would strand the old value.
        // `file_only()` builds the store with the keyring disabled on purpose
        // (tests, and hosts without a credential store). Attempting a keychain
        // delete there would reach real system credentials.
        if !self.ever_keyring_enabled {
            self.delete_file_entry(key)?;
            return Ok(());
        }
        let keyring_error = match keyring::Entry::new(&self.service, key) {
            Ok(entry) => match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => None,
                Err(error) => {
                    // Do not disable the keyring: a failed delete must not
                    // reroute all future secrets to the file.
                    warn!(%error, "keyring delete failed");
                    Some(error)
                }
            },
            Err(error) => {
                debug!(%error, "keyring entry could not be created for deletion");
                Some(error)
            }
        };
        self.delete_file_entry(key)?;
        match keyring_error {
            None => Ok(()),
            Some(error) => Err(AppError::io(format!(
                "the secret was removed from the local file but the system keychain \
                 still holds it: {error}"
            ))),
        }
    }

    /// Remove every secret belonging to an account.
    ///
    /// Every suffix is attempted even when one fails, so a single failure does
    /// not leave the remaining credentials behind; the first error is returned.
    pub fn delete_prefix(&self, account_id: &str) -> Result<()> {
        let mut first_error: Option<AppError> = None;
        for suffix in ACCOUNT_KEY_SUFFIXES {
            if let Err(error) = self.delete(&account_key(account_id, suffix)) {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }

        // Also remove any unknown keys under this account prefix from the file
        // backend (forward compatibility with future secret kinds).
        let prefix = format!("account:{account_id}:");
        let _guard = self.lock_files();
        let mut map = self.read_file_map_unlocked()?;
        let before = map.len();
        map.retain(|key, _| !key.starts_with(&prefix));
        if map.len() != before {
            self.write_file_map_unlocked(&map)?;
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn disable_keyring(&self) {
        if !self.keyring_enabled.swap(false, Ordering::SeqCst) {
            return;
        }
        tracing::info!(
            dir = %self.dir.display(),
            "secret storage switched to the encrypted file fallback"
        );
    }

    fn lock_files(&self) -> std::sync::MutexGuard<'_, ()> {
        self.file_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // ── Encrypted file backend ──────────────────────────────────────────

    fn read_file_map(&self) -> Result<BTreeMap<String, String>> {
        let _guard = self.lock_files();
        self.read_file_map_unlocked()
    }

    fn read_file_map_unlocked(&self) -> Result<BTreeMap<String, String>> {
        let path = self.dir.join(DATA_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeMap::new())
            }
            Err(error) => {
                return Err(AppError::io(format!(
                    "cannot read secret file {}: {error}",
                    path.display()
                )))
            }
        };
        if bytes.is_empty() {
            return Ok(BTreeMap::new());
        }
        let plaintext = self.decrypt(&bytes)?;
        serde_json::from_slice(&plaintext).map_err(|error| {
            AppError::io(format!(
                "secret file {} is not valid JSON: {error}",
                path.display()
            ))
        })
    }

    fn set_file_entry(&self, key: &str, value: &str) -> Result<()> {
        let _guard = self.lock_files();
        let mut map = self.read_file_map_unlocked()?;
        map.insert(key.to_string(), value.to_string());
        self.write_file_map_unlocked(&map)
    }

    fn delete_file_entry(&self, key: &str) -> Result<()> {
        let _guard = self.lock_files();
        let mut map = self.read_file_map_unlocked()?;
        if map.remove(key).is_some() {
            self.write_file_map_unlocked(&map)?;
        }
        Ok(())
    }

    fn remove_file_entry_if_present(&self, key: &str) -> Result<()> {
        if !self.dir.join(DATA_FILE).exists() {
            return Ok(());
        }
        let _guard = self.lock_files();
        let mut map = self.read_file_map_unlocked()?;
        if map.remove(key).is_some() {
            self.write_file_map_unlocked(&map)?;
        }
        Ok(())
    }

    fn write_file_map_unlocked(&self, map: &BTreeMap<String, String>) -> Result<()> {
        fs::create_dir_all(&self.dir).map_err(|error| {
            AppError::io(format!(
                "cannot create secret directory {}: {error}",
                self.dir.display()
            ))
        })?;
        let plaintext = serde_json::to_vec(map)
            .map_err(|error| AppError::io(format!("cannot encode secrets: {error}")))?;
        let encrypted = self.encrypt(&plaintext)?;
        write_private(&self.dir.join(DATA_FILE), &encrypted)
    }

    fn load_or_create_key(&self) -> Result<[u8; 32]> {
        let path = self.dir.join(KEY_FILE);
        match fs::read(&path) {
            Ok(bytes) => {
                if bytes.len() != 32 {
                    return Err(AppError::io(format!(
                        "secret key file {} is corrupt (expected 32 bytes, found {})",
                        path.display(),
                        bytes.len()
                    )));
                }
                let mut key = [0u8; 32];
                key.copy_from_slice(&bytes);
                Ok(key)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(&self.dir).map_err(|error| {
                    AppError::io(format!(
                        "cannot create secret directory {}: {error}",
                        self.dir.display()
                    ))
                })?;
                let mut key = [0u8; 32];
                let mut rng = OsRng;
                rng.fill_bytes(&mut key);
                write_private(&path, &key)?;
                tracing::info!(path = %path.display(), "created a new secret encryption key");
                Ok(key)
            }
            Err(error) => Err(AppError::io(format!(
                "cannot read secret key file {}: {error}",
                path.display()
            ))),
        }
    }

    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let key = self.load_or_create_key()?;
        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|error| AppError::internal(format!("invalid AES key: {error}")))?;
        let mut nonce_bytes = [0u8; 12];
        let mut rng = OsRng;
        rng.fill_bytes(&mut nonce_bytes);
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
            .map_err(|error| AppError::internal(format!("secret encryption failed: {error}")))?;

        let mut out = Vec::with_capacity(MAGIC.len() + nonce_bytes.len() + ciphertext.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    fn decrypt(&self, bytes: &[u8]) -> Result<Vec<u8>> {
        if !bytes.starts_with(MAGIC) {
            return Err(AppError::io(
                "secret file has an unknown format (magic mismatch)",
            ));
        }
        let rest = &bytes[MAGIC.len()..];
        if rest.len() < 12 + 16 {
            return Err(AppError::io("secret file is truncated"));
        }
        let (nonce_bytes, ciphertext) = rest.split_at(12);
        let key = self.load_or_create_key()?;
        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|error| AppError::internal(format!("invalid AES key: {error}")))?;
        cipher
            .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
            .map_err(|_| {
                AppError::io(
                    "secret file could not be decrypted (key file changed or file corrupted)",
                )
            })
    }
}

/// Write a file with owner-only permissions (0600 on unix).
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            AppError::io(format!(
                "cannot create directory {}: {error}",
                parent.display()
            ))
        })?;
    }

    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let tmp = temp_path(path);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|error| AppError::io(format!("cannot write {}: {error}", tmp.display())))?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| AppError::io(format!("cannot write {}: {error}", tmp.display())))?;
        drop(file);
        fs::rename(&tmp, path).map_err(|error| {
            let _ = fs::remove_file(&tmp);
            AppError::io(format!("cannot replace {}: {error}", path.display()))
        })?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let tmp = temp_path(path);
        fs::write(&tmp, bytes)
            .map_err(|error| AppError::io(format!("cannot write {}: {error}", tmp.display())))?;
        if path.exists() {
            let _ = fs::remove_file(path);
        }
        fs::rename(&tmp, path).map_err(|error| {
            let _ = fs::remove_file(&tmp);
            AppError::io(format!("cannot replace {}: {error}", path.display()))
        })?;
        Ok(())
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "secrets.tmp".to_string());
    path.with_file_name(format!(".{name}.tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_store(dir: &Path) -> SecretStore {
        SecretStore::file_only(dir)
    }

    #[test]
    fn file_fallback_round_trip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = file_store(dir.path());

        assert_eq!(store.get("account:a:password").expect("get"), None);
        store
            .set("account:a:password", "hunter2")
            .expect("set password");
        store
            .set("account:a:refresh_token", "refresh-token")
            .expect("set token");
        store.set("account:b:password", "other").expect("set other");

        assert_eq!(
            store.get("account:a:password").expect("get").as_deref(),
            Some("hunter2")
        );

        // A fresh instance must see the persisted values.
        let reopened = file_store(dir.path());
        assert_eq!(
            reopened.get("account:a:refresh_token").expect("get"),
            Some("refresh-token".to_string())
        );

        // The file must not contain plaintext secrets.
        let raw = fs::read(dir.path().join(DATA_FILE)).expect("read secret file");
        let raw_text = String::from_utf8_lossy(&raw);
        assert!(!raw_text.contains("hunter2"));
        assert!(!raw_text.contains("refresh-token"));
        assert!(raw.starts_with(MAGIC));

        // delete_prefix removes only that account's keys.
        reopened.delete_prefix("a").expect("delete prefix");
        assert_eq!(reopened.get("account:a:password").expect("get"), None);
        assert_eq!(reopened.get("account:a:refresh_token").expect("get"), None);
        assert_eq!(
            reopened.get("account:b:password").expect("get"),
            Some("other".to_string())
        );

        reopened.delete("account:b:password").expect("delete");
        assert_eq!(reopened.get("account:b:password").expect("get"), None);
    }

    #[cfg(unix)]
    #[test]
    fn secret_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let store = file_store(dir.path());
        store.set("k", "v").expect("set");

        for name in [KEY_FILE, DATA_FILE] {
            let mode = fs::metadata(dir.path().join(name))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{name} must be 0600, was {mode:o}");
        }
    }

    #[test]
    fn account_key_helpers() {
        assert_eq!(password_key("x"), "account:x:password");
        assert_eq!(refresh_key("x"), "account:x:refresh_token");
        assert_eq!(
            account_key("x", "refresh_token:graph"),
            "account:x:refresh_token:graph"
        );
        assert_eq!(google_client_secret_key(), "settings:google_client_secret");
    }

    #[test]
    fn settings_secret_is_not_removed_by_account_prefix_delete() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = file_store(dir.path());
        store
            .set(SETTINGS_GOOGLE_SECRET_KEY, "client-secret")
            .expect("set settings secret");
        store.set("account:a:password", "pw").expect("set password");

        store.delete_prefix("a").expect("delete prefix");

        assert_eq!(
            store.get(SETTINGS_GOOGLE_SECRET_KEY).expect("get"),
            Some("client-secret".to_string())
        );
    }

    #[test]
    fn corrupt_key_file_is_reported() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join(KEY_FILE), b"short").expect("write key");
        let store = file_store(dir.path());
        let error = store.set("k", "v").expect_err("must fail");
        assert!(error.message().contains("corrupt"));
    }

    /// Manual test: requires a working OS keyring and will prompt on macOS.
    #[ignore = "requires an interactive OS keyring"]
    #[test]
    fn keyring_round_trip() {
        let store = SecretStore::with_dir(std::env::temp_dir().join("nuncio-keyring-test"));
        store
            .set("account:test-keyring:password", "value")
            .expect("keyring set");
        assert_eq!(
            store.get("account:test-keyring:password").expect("get"),
            Some("value".to_string())
        );
        store
            .delete("account:test-keyring:password")
            .expect("delete");
    }
}
