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
        if let Some(delay) = patch
            .mail
            .as_ref()
            .and_then(|mail| mail.mark_read_delay_seconds)
        {
            if delay > 3600 {
                return Err(AppError::invalid_input(
                    "markReadDelaySeconds must be <= 3600",
                ));
            }
        }
        if let Some(lines) = patch.mail.as_ref().and_then(|mail| mail.preview_lines) {
            if lines > 10 {
                return Err(AppError::invalid_input("previewLines must be <= 10"));
            }
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
    use crate::models::{HoverAction, ThemeMode};
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
            "mail": { "previewLines": 2, "hoverActions": ["delete", "flag"] }
        }))
        .expect("patch");
        let updated = service.update(patch).await.expect("update");
        assert_eq!(updated.theme, ThemeMode::Dark);
        assert_eq!(updated.mail.preview_lines, 2);
        assert_eq!(
            updated.mail.hover_actions,
            vec![HoverAction::Delete, HoverAction::Flag]
        );
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
                serde_json::from_value(serde_json::json!({"startMinimized": true})).unwrap();
            b.update(patch).await.unwrap();
        });
        first.await.expect("first");
        second.await.expect("second");
        let settings = service.current();
        assert!(!settings.close_to_tray);
        assert!(settings.start_minimized);
    }

    #[tokio::test]
    async fn validation_rejects_bad_values() {
        let patch: SettingsPatch =
            serde_json::from_value(serde_json::json!({"mail": {"previewLines": 99}}))
                .expect("patch");
        assert!(SettingsService::validate(&patch).is_err());
        let patch: SettingsPatch =
            serde_json::from_value(serde_json::json!({"mail": {"previewLines": 3}}))
                .expect("patch");
        assert!(SettingsService::validate(&patch).is_ok());
    }
}
