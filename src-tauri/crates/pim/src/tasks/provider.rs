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
    /// Provider-reported built-in marker (`none`, `defaultList`,
    /// `flaggedEmails`). Built-in lists cannot be renamed or deleted.
    pub well_known_list_name: String,
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
            well_known_list_name: nuncio_core::models::WELL_KNOWN_NONE.to_string(),
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
    /// `remote_id` was derived from the href because the VTODO has no UID
    /// (CalDAV). It is a placeholder, not a real provider UID, and must never
    /// be matched against a real UID of another resource.
    pub uid_is_derived: bool,
}

/// A batch of task changes.
#[derive(Debug, Clone, Default)]
pub struct TaskDelta {
    pub tasks: Vec<RemoteTask>,
    pub deleted_remote_ids: Vec<String>,
    pub deleted_hrefs: Vec<String>,
    /// Every href the server listed in this pass, including objects whose ETag
    /// matched the local baseline and were therefore not downloaded.
    ///
    /// Only a full collection listing can supply this (the DAV ETag fallback);
    /// an incremental delta leaves it empty. A full-sync prune must treat a
    /// listed href as present even when it is absent from [`Self::tasks`].
    pub listed_hrefs: Vec<String>,
    /// A listed object's identity could not be read this round. The object is
    /// present but unrefreshed: do not prune and do not advance the cursor.
    pub incomplete: bool,
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
    /// `remote_id` is a href-derived placeholder, not a real provider UID.
    /// A provider that rewrites the object must not write the placeholder into
    /// the VTODO as a real `UID`: it can collide with a genuine UID of the same
    /// spelling. The rewrite instead receives a freshly assigned real UID.
    pub uid_is_derived: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteWriteResult {
    pub remote_id: Option<String>,
    pub etag: Option<String>,
    pub href: Option<String>,
    /// `remote_id` is a href-derived placeholder (the written object has no
    /// real UID). Persisted so a later delete tombstone matches only the same
    /// kind of identity.
    pub uid_is_derived: bool,
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

    /// The identity a create of `task_id` will use when the client chooses
    /// it, so a delete racing an in-flight create can suppress the object
    /// before its receipt arrives.
    ///
    /// Providers that allocate ids server-side return `None`: an unacknowledged
    /// create cannot be attributed in advance. The sync engine then isolates
    /// the list instead of guessing.
    fn predicted_create_identity(
        &self,
        _list: &RemoteTaskList,
        _task_id: &str,
    ) -> Option<RemoteTaskRef> {
        None
    }

    /// Rewrite an existing task.
    ///
    /// `assigned_uid` is a real UID the service already persisted for a task
    /// whose stored identity was derived from its href. Providers that address
    /// objects by a real UID must write this value instead of the placeholder;
    /// providers with provider-side ids ignore it.
    async fn update_task(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        current: &RemoteTaskRef,
        task: &Task,
        assigned_uid: Option<&str>,
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
    /// Create a subtask. `previous` is the sibling it should follow, for
    /// providers whose API takes an explicit position (Google Tasks).
    async fn create_step(
        &self,
        _account: &Account,
        _list: &RemoteTaskList,
        _parent: &RemoteTaskRef,
        _previous: Option<&str>,
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
