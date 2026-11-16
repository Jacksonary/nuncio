//! Tauri bridges for the core traits: `EventSink` (Rust -> webview events) and
//! `Notifier` (OS notifications + taskbar/dock badge).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};
use tracing::{debug, warn};

use nuncio_auth::UrlOpener;
use nuncio_core::error::AppError;
use nuncio_core::models::{
    Account, AppModule, AppSettings, Language, MessageSummary, NavigateTarget,
};
use nuncio_core::traits::{EventSink, Notifier};

use crate::state::AppState;

/// Event sink backed by `AppHandle::emit` (all windows receive it).
#[derive(Clone)]
pub struct AppEventSink {
    app: AppHandle,
}

impl AppEventSink {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

impl EventSink for AppEventSink {
    fn emit(&self, event: &str, payload: Value) {
        if let Err(error) = self.app.emit(event, payload) {
            warn!(event, %error, "failed to emit frontend event");
        }
    }
}

/// Remembers which navigation target belongs to a shown notification so a
/// click can route the user to the right message/event/task.
#[derive(Default)]
pub struct NotificationRegistry {
    next_id: AtomicI32,
    entries: Mutex<VecDeque<(i32, NavigateTarget)>>,
}

impl NotificationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, target: NavigateTarget) -> i32 {
        let id = self
            .next_id
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.push_back((id, target));
        // Notifications are short lived; keep the mapping bounded.
        while entries.len() > 256 {
            entries.pop_front();
        }
        id
    }

    pub fn take(&self, id: i32) -> Option<NavigateTarget> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let index = entries.iter().position(|(entry, _)| *entry == id)?;
        entries.remove(index).map(|(_, target)| target)
    }
}

/// OS notification + unread badge implementation.
pub struct AppNotifier {
    app: AppHandle,
    registry: Arc<NotificationRegistry>,
}

impl std::fmt::Debug for AppNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppNotifier").finish_non_exhaustive()
    }
}

impl AppNotifier {
    pub fn new(app: AppHandle, registry: Arc<NotificationRegistry>) -> Self {
        Self { app, registry }
    }

    pub fn registry(&self) -> Arc<NotificationRegistry> {
        self.registry.clone()
    }

    fn settings(&self) -> AppSettings {
        self.app
            .try_state::<AppState>()
            .map(|state| state.settings())
            .unwrap_or_default()
    }

    fn show(&self, id: i32, title: &str, body: &str, settings: &AppSettings) {
        use tauri_plugin_notification::NotificationExt;

        let mut builder = self
            .app
            .notification()
            .builder()
            .id(id)
            .title(title)
            .body(body);
        if settings.notifications.sound {
            builder = builder.sound("default");
        }
        if let Err(error) = builder.show() {
            warn!(%error, "failed to show a system notification");
        }
    }

    fn target_for_mail(account: &Account, items: &[MessageSummary]) -> NavigateTarget {
        match items.first() {
            Some(message) => NavigateTarget::mail(
                message.id.clone(),
                Some(account.id.clone()),
                Some(message.folder_id.clone()),
            ),
            None => NavigateTarget::module(AppModule::Mail),
        }
    }
}

