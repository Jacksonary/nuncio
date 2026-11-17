//! Test support: mock servers, static credentials and a recording notifier.

pub mod mock_imap;
pub mod mock_pop3;
pub mod mock_smtp;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nuncio_core::models::MessageSummary;
use nuncio_core::models::{
    Account, AccountState, AuthKind, NavigateTarget, ProviderCapabilities, ProviderKind,
    ServerConfig,
};
use nuncio_core::providers::DavService;
use nuncio_core::traits::{CredentialProvider, MailAuth, Notifier, TokenResource};
use nuncio_core::{CoreContext, Db, Paths, Result};

pub struct StaticCreds {
    pub auth: MailAuth,
    pub failures: Mutex<Vec<String>>,
}

impl StaticCreds {
    pub fn password(username: &str, password: &str) -> Arc<Self> {
        Arc::new(Self {
            auth: MailAuth::Password {
                username: username.to_string(),
                password: password.to_string(),
            },
            failures: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl CredentialProvider for StaticCreds {
    async fn mail_auth(&self, _account: &Account) -> Result<MailAuth> {
        Ok(self.auth.clone())
    }

    async fn api_token(&self, _account_id: &str, _resource: TokenResource) -> Result<String> {
        Ok("token".into())
    }

    async fn dav_auth(&self, _account: &Account, _service: DavService) -> Result<(String, String)> {
        Ok(("user".into(), "pass".into()))
    }

    async fn has_dav_password(&self, _account_id: &str, _service: DavService) -> bool {
        false
    }

    async fn report_auth_failure(&self, account_id: &str, _resource: TokenResource) {
        self.failures
            .lock()
            .expect("failures")
            .push(account_id.into());
    }
}

#[derive(Default)]
pub struct RecordingNotifier {
    pub new_mail: Mutex<Vec<MessageSummary>>,
    pub badges: Mutex<Vec<u32>>,
}

impl Notifier for RecordingNotifier {
    fn new_mail(&self, _account: &Account, items: &[MessageSummary]) {
        self.new_mail
            .lock()
            .expect("notifier")
            .extend(items.iter().cloned());
    }

    fn reminder(&self, _title: &str, _body: &str, _target: NavigateTarget) {}

    fn unread_badge(&self, count: u32) {
        self.badges.lock().expect("badges").push(count);
    }
}

pub fn unique_paths() -> Paths {
    let root = std::env::temp_dir().join(format!(
        "nuncio-mail-test-{}",
        nuncio_core::models::new_id()
    ));
    Paths::new(root)
}

pub async fn context(creds: Arc<StaticCreds>, notifier: Arc<RecordingNotifier>) -> CoreContext {
    context_with_sink(creds, notifier).await.0
}

pub async fn context_with_sink(
    creds: Arc<StaticCreds>,
    notifier: Arc<RecordingNotifier>,
) -> (
    CoreContext,
    Arc<nuncio_core::traits::testing::RecordingEventSink>,
) {
    let db = Db::open_in_memory().expect("in-memory db");
    let sink = Arc::new(nuncio_core::traits::testing::RecordingEventSink::new());
    let ctx = CoreContext::new(db, unique_paths(), sink.clone(), creds, notifier)
        .await
        .expect("core context");
    (ctx, sink)
}

pub fn account(
    id: &str,
    email: &str,
    imap: Option<ServerConfig>,
    smtp: Option<ServerConfig>,
    pop3: Option<ServerConfig>,
) -> Account {
    Account {
        id: id.to_string(),
        provider: if pop3.is_some() {
            ProviderKind::Pop3
        } else {
            ProviderKind::Imap
        },
        email: email.to_string(),
        display_name: "Test".into(),
        color: "#336699".into(),
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        imap,
        smtp,
        pop3,
        caldav_url: None,
        carddav_url: None,
        sent_copy: None,
        sync_window_days: 0,
        mail_enabled: true,
        calendar_enabled: false,
        contacts_enabled: false,
        tasks_enabled: false,
        notifications_enabled: true,
        include_in_unified: true,
        sort_order: 0,
        state: AccountState::Ok,
        state_message: None,
        capabilities: ProviderCapabilities::defaults_for(ProviderKind::Imap),
        created_at: nuncio_core::models::now(),
    }
}

pub async fn insert_account(ctx: &CoreContext, account: &Account) {
    let account = account.clone();
    ctx.db
        .write(move |db| nuncio_core::repo::accounts::insert(db, &account))
        .await
        .expect("insert account");
}

pub fn message_raw(from: &str, to: &str, subject: &str, body: &str, message_id: &str) -> Vec<u8> {
    // Notifications only cover mail from the last 24 hours, so the fixture date
    // has to follow the wall clock instead of being pinned to a fixed day.
    let date = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S +0000");
    format!(
        "From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nDate: {date}\r\nMessage-ID: <{message_id}>\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{body}\r\n"
    )
    .into_bytes()
}
