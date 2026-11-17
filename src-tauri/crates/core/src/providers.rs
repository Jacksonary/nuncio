//! Verified provider directory.
//!
//! This is the single source of truth for provider-specific knowledge: mail
//! hosts, DAV endpoints, who files the Sent copy and whether IMAP text SEARCH
//! works. Other crates read this table instead of carrying their own copies,
//! so a fix here reaches discovery, account creation and sync together.
//!
//! Every value below was checked against the provider's own help page and/or a
//! live probe (DNS + TLS handshake + DAV PROPFIND). Do not add, remove or
//! "fix" a value from memory: the order of [`PROVIDERS`] is significant for MX
//! matching (see [`by_mx`]), and more specific entries are deliberately placed
//! before broader ones (aol before yahoo, 189 before 21cn).

use crate::models::{
    Account, AuthKind, OAuthProvider, ProviderKind, Security, SentCopyMode, ServerConfig,
};

/// Incoming/outgoing server settings for a provider.
///
/// `host` may contain `{domain}`; [`ServerTemplate::host_for`] substitutes the
/// account's e-mail domain (a few providers, e.g. NetEase, use per-brand hosts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerTemplate {
    pub host: &'static str,
    pub port: u16,
    pub security: Security,
}

impl ServerTemplate {
    /// Resolve the host template for `domain`.
    ///
    /// The substitution is lower-cased because DNS names are
    /// case-insensitive and the stored e-mail address may not be normalized.
    pub fn host_for(&self, domain: &str) -> String {
        let domain = normalize_domain(domain);
        self.host.replace("{domain}", &domain)
    }

    pub fn config(&self, domain: &str, username: &str) -> ServerConfig {
        ServerConfig::new(self.host_for(domain), self.port, self.security, username)
    }
}

/// Who files a copy of outgoing mail into the Sent folder.
///
/// * `Server` — the provider does it itself, so the client must NOT append a
///   copy (appending would duplicate the message).
/// * `Client` — the provider does not, so the client must append.
/// * `Configurable` — there is a web setting whose default is off or unknown;
///   behave like `Client` but tell the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentCopy {
    Server,
    Client,
    Configurable,
}

/// One row of the verified provider table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub kind: ProviderKind,
    pub auth_kind: AuthKind,
    pub oauth_provider: Option<OAuthProvider>,
    pub domains: &'static [&'static str],
    /// Lower-case substrings matched against MX host names, in priority order.
    pub mx: &'static [&'static str],
    pub imap: Option<ServerTemplate>,
    pub smtp: Option<ServerTemplate>,
    pub caldav: Option<&'static str>,
    pub carddav: Option<&'static str>,
    pub sent_copy: SentCopy,
    /// `false` when the provider's IMAP text SEARCH is broken and must never
    /// be used (it returns no hits, or every message).
    pub server_text_search: bool,
    pub note: Option<&'static str>,
}

/// Directory for DAV services.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DavService {
    Calendar,
    Contacts,
}

impl DavService {
    /// IPC/secret-suffix spelling: `"calendar"` or `"contacts"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Calendar => "calendar",
            Self::Contacts => "contacts",
        }
    }

    /// Parse the IPC spelling; `None` for anything else.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "calendar" => Some(Self::Calendar),
            "contacts" => Some(Self::Contacts),
            _ => None,
        }
    }
}

const TLS: Security = Security::Tls;
const STARTTLS: Security = Security::Starttls;

macro_rules! server {
    ($host:literal, $port:literal, $security:expr) => {
        Some(ServerTemplate {
            host: $host,
            port: $port,
            security: $security,
        })
    };
}

