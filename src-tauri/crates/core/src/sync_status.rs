//! In-memory sync status registry, mirrored to the UI via `sync://status`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::Utc;

use crate::events;
use crate::models::{now, SyncModule, SyncProgress, SyncState, SyncStatus};
use crate::traits::EventSink;

#[derive(Default)]
struct Inner {
    statuses: HashMap<(String, SyncModule), SyncStatus>,
    /// Generation of the pass that currently owns the `full_sync` marker for
    /// a key. [`SyncStatusHub::begin`] and
    /// [`SyncStatusHub::mark_full_sync`] allocate fresh values, so cleanup of
    /// an older pass can never clear a marker a newer pass has set.
    generations: HashMap<(String, SyncModule), u64>,
    next_generation: u64,
    /// Sequence stamped on every published status. It is allocated under the
    /// same lock as the state change, so it orders changes even when their
    /// (`EventSink`) delivery is reordered afterwards. Unlike `generations` it
    /// is never reset by [`SyncStatusHub::clear`]: a status published after a
    /// clear must still look newer than every snapshot the UI already saw.
    next_seq: u64,
}

impl Inner {
    fn next_generation(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    fn next_seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }
}

/// Keyed by `(account_id, module)`.
#[derive(Clone)]
pub struct SyncStatusHub {
    inner: Arc<Mutex<Inner>>,
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
            inner: Arc::new(Mutex::new(Inner::default())),
            events,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Replace the status for an account+module and emit `sync://status`.
    pub fn set(&self, mut status: SyncStatus) -> SyncStatus {
        let key = (status.account_id.clone(), status.module);
        {
            let mut inner = self.lock();
            status.seq = inner.next_seq();
            inner.statuses.insert(key, status.clone());
        }
        events::emit_sync_status(self.events.as_ref(), &status);
        status
    }

    pub fn get(&self, account_id: &str, module: SyncModule) -> Option<SyncStatus> {
        self.lock()
            .statuses
            .get(&(account_id.to_string(), module))
            .cloned()
    }

