//! Cached application settings with deep-merge updates.

use std::sync::{Arc, RwLock};

use crate::db::Db;
use crate::error::AppError;
use crate::events;
use crate::models::{merge_settings, AppSettings, SettingsPatch};
use crate::repo;
use crate::traits::EventSink;
use crate::Result;

struct Inner {
    db: Db,
    events: Arc<dyn EventSink>,
    cache: RwLock<AppSettings>,
    /// Serializes concurrent updates so read-merge-write cannot lose a patch.
    update_lock: tokio::sync::Mutex<()>,
}

/// Settings service (load / get / patch / persist / emit).
#[derive(Clone)]
pub struct SettingsService {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for SettingsService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let current = self.current();
        f.debug_struct("SettingsService")
            .field("theme", &current.theme)
            .field("language", &current.language)
            .field("close_to_tray", &current.close_to_tray)
            .finish_non_exhaustive()
    }
}

impl SettingsService {
    /// Load settings from the database (writing defaults when the row is
    /// missing or contains an empty document).
    pub async fn load(db: Db, events: Arc<dyn EventSink>) -> Result<Self> {
        let stored = db.read(repo::settings::load).await.unwrap_or_else(|error| {
            tracing::warn!(%error, "failed to load settings, using defaults");
            None
        });

        let (settings, needs_save) = match stored {
            Some(settings) => (settings, false),
            None => (AppSettings::default(), true),
        };

        let service = Self {
            inner: Arc::new(Inner {
                db,
                events,
                cache: RwLock::new(settings),
                update_lock: tokio::sync::Mutex::new(()),
            }),
        };
        if needs_save {
            service.persist(&service.current()).await?;
        }
        Ok(service)
    }

