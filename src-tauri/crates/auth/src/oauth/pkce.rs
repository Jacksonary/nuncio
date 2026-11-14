//! PKCE (RFC 7636, S256) helpers and random token generation.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// A PKCE verifier/challenge pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    /// Generate a fresh 64-character verifier (43..=128 per RFC 7636).
    pub fn generate() -> Self {
        let mut bytes = [0u8; 48];
        let mut rng = OsRng;
        rng.fill_bytes(&mut bytes);
        Self::from_verifier(URL_SAFE_NO_PAD.encode(bytes))
    }

    /// Build a pair for an explicit verifier.  Used by tests and by callers
    /// that want to control the value.
    pub fn from_verifier(verifier: impl Into<String>) -> Self {
        let verifier = verifier.into();
        let challenge = s256_challenge(&verifier);
        Self {
            verifier,
            challenge,
        }
    }

    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    pub fn challenge(&self) -> &str {
        &self.challenge
    }
}

/// `BASE64URL-NO-PAD(SHA256(verifier))` — the S256 code challenge.
pub fn s256_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Random URL-safe token (32 bytes -> 43 characters).  Used for OAuth `state`.
pub fn random_state() -> String {
    let mut bytes = [0u8; 32];
    let mut rng = OsRng;
    rng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Random URL-safe bytes of arbitrary length (session ids, test helpers).
pub fn random_token(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    let mut rng = OsRng;
    rng.fill_bytes(&mut buffer);
    URL_SAFE_NO_PAD.encode(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636 Appendix B test vector.
    #[test]
    fn rfc7636_s256_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            s256_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let pkce = Pkce::from_verifier(verifier);
        assert_eq!(pkce.verifier(), verifier);
        assert_eq!(pkce.challenge(), s256_challenge(verifier));
    }

    #[test]
    fn generated_verifier_is_rfc_compliant() {
        let pkce = Pkce::generate();
        assert!(
            (43..=128).contains(&pkce.verifier().len()),
            "verifier length {} out of range",
            pkce.verifier().len()
        );
        assert!(pkce
            .verifier()
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~')));
        assert!(!pkce.challenge().contains('='));
        // A different verifier must produce a different challenge.
        let other = Pkce::generate();
        assert_ne!(pkce.verifier(), other.verifier());
        assert_ne!(pkce.challenge(), other.challenge());
    }

    #[test]
    fn random_state_is_unique_and_url_safe() {
        let a = random_state();
        let b = random_state();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
        assert!(!a.contains('+') && !a.contains('/') && !a.contains('='));
    }
}
