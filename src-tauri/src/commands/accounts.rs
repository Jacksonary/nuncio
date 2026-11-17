//! Account, identity, signature and OAuth commands.

use serde::Serialize;
use tauri::State;
use tracing::{debug, info, warn};

use nuncio_core::events;
use nuncio_core::models::{
    now, Account, AccountPatch, AccountState, AuthKind, ConnectionTestResult, DiscoveryResult,
    Identity, NewAccountInput, OAuthProvider, OAuthResult, ProviderCapabilities, ProviderKind,
    Security, ServerConfig, Signature,
};
use nuncio_core::providers::DavService;
use nuncio_core::{repo, AppError};
use nuncio_mail::MailService;

use crate::state::AppState;

/* ───────────────────────────── accounts ───────────────────────────── */

#[tauri::command]
pub async fn account_list(state: State<'_, AppState>) -> nuncio_core::Result<Vec<Account>> {
    state.ctx.db.read(repo::accounts::list).await
}

#[tauri::command]
pub async fn account_discover(
    state: State<'_, AppState>,
    email: String,
) -> nuncio_core::Result<DiscoveryResult> {
    state.auth.discover(email.trim()).await
}

/// Provider facts the account settings page needs on top of the stored row:
/// DAV defaults, who files the Sent copy and the provider help note.
///
/// The enum values are flattened to strings so this stays a plain IPC shape;
/// `None` means the domain is not in the verified directory (never a guess).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfoDto {
    pub id: &'static str,
    pub name: &'static str,
    pub caldav_url: Option<&'static str>,
    pub carddav_url: Option<&'static str>,
    /// "server" | "client" | "configurable".
    pub sent_copy: &'static str,
    pub note: Option<&'static str>,
    pub oauth: Option<&'static str>,
}

#[tauri::command]
pub fn account_provider_info(email: String) -> Option<ProviderInfoDto> {
    let provider = nuncio_core::providers::by_email(email.trim())?;
    let sent_copy = match provider.sent_copy {
        nuncio_core::providers::SentCopy::Server => "server",
        nuncio_core::providers::SentCopy::Client => "client",
        nuncio_core::providers::SentCopy::Configurable => "configurable",
    };
    Some(ProviderInfoDto {
        id: provider.id,
        name: provider.name,
        caldav_url: provider.caldav,
        carddav_url: provider.carddav,
        sent_copy,
        note: provider.note,
        oauth: provider.oauth_provider.map(|oauth| oauth.as_str()),
    })
}

fn add_error(result: &mut ConnectionTestResult, message: String) {
    result.error = Some(match result.error.take() {
        Some(existing) if !existing.is_empty() => format!("{existing}; {message}"),
        _ => message,
    });
}

/// Provider capabilities derived from the persisted account settings.
///
/// Used both when creating an account and when an update adds/removes DAV
/// endpoints, so the calendar/contacts/tasks switches follow the configuration.
fn capabilities_for_parts(
    provider: ProviderKind,
    auth_kind: AuthKind,
    has_caldav: bool,
    has_carddav: bool,
) -> ProviderCapabilities {
    let mut caps = ProviderCapabilities::defaults_for(provider);
    if auth_kind == AuthKind::Password
        && matches!(provider, ProviderKind::Gmail | ProviderKind::Outlook)
    {
        // Without OAuth the Graph/Google APIs are not usable.
        caps.calendar = false;
        caps.contacts = false;
        caps.tasks = false;
        caps.task_importance = false;
        caps.task_reminders = false;
        caps.task_steps = false;
        caps.task_recurrence = false;
    }
    if has_caldav {
        caps.calendar = true;
        // CalDAV VTODO supports importance, reminders, sub-steps and RRULE.
        caps.tasks = true;
        caps.task_importance = true;
        caps.task_reminders = true;
        caps.task_steps = true;
        caps.task_recurrence = true;
    }
    if has_carddav {
        caps.contacts = true;
    }
    caps
}

fn capabilities_for(input: &NewAccountInput) -> ProviderCapabilities {
    capabilities_for_parts(
        input.provider,
        input.auth_kind,
        input
            .caldav_url
            .as_deref()
            .is_some_and(|url| !url.trim().is_empty()),
        input
            .carddav_url
            .as_deref()
            .is_some_and(|url| !url.trim().is_empty()),
    )
}