pub static PROVIDERS: &[ProviderInfo] = &[
    // developers.google.com/workspace/gmail/imap/imap-smtp; sent copy:
    // support.google.com/mail/answer/78892
    ProviderInfo {
        id: "gmail",
        name: "Gmail",
        kind: ProviderKind::Gmail,
        auth_kind: AuthKind::Oauth,
        oauth_provider: Some(OAuthProvider::Google),
        domains: &["gmail.com", "googlemail.com"],
        mx: &["google.com"],
        imap: server!("imap.gmail.com", 993, TLS),
        smtp: server!("smtp.gmail.com", 465, TLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Server,
        server_text_search: true,
        note: None,
    },
    // support.microsoft.com POP/IMAP/SMTP settings for Outlook.com
    // (smtp-mail.outlook.com is the personal-account host). outlook.cn is
    // 21Vianet infrastructure, not a consumer mailbox domain. msn.cn is kept:
    // confirmed by a live MSA account in this project.
    ProviderInfo {
        id: "outlook",
        name: "Outlook.com",
        kind: ProviderKind::Outlook,
        auth_kind: AuthKind::Oauth,
        oauth_provider: Some(OAuthProvider::Microsoft),
        domains: &[
            "outlook.com",
            "hotmail.com",
            "live.com",
            "msn.com",
            "msn.cn",
            "live.cn",
            "hotmail.co.uk",
            "hotmail.fr",
            "hotmail.de",
            "hotmail.it",
            "hotmail.es",
        ],
        mx: &[],
        imap: server!("outlook.office365.com", 993, TLS),
        smtp: server!("smtp-mail.outlook.com", 587, STARTTLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Server,
        server_text_search: true,
        note: None,
    },
    // learn.microsoft.com Exchange Online POP3/IMAP4 + authenticated client
    // SMTP submission
    ProviderInfo {
        id: "microsoft365",
        name: "Microsoft 365",
        kind: ProviderKind::Outlook,
        auth_kind: AuthKind::Oauth,
        oauth_provider: Some(OAuthProvider::Microsoft),
        domains: &[],
        mx: &["mail.protection.outlook.com"],
        imap: server!("outlook.office365.com", 993, TLS),
        smtp: server!("smtp.office365.com", 587, STARTTLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Server,
        server_text_search: true,
        note: None,
    },
    // support.apple.com/102525; DAV hosts probe-confirmed (OPTIONS advertises
    // calendar-access / addressbook)
    ProviderInfo {
        id: "icloud",
        name: "iCloud Mail",
        kind: ProviderKind::Icloud,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["icloud.com", "me.com", "mac.com"],
        mx: &["mail.icloud.com"],
        imap: server!("imap.mail.me.com", 993, TLS),
        smtp: server!("smtp.mail.me.com", 587, STARTTLS),
        caldav: Some("https://caldav.icloud.com"),
        carddav: Some("https://contacts.icloud.com"),
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: Some("discovery.note.appPasswordApple"),
    },
    // hosts probe-confirmed; DAV: OPTIONS 200 'dav: calendar-access' /
    // 'addressbook', PROPFIND 401; sent copy: Microsoft known-issue page names
    // AOL among auto-saving servers. Must stay ABOVE yahoo: AOL MX is
    // mx-aol.mail.gm0.yahoodns.net.
    ProviderInfo {
        id: "aol",
        name: "AOL Mail",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["aol.com", "aim.com", "verizon.net"],
        mx: &["mx-aol"],
        imap: server!("imap.aol.com", 993, TLS),
        smtp: server!("smtp.aol.com", 465, TLS),
        caldav: Some("https://caldav.aol.com"),
        carddav: Some("https://carddav.aol.com"),
        sent_copy: SentCopy::Server,
        server_text_search: true,
        note: Some("discovery.note.appPassword"),
    },
    // hosts probe-confirmed (TLS + banner). CalDAV deliberately NOT preset:
    // caldav.calendar.yahoo.com answered 403 geo-block from mainland China, so
    // it could not be verified and a 403 maps to needsReauth. Sent copy
    // confirmed (Yahoo files sent mail itself, no switch).
    ProviderInfo {
        id: "yahoo",
        name: "Yahoo Mail",
        kind: ProviderKind::Yahoo,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &[
            "yahoo.com",
            "ymail.com",
            "rocketmail.com",
            "yahoo.co.uk",
            "yahoo.com.au",
            "yahoo.de",
            "yahoo.fr",
            "yahoo.es",
            "yahoo.it",
            "yahoo.ca",
            "yahoo.com.br",
            "yahoo.com.mx",
            "yahoo.co.in",
            "yahoo.com.sg",
            "yahoo.com.hk",
            "yahoo.com.tw",
        ],
        mx: &["yahoodns.net"],
        imap: server!("imap.mail.yahoo.com", 993, TLS),
        smtp: server!("smtp.mail.yahoo.com", 465, TLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Server,
        server_text_search: true,
        note: Some("discovery.note.appPasswordYahoo"),
    },
    // fastmail.help server-names-and-ports; existing DAV values kept (root also
    // confirmed)
    ProviderInfo {
        id: "fastmail",
        name: "Fastmail",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["fastmail.com", "fastmail.fm"],
        mx: &["messagingengine.com"],
        imap: server!("imap.fastmail.com", 993, TLS),
        smtp: server!("smtp.fastmail.com", 465, TLS),
        caldav: Some("https://caldav.fastmail.com/dav/calendars"),
        carddav: Some("https://carddav.fastmail.com/dav/addressbooks"),
        sent_copy: SentCopy::Configurable,
        server_text_search: true,
        note: Some("discovery.note.appPassword"),
    },
    // zoho.com/mail/help/imap-access.html; carddav corrected from
    // contacts.zoho.com (root) to /carddav. Regional data centres (zoho.eu /
    // .in / .com.au / .jp / .com.cn) are NOT preset: their host names were not
    // all on one official page. No MX: paid custom domains use the imappro/
    // smtppro hosts which were not verified.
    ProviderInfo {
        id: "zoho",
        name: "Zoho Mail",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["zoho.com", "zohomail.com"],
        mx: &[],
        imap: server!("imap.zoho.com", 993, TLS),
        smtp: server!("smtp.zoho.com", 465, TLS),
        caldav: Some("https://calendar.zoho.com/"),
        carddav: Some("https://contacts.zoho.com/carddav"),
        sent_copy: SentCopy::Configurable,
        server_text_search: true,
        note: Some("discovery.note.enableImap"),
    },
    // proton.me/support bridge default ports
    ProviderInfo {
        id: "proton",
        name: "Proton Mail (Bridge)",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["proton.me", "protonmail.com", "protonmail.ch", "pm.me"],
        mx: &["protonmail.ch"],
        imap: server!("127.0.0.1", 1143, STARTTLS),
        smtp: server!("127.0.0.1", 1025, STARTTLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: Some("discovery.note.protonBridge"),
    },
    // Tuta documents that it offers no IMAP/SMTP (end-to-end encrypted).
    // Discovery must surface the note and NOT fall through to an imap.<domain>
    // guess.
    ProviderInfo {
        id: "tuta",
        name: "Tuta",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["tuta.com", "tutanota.com", "tuta.io"],
        mx: &[],
        imap: None,
        smtp: None,
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: Some("discovery.note.noImap"),
    },
    // service.mail.qq.com; DAV probe: /.well-known/caldav 301 -> /calendar/,
    // /.well-known/carddav 301 -> /card/, both 401 Basic realm=qq.com. Text
    // SEARCH returns 0 hits. 'SMTP发信后保存到服务器' is opt-in. Consumer MX
    // (mx1/2/3.qq.com) intentionally NOT mapped: it is not Exmail.
    ProviderInfo {
        id: "qq",
        name: "QQ 邮箱",
        kind: ProviderKind::Qq,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["qq.com", "foxmail.com", "vip.qq.com"],
        mx: &[],
        imap: server!("imap.qq.com", 993, TLS),
        smtp: server!("smtp.qq.com", 465, TLS),
        caldav: Some("https://dav.qq.com/calendar/"),
        carddav: Some("https://dav.qq.com/card/"),
        sent_copy: SentCopy::Configurable,
        server_text_search: false,
        note: Some("discovery.note.authCodeEnable"),
    },
    // exmail.qq.com help; MX mxbiz1/mxbiz2.qq.com; DAV probe 401
    ProviderInfo {
        id: "exmail",
        name: "腾讯企业邮",
        kind: ProviderKind::Qq,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["exmail.qq.com"],
        mx: &["mxbiz"],
        imap: server!("imap.exmail.qq.com", 993, TLS),
        smtp: server!("smtp.exmail.qq.com", 465, TLS),
        caldav: Some("https://dav.exmail.qq.com/calendar/"),
        carddav: Some("https://dav.exmail.qq.com/card/"),
        sent_copy: SentCopy::Configurable,
        server_text_search: true,
        note: Some("discovery.note.enterprise"),
    },
    // help.mail.163.com 205712 (protocols ON by default, auth code required);
    // NetEase Exchange shutdown notice names contacts.163.com for CardDAV and
    // gives NO CalDAV (caldav.163.com answers 403 and must not ship). Sent:
    // 设置-常规设置-发送邮件后设置-邮件保存规则.
    ProviderInfo {
        id: "netease163",
        name: "网易 163 邮箱",
        kind: ProviderKind::Netease,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["163.com"],
        mx: &[],
        imap: server!("imap.163.com", 993, TLS),
        smtp: server!("smtp.163.com", 465, TLS),
        caldav: None,
        carddav: Some("https://contacts.163.com"),
        sent_copy: SentCopy::Configurable,
        server_text_search: false,
        note: Some("discovery.note.authCode"),
    },
    // per-brand hosts (imap.126.com, imap.yeah.net, imap.188.com,
    // imap.vip.163.com) per NetEase help. contacts.163.com is documented for
    // 163 only, so no CardDAV preset for the other brands.
    ProviderInfo {
        id: "netease",
        name: "网易邮箱",
        kind: ProviderKind::Netease,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["126.com", "yeah.net", "188.com", "vip.163.com"],
        mx: &[],
        imap: server!("imap.{domain}", 993, TLS),
        smtp: server!("smtp.{domain}", 465, TLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Configurable,
        server_text_search: false,
        note: Some("discovery.note.authCode"),
    },
    // qiye.163.com help; CardDAV root answers 405, /.well-known/carddav answers
    // 401 -> must use the well-known path. The old match_mx mapped ANY
    // *.163.com / *.netease.com MX here; that is wrong (consumer MX is
    // *.mxmail.netease.com) and must become these two specific patterns.
    ProviderInfo {
        id: "netease_qiye",
        name: "网易企业邮箱",
        kind: ProviderKind::Netease,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &[],
        mx: &["qiye163mx", "qiye.163.com"],
        imap: server!("imap.qiye.163.com", 993, TLS),
        smtp: server!("smtp.qiye.163.com", 465, TLS),
        caldav: Some("https://caldav.qiye.163.com"),
        carddav: Some("https://contacts.qiye.163.com/.well-known/carddav"),
        sent_copy: SentCopy::Configurable,
        server_text_search: true,
        note: Some("discovery.note.enterprise"),
    },
    // hosts probe-confirmed; note unchanged from the current table
    ProviderInfo {
        id: "aliyun",
        name: "阿里邮箱",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["aliyun.com"],
        mx: &[],
        imap: server!("imap.aliyun.com", 993, TLS),
        smtp: server!("smtp.aliyun.com", 465, TLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: Some("discovery.note.authCode"),
    },
    // qiye.aliyun.com help. DingTalk Mail custom domains also resolve here
    // (imap.em.dingtalk.com CNAMEs to *.qiye.aliyun.com, MX
    // mx2.mail.aliyun.com), which is why mail.aliyun.com is a pattern.
    ProviderInfo {
        id: "aliyun_qiye",
        name: "阿里企业邮箱",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &[],
        mx: &["mxhichina.com", "qiye.aliyun.com", "mail.aliyun.com"],
        imap: server!("imap.qiye.aliyun.com", 993, TLS),
        smtp: server!("smtp.qiye.aliyun.com", 465, TLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Configurable,
        server_text_search: true,
        note: Some("discovery.note.enterprise"),
    },
    // mail.10086.cn help: POP3/IMAP closed by default. Text SEARCH criteria
    // are ignored (returns every message). KNOWN LIMITATION, not for this
    // change: imap.139.com:993 only negotiates legacy TLS and rustls fails the
    // handshake.
    ProviderInfo {
        id: "china139",
        name: "139 邮箱",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["139.com"],
        mx: &[],
        imap: server!("imap.139.com", 993, TLS),
        smtp: server!("smtp.139.com", 465, TLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Client,
        server_text_search: false,
        note: Some("discovery.note.authCodeEnable"),
    },
    // 189.cn help (hosts); must stay ABOVE china21cn because both MX live under
    // 21cn.com
    ProviderInfo {
        id: "china189",
        name: "189 邮箱",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["189.cn"],
        mx: &["mta-189.21cn.com", "mx2-189.21cn.com"],
        imap: server!("imap.189.cn", 993, TLS),
        smtp: server!("smtp.189.cn", 465, TLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: None,
    },
    // 21cn help; hosts probe-confirmed
    ProviderInfo {
        id: "china21cn",
        name: "21CN 邮箱",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["21cn.com", "21cn.net", "vip.21cn.com"],
        mx: &["mta.21cn.com"],
        imap: server!("imap.21cn.com", 993, TLS),
        smtp: server!("smtp.21cn.com", 465, TLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: None,
    },
    // help.sina.com.cn/comquestiondetail/view/160 per-brand table. Port 587 is
    // NOT a TLS fallback (no STARTTLS advertised).
    ProviderInfo {
        id: "sina",
        name: "新浪邮箱",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["sina.com"],
        mx: &[],
        imap: server!("imap.sina.com", 993, TLS),
        smtp: server!("smtp.sina.com", 465, TLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: Some("discovery.note.authCodeEnable"),
    },
    // same official table: @sina.cn uses *.sina.cn. The current table sends
    // sina.cn to imap.sina.com — a bug this fixes.
    ProviderInfo {
        id: "sina_cn",
        name: "新浪邮箱",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["sina.cn"],
        mx: &[],
        imap: server!("imap.sina.cn", 993, TLS),
        smtp: server!("smtp.sina.cn", 465, TLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: Some("discovery.note.authCodeEnable"),
    },
    // same official table; TLS probe OK
    ProviderInfo {
        id: "sina_vip",
        name: "新浪 VIP 邮箱",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["vip.sina.com", "vip.sina.cn"],
        mx: &[],
        imap: server!("imap.vip.sina.com", 993, TLS),
        smtp: server!("smtp.vip.sina.com", 465, TLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: Some("discovery.note.authCodeEnable"),
    },
    // hosts probe-confirmed (no public help page); unchanged hosts from the
    // current table
    ProviderInfo {
        id: "sohu",
        name: "搜狐邮箱",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["sohu.com", "vip.sohu.com"],
        mx: &[],
        imap: server!("imap.sohu.com", 993, TLS),
        smtp: server!("smtp.sohu.com", 465, TLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: Some("discovery.note.authCodeEnable"),
    },
    // feishu.cn help (Foxmail guide); explicitly no CalDAV/CardDAV
    ProviderInfo {
        id: "feishu",
        name: "飞书邮箱",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &[],
        mx: &["feishu.cn"],
        imap: server!("imap.feishu.cn", 993, TLS),
        smtp: server!("smtp.feishu.cn", 465, TLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: Some("discovery.note.enterprise"),
    },
    // larksuite.com help (EN)
    ProviderInfo {
        id: "lark",
        name: "Lark Mail",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &[],
        mx: &["larksuite.com"],
        imap: server!("imap.larksuite.com", 993, TLS),
        smtp: server!("smtp.larksuite.com", 465, TLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: Some("discovery.note.enterprise"),
    },
    // yandex.com/support/yandex-360 calendar 'Server URL:
    // https://caldav.yandex.ru' + address book 'carddav.yandex.ru'; PROPFIND
    // 401. ONLY the .ru DAV hosts work: caldav.yandex.com,
    // carddav.yandex.com and yandex.com/.well-known/caldav all answer 405 —
    // never derive the DAV host from the mail domain.
    ProviderInfo {
        id: "yandex",
        name: "Yandex Mail",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["yandex.ru", "yandex.com", "ya.ru", "yandex.by", "yandex.kz"],
        mx: &["mx.yandex.ru", "mx.yandex.net"],
        imap: server!("imap.yandex.com", 993, TLS),
        smtp: server!("smtp.yandex.com", 465, TLS),
        caldav: Some("https://caldav.yandex.ru"),
        carddav: Some("https://carddav.yandex.ru"),
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: Some("discovery.note.appPasswordYandex"),
    },
    // help.mail.ru; CalDAV host is calendar.mail.ru (caldav.mail.ru resolves
    // but times out). CardDAV not documented.
    ProviderInfo {
        id: "mailru",
        name: "Mail.ru",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &[
            "mail.ru",
            "bk.ru",
            "inbox.ru",
            "list.ru",
            "internet.ru",
            "xmail.ru",
        ],
        mx: &["mxs.mail.ru"],
        imap: server!("imap.mail.ru", 993, TLS),
        smtp: server!("smtp.mail.ru", 465, TLS),
        caldav: Some("https://calendar.mail.ru"),
        carddav: None,
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: Some("discovery.note.appPassword"),
    },
    // help.rambler.ru
    ProviderInfo {
        id: "rambler",
        name: "Rambler",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["rambler.ru", "lenta.ru"],
        mx: &[],
        imap: server!("imap.rambler.ru", 993, TLS),
        smtp: server!("smtp.rambler.ru", 465, TLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: Some("discovery.note.enableImap"),
    },
    // support.gmx.com: SMTP host mail.gmx.com (smtp.gmx.com is a CNAME);
    // CalDAV documented. CardDAV for gmx.com not verified.
    ProviderInfo {
        id: "gmx",
        name: "GMX",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["gmx.com"],
        mx: &[],
        imap: server!("imap.gmx.com", 993, TLS),
        smtp: server!("mail.gmx.com", 587, STARTTLS),
        caldav: Some("https://caldav.gmx.com"),
        carddav: None,
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: Some("discovery.note.enableImap"),
    },
    // IMAP/SMTP hosts kept as in the current table (imap.gmx.net was not
    // verified). caldav.gmx.net PROPFIND 401;
    // carddav.gmx.net/.well-known/carddav 401. caldav.gmx.de has NO DNS
    // record — do not use it.
    ProviderInfo {
        id: "gmx_net",
        name: "GMX",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["gmx.net", "gmx.de", "gmx.at", "gmx.ch"],
        mx: &[],
        imap: server!("imap.gmx.com", 993, TLS),
        smtp: server!("mail.gmx.com", 587, STARTTLS),
        caldav: Some("https://caldav.gmx.net"),
        carddav: Some("https://carddav.gmx.net"),
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: Some("discovery.note.enableImap"),
    },
    // hilfe.web.de IMAP/SMTP; DAV probe 401
    ProviderInfo {
        id: "webde",
        name: "WEB.DE",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["web.de"],
        mx: &[],
        imap: server!("imap.web.de", 993, TLS),
        smtp: server!("smtp.web.de", 587, STARTTLS),
        caldav: Some("https://caldav.web.de"),
        carddav: Some("https://carddav.web.de"),
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: Some("discovery.note.enableImap"),
    },
    // telekom.de help; DAV corrected: the bare host answers 405, these paths
    // answer 401
    ProviderInfo {
        id: "tonline",
        name: "Telekom Mail",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["t-online.de", "magenta.de"],
        mx: &["t-online.de", "magenta.de"],
        imap: server!("secureimap.t-online.de", 993, TLS),
        smtp: server!("securesmtp.t-online.de", 465, TLS),
        caldav: Some("https://spica.t-online.de/spica-calendar/caldav"),
        carddav: Some("https://spica.t-online.de/spica-contacts/carddav"),
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: Some("discovery.note.appPassword"),
    },
    // kb.mailbox.org
    ProviderInfo {
        id: "mailboxorg",
        name: "mailbox.org",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["mailbox.org"],
        mx: &["mailbox.org"],
        imap: server!("imap.mailbox.org", 993, TLS),
        smtp: server!("smtp.mailbox.org", 465, TLS),
        caldav: Some("https://dav.mailbox.org"),
        carddav: Some("https://dav.mailbox.org/carddav"),
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: None,
    },
    // posteo.de/hilfe (CalDAV 8443, CardDAV 8843)
    ProviderInfo {
        id: "posteo",
        name: "Posteo",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &[
            "posteo.de",
            "posteo.com",
            "posteo.net",
            "posteo.org",
            "posteo.uk",
        ],
        mx: &["posteo.de"],
        imap: server!("posteo.de", 993, TLS),
        smtp: server!("posteo.de", 465, TLS),
        caldav: Some("https://posteo.de:8443"),
        carddav: Some("https://posteo.de:8843"),
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: None,
    },
    // napoveda.seznam.cz; CardDAV not documented
    ProviderInfo {
        id: "seznam",
        name: "Seznam Email",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["seznam.cz", "email.cz", "post.cz"],
        mx: &[],
        imap: server!("imap.seznam.cz", 993, TLS),
        smtp: server!("smtp.seznam.cz", 465, TLS),
        caldav: Some("https://caldav.seznam.cz"),
        carddav: None,
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: None,
    },
    // aiuto.libero.it: incoming host is imapmail.libero.it (imap.libero.it does
    // not exist)
    ProviderInfo {
        id: "libero",
        name: "Libero Mail",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["libero.it", "iol.it", "inwind.it", "blu.it", "giallo.it"],
        mx: &[],
        imap: server!("imapmail.libero.it", 993, TLS),
        smtp: server!("smtp.libero.it", 465, TLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: None,
    },
    // assistance.orange.fr; DAV probe 401 at root and /.well-known
    ProviderInfo {
        id: "orange",
        name: "Orange Mail",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["orange.fr", "wanadoo.fr"],
        mx: &[],
        imap: server!("imap.orange.fr", 993, TLS),
        smtp: server!("smtp.orange.fr", 465, TLS),
        caldav: Some("https://caldav.orange.fr"),
        carddav: Some("https://carddav.orange.fr"),
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: Some("discovery.note.appPassword"),
    },
    // assistance.free.fr
    ProviderInfo {
        id: "free",
        name: "Free",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["free.fr", "aliceadsl.fr", "libertysurf.fr"],
        mx: &[],
        imap: server!("imap.free.fr", 993, TLS),
        smtp: server!("smtp.free.fr", 587, STARTTLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: None,
    },
    // aide.laposte.net
    ProviderInfo {
        id: "laposte",
        name: "La Poste",
        kind: ProviderKind::Imap,
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        domains: &["laposte.net"],
        mx: &[],
        imap: server!("imap.laposte.net", 993, TLS),
        smtp: server!("smtp.laposte.net", 587, STARTTLS),
        caldav: None,
        carddav: None,
        sent_copy: SentCopy::Client,
        server_text_search: true,
        note: None,
    },
];

/// Trim, drop one or more trailing dots and lower-case a domain.
///
/// Inputs come from user typing, DNS and the account row, none of which are
/// guaranteed to be normalized.
fn normalize_domain(domain: &str) -> String {
    domain.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Look up a provider by e-mail domain (exact match).
pub fn by_domain(domain: &str) -> Option<&'static ProviderInfo> {
    let domain = normalize_domain(domain);
    if domain.is_empty() {
        return None;
    }
    PROVIDERS
        .iter()
        .find(|provider| provider.domains.contains(&domain.as_str()))
}

/// Look up a provider by the domain part of an e-mail address.
///
/// `rsplit_once` so an address whose local part contains `@` (legal when
/// quoted) still resolves to the right domain.
pub fn by_email(email: &str) -> Option<&'static ProviderInfo> {
    let (_, domain) = email.rsplit_once('@')?;
    by_domain(domain)
}

/// Match MX hosts against the table, in MX preference order.
///
/// The first host that matches anything wins; a later host is only considered
/// when the earlier ones are all empty or unknown. Within a host the table
/// order decides, which is why specific entries (aol, 189, exmail) sit above
/// broad ones (yahoo, 21cn).
pub fn by_mx(hosts: &[String]) -> Option<&'static ProviderInfo> {
    for raw in hosts {
        let host = normalize_domain(raw);
        if host.is_empty() {
            continue;
        }
        for provider in PROVIDERS {
            if provider.mx.iter().any(|pattern| host.contains(pattern)) {
                return Some(provider);
            }
        }
    }
    None
}

/// CalDAV/CardDAV URL to use for an account.
///
/// The user's own value always wins; only when it is absent do we fall back to
/// the directory default for the account's e-mail domain. We never guess a DAV
/// host from the mail domain: that is how Yandex ended up on a 405 (only
/// `*.yandex.ru` serves DAV, while mail is `*.yandex.com`).
pub fn dav_url(account: &Account, service: DavService) -> Option<String> {
    let explicit = match service {
        DavService::Calendar => account.caldav_url.as_deref(),
        DavService::Contacts => account.carddav_url.as_deref(),
    };
    if let Some(value) = explicit {
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    let provider = by_email(&account.email)?;
    let directory = match service {
        DavService::Calendar => provider.caldav,
        DavService::Contacts => provider.carddav,
    };
    directory.map(str::to_string)
}

/// True when the server files SMTP-sent mail into Sent by itself, so the
/// client must NOT append a copy.
///
/// An explicit per-account choice wins. Otherwise we trust the Gmail extension
/// probe first (it is live evidence), then the static directory fact. Unknown
/// domains are treated as "client appends", the safe default: a duplicate Sent
/// entry is visible and recoverable, a silently missing one is not.
pub fn server_files_sent(account: &Account) -> bool {
    match account.sent_copy {
        Some(SentCopyMode::Server) => true,
        Some(SentCopyMode::Client) => false,
        None => {
            account.capabilities.gmail_extensions
                || by_email(&account.email).is_some_and(|p| p.sent_copy == SentCopy::Server)
        }
    }
}

/// False for providers whose IMAP text SEARCH is broken (QQ, NetEase, 139).
///
/// Unknown domains default to `true`: the probe in the IMAP layer still has
/// the final say, but the directory must not disable search for everyone just
/// because these four servers are broken.
pub fn server_text_search(account: &Account) -> bool {
    by_email(&account.email).is_none_or(|provider| provider.server_text_search)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AccountState, ProviderCapabilities};

    fn account(email: &str) -> Account {
        Account {
            id: "test".into(),
            provider: ProviderKind::Imap,
            email: email.into(),
            display_name: "Test".into(),
            color: "#000000".into(),
            auth_kind: AuthKind::Password,
            oauth_provider: None,
            imap: None,
            smtp: None,
            pop3: None,
            caldav_url: None,
            carddav_url: None,
            sent_copy: None,
            sync_window_days: 90,
            mail_enabled: true,
            calendar_enabled: true,
            contacts_enabled: true,
            tasks_enabled: true,
            notifications_enabled: true,
            include_in_unified: true,
            sort_order: 0,
            state: AccountState::Ok,
            state_message: None,
            capabilities: ProviderCapabilities::default(),
            created_at: crate::models::now(),
        }
    }

    fn mx(host: &str) -> Option<&'static ProviderInfo> {
        by_mx(&[host.to_string()])
    }

    #[test]
    fn table_domains_are_unique_and_lower_case() {
        let mut seen = std::collections::HashSet::new();
        for provider in PROVIDERS {
            for domain in provider.domains {
                assert_eq!(*domain, domain.to_ascii_lowercase(), "{}", provider.id);
                assert!(seen.insert(*domain), "duplicate domain {domain}");
            }
        }
    }

    #[test]
    fn table_mail_and_dav_shape_is_consistent() {
        for provider in PROVIDERS {
            assert_eq!(
                provider.imap.is_some(),
                provider.smtp.is_some(),
                "{} has only one of imap/smtp",
                provider.id
            );
            match (provider.imap, provider.smtp) {
                (Some(imap), Some(smtp)) => {
                    assert!(!imap.host.is_empty(), "{} imap host", provider.id);
                    assert!(!smtp.host.is_empty(), "{} smtp host", provider.id);
                    assert_ne!(imap.port, 0, "{} imap port", provider.id);
                    assert_ne!(smtp.port, 0, "{} smtp port", provider.id);
                }
                (None, None) => assert_eq!(provider.id, "tuta"),
                _ => unreachable!(),
            }
            if let Some(url) = provider.caldav {
                assert!(url.starts_with("https://"), "{} caldav", provider.id);
            }
            if let Some(url) = provider.carddav {
                assert!(url.starts_with("https://"), "{} carddav", provider.id);
            }
        }
    }

    #[test]
    fn only_broken_search_providers_disable_text_search() {
        let disabled: Vec<&str> = PROVIDERS
            .iter()
            .filter(|provider| !provider.server_text_search)
            .map(|provider| provider.id)
            .collect();
        assert_eq!(disabled, vec!["qq", "netease163", "netease", "china139"]);
    }

    #[test]
    fn by_domain_handles_dotted_and_case_insensitive_input() {
        assert_eq!(by_domain(" Yandex.RU. ").map(|p| p.id), Some("yandex"));
        assert_eq!(by_domain(""), None);
        assert_eq!(by_domain("."), None);
    }

    #[test]
    fn by_domain_pins_per_brand_hosts() {
        assert_eq!(
            by_domain("sina.cn").and_then(|p| p.imap).map(|t| t.host),
            Some("imap.sina.cn")
        );
        assert_eq!(
            by_domain("126.com")
                .and_then(|p| p.imap)
                .map(|t| t.host_for("126.com")),
            Some("imap.126.com".to_string())
        );
        assert!(by_domain("outlook.cn").is_none());
        assert_eq!(by_domain("msn.cn").map(|p| p.id), Some("outlook"));
    }

    #[test]
    fn by_domain_pins_dav_endpoints() {
        let yandex = by_domain("yandex.com").expect("yandex");
        assert_eq!(yandex.caldav, Some("https://caldav.yandex.ru"));
        assert_eq!(yandex.carddav, Some("https://carddav.yandex.ru"));

        let netease163 = by_domain("163.com").expect("163");
        assert_eq!(netease163.caldav, None);
        assert_eq!(netease163.carddav, Some("https://contacts.163.com"));
        assert_eq!(by_domain("126.com").expect("126").carddav, None);
    }

    #[test]
    fn tuta_has_no_mail_servers() {
        assert!(by_domain("tuta.com").expect("tuta").imap.is_none());
    }

    #[test]
    fn by_mx_prefers_specific_entries() {
        assert_eq!(
            mx("mx-aol.mail.gm0.yahoodns.net").map(|p| p.id),
            Some("aol")
        );
        assert_eq!(mx("mta7.am0.yahoodns.net").map(|p| p.id), Some("yahoo"));
        assert_eq!(mx("mta-189.21cn.com").map(|p| p.id), Some("china189"));
        assert_eq!(mx("mta.21cn.com").map(|p| p.id), Some("china21cn"));
    }

    #[test]
    fn by_mx_does_not_confuse_consumer_and_enterprise() {
        assert_eq!(mx("mxbiz1.qq.com").map(|p| p.id), Some("exmail"));
        assert_eq!(mx("mx1.qq.com").map(|p| p.id), None);
        assert_eq!(mx("163mx01.mxmail.netease.com").map(|p| p.id), None);
        assert_eq!(
            mx("qiye163mx01.mxmail.netease.com").map(|p| p.id),
            Some("netease_qiye")
        );
    }

    #[test]
    fn by_mx_matches_google_microsoft_and_others() {
        assert_eq!(mx("aspmx.l.google.com").map(|p| p.id), Some("gmail"));
        assert_eq!(
            mx("contoso-com.mail.protection.outlook.com").map(|p| p.id),
            Some("microsoft365")
        );
        assert_eq!(mx("mx.yandex.net").map(|p| p.id), Some("yandex"));
        assert_eq!(mx("mx2.mail.aliyun.com").map(|p| p.id), Some("aliyun_qiye"));
    }

    #[test]
    fn by_mx_keeps_looking_after_an_unknown_host() {
        let hosts = vec![
            "unknown.example".to_string(),
            "aspmx.l.google.com".to_string(),
        ];
        assert_eq!(by_mx(&hosts).map(|p| p.id), Some("gmail"));
    }

    #[test]
    fn dav_url_prefers_explicit_value_then_directory() {
        let mut explicit = account("me@yahoo.com");
        explicit.caldav_url = Some("https://custom.example/dav".into());
        assert_eq!(
            dav_url(&explicit, DavService::Calendar),
            Some("https://custom.example/dav".into())
        );

        let mut blank = account("me@yandex.ru");
        blank.caldav_url = Some("   ".into());
        assert_eq!(
            dav_url(&blank, DavService::Calendar),
            Some("https://caldav.yandex.ru".into())
        );

        let plain = account("me@yandex.com");
        assert_eq!(
            dav_url(&plain, DavService::Calendar),
            Some("https://caldav.yandex.ru".into())
        );
        assert_eq!(
            dav_url(&plain, DavService::Contacts),
            Some("https://carddav.yandex.ru".into())
        );

        let unknown = account("me@unknown.example");
        assert_eq!(dav_url(&unknown, DavService::Calendar), None);
    }

    #[test]
    fn server_files_sent_honours_automatic_and_explicit_choice() {
        let mut gmail = account("me@gmail.com");
        gmail.capabilities.gmail_extensions = true;
        assert!(server_files_sent(&gmail));

        assert!(server_files_sent(&account("me@yahoo.com")));
        assert!(!server_files_sent(&account("me@yandex.com")));

        let mut yahoo_client = account("me@yahoo.com");
        yahoo_client.sent_copy = Some(SentCopyMode::Client);
        assert!(!server_files_sent(&yahoo_client));

        let mut yandex_server = account("me@yandex.com");
        yandex_server.sent_copy = Some(SentCopyMode::Server);
        assert!(server_files_sent(&yandex_server));
    }

    #[test]
    fn server_text_search_defaults_to_true_for_unknown_domains() {
        assert!(!server_text_search(&account("me@qq.com")));
        assert!(server_text_search(&account("me@gmail.com")));
        assert!(server_text_search(&account("me@unknown.example")));
    }

    #[test]
    fn server_template_substitutes_and_builds_config() {
        let template = ServerTemplate {
            host: "imap.{domain}",
            port: 993,
            security: TLS,
        };
        assert_eq!(template.host_for("126.com"), "imap.126.com");
        let config = template.config("Yeah.NET.", "user@yeah.net");
        assert_eq!(config.host, "imap.yeah.net");
        assert_eq!(config.port, 993);
        assert_eq!(config.security, Security::Tls);
        assert_eq!(config.username, "user@yeah.net");
    }
}