    pub fn all(&self) -> Vec<SyncStatus> {
        let mut statuses: Vec<SyncStatus> = self.lock().statuses.values().cloned().collect();
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
        let mut inner = self.lock();
        inner.statuses.retain(|(id, _), _| id != account_id);
        inner.generations.retain(|(id, _), _| id != account_id);
    }

    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.statuses.clear();
        inner.generations.clear();
        // `next_generation` keeps counting: a cleanup from a task cancelled
        // before the clear must never match a generation allocated after it.
    }

    /// Mark a module as syncing (with optional initial progress total).
    pub fn begin(&self, account_id: &str, module: SyncModule, total: Option<u64>) -> SyncStatus {
        let key = (account_id.to_string(), module);
        let status = {
            let mut inner = self.lock();
            // Invalidate every marker left behind by the previous pass before
            // its owner had a chance to clean up.
            let generation = inner.next_generation();
            inner.generations.insert(key.clone(), generation);
            let seq = inner.next_seq();
            let status = inner
                .statuses
                .entry(key)
                .or_insert_with(|| SyncStatus::new(account_id, module));
            status.state = SyncState::Syncing;
            status.message = None;
            status.progress = total.map(|total| SyncProgress { done: 0, total });
            // A new pass starts here; the caller marks a first full scan
            // active again if this pass is one (see [`Self::mark_full_sync`]).
            status.full_sync = false;
            status.seq = seq;
            status.clone()
        };
        events::emit_sync_status(self.events.as_ref(), &status);
        status
    }

    /// Mark whether this module is importing a folder's first full pass.
    ///
    /// This is deliberately separate from `progress`: a large mailbox may take
    /// several paginated passes and the total is unknown, so the UI only shows
    /// that the work is ongoing. Returns `None` when no status was begun for
    /// this account+module, which keeps direct/test callers side-effect free.
    pub fn set_full_sync(
        &self,
        account_id: &str,
        module: SyncModule,
        full_sync: bool,
    ) -> Option<SyncStatus> {
        let mut status = self.get(account_id, module)?;
        if status.full_sync == full_sync {
            return Some(status);
        }
        status.full_sync = full_sync;
        Some(self.set(status))
    }

    /// Mark a first full pass active and return an RAII [`FullSyncMarker`].
    ///
    /// The marker clears the flag when it is dropped, whichever way the pass
    /// ends: normal return, error, or the task being aborted (a dropped
    /// future). Each call takes ownership of the flag with a fresh
    /// generation, and [`Self::begin`] allocates one too, so an older pass's
    /// cleanup can never clear the flag of a newer pass.
    ///
    /// Returns `None` when no status was begun for this account+module, which
    /// keeps direct/test callers side-effect free.
    pub fn mark_full_sync(&self, account_id: &str, module: SyncModule) -> Option<FullSyncMarker> {
        let key = (account_id.to_string(), module);
        let (status, emit, generation) = {
            let mut inner = self.lock();
            if !inner.statuses.contains_key(&key) {
                return None;
            }
            let generation = inner.next_generation();
            inner.generations.insert(key.clone(), generation);
            let emit = !inner
                .statuses
                .get(&key)
                .expect("status existence checked above")
                .full_sync;
            let seq = emit.then(|| inner.next_seq());
            let status = inner
                .statuses
                .get_mut(&key)
                .expect("status existence checked above");
            status.full_sync = true;
            if let Some(seq) = seq {
                status.seq = seq;
            }
            (status.clone(), emit, generation)
        };
        if emit {
            events::emit_sync_status(self.events.as_ref(), &status);
        }
        Some(FullSyncMarker {
            hub: self.clone(),
            key,
            generation,
        })
    }

    /// Clear the marker only if `generation` still owns it.
    ///
    /// The `seq` is allocated under the lock, before this snapshot leaves it,
    /// so a newer pass that commits in between publishes a higher sequence and
    /// the UI can discard this late, now-stale `false`.
    fn clear_full_sync_if_owned(&self, key: &(String, SyncModule), generation: u64) {
        let status = {
            let mut inner = self.lock();
            if inner.generations.get(key) != Some(&generation) {
                return;
            }
            if !inner
                .statuses
                .get(key)
                .is_some_and(|status| status.full_sync)
            {
                return;
            }
            let seq = inner.next_seq();
            let status = inner
                .statuses
                .get_mut(key)
                .expect("full_sync checked above");
            status.full_sync = false;
            status.seq = seq;
            status.clone()
        };
        events::emit_sync_status(self.events.as_ref(), &status);
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
        status.full_sync = false;
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
        status.full_sync = false;
        status.message = Some(message.into());
        self.set(status)
    }

    pub fn needs_reauth(&self, account_id: &str, module: SyncModule) -> SyncStatus {
        let mut status = self
            .get(account_id, module)
            .unwrap_or_else(|| SyncStatus::new(account_id, module));
        status.state = SyncState::NeedsReauth;
        status.progress = None;
        status.full_sync = false;
        status.message = None;
        self.set(status)
    }

    /// Record a transient failure that a later poll will retry.
    ///
    /// `last_sync_at` is deliberately not touched: a retry is not a successful
    /// sync, so the UI must keep showing the age of the last real one.
    pub fn retrying(
        &self,
        account_id: &str,
        module: SyncModule,
        message: impl Into<String>,
    ) -> SyncStatus {
        let mut status = self
            .get(account_id, module)
            .unwrap_or_else(|| SyncStatus::new(account_id, module));
        status.state = SyncState::Retrying;
        status.progress = None;
        status.full_sync = false;
        status.message = Some(message.into());
        self.set(status)
    }

    /// True if any module is currently syncing (drives the tray spinner).
    pub fn is_syncing(&self) -> bool {
        self.lock()
            .statuses
            .values()
            .any(|status| status.state == SyncState::Syncing)
    }
}

