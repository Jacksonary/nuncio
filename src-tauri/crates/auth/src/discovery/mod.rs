//! E-mail server auto-discovery.
//!
//! Order (first hit wins):
//! 1. built-in provider presets (instant, offline);
//! 2. Thunderbird ISPDB (`allow_ispdb` must be true);
//! 3. the domain's own Mozilla autoconfig file;
//! 4. MX-record inference (Google, Outlook, QQ Exmail, Aliyun, Netease);
//! 5. `imap.<domain>:993` / `smtp.<domain>:465` guess.
//!
//! CalDAV/CardDAV endpoints come from presets or an RFC 6764 well-known probe.

pub mod autoconfig;
pub mod dav;
pub mod mx;
pub mod presets;

use std::time::Duration;

use nuncio_core::error::AppError;
use nuncio_core::models::{AuthKind, DiscoveryResult, ProviderKind, Security, ServerConfig};
use nuncio_core::Result;

use crate::http::DISCOVERY_TIMEOUT;

/// Maximum accepted autoconfig/ISPDB document size.
const MAX_CONFIG_BYTES: usize = 512 * 1024;

/// Split and validate an e-mail address into `(local_part, domain)`.
pub fn split_email(email: &str) -> Result<(String, String)> {
    let mut value = email.trim();
    if let Some(stripped) = value
        .strip_prefix("mailto:")
        .or_else(|| value.strip_prefix("MAILTO:"))
    {
        value = stripped.trim();
    }
    let (local, domain) = value.rsplit_once('@').ok_or_else(|| {
        AppError::invalid_input("enter a complete e-mail address, for example name@example.com")
    })?;
    let local = local.trim();
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    if local.is_empty()
        || domain.is_empty()
        || !domain.contains('.')
        || local.chars().any(|c| c.is_whitespace())
        || domain.chars().any(|c| c.is_whitespace())
    {
        return Err(AppError::invalid_input(format!(
            "'{email}' is not a valid e-mail address"
        )));
    }
    Ok((local.to_string(), domain))
}

/// Discover mail server settings using a fresh HTTP client.
pub async fn discover(email: &str, allow_ispdb: bool) -> Result<DiscoveryResult> {
    let http = crate::http::http_client()?;
    discover_with_client(&http, email, allow_ispdb).await
}

/// Discover mail server settings with a caller-supplied HTTP client.
pub async fn discover_with_client(
    http: &reqwest::Client,
    email: &str,
    allow_ispdb: bool,
) -> Result<DiscoveryResult> {
    let (_, domain) = split_email(email)?;
    let email = email.trim();

    // 1. Built-in presets. The directory already carries DAV endpoints, so we
    // return as-is: for a known provider a missing DAV URL is authoritative
    // ("163 has no CalDAV"), and probing would surface a 403 as a false hit.
    // Entries with no IMAP/SMTP (Tuta) are final too; they must not fall
    // through to the `imap.<domain>` guess.
    if let Some(preset) = presets::lookup(&domain) {
        tracing::debug!(%domain, "discovery matched a built-in preset");
        return Ok(preset.into_result(email));
    }

    // 2. Thunderbird ISPDB.
    if allow_ispdb {
        let url = format!("https://autoconfig.thunderbird.net/v1.1/{domain}");
        if let Some(xml) = fetch_text(http, &url).await {
            match autoconfig::parse(&xml, email, "ispdb") {
                Ok(mut result) => {
                    tracing::debug!(%domain, "discovery matched ISPDB");
                    attach_dav(http, &domain, &mut result).await;
                    return Ok(result);
                }
                Err(error) => {
                    tracing::debug!(%error, %domain, "ISPDB document could not be used");
                }
            }
        }
    }

    // 3. Domain autoconfig (both well-known locations in parallel).
    let first = format!(
        "https://autoconfig.{domain}/mail/config-v1.1.xml?emailaddress={}",
        urlencoding::encode(email)
    );
    let second = format!("https://{domain}/.well-known/autoconfig/mail/config-v1.1.xml");
    let (first_xml, second_xml) = tokio::join!(fetch_text(http, &first), fetch_text(http, &second));
    for xml in [first_xml, second_xml].into_iter().flatten() {
        match autoconfig::parse(&xml, email, "autoconfig") {
            Ok(mut result) => {
                tracing::debug!(%domain, "discovery matched the domain autoconfig file");
                attach_dav(http, &domain, &mut result).await;
                return Ok(result);
            }
            Err(error) => {
                tracing::debug!(%error, %domain, "autoconfig document could not be used");
            }
        }
    }

    // 4. MX inference. The MX entry's directory DAV URLs win; `attach_dav`
    // only probes the custom domain's well-known locations when the entry has
    // none (or is missing one side).
    let mx_hosts = mx::lookup_mx(&domain).await;
    if let Some(preset) = presets::match_mx(&mx_hosts) {
        tracing::debug!(%domain, ?mx_hosts, "discovery matched an MX provider hint");
        let mut result = preset.into_result(email);
        attach_dav(http, &domain, &mut result).await;
        return Ok(result);
    }

    // 5. Generic guess.
    tracing::debug!(%domain, ?mx_hosts, "discovery fell back to a host guess");
    let mut result = guess_result(&domain, email);
    attach_dav(http, &domain, &mut result).await;
    Ok(result)
}

