//! Nuncio app crate — Tauri wiring, OS integration and IPC commands.
//!
//! See `docs/ARCHITECTURE.md` §1/§5. The app owns no domain logic: it builds the
//! `CoreContext`, starts the auth/mail/PIM services and adapts them to the
//! contract in `src/ipc/api.ts`.

pub mod backup;
pub mod badge;
pub mod bridges;
pub mod build_info;
pub mod commands;
pub mod language;
pub mod logging;
pub mod pending;
pub mod state;
pub mod tray;
pub mod windows;

use std::sync::Arc;

use tauri::{AppHandle, Emitter, Manager, UriSchemeResponder};
use tracing::{info, warn};

use nuncio_auth::AuthService;
use nuncio_core::traits::{EventSink, Notifier};
use nuncio_core::{CoreContext, Db, Paths, SettingsService, SyncStatusHub};
use nuncio_mail::MailService;
use nuncio_pim::PimService;

use crate::bridges::{AppEventSink, AppNotifier, AppUrlOpener, NotificationRegistry};
use crate::state::AppState;

/// Build the services and return the fully initialized state.
async fn initialize(app: &AppHandle) -> nuncio_core::Result<AppState> {
    let data_dir = app
        .path()
        .app_local_data_dir()
        .map_err(|error| nuncio_core::AppError::io(format!("no data directory: {error}")))?;
    let paths = Paths::new(data_dir);
    paths.ensure_dirs()?;
    logging::init(&paths.logs_dir())?;
    nuncio_auth::install_crypto_provider();
    info!(
        version = app.package_info().version.to_string().as_str(),
        data = %paths.root().display(),
        "starting Nuncio"
    );

    let db = Db::open(paths.db_file())?;
    db.migrate_core()?;

    let events: Arc<dyn EventSink> = Arc::new(AppEventSink::new(app.clone()));
    let registry = Arc::new(NotificationRegistry::new());
    let notifier_impl = Arc::new(AppNotifier::new(app.clone(), registry));
    let notifier: Arc<dyn Notifier> = notifier_impl.clone();

    let mut auth = AuthService::new(
        db.clone(),
        paths.clone(),
        events.clone(),
        build_info::oauth_config(),
        Arc::new(AppUrlOpener::new(app.clone())),
    )?;
    let settings = SettingsService::load(db.clone(), events.clone()).await?;
    auth = auth.with_settings(settings.clone());

    let ctx = CoreContext {
        db: db.clone(),
        paths: paths.clone(),
        events: events.clone(),
        creds: Arc::new(auth.clone()),
        notifier,
        settings: settings.clone(),
        sync_status: SyncStatusHub::new(events),
    };

    let mail = MailService::new(ctx.clone());
    let pim = PimService::new(ctx.clone(), Arc::new(mail.clone()))?;

    // Servers that cannot push (Graph, IMAP without IDLE) are polled. Poll at
    // 30 s while the window is focused and fall back to the configured
    // cadence (2 min) otherwise, so a background app does not wake the CPU
    // (or the network) for nothing.
    if let Some(window) = app.get_webview_window("main") {
        window.on_window_event(|event| {
            if let tauri::WindowEvent::Focused(focused) = event {
                nuncio_mail::set_poll_override_secs(if *focused { 30 } else { 0 });
            }
        });
        // The focus event only fires on a change; a window that starts focused
        // would otherwise keep the background cadence until the first blur.
        nuncio_mail::set_poll_override_secs(if window.is_focused().unwrap_or(false) {
            30
        } else {
            0
        });
    }

    Ok(AppState::new(
        app.clone(),
        ctx,
        auth,
        mail,
        pim,
        notifier_impl,
    ))
}

fn register_notification_actions(app: &AppHandle, registry: Arc<NotificationRegistry>) {
    use tauri_plugin_notification::NotificationExt;

    let handle = app.clone();
    let result = app.notification().on_action(move |event| {
        if event.action_id() == "dismiss" {
            return;
        }
        let Some(notification) = event.notification() else {
            return;
        };
        let Some(target) = registry.take(notification.id()) else {
            return;
        };
        if let Err(error) = windows::show_main_window(&handle, Some(target)) {
            warn!(%error, "cannot route a notification click");
        }
    });
    if let Err(error) = result {
        warn!(%error, "cannot register notification actions");
    }
}

fn setup_deep_links(app: &AppHandle) {
    use tauri_plugin_deep_link::DeepLinkExt;

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    if let Err(error) = app.deep_link().register_all() {
        warn!(%error, "cannot register the deep-link schemes");
    }

    let handle = app.clone();
    app.deep_link().on_open_url(move |event| {
        let items: Vec<pending::PendingOpen> =
            event.urls().iter().filter_map(pending::from_url).collect();
        pending::open_and_focus(&handle, items);
    });

    // URLs that launched the app (Windows/Linux argv or macOS Info.plist).
    match app.deep_link().get_current() {
        Ok(Some(urls)) => {
            let items: Vec<pending::PendingOpen> =
                urls.iter().filter_map(pending::from_url).collect();
            pending::enqueue(app, items);
        }
        Ok(None) => {}
        Err(error) => tracing::debug!(%error, "cannot read the initial deep link"),
    }
}

