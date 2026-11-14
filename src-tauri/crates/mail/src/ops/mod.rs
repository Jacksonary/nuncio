//! Persistent server-operation queue.
//!
//! The command-facing service applies changes locally first and then enqueues
//! one of the typed payloads below. The account worker drains due operations in
//! creation order, batches compatible flag changes, and retries with the
//! exponential backoff stored by `nuncio_core::repo::pending_ops`.

pub mod executor;

use nuncio_core::models::NewPendingOp;
use nuncio_core::{repo, AppError, CoreContext, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::debug;

/// Operation kind identifiers persisted in `pending_op.kind`.
pub const KIND_SET_FLAGS: &str = "setFlags";
pub const KIND_MOVE: &str = "move";
pub const KIND_DELETE: &str = "delete";
pub const KIND_ARCHIVE: &str = "archive";
pub const KIND_MARK_JUNK: &str = "markJunk";
pub const KIND_APPEND_DRAFT: &str = "appendDraft";
pub const KIND_DELETE_REMOTE_DRAFT: &str = "deleteRemoteDraft";
pub const KIND_APPEND_TO_SENT: &str = "appendToSent";
pub const KIND_FOLDER_CREATE: &str = "folderCreate";
pub const KIND_FOLDER_RENAME: &str = "folderRename";
pub const KIND_FOLDER_DELETE: &str = "folderDelete";
pub const KIND_FOLDER_SUBSCRIBE: &str = "folderSubscribe";
pub const KIND_FOLDER_MARK_READ: &str = "folderMarkRead";
pub const KIND_SEND: &str = "send";

/// Local flag state captured before an optimistic change, so a permanently
/// failed `UID STORE` can be rolled back.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlagRollback {
    pub uid: i64,
    pub seen: bool,
    pub flagged: bool,
    pub answered: bool,
    pub forwarded: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetFlagsOp {
    pub folder_id: String,
    pub uids: Vec<i64>,
    pub seen: Option<bool>,
    pub flagged: Option<bool>,
    pub answered: Option<bool>,
    pub forwarded: Option<bool>,
    /// Flags before the optimistic local update (empty for ops queued by
    /// background code paths that did not change local flags first).
    #[serde(default)]
    pub previous: Vec<FlagRollback>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveOp {
    pub folder_id: String,
    pub target_folder_id: String,
    pub uids: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteOp {
    pub folder_id: String,
    pub uids: Vec<i64>,
    pub permanent: bool,
    /// Folder the optimistic local change moved the rows into (Trash).
    #[serde(default)]
    pub target_folder_id: Option<String>,
    /// Rows were hidden with `is_deleted_local` instead of being moved.
    #[serde(default)]
    pub soft_hidden: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveOp {
    pub folder_id: String,
    pub uids: Vec<i64>,
    /// Archive folder chosen when the op was queued (rollback target).
    #[serde(default)]
    pub target_folder_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkJunkOp {
    pub folder_id: String,
    pub uids: Vec<i64>,
    pub value: bool,
    /// Destination folder chosen when the op was queued (rollback target).
    #[serde(default)]
    pub target_folder_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppendDraftOp {
    pub draft_id: String,
    /// Drafts folder id.
    pub folder_id: String,
    pub rfc822_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteRemoteDraftOp {
    pub folder_id: String,
    pub uid: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppendToSentOp {
    pub folder_id: String,
    pub rfc822_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderCreateOp {
    pub parent_id: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderRenameOp {
    pub folder_id: String,
    pub new_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderDeleteOp {
    pub folder_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderSubscribeOp {
    pub folder_id: String,
    pub subscribe: bool,
}

/// Mark every message in a folder as seen (`UID STORE 1:* +FLAGS \Seen`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderMarkReadOp {
    pub folder_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendOp {
    pub outbox_id: String,
}

async fn enqueue(
    ctx: &CoreContext,
    account_id: &str,
    kind: &str,
    payload: serde_json::Value,
    description: &str,
) -> Result<()> {
    enqueue_operation(ctx, account_id, kind, payload, description)
        .await
        .map(|_| ())
}

/// Queue one server operation and return the persisted row (the command layer
/// records its id so that "undo last" can cancel a still-pending action).
pub async fn enqueue_operation(
    ctx: &CoreContext,
    account_id: &str,
    kind: &str,
    payload: serde_json::Value,
    description: &str,
) -> Result<nuncio_core::models::PendingOp> {
    let op = NewPendingOp::new(account_id, kind, payload, description);
    let account_id = account_id.to_string();
    let stored = ctx
        .db
        .write(move |db| repo::pending_ops::enqueue(db, op))
        .await?;
    debug!(account = account_id, kind, "queued server operation");
    Ok(stored)
}

/// Queue a (batched) flag change.
#[allow(clippy::too_many_arguments)]
pub async fn enqueue_set_flags(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    uids: &[i64],
    seen: Option<bool>,
    flagged: Option<bool>,
    answered: Option<bool>,
    forwarded: Option<bool>,
    description: &str,
) -> Result<()> {
    if uids.is_empty()
        || (seen.is_none() && flagged.is_none() && answered.is_none() && forwarded.is_none())
    {
        return Ok(());
    }
    let payload = SetFlagsOp {
        folder_id: folder_id.to_string(),
        uids: uids.to_vec(),
        seen,
        flagged,
        answered,
        forwarded,
        previous: Vec::new(),
    };
    enqueue(
        ctx,
        account_id,
        KIND_SET_FLAGS,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode setFlags payload: {error}"))
        })?,
        description,
    )
    .await
}

/// Queue a move of several messages to another folder.
pub async fn enqueue_move(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    target_folder_id: &str,
    uids: &[i64],
    description: &str,
) -> Result<()> {
    if uids.is_empty() {
        return Ok(());
    }
    let payload = MoveOp {
        folder_id: folder_id.to_string(),
        target_folder_id: target_folder_id.to_string(),
        uids: uids.to_vec(),
    };
    enqueue(
        ctx,
        account_id,
        KIND_MOVE,
        serde_json::to_value(payload)
            .map_err(|error| AppError::internal(format!("cannot encode move payload: {error}")))?,
        description,
    )
    .await
}

/// Queue a delete (trash move or permanent expunge).
pub async fn enqueue_delete(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    uids: &[i64],
    permanent: bool,
    description: &str,
) -> Result<()> {
    let payload = DeleteOp {
        folder_id: folder_id.to_string(),
        uids: uids.to_vec(),
        permanent,
        target_folder_id: None,
        soft_hidden: false,
    };
    enqueue(
        ctx,
        account_id,
        KIND_DELETE,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode delete payload: {error}"))
        })?,
        description,
    )
    .await
}

/// Queue an archive (Gmail label removal or move to the Archive folder).
pub async fn enqueue_archive(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    uids: &[i64],
    description: &str,
) -> Result<()> {
    if uids.is_empty() {
        return Ok(());
    }
    let payload = ArchiveOp {
        folder_id: folder_id.to_string(),
        uids: uids.to_vec(),
        target_folder_id: None,
    };
    enqueue(
        ctx,
        account_id,
        KIND_ARCHIVE,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode archive payload: {error}"))
        })?,
        description,
    )
    .await
}

/// Queue moving messages to/from the junk folder.
pub async fn enqueue_mark_junk(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    uids: &[i64],
    value: bool,
    description: &str,
) -> Result<()> {
    let payload = MarkJunkOp {
        folder_id: folder_id.to_string(),
        uids: uids.to_vec(),
        value,
        target_folder_id: None,
    };
    enqueue(
        ctx,
        account_id,
        KIND_MARK_JUNK,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode markJunk payload: {error}"))
        })?,
        description,
    )
    .await
}

/// Queue appending a draft to the server Drafts folder.
pub async fn enqueue_append_draft(
    ctx: &CoreContext,
    account_id: &str,
    draft_id: &str,
    folder_id: &str,
    rfc822_path: &str,
) -> Result<()> {
    let payload = AppendDraftOp {
        draft_id: draft_id.to_string(),
        folder_id: folder_id.to_string(),
        rfc822_path: rfc822_path.to_string(),
    };
    enqueue(
        ctx,
        account_id,
        KIND_APPEND_DRAFT,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode appendDraft payload: {error}"))
        })?,
        "Save draft to server",
    )
    .await
}

/// Queue deletion of a previously uploaded remote draft.
pub async fn enqueue_delete_remote_draft(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    uid: i64,
) -> Result<()> {
    let payload = DeleteRemoteDraftOp {
        folder_id: folder_id.to_string(),
        uid,
    };
    enqueue(
        ctx,
        account_id,
        KIND_DELETE_REMOTE_DRAFT,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode deleteRemoteDraft payload: {error}"))
        })?,
        "Remove old remote draft",
    )
    .await
}

