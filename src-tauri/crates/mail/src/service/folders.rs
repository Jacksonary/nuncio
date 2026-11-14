//! Folder commands: listing, unified counts, create/rename/move/delete,
//! favorites/sync flags, mark-all-read and emptying trash/junk.

use nuncio_core::models::{Folder, FolderRole, ProviderKind, UnifiedCounts};
use nuncio_core::{events, repo, AppError, Result};
use tracing::debug;

use crate::ops::{self, FolderMarkReadOp, FolderSubscribeOp, KIND_FOLDER_MARK_READ};

use super::MailService;

fn is_local_only(provider: ProviderKind) -> bool {
    matches!(provider, ProviderKind::Local | ProviderKind::Pop3)
}

impl MailService {
    /// All folders of an account (tree order is applied by the UI from
    /// `parentId`/`sortOrder`).
    pub async fn folder_list(&self, account_id: &str) -> Result<Vec<Folder>> {
        let account_id = account_id.to_string();
        self.inner
            .ctx
            .db
            .read(move |db| repo::folders::list(db, &account_id))
            .await
    }

    /// Inbox / flagged / drafts / outbox counts for the unified sidebar.
    pub async fn folder_unified_counts(&self) -> Result<UnifiedCounts> {
        self.inner.ctx.db.read(repo::folders::unified_counts).await
    }

    pub async fn folder_create(
        &self,
        account_id: &str,
        parent_id: Option<&str>,
        name: &str,
    ) -> Result<Folder> {
        let name = name.trim();
        if name.is_empty() {
            return Err(AppError::invalid_input("folder name must not be empty"));
        }
        let account = self.account(account_id).await?;
        if let Some(parent_id) = parent_id {
            let parent = self.folder(parent_id).await?;
            if parent.account_id != account.id {
                return Err(AppError::invalid_input(
                    "parent folder belongs to another account",
                ));
            }
        }
        if is_local_only(account.provider) {
            let account_id_owned = account.id.clone();
            let parent = parent_id.map(str::to_string);
            let name_owned = name.to_string();
            let mut folder = self
                .inner
                .ctx
                .db
                .write(move |db| {
                    repo::folders::create(db, &account_id_owned, parent.as_deref(), &name_owned)
                })
                .await?;
            // Keep the local remote path hierarchical so a later rename/move
            // still has something sensible to work with.
            if folder.parent_id.is_some() && !folder.remote_path.contains('/') {
                folder = self.rename_local_path(&folder).await?;
            }
            self.emit_changed(
                &account.id,
                vec![folder.id.clone()],
                Vec::new(),
                events::MailChangeKind::Folders,
            );
            return Ok(folder);
        }

        self.inner
            .engine
            .create_folder(&account.id, parent_id, name)
            .await?;
        let loaded = self.folder_list(&account.id).await?;
        let parent = parent_id.map(str::to_string);
        let created = loaded
            .into_iter()
            .find(|folder| folder.parent_id == parent && folder.name.eq_ignore_ascii_case(name));
        match created {
            Some(folder) => {
                self.emit_changed(
                    &account.id,
                    vec![folder.id.clone()],
                    Vec::new(),
                    events::MailChangeKind::Folders,
                );
                Ok(folder)
            }
            None => Err(AppError::internal(
                "folder was created on the server but did not appear locally",
            )),
        }
    }

    /// Ensure the local remote path reflects the parent hierarchy.
    async fn rename_local_path(&self, folder: &Folder) -> Result<Folder> {
        let Some(parent_id) = folder.parent_id.clone() else {
            return Ok(folder.clone());
        };
        let parent = self.folder(&parent_id).await?;
        let remote = format!(
            "{}/{}",
            parent.remote_path.trim_end_matches('/'),
            folder.name
        );
        let id = folder.id.clone();
        self.inner
            .ctx
            .db
            .write(move |db| {
                db.execute(
                    "UPDATE folder SET remote_path = ?2, updated_at = ?3 WHERE id = ?1",
                    rusqlite::params![id, remote, repo::now_string()],
                )?;
                Ok(())
            })
            .await?;
        self.folder(&folder.id).await
    }

    pub async fn folder_rename(&self, id: &str, name: &str) -> Result<Folder> {
        let name = name.trim();
        if name.is_empty() {
            return Err(AppError::invalid_input("folder name must not be empty"));
        }
        let folder = self.folder(id).await?;
        let account = self.account(&folder.account_id).await?;
        if is_local_only(account.provider) {
            let id_owned = folder.id.clone();
            let name_owned = name.to_string();
            let updated = self
                .inner
                .ctx
                .db
                .write(move |db| repo::folders::rename(db, &id_owned, &name_owned))
                .await?;
            self.emit_changed(
                &account.id,
                vec![updated.id.clone()],
                Vec::new(),
                events::MailChangeKind::Folders,
            );
            return Ok(updated);
        }
        self.inner
            .engine
            .rename_folder(&account.id, &folder.id, name)
            .await?;
        let updated = self.folder(&folder.id).await?;
        self.emit_changed(
            &account.id,
            vec![updated.id.clone()],
            Vec::new(),
            events::MailChangeKind::Folders,
        );
        Ok(updated)
    }

