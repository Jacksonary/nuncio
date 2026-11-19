//! Rust mirror of the IPC contract in `src/ipc/types.ts`.
//!
//! Every type here serializes with `camelCase` field names and the same literal
//! values as the TypeScript definition. Optional values serialize as `null`
//! (they are never omitted), so the frontend can rely on the shape.
//!
//! Internal row types used by the repositories (`StoredMessage`, `StoredEvent`,
//! `PendingOp`, `OutboxItem`, `UndoEntry`) also live here so that other crates
//! can build and inspect them.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::error::AppError;
use crate::Result;

/// Identifier (UUID v4 string, or a well-known built-in id such as `"local"`).
pub type Id = String;
/// ISO-8601 instant in UTC (`2026-11-12T08:00:00Z`).
pub type IsoDateTime = String;
/// Calendar date (`2026-11-12`).
pub type IsoDate = String;

/// Built-in account that owns local calendars, contacts and tasks.
pub const LOCAL_ACCOUNT_ID: &str = "local";
pub const LOCAL_CALENDAR_ID: &str = "local-calendar";
pub const LOCAL_ADDRESS_BOOK_ID: &str = "local-contacts";
pub const LOCAL_TASK_LIST_ID: &str = "local-tasks";

/// Generate a new random identifier.
pub fn new_id() -> Id {
    Uuid::new_v4().to_string()
}

/// Current UTC instant truncated to whole seconds (stable, RFC3339 friendly).
pub fn now() -> DateTime<Utc> {
    let ts = Utc::now().timestamp();
    DateTime::from_timestamp(ts, 0).unwrap_or_else(Utc::now)
}

/// Helper for patch fields typed `T | null` in TypeScript: distinguishes an
/// absent field (`None`) from an explicit `null` (`Some(None)`).
pub fn deserialize_double_option<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// Generic string-union conversion helper used by repository code.
pub fn enum_from_db<T: FromStr>(value: &str, fallback: T) -> T {
    value.parse::<T>().unwrap_or(fallback)
}

/* ───────────────────────────── Accounts ───────────────────────────── */

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderKind {
    Gmail,
    Outlook,
    Icloud,
    Yahoo,
    Qq,
    Netease,
    Imap,
    Pop3,
    Local,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gmail => "gmail",
            Self::Outlook => "outlook",
            Self::Icloud => "icloud",
            Self::Yahoo => "yahoo",
            Self::Qq => "qq",
            Self::Netease => "netease",
            Self::Imap => "imap",
            Self::Pop3 => "pop3",
            Self::Local => "local",
        }
    }

    pub fn is_local(self) -> bool {
        matches!(self, Self::Local)
    }

    /// Providers that receive mail over IMAP (i.e. not local/POP3).
    pub fn uses_imap(self) -> bool {
        !matches!(self, Self::Local | Self::Pop3)
    }

    /// Providers whose mail account authenticates with OAuth2.
    pub fn default_auth_kind(self) -> AuthKind {
        match self {
            Self::Gmail | Self::Outlook => AuthKind::Oauth,
            _ => AuthKind::Password,
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProviderKind {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "gmail" => Self::Gmail,
            "outlook" => Self::Outlook,
            "icloud" => Self::Icloud,
            "yahoo" => Self::Yahoo,
            "qq" => Self::Qq,
            "netease" => Self::Netease,
            "imap" => Self::Imap,
            "pop3" => Self::Pop3,
            "local" => Self::Local,
            other => {
                return Err(AppError::invalid_input(format!(
                    "unknown provider kind: {other}"
                )))
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthKind {
    Oauth,
    Password,
}

impl AuthKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Oauth => "oauth",
            Self::Password => "password",
        }
    }
}

impl FromStr for AuthKind {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "oauth" => Ok(Self::Oauth),
            "password" => Ok(Self::Password),
            other => Err(AppError::invalid_input(format!(
                "unknown auth kind: {other}"
            ))),
        }
    }
}

/// Who is responsible for filing a copy of sent mail into the Sent folder.
///
/// This is the user-facing account setting; `None` on an account means
/// "automatic" and is resolved by `providers::server_files_sent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SentCopyMode {
    Server,
    Client,
}

impl SentCopyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Client => "client",
        }
    }
}

impl FromStr for SentCopyMode {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "server" => Ok(Self::Server),
            "client" => Ok(Self::Client),
            other => Err(AppError::invalid_input(format!(
                "unknown sent copy mode: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OAuthProvider {
    Google,
    Microsoft,
}

impl OAuthProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::Microsoft => "microsoft",
        }
    }
}

impl FromStr for OAuthProvider {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "google" => Ok(Self::Google),
            "microsoft" => Ok(Self::Microsoft),
            other => Err(AppError::invalid_input(format!(
                "unknown OAuth provider: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Security {
    Tls,
    Starttls,
    None,
}

impl Security {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tls => "tls",
            Self::Starttls => "starttls",
            Self::None => "none",
        }
    }
}

impl FromStr for Security {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "tls" => Ok(Self::Tls),
            "starttls" => Ok(Self::Starttls),
            "none" => Ok(Self::None),
            other => Err(AppError::invalid_input(format!(
                "unknown security mode: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AccountState {
    #[default]
    Ok,
    Syncing,
    NeedsReauth,
    Error,
    Offline,
    Disabled,
}

impl AccountState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Syncing => "syncing",
            Self::NeedsReauth => "needsReauth",
            Self::Error => "error",
            Self::Offline => "offline",
            Self::Disabled => "disabled",
        }
    }
}

impl FromStr for AccountState {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "ok" => Ok(Self::Ok),
            "syncing" => Ok(Self::Syncing),
            "needsReauth" => Ok(Self::NeedsReauth),
            "error" => Ok(Self::Error),
            "offline" => Ok(Self::Offline),
            "disabled" => Ok(Self::Disabled),
            other => Err(AppError::invalid_input(format!(
                "unknown account state: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub security: Security,
    /// Login user name; defaults to the account e-mail.
    pub username: String,
}

impl ServerConfig {
    pub fn new(
        host: impl Into<String>,
        port: u16,
        security: Security,
        username: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            security,
            username: username.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCapabilities {
    pub idle: bool,
    pub condstore: bool,
    pub qresync: bool,
    #[serde(rename = "move")]
    pub r#move: bool,
    pub uidplus: bool,
    pub gmail_extensions: bool,
    pub special_use: bool,
    pub server_search: bool,
    /// Server saves sent mail itself (Gmail) -> do not APPEND to Sent.
    pub auto_saves_sent: bool,
    pub calendar: bool,
    pub contacts: bool,
    pub tasks: bool,
    pub task_importance: bool,
    pub task_reminders: bool,
    pub task_steps: bool,
    pub task_recurrence: bool,
}

impl Default for ProviderCapabilities {
    fn default() -> Self {
        Self::defaults_for(ProviderKind::Imap)
    }
}

impl ProviderCapabilities {
    /// Sensible capability defaults for each well-known provider.
    ///
    /// These are *initial* values persisted on the account; the sync engines
    /// refine them after a `CAPABILITY` probe.
    ///
    /// The IMAP transport extensions (`idle`, `condstore`, `qresync`, `move`,
    /// `uidplus`, `special_use`, `server_search`) are deliberately `false`
    /// here: they are only discoverable from the server, and an account that
    /// has never connected must not claim them. A provider guess is not
    /// evidence — advertising `idle` before the first successful `CAPABILITY`
    /// makes the engine open an IDLE session against a server nobody has
    /// talked to yet (and, for a broken account, doubles the failing connect
    /// attempts). `ServerCapabilities::merge_into` fills them from the live
    /// probe as soon as the account connects.
    pub fn defaults_for(kind: ProviderKind) -> Self {
        let mut caps = Self {
            idle: false,
            condstore: false,
            qresync: false,
            r#move: false,
            uidplus: false,
            gmail_extensions: false,
            special_use: false,
            server_search: false,
            auto_saves_sent: false,
            calendar: false,
            contacts: false,
            tasks: false,
            task_importance: false,
            task_reminders: false,
            task_steps: false,
            task_recurrence: false,
        };
        match kind {
            ProviderKind::Gmail => {
                // Provider-level facts, not probes: Gmail files sent mail
                // itself and exposes labels through X-GM-EXT-1.
                caps.gmail_extensions = true;
                caps.auto_saves_sent = true;
                caps.calendar = true;
                caps.contacts = true;
                caps.tasks = true;
                caps.task_steps = true;
            }
            ProviderKind::Outlook => {
                caps.calendar = true;
                caps.contacts = true;
                caps.tasks = true;
                caps.task_importance = true;
                caps.task_reminders = true;
                caps.task_steps = true;
                caps.task_recurrence = true;
            }
            ProviderKind::Icloud => {
                caps.calendar = true;
                caps.contacts = true;
            }
            ProviderKind::Yahoo => {}
            ProviderKind::Qq | ProviderKind::Netease => {}
            ProviderKind::Imap => {}
            ProviderKind::Pop3 => {
                caps.idle = false;
                caps.condstore = false;
                caps.qresync = false;
                caps.r#move = false;
                caps.uidplus = false;
                caps.special_use = false;
                caps.server_search = false;
            }
            ProviderKind::Local => {
                caps.idle = false;
                caps.condstore = false;
                caps.qresync = false;
                caps.r#move = false;
                caps.uidplus = false;
                caps.gmail_extensions = false;
                caps.special_use = false;
                caps.server_search = false;
                caps.calendar = true;
                caps.contacts = true;
                caps.tasks = true;
                caps.task_importance = true;
                caps.task_reminders = true;
                caps.task_steps = true;
                caps.task_recurrence = true;
            }
        }
        caps
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: Id,
    pub provider: ProviderKind,
    pub email: String,
    pub display_name: String,
    /// Hex color (`#RRGGBB`) used in unified views.
    pub color: String,
    pub auth_kind: AuthKind,
    pub oauth_provider: Option<OAuthProvider>,
    pub imap: Option<ServerConfig>,
    pub smtp: Option<ServerConfig>,
    pub pop3: Option<ServerConfig>,
    pub caldav_url: Option<String>,
    pub carddav_url: Option<String>,
    /// `None` means automatic (see `providers::server_files_sent`).
    #[serde(default)]
    pub sent_copy: Option<SentCopyMode>,
    pub sync_window_days: u32,
    pub mail_enabled: bool,
    pub calendar_enabled: bool,
    pub contacts_enabled: bool,
    pub tasks_enabled: bool,
    pub notifications_enabled: bool,
    pub include_in_unified: bool,
    pub sort_order: i64,
    pub state: AccountState,
    pub state_message: Option<String>,
    pub capabilities: ProviderCapabilities,
    pub created_at: DateTime<Utc>,
}

impl Account {
    pub fn is_mail_active(&self) -> bool {
        self.mail_enabled && !self.provider.is_local() && self.state != AccountState::Disabled
    }

    /// Address used for "mentions me" checks and default identities.
    pub fn address(&self) -> Address {
        Address {
            name: Some(self.display_name.clone()).filter(|n| !n.is_empty()),
            email: self.email.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryResult {
    pub provider: ProviderKind,
    pub auth_kind: AuthKind,
    pub oauth_provider: Option<OAuthProvider>,
    pub imap: Option<ServerConfig>,
    pub smtp: Option<ServerConfig>,
    pub pop3: Option<ServerConfig>,
    pub caldav_url: Option<String>,
    pub carddav_url: Option<String>,
    /// `"preset" | "ispdb" | "autoconfig" | "mx" | "guess"`.
    pub source: String,
    /// Human hint, e.g. "QQ 邮箱需使用授权码而不是登录密码".
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthResult {
    /// Opaque handle referencing tokens held by the backend until account_create.
    pub session_id: String,
    pub provider: OAuthProvider,
    pub email: String,
    pub display_name: String,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewAccountInput {
    pub provider: ProviderKind,
    pub email: String,
    pub display_name: String,
    pub auth_kind: AuthKind,
    /// Password / app password / authorization code (authKind = "password").
    pub password: Option<String>,
    /// From auth_oauth_login (authKind = "oauth").
    pub oauth_session_id: Option<String>,
    pub imap: Option<ServerConfig>,
    pub smtp: Option<ServerConfig>,
    pub pop3: Option<ServerConfig>,
    pub caldav_url: Option<String>,
    pub carddav_url: Option<String>,
    pub sync_window_days: u32,
    pub mail_enabled: bool,
    pub calendar_enabled: bool,
    pub contacts_enabled: bool,
    pub tasks_enabled: bool,
}

// Never expose the password in logs.
impl std::fmt::Debug for NewAccountInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewAccountInput")
            .field("provider", &self.provider)
            .field("email", &self.email)
            .field("display_name", &self.display_name)
            .field("auth_kind", &self.auth_kind)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("oauth_session_id", &self.oauth_session_id)
            .field("imap", &self.imap)
            .field("smtp", &self.smtp)
            .field("pop3", &self.pop3)
            .field("caldav_url", &self.caldav_url)
            .field("carddav_url", &self.carddav_url)
            .field("sync_window_days", &self.sync_window_days)
            .field("mail_enabled", &self.mail_enabled)
            .field("calendar_enabled", &self.calendar_enabled)
            .field("contacts_enabled", &self.contacts_enabled)
            .field("tasks_enabled", &self.tasks_enabled)
            .finish()
    }
}

/// Patch for `account_update`. `Some(None)` means "clear the value".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AccountPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_double_option"
    )]
    pub imap: Option<Option<ServerConfig>>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_double_option"
    )]
    pub smtp: Option<Option<ServerConfig>>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_double_option"
    )]
    pub pop3: Option<Option<ServerConfig>>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_double_option"
    )]
    pub caldav_url: Option<Option<String>>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_double_option"
    )]
    pub carddav_url: Option<Option<String>>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_double_option"
    )]
    pub sent_copy: Option<Option<SentCopyMode>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_window_days: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mail_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calendar_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contacts_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tasks_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notifications_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_in_unified: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionTestResult {
    pub imap_ok: Option<bool>,
    pub smtp_ok: Option<bool>,
    pub pop3_ok: Option<bool>,
    pub dav_ok: Option<bool>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub id: Id,
    pub account_id: Id,
    pub name: String,
    pub email: String,
    pub reply_to: Option<String>,
    pub signature_id: Option<Id>,
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Signature {
    pub id: Id,
    /// `null` = available for every account.
    pub account_id: Option<Id>,
    pub name: String,
    pub html: String,
    pub use_for_new: bool,
    pub use_for_reply: bool,
}

/* ─────────────────────────────── Mail ─────────────────────────────── */

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum FolderRole {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Junk,
    Archive,
    All,
    Flagged,
    Important,
    Outbox,
    #[default]
    Other,
}

impl FolderRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Sent => "sent",
            Self::Drafts => "drafts",
            Self::Trash => "trash",
            Self::Junk => "junk",
            Self::Archive => "archive",
            Self::All => "all",
            Self::Flagged => "flagged",
            Self::Important => "important",
            Self::Outbox => "outbox",
            Self::Other => "other",
        }
    }

    /// True for roles that map to a real IMAP folder (local rules apply).
    pub fn is_selectable_role(self) -> bool {
        !matches!(self, Self::Flagged | Self::Important | Self::Outbox)
    }
}

