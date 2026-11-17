//! The one rule for choosing a PIM backend for an account.
//!
//! Calendars, contacts and tasks used to carry three slightly different
//! copies of this match, and they disagreed about DAV: calendars guessed
//! `https://<mail-domain>/.well-known/caldav` for any IMAP account while
//! contacts/tasks only accepted an explicit URL.  That divergence is how
//! Yandex (mail on `*.yandex.com`, DAV on `*.yandex.ru`) synced against a host
//! that answers 405 every 15 minutes.  There is now a single resolver so a
//! service can only be reached through a URL we actually know.

use nuncio_core::models::{Account, OAuthProvider, ProviderKind};
use nuncio_core::providers::{self, DavService};

use crate::error::AppError;

/// Which provider backend an account uses for one PIM service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    Google,
    Graph,
    Dav,
    Local,
    Unsupported,
}

/// Resolve the backend for `account` and `service`.
///
/// OAuth accounts are pinned to their API.  A DAV backend is chosen only when
/// [`providers::dav_url`] returns a URL: the account's own value or a verified
/// directory entry, never a host derived from the mail domain.
pub(crate) fn backend_for(account: &Account, service: DavService) -> Backend {
    match account.oauth_provider {
        Some(OAuthProvider::Google) => Backend::Google,
        Some(OAuthProvider::Microsoft) => Backend::Graph,
        None if account.provider == ProviderKind::Local => Backend::Local,
        None if providers::dav_url(account, service).is_some() => Backend::Dav,
        _ => Backend::Unsupported,
    }
}

/// How a DAV service was configured for an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DavConfig {
    /// The user entered the service URL or stored a dedicated password.
    Explicit,
    /// The URL came from the provider directory and the mail password is
    /// reused.  Such a service is best-effort: many providers reject the mail
    /// password on their DAV endpoints, which is not a sign-in problem.
    Implicit,
}

/// Decide whether a DAV service is explicitly configured.
///
/// `has_dav_password` is the result of
/// [`nuncio_core::traits::CredentialProvider::has_dav_password`] for this
/// service.  Either an explicit URL or a dedicated password makes the
/// configuration explicit; the directory URL plus the reused mail password
/// stays implicit.
pub(crate) fn dav_config(
    account: &Account,
    service: DavService,
    has_dav_password: bool,
) -> DavConfig {
    let explicit_url = match service {
        DavService::Calendar => account.caldav_url.as_deref(),
        DavService::Contacts => account.carddav_url.as_deref(),
    }
    .is_some_and(|url| !url.trim().is_empty());

    if explicit_url || has_dav_password {
        DavConfig::Explicit
    } else {
        DavConfig::Implicit
    }
}

