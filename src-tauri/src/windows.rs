//! Window management, link handling and process-level run events.

use std::sync::atomic::Ordering;

use tauri::{
    AppHandle, Emitter, Manager, RunEvent, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
    Window, WindowEvent, Wry,
};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tracing::{debug, info, warn};

use nuncio_core::events;
use nuncio_core::models::{Draft, Language, NavigateTarget};
use nuncio_core::traits::Notifier;
use nuncio_core::AppError;

use crate::pending::{self, PendingOpen};
use crate::state::AppState;

/// Window state persisted by the plugin (visibility is intentionally excluded:
/// a cold start must follow the start-minimized setting, not the last close).
pub fn window_state_flags() -> tauri_plugin_window_state::StateFlags {
    use tauri_plugin_window_state::StateFlags;
    let flags = StateFlags::SIZE
        | StateFlags::POSITION
        | StateFlags::MAXIMIZED
        | StateFlags::FULLSCREEN;
    // macOS keeps native decorations (overlay title bar). Windows/Linux use a
    // custom title bar drawn by the frontend, so restoring a persisted
    // "decorations" value would reintroduce the native bar.
    #[cfg(target_os = "macos")]
    {
        flags | StateFlags::DECORATIONS
    }
    #[cfg(not(target_os = "macos"))]
    {
        flags
    }
}

/// The main window configuration from `tauri.conf.json`.
fn main_window_config(app: &AppHandle) -> nuncio_core::Result<tauri::utils::config::WindowConfig> {
    app.config()
        .app
        .windows
        .iter()
        .find(|window| window.label == "main")
        .cloned()
        .ok_or_else(|| AppError::internal("the main window is missing from tauri.conf.json"))
}

fn is_internal_url(app: &AppHandle, url: &Url) -> bool {
    match url.scheme() {
        "tauri" | "about" | "data" => true,
        "http" | "https" => {
            if url.host_str() == Some("tauri.localhost") {
                return true;
            }
            app.config()
                .build
                .dev_url
                .as_ref()
                .is_some_and(|dev| dev.origin() == url.origin())
        }
        _ => false,
    }
}

/// Add the navigation/new-window guards shared by every application window.
fn attach_handlers<'a>(
    builder: WebviewWindowBuilder<'a, Wry, AppHandle<Wry>>,
    app: AppHandle,
) -> WebviewWindowBuilder<'a, Wry, AppHandle<Wry>> {
    let nav_app = app.clone();
    let new_window_app = app.clone();
    let page_app = app;
    builder
        .on_navigation(move |url| {
            let internal = is_internal_url(&nav_app, url);
            tracing::debug!(%url, internal, "navigation requested");
            if internal {
                true
            } else {
                handle_external_url(&nav_app, url.clone());
                false
            }
        })
        .on_new_window(move |url, _features| {
            handle_external_url(&new_window_app, url);
            tauri::webview::NewWindowResponse::Deny
        })
        .on_page_load(move |window, payload| {
            tracing::debug!(
                label = window.label(),
                url = %payload.url(),
                "webview page load"
            );
            if window.label() != "main" {
                return;
            }
            let app = window.app_handle();
            let pending = app.try_state::<AppState>().and_then(|state| {
                state
                    .pending_navigate
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
            });
            if let Some(target) = pending {
                let _ = page_app.emit(events::NAVIGATE, target);
            }
        })
}

/// Create the main window from its configuration (it is not auto-created so
/// the link handlers above can be installed).
pub fn create_main_window(app: &AppHandle) -> nuncio_core::Result<WebviewWindow> {
    let config = main_window_config(app)?;
    let builder = WebviewWindowBuilder::from_config(app, &config)
        .map_err(|error| AppError::internal(format!("cannot build the main window: {error}")))?;
    // macOS keeps the native overlay title bar (traffic lights float over the
    // custom bar). Windows/Linux draw the whole title bar in the frontend.
    #[cfg(not(target_os = "macos"))]
    let builder = builder.decorations(false);
    let builder = attach_handlers(builder, app.clone());
    let window = builder
        .build()
        .map_err(|error| AppError::internal(format!("cannot create the main window: {error}")))?;
    // Undecorated Windows windows lose the native drop shadow; restore it.
    #[cfg(target_os = "windows")]
    if let Err(error) = window.set_shadow(true) {
        debug!(%error, "cannot enable the main window shadow");
    }
    Ok(window)
}