impl Notifier for AppNotifier {
    fn new_mail(&self, account: &Account, items: &[MessageSummary]) {
        if items.is_empty() {
            return;
        }
        let settings = self.settings();
        if !settings.notifications.enabled || !account.notifications_enabled {
            return;
        }
        let language = crate::language::resolve(&settings);
        let account_label = if account.display_name.trim().is_empty() {
            account.email.clone()
        } else {
            account.display_name.clone()
        };
        let (title, body) = if language == Language::ZhCn {
            let title = format!("{account_label} 的新邮件");
            let body = if settings.notifications.show_preview {
                let first = &items[0];
                let sender = first
                    .r#from
                    .as_ref()
                    .map(|address| {
                        address
                            .name
                            .clone()
                            .filter(|name| !name.trim().is_empty())
                            .unwrap_or_else(|| address.email.clone())
                    })
                    .unwrap_or_default();
                let mut body = format!("{sender}: {}", first.subject);
                if items.len() > 1 {
                    body.push_str(&format!("（另有 {} 封）", items.len() - 1));
                }
                body
            } else {
                format!("{} 封未读邮件", items.len())
            };
            (title, body)
        } else {
            let title = format!("New mail in {account_label}");
            let body = if settings.notifications.show_preview {
                let first = &items[0];
                let sender = first
                    .r#from
                    .as_ref()
                    .map(|address| {
                        address
                            .name
                            .clone()
                            .filter(|name| !name.trim().is_empty())
                            .unwrap_or_else(|| address.email.clone())
                    })
                    .unwrap_or_default();
                let mut body = format!("{sender}: {}", first.subject);
                if items.len() > 1 {
                    body.push_str(&format!(" (+{} more)", items.len() - 1));
                }
                body
            } else {
                format!("{} new message(s)", items.len())
            };
            (title, body)
        };
        let target = Self::target_for_mail(account, items);
        let id = self.registry.insert(target);
        debug!(account = %account.id, count = items.len(), "showing new-mail notification");
        self.show(id, &title, &body, &settings);
    }

    fn reminder(&self, title: &str, body: &str, target: NavigateTarget) {
        let settings = self.settings();
        if !settings.notifications.enabled {
            return;
        }
        let enabled = match target.module {
            AppModule::Calendar => settings.notifications.event_reminders,
            AppModule::Todo => settings.notifications.task_reminders,
            _ => true,
        };
        if !enabled {
            return;
        }
        let id = self.registry.insert(target);
        self.show(id, title, body, &settings);
    }

    fn unread_badge(&self, count: u32) {
        let window = self
            .app
            .get_webview_window("main")
            .or_else(|| self.app.webview_windows().into_values().next());
        let Some(window) = window else {
            return;
        };
        let count = count.min(i32::MAX as u32);
        #[cfg(target_os = "macos")]
        {
            let label = (count > 0).then(|| count.to_string());
            if let Err(error) = window.set_badge_label(label) {
                debug!(%error, "dock badge is not available");
            }
        }
        #[cfg(target_os = "windows")]
        {
            let mut drawn = false;
            match crate::badge::overlay_png(count) {
                // `None` means "no badge": passing a fully transparent image
                // instead leaves the overlay slot occupied, and the compositor
                // renders banding artifacts over the taskbar icon.
                None => {
                    if window.set_overlay_icon(None).is_ok() {
                        drawn = true;
                    }
                }
                Some(bytes) => match tauri::image::Image::from_bytes(&bytes) {
                    Ok(image) => {
                        if window.set_overlay_icon(Some(image)).is_ok() {
                            drawn = true;
                        }
                    }
                    Err(error) => debug!(%error, "cannot decode the overlay icon"),
                },
            }
            if !drawn {
                let title = if count > 0 {
                    format!("({count}) Nuncio")
                } else {
                    "Nuncio".to_string()
                };
                let _ = window.set_title(&title);
            }
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let value = (count > 0).then_some(count as i64);
            if let Err(error) = window.set_badge_count(value) {
                debug!(%error, "unread badge is not supported by this desktop");
            }
        }
    }
}

/// Convert stored settings into a notification/tray language.
pub fn notification_language(settings: &AppSettings) -> Language {
    crate::language::resolve(settings)
}

/// Opens URLs with the system browser through `tauri-plugin-opener`.
///
/// `nuncio-auth` owns the OAuth loopback flow but stays Tauri-free, so the app
/// supplies the platform opener here.
pub struct AppUrlOpener {
    app: AppHandle,
}

impl AppUrlOpener {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

impl std::fmt::Debug for AppUrlOpener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppUrlOpener").finish_non_exhaustive()
    }
}

impl UrlOpener for AppUrlOpener {
    fn open(&self, url: &str) -> nuncio_core::Result<()> {
        use tauri_plugin_opener::OpenerExt;

        self.app
            .opener()
            .open_url(url, None::<String>)
            .map_err(|error| AppError::io(format!("cannot open the browser: {error}")))
    }
}