impl fmt::Display for FolderRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for FolderRole {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "inbox" => Self::Inbox,
            "sent" => Self::Sent,
            "drafts" => Self::Drafts,
            "trash" => Self::Trash,
            "junk" => Self::Junk,
            "archive" => Self::Archive,
            "all" => Self::All,
            "flagged" => Self::Flagged,
            "important" => Self::Important,
            "outbox" => Self::Outbox,
            "other" => Self::Other,
            other => {
                return Err(AppError::invalid_input(format!(
                    "unknown folder role: {other}"
                )))
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    pub id: Id,
    pub account_id: Id,
    pub parent_id: Option<Id>,
    pub remote_path: String,
    pub name: String,
    pub role: FolderRole,
    pub unread_count: i64,
    pub total_count: i64,
    pub is_favorite: bool,
    pub sync_enabled: bool,
    /// `\Noselect` folders are shown as tree nodes only.
    pub is_selectable: bool,
    pub sort_order: i64,
    pub last_sync_at: Option<DateTime<Utc>>,
}

impl Folder {
    pub fn new(
        id: impl Into<Id>,
        account_id: impl Into<Id>,
        name: impl Into<String>,
        role: FolderRole,
    ) -> Self {
        let name = name.into();
        Self {
            id: id.into(),
            account_id: account_id.into(),
            parent_id: None,
            remote_path: name.clone(),
            name,
            role,
            unread_count: 0,
            total_count: 0,
            is_favorite: false,
            sync_enabled: true,
            is_selectable: true,
            sort_order: 0,
            last_sync_at: None,
        }
    }
}

/// A node of the folder tree (children are sorted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderTree {
    pub folder: Folder,
    pub children: Vec<FolderTree>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Address {
    pub name: Option<String>,
    pub email: String,
}

impl Address {
    pub fn new(email: impl Into<String>) -> Self {
        Self {
            name: None,
            email: email.into(),
        }
    }