fn refresh_badge(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let mail = state.mail.clone();
    let notifier = state.notifier.clone();
    tauri::async_runtime::spawn(async move {
        match mail.folder_unified_counts().await {
            Ok(counts) => notifier.unread_badge(counts.inbox.max(0) as u32),
            Err(error) => debug!(%error, "cannot refresh the unread badge"),
        }
    });
}

/// Abandon a running close-to-tray handshake, if any.
///
/// Clearing `flush_seq` is what stops the waiting destroy task: it claims its
/// own id back with a compare-exchange at the end, so once the id is gone the
/// task returns without destroying or cancelling anything. Emitting
/// `CLOSE_CANCELLED` unfreezes the composer, which stays locked until this
/// signal arrives.
///
/// The caller must show the window afterwards; this only releases the handshake.
fn cancel_pending_close(app: &AppHandle, state: &AppState) -> bool {
    let cleared = state
        .flush_seq
        .swap(0, std::sync::atomic::Ordering::SeqCst)
        != 0;
    if cleared {
        let _ = app.emit(events::CLOSE_CANCELLED, ());
    }
    cleared
}

/// Bring the main window back (recreating it after close-to-tray destroyed it).
///
/// A pending navigation is only emitted when the window already exists; for a
/// freshly created window it is delivered once the page has loaded.
pub fn show_main_window(
    app: &AppHandle,
    pending_navigate: Option<NavigateTarget>,
) -> nuncio_core::Result<()> {
    let state = app
        .try_state::<AppState>()
        .ok_or_else(|| AppError::internal("application state is not ready"))?;
    let _guard = state
        .main_window_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    show_main_window_locked(app, &state, pending_navigate)
}

/// Body of `show_main_window`; the caller must hold `main_window_lock`.
///
/// Split out so the tray toggle can reuse it without trying to take a lock it
/// already owns (the guard is not re-entrant).
fn show_main_window_locked(
    app: &AppHandle,
    state: &AppState,
    pending_navigate: Option<NavigateTarget>,
) -> nuncio_core::Result<()> {
    if let Some(target) = pending_navigate {
        *state
            .pending_navigate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(target);
    }

    // A teardown is already committed and running outside this lock. Showing the
    // window now would either resurrect one that is about to be destroyed or
    // race the destroy itself, so ask the destroy task to present it again once
    // it is done and return without touching anything.
    if state
        .tear_down
        .load(std::sync::atomic::Ordering::SeqCst)
        != 0
    {
        state
            .reopen_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
        return Ok(());
    }

    // Re-opening during a close handshake cancels that close: bringing the
    // window back and then destroying it a moment later would lose whatever the
    // user is about to type.
    cancel_pending_close(app, state);

    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        let target = state
            .pending_navigate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(target) = target {
            let _ = app.emit(events::NAVIGATE, target);
        }
        return Ok(());
    }

    let window = create_main_window(app)?;
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
    refresh_badge(app);
    Ok(())
}

/// Tray left-click behaviour: focus/present, hide when already focused.
pub fn toggle_main_window(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    // Same lock as `show_main_window` and the destroy task: the toggle decides
    // whether to present or hide, and a decision taken outside it could present
    // a window the destroy task has already committed to removing.
    let guard = state
        .main_window_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if state
        .tear_down
        .load(std::sync::atomic::Ordering::SeqCst)
        != 0
    {
        state
            .reopen_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
        return;
    }

    if let Some(window) = app.get_webview_window("main") {
        // A tray click during a close handshake cancels that close; otherwise it
        // would show the window and be destroyed a moment later.
        cancel_pending_close(app, &state);
        let visible = window.is_visible().unwrap_or(false);
        let focused = window.is_focused().unwrap_or(false);
        if visible && focused {
            let _ = window.hide();
            return;
        }
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }

    // Recreating needs the lock too, so drop it first and let the shared path
    // take it; the guard is not re-entrant.
    drop(guard);
    if let Err(error) = show_main_window(app, None) {
        warn!(%error, "cannot toggle the main window");
    }
}