    pub async fn folder_move(&self, id: &str, new_parent_id: Option<&str>) -> Result<Folder> {
        let folder = self.folder(id).await?;
        let account = self.account(&folder.account_id).await?;
        if new_parent_id == Some(folder.id.as_str()) {
            return Err(AppError::invalid_input("a folder cannot be its own parent"));
        }
        if is_local_only(account.provider) {
            let id_owned = folder.id.clone();
            let parent = new_parent_id.map(str::to_string);
            let updated = self
                .inner
                .ctx
                .db
                .write(move |db| repo::folders::move_folder(db, &id_owned, parent.as_deref()))
                .await?;
            let updated = self.rename_local_path(&updated).await?;
            self.emit_changed(
                &account.id,
                vec![updated.id.clone()],
                Vec::new(),
                events::MailChangeKind::Folders,
            );
            return Ok(updated);
        }
        self.inner
            .engine
            .move_folder(&account.id, &folder.id, new_parent_id)
            .await?;
        let updated = self.folder(&folder.id).await?;
        self.emit_changed(
            &account.id,
            vec![updated.id.clone()],
            Vec::new(),
            events::MailChangeKind::Folders,
        );
        Ok(updated)
    }

    pub async fn folder_delete(&self, id: &str) -> Result<()> {
        let folder = self.folder(id).await?;
        if matches!(
            folder.role,
            FolderRole::Inbox
                | FolderRole::Sent
                | FolderRole::Drafts
                | FolderRole::Trash
                | FolderRole::Junk
                | FolderRole::All
        ) {
            return Err(AppError::invalid_input("a system folder cannot be deleted"));
        }
        let account = self.account(&folder.account_id).await?;
        if is_local_only(account.provider) {
            let id_owned = folder.id.clone();
            self.inner
                .ctx
                .db
                .write(move |db| repo::folders::delete(db, &id_owned))
                .await?;
        } else {
            self.inner
                .engine
                .delete_folder(&account.id, &folder.id)
                .await?;
        }
        self.emit_changed(
            &account.id,
            vec![folder.id.clone()],
            Vec::new(),
            events::MailChangeKind::Folders,
        );
        self.update_badge().await;
        Ok(())
    }

    pub async fn folder_set_favorite(&self, id: &str, value: bool) -> Result<()> {
        let folder = self.folder(id).await?;
        let id_owned = folder.id.clone();
        self.inner
            .ctx
            .db
            .write(move |db| repo::folders::set_favorite(db, &id_owned, value))
            .await?;
        self.emit_changed(
            &folder.account_id,
            vec![folder.id],
            Vec::new(),
            events::MailChangeKind::Folders,
        );
        Ok(())
    }

    pub async fn folder_set_sync_enabled(&self, id: &str, value: bool) -> Result<()> {
        let folder = self.folder(id).await?;
        let id_owned = folder.id.clone();
        self.inner
            .ctx
            .db
            .write(move |db| repo::folders::set_sync_enabled(db, &id_owned, value))
            .await?;
        self.emit_changed(
            &folder.account_id,
            vec![folder.id.clone()],
            Vec::new(),
            events::MailChangeKind::Folders,
        );
        if value {
            let engine = self.inner.engine.clone();
            let folder_id = folder.id.clone();
            tokio::spawn(async move {
                if let Err(error) = engine.sync_folder(&folder_id).await {
                    debug!(%error, folder = %folder_id, "folder sync after enabling failed");
                }
            });
        }
        Ok(())
    }

    /// Mark every message in the folder as read (server `STORE 1:* +FLAGS`).
    pub async fn folder_mark_all_read(&self, id: &str) -> Result<()> {
        let folder = self.folder(id).await?;
        let id_owned = folder.id.clone();
        self.inner
            .ctx
            .db
            .write(move |db| {
                repo::messages::mark_all_read(db, &id_owned)?;
                Ok(())
            })
            .await?;
        let account = self.account(&folder.account_id).await?;
        if !is_local_only(account.provider) {
            let payload = FolderMarkReadOp {
                folder_id: folder.id.clone(),
            };
            ops::enqueue_operation(
                &self.inner.ctx,
                &account.id,
                KIND_FOLDER_MARK_READ,
                serde_json::to_value(payload).map_err(|error| {
                    AppError::internal(format!("cannot encode folderMarkRead: {error}"))
                })?,
                "Mark folder as read",
            )
            .await?;
        }
        self.emit_changed(
            &folder.account_id,
            vec![folder.id],
            Vec::new(),
            events::MailChangeKind::Updated,
        );
        self.update_badge().await;
        Ok(())
    }

