//! Undo for optimistic message actions.

use std::collections::HashMap;

use nuncio_core::events::MailChangeKind;
use nuncio_core::models::{PendingOpState, ProviderKind, StoredMessage, UndoEntry};
use nuncio_core::{events, repo, AppError, Result};
use tracing::{debug, warn};

use crate::ops::{self, MarkJunkOp, MoveOp, SetFlagsOp, KIND_MARK_JUNK, KIND_MOVE, KIND_SET_FLAGS};

use super::messages::{UndoEntryState, UndoPayload};
use super::MailService;

fn is_local_only(provider: ProviderKind) -> bool {
    matches!(provider, ProviderKind::Local | ProviderKind::Pop3)
}

impl MailService {
    /// Revert the most recent undoable action (the keyboard shortcut path).
    pub(crate) async fn undo_last(&self) -> Result<bool> {
        let entry = self
            .inner
            .ctx
            .db
            .read(|db| repo::undo::pop_last(db, None))
            .await?;
        self.apply_undo_entry(entry).await
    }

    /// Revert one specific action, as identified by the id returned when it ran.
    ///
    /// Toasts use this so an action that happened afterwards is not the one
    /// that gets reverted.
    pub(crate) async fn undo_by_id(&self, undo_id: &str) -> Result<bool> {
        let undo_id = undo_id.to_string();
        let entry = self
            .inner
            .ctx
            .db
            .read(move |db| repo::undo::pop_by_id(db, &undo_id))
            .await?;
        self.apply_undo_entry(entry).await
    }

    async fn apply_undo_entry(&self, entry: Option<UndoEntry>) -> Result<bool> {
        let Some(entry) = entry else {
            return Ok(false);
        };
        let payload: UndoPayload = serde_json::from_value(entry.payload.clone())
            .map_err(|error| AppError::internal(format!("invalid undo payload: {error}")))?;
        if payload.entries.is_empty() {
            return Ok(false);
        }

        // Inspect the queued operations recorded with the action.
        let mut states: Vec<PendingOpState> = Vec::new();
        for op_id in &payload.op_ids {
            let id = op_id.clone();
            if let Some(op) = self
                .inner
                .ctx
                .db
                .read(move |db| repo::pending_ops::get(db, &id))
                .await?
            {
                states.push(op.state);
            }
        }
        let any_done = states.contains(&PendingOpState::Done);
        let any_open = states
            .iter()
            .any(|state| matches!(state, PendingOpState::Pending | PendingOpState::InProgress));

        // A permanent delete that already reached the server cannot be undone.
        if payload.action == "delete"
            && payload.entries.iter().any(|entry| entry.permanent)
            && any_done
        {
            warn!("cannot undo a permanent delete that already ran on the server");
            return Ok(false);
        }

        // Cancel operations that have not run yet.
        if any_open {
            for op_id in &payload.op_ids {
                let id = op_id.clone();
                self.inner
                    .ctx
                    .db
                    .write(move |db| repo::pending_ops::cancel(db, &id))
                    .await?;
            }
        }

        // Restore local state.
        let entries = payload.entries.clone();
        self.inner
            .ctx
            .db
            .write(move |db| {
                let mut folders: Vec<String> = Vec::new();
                for entry in &entries {
                    if !folders.contains(&entry.folder_id) {
                        folders.push(entry.folder_id.clone());
                    }
                    if let Some(target) = &entry.target_folder_id {
                        if !folders.contains(target) {
                            folders.push(target.clone());
                        }
                    }
                    let exists = repo::messages::get(db, &entry.id)?.is_some();
                    if exists {
                        repo::messages::set_flags(
                            db,
                            std::slice::from_ref(&entry.id),
                            Some(entry.seen),
                            Some(entry.flagged),
                            None,
                            None,
                        )?;
                        repo::messages::move_to_folder(
                            db,
                            std::slice::from_ref(&entry.id),
                            &entry.folder_id,
                        )?;
                        repo::messages::set_deleted_local(db, &entry.id, false)?;
                    } else {
                        let mut snapshot: StoredMessage = entry.snapshot.clone().into_stored();
                        snapshot.summary.seen = entry.seen;
                        snapshot.summary.flagged = entry.flagged;
                        snapshot.is_deleted_local = false;
                        repo::messages::upsert(db, &snapshot)?;
                    }
                    repo::attachments::replace_for_message(db, &entry.id, &entry.attachments)?;
                }
                for folder in folders {
                    repo::folders::recompute_counts(db, &folder)?;
                }
                Ok(())
            })
            .await?;

        // Best-effort compensating server operations when the original action
        // already reached the server.
        if any_done {
            self.queue_undo_compensation(&payload).await?;
        }

        let mut per_account: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();
        for entry in &payload.entries {
            let bucket = per_account.entry(entry.account_id.clone()).or_default();
            for folder in [
                Some(entry.folder_id.clone()),
                entry.target_folder_id.clone(),
            ]
            .into_iter()
            .flatten()
            {
                if !bucket.0.contains(&folder) {
                    bucket.0.push(folder);
                }
            }
            bucket.1.push(entry.id.clone());
        }
        for (account_id, (folders, message_ids)) in per_account {
            events::emit_mail_changed(
                self.inner.ctx.events.as_ref(),
                account_id,
                folders,
                message_ids,
                MailChangeKind::Updated,
            );
        }
        self.update_badge().await;
        debug!(action = %payload.action, count = payload.entries.len(), "undid message action");
        Ok(true)
    }