/// Fill the well-known IMAP/SMTP endpoints for OAuth providers.
///
/// The Gmail/Outlook tiles jump straight from the browser sign-in to the
/// details step, so the wizard sends `imap: null`/`smtp: null`. Servers the
/// user entered by hand always win.
fn apply_oauth_server_presets(input: &mut NewAccountInput) {
    if input.auth_kind != AuthKind::Oauth {
        return;
    }
    let username = input.email.trim().to_string();
    let (imap, smtp) = match input.provider {
        ProviderKind::Gmail => (
            ServerConfig::new("imap.gmail.com", 993, Security::Tls, &username),
            ServerConfig::new("smtp.gmail.com", 465, Security::Tls, &username),
        ),
        ProviderKind::Outlook => (
            ServerConfig::new("outlook.office365.com", 993, Security::Tls, &username),
            ServerConfig::new("smtp.office365.com", 587, Security::Starttls, &username),
        ),
        _ => return,
    };
    if input.imap.is_none() {
        input.imap = Some(imap);
    }
    if input.smtp.is_none() {
        input.smtp = Some(smtp);
    }
}

fn probe_account(input: &NewAccountInput, id: &str) -> Account {
    Account {
        id: id.to_string(),
        provider: input.provider,
        email: input.email.clone(),
        display_name: input.display_name.clone(),
        color: default_color(input.provider, &input.email),
        auth_kind: AuthKind::Password,
        oauth_provider: None,
        imap: input.imap.clone(),
        smtp: input.smtp.clone(),
        pop3: input.pop3.clone(),
        caldav_url: input.caldav_url.clone(),
        carddav_url: input.carddav_url.clone(),
        sent_copy: None,
        sync_window_days: input.sync_window_days,
        mail_enabled: input.mail_enabled,
        calendar_enabled: input.calendar_enabled,
        contacts_enabled: input.contacts_enabled,
        tasks_enabled: input.tasks_enabled,
        notifications_enabled: false,
        include_in_unified: false,
        sort_order: 0,
        state: AccountState::Ok,
        state_message: None,
        capabilities: capabilities_for(input),
        created_at: now(),
    }
}

#[tauri::command]
pub async fn account_test(
    state: State<'_, AppState>,
    mut input: NewAccountInput,
) -> nuncio_core::Result<ConnectionTestResult> {
    apply_oauth_server_presets(&mut input);
    let mut result = state.mail.test_connection(&input).await?;

    if input.auth_kind == AuthKind::Password
        && (input.caldav_url.is_some() || input.carddav_url.is_some())
    {
        let password = input.password.clone().unwrap_or_default();
        if !password.trim().is_empty() {
            let temp_id = format!("__probe__{}", uuid::Uuid::new_v4());
            let account = probe_account(&input, &temp_id);
            state.auth.set_password(&temp_id, &password)?;
            let dav = state.pim.test_dav(&account).await;
            if let Err(error) = state.auth.delete_account_secrets(&temp_id) {
                debug!(%error, "cannot clean up the temporary DAV probe secret");
            }
            match dav {
                Ok(value) => result.dav_ok = Some(value),
                Err(error) => {
                    result.dav_ok = Some(false);
                    add_error(&mut result, format!("DAV: {}", error.message()));
                }
            }
        }
    }
    Ok(result)
}

fn default_color(provider: ProviderKind, email: &str) -> String {
    let fixed = match provider {
        ProviderKind::Gmail => Some("#D13438"),
        ProviderKind::Outlook => Some("#0F6CBD"),
        ProviderKind::Icloud => Some("#0078D4"),
        ProviderKind::Yahoo => Some("#6001D2"),
        ProviderKind::Qq => Some("#12B7F5"),
        ProviderKind::Netease => Some("#D0021B"),
        _ => None,
    };
    if let Some(color) = fixed {
        return color.to_string();
    }
    const PALETTE: [&str; 8] = [
        "#0F6CBD", "#8764B8", "#C239B3", "#D13438", "#CA5010", "#498205", "#038387", "#4F6BED",
    ];
    let hash = email.bytes().fold(0usize, |acc, byte| {
        acc.wrapping_mul(31).wrapping_add(byte as usize)
    });
    PALETTE[hash % PALETTE.len()].to_string()
}

