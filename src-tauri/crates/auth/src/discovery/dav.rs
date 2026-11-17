//! CalDAV / CardDAV endpoint discovery via RFC 6764 well-known URIs.

use nuncio_core::error::AppError;

use crate::http::DISCOVERY_TIMEOUT;

/// CalDAV/CardDAV endpoints found for a domain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DavUrls {
    pub caldav_url: Option<String>,
    pub carddav_url: Option<String>,
}

/// Return directory/preset endpoints or probe `/.well-known/{caldav,carddav}`.
///
/// The preset lookup is directory-backed and also answers for known providers
/// with no DAV service: `Some((None, None))` means "known, do not probe" (163
/// has no CalDAV; `caldav.163.com` only answers 403). Probing is therefore
/// limited to domains the directory does not know.
///
/// Probes are best-effort: any network error simply leaves the field empty.
pub async fn discover(http: &reqwest::Client, domain: &str) -> DavUrls {
    let domain = domain.trim().trim_end_matches('.');
    if domain.is_empty() {
        return DavUrls::default();
    }

    if let Some((caldav_url, carddav_url)) = crate::discovery::presets::preset_dav(domain) {
        return DavUrls {
            caldav_url,
            carddav_url,
        };
    }

    let caldav_url = format!("https://{domain}/.well-known/caldav");
    let carddav_url = format!("https://{domain}/.well-known/carddav");
    let (caldav, carddav) = tokio::join!(probe(http, &caldav_url), probe(http, &carddav_url));

    DavUrls {
        caldav_url: caldav,
        carddav_url: carddav,
    }
}

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop><d:resourcetype/></d:prop>
</d:propfind>"#;

async fn probe(http: &reqwest::Client, url: &str) -> Option<String> {
    let method = reqwest::Method::from_bytes(b"PROPFIND").ok()?;
    let response = http
        .request(method, url)
        .header("Depth", "0")
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(PROPFIND_BODY)
        .timeout(DISCOVERY_TIMEOUT)
        .send()
        .await
        .map_err(|error| {
            tracing::debug!(%error, url, "well-known DAV probe failed");
            error
        })
        .ok()?;

    let status = response.status().as_u16();
    if matches!(status, 200 | 207 | 301 | 302 | 303 | 307 | 308) {
        // After following redirects this is the canonical DAV collection URL.
        Some(response.url().to_string())
    } else {
        tracing::debug!(
            url,
            status,
            "well-known DAV probe returned an unexpected status"
        );
        None
    }
}

/// Build a CalDAV URL validation error for misconfigured accounts.
pub fn invalid_url(value: &str) -> AppError {
    AppError::invalid_input(format!("invalid CalDAV/CardDAV URL: {value}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_domain_yields_defaults() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let client = crate::http::http_client().expect("client");
        let urls = runtime.block_on(discover(&client, "   "));
        assert!(urls.caldav_url.is_none());
        assert!(urls.carddav_url.is_none());
    }

    #[test]
    fn presets_do_not_need_network() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let client = crate::http::http_client().expect("client");
        let urls = runtime.block_on(discover(&client, "icloud.com"));
        assert_eq!(
            urls.caldav_url.as_deref(),
            Some("https://caldav.icloud.com")
        );
        assert_eq!(
            urls.carddav_url.as_deref(),
            Some("https://contacts.icloud.com")
        );
    }

    #[test]
    fn known_provider_without_dav_does_not_probe() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let client = crate::http::http_client().expect("client");
        let urls = runtime.block_on(discover(&client, "163.com"));
        assert!(urls.caldav_url.is_none());
        assert_eq!(
            urls.carddav_url.as_deref(),
            Some("https://contacts.163.com")
        );
    }

    #[test]
    fn invalid_url_message_is_readable() {
        assert!(invalid_url("ftp://x").message().contains("CalDAV"));
    }
}
