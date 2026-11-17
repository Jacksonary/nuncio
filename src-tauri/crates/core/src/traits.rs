//! Traits that decouple domain crates from Tauri and the OS.
//!
//! The app crate implements `EventSink` (Tauri `AppHandle::emit`) and `Notifier`
//! (OS notifications + badge); `nuncio-auth` implements `CredentialProvider`;
//! `nuncio-mail` implements `MailSender` (used by `nuncio-pim` for iTIP replies).

use async_trait::async_trait;
use serde_json::Value;

use crate::models::{Account, MessageSummary, NavigateTarget};
use crate::providers::DavService;
use crate::Result;

/// Sink for backend -> frontend events (`src/ipc/api.ts` `EventPayloads`).
pub trait EventSink: Send + Sync {
    fn emit(&self, event: &str, payload: Value);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenResource {
    Mail,
    GoogleApis,
    MicrosoftGraph,
    /// CalDAV / CardDAV basic auth (no OAuth token).
    Dav,
}

impl TokenResource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mail => "mail",
            Self::GoogleApis => "googleApis",
            Self::MicrosoftGraph => "microsoftGraph",
            Self::Dav => "dav",
        }
    }
}

/// Authentication material for IMAP/POP3/SMTP.
#[derive(Clone, PartialEq, Eq)]
pub enum MailAuth {
    Password {
        username: String,
        password: String,
    },
    XOAuth2 {
        username: String,
        access_token: String,
    },
}

// Never print password/token values in logs.
impl std::fmt::Debug for MailAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Password { username, .. } => f
                .debug_struct("MailAuth::Password")
                .field("username", username)
                .field("password", &"<redacted>")
                .finish(),
            Self::XOAuth2 { username, .. } => f
                .debug_struct("MailAuth::XOAuth2")
                .field("username", username)
                .field("access_token", &"<redacted>")
                .finish(),
        }
    }
}

#[async_trait]
pub trait CredentialProvider: Send + Sync {
    /// Credentials for IMAP/POP3/SMTP.
    async fn mail_auth(&self, account: &Account) -> Result<MailAuth>;

    /// OAuth access token for an API resource (Google Calendar/People/Tasks,
    /// Microsoft Graph, ...).
    async fn api_token(&self, account_id: &str, resource: TokenResource) -> Result<String>;

    /// Basic auth user/password for one DAV service.
    ///
    /// Providers may require a separate app password per service (Yandex,
    /// for example), so the service is part of the lookup key.
    async fn dav_auth(&self, account: &Account, service: DavService) -> Result<(String, String)>;

    /// Whether a dedicated, non-empty DAV password is stored for `service`.
    async fn has_dav_password(&self, account_id: &str, service: DavService) -> bool;

    /// The server rejected the credential: refresh or mark `needsReauth`.
    async fn report_auth_failure(&self, account_id: &str, resource: TokenResource);
}

/// OS notifications and unread badge updates.
pub trait Notifier: Send + Sync {
    fn new_mail(&self, account: &Account, items: &[MessageSummary]);
    fn reminder(&self, title: &str, body: &str, target: NavigateTarget);
    fn unread_badge(&self, count: u32);
}

/// Sends a fully built RFC822 message (implemented by `nuncio-mail`).
#[async_trait]
pub trait MailSender: Send + Sync {
    async fn send_raw(&self, account_id: &str, rfc822: Vec<u8>, save_to_sent: bool) -> Result<()>;
}

/// Implementations used by unit tests in other crates (never in production).
pub mod testing {
    use super::*;
    use async_trait::async_trait;
    use serde_json::Value;
    use std::sync::Mutex;

    /// Event sink that discards everything.
    #[derive(Debug, Default, Clone)]
    pub struct NullEventSink;

    impl EventSink for NullEventSink {
        fn emit(&self, _event: &str, _payload: Value) {}
    }

    /// Event sink that records `(event, payload)` pairs for assertions.
    #[derive(Debug, Default)]
    pub struct RecordingEventSink {
        events: Mutex<Vec<(String, Value)>>,
    }

    impl RecordingEventSink {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn events(&self) -> Vec<(String, Value)> {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        pub fn take(&self) -> Vec<(String, Value)> {
            std::mem::take(
                &mut *self
                    .events
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
            )
        }

        pub fn len(&self) -> usize {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len()
        }

        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }

        pub fn last(&self) -> Option<(String, Value)> {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .last()
                .cloned()
        }