fn validate_new_account(input: &NewAccountInput) -> nuncio_core::Result<()> {
    if input.provider == ProviderKind::Local {
        return Err(AppError::invalid_input(
            "the local account is built in and cannot be created",
        ));
    }
    if input.email.trim().is_empty() {
        return Err(AppError::invalid_input("the account e-mail is required"));
    }
    match input.auth_kind {
        AuthKind::Password => {
            if input
                .password
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
            {
                return Err(AppError::invalid_input(
                    "a password, app password or authorization code is required",
                ));
            }
        }
        AuthKind::Oauth => {
            if input
                .oauth_session_id
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
            {
                return Err(AppError::invalid_input(
                    "the OAuth sign-in session is missing; sign in again",
                ));
            }
            match input.provider {
                ProviderKind::Gmail | ProviderKind::Outlook => {}
                _ => {
                    return Err(AppError::invalid_input(
                        "OAuth is only supported for Google and Microsoft accounts",
                    ));
                }
            }
        }
    }
    if input.mail_enabled {
        if input.provider == ProviderKind::Pop3 {
            if input.pop3.is_none() {
                return Err(AppError::invalid_input(
                    "a POP3 server is required for this account",
                ));
            }
        } else if input.provider.uses_imap()
            && input.imap.is_none()
            && input.auth_kind != AuthKind::Oauth
        {
            // OAuth accounts get their provider preset filled in by the
            // orchestration layer before validation.
            return Err(AppError::invalid_input(
                "an IMAP server is required for this account",
            ));
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn account_create(
    state: State<'_, AppState>,
    mut input: NewAccountInput,
) -> nuncio_core::Result<Account> {
    apply_oauth_server_presets(&mut input);
    validate_new_account(&input)?;
    let ctx = &state.ctx;
    let account_id = nuncio_core::models::new_id();
    let sort_order = ctx.db.read(repo::accounts::next_sort_order).await?;
    let provider = input.provider;
    let oauth_provider = if input.auth_kind == AuthKind::Oauth {
        match provider {
            ProviderKind::Gmail => Some(OAuthProvider::Google),
            ProviderKind::Outlook => Some(OAuthProvider::Microsoft),
            _ => None,
        }
    } else {
        None
    };
    let account = Account {
        id: account_id.clone(),
        provider,
        email: input.email.trim().to_string(),
        display_name: if input.display_name.trim().is_empty() {
            input.email.trim().to_string()
        } else {
            input.display_name.trim().to_string()
        },
        color: default_color(provider, input.email.trim()),
        auth_kind: input.auth_kind,
        oauth_provider,
        imap: input.imap.clone(),
        smtp: input.smtp.clone(),
        pop3: input.pop3.clone(),
        caldav_url: input.caldav_url.clone(),
        carddav_url: input.carddav_url.clone(),
        sent_copy: None,
        sync_window_days: input.sync_window_days,
        mail_enabled: input.mail_enabled,
        calendar_enabled: input.calendar_enabled,
        contacts_enabled: input.contacts_enabled,
        tasks_enabled: input.tasks_enabled,
        notifications_enabled: true,
        include_in_unified: input.mail_enabled,
        sort_order,
        state: AccountState::Ok,
        state_message: None,
        capabilities: capabilities_for(&input),
        created_at: now(),
    };

    ctx.db
        .write({
            let account = account.clone();
            move |conn| repo::accounts::insert(conn, &account)
        })
        .await?;

    if let Err(error) = state.auth.apply_new_account(&account_id, &input) {
        // Roll back the row so a failed credential step does not leave a
        // half-created account behind.
        let id = account_id.clone();
        let _ = ctx
            .db
            .write(move |conn| repo::accounts::delete(conn, &id))
            .await;
        return Err(error);
    }

    state.auth.on_account_added(&account);
    if let Err(error) = state.mail.on_account_added(&account_id).await {
        warn!(account = %account_id, %error, "cannot start the mail engine for the new account");
    }
    state.pim.on_account_added(&account_id).await;
    events::emit_accounts_changed(ctx.events.as_ref(), vec![account_id.clone()]);
    info!(account = %account_id, provider = provider.as_str(), "account created");

    let mail: MailService = state.mail.clone();
    let pim = state.pim.clone();
    let id = account_id.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(error) = mail.sync_now(Some(&id)).await {
            debug!(%error, "initial mail sync failed");
        }
        if let Err(error) = pim.sync_now(Some(&id), None).await {
            debug!(%error, "initial PIM sync failed");
        }
    });

    Ok(account)
}

#[tauri::command]
pub async fn account_update(
    state: State<'_, AppState>,
    id: String,
    patch: AccountPatch,
) -> nuncio_core::Result<Account> {
    let ctx = &state.ctx;
    let account = ctx
        .db
        .write({
            let id = id.clone();
            let patch = patch.clone();
            move |conn| repo::accounts::update_patch(conn, &id, &patch)
        })
        .await?;

    // Re-derive the capability flags: filling in a CalDAV/CardDAV URL after
    // creation turns calendar/contacts/tasks support on (and clearing it turns
    // them off) without forcing the user to recreate the account.
    let capabilities = capabilities_for_parts(
        account.provider,
        account.auth_kind,
        account
            .caldav_url
            .as_deref()
            .is_some_and(|url| !url.trim().is_empty()),
        account
            .carddav_url
            .as_deref()
            .is_some_and(|url| !url.trim().is_empty()),
    );
    let account = if capabilities != account.capabilities {
        ctx.db
            .write({
                let id = id.clone();
                move |conn| {
                    repo::accounts::set_capabilities(conn, &id, &capabilities)?;
                    repo::accounts::get_required(conn, &id)
                }
            })
            .await?
    } else {
        account
    };

    let mail_changed = patch.imap.is_some()
        || patch.smtp.is_some()
        || patch.pop3.is_some()
        || patch.sync_window_days.is_some()
        || patch.mail_enabled.is_some();
    let pim_changed = patch.caldav_url.is_some()
        || patch.carddav_url.is_some()
        || patch.calendar_enabled.is_some()
        || patch.contacts_enabled.is_some()
        || patch.tasks_enabled.is_some();
    state.auth.on_account_changed(&account);
    if mail_changed {
        if let Err(error) = state.mail.on_account_changed(&id).await {
            warn!(account = %id, %error, "cannot restart the mail engine");
        }
    }
    if pim_changed {
        state.pim.on_account_changed(&id).await;
    }
    events::emit_accounts_changed(ctx.events.as_ref(), vec![id]);
    Ok(account)
}

#[tauri::command]
pub async fn account_update_password(
    state: State<'_, AppState>,
    id: String,
    password: String,
) -> nuncio_core::Result<()> {
    let ctx = &state.ctx;
    let account = ctx
        .db
        .read({
            let id = id.clone();
            move |conn| repo::accounts::get_required(conn, &id)
        })
        .await?;
    if account.auth_kind == AuthKind::Oauth {
        return Err(AppError::unsupported(
            "this account signs in with OAuth; use re-authentication instead",
        ));
    }
    state.auth.set_password(&id, password.trim())?;
    ctx.db
        .write({
            let id = id.clone();
            move |conn| repo::accounts::update_state(conn, &id, AccountState::Ok, None)
        })
        .await?;
    state.auth.on_account_changed(&account);
    ctx.sync_status.remove_account(&id);
    if let Err(error) = state.mail.on_account_changed(&id).await {
        warn!(account = %id, %error, "cannot restart the mail engine");
    }
    state.pim.on_account_changed(&id).await;
    events::emit_accounts_changed(ctx.events.as_ref(), vec![id]);
    Ok(())
}

/// Which DAV services have a dedicated password stored.  Never exposes it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DavPasswordStatus {
    pub calendar: bool,
    pub contacts: bool,
}