    /// Build a service around an explicit settings value (tests / import).
    pub fn from_settings(db: Db, events: Arc<dyn EventSink>, settings: AppSettings) -> Self {
        Self {
            inner: Arc::new(Inner {
                db,
                events,
                cache: RwLock::new(settings),
                update_lock: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// Current cached settings.
    pub fn current(&self) -> AppSettings {
        self.inner
            .cache
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Same as [`SettingsService::current`]; async for command parity.
    pub async fn get(&self) -> AppSettings {
        self.current()
    }

    /// Apply a deep partial patch, persist it and emit `settings://changed`.
    pub async fn update(&self, patch: SettingsPatch) -> Result<AppSettings> {
        let _guard = self.inner.update_lock.lock().await;
        let merged = merge_settings(&self.current(), &patch)?;
        self.persist(&merged).await?;
        self.replace_cache(merged.clone());
        events::emit_settings_changed(self.inner.events.as_ref(), &merged);
        Ok(merged)
    }

    /// Replace all settings (backup restore / factory reset).
    pub async fn replace(&self, settings: AppSettings) -> Result<AppSettings> {
        let _guard = self.inner.update_lock.lock().await;
        self.persist(&settings).await?;
        self.replace_cache(settings.clone());
        events::emit_settings_changed(self.inner.events.as_ref(), &settings);
        Ok(settings)
    }

    /// Adopt settings that a caller has already written inside its own
    /// transaction. Only the cache and the change event are updated, so a
    /// restore cannot fail after its database work has committed.
    pub async fn adopt_persisted(&self, settings: AppSettings) -> AppSettings {
        let _guard = self.inner.update_lock.lock().await;
        self.replace_cache(settings.clone());
        events::emit_settings_changed(self.inner.events.as_ref(), &settings);
        settings
    }

    async fn persist(&self, settings: &AppSettings) -> Result<()> {
        let settings = settings.clone();
        self.inner
            .db
            .write(move |conn| repo::settings::save(conn, &settings))
            .await
    }

    fn replace_cache(&self, settings: AppSettings) {
        let mut cache = self
            .inner
            .cache
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *cache = settings;
    }

    /// Hint used by startup code to decide whether to recreate the tray/menus.
    pub fn close_to_tray(&self) -> bool {
        self.current().close_to_tray
    }

    /// Markup helper used by `account_delete` etc. to reject unsupported patches.
    pub fn validate(patch: &SettingsPatch) -> Result<()> {
        if let Some(theme) = patch.theme {
            let _ = theme;
        }
        if let Some(cache) = patch
            .mail
            .as_ref()
            .and_then(|mail| mail.body_cache_limit_mb)
        {
            if cache < 50 {
                return Err(AppError::invalid_input("bodyCacheLimitMb must be >= 50"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ThemeMode;
    use crate::traits::testing::RecordingEventSink;

    async fn service() -> (SettingsService, Arc<RecordingEventSink>) {
        let db = Db::open_in_memory().expect("db");
        db.migrate_core().expect("migrate");
        let sink = Arc::new(RecordingEventSink::new());
        let service = SettingsService::load(db, sink.clone()).await.expect("load");
        (service, sink)
    }

    #[tokio::test]
    async fn defaults_are_loaded_and_persisted() {
        let (service, _sink) = service().await;
        assert_eq!(service.current(), AppSettings::default());
        // A second load sees persisted defaults.
        let db = service.inner.db.clone();
        let reloaded = SettingsService::load(db, service.inner.events.clone())
            .await
            .expect("reload");
        assert_eq!(reloaded.current(), AppSettings::default());
    }

    #[tokio::test]
    async fn update_merges_and_emits() {
        let (service, sink) = service().await;
        let patch: SettingsPatch = serde_json::from_value(serde_json::json!({
            "theme": "dark",
            "mail": { "bodyCacheLimitMb": 120 }
        }))
        .expect("patch");
        let updated = service.update(patch).await.expect("update");
        assert_eq!(updated.theme, ThemeMode::Dark);
        assert_eq!(updated.mail.body_cache_limit_mb, 120);
        // Untouched sections survive the patch.
        assert_eq!(updated.calendar.work_days, vec![1, 2, 3, 4, 5]);

        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "settings://changed");
        assert_eq!(events[0].1["theme"], "dark");
    }

    #[tokio::test]
    async fn concurrent_updates_do_not_lose_patches() {
        let (service, _sink) = service().await;
        let a = service.clone();
        let b = service.clone();
        let first = tokio::spawn(async move {
            let patch: SettingsPatch =
                serde_json::from_value(serde_json::json!({"closeToTray": false})).unwrap();
            a.update(patch).await.unwrap();
        });
        let second = tokio::spawn(async move {
            let patch: SettingsPatch =
                serde_json::from_value(serde_json::json!({"launchAtStartup": true})).unwrap();
            b.update(patch).await.unwrap();
        });
        first.await.expect("first");
        second.await.expect("second");
        let settings = service.current();
        assert!(!settings.close_to_tray);
        assert!(settings.launch_at_startup);
    }

    #[tokio::test]
    async fn stored_settings_with_removed_keys_load_and_are_dropped() {
        let db = Db::open_in_memory().expect("db");
        db.migrate_core().expect("migrate");
        let legacy = serde_json::json!({
            "startMinimized": true,
            "closeToTray": false,
            "mail": { "hoverActions": ["delete"], "bodyCacheLimitMb": 300 },
            "contacts": { "sortBy": "lastName", "displayOrder": "lastFirst", "pollMinutes": 120 },
            "tasks": { "showCompleted": true, "newTasksOnTop": false, "pollMinutes": 45 }
        });
        let json = serde_json::to_string(&legacy).expect("json");
        db.write(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO settings (id, json, updated_at)
                 VALUES (1, ?1, '1970-01-01T00:00:00Z')",
                rusqlite::params![json],
            )?;
            Ok(())
        })
        .await
        .expect("seed");

        let sink = Arc::new(RecordingEventSink::new());
        let service = SettingsService::load(db.clone(), sink).await.expect("load");
        let settings = service.current();
        assert!(!settings.close_to_tray);
        assert_eq!(settings.mail.body_cache_limit_mb, 300);
        assert_eq!(settings.contacts.poll_minutes, 120);
        assert_eq!(settings.tasks.poll_minutes, 45);

        // The next write persists the cleaned document: removed keys are gone.
        let patch: SettingsPatch =
            serde_json::from_value(serde_json::json!({"theme": "dark"})).expect("patch");
        service.update(patch).await.expect("update");
        let stored: Option<AppSettings> = db.read(repo::settings::load).await.expect("read back");
        let stored = stored.expect("row");
        let value = serde_json::to_value(&stored).expect("serialize");
        assert!(value.get("startMinimized").is_none());
        assert!(value["mail"].get("hoverActions").is_none());
        assert!(value["contacts"].get("sortBy").is_none());
        assert!(value["contacts"].get("displayOrder").is_none());
        assert!(value["tasks"].get("showCompleted").is_none());
        assert!(value["tasks"].get("newTasksOnTop").is_none());
        assert_eq!(value["contacts"]["pollMinutes"], 120);
        assert_eq!(value["tasks"]["pollMinutes"], 45);
    }

    #[tokio::test]
    async fn validation_rejects_bad_values() {
        let patch: SettingsPatch =
            serde_json::from_value(serde_json::json!({"mail": {"bodyCacheLimitMb": 10}}))
                .expect("patch");
        assert!(SettingsService::validate(&patch).is_err());
        let patch: SettingsPatch =
            serde_json::from_value(serde_json::json!({"mail": {"bodyCacheLimitMb": 100}}))
                .expect("patch");
        assert!(SettingsService::validate(&patch).is_ok());
    }
}