    /// Permanently delete everything in a trash/junk folder.
    pub async fn folder_empty(&self, id: &str) -> Result<()> {
        let folder = self.folder(id).await?;
        if !matches!(folder.role, FolderRole::Trash | FolderRole::Junk) {
            return Err(AppError::invalid_input(
                "only a trash or junk folder can be emptied",
            ));
        }
        let account = self.account(&folder.account_id).await?;
        if is_local_only(account.provider) {
            let folder_id = folder.id.clone();
            self.inner
                .ctx
                .db
                .write(move |db| {
                    repo::messages::delete_all_in_folder(db, &folder_id)?;
                    Ok(())
                })
                .await?;
        } else {
            match self
                .inner
                .engine
                .expunge_folder(&account.id, &folder.id)
                .await
            {
                Ok(()) => {}
                Err(error) if matches!(error.code(), "notFound" | "internal") => {
                    let mut connection =
                        crate::imap::connect_account(&self.inner.ctx, &account).await?;
                    connection.select(&folder.remote_path).await?;
                    crate::ops::executor::store(
                        &mut connection,
                        "1:*",
                        "+FLAGS.SILENT (\\Deleted)",
                    )
                    .await?;
                    crate::ops::executor::expire(&mut connection, "1:*").await?;
                    let folder_id = folder.id.clone();
                    self.inner
                        .ctx
                        .db
                        .write(move |db| {
                            repo::messages::delete_all_in_folder(db, &folder_id)?;
                            Ok(())
                        })
                        .await?;
                }
                Err(error) => return Err(error),
            }
        }
        self.emit_changed(
            &folder.account_id,
            vec![folder.id],
            Vec::new(),
            events::MailChangeKind::Deleted,
        );
        self.update_badge().await;
        Ok(())
    }

    /// Synchronize a single folder immediately.
    pub async fn folder_sync(&self, id: &str) -> Result<()> {
        let folder = self.folder(id).await?;
        match self.inner.engine.sync_folder(&folder.id).await {
            Ok(()) => Ok(()),
            Err(error) if matches!(error.code(), "notFound" | "internal") => {
                let account = self.account(&folder.account_id).await?;
                if let Some(_imap) = account.imap.as_ref() {
                    let mut connection =
                        crate::imap::connect_account(&self.inner.ctx, &account).await?;
                    crate::sync::sync_folder_by_id(
                        &self.inner.ctx,
                        &mut connection,
                        &account,
                        &folder.id,
                    )
                    .await
                } else {
                    crate::pop3::sync::sync_account(&self.inner.ctx, &account).await
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Subscribe/unsubscribe a folder (IMAP only) and mirror the flag locally.
    pub async fn folder_set_subscribed(&self, id: &str, subscribed: bool) -> Result<()> {
        let folder = self.folder(id).await?;
        let account = self.account(&folder.account_id).await?;
        if is_local_only(account.provider) {
            return Ok(());
        }
        let payload = FolderSubscribeOp {
            folder_id: folder.id.clone(),
            subscribe: subscribed,
        };
        ops::enqueue_operation(
            &self.inner.ctx,
            &account.id,
            ops::KIND_FOLDER_SUBSCRIBE,
            serde_json::to_value(payload).map_err(|error| {
                AppError::internal(format!("cannot encode folderSubscribe: {error}"))
            })?,
            if subscribed {
                "Subscribe folder"
            } else {
                "Unsubscribe folder"
            },
        )
        .await?;
        Ok(())
    }

    /// Emit a folder-count refresh for the whole account (used after bulk
    /// operations performed by the engine).
    pub async fn refresh_account_folders(&self, account_id: &str) -> Result<()> {
        let account_id_owned = account_id.to_string();
        self.inner
            .ctx
            .db
            .write(move |db| {
                repo::folders::recompute_counts_for_account(db, &account_id_owned)?;
                Ok(())
            })
            .await?;
        let folders = self.folder_list(account_id).await?;
        let ids = folders.into_iter().map(|folder| folder.id).collect();
        events::emit_folders_changed(self.inner.ctx.events.as_ref(), account_id, ids);
        Ok(())
    }

    /// Internal helper used by tests and the app's context menu.
    pub async fn folder_by_role(
        &self,
        account_id: &str,
        role: FolderRole,
    ) -> Result<Option<Folder>> {
        let account_id = account_id.to_string();
        self.inner
            .ctx
            .db
            .read(move |db| repo::folders::by_role(db, &account_id, role))
            .await
    }
}
