//! The one mapping from a failed PIM sync attempt to its module status.
//!
//! This lives beside the three services rather than in `sync_status` because
//! the split is PIM semantics, not storage: a transport failure and a provider
//! refusal are both transient (the next poll retries), while every other error
//! is actionable and must stay visible as an error.

use nuncio_core::models::{SyncModule, SyncStatus};
use nuncio_core::SyncStatusHub;

use crate::error::AppError;

/// Update the status hub for a failed sync of one account+module.
///
/// `Unsupported` means this account has no backend for the module at all, so
/// it settles idle instead of red: the same treatment the services already
/// give it before reaching here. Keeping it in the match makes the
/// classification total and testable in one place.
pub(crate) fn record_failure(
    hub: &SyncStatusHub,
    account_id: &str,
    module: SyncModule,
    error: &AppError,
) -> SyncStatus {
    match error {
        AppError::Auth(_) => hub.needs_reauth(account_id, module),
        AppError::Network(_) | AppError::Service(_) => {
            hub.retrying(account_id, module, error.message())
        }
        AppError::Unsupported(_) => hub.finish(account_id, module),
        _ => hub.fail(account_id, module, error.message()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nuncio_core::models::SyncState;
    use nuncio_core::traits::testing::RecordingEventSink;

    use super::*;

    fn hub() -> SyncStatusHub {
        SyncStatusHub::new(Arc::new(RecordingEventSink::new()))
    }

    #[test]
    fn transport_and_provider_failures_retry() {
        let hub = hub();
        let network = record_failure(
            &hub,
            "a1",
            SyncModule::Calendar,
            &AppError::network("connection reset"),
        );
        assert_eq!(network.state, SyncState::Retrying);
        assert_eq!(network.message.as_deref(), Some("connection reset"));

        let service = record_failure(
            &hub,
            "a2",
            SyncModule::Tasks,
            &AppError::service("Graph rate limited"),
        );
        assert_eq!(service.state, SyncState::Retrying);
        assert_eq!(service.message.as_deref(), Some("Graph rate limited"));
    }

    #[test]
    fn actionable_failures_stay_errors() {
        let hub = hub();
        let status = record_failure(
            &hub,
            "a1",
            SyncModule::Contacts,
            &AppError::protocol("provider returned 418"),
        );
        assert_eq!(status.state, SyncState::Error);
        assert_eq!(status.message.as_deref(), Some("provider returned 418"));
    }

    #[test]
    fn auth_and_unsupported_keep_their_quiet_states() {
        let hub = hub();
        let auth = record_failure(&hub, "a1", SyncModule::Calendar, &AppError::auth("expired"));
        assert_eq!(auth.state, SyncState::NeedsReauth);

        let unsupported = record_failure(
            &hub,
            "a1",
            SyncModule::Tasks,
            &AppError::unsupported("no backend"),
        );
        assert_eq!(unsupported.state, SyncState::Idle);
        assert_eq!(unsupported.message, None);
    }
}
