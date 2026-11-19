//! Manual synchronisation and sync-status queries.

use tauri::State;
use tracing::debug;

use nuncio_core::models::{SyncModule, SyncStatus};

use crate::state::AppState;

/// Which sync paths a `module` request selects: `(mail, pim)`.
///
/// `module` is authoritative. In particular `Some(Calendar)` must not also
/// sync mail, which is what the old "anything but Mail" PIM condition did.
fn sync_targets(module: Option<SyncModule>) -> (bool, bool) {
    match module {
        None => (true, true),
        Some(SyncModule::Mail) => (true, false),
        Some(SyncModule::Calendar | SyncModule::Contacts | SyncModule::Tasks) => (false, true),
    }
}

#[tauri::command]
pub async fn sync_now(
    state: State<'_, AppState>,
    account_id: Option<String>,
    module: Option<SyncModule>,
) -> nuncio_core::Result<()> {
    let account_ref = account_id.as_deref();
    let (sync_mail, sync_pim) = sync_targets(module);
    let mut first_error = None;

    if sync_mail {
        if let Err(error) = state.mail.sync_now(account_ref).await {
            debug!(%error, "mail sync failed");
            first_error = Some(error);
        }
    }
    if sync_pim {
        if let Err(error) = state.pim.sync_now(account_ref, module).await {
            debug!(%error, "PIM sync failed");
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }

    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[tauri::command]
pub async fn sync_status(state: State<'_, AppState>) -> nuncio_core::Result<Vec<SyncStatus>> {
    Ok(state.ctx.sync_status.all())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_scopes_the_sync_paths() {
        assert_eq!(sync_targets(None), (true, true));
        assert_eq!(sync_targets(Some(SyncModule::Mail)), (true, false));
        for module in [
            SyncModule::Calendar,
            SyncModule::Contacts,
            SyncModule::Tasks,
        ] {
            assert_eq!(
                sync_targets(Some(module)),
                (false, true),
                "{module:?} must sync its PIM module without mail"
            );
        }
    }
}
