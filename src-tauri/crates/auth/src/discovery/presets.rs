//! Built-in provider presets and MX-record based matching.
//!
//! Presets give instant, offline discovery for the most common consumer
//! providers (including the Chinese ones from the product plan) and are also
//! used when an MX lookup identifies the provider behind a custom domain.

use nuncio_core::models::{
    AuthKind, DiscoveryResult, OAuthProvider, ProviderKind, Security, ServerConfig,
};

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
pub const NOTE_APP_PASSWORD_APPLE: &str = "discovery.note.appPasswordApple";
pub const NOTE_APP_PASSWORD_YAHOO: &str = "discovery.note.appPasswordYahoo";
pub const NOTE_APP_PASSWORD: &str = "discovery.note.appPassword";
pub const NOTE_PROTON_BRIDGE: &str = "discovery.note.protonBridge";
pub const NOTE_GUESSED: &str = "discovery.note.guessed";
pub const NOTE_ENTERPRISE: &str = "discovery.note.enterprise";

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

fn server(host: impl Into<String>, port: u16, security: Security) -> Option<ServerSpec> {
    Some(ServerSpec::new(host, port, security))
}

fn oauth_preset(
    provider: ProviderKind,
    oauth: OAuthProvider,
    imap: Option<ServerSpec>,
    smtp: Option<ServerSpec>,
) -> Preset {
    Preset {
        provider,
        auth_kind: AuthKind::Oauth,
        oauth_provider: Some(oauth),
        imap,
        smtp,
        pop3: None,
        caldav_url: None,
        carddav_url: None,
        note: None,
        source: "preset",
    }
}

fn password_preset(
    provider: ProviderKind,
    imap: Option<ServerSpec>,
    smtp: Option<ServerSpec>,
    note: Option<&str>,
) -> Preset {
    Preset {
        provider,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        imap,
        smtp,
        pop3: None,
        caldav_url: None,
        carddav_url: None,
        note: note.map(str::to_string),
        source: "preset",
    }
}

/// Gmail / Google Workspace (consumer domains).
pub fn gmail_preset() -> Preset {
    oauth_preset(
        ProviderKind::Gmail,
        OAuthProvider::Google,
        server("imap.gmail.com", 993, Security::Tls),
        server("smtp.gmail.com", 465, Security::Tls),
    )
}

/// Outlook.com / Microsoft 365.
pub fn outlook_preset() -> Preset {
    oauth_preset(
        ProviderKind::Outlook,
        OAuthProvider::Microsoft,
        server("outlook.office365.com", 993, Security::Tls),
        server("smtp.office365.com", 587, Security::Starttls),
    )
}

/// QQ / Foxmail consumer mail.
pub fn qq_preset() -> Preset {
    password_preset(
        ProviderKind::Qq,
        server("imap.qq.com", 993, Security::Tls),
        server("smtp.qq.com", 465, Security::Tls),
        Some(NOTE_AUTH_CODE),
    )
}

/// Tencent Exmail (enterprise QQ mail), including custom domains via MX.
pub fn exmail_preset() -> Preset {
    password_preset(
        ProviderKind::Qq,
        server("imap.exmail.qq.com", 993, Security::Tls),
        server("smtp.exmail.qq.com", 465, Security::Tls),
        Some(NOTE_ENTERPRISE),
    )
}

/// Netease 163 / 126 / yeah / vip / 188.
pub fn netease_preset(domain: &str) -> Preset {
    password_preset(
        ProviderKind::Netease,
        server(format!("imap.{domain}"), 993, Security::Tls),
        server(format!("smtp.{domain}"), 465, Security::Tls),
        Some(NOTE_AUTH_CODE),
    )
}

/// Netease enterprise mail (`qiye.163.com`).
pub fn netease_enterprise_preset() -> Preset {
    password_preset(
        ProviderKind::Netease,
        server("imap.qiye.163.com", 993, Security::Tls),
        server("smtp.qiye.163.com", 465, Security::Tls),
        Some(NOTE_ENTERPRISE),
    )
}

/// Alibaba enterprise mail.
pub fn aliyun_enterprise_preset() -> Preset {
    password_preset(
        ProviderKind::Imap,
        server("imap.qiye.aliyun.com", 993, Security::Tls),
        server("smtp.qiye.aliyun.com", 465, Security::Tls),
        Some(NOTE_ENTERPRISE),
    )
}