    async fn queue_undo_compensation(&self, payload: &UndoPayload) -> Result<()> {
        for entry in &payload.entries {
            let account = match self.account(&entry.account_id).await {
                Ok(account) => account,
                Err(error) => {
                    warn!(account = %entry.account_id, %error, "undo: account missing");
                    continue;
                }
            };
            if is_local_only(account.provider) {
                continue;
            }
            match payload.action.as_str() {
                "setSeen" => {
                    let Some(uid) = entry.uid else { continue };
                    let op = SetFlagsOp {
                        folder_id: entry.folder_id.clone(),
                        uids: vec![uid],
                        seen: Some(entry.seen),
                        flagged: None,
                        answered: None,
                        forwarded: None,
                        previous: vec![crate::ops::FlagRollback {
                            uid,
                            seen: !entry.seen,
                            flagged: entry.flagged,
                            answered: entry.answered,
                            forwarded: entry.forwarded,
                        }],
                    };
                    ops::enqueue_operation(
                        &self.inner.ctx,
                        &account.id,
                        KIND_SET_FLAGS,
                        serde_json::to_value(op).map_err(|error| {
                            AppError::internal(format!("undo setFlags: {error}"))
                        })?,
                        "Undo: restore read state",
                    )
                    .await?;
                }
                "setFlagged" => {
                    let Some(uid) = entry.uid else { continue };
                    let op = SetFlagsOp {
                        folder_id: entry.folder_id.clone(),
                        uids: vec![uid],
                        seen: None,
                        flagged: Some(entry.flagged),
                        answered: None,
                        forwarded: None,
                        previous: vec![crate::ops::FlagRollback {
                            uid,
                            seen: entry.seen,
                            flagged: !entry.flagged,
                            answered: entry.answered,
                            forwarded: entry.forwarded,
                        }],
                    };
                    ops::enqueue_operation(
                        &self.inner.ctx,
                        &account.id,
                        KIND_SET_FLAGS,
                        serde_json::to_value(op).map_err(|error| {
                            AppError::internal(format!("undo setFlags: {error}"))
                        })?,
                        "Undo: restore flag state",
                    )
                    .await?;
                }
                "move" | "archive" | "delete" => {
                    let Some(target) = entry.target_folder_id.as_deref() else {
                        continue;
                    };
                    if entry.permanent {
                        continue;
                    }
                    let Some((remote_folder, uid)) =
                        self.find_remote_location(entry, target).await?
                    else {
                        warn!(
                            message = %entry.id,
                            "undo: could not locate the moved message on the server yet"
                        );
                        continue;
                    };
                    let op = MoveOp {
                        folder_id: remote_folder,
                        target_folder_id: entry.folder_id.clone(),
                        uids: vec![uid],
                    };
                    // Move/archive/delete inverse: move the message back.
                    ops::enqueue_operation(
                        &self.inner.ctx,
                        &account.id,
                        KIND_MOVE,
                        serde_json::to_value(op)
                            .map_err(|error| AppError::internal(format!("undo move: {error}")))?,
                        "Undo: move message back",
                    )
                    .await?;
                }
                "markJunk" => {
                    let value = payload.value.unwrap_or(false);
                    let Some(target) = entry.target_folder_id.as_deref() else {
                        continue;
                    };
                    let Some((remote_folder, uid)) =
                        self.find_remote_location(entry, target).await?
                    else {
                        continue;
                    };
                    let op = MarkJunkOp {
                        folder_id: remote_folder,
                        uids: vec![uid],
                        value: !value,
                        target_folder_id: Some(entry.folder_id.clone()),
                    };
                    ops::enqueue_operation(
                        &self.inner.ctx,
                        &account.id,
                        KIND_MARK_JUNK,
                        serde_json::to_value(op).map_err(|error| {
                            AppError::internal(format!("undo markJunk: {error}"))
                        })?,
                        "Undo: move message back",
                    )
                    .await?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Look up the message in the folder it was moved to, returning the folder
    /// id and UID that the inverse server operation must use.
    async fn find_remote_location(
        &self,
        entry: &UndoEntryState,
        target_folder_id: &str,
    ) -> Result<Option<(String, i64)>> {
        let Some(header) = entry.snapshot.message_id_header.clone() else {
            // Without a Message-ID fall back to the recorded UID in the target
            // folder (best effort; UIDs are folder-local).
            return Ok(entry.uid.map(|uid| (target_folder_id.to_string(), uid)));
        };
        let account_id = entry.account_id.clone();
        let target = target_folder_id.to_string();
        let found = self
            .inner
            .ctx
            .db
            .read(move |db| {
                let rows = repo::messages::by_message_id(db, &header, Some(&account_id))?;
                Ok(rows
                    .into_iter()
                    .find(|row| row.summary.folder_id == target)
                    .and_then(|row| row.uid.map(|uid| (row.summary.folder_id, uid))))
            })
            .await?;
        Ok(found)
    }
}
