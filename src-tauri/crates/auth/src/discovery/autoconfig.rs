//! Mozilla autoconfig (`config-v1.1.xml`) and Thunderbird ISPDB parsing.
//!
//! Both use the same XML dialect; ISPDB is just a hosted copy of the file.
//! We keep the *preferred* incoming/outgoing servers: SSL first, then STARTTLS,
//! then plaintext (the UI can warn about the latter).

use serde::Deserialize;

use nuncio_core::error::AppError;
use nuncio_core::models::{AuthKind, DiscoveryResult, ProviderKind, Security, ServerConfig};

#[derive(Debug, Default, Deserialize)]
struct ClientConfig {
    #[serde(rename = "emailProvider", default)]
    providers: Vec<EmailProvider>,
}

#[derive(Debug, Default, Deserialize)]
struct EmailProvider {
    #[serde(rename = "incomingServer", default)]
    incoming: Vec<XmlServer>,
    #[serde(rename = "outgoingServer", default)]
    outgoing: Vec<XmlServer>,
}

#[derive(Debug, Default, Deserialize)]
struct XmlServer {
    #[serde(rename = "@type", default)]
    server_type: Option<String>,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(rename = "socketType", default)]
    socket_type: Option<String>,
    #[serde(default)]
    username: Option<String>,
}

fn security_rank(security: Security) -> u8 {
    match security {
        Security::Tls => 0,
        Security::Starttls => 1,
        Security::None => 2,
    }
}

fn parse_security(socket_type: Option<&str>) -> Security {
    let value = socket_type.unwrap_or_default().trim().to_ascii_lowercase();
    if value.contains("starttls") {
        Security::Starttls
    } else if value.contains("ssl") || value == "tls" || value.contains("ssl/tls") {
        Security::Tls
    } else {
        Security::None
    }
}

fn substitute(value: &str, email: &str) -> String {
    let local_part = email.split('@').next().unwrap_or(email);
    let replaced = value
        .trim()
        .replace("%EMAILADDRESS%", email)
        .replace("%EMAILLOCALPART%", local_part);
    if replaced.is_empty() {
        email.to_string()
    } else {
        replaced
    }
}

fn default_port(kind: &str, security: Security) -> u16 {
    match (kind, security) {
        ("imap", Security::Tls) => 993,
        ("imap", _) => 143,
        ("pop3", Security::Tls) => 995,
        ("pop3", _) => 110,
        ("smtp", Security::Tls) => 465,
        ("smtp", _) => 587,
        _ => 0,
    }
}

fn build_server(server: &XmlServer, kind: &str, email: &str) -> Option<ServerConfig> {
    let host = server
        .hostname
        .as_deref()
        .map(str::trim)
        .filter(|host| !host.is_empty())?
        .to_string();
    let security = parse_security(server.socket_type.as_deref());
    let port = server
        .port
        .filter(|port| *port != 0)
        .unwrap_or_else(|| default_port(kind, security));
    let username = substitute(
        server.username.as_deref().unwrap_or("%EMAILADDRESS%"),
        email,
    );
    Some(ServerConfig::new(host, port, security, username))
}

fn pick<'a>(servers: &'a [XmlServer], kind: &str) -> Option<&'a XmlServer> {
    let mut candidates: Vec<&XmlServer> = servers
        .iter()
        .filter(|server| {
            server
                .server_type
                .as_deref()
                .map(|value| value.eq_ignore_ascii_case(kind))
                .unwrap_or(false)
                && server
                    .hostname
                    .as_deref()
                    .map(|host| !host.trim().is_empty())
                    .unwrap_or(false)
        })
        .collect();
    candidates.sort_by_key(|server| security_rank(parse_security(server.socket_type.as_deref())));
    candidates.first().copied()
}