    pub fn with_name(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            email: email.into(),
        }
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.name {
            Some(name) if !name.is_empty() => write!(f, "{name} <{}>", self.email),
            _ => f.write_str(&self.email),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Importance {
    Low,
    #[default]
    Normal,
    High,
}

impl Importance {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
        }
    }
}

impl FromStr for Importance {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "low" => Ok(Self::Low),
            "normal" => Ok(Self::Normal),
            "high" => Ok(Self::High),
            other => Err(AppError::invalid_input(format!(
                "unknown importance: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageSummary {
    pub id: Id,
    pub account_id: Id,
    pub folder_id: Id,
    pub thread_id: String,
    pub subject: String,
    pub r#from: Option<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub date: DateTime<Utc>,
    pub preview: String,
    pub seen: bool,
    pub flagged: bool,
    pub answered: bool,
    pub forwarded: bool,
    pub is_draft: bool,
    pub has_attachments: bool,
    pub importance: Importance,
    pub size: u64,
    /// Messages in the conversation (1 when conversation view is off).
    pub thread_count: i64,
    /// Unread messages in the conversation.
    pub thread_unread: i64,
}

impl MessageSummary {
    /// Minimal summary for a message; callers fill the remaining fields.
    pub fn new(
        id: impl Into<Id>,
        account_id: impl Into<Id>,
        folder_id: impl Into<Id>,
        thread_id: impl Into<String>,
        date: DateTime<Utc>,
    ) -> Self {
        Self {
            id: id.into(),
            account_id: account_id.into(),
            folder_id: folder_id.into(),
            thread_id: thread_id.into(),
            subject: String::new(),
            r#from: None,
            to: Vec::new(),
            cc: Vec::new(),
            date,
            preview: String::new(),
            seen: false,
            flagged: false,
            answered: false,
            forwarded: false,
            is_draft: false,
            has_attachments: false,
            importance: Importance::Normal,
            size: 0,
            thread_count: 1,
            thread_unread: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum MailScope {
    Folder {
        folder_id: Id,
    },
    /// Unified view across accounts with includeInUnified = true.
    Unified {
        role: FolderRole,
    },
    Search {
        query: SearchQuery,
    },
}

impl MailScope {
    pub fn folder(folder_id: impl Into<Id>) -> Self {
        Self::Folder {
            folder_id: folder_id.into(),
        }
    }

    pub fn unified(role: FolderRole) -> Self {
        Self::Unified { role }
    }

    pub fn search(query: SearchQuery) -> Self {
        Self::Search { query }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum MailFilter {
    #[default]
    All,
    Unread,
    Flagged,
    Attachments,
    MentionsMe,
}

impl MailFilter {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Unread => "unread",
            Self::Flagged => "flagged",
            Self::Attachments => "attachments",
            Self::MentionsMe => "mentionsMe",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageListQuery {
    pub scope: MailScope,
    pub filter: MailFilter,
    pub conversation: bool,
    /// Opaque cursor returned by the previous page; `null` for first page.
    pub cursor: Option<String>,
    pub limit: u32,
}

impl MessageListQuery {
    pub fn new(scope: MailScope) -> Self {
        Self {
            scope,
            filter: MailFilter::All,
            conversation: true,
            cursor: None,
            limit: 50,
        }
    }

    pub fn with_filter(mut self, filter: MailFilter) -> Self {
        self.filter = filter;
        self
    }

    pub fn with_conversation(mut self, conversation: bool) -> Self {
        self.conversation = conversation;
        self
    }

    pub fn with_cursor(mut self, cursor: Option<String>) -> Self {
        self.cursor = cursor;
        self
    }

    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = limit;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePage {
    pub items: Vec<MessageSummary>,
    pub next_cursor: Option<String>,
    pub total: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SearchQuery {
    pub text: String,
    pub account_id: Option<Id>,
    pub folder_id: Option<Id>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub subject: Option<String>,
    pub has_attachment: Option<bool>,
    pub unread: Option<bool>,
    pub flagged: Option<bool>,
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    /// Also query the IMAP server (results are fetched and cached).
    pub include_server: bool,
}

impl SearchQuery {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub id: Id,
    pub message_id: Id,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    pub content_id: Option<String>,
    pub is_inline: bool,
    pub is_downloaded: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvitePreview {
    /// iTIP METHOD: REQUEST | CANCEL | REPLY | COUNTER | PUBLISH.
    pub method: String,
    pub event: CalendarEvent,
    /// Matching event already in a local calendar.
    pub existing_event_id: Option<Id>,
    pub can_respond: bool,
    pub my_response: Option<AttendeeStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageDetail {
    pub summary: MessageSummary,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub message_id_header: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// Sanitized HTML document ready for `<iframe sandbox srcdoc>`.
    pub html: String,
    pub text: Option<String>,
    pub has_remote_content: bool,
    pub remote_content_allowed: bool,
    pub attachments: Vec<Attachment>,
    pub invite: Option<InvitePreview>,
    pub list_unsubscribe: Option<String>,
    /// Raw RFC822 header block.
    pub raw_headers: String,
    /// Present when opened from an .eml file not stored in any account.
    pub external_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum MessageAction {
    SetSeen { value: bool },
    SetFlagged { value: bool },
    Move { folder_id: Id },
    Archive,
    Delete { permanent: bool },
    MarkJunk { value: bool },
}

impl MessageAction {
    /// Stable identifier used in the undo log and the operation queue.
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::SetSeen { .. } => "setSeen",
            Self::SetFlagged { .. } => "setFlagged",
            Self::Move { .. } => "move",
            Self::Archive => "archive",
            Self::Delete { .. } => "delete",
            Self::MarkJunk { .. } => "markJunk",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReplyMode {
    Reply,
    ReplyAll,
    Forward,
    ForwardAsAttachment,
    EditAsNew,
}

impl ReplyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::ReplyAll => "replyAll",
            Self::Forward => "forward",
            Self::ForwardAsAttachment => "forwardAsAttachment",
            Self::EditAsNew => "editAsNew",
        }
    }
}

impl FromStr for ReplyMode {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "reply" => Ok(Self::Reply),
            "replyAll" => Ok(Self::ReplyAll),
            "forward" => Ok(Self::Forward),
            "forwardAsAttachment" => Ok(Self::ForwardAsAttachment),
            "editAsNew" => Ok(Self::EditAsNew),
            other => Err(AppError::invalid_input(format!(
                "unknown reply mode: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftAttachment {
    pub id: Id,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    /// Local staged file path (backend-managed).
    pub path: String,
    /// Inline image: referenced from HTML as `cid:<contentId>`.
    pub content_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Draft {
    /// Local draft id (`null` until first save).
    pub id: Option<Id>,
    pub account_id: Id,
    pub identity_id: Option<Id>,
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub subject: String,
    /// Editor HTML (inline images use `cid:<contentId>`).
    pub html: String,
    pub attachments: Vec<DraftAttachment>,
    pub reply_mode: Option<ReplyMode>,
    /// Local message id being replied to / forwarded.
    pub source_message_id: Option<Id>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub importance: Importance,
    pub request_read_receipt: bool,
    /// Remote message id of the draft in the Drafts folder, if synced.
    pub remote_draft_message_id: Option<Id>,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuleField {
    #[serde(rename = "from")]
    From,
    To,
    Cc,
    Subject,
    Body,
    HasAttachment,
    AnyRecipient,
}

impl RuleField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::From => "from",
            Self::To => "to",
            Self::Cc => "cc",
            Self::Subject => "subject",
            Self::Body => "body",
            Self::HasAttachment => "hasAttachment",
            Self::AnyRecipient => "anyRecipient",
        }
    }
}

impl FromStr for RuleField {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "from" => Ok(Self::From),
            "to" => Ok(Self::To),
            "cc" => Ok(Self::Cc),
            "subject" => Ok(Self::Subject),
            "body" => Ok(Self::Body),
            "hasAttachment" => Ok(Self::HasAttachment),
            "anyRecipient" => Ok(Self::AnyRecipient),
            other => Err(AppError::invalid_input(format!(
                "unknown rule field: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuleOp {
    Contains,
    NotContains,
    Equals,
    StartsWith,
    EndsWith,
    MatchesRegex,
    IsTrue,
}

impl RuleOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Contains => "contains",
            Self::NotContains => "notContains",
            Self::Equals => "equals",
            Self::StartsWith => "startsWith",
            Self::EndsWith => "endsWith",
            Self::MatchesRegex => "matchesRegex",
            Self::IsTrue => "isTrue",
        }
    }
}

impl FromStr for RuleOp {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "contains" => Ok(Self::Contains),
            "notContains" => Ok(Self::NotContains),
            "equals" => Ok(Self::Equals),
            "startsWith" => Ok(Self::StartsWith),
            "endsWith" => Ok(Self::EndsWith),
            "matchesRegex" => Ok(Self::MatchesRegex),
            "isTrue" => Ok(Self::IsTrue),
            other => Err(AppError::invalid_input(format!("unknown rule op: {other}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuleActionKind {
    Move,
    MarkRead,
    Flag,
    Delete,
    MarkJunk,
    Archive,
    Notify,
    SkipNotify,
}

impl RuleActionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Move => "move",
            Self::MarkRead => "markRead",
            Self::Flag => "flag",
            Self::Delete => "delete",
            Self::MarkJunk => "markJunk",
            Self::Archive => "archive",
            Self::Notify => "notify",
            Self::SkipNotify => "skipNotify",
        }
    }
}

impl FromStr for RuleActionKind {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "move" => Ok(Self::Move),
            "markRead" => Ok(Self::MarkRead),
            "flag" => Ok(Self::Flag),
            "delete" => Ok(Self::Delete),
            "markJunk" => Ok(Self::MarkJunk),
            "archive" => Ok(Self::Archive),
            "notify" => Ok(Self::Notify),
            "skipNotify" => Ok(Self::SkipNotify),
            other => Err(AppError::invalid_input(format!(
                "unknown rule action: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleCondition {
    pub field: RuleField,
    pub op: RuleOp,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleAction {
    pub kind: RuleActionKind,
    /// Target folder for "move".
    pub folder_id: Option<Id>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailRule {
    pub id: Id,
    /// `null` = applies to all accounts.
    pub account_id: Option<Id>,
    pub name: String,
    pub enabled: bool,
    /// `true` = all conditions must match (AND); `false` = any (OR).
    pub match_all: bool,
    pub conditions: Vec<RuleCondition>,
    pub actions: Vec<RuleAction>,
    pub stop_processing: bool,
    pub sort_order: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnifiedCounts {
    pub inbox: i64,
    pub flagged: i64,
    pub drafts: i64,
    pub outbox: i64,
}

/* ───────────────────────────── Calendar ───────────────────────────── */

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Calendar {
    pub id: Id,
    /// Owning account; local calendars belong to the built-in "local" account.
    pub account_id: Id,
    pub name: String,
    pub color: String,
    pub is_read_only: bool,
    pub is_visible: bool,
    pub is_default: bool,
    /// Read-only webcal subscription.
    pub subscription_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Weekday {
    #[serde(rename = "MO")]
    Mo,
    #[serde(rename = "TU")]
    Tu,
    #[serde(rename = "WE")]
    We,
    #[serde(rename = "TH")]
    Th,
    #[serde(rename = "FR")]
    Fr,
    #[serde(rename = "SA")]
    Sa,
    #[serde(rename = "SU")]
    Su,
}

impl Weekday {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mo => "MO",
            Self::Tu => "TU",
            Self::We => "WE",
            Self::Th => "TH",
            Self::Fr => "FR",
            Self::Sa => "SA",
            Self::Su => "SU",
        }
    }

    /// ISO weekday number (Monday = 1).
    pub fn iso_number(self) -> u32 {
        match self {
            Self::Mo => 1,
            Self::Tu => 2,
            Self::We => 3,
            Self::Th => 4,
            Self::Fr => 5,
            Self::Sa => 6,
            Self::Su => 7,
        }
    }
}

impl fmt::Display for Weekday {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Weekday {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_uppercase().as_str() {
            "MO" => Ok(Self::Mo),
            "TU" => Ok(Self::Tu),
            "WE" => Ok(Self::We),
            "TH" => Ok(Self::Th),
            "FR" => Ok(Self::Fr),
            "SA" => Ok(Self::Sa),
            "SU" => Ok(Self::Su),
            other => Err(AppError::invalid_input(format!("unknown weekday: {other}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum RecurrenceFreq {
    Daily,
    #[default]
    Weekly,
    Monthly,
    Yearly,
}

impl RecurrenceFreq {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
            Self::Yearly => "yearly",
        }
    }
}

impl FromStr for RecurrenceFreq {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "daily" => Ok(Self::Daily),
            "weekly" => Ok(Self::Weekly),
            "monthly" => Ok(Self::Monthly),
            "yearly" => Ok(Self::Yearly),
            other => Err(AppError::invalid_input(format!(
                "unknown recurrence frequency: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RecurrenceRule {
    pub freq: RecurrenceFreq,
    pub interval: u32,
    pub by_weekday: Vec<Weekday>,
    /// For monthly/yearly "2nd Tuesday" style rules (with byWeekday).
    pub by_set_pos: Vec<i32>,
    pub by_month_day: Vec<i32>,
    pub by_month: Vec<i32>,
    pub count: Option<u32>,
    pub until: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AttendeeStatus {
    #[default]
    NeedsAction,
    Accepted,
    Declined,
    Tentative,
    Delegated,
}

impl AttendeeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NeedsAction => "needsAction",
            Self::Accepted => "accepted",
            Self::Declined => "declined",
            Self::Tentative => "tentative",
            Self::Delegated => "delegated",
        }
    }
}

impl FromStr for AttendeeStatus {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "needsAction" => Ok(Self::NeedsAction),
            "accepted" => Ok(Self::Accepted),
            "declined" => Ok(Self::Declined),
            "tentative" => Ok(Self::Tentative),
            "delegated" => Ok(Self::Delegated),
            other => Err(AppError::invalid_input(format!(
                "unknown attendee status: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AttendeeRole {
    #[default]
    Required,
    Optional,
    Resource,
    Chair,
}

impl AttendeeRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Optional => "optional",
            Self::Resource => "resource",
            Self::Chair => "chair",
        }
    }
}

impl FromStr for AttendeeRole {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "required" => Ok(Self::Required),
            "optional" => Ok(Self::Optional),
            "resource" => Ok(Self::Resource),
            "chair" => Ok(Self::Chair),
            other => Err(AppError::invalid_input(format!(
                "unknown attendee role: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ShowAs {
    #[default]
    Busy,
    Free,
    Tentative,
    Oof,
    WorkingElsewhere,
}

impl ShowAs {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::Free => "free",
            Self::Tentative => "tentative",
            Self::Oof => "oof",
            Self::WorkingElsewhere => "workingElsewhere",
        }
    }
}

impl FromStr for ShowAs {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "busy" => Ok(Self::Busy),
            "free" => Ok(Self::Free),
            "tentative" => Ok(Self::Tentative),
            "oof" => Ok(Self::Oof),
            "workingElsewhere" => Ok(Self::WorkingElsewhere),
            other => Err(AppError::invalid_input(format!("unknown showAs: {other}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EventStatus {
    #[default]
    Confirmed,
    Tentative,
    Cancelled,
}

impl EventStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Tentative => "tentative",
            Self::Cancelled => "cancelled",
        }
    }
}

impl FromStr for EventStatus {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "confirmed" => Ok(Self::Confirmed),
            "tentative" => Ok(Self::Tentative),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(AppError::invalid_input(format!(
                "unknown event status: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attendee {
    pub email: String,
    pub name: Option<String>,
    pub role: AttendeeRole,
    pub status: AttendeeStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarEvent {
    pub id: Id,
    pub calendar_id: Id,
    pub account_id: Id,
    /// iCalendar UID.
    pub uid: String,
    pub title: String,
    pub location: String,
    /// Plain text description.
    pub description: String,
    pub start: String,
    pub end: String,
    pub all_day: bool,
    /// IANA time zone, e.g. "Asia/Shanghai".
    pub time_zone: String,
    pub recurrence: Option<RecurrenceRule>,
    /// Series master id when this is a detached occurrence (exception).
    pub series_id: Option<Id>,
    /// Original start of this occurrence for exceptions.
    pub recurrence_id: Option<String>,
    pub status: EventStatus,
    pub show_as: ShowAs,
    pub is_private: bool,
    pub organizer: Option<Attendee>,
    pub attendees: Vec<Attendee>,
    /// Minutes before start.
    pub reminders: Vec<i32>,
    pub is_organizer: bool,
    pub my_response: Option<AttendeeStatus>,
    pub online_meeting_url: Option<String>,
    /// Overrides calendar color when set.
    pub color: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl CalendarEvent {
    pub fn new(
        id: impl Into<Id>,
        calendar_id: impl Into<Id>,
        account_id: impl Into<Id>,
        uid: impl Into<String>,
        title: impl Into<String>,
        start: impl Into<String>,
        end: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            calendar_id: calendar_id.into(),
            account_id: account_id.into(),
            uid: uid.into(),
            title: title.into(),
            location: String::new(),
            description: String::new(),
            start: start.into(),
            end: end.into(),
            all_day: false,
            time_zone: "UTC".into(),
            recurrence: None,
            series_id: None,
            recurrence_id: None,
            status: EventStatus::Confirmed,
            show_as: ShowAs::Busy,
            is_private: false,
            organizer: None,
            attendees: Vec::new(),
            reminders: Vec::new(),
            is_organizer: false,
            my_response: None,
            online_meeting_url: None,
            color: None,
            updated_at: None,
        }
    }
}

/// One concrete occurrence for rendering a time range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventInstance {
    pub event_id: Id,
    /// Stable key: `${eventId}:${start}`.
    pub instance_key: String,
    pub calendar_id: Id,
    pub account_id: Id,
    pub title: String,
    pub location: String,
    pub start: String,
    pub end: String,
    pub all_day: bool,
    pub color: String,
    pub is_recurring: bool,
    pub show_as: ShowAs,
    pub status: EventStatus,
    pub my_response: Option<AttendeeStatus>,
    pub has_attendees: bool,
    pub has_reminder: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventInput {
    pub calendar_id: Id,
    pub title: String,
    pub location: String,
    pub description: String,
    pub start: String,
    pub end: String,
    pub all_day: bool,
    pub time_zone: String,
    pub recurrence: Option<RecurrenceRule>,
    pub show_as: ShowAs,
    pub is_private: bool,
    pub attendees: Vec<Attendee>,
    pub reminders: Vec<i32>,
    pub online_meeting_url: Option<String>,
    pub color: Option<String>,
    /// Send invitations / updates to attendees.
    pub notify_attendees: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RecurrenceEditScope {
    This,
    ThisAndFollowing,
    All,
}

/* ───────────────────────────── Contacts ───────────────────────────── */

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressBook {
    pub id: Id,
    pub account_id: Id,
    pub name: String,
    pub is_read_only: bool,
    pub is_default: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ContactFieldType {
    Home,
    Work,
    Mobile,
    Fax,
    Pager,
    Main,
    #[default]
    Other,
}

impl ContactFieldType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Work => "work",
            Self::Mobile => "mobile",
            Self::Fax => "fax",
            Self::Pager => "pager",
            Self::Main => "main",
            Self::Other => "other",
        }
    }
}

impl FromStr for ContactFieldType {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "home" => Ok(Self::Home),
            "work" => Ok(Self::Work),
            "mobile" => Ok(Self::Mobile),
            "fax" => Ok(Self::Fax),
            "pager" => Ok(Self::Pager),
            "main" => Ok(Self::Main),
            "other" => Ok(Self::Other),
            other => Err(AppError::invalid_input(format!(
                "unknown contact field type: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContactField {
    #[serde(rename = "type")]
    pub r#type: ContactFieldType,
    pub value: String,
    pub is_primary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PostalAddress {
    #[serde(rename = "type")]
    pub r#type: ContactFieldType,
    pub street: String,
    pub city: String,
    pub region: String,
    pub postal_code: String,
    pub country: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Contact {
    pub id: Id,
    pub address_book_id: Id,
    pub account_id: Id,
    pub display_name: String,
    pub given_name: String,
    pub family_name: String,
    pub middle_name: String,
    pub nickname: String,
    pub company: String,
    pub job_title: String,
    pub department: String,
    pub emails: Vec<ContactField>,
    pub phones: Vec<ContactField>,
    pub addresses: Vec<PostalAddress>,
    pub websites: Vec<String>,
    /// "YYYY-MM-DD" or "--MM-DD" when year unknown.
    pub birthday: Option<String>,
    pub notes: String,
    /// data: URL of a small (<=256px) photo.
    pub photo: Option<String>,
    pub is_favorite: bool,
    pub group_ids: Vec<Id>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl Contact {
    pub fn new(
        id: impl Into<Id>,
        address_book_id: impl Into<Id>,
        account_id: impl Into<Id>,
    ) -> Self {
        Self {
            id: id.into(),
            address_book_id: address_book_id.into(),
            account_id: account_id.into(),
            display_name: String::new(),
            given_name: String::new(),
            family_name: String::new(),
            middle_name: String::new(),
            nickname: String::new(),
            company: String::new(),
            job_title: String::new(),
            department: String::new(),
            emails: Vec::new(),
            phones: Vec::new(),
            addresses: Vec::new(),
            websites: Vec::new(),
            birthday: None,
            notes: String::new(),
            photo: None,
            is_favorite: false,
            group_ids: Vec::new(),
            updated_at: None,
        }
    }

    /// Best display label: display name, else "Given Family", else first email.
    pub fn label(&self) -> String {
        if !self.display_name.is_empty() {
            return self.display_name.clone();
        }
        let joined = [self.given_name.as_str(), self.family_name.as_str()]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        if !joined.is_empty() {
            return joined;
        }
        self.emails
            .first()
            .map(|e| e.value.clone())
            .unwrap_or_default()
    }

    pub fn primary_email(&self) -> Option<String> {
        self.emails
            .iter()
            .find(|e| e.is_primary)
            .or_else(|| self.emails.first())
            .map(|e| e.value.clone())
    }

    pub fn primary_phone(&self) -> Option<String> {
        self.phones
            .iter()
            .find(|p| p.is_primary)
            .or_else(|| self.phones.first())
            .map(|p| p.value.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContactSummary {
    pub id: Id,
    pub account_id: Id,
    pub address_book_id: Id,
    pub display_name: String,
    pub primary_email: Option<String>,
    pub primary_phone: Option<String>,
    pub company: String,
    pub photo: Option<String>,
    pub is_favorite: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContactGroup {
    pub id: Id,
    pub address_book_id: Id,
    pub account_id: Id,
    pub name: String,
    pub member_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContactListQuery {
    pub address_book_id: Option<Id>,
    pub group_id: Option<Id>,
    pub query: Option<String>,
    pub favorites_only: bool,
    pub cursor: Option<String>,
    pub limit: u32,
}

impl Default for ContactListQuery {
    fn default() -> Self {
        Self {
            address_book_id: None,
            group_id: None,
            query: None,
            favorites_only: false,
            cursor: None,
            limit: 100,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContactPage {
    pub items: Vec<ContactSummary>,
    pub next_cursor: Option<String>,
    pub total: i64,
}

/* ─────────────────────────────── To Do ────────────────────────────── */

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SmartList {
    MyDay,
    Important,
    Planned,
    All,
    Completed,
    FlaggedEmail,
}

impl SmartList {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MyDay => "myDay",
            Self::Important => "important",
            Self::Planned => "planned",
            Self::All => "all",
            Self::Completed => "completed",
            Self::FlaggedEmail => "flaggedEmail",
        }
    }
}

impl FromStr for SmartList {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "myDay" => Ok(Self::MyDay),
            "important" => Ok(Self::Important),
            "planned" => Ok(Self::Planned),
            "all" => Ok(Self::All),
            "completed" => Ok(Self::Completed),
            "flaggedEmail" => Ok(Self::FlaggedEmail),
            other => Err(AppError::invalid_input(format!(
                "unknown smart list: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskStatus {
    #[default]
    NotStarted,
    Completed,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "notStarted",
            Self::Completed => "completed",
        }
    }
}

impl FromStr for TaskStatus {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "notStarted" => Ok(Self::NotStarted),
            "completed" => Ok(Self::Completed),
            other => Err(AppError::invalid_input(format!(
                "unknown task status: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskList {
    pub id: Id,
    pub account_id: Id,
    pub name: String,
    pub color: String,
    pub is_default: bool,
    pub is_read_only: bool,
    pub incomplete_count: i64,
    pub sort_order: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStep {
    pub id: Id,
    pub title: String,
    pub is_completed: bool,
    pub sort_order: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: Id,
    pub list_id: Id,
    pub account_id: Id,
    pub title: String,
    pub notes: String,
    pub status: TaskStatus,
    pub importance: Importance,
    pub due_date: Option<String>,
    pub reminder_at: Option<DateTime<Utc>>,
    pub recurrence: Option<RecurrenceRule>,
    /// Date the task was added to "My Day" (it only shows on that date).
    pub my_day_date: Option<String>,
    pub completed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub steps: Vec<TaskStep>,
    pub sort_order: i64,
    /// Linked e-mail (for "flagged email" tasks).
    pub linked_message_id: Option<Id>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskInput {
    pub title: String,
    pub notes: String,
    pub importance: Importance,
    pub due_date: Option<String>,
    pub reminder_at: Option<DateTime<Utc>>,
    pub recurrence: Option<RecurrenceRule>,
    pub my_day_date: Option<String>,
    pub steps: Vec<TaskStep>,
}

impl Default for TaskInput {
    fn default() -> Self {
        Self {
            title: String::new(),
            notes: String::new(),
            importance: Importance::Normal,
            due_date: None,
            reminder_at: None,
            recurrence: None,
            my_day_date: None,
            steps: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskQuery {
    pub list_id: Option<Id>,
    pub smart: Option<SmartList>,
    pub include_completed: bool,
    /// Client local date for "My Day" / "planned" calculations.
    pub today: String,
}

impl TaskQuery {
    pub fn smart(smart: SmartList, today: impl Into<String>) -> Self {
        Self {
            list_id: None,
            smart: Some(smart),
            include_completed: false,
            today: today.into(),
        }
    }

    pub fn list(list_id: impl Into<Id>, today: impl Into<String>) -> Self {
        Self {
            list_id: Some(list_id.into()),
            smart: None,
            include_completed: false,
            today: today.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmartListCounts {
    pub my_day: i64,
    pub important: i64,
    pub planned: i64,
    pub all: i64,
    pub completed: i64,
    pub flagged_email: i64,
}

/* ───────────────────────────── Settings ───────────────────────────── */

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Language {
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en-US")]
    EnUs,
}

impl Language {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::ZhCn => "zh-CN",
            Self::EnUs => "en-US",
        }
    }
}

impl FromStr for Language {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "system" => Ok(Self::System),
            "zh-CN" => Ok(Self::ZhCn),
            "en-US" => Ok(Self::EnUs),
            other => Err(AppError::invalid_input(format!(
                "unknown language: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HoverAction {
    Archive,
    Delete,
    Flag,
    Read,
    Move,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CalendarView {
    Day,
    WorkWeek,
    Week,
    Month,
    Year,
    Agenda,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MailDensity {
    Compact,
    #[default]
    Normal,
    Spacious,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MarkReadMode {
    #[default]
    OnOpen,
    AfterDelay,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteImagesPolicy {
    Ask,
    TrustedOnly,
    /// Mirrors the defaults of the Outlook and 163 web clients: images load
    /// without a prompt. No proxy is involved, so loading them reveals the
    /// reader's IP to the sender; users who care can switch to `Ask`.
    #[default]
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AutoAdvance {
    #[default]
    Next,
    Previous,
    List,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContactsSortBy {
    #[default]
    FirstName,
    LastName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DisplayOrder {
    #[default]
    FirstLast,
    LastFirst,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MailSettings {
    pub conversation_view: bool,
    pub preview_lines: u32,
    pub density: MailDensity,
    pub show_avatars: bool,
    pub mark_read_mode: MarkReadMode,
    pub mark_read_delay_seconds: u32,
    pub remote_images: RemoteImagesPolicy,
    pub light_message_background: bool,
    pub hover_actions: Vec<HoverAction>,
    pub confirm_delete: bool,
    pub auto_advance: AutoAdvance,
    pub default_font_family: String,
    pub default_font_size: u32,
    pub prefetch_unread_bodies: u32,
    pub body_cache_limit_mb: u32,
    pub other_folders_poll_minutes: u32,
    /// Include the original message when replying.
    pub quote_original: bool,
}

impl Default for MailSettings {
    fn default() -> Self {
        Self {
            conversation_view: true,
            preview_lines: 1,
            density: MailDensity::Normal,
            show_avatars: true,
            mark_read_mode: MarkReadMode::OnOpen,
            mark_read_delay_seconds: 5,
            remote_images: RemoteImagesPolicy::Always,
            light_message_background: false,
            hover_actions: vec![HoverAction::Archive, HoverAction::Delete, HoverAction::Flag],
            confirm_delete: true,
            auto_advance: AutoAdvance::Next,
            default_font_family: String::new(),
            default_font_size: 14,
            prefetch_unread_bodies: 50,
            body_cache_limit_mb: 500,
            other_folders_poll_minutes: 15,
            quote_original: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CalendarSettings {
    pub default_view: CalendarView,
    pub first_day_of_week: u32,
    pub work_days: Vec<u32>,
    pub work_hours_start: String,
    pub work_hours_end: String,
    pub default_reminder_minutes: u32,
    pub default_event_duration_minutes: u32,
    pub show_week_numbers: bool,
    pub poll_minutes: u32,
}

impl Default for CalendarSettings {
    fn default() -> Self {
        Self {
            default_view: CalendarView::Week,
            first_day_of_week: 1,
            work_days: vec![1, 2, 3, 4, 5],
            work_hours_start: "09:00".into(),
            work_hours_end: "18:00".into(),
            default_reminder_minutes: 15,
            default_event_duration_minutes: 30,
            show_week_numbers: false,
            poll_minutes: 15,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ContactsSettings {
    pub sort_by: ContactsSortBy,
    pub display_order: DisplayOrder,
    pub poll_minutes: u32,
}

impl Default for ContactsSettings {
    fn default() -> Self {
        Self {
            sort_by: ContactsSortBy::FirstName,
            display_order: DisplayOrder::FirstLast,
            poll_minutes: 60,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TasksSettings {
    pub show_completed: bool,
    pub poll_minutes: u32,
    pub new_tasks_on_top: bool,
}

impl Default for TasksSettings {
    fn default() -> Self {
        Self {
            show_completed: false,
            poll_minutes: 30,
            new_tasks_on_top: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NotificationsSettings {
    pub enabled: bool,
    pub sound: bool,
    pub show_preview: bool,
    pub event_reminders: bool,
    pub task_reminders: bool,
}

impl Default for NotificationsSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            sound: true,
            show_preview: true,
            event_reminders: true,
            task_reminders: true,
        }
    }
}

impl std::fmt::Debug for AdvancedSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdvancedSettings")
            .field(
                "google_client_id",
                &self.google_client_id.as_ref().map(|_| "<set>"),
            )
            .field(
                "google_client_secret",
                &self.google_client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "microsoft_client_id",
                &self.microsoft_client_id.as_ref().map(|_| "<set>"),
            )
            .finish()
    }
}

impl std::fmt::Debug for AdvancedSettingsPatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdvancedSettingsPatch")
            .field(
                "google_client_id",
                &self.google_client_id.as_ref().map(|_| "<set>"),
            )
            .field(
                "google_client_secret",
                &self.google_client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "microsoft_client_id",
                &self.microsoft_client_id.as_ref().map(|_| "<set>"),
            )
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PrivacySettings {
    pub allow_ispdb_lookup: bool,
}

impl Default for PrivacySettings {
    fn default() -> Self {
        Self {
            allow_ispdb_lookup: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdatesSettings {
    pub auto_check: bool,
}

impl Default for UpdatesSettings {
    fn default() -> Self {
        Self { auto_check: true }
    }
}

#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AdvancedSettings {
    pub google_client_id: Option<String>,
    pub google_client_secret: Option<String>,
    pub microsoft_client_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    pub theme: ThemeMode,
    /// `null` = follow system accent.
    pub accent_color: Option<String>,
    pub language: Language,
    pub close_to_tray: bool,
    pub launch_at_startup: bool,
    pub start_minimized: bool,
    pub mail: MailSettings,
    pub calendar: CalendarSettings,
    pub contacts: ContactsSettings,
    pub tasks: TasksSettings,
    pub notifications: NotificationsSettings,
    pub privacy: PrivacySettings,
    pub updates: UpdatesSettings,
    pub advanced: AdvancedSettings,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme: ThemeMode::System,
            accent_color: None,
            language: Language::System,
            close_to_tray: true,
            launch_at_startup: false,
            start_minimized: false,
            mail: MailSettings::default(),
            calendar: CalendarSettings::default(),
            contacts: ContactsSettings::default(),
            tasks: TasksSettings::default(),
            notifications: NotificationsSettings::default(),
            privacy: PrivacySettings::default(),
            updates: UpdatesSettings::default(),
            advanced: AdvancedSettings::default(),
        }
    }
}

impl AppSettings {
    /// Serialize the settings to a JSON object (used for deep merge and storage).
    pub fn to_value(&self) -> Result<Value> {
        serde_json::to_value(self).map_err(AppError::from)
    }
}

/* ──────────────────── Settings patch (deep partial) ───────────────── */

macro_rules! patch_struct {
    ($name:ident { $( $field:ident : $ty:ty ),* $(,)? }) => {
        #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase", default)]
        pub struct $name {
            $(
                #[serde(skip_serializing_if = "Option::is_none")]
                pub $field: Option<$ty>,
            )*
        }
    };
}

patch_struct!(MailSettingsPatch {
    conversation_view: bool,
    preview_lines: u32,
    density: MailDensity,
    show_avatars: bool,
    mark_read_mode: MarkReadMode,
    mark_read_delay_seconds: u32,
    remote_images: RemoteImagesPolicy,
    light_message_background: bool,
    hover_actions: Vec<HoverAction>,
    confirm_delete: bool,
    auto_advance: AutoAdvance,
    default_font_family: String,
    default_font_size: u32,
    prefetch_unread_bodies: u32,
    body_cache_limit_mb: u32,
    other_folders_poll_minutes: u32,
    quote_original: bool,
});

patch_struct!(CalendarSettingsPatch {
    default_view: CalendarView,
    first_day_of_week: u32,
    work_days: Vec<u32>,
    work_hours_start: String,
    work_hours_end: String,
    default_reminder_minutes: u32,
    default_event_duration_minutes: u32,
    show_week_numbers: bool,
    poll_minutes: u32,
});

patch_struct!(ContactsSettingsPatch {
    sort_by: ContactsSortBy,
    display_order: DisplayOrder,
    poll_minutes: u32,
});

patch_struct!(TasksSettingsPatch {
    show_completed: bool,
    poll_minutes: u32,
    new_tasks_on_top: bool,
});

patch_struct!(NotificationsSettingsPatch {
    enabled: bool,
    sound: bool,
    show_preview: bool,
    event_reminders: bool,
    task_reminders: bool,
});

patch_struct!(PrivacySettingsPatch {
    allow_ispdb_lookup: bool,
});

patch_struct!(UpdatesSettingsPatch { auto_check: bool });

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AdvancedSettingsPatch {
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_double_option"
    )]
    pub google_client_id: Option<Option<String>>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_double_option"
    )]
    pub google_client_secret: Option<Option<String>>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_double_option"
    )]
    pub microsoft_client_id: Option<Option<String>>,
}

/// Deep partial used by `settings_update` (objects are merged, arrays replaced).
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SettingsPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<ThemeMode>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_double_option"
    )]
    pub accent_color: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_to_tray: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch_at_startup: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_minimized: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mail: Option<MailSettingsPatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calendar: Option<CalendarSettingsPatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contacts: Option<ContactsSettingsPatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tasks: Option<TasksSettingsPatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notifications: Option<NotificationsSettingsPatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub privacy: Option<PrivacySettingsPatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updates: Option<UpdatesSettingsPatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advanced: Option<AdvancedSettingsPatch>,
}

impl std::fmt::Debug for SettingsPatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsPatch")
            .field("theme", &self.theme)
            .field("accent_color", &self.accent_color)
            .field("language", &self.language)
            .field("close_to_tray", &self.close_to_tray)
            .field("launch_at_startup", &self.launch_at_startup)
            .field("start_minimized", &self.start_minimized)
            .field("mail", &self.mail)
            .field("calendar", &self.calendar)
            .field("contacts", &self.contacts)
            .field("tasks", &self.tasks)
            .field("notifications", &self.notifications)
            .field("privacy", &self.privacy)
            .field("updates", &self.updates)
            .field("advanced", &self.advanced)
            .finish()
    }
}

impl SettingsPatch {
    /// Serialize to the JSON object that is deep-merged into the current settings.
    /// `None` fields are omitted so they do not overwrite anything.
    pub fn to_patch_value(&self) -> Result<Value> {
        serde_json::to_value(self).map_err(AppError::from)
    }
}

/// Recursively merge `patch` into `base`: JSON objects are merged key-by-key,
/// arrays and scalars are replaced, `null` explicitly clears a value.
pub fn deep_merge(base: &mut Value, patch: Value) {
    match (base, patch) {
        (Value::Object(base_map), Value::Object(patch_map)) => {
            for (key, value) in patch_map {
                if value.is_null() {
                    base_map.insert(key, Value::Null);
                } else {
                    match base_map.get_mut(&key) {
                        Some(existing) => deep_merge(existing, value),
                        None => {
                            base_map.insert(key, value);
                        }
                    }
                }
            }
        }
        (base_slot, replacement) => {
            *base_slot = replacement;
        }
    }
}

/// Apply a settings patch on top of existing settings.
pub fn merge_settings(base: &AppSettings, patch: &SettingsPatch) -> Result<AppSettings> {
    let mut value = base.to_value()?;
    deep_merge(&mut value, patch.to_patch_value()?);
    serde_json::from_value(value)
        .map_err(|e| AppError::invalid_input(format!("invalid settings patch: {e}")))
}

/* ─────────────────────────── Sync / app state ─────────────────────── */

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SyncModule {
    Mail,
    Calendar,
    Contacts,
    Tasks,
}

impl SyncModule {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mail => "mail",
            Self::Calendar => "calendar",
            Self::Contacts => "contacts",
            Self::Tasks => "tasks",
        }
    }

    /// Default poll interval from the settings (minutes) if no IDLE is used.
    pub fn default_poll_minutes(self) -> u32 {
        match self {
            Self::Mail => 15,
            Self::Calendar => 15,
            Self::Contacts => 60,
            Self::Tasks => 30,
        }
    }
}

impl fmt::Display for SyncModule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SyncModule {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "mail" => Ok(Self::Mail),
            "calendar" => Ok(Self::Calendar),
            "contacts" => Ok(Self::Contacts),
            "tasks" => Ok(Self::Tasks),
            other => Err(AppError::invalid_input(format!(
                "unknown sync module: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SyncState {
    #[default]
    Idle,
    Syncing,
    Error,
    /// A transient provider/transport failure; the next poll will retry.
    /// Device connectivity is not represented here — the UI reads that from
    /// `navigator.onLine` itself.
    Retrying,
    NeedsReauth,
}

impl SyncState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Syncing => "syncing",
            Self::Error => "error",
            Self::Retrying => "retrying",
            Self::NeedsReauth => "needsReauth",
        }
    }
}

impl FromStr for SyncState {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "idle" => Ok(Self::Idle),
            "syncing" => Ok(Self::Syncing),
            "error" => Ok(Self::Error),
            "retrying" => Ok(Self::Retrying),
            "needsReauth" => Ok(Self::NeedsReauth),
            other => Err(AppError::invalid_input(format!(
                "unknown sync state: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncProgress {
    pub done: u64,
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    pub account_id: Id,
    pub module: SyncModule,
    pub state: SyncState,
    /// e.g. `{ done: 120, total: 800 }` during initial sync.
    pub progress: Option<SyncProgress>,
    pub message: Option<String>,
    pub last_sync_at: Option<DateTime<Utc>>,
}

impl SyncStatus {
    pub fn new(account_id: impl Into<Id>, module: SyncModule) -> Self {
        Self {
            account_id: account_id.into(),
            module,
            state: SyncState::Idle,
            progress: None,
            message: None,
            last_sync_at: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Platform {
    #[default]
    Windows,
    #[serde(rename = "macos")]
    Macos,
    Linux,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    pub platform: Platform,
    pub data_dir: String,
    pub log_dir: String,
    pub build_has_google_client: bool,
    pub build_has_microsoft_client: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageUsage {
    pub database_bytes: u64,
    pub mime_cache_bytes: u64,
    pub attachment_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AppModule {
    #[default]
    Mail,
    Calendar,
    People,
    Todo,
}

impl AppModule {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mail => "mail",
            Self::Calendar => "calendar",
            Self::People => "people",
            Self::Todo => "todo",
        }
    }
}

impl fmt::Display for AppModule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AppModule {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "mail" => Ok(Self::Mail),
            "calendar" => Ok(Self::Calendar),
            "people" => Ok(Self::People),
            "todo" => Ok(Self::Todo),
            other => Err(AppError::invalid_input(format!(
                "unknown app module: {other}"
            ))),
        }
    }
}

/* ─────────────────────── Internal / persistence types ─────────────── */

/// Complete message row, including metadata that is not part of
/// `MessageSummary`. Used by the message repository and the mail engine.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredMessage {
    pub summary: MessageSummary,
    pub bcc: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub message_id_header: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub uid: Option<i64>,
    pub uidvalidity: Option<i64>,
    pub gm_msgid: Option<String>,
    pub gm_thrid: Option<String>,
    pub labels: Vec<String>,
    pub raw_headers: Option<String>,
    pub body_path: Option<String>,
    pub body_cached_at: Option<DateTime<Utc>>,
    pub internal_date: Option<DateTime<Utc>>,
    pub is_deleted_local: bool,
}

impl StoredMessage {
    pub fn new(summary: MessageSummary) -> Self {
        Self {
            summary,
            bcc: Vec::new(),
            reply_to: Vec::new(),
            message_id_header: None,
            in_reply_to: None,
            references: Vec::new(),
            uid: None,
            uidvalidity: None,
            gm_msgid: None,
            gm_thrid: None,
            labels: Vec::new(),
            raw_headers: None,
            body_path: None,
            body_cached_at: None,
            internal_date: None,
            is_deleted_local: false,
        }
    }

    pub fn id(&self) -> &str {
        &self.summary.id
    }

    pub fn thread_id(&self) -> &str {
        &self.summary.thread_id
    }
}

impl From<MessageSummary> for StoredMessage {
    fn from(summary: MessageSummary) -> Self {
        Self::new(summary)
    }
}

/// Complete calendar event row plus sync metadata (`raw_ics`, `etag`, ...).
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEvent {
    pub event: CalendarEvent,
    pub raw_ics: Option<String>,
    pub etag: Option<String>,
    pub remote_id: Option<String>,
    pub href: Option<String>,
    pub rrule: Option<String>,
    /// EXDATE values as stored in iCalendar (RFC3339 or `YYYY-MM-DD`).
    pub exdates: Vec<String>,
}

impl StoredEvent {
    pub fn new(event: CalendarEvent) -> Self {
        Self {
            event,
            raw_ics: None,
            etag: None,
            remote_id: None,
            href: None,
            rrule: None,
            exdates: Vec::new(),
        }
    }
}

impl From<CalendarEvent> for StoredEvent {
    fn from(event: CalendarEvent) -> Self {
        Self::new(event)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PendingOpState {
    Pending,
    InProgress,
    Done,
    Failed,
    Cancelled,
}

impl PendingOpState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "inProgress",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl FromStr for PendingOpState {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "pending" => Ok(Self::Pending),
            "inProgress" => Ok(Self::InProgress),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(AppError::invalid_input(format!(
                "unknown pending op state: {other}"
            ))),
        }
    }
}

/// A queued server operation (IMAP STORE/MOVE/APPEND, CalDAV/CardDAV write...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingOp {
    pub id: Id,
    pub account_id: Id,
    pub kind: String,
    pub payload: Value,
    pub state: PendingOpState,
    pub attempts: u32,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub description: String,
    pub last_error: Option<String>,
}

impl PendingOp {
    /// Exponential backoff: 5s, 10s, 20s, ... capped at 10 minutes.
    pub fn retry_delay(attempts: u32) -> chrono::Duration {
        let shift = attempts.min(7);
        let seconds = 5u64.saturating_mul(1u64 << shift).min(600);
        chrono::Duration::seconds(seconds as i64)
    }
}

/// Values required to enqueue a new pending operation.
#[derive(Debug, Clone, PartialEq)]
pub struct NewPendingOp {
    pub account_id: Id,
    pub kind: String,
    pub payload: Value,
    pub description: String,
}

impl NewPendingOp {
    pub fn new(
        account_id: impl Into<Id>,
        kind: impl Into<String>,
        payload: Value,
        description: impl Into<String>,
    ) -> Self {
        Self {
            account_id: account_id.into(),
            kind: kind.into(),
            payload,
            description: description.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OutboxState {
    Pending,
    Sending,
    Sent,
    Failed,
}

impl OutboxState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Sending => "sending",
            Self::Sent => "sent",
            Self::Failed => "failed",
        }
    }
}

impl FromStr for OutboxState {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "pending" => Ok(Self::Pending),
            "sending" => Ok(Self::Sending),
            "sent" => Ok(Self::Sent),
            "failed" => Ok(Self::Failed),
            other => Err(AppError::invalid_input(format!(
                "unknown outbox state: {other}"
            ))),
        }
    }
}

/// A message waiting to be sent over SMTP (offline outbox).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxItem {
    pub id: Id,
    pub account_id: Id,
    pub draft_id: Option<Id>,
    pub recipients: Vec<Address>,
    pub subject: String,
    /// Path of the built RFC822 message inside `Paths::staging`.
    pub rfc822_path: String,
    pub state: OutboxState,
    pub attempts: u32,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub sent_at: Option<DateTime<Utc>>,
    /// When the current `sending` attempt began; `None` while pending.
    pub sending_started_at: Option<DateTime<Utc>>,
}

impl OutboxItem {
    pub fn new(
        account_id: impl Into<Id>,
        draft_id: Option<Id>,
        recipients: Vec<Address>,
        subject: impl Into<String>,
        rfc822_path: impl Into<String>,
    ) -> Self {
        Self {
            id: new_id(),
            account_id: account_id.into(),
            draft_id,
            recipients,
            subject: subject.into(),
            rfc822_path: rfc822_path.into(),
            state: OutboxState::Pending,
            attempts: 0,
            next_retry_at: None,
            last_error: None,
            created_at: now(),
            sent_at: None,
            sending_started_at: None,
        }
    }
}

/// One reversible action (used by `message_undo_last`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoEntry {
    pub id: Id,
    pub account_id: Id,
    pub kind: String,
    pub description: String,
    pub payload: Value,
    pub created_at: DateTime<Utc>,
}

impl UndoEntry {
    pub fn new(
        account_id: impl Into<Id>,
        kind: impl Into<String>,
        description: impl Into<String>,
        payload: Value,
    ) -> Self {
        Self {
            id: new_id(),
            account_id: account_id.into(),
            kind: kind.into(),
            description: description.into(),
            payload,
            created_at: now(),
        }
    }
}

/// Payload of `app://navigate` (also used for notification clicks).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigateTarget {
    pub module: AppModule,
    pub account_id: Option<Id>,
    pub folder_id: Option<Id>,
    pub message_id: Option<Id>,
    pub event_id: Option<Id>,
    pub task_id: Option<Id>,
}

impl NavigateTarget {
    pub fn module(module: AppModule) -> Self {
        Self {
            module,
            ..Self::default()
        }
    }

    pub fn mail(message_id: impl Into<Id>, account_id: Option<Id>, folder_id: Option<Id>) -> Self {
        Self {
            module: AppModule::Mail,
            account_id,
            folder_id,
            message_id: Some(message_id.into()),
            event_id: None,
            task_id: None,
        }
    }

    pub fn calendar(event_id: impl Into<Id>) -> Self {
        Self {
            module: AppModule::Calendar,
            event_id: Some(event_id.into()),
            ..Self::default()
        }
    }

    pub fn task(task_id: impl Into<Id>) -> Self {
        Self {
            module: AppModule::Todo,
            task_id: Some(task_id.into()),
            ..Self::default()
        }
    }
}

/// Opaque pagination cursor, encoded as base64url(`key` + US + `id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageCursor {
    pub key: String,
    pub id: String,
}

impl PageCursor {
    pub fn new(key: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            id: id.into(),
        }
    }

    /// Cursor from a message date/id pair.
    pub fn message(date: DateTime<Utc>, id: impl Into<Id>) -> Self {
        Self::new(date.to_rfc3339_opts(chrono::SecondsFormat::Secs, true), id)
    }

    /// Cursor from a contact display name/id pair.
    pub fn contact(name: impl Into<String>, id: impl Into<Id>) -> Self {
        Self::new(name, id)
    }

    pub fn encode(&self) -> String {
        let raw = format!("{}\u{1f}{}", self.key, self.id);
        URL_SAFE_NO_PAD.encode(raw.as_bytes())
    }

    pub fn decode(encoded: &str) -> Result<Self> {
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded.trim().as_bytes())
            .map_err(|_| AppError::invalid_input("invalid page cursor"))?;
        let raw =
            String::from_utf8(bytes).map_err(|_| AppError::invalid_input("invalid page cursor"))?;
        let (key, id) = raw
            .split_once('\u{1f}')
            .ok_or_else(|| AppError::invalid_input("invalid page cursor"))?;
        Ok(Self {
            key: key.to_string(),
            id: id.to_string(),
        })
    }
}

/// Alias documenting that message pagination uses `PageCursor`.
pub type MessageCursor = PageCursor;

/// One `sync_state` entry (account + module + key -> value).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncCursor {
    pub key: String,
    pub value: String,
}

impl SyncCursor {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }

    /// Parse the value as JSON of type `T`.
    pub fn json<T: for<'de> Deserialize<'de>>(&self) -> Result<Option<T>> {
        serde_json::from_str(&self.value)
            .map(Some)
            .map_err(|e| AppError::internal(format!("invalid sync cursor JSON: {e}")))
    }
}

/// Convenience map for provider capability overrides persisted in `sync_state`.
pub type CapabilityOverrides = BTreeMap<String, bool>;

/* ──────────────────────────────── tests ───────────────────────────── */

#[cfg(test)]
mod tests {
    use super::*;

    fn json<T: Serialize>(value: &T) -> String {
        serde_json::to_string(value).expect("serialize")
    }

    #[test]
    fn account_state_literal_values() {
        assert_eq!(json(&AccountState::NeedsReauth), "\"needsReauth\"");
        assert_eq!(json(&AccountState::Ok), "\"ok\"");
    }

    #[test]
    fn language_literal_values() {
        assert_eq!(json(&Language::ZhCn), "\"zh-CN\"");
        assert_eq!(json(&Language::EnUs), "\"en-US\"");
        assert_eq!(json(&Language::System), "\"system\"");
    }

    #[test]
    fn weekday_literal_values() {
        let weekdays = [
            Weekday::Mo,
            Weekday::Tu,
            Weekday::We,
            Weekday::Th,
            Weekday::Fr,
            Weekday::Sa,
            Weekday::Su,
        ];
        let encoded: Vec<String> = weekdays.iter().map(json).collect();
        assert_eq!(
            encoded,
            ["\"MO\"", "\"TU\"", "\"WE\"", "\"TH\"", "\"FR\"", "\"SA\"", "\"SU\""]
        );
    }

    #[test]
    fn mail_scope_tagged_union() {
        let folder = MailScope::folder("f1");
        assert_eq!(json(&folder), "{\"kind\":\"folder\",\"folderId\":\"f1\"}");
        let unified = MailScope::unified(FolderRole::Inbox);
        assert_eq!(json(&unified), "{\"kind\":\"unified\",\"role\":\"inbox\"}");
        let search = MailScope::search(SearchQuery::text("hi"));
        let value: Value = serde_json::from_str(&json(&search)).expect("parse");
        assert_eq!(value["kind"], "search");
        assert_eq!(value["query"]["text"], "hi");
        assert_eq!(value["query"]["accountId"], Value::Null);
        // Round-trip
        let back: MailScope = serde_json::from_str(&json(&search)).expect("deserialize");
        assert_eq!(back, search);
    }

    #[test]
    fn message_action_tagged_union() {
        assert_eq!(json(&MessageAction::Archive), "{\"kind\":\"archive\"}");
        assert_eq!(
            json(&MessageAction::Move {
                folder_id: "x".into()
            }),
            "{\"kind\":\"move\",\"folderId\":\"x\"}"
        );
        assert_eq!(
            json(&MessageAction::Delete { permanent: true }),
            "{\"kind\":\"delete\",\"permanent\":true}"
        );
    }

    #[test]
    fn account_patch_distinguishes_absent_and_null() {
        let absent: AccountPatch = serde_json::from_str("{}").expect("parse");
        assert!(absent.imap.is_none());

        let explicit_null: AccountPatch = serde_json::from_str("{\"imap\":null}").expect("parse");
        assert_eq!(explicit_null.imap, Some(None));

        let patch: AccountPatch = serde_json::from_str(
            "{\"displayName\":\"A\",\"imap\":{\"host\":\"h\",\"port\":993,\"security\":\"tls\",\"username\":\"u\"}}",
        )
        .expect("parse");
        assert_eq!(patch.display_name.as_deref(), Some("A"));
        assert_eq!(
            patch.imap.as_ref().and_then(|v| v.as_ref()).map(|c| c.port),
            Some(993)
        );
    }

    #[test]
    fn settings_patch_serialization_omits_absent() {
        let patch = SettingsPatch {
            theme: Some(ThemeMode::Dark),
            mail: Some(MailSettingsPatch {
                preview_lines: Some(2),
                ..Default::default()
            }),
            ..Default::default()
        };
        let value = patch.to_patch_value().expect("value");
        assert_eq!(value["theme"], "dark");
        assert_eq!(value["mail"]["previewLines"], 2);
        assert!(value.get("language").is_none());
        assert!(value["mail"].get("density").is_none());
    }

    #[test]
    fn settings_defaults_match_spec() {
        let s = AppSettings::default();
        assert_eq!(s.theme, ThemeMode::System);
        assert_eq!(s.language, Language::System);
        assert!(s.close_to_tray);
        assert_eq!(s.mail.preview_lines, 1);
        assert_eq!(s.mail.density, MailDensity::Normal);
        assert_eq!(s.mail.mark_read_mode, MarkReadMode::OnOpen);
        assert_eq!(s.mail.remote_images, RemoteImagesPolicy::Always);
        assert_eq!(
            s.mail.hover_actions,
            vec![HoverAction::Archive, HoverAction::Delete, HoverAction::Flag]
        );
        assert_eq!(s.mail.prefetch_unread_bodies, 50);
        assert_eq!(s.mail.body_cache_limit_mb, 500);
        assert_eq!(s.mail.other_folders_poll_minutes, 15);
        assert_eq!(s.calendar.default_view, CalendarView::Week);
        assert_eq!(s.calendar.first_day_of_week, 1);
        assert_eq!(s.calendar.work_days, vec![1, 2, 3, 4, 5]);
        assert_eq!(s.calendar.work_hours_start, "09:00");
        assert_eq!(s.calendar.work_hours_end, "18:00");
        assert_eq!(s.calendar.default_reminder_minutes, 15);
        assert_eq!(s.calendar.default_event_duration_minutes, 30);
        assert_eq!(s.calendar.poll_minutes, 15);
        assert_eq!(s.contacts.poll_minutes, 60);
        assert_eq!(s.tasks.poll_minutes, 30);
        assert!(s.notifications.enabled);
        assert!(s.privacy.allow_ispdb_lookup);
        assert!(s.updates.auto_check);
    }

    #[test]
    fn deep_merge_merges_objects_replaces_arrays() {
        let base = AppSettings::default();
        let patch: SettingsPatch = serde_json::from_value(serde_json::json!({
            "mail": { "previewLines": 3, "hoverActions": ["delete"] },
            "calendar": { "workDays": [1] }
        }))
        .expect("patch");
        let merged = merge_settings(&base, &patch).expect("merge");
        assert_eq!(merged.mail.preview_lines, 3);
        assert_eq!(merged.mail.hover_actions, vec![HoverAction::Delete]);
        assert_eq!(merged.calendar.work_days, vec![1]);
        // untouched values preserved
        assert_eq!(merged.mail.density, MailDensity::Normal);
        assert_eq!(merged.calendar.work_hours_start, "09:00");
    }

    #[test]
    fn deep_merge_can_clear_nullable_values() {
        let base = AppSettings {
            accent_color: Some("#123456".into()),
            ..AppSettings::default()
        };
        let patch: SettingsPatch =
            serde_json::from_value(serde_json::json!({ "accentColor": null })).expect("patch");
        let merged = merge_settings(&base, &patch).expect("merge");
        assert!(merged.accent_color.is_none());
    }

    #[test]
    fn page_cursor_round_trip() {
        let cursor = PageCursor::message(now(), "abc");
        let encoded = cursor.encode();
        let decoded = PageCursor::decode(&encoded).expect("decode");
        assert_eq!(decoded, cursor);
        assert!(PageCursor::decode("!!!not-base64!!!").is_err());
    }

    #[test]
    fn provider_capabilities_defaults() {
        let gmail = ProviderCapabilities::defaults_for(ProviderKind::Gmail);
        assert!(gmail.gmail_extensions);
        assert!(gmail.auto_saves_sent);
        assert!(gmail.calendar);

        let pop = ProviderCapabilities::defaults_for(ProviderKind::Pop3);
        assert!(!pop.idle);
        assert!(!pop.server_search);

        let local = ProviderCapabilities::defaults_for(ProviderKind::Local);
        assert!(local.calendar && local.contacts && local.tasks);
        assert!(!local.idle);
    }

    #[test]
    fn never_probed_accounts_advertise_no_transport_capabilities() {
        // An account that has never connected (the Outlook account that could
        // not reach outlook.office365.com) must not claim things only a live
        // CAPABILITY response can prove.
        for kind in [
            ProviderKind::Gmail,
            ProviderKind::Outlook,
            ProviderKind::Icloud,
            ProviderKind::Yahoo,
            ProviderKind::Qq,
            ProviderKind::Netease,
            ProviderKind::Imap,
            ProviderKind::Pop3,
            ProviderKind::Local,
        ] {
            let caps = ProviderCapabilities::defaults_for(kind);
            assert!(!caps.idle, "{kind:?} should not pre-advertise IDLE");
            assert!(!caps.condstore, "{kind:?} should not pre-advertise CONDSTORE");
            assert!(!caps.qresync, "{kind:?} should not pre-advertise QRESYNC");
            assert!(!caps.r#move, "{kind:?} should not pre-advertise MOVE");
            assert!(!caps.uidplus, "{kind:?} should not pre-advertise UIDPLUS");
            assert!(
                !caps.special_use,
                "{kind:?} should not pre-advertise SPECIAL-USE"
            );
        }
        // Provider-level facts survive: Outlook keeps its Graph-backed PIM
        // features, Gmail keeps label/sent-folder behaviour.
        let outlook = ProviderCapabilities::defaults_for(ProviderKind::Outlook);
        assert!(outlook.calendar && outlook.contacts && outlook.tasks);
        let gmail = ProviderCapabilities::defaults_for(ProviderKind::Gmail);
        assert!(gmail.gmail_extensions && gmail.auto_saves_sent);
    }

    #[test]
    fn provider_capabilities_field_names() {
        let caps = ProviderCapabilities::defaults_for(ProviderKind::Gmail);
        let value: Value = serde_json::from_str(&json(&caps)).expect("parse");
        // `move`/`uidplus`/`idle`/`condstore` are probed from CAPABILITY and are
        // deliberately false until a session has actually connected, so only
        // provider-level facts are asserted here.
        assert_eq!(value["move"], false);
        assert_eq!(value["idle"], false);
        assert_eq!(value["condstore"], false);
        assert_eq!(value["gmailExtensions"], true);
        assert_eq!(value["autoSavesSent"], true);
        assert_eq!(value["taskRecurrence"], false);
    }

    #[test]
    fn contact_field_type_literal() {
        let field = ContactField {
            r#type: ContactFieldType::Mobile,
            value: "123".into(),
            is_primary: true,
        };
        let value: Value = serde_json::from_str(&json(&field)).expect("parse");
        assert_eq!(value["type"], "mobile");
        assert_eq!(value["isPrimary"], true);
    }

    #[test]
    fn full_settings_round_trip() {
        let settings = AppSettings::default();
        let encoded = serde_json::to_string(&settings).expect("serialize");
        let decoded: AppSettings = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, settings);
    }

    #[test]
    fn rule_field_from_literal() {
        let condition: RuleCondition =
            serde_json::from_str("{\"field\":\"from\",\"op\":\"matchesRegex\",\"value\":\"a\"}")
                .expect("deserialize");
        assert_eq!(condition.field, RuleField::From);
        assert_eq!(json(&condition.field), "\"from\"");
    }

    #[test]
    fn navigate_target_payload_shape() {
        let target = NavigateTarget::mail("m1", Some("a1".into()), Some("f1".into()));
        let value: Value = serde_json::from_str(&json(&target)).expect("parse");
        assert_eq!(value["module"], "mail");
        assert_eq!(value["messageId"], "m1");
        assert_eq!(value["eventId"], Value::Null);
    }

    #[test]
    fn pending_op_retry_backoff_is_bounded() {
        assert_eq!(PendingOp::retry_delay(0).num_seconds(), 5);
        assert_eq!(PendingOp::retry_delay(1).num_seconds(), 10);
        assert_eq!(PendingOp::retry_delay(20).num_seconds(), 600);
    }
}