/// RAII ownership of the `full_sync` marker for one pass.
///
/// Dropping it clears the marker only if this pass still owns it; the flag is
/// left alone when a newer pass (or a newer full-sync pass in the same round)
/// has taken over, which is exactly what happens when a cancelled task is
/// dropped after the worker restarted. Because the cleanup lives in `Drop`,
/// it also runs for an aborted future whose `await` never returns.
#[derive(Debug)]
pub struct FullSyncMarker {
    hub: SyncStatusHub,
    key: (String, SyncModule),
    generation: u64,
}

impl FullSyncMarker {
    /// Generation this marker owns (useful in tests).
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

impl Drop for FullSyncMarker {
    fn drop(&mut self) {
        self.hub
            .clear_full_sync_if_owned(&self.key, self.generation);
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
    fn full_sync_marker_is_cleared_by_every_terminal_state() {
        let sink = Arc::new(RecordingEventSink::new());
        let hub = SyncStatusHub::new(sink);

        hub.begin("a1", SyncModule::Mail, None);
        assert!(
            hub.set_full_sync("a1", SyncModule::Mail, true)
                .expect("status")
                .full_sync
        );
        assert!(hub.get("a1", SyncModule::Mail).expect("status").full_sync);

        hub.finish("a1", SyncModule::Mail);
        assert!(!hub.get("a1", SyncModule::Mail).expect("status").full_sync);

        hub.begin("a1", SyncModule::Mail, None);
        hub.set_full_sync("a1", SyncModule::Mail, true);
        hub.fail("a1", SyncModule::Mail, "boom");
        assert!(!hub.get("a1", SyncModule::Mail).expect("status").full_sync);

        hub.begin("a1", SyncModule::Mail, None);
        hub.set_full_sync("a1", SyncModule::Mail, true);
        hub.retrying("a1", SyncModule::Mail, "later");
        assert!(!hub.get("a1", SyncModule::Mail).expect("status").full_sync);

        // A new pass starts from a clean marker even if the previous one did
        // not reach a terminal transition (for example an aborted task).
        hub.begin("a1", SyncModule::Mail, None);
        hub.set_full_sync("a1", SyncModule::Mail, true);
        hub.begin("a1", SyncModule::Mail, None);
        assert!(!hub.get("a1", SyncModule::Mail).expect("status").full_sync);
    }

    #[test]
    fn full_sync_marker_is_a_noop_without_a_begun_status() {
        let hub = SyncStatusHub::new(Arc::new(RecordingEventSink::new()));
        assert!(hub.set_full_sync("a1", SyncModule::Mail, true).is_none());
        assert!(hub.mark_full_sync("a1", SyncModule::Mail).is_none());
        assert!(hub.get("a1", SyncModule::Mail).is_none());
    }

    #[test]
    fn full_sync_marker_drop_clears_only_the_generation_it_owns() {
        let hub = SyncStatusHub::new(Arc::new(RecordingEventSink::new()));

        // ② A pass that returns (or fails, or is dropped) normally clears it.
        hub.begin("a1", SyncModule::Mail, None);
        let marker = hub.mark_full_sync("a1", SyncModule::Mail).expect("marker");
        assert!(hub.get("a1", SyncModule::Mail).expect("status").full_sync);
        drop(marker);
        assert!(
            !hub.get("a1", SyncModule::Mail).expect("status").full_sync,
            "dropping the marker must clear the flag"
        );

        // ③ A stale marker dropped after a newer pass set its own marker must
        // not clear the newer pass's flag.
        hub.begin("a1", SyncModule::Mail, None);
        let stale = hub
            .mark_full_sync("a1", SyncModule::Mail)
            .expect("stale marker");
        hub.begin("a1", SyncModule::Mail, None);
        let current = hub
            .mark_full_sync("a1", SyncModule::Mail)
            .expect("current marker");
        assert!(hub.get("a1", SyncModule::Mail).expect("status").full_sync);
        drop(stale);
        assert!(
            hub.get("a1", SyncModule::Mail).expect("status").full_sync,
            "cleanup from an older pass must not clear the current marker"
        );
        drop(current);
        assert!(!hub.get("a1", SyncModule::Mail).expect("status").full_sync);
    }

    #[test]
    fn a_newer_full_sync_marker_owns_the_flag_until_it_is_dropped() {
        let hub = SyncStatusHub::new(Arc::new(RecordingEventSink::new()));
        hub.begin("a1", SyncModule::Mail, None);
        let first = hub
            .mark_full_sync("a1", SyncModule::Mail)
            .expect("first marker");
        let second = hub
            .mark_full_sync("a1", SyncModule::Mail)
            .expect("second marker");
        assert!(second.generation() > first.generation());
        drop(first);
        assert!(
            hub.get("a1", SyncModule::Mail).expect("status").full_sync,
            "the second marker still owns the flag"
        );
        drop(second);
        assert!(!hub.get("a1", SyncModule::Mail).expect("status").full_sync);
    }

    #[test]
    fn failures_and_accounts() {
        let sink = Arc::new(RecordingEventSink::new());
        let hub = SyncStatusHub::new(sink);
        hub.fail("a1", SyncModule::Calendar, "boom");
        hub.needs_reauth("a1", SyncModule::Contacts);
        hub.retrying("a2", SyncModule::Tasks, "provider rate limited");
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

    #[test]
    fn retrying_clears_progress_without_claiming_a_sync() {
        let hub = SyncStatusHub::new(Arc::new(RecordingEventSink::new()));
        hub.finish("a1", SyncModule::Calendar);
        let last_sync = hub
            .get("a1", SyncModule::Calendar)
            .expect("status")
            .last_sync_at;
        assert!(last_sync.is_some(), "finish records a successful sync");

        hub.begin("a1", SyncModule::Calendar, Some(100));
        let status = hub.retrying("a1", SyncModule::Calendar, "provider rate limited");
        assert_eq!(status.state, SyncState::Retrying);
        assert_eq!(status.progress, None);
        assert_eq!(status.message.as_deref(), Some("provider rate limited"));
        assert_eq!(
            status.last_sync_at, last_sync,
            "a retry must not masquerade as a successful sync"
        );
    }

    #[test]
    fn wire_strings_match_the_frontend_contract() {
        let retrying = serde_json::to_value(SyncState::Retrying).expect("serialize");
        assert_eq!(retrying, "retrying");
        assert_eq!(
            "retrying".parse::<SyncState>().expect("parse"),
            SyncState::Retrying
        );
        assert!(
            serde_json::from_str::<SyncState>("\"offline\"").is_err(),
            "offline is no longer a module sync state"
        );
    }

    #[test]
    fn published_snapshots_get_increasing_sequences() {
        let sink = Arc::new(RecordingEventSink::new());
        let hub = SyncStatusHub::new(sink.clone());

        let begun = hub.begin("a1", SyncModule::Mail, Some(10));
        let progressed = hub.progress("a1", SyncModule::Mail, 5, 10);
        let finished = hub.finish("a1", SyncModule::Mail);
        assert!(begun.seq < progressed.seq);
        assert!(progressed.seq < finished.seq);

        let emitted: Vec<u64> = sink
            .events()
            .iter()
            .map(|(_, payload)| payload["seq"].as_u64().expect("seq"))
            .collect();
        assert_eq!(
            emitted,
            vec![begun.seq, progressed.seq, finished.seq],
            "the emitted sequence must follow the committed state changes"
        );
    }

    /// Regression for the full-sync marker race: the old marker's cleanup
    /// commits `false` under the lock, but its event is delivered only after a
    /// newer pass has published `true`. The sequence stamp is what lets the UI
    /// reject the late snapshot instead of showing a stale `fullSync=false`.
    #[test]
    fn stale_marker_cleanup_is_delivered_with_an_older_sequence() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;

        /// Holds delivery of the first raw `fullSync=false` event after being
        /// armed, which reproduces an IPC reorder after the cleanup unlocked.
        struct GatedSink {
            armed: AtomicBool,
            entered: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
            events: Mutex<Vec<serde_json::Value>>,
        }

        impl EventSink for GatedSink {
            fn emit(&self, event: &str, payload: serde_json::Value) {
                if event == events::SYNC_STATUS
                    && payload["fullSync"] == false
                    && self.armed.swap(false, Ordering::SeqCst)
                {
                    self.entered.send(()).expect("test receiver alive");
                    self.release
                        .lock()
                        .expect("release guard")
                        .recv()
                        .expect("release sender alive");
                }
                self.events.lock().expect("events guard").push(payload);
            }
        }

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let sink = Arc::new(GatedSink {
            armed: AtomicBool::new(false),
            entered: entered_tx,
            release: Mutex::new(release_rx),
            events: Mutex::new(Vec::new()),
        });
        let hub = SyncStatusHub::new(sink.clone());

        hub.begin("acc", SyncModule::Mail, None);
        let old = hub
            .mark_full_sync("acc", SyncModule::Mail)
            .expect("old marker");
        sink.armed.store(true, Ordering::SeqCst);

        let cleanup = std::thread::spawn(move || drop(old));
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("cleanup committed false and entered emit");

        // The old cleanup has committed its `false` but the event is still
        // undelivered; a restarted worker begins the new pass meanwhile.
        hub.begin("acc", SyncModule::Mail, None);
        // Keep the new marker alive across the assertions below.
        let _new = hub
            .mark_full_sync("acc", SyncModule::Mail)
            .expect("new marker");

        release_tx.send(()).expect("release cleanup");
        cleanup.join().expect("cleanup thread");

        let current = hub.get("acc", SyncModule::Mail).expect("status");
        assert!(current.full_sync, "the registry belongs to the new pass");

        let events = sink.events.lock().expect("events guard");
        let last = events.last().expect("events were emitted");
        assert_eq!(
            last["fullSync"], false,
            "raw delivery order can put the stale cleanup last"
        );
        assert!(
            last["seq"].as_u64().expect("seq") < current.seq,
            "the stale cleanup must be older than the current round"
        );

        // The frontend keeps the highest-sequence snapshot per account+module.
        let winner = events
            .iter()
            .filter(|payload| payload["accountId"] == "acc" && payload["module"] == "mail")
            .max_by_key(|payload| payload["seq"].as_u64())
            .expect("snapshots were emitted");
        assert_eq!(
            winner["fullSync"], true,
            "the newest snapshot is the new round"
        );
        assert_eq!(
            winner["seq"].as_u64().expect("seq"),
            current.seq,
            "the UI's winner must match the backend registry"
        );
    }

    /// Regression for `clear` / `remove_account`: a marker invalidated by
    /// either must not change the registry or publish an event afterwards.
    #[test]
    fn clearing_statuses_silences_earlier_markers() {
        for remove_only in [false, true] {
            let sink = Arc::new(RecordingEventSink::new());
            let hub = SyncStatusHub::new(sink.clone());

            hub.begin("acc", SyncModule::Mail, None);
            let old = hub
                .mark_full_sync("acc", SyncModule::Mail)
                .expect("old marker");
            if remove_only {
                hub.remove_account("acc");
            } else {
                hub.clear();
            }

            hub.begin("acc", SyncModule::Mail, None);
            let new = hub
                .mark_full_sync("acc", SyncModule::Mail)
                .expect("new marker");
            sink.take();

            drop(old);
            assert!(
                sink.is_empty(),
                "a marker invalidated by clear/remove must not publish"
            );
            assert!(
                hub.get("acc", SyncModule::Mail).expect("status").full_sync,
                "the old marker must not clear the new pass"
            );

            drop(new);
            assert_eq!(
                sink.len(),
                1,
                "only the marker that still owns the flag publishes its cleanup"
            );
            assert!(!hub.get("acc", SyncModule::Mail).expect("status").full_sync);
        }
    }
}
