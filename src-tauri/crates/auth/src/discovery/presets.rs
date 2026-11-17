//! Provider presets backed by the verified directory in `nuncio-core`.
//!
//! Presets give instant, offline discovery for the most common consumer
//! providers (including the Chinese ones from the product plan) and are also
//! used when an MX lookup identifies the provider behind a custom domain.
//!
//! This module deliberately carries no provider data of its own: hosts, DAV
//! endpoints, notes and auth kinds all come from [`nuncio_core::providers`], so
//! discovery and account creation cannot drift apart.

use nuncio_core::models::{
    AuthKind, DiscoveryResult, OAuthProvider, ProviderKind, Security, ServerConfig,
};
use nuncio_core::providers::{self, ProviderInfo};

/// One host/port/security triple before the username is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    pub host: String,
    pub port: u16,
    pub security: Security,
}

impl ServerSpec {
    pub fn new(host: impl Into<String>, port: u16, security: Security) -> Self {
        Self {
            host: host.into(),
            port,
            security,
        }
    }

    pub fn config(&self, username: &str) -> ServerConfig {
        ServerConfig::new(self.host.clone(), self.port, self.security, username)
    }
}

/// i18n keys for discovery hints (translated by the settings UI).
pub const NOTE_AUTH_CODE: &str = "discovery.note.authCode";
pub const NOTE_AUTH_CODE_ENABLE: &str = "discovery.note.authCodeEnable";
pub const NOTE_APP_PASSWORD_APPLE: &str = "discovery.note.appPasswordApple";
pub const NOTE_APP_PASSWORD_YAHOO: &str = "discovery.note.appPasswordYahoo";
pub const NOTE_APP_PASSWORD_YANDEX: &str = "discovery.note.appPasswordYandex";
pub const NOTE_APP_PASSWORD: &str = "discovery.note.appPassword";
pub const NOTE_PROTON_BRIDGE: &str = "discovery.note.protonBridge";
pub const NOTE_GUESSED: &str = "discovery.note.guessed";
pub const NOTE_ENTERPRISE: &str = "discovery.note.enterprise";
pub const NOTE_ENABLE_IMAP: &str = "discovery.note.enableImap";
pub const NOTE_NO_IMAP: &str = "discovery.note.noImap";

/// A provider preset before the account e-mail is applied.
#[derive(Debug, Clone, PartialEq)]
pub struct Preset {
    pub provider: ProviderKind,
    pub auth_kind: AuthKind,
    pub oauth_provider: Option<OAuthProvider>,
    pub imap: Option<ServerSpec>,
    pub smtp: Option<ServerSpec>,
    pub pop3: Option<ServerSpec>,
    pub caldav_url: Option<String>,
    pub carddav_url: Option<String>,
    pub note: Option<String>,
    pub source: &'static str,
}

impl Preset {
    pub fn into_result(self, email: &str) -> DiscoveryResult {
        DiscoveryResult {
            provider: self.provider,
            auth_kind: self.auth_kind,
            oauth_provider: self.oauth_provider,
            imap: self.imap.map(|spec| spec.config(email)),
            smtp: self.smtp.map(|spec| spec.config(email)),
            pop3: self.pop3.map(|spec| spec.config(email)),
            caldav_url: self.caldav_url,
            carddav_url: self.carddav_url,
            source: self.source.to_string(),
            note: self.note,
        }
    }

    fn with_source(mut self, source: &'static str) -> Self {
        self.source = source;
        self
    }
}

/// Build a preset from a directory row.
///
/// `domain` resolves `{domain}` host templates (NetEase brands). It must be the
/// account's own domain: for an MX hit the provider is behind a custom domain,
/// but no MX-reachable entry uses a template, so a placeholder is harmless.
fn preset_from_info(info: &ProviderInfo, domain: &str) -> Preset {
    let spec = |template: Option<nuncio_core::providers::ServerTemplate>| {
        template.map(|template| {
            ServerSpec::new(template.host_for(domain), template.port, template.security)
        })
    };
    Preset {
        provider: info.kind,
        auth_kind: info.auth_kind,
        oauth_provider: info.oauth_provider,
        imap: spec(info.imap),
        smtp: spec(info.smtp),
        pop3: None,
        caldav_url: info.caldav.map(str::to_string),
        carddav_url: info.carddav.map(str::to_string),
        note: info.note.map(str::to_string),
        source: "preset",
    }
}

/// Match an exact e-mail domain against the built-in directory.
pub fn lookup(domain: &str) -> Option<Preset> {
    let info = providers::by_domain(domain)?;
    Some(preset_from_info(info, domain))
}