/// Store or clear the optional per-service CalDAV/CardDAV password.
///
/// `service` is `"calendar"` or `"contacts"`.  An empty or null password
/// clears the secret and returns the account to using its mail password.
#[tauri::command]
pub async fn account_set_dav_password(
    state: State<'_, AppState>,
    id: String,
    service: String,
    password: Option<String>,
) -> nuncio_core::Result<()> {
    let service = DavService::parse(&service).ok_or_else(|| {
        AppError::invalid_input("service must be either \"calendar\" or \"contacts\"")
    })?;
    let ctx = &state.ctx;
    let account = ctx
        .db
        .read({
            let id = id.clone();
            move |conn| repo::accounts::get_required(conn, &id)
        })
        .await?;
    if account.auth_kind == AuthKind::Oauth {
        return Err(AppError::unsupported(
            "this account signs in with OAuth; DAV uses its access token",
        ));
    }

    state
        .auth
        .set_dav_password(&id, service, password.as_deref())?;
    // A new credential invalidates a stale auth status and must reach the
    // running sync loops.
    ctx.sync_status.remove_account(&id);
    state.pim.on_account_changed(&id).await;
    Ok(())
}

/// Whether a dedicated CalDAV/CardDAV password is stored for this account.
#[tauri::command]
pub async fn account_dav_password_status(
    state: State<'_, AppState>,
    id: String,
) -> nuncio_core::Result<DavPasswordStatus> {
    state
        .ctx
        .db
        .read({
            let id = id.clone();
            move |conn| repo::accounts::get_required(conn, &id)
        })
        .await?;
    Ok(DavPasswordStatus {
        calendar: state.auth.dav_password_stored(&id, DavService::Calendar),
        contacts: state.auth.dav_password_stored(&id, DavService::Contacts),
    })
}

