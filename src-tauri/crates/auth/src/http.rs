//! Shared HTTP client and rustls provider installation.
//!
//! `reqwest` is built with the `rustls-no-provider` feature, so the process
//! must install a `CryptoProvider` before the first TLS handshake.  The app
//! crate does this at startup; [`install_crypto_provider`] is idempotent and
//! safe to call from anywhere (including tests).

use std::sync::Once;
use std::time::Duration;

use nuncio_core::error::AppError;
use nuncio_core::Result;

static INSTALL: Once = Once::new();

/// Timeout for short discovery / configuration HTTP requests.
pub const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(6);

/// Install the rustls **ring** provider once per process.
///
/// Ignoring the error is intentional: it only means another component already
/// installed a provider, which is fine.
pub fn install_crypto_provider() {
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Build a rustls-backed `reqwest` client with sensible desktop timeouts.
///
/// Discovery passes use [`DISCOVERY_TIMEOUT`] per request; token exchange uses
/// its own longer per-request timeout.
pub fn http_client() -> Result<reqwest::Client> {
    install_crypto_provider();
    reqwest::Client::builder()
        .use_rustls_tls()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("Nuncio/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| AppError::network(format!("cannot create HTTP client: {error}")))
}