/// Queue appending a sent message to the Sent folder.
pub async fn enqueue_append_to_sent(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    rfc822_path: &str,
) -> Result<()> {
    let payload = AppendToSentOp {
        folder_id: folder_id.to_string(),
        rfc822_path: rfc822_path.to_string(),
    };
    enqueue(
        ctx,
        account_id,
        KIND_APPEND_TO_SENT,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode appendToSent payload: {error}"))
        })?,
        "Copy sent message to Sent",
    )
    .await
}

pub async fn enqueue_folder_create(
    ctx: &CoreContext,
    account_id: &str,
    parent_id: Option<&str>,
    name: &str,
) -> Result<()> {
    let payload = FolderCreateOp {
        parent_id: parent_id.map(str::to_string),
        name: name.to_string(),
    };
    enqueue(
        ctx,
        account_id,
        KIND_FOLDER_CREATE,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode folderCreate payload: {error}"))
        })?,
        "Create folder",
    )
    .await
}

pub async fn enqueue_folder_rename(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    new_name: &str,
) -> Result<()> {
    let payload = FolderRenameOp {
        folder_id: folder_id.to_string(),
        new_name: new_name.to_string(),
    };
    enqueue(
        ctx,
        account_id,
        KIND_FOLDER_RENAME,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode folderRename payload: {error}"))
        })?,
        "Rename folder",
    )
    .await
}