/// Generic `imap.<domain>` / `smtp.<domain>` fallback.
pub fn guess_result(domain: &str, email: &str) -> DiscoveryResult {
    DiscoveryResult {
        provider: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        imap: Some(ServerConfig::new(
            format!("imap.{domain}"),
            993,
            Security::Tls,
            email,
        )),
        smtp: Some(ServerConfig::new(
            format!("smtp.{domain}"),
            465,
            Security::Tls,
            email,
        )),
        pop3: None,
        caldav_url: None,
        carddav_url: None,
        source: "guess".to_string(),
        note: Some(presets::NOTE_GUESSED.to_string()),
    }
}

/// Discover CalDAV/CardDAV endpoints using a fresh HTTP client.
pub async fn discover_dav(email_or_domain: &str) -> dav::DavUrls {
    match crate::http::http_client() {
        Ok(http) => discover_dav_with_client(&http, email_or_domain).await,
        Err(error) => {
            tracing::debug!(%error, "cannot create an HTTP client for DAV discovery");
            dav::DavUrls::default()
        }
    }
}

/// Discover CalDAV/CardDAV endpoints with a caller-supplied HTTP client.
pub async fn discover_dav_with_client(
    http: &reqwest::Client,
    email_or_domain: &str,
) -> dav::DavUrls {
    let value = email_or_domain.trim();
    let domain = if value.contains('@') {
        match split_email(value) {
            Ok((_, domain)) => domain,
            Err(_) => return dav::DavUrls::default(),
        }
    } else {
        value.trim_end_matches('.').to_ascii_lowercase()
    };
    dav::discover(http, &domain).await
}

async fn attach_dav(http: &reqwest::Client, domain: &str, result: &mut DiscoveryResult) {
    if result.caldav_url.is_some() && result.carddav_url.is_some() {
        return;
    }
    let urls = dav::discover(http, domain).await;
    if result.caldav_url.is_none() {
        result.caldav_url = urls.caldav_url;
    }
    if result.carddav_url.is_none() {
        result.carddav_url = urls.carddav_url;
    }
}

/// Fetch a small text document with the discovery timeout.
async fn fetch_text(http: &reqwest::Client, url: &str) -> Option<String> {
    let response = http
        .get(url)
        .timeout(DISCOVERY_TIMEOUT)
        .send()
        .await
        .map_err(|error| {
            tracing::debug!(%error, url, "autoconfig request failed");
            error
        })
        .ok()?;

    if !response.status().is_success() {
        tracing::debug!(
            url,
            status = response.status().as_u16(),
            "autoconfig returned an error status"
        );
        return None;
    }
    if response
        .content_length()
        .map(|length| length > MAX_CONFIG_BYTES as u64)
        .unwrap_or(false)
    {
        tracing::debug!(url, "autoconfig document is too large; ignored");
        return None;
    }

    let text = response.text().await.ok()?;
    if text.len() > MAX_CONFIG_BYTES {
        return None;
    }
    Some(text)
}