        pub fn last_named(&self, name: &str) -> Option<Value> {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .rev()
                .find(|(event, _)| event == name)
                .map(|(_, payload)| payload.clone())
        }

        pub fn clear(&self) {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clear();
        }
    }

    impl EventSink for RecordingEventSink {
        fn emit(&self, event: &str, payload: Value) {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((event.to_string(), payload));
        }
    }

    /// Notifier that does nothing.
    #[derive(Debug, Default, Clone)]
    pub struct NullNotifier;

    impl Notifier for NullNotifier {
        fn new_mail(&self, _account: &Account, _items: &[MessageSummary]) {}
        fn reminder(&self, _title: &str, _body: &str, _target: NavigateTarget) {}
        fn unread_badge(&self, _count: u32) {}
    }

    /// Credential provider that refuses everything (accounts are unusable).
    #[derive(Debug, Default, Clone)]
    pub struct NullCredentialProvider;

    #[async_trait]
    impl CredentialProvider for NullCredentialProvider {
        async fn mail_auth(&self, _account: &Account) -> Result<MailAuth> {
            Err(crate::AppError::unsupported(
                "no credential provider configured",
            ))
        }

        async fn api_token(&self, _account_id: &str, _resource: TokenResource) -> Result<String> {
            Err(crate::AppError::unsupported(
                "no credential provider configured",
            ))
        }

        async fn dav_auth(
            &self,
            _account: &Account,
            _service: DavService,
        ) -> Result<(String, String)> {
            Err(crate::AppError::unsupported(
                "no credential provider configured",
            ))
        }

        async fn has_dav_password(&self, _account_id: &str, _service: DavService) -> bool {
            false
        }

        async fn report_auth_failure(&self, _account_id: &str, _resource: TokenResource) {}
    }

    /// Mail sender that drops messages (tests of `nuncio-pim` iTIP flows).
    #[derive(Debug, Default, Clone)]
    pub struct NoopMailSender;

    #[async_trait]
    impl MailSender for NoopMailSender {
        async fn send_raw(
            &self,
            _account_id: &str,
            _rfc822: Vec<u8>,
            _save_to_sent: bool,
        ) -> Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{NullNotifier, RecordingEventSink};
    use super::*;
    use crate::events;
    use serde_json::json;

    #[test]
    fn recording_sink_captures_events() {
        let sink = RecordingEventSink::new();
        sink.emit("a", json!({"x": 1}));
        sink.emit("b", json!({"y": 2}));
        assert_eq!(sink.len(), 2);
        assert_eq!(sink.last_named("a"), Some(json!({"x": 1})));
        assert_eq!(sink.take().len(), 2);
        assert!(sink.is_empty());
    }

    #[test]
    fn null_notifier_is_usable() {
        let notifier = NullNotifier;
        notifier.unread_badge(3);
    }

    #[test]
    fn event_constants_are_stable() {
        assert_eq!(events::SYNC_STATUS, "sync://status");
        assert_eq!(events::MAIL_CHANGED, "mail://changed");
        assert_eq!(events::NAVIGATE, "app://navigate");
    }

    #[test]
    fn secrets_are_redacted_in_mail_auth_debug() {
        let auth = MailAuth::Password {
            username: "u".into(),
            password: "secret".into(),
        };
        let rendered = format!("{auth:?}");
        assert!(!rendered.contains("secret"));
        assert!(rendered.contains("redacted"));
        assert!(matches!(auth, MailAuth::Password { .. }));
    }

    #[tokio::test]
    async fn null_credential_provider_errors() {
        use crate::models::ProviderKind;
        let provider = testing::NullCredentialProvider;
        let account = Account {
            id: "a".into(),
            provider: ProviderKind::Imap,
            email: "a@b".into(),
            display_name: "a".into(),
            color: "#000000".into(),
            auth_kind: crate::models::AuthKind::Password,
            oauth_provider: None,
            imap: None,
            smtp: None,
            pop3: None,
            caldav_url: None,
            carddav_url: None,
            sent_copy: None,
            sync_window_days: 90,
            mail_enabled: true,
            calendar_enabled: false,
            contacts_enabled: false,
            tasks_enabled: false,
            notifications_enabled: true,
            include_in_unified: true,
            sort_order: 0,
            state: crate::models::AccountState::Ok,
            state_message: None,
            capabilities: Default::default(),
            created_at: crate::models::now(),
        };
        assert!(provider.mail_auth(&account).await.is_err());
    }
}