pub async fn enqueue_folder_delete(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
) -> Result<()> {
    let payload = FolderDeleteOp {
        folder_id: folder_id.to_string(),
    };
    enqueue(
        ctx,
        account_id,
        KIND_FOLDER_DELETE,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode folderDelete payload: {error}"))
        })?,
        "Delete folder",
    )
    .await
}

pub async fn enqueue_folder_subscribe(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    subscribe: bool,
) -> Result<()> {
    let payload = FolderSubscribeOp {
        folder_id: folder_id.to_string(),
        subscribe,
    };
    enqueue(
        ctx,
        account_id,
        KIND_FOLDER_SUBSCRIBE,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode folderSubscribe payload: {error}"))
        })?,
        if subscribe {
            "Subscribe folder"
        } else {
            "Unsubscribe folder"
        },
    )
    .await
}

pub async fn enqueue_folder_mark_read(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
) -> Result<()> {
    let payload = FolderMarkReadOp {
        folder_id: folder_id.to_string(),
    };
    enqueue(
        ctx,
        account_id,
        KIND_FOLDER_MARK_READ,
        serde_json::to_value(payload).map_err(|error| {
            AppError::internal(format!("cannot encode folderMarkRead payload: {error}"))
        })?,
        "Mark folder as read",
    )
    .await
}

pub async fn enqueue_send(ctx: &CoreContext, account_id: &str, outbox_id: &str) -> Result<()> {
    enqueue(
        ctx,
        account_id,
        KIND_SEND,
        json!({ "outboxId": outbox_id }),
        "Send message",
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::models::{AccountState, ProviderKind};

    async fn test_ctx() -> CoreContext {
        let dir =
            std::env::temp_dir().join(format!("nuncio-mail-ops-{}", nuncio_core::models::new_id()));
        CoreContext::for_test(nuncio_core::Paths::new(dir))
            .await
            .expect("context")
    }

    async fn account(ctx: &CoreContext) -> nuncio_core::models::Account {
        let now = nuncio_core::models::now();
        let account = nuncio_core::models::Account {
            id: "acc".into(),
            provider: ProviderKind::Imap,
            email: "me@example.com".into(),
            display_name: "Me".into(),
            color: "#000000".into(),
            auth_kind: nuncio_core::models::AuthKind::Password,
            oauth_provider: None,
            imap: None,
            smtp: None,
            pop3: None,
            caldav_url: None,
            carddav_url: None,
            sync_window_days: 90,
            mail_enabled: true,
            calendar_enabled: false,
            contacts_enabled: false,
            tasks_enabled: false,
            notifications_enabled: true,
            include_in_unified: true,
            sort_order: 0,
            state: AccountState::Ok,
            state_message: None,
            capabilities: Default::default(),
            created_at: now,
        };
        ctx.db
            .write({
                let account = account.clone();
                move |db| repo::accounts::insert(db, &account)
            })
            .await
            .expect("account insert");
        account
    }

    #[tokio::test]
    async fn enqueues_typed_payloads() {
        let ctx = test_ctx().await;
        let account = account(&ctx).await;
        enqueue_set_flags(
            &ctx,
            &account.id,
            "folder",
            &[1, 2],
            Some(true),
            None,
            None,
            None,
            "read",
        )
        .await
        .expect("queue");
        enqueue_move(&ctx, &account.id, "a", "b", &[3], "move")
            .await
            .expect("queue");
        let ops = ctx
            .db
            .read(move |db| repo::pending_ops::list(db, Some("acc")))
            .await
            .expect("list");
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].kind, KIND_SET_FLAGS);
        let payload: SetFlagsOp = repo::pending_ops::payload(&ops[0]).expect("payload");
        assert_eq!(payload.uids, vec![1, 2]);
        assert_eq!(payload.seen, Some(true));
    }

    #[test]
    fn empty_flag_updates_are_not_queued() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let ctx = test_ctx().await;
            let account = account(&ctx).await;
            enqueue_set_flags(&ctx, &account.id, "f", &[], None, None, None, None, "noop")
                .await
                .expect("noop");
            let count = ctx
                .db
                .read(move |db| repo::pending_ops::pending_count(db, Some("acc")))
                .await
                .expect("count");
            assert_eq!(count, 0);
        });
    }
}
