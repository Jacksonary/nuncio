//! In-memory sync status registry, mirrored to the UI via `sync://status`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::Utc;

use crate::events;
use crate::models::{now, SyncModule, SyncProgress, SyncState, SyncStatus};
use crate::traits::EventSink;

/// Keyed by `(account_id, module)`.
#[derive(Clone)]
pub struct SyncStatusHub {
    statuses: Arc<Mutex<HashMap<(String, SyncModule), SyncStatus>>>,
    events: Arc<dyn EventSink>,
}

impl std::fmt::Debug for SyncStatusHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncStatusHub")
            .field("entries", &self.snapshot().len())
            .finish()
    }
}

impl SyncStatusHub {
    pub fn new(events: Arc<dyn EventSink>) -> Self {
        Self {
            statuses: Arc::new(Mutex::new(HashMap::new())),
            events,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, SyncModule), SyncStatus>> {
        self.statuses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Replace the status for an account+module and emit `sync://status`.
    pub fn set(&self, status: SyncStatus) -> SyncStatus {
        let key = (status.account_id.clone(), status.module);
        self.lock().insert(key, status.clone());
        events::emit_sync_status(self.events.as_ref(), &status);
        status
    }

    pub fn get(&self, account_id: &str, module: SyncModule) -> Option<SyncStatus> {
        self.lock().get(&(account_id.to_string(), module)).cloned()
    }

    pub fn all(&self) -> Vec<SyncStatus> {
        let mut statuses: Vec<SyncStatus> = self.lock().values().cloned().collect();
        statuses.sort_by(|a, b| {
            a.account_id
                .cmp(&b.account_id)
                .then_with(|| a.module.as_str().cmp(b.module.as_str()))
        });
        statuses
    }

    pub fn snapshot(&self) -> Vec<SyncStatus> {
        self.all()
    }

    pub fn for_account(&self, account_id: &str) -> Vec<SyncStatus> {
        self.all()
            .into_iter()
            .filter(|status| status.account_id == account_id)
            .collect()
    }

    pub fn remove_account(&self, account_id: &str) {
        self.lock().retain(|(id, _), _| id != account_id);
    }

    pub fn clear(&self) {
        self.lock().clear();
    }

    /// Mark a module as syncing (with optional initial progress total).
    pub fn begin(&self, account_id: &str, module: SyncModule, total: Option<u64>) -> SyncStatus {
        let previous = self.get(account_id, module);
        let mut status = previous.unwrap_or_else(|| SyncStatus::new(account_id, module));
        status.state = SyncState::Syncing;
        status.message = None;
        status.progress = total.map(|total| SyncProgress { done: 0, total });
        self.set(status)
    }

    /// Update progress while syncing.
    pub fn progress(
        &self,
        account_id: &str,
        module: SyncModule,
        done: u64,
        total: u64,
    ) -> SyncStatus {
        let mut status = self
            .get(account_id, module)
            .unwrap_or_else(|| SyncStatus::new(account_id, module));
        status.state = SyncState::Syncing;
        status.progress = Some(SyncProgress {
            done: done.min(total),
            total,
        });
        self.set(status)
    }

    /// Mark the module idle and record `last_sync_at`.
    pub fn finish(&self, account_id: &str, module: SyncModule) -> SyncStatus {
        let mut status = self
            .get(account_id, module)
            .unwrap_or_else(|| SyncStatus::new(account_id, module));
        status.state = SyncState::Idle;
        status.progress = None;
        status.message = None;
        status.last_sync_at = Some(now());
        self.set(status)
    }

    /// Record a recoverable error (keeps `last_sync_at`).
    pub fn fail(
        &self,
        account_id: &str,
        module: SyncModule,
        message: impl Into<String>,
    ) -> SyncStatus {
        let mut status = self
            .get(account_id, module)
            .unwrap_or_else(|| SyncStatus::new(account_id, module));
        status.state = SyncState::Error;
        status.progress = None;
        status.message = Some(message.into());
        self.set(status)
    }

    pub fn needs_reauth(&self, account_id: &str, module: SyncModule) -> SyncStatus {
        let mut status = self
            .get(account_id, module)
            .unwrap_or_else(|| SyncStatus::new(account_id, module));
        status.state = SyncState::NeedsReauth;
        status.progress = None;
        status.message = None;
        self.set(status)
    }

    pub fn offline(&self, account_id: &str, module: SyncModule) -> SyncStatus {
        let mut status = self
            .get(account_id, module)
            .unwrap_or_else(|| SyncStatus::new(account_id, module));
        status.state = SyncState::Offline;
        status.progress = None;
        self.set(status)
    }

    /// True if any module is currently syncing (drives the tray spinner).
    pub fn is_syncing(&self) -> bool {
        self.lock()
            .values()
            .any(|status| status.state == SyncState::Syncing)
    }
}

/// Convenience timestamp helper used by callers that do not import chrono.
pub fn utc_now() -> chrono::DateTime<Utc> {
    now()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::testing::RecordingEventSink;

    #[test]
    fn begin_progress_finish_emit_events() {
        let sink = Arc::new(RecordingEventSink::new());
        let hub = SyncStatusHub::new(sink.clone());

        let status = hub.begin("a1", SyncModule::Mail, Some(800));
        assert_eq!(status.state, SyncState::Syncing);
        assert_eq!(
            status.progress,
            Some(SyncProgress {
                done: 0,
                total: 800
            })
        );

        hub.progress("a1", SyncModule::Mail, 120, 800);
        let finished = hub.finish("a1", SyncModule::Mail);
        assert_eq!(finished.state, SyncState::Idle);
        assert!(finished.last_sync_at.is_some());
        assert_eq!(
            hub.get("a1", SyncModule::Mail).expect("status").state,
            SyncState::Idle
        );

        let events = sink.events();
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|(event, _)| event == "sync://status"));
        assert_eq!(events[1].1["progress"]["done"], 120);
    }

    #[test]
    fn failures_and_accounts() {
        let sink = Arc::new(RecordingEventSink::new());
        let hub = SyncStatusHub::new(sink);
        hub.fail("a1", SyncModule::Calendar, "boom");
        hub.needs_reauth("a1", SyncModule::Contacts);
        hub.offline("a2", SyncModule::Tasks);
        assert_eq!(hub.all().len(), 3);
        assert_eq!(hub.for_account("a1").len(), 2);
        assert_eq!(
            hub.get("a1", SyncModule::Calendar)
                .expect("status")
                .message
                .as_deref(),
            Some("boom")
        );
        assert!(!hub.is_syncing());
        hub.remove_account("a1");
        assert_eq!(hub.all().len(), 1);
        hub.clear();
        assert!(hub.all().is_empty());
    }

    #[test]
    fn progress_clamps_and_preserves_total() {
        let hub = SyncStatusHub::new(Arc::new(RecordingEventSink::new()));
        hub.begin("a", SyncModule::Mail, Some(10));
        let status = hub.progress("a", SyncModule::Mail, 50, 10);
        assert_eq!(
            status.progress,
            Some(SyncProgress {
                done: 10,
                total: 10
            })
        );
    }
}