#[tauri::command]
pub async fn account_reauth(
    state: State<'_, AppState>,
    id: String,
) -> nuncio_core::Result<Account> {
    let ctx = &state.ctx;
    let account = ctx
        .db
        .read({
            let id = id.clone();
            move |conn| repo::accounts::get_required(conn, &id)
        })
        .await?;
    if account.auth_kind != AuthKind::Oauth {
        return Err(AppError::unsupported(
            "this account uses a password; update the password instead",
        ));
    }
    let provider = account.oauth_provider.or(match account.provider {
        ProviderKind::Gmail => Some(OAuthProvider::Google),
        ProviderKind::Outlook => Some(OAuthProvider::Microsoft),
        _ => None,
    });
    let Some(provider) = provider else {
        return Err(AppError::unsupported(
            "this account has no OAuth provider configured",
        ));
    };

    let result = state
        .auth
        .oauth_login(provider, Some(account.email.clone()))
        .await?;
    if !result.email.eq_ignore_ascii_case(&account.email) {
        let _ = state.auth.discard_session(&result.session_id);
        return Err(AppError::conflict(format!(
            "signed in as {}, expected {}",
            result.email, account.email
        )));
    }
    state.auth.attach_session(&result.session_id, &id)?;
    ctx.db
        .write({
            let id = id.clone();
            move |conn| repo::accounts::update_state(conn, &id, AccountState::Ok, None)
        })
        .await?;

    let account = ctx
        .db
        .read(move |conn| repo::accounts::get_required(conn, &id))
        .await?;
    state.auth.on_account_changed(&account);
    ctx.sync_status.remove_account(&account.id);
    if let Err(error) = state.mail.on_account_changed(&account.id).await {
        warn!(account = %account.id, %error, "cannot restart the mail engine");
    }
    state.pim.on_account_changed(&account.id).await;
    events::emit_accounts_changed(ctx.events.as_ref(), vec![account.id.clone()]);
    Ok(account)
}

