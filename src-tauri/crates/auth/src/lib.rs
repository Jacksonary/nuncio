//! nuncio-auth — secrets, OAuth2 (PKCE), credential management and server
//! auto-discovery for Nuncio.
//!
//! The crate only depends on `nuncio-core`; every network endpoint and
//! provider-specific detail lives here so the app, mail and PIM crates stay
//! free of auth logic.
//!
//! Main entry points:
//! * [`AuthService`] — implements [`nuncio_core::traits::CredentialProvider`],
//!   runs the interactive browser flow and caches/refreshes OAuth tokens.
//! * [`SecretStore`] — OS keyring with an AES-256-GCM file fallback.
//! * [`discover`] / [`discover_dav`] — presets, ISPDB, autoconfig, MX and
//!   well-known DAV discovery.
//! * [`http_client`] — rustls-backed client shared by the above.
//!
//! ```no_run
//! use std::sync::Arc;
//! use nuncio_auth::{AuthService, OAuthConfig, SystemUrlOpener};
//! use nuncio_core::{Db, Paths};
//! use nuncio_core::traits::testing::NullEventSink;
//!
//! # async fn run() -> nuncio_core::Result<()> {
//! let db = Db::open_in_memory()?;
//! db.migrate_core()?;
//! let paths = Paths::new("/tmp/nuncio");
//! paths.ensure_dirs()?;
//! let auth = AuthService::new(
//!     db,
//!     paths,
//!     Arc::new(NullEventSink),
//!     OAuthConfig::from_env(),
//!     Arc::new(SystemUrlOpener),
//! )?;
//! let discovery = auth.discover("someone@gmail.com").await?;
//! println!("{}", discovery.source);
//! # Ok(()) }
//! ```

pub mod discovery;
pub mod http;
pub mod oauth;
pub mod secrets;
pub mod service;

pub use discovery::dav::DavUrls;
pub use discovery::{discover, discover_dav, split_email};
pub use http::{http_client, install_crypto_provider};
pub use oauth::flow::{SystemUrlOpener, UrlOpener};
pub use oauth::{AuthorizationRequest, OAuthConfig, ResolvedClient};
pub use secrets::SecretStore;
pub use service::{oauth_config_from_settings, preset_for_domain, AuthService};