/// Wait until the main window is really gone, bounded.
///
/// `destroy()` only queues the destroy, so the caller cannot treat its return as
/// completion. `WindowEvent::Destroyed` is the honest signal; registering again
/// (rather than relying on a stored permit) means a notification left over from
/// an earlier window cannot be mistaken for this one.
async fn wait_for_destroy(app: &AppHandle, seen: u64, patience_secs: u64) -> bool {
    let Some(state) = app.try_state::<AppState>() else {
        return app.get_webview_window("main").is_none();
    };
    let counter = state.window_destroyed_seq.clone();
    // The window may already be gone by the time we get here, in which case no
    // further event is coming.
    if app.get_webview_window("main").is_none() {
        return true;
    }
    // One polling loop instead of a channel: the counter cannot carry a stale
    // permit from a previous handshake the way a notification would, and 25 ms
    // is far below the interval at which the platform destroys a webview.
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(patience_secs);
    loop {
        if counter.load(std::sync::atomic::Ordering::SeqCst) != seen
            || app.get_webview_window("main").is_none()
        {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return app.get_webview_window("main").is_none();
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Save the window state and destroy the main webview (close-to-tray).
///
/// Returns whether the window is really gone. `destroy()` only *queues* the
/// destroy on the event loop, so returning right after it would let a re-open
/// find the old window still registered, present it, and then watch the queued
/// destroy take it away. The caller must not publish "teardown done" before this
/// returns `true`.
pub fn destroy_main_window(app: &AppHandle) -> bool {
    use tauri_plugin_window_state::AppHandleExt;
    if let Err(error) = app.save_window_state(window_state_flags()) {
        debug!(%error, "cannot save the window state");
    }
    let Some(window) = app.get_webview_window("main") else {
        return true;
    };
    let _ = window.hide();
    if let Err(error) = window.destroy() {
        warn!(%error, "cannot destroy the main window");
        return app.get_webview_window("main").is_none();
    }
    info!("main window destroy requested, app stays in the tray");
    // `destroy()` only queues; the registration disappears when the event loop
    // gets to it. The caller is an async task, so it must not block a worker
    // waiting for that — the notification comes from `WindowEvent::Destroyed`.
    true
}

/// Open an internal route (`/message/<id>`, `/compose/<id>`, ...) in a new
/// window. The label prefix must stay `msg-*` or `compose-*` for the capability
/// set to apply.
pub fn open_route_window(app: &AppHandle, route: &str, title: &str) -> nuncio_core::Result<()> {
    let route = if route.starts_with('/') {
        route.to_string()
    } else {
        format!("/{route}")
    };
    if route.len() > 2048 || route.contains('\n') || route.contains('\r') {
        return Err(AppError::invalid_input("invalid window route"));
    }
    let prefix = if route.starts_with("/compose") {
        "compose"
    } else {
        "msg"
    };
    let label = format!("{prefix}-{}", uuid::Uuid::new_v4());
    let title = if title.trim().is_empty() {
        "Nuncio"
    } else {
        title
    };

    let config = main_window_config(app)?;
    let effects = config.window_effects.clone();
    let mut builder = WebviewWindowBuilder::new(
        app,
        label.clone(),
        WebviewUrl::App(format!("index.html#{route}").into()),
    )
    .title(title)
    .inner_size(900.0, 700.0)
    .min_inner_size(480.0, 360.0)
    .transparent(true);
    // Same title-bar model as the main window: macOS overlay traffic lights,
    // Windows/Linux undecorated with the frontend title bar.
    #[cfg(target_os = "macos")]
    {
        builder = builder
            .title_bar_style(tauri::TitleBarStyle::Overlay)
            .hidden_title(true);
    }
    #[cfg(not(target_os = "macos"))]
    {
        builder = builder.decorations(false);
    }
    #[cfg(target_os = "windows")]
    {
        builder = builder.shadow(true);
    }
    if let Some(effects) = effects {
        builder = builder.effects(effects);
    }
    let builder = attach_handlers(builder, app.clone());
    builder
        .build()
        .map_err(|error| AppError::internal(format!("cannot open the window: {error}")))?;
    Ok(())
}

/// Open a compose window for a draft; drafts without an id use the new-message
/// route with the account preselected.
pub fn open_compose_window(app: &AppHandle, draft: &Draft) -> nuncio_core::Result<()> {
    let route = match draft.id.as_deref() {
        Some(id) => format!("/compose/{id}"),
        None => format!(
            "/compose/new?accountId={}",
            urlencoding::encode(&draft.account_id)
        ),
    };
    open_route_window(app, &route, "Nuncio")
}

/// Route a URL that must not open inside the application.
///
/// Links inside the sandboxed reader iframe arrive here through
/// `on_new_window` (the document uses `<base target="_blank">`), and foreign
/// top-level navigations arrive through `on_navigation`. Neither may open
/// silently: the user is shown the full URL and has to confirm it. Only web,
/// mail and calendar-subscription schemes are allowed; anything else (for
/// example `file:` or `javascript:`) is rejected and logged.
pub fn handle_external_url(app: &AppHandle, url: Url) {
    let scheme = url.scheme();
    match scheme {
        "http" | "https" | "mailto" | "webcal" | "webcals" => {}
        "file" => {
            debug!(%url, "ignoring a file link from message content");
            return;
        }
        other => {
            debug!(scheme = other, url = %url, "ignoring an unsupported link scheme");
            return;
        }
    }

    let language = app
        .try_state::<AppState>()
        .map(|state| crate::language::resolve(&state.settings()))
        .unwrap_or(Language::EnUs);
    let (title, hint, open_label, cancel_label) = if language == Language::ZhCn {
        (
            "打开外部链接？",
            "邮件内容请求打开下面的链接。请确认地址后再继续：",
            "打开",
            "取消",
        )
    } else {
        (
            "Open external link?",
            "The message wants to open the following link. Check the address before continuing:",
            "Open",
            "Cancel",
        )
    };
    let message = format!("{hint}\n\n{}", url.as_str());
    debug!(%url, "asking the user to confirm an external link");

    let handle = app.clone();
    app.dialog()
        .message(message)
        .title(title)
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            open_label.to_string(),
            cancel_label.to_string(),
        ))
        .show(move |confirmed| {
            if confirmed {
                open_confirmed_url(&handle, url);
            }
        });
}

/// Open a URL the user explicitly confirmed (`tel:` is deliberately not
/// handled: it is not a mail/calendar/web target and would bypass the check).
fn open_confirmed_url(app: &AppHandle, url: Url) {
    use tauri_plugin_opener::OpenerExt;

    match url.scheme() {
        "http" | "https" => {
            if let Err(error) = app.opener().open_url(url.as_str(), None::<String>) {
                warn!(%error, url = %url, "cannot open the external URL");
            }
        }
        "mailto" => {
            let app = app.clone();
            let value = url.to_string();
            tauri::async_runtime::spawn(async move {
                if let Err(error) = open_mailto(&app, &value).await {
                    warn!(%error, "cannot compose from the mailto link");
                    pending::open_and_focus(&app, vec![PendingOpen::new("mailto", value)]);
                }
            });
        }
        "webcal" | "webcals" => {
            pending::open_and_focus(app, vec![PendingOpen::new("webcal", url.to_string())]);
        }
        other => debug!(
            scheme = other,
            "refusing to open an unsupported link scheme"
        ),
    }
}

async fn open_mailto(app: &AppHandle, url: &str) -> nuncio_core::Result<()> {
    let state = app
        .try_state::<AppState>()
        .ok_or_else(|| AppError::internal("application state is not ready"))?;
    let mut draft = state.mail.compose_from_mailto(url).await?;
    if draft.id.is_none() {
        // Persist so the compose window can load the prefilled content.
        draft = state.mail.compose_save(&draft).await?;
    }
    open_compose_window(app, &draft)
}

/// Handle application-wide window events.
pub fn handle_window_event(window: &Window, event: &WindowEvent) {
    // A destroy is what the close-to-tray task waits for; it is handled before
    // the close-request filter below because it is a different event entirely.
    if let WindowEvent::Destroyed = event {
        if window.label() == "main" {
            if let Some(state) = window.app_handle().try_state::<AppState>() {
                state
                    .window_destroyed_seq
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        return;
    }
    if !matches!(event, WindowEvent::CloseRequested { .. }) {
        return;
    }
    if window.label() != "main" {
        return;
    }
    let WindowEvent::CloseRequested { api, .. } = event else {
        return;
    };
    let app = window.app_handle().clone();
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };

    if state.settings().close_to_tray {
        api.prevent_close();
        // Give the frontend a chance to flush unsaved drafts before the webview
        // is destroyed; without this the last seconds of typing in the inline
        // composer are lost.
        //
        // Every attempt gets its own id. A boolean could not tell two attempts
        // apart: cancel a close and start another one inside the 10 s poll
        // window, and the first task would read the *second* attempt's ack,
        // clear the flag and destroy a window the second one was still saving.
        let ordering = std::sync::atomic::Ordering::SeqCst;
        let seq = state.flush_seq_next.fetch_add(1, ordering);
        if state
            .flush_seq
            .compare_exchange(0, seq, ordering, ordering)
            .is_err()
        {
            // A close handshake is already running and owns the teardown; this
            // request just hides the window, exactly as close-to-tray would.
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.hide();
            }
            return;
        }
        // Hide the window *before* flushing. Close-to-tray hides it anyway,
        // and a hidden window cannot take input — which removes the entire
        // class of "user edits between the save and the teardown" races that
        // guarding individual handlers kept missing. The webview stays alive
        // so the save can still run; if it fails, the window is shown again.
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.hide();
        }
        let ack = state.flush_ack.clone();
        ack.store(0, ordering);
        let _ = app.emit(events::FLUSH_DRAFTS, seq);
        let handle = app.clone();
        let live = state.flush_seq.clone();
        let live_tear_down = state.tear_down.clone();
        let reopen_pending = state.reopen_pending.clone();
        let window_lock = state.main_window_lock.clone();
        tauri::async_runtime::spawn(async move {
            let mut acknowledged = false;
            for _ in 0..100 {
                // Only *our* ack counts: a later handshake stores its own id.
                if ack.load(ordering) == seq {
                    acknowledged = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            // Deciding and acting must be one step with respect to re-opening.
            // Without the lock there is a gap between "the id is mine" and
            // "destroy it" in which `show_main_window` finds nothing to cancel
            // (the id is already cleared) and presents a window that is then
            // destroyed underneath the user. `show_main_window` and
            // `toggle_main_window` take the same lock, so either they cancel us
            // first and we return silently, or we destroy and they recreate.
            let committed = {
                let _guard = window_lock
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                // Claim the completion only if nobody replaced us. A tray click
                // or a newer close has already cleared/reassigned the id; that
                // owner is responsible for the window and this task must stay
                // silent, or it would emit `CLOSE_CANCELLED` for a close it no
                // longer controls.
                let still_ours = live.compare_exchange(seq, 0, ordering, ordering).is_ok();
                if !still_ours {
                    return;
                }
                if !acknowledged {
                    None
                } else {
                    // Publish the teardown *before* releasing the lock, so a
                    // re-open arriving from here on defers to this task instead
                    // of presenting a window that is about to be destroyed. The
                    // destroy itself must run outside the lock: it saves the
                    // window state, which blocks on the UI thread, and a Reopen
                    // dispatched on that thread waits for this same lock.
                    let teardown = live_tear_down.load(ordering);
                    live_tear_down.store(seq.max(teardown), ordering);
                    Some(seq)
                }
            };
            if let Some(seq_id) = committed {
                // Read before requesting: a count that differs afterwards means
                // *this* window reported Destroyed, not an earlier one.
                let seen = handle
                    .try_state::<AppState>()
                    .map(|state| state.window_destroyed_seq.load(ordering))
                    .unwrap_or(0);
                // Distinguish "the request was dropped" from "it was accepted and
                // has not finished yet". Both look like a timeout, and the two need
                // opposite handling: restoring the window under a *pending* destroy
                // loses whatever the user types before it lands, while refusing to
                // restore under a *dropped* one leaves an app that can never be
                // shown. Retrying separates them — a destroy that only needed more
                // time disappears on its own, so a window still standing after
                // several confirmed requests means the platform refused it.
                let mut gone = false;
                // The window stays hidden *and the composer stays frozen* for the
                // whole retry loop. Nothing can be typed into it, so a destroy
                // that lands late has nothing to destroy — the entire point of
                // waiting instead of deciding after one attempt.
                for (attempt, patience) in [(1u32, 3u64), (2, 5), (3, 7)] {
                    let seen_now = handle
                        .try_state::<AppState>()
                        .map(|state| state.window_destroyed_seq.load(ordering))
                        .unwrap_or(seen);
                    if !destroy_main_window(&handle) {
                        // Nothing left to destroy.
                        gone = handle.get_webview_window("main").is_none();
                        break;
                    }
                    if wait_for_destroy(&handle, seen_now, patience).await {
                        gone = true;
                        break;
                    }
                    warn!(attempt, "close-to-tray: destroy not confirmed yet");
                }
                if !gone {
                    // Nothing confirmed the destroy, but that is NOT evidence that
                    // the platform refused it. `destroy()` only queues the request;
                    // if the event loop was busy, the queued destroys land *after*
                    // this point. Restoring the window here — as an earlier version
                    // did — hands the user a window that a queued request then
                    // destroys, together with whatever they typed in the meantime.
                    //
                    // So keep the window hidden and the composer frozen, leave
                    // `tear_down` published, and let the `Destroyed` handler finish
                    // the job: it clears the marker and re-presents the window if a
                    // re-open asked for it. The user sees a hidden window, which is
                    // what close-to-tray means, and no input can be lost because
                    // nothing is editable.
                    warn!("close-to-tray: destroy still unconfirmed; keeping the window hidden");
                    return;
                }
                // Teardown really finished: a re-open that arrived meanwhile gets
                // its window back now, and the deferral flag is cleared under the
                // lock so it cannot race a new handshake.
                let reopen = {
                    let _guard = window_lock
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    // Only clear our own marker. Committing clears `flush_seq`,
                    // so a second CloseRequested can claim a new id and publish
                    // its own teardown while this task is still waiting; an
                    // unconditional `store(0)` here would drop that newer
                    // protection and let the next re-open present a window the
                    // newer task is about to destroy.
                    let current = live_tear_down.load(ordering);
                    if current == seq_id {
                        live_tear_down.store(0, ordering);
                    } else if current != 0 {
                        // Somebody else's teardown is still running: it owns the
                        // re-open too, so leave the flag for it to consume.
                        return;
                    }
                    reopen_pending.swap(false, ordering)
                };
                if reopen {
                    // Navigate targets queued by the re-open are stored in
                    // `pending_navigate`; `show_main_window` reads them itself.
                    let _ = show_main_window(&handle, None);
                }
            } else {
                // The draft could not be saved: keep the webview (and the
                // user's text) alive and bring the window back instead of
                // destroying it silently. Tell the composer to unfreeze —
                // it stays locked until this explicit signal arrives.
                warn!("drafts were not flushed; close-to-tray cancelled");
                let _ = handle.emit(events::CLOSE_CANCELLED, ());
                if let Some(window) = handle.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        });
        return;
    }

    // With other windows still open (e.g. a compose window) closing the main
    // window is not a quit request: let it close and keep the process alive
    // until the last window goes away.
    if app.webview_windows().len() > 1 {
        return;
    }

    api.prevent_close();
    if state.exiting.swap(true, Ordering::SeqCst) {
        app.exit(0);
        return;
    }
    let mail = state.mail.clone();
    let pim = state.pim.clone();
    let auth = state.auth.clone();
    info!("shutting down services before exit");
    tauri::async_runtime::spawn(async move {
        mail.shutdown().await;
        pim.shutdown().await;
        auth.shutdown().await;
        app.exit(0);
    });
}

/// Process-level run events (exit interception, macOS open/reopen).
pub fn handle_run_event(app: &AppHandle, event: &RunEvent) {
    match event {
        RunEvent::ExitRequested { api, code, .. } => {
            let Some(state) = app.try_state::<AppState>() else {
                return;
            };
            // `api` is unused on macOS (see below); keep the binding used.
            let _ = &api;
            if code.is_some() || state.is_exiting() {
                return;
            }
            if state.settings().close_to_tray {
                // macOS keeps the process alive when the last window closes;
                // only the platforms whose event loop exits need a veto.
                #[cfg(not(target_os = "macos"))]
                api.prevent_exit();
            } else {
                state.mark_exiting();
            }
        }
        #[cfg(target_os = "macos")]
        RunEvent::Opened { urls } => {
            let files: Vec<PendingOpen> = urls
                .iter()
                .filter_map(|url| {
                    if url.scheme() != "file" {
                        return None;
                    }
                    url.to_file_path()
                        .ok()
                        .and_then(|path| pending::from_path(&path))
                })
                .collect();
            pending::open_and_focus(app, files);
        }
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => {
            if let Err(error) = show_main_window(app, None) {
                warn!(%error, "cannot reopen the main window");
            }
        }
        _ => {}
    }
}

/// Shut the domain services down and terminate the process. Safe to call twice.
pub async fn shutdown_and_exit(app: &AppHandle) {
    use std::sync::atomic::Ordering;
    let Some(state) = app.try_state::<AppState>() else {
        app.exit(0);
        return;
    };
    if state.exiting.swap(true, Ordering::SeqCst) {
        app.exit(0);
        return;
    }
    info!("shutting down Nuncio");
    state.mail.shutdown().await;
    state.pim.shutdown().await;
    state.auth.shutdown().await;
    app.exit(0);
}