#[tauri::command]
pub async fn account_delete(state: State<'_, AppState>, id: String) -> nuncio_core::Result<()> {
    let ctx = &state.ctx;
    if id == nuncio_core::models::LOCAL_ACCOUNT_ID {
        return Err(AppError::invalid_input(
            "the built-in local account cannot be deleted",
        ));
    }
    // Collect cache directories before the cascade removes the rows.
    let account_id = id.clone();
    let message_ids: Vec<String> = ctx
        .db
        .read(move |conn| {
            let mut statement = conn.prepare("SELECT id FROM message WHERE account_id = ?1")?;
            let rows = statement.query_map([&account_id], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<String>>>()
                .map_err(AppError::from)
        })
        .await
        .unwrap_or_default();
    let account_id = id.clone();
    let draft_ids: Vec<String> = ctx
        .db
        .read(move |conn| {
            let mut statement = conn.prepare("SELECT id FROM draft WHERE account_id = ?1")?;
            let rows = statement.query_map([&account_id], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<String>>>()
                .map_err(AppError::from)
        })
        .await
        .unwrap_or_default();

    if let Err(error) = state.mail.on_account_removed(&id).await {
        debug!(account = %id, %error, "cannot stop the mail engine");
    }
    state.pim.on_account_removed(&id).await;
    state.auth.on_account_removed(&id);
    // Report rather than swallow: if the OS keychain still holds this account's
    // password or refresh token, the user was told the account is gone while the
    // credential survives. The account row is still removed (the user asked for
    // that), but the command surfaces the leftover so it can be retried.
    let secret_error = state
        .auth
        .delete_account_secrets(&id)
        .err()
        .map(|error| error.message());
    ctx.sync_status.remove_account(&id);
    ctx.db
        .write({
            let id = id.clone();
            move |conn| repo::accounts::delete(conn, &id)
        })
        .await?;

    let paths = ctx.paths.clone();
    let mime_dir = paths
        .mime_root()
        .join(nuncio_core::paths::sanitize_component(&id));
    remove_dir_best_effort(&mime_dir).await;
    for message_id in message_ids {
        remove_dir_best_effort(&paths.attachment_dir(&message_id)).await;
    }
    for draft_id in draft_ids {
        remove_dir_best_effort(&paths.draft_staging_dir(&draft_id)).await;
    }

    events::emit_accounts_changed(ctx.events.as_ref(), vec![id.clone()]);
    info!(account = %id, "account deleted");
    match secret_error {
        None => Ok(()),
        Some(detail) => Err(AppError::io(format!(
            "the account was removed, but its stored credentials could not be deleted \
             from the system keychain and may reappear: {detail}"
        ))),
    }
}

async fn remove_dir_best_effort(path: &std::path::Path) {
    if let Err(error) = tokio::fs::remove_dir_all(path).await {
        if error.kind() != std::io::ErrorKind::NotFound {
            debug!(path = %path.display(), %error, "cannot remove account data");
        }
    }
}

#[tauri::command]
pub async fn account_reorder(
    state: State<'_, AppState>,
    ids: Vec<String>,
) -> nuncio_core::Result<()> {
    state
        .ctx
        .db
        .write(move |conn| repo::accounts::reorder(conn, &ids))
        .await?;
    events::emit_accounts_changed(state.ctx.events.as_ref(), Vec::new());
    Ok(())
}

/* ───────────────────────────── OAuth ───────────────────────────── */

#[tauri::command]
pub async fn auth_oauth_login(
    state: State<'_, AppState>,
    provider: OAuthProvider,
    login_hint: Option<String>,
) -> nuncio_core::Result<OAuthResult> {
    state.auth.oauth_login(provider, login_hint).await
}

#[tauri::command]
pub async fn auth_oauth_cancel(state: State<'_, AppState>) -> nuncio_core::Result<()> {
    state.auth.oauth_cancel().await
}

/* ─────────────────── identities & signatures ─────────────────────── */

#[tauri::command]
pub async fn identity_list(
    state: State<'_, AppState>,
    account_id: Option<String>,
) -> nuncio_core::Result<Vec<Identity>> {
    state
        .ctx
        .db
        .read(move |conn| repo::identities::list(conn, account_id.as_deref()))
        .await
}

#[tauri::command]
pub async fn identity_upsert(
    state: State<'_, AppState>,
    mut identity: Identity,
) -> nuncio_core::Result<Identity> {
    let result = state
        .ctx
        .db
        .write(move |conn| repo::identities::upsert(conn, &mut identity))
        .await?;
    events::emit_accounts_changed(state.ctx.events.as_ref(), Vec::new());
    Ok(result)
}

#[tauri::command]
pub async fn identity_delete(state: State<'_, AppState>, id: String) -> nuncio_core::Result<()> {
    state
        .ctx
        .db
        .write(move |conn| repo::identities::delete(conn, &id))
        .await?;
    events::emit_accounts_changed(state.ctx.events.as_ref(), Vec::new());
    Ok(())
}

#[tauri::command]
pub async fn signature_list(state: State<'_, AppState>) -> nuncio_core::Result<Vec<Signature>> {
    state.ctx.db.read(repo::signatures::list).await
}

#[tauri::command]
pub async fn signature_upsert(
    state: State<'_, AppState>,
    mut signature: Signature,
) -> nuncio_core::Result<Signature> {
    // Signatures are injected into outgoing mail HTML; strip scripts, event
    // handlers, forms and javascript:/data: URLs before storing them.
    signature.html = nuncio_mime::sanitize_fragment(&signature.html);
    state
        .ctx
        .db
        .write(move |conn| repo::signatures::upsert(conn, &mut signature))
        .await
}

#[tauri::command]
pub async fn signature_delete(state: State<'_, AppState>, id: String) -> nuncio_core::Result<()> {
    state
        .ctx
        .db
        .write(move |conn| repo::signatures::delete(conn, &id))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oauth_input(provider: ProviderKind) -> NewAccountInput {
        NewAccountInput {
            provider,
            email: "User@Example.com".to_string(),
            display_name: String::new(),
            auth_kind: AuthKind::Oauth,
            password: None,
            oauth_session_id: Some("session".to_string()),
            imap: None,
            smtp: None,
            pop3: None,
            caldav_url: None,
            carddav_url: None,
            sync_window_days: 90,
            mail_enabled: true,
            calendar_enabled: true,
            contacts_enabled: true,
            tasks_enabled: true,
        }
    }

    #[test]
    fn gmail_oauth_gets_server_presets() {
        let mut input = oauth_input(ProviderKind::Gmail);
        apply_oauth_server_presets(&mut input);
        let imap = input.imap.as_ref().expect("gmail imap preset");
        assert_eq!(imap.host, "imap.gmail.com");
        assert_eq!(imap.port, 993);
        assert_eq!(imap.security, Security::Tls);
        assert_eq!(imap.username, "User@Example.com");
        let smtp = input.smtp.as_ref().expect("gmail smtp preset");
        assert_eq!(smtp.host, "smtp.gmail.com");
        assert_eq!(smtp.port, 465);
        assert!(validate_new_account(&input).is_ok());
    }

    #[test]
    fn outlook_oauth_gets_starttls_smtp() {
        let mut input = oauth_input(ProviderKind::Outlook);
        apply_oauth_server_presets(&mut input);
        let smtp = input.smtp.as_ref().expect("outlook smtp preset");
        assert_eq!(smtp.host, "smtp.office365.com");
        assert_eq!(smtp.port, 587);
        assert_eq!(smtp.security, Security::Starttls);
        assert_eq!(
            input.imap.as_ref().expect("outlook imap preset").host,
            "outlook.office365.com"
        );
    }

    #[test]
    fn manual_servers_are_not_overwritten() {
        let mut input = oauth_input(ProviderKind::Gmail);
        let custom = ServerConfig::new("mail.example.com", 993, Security::Tls, "user");
        input.imap = Some(custom.clone());
        apply_oauth_server_presets(&mut input);
        assert_eq!(input.imap, Some(custom));
        assert_eq!(
            input.smtp.expect("smtp preset still filled").host,
            "smtp.gmail.com"
        );
    }

    #[test]
    fn oauth_without_presets_is_still_validated() {
        let mut input = oauth_input(ProviderKind::Gmail);
        input.oauth_session_id = None;
        assert!(validate_new_account(&input).is_err());
    }

    #[test]
    fn dav_urls_enable_pim_capabilities() {
        let mut input = oauth_input(ProviderKind::Imap);
        input.auth_kind = AuthKind::Password;
        input.password = Some("secret".to_string());
        input.oauth_session_id = None;
        input.imap = Some(ServerConfig::new(
            "imap.example.com",
            993,
            Security::Tls,
            "user",
        ));
        let before = capabilities_for(&input);
        assert!(!before.calendar && !before.contacts && !before.tasks);
        input.caldav_url = Some("https://example.com/caldav".to_string());
        input.carddav_url = Some("https://example.com/carddav".to_string());
        let after = capabilities_for(&input);
        assert!(after.calendar && after.tasks && after.contacts);
        // CalDAV VTODO supports the richer task fields.
        assert!(
            after.task_importance
                && after.task_reminders
                && after.task_steps
                && after.task_recurrence
        );
    }

    #[test]
    fn password_gmail_loses_api_capabilities() {
        let mut input = oauth_input(ProviderKind::Gmail);
        input.auth_kind = AuthKind::Password;
        input.password = Some("app-password".to_string());
        input.oauth_session_id = None;
        let caps = capabilities_for(&input);
        assert!(!caps.calendar && !caps.contacts && !caps.tasks);
    }
}