/// Match an exact e-mail domain against the built-in table.
pub fn lookup(domain: &str) -> Option<Preset> {
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    let preset = match domain.as_str() {
        "gmail.com" | "googlemail.com" => gmail_preset(),

        "outlook.com" | "hotmail.com" | "live.com" | "msn.com" | "outlook.cn" => outlook_preset(),

        "icloud.com" | "me.com" | "mac.com" => Preset {
            provider: ProviderKind::Icloud,
            auth_kind: AuthKind::Password,
            oauth_provider: None,
            imap: server("imap.mail.me.com", 993, Security::Tls),
            smtp: server("smtp.mail.me.com", 587, Security::Starttls),
            pop3: None,
            caldav_url: Some("https://caldav.icloud.com".to_string()),
            carddav_url: Some("https://contacts.icloud.com".to_string()),
            note: Some(NOTE_APP_PASSWORD_APPLE.to_string()),
            source: "preset",
        },

        "yahoo.com" | "ymail.com" => password_preset(
            ProviderKind::Yahoo,
            server("imap.mail.yahoo.com", 993, Security::Tls),
            server("smtp.mail.yahoo.com", 465, Security::Tls),
            Some(NOTE_APP_PASSWORD_YAHOO),
        ),

        "qq.com" | "foxmail.com" | "vip.qq.com" => qq_preset(),
        "exmail.qq.com" => exmail_preset(),

        "163.com" | "126.com" | "yeah.net" | "vip.163.com" | "188.com" => netease_preset(&domain),

        "aliyun.com" => password_preset(
            ProviderKind::Imap,
            server("imap.aliyun.com", 993, Security::Tls),
            server("smtp.aliyun.com", 465, Security::Tls),
            Some(NOTE_AUTH_CODE),
        ),

        "139.com" => password_preset(
            ProviderKind::Imap,
            server("imap.139.com", 993, Security::Tls),
            server("smtp.139.com", 465, Security::Tls),
            Some(NOTE_AUTH_CODE),
        ),

        "sina.com" | "sina.cn" => password_preset(
            ProviderKind::Imap,
            server("imap.sina.com", 993, Security::Tls),
            server("smtp.sina.com", 465, Security::Tls),
            Some(NOTE_AUTH_CODE),
        ),

        "sohu.com" => password_preset(
            ProviderKind::Imap,
            server("imap.sohu.com", 993, Security::Tls),
            server("smtp.sohu.com", 465, Security::Tls),
            Some(NOTE_AUTH_CODE),
        ),

        "fastmail.com" => Preset {
            caldav_url: Some("https://caldav.fastmail.com/dav/calendars".to_string()),
            carddav_url: Some("https://carddav.fastmail.com/dav/addressbooks".to_string()),
            ..password_preset(
                ProviderKind::Imap,
                server("imap.fastmail.com", 993, Security::Tls),
                server("smtp.fastmail.com", 465, Security::Tls),
                Some(NOTE_APP_PASSWORD),
            )
        },

        "zoho.com" => password_preset(
            ProviderKind::Imap,
            server("imap.zoho.com", 993, Security::Tls),
            server("smtp.zoho.com", 465, Security::Tls),
            Some(NOTE_APP_PASSWORD),
        ),

        "yandex.com" | "yandex.ru" => password_preset(
            ProviderKind::Imap,
            server("imap.yandex.com", 993, Security::Tls),
            server("smtp.yandex.com", 465, Security::Tls),
            Some(NOTE_APP_PASSWORD),
        ),

        "gmx.com" | "gmx.net" => password_preset(
            ProviderKind::Imap,
            server("imap.gmx.com", 993, Security::Tls),
            server("smtp.gmx.com", 587, Security::Starttls),
            Some(NOTE_APP_PASSWORD),
        ),

        "aol.com" => password_preset(
            ProviderKind::Imap,
            server("imap.aol.com", 993, Security::Tls),
            server("smtp.aol.com", 465, Security::Tls),
            Some(NOTE_APP_PASSWORD),
        ),

        "mail.ru" => password_preset(
            ProviderKind::Imap,
            server("imap.mail.ru", 993, Security::Tls),
            server("smtp.mail.ru", 465, Security::Tls),
            Some(NOTE_APP_PASSWORD),
        ),

        "proton.me" | "protonmail.com" | "pm.me" => password_preset(
            ProviderKind::Imap,
            server("127.0.0.1", 1143, Security::Starttls),
            server("127.0.0.1", 1025, Security::Starttls),
            Some(NOTE_PROTON_BRIDGE),
        ),

        _ => return None,
    };
    Some(preset)
}

