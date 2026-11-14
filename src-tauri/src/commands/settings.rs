//! Settings commands and their OS side effects.

use tauri::{Manager, State, Theme};
use tracing::{info, warn};

use nuncio_core::models::{AdvancedSettingsPatch, AppSettings, SettingsPatch, ThemeMode};
use nuncio_core::SettingsService;

use crate::state::AppState;

/// Placeholder returned instead of a stored OAuth client secret. Sending it
/// back means "keep the stored secret".
const SECRET_MASK: &str = "********";

#[tauri::command]
pub async fn settings_get(state: State<'_, AppState>) -> nuncio_core::Result<AppSettings> {
    Ok(mask_secrets(&state, state.ctx.settings.get().await))
}

#[tauri::command]
pub async fn settings_update(
    state: State<'_, AppState>,
    mut patch: SettingsPatch,
) -> nuncio_core::Result<AppSettings> {
    // The advanced Google client secret lives in the keyring, never in the
    // settings table. Pull it out before the patch is merged/persisted.
    let secret = patch
        .advanced
        .as_mut()
        .and_then(|advanced| advanced.google_client_secret.take());
    // Force the settings table to hold no plaintext secret, even if an older
    // build left one behind (the keyring is the only store).
    match patch.advanced.as_mut() {
        Some(advanced) => advanced.google_client_secret = Some(None),
        None => {
            patch.advanced = Some(AdvancedSettingsPatch {
                google_client_secret: Some(None),
                ..Default::default()
            })
        }
    }
    SettingsService::validate(&patch)?;
    if let Some(secret) = secret {
        match secret.as_deref().map(str::trim) {
            Some(SECRET_MASK) => {}
            Some(value) if !value.is_empty() => {
                state
                    .auth
                    .set_google_client_secret(Some(value.to_string()))
                    .await?;
            }
            _ => state.auth.set_google_client_secret(None).await?,
        }
    }
    let settings = state.ctx.settings.update(patch).await?;
    apply_side_effects(&state, &settings);
    Ok(mask_secrets(&state, settings))
}

/// Never hand the real client secret to the frontend (nor let it linger in the
/// settings table). A configured secret is reported as [`SECRET_MASK`].
fn mask_secrets(state: &AppState, mut settings: AppSettings) -> AppSettings {
    let stored_in_settings = settings
        .advanced
        .google_client_secret
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    if state.auth.has_google_client_secret() || stored_in_settings {
        settings.advanced.google_client_secret = Some(SECRET_MASK.to_string());
    } else {
        settings.advanced.google_client_secret = None;
    }
    settings
}

/// Apply settings that live outside the database.
pub fn apply_side_effects(state: &AppState, settings: &AppSettings) {
    let app = &state.app;

    let theme = match settings.theme {
        ThemeMode::System => None,
        ThemeMode::Light => Some(Theme::Light),
        ThemeMode::Dark => Some(Theme::Dark),
    };
    app.set_theme(theme);

    // Autostart is best effort: the setting is already persisted, so a failure
    // must not make the command fail.
    {
        use tauri_plugin_autostart::ManagerExt;
        let manager = app.autolaunch();
        let result = if settings.launch_at_startup {
            manager.enable()
        } else {
            manager.disable()
        };
        if let Err(error) = result {
            warn!(%error, "cannot update the autostart entry");
        }
    }

    crate::tray::refresh(app);
    info!(
        theme = ?settings.theme,
        language = settings.language.as_str(),
        "settings applied"
    );
}

/// Called once during startup (after the first settings load).
pub fn apply_startup(state: &AppState) {
    let settings = state.settings();
    apply_side_effects(state, &settings);
    if let Some(window) = state.app.get_webview_window("main") {
        // The window is created visible=false to avoid a flash; show it here
        // unless the user asked to start minimized.
        let minimized =
            settings.start_minimized || std::env::args().any(|argument| argument == "--minimized");
        if !minimized {
            let _ = window.show();
            let _ = window.set_focus();
        }
    }
}
