//! Tray icon: menu, left-click toggle and localized labels.

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};
use tracing::{info, warn};

use nuncio_core::events;
use nuncio_core::models::Language;

use crate::state::AppState;

pub const TRAY_ID: &str = "main-tray";
const ITEM_OPEN: &str = "tray.open";
const ITEM_NEW: &str = "tray.new";
const ITEM_SYNC: &str = "tray.sync";
const ITEM_QUIT: &str = "tray.quit";

struct Labels {
    open: &'static str,
    new_mail: &'static str,
    sync: &'static str,
    quit: &'static str,
    tooltip: &'static str,
}

fn labels(language: Language) -> Labels {
    if language == Language::ZhCn {
        Labels {
            open: "打开 Nuncio",
            new_mail: "新邮件",
            sync: "立即同步",
            quit: "退出",
            tooltip: "Nuncio",
        }
    } else {
        Labels {
            open: "Open Nuncio",
            new_mail: "New Message",
            sync: "Sync Now",
            quit: "Quit",
            tooltip: "Nuncio",
        }
    }
}

fn menu_language(app: &AppHandle) -> Language {
    app.try_state::<AppState>()
        .map(|state| crate::language::resolve(&state.settings()))
        .unwrap_or(Language::EnUs)
}

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let labels = labels(menu_language(app));
    let open = MenuItem::with_id(app, ITEM_OPEN, labels.open, true, None::<&str>)?;
    let new_mail = MenuItem::with_id(app, ITEM_NEW, labels.new_mail, true, None::<&str>)?;
    let sync = MenuItem::with_id(app, ITEM_SYNC, labels.sync, true, None::<&str>)?;
    let quit = MenuItem::with_id(app, ITEM_QUIT, labels.quit, true, None::<&str>)?;
    Menu::with_items(app, &[&open, &new_mail, &sync, &quit])
}

/// Tray icon.
///
/// macOS menu-bar items must be a *template* image: a black-on-transparent
/// glyph that the system tints for light/dark mode and for the selected state.
/// Using the colorful app icon there looks wrong and disappears against a dark
/// menu bar, so the monochrome 16px master is embedded and flagged as a
/// template. Everywhere else the tray keeps the app icon, which is what users
/// expect on Windows and on the Linux desktop panels.
///
/// The bytes are compiled in: the tray is built during setup, before any
/// asset protocol is available, and a missing file would otherwise leave the
/// app with no tray at all.
#[cfg(target_os = "macos")]
fn tray_icon() -> tauri::image::Image<'static> {
    const TEMPLATE: &[u8] = include_bytes!("../icons/tray/template-16@2x.png");
    tauri::image::Image::from_bytes(TEMPLATE)
        .expect("embedded tray template is a valid PNG")
}

#[cfg(not(target_os = "macos"))]
fn tray_icon() -> tauri::image::Image<'static> {
    const COLOR: &[u8] = include_bytes!("../icons/32x32.png");
    tauri::image::Image::from_bytes(COLOR).expect("embedded tray icon is a valid PNG")
}

/// Install the tray icon once (called from setup).
pub fn setup(app: &AppHandle) -> nuncio_core::Result<()> {
    let menu = build_menu(app)
        .map_err(|error| nuncio_core::AppError::internal(format!("tray menu: {error}")))?;
    let labels = labels(menu_language(app));
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .tooltip(labels.tooltip)
        // Left click toggles the main window; the menu is opened on right click.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            let id = event.id().as_ref();
            match id {
                ITEM_OPEN => {
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        if let Err(error) = crate::windows::show_main_window(&app, None) {
                            warn!(%error, "cannot open the main window from the tray");
                        }
                    });
                }
                ITEM_NEW => {
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        new_message(&app).await;
                    });
                }
                ITEM_SYNC => {
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        if let Some(state) = app.try_state::<AppState>() {
                            if let Err(error) = state.mail.sync_now(None).await {
                                warn!(%error, "tray sync (mail) failed");
                            }
                            if let Err(error) = state.pim.sync_now(None, None).await {
                                warn!(%error, "tray sync (PIM) failed");
                            }
                        }
                    });
                }
                ITEM_QUIT => {
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        crate::windows::shutdown_and_exit(&app).await;
                    });
                }
                other => tracing::debug!(id = other, "unknown tray menu item"),
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle().clone();
                tauri::async_runtime::spawn(async move {
                    crate::windows::toggle_main_window(&app);
                });
            }
        });

    builder = builder.icon(tray_icon()).icon_as_template(cfg!(target_os = "macos"));
    builder.build(app).map_err(|error| {
        nuncio_core::AppError::internal(format!("cannot create the tray: {error}"))
    })?;
    info!("tray icon installed");
    Ok(())
}

async fn new_message(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let draft = match state.mail.compose_create(None).await {
        Ok(draft) => draft,
        Err(error) => {
            warn!(%error, "cannot create a draft from the tray");
            if let Err(error) = crate::windows::show_main_window(app, None) {
                warn!(%error, "cannot show the main window");
            }
            return;
        }
    };

    // The shell event bridge listens for `app://compose` and opens the
    // composer window itself. When the main window was destroyed (close to
    // tray frees the WebView) there is no listener, so open the matching
    // route window directly instead.
    if app.get_webview_window("main").is_some() {
        events::emit_compose(state.ctx.events.as_ref(), &draft);
        return;
    }
    if let Err(error) = crate::windows::open_compose_window(app, &draft) {
        warn!(%error, "cannot open a compose window from the tray");
    }
}

/// Rebuild the menu after the UI language changed.
pub fn refresh(app: &AppHandle) {
    let menu = match build_menu(app) {
        Ok(menu) => menu,
        Err(error) => {
            warn!(%error, "cannot rebuild the tray menu");
            return;
        }
    };
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Err(error) = tray.set_menu(Some(menu)) {
            warn!(%error, "cannot update the tray menu");
        }
        let labels = labels(menu_language(app));
        let _ = tray.set_tooltip(Some(labels.tooltip));
    }
}
