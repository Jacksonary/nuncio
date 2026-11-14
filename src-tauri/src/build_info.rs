//! Build-time OAuth client configuration.
//!
//! `build.rs` turns `.env.local` into `rustc-env` values for this crate, but a
//! dependency crate cannot see them. Runtime environment variables take
//! precedence so packaged builds and containers can override the values.

use nuncio_auth::OAuthConfig;

const BUILD_GOOGLE_ID: Option<&str> = option_env!("NUNCIO_GOOGLE_CLIENT_ID");
const BUILD_GOOGLE_SECRET: Option<&str> = option_env!("NUNCIO_GOOGLE_CLIENT_SECRET");
const BUILD_MICROSOFT_ID: Option<&str> = option_env!("NUNCIO_MS_CLIENT_ID");

fn value(runtime: &str, build: Option<&str>) -> Option<String> {
    std::env::var(runtime)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            build
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
}

pub fn google_client_id() -> Option<String> {
    value("NUNCIO_GOOGLE_CLIENT_ID", BUILD_GOOGLE_ID)
}

pub fn google_client_secret() -> Option<String> {
    value("NUNCIO_GOOGLE_CLIENT_SECRET", BUILD_GOOGLE_SECRET)
}

pub fn microsoft_client_id() -> Option<String> {
    value("NUNCIO_MS_CLIENT_ID", BUILD_MICROSOFT_ID)
}

pub fn has_google_client() -> bool {
    google_client_id().is_some()
}

pub fn has_microsoft_client() -> bool {
    microsoft_client_id().is_some()
}

/// OAuth clients from the build/runtime environment. `settings.advanced`
/// overrides are layered on top by `AuthService::with_settings`.
pub fn oauth_config() -> OAuthConfig {
    OAuthConfig::new(
        google_client_id(),
        google_client_secret(),
        microsoft_client_id(),
    )
}
