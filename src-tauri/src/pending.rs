//! Received `mailto:` / `webcal:` / `.eml` / `.ics` / `.vcf` open requests.
//!
//! The OS delivers these through argv, deep links or `RunEvent::Opened` before
//! (or after) the webview is ready, so they are queued until the frontend calls
//! `app_take_pending_open`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tracing::{debug, warn};
use url::Url;

use nuncio_core::events;

use crate::state::AppState;

/// One item waiting for the frontend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingOpen {
    /// `eml` | `ics` | `vcf` | `mailto` | `webcal`
    pub kind: String,
    pub value: String,
}

impl PendingOpen {
    pub fn new(kind: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            value: value.into(),
        }
    }
}

pub type PendingQueue = Arc<Mutex<Vec<PendingOpen>>>;

/// Classify a command-line argument. URLs are handled by the deep-link plugin;
/// only local files are picked up here.
pub fn from_argument(argument: &str) -> Option<PendingOpen> {
    let value = argument.trim();
    if value.is_empty() || value.starts_with('-') {
        return None;
    }
    if value.starts_with("file://") {
        let url = Url::parse(value).ok()?;
        let path = url.to_file_path().ok()?;
        return from_path(&path);
    }
    if value.contains("://") || value.starts_with("mailto:") || value.starts_with("webcal:") {
        return from_url_str(value);
    }
    from_path(Path::new(value))
}

/// Classify a path by extension.
pub fn from_path(path: &Path) -> Option<PendingOpen> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let kind = match extension.as_str() {
        "eml" => "eml",
        "ics" => "ics",
        "vcf" => "vcf",
        _ => return None,
    };
    Some(PendingOpen::new(kind, path.to_string_lossy().into_owned()))
}

/// Classify a URL received from a deep link or argv.
pub fn from_url(url: &Url) -> Option<PendingOpen> {
    match url.scheme().to_ascii_lowercase().as_str() {
        "file" => url.to_file_path().ok().and_then(|path| from_path(&path)),
        "mailto" => Some(PendingOpen::new("mailto", url.as_str())),
        "webcal" | "webcals" => Some(PendingOpen::new("webcal", url.as_str())),
        "nuncio" => nuncio_deep_link(url),
        _ => None,
    }
}

fn from_url_str(value: &str) -> Option<PendingOpen> {
    Url::parse(value).ok().and_then(|url| from_url(&url))
}

/// `nuncio://open?url=mailto:...` (optional convenience scheme for tests and
/// advanced users). Unknown nuncio routes are ignored.
fn nuncio_deep_link(url: &Url) -> Option<PendingOpen> {
    let target = url
        .query_pairs()
        .find_map(|(key, value)| (key == "url").then(|| value.into_owned()))?;
    from_url_str(&target)
}

/// Parse all argv entries, ignoring the executable and flags.
pub fn from_args<I, S>(args: I) -> Vec<PendingOpen>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut items: Vec<PendingOpen> = Vec::new();
    for (index, argument) in args.into_iter().enumerate() {
        if index == 0 {
            continue;
        }
        if let Some(item) = from_argument(argument.as_ref()) {
            if !items.contains(&item) {
                items.push(item);
            }
        }
    }
    items
}

/// Append items to the queue and notify the frontend.
pub fn enqueue(app: &AppHandle, items: Vec<PendingOpen>) {
    if items.is_empty() {
        return;
    }
    if let Some(state) = app.try_state::<AppState>() {
        let mut queue = state
            .pending_open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for item in items {
            if !queue.contains(&item) {
                debug!(kind = %item.kind, "queued an open request");
                queue.push(item);
            }
        }
    } else {
        warn!("open request arrived before application state was ready");
        return;
    }
    let _ = app.emit(events::OPEN_REQUESTED, serde_json::json!({}));
}

/// Queue items, bring the main window up and ask the frontend to drain it.
pub fn open_and_focus(app: &AppHandle, items: Vec<PendingOpen>) {
    if items.is_empty() {
        return;
    }
    enqueue(app, items);
    // Creating a webview from a synchronous event handler deadlocks on
    // Windows; the window manager helper is therefore always scheduled.
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(error) = crate::windows::show_main_window(&handle, None) {
            warn!(%error, "cannot show the main window for an open request");
        }
    });
    let _ = app.emit(events::OPEN_REQUESTED, serde_json::json!({}));
}

/// Drain the queue (used by `app_take_pending_open`).
pub fn take(app: &AppHandle) -> Vec<PendingOpen> {
    match app.try_state::<AppState>() {
        Some(state) => {
            let mut queue = state
                .pending_open
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *queue)
        }
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_paths_and_urls() {
        assert_eq!(
            from_argument("/tmp/mail.eml"),
            Some(PendingOpen::new("eml", "/tmp/mail.eml"))
        );
        assert_eq!(
            from_argument("mailto:someone@example.com?subject=Hi"),
            Some(PendingOpen::new(
                "mailto",
                "mailto:someone@example.com?subject=Hi"
            ))
        );
        assert_eq!(
            from_argument("webcal://example.com/calendar.ics"),
            Some(PendingOpen::new(
                "webcal",
                "webcal://example.com/calendar.ics"
            ))
        );
        assert_eq!(from_argument("--minimized"), None);
        assert_eq!(from_argument("/tmp/photo.png"), None);
    }

    #[test]
    fn skips_executable_argument() {
        let items = from_args(["/Applications/Nuncio", "/tmp/a.vcf"]);
        assert_eq!(items, vec![PendingOpen::new("vcf", "/tmp/a.vcf")]);
    }

    #[test]
    fn parses_nuncio_open_links() {
        let url = Url::parse("nuncio://open?url=mailto%3Aa%40b.c").expect("url");
        assert_eq!(
            from_url(&url),
            Some(PendingOpen::new("mailto", "mailto:a@b.c"))
        );
    }
}