fn handle_second_instance(app: &AppHandle, argv: Vec<String>) {
    let items = pending::from_args(argv);
    if items.is_empty() {
        if let Err(error) = windows::show_main_window(app, None) {
            warn!(%error, "cannot focus the running instance");
        }
    } else {
        pending::open_and_focus(app, items);
    }
}

fn respond(responder: UriSchemeResponder, status: u16, content_type: &str, body: Vec<u8>) {
    use tauri::http::Response;

    let response = Response::builder()
        .status(status)
        .header("Content-Type", content_type)
        .header("Cache-Control", "private, max-age=3600")
        .header("X-Content-Type-Options", "nosniff")
        .body(body)
        .unwrap_or_else(|_| Response::new(Vec::new()));
    responder.respond(response);
}

fn cid_protocol(
    app: &AppHandle,
    request: tauri::http::Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    let path = request.uri().path().to_string();
    let mut parts = path.trim_start_matches('/').splitn(3, '/');
    if parts.next() != Some("cid") {
        respond(responder, 404, "text/plain", b"not found".to_vec());
        return;
    }
    let decode = |value: Option<&str>| {
        value
            .and_then(|value| urlencoding::decode(value).ok())
            .map(|value| value.into_owned())
    };
    let message_id = decode(parts.next());
    let content_id = decode(parts.next());
    let (Some(message_id), Some(content_id)) = (message_id, content_id) else {
        respond(responder, 400, "text/plain", b"bad request".to_vec());
        return;
    };
    let Some(state) = app.try_state::<AppState>() else {
        respond(responder, 503, "text/plain", b"not ready".to_vec());
        return;
    };
    let mail = state.mail.clone();
    tauri::async_runtime::spawn(async move {
        match mail.resolve_cid(&message_id, &content_id).await {
            Some((mime_type, bytes)) => respond(responder, 200, &mime_type, bytes),
            None => respond(responder, 404, "text/plain", b"not found".to_vec()),
        }
    });
}

fn setup(app: &mut tauri::App) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let handle = app.handle().clone();
    let state = tauri::async_runtime::block_on(initialize(&handle))?;
    app.manage(state.clone());

    // Main window (created manually so navigation/new-window guards apply).
    if app.get_webview_window("main").is_none() {
        windows::create_main_window(&handle)?;
    }

    register_notification_actions(&handle, state.notifier.registry());
    tray::setup(&handle)?;
    setup_deep_links(&handle);

    tauri::async_runtime::block_on(async {
        state.auth.start();
        if let Err(error) = state.mail.start().await {
            warn!(%error, "cannot start the mail engine");
        }
        if let Err(error) = state.pim.start().await {
            warn!(%error, "cannot start the calendar/contacts/tasks engine");
        }
    });

    // Theme, autostart, tray labels and the initial window visibility.
    commands::settings::apply_startup(&state);

    // Files passed on the command line (Windows/Linux).
    let items = pending::from_args(std::env::args());
    if !items.is_empty() {
        pending::enqueue(&handle, items);
        let _ = handle.emit(nuncio_core::events::OPEN_REQUESTED, serde_json::json!({}));
    }

    info!("Nuncio is ready");
    Ok(())
}

/// Run the desktop application.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    use tauri_plugin_window_state::Builder as WindowStateBuilder;

    let builder = tauri::Builder::default()
        // Must be the first plugin so a second instance is detected before
        // anything else initializes.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                handle_second_instance(&app, argv);
            });
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_notification::init())
        // Do not append `--minimized` to the autostart command: a login launch
        // should behave like a normal start and show the window.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .plugin(
            WindowStateBuilder::new()
                .with_state_flags(windows::window_state_flags())
                // Secondary windows have random labels and fixed geometry;
                // tracking them would grow the state file forever.
                .with_filter(|label| label == "main")
                .build(),
        )
        .register_asynchronous_uri_scheme_protocol("nuncio", |ctx, request, responder| {
            cid_protocol(ctx.app_handle(), request, responder);
        })
        .setup(setup)
        .on_window_event(|window, event| {
            windows::handle_window_event(window, event);
        })
        .invoke_handler(nuncio_commands!());

    let app = match builder.build(tauri::generate_context!()) {
        Ok(app) => app,
        Err(error) => {
            eprintln!("Nuncio failed to start: {error}");
            return;
        }
    };
    app.run(|app_handle, event| {
        windows::handle_run_event(app_handle, &event);
    });
}
