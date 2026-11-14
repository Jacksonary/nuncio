//! Application context passed to every domain service.

use std::sync::Arc;

use crate::db::Db;
use crate::paths::Paths;
use crate::settings::SettingsService;
use crate::sync_status::SyncStatusHub;
use crate::traits::{CredentialProvider, EventSink, Notifier};
use crate::Result;

/// Shared, cloneable handle to the database, paths, event/notification bridges,
/// settings and sync status registry.
#[derive(Clone)]
pub struct CoreContext {
    pub db: Db,
    pub paths: Paths,
    pub events: Arc<dyn EventSink>,
    pub creds: Arc<dyn CredentialProvider>,
    pub notifier: Arc<dyn Notifier>,
    pub settings: SettingsService,
    pub sync_status: SyncStatusHub,
}

impl std::fmt::Debug for CoreContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreContext")
            .field("db", &self.db)
            .field("paths", &self.paths)
            .finish_non_exhaustive()
    }
}

impl CoreContext {
    /// Open the application database, run core migrations, ensure the folder
    /// layout exists and load settings.
    pub async fn new(
        db: Db,
        paths: Paths,
        events: Arc<dyn EventSink>,
        creds: Arc<dyn CredentialProvider>,
        notifier: Arc<dyn Notifier>,
    ) -> Result<Self> {
        paths.ensure_dirs()?;
        db.migrate_core()?;
        let settings = SettingsService::load(db.clone(), events.clone()).await?;
        let sync_status = SyncStatusHub::new(events.clone());
        Ok(Self {
            db,
            paths,
            events,
            creds,
            notifier,
            settings,
            sync_status,
        })
    }

    /// Context with no-op bridges and an in-memory database (used by tests in
    /// other crates). Paths are created under the given root.
    pub async fn for_test(paths: Paths) -> Result<Self> {
        use crate::traits::testing::{NullCredentialProvider, NullEventSink, NullNotifier};
        let db = Db::open_in_memory()?;
        Self::new(
            db,
            paths,
            Arc::new(NullEventSink),
            Arc::new(NullCredentialProvider),
            Arc::new(NullNotifier),
        )
        .await
    }

    /// Apply a migration namespace owned by a domain crate.
    pub fn migrate(&self, namespace: &str, migrations: &[(u32, &str)]) -> Result<()> {
        self.db.migrate(namespace, migrations)?;
        Ok(())
    }
}
