//! Shared application state managed by Tauri and injected into commands.

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};

use tauri::AppHandle;

use nuncio_auth::AuthService;
use nuncio_core::models::{AppSettings, NavigateTarget};
use nuncio_core::CoreContext;
use nuncio_mail::MailService;
use nuncio_pim::PimService;

use crate::bridges::AppNotifier;
use crate::pending::{PendingOpen, PendingQueue};

/// Everything a command or background task needs.
#[derive(Clone)]
pub struct AppState {
    pub app: AppHandle,
    pub ctx: CoreContext,
    pub auth: AuthService,
    pub mail: MailService,
    pub pim: PimService,
    pub notifier: Arc<AppNotifier>,
    pub pending_open: PendingQueue,
    pub pending_navigate: Arc<Mutex<Option<NavigateTarget>>>,
    /// Set when the user explicitly quits, so `ExitRequested` may proceed.
    pub exiting: Arc<AtomicBool>,
    /// Serializes recreation of the destroyed main window.
    pub main_window_lock: Arc<Mutex<()>>,
    /// Id of the close-to-tray handshake the frontend has acknowledged, `0` when
    /// none. Storing the id rather than a bare flag lets an old destroy task
    /// recognise that the ack belongs to somebody else's handshake.
    pub flush_ack: Arc<AtomicU64>,
    /// Id of the live close-to-tray flush handshake, `0` when none is running.
    ///
    /// A single boolean could not tell two handshakes apart: cancel a close and
    /// start another one inside the 10 s poll window, and the first task would
    /// read the *second* handshake's ack, clear the flag and destroy a window the
    /// second handshake was still saving. Every task now owns exactly one id.
    pub flush_seq: Arc<AtomicU64>,
    /// Source of handshake ids; only ever incremented.
    pub flush_seq_next: Arc<AtomicU64>,
    /// Handshake id whose window teardown has been committed, `0` when none.
    ///
    /// Published while the destroy runs, *outside* `main_window_lock`: the
    /// teardown calls `save_window_state`, which blocks on the UI thread, and
    /// holding the lock there would deadlock against a Reopen that waits for the
    /// same lock on that thread. A re-open that observes this value defers to
    /// the destroy task through `reopen_pending` instead of racing it.
    pub tear_down: Arc<AtomicU64>,
    /// Set when a re-open arrived while a teardown was committed.
    pub reopen_pending: Arc<AtomicBool>,
    /// Bumped from `WindowEvent::Destroyed` for the main window.
    ///
    /// `WebviewWindow::destroy()` only *queues* the destroy, so a task that has
    /// to know whether the window is really gone cannot just call it and return;
    /// and it must not block a worker thread polling either. The event is the
    /// only honest source for that.
    ///
    /// A monotonic counter rather than a `Notify`: a notification that arrives
    /// after its waiter gave up would otherwise stay armed and be consumed by the
    /// *next* handshake, which would report "destroyed" for a window that is
    /// still alive.
    pub window_destroyed_seq: Arc<AtomicU64>,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        app: AppHandle,
        ctx: CoreContext,
        auth: AuthService,
        mail: MailService,
        pim: PimService,
        notifier: Arc<AppNotifier>,
    ) -> Self {
        Self {
            app,
            ctx,
            auth,
            mail,
            pim,
            notifier,
            pending_open: Arc::new(Mutex::new(Vec::<PendingOpen>::new())),
            pending_navigate: Arc::new(Mutex::new(None)),
            exiting: Arc::new(AtomicBool::new(false)),
            main_window_lock: Arc::new(Mutex::new(())),
            flush_ack: Arc::new(AtomicU64::new(0)),
            flush_seq: Arc::new(AtomicU64::new(0)),
            flush_seq_next: Arc::new(AtomicU64::new(1)),
            tear_down: Arc::new(AtomicU64::new(0)),
            reopen_pending: Arc::new(AtomicBool::new(false)),
            window_destroyed_seq: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn settings(&self) -> AppSettings {
        self.ctx.settings.current()
    }

    pub fn is_exiting(&self) -> bool {
        self.exiting.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn mark_exiting(&self) {
        self.exiting
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}