/// Match MX hosts (in preference order) to a provider preset.
pub fn match_mx(hosts: &[String]) -> Option<Preset> {
    let info = providers::by_mx(hosts)?;
    // `match_mx` has no account domain; the directory's first domain is only
    // used to fold host templates, which no MX-matched entry has today.
    let domain = info.domains.first().copied().unwrap_or_default();
    Some(preset_from_info(info, domain).with_source("mx"))
}

/// Preset CalDAV/CardDAV endpoints for a domain, if the provider is known.
///
/// `Some((None, None))` is meaningful: the provider is in the directory and has
/// no DAV service (e.g. 163), so callers must not probe well-known either.
pub fn preset_dav(domain: &str) -> Option<(Option<String>, Option<String>)> {
    let info = providers::by_domain(domain)?;
    Some((
        info.caldav.map(str::to_string),
        info.carddav.map(str::to_string),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gmail_lookup_uses_oauth_and_tls() {
        let preset = lookup("gmail.com").expect("gmail");
        let result = preset.into_result("User@Gmail.com");
        assert_eq!(result.provider, ProviderKind::Gmail);
        assert_eq!(result.auth_kind, AuthKind::Oauth);
        assert_eq!(result.oauth_provider, Some(OAuthProvider::Google));
        let imap = result.imap.expect("imap");
        assert_eq!(imap.host, "imap.gmail.com");
        assert_eq!(imap.port, 993);
        assert_eq!(imap.security, Security::Tls);
        assert_eq!(imap.username, "User@Gmail.com");
        assert_eq!(result.smtp.expect("smtp").port, 465);
        assert_eq!(result.source, "preset");
    }

    #[test]
    fn outlook_lookup_uses_starttls_smtp() {
        let result = lookup("hotmail.com")
            .expect("outlook")
            .into_result("a@hotmail.com");
        assert_eq!(result.provider, ProviderKind::Outlook);
        assert_eq!(result.oauth_provider, Some(OAuthProvider::Microsoft));
        let smtp = result.smtp.expect("smtp");
        assert_eq!(smtp.host, "smtp-mail.outlook.com");
        assert_eq!(smtp.port, 587);
        assert_eq!(smtp.security, Security::Starttls);
    }

    #[test]
    fn outlook_cn_is_not_a_consumer_domain() {
        assert!(lookup("outlook.cn").is_none());
    }

    #[test]
    fn chinese_providers_have_notes() {
        for (domain, provider, note) in [
            ("qq.com", ProviderKind::Qq, NOTE_AUTH_CODE_ENABLE),
            ("163.com", ProviderKind::Netease, NOTE_AUTH_CODE),
            ("126.com", ProviderKind::Netease, NOTE_AUTH_CODE),
            ("aliyun.com", ProviderKind::Imap, NOTE_AUTH_CODE),
        ] {
            let result = lookup(domain)
                .expect(domain)
                .into_result(&format!("a@{domain}"));
            assert_eq!(result.provider, provider);
            assert_eq!(result.auth_kind, AuthKind::Password);
            assert_eq!(result.note.as_deref(), Some(note), "{domain}");
        }

        for (domain, key) in [
            ("exmail.qq.com", NOTE_ENTERPRISE),
            ("yahoo.com", NOTE_APP_PASSWORD_YAHOO),
            ("yandex.ru", NOTE_APP_PASSWORD_YANDEX),
            ("icloud.com", NOTE_APP_PASSWORD_APPLE),
            ("proton.me", NOTE_PROTON_BRIDGE),
            ("zoho.com", NOTE_ENABLE_IMAP),
        ] {
            let result = lookup(domain)
                .expect(domain)
                .into_result(&format!("a@{domain}"));
            assert_eq!(result.note.as_deref(), Some(key), "{domain}");
        }

        for (mx, domain) in [
            ("mxbiz1.qq.com", "a@corp.example"),
            ("mx.qiye.163.com", "a@corp.example"),
            ("mx.qiye.aliyun.com", "a@corp.example"),
        ] {
            let preset = match_mx(&[mx.to_string()]).expect(mx);
            assert_eq!(
                preset.note.as_deref(),
                Some(NOTE_ENTERPRISE),
                "{mx} should use the enterprise note"
            );
            assert!(preset.into_result(domain).imap.is_some());
        }

        let netease = lookup("126.com").expect("netease").into_result("a@126.com");
        assert_eq!(netease.imap.expect("imap").host, "imap.126.com");
        assert_eq!(netease.smtp.expect("smtp").host, "smtp.126.com");
    }

    #[test]
    fn icloud_and_fastmail_include_dav() {
        let icloud = lookup("icloud.com")
            .expect("icloud")
            .into_result("a@icloud.com");
        assert_eq!(
            icloud.caldav_url.as_deref(),
            Some("https://caldav.icloud.com")
        );
        assert_eq!(
            icloud.carddav_url.as_deref(),
            Some("https://contacts.icloud.com")
        );
        assert_eq!(icloud.note.as_deref(), Some(NOTE_APP_PASSWORD_APPLE));

        let fastmail = lookup("fastmail.com")
            .expect("fastmail")
            .into_result("a@fastmail.com");
        assert_eq!(fastmail.note.as_deref(), Some(NOTE_APP_PASSWORD));
        assert!(fastmail
            .caldav_url
            .expect("caldav")
            .contains("caldav.fastmail.com"));
        assert!(fastmail
            .carddav_url
            .expect("carddav")
            .contains("carddav.fastmail.com"));
    }

    #[test]
    fn yandex_lookup_carries_the_ru_dav_hosts() {
        let result = lookup("yandex.ru")
            .expect("yandex")
            .into_result("a@yandex.ru");
        assert_eq!(
            result.caldav_url.as_deref(),
            Some("https://caldav.yandex.ru")
        );
        assert_eq!(
            result.carddav_url.as_deref(),
            Some("https://carddav.yandex.ru")
        );
    }

    #[test]
    fn sina_cn_uses_its_own_brand_host() {
        let result = lookup("sina.cn").expect("sina.cn").into_result("a@sina.cn");
        assert_eq!(result.imap.expect("imap").host, "imap.sina.cn");
    }

    #[test]
    fn netease_brands_use_per_domain_hosts_and_the_full_address() {
        let result = lookup("126.com")
            .expect("126.com")
            .into_result("someone@126.com");
        let imap = result.imap.expect("imap");
        assert_eq!(imap.host, "imap.126.com");
        assert_eq!(imap.username, "someone@126.com");
    }

    #[test]
    fn proton_uses_bridge() {
        let result = lookup("proton.me")
            .expect("proton")
            .into_result("a@proton.me");
        assert_eq!(result.imap.expect("imap").host, "127.0.0.1");
        assert_eq!(result.note.as_deref(), Some(NOTE_PROTON_BRIDGE));
    }

    #[test]
    fn unknown_domain_has_no_preset() {
        assert!(lookup("example.org").is_none());
    }

    #[test]
    fn mx_matching() {
        let hosts = vec!["aspmx.l.google.com.".to_string()];
        let result = match_mx(&hosts)
            .expect("google")
            .into_result("a@custom.com");
        assert_eq!(result.provider, ProviderKind::Gmail);
        assert_eq!(result.source, "mx");

        let hosts = vec!["custom-com.mail.protection.outlook.com.".to_string()];
        assert_eq!(
            match_mx(&hosts).expect("outlook").provider,
            ProviderKind::Outlook
        );

        let hosts = vec!["mxw.mxhichina.com.".to_string()];
        let result = match_mx(&hosts).expect("aliyun").into_result("a@corp.cn");
        assert_eq!(result.imap.expect("imap").host, "imap.qiye.aliyun.com");

        let hosts = vec!["mx1.qiye.163.com.".to_string()];
        assert_eq!(
            match_mx(&hosts).expect("netease").imap.expect("imap").host,
            "imap.qiye.163.com"
        );

        let hosts = vec!["mxbiz1.qq.com.".to_string()];
        assert_eq!(
            match_mx(&hosts).expect("exmail").imap.expect("imap").host,
            "imap.exmail.qq.com"
        );

        assert!(match_mx(&["mx.example.org".to_string()]).is_none());
    }

    #[test]
    fn consumer_netease_mx_is_not_the_enterprise_preset() {
        assert!(match_mx(&["163mx01.mxmail.netease.com".to_string()]).is_none());
    }

    #[test]
    fn dav_presets() {
        let (caldav, carddav) = preset_dav("icloud.com").expect("icloud dav");
        assert!(caldav.expect("caldav").contains("caldav.icloud.com"));
        assert!(carddav.expect("carddav").contains("contacts.icloud.com"));

        // A known provider without DAV must report "known, nothing to probe".
        assert_eq!(
            preset_dav("163.com"),
            Some((None, Some("https://contacts.163.com".into())))
        );
        assert_eq!(preset_dav("126.com"), Some((None, None)));
        assert!(preset_dav("example.org").is_none());
    }
}