/// Parse an autoconfig document.  `source` is `"ispdb"` or `"autoconfig"`.
pub fn parse(xml: &str, email: &str, source: &str) -> Result<DiscoveryResult, AppError> {
    let config: ClientConfig = quick_xml::de::from_str(xml)
        .map_err(|error| AppError::protocol(format!("invalid autoconfig XML: {error}")))?;
    let provider = config
        .providers
        .into_iter()
        .next()
        .ok_or_else(|| AppError::protocol("autoconfig XML has no emailProvider element"))?;

    let imap =
        pick(&provider.incoming, "imap").and_then(|server| build_server(server, "imap", email));
    let pop3 = if imap.is_none() {
        pick(&provider.incoming, "pop3").and_then(|server| build_server(server, "pop3", email))
    } else {
        None
    };
    let smtp =
        pick(&provider.outgoing, "smtp").and_then(|server| build_server(server, "smtp", email));

    if imap.is_none() && pop3.is_none() {
        return Err(AppError::protocol(
            "autoconfig XML has no usable incoming server",
        ));
    }

    Ok(DiscoveryResult {
        provider: if imap.is_some() {
            ProviderKind::Imap
        } else {
            ProviderKind::Pop3
        },
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        imap,
        smtp,
        pop3,
        caldav_url: None,
        carddav_url: None,
        source: source.to_string(),
        note: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<clientConfig version="1.1">
  <emailProvider id="example.com">
    <domain>example.com</domain>
    <displayName>Example Mail</displayName>
    <incomingServer type="imap">
      <hostname>imap.example.com</hostname>
      <port>993</port>
      <socketType>SSL</socketType>
      <username>%EMAILADDRESS%</username>
      <authentication>password-cleartext</authentication>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>smtp.example.com</hostname>
      <port>465</port>
      <socketType>SSL</socketType>
      <username>%EMAILLOCALPART%</username>
      <authentication>password-cleartext</authentication>
    </outgoingServer>
  </emailProvider>
</clientConfig>"#;

    #[test]
    fn parses_ispdb_document() {
        let result = parse(SAMPLE, "user@example.com", "ispdb").expect("parse");
        assert_eq!(result.provider, ProviderKind::Imap);
        assert_eq!(result.auth_kind, AuthKind::Password);
        assert_eq!(result.source, "ispdb");

        let imap = result.imap.expect("imap");
        assert_eq!(imap.host, "imap.example.com");
        assert_eq!(imap.port, 993);
        assert_eq!(imap.security, Security::Tls);
        assert_eq!(imap.username, "user@example.com");

        let smtp = result.smtp.expect("smtp");
        assert_eq!(smtp.host, "smtp.example.com");
        assert_eq!(smtp.port, 465);
        assert_eq!(smtp.username, "user");
    }

    #[test]
    fn prefers_ssl_over_starttls() {
        let xml = r#"<clientConfig><emailProvider>
            <incomingServer type="imap">
              <hostname>plain.example.com</hostname><socketType>STARTTLS</socketType>
            </incomingServer>
            <incomingServer type="imap">
              <hostname>secure.example.com</hostname><socketType>SSL</socketType>
            </incomingServer>
            <outgoingServer type="smtp">
              <hostname>smtp.example.com</hostname><socketType>STARTTLS</socketType>
            </outgoingServer>
        </emailProvider></clientConfig>"#;
        let result = parse(xml, "user@example.com", "autoconfig").expect("parse");
        let imap = result.imap.expect("imap");
        assert_eq!(imap.host, "secure.example.com");
        // Default port for implicit TLS.
        assert_eq!(imap.port, 993);
        let smtp = result.smtp.expect("smtp");
        assert_eq!(smtp.security, Security::Starttls);
        assert_eq!(smtp.port, 587);
    }

    #[test]
    fn supports_pop3_when_no_imap_exists() {
        let xml = r#"<clientConfig><emailProvider>
            <incomingServer type="pop3">
              <hostname>pop.example.com</hostname><socketType>SSL</socketType>
            </incomingServer>
        </emailProvider></clientConfig>"#;
        let result = parse(xml, "user@example.com", "ispdb").expect("parse");
        assert_eq!(result.provider, ProviderKind::Pop3);
        assert!(result.imap.is_none());
        let pop3 = result.pop3.expect("pop3");
        assert_eq!(pop3.host, "pop.example.com");
        assert_eq!(pop3.port, 995);
    }

    #[test]
    fn malformed_xml_is_rejected() {
        assert!(parse("<clientConfig><emailProvider>", "a@b.c", "ispdb").is_err());
        assert!(parse("<clientConfig></clientConfig>", "a@b.c", "ispdb").is_err());
    }
}
