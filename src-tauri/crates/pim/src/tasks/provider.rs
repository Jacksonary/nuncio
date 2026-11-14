//! Provider-neutral task abstraction.
//!
//! Google Tasks, Microsoft Graph To Do and CalDAV VTODO differ a lot in what
//! they support; [`TaskCapabilities`] tells the sync engine which local-only
//! fields must be preserved when a remote task is applied.

use async_trait::async_trait;
use nuncio_core::models::{Account, Task, TaskStep};

use crate::error::Result;

/// A provider-side task list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTaskList {
    pub local_id: String,
    /// Google/Graph list id or the absolute CalDAV collection URL.
    pub remote_id: String,
    pub href: Option<String>,
    pub name: String,
    pub color: Option<String>,
    pub read_only: bool,
}

impl RemoteTaskList {
    pub fn new(remote_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            local_id: String::new(),
            remote_id: remote_id.into(),
            href: None,
            name: name.into(),
            color: None,
            read_only: false,
        }
    }
}

/// Incremental cursor for one task list.
#[derive(Debug, Clone, Default)]
pub struct TaskCursor {
    pub local_list_id: String,
    /// Google `updatedMin` (the next window start), Graph deltaLink or DAV
    /// sync token.
    pub sync_token: Option<String>,
    pub ctag: Option<String>,
    pub etags: Vec<(String, String)>,
}

/// One task fetched from a provider.
#[derive(Debug, Clone)]
pub struct RemoteTask {
    pub task: Task,
    pub remote_id: Option<String>,
    pub href: Option<String>,
    pub etag: Option<String>,
    /// Google subtasks: provider id of the parent task.
    pub parent_remote_id: Option<String>,
    /// `false` when the provider could not return the complete step set in
    /// this batch (the sync engine then keeps local steps).
    pub steps_complete: bool,
}

/// A batch of task changes.
#[derive(Debug, Clone, Default)]
pub struct TaskDelta {
    pub tasks: Vec<RemoteTask>,
    pub deleted_remote_ids: Vec<String>,
    pub deleted_hrefs: Vec<String>,
    pub sync_token: Option<String>,
    pub ctag: Option<String>,
    pub full_sync: bool,
}

/// Identity needed to address a stored task on the provider.
#[derive(Debug, Clone, Default)]
pub struct RemoteTaskRef {
    pub remote_id: Option<String>,
    pub href: Option<String>,
    pub etag: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteWriteResult {
    pub remote_id: Option<String>,
    pub etag: Option<String>,
    pub href: Option<String>,
}

/// Which fields the provider can actually store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskCapabilities {
    pub importance: bool,
    pub reminders: bool,
    pub steps: bool,
    pub recurrence: bool,
    /// `true` when the provider stores a manual ordering we can read and push
    /// (Google Tasks `position`); otherwise the local `sort_order` wins.
    pub ordering: bool,
    /// `true` when the server itself creates the next occurrence of a
    /// completed recurring task (Microsoft To Do).
    pub server_recurring_completion: bool,
}

impl TaskCapabilities {
    pub fn all() -> Self {
        Self {
            importance: true,
            reminders: true,
            steps: true,
            recurrence: true,
            ordering: true,
            server_recurring_completion: false,
        }
    }

    pub fn local() -> Self {
        Self {
            server_recurring_completion: false,
            ..Self::all()
        }
    }
}

/// Task backend.
#[async_trait]
pub trait TaskProvider: Send + Sync {
    fn name(&self) -> &'static str;

    fn capabilities(&self) -> TaskCapabilities;

    async fn list_task_lists(&self, account: &Account) -> Result<Vec<RemoteTaskList>>;

    async fn create_task_list(
        &self,
        account: &Account,
        name: &str,
        color: &str,
    ) -> Result<RemoteTaskList>;

    async fn update_task_list(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        name: Option<&str>,
        color: Option<&str>,
    ) -> Result<()>;

    async fn delete_task_list(&self, account: &Account, list: &RemoteTaskList) -> Result<()>;

    async fn sync_tasks(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        cursor: &TaskCursor,
    ) -> Result<TaskDelta>;

    async fn create_task(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        task: &Task,
    ) -> Result<RemoteWriteResult>;

    async fn update_task(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        current: &RemoteTaskRef,
        task: &Task,
    ) -> Result<RemoteWriteResult>;

    async fn delete_task(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        current: &RemoteTaskRef,
    ) -> Result<()>;

    /// Move between lists.  The default implementation creates a copy in the
    /// target list and deletes the source (Graph, CalDAV); Google overrides it
    /// with `tasks.move`.
    async fn move_task(
        &self,
        account: &Account,
        from: &RemoteTaskList,
        to: &RemoteTaskList,
        current: &RemoteTaskRef,
        task: &Task,
    ) -> Result<RemoteWriteResult> {
        let created = self.create_task(account, to, task).await?;
        self.delete_task(account, from, current).await?;
        Ok(created)
    }

    /// Push a manual ordering.  `remote_ids` is in the desired order.  The
    /// default is a no-op for providers without server-side ordering.
    async fn reorder_tasks(
        &self,
        _account: &Account,
        _list: &RemoteTaskList,
        _remote_ids: &[String],
    ) -> Result<()> {
        Ok(())
    }

    /// Create a sub-step on the provider (Google subtask, Graph checklist
    /// item).  `None` means the provider stores steps inside the task payload
    /// (CalDAV).
    async fn create_step(
        &self,
        _account: &Account,
        _list: &RemoteTaskList,
        _parent: &RemoteTaskRef,
        _step: &TaskStep,
    ) -> Result<Option<RemoteWriteResult>> {
        Ok(None)
    }

    async fn update_step(
        &self,
        _account: &Account,
        _list: &RemoteTaskList,
        _parent: &RemoteTaskRef,
        _remote_step_id: &str,
        _step: &TaskStep,
    ) -> Result<()> {
        Ok(())
    }

    async fn delete_step(
        &self,
        _account: &Account,
        _list: &RemoteTaskList,
        _parent: &RemoteTaskRef,
        _remote_step_id: &str,
    ) -> Result<()> {
        Ok(())
    }
}
