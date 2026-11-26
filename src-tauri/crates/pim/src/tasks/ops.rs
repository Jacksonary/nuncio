//! Serializable payloads for `pim.task.*` pending operations.

use nuncio_core::models::{PendingOp as CorePendingOp, Task, TaskList};
use serde::{Deserialize, Serialize};

use crate::error::{internal, Result};

pub const KIND_PREFIX: &str = "pim.task.";

pub const KIND_SAVE: &str = "pim.task.save";
pub const KIND_DELETE: &str = "pim.task.delete";
pub const KIND_MOVE: &str = "pim.task.move";
pub const KIND_REORDER: &str = "pim.task.reorder";
pub const KIND_LIST_CREATE: &str = "pim.task.list.create";
pub const KIND_LIST_UPDATE: &str = "pim.task.list.update";
pub const KIND_LIST_DELETE: &str = "pim.task.list.delete";

/// A task plus its provider sync metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSnapshot {
    pub task: Task,
    pub etag: Option<String>,
    pub remote_id: Option<String>,
    pub href: Option<String>,
    /// `remote_id` was derived from the href rather than being a real provider
    /// UID. A snapshot written by an older build defaults to a real UID, the
    /// same rule the task row and tombstone columns use.
    #[serde(default)]
    pub uid_is_derived: bool,
}

/// A task list with every task it contained (for delete rollback).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskListSnapshot {
    pub list: TaskList,
    pub provider: Option<String>,
    pub remote_id: Option<String>,
    pub href: Option<String>,
    pub tasks: Vec<TaskSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum TaskOp {
    Save {
        task_id: String,
        /// A real provider UID assigned to a task whose local identity was
        /// derived from its href (`uid_is_derived`).
        ///
        /// Generated and persisted **before** the first wire attempt so a retry
        /// reuses the same value instead of writing a different UID on every
        /// attempt. `None` until the executor needs one (and forever for tasks
        /// that already carry a real UID).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        assigned_uid: Option<String>,
        /// `None` for a create (rollback deletes the row).
        snapshot: Option<TaskSnapshot>,
    },
    Delete {
        snapshot: TaskSnapshot,
    },
    Move {
        task_id: String,
        from_list_id: String,
        snapshot: TaskSnapshot,
    },
    /// Push the local custom order to providers that support it (Google Tasks).
    Reorder {
        list_id: String,
        ids: Vec<String>,
    },
    ListCreate {
        list_id: String,
    },
    ListUpdate {
        list_id: String,
        previous_name: String,
        previous_color: String,
    },
    ListDelete {
        snapshot: TaskListSnapshot,
    },
}

impl TaskOp {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Save { .. } => KIND_SAVE,
            Self::Delete { .. } => KIND_DELETE,
            Self::Move { .. } => KIND_MOVE,
            Self::Reorder { .. } => KIND_REORDER,
            Self::ListCreate { .. } => KIND_LIST_CREATE,
            Self::ListUpdate { .. } => KIND_LIST_UPDATE,
            Self::ListDelete { .. } => KIND_LIST_DELETE,
        }
    }

    pub fn description(&self, label: &str) -> String {
        match self {
            Self::Save { snapshot: None, .. } => format!("创建任务「{label}」"),
            Self::Save { .. } => format!("更新任务「{label}」"),
            Self::Delete { .. } => format!("删除任务「{label}」"),
            Self::Move { .. } => format!("移动任务「{label}」"),
            Self::Reorder { .. } => "调整任务顺序".to_string(),
            Self::ListCreate { .. } => "创建任务列表".to_string(),
            Self::ListUpdate { .. } => "更新任务列表".to_string(),
            Self::ListDelete { .. } => "删除任务列表".to_string(),
        }
    }
}

pub fn decode(op: &CorePendingOp) -> Result<TaskOp> {
    serde_json::from_value(op.payload.clone())
        .map_err(|error| internal(format!("invalid task pending op payload: {error}")))
}

pub fn to_value(op: &TaskOp) -> Result<serde_json::Value> {
    serde_json::to_value(op)
        .map_err(|error| internal(format!("cannot encode task pending op: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let op = TaskOp::ListCreate {
            list_id: "list-1".into(),
        };
        let value = to_value(&op).unwrap();
        assert_eq!(value["op"], "listCreate");
        let decoded: TaskOp = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.kind(), KIND_LIST_CREATE);
        assert!(decoded.description("x").contains("任务列表"));
    }

    #[test]
    fn kinds_share_prefix() {
        let kinds = [
            KIND_SAVE,
            KIND_DELETE,
            KIND_MOVE,
            KIND_REORDER,
            KIND_LIST_CREATE,
            KIND_LIST_UPDATE,
            KIND_LIST_DELETE,
        ];
        assert!(kinds.iter().all(|kind| kind.starts_with(KIND_PREFIX)));
    }
}