/// True when an authentication failure from a DAV service must be swallowed.
///
/// Only a DAV backend can be implicit: Google/Graph accounts always have an
/// explicit OAuth sign-in.  An implicit authentication failure is expected
/// (the mail password is not valid for this DAV endpoint), so no account-wide
/// "sign-in expired" state may be produced.  An explicit failure keeps the
/// normal `needsReauth` handling.
pub(crate) fn is_implicit_dav_auth_failure(
    account: &Account,
    service: DavService,
    has_dav_password: bool,
    error: &AppError,
) -> bool {
    matches!(error, AppError::Auth(_))
        && backend_for(account, service) == Backend::Dav
        && dav_config(account, service, has_dav_password) == DavConfig::Implicit
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::models::{AccountState, AuthKind, ProviderCapabilities};

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
            created_at: nuncio_core::models::now(),
        }
    }

    #[test]
    fn resolver_covers_every_service_with_one_rule() {
        let mut google = account("me@gmail.com");
        google.provider = ProviderKind::Gmail;
        google.auth_kind = AuthKind::Oauth;
        google.oauth_provider = Some(OAuthProvider::Google);
        assert_eq!(backend_for(&google, DavService::Calendar), Backend::Google);
        assert_eq!(backend_for(&google, DavService::Contacts), Backend::Google);

        let mut microsoft = account("me@outlook.com");
        microsoft.provider = ProviderKind::Outlook;
        microsoft.auth_kind = AuthKind::Oauth;
        microsoft.oauth_provider = Some(OAuthProvider::Microsoft);
        assert_eq!(
            backend_for(&microsoft, DavService::Calendar),
            Backend::Graph
        );
        assert_eq!(
            backend_for(&microsoft, DavService::Contacts),
            Backend::Graph
        );

        let mut local = account("");
        local.provider = ProviderKind::Local;
        assert_eq!(backend_for(&local, DavService::Calendar), Backend::Local);
        assert_eq!(backend_for(&local, DavService::Contacts), Backend::Local);

        // Directory default, no stored URL: Yandex mail is *.yandex.com but
        // its DAV servers are *.yandex.ru.
        let yandex = account("me@yandex.com");
        assert_eq!(backend_for(&yandex, DavService::Calendar), Backend::Dav);
        assert_eq!(backend_for(&yandex, DavService::Contacts), Backend::Dav);

        // 163 has CardDAV but no verified CalDAV.
        let netease = account("me@163.com");
        assert_eq!(backend_for(&netease, DavService::Contacts), Backend::Dav);
        assert_eq!(
            backend_for(&netease, DavService::Calendar),
            Backend::Unsupported
        );

        let unknown = account("me@unknown.example");
        assert_eq!(
            backend_for(&unknown, DavService::Calendar),
            Backend::Unsupported
        );
        assert_eq!(
            backend_for(&unknown, DavService::Contacts),
            Backend::Unsupported
        );

        // A user-entered URL is enough even when the domain is unknown.
        let mut custom = account("me@unknown.example");
        custom.caldav_url = Some("https://dav.example.com/cal/".into());
        custom.carddav_url = Some("https://dav.example.com/ab/".into());
        assert_eq!(backend_for(&custom, DavService::Calendar), Backend::Dav);
        assert_eq!(backend_for(&custom, DavService::Contacts), Backend::Dav);
    }

    #[test]
    fn dav_config_is_explicit_for_a_url_or_a_dedicated_password() {
        // Directory default + reused mail password: implicit.
        let yandex = account("me@yandex.com");
        assert_eq!(
            dav_config(&yandex, DavService::Calendar, false),
            DavConfig::Implicit
        );
        assert_eq!(
            dav_config(&yandex, DavService::Contacts, false),
            DavConfig::Implicit
        );

        // A dedicated password alone makes it explicit.
        assert_eq!(
            dav_config(&yandex, DavService::Calendar, true),
            DavConfig::Explicit
        );

        // A user-entered URL alone makes it explicit, even on an unknown domain.
        let mut custom = account("me@unknown.example");
        custom.caldav_url = Some("https://dav.example.com/cal/".into());
        custom.carddav_url = Some("   ".into());
        assert_eq!(
            dav_config(&custom, DavService::Calendar, false),
            DavConfig::Explicit
        );
        // A blank URL is not an explicit choice.
        assert_eq!(
            dav_config(&custom, DavService::Contacts, false),
            DavConfig::Implicit
        );
    }

    #[test]
    fn only_implicit_dav_auth_failures_are_quiet() {
        let yandex = account("me@yandex.com");
        assert!(is_implicit_dav_auth_failure(
            &yandex,
            DavService::Calendar,
            false,
            &AppError::auth("rejected"),
        ));
        // An explicit password or URL keeps the normal needsReauth path.
        assert!(!is_implicit_dav_auth_failure(
            &yandex,
            DavService::Calendar,
            true,
            &AppError::auth("rejected"),
        ));
        let mut custom = account("me@yandex.com");
        custom.carddav_url = Some("https://dav.example.com/ab/".into());
        assert!(!is_implicit_dav_auth_failure(
            &custom,
            DavService::Contacts,
            false,
            &AppError::auth("rejected"),
        ));
        // Network/other errors keep their normal handling.
        assert!(!is_implicit_dav_auth_failure(
            &yandex,
            DavService::Calendar,
            false,
            &AppError::network("offline"),
        ));
        // Unsupported services are handled by the existing Unsupported branch.
        assert!(!is_implicit_dav_auth_failure(
            &account("me@unknown.example"),
            DavService::Calendar,
            false,
            &AppError::auth("rejected"),
        ));
        // OAuth backends never have implicit DAV credentials.
        let mut google = account("me@gmail.com");
        google.provider = ProviderKind::Gmail;
        google.auth_kind = AuthKind::Oauth;
        google.oauth_provider = Some(OAuthProvider::Google);
        assert!(!is_implicit_dav_auth_failure(
            &google,
            DavService::Calendar,
            false,
            &AppError::auth("rejected"),
        ));
    }
}
