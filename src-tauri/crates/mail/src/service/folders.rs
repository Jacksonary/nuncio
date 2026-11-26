//! Folder commands: listing, unified counts, create/rename/move/delete,
//! favorites/sync flags, mark-all-read and emptying trash/junk.

use nuncio_core::models::{Folder, FolderRole, NewPendingOp, ProviderKind, UnifiedCounts};
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
        let account = self.account(&folder.account_id).await?;
        let remote = !is_local_only(account.provider);
        let folder_id = folder.id.clone();
        let account_id = account.id.clone();
        // One id serves as both the queued operation id and the writer owner.
        // The executor can then tell, per message, whether this folder-wide
        // intent is still the newest `seen` write.
        let writer_id = nuncio_core::models::new_id();
        let writer_for_db = writer_id.clone();
        let writer_for_op = writer_id.clone();
        let payload_json = if remote {
            Some(
                serde_json::to_value(FolderMarkReadOp {
                    folder_id: folder_id.clone(),
                })
                .map_err(|error| {
                    AppError::internal(format!("cannot encode folderMarkRead: {error}"))
                })?,
            )
        } else {
            None
        };
        // The local value, the writer rows and the queue row are one
        // transaction. A user action that commits just after this one records
        // its own writer, so the retried folder operation skips that row.
        self.inner
            .ctx
            .db
            .write(move |db| {
                let ids = repo::messages::ids_in_folder(db, &folder_id)?;
                repo::messages::mark_all_read(db, &folder_id)?;
                for message_id in &ids {
                    repo::messages::record_flag_writer(
                        db,
                        message_id,
                        "seen",
                        &writer_for_db,
                        Some(true),
                        "folderMarkRead",
                    )?;
                }
                if let Some(payload) = payload_json {
                    repo::pending_ops::enqueue_with_id(
                        db,
                        &writer_for_op,
                        NewPendingOp::new(
                            account_id,
                            KIND_FOLDER_MARK_READ,
                            payload,
                            "Mark folder as read",
                        ),
                    )?;
                }
                Ok(())
            })
            .await?;
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
    ///
    /// Returns `false` when the messages could not be removed from the server
    /// (no UIDPLUS): only their `\Deleted` flag was set and the local rows were
    /// left untouched, so the caller must not report the folder as emptied.
    pub async fn folder_empty(&self, id: &str) -> Result<bool> {
        let folder = self.folder(id).await?;
        if !matches!(folder.role, FolderRole::Trash | FolderRole::Junk) {
            return Err(AppError::invalid_input(
                "only a trash or junk folder can be emptied",
            ));
        }
        let account = self.account(&folder.account_id).await?;
        let expunged = if is_local_only(account.provider) {
            let folder_id = folder.id.clone();
            self.inner
                .ctx
                .db
                .write(move |db| {
                    repo::messages::delete_all_in_folder(db, &folder_id)?;
                    Ok(())
                })
                .await?;
            true
        } else if crate::graph::uses_graph(&account) {
            // Graph has no EXPUNGE. Deleting every message one by one is the
            // only equivalent, and it is expensive enough that the user should
            // see a clear answer instead of a silent half-empty folder. Falling
            // through to IMAP here would fail with a completely unrelated
            // authentication error.
            return Err(AppError::unsupported(
                "emptying a folder is not supported for Microsoft accounts yet",
            ));
        } else {
            match self
                .inner
                .engine
                .expunge_folder(&account.id, &folder.id)
                .await
            {
                Ok(expunged) => expunged,
                Err(error) if matches!(error.code(), "notFound" | "internal") => {
                    // Graph accounts never had an IMAP session to fall back to;
                    // connecting would surface an authentication error that has
                    // nothing to do with what the user asked for.
                    if crate::graph::uses_graph(&account) {
                        return Err(error);
                    }
                    let mut connection =
                        crate::imap::connect_account(&self.inner.ctx, &account).await?;
                    // Same rule as the actor path: without a directed expunge
                    // there is no safe way to empty the folder, so do nothing
                    // and report `false` to the UI instead of flagging every
                    // message `\Deleted`.
                    if !connection.caps.directed_expunge() {
                        false
                    } else {
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
                        true
                    }
                }
                Err(error) => return Err(error),
            }
        };
        if expunged {
            self.emit_changed(
                &folder.account_id,
                vec![folder.id],
                Vec::new(),
                events::MailChangeKind::Deleted,
            );
            self.update_badge().await;
        }
        Ok(expunged)
    }

    /// Synchronize a single folder immediately.
    pub async fn folder_sync(&self, id: &str) -> Result<()> {
        let folder = self.folder(id).await?;
        match self.inner.engine.sync_folder(&folder.id).await {
            Ok(()) => Ok(()),
            Err(error) if matches!(error.code(), "notFound" | "internal") => {
                let account = self.account(&folder.account_id).await?;
                // Same rule: the direct-sync fallback is an IMAP path. A Graph
                // account has no IMAP connection, so surface the engine error
                // instead of failing on a confusing authentication message.
                if crate::graph::uses_graph(&account) {
                    return Err(error);
                }
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

    /// Load the next batch of older mail for a real folder (IMAP / Graph).
    /// POP3 and local accounts answer `{ loaded: 0, hasMore: false }`.
    pub async fn folder_load_older(
        &self,
        id: &str,
    ) -> Result<nuncio_core::models::LoadOlderResult> {
        let folder = self.folder(id).await?;
        let account = self.account(&folder.account_id).await?;
        if is_local_only(account.provider) {
            return Ok(nuncio_core::models::LoadOlderResult::default());
        }
        let result = self.inner.engine.load_older(&folder.id).await?;
        if result.loaded > 0 {
            self.emit_changed(
                &folder.account_id,
                vec![folder.id.clone()],
                Vec::new(),
                events::MailChangeKind::Updated,
            );
            self.update_badge().await;
        }
        Ok(result)
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