/// Convenience: a client suitable for discovery when the caller has none.
pub fn discovery_http_client() -> Result<reqwest::Client> {
    crate::http::http_client()
}

/// Default timeout re-exported for callers that build their own requests.
pub const DEFAULT_TIMEOUT: Duration = DISCOVERY_TIMEOUT;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_valid_addresses() {
        assert_eq!(
            split_email("User@Example.COM").expect("valid"),
            ("User".to_string(), "example.com".to_string())
        );
        assert_eq!(
            split_email("  a@b.example. ").expect("valid"),
            ("a".to_string(), "b.example".to_string())
        );
    }

    #[test]
    fn rejects_invalid_addresses() {
        for value in ["", "nobody", "a@b", "@example.com", "a b@example.com"] {
            assert!(split_email(value).is_err(), "{value} should be rejected");
        }
    }

    #[test]
    fn guess_uses_standard_hosts_and_documents_itself() {
        let result = guess_result("example.org", "user@example.org");
        assert_eq!(result.provider, ProviderKind::Imap);
        assert_eq!(result.source, "guess");
        let imap = result.imap.expect("imap");
        assert_eq!(imap.host, "imap.example.org");
        assert_eq!(imap.port, 993);
        assert_eq!(imap.username, "user@example.org");
        assert_eq!(result.smtp.expect("smtp").host, "smtp.example.org");
        assert_eq!(
            result.note.as_deref(),
            Some(presets::NOTE_GUESSED),
            "the note is a stable i18n key"
        );
    }

    #[tokio::test]
    async fn preset_discovery_is_offline() {
        let client = crate::http::http_client().expect("client");
        let result = discover_with_client(&client, "someone@gmail.com", false)
            .await
            .expect("discover");
        assert_eq!(result.provider, ProviderKind::Gmail);
        assert_eq!(result.source, "preset");
        assert_eq!(result.auth_kind, AuthKind::Oauth);
    }

    #[tokio::test]
    async fn preset_carries_directory_dav_without_probing() {
        // allow_ispdb = false and a preset hit means no network at all; 163 has
        // no CalDAV in the verified directory, and probing caldav.163.com only
        // yields a 403, so the field must stay empty.
        let client = crate::http::http_client().expect("client");
        let result = discover_with_client(&client, "a@163.com", false)
            .await
            .expect("discover");
        assert_eq!(result.source, "preset");
        assert_eq!(result.caldav_url, None);
        assert_eq!(
            result.carddav_url.as_deref(),
            Some("https://contacts.163.com")
        );
    }

    #[tokio::test]
    async fn tuta_is_a_final_preset_without_mail_servers() {
        let client = crate::http::http_client().expect("client");
        let result = discover_with_client(&client, "a@tuta.com", false)
            .await
            .expect("discover");
        assert_eq!(result.source, "preset");
        assert!(result.imap.is_none());
        assert!(result.smtp.is_none());
        assert_eq!(result.note.as_deref(), Some(presets::NOTE_NO_IMAP));
    }

    #[tokio::test]
    async fn invalid_email_is_rejected_before_any_network() {
        let client = crate::http::http_client().expect("client");
        let error = discover_with_client(&client, "not-an-email", true)
            .await
            .expect_err("must fail");
        assert_eq!(error.code(), "invalidInput");
    }

    #[tokio::test]
    #[ignore = "requires network access"]
    async fn discovers_a_real_domain_via_autoconfig() {
        let client = crate::http::http_client().expect("client");
        let result = discover_with_client(&client, "user@fastmail.com", true)
            .await
            .expect("discover");
        // Preset wins over ISPDB, but the result must be usable.
        assert!(result.imap.is_some());
    }
}
