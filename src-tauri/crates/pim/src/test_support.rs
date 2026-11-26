//! Test-only rendezvous hooks for the interleaving regression tests.
//!
//! A service write can be paused *immediately before it takes the database
//! write lock*. A test then commits a concurrent change and only afterwards
//! releases the service, so the ordering is deterministic instead of relying
//! on a `sleep` to guess when the service finished its caller-side reads.
//!
//! Gates are keyed by entity id (UUIDs, so unique across tests) and removed
//! when the installing guard drops. This module is compiled only for
//! `cfg(test)`; release builds contain none of it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::Notify;

/// One rendezvous between a service write and its test.
///
/// The service awaits [`WriteGate::hold`] before entering the write
/// transaction; the test awaits the `entered` receiver returned by
/// [`WriteGate::new`] to learn that the service is parked, then calls
/// [`WriteGate::release`].
pub(crate) struct WriteGate {
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Notify,
}

impl WriteGate {
    /// Creates a gate plus the receiver that fires once the service parks.
    pub(crate) fn new() -> (Arc<Self>, tokio::sync::oneshot::Receiver<()>) {
        let (entered, reached) = tokio::sync::oneshot::channel();
        (
            Arc::new(Self {
                entered: Mutex::new(Some(entered)),
                release: Notify::new(),
            }),
            reached,
        )
    }

    async fn hold(&self) {
        let entered = self
            .entered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(entered) = entered {
            let _ = entered.send(());
        }
        // `notify_one` stores a permit when no waiter is registered yet, so a
        // release signalled between the send above and this await is kept.
        self.release.notified().await;
    }

    /// Lets the parked service continue into its write transaction.
    pub(crate) fn release(&self) {
        self.release.notify_one();
    }
}

static GATES: OnceLock<Mutex<HashMap<String, Arc<WriteGate>>>> = OnceLock::new();

fn gates() -> &'static Mutex<HashMap<String, Arc<WriteGate>>> {
    GATES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Removes the gate registration when the test finishes (including unwinding),
/// so a panicking test cannot leave a parked write behind.
pub(crate) struct GateGuard {
    key: String,
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        gates()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.key);
    }
}

/// Registers `gate` for one entity id. Only the next write of that entity is
/// affected.
pub(crate) fn install(key: impl Into<String>, gate: Arc<WriteGate>) -> GateGuard {
    let key = key.into();
    gates()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key.clone(), gate);
    GateGuard { key }
}

/// Awaited by a service right before it takes the write lock. No-op unless a
/// test installed a gate for `key`.
pub(crate) async fn wait_for_write(key: &str) {
    let gate = gates()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(key)
        .cloned();
    if let Some(gate) = gate {
        gate.hold().await;
    }
}