/// Match MX hosts (in preference order) to a provider preset.
pub fn match_mx(hosts: &[String]) -> Option<Preset> {
    for host in hosts {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() {
            continue;
        }

        if host.contains("google.com") || host.contains("googlemail.com") {
            return Some(gmail_preset().with_source("mx"));
        }
        if host.contains("outlook.com") || host.contains("protection.outlook.com") {
            return Some(outlook_preset().with_source("mx"));
        }
        if host.contains("mxhichina.com") || host.contains("qiye.aliyun.com") {
            return Some(aliyun_enterprise_preset().with_source("mx"));
        }
        if host.contains("qiye.163.com") || host.contains("163.com") || host.contains("netease.com")
        {
            return Some(netease_enterprise_preset().with_source("mx"));
        }
        if host.contains("qq.com") {
            return Some(exmail_preset().with_source("mx"));
        }
    }
    None
}

/// Preset CalDAV/CardDAV endpoints for a domain, if known.
pub fn preset_dav(domain: &str) -> Option<(Option<String>, Option<String>)> {
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    match domain.as_str() {
        "icloud.com" | "me.com" | "mac.com" => Some((
            Some("https://caldav.icloud.com".to_string()),
            Some("https://contacts.icloud.com".to_string()),
        )),
        "fastmail.com" => Some((
            Some("https://caldav.fastmail.com/dav/calendars".to_string()),
            Some("https://carddav.fastmail.com/dav/addressbooks".to_string()),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gmail_preset_uses_oauth_and_tls() {
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
    fn outlook_preset_uses_starttls_smtp() {
        let result = lookup("hotmail.com")
            .expect("outlook")
            .into_result("a@hotmail.com");
        assert_eq!(result.provider, ProviderKind::Outlook);
        assert_eq!(result.oauth_provider, Some(OAuthProvider::Microsoft));
        let smtp = result.smtp.expect("smtp");
        assert_eq!(smtp.host, "smtp.office365.com");
        assert_eq!(smtp.port, 587);
        assert_eq!(smtp.security, Security::Starttls);
    }

    #[test]
    fn chinese_providers_have_notes() {
        for (domain, provider) in [
            ("qq.com", ProviderKind::Qq),
            ("163.com", ProviderKind::Netease),
            ("126.com", ProviderKind::Netease),
            ("aliyun.com", ProviderKind::Imap),
        ] {
            let result = lookup(domain)
                .expect(domain)
                .into_result(&format!("a@{domain}"));
            assert_eq!(result.provider, provider);
            assert_eq!(result.auth_kind, AuthKind::Password);
            assert_eq!(
                result.note.as_deref(),
                Some(NOTE_AUTH_CODE),
                "{domain} should ask for an authorization code"
            );
        }

        for (domain, key) in [
            ("exmail.qq.com", NOTE_ENTERPRISE),
            ("yahoo.com", NOTE_APP_PASSWORD_YAHOO),
            ("icloud.com", NOTE_APP_PASSWORD_APPLE),
            ("proton.me", NOTE_PROTON_BRIDGE),
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

        let hosts = vec!["mx1.qq.com.".to_string()];
        assert_eq!(
            match_mx(&hosts).expect("exmail").imap.expect("imap").host,
            "imap.exmail.qq.com"
        );

        assert!(match_mx(&["mx.example.org".to_string()]).is_none());
    }

    #[test]
    fn dav_presets() {
        let (caldav, carddav) = preset_dav("icloud.com").expect("icloud dav");
        assert!(caldav.expect("caldav").contains("caldav.icloud.com"));
        assert!(carddav.expect("carddav").contains("contacts.icloud.com"));
        assert!(preset_dav("example.org").is_none());
    }
}
