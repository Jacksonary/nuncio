//! Manual synchronisation and sync-status queries.

use tauri::State;
use tracing::debug;

use nuncio_core::models::{SyncModule, SyncStatus};

use crate::state::AppState;

#[tauri::command]
pub async fn sync_now(
    state: State<'_, AppState>,
    account_id: Option<String>,
    module: Option<SyncModule>,
) -> nuncio_core::Result<()> {
    let account_ref = account_id.as_deref();
    let mut first_error = None;

    if module.is_none() || module == Some(SyncModule::Mail) {
        if let Err(error) = state.mail.sync_now(account_ref).await {
            debug!(%error, "mail sync failed");
            first_error = Some(error);
        }
    }
    if module != Some(SyncModule::Mail) {
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
