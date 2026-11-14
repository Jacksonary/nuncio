//! Encrypted settings/account backup.
//!
//! File layout: `NUNCIOBK1` magic, 16-byte salt, 12-byte nonce, then the
//! AES-256-GCM ciphertext (which includes the tag). The key is derived from the
//! user password with Argon2id.

use std::collections::HashMap;

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use nuncio_core::models::{Account, AppSettings, Identity, MailRule, Signature};
use nuncio_core::AppError;

pub const MAGIC: &[u8; 9] = b"NUNCIOBK1";
pub const FORMAT: &str = "nuncio-backup";
pub const VERSION: u32 = 1;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

/// Decrypted backup document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupDocument {
    pub format: String,
    pub version: u32,
    pub exported_at: String,
    pub settings: AppSettings,
    pub accounts: Vec<Account>,
    pub identities: Vec<Identity>,
    pub signatures: Vec<Signature>,
    pub rules: Vec<MailRule>,
    /// `account:<id>:<suffix>` -> secret. Present only when requested.
    #[serde(default)]
    pub secrets: Option<HashMap<String, String>>,
}

impl BackupDocument {
    pub fn new(
        settings: AppSettings,
        accounts: Vec<Account>,
        identities: Vec<Identity>,
        signatures: Vec<Signature>,
        rules: Vec<MailRule>,
        secrets: Option<HashMap<String, String>>,
    ) -> Self {
        Self {
            format: FORMAT.to_string(),
            version: VERSION,
            exported_at: nuncio_core::models::now().to_rfc3339(),
            settings,
            accounts,
            identities,
            signatures,
            rules,
            secrets,
        }
    }

    pub fn validate(&self) -> nuncio_core::Result<()> {
        if self.format != FORMAT {
            return Err(AppError::invalid_input("this file is not a Nuncio backup"));
        }
        if self.version != VERSION {
            return Err(AppError::invalid_input(format!(
                "unsupported backup version {}",
                self.version
            )));
        }
        Ok(())
    }
}

fn derive_key(password: &str, salt: &[u8]) -> nuncio_core::Result<[u8; KEY_LEN]> {
    if password.is_empty() {
        return Err(AppError::invalid_input("the backup password is required"));
    }
    let params = Params::new(65_536, 3, 1, Some(KEY_LEN))
        .map_err(|error| AppError::internal(format!("argon2 parameters: {error}")))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; KEY_LEN];
    argon
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|error| AppError::internal(format!("key derivation failed: {error}")))?;
    Ok(key)
}

/// Serialize and encrypt a backup document.
pub fn encrypt(document: &BackupDocument, password: &str) -> nuncio_core::Result<Vec<u8>> {
    let plain = serde_json::to_vec(document).map_err(AppError::from)?;
    let mut salt = [0u8; SALT_LEN];
    let mut nonce_bytes = [0u8; NONCE_LEN];
    let mut rng = rand::rngs::OsRng;
    rng.fill_bytes(&mut salt);
    rng.fill_bytes(&mut nonce_bytes);

    let key = derive_key(password, &salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|error| AppError::internal(format!("cipher setup failed: {error}")))?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plain.as_ref())
        .map_err(|error| AppError::internal(format!("encryption failed: {error}")))?;

    let mut out = Vec::with_capacity(MAGIC.len() + SALT_LEN + NONCE_LEN + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypt and parse a backup file.
pub fn decrypt(bytes: &[u8], password: &str) -> nuncio_core::Result<BackupDocument> {
    let header = MAGIC.len() + SALT_LEN + NONCE_LEN;
    if bytes.len() <= header || &bytes[..MAGIC.len()] != MAGIC {
        return Err(AppError::invalid_input(
            "this file is not a Nuncio backup (bad header)",
        ));
    }
    let salt = &bytes[MAGIC.len()..MAGIC.len() + SALT_LEN];
    let nonce_bytes = &bytes[MAGIC.len() + SALT_LEN..header];
    let ciphertext = &bytes[header..];

    let key = derive_key(password, salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|error| AppError::internal(format!("cipher setup failed: {error}")))?;
    let nonce = Nonce::from_slice(nonce_bytes);
    let plain = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| AppError::invalid_input("wrong password or corrupted backup data"))?;
    let document: BackupDocument = serde_json::from_slice(&plain)
        .map_err(|_| AppError::invalid_input("wrong password or corrupted backup data"))?;
    document.validate()?;
    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> BackupDocument {
        BackupDocument::new(
            AppSettings::default(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
        )
    }

    #[test]
    fn round_trip() {
        let encrypted = encrypt(&document(), "correct horse").expect("encrypt");
        assert_eq!(&encrypted[..MAGIC.len()], MAGIC);
        let restored = decrypt(&encrypted, "correct horse").expect("decrypt");
        assert_eq!(restored.format, FORMAT);
        assert_eq!(restored.version, VERSION);
    }

    #[test]
    fn wrong_password_fails() {
        let encrypted = encrypt(&document(), "one").expect("encrypt");
        assert!(decrypt(&encrypted, "two").is_err());
    }

    #[test]
    fn rejects_non_backup_files() {
        assert!(decrypt(b"hello world", "x").is_err());
    }
}
