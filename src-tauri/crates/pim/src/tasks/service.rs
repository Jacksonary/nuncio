//! Task sync engine, command facade and pending-operation executor.

use base64::Engine as _;
use chrono::Utc;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use nuncio_core::events::{emit_mail_changed, emit_tasks_changed, MailChangeKind};
use nuncio_core::models::{
    new_id, Account, Importance, NewPendingOp, NewTaskFromMessage, OAuthProvider, PendingOp,
    PendingOpState, SmartListCounts, SyncModule, Task, TaskAttachment, TaskInput, TaskList,
    TaskQuery, TaskStatus, TaskStep,
};
use nuncio_core::providers::DavService;
use nuncio_core::repo::{
    self, messages as messages_repo, sync_state, task_lists, tasks as tasks_repo,
};
use nuncio_core::CoreContext;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::backend::{backend_for, is_implicit_dav_auth_failure, Backend};
use crate::error::{invalid, unsupported, AppError, Result};
use crate::http::HttpClient;
use crate::op_worker::{self, PimOpExecutor};
use crate::status::record_failure;
use crate::store::{self, TaskListRemote};
use crate::tasks::dav::CalDavTaskProvider;
use crate::tasks::google::GoogleTasksProvider;
use crate::tasks::graph::{GraphTasksClient, GraphTasksProvider};
use crate::tasks::map::{self, next_occurrence, sort_for_list, today};
use crate::tasks::ops::{self, TaskListSnapshot, TaskOp, TaskSnapshot};
use crate::tasks::provider::{
    RemoteTask, RemoteTaskList, RemoteTaskRef, TaskCapabilities, TaskCursor, TaskDelta,
    TaskProvider,
};

/// `sync_state.module` used for task cursors.
pub const SYNC_MODULE: &str = "tasks";

/// Task service handle shared by `PimService`.
#[derive(Clone)]
pub struct TasksService {
    ctx: CoreContext,
    http: HttpClient,
    notify: Arc<Notify>,
    shutdown: Arc<AtomicBool>,
    worker: Arc<Mutex<Option<JoinHandle<()>>>>,
    locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl std::fmt::Debug for TasksService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TasksService").finish_non_exhaustive()
    }
}

impl TasksService {
    pub fn new(ctx: CoreContext, http: HttpClient) -> Self {
        Self {
            ctx,
            http,
            notify: Arc::new(Notify::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            worker: Arc::new(Mutex::new(None)),
            locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn context(&self) -> &CoreContext {
        &self.ctx
    }

    pub fn notify(&self) {
        self.notify.notify_waiters();
    }

    pub fn start(&self) {
        if self
            .worker
            .lock()
            .map(|slot| slot.is_some())
            .unwrap_or(false)
        {
            return;
        }
        self.shutdown.store(false, Ordering::SeqCst);
        let _ = op_worker::recover_interrupted(&self.ctx, ops::KIND_PREFIX);
        let executor: Arc<dyn PimOpExecutor> = Arc::new(self.clone());
        let ctx = self.ctx.clone();
        let notify = self.notify.clone();
        let shutdown = self.shutdown.clone();
        let handle = tokio::spawn(async move {
            op_worker::run_worker(ctx, notify, shutdown, executor).await;
        });
        if let Ok(mut slot) = self.worker.lock() {
            *slot = Some(handle);
        }
    }

    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
        if let Ok(mut slot) = self.worker.lock() {
            if let Some(handle) = slot.take() {
                handle.abort();
            }
        }
    }

    fn provider_for(&self, account: &Account) -> Option<Box<dyn TaskProvider>> {
        match backend_for(account, DavService::Calendar) {
            Backend::Google => Some(Box::new(GoogleTasksProvider::new(self.http.clone()))),
            Backend::Graph => Some(Box::new(GraphTasksProvider::new(self.http.clone()))),
            Backend::Dav => Some(Box::new(CalDavTaskProvider::new(self.http.clone()))),
            Backend::Local | Backend::Unsupported => None,
        }
    }

    fn is_provider(&self, account: &Account) -> bool {
        matches!(
            backend_for(account, DavService::Calendar),
            Backend::Google | Backend::Graph | Backend::Dav
        )
    }

    /* ───────────────────────────── sync ───────────────────────────── */

    pub async fn sync_account(&self, account_id: &str, _force: bool) -> Result<()> {
        let lock = {
            let mut locks = self
                .locks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            locks
                .entry(account_id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        let account = self
            .ctx
            .db
            .read({
                let id = account_id.to_string();
                move |conn| repo::accounts::get_required(conn, &id)
            })
            .await?;
        if !account.tasks_enabled {
            return Ok(());
        }
        self.ctx
            .sync_status
            .begin(&account.id, SyncModule::Tasks, None);
        match self.sync_account_inner(&account).await {
            Ok(()) => {
                self.ctx.sync_status.finish(&account.id, SyncModule::Tasks);
                Ok(())
            }
            Err(error) => {
                // CalDAV task lists use the Calendar DAV service.  An implicit
                // configuration rejecting the mail password is expected.
                if matches!(error, AppError::Auth(_))
                    && backend_for(&account, DavService::Calendar) == Backend::Dav
                {
                    let dedicated = self
                        .ctx
                        .creds
                        .has_dav_password(&account.id, DavService::Calendar)
                        .await;
                    if is_implicit_dav_auth_failure(
                        &account,
                        DavService::Calendar,
                        dedicated,
                        &error,
                    ) {
                        debug!(
                            account_id = %account.id,
                            "tasks needs its own password for this provider; set it in account settings"
                        );
                        self.ctx.sync_status.finish(&account.id, SyncModule::Tasks);
                        return Ok(());
                    }
                }
                record_failure(
                    &self.ctx.sync_status,
                    &account.id,
                    SyncModule::Tasks,
                    &error,
                );
                Err(error)
            }
        }
    }

    async fn sync_account_inner(&self, account: &Account) -> Result<()> {
        let Some(provider) = self.provider_for(account) else {
            return Ok(());
        };
        let provider_name = provider.name().to_string();
        let remotes = provider.list_task_lists(account).await?;
        let (lists, lists_changed) = self
            .upsert_remote_lists(account, &provider_name, remotes)
            .await?;
        let mut changed: Vec<String> = Vec::new();
        if lists_changed {
            changed.extend(lists.iter().map(|(list, _)| list.id.clone()));
        }
        let caps = provider.capabilities();
        let mut first_error = None;
        for (list, remote) in &lists {
            match self
                .sync_one_list(account, provider.as_ref(), caps, list, remote)
                .await
            {
                Ok(true) => changed.push(list.id.clone()),
                Ok(false) => {}
                Err(error) => {
                    warn!(list = %list.id, %error, "task list sync failed");
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        if !changed.is_empty() {
            changed.sort();
            changed.dedup();
            emit_tasks_changed(self.ctx.events.as_ref(), changed);
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn upsert_remote_lists(
        &self,
        account: &Account,
        provider_name: &str,
        remotes: Vec<RemoteTaskList>,
    ) -> Result<(Vec<(TaskList, RemoteTaskList)>, bool)> {
        let account_id = account.id.clone();
        let provider_name = provider_name.to_string();
        self.ctx
            .db
            .write(move |conn| {
                let existing: Vec<TaskListRemote> = store::list_task_lists_for_account(conn, &account_id)?
                    .into_iter()
                    .filter(|mapping| mapping.provider == provider_name)
                    .collect();
                let remote_ids: HashSet<String> =
                    remotes.iter().map(|remote| remote.remote_id.clone()).collect();
                let mut changed = false;
                let mut pairs = Vec::new();

                for mut remote in remotes {
                    let mapping = store::get_task_list_by_remote(
                        conn,
                        &account_id,
                        &provider_name,
                        &remote.remote_id,
                    )?;
                    match mapping {
                        Some(mapping) => match task_lists::get(conn, &mapping.task_list_id)? {
                            Some(mut list) => {
                                let name = if remote.name.trim().is_empty() {
                                    list.name.clone()
                                } else {
                                    remote.name.clone()
                                };
                                let color = remote
                                    .color
                                    .clone()
                                    .unwrap_or_else(|| list.color.clone());
                                if list.name != name || list.color != color {
                                    list = task_lists::update(conn, &list.id, Some(&name), Some(&color))?;
                                    changed = true;
                                }
                                if list.is_read_only != remote.read_only {
                                    conn.execute(
                                        "UPDATE task_list SET is_read_only = ?2, updated_at = ?3 WHERE id = ?1",
                                        rusqlite::params![list.id, remote.read_only as i64, repo::now_string()],
                                    )?;
                                    list.is_read_only = remote.read_only;
                                    changed = true;
                                }
                                if list.well_known_list_name != remote.well_known_list_name
                                    || (remote.well_known_list_name
                                        == nuncio_core::models::WELL_KNOWN_DEFAULT
                                        && !list.is_default)
                                {
                                    apply_well_known(conn, &list.id, &remote.well_known_list_name)?;
                                    list = task_lists::get_required(conn, &list.id)?;
                                    changed = true;
                                }
                                if mapping.href != remote.href {
                                    store::set_task_list(
                                        conn,
                                        &TaskListRemote {
                                            href: remote.href.clone(),
                                            ..mapping.clone()
                                        },
                                    )?;
                                }
                                remote.local_id = list.id.clone();
                                pairs.push((list, remote));
                            }
                            None => {
                                store::delete_task_list(conn, &mapping.task_list_id)?;
                                let list = create_remote_list(conn, &account_id, &remote)?;
                                store::set_task_list(
                                    conn,
                                    &TaskListRemote::new(
                                        list.id.clone(),
                                        &account_id,
                                        &provider_name,
                                        &remote.remote_id,
                                        remote.href.clone(),
                                    ),
                                )?;
                                remote.local_id = list.id.clone();
                                changed = true;
                                pairs.push((list, remote));
                            }
                        },
                        None => {
                            let list = create_remote_list(conn, &account_id, &remote)?;
                            store::set_task_list(
                                conn,
                                &TaskListRemote::new(
                                    list.id.clone(),
                                    &account_id,
                                    &provider_name,
                                    &remote.remote_id,
                                    remote.href.clone(),
                                ),
                            )?;
                            remote.local_id = list.id.clone();
                            changed = true;
                            pairs.push((list, remote));
                        }
                    }
                }

                for mapping in existing {
                    if !remote_ids.contains(&mapping.remote_id) {
                        if let Err(error) = task_lists::delete(conn, &mapping.task_list_id) {
                            if !error.is_not_found() {
                                return Err(error);
                            }
                        }
                        changed = true;
                    }
                }
                Ok((pairs, changed))
            })
            .await
    }

    async fn sync_one_list(
        &self,
        account: &Account,
        provider: &dyn TaskProvider,
        caps: TaskCapabilities,
        list: &TaskList,
        remote: &RemoteTaskList,
    ) -> Result<bool> {
        let list_id = list.id.clone();
        let state = self
            .ctx
            .db
            .read({
                let list_id = list_id.clone();
                move |conn| load_cursor(conn, &list_id)
            })
            .await?;
        let etags = if provider.name() == "caldav" {
            self.ctx
                .db
                .read({
                    let list_id = list_id.clone();
                    move |conn| store::task_etags(conn, &list_id)
                })
                .await?
        } else {
            Vec::new()
        };
        let cursor = TaskCursor {
            local_list_id: list.id.clone(),
            sync_token: state.as_ref().and_then(|state| state.sync_token.clone()),
            ctag: state.as_ref().and_then(|state| state.ctag.clone()),
            etags,
        };
        let delta = provider.sync_tasks(account, remote, &cursor).await?;
        let changed = delta.full_sync
            || !delta.tasks.is_empty()
            || !delta.deleted_remote_ids.is_empty()
            || !delta.deleted_hrefs.is_empty();
        let account_id = account.id.clone();
        let list_clone = list.clone();
        self.ctx
            .db
            .write(move |conn| commit_delta(conn, &account_id, &list_clone, caps, &delta))
            .await?;
        Ok(changed)
    }

    /* ──────────────────────── command facade ──────────────────────── */

    pub async fn tasklist_list(&self) -> Result<Vec<TaskList>> {
        self.ctx.db.read(task_lists::list).await
    }

    pub async fn tasklist_create(
        &self,
        account_id: &str,
        name: &str,
        color: &str,
    ) -> Result<TaskList> {
        let name = name.trim();
        if name.is_empty() {
            return Err(invalid("task list name must not be empty"));
        }
        let color = normalize_color_or_default(color);
        let account = self.load_account(account_id).await?;
        if backend_for(&account, DavService::Calendar) == Backend::Unsupported {
            return Err(unsupported("this account has no task provider"));
        }
        let account_id = account.id.clone();
        let name = name.to_string();
        let list = self
            .ctx
            .db
            .write(move |conn| task_lists::create(conn, &account_id, &name, &color))
            .await?;
        if self.is_provider(&account) {
            let op = TaskOp::ListCreate {
                list_id: list.id.clone(),
            };
            self.enqueue_op(&account.id, &op, format!("创建任务列表「{}」", list.name))
                .await?;
        }
        emit_tasks_changed(self.ctx.events.as_ref(), vec![list.id.clone()]);
        Ok(list)
    }

    pub async fn tasklist_update(
        &self,
        id: &str,
        name: Option<&str>,
        color: Option<&str>,
    ) -> Result<TaskList> {
        if let Some(name) = name {
            if name.trim().is_empty() {
                return Err(invalid("task list name must not be empty"));
            }
        }
        let color = color.map(normalize_color_or_default);
        let name = name.map(str::to_string);
        let id = id.to_string();
        // The provider refuses to rename its own lists, and the failure only
        // surfaces after the local row was already changed. Reject it here so
        // the user sees one clear error instead of a success followed by a
        // rollback.
        {
            let id = id.clone();
            let existing = self
                .ctx
                .db
                .read(move |conn| {
                    let list = task_lists::get_required(conn, &id)?;
                    Ok((list.is_builtin(), list.name))
                })
                .await?;
            let (builtin, current_name) = existing;
            if builtin
                && name
                    .as_ref()
                    .is_some_and(|requested| requested != &current_name)
            {
                return Err(invalid(
                    "built-in task lists cannot be renamed; the provider owns their name",
                ));
            }
        }
        let (previous, updated) = self
            .ctx
            .db
            .write(move |conn| {
                let previous = task_lists::get_required(conn, &id)?;
                let updated = task_lists::update(conn, &id, name.as_deref(), color.as_deref())?;
                Ok((previous, updated))
            })
            .await?;
        let account = self.load_account(&updated.account_id).await?;
        if self.is_provider(&account) {
            let op = TaskOp::ListUpdate {
                list_id: updated.id.clone(),
                previous_name: previous.name,
                previous_color: previous.color,
            };
            self.enqueue_op(
                &account.id,
                &op,
                format!("更新任务列表「{}」", updated.name),
            )
            .await?;
        }
        emit_tasks_changed(self.ctx.events.as_ref(), vec![updated.id.clone()]);
        Ok(updated)
    }

    pub async fn tasklist_delete(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        {
            let id = id.clone();
            let builtin = self
                .ctx
                .db
                .read(move |conn| {
                    let list = task_lists::get_required(conn, &id)?;
                    Ok(list.is_builtin())
                })
                .await?;
            if builtin {
                return Err(invalid(
                    "built-in task lists cannot be deleted; the provider owns them",
                ));
            }
        }
        let id_for_db = id.clone();
        let (snapshot, account_id) = self
            .ctx
            .db
            .write(move |conn| {
                let list = task_lists::get_required(conn, &id_for_db)?;
                let mapping = store::get_task_list_by_local(conn, &id_for_db)?;
                let tasks = tasks_repo::query(
                    conn,
                    &TaskQuery {
                        list_id: Some(id_for_db.clone()),
                        smart: None,
                        include_completed: true,
                        today: map::today(),
                    },
                )?;
                let mut snapshots = Vec::new();
                for task in tasks {
                    if let Some(snapshot) = snapshot_task(conn, &task.id)? {
                        snapshots.push(snapshot);
                    }
                }
                let snapshot = TaskListSnapshot {
                    list: list.clone(),
                    provider: mapping.as_ref().map(|m| m.provider.clone()),
                    remote_id: mapping.as_ref().map(|m| m.remote_id.clone()),
                    href: mapping.as_ref().and_then(|m| m.href.clone()),
                    tasks: snapshots,
                };
                let account_id = list.account_id.clone();
                task_lists::delete(conn, &id_for_db)?;
                Ok((snapshot, account_id))
            })
            .await?;
        let account = self.load_account(&account_id).await?;
        if self.is_provider(&account) && snapshot.remote_id.is_some() {
            let op = TaskOp::ListDelete { snapshot };
            self.enqueue_op(&account.id, &op, "删除任务列表".to_string())
                .await?;
        }
        emit_tasks_changed(self.ctx.events.as_ref(), vec![id]);
        Ok(())
    }

    pub async fn task_smart_counts(&self, today: String) -> Result<SmartListCounts> {
        self.ctx
            .db
            .read(move |conn| tasks_repo::smart_counts(conn, &today))
            .await
    }

    pub async fn task_query(&self, query: TaskQuery) -> Result<Vec<Task>> {
        let is_list = query.smart.is_none() && query.list_id.is_some();
        let mut tasks = self
            .ctx
            .db
            .read(move |conn| tasks_repo::query(conn, &query))
            .await?;
        if is_list {
            sort_for_list(&mut tasks);
        }
        self.prune_missing_links(&mut tasks).await?;
        Ok(tasks)
    }

    /* ───────────────────────── task attachments ───────────────────── */

    /// Resolve what the Graph attachment endpoints need: the account, the
    /// remote list id and the remote task id.
    async fn attachment_scope(&self, task_id: &str) -> Result<(Account, String, String)> {
        let id = task_id.to_string();
        let task = self
            .ctx
            .db
            .read(move |conn| tasks_repo::get_required(conn, &id))
            .await?;
        let account = self.load_account(&task.account_id).await?;
        if account.oauth_provider != Some(OAuthProvider::Microsoft) {
            return Err(unsupported(
                "task attachments are only supported for Microsoft accounts",
            ));
        }
        let Some(mapping) = self.list_mapping(&task.list_id).await? else {
            return Err(AppError::conflict("task list is not mapped yet"));
        };
        let Some(snapshot) = self.snapshot_task(task_id).await? else {
            return Err(AppError::not_found(format!("task {task_id} not found")));
        };
        let Some(remote_id) = snapshot.remote_id else {
            return Err(AppError::conflict(
                "the task has not been uploaded yet; sync it first",
            ));
        };
        Ok((account, mapping.remote_id, remote_id))
    }

    pub async fn task_attachments(&self, task_id: &str) -> Result<Vec<TaskAttachment>> {
        let (account, list_remote, task_remote) = self.attachment_scope(task_id).await?;
        let client = GraphTasksClient::new(self.http.clone());
        let items = client
            .list_task_attachments(&account.id, &list_remote, &task_remote)
            .await?;
        Ok(items
            .into_iter()
            .map(|item| {
                let content_type = item
                    .content_type
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| attachment_content_type(&item.name).to_string());
                TaskAttachment {
                    id: item.id,
                    name: item.name,
                    size: item.size,
                    content_type,
                }
            })
            .collect())
    }

    pub async fn task_attachment_add(
        &self,
        task_id: &str,
        path: &str,
    ) -> Result<Vec<TaskAttachment>> {
        let (account, list_remote, task_remote) = self.attachment_scope(task_id).await?;
        // Graph accepts inline content only below 3 MB; larger files need an
        // upload session, which this client does not implement yet. Check the
        // size before reading, or a picked 2 GB file would be loaded into
        // memory only to be rejected.
        const LIMIT: u64 = 3 * 1024 * 1024;
        let size = tokio::fs::metadata(path)
            .await
            .map_err(|error| AppError::io(format!("cannot read {path}: {error}")))?
            .len();
        if size >= LIMIT {
            return Err(invalid(format!(
                "attachment is too large ({:.1} MB, limit {} MB)",
                size as f64 / (1024.0 * 1024.0),
                LIMIT / (1024 * 1024)
            )));
        }
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|error| AppError::io(format!("cannot read {path}: {error}")))?;
        let name = path
            .rsplit(['/', '\\'])
            .next()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("attachment")
            .to_string();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let client = GraphTasksClient::new(self.http.clone());
        client
            .create_task_attachment(
                &account.id,
                &list_remote,
                &task_remote,
                &name,
                attachment_content_type(&name),
                &encoded,
            )
            .await?;
        self.task_attachments(task_id).await
    }

    pub async fn task_attachment_remove(&self, task_id: &str, attachment_id: &str) -> Result<()> {
        let (account, list_remote, task_remote) = self.attachment_scope(task_id).await?;
        let client = GraphTasksClient::new(self.http.clone());
        client
            .delete_task_attachment(&account.id, &list_remote, &task_remote, attachment_id)
            .await
    }

    /// Download one attachment into the local cache and return its path, so
    /// the command layer can hand it to the OS opener.
    /// Download one attachment: its display name, content type and bytes.
    async fn attachment_payload(
        &self,
        task_id: &str,
        attachment_id: &str,
    ) -> Result<(String, String, Vec<u8>)> {
        let (account, list_remote, task_remote) = self.attachment_scope(task_id).await?;
        let client = GraphTasksClient::new(self.http.clone());
        let meta = client
            .list_task_attachments(&account.id, &list_remote, &task_remote)
            .await?
            .into_iter()
            .find(|item| item.id == attachment_id);
        let (name, content_type) = match meta {
            Some(item) => {
                let content_type = item
                    .content_type
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| attachment_content_type(&item.name).to_string());
                (item.name, content_type)
            }
            None => (
                attachment_id.to_string(),
                "application/octet-stream".to_string(),
            ),
        };
        let bytes = client
            .download_task_attachment(&account.id, &list_remote, &task_remote, attachment_id)
            .await?;
        Ok((name, content_type, bytes))
    }

    /// Inline preview payload: a `data:` URL the webview can render directly.
    pub async fn task_attachment_data(&self, task_id: &str, attachment_id: &str) -> Result<String> {
        let (_, content_type, bytes) = self.attachment_payload(task_id, attachment_id).await?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Ok(format!("data:{content_type};base64,{encoded}"))
    }

    /// Write one attachment to a user-chosen path.
    pub async fn task_attachment_save(
        &self,
        task_id: &str,
        attachment_id: &str,
        path: &str,
    ) -> Result<()> {
        let (_, _, bytes) = self.attachment_payload(task_id, attachment_id).await?;
        tokio::fs::write(path, &bytes)
            .await
            .map_err(|error| AppError::io(format!("cannot save the attachment: {error}")))?;
        Ok(())
    }

    pub async fn task_get(&self, id: &str) -> Result<Task> {
        let id = id.to_string();
        let mut task = self
            .ctx
            .db
            .read(move |conn| tasks_repo::get_required(conn, &id))
            .await?;
        self.prune_missing_links(std::slice::from_mut(&mut task))
            .await?;
        Ok(task)
    }

    /// Drop links whose source message no longer exists.
    ///
    /// Mail deletion cannot cascade into `task` (the column has no foreign
    /// key), so a task can outlive the message it was created from. Returning
    /// the stale id makes the task detail offer "open e-mail" and open a
    /// message window for a row that is gone; clearing it here also stops the
    /// task from counting as flagged mail once it is read again.
    async fn prune_missing_links(&self, tasks: &mut [Task]) -> Result<()> {
        let linked: Vec<String> = tasks
            .iter()
            .filter_map(|task| task.linked_message_id.clone())
            .collect();
        if linked.is_empty() {
            return Ok(());
        }
        let existing: HashSet<String> = self
            .ctx
            .db
            .read(move |conn| {
                let mut existing = HashSet::new();
                for message_id in linked {
                    // A soft-deleted message (Trash) is gone from the user's
                    // point of view: keep the task but drop the stale link.
                    if let Some(message) = messages_repo::get(conn, &message_id)? {
                        if !message.is_deleted_local {
                            existing.insert(message_id);
                        }
                    }
                }
                Ok(existing)
            })
            .await?;

        let mut cleared_ids = Vec::new();
        let mut cleared_lists = Vec::new();
        for task in tasks.iter_mut() {
            let Some(message_id) = task.linked_message_id.clone() else {
                continue;
            };
            if existing.contains(&message_id) {
                continue;
            }
            task.linked_message_id = None;
            cleared_ids.push(task.id.clone());
            cleared_lists.push(task.list_id.clone());
        }
        if cleared_ids.is_empty() {
            return Ok(());
        }
        self.ctx
            .db
            .write(move |conn| {
                for task_id in cleared_ids {
                    tasks_repo::set_linked_message(conn, &task_id, None)?;
                }
                Ok(())
            })
            .await?;
        // Other windows may still hold the stale link; the cleared rows live
        // in these lists.
        cleared_lists.sort();
        cleared_lists.dedup();
        emit_tasks_changed(self.ctx.events.as_ref(), cleared_lists);
        Ok(())
    }

    pub async fn task_create(&self, list_id: &str, input: TaskInput) -> Result<Task> {
        if input.title.trim().is_empty() {
            return Err(invalid("task title must not be empty"));
        }
        let list_id = list_id.to_string();
        let (task, op_id) = self
            .ctx
            .db
            .write({
                let input = input.clone();
                move |conn| {
                    let list = task_lists::get_required(conn, &list_id)?;
                    if list.is_read_only {
                        return Err(invalid("task list is read-only"));
                    }
                    // The row records the operation that owns its content, so
                    // the id must exist before the local write. The row, its
                    // owner state and the queued operation commit together:
                    // a failure leaves neither a row with a ghost owner nor a
                    // queue entry without a row.
                    let op_id = task_provider_account(conn, &list.account_id)?.then(new_id);
                    // New tasks are prepended (sort_order = MIN - 1).
                    let task = tasks_repo::create(conn, &list_id, &input)?;
                    store::bump_task_content(conn, &task.id, op_id.as_deref())?;
                    let task = tasks_repo::get_required(conn, &task.id)?;
                    if let Some(op_id) = op_id.as_deref() {
                        let op = TaskOp::Save {
                            task_id: task.id.clone(),
                            assigned_uid: None,
                            snapshot: None,
                        };
                        enqueue_op_with_id_sync(
                            conn,
                            op_id,
                            &list.account_id,
                            &op,
                            format!("创建任务「{}」", task.title),
                        )?;
                    }
                    Ok((task, op_id))
                }
            })
            .await?;
        if op_id.is_some() {
            self.notify.notify_one();
        }
        emit_tasks_changed(self.ctx.events.as_ref(), vec![task.list_id.clone()]);
        Ok(task)
    }

    pub async fn task_update(&self, id: &str, input: TaskInput) -> Result<Task> {
        if input.title.trim().is_empty() {
            return Err(invalid("task title must not be empty"));
        }
        let id = id.to_string();
        let (task, op_id) = self
            .ctx
            .db
            .write({
                let input = input.clone();
                move |conn| {
                    let snapshot = snapshot_task(conn, &id)?
                        .ok_or_else(|| AppError::not_found("task not found"))?;
                    let list = task_lists::get_required(conn, &snapshot.task.list_id)?;
                    if list.is_read_only {
                        return Err(invalid("task list is read-only"));
                    }
                    let account_id = snapshot.task.account_id.clone();
                    let op_id = task_provider_account(conn, &account_id)?.then(new_id);
                    let task = tasks_repo::update(conn, &id, &input)?;
                    store::bump_task_content(conn, &task.id, op_id.as_deref())?;
                    if let Some(op_id) = op_id.as_deref() {
                        let op = TaskOp::Save {
                            task_id: task.id.clone(),
                            assigned_uid: None,
                            snapshot: Some(snapshot),
                        };
                        enqueue_op_with_id_sync(
                            conn,
                            op_id,
                            &account_id,
                            &op,
                            format!("更新任务「{}」", task.title),
                        )?;
                    }
                    Ok((task, op_id))
                }
            })
            .await?;
        if op_id.is_some() {
            self.notify.notify_one();
        }
        emit_tasks_changed(self.ctx.events.as_ref(), vec![task.list_id.clone()]);
        Ok(task)
    }

    pub async fn task_set_completed(&self, id: &str, value: bool) -> Result<Task> {
        let id = id.to_string();
        let current = self.task_get(&id).await?;
        let account = self.load_account(&current.account_id).await?;
        let is_provider = self.is_provider(&account);
        let provider = self.provider_for(&account);
        let server_recurs = provider
            .as_ref()
            .map(|provider| provider.capabilities().server_recurring_completion)
            .unwrap_or(false);
        let today_date = today();
        let account_id = account.id.clone();
        let (task, queued) = self
            .ctx
            .db
            .write(move |conn| {
                let current = tasks_repo::get_required(conn, &id)?;
                let was_completed = current.status == TaskStatus::Completed;
                let snapshot = snapshot_task(conn, &id)?
                    .ok_or_else(|| AppError::not_found("task not found"))?;
                let save_op_id = is_provider.then(new_id);
                let mut updated = tasks_repo::set_completed(conn, &id, value)?;
                store::bump_task_content(conn, &updated.id, save_op_id.as_deref())?;
                let (next, next_op_id) =
                    if value && !was_completed && !server_recurs && current.recurrence.is_some() {
                        let next_op_id = is_provider.then(new_id);
                        let next = next_occurrence(&current, &today_date)
                            .map(|input| -> Result<Task> {
                                let next = tasks_repo::create(conn, &current.list_id, &input)?;
                                store::bump_task_content(conn, &next.id, next_op_id.as_deref())?;
                                Ok(next)
                            })
                            .transpose()?;
                        if next.is_some() {
                            // The completed row is the old occurrence, not a
                            // running series; the new task carries the rule.
                            let mut cleared = input_from_task(&updated);
                            cleared.recurrence = None;
                            updated = tasks_repo::update(conn, &id, &cleared)?;
                            store::bump_task_content(conn, &updated.id, save_op_id.as_deref())?;
                        }
                        (next, next_op_id)
                    } else {
                        (None, None)
                    };
                // The row writes and every upload they imply commit together,
                // so a failed enqueue rolls the completion (and the next
                // occurrence) back instead of leaving a row owned by an
                // operation that does not exist.
                if let Some(save_op_id) = save_op_id.as_deref() {
                    let op = TaskOp::Save {
                        task_id: updated.id.clone(),
                        assigned_uid: None,
                        snapshot: Some(snapshot),
                    };
                    enqueue_op_with_id_sync(
                        conn,
                        save_op_id,
                        &account_id,
                        &op,
                        format!("完成任务「{}」", updated.title),
                    )?;
                    if let (Some(next), Some(next_op_id)) = (&next, next_op_id.as_deref()) {
                        let op = TaskOp::Save {
                            task_id: next.id.clone(),
                            assigned_uid: None,
                            snapshot: None,
                        };
                        enqueue_op_with_id_sync(
                            conn,
                            next_op_id,
                            &account_id,
                            &op,
                            format!("创建下一期任务「{}」", next.title),
                        )?;
                    }
                }
                Ok((updated, save_op_id.is_some()))
            })
            .await?;
        if queued {
            self.notify.notify_one();
        }
        emit_tasks_changed(self.ctx.events.as_ref(), vec![task.list_id.clone()]);
        self.notify();
        Ok(task)
    }

    pub async fn task_set_importance(&self, id: &str, importance: Importance) -> Result<Task> {
        let id = id.to_string();
        let account_id = {
            let id = id.clone();
            self.ctx
                .db
                .read(move |conn| Ok(tasks_repo::get_required(conn, &id)?.account_id))
                .await?
        };
        let account = self.load_account(&account_id).await?;
        let is_provider = self.is_provider(&account);
        let (task, op_id) = self
            .ctx
            .db
            .write(move |conn| {
                let snapshot = snapshot_task(conn, &id)?
                    .ok_or_else(|| AppError::not_found("task not found"))?;
                let op_id = is_provider.then(new_id);
                let task = tasks_repo::set_importance(conn, &id, importance)?;
                store::bump_task_content(conn, &task.id, op_id.as_deref())?;
                if let Some(op_id) = op_id.as_deref() {
                    let op = TaskOp::Save {
                        task_id: task.id.clone(),
                        assigned_uid: None,
                        snapshot: Some(snapshot),
                    };
                    enqueue_op_with_id_sync(
                        conn,
                        op_id,
                        &account.id,
                        &op,
                        format!("更新任务「{}」", task.title),
                    )?;
                }
                Ok((task, op_id))
            })
            .await?;
        if op_id.is_some() {
            self.notify.notify_one();
        }
        emit_tasks_changed(self.ctx.events.as_ref(), vec![task.list_id.clone()]);
        Ok(task)
    }

    pub async fn task_set_my_day(&self, id: &str, date: Option<&str>) -> Result<Task> {
        let id = id.to_string();
        let date = date.map(str::to_string);
        let task = self
            .ctx
            .db
            .write(move |conn| tasks_repo::set_my_day(conn, &id, date.as_deref()))
            .await?;
        // My Day is a local-only field for every provider.
        emit_tasks_changed(self.ctx.events.as_ref(), vec![task.list_id.clone()]);
        Ok(task)
    }

    pub async fn task_move(&self, id: &str, list_id: &str) -> Result<Task> {
        let id = id.to_string();
        let target = list_id.to_string();
        let account_id = {
            let id = id.clone();
            self.ctx
                .db
                .read(move |conn| Ok(tasks_repo::get_required(conn, &id)?.account_id))
                .await?
        };
        let account = self.load_account(&account_id).await?;
        let is_provider = self.is_provider(&account);
        let (task, from_list, op_id) = self
            .ctx
            .db
            .write(move |conn| {
                let snapshot = snapshot_task(conn, &id)?
                    .ok_or_else(|| AppError::not_found("task not found"))?;
                let from_list = snapshot.task.list_id.clone();
                let list = task_lists::get_required(conn, &target)?;
                if list.is_read_only {
                    return Err(invalid("task list is read-only"));
                }
                let op_id = is_provider.then(new_id);
                let task = tasks_repo::move_task(conn, &id, &target)?;
                store::bump_task_row(conn, &task.id, op_id.as_deref())?;
                if let Some(op_id) = op_id.as_deref() {
                    let op = TaskOp::Move {
                        task_id: task.id.clone(),
                        from_list_id: from_list.clone(),
                        snapshot,
                    };
                    enqueue_op_with_id_sync(
                        conn,
                        op_id,
                        &account.id,
                        &op,
                        format!("移动任务「{}」", task.title),
                    )?;
                }
                Ok((task, from_list, op_id))
            })
            .await?;
        if op_id.is_some() {
            self.notify.notify_one();
        }
        emit_tasks_changed(
            self.ctx.events.as_ref(),
            vec![from_list, task.list_id.clone()],
        );
        Ok(task)
    }

    pub async fn task_reorder(&self, list_id: &str, ids: Vec<String>) -> Result<()> {
        let list_id = list_id.to_string();
        // Provider capability is account configuration, not task state, so it
        // is safe to read before the reorder transaction.
        let list = self
            .ctx
            .db
            .read({
                let id = list_id.clone();
                move |conn| task_lists::get_required(conn, &id)
            })
            .await?;
        let account = self.load_account(&list.account_id).await?;
        // Push the order to providers that expose one.  Google's `tasks.move`
        // is the only backend with a server-side position; Graph/CalDAV keep
        // the local order (see `TaskCapabilities::ordering`).
        let orderable = self.is_provider(&account)
            && self
                .provider_for(&account)
                .is_some_and(|provider| provider.capabilities().ordering);
        let account_id = account.id.clone();
        let list_id_for_db = list_id.clone();
        self.ctx
            .db
            .write(move |conn| {
                tasks_repo::reorder(conn, &list_id_for_db, &ids)?;
                if orderable {
                    // The local order and its upload intent commit together;
                    // otherwise a failed enqueue leaves the list locally
                    // reordered but the server on the old order.
                    enqueue_op_sync(
                        conn,
                        &account_id,
                        &TaskOp::Reorder {
                            list_id: list_id_for_db.clone(),
                            ids: ids.clone(),
                        },
                        "调整任务顺序".to_string(),
                    )?;
                }
                Ok(())
            })
            .await?;
        if orderable {
            self.notify.notify_one();
        }
        emit_tasks_changed(self.ctx.events.as_ref(), vec![list_id]);
        Ok(())
    }

    pub async fn task_delete(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        // Provider capability is account configuration, not task state, so it
        // is safe to read before the delete transaction. The snapshot itself is
        // re-read inside, where it can see a receipt that landed in between.
        let account_id = {
            let id = id.clone();
            self.ctx
                .db
                .read(move |conn| Ok(tasks_repo::get_required(conn, &id)?.account_id))
                .await?
        };
        let account = self.load_account(&account_id).await?;
        let is_provider = self.is_provider(&account);
        // A create that is still on the wire has not written its identity to
        // the row yet. If the provider lets the client choose that identity,
        // resolve it now so the delete can suppress the object even when the
        // acknowledgement is lost.
        let predicted_create = if is_provider {
            self.predicted_create_identity(&account, &id).await?
        } else {
            None
        };
        let snapshot = self
            .ctx
            .db
            .write({
                let id = id.clone();
                let account_id = account.id.clone();
                move |conn| {
                    let snapshot = snapshot_task(conn, &id)?
                        .ok_or_else(|| AppError::not_found("task not found"))?;
                    let has_identity = snapshot.remote_id.is_some() || snapshot.href.is_some();
                    // A save that is already on the wire can still commit a
                    // remote object (a create) or upgrade a derived identity to
                    // a real UID. Bind that cleanup to the operation that may
                    // have sent it; an operation that provably never sent is
                    // cancelled instead, so it cannot produce an object after
                    // the row is gone.
                    let owner = if is_provider {
                        task_save_cleanup_owner(conn, &account_id, &id)?
                    } else {
                        None
                    };
                    // Only a create (no stored identity) needs the predicted
                    // identity, and only a possibly-sent save can have been
                    // the writer that produced it.
                    let predicted = if owner.is_some() && !has_identity {
                        predicted_create.as_ref()
                    } else {
                        None
                    };
                    if let Some((owner_op_id, owner_attempt)) = &owner {
                        store::record_task_delete_intent(
                            conn,
                            &id,
                            &snapshot.task.account_id,
                            &snapshot.task.list_id,
                            predicted
                                .and_then(|identity| identity.remote_id.as_deref())
                                .or(snapshot.remote_id.as_deref()),
                            predicted
                                .and_then(|identity| identity.href.as_deref())
                                .or(snapshot.href.as_deref()),
                            snapshot.etag.as_deref(),
                            predicted.map_or(snapshot.uid_is_derived, |identity| {
                                identity.uid_is_derived
                            }),
                            Some(owner_op_id),
                            Some(*owner_attempt),
                        )?;
                    }
                    tasks_repo::delete(conn, &id)?;
                    // The queued DELETE has not reached the server yet, and a
                    // delta captured before it can still name this object.
                    // Record the identity in the same transaction as the local
                    // delete so a sync racing this command cannot import it
                    // again.
                    if has_identity {
                        // The DELETE is queued below in this same transaction;
                        // its TTL starts when the executor settles the
                        // tombstone after a successful delete.
                        store::insert_task_delete_tombstone(
                            conn,
                            &snapshot.task.account_id,
                            &snapshot.task.list_id,
                            snapshot.remote_id.as_deref(),
                            snapshot.href.as_deref(),
                            snapshot.uid_is_derived,
                        )?;
                    } else if let Some(identity) = predicted {
                        // The create may have committed server-side while its
                        // receipt was lost. Protect the identity it will use;
                        // the receipt (if it arrives) settles this tombstone
                        // and queues the DELETE through the normal path.
                        store::insert_task_delete_tombstone(
                            conn,
                            &snapshot.task.account_id,
                            &snapshot.task.list_id,
                            identity.remote_id.as_deref(),
                            identity.href.as_deref(),
                            identity.uid_is_derived,
                        )?;
                    }
                    if is_provider && has_identity {
                        // Local delete, tombstone and the upload that removes
                        // the object must commit together. Enqueueing after the
                        // transaction left a permanent suppression behind when
                        // the queue insert failed, and the task could never be
                        // imported again.
                        enqueue_op_sync(
                            conn,
                            &account_id,
                            &TaskOp::Delete {
                                snapshot: snapshot.clone(),
                            },
                            format!("删除任务「{}」", snapshot.task.title),
                        )?;
                    }
                    Ok(snapshot)
                }
            })
            .await?;
        if is_provider {
            self.notify.notify_one();
        }
        emit_tasks_changed(
            self.ctx.events.as_ref(),
            vec![snapshot.task.list_id.clone()],
        );
        Ok(())
    }

    pub async fn task_search(&self, text: &str) -> Result<Vec<Task>> {
        let text = text.trim().to_string();
        if text.is_empty() {
            return Ok(Vec::new());
        }
        self.ctx
            .db
            .read(move |conn| tasks_repo::search(conn, &text))
            .await
    }

    /// Create a To Do task linked to an e-mail and flag the message.
    pub async fn message_create_task(
        &self,
        message_id: &str,
        options: NewTaskFromMessage,
    ) -> Result<Task> {
        let id = message_id.to_string();
        let message = self
            .ctx
            .db
            .read(move |conn| messages_repo::get_required(conn, &id))
            .await?;
        let summary = message.summary.clone();
        let title = if options.title.trim().is_empty() {
            if summary.subject.trim().is_empty() {
                "(无主题)".to_string()
            } else {
                summary.subject.clone()
            }
        } else {
            options.title.trim().to_string()
        };
        let sender = summary
            .r#from
            .as_ref()
            .map(|address| address.to_string())
            .unwrap_or_default();
        let notes = if options.notes.trim().is_empty() {
            format!(
                "发件人：{}\n日期：{}",
                sender,
                summary.date.format("%Y-%m-%d %H:%M")
            )
        } else {
            options.notes.clone()
        };
        let input = TaskInput {
            title,
            notes,
            importance: Importance::Normal,
            due_date: options.due_date,
            reminder_at: options.reminder_at,
            recurrence: None,
            my_day_date: options.my_day_date,
            steps: Vec::new(),
        };
        let task = self.task_create(&options.list_id, input).await?;
        let account_id = summary.account_id.clone();
        let task_id = task.id.clone();
        let message_for_link = summary.id.clone();
        self.ctx
            .db
            .write(move |conn| {
                tasks_repo::set_linked_message(conn, &task_id, Some(&message_for_link))
            })
            .await?;
        // Flag the source message only when the user asked for it.
        //
        // The local value, the queue row and the per-field writer are one
        // transaction, and the source row is re-checked inside it. This path
        // creates its payload outside the writer-gated flow (the task dialog is
        // not an undoable mail action), so it must carry `messageIds`: a
        // payload without them would be treated as legacy and retried even
        // after the user cancelled the star. The identity check prevents a
        // writer from being created for a projection that was deleted or
        // rebound while the task was being created.
        if options.mark_flagged && !summary.flagged {
            let account = self.load_account(&account_id).await?;
            let message_ids = vec![summary.id.clone()];
            let folder_id = summary.folder_id.clone();
            let identity = messages_repo::MessageIdentity::of(&message);
            let queued: Option<(String, serde_json::Value)> = if !account.provider.is_local() {
                message.uid.map(|uid| {
                    let payload = serde_json::json!({
                        "folderId": folder_id.clone(),
                        "uids": [uid],
                        "seen": serde_json::Value::Null,
                        "flagged": true,
                        "answered": serde_json::Value::Null,
                        "forwarded": serde_json::Value::Null,
                        "sourceUidvalidity": message.uidvalidity,
                        "previous": [{
                            "uid": uid,
                            "seen": summary.seen,
                            "flagged": summary.flagged,
                            "answered": summary.answered,
                            "forwarded": summary.forwarded,
                        }],
                        "messageIds": [summary.id.clone()],
                        "outcomes": [],
                        "remoteIds": message
                            .gm_msgid
                            .as_deref()
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(|value| vec![value.to_string()])
                            .unwrap_or_default(),
                    });
                    (account.id.clone(), payload)
                })
            } else {
                None
            };
            let applied = self
                .ctx
                .db
                .write(move |conn| {
                    if !messages_repo::identity_matches(conn, &identity)? {
                        return Ok(false);
                    }
                    messages_repo::set_flags(
                        conn,
                        std::slice::from_ref(&identity.id),
                        None,
                        Some(true),
                        None,
                        None,
                    )?;
                    if let Some((account_id, payload)) = queued {
                        let op = repo::pending_ops::enqueue(
                            conn,
                            NewPendingOp::new(
                                account_id,
                                "setFlags",
                                payload,
                                "标记邮件".to_string(),
                            ),
                        )?;
                        // The executor only sends a payload with `messageIds`
                        // while this exact operation is still the field's
                        // writer.
                        messages_repo::record_flag_writers(
                            conn,
                            &identity.id,
                            &op.id,
                            "setFlagged",
                            None,
                            Some(true),
                            None,
                            None,
                        )?;
                    }
                    Ok(true)
                })
                .await?;
            if applied {
                emit_mail_changed(
                    self.ctx.events.as_ref(),
                    account_id.clone(),
                    vec![folder_id],
                    message_ids,
                    MailChangeKind::Updated,
                );
            }
        }
        let task = self.task_get(&task.id).await?;
        emit_tasks_changed(self.ctx.events.as_ref(), vec![task.list_id.clone()]);
        Ok(task)
    }

    /* ───────────────────────── helpers ───────────────────────────── */

    async fn enqueue_op(&self, account_id: &str, op: &TaskOp, description: String) -> Result<()> {
        let account_id = account_id.to_string();
        let op = op.clone();
        self.ctx
            .db
            .write(move |conn| enqueue_task_op(conn, None, &account_id, &op, description))
            .await?;
        self.notify.notify_one();
        Ok(())
    }

    /// Persist the UID assigned to a derived-identity task before the first
    /// wire attempt.
    ///
    /// The value lives on the operation payload (not only in memory) so a
    /// retry of the *same* operation reuses it. Rewriting it per attempt would
    /// change the VTODO `UID` after an ambiguous failure and could leave the
    /// server with one identity while the client believes in another.
    async fn persist_assigned_uid(&self, op: &PendingOp, uid: &str) -> Result<()> {
        let mut payload = ops::decode(op)?;
        match &mut payload {
            TaskOp::Save { assigned_uid, .. } => *assigned_uid = Some(uid.to_string()),
            _ => return Ok(()),
        }
        let value = ops::to_value(&payload)?;
        let id = op.id.clone();
        self.ctx
            .db
            .write(move |conn| repo::pending_ops::update_payload(conn, &id, &value))
            .await?;
        Ok(())
    }

    /// Persist, before the provider call, that this operation is about to
    /// issue a wire command.
    ///
    /// A zero-row update means the operation was cancelled concurrently (a
    /// delete racing this save), so the caller must not send anything.
    async fn mark_send_intent(&self, op_id: &str) -> Result<()> {
        let id = op_id.to_string();
        let changed = self
            .ctx
            .db
            .write(move |conn| repo::pending_ops::mark_send_intent(conn, &id))
            .await?;
        if changed == 0 {
            return Err(AppError::conflict(
                "task operation was cancelled before it was sent",
            ));
        }
        Ok(())
    }

    async fn load_account(&self, id: &str) -> Result<Account> {
        let id = id.to_string();
        self.ctx
            .db
            .read(move |conn| repo::accounts::get_required(conn, &id))
            .await
    }

    async fn snapshot_task(&self, id: &str) -> Result<Option<TaskSnapshot>> {
        let id = id.to_string();
        self.ctx.db.read(move |conn| snapshot_task(conn, &id)).await
    }

    async fn list_mapping(&self, list_id: &str) -> Result<Option<TaskListRemote>> {
        let id = list_id.to_string();
        self.ctx
            .db
            .read(move |conn| store::get_task_list_by_local(conn, &id))
            .await
    }

    fn remote_list(&self, mapping: &TaskListRemote) -> RemoteTaskList {
        RemoteTaskList {
            local_id: mapping.task_list_id.clone(),
            remote_id: mapping.remote_id.clone(),
            href: mapping.href.clone(),
            name: String::new(),
            color: None,
            read_only: false,
            well_known_list_name: nuncio_core::models::WELL_KNOWN_NONE.to_string(),
        }
    }

    /// The identity a create of `task_id` will use, when the provider chooses
    /// it client-side. See [`TaskProvider::predicted_create_identity`].
    async fn predicted_create_identity(
        &self,
        account: &Account,
        task_id: &str,
    ) -> Result<Option<RemoteTaskRef>> {
        let Some(provider) = self.provider_for(account) else {
            return Ok(None);
        };
        let id = task_id.to_string();
        // The row is gone (or the delete transaction below will report
        // not-found): there is nothing to attribute.
        let Some(task) = self
            .ctx
            .db
            .read(move |conn| tasks_repo::get(conn, &id))
            .await?
        else {
            return Ok(None);
        };
        let Some(mapping) = self.list_mapping(&task.list_id).await? else {
            return Ok(None);
        };
        Ok(provider.predicted_create_identity(&self.remote_list(&mapping), task_id))
    }

    async fn reconcile_steps(
        &self,
        account: &Account,
        provider: &dyn TaskProvider,
        list: &RemoteTaskList,
        task: &Task,
        remote_id: &str,
        href: Option<&str>,
    ) -> Result<()> {
        if !provider.capabilities().steps {
            return Ok(());
        }
        let task_id = task.id.clone();
        let mappings = self
            .ctx
            .db
            .read({
                let id = task_id.clone();
                move |conn| store::step_mappings_for_task(conn, &id)
            })
            .await?;
        let parent = RemoteTaskRef {
            remote_id: Some(remote_id.to_string()),
            href: href.map(str::to_string),
            etag: None,
            // Provider-returned parent ids are real identities.
            uid_is_derived: false,
        };
        let current: HashSet<String> = task.steps.iter().map(|step| step.id.clone()).collect();
        // Errors are collected rather than propagated: one step failing must
        // not stop the others. Creating first matters, because an update of an
        // already-mapped step can fail on its own (a stale ETag, a step edited
        // elsewhere) and used to abort the whole reconciliation, which is why
        // a second step could never be added after a failed first update.
        let mut failures: Vec<AppError> = Vec::new();

        // Google Tasks takes the preceding sibling as a query parameter;
        // without it a new subtask is inserted at the top of the list and the
        // steps end up in reverse order.
        let mut previous: Option<String> = None;
        for step in &task.steps {
            if let Some((_, remote)) = mappings.iter().find(|(step_id, _)| step_id == &step.id) {
                previous = Some(remote.clone());
                continue;
            }
            match provider
                .create_step(account, list, &parent, previous.as_deref(), step)
                .await
            {
                Ok(Some(result)) => {
                    if let Some(remote_step) = result.remote_id {
                        previous = Some(remote_step.clone());
                        let step_id = step.id.clone();
                        let task_id = task.id.clone();
                        if let Err(error) = self
                            .ctx
                            .db
                            .write({
                                let step_id = step_id.clone();
                                move |conn| {
                                    store::set_step_mapping(conn, &step_id, &task_id, &remote_step)
                                }
                            })
                            .await
                        {
                            failures.push(error);
                        }
                    }
                }
                Ok(None) => previous = None,
                Err(error) => {
                    failures.push(error);
                    // The chain is broken for the following siblings; let them
                    // be inserted without an anchor instead of guessing.
                    previous = None;
                }
            }
        }

        for step in &task.steps {
            let Some((_, remote_step)) = mappings.iter().find(|(step_id, _)| step_id == &step.id)
            else {
                // Created above, or creation failed and was recorded already.
                continue;
            };
            match provider
                .update_step(account, list, &parent, remote_step, step)
                .await
            {
                Ok(()) => {}
                Err(error) if error.code() == "notFound" => {
                    // The remote item was deleted elsewhere. Recreate it rather
                    // than failing the operation forever on a row that is gone.
                    match provider
                        .create_step(account, list, &parent, None, step)
                        .await
                    {
                        Ok(Some(result)) => {
                            if let Some(remote_step) = result.remote_id {
                                let step_id = step.id.clone();
                                let task_id = task.id.clone();
                                if let Err(error) = self
                                    .ctx
                                    .db
                                    .write({
                                        let step_id = step_id.clone();
                                        move |conn| {
                                            store::set_step_mapping(
                                                conn,
                                                &step_id,
                                                &task_id,
                                                &remote_step,
                                            )
                                        }
                                    })
                                    .await
                                {
                                    failures.push(error);
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(error) => failures.push(error),
                    }
                }
                Err(error) => failures.push(error),
            }
        }

        for (step_id, remote_step) in mappings {
            if current.contains(&step_id) {
                continue;
            }
            match provider
                .delete_step(account, list, &parent, &remote_step)
                .await
            {
                Ok(()) => {
                    let step_id_clone = step_id.clone();
                    if let Err(error) = self
                        .ctx
                        .db
                        .write(move |conn| store::delete_step_mapping(conn, &step_id_clone))
                        .await
                    {
                        failures.push(error);
                    }
                }
                Err(error) => failures.push(error),
            }
        }

        match failures.into_iter().next() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/* ─────────────────────────── executor ─────────────────────────── */

#[async_trait::async_trait]
impl PimOpExecutor for TasksService {
    fn kind_prefix(&self) -> &'static str {
        ops::KIND_PREFIX
    }

    fn module(&self) -> SyncModule {
        SyncModule::Tasks
    }

    async fn execute(&self, op: &PendingOp) -> Result<()> {
        let decoded = ops::decode(op)?;
        let account = self.load_account(&op.account_id).await?;
        let Some(provider) = self.provider_for(&account) else {
            return Ok(());
        };
        match &decoded {
            TaskOp::Save {
                task_id,
                assigned_uid: sticky_uid,
                ..
            } => {
                let snapshot = match self.snapshot_task(task_id).await? {
                    Some(snapshot) => snapshot,
                    None => return Ok(()),
                };
                let task = snapshot.task.clone();
                let Some(mapping) = self.list_mapping(&task.list_id).await? else {
                    return Err(AppError::conflict("task list is not mapped yet"));
                };
                let list = self.remote_list(&mapping);
                let current = RemoteTaskRef {
                    remote_id: snapshot.remote_id.clone(),
                    href: snapshot.href.clone(),
                    etag: snapshot.etag.clone(),
                    uid_is_derived: snapshot.uid_is_derived,
                };
                let created = current.remote_id.is_none() && current.href.is_none();
                // A task whose local identity was derived from its href has no
                // real UID yet. Writing the placeholder back as a UID collides
                // with a genuine UID of the same spelling, so the first rewrite
                // assigns a fresh UID. The value is persisted on the operation
                // payload *before* the wire command: a retry after an ambiguous
                // failure must reuse it.
                let assigned_uid = if !created && snapshot.uid_is_derived {
                    let uid = match sticky_uid.clone() {
                        Some(uid) if !uid.trim().is_empty() => uid,
                        _ => {
                            let uid = new_id();
                            self.persist_assigned_uid(op, &uid).await?;
                            uid
                        }
                    };
                    Some(uid)
                } else {
                    None
                };
                // Persist the send intent before the wire command. A delete
                // racing this save may only bind a cleanup intent (and cancel
                // an unsent save) when it can see that the save may have
                // reached the provider.
                self.mark_send_intent(&op.id).await?;
                let result = if !created {
                    provider
                        .update_task(&account, &list, &current, &task, assigned_uid.as_deref())
                        .await?
                } else {
                    provider.create_task(&account, &list, &task).await?
                };
                let account_id = account.id.clone();
                let op_id = op.id.clone();
                let receipt = TaskSaveReceipt {
                    remote_id: result.remote_id.clone(),
                    href: result.href.clone(),
                    etag: result.etag.clone(),
                    uid_is_derived: result.uid_is_derived,
                };
                let base = snapshot.clone();
                let mapping = self
                    .ctx
                    .db
                    .write(move |conn| {
                        record_task_save_receipt(conn, &account_id, &op_id, &base, &receipt)
                    })
                    .await;
                if let Err(error) = mapping {
                    // The remote row exists but we failed to record its id. A
                    // retry would create a second copy (neither Google Tasks nor
                    // Graph accepts a client-chosen id), so undo the creation
                    // instead of leaving an orphan behind.
                    if created {
                        let created_ref = RemoteTaskRef {
                            remote_id: result.remote_id.clone(),
                            href: result.href.clone(),
                            etag: result.etag.clone(),
                            uid_is_derived: result.uid_is_derived,
                        };
                        let undone = provider.delete_task(&account, &list, &created_ref).await;
                        match undone {
                            Ok(()) => {}
                            Err(cleanup) => {
                                // The remote task is now an orphan that this
                                // client can no longer undo. Everything turns on
                                // whether the next attempt knows about it: with
                                // the id stored it updates that row, without it
                                // the retry creates a second task and the user
                                // ends up with duplicates.
                                //
                                // The write above may have failed on the `href`
                                // side (it covers two statements), so retry the
                                // part that matters alone rather than giving up.
                                warn!(
                                    %cleanup,
                                    task = %task.id,
                                    "could not undo a remote task whose local mapping failed"
                                );
                                if let Some(orphan_id) = result.remote_id.clone() {
                                    let id = task.id.clone();
                                    let etag = result.etag.clone();
                                    let uid_is_derived = result.uid_is_derived;
                                    let orphan_for_log = orphan_id.clone();
                                    if let Err(store_error) = self
                                        .ctx
                                        .db
                                        .write(move |conn| {
                                            conn.execute(
                                                "UPDATE task
                                                    SET remote_id = COALESCE(remote_id, ?2),
                                                        etag = COALESCE(etag, ?3),
                                                        updated_at = ?4
                                                  WHERE id = ?1",
                                                rusqlite::params![
                                                    id,
                                                    orphan_id,
                                                    etag,
                                                    repo::now_string()
                                                ],
                                            )?;
                                            store::set_task_uid_is_derived(
                                                conn,
                                                &id,
                                                uid_is_derived,
                                            )?;
                                            Ok(())
                                        })
                                        .await
                                    {
                                        // Nothing left to try: the retry will
                                        // create a duplicate, which is the
                                        // documented residual risk of a triple
                                        // failure. Log it with the id so it can
                                        // be reconciled by hand.
                                        warn!(
                                            %store_error,
                                            task = %task.id,
                                            remote_id = %orphan_for_log,
                                            "could not record an orphaned remote task id; \
                                             a retry may create a duplicate"
                                        );
                                    }
                                }
                            }
                        }
                    }
                    return Err(error);
                }
                let remote_id = result
                    .remote_id
                    .or(snapshot.remote_id)
                    .ok_or_else(|| AppError::conflict("provider returned no task id"))?;
                let href = result.href.or(snapshot.href);
                self.reconcile_steps(
                    &account,
                    provider.as_ref(),
                    &list,
                    &task,
                    &remote_id,
                    href.as_deref(),
                )
                .await?;
            }
            TaskOp::Delete { snapshot } => {
                let Some(mapping) = self.list_mapping(&snapshot.task.list_id).await? else {
                    return Ok(());
                };
                let list = self.remote_list(&mapping);
                let current = RemoteTaskRef {
                    remote_id: snapshot.remote_id.clone(),
                    href: snapshot.href.clone(),
                    etag: snapshot.etag.clone(),
                    uid_is_derived: snapshot.uid_is_derived,
                };
                provider.delete_task(&account, &list, &current).await?;
                // The remote DELETE succeeded, so the suppression window starts
                // *now*. Until this settlement the tombstone was `pending` and
                // protected the object regardless of elapsed wall time.
                let task_id = snapshot.task.id.clone();
                let list_id = snapshot.task.list_id.clone();
                let remote_id = snapshot.remote_id.clone();
                let href = snapshot.href.clone();
                let uid_is_derived = snapshot.uid_is_derived;
                self.ctx
                    .db
                    .write(move |conn| {
                        store::settle_task_delete_tombstones(
                            conn,
                            &list_id,
                            remote_id.as_deref(),
                            href.as_deref(),
                            uid_is_derived,
                            Utc::now()
                                + chrono::Duration::seconds(
                                    store::TASK_DELETE_TOMBSTONE_TTL_SECONDS,
                                ),
                        )?;
                        // The remote object is gone; no cleanup is owed even if
                        // a save receipt was attached while this DELETE was
                        // queued.
                        store::remove_task_delete_intent(conn, &task_id)?;
                        Ok(())
                    })
                    .await?;
            }
            TaskOp::Move {
                task_id,
                from_list_id,
                snapshot,
            } => {
                let task = self
                    .ctx
                    .db
                    .read({
                        let id = task_id.clone();
                        move |conn| tasks_repo::get_required(conn, &id)
                    })
                    .await?;
                let Some(from_mapping) = self.list_mapping(from_list_id).await? else {
                    return Ok(());
                };
                let Some(to_mapping) = self.list_mapping(&task.list_id).await? else {
                    return Ok(());
                };
                let from_list = self.remote_list(&from_mapping);
                let to_list = self.remote_list(&to_mapping);
                let current = RemoteTaskRef {
                    remote_id: snapshot.remote_id.clone(),
                    href: snapshot.href.clone(),
                    etag: snapshot.etag.clone(),
                    uid_is_derived: snapshot.uid_is_derived,
                };
                let same_account = from_mapping.account_id == to_mapping.account_id;
                let result = if same_account {
                    provider
                        .move_task(&account, &from_list, &to_list, &current, &task)
                        .await?
                } else {
                    let from_account = self.load_account(&from_mapping.account_id).await?;
                    let from_provider = self
                        .provider_for(&from_account)
                        .ok_or_else(|| unsupported("source list has no provider"))?;
                    let to_account = self.load_account(&to_mapping.account_id).await?;
                    let to_provider = self
                        .provider_for(&to_account)
                        .ok_or_else(|| unsupported("target list has no provider"))?;
                    let created = to_provider
                        .create_task(&to_account, &to_list, &task)
                        .await?;
                    from_provider
                        .delete_task(&from_account, &from_list, &current)
                        .await?;
                    created
                };
                let id = task.id.clone();
                let new_remote_id = result.remote_id.clone().or(snapshot.remote_id.clone());
                let new_href = result.href.clone().or(snapshot.href.clone());
                let moved_via_api = same_account && provider.name() == "google";
                let source_list_id = from_list_id.clone();
                let source_account_id = from_mapping.account_id.clone();
                let target_list_id = task.list_id.clone();
                let source_remote_id = snapshot.remote_id.clone();
                let source_href = snapshot.href.clone();
                let source_uid_is_derived = snapshot.uid_is_derived;
                // The identity the task now carries in the target list. It is
                // the same resource for Google's relink move, and a fresh
                // object for the create+delete fallback.
                let target_remote_id = result.remote_id.clone().or(source_remote_id.clone());
                let target_href = result.href.clone().or(source_href.clone());
                let target_uid_is_derived = if result.remote_id.is_some() {
                    result.uid_is_derived
                } else {
                    source_uid_is_derived
                };
                self.ctx
                    .db
                    .write({
                        let remote_id = result.remote_id.clone();
                        let has_remote_id = remote_id.is_some();
                        let uid_is_derived = result.uid_is_derived;
                        let etag = result.etag.clone();
                        let href = result.href.clone();
                        let clear_steps = !moved_via_api;
                        move |conn| {
                            conn.execute(
                                "UPDATE task SET remote_id = COALESCE(?2, remote_id),
                                                 etag = COALESCE(?3, etag),
                                                 updated_at = ?4
                                 WHERE id = ?1",
                                rusqlite::params![id, remote_id, etag, repo::now_string()],
                            )?;
                            if has_remote_id {
                                store::set_task_uid_is_derived(conn, &id, uid_is_derived)?;
                            }
                            if let Some(href) = &href {
                                store::set_task_href(conn, &id, href)?;
                            }
                            if clear_steps {
                                store::clear_step_mappings(conn, &id)?;
                            }
                            // The provider removed the object from the source
                            // list (a MOVE relinks it, the default
                            // implementation deletes the source copy). A delta
                            // for the source list captured before the move
                            // still names the old identity and would re-import
                            // the task there; a settled tombstone scoped to
                            // the source list suppresses it without hiding the
                            // task in the target list.
                            record_task_move_tombstone(
                                conn,
                                &source_account_id,
                                &source_list_id,
                                &target_list_id,
                                source_remote_id.as_deref(),
                                source_href.as_deref(),
                                source_uid_is_derived,
                            )?;
                            // The task just arrived in the target list. Any
                            // "it left here" marker from an earlier move of
                            // this same resource is now stale: leaving it
                            // would hide the returned task from deltas and let
                            // a full listing prune its local row. Only `move`
                            // tombstones are cleared; a pending DELETE is
                            // still owed to the provider and stays.
                            store::remove_task_move_tombstones(
                                conn,
                                &target_list_id,
                                target_remote_id.as_deref(),
                                target_href.as_deref(),
                                target_uid_is_derived,
                            )?;
                            Ok(())
                        }
                    })
                    .await?;
                // The default move implementation (and cross-account moves)
                // creates a fresh task, so provider-side sub-steps must be
                // recreated in the target list.
                if !moved_via_api {
                    if let (Some(remote_id), true) = (new_remote_id, provider.capabilities().steps)
                    {
                        let target_account = self.load_account(&task.account_id).await?;
                        self.reconcile_steps(
                            &target_account,
                            provider.as_ref(),
                            &to_list,
                            &task,
                            &remote_id,
                            new_href.as_deref(),
                        )
                        .await?;
                    }
                }
            }
            TaskOp::Reorder { list_id, ids } => {
                let Some(mapping) = self.list_mapping(list_id).await? else {
                    return Ok(());
                };
                let list = self.remote_list(&mapping);
                let mut remote_ids = Vec::new();
                for id in ids {
                    if let Some(snapshot) = self.snapshot_task(id).await? {
                        if let Some(remote_id) = snapshot.remote_id {
                            remote_ids.push(remote_id);
                        }
                    }
                }
                if remote_ids.len() > 1 {
                    provider.reorder_tasks(&account, &list, &remote_ids).await?;
                }
            }
            TaskOp::ListCreate { list_id } => {
                if self.list_mapping(list_id).await?.is_some() {
                    return Ok(());
                }
                let list = self.load_task_list(list_id).await?;
                let remote = provider
                    .create_task_list(&account, &list.name, &list.color)
                    .await?;
                let mapping = TaskListRemote::new(
                    list.id.clone(),
                    account.id.clone(),
                    provider.name(),
                    remote.remote_id,
                    remote.href,
                );
                self.ctx
                    .db
                    .write(move |conn| store::set_task_list(conn, &mapping))
                    .await?;
            }
            TaskOp::ListUpdate { list_id, .. } => {
                let Some(mapping) = self.list_mapping(list_id).await? else {
                    return Ok(());
                };
                let list = self.load_task_list(list_id).await?;
                let remote = self.remote_list(&mapping);
                // A built-in list's name belongs to the provider: a rename is
                // always rejected, and the rollback would then undo the colour
                // the user just picked. Colour itself is local-only.
                let name = if list.is_builtin() {
                    None
                } else {
                    Some(list.name.as_str())
                };
                provider
                    .update_task_list(&account, &remote, name, Some(&list.color))
                    .await?;
            }
            TaskOp::ListDelete { snapshot } => {
                if let Some(remote_id) = &snapshot.remote_id {
                    let list = RemoteTaskList {
                        local_id: snapshot.list.id.clone(),
                        remote_id: remote_id.clone(),
                        href: snapshot.href.clone(),
                        name: snapshot.list.name.clone(),
                        color: Some(snapshot.list.color.clone()),
                        read_only: snapshot.list.is_read_only,
                        well_known_list_name: snapshot.list.well_known_list_name.clone(),
                    };
                    provider.delete_task_list(&account, &list).await?;
                }
            }
        }
        emit_tasks_changed(
            self.ctx.events.as_ref(),
            self.affected_lists(&decoded).await?,
        );
        Ok(())
    }

    async fn rollback(&self, op: &PendingOp) -> Result<()> {
        let op_row = op.clone();
        // Rolling back mutates exactly the rows the UI is showing, and the
        // failure path already told the user something went wrong. Without a
        // changed event the list keeps rendering the rolled-back values until
        // the next sync, so the user sees an error toast over state that no
        // longer matches the database.
        let restore = self
            .ctx
            .db
            .write(move |conn| rollback_conn(conn, &op_row))
            .await;
        if restore.is_ok() {
            // An empty list means "anything may have changed"; the frontend
            // invalidates every task query for it.
            emit_tasks_changed(self.ctx.events.as_ref(), Vec::new());
        }
        restore
    }

    fn supports_atomic_rollback(&self) -> bool {
        true
    }

    fn rollback_in_tx(&self, conn: &rusqlite::Connection, op: &PendingOp) -> Result<()> {
        rollback_conn(conn, op)
    }

    fn on_failure(&self, ctx: &CoreContext, _op: &PendingOp) {
        // The rollback committed with `mark_failed`; the task list is showing
        // the rows it just rewrote, so it must re-query them. An empty list
        // means "anything may have changed".
        emit_tasks_changed(ctx.events.as_ref(), Vec::new());
    }
}

/// Undo a permanently failed task operation on an existing transaction.
///
/// The shared worker calls this in the same transaction as `mark_failed` when
/// [`TasksService::supports_atomic_rollback`] is set, so a delta can never
/// observe "operation no longer outstanding, optimistic row still present".
fn rollback_conn(conn: &rusqlite::Connection, op: &PendingOp) -> Result<()> {
    let decoded = ops::decode(op)?;
    match decoded {
        TaskOp::Save {
            task_id, snapshot, ..
        } => match snapshot {
            Some(snapshot) => {
                // A newer local write must win over an older snapshot. Every
                // local edit is re-read from the row when its queued save
                // executes, so this operation's payload cannot recover content
                // that a newer one would have replaced.
                if !task_content_owned_by(conn, op, &task_id)? {
                    warn!(
                        op = %op.id,
                        task = %task_id,
                        "keeping local task: another writer owns its content"
                    );
                    return Ok(());
                }
                // The save never removes its own row. If it is gone now, the
                // user deleted it while the operation was being retried;
                // re-inserting the snapshot would resurrect data the user
                // removed.
                if tasks_repo::get(conn, &snapshot.task.id)?.is_none() {
                    warn!(
                        task = %snapshot.task.id,
                        "not restoring task: the row no longer exists"
                    );
                    return Ok(());
                }
                restore_task_content(conn, &snapshot, &op.id)
            }
            None => {
                // Same ownership rule as an edit rollback: deleting the row
                // would erase a newer edit whose save is still queued (that
                // save would then find no row and complete silently). Keeping
                // it lets the newer operation create the task remotely.
                if !task_content_owned_by(conn, op, &task_id)? {
                    warn!(
                        op = %op.id,
                        task = %task_id,
                        "keeping local task: another writer owns its content"
                    );
                    return Ok(());
                }
                // The create may have landed after all: a later save that
                // executed while this one was waiting for a retry creates the
                // object and records its identity, and a delta can match it to
                // this row. The row is then the only local handle on a real
                // provider object, so it must not be removed.
                if let Some(current) = snapshot_task(conn, &task_id)? {
                    if current.remote_id.is_some() || current.href.is_some() {
                        warn!(
                            op = %op.id,
                            task = %task_id,
                            "not removing task: it now carries a provider identity"
                        );
                        return Ok(());
                    }
                }
                let _ = tasks_repo::delete(conn, &task_id);
                Ok(())
            }
        },
        TaskOp::Delete { snapshot } => {
            // The remote DELETE failed permanently and the task is restored,
            // so it must be allowed to sync again.
            store::remove_task_delete_tombstones(
                conn,
                &snapshot.task.list_id,
                snapshot.remote_id.as_deref(),
                snapshot.href.as_deref(),
                snapshot.uid_is_derived,
            )?;
            // A delete that raced an in-flight save no longer owes cleanup
            // once the user-facing row is restored.
            store::remove_task_delete_intent(conn, &snapshot.task.id)?;
            restore_task(conn, &snapshot, Some(&op.id))
        }
        TaskOp::Move {
            task_id,
            from_list_id,
            snapshot,
        } => {
            // A move snapshot is a full row snapshot: restoring it after a
            // newer save or delta would replace content and position the user
            // changed afterwards. Undo the move only while this operation is
            // still the row's last writer.
            if !task_row_owned_by(conn, op, &task_id)? {
                warn!(
                    op = %op.id,
                    task = %task_id,
                    "keeping local task: it was written after the failed move was queued"
                );
                return Ok(());
            }
            if tasks_repo::move_task(conn, &task_id, &from_list_id).is_ok() {
                store::bump_task_row(conn, &task_id, Some(&op.id))?;
            }
            restore_task(conn, &snapshot, Some(&op.id))
        }
        TaskOp::Reorder { .. } => Ok(()),
        TaskOp::ListCreate { list_id } => {
            let _ = task_lists::delete(conn, &list_id);
            Ok(())
        }
        TaskOp::ListUpdate {
            list_id,
            previous_name,
            previous_color,
        } => {
            if task_lists::get(conn, &list_id)?.is_some() {
                task_lists::update(conn, &list_id, Some(&previous_name), Some(&previous_color))?;
            }
            Ok(())
        }
        TaskOp::ListDelete { snapshot } => {
            conn.execute(
                "INSERT OR IGNORE INTO task_list
                     (id, account_id, name, color, is_default, is_read_only,
                      well_known_list_name, sort_order, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                rusqlite::params![
                    snapshot.list.id,
                    snapshot.list.account_id,
                    snapshot.list.name,
                    snapshot.list.color,
                    snapshot.list.is_default as i64,
                    snapshot.list.is_read_only as i64,
                    snapshot.list.well_known_list_name,
                    snapshot.list.sort_order,
                    repo::now_string()
                ],
            )?;
            if let Some(remote_id) = &snapshot.remote_id {
                store::set_task_list(
                    conn,
                    &TaskListRemote::new(
                        snapshot.list.id.clone(),
                        snapshot.list.account_id.clone(),
                        snapshot
                            .provider
                            .clone()
                            .unwrap_or_else(|| "restored".to_string()),
                        remote_id.clone(),
                        snapshot.href.clone(),
                    ),
                )?;
            }
            for task in &snapshot.tasks {
                restore_task(conn, task, Some(&op.id))?;
            }
            Ok(())
        }
    }
}

impl TasksService {
    async fn load_task_list(&self, id: &str) -> Result<TaskList> {
        let id = id.to_string();
        self.ctx
            .db
            .read(move |conn| task_lists::get_required(conn, &id))
            .await
    }

    async fn affected_lists(&self, op: &TaskOp) -> Result<Vec<String>> {
        let mut lists = Vec::new();
        match op {
            TaskOp::Save { task_id, .. } => {
                if let Some(task) = self
                    .snapshot_task(task_id)
                    .await?
                    .map(|snapshot| snapshot.task)
                {
                    lists.push(task.list_id);
                }
            }
            TaskOp::Delete { snapshot } => lists.push(snapshot.task.list_id.clone()),
            TaskOp::Move {
                task_id,
                from_list_id,
                ..
            } => {
                lists.push(from_list_id.clone());
                if let Some(task) = self
                    .snapshot_task(task_id)
                    .await?
                    .map(|snapshot| snapshot.task)
                {
                    lists.push(task.list_id);
                }
            }
            TaskOp::Reorder { list_id, .. } => lists.push(list_id.clone()),
            TaskOp::ListCreate { list_id } => lists.push(list_id.clone()),
            TaskOp::ListUpdate { list_id, .. } => lists.push(list_id.clone()),
            TaskOp::ListDelete { snapshot } => lists.push(snapshot.list.id.clone()),
        }
        lists.sort();
        lists.dedup();
        Ok(lists)
    }
}

/* ─────────────────────────── sync helpers ─────────────────────────── */

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredCursor {
    sync_token: Option<String>,
    ctag: Option<String>,
}

fn cursor_key(list_id: &str) -> String {
    format!("list:{list_id}")
}

fn load_cursor(conn: &rusqlite::Connection, list_id: &str) -> Result<Option<StoredCursor>> {
    let mut stmt =
        conn.prepare("SELECT value FROM sync_state WHERE module = ?1 AND key = ?2 LIMIT 1")?;
    let mut rows = stmt.query(rusqlite::params![SYNC_MODULE, cursor_key(list_id)])?;
    if let Some(row) = rows.next()? {
        let value: String = row.get(0)?;
        return serde_json::from_str::<StoredCursor>(&value)
            .map(Some)
            .map_err(|error| AppError::db(format!("invalid task cursor: {error}")));
    }
    Ok(None)
}

fn save_cursor(
    conn: &rusqlite::Connection,
    list_id: &str,
    sync_token: Option<&str>,
    ctag: Option<&str>,
) -> Result<()> {
    let account_id: String = conn.query_row(
        "SELECT account_id FROM task_list WHERE id = ?1",
        rusqlite::params![list_id],
        |row| row.get(0),
    )?;
    let cursor = StoredCursor {
        sync_token: sync_token.map(str::to_string),
        ctag: ctag.map(str::to_string),
    };
    let encoded = serde_json::to_string(&cursor)
        .map_err(|error| AppError::internal(format!("cannot encode task cursor: {error}")))?;
    sync_state::set(
        conn,
        &account_id,
        SYNC_MODULE,
        &cursor_key(list_id),
        &encoded,
    )
}

fn create_remote_list(
    conn: &rusqlite::Connection,
    account_id: &str,
    remote: &RemoteTaskList,
) -> Result<TaskList> {
    let list = task_lists::create(
        conn,
        account_id,
        &remote.name,
        &remote
            .color
            .clone()
            .unwrap_or_else(|| "#0078D4".to_string()),
    )?;
    if remote.read_only {
        conn.execute(
            "UPDATE task_list SET is_read_only = 1 WHERE id = ?1",
            rusqlite::params![list.id],
        )?;
    }
    apply_well_known(conn, &list.id, &remote.well_known_list_name)?;
    task_lists::get_required(conn, &list.id)
}

/// Persist a provider's built-in list marker and keep `is_default` aligned
/// with it.
///
/// `task_lists::create` marks whichever list was inserted first as the
/// account default, which is only correct by accident. Graph names the
/// default explicitly (`wellknownListName = defaultList`), so that marker
/// decides instead.
fn apply_well_known(conn: &rusqlite::Connection, list_id: &str, well_known: &str) -> Result<()> {
    let current = task_lists::get_required(conn, list_id)?;
    if current.well_known_list_name != well_known {
        conn.execute(
            "UPDATE task_list SET well_known_list_name = ?2, updated_at = ?3 WHERE id = ?1",
            rusqlite::params![list_id, well_known, repo::now_string()],
        )?;
    }
    if well_known == nuncio_core::models::WELL_KNOWN_DEFAULT && !current.is_default {
        task_lists::set_default(conn, list_id)?;
    }
    Ok(())
}

/// Content type for an attachment, guessed from its extension.
///
/// Graph stores whatever we send, and the client later opens the cached file
/// through the OS, so the type only has to be good enough to be sensible.
fn attachment_content_type(name: &str) -> &'static str {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "pdf" => "application/pdf",
        "txt" | "md" | "log" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "zip" => "application/zip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

/// Queue a task operation with a receipt-order sequence that cannot rewind.
///
/// `pending_op.seq` is normally `MAX(seq) + 1`, so `prune_finished` deleting
/// settled rows lets a later operation take an older number. Save receipts
/// persist their operation's sequence to order confirmations, and a reused
/// number made a past confirmation look like a future one (the new receipt was
/// dropped, the real UID/ETag never reached the row, and a later delete then
/// let the real object resurrect the task). Task operations therefore take
/// their sequence from [`store::next_task_write_seq`], a PIM-owned high-water
/// mark that housekeeping never touches.
///
/// The sequence belongs to the *enqueue* of the operation, not to the moment
/// its response arrives: "only an operation queued later supersedes this one"
/// is the ordering receipts rely on. Allocating a fresh number when the
/// receipt is written would let a late response from an older operation
/// outrank the newer operation that was already queued.
fn enqueue_task_op(
    conn: &rusqlite::Connection,
    op_id: Option<&str>,
    account_id: &str,
    op: &TaskOp,
    description: String,
) -> Result<()> {
    let new_op = NewPendingOp::new(account_id, op.kind(), ops::to_value(op)?, description);
    let queued = match op_id {
        Some(op_id) => repo::pending_ops::enqueue_with_id(conn, op_id, new_op)?,
        None => repo::pending_ops::enqueue(conn, new_op)?,
    };
    let sequence = store::next_task_write_seq(conn)?;
    conn.execute(
        "UPDATE pending_op SET seq = ?2 WHERE id = ?1",
        rusqlite::params![queued.id, sequence],
    )?;
    Ok(())
}

/// Queue a task operation on an existing transaction so the local row and its
/// upload intent commit atomically.
fn enqueue_op_sync(
    conn: &rusqlite::Connection,
    account_id: &str,
    op: &TaskOp,
    description: String,
) -> Result<()> {
    enqueue_task_op(conn, None, account_id, op, description)
}

/// Queue an operation under an id chosen before the local row was written, on
/// an existing transaction.
///
/// The row records that id as its content/row owner, so a rollback can still
/// tell "the newer save that succeeded and was pruned" apart from "nobody
/// wrote after me" without consulting the queue. Both the row write and this
/// insert must commit or roll back together: a row that names an operation
/// which was never queued cannot be recovered.
fn enqueue_op_with_id_sync(
    conn: &rusqlite::Connection,
    op_id: &str,
    account_id: &str,
    op: &TaskOp,
    description: String,
) -> Result<()> {
    enqueue_task_op(conn, Some(op_id), account_id, op, description)
}

/// The save operation that a `task_delete` must account for.
///
/// A save without a persisted send intent cannot have issued a wire command:
/// cancel it, so it cannot upload after the local row is gone. A save that did
/// send (or whose outcome is ambiguous) may own a remote object; its earliest
/// operation is returned as the cleanup owner and a delete intent is recorded.
fn task_save_cleanup_owner(
    conn: &rusqlite::Connection,
    account_id: &str,
    task_id: &str,
) -> Result<Option<(String, u32)>> {
    let mut owner: Option<(String, u32)> = None;
    for op in repo::pending_ops::list(conn, Some(account_id))? {
        if op.kind != ops::KIND_SAVE {
            continue;
        }
        let Ok(TaskOp::Save {
            task_id: saved_id, ..
        }) = ops::decode(&op)
        else {
            continue;
        };
        if saved_id != task_id {
            continue;
        }
        if op.send_intent || op.outcome_unknown {
            if owner.is_none() {
                owner = Some((op.id.clone(), op.attempts));
            }
            continue;
        }
        if repo::pending_ops::is_outstanding(&op) {
            repo::pending_ops::cancel(conn, &op.id)?;
        }
    }
    Ok(owner)
}

/// Local task ids whose edit has not reached the server yet.
///
/// A delta response describes the server, which does not know about an edit
/// that is still queued. Replacing the local steps from that response would
/// silently discard the user's change before the worker ever uploads it, which
/// is how a freshly added step could disappear. Only steps are protected here:
/// the other fields keep their pre-existing last-writer-wins behaviour.
fn tasks_with_pending_writes(
    conn: &rusqlite::Connection,
    account_id: &str,
) -> Result<HashSet<String>> {
    let mut ids = HashSet::new();
    for op in repo::pending_ops::list(conn, Some(account_id))? {
        if op.kind != ops::KIND_SAVE
            || !matches!(
                op.state,
                PendingOpState::Pending | PendingOpState::InProgress
            )
        {
            continue;
        }
        // Decode through `ops` instead of reading a JSON key by hand: the
        // payload keeps snake_case struct fields (`task_id`) even though the
        // variant tag is camelCase, so a hand-written `taskId` silently
        // matched nothing and disabled this whole guard.
        if let Ok(TaskOp::Save { task_id, .. }) = ops::decode(&op) {
            ids.insert(task_id);
        }
    }
    Ok(ids)
}

/// Apply one delta and commit its cursor in the same transaction.
///
/// An `incomplete` round is uncertain: a listed object's identity could not be
/// read, so its changed data must still land, but the cursor must stay where it
/// was. Acknowledging it would let the provider move on and the object would
/// never be retried.
fn commit_delta(
    conn: &rusqlite::Connection,
    account_id: &str,
    list: &TaskList,
    caps: TaskCapabilities,
    delta: &TaskDelta,
) -> Result<()> {
    let cursor_may_advance = apply_delta(conn, account_id, list, caps, delta)?;
    if !delta.incomplete && cursor_may_advance {
        save_cursor(
            conn,
            &list.id,
            delta.sync_token.as_deref(),
            delta.ctag.as_deref(),
        )?;
    }
    Ok(())
}

/// Apply one batch.
///
/// Returns `false` when the cursor must stay where it was even though the
/// batch was not marked `incomplete`: an unattributed uncertain create may be
/// one of the imported objects, so the batch must be replayed once the delete
/// intent resolves (or its bounded release window elapses).
fn apply_delta(
    conn: &rusqlite::Connection,
    account_id: &str,
    list: &TaskList,
    caps: TaskCapabilities,
    delta: &TaskDelta,
) -> Result<bool> {
    // Expired tombstones no longer suppress anything; drop them before the
    // batch is matched so a long-lived database does not accumulate rows.
    let now = Utc::now();
    store::prune_expired_task_delete_tombstones(conn, now)?;
    // An intent whose create receipt was lost and whose owner can no longer
    // act is released after a bounded window; until then the list defers new
    // imports below.
    let released = store::release_expired_unknown_task_delete_intents(
        conn,
        account_id,
        store::TASK_DELETE_UNKNOWN_RELEASE_TTL_SECONDS,
        now,
    )?;
    if released > 0 {
        warn!(
            released,
            "released unaddressable task delete intents after their wall-clock TTL"
        );
    }
    let tombstones = store::active_task_delete_tombstones(conn, &list.id, now)?;
    // A create whose receipt was lost has no identity to match against. New
    // server objects cannot be told apart from the deleted task, so the list
    // holds its cursor and does not import them until the ambiguity resolves.
    let defer_unattributed_imports = store::has_unattributed_task_delete_intent(conn, &list.id)?;
    let mut cursor_blocked = false;

    // Deletions: task rows first, then child steps (Google subtasks).
    for remote_id in &delta.deleted_remote_ids {
        // `deleted_remote_ids` always names a **real** provider UID (CalDAV
        // reports its UID-less resources through `deleted_hrefs`). Restrict the
        // lookup to the same identity kind so a delete cannot consume a
        // UID-less row whose href-derived placeholder happens to spell the
        // deleted UID.
        if let Some(task_id) = store::task_by_real_remote_id(conn, &list.id, remote_id)? {
            tasks_repo::delete(conn, &task_id)?;
            continue;
        }
        if let Some((step_id, task_id, _)) = store::step_lookup_by_remote(conn, remote_id)? {
            if let Some(mut parent) = tasks_repo::get(conn, &task_id)? {
                parent.steps.retain(|step| step.id != step_id);
                let input = input_from_task(&parent);
                tasks_repo::update(conn, &parent.id, &input)?;
                store::bump_task_content(conn, &parent.id, None)?;
                store::delete_step_mapping(conn, &step_id)?;
            }
        }
    }
    for href in &delta.deleted_hrefs {
        // An href names one resource regardless of identity kind, and earlier
        // builds could have left two rows for it. Delete every row at this
        // normalized href; deleting only the first would leak the other
        // forever, because the server will not report this deletion again.
        for task_id in store::task_ids_by_href(conn, &list.id, href)? {
            let _ = tasks_repo::delete(conn, &task_id);
        }
    }

    let pending_writes = tasks_with_pending_writes(conn, account_id)?;
    // Identity kind is part of the key: a derived placeholder and a real UID
    // that spell the same string are different resources.
    let mut seen_remote: HashSet<(String, bool)> = HashSet::new();
    let mut seen_steps: HashSet<String> = HashSet::new();
    let mut incomplete_steps: HashSet<String> = HashSet::new();
    // Tasks a tombstone suppressed even though a live local row matched. Their
    // steps were not enumerated by this batch, so the full-listing step prune
    // below must not treat those steps as provider-side absences.
    let mut suppressed_tasks: HashSet<String> = HashSet::new();
    let mut local_by_remote: HashMap<(String, bool), String> = HashMap::new();
    let mut parents = Vec::new();
    let mut children = Vec::new();
    for remote in &delta.tasks {
        if remote.parent_remote_id.is_some() {
            children.push(remote);
        } else {
            parents.push(remote);
        }
    }

    for remote in parents {
        let Some(remote_id) = remote.remote_id.clone() else {
            continue;
        };
        let existing = find_task(conn, list, remote)?;
        // The user deleted this object locally; a delta captured before the
        // remote DELETE (or a re-delivery under another href spelling) must
        // not resurrect it.
        if tombstones.iter().any(|tombstone| {
            tombstone.matches(
                Some(&remote_id),
                remote.href.as_deref(),
                remote.uid_is_derived,
            )
        }) {
            // A tombstone suppresses an import/update, but it is not evidence
            // that a *live* local row is gone. A task moved away and back can
            // still match an older "left this list" tombstone, especially when
            // the provider keeps the same id (Google's move). Register the
            // existing row as present so the full-listing prune does not
            // delete it and its local step mappings.
            if let Some(existing) = &existing {
                // Register the identity the row actually stores: `find_task`
                // can fall back to a duplicate at the same href, whose kind
                // differs from the incoming one, and the prune compares the
                // stored kind.
                seen_remote.insert((remote_id.clone(), remote.uid_is_derived));
                if let Some(snapshot) = snapshot_task(conn, &existing.id)? {
                    if let Some(stored_id) = &snapshot.remote_id {
                        seen_remote.insert((stored_id.clone(), snapshot.uid_is_derived));
                    }
                }
                local_by_remote.insert(
                    (remote_id.clone(), remote.uid_is_derived),
                    existing.id.clone(),
                );
                suppressed_tasks.insert(existing.id.clone());
            }
            continue;
        }
        seen_remote.insert((remote_id.clone(), remote.uid_is_derived));
        if existing.is_none() && defer_unattributed_imports {
            // An uncertain create may already be this object. Importing it
            // would resurrect the task the user deleted; the batch is deferred
            // (cursor held) until the intent resolves.
            cursor_blocked = true;
            continue;
        }
        let id = existing
            .as_ref()
            .map(|task| task.id.clone())
            .unwrap_or_else(new_id);
        let write_in_flight = pending_writes.contains(&id);
        let mut task = remote.task.clone();
        task.id = id.clone();
        task.list_id = list.id.clone();
        task.account_id = account_id.to_string();
        if let Some(existing) = &existing {
            if write_in_flight {
                // A queued save owns every user-editable field, not just the
                // steps: applying this (older) server copy would undo the
                // edit — a changed reminder, for instance — before the worker
                // ever uploads it. Only the sync metadata is refreshed below.
                task = existing.clone();
                task.id = id.clone();
                task.list_id = list.id.clone();
                task.account_id = account_id.to_string();
            } else {
                map::preserve_local_fields(&mut task, existing, caps);
                task.created_at = existing.created_at;
                if caps.steps && !remote.steps_complete {
                    task.steps = existing.steps.clone();
                }
            }
        }
        let existing_steps: Vec<TaskStep> = existing
            .as_ref()
            .map(|task| task.steps.clone())
            .unwrap_or_default();
        let mut pending_step_mappings: Vec<(String, String)> = Vec::new();
        if write_in_flight {
            task.steps = existing_steps.clone();
        } else if caps.steps && remote.steps_complete {
            let has_mappings = !store::step_mappings_for_task(conn, &id)?.is_empty();
            if remote.task.steps.is_empty() && has_mappings {
                // A delta response can omit unchanged subtasks.  Never drop
                // local steps (and their mappings) in that case: doing so
                // would make the next reconcile delete them remotely.
                task.steps = existing_steps.clone();
            } else {
                for step in &remote.task.steps {
                    if !step.id.trim().is_empty() {
                        seen_steps.insert(step.id.clone());
                    }
                }
                task.steps = map_steps(
                    conn,
                    &id,
                    &task.steps,
                    &existing_steps,
                    &mut pending_step_mappings,
                )?;
            }
        } else if !caps.steps {
            task.steps = existing_steps;
        }
        let saved =
            tasks_repo::upsert_remote(conn, &task, remote.etag.as_deref(), Some(&remote_id))?;
        store::bump_task_content(conn, &saved.id, None)?;
        for (local_step, remote_step) in pending_step_mappings {
            store::set_step_mapping(conn, &local_step, &saved.id, &remote_step)?;
        }
        // The row now carries this object's identity; record where it came
        // from so later cleanup and matching cannot cross the kind boundary.
        store::set_task_uid_is_derived(conn, &saved.id, remote.uid_is_derived)?;
        if let Some(href) = &remote.href {
            store::set_task_href(conn, &saved.id, href)?;
        }
        if caps.steps && !remote.steps_complete {
            incomplete_steps.insert(saved.id.clone());
        }
        local_by_remote.insert((remote_id, remote.uid_is_derived), saved.id);
    }

    for remote in children {
        let Some(remote_id) = remote.remote_id.clone() else {
            continue;
        };
        let Some(parent_remote) = remote.parent_remote_id.clone() else {
            continue;
        };
        if tombstones.iter().any(|tombstone| {
            tombstone.matches(
                Some(&remote_id),
                remote.href.as_deref(),
                remote.uid_is_derived,
            )
        }) {
            // Same rule as a suppressed parent: a locally mapped step still
            // exists, so it must not be pruned as if the provider omitted it.
            if store::step_lookup_by_remote(conn, &remote_id)?.is_some() {
                seen_steps.insert(remote_id.clone());
            }
            continue;
        }
        let parent_id = local_by_remote
            .get(&(parent_remote.clone(), false))
            .or_else(|| local_by_remote.get(&(parent_remote.clone(), true)))
            .cloned()
            .or_else(|| {
                store::task_by_real_remote_id(conn, &list.id, &parent_remote)
                    .ok()
                    .flatten()
                    .or_else(|| {
                        store::task_by_remote_id_kind(conn, &list.id, &parent_remote, true)
                            .ok()
                            .flatten()
                    })
            });
        match parent_id {
            Some(parent_id) => {
                // A mapped step is an update to something this client already
                // knows. A step without a mapping is a *new* provider object,
                // exactly like a top-level task with no local row: it may be
                // the object the receipt-less create committed, so defer it
                // and hold the cursor while the intent can still resolve.
                let mapped_step = store::step_local_id(conn, &parent_id, &remote_id)?;
                if mapped_step.is_none() && defer_unattributed_imports {
                    cursor_blocked = true;
                    continue;
                }
                seen_remote.insert((remote_id.clone(), remote.uid_is_derived));
                seen_steps.insert(remote_id.clone());
                if pending_writes.contains(&parent_id) {
                    // Google reports subtasks as their own rows, so they reach
                    // this loop rather than the parent branch above. A queued
                    // save still owns the local steps: applying the older
                    // remote state here would undo the user's edit before it
                    // is uploaded.
                    continue;
                }
                let step_id = mapped_step.unwrap_or_else(new_id);
                store::set_step_mapping(conn, &step_id, &parent_id, &remote_id)?;
                let mut parent = tasks_repo::get_required(conn, &parent_id)?;
                // Keep the local position of a step we already know: the
                // provider does not store a manual order, so its index here
                // would otherwise overwrite the user's drag.
                let title = remote.task.title.clone();
                let is_completed = remote.task.status == TaskStatus::Completed;
                match parent.steps.iter().position(|step| step.id == step_id) {
                    Some(index) => {
                        let sort_order = parent.steps[index].sort_order;
                        parent.steps[index] = TaskStep {
                            id: step_id.clone(),
                            title,
                            is_completed,
                            sort_order,
                        };
                    }
                    None => {
                        let sort_order = parent
                            .steps
                            .iter()
                            .map(|step| step.sort_order + 1)
                            .max()
                            .unwrap_or(0);
                        parent.steps.push(TaskStep {
                            id: step_id.clone(),
                            title,
                            is_completed,
                            sort_order,
                        });
                    }
                }
                let meta = tasks_repo::sync_meta(conn, &parent_id)?;
                tasks_repo::upsert_remote(
                    conn,
                    &parent,
                    meta.as_ref().and_then(|meta| meta.etag.as_deref()),
                    meta.as_ref().and_then(|meta| meta.remote_id.as_deref()),
                )?;
                store::bump_task_content(conn, &parent_id, None)?;
            }
            None => {
                // Parent not synced yet: keep the child as a top-level task
                // rather than dropping data; a later full sync attaches it.
                let existing = find_task(conn, list, remote)?;
                if existing.is_none() && defer_unattributed_imports {
                    // Same ambiguity as a top-level object: this step id may
                    // belong to the create whose receipt was lost.
                    cursor_blocked = true;
                    continue;
                }
                let id = existing
                    .as_ref()
                    .map(|task| task.id.clone())
                    .unwrap_or_else(new_id);
                let mut task = remote.task.clone();
                task.id = id.clone();
                task.list_id = list.id.clone();
                task.account_id = account_id.to_string();
                if let Some(existing) = &existing {
                    map::preserve_local_fields(&mut task, existing, caps);
                    task.created_at = existing.created_at;
                }
                let saved = tasks_repo::upsert_remote(
                    conn,
                    &task,
                    remote.etag.as_deref(),
                    Some(&remote_id),
                )?;
                store::bump_task_content(conn, &saved.id, None)?;
                store::set_task_uid_is_derived(conn, &saved.id, remote.uid_is_derived)?;
                local_by_remote
                    .insert((remote_id.clone(), remote.uid_is_derived), saved.id.clone());
                seen_remote.insert((remote_id, remote.uid_is_derived));
            }
        }
    }

    // An incomplete round proves nothing about the objects whose identity
    // could not be read; pruning would treat "not downloaded" as "gone". The
    // cursor is likewise held by `commit_delta`.
    if delta.full_sync && !delta.incomplete {
        // A provider that performs a full listing does not necessarily
        // re-deliver unchanged objects: the DAV ETag fallback only fetches
        // hrefs whose ETag moved. Presence therefore has two sources: the
        // objects that entered this batch (`seen_remote`/`seen_steps`) and the
        // authoritative server listing (`listed_hrefs`). Only an object in
        // neither is stale.
        let listed_hrefs: HashSet<String> = delta
            .listed_hrefs
            .iter()
            .map(|href| crate::dav::href_key(href))
            .collect();
        let mut stmt = conn.prepare(
            "SELECT t.id, t.remote_id, r.href, t.uid_is_derived
             FROM task t LEFT JOIN pim_task_resource r ON r.task_id = t.id
             WHERE t.list_id = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![list.id])?;
        let mut stale = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let remote_id: Option<String> = row.get(1)?;
            let href: Option<String> = row.get(2)?;
            let uid_is_derived: bool = row.get(3)?;
            // Compare identities the same way the upsert does: a listed href
            // is present for both identity kinds, and a real UID is only
            // matched by another real UID (or a provider step id, which is
            // always real). A derived placeholder that merely spells the same
            // string as another resource's real UID must not keep this row
            // alive during a full listing.
            let seen_by_identity = remote_id.as_ref().is_some_and(|remote_id| {
                seen_remote.contains(&(remote_id.clone(), uid_is_derived))
                    || (!uid_is_derived && seen_steps.contains(remote_id))
            });
            let seen = seen_by_identity
                || href
                    .as_ref()
                    .is_some_and(|href| listed_hrefs.contains(&crate::dav::href_key(href)));
            // A purely local task (no remote identity) is never pruned by a
            // provider listing.
            if !seen && (remote_id.is_some() || href.is_some()) {
                stale.push(id);
            }
        }
        drop(rows);
        drop(stmt);
        for id in stale {
            let _ = tasks_repo::delete(conn, &id);
        }

        // Prune step mappings whose provider child disappeared.
        if caps.steps {
            let tasks = tasks_repo::query(
                conn,
                &TaskQuery {
                    list_id: Some(list.id.clone()),
                    smart: None,
                    include_completed: true,
                    today: map::today(),
                },
            )?;
            for task in tasks {
                // Steps of a task whose write is still queued were kept above,
                // so they are missing from `seen_steps`; pruning them here
                // would delete the mappings the queued operation relies on. A
                // task a tombstone suppressed was not enumerated either (its
                // row is still present), so its steps prove nothing about the
                // provider's current child set.
                if incomplete_steps.contains(&task.id)
                    || suppressed_tasks.contains(&task.id)
                    || pending_writes.contains(&task.id)
                {
                    continue;
                }
                let mappings = store::step_mappings_for_task(conn, &task.id)?;
                let missing: Vec<(String, String)> = mappings
                    .into_iter()
                    .filter(|(_, remote_step)| !seen_steps.contains(remote_step))
                    .collect();
                if missing.is_empty() {
                    continue;
                }
                let mut updated = task.clone();
                for (step_id, _) in &missing {
                    updated.steps.retain(|step| &step.id != step_id);
                }
                let input = input_from_task(&updated);
                tasks_repo::update(conn, &task.id, &input)?;
                store::bump_task_content(conn, &task.id, None)?;
                for (step_id, _) in missing {
                    store::delete_step_mapping(conn, &step_id)?;
                }
            }
        }
    }
    Ok(!cursor_blocked)
}

/// Assign local ids to provider steps and remember the mapping.
fn map_steps(
    conn: &rusqlite::Connection,
    task_id: &str,
    steps: &[TaskStep],
    existing: &[TaskStep],
    mappings: &mut Vec<(String, String)>,
) -> Result<Vec<TaskStep>> {
    // Step order is a local property: none of the supported providers exposes
    // a writable order for checklist items (Graph rejects `orderDateTime`
    // outright), so re-applying the server's order on every sync would undo
    // the user's drag. Existing steps keep their local order; only steps this
    // client has never seen are appended in the order the server lists them.
    let mut orders: HashMap<&str, i64> = existing
        .iter()
        .map(|step| (step.id.as_str(), step.sort_order))
        .collect();
    let mut next_order = existing
        .iter()
        .map(|step| step.sort_order + 1)
        .max()
        .unwrap_or(0);
    let mut out = Vec::with_capacity(steps.len());
    for step in steps {
        let remote_id = step.id.trim();
        if remote_id.is_empty() {
            let sort_order = next_order;
            next_order += 1;
            out.push(TaskStep {
                id: new_id(),
                title: step.title.clone(),
                is_completed: step.is_completed,
                sort_order,
            });
            continue;
        }
        let local_id = store::step_local_id(conn, task_id, remote_id)?.unwrap_or_else(new_id);
        // Mappings are written after the task row exists (FK); collecting them
        // here also covers brand-new tasks with embedded checklist items.
        mappings.push((local_id.clone(), remote_id.to_string()));
        let sort_order = match orders.remove(local_id.as_str()) {
            Some(order) => order,
            None => {
                let order = next_order;
                next_order += 1;
                order
            }
        };
        out.push(TaskStep {
            id: local_id,
            title: step.title.clone(),
            is_completed: step.is_completed,
            sort_order,
        });
    }
    Ok(out)
}

fn find_task(
    conn: &rusqlite::Connection,
    list: &TaskList,
    remote: &RemoteTask,
) -> Result<Option<Task>> {
    // One href is one resource. Prefer a row of the incoming identity kind
    // when historical duplicates exist; either way updating it in place is
    // correct, because the provider addressed the same URL.
    if let Some(href) = remote.href.as_deref().filter(|value| !value.is_empty()) {
        let candidates = store::task_candidates_by_href(conn, &list.id, href)?;
        let row = candidates
            .iter()
            .find(|(_, stored_is_derived)| *stored_is_derived == remote.uid_is_derived)
            .or_else(|| candidates.first());
        if let Some((id, _)) = row {
            return tasks_repo::get(conn, id);
        }
    }
    // A derived id is the normalized href. Without an href it names no
    // resource, and it must never match a real provider UID that happens to
    // spell the same string.
    if !remote.uid_is_derived {
        if let Some(remote_id) = remote
            .remote_id
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            if let Some(id) = store::task_by_real_remote_id(conn, &list.id, remote_id)? {
                return tasks_repo::get(conn, &id);
            }
        }
    }
    Ok(None)
}

fn input_from_task(task: &Task) -> TaskInput {
    TaskInput {
        title: task.title.clone(),
        notes: task.notes.clone(),
        importance: task.importance,
        due_date: task.due_date.clone(),
        reminder_at: task.reminder_at,
        recurrence: task.recurrence.clone(),
        my_day_date: task.my_day_date.clone(),
        steps: task.steps.clone(),
    }
}

/// Does this account push task edits to a provider (and therefore queue an
/// operation for every local write)?
///
/// Used inside the same transaction as the row write so the operation id can
/// be recorded on the row before the operation itself is enqueued.
fn task_provider_account(conn: &rusqlite::Connection, account_id: &str) -> Result<bool> {
    let account = repo::accounts::get_required(conn, account_id)?;
    Ok(matches!(
        backend_for(&account, DavService::Calendar),
        Backend::Google | Backend::Graph | Backend::Dav
    ))
}

fn snapshot_task(conn: &rusqlite::Connection, id: &str) -> Result<Option<TaskSnapshot>> {
    let Some(task) = tasks_repo::get(conn, id)? else {
        return Ok(None);
    };
    let meta = tasks_repo::sync_meta(conn, id)?;
    Ok(Some(TaskSnapshot {
        task,
        etag: meta.as_ref().and_then(|meta| meta.etag.clone()),
        remote_id: meta.as_ref().and_then(|meta| meta.remote_id.clone()),
        href: store::task_href(conn, id)?,
        uid_is_derived: store::task_uid_is_derived(conn, id)?,
    }))
}

/// Roll back a failed save's content, keeping the identity the current row
/// holds.
///
/// The snapshot is taken when the save is queued, but an earlier save for the
/// same task may have its upload confirmed afterwards (a second edit within the
/// same transaction window is enough for the stale snapshot to be queued).
/// Restoring the snapshot's `remote_id`/`href`/`etag` would then forget the
/// real UID and ETag the provider already acknowledged — and `uid_is_derived`
/// would flip back to `true`, so the next edit would allocate a *second* UID
/// for the same VTODO.
///
/// Those columns describe the server object, not local content: the current
/// row always wins. The same is true of position (`list_id`, `sort_order`),
/// ownership (`account_id`) and the always-local fields (`my_day_date`,
/// `linked_message_id`): a later move, reorder or My Day toggle may own them
/// now, and the snapshot predates it. Only editable content (title, notes,
/// dates, status, steps) is rolled back from the snapshot.
fn restore_task_content(
    conn: &rusqlite::Connection,
    snapshot: &TaskSnapshot,
    owner: &str,
) -> Result<()> {
    let Some(current) = snapshot_task(conn, &snapshot.task.id)? else {
        return Ok(());
    };
    let mut content = current.task.clone();
    content.title = snapshot.task.title.clone();
    content.notes = snapshot.task.notes.clone();
    content.status = snapshot.task.status;
    content.importance = snapshot.task.importance;
    content.due_date = snapshot.task.due_date.clone();
    content.reminder_at = snapshot.task.reminder_at;
    content.recurrence = snapshot.task.recurrence.clone();
    content.completed_at = snapshot.task.completed_at;
    content.steps = snapshot.task.steps.clone();
    tasks_repo::upsert_remote(
        conn,
        &content,
        current.etag.as_deref(),
        current.remote_id.as_deref(),
    )?;
    store::bump_task_content(conn, &snapshot.task.id, Some(owner))?;
    // Keep the href and identity kind the row currently has; `upsert_remote`
    // only knows the remote id and ETag columns.
    if let Some(href) = current.href.as_deref() {
        store::set_task_href(conn, &snapshot.task.id, href)?;
    }
    store::set_task_uid_is_derived(conn, &snapshot.task.id, current.uid_is_derived)?;
    Ok(())
}

/// May this failed operation still roll back the task's content?
///
/// Two independent pieces of evidence must agree:
///
/// * no later *queued* operation for the same task exists — a newer save
///   already shows its content in the row and re-reads the row when it
///   executes, so it cannot recover content an older snapshot would replace;
/// * the task's persisted `content_op_id` (in `pim_task_content_state`) is
///   still this operation's id. That state survives `prune_finished`, so a
///   newer save that succeeded and was pruned leaves durable proof of
///   ownership behind. Timestamps are never consulted: they have second
///   precision, and the queue row of the newer writer may be gone by the time
///   this failure settles.
fn task_content_owned_by(
    conn: &rusqlite::Connection,
    op: &PendingOp,
    task_id: &str,
) -> Result<bool> {
    if task_has_later_write(conn, op, task_id)? {
        return Ok(false);
    }
    Ok(store::task_content_writer(conn, task_id)?.as_deref() == Some(op.id.as_str()))
}

/// May this failed move still undo its position and restore its snapshot?
///
/// A move does not change content, so it is tracked by `row_op_id` (which is
/// also refreshed by a save receipt and a delta upsert) instead of
/// `content_op_id`. The queued-operation check catches a newer operation that
/// has not run yet; the persisted state catches one that already settled and
/// was pruned.
fn task_row_owned_by(conn: &rusqlite::Connection, op: &PendingOp, task_id: &str) -> Result<bool> {
    if task_has_later_write(conn, op, task_id)? {
        return Ok(false);
    }
    Ok(store::task_row_writer(conn, task_id)?.as_deref() == Some(op.id.as_str()))
}

/// Is there a queued operation after `op` that writes this task?
///
/// Both accounts are scanned: a cross-account move queues its target-side
/// operation under another account but still writes this task.
fn task_has_later_write(
    conn: &rusqlite::Connection,
    op: &PendingOp,
    task_id: &str,
) -> Result<bool> {
    for other in repo::pending_ops::list(conn, None)? {
        if other.id == op.id || other.sequence <= op.sequence {
            continue;
        }
        if !other.kind.starts_with(ops::KIND_PREFIX) {
            continue;
        }
        let Ok(decoded) = ops::decode(&other) else {
            continue;
        };
        if task_op_ids(&decoded).iter().any(|id| id == task_id) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Local task ids an operation writes. An operation that only pushes the local
/// order (`Reorder`) is not listed: the content rollback preserves
/// `sort_order`, so it cannot undo it.
fn task_op_ids(op: &TaskOp) -> Vec<String> {
    match op {
        TaskOp::Save { task_id, .. } | TaskOp::Move { task_id, .. } => vec![task_id.clone()],
        TaskOp::Delete { snapshot } => vec![snapshot.task.id.clone()],
        TaskOp::ListDelete { snapshot } => snapshot
            .tasks
            .iter()
            .map(|task| task.task.id.clone())
            .collect(),
        TaskOp::Reorder { .. } | TaskOp::ListCreate { .. } | TaskOp::ListUpdate { .. } => {
            Vec::new()
        }
    }
}

/// A successful provider write for a task.
struct TaskSaveReceipt {
    remote_id: Option<String>,
    href: Option<String>,
    etag: Option<String>,
    uid_is_derived: bool,
}

/// Record that a successful move removed the task from its source list.
///
/// The default move implementation creates the task in the target list and
/// deletes the source copy; Google's `tasks.move` relinks the same object. In
/// both cases the source list no longer contains it, but a delta for the
/// source list that was fetched before the move still names the old
/// `(list, identity)` pair and would re-import the task there.
///
/// The tombstone is a *settled* one: the provider-side removal already
/// completed, so its suppression window starts now. It is scoped to the source
/// list, so the task still imports/updates normally in the target list even
/// when both lists share the same provider id (Google).
fn record_task_move_tombstone(
    conn: &rusqlite::Connection,
    account_id: &str,
    from_list_id: &str,
    to_list_id: &str,
    remote_id: Option<&str>,
    href: Option<&str>,
    uid_is_derived: bool,
) -> Result<()> {
    if from_list_id == to_list_id {
        // A move within the same list must not hide the task from it.
        return Ok(());
    }
    if remote_id.is_none() && href.is_none() {
        // The source copy never had a remote identity, so no delta can name
        // it.
        return Ok(());
    }
    store::insert_settled_task_move_tombstone(
        conn,
        account_id,
        from_list_id,
        remote_id,
        href,
        uid_is_derived,
        Utc::now() + chrono::Duration::seconds(store::TASK_DELETE_TOMBSTONE_TTL_SECONDS),
    )
}

/// May this save receipt still write the task's identity?
///
/// The row writer alone is not enough. A delta upsert attributes its write to
/// the external writer (`""`) even when it is only echoing the identity the
/// save started from; treating that as "another operation already confirmed"
/// discarded the acknowledgement of a real UID upgrade, and a later delete
/// then only suppressed the derived placeholder while the real object came
/// back. The decision therefore combines three pieces of evidence:
///
/// * the identity baseline this save started from (`base`),
/// * the current row writer (who wrote last, and whether that operation is
///   still outstanding),
/// * the last identity another operation **actually** confirmed
///   (`receipt_op_id` + its durable write sequence).
///
/// A genuinely newer confirmation still wins: only a receipt whose operation
/// was queued after this one may keep this receipt out when the row writer was
/// overwritten by a delta. The sequence used here comes from
/// [`store::next_task_write_seq`], a high-water mark outside the pending queue:
/// a queue sequence allocated by `MAX(seq) + 1` rewinds after `prune_finished`
/// and would make a past confirmation look like a future one.
fn save_receipt_is_superseded(
    conn: &rusqlite::Connection,
    id: &str,
    op_id: &str,
    base: &TaskSnapshot,
) -> Result<bool> {
    let confirmed = store::task_confirmed_receipt(conn, id)?;
    // This operation already confirmed an identity and is being replayed (for
    // example after a sync rewrote the owner); re-recording it is harmless.
    // A *different* operation's receipt is decided below, because matching
    // only the resource id would still let an older response regress the
    // confirmed ETag.
    if confirmed
        .as_ref()
        .is_some_and(|confirmed| confirmed.op_id == op_id)
    {
        return Ok(false);
    }
    match store::task_row_writer(conn, id)? {
        // This operation wrote the row last.
        Some(owner) if owner == op_id => Ok(false),
        // The external writer: a delta upsert (or a local-only edit). A delta
        // copies what the provider listed and never confirms an in-flight PUT,
        // so on its own it cannot outrank a real acknowledgement. Only an
        // identity another operation confirmed after this save was queued can.
        Some(owner) if owner.is_empty() => {
            let Some(confirmed) = confirmed else {
                return Ok(false);
            };
            let our_sequence = repo::pending_ops::get(conn, op_id)?.map(|op| op.sequence);
            match (confirmed.op_sequence, our_sequence) {
                // Durable enqueue order is authoritative: only a later
                // operation's receipt supersedes this one. Comparing
                // identities alone would let an older receipt regress the
                // confirmed ETag. Both values come from the same monotonic
                // source and survive queue pruning.
                (Some(confirmed), Some(ours)) => Ok(confirmed > ours),
                // Without ordering evidence, the baseline decides: a
                // confirmation of the identity this save started from is an
                // older echo the delta re-delivered, so this receipt is the
                // only new acknowledgement. Any other confirmed identity is
                // protected.
                (None, _) => Ok(!confirmed.same_identity(
                    base.remote_id.as_deref(),
                    base.href.as_deref(),
                    base.uid_is_derived,
                )),
                // Our operation has no queue row but the confirmation does:
                // keep the confirmed identity.
                (Some(_), None) => Ok(true),
            }
        }
        // A different operation wrote the row. While it is outstanding it has
        // not confirmed anything, so this receipt is the only acknowledgement.
        // Once it settled (or its state survived pruning) it already owns the
        // identity.
        Some(owner) => Ok(repo::pending_ops::get(conn, &owner)?
            .is_none_or(|op| !repo::pending_ops::is_outstanding(&op))),
        // No state row: a row written before the revision scheme is unowned and
        // may still be claimed.
        None => Ok(false),
    }
}

/// Persist a save receipt, tolerating a row the user deleted while the save
/// was on the wire.
///
/// When the row still exists the receipt is written to it as before. When it
/// was deleted, writing to `task` would fail the href foreign key and the
/// worker's rollback would re-insert the snapshot — resurrecting a task the
/// user deleted. Instead the receipt is attached to the recorded delete
/// intent, the pending tombstone is re-keyed to the confirmed identity, and
/// the queued DELETE (or a fresh one, for a create that had no identity yet)
/// is pointed at the real object.
fn record_task_save_receipt(
    conn: &rusqlite::Connection,
    account_id: &str,
    op_id: &str,
    base: &TaskSnapshot,
    receipt: &TaskSaveReceipt,
) -> Result<()> {
    let id = &base.task.id;
    // Only an operation whose confirmation is the newest one may write the
    // row's identity. A newer save that already confirmed its `remote_id`
    // owns the row (`row_op_id`) and the state survives pruning, so a late
    // receipt from an older save must not replace the confirmed identity and
    // the identity kind.
    //
    // A newer operation that is still queued or on the wire has not confirmed
    // anything yet: this receipt is the only identity the provider has
    // acknowledged, and a later failure of that newer save must keep it. No
    // state row (a row that predates migration 0014) means the writer is
    // unknown; per the content-state contract that row is unowned and may
    // still be claimed.
    if save_receipt_is_superseded(conn, id, op_id, base)? {
        let owner = store::task_row_writer(conn, id)?.unwrap_or_default();
        warn!(
            task = %id,
            op = %op_id,
            owner,
            "dropping a late save receipt; a newer writer already confirmed the task identity"
        );
        return Ok(());
    }
    let updated = conn.execute(
        "UPDATE task SET remote_id = COALESCE(?2, remote_id),
                         etag = COALESCE(?3, etag),
                         updated_at = ?4
         WHERE id = ?1",
        rusqlite::params![id, receipt.remote_id, receipt.etag, repo::now_string()],
    )?;
    if updated != 0 {
        // A receipt carries identity (UID/ETag), not content, so it never
        // changes the content owner. It is still a row write: a failed move
        // must not undo a row whose identity a later operation confirmed. The
        // helper also records the acknowledged identity separately from the
        // content/delta writer, so a later sync cannot erase the evidence.
        let sequence = repo::pending_ops::get(conn, op_id)?.map(|op| op.sequence);
        store::record_task_receipt_writer(
            conn,
            id,
            op_id,
            sequence,
            receipt.remote_id.as_deref(),
            receipt.href.as_deref(),
            receipt.uid_is_derived,
        )?;
        if receipt.remote_id.is_some() {
            // The row now carries this object's identity; record where it came
            // from so later cleanup and matching cannot cross the kind
            // boundary.
            store::set_task_uid_is_derived(conn, id, receipt.uid_is_derived)?;
        }
        if let Some(href) = &receipt.href {
            store::set_task_href(conn, id, href)?;
        }
        return Ok(());
    }
    let Some(intent) = store::task_delete_intent(conn, id)? else {
        // The row was removed by something other than a user delete (a delta
        // prune). There is no local cleanup to own; the next delta imports the
        // provider object if it still exists.
        warn!(
            task = %id,
            "task row disappeared during a save and no delete intent exists; \
             dropping the provider receipt"
        );
        return Ok(());
    };
    if let Some(owner) = intent.owner_op_id.as_deref() {
        if owner != op_id {
            // Another operation may own the cleanup, but this operation did
            // reach the provider and produced its own addressable object. Keep
            // the accurate identity so the object can be deleted.
            warn!(
                task = %id,
                op = %op_id,
                owner,
                "recording a save receipt against a delete intent owned by another operation"
            );
        }
    }
    let old_remote_id = intent.remote_id.clone();
    let old_href = intent.href.clone();
    let old_uid_is_derived = intent.uid_is_derived;
    let Some(attached) = store::attach_task_delete_intent(
        conn,
        id,
        receipt.remote_id.as_deref(),
        receipt.href.as_deref(),
        receipt.etag.as_deref(),
        receipt.uid_is_derived,
    )?
    else {
        return Ok(());
    };
    // The tombstone captured the identity at delete time. Move it to the one
    // the provider just acknowledged so a delta echo of the real UID is
    // suppressed too; when the delete had no identity there was no tombstone
    // yet and a fresh pending one is needed.
    let rekeyed = store::rekey_task_delete_tombstones(
        conn,
        &attached.task_list_id,
        old_remote_id.as_deref(),
        old_href.as_deref(),
        old_uid_is_derived,
        attached.remote_id.as_deref(),
        attached.href.as_deref(),
        attached.uid_is_derived,
    )?;
    if rekeyed == 0 && (attached.remote_id.is_some() || attached.href.is_some()) {
        store::insert_task_delete_tombstone(
            conn,
            &attached.account_id,
            &attached.task_list_id,
            attached.remote_id.as_deref(),
            attached.href.as_deref(),
            attached.uid_is_derived,
        )?;
    }
    // Address the remote object with the identity the provider confirmed. A
    // create deleted before its receipt landed has no DELETE op yet, so queue
    // one from the pre-wire snapshot content.
    let already_queued =
        update_pending_task_delete_ops(conn, &attached.task_id, account_id, receipt)?;
    if !already_queued && (attached.remote_id.is_some() || attached.href.is_some()) {
        let mut snapshot = base.clone();
        snapshot.remote_id = attached.remote_id.clone();
        snapshot.href = attached.href.clone();
        snapshot.etag = attached.etag.clone();
        snapshot.uid_is_derived = attached.uid_is_derived;
        enqueue_op_sync(
            conn,
            &attached.account_id,
            &TaskOp::Delete { snapshot },
            "删除任务".to_string(),
        )?;
    }
    Ok(())
}

/// Point outstanding DELETE operations for a task at a confirmed identity.
fn update_pending_task_delete_ops(
    conn: &rusqlite::Connection,
    task_id: &str,
    account_id: &str,
    receipt: &TaskSaveReceipt,
) -> Result<bool> {
    let mut updated = false;
    for op in repo::pending_ops::list(conn, Some(account_id))? {
        if op.kind != ops::KIND_DELETE || !repo::pending_ops::is_outstanding(&op) {
            continue;
        }
        let Ok(TaskOp::Delete { mut snapshot }) = ops::decode(&op) else {
            continue;
        };
        if snapshot.task.id != task_id {
            continue;
        }
        if receipt.remote_id.is_some() {
            snapshot.remote_id = receipt.remote_id.clone();
            snapshot.uid_is_derived = receipt.uid_is_derived;
        }
        if receipt.href.is_some() {
            snapshot.href = receipt.href.clone();
        }
        if receipt.etag.is_some() {
            snapshot.etag = receipt.etag.clone();
        }
        let value = ops::to_value(&TaskOp::Delete { snapshot })?;
        repo::pending_ops::update_payload(conn, &op.id, &value)?;
        updated = true;
    }
    Ok(updated)
}

fn restore_task(
    conn: &rusqlite::Connection,
    snapshot: &TaskSnapshot,
    owner: Option<&str>,
) -> Result<()> {
    tasks_repo::upsert_remote(
        conn,
        &snapshot.task,
        snapshot.etag.as_deref(),
        snapshot.remote_id.as_deref(),
    )?;
    store::bump_task_content(conn, &snapshot.task.id, owner)?;
    if let Some(href) = &snapshot.href {
        store::set_task_href(conn, &snapshot.task.id, href)?;
    }
    // `upsert_remote` does not know about the identity source; restore it
    // explicitly so a rolled-back row keeps the same kind it had before.
    store::set_task_uid_is_derived(conn, &snapshot.task.id, snapshot.uid_is_derived)?;
    Ok(())
}

fn normalize_color_or_default(color: &str) -> String {
    crate::util::normalize_color(color).unwrap_or_else(|| "#0078D4".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use nuncio_core::models::{MessageSummary, RecurrenceFreq, RecurrenceRule, StoredMessage};
    use nuncio_core::{CoreContext, Paths};
    use rusqlite::OptionalExtension as _;

    async fn service() -> (CoreContext, TasksService) {
        let dir = std::env::temp_dir().join(format!("nuncio-pim-tasks-{}", new_id()));
        let ctx = CoreContext::for_test(Paths::new(dir)).await.expect("ctx");
        ctx.migrate(store::PIM_NAMESPACE, store::PIM_MIGRATIONS)
            .expect("migrations");
        let http = HttpClient::new(ctx.creds.clone()).expect("http");
        let service = TasksService::new(ctx.clone(), http);
        (ctx, service)
    }

    fn input(title: &str) -> TaskInput {
        TaskInput {
            title: title.into(),
            notes: String::new(),
            importance: Importance::Normal,
            due_date: None,
            reminder_at: None,
            recurrence: None,
            my_day_date: None,
            steps: Vec::new(),
        }
    }

    #[tokio::test]
    async fn local_crud_and_smart_counts() {
        let (_ctx, service) = service().await;
        let task = service
            .task_create("local-tasks", input("Write report"))
            .await
            .expect("create");
        service
            .task_set_importance(&task.id, Importance::High)
            .await
            .expect("importance");
        service
            .task_set_my_day(&task.id, Some("2026-11-12"))
            .await
            .expect("my day");
        let counts = service
            .task_smart_counts("2026-11-12".into())
            .await
            .expect("counts");
        assert_eq!(counts.all, 1);
        assert_eq!(counts.important, 1);
        assert_eq!(counts.my_day, 1);

        let listed = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .expect("query");
        assert_eq!(listed.len(), 1);

        let completed = service
            .task_set_completed(&task.id, true)
            .await
            .expect("complete");
        assert_eq!(completed.status, TaskStatus::Completed);
        let counts = service
            .task_smart_counts("2026-11-12".into())
            .await
            .expect("counts");
        assert_eq!(counts.all, 0);
        assert_eq!(counts.completed, 1);

        service.task_delete(&task.id).await.expect("delete");
        assert!(service.task_get(&task.id).await.is_err());
    }

    #[tokio::test]
    async fn clears_link_when_linked_message_is_gone() {
        let (ctx, service) = service().await;
        let task = service
            .task_create("local-tasks", input("Reply to the report"))
            .await
            .expect("create");

        let message_id = new_id();
        let folder_id = ctx
            .db
            .write({
                let message_id = message_id.clone();
                move |conn| {
                    let folder = repo::folders::create(conn, "local", None, "Inbox")?;
                    let summary = MessageSummary::new(
                        message_id,
                        "local",
                        folder.id.clone(),
                        "thread-1",
                        Utc::now(),
                    );
                    repo::messages::upsert(conn, &StoredMessage::new(summary))?;
                    Ok(folder.id)
                }
            })
            .await
            .expect("message");

        let task_id = task.id.clone();
        let linked = message_id.clone();
        ctx.db
            .write(move |conn| tasks_repo::set_linked_message(conn, &task_id, Some(&linked)))
            .await
            .expect("link");
        assert!(service
            .task_get(&task.id)
            .await
            .expect("get")
            .linked_message_id
            .is_some());

        let ids = vec![message_id];
        ctx.db
            .write(move |conn| repo::messages::delete_local(conn, &ids, true))
            .await
            .expect("delete message");

        let reloaded = service.task_get(&task.id).await.expect("get after delete");
        assert_eq!(reloaded.linked_message_id, None);
        // The link is cleared in storage, not just hidden from the caller.
        let persisted = ctx
            .db
            .read({
                let id = task.id.clone();
                move |conn| tasks_repo::get_required(conn, &id)
            })
            .await
            .expect("stored task");
        assert_eq!(persisted.linked_message_id, None);

        // The list path prunes too, so a stale link cannot reach the UI via a
        // cached row or a "Flagged email" query.
        let task_id = task.id.clone();
        let missing = "message-that-never-existed".to_string();
        ctx.db
            .write(move |conn| tasks_repo::set_linked_message(conn, &task_id, Some(&missing)))
            .await
            .expect("re-link");
        let listed = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .expect("query");
        let listed = listed
            .iter()
            .find(|item| item.id == task.id)
            .expect("task in list");
        assert_eq!(listed.linked_message_id, None);
        let _ = folder_id;
    }

    #[tokio::test]
    async fn completes_recurring_task_and_creates_next() {
        let (_ctx, service) = service().await;
        // A due date far in the future keeps the assertion independent of the
        // wall clock: the next occurrence is simply due + 1 day.
        let mut task_input = input("Water plants");
        task_input.due_date = Some("2099-01-01".into());
        task_input.recurrence = Some(RecurrenceRule {
            freq: RecurrenceFreq::Daily,
            interval: 1,
            ..Default::default()
        });
        let task = service
            .task_create("local-tasks", task_input)
            .await
            .expect("create");
        let completed = service
            .task_set_completed(&task.id, true)
            .await
            .expect("complete");
        assert_eq!(completed.status, TaskStatus::Completed);

        let open = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .expect("query");
        assert_eq!(open.len(), 1, "next occurrence is created");
        assert_ne!(open[0].id, task.id);
        assert_eq!(open[0].due_date.as_deref(), Some("2099-01-02"));
    }

    #[tokio::test]
    async fn moves_and_searches() {
        let (_ctx, service) = service().await;
        let other = service
            .tasklist_create("local", "Work", "#00AA00")
            .await
            .expect("list");
        let task = service
            .task_create("local-tasks", input("Prepare slides"))
            .await
            .expect("create");
        let moved = service.task_move(&task.id, &other.id).await.expect("move");
        assert_eq!(moved.list_id, other.id);
        let found = service.task_search("slides").await.expect("search");
        assert_eq!(found.len(), 1);
        service
            .task_reorder(&other.id, vec![task.id.clone()])
            .await
            .expect("reorder");
    }

    #[tokio::test]
    async fn applies_google_style_parent_and_child() {
        let (_ctx, service) = service().await;
        let list = service
            .ctx
            .db
            .read(|conn| task_lists::get_required(conn, "local-tasks"))
            .await
            .expect("list");
        let now = Utc::now();
        let parent = Task {
            id: String::new(),
            list_id: list.id.clone(),
            account_id: list.account_id.clone(),
            title: "Parent".into(),
            notes: String::new(),
            status: TaskStatus::NotStarted,
            importance: Importance::Normal,
            due_date: None,
            reminder_at: None,
            recurrence: None,
            my_day_date: None,
            completed_at: None,
            created_at: now,
            updated_at: now,
            steps: Vec::new(),
            sort_order: 0,
            linked_message_id: None,
        };
        let child = Task {
            title: "Child".into(),
            ..parent.clone()
        };
        let delta = TaskDelta {
            tasks: vec![
                RemoteTask {
                    task: parent,
                    remote_id: Some("p1".into()),
                    href: None,
                    etag: None,
                    parent_remote_id: None,
                    steps_complete: true,
                    uid_is_derived: false,
                },
                RemoteTask {
                    task: child,
                    remote_id: Some("c1".into()),
                    href: None,
                    etag: None,
                    parent_remote_id: Some("p1".into()),
                    steps_complete: true,
                    uid_is_derived: false,
                },
            ],
            full_sync: true,
            ..Default::default()
        };
        service
            .ctx
            .db
            .write(move |conn| apply_delta(conn, "local", &list, TaskCapabilities::all(), &delta))
            .await
            .expect("apply");
        let tasks = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .expect("query");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Parent");
        assert_eq!(tasks[0].steps.len(), 1);
        assert_eq!(tasks[0].steps[0].title, "Child");
    }

    #[tokio::test]
    async fn incremental_delta_never_clears_steps() {
        let (_ctx, service) = service().await;
        let list = service
            .ctx
            .db
            .read(|conn| task_lists::get_required(conn, "local-tasks"))
            .await
            .expect("list");
        let now = Utc::now();
        let mut parent = Task {
            id: String::new(),
            list_id: list.id.clone(),
            account_id: list.account_id.clone(),
            title: "Parent".into(),
            notes: String::new(),
            status: TaskStatus::NotStarted,
            importance: Importance::Normal,
            due_date: None,
            reminder_at: None,
            recurrence: None,
            my_day_date: None,
            completed_at: None,
            created_at: now,
            updated_at: now,
            steps: vec![TaskStep {
                id: "remote-step-1".into(),
                title: "Step".into(),
                is_completed: false,
                sort_order: 0,
            }],
            sort_order: 5,
            linked_message_id: None,
        };
        let full = TaskDelta {
            tasks: vec![RemoteTask {
                task: parent.clone(),
                remote_id: Some("p1".into()),
                href: None,
                etag: None,
                parent_remote_id: None,
                steps_complete: true,
                uid_is_derived: false,
            }],
            full_sync: true,
            ..Default::default()
        };
        let list_for_db = list.clone();
        service
            .ctx
            .db
            .write(move |conn| {
                apply_delta(conn, "local", &list_for_db, TaskCapabilities::all(), &full)
            })
            .await
            .expect("full delta");
        let tasks = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .expect("query");
        assert_eq!(tasks[0].steps.len(), 1);
        let saved_id = tasks[0].id.clone();

        // `updatedMin` returns only changed parents: steps are absent and must
        // stay untouched (and step mappings preserved).
        parent.id = saved_id.clone();
        parent.steps = Vec::new();
        let incremental = TaskDelta {
            tasks: vec![RemoteTask {
                task: parent.clone(),
                remote_id: Some("p1".into()),
                href: None,
                etag: Some("e2".into()),
                parent_remote_id: None,
                steps_complete: false,
                uid_is_derived: false,
            }],
            full_sync: false,
            ..Default::default()
        };
        let list_for_db = list.clone();
        service
            .ctx
            .db
            .write(move |conn| {
                apply_delta(
                    conn,
                    "local",
                    &list_for_db,
                    TaskCapabilities::all(),
                    &incremental,
                )
            })
            .await
            .expect("incremental delta");
        let tasks = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .expect("query");
        assert_eq!(tasks[0].steps.len(), 1, "incremental delta keeps steps");
        let mappings = service
            .ctx
            .db
            .read({
                let id = saved_id.clone();
                move |conn| crate::store::step_mappings_for_task(conn, &id)
            })
            .await
            .expect("mappings");
        assert_eq!(mappings.len(), 1, "step mapping survives the delta");

        // Even a delta marked "complete" that happens to carry an empty step
        // list must not wipe mapped local steps.
        let empty_complete = TaskDelta {
            tasks: vec![RemoteTask {
                task: parent,
                remote_id: Some("p1".into()),
                href: None,
                etag: Some("e3".into()),
                parent_remote_id: None,
                steps_complete: true,
                uid_is_derived: false,
            }],
            full_sync: false,
            ..Default::default()
        };
        let list_for_db = list.clone();
        service
            .ctx
            .db
            .write(move |conn| {
                apply_delta(
                    conn,
                    "local",
                    &list_for_db,
                    TaskCapabilities::all(),
                    &empty_complete,
                )
            })
            .await
            .expect("empty complete delta");
        let tasks = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .expect("query");
        assert_eq!(tasks[0].steps.len(), 1);
    }

    /// A delta describes the server, which does not know about a write that is
    /// still queued. Applying it used to replace the local steps and silently
    /// discard a step the user had just added (the second "添加步骤" that
    /// vanished).
    #[tokio::test]
    async fn delta_keeps_steps_while_a_save_is_queued() {
        let (ctx, _service) = service().await;
        let list = ctx
            .db
            .read(|conn| task_lists::get_required(conn, "local-tasks"))
            .await
            .expect("list");
        let now = Utc::now();
        let step = |id: &str, title: &str| TaskStep {
            id: id.into(),
            title: title.into(),
            is_completed: false,
            sort_order: 0,
        };
        let task = Task {
            id: "local-task".into(),
            list_id: list.id.clone(),
            account_id: list.account_id.clone(),
            title: "测试todo".into(),
            notes: String::new(),
            status: TaskStatus::NotStarted,
            importance: Importance::Normal,
            due_date: None,
            reminder_at: None,
            recurrence: None,
            my_day_date: None,
            completed_at: None,
            created_at: now,
            updated_at: now,
            steps: vec![step("s1", "11"), step("s2", "22")],
            sort_order: 0,
            linked_message_id: None,
        };
        ctx.db
            .write({
                let task = task.clone();
                move |conn| tasks_repo::upsert_remote(conn, &task, None, Some("r1"))
            })
            .await
            .expect("task");
        ctx.db
            .write(|conn| {
                // Serialize through `ops` so the test uses the same payload
                // shape the real command path writes (`task_id`, not
                // `taskId`).
                let payload = ops::to_value(&TaskOp::Save {
                    task_id: "local-task".into(),
                    assigned_uid: None,
                    snapshot: None,
                })
                .expect("payload");
                repo::pending_ops::enqueue(
                    conn,
                    nuncio_core::models::NewPendingOp::new(
                        "local",
                        ops::KIND_SAVE,
                        payload,
                        "更新任务",
                    ),
                )
            })
            .await
            .expect("queue");

        let mut remote = task.clone();
        remote.id = String::new();
        remote.steps = vec![step("remote-1", "11")];
        let delta = TaskDelta {
            tasks: vec![RemoteTask {
                task: remote,
                remote_id: Some("r1".into()),
                href: None,
                etag: None,
                parent_remote_id: None,
                steps_complete: true,
                uid_is_derived: false,
            }],
            full_sync: false,
            ..Default::default()
        };
        ctx.db
            .write(move |conn| apply_delta(conn, "local", &list, TaskCapabilities::all(), &delta))
            .await
            .expect("delta");

        let stored = ctx
            .db
            .read(|conn| tasks_repo::get_required(conn, "local-task"))
            .await
            .expect("stored");
        assert_eq!(
            stored.steps.len(),
            2,
            "a step with a queued write must survive the delta"
        );
    }

    #[tokio::test]
    async fn default_list_marker_sets_the_account_default() {
        let (ctx, _service) = service().await;
        let other_id = ctx
            .db
            .write(|conn| {
                // `create` marks the first list of an account as default; this
                // one is second, so the marker alone has to move the flag.
                let other = task_lists::create(conn, "local", "Other", "#123456")?;
                apply_well_known(conn, &other.id, nuncio_core::models::WELL_KNOWN_DEFAULT)?;
                Ok(other.id)
            })
            .await
            .expect("other list");
        let (default_id, defaults) = ctx
            .db
            .read(|conn| {
                let default = task_lists::default_for_account(conn, "local")?;
                let count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM task_list WHERE account_id = 'local' AND is_default = 1",
                    [],
                    |row| row.get(0),
                )?;
                Ok((default.map(|list| list.id), count))
            })
            .await
            .expect("default");
        assert_eq!(default_id.as_deref(), Some(other_id.as_str()));
        assert_eq!(defaults, 1, "only one list may be the default");
    }

    /// Providers expose no writable step order, so the server's order must not
    /// overwrite the order the user dragged into place.
    #[tokio::test]
    async fn delta_keeps_local_step_order() {
        let (ctx, _service) = service().await;
        let list = ctx
            .db
            .read(|conn| task_lists::get_required(conn, "local-tasks"))
            .await
            .expect("list");
        let now = Utc::now();
        let step = |id: &str, title: &str, order: i64| TaskStep {
            id: id.into(),
            title: title.into(),
            is_completed: false,
            sort_order: order,
        };
        let task = Task {
            id: "local-task".into(),
            list_id: list.id.clone(),
            account_id: list.account_id.clone(),
            title: "测试todo".into(),
            notes: String::new(),
            status: TaskStatus::NotStarted,
            importance: Importance::Normal,
            due_date: None,
            reminder_at: None,
            recurrence: None,
            my_day_date: None,
            completed_at: None,
            created_at: now,
            updated_at: now,
            steps: vec![step("s1", "11", 0), step("s2", "22", 1)],
            sort_order: 0,
            linked_message_id: None,
        };
        ctx.db
            .write({
                let task = task.clone();
                move |conn| {
                    tasks_repo::upsert_remote(conn, &task, None, Some("r1"))?;
                    crate::store::set_step_mapping(conn, "s1", "local-task", "remote-1")?;
                    crate::store::set_step_mapping(conn, "s2", "local-task", "remote-2")?;
                    Ok(())
                }
            })
            .await
            .expect("task");

        let mut remote = task.clone();
        remote.id = String::new();
        remote.steps = vec![step("remote-2", "22", 0), step("remote-1", "11", 1)];
        let delta = TaskDelta {
            tasks: vec![RemoteTask {
                task: remote,
                remote_id: Some("r1".into()),
                href: None,
                etag: None,
                parent_remote_id: None,
                steps_complete: true,
                uid_is_derived: false,
            }],
            full_sync: false,
            ..Default::default()
        };
        ctx.db
            .write(move |conn| apply_delta(conn, "local", &list, TaskCapabilities::all(), &delta))
            .await
            .expect("delta");

        let stored = ctx
            .db
            .read(|conn| tasks_repo::get_required(conn, "local-task"))
            .await
            .expect("stored");
        let mut ordered = stored.steps.clone();
        ordered.sort_by_key(|step| step.sort_order);
        let titles: Vec<String> = ordered.iter().map(|step| step.title.clone()).collect();
        assert_eq!(titles, vec!["11", "22"], "local step order must survive");
    }

    /// A delta arriving before the queued save uploads must not undo the edit.
    /// Reminders were the visible casualty: the reminder reverted and then the
    /// worker uploaded the reverted value.
    #[tokio::test]
    async fn delta_keeps_local_fields_while_a_save_is_queued() {
        let (ctx, _service) = service().await;
        let list = ctx
            .db
            .read(|conn| task_lists::get_required(conn, "local-tasks"))
            .await
            .expect("list");
        let now = Utc::now();
        let reminder: chrono::DateTime<Utc> = "2026-11-20T09:15:00Z".parse().expect("date");
        let stale: chrono::DateTime<Utc> = "2026-11-19T21:15:00Z".parse().expect("date");
        let task = Task {
            id: "local-task".into(),
            list_id: list.id.clone(),
            account_id: list.account_id.clone(),
            title: "测试todo".into(),
            notes: String::new(),
            status: TaskStatus::NotStarted,
            importance: Importance::Normal,
            due_date: None,
            reminder_at: Some(reminder),
            recurrence: None,
            my_day_date: None,
            completed_at: None,
            created_at: now,
            updated_at: now,
            steps: Vec::new(),
            sort_order: 0,
            linked_message_id: None,
        };
        ctx.db
            .write({
                let task = task.clone();
                move |conn| {
                    tasks_repo::upsert_remote(conn, &task, None, Some("r1"))?;
                    let payload = ops::to_value(&TaskOp::Save {
                        task_id: "local-task".into(),
                        assigned_uid: None,
                        snapshot: None,
                    })
                    .expect("payload");
                    repo::pending_ops::enqueue(
                        conn,
                        nuncio_core::models::NewPendingOp::new(
                            "local",
                            ops::KIND_SAVE,
                            payload,
                            "更新任务",
                        ),
                    )?;
                    Ok(())
                }
            })
            .await
            .expect("task");

        let mut remote = task.clone();
        remote.id = String::new();
        remote.reminder_at = Some(stale);
        let delta = TaskDelta {
            tasks: vec![RemoteTask {
                task: remote,
                remote_id: Some("r1".into()),
                href: None,
                etag: None,
                parent_remote_id: None,
                steps_complete: true,
                uid_is_derived: false,
            }],
            full_sync: false,
            ..Default::default()
        };
        ctx.db
            .write(move |conn| apply_delta(conn, "local", &list, TaskCapabilities::all(), &delta))
            .await
            .expect("delta");

        let stored = ctx
            .db
            .read(|conn| tasks_repo::get_required(conn, "local-task"))
            .await
            .expect("stored");
        assert_eq!(
            stored.reminder_at,
            Some(reminder),
            "a queued save must not be undone by an older delta"
        );
    }

    /// Microsoft To Do's built-in lists cannot be renamed or deleted. Rejecting
    /// it before the local write is what prevents the "success toast, then
    /// rollback toast" pair.
    #[tokio::test]
    async fn built_in_lists_cannot_be_renamed_or_deleted() {
        let (ctx, service) = service().await;
        ctx.db
            .write(|conn| {
                conn.execute(
                    "UPDATE task_list SET well_known_list_name = 'flaggedEmails' WHERE id = 'local-tasks'",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("mark");
        assert!(service.tasklist_delete("local-tasks").await.is_err());
        assert!(service
            .tasklist_update("local-tasks", Some("Renamed"), None)
            .await
            .is_err());
        // A colour is local-only and stays editable.
        assert!(service
            .tasklist_update("local-tasks", None, Some("#ff0000"))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn preserves_local_only_fields() {
        let (_ctx, service) = service().await;
        let list = service
            .ctx
            .db
            .read(|conn| task_lists::get_required(conn, "local-tasks"))
            .await
            .expect("list");
        let now = Utc::now();
        let mut task_input = input("Idea");
        task_input.importance = Importance::High;
        task_input.reminder_at = Some(now);
        task_input.my_day_date = Some("2026-11-12".into());
        task_input.recurrence = Some(RecurrenceRule {
            freq: RecurrenceFreq::Weekly,
            interval: 1,
            ..Default::default()
        });
        let created = service
            .task_create(&list.id, task_input)
            .await
            .expect("create");
        let created_id = created.id.clone();
        service
            .ctx
            .db
            .write({
                let created_id = created_id.clone();
                move |conn| {
                    conn.execute(
                        "UPDATE task SET remote_id = 'r1', etag = 'e1', updated_at = ?2 WHERE id = ?1",
                        rusqlite::params![created_id, repo::now_string()],
                    )?;
                    Ok(())
                }
            })
            .await
            .expect("link");
        let caps = TaskCapabilities {
            importance: false,
            reminders: false,
            steps: true,
            recurrence: false,
            ordering: false,
            server_recurring_completion: false,
        };
        let remote_task = Task {
            title: "Idea from server".into(),
            importance: Importance::Normal,
            reminder_at: None,
            recurrence: None,
            my_day_date: None,
            ..created.clone()
        };
        let delta = TaskDelta {
            tasks: vec![RemoteTask {
                task: remote_task,
                remote_id: Some("r1".into()),
                href: None,
                etag: Some("e2".into()),
                parent_remote_id: None,
                steps_complete: true,
                uid_is_derived: false,
            }],
            ..Default::default()
        };
        service
            .ctx
            .db
            .write(move |conn| apply_delta(conn, "local", &list, caps, &delta))
            .await
            .expect("apply");
        let applied = service.task_get(&created.id).await.expect("task");
        assert_eq!(applied.title, "Idea from server");
        assert_eq!(applied.importance, Importance::High);
        assert!(applied.reminder_at.is_some());
        assert!(applied.recurrence.is_some());
        assert_eq!(applied.my_day_date.as_deref(), Some("2026-11-12"));
    }

    /* ─────────────────── DAV task identity / prune ─────────────────── */

    fn remote_task_with_kind(
        list: &TaskList,
        title: &str,
        remote_id: &str,
        href: Option<&str>,
        etag: Option<&str>,
        uid_is_derived: bool,
    ) -> RemoteTask {
        let now = Utc::now();
        RemoteTask {
            task: Task {
                id: String::new(),
                list_id: list.id.clone(),
                account_id: list.account_id.clone(),
                title: title.into(),
                notes: String::new(),
                status: TaskStatus::NotStarted,
                importance: Importance::Normal,
                due_date: None,
                reminder_at: None,
                recurrence: None,
                my_day_date: None,
                completed_at: None,
                created_at: now,
                updated_at: now,
                steps: Vec::new(),
                sort_order: 0,
                linked_message_id: None,
            },
            remote_id: Some(remote_id.into()),
            href: href.map(str::to_string),
            etag: etag.map(str::to_string),
            parent_remote_id: None,
            steps_complete: true,
            uid_is_derived,
        }
    }

    fn remote_task(
        list: &TaskList,
        title: &str,
        remote_id: &str,
        href: Option<&str>,
        etag: Option<&str>,
    ) -> RemoteTask {
        remote_task_with_kind(list, title, remote_id, href, etag, false)
    }

    async fn apply(ctx: &CoreContext, list: &TaskList, delta: TaskDelta) {
        let list = list.clone();
        ctx.db
            .write(move |conn| apply_delta(conn, "local", &list, TaskCapabilities::all(), &delta))
            .await
            .expect("apply delta");
    }

    async fn task_count(service: &TasksService) -> usize {
        service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .expect("query")
            .len()
    }

    /// A VTODO without a UID is identified by the normalized href. The same
    /// resource delivered as `%41.ics` and then `A.ics` is one task, not two.
    #[tokio::test]
    async fn no_uid_task_is_imported_once_across_href_spellings() {
        let (ctx, service) = service().await;
        let list = ctx
            .db
            .read(|conn| task_lists::get_required(conn, "local-tasks"))
            .await
            .expect("list");
        let encoded = "https://dav/tasks/%41.ics";
        let plain = "https://dav/tasks/A.ics";
        let remote_id = crate::tasks::dav::task_remote_id("", encoded);
        assert_eq!(remote_id, "A");
        assert_eq!(remote_id, crate::tasks::dav::task_remote_id("", plain));

        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "Buy milk",
                    &remote_id,
                    Some(encoded),
                    Some("e1"),
                    true,
                )],
                listed_hrefs: vec![encoded.into()],
                full_sync: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(task_count(&service).await, 1);

        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "Buy milk",
                    &remote_id,
                    Some(plain),
                    Some("e1"),
                    true,
                )],
                listed_hrefs: vec![plain.into()],
                full_sync: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            task_count(&service).await,
            1,
            "an equivalent href spelling must not import a second task"
        );
        let tasks = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .expect("query");
        let href = ctx
            .db
            .read({
                let id = tasks[0].id.clone();
                move |conn| store::task_href(conn, &id)
            })
            .await
            .expect("href");
        assert_eq!(
            href.as_deref(),
            Some(plain),
            "the request href keeps the newest raw spelling"
        );
    }

    /// Deleting a no-UID task records a tombstone; a stale delta that names the
    /// same resource under the other spelling must not resurrect it.
    #[tokio::test]
    async fn deleted_no_uid_task_is_not_resurrected_by_another_spelling() {
        let (ctx, service) = service().await;
        let list = ctx
            .db
            .read(|conn| task_lists::get_required(conn, "local-tasks"))
            .await
            .expect("list");
        let encoded = "https://dav/tasks/%41.ics";
        let plain = "https://dav/tasks/A.ics";
        let remote_id = crate::tasks::dav::task_remote_id("", encoded);
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "Buy milk",
                    &remote_id,
                    Some(encoded),
                    Some("e1"),
                    true,
                )],
                listed_hrefs: vec![encoded.into()],
                full_sync: true,
                ..Default::default()
            },
        )
        .await;
        let tasks = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .expect("query");
        let id = tasks[0].id.clone();
        service.task_delete(&id).await.expect("delete");
        assert!(service.task_get(&id).await.is_err(), "row is gone locally");

        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "Buy milk",
                    &remote_id,
                    Some(plain),
                    Some("e1"),
                    true,
                )],
                listed_hrefs: vec![plain.into()],
                full_sync: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            task_count(&service).await,
            0,
            "the delete tombstone must block an equivalent re-delivery"
        );
    }

    /// The ETag fallback fetches only changed hrefs but still marks the batch
    /// `full_sync`. A listed, unchanged task is present on the server and must
    /// not be pruned.
    #[tokio::test]
    async fn full_sync_keeps_a_listed_unchanged_task() {
        let (ctx, service) = service().await;
        let list = ctx
            .db
            .read(|conn| task_lists::get_required(conn, "local-tasks"))
            .await
            .expect("list");
        let encoded = "https://dav/tasks/%41.ics";
        let plain = "https://dav/tasks/A.ics";
        let remote_id = crate::tasks::dav::task_remote_id("", encoded);
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "Buy milk",
                    &remote_id,
                    Some(encoded),
                    Some("e1"),
                    true,
                )],
                listed_hrefs: vec![encoded.into()],
                full_sync: true,
                ..Default::default()
            },
        )
        .await;

        // Unchanged ETag -> absent from `tasks`, present in the listing under
        // the equivalent spelling.
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: Vec::new(),
                listed_hrefs: vec![plain.into()],
                full_sync: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            task_count(&service).await,
            1,
            "an object in the authoritative listing is not deleted"
        );
    }

    /// A full listing that does not name the task proves it is gone.
    #[tokio::test]
    async fn full_sync_prunes_a_task_missing_from_the_listing() {
        let (ctx, service) = service().await;
        let list = ctx
            .db
            .read(|conn| task_lists::get_required(conn, "local-tasks"))
            .await
            .expect("list");
        let href = "https://dav/tasks/T1.ics";
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task(&list, "Buy milk", "T1", Some(href), Some("e1"))],
                listed_hrefs: vec![href.into()],
                full_sync: true,
                ..Default::default()
            },
        )
        .await;

        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: Vec::new(),
                listed_hrefs: Vec::new(),
                full_sync: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(task_count(&service).await, 0, "an unlisted task is pruned");
    }

    /// A round where a listed object's identity could not be read proves
    /// nothing: it must not prune and must not advance the cursor.
    #[tokio::test]
    async fn incomplete_round_keeps_tasks_and_holds_the_cursor() {
        let (ctx, service) = service().await;
        let list = ctx
            .db
            .read(|conn| task_lists::get_required(conn, "local-tasks"))
            .await
            .expect("list");
        let href = "https://dav/tasks/T1.ics";
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task(&list, "Buy milk", "T1", Some(href), Some("e1"))],
                listed_hrefs: vec![href.into()],
                full_sync: true,
                ..Default::default()
            },
        )
        .await;
        ctx.db
            .write(|conn| save_cursor(conn, "local-tasks", Some("old-token"), Some("old-ctag")))
            .await
            .expect("cursor");

        let delta = TaskDelta {
            tasks: Vec::new(),
            listed_hrefs: Vec::new(),
            incomplete: true,
            sync_token: Some("new-token".into()),
            ctag: Some("new-ctag".into()),
            full_sync: true,
            ..Default::default()
        };
        let list_for_db = list.clone();
        ctx.db
            .write(move |conn| {
                commit_delta(conn, "local", &list_for_db, TaskCapabilities::all(), &delta)
            })
            .await
            .expect("incomplete round");
        assert_eq!(
            task_count(&service).await,
            1,
            "an incomplete full sync must not prune"
        );
        let cursor = ctx
            .db
            .read(|conn| load_cursor(conn, "local-tasks"))
            .await
            .expect("load cursor")
            .expect("cursor row");
        assert_eq!(cursor.sync_token.as_deref(), Some("old-token"));
        assert_eq!(cursor.ctag.as_deref(), Some("old-ctag"));
    }

    /// The tombstone is removed when a permanently failed DELETE is rolled
    /// back, so the restored task can sync again.
    #[tokio::test]
    async fn rollback_removes_the_delete_tombstone() {
        let (ctx, service) = service().await;
        let list = ctx
            .db
            .read(|conn| task_lists::get_required(conn, "local-tasks"))
            .await
            .expect("list");
        let href = "https://dav/tasks/T1.ics";
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task(&list, "Buy milk", "T1", Some(href), Some("e1"))],
                listed_hrefs: vec![href.into()],
                full_sync: true,
                ..Default::default()
            },
        )
        .await;
        let tasks = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .expect("query");
        let id = tasks[0].id.clone();
        let snapshot = ctx
            .db
            .read({
                let id = id.clone();
                move |conn| snapshot_task(conn, &id)
            })
            .await
            .expect("snapshot")
            .expect("task snapshot");
        service.task_delete(&id).await.expect("delete");
        let tombstones = ctx
            .db
            .read(|conn| store::active_task_delete_tombstones(conn, "local-tasks", Utc::now()))
            .await
            .expect("tombstones");
        assert_eq!(tombstones.len(), 1, "delete records a tombstone");

        // Simulate the worker's queued delete, then its permanent failure.
        let payload = ops::to_value(&TaskOp::Delete { snapshot }).expect("payload");
        ctx.db
            .write(move |conn| {
                repo::pending_ops::enqueue(
                    conn,
                    nuncio_core::models::NewPendingOp::new(
                        "local",
                        ops::KIND_DELETE,
                        payload,
                        "删除任务",
                    ),
                )
            })
            .await
            .expect("queue delete");
        let op = ctx
            .db
            .read(|conn| repo::pending_ops::list(conn, Some("local")))
            .await
            .expect("ops")
            .into_iter()
            .find(|op| op.kind == ops::KIND_DELETE)
            .expect("delete op");
        service.rollback(&op).await.expect("rollback");
        let tombstones = ctx
            .db
            .read(|conn| store::active_task_delete_tombstones(conn, "local-tasks", Utc::now()))
            .await
            .expect("tombstones");
        assert!(tombstones.is_empty(), "rollback lifts the suppression");
        assert!(service.task_get(&id).await.is_ok(), "task is restored");
    }

    /// A failed save may only roll back the content it still owns. A newer
    /// edit is already in the row, and its queued save re-reads the row when it
    /// executes, so its payload cannot recover content an older snapshot
    /// restores over it.
    #[tokio::test]
    async fn save_rollback_keeps_a_newer_queued_edit() {
        let (ctx, service) = service().await;
        let list = ctx
            .db
            .read(|conn| task_lists::get_required(conn, "local-tasks"))
            .await
            .expect("list");
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task(&list, "Original", "T1", None, None)],
                ..Default::default()
            },
        )
        .await;
        let task = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .expect("query")
            .remove(0);
        make_provider(&ctx).await;
        service
            .task_update(&task.id, input("Edit B"))
            .await
            .expect("edit B");
        service
            .task_update(&task.id, input("Edit C must win"))
            .await
            .expect("edit C");
        let save_ops = ctx
            .db
            .read(|conn| repo::pending_ops::list(conn, Some("local")))
            .await
            .expect("ops")
            .into_iter()
            .filter(|op| op.kind == ops::KIND_SAVE)
            .collect::<Vec<_>>();
        assert_eq!(save_ops.len(), 2, "both edits are queued");
        let failed = save_ops.into_iter().next().expect("older save");
        let failed = ctx
            .db
            .write(move |conn| {
                repo::pending_ops::mark_failed(conn, &failed.id, "edit B failed")?;
                repo::pending_ops::get_required(conn, &failed.id)
            })
            .await
            .expect("mark failed");
        service.rollback(&failed).await.expect("rollback");
        assert_eq!(
            service.task_get(&task.id).await.expect("task").title,
            "Edit C must win",
            "an older save's rollback must not erase a newer queued edit"
        );
    }

    /// Content rollback must never move the task back. The snapshot predates a
    /// later move, and the current list is the user's latest choice.
    #[tokio::test]
    async fn save_rollback_keeps_the_current_list_and_account() {
        let (ctx, service) = service().await;
        let list = ctx
            .db
            .read(|conn| task_lists::get_required(conn, "local-tasks"))
            .await
            .expect("list");
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task(&list, "Original", "T1", None, None)],
                ..Default::default()
            },
        )
        .await;
        let task = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .expect("query")
            .remove(0);
        // The extra list is local-only, so it is not pushed to the provider and
        // does not add an operation of its own.
        let other = service
            .tasklist_create("local", "Other", "#112233")
            .await
            .expect("list");
        make_provider(&ctx).await;
        service
            .task_update(&task.id, input("Older save edit"))
            .await
            .expect("edit");
        let save_op = ctx
            .db
            .read(|conn| repo::pending_ops::list(conn, Some("local")))
            .await
            .expect("ops")
            .into_iter()
            .find(|op| op.kind == ops::KIND_SAVE)
            .expect("save op");
        service.task_move(&task.id, &other.id).await.expect("move");
        let task_id = task.id.clone();
        let save_op = ctx
            .db
            .write(move |conn| {
                // Put the rollback in the state that actually restores content:
                // the move already finished (its operation is gone) and the row
                // carries the same second as this save's enqueue, so the
                // ownership check accepts the snapshot. The content restore must
                // still keep the new position.
                conn.execute(
                    "DELETE FROM pending_op WHERE kind = ?1",
                    rusqlite::params![ops::KIND_MOVE],
                )?;
                conn.execute(
                    "UPDATE task SET updated_at = ?1 WHERE id = ?2",
                    rusqlite::params![repo::dt_string(save_op.created_at), task_id],
                )?;
                repo::pending_ops::mark_failed(conn, &save_op.id, "save failed after the move")?;
                repo::pending_ops::get_required(conn, &save_op.id)
            })
            .await
            .expect("mark failed");
        service.rollback(&save_op).await.expect("rollback");
        let row = service.task_get(&task.id).await.expect("task");
        assert_eq!(
            row.title, "Original",
            "with no later writer the snapshot's content is restored"
        );
        assert_eq!(
            row.list_id, other.id,
            "a save's content rollback must not undo a later move"
        );
        assert_eq!(
            row.account_id, "local",
            "the rollback keeps the account the row currently belongs to"
        );
    }

    /// With no later writer the snapshot's content is still restored.
    #[tokio::test]
    async fn save_rollback_restores_content_when_unchanged() {
        let (ctx, service) = service().await;
        let list = ctx
            .db
            .read(|conn| task_lists::get_required(conn, "local-tasks"))
            .await
            .expect("list");
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task(&list, "Original", "T1", None, None)],
                ..Default::default()
            },
        )
        .await;
        let task = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .expect("query")
            .remove(0);
        make_provider(&ctx).await;
        let mut edit = input("Failed edit");
        // My Day is local-only: it never reaches the provider, so a failed
        // upload must not revert it to the snapshot's older value.
        edit.my_day_date = Some("2026-11-12".into());
        service.task_update(&task.id, edit).await.expect("edit");
        let save_op = ctx
            .db
            .read(|conn| repo::pending_ops::list(conn, Some("local")))
            .await
            .expect("ops")
            .into_iter()
            .find(|op| op.kind == ops::KIND_SAVE)
            .expect("save op");
        let save_op = ctx
            .db
            .write(move |conn| {
                repo::pending_ops::mark_failed(conn, &save_op.id, "edit failed")?;
                repo::pending_ops::get_required(conn, &save_op.id)
            })
            .await
            .expect("mark failed");
        service.rollback(&save_op).await.expect("rollback");
        let row = service.task_get(&task.id).await.expect("task");
        assert_eq!(
            row.title, "Original",
            "an unchanged task is still rolled back to its snapshot"
        );
        assert_eq!(row.list_id, "local-tasks", "the list is untouched");
        assert_eq!(
            row.my_day_date.as_deref(),
            Some("2026-11-12"),
            "a local-only field is never rolled back"
        );
    }

    /// The production path: the worker settles a permanently failed save in
    /// the same transaction as its rollback, and that rollback must obey the
    /// same ownership rule.
    #[tokio::test]
    async fn worker_save_rollback_keeps_a_newer_queued_edit() {
        let (mut ctx, service) = service().await;
        let list = ctx
            .db
            .read(|conn| task_lists::get_required(conn, "local-tasks"))
            .await
            .expect("list");
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task(&list, "Original", "T1", None, None)],
                ..Default::default()
            },
        )
        .await;
        let task = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .expect("query")
            .remove(0);
        // The list has no remote mapping, so the save fails before any wire
        // call; making its `attempts` reach the cap turns the next failure
        // into a permanent one and exercises the worker's rollback branch.
        make_provider(&ctx).await;
        service
            .task_update(&task.id, input("Edit B"))
            .await
            .expect("edit B");
        service
            .task_update(&task.id, input("Edit C must win"))
            .await
            .expect("edit C");
        let failed = ctx
            .db
            .read(|conn| repo::pending_ops::list(conn, Some("local")))
            .await
            .expect("ops")
            .into_iter()
            .find(|op| op.kind == ops::KIND_SAVE)
            .expect("older save");
        let failed_id = failed.id.clone();
        ctx.db
            .write(move |conn| {
                conn.execute(
                    "UPDATE pending_op SET attempts = ?2 WHERE id = ?1",
                    rusqlite::params![failed_id, crate::op_worker::MAX_ATTEMPTS],
                )?;
                Ok(())
            })
            .await
            .expect("raise attempts");
        let events = Arc::new(nuncio_core::traits::testing::RecordingEventSink::new());
        ctx.events = events.clone();
        crate::op_worker::process_pass(&ctx, Arc::new(service.clone()))
            .await
            .expect("pass");
        let state = ctx
            .db
            .read(move |conn| repo::pending_ops::get_required(conn, &failed.id))
            .await
            .expect("op")
            .state;
        assert_eq!(state, PendingOpState::Failed, "the older save settles");
        assert!(
            events
                .last_named(nuncio_core::events::TASKS_CHANGED)
                .is_some(),
            "a committed rollback must announce the task rows it rewrote"
        );
        assert_eq!(
            service.task_get(&task.id).await.expect("task").title,
            "Edit C must win",
            "the worker's rollback must not erase a newer queued edit"
        );
    }

    /// A failed create removes the row only while the row is still its own. A
    /// newer queued edit owns the row by then, and a row that already carries a
    /// provider identity names a real object.
    #[tokio::test]
    async fn create_rollback_keeps_a_newer_queued_edit() {
        let (ctx, service) = service().await;
        make_provider(&ctx).await;
        let plain = service
            .task_create("local-tasks", input("Failed create"))
            .await
            .expect("create");
        let edited = service
            .task_create("local-tasks", input("Failed create with edit"))
            .await
            .expect("create");
        service
            .task_update(&edited.id, input("Newer edit must survive"))
            .await
            .expect("edit");
        let create_ops = ctx
            .db
            .read(|conn| repo::pending_ops::list(conn, Some("local")))
            .await
            .expect("ops");
        for (task_id, op) in [
            (
                plain.id.as_str(),
                create_ops
                    .iter()
                    .find(|op| {
                        op.kind == ops::KIND_SAVE
                            && ops::decode(op)
                                .is_ok_and(|op| matches!(op, TaskOp::Save { task_id, .. } if task_id == plain.id))
                    })
                    .expect("plain create op"),
            ),
            (
                edited.id.as_str(),
                create_ops
                    .iter()
                    .find(|op| {
                        op.kind == ops::KIND_SAVE
                            && ops::decode(op)
                                .is_ok_and(|op| matches!(op, TaskOp::Save { task_id, .. } if task_id == edited.id))
                    })
                    .expect("edited create op"),
            ),
        ] {
            let op_id = op.id.clone();
            let failed = ctx
                .db
                .write(move |conn| {
                    repo::pending_ops::mark_failed(conn, &op_id, "create failed")?;
                    repo::pending_ops::get_required(conn, &op_id)
                })
                .await
                .expect("mark failed");
            service.rollback(&failed).await.expect("rollback");
            let row = service.task_get(task_id).await;
            if task_id == edited.id {
                assert_eq!(
                    row.expect("newer edit keeps the row").title,
                    "Newer edit must survive",
                    "the failed create must not erase a newer queued edit"
                );
            } else {
                assert!(row.is_err(), "an untouched failed create is removed");
            }
        }
    }

    /// Make the built-in account a CalDAV task provider so local edits queue
    /// operations without touching the network.
    async fn make_provider(ctx: &CoreContext) {
        ctx.db
            .write(|db| {
                db.execute(
                    "UPDATE account SET provider = 'imap', caldav_url = 'https://dav/tasks/' \
                     WHERE id = 'local'",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("provider account");
    }

    /// A UID-less VTODO (`A.ics` -> placeholder `A`) and a different resource
    /// whose *real* UID is `A` must stay two rows. Matching only the string
    /// value merged them and one overwrote the other.
    #[tokio::test]
    async fn review_vtodo_derived_and_real_uid_are_distinct_resources() {
        let (ctx, service) = service().await;
        let list = ctx
            .db
            .read(|db| task_lists::get_required(db, "local-tasks"))
            .await
            .expect("list");
        let derived = crate::tasks::dav::task_remote_id("", "https://dav/tasks/A.ics");
        let real = crate::tasks::dav::task_remote_id("A", "https://dav/tasks/B.ics");
        assert_eq!(derived, real);
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![
                    remote_task_with_kind(
                        &list,
                        "No UID task",
                        &derived,
                        Some("https://dav/tasks/A.ics"),
                        Some("e1"),
                        true,
                    ),
                    remote_task_with_kind(
                        &list,
                        "Task with real UID A",
                        &real,
                        Some("https://dav/tasks/B.ics"),
                        Some("e2"),
                        false,
                    ),
                ],
                listed_hrefs: vec![
                    "https://dav/tasks/A.ics".into(),
                    "https://dav/tasks/B.ics".into(),
                ],
                full_sync: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            task_count(&service).await,
            2,
            "distinct resources must not overwrite each other because derived and real IDs share text"
        );
    }

    /// A tombstone for `uid-old` at a resource URL must not suppress a brand
    /// new task `uid-new` created at the same href.
    #[tokio::test]
    async fn review_vtodo_tombstone_does_not_suppress_a_different_uid_at_same_href() {
        let (ctx, service) = service().await;
        let list = ctx
            .db
            .read(|db| task_lists::get_required(db, "local-tasks"))
            .await
            .expect("list");
        let href = "https://dav/tasks/resource.ics";
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task(
                    &list,
                    "Old task",
                    "uid-old",
                    Some(href),
                    Some("e1"),
                )],
                ..Default::default()
            },
        )
        .await;
        let task = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .expect("query")
            .remove(0);
        service.task_delete(&task.id).await.expect("delete");
        let for_db = list.clone();
        ctx.db
            .write(move |db| {
                commit_delta(
                    db,
                    "local",
                    &for_db,
                    TaskCapabilities::all(),
                    &TaskDelta {
                        tasks: vec![remote_task(
                            &for_db,
                            "New task",
                            "uid-new",
                            Some("https://dav/tasks/resource.ics"),
                            Some("e2"),
                        )],
                        sync_token: Some("after-replacement".into()),
                        ..Default::default()
                    },
                )
            })
            .await
            .expect("replacement delta");
        assert_eq!(
            task_count(&service).await,
            1,
            "a different UID at the reused href is a new task, not the deleted object"
        );
    }

    /// A queued DELETE whose tombstone outlived the old wall-clock TTL must
    /// still suppress the object: the TTL starts only after settlement.
    #[tokio::test]
    async fn review_vtodo_pending_delete_outlives_tombstone_ttl() {
        let (ctx, service) = service().await;
        let list = ctx
            .db
            .read(|db| task_lists::get_required(db, "local-tasks"))
            .await
            .expect("list");
        let href = "https://dav/tasks/T1.ics";
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task(
                    &list,
                    "Delete me",
                    "T1",
                    Some(href),
                    Some("e1"),
                )],
                ..Default::default()
            },
        )
        .await;
        let task = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .expect("query")
            .remove(0);
        ctx.db
            .write(|db| {
                db.execute(
                    "UPDATE account SET provider = 'imap', caldav_url = 'https://dav/tasks/' WHERE id = 'local'",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("account");
        service.task_delete(&task.id).await.expect("delete");
        ctx.db
            .write(|db| {
                let queued = repo::pending_ops::list(db, Some("local"))?;
                assert!(
                    queued.iter().any(
                        |op| op.kind == ops::KIND_DELETE && op.state == PendingOpState::Pending
                    ),
                    "task_delete really queued a provider DELETE"
                );
                db.execute(
                    "UPDATE pim_task_delete_tombstone SET suppress_until = '2000-01-01T00:00:00Z'",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("age the tombstone");
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task(
                    &list,
                    "Delete me",
                    "T1",
                    Some(href),
                    Some("e1"),
                )],
                sync_token: Some("after-offline".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            task_count(&service).await,
            0,
            "a pending DELETE must keep suppressing the object regardless of elapsed wall time"
        );
    }

    struct ReviewTaskCreds;
    #[async_trait::async_trait]
    impl nuncio_core::traits::CredentialProvider for ReviewTaskCreds {
        async fn mail_auth(
            &self,
            _: &Account,
        ) -> nuncio_core::Result<nuncio_core::traits::MailAuth> {
            Err(nuncio_core::AppError::unsupported("no mail"))
        }
        async fn api_token(
            &self,
            _: &str,
            _: nuncio_core::traits::TokenResource,
        ) -> nuncio_core::Result<String> {
            Err(nuncio_core::AppError::unsupported("no token"))
        }
        async fn dav_auth(
            &self,
            _: &Account,
            _: DavService,
        ) -> nuncio_core::Result<(String, String)> {
            Ok(("user".into(), "password".into()))
        }
        async fn has_dav_password(&self, _: &str, _: DavService) -> bool {
            true
        }
        async fn report_auth_failure(&self, _: &str, _: nuncio_core::traits::TokenResource) {}
    }

    #[allow(clippy::type_complexity)]
    async fn review_dav_fixture(
        second_put_fails: bool,
    ) -> (
        CoreContext,
        TasksService,
        TaskList,
        String,
        tokio::sync::oneshot::Receiver<String>,
        Arc<Notify>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/tasks/", listener.local_addr().unwrap());
        let (arrived_tx, arrived) = tokio::sync::oneshot::channel();
        let release = Arc::new(Notify::new());
        let gate = release.clone();
        let server = tokio::spawn(async move {
            let mut arrived_tx = Some(arrived_tx);
            for request_index in 0..if second_put_fails { 2 } else { 1 } {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&chunk[..n]);
                    if let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&request[..header_end]);
                        let length: usize = header
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        if request.len() >= header_end + 4 + length {
                            break;
                        }
                    }
                }
                let text = String::from_utf8(request).unwrap();
                assert!(text.starts_with("PUT "), "expected task PUT: {text}");
                if request_index == 0 {
                    let uid = text
                        .lines()
                        .find_map(|l| l.strip_prefix("UID:"))
                        .unwrap()
                        .trim()
                        .to_string();
                    arrived_tx.take().unwrap().send(uid).unwrap();
                    gate.notified().await;
                    socket
                        .write_all(
                            b"HTTP/1.1 204 No Content\r\nETag: \"e2\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                } else {
                    socket
                        .write_all(
                            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                }
            }
        });
        let (ctx, _) = service().await;
        let dav_url = base.clone();
        ctx.db
            .write(move |db| {
                db.execute(
                    "UPDATE account SET provider = 'imap', caldav_url = ?1 WHERE id = 'local'",
                    rusqlite::params![dav_url],
                )?;
                store::set_task_list(
                    db,
                    &TaskListRemote {
                        task_list_id: "local-tasks".into(),
                        account_id: "local".into(),
                        provider: "caldav".into(),
                        remote_id: dav_url.clone(),
                        href: Some(dav_url),
                    },
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let service = TasksService::new(
            ctx.clone(),
            HttpClient::new(Arc::new(ReviewTaskCreds)).unwrap(),
        );
        let list = ctx
            .db
            .read(|db| task_lists::get_required(db, "local-tasks"))
            .await
            .unwrap();
        let href = format!("{base}A.ics");
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "Original",
                    "A",
                    Some(href.as_str()),
                    Some("\"e1\""),
                    true,
                )],
                ..Default::default()
            },
        )
        .await;
        (ctx, service, list, href, arrived, release, server)
    }

    /// Like [`review_dav_fixture`], but the provider commits the first PUT and
    /// then drops the connection without an acknowledgement: the write outcome
    /// is unknown and the receipt never arrives.
    #[allow(clippy::type_complexity)]
    async fn review_dav_lost_receipt_fixture() -> (
        CoreContext,
        TasksService,
        TaskList,
        String,
        tokio::sync::oneshot::Receiver<String>,
        Arc<Notify>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/tasks/", listener.local_addr().unwrap());
        let (arrived_tx, arrived) = tokio::sync::oneshot::channel();
        let release = Arc::new(Notify::new());
        let gate = release.clone();
        let server = tokio::spawn(async move {
            let mut arrived_tx = Some(arrived_tx);
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&chunk[..n]);
                if let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..header_end]);
                    let length: usize = header
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= header_end + 4 + length {
                        break;
                    }
                }
            }
            let text = String::from_utf8(request).unwrap();
            assert!(text.starts_with("PUT "), "expected task PUT: {text}");
            let uid = text
                .lines()
                .find_map(|l| l.strip_prefix("UID:"))
                .unwrap()
                .trim()
                .to_string();
            arrived_tx.take().unwrap().send(uid).unwrap();
            gate.notified().await;
            // The provider committed the PUT but lost the acknowledgement.
            drop(socket);
        });
        let (ctx, _) = service().await;
        let dav_url = base.clone();
        ctx.db
            .write(move |db| {
                db.execute(
                    "UPDATE account SET provider = 'imap', caldav_url = ?1 WHERE id = 'local'",
                    rusqlite::params![dav_url],
                )?;
                store::set_task_list(
                    db,
                    &TaskListRemote {
                        task_list_id: "local-tasks".into(),
                        account_id: "local".into(),
                        provider: "caldav".into(),
                        remote_id: dav_url.clone(),
                        href: Some(dav_url),
                    },
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let service = TasksService::new(
            ctx.clone(),
            HttpClient::new(Arc::new(ReviewTaskCreds)).unwrap(),
        );
        let list = ctx
            .db
            .read(|db| task_lists::get_required(db, "local-tasks"))
            .await
            .unwrap();
        let href = format!("{base}A.ics");
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "Original",
                    "A",
                    Some(href.as_str()),
                    Some("\"e1\""),
                    true,
                )],
                ..Default::default()
            },
        )
        .await;
        (ctx, service, list, href, arrived, release, server)
    }

    /// Regression 1: the local delete, its tombstone and the queued DELETE are
    /// one transaction. A failed enqueue must leave neither a tombstone nor a
    /// suppression that hides the task from later syncs.
    #[tokio::test]
    async fn review_vtodo_enqueue_failure_does_not_leave_an_eternal_orphan_tombstone() {
        let (ctx, service) = service().await;
        let list = ctx
            .db
            .read(|db| task_lists::get_required(db, "local-tasks"))
            .await
            .unwrap();
        let href = "https://dav/tasks/T1.ics";
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task(&list, "Task", "T1", Some(href), Some("e1"))],
                ..Default::default()
            },
        )
        .await;
        let task = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .unwrap()
            .remove(0);
        ctx.db
            .write(|db| {
                db.execute(
                    "UPDATE account SET provider='imap',caldav_url='https://dav/tasks/' WHERE id='local'",
                    [],
                )?;
                db.execute_batch(
                    "CREATE TRIGGER review_reject_delete_queue BEFORE INSERT ON pending_op \
                     WHEN NEW.kind = 'pim.task.delete' \
                     BEGIN SELECT RAISE(ABORT, 'injected queue failure'); END;",
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(
            service.task_delete(&task.id).await.is_err(),
            "a failed DELETE enqueue must fail the local delete"
        );
        ctx.db
            .write(|db| {
                db.execute_batch("DROP TRIGGER review_reject_delete_queue;")?;
                assert!(!repo::pending_ops::list(db, Some("local"))?
                    .iter()
                    .any(|op| op.kind == ops::KIND_DELETE));
                assert!(
                    service_task_tombstones(db).is_empty(),
                    "the rolled-back delete must not leave a tombstone"
                );
                db.execute(
                    "UPDATE pim_task_delete_tombstone SET suppress_until='2000-01-01T00:00:00Z'",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task(&list, "Task", "T1", Some(href), Some("e1"))],
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            task_count(&service).await,
            1,
            "a failed enqueue must not leave no DELETE and a task that is suppressed forever"
        );
    }

    fn service_task_tombstones(conn: &rusqlite::Connection) -> Vec<store::TaskDeleteTombstone> {
        store::active_task_delete_tombstones(conn, "local-tasks", Utc::now()).unwrap_or_default()
    }

    /// Regression 2: A confirms a real UID/ETag while B is queued. B's failure
    /// must roll back content only; the confirmed identity stays.
    #[tokio::test]
    async fn review_vtodo_save_rollback_keeps_a_confirmed_real_uid() {
        let (ctx, service, _, _, arrived, release, server) = review_dav_fixture(true).await;
        let task = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .unwrap()
            .remove(0);
        service
            .task_update(&task.id, input("Edit A"))
            .await
            .unwrap();
        let first = tokio::spawn({
            let ctx = ctx.clone();
            let service = service.clone();
            async move { op_worker::process_pass(&ctx, Arc::new(service)).await }
        });
        let uid = tokio::time::timeout(std::time::Duration::from_secs(5), arrived)
            .await
            .unwrap()
            .unwrap();
        service
            .task_update(&task.id, input("Edit B while A is on the wire"))
            .await
            .unwrap();
        release.notify_one();
        first.await.unwrap().unwrap();
        let before = ctx
            .db
            .read({
                let id = task.id.clone();
                move |db| snapshot_task(db, &id)
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.remote_id.as_deref(), Some(uid.as_str()));
        assert!(!before.uid_is_derived);
        op_worker::process_pass(&ctx, Arc::new(service.clone()))
            .await
            .unwrap();
        server.await.unwrap();
        let after = ctx
            .db
            .read({
                let id = task.id.clone();
                move |db| snapshot_task(db, &id)
            })
            .await
            .unwrap()
            .unwrap();
        // `DavClient::put_resource` strips the surrounding quotes from the
        // ETag header, so the confirmed value is stored as `e2`.
        assert_eq!(
            (after.remote_id, after.uid_is_derived, after.etag),
            (Some(uid), false, Some("e2".into())),
            "failure of B must preserve the real UID and ETag confirmed by A"
        );
    }

    /// Regression: an older save must not roll its snapshot over content a
    /// newer save confirmed, even when the newer operation has already been
    /// pruned and both writes share the same stored second.
    ///
    /// The queue row of the confirmed save is deliberately removed with the
    /// production `prune_finished`; only the task row's persisted content
    /// owner is left to prove that the older snapshot no longer applies.
    #[tokio::test]
    async fn audit_old_task_rollback_cannot_erase_a_same_second_confirmed_edit_after_prune() {
        let (ctx, service, _, _, arrived, release, server) = review_dav_fixture(true).await;
        let task = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .unwrap()
            .remove(0);
        service
            .task_update(&task.id, input("Older B"))
            .await
            .unwrap();
        let older = ctx
            .db
            .write(|db| {
                let older = repo::pending_ops::list(db, Some("local"))?
                    .into_iter()
                    .find(|op| op.kind == ops::KIND_SAVE)
                    .unwrap();
                // Keep B out of the first pass; only C may reach the server.
                db.execute(
                    "UPDATE pending_op SET next_retry_at = '2099-01-01T00:00:00Z' WHERE id = ?1",
                    rusqlite::params![older.id],
                )?;
                Ok(older)
            })
            .await
            .unwrap();
        service
            .task_update(&task.id, input("Confirmed C must remain"))
            .await
            .unwrap();
        let pass = tokio::spawn({
            let ctx = ctx.clone();
            let service = service.clone();
            async move { op_worker::process_pass(&ctx, Arc::new(service)).await }
        });
        let _uid = tokio::time::timeout(std::time::Duration::from_secs(5), arrived)
            .await
            .unwrap()
            .unwrap();
        release.notify_one();
        pass.await.unwrap().unwrap();
        assert_eq!(
            service.task_get(&task.id).await.unwrap().title,
            "Confirmed C must remain"
        );
        let task_id = task.id.clone();
        ctx.db
            .write(move |db| {
                // C completed in the same stored second as B was enqueued.
                // Age every queue row and the task row equally so the old
                // second-precision comparison would consider them tied, then
                // let the production retention drop C's `done` evidence.
                db.execute(
                    "UPDATE pending_op SET created_at = '2000-01-01T00:00:00Z', \
                     updated_at = '2000-01-01T00:00:00Z'",
                    [],
                )?;
                db.execute(
                    "UPDATE task SET created_at = '2000-01-01T00:00:00Z', \
                     updated_at = '2000-01-01T00:00:00Z' WHERE id = ?1",
                    rusqlite::params![task_id],
                )?;
                assert_eq!(
                    repo::pending_ops::prune_finished(db, Utc::now() - chrono::Duration::days(7))?,
                    1
                );
                assert_eq!(
                    repo::pending_ops::list(db, Some("local"))?.len(),
                    1,
                    "C's receipt is gone but B remains pending"
                );
                db.execute(
                    "UPDATE pending_op SET next_retry_at = NULL, attempts = 5 WHERE id = ?1",
                    rusqlite::params![older.id],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        op_worker::process_pass(&ctx, Arc::new(service.clone()))
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(
            service.task_get(&task.id).await.unwrap().title,
            "Confirmed C must remain",
            "pruning C removed the only seq evidence; the task row's persisted \
             content owner must still protect the confirmed edit"
        );
    }

    /// Regression 3: deleting while a PUT is in flight must not resurrect the
    /// task, must not fail the href foreign key, and must suppress the real UID
    /// the late receipt reports.
    #[tokio::test]
    async fn review_vtodo_inflight_save_does_not_resurrect_a_deleted_task() {
        let (ctx, service, list, href, arrived, release, server) = review_dav_fixture(false).await;
        let task = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .unwrap()
            .remove(0);
        service
            .task_update(&task.id, input("Assign UID"))
            .await
            .unwrap();
        let pass = tokio::spawn({
            let ctx = ctx.clone();
            let service = service.clone();
            async move { op_worker::process_pass(&ctx, Arc::new(service)).await }
        });
        let uid = tokio::time::timeout(std::time::Duration::from_secs(5), arrived)
            .await
            .unwrap()
            .unwrap();
        service.task_delete(&task.id).await.unwrap();
        release.notify_one();
        pass.await.unwrap().unwrap();
        server.await.unwrap();
        let revived = ctx
            .db
            .read({
                let id = task.id.clone();
                move |db| snapshot_task(db, &id)
            })
            .await
            .unwrap();
        assert!(
            revived.is_none(),
            "late Save receipt/rollback resurrected the locally deleted task before any delta: {:?}",
            revived
                .as_ref()
                .map(|s| (&s.remote_id, s.uid_is_derived, &s.etag))
        );
        assert!(ctx
            .db
            .read(|db| Ok(repo::pending_ops::list(db, Some("local"))?
                .iter()
                .any(|op| op.kind == ops::KIND_DELETE && op.state == PendingOpState::Pending)))
            .await
            .unwrap());
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "Assigned UID echo",
                    &uid,
                    Some(&href),
                    Some("\"e2\""),
                    false,
                )],
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            task_count(&service).await,
            0,
            "deleting during PUT must also suppress the real UID in that PUT receipt"
        );
    }

    /// Regression 4: the receipt of an in-flight rewrite upgraded the tombstone
    /// from a derived placeholder to a real UID. A delta captured before the
    /// rewrite still names the derived identity and must stay suppressed; the
    /// upgrade must add the new identity, not replace the old one.
    #[tokio::test]
    async fn review_vtodo_late_uidless_snapshot_remains_suppressed_after_rekey() {
        let (ctx, service, list, href, arrived, release, server) = review_dav_fixture(false).await;
        let task = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .unwrap()
            .remove(0);
        service
            .task_update(&task.id, input("Assign UID"))
            .await
            .unwrap();
        let pass = tokio::spawn({
            let ctx = ctx.clone();
            let service = service.clone();
            async move { op_worker::process_pass(&ctx, Arc::new(service)).await }
        });
        let uid = tokio::time::timeout(std::time::Duration::from_secs(5), arrived)
            .await
            .unwrap()
            .unwrap();
        service.task_delete(&task.id).await.unwrap();
        release.notify_one();
        pass.await.unwrap().unwrap();
        server.await.unwrap();
        assert_eq!(task_count(&service).await, 0);
        let stones = ctx
            .db
            .read(|db| store::active_task_delete_tombstones(db, "local-tasks", Utc::now()))
            .await
            .unwrap();
        assert!(
            stones
                .iter()
                .any(|stone| stone.remote_id.as_deref() == Some("A") && stone.uid_is_derived),
            "the pre-upgrade derived tombstone must survive re-keying"
        );
        assert!(
            stones
                .iter()
                .any(|stone| stone.remote_id.as_deref() == Some(uid.as_str())
                    && !stone.uid_is_derived),
            "the confirmed real UID must also be tombstoned"
        );
        // This snapshot was fetched before PUT assigned the real UID; its
        // delivery is allowed to finish after the receipt re-keyed the
        // tombstone.
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "Old UID-less snapshot",
                    "A",
                    Some(href.as_str()),
                    Some("\"e1\""),
                    true,
                )],
                sync_token: Some("after-old-snapshot".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            task_count(&service).await,
            0,
            "re-keying must not release the old identity while its delayed delta can still arrive"
        );
    }

    /// Regression 5: same in-flight upgrade, but the delayed snapshot already
    /// carries the real UID. The added tombstone must suppress it.
    #[tokio::test]
    async fn review_vtodo_late_real_uid_snapshot_remains_suppressed_after_rekey() {
        let (ctx, service, list, href, arrived, release, server) = review_dav_fixture(false).await;
        let task = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .unwrap()
            .remove(0);
        service
            .task_update(&task.id, input("Assign UID"))
            .await
            .unwrap();
        let pass = tokio::spawn({
            let ctx = ctx.clone();
            let service = service.clone();
            async move { op_worker::process_pass(&ctx, Arc::new(service)).await }
        });
        let uid = tokio::time::timeout(std::time::Duration::from_secs(5), arrived)
            .await
            .unwrap()
            .unwrap();
        service.task_delete(&task.id).await.unwrap();
        release.notify_one();
        pass.await.unwrap().unwrap();
        server.await.unwrap();
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "New UID snapshot",
                    &uid,
                    Some(&href),
                    Some("\"e2\""),
                    false,
                )],
                sync_token: Some("after-new-snapshot".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            task_count(&service).await,
            0,
            "the confirmed real-UID identity must suppress the delayed snapshot too"
        );
    }

    /// Regression 6: a create whose PUT committed but whose acknowledgement was
    /// lost, deleted locally in the meantime. CalDAV lets the client choose the
    /// UID/href, so the delete records that identity and the delta echo cannot
    /// re-import the task.
    #[tokio::test]
    async fn review_vtodo_deleted_inflight_create_with_lost_receipt_does_not_return() {
        let (ctx, service, list, seed_href, arrived, release, server) =
            review_dav_lost_receipt_fixture().await;
        let task = service
            .task_create("local-tasks", input("Create then delete"))
            .await
            .unwrap();
        let pass = tokio::spawn({
            let ctx = ctx.clone();
            let service = service.clone();
            async move { op_worker::process_pass(&ctx, Arc::new(service)).await }
        });
        let uid = tokio::time::timeout(std::time::Duration::from_secs(5), arrived)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            uid, task.id,
            "the provider wrote this create's deterministic UID"
        );
        service.task_delete(&task.id).await.unwrap();
        release.notify_one();
        pass.await.unwrap().unwrap();
        server.await.unwrap();
        let intent = ctx
            .db
            .read({
                let id = task.id.clone();
                move |db| store::task_delete_intent(db, &id)
            })
            .await
            .unwrap()
            .expect("delete intent survives the lost receipt");
        assert_eq!(
            intent.remote_id.as_deref(),
            Some(task.id.as_str()),
            "the identity the create will use must be attributed before its receipt"
        );
        assert!(
            intent.href.is_some(),
            "the deterministic CalDAV resource href must be attributed too"
        );
        assert!(!intent.uid_is_derived);
        let save = ctx
            .db
            .read(|db| {
                Ok(repo::pending_ops::list(db, Some("local"))?
                    .into_iter()
                    .find(|op| op.kind == ops::KIND_SAVE)
                    .unwrap())
            })
            .await
            .unwrap();
        assert!(
            save.send_intent && save.outcome_unknown && save.state == PendingOpState::Pending,
            "the request is ambiguous, not a definitive rejection"
        );
        let href = format!("{}{}.ics", seed_href.strip_suffix("A.ics").unwrap(), uid);
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "Server-committed create",
                    &uid,
                    Some(&href),
                    Some("\"e2\""),
                    false,
                )],
                sync_token: Some("after-lost-create-receipt".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            task_count(&service).await,
            1,
            "only the unrelated fixture task may remain; the deleted in-flight create must not return"
        );
    }

    /// Regression 7: a provider that allocates ids server-side gives the delete
    /// nothing to attribute. The identity-less intent must defer imports of new
    /// objects (holding the cursor) instead of treating "committed but
    /// unacknowledged" as "not created".
    #[tokio::test]
    async fn review_vtodo_unattributed_uncertain_create_defers_imports() {
        let (ctx, service) = service().await;
        let list = ctx
            .db
            .read(|db| task_lists::get_required(db, "local-tasks"))
            .await
            .unwrap();
        let op_id = ctx
            .db
            .write(|conn| {
                let op = repo::pending_ops::enqueue(
                    conn,
                    NewPendingOp::new(
                        "local",
                        ops::KIND_SAVE,
                        serde_json::json!({ "taskId": "ghost" }),
                        "create".to_string(),
                    ),
                )?;
                repo::pending_ops::mark_send_intent(conn, &op.id)?;
                repo::pending_ops::mark_outcome_unknown(conn, &op.id)?;
                store::record_task_delete_intent(
                    conn,
                    "ghost",
                    "local",
                    "local-tasks",
                    None,
                    None,
                    None,
                    false,
                    Some(&op.id),
                    Some(0),
                )?;
                Ok(op.id)
            })
            .await
            .unwrap();
        assert!(!op_id.is_empty());
        let list_for_db = list.clone();
        let cursor_may_advance = ctx
            .db
            .write(move |conn| {
                apply_delta(
                    conn,
                    "local",
                    &list_for_db,
                    TaskCapabilities::all(),
                    &TaskDelta {
                        tasks: vec![remote_task(
                            &list_for_db,
                            "Server-committed create",
                            "ghost-uid",
                            None,
                            None,
                        )],
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap();
        assert!(
            !cursor_may_advance,
            "the cursor must be held while an unattributable create can resolve"
        );
        assert_eq!(task_count(&service).await, 0);
    }

    #[allow(clippy::type_complexity)]
    async fn audit_dav_alias_fixture(
        second_put_fails: bool,
    ) -> (
        CoreContext,
        TasksService,
        TaskList,
        String,
        tokio::sync::oneshot::Receiver<String>,
        Arc<Notify>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/tasks/", listener.local_addr().unwrap());
        let (arrived_tx, arrived) = tokio::sync::oneshot::channel();
        let release = Arc::new(Notify::new());
        let gate = release.clone();
        let server = tokio::spawn(async move {
            let mut arrived_tx = Some(arrived_tx);
            for request_index in 0..if second_put_fails { 2 } else { 1 } {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&chunk[..n]);
                    if let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&request[..header_end]);
                        let length: usize = header
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        if request.len() >= header_end + 4 + length {
                            break;
                        }
                    }
                }
                let text = String::from_utf8(request).unwrap();
                assert!(
                    (request_index == 0 && text.starts_with("PUT "))
                        || (request_index == 1 && text.starts_with("DELETE ")),
                    "unexpected DAV request: {text}"
                );
                if request_index == 0 {
                    let uid = text
                        .lines()
                        .find_map(|l| l.strip_prefix("UID:"))
                        .unwrap()
                        .trim()
                        .to_string();
                    arrived_tx.take().unwrap().send(uid).unwrap();
                    gate.notified().await;
                    socket
                        .write_all(
                            b"HTTP/1.1 204 No Content\r\nETag: \"e2\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                } else {
                    socket
                        .write_all(
                            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                }
            }
        });
        let (ctx, _) = service().await;
        let dav_url = base.clone();
        ctx.db
            .write(move |db| {
                db.execute(
                    "UPDATE account SET provider = 'imap', caldav_url = ?1 WHERE id = 'local'",
                    rusqlite::params![dav_url],
                )?;
                store::set_task_list(
                    db,
                    &TaskListRemote {
                        task_list_id: "local-tasks".into(),
                        account_id: "local".into(),
                        provider: "caldav".into(),
                        remote_id: dav_url.clone(),
                        href: Some(dav_url),
                    },
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let service = TasksService::new(
            ctx.clone(),
            HttpClient::new(Arc::new(ReviewTaskCreds)).unwrap(),
        );
        let list = ctx
            .db
            .read(|db| task_lists::get_required(db, "local-tasks"))
            .await
            .unwrap();
        let href = format!("{base}A.ics");
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "Original",
                    "A",
                    Some(href.as_str()),
                    Some("\"e1\""),
                    true,
                )],
                ..Default::default()
            },
        )
        .await;
        (ctx, service, list, href, arrived, release, server)
    }

    /// The resource is written under `%41.ics`, the sync then stores the plain
    /// spelling, and the user deletes it. The successful DELETE must settle the
    /// derived tombstone for *both* spellings; leaving one `pending` would
    /// suppress every later UID-less resource at that URL forever.
    #[tokio::test]
    async fn audit_vtodo_equivalent_href_is_settled_after_uid_upgrade_and_delete() {
        let (ctx, service, list, plain, arrived, release, server) =
            audit_dav_alias_fixture(true).await;
        let task = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .unwrap()
            .remove(0);
        let encoded = plain.replace("A.ics", "%41.ics");
        assert_eq!(crate::dav::href_key(&plain), crate::dav::href_key(&encoded));
        ctx.db
            .write({
                let id = task.id.clone();
                let href = encoded.clone();
                move |db| store::set_task_href(db, &id, &href)
            })
            .await
            .unwrap();
        service
            .task_update(&task.id, input("Assign real UID"))
            .await
            .unwrap();
        let pass = tokio::spawn({
            let ctx = ctx.clone();
            let service = service.clone();
            async move { op_worker::process_pass(&ctx, Arc::new(service)).await }
        });
        let _uid = tokio::time::timeout(std::time::Duration::from_secs(5), arrived)
            .await
            .unwrap()
            .unwrap();
        // The service can sync while its operation worker waits for PUT. This
        // equivalent spelling becomes the current row's raw href at deletion.
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "Server snapshot",
                    "A",
                    Some(&plain),
                    Some("e1"),
                    true,
                )],
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            ctx.db
                .read({
                    let id = task.id.clone();
                    move |db| store::task_href(db, &id)
                })
                .await
                .unwrap()
                .as_deref(),
            Some(plain.as_str())
        );
        service.task_delete(&task.id).await.unwrap();
        release.notify_one();
        pass.await.unwrap().unwrap();
        // The receipt has the request's encoded href. The actual DELETE then
        // succeeds with that same href and the confirmed new ETag.
        op_worker::process_pass(&ctx, Arc::new(service.clone()))
            .await
            .unwrap();
        server.await.unwrap();
        ctx.db
            .write(|db| {
                assert!(
                    repo::pending_ops::list(db, Some("local"))?.iter().any(|op| {
                        op.kind == ops::KIND_DELETE && op.state == PendingOpState::Done
                    }),
                    "the DELETE succeeded"
                );
                db.execute(
                    "UPDATE pim_task_delete_tombstone SET suppress_until='2000-01-01T00:00:00Z' WHERE state='settled'",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "A newly uploaded UID-less resource",
                    "A",
                    Some(&plain),
                    Some("e3"),
                    true,
                )],
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            task_count(&service).await,
            1,
            "an equivalent raw href must not leave the old derived tombstone permanently pending after successful DELETE"
        );
    }

    /// A child with no local step mapping is a new provider object. While an
    /// unattributed delete intent can still resolve, importing it would risk
    /// resurrecting the deleted task through its child, so the batch is
    /// deferred and the cursor held exactly like a new top-level object.
    #[tokio::test]
    async fn audit_vtodo_unattributed_isolation_defers_new_children_of_known_parents() {
        let (ctx, service) = service().await;
        let list = ctx
            .db
            .read(|db| task_lists::get_required(db, "local-tasks"))
            .await
            .unwrap();
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task(&list, "Known parent", "P", None, None)],
                ..Default::default()
            },
        )
        .await;
        let parent = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .unwrap()
            .remove(0);
        ctx.db
            .write(|db| {
                let payload = ops::to_value(&TaskOp::Save {
                    task_id: "deleted-create".into(),
                    assigned_uid: None,
                    snapshot: None,
                })
                .unwrap();
                let op = repo::pending_ops::enqueue(
                    db,
                    NewPendingOp::new("local", ops::KIND_SAVE, payload, "Unknown create"),
                )?;
                repo::pending_ops::mark_send_intent(db, &op.id)?;
                repo::pending_ops::mark_outcome_unknown(db, &op.id)?;
                store::record_task_delete_intent(
                    db,
                    "deleted-create",
                    "local",
                    "local-tasks",
                    None,
                    None,
                    None,
                    false,
                    Some(&op.id),
                    Some(0),
                )?;
                save_cursor(db, "local-tasks", Some("cursor-before-child"), None)?;
                Ok(())
            })
            .await
            .unwrap();
        let mut child = remote_task(&list, "New provider child", "C", None, None);
        child.parent_remote_id = Some("P".into());
        let list_copy = list.clone();
        ctx.db
            .write(move |db| {
                commit_delta(
                    db,
                    "local",
                    &list_copy,
                    TaskCapabilities::all(),
                    &TaskDelta {
                        tasks: vec![child],
                        sync_token: Some("cursor-after-child".into()),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap();
        let (step_count, cursor) = ctx
            .db
            .read(move |db| {
                Ok((
                    tasks_repo::get_required(db, &parent.id)?.steps.len(),
                    load_cursor(db, "local-tasks")?.and_then(|c| c.sync_token),
                ))
            })
            .await
            .unwrap();
        assert_eq!(
            (step_count, cursor),
            (0, Some("cursor-before-child".to_string())),
            "a new child is still an unattributed new object; defer it and hold the cursor"
        );
    }

    /* ───────── row + queue atomicity regressions ────────────────────── */

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ContentState {
        content_rev: i64,
        content_op_id: Option<String>,
        row_rev: i64,
        row_op_id: Option<String>,
    }

    async fn content_state(ctx: &CoreContext, task_id: &str) -> ContentState {
        let task_id = task_id.to_string();
        ctx.db
            .read(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT content_rev, content_op_id, row_rev, row_op_id \
                         FROM pim_task_content_state WHERE task_id = ?1",
                        rusqlite::params![task_id],
                        |row| {
                            Ok(ContentState {
                                content_rev: row.get(0)?,
                                content_op_id: row.get(1)?,
                                row_rev: row.get(2)?,
                                row_op_id: row.get(3)?,
                            })
                        },
                    )
                    .optional()?
                    .unwrap_or(ContentState {
                        content_rev: 0,
                        content_op_id: None,
                        row_rev: 0,
                        row_op_id: None,
                    }))
            })
            .await
            .expect("content state")
    }

    async fn queue_len(ctx: &CoreContext) -> usize {
        ctx.db
            .read(|conn| repo::pending_ops::list(conn, Some("local")))
            .await
            .expect("queue")
            .len()
    }

    async fn sort_orders(ctx: &CoreContext, list_id: &str) -> Vec<(String, i64)> {
        let list_id = list_id.to_string();
        ctx.db
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, sort_order FROM task WHERE list_id = ?1 ORDER BY id ASC",
                )?;
                let rows = stmt.query_map(rusqlite::params![list_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            })
            .await
            .expect("sort orders")
    }

    /// The local row write, its content/row owner state and the queued
    /// operation must commit together. Injecting a `pending_op` insert failure
    /// must roll every one of them back, for every entry point that both edits
    /// a row and enqueues its upload — otherwise the row keeps an owner that
    /// names an operation which does not exist, and a stale delta can then
    /// overwrite the edit because no queued save protects it.
    #[tokio::test]
    async fn task_row_and_queue_write_commit_together() {
        let (ctx, service) = service().await;
        make_provider(&ctx).await;
        let other = ctx
            .db
            .write(|conn| task_lists::create(conn, "local", "Other", "#00AA00"))
            .await
            .expect("other list");
        let original = service
            .task_create("local-tasks", input("Original"))
            .await
            .expect("create");
        let second = service
            .task_create("local-tasks", input("Second"))
            .await
            .expect("create");

        let baseline = content_state(&ctx, &original.id).await;
        assert!(
            baseline.content_op_id.is_some(),
            "a provider create records the queued operation as its content owner"
        );
        let queue_before = queue_len(&ctx).await;

        ctx.db
            .write(|conn| {
                Ok(conn.execute_batch(
                    "CREATE TRIGGER audit_reject_queue BEFORE INSERT ON pending_op \
                     BEGIN SELECT RAISE(ABORT, 'injected queue failure'); END;",
                )?)
            })
            .await
            .expect("trigger");

        assert!(
            service
                .task_create("local-tasks", input("Ghost"))
                .await
                .is_err(),
            "a failed enqueue must fail the create"
        );
        assert_eq!(
            ctx.db
                .read(|conn| tasks_repo::count_in_list(conn, "local-tasks"))
                .await
                .expect("task count"),
            2,
            "a failed create leaves no row behind"
        );
        assert_eq!(
            content_state(&ctx, &original.id).await,
            baseline,
            "a failed create must not bump another row's content state"
        );

        assert!(service
            .task_update(&original.id, input("Edited"))
            .await
            .is_err());
        assert_eq!(
            service.task_get(&original.id).await.expect("task").title,
            "Original",
            "a failed update rolls the row content back"
        );
        assert_eq!(content_state(&ctx, &original.id).await, baseline);

        assert!(service
            .task_set_completed(&original.id, true)
            .await
            .is_err());
        assert_eq!(
            service.task_get(&original.id).await.expect("task").status,
            TaskStatus::NotStarted,
            "a failed completion rolls the status back"
        );

        assert!(service
            .task_set_importance(&original.id, Importance::High)
            .await
            .is_err());
        assert_eq!(
            service
                .task_get(&original.id)
                .await
                .expect("task")
                .importance,
            Importance::Normal
        );

        assert!(service.task_move(&original.id, &other.id).await.is_err());
        assert_eq!(
            service.task_get(&original.id).await.expect("task").list_id,
            "local-tasks",
            "a failed move rolls the list change back"
        );

        let order_before = sort_orders(&ctx, "local-tasks").await;
        assert!(service
            .task_reorder(
                "local-tasks",
                vec![second.id.clone(), original.id.clone()],
            )
            .await
            .is_err());
        assert_eq!(
            sort_orders(&ctx, "local-tasks").await,
            order_before,
            "a failed reorder rolls the local order back"
        );

        assert_eq!(
            queue_len(&ctx).await,
            queue_before,
            "no queue entry may survive a rolled-back write"
        );
        assert_eq!(content_state(&ctx, &original.id).await, baseline);

        let ghosts: i64 = ctx
            .db
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM pim_task_content_state s \
                     WHERE COALESCE(s.content_op_id, '') <> '' \
                       AND NOT EXISTS (SELECT 1 FROM pending_op o WHERE o.id = s.content_op_id)",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("ghost owners");
        assert_eq!(ghosts, 0, "no row may be owned by a missing operation");

        // With the trigger gone the same entries queue normally again.
        ctx.db
            .write(|conn| Ok(conn.execute_batch("DROP TRIGGER audit_reject_queue;")?))
            .await
            .expect("drop trigger");
        service
            .task_update(&original.id, input("After recovery"))
            .await
            .expect("update queues");
        assert_eq!(
            service.task_get(&original.id).await.expect("task").title,
            "After recovery"
        );
        assert_eq!(queue_len(&ctx).await, queue_before + 1);
    }

    /// A CalDAV fixture that records the single PUT the worker sends.
    #[allow(clippy::type_complexity)]
    async fn review_dav_put_capture_fixture() -> (
        CoreContext,
        TasksService,
        TaskList,
        String,
        tokio::sync::oneshot::Receiver<String>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/tasks/", listener.local_addr().unwrap());
        let (request_tx, request_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&chunk[..n]);
                if let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..header_end]);
                    let length: usize = header
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= header_end + 4 + length {
                        break;
                    }
                }
            }
            request_tx
                .send(String::from_utf8(request).unwrap())
                .expect("capture request");
            socket
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nETag: \"e2\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let (ctx, _) = service().await;
        let dav_url = base.clone();
        ctx.db
            .write(move |db| {
                db.execute(
                    "UPDATE account SET provider = 'imap', caldav_url = ?1 WHERE id = 'local'",
                    rusqlite::params![dav_url],
                )?;
                store::set_task_list(
                    db,
                    &TaskListRemote {
                        task_list_id: "local-tasks".into(),
                        account_id: "local".into(),
                        provider: "caldav".into(),
                        remote_id: dav_url.clone(),
                        href: Some(dav_url),
                    },
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let service = TasksService::new(
            ctx.clone(),
            HttpClient::new(Arc::new(ReviewTaskCreds)).unwrap(),
        );
        let list = ctx
            .db
            .read(|db| task_lists::get_required(db, "local-tasks"))
            .await
            .unwrap();
        let href = format!("{base}A.ics");
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "Original",
                    "A",
                    Some(href.as_str()),
                    Some("\"e1\""),
                    true,
                )],
                ..Default::default()
            },
        )
        .await;
        (ctx, service, list, href, request_rx, server)
    }

    /// An old delta captured before the edit can land after the queue insert
    /// (both timestamps have second precision). Because the edit and its
    /// upload intent now commit in one transaction, the pending save protects
    /// the row and the PUT carries the user's latest content, not the stale
    /// server copy.
    #[tokio::test]
    async fn queued_save_uploads_latest_content_after_a_stale_delta() {
        let (ctx, service, list, href, request_rx, server) =
            review_dav_put_capture_fixture().await;
        let task = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .unwrap()
            .remove(0);
        service
            .task_update(&task.id, input("用户最新内容"))
            .await
            .expect("edit is queued with the row");

        // Same stored second: a delta captured before the edit arrives now.
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "旧快照",
                    "A",
                    Some(href.as_str()),
                    Some("\"e1\""),
                    true,
                )],
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            service.task_get(&task.id).await.expect("task").title,
            "用户最新内容",
            "a stale delta must not overwrite a queued edit"
        );

        let save = ctx
            .db
            .read(|db| {
                repo::pending_ops::list(db, Some("local")).map(|ops| {
                    ops.into_iter()
                        .find(|op| op.kind == ops::KIND_SAVE)
                        .expect("queued save")
                })
            })
            .await
            .expect("queue");
        assert_eq!(
            ops::decode(&save)
                .ok()
                .and_then(|op| match op {
                    TaskOp::Save { task_id, .. } => Some(task_id),
                    _ => None,
                })
                .as_deref(),
            Some(task.id.as_str())
        );

        op_worker::process_pass(&ctx, Arc::new(service.clone()))
            .await
            .expect("worker pass");
        let request = tokio::time::timeout(std::time::Duration::from_secs(5), request_rx)
            .await
            .expect("worker sent no PUT")
            .expect("captured request");
        server.await.unwrap();
        assert!(
            request.contains("用户最新内容"),
            "the upload must carry the user's latest content: {request}"
        );
        assert!(
            !request.contains("旧快照"),
            "the upload must not carry the stale snapshot: {request}"
        );
    }

    /// Regression ①: a late receipt from an older save must not overwrite the
    /// `remote_id` and identity kind a newer save already confirmed.
    #[tokio::test]
    async fn late_save_receipt_keeps_the_newer_confirmed_identity() {
        let (ctx, service) = service().await;
        let task = service
            .task_create("local-tasks", input("Receipt ownership"))
            .await
            .expect("create");
        let id = task.id.clone();
        let base = ctx
            .db
            .read({
                let id = id.clone();
                move |conn| snapshot_task(conn, &id)
            })
            .await
            .expect("snapshot")
            .expect("task row");

        let old = TaskSaveReceipt {
            remote_id: Some("old-uid".into()),
            href: Some("https://dav/tasks/old.ics".into()),
            etag: Some("\"old\"".into()),
            uid_is_derived: true,
        };
        let new = TaskSaveReceipt {
            remote_id: Some("new-uid".into()),
            href: Some("https://dav/tasks/new.ics".into()),
            etag: Some("\"new\"".into()),
            uid_is_derived: false,
        };
        ctx.db
            .write({
                let id = id.clone();
                let base = base.clone();
                move |conn| {
                    // The older save's local write owns the row, then confirms
                    // its identity.
                    store::bump_task_content(conn, &id, Some("op-old"))?;
                    record_task_save_receipt(conn, "local", "op-old", &base, &old)?;
                    // A newer save supersedes it and confirms a real UID.
                    store::bump_task_content(conn, &id, Some("op-new"))?;
                    record_task_save_receipt(conn, "local", "op-new", &base, &new)?;
                    // The older receipt arrives late and must be dropped.
                    record_task_save_receipt(conn, "local", "op-old", &base, &old)?;
                    Ok(())
                }
            })
            .await
            .expect("receipts");

        let meta = ctx
            .db
            .read({
                let id = id.clone();
                move |conn| tasks_repo::sync_meta(conn, &id)
            })
            .await
            .expect("meta")
            .expect("task row");
        assert_eq!(
            meta.remote_id.as_deref(),
            Some("new-uid"),
            "the late receipt must not replace the newer confirmed remote id"
        );
        assert_eq!(
            meta.etag.as_deref(),
            Some("\"new\""),
            "the late receipt must not replace the newer ETag"
        );
        let (href, uid_is_derived) = ctx
            .db
            .read({
                let id = id.clone();
                move |conn| {
                    Ok((
                        store::task_href(conn, &id)?,
                        store::task_uid_is_derived(conn, &id)?,
                    ))
                }
            })
            .await
            .expect("identity");
        assert_eq!(href.as_deref(), Some("https://dav/tasks/new.ics"));
        assert!(
            !uid_is_derived,
            "the identity kind confirmed by the newer save must survive"
        );
    }

    /// A delta that overwrites the row writer must not erase the proof that a
    /// newer operation already confirmed an identity: an older receipt that
    /// arrives afterwards is still dropped by the confirmed-receipt record.
    #[tokio::test]
    async fn late_save_receipt_after_a_delta_keeps_the_newer_confirmed_identity() {
        let (ctx, service) = service().await;
        let task = service
            .task_create("local-tasks", input("Receipt ownership after delta"))
            .await
            .expect("create");
        let id = task.id.clone();
        let base = ctx
            .db
            .read({
                let id = id.clone();
                move |conn| snapshot_task(conn, &id)
            })
            .await
            .expect("snapshot")
            .expect("task row");

        let old = TaskSaveReceipt {
            remote_id: Some("old-uid".into()),
            href: Some("https://dav/tasks/old.ics".into()),
            etag: Some("\"old\"".into()),
            uid_is_derived: true,
        };
        let new = TaskSaveReceipt {
            remote_id: Some("new-uid".into()),
            href: Some("https://dav/tasks/new.ics".into()),
            etag: Some("\"new\"".into()),
            uid_is_derived: false,
        };
        ctx.db
            .write({
                let id = id.clone();
                let base = base.clone();
                move |conn| {
                    store::bump_task_content(conn, &id, Some("op-old"))?;
                    record_task_save_receipt(conn, "local", "op-old", &base, &old)?;
                    store::bump_task_content(conn, &id, Some("op-new"))?;
                    record_task_save_receipt(conn, "local", "op-new", &base, &new)?;
                    // A sync upsert attributes the row write to the external
                    // writer, erasing the queued-operation owner.
                    store::bump_task_content(conn, &id, None)?;
                    // The older receipt then arrives late.
                    record_task_save_receipt(conn, "local", "op-old", &base, &old)?;
                    Ok(())
                }
            })
            .await
            .expect("receipts");

        let meta = ctx
            .db
            .read({
                let id = id.clone();
                move |conn| tasks_repo::sync_meta(conn, &id)
            })
            .await
            .expect("meta")
            .expect("task row");
        assert_eq!(
            meta.remote_id.as_deref(),
            Some("new-uid"),
            "a delta must not let a late receipt overwrite the newer confirmed remote id"
        );
        assert_eq!(
            meta.etag.as_deref(),
            Some("\"new\""),
            "the newer confirmed ETag must survive"
        );
        let (href, uid_is_derived) = ctx
            .db
            .read({
                let id = id.clone();
                move |conn| {
                    Ok((
                        store::task_href(conn, &id)?,
                        store::task_uid_is_derived(conn, &id)?,
                    ))
                }
            })
            .await
            .expect("identity");
        assert_eq!(href.as_deref(), Some("https://dav/tasks/new.ics"));
        assert!(!uid_is_derived, "the newer identity kind must survive");
    }

    /// Regression ③: a successful move removes the task from its source list,
    /// so a delta for the source list captured before the move must not
    /// re-import it there. The task stays in the target list.
    #[tokio::test]
    async fn move_success_suppresses_source_list_reenimport() {
        let (ctx, service) = service().await;
        let source = ctx
            .db
            .read(|conn| task_lists::get_required(conn, "local-tasks"))
            .await
            .expect("source list");
        let target = service
            .tasklist_create("local", "Work", "#00AA00")
            .await
            .expect("target list");
        let href = "https://dav/tasks/moved.ics";
        apply(
            &ctx,
            &source,
            TaskDelta {
                tasks: vec![remote_task(
                    &source,
                    "Moved task",
                    "T1",
                    Some(href),
                    Some("\"e1\""),
                )],
                ..Default::default()
            },
        )
        .await;
        let task = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .expect("source query")
            .remove(0);
        let snapshot = ctx
            .db
            .read({
                let id = task.id.clone();
                move |conn| snapshot_task(conn, &id)
            })
            .await
            .expect("snapshot")
            .expect("task row");

        // Simulate a provider-side move that already succeeded.
        ctx.db
            .write({
                let id = task.id.clone();
                let target_id = target.id.clone();
                let source_id = source.id.clone();
                let snapshot = snapshot.clone();
                move |conn| {
                    tasks_repo::move_task(conn, &id, &target_id)?;
                    record_task_move_tombstone(
                        conn,
                        "local",
                        &source_id,
                        &target_id,
                        snapshot.remote_id.as_deref(),
                        snapshot.href.as_deref(),
                        snapshot.uid_is_derived,
                    )
                }
            })
            .await
            .expect("move and tombstone");

        // A stale delta for the source list still names the task.
        apply(
            &ctx,
            &source,
            TaskDelta {
                tasks: vec![remote_task(
                    &source,
                    "Moved task",
                    "T1",
                    Some(href),
                    Some("\"e1\""),
                )],
                ..Default::default()
            },
        )
        .await;

        let source_tasks = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .expect("source query");
        assert!(
            source_tasks.is_empty(),
            "the source list must not re-import the moved task"
        );
        let target_tasks = service
            .task_query(TaskQuery::list(&target.id, "2026-11-12"))
            .await
            .expect("target query");
        assert_eq!(
            target_tasks.len(),
            1,
            "the task must stay in the target list"
        );
        assert_eq!(target_tasks[0].id, task.id);
    }

    /// Audit fixture: a DAV PUT is captured and held so a stale delta can be
    /// applied while the worker is still on the wire.
    #[allow(clippy::type_complexity)]
    async fn audit_gated_dav_put_capture_fixture() -> (
        CoreContext,
        TasksService,
        TaskList,
        String,
        tokio::sync::oneshot::Receiver<String>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/tasks/", listener.local_addr().unwrap());
        let (request_tx, request_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&chunk[..n]);
                if let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..header_end]);
                    let length: usize = header
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= header_end + 4 + length {
                        break;
                    }
                }
            }
            request_tx
                .send(String::from_utf8(request).unwrap())
                .expect("capture request");
            crate::test_support::wait_for_write("audit-task-receipt").await;
            socket
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nETag: \"e2\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let (ctx, _) = service().await;
        let dav_url = base.clone();
        ctx.db
            .write(move |db| {
                db.execute(
                    "UPDATE account SET provider = 'imap', caldav_url = ?1 WHERE id = 'local'",
                    rusqlite::params![dav_url],
                )?;
                store::set_task_list(
                    db,
                    &TaskListRemote {
                        task_list_id: "local-tasks".into(),
                        account_id: "local".into(),
                        provider: "caldav".into(),
                        remote_id: dav_url.clone(),
                        href: Some(dav_url),
                    },
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let service = TasksService::new(
            ctx.clone(),
            HttpClient::new(Arc::new(ReviewTaskCreds)).unwrap(),
        );
        let list = ctx
            .db
            .read(|db| task_lists::get_required(db, "local-tasks"))
            .await
            .unwrap();
        let href = format!("{base}A.ics");
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "Original",
                    "A",
                    Some(href.as_str()),
                    Some("\"e1\""),
                    true,
                )],
                ..Default::default()
            },
        )
        .await;
        (ctx, service, list, href, request_rx, server)
    }

    /// A stale delta captured before a UID-upgrade PUT must not discard the
    /// real UID/ETag the provider acknowledged.
    ///
    /// The delta writes the row with the external (delta) owner and the old
    /// derived identity. The receipt guard must treat that owner as "not a
    /// confirmation", keep the acknowledged identity, and let the subsequent
    /// delete tombstone name the real UID so the task cannot be re-imported.
    #[tokio::test]
    async fn audit_stale_delta_during_uid_upgrade_does_not_discard_the_real_receipt() {
        let (gate, entered) = crate::test_support::WriteGate::new();
        let _guard = crate::test_support::install("audit-task-receipt", gate.clone());
        let (ctx, service, list, href, request_rx, server) =
            audit_gated_dav_put_capture_fixture().await;
        let task = service
            .task_query(TaskQuery::list("local-tasks", "2026-11-12"))
            .await
            .unwrap()
            .remove(0);
        service
            .task_update(&task.id, input("Edited real UID"))
            .await
            .unwrap();
        let worker = tokio::spawn({
            let ctx = ctx.clone();
            let service = service.clone();
            async move { op_worker::process_pass(&ctx, Arc::new(service)).await }
        });
        let request = request_rx.await.unwrap();
        entered.await.unwrap();
        assert!(request.starts_with("PUT "));
        // The server still lists the pre-upgrade derived identity while the
        // PUT is in flight.
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "Original",
                    "A",
                    Some(&href),
                    Some("e1"),
                    true,
                )],
                ..Default::default()
            },
        )
        .await;
        gate.release();
        worker.await.unwrap().unwrap();
        server.await.unwrap();
        let (snapshot, ops) = ctx
            .db
            .read({
                let id = task.id.clone();
                move |db| {
                    Ok((
                        snapshot_task(db, &id)?.unwrap(),
                        repo::pending_ops::list(db, Some("local"))?,
                    ))
                }
            })
            .await
            .unwrap();
        let assigned = ops
            .into_iter()
            .find_map(|op| match ops::decode(&op).unwrap() {
                TaskOp::Save { assigned_uid, .. } => assigned_uid,
                _ => None,
            })
            .unwrap();
        assert!(
            request.contains(&assigned),
            "the provider actually acknowledged the newly assigned UID"
        );
        assert_eq!(
            (snapshot.remote_id.clone(), snapshot.uid_is_derived),
            (Some(assigned.clone()), false),
            "a stale pre-PUT delta must not discard the real UID receipt"
        );
        service.task_delete(&task.id).await.unwrap();
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "Edited real UID",
                    &assigned,
                    Some(&href),
                    Some("e2"),
                    false,
                )],
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            task_count(&service).await,
            0,
            "losing the UID receipt made the pending DELETE tombstone derived-only; \
             the acknowledged real object resurrected immediately"
        );
    }

    /// Moving A -> B -> A and then running a full sync must not prune the task
    /// (or its step mappings) because of the first move's source tombstone.
    #[tokio::test]
    async fn audit_move_back_full_sync_does_not_prune_the_returned_task() {
        let (ctx, service) = service().await;
        let source = ctx
            .db
            .read(|db| task_lists::get_required(db, "local-tasks"))
            .await
            .unwrap();
        let target = service.tasklist_create("local", "Other", "#00AA00").await.unwrap();
        apply(
            &ctx,
            &source,
            TaskDelta {
                tasks: vec![remote_task(&source, "Google same identity", "T1", None, None)],
                ..Default::default()
            },
        )
        .await;
        let task = service
            .task_query(TaskQuery::list(&source.id, "2026-11-12"))
            .await
            .unwrap()
            .remove(0);
        // A provider-side step that must survive the full sync.
        let step_id = new_id();
        ctx.db
            .write({
                let id = task.id.clone();
                let step_id = step_id.clone();
                move |db| {
                    let mut task = tasks_repo::get_required(db, &id)?;
                    task.steps.push(TaskStep {
                        id: step_id.clone(),
                        title: "Step".into(),
                        is_completed: false,
                        sort_order: 0,
                    });
                    let input = input_from_task(&task);
                    tasks_repo::update(db, &id, &input)?;
                    store::set_step_mapping(db, &step_id, &id, "S1")?;
                    Ok(())
                }
            })
            .await
            .unwrap();
        // Two successful moves. Google keeps the provider id, so the same
        // identity is now back in the source list while the source still
        // carries a settled move tombstone for it.
        ctx.db
            .write({
                let id = task.id.clone();
                let src = source.id.clone();
                let dst = target.id.clone();
                move |db| {
                    tasks_repo::move_task(db, &id, &dst)?;
                    record_task_move_tombstone(db, "local", &src, &dst, Some("T1"), None, false)?;
                    tasks_repo::move_task(db, &id, &src)?;
                    record_task_move_tombstone(db, "local", &dst, &src, Some("T1"), None, false)?;
                    Ok(())
                }
            })
            .await
            .unwrap();
        apply(
            &ctx,
            &source,
            TaskDelta {
                tasks: vec![remote_task(&source, "Google same identity", "T1", None, None)],
                full_sync: true,
                ..Default::default()
            },
        )
        .await;
        assert!(
            service.task_get(&task.id).await.is_ok(),
            "the old source tombstone must not make a full sync delete a task moved back to that list"
        );
        let mappings = ctx
            .db
            .read({
                let id = task.id.clone();
                move |db| store::step_mappings_for_task(db, &id)
            })
            .await
            .unwrap();
        assert!(
            mappings
                .iter()
                .any(|(local, remote)| local == &step_id && remote == "S1"),
            "a suppressed-but-present task must keep its step mappings"
        );
    }

    /// Returning to a list clears only the stale "left here" marker; a
    /// pending DELETE tombstone is still owed to the provider and must stay.
    #[tokio::test]
    async fn move_back_clears_only_the_stale_move_tombstone() {
        let (ctx, service) = service().await;
        let target = service
            .tasklist_create("local", "Return target", "#00AA00")
            .await
            .unwrap();
        let removed = ctx
            .db
            .write({
                let target_id = target.id.clone();
                move |db| {
                    store::insert_settled_task_move_tombstone(
                        db,
                        "local",
                        &target_id,
                        Some("T-move"),
                        None,
                        false,
                        Utc::now()
                            + chrono::Duration::seconds(
                                store::TASK_DELETE_TOMBSTONE_TTL_SECONDS,
                            ),
                    )?;
                    // A user delete whose remote cleanup has not settled.
                    store::insert_task_delete_tombstone(
                        db,
                        "local",
                        &target_id,
                        Some("T-delete"),
                        None,
                        false,
                    )?;
                    store::remove_task_move_tombstones(
                        db,
                        &target_id,
                        Some("T-move"),
                        None,
                        false,
                    )
                }
            })
            .await
            .unwrap();
        assert_eq!(removed, 1, "the stale move tombstone must be dropped");
        let remaining = ctx
            .db
            .read({
                let target_id = target.id.clone();
                move |db| store::active_task_delete_tombstones(db, &target_id, Utc::now())
            })
            .await
            .unwrap();
        assert_eq!(
            remaining.len(),
            1,
            "a pending DELETE tombstone must not be cleared"
        );
        assert_eq!(remaining[0].remote_id.as_deref(), Some("T-delete"));
    }

    /// Regression: housekeeping must not let a reused queue sequence make an
    /// older confirmation look newer than the save that is on the wire.
    ///
    /// A real enqueue records a receipt with sequence 2. The production
    /// `prune_finished` then removes the settled queue, and a new Save used to
    /// get `MAX(seq) + 1 = 1`. A stale UID-less delta rewrote the row writer,
    /// and `save_receipt_is_superseded` then compared the persisted receipt
    /// (2) with the new operation (1): the *past* confirmation looked like a
    /// *future* one, the real UID/ETag the PUT acknowledged was dropped, and
    /// the subsequent delete suppressed only the derived placeholder. The real
    /// object then resurrected the task. Task operations now allocate from the
    /// durable `pim_write_seq` high-water mark, so the new Save is ordered
    /// after the receipt.
    #[tokio::test]
    async fn save_receipt_survives_sequence_reuse_after_prune() {
        let (gate, entered) = crate::test_support::WriteGate::new();
        let _guard = crate::test_support::install("audit-task-receipt", gate.clone());
        let (ctx, service, list, href, request_rx, server) =
            audit_gated_dav_put_capture_fixture().await;
        let task = service
            .task_query(TaskQuery::list(&list.id, "2026-11-12"))
            .await
            .unwrap()
            .remove(0);
        service
            .task_update(&task.id, input("First queued edit"))
            .await
            .unwrap();
        service
            .task_update(&task.id, input("Confirmed edit before housekeeping"))
            .await
            .unwrap();
        let prior = ctx
            .db
            .write({
                let id = task.id.clone();
                let href = href.clone();
                move |db| {
                    let ops = repo::pending_ops::list(db, Some("local"))?;
                    let latest = ops.iter().max_by_key(|op| op.sequence).unwrap();
                    let base = snapshot_task(db, &id)?.unwrap();
                    record_task_save_receipt(
                        db,
                        "local",
                        &latest.id,
                        &base,
                        &TaskSaveReceipt {
                            remote_id: Some("confirmed-real-uid".into()),
                            href: Some(href),
                            etag: Some("e1".into()),
                            uid_is_derived: false,
                        },
                    )?;
                    for op in &ops {
                        repo::pending_ops::mark_done(db, &op.id)?;
                    }
                    db.execute(
                        "UPDATE pending_op SET updated_at='2000-01-01T00:00:00Z'",
                        [],
                    )?;
                    assert_eq!(repo::pending_ops::prune_finished(db, Utc::now())?, 2);
                    Ok(store::task_confirmed_receipt(db, &id)?.unwrap())
                }
            })
            .await
            .unwrap();
        // After housekeeping, another DAV client rewrites this resource
        // without a UID. This is an ordinary delta: it changes the baseline
        // before the next local edit.
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "External UID-less rewrite",
                    "A",
                    Some(&href),
                    Some("e1"),
                    true,
                )],
                ..Default::default()
            },
        )
        .await;
        service
            .task_update(&task.id, input("Latest user edit"))
            .await
            .unwrap();
        let next = ctx
            .db
            .read(|db| Ok(repo::pending_ops::list(db, Some("local"))?.remove(0)))
            .await
            .unwrap();
        assert!(
            next.sequence > prior.op_sequence.unwrap(),
            "the durable task write sequence must not rewind after housekeeping"
        );
        let worker = tokio::spawn({
            let ctx = ctx.clone();
            let service = service.clone();
            async move { op_worker::process_pass(&ctx, Arc::new(service)).await }
        });
        let request = request_rx.await.unwrap();
        entered.await.unwrap();
        let new_uid = ctx
            .db
            .read({
                let op_id = next.id.clone();
                move |db| {
                    let queued = repo::pending_ops::get_required(db, &op_id)?;
                    Ok(match ops::decode(&queued)? {
                        TaskOp::Save { assigned_uid, .. } => assigned_uid.unwrap(),
                        _ => unreachable!(),
                    })
                }
            })
            .await
            .unwrap();
        assert!(
            request.contains(&format!("UID:{new_uid}")),
            "the real PUT sent a new UID from the current derived baseline"
        );
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task_with_kind(
                    &list,
                    "Old UID-less listing",
                    "A",
                    Some(&href),
                    Some("e1"),
                    true,
                )],
                ..Default::default()
            },
        )
        .await;
        gate.release();
        worker.await.unwrap().unwrap();
        server.await.unwrap();
        let after = ctx
            .db
            .read({
                let id = task.id.clone();
                move |db| Ok(snapshot_task(db, &id)?.unwrap())
            })
            .await
            .unwrap();
        assert_eq!(
            (after.remote_id.clone(), after.uid_is_derived),
            (Some(new_uid.clone()), false),
            "the receipt of the new PUT must not be dropped"
        );
        service.task_delete(&task.id).await.unwrap();
        apply(
            &ctx,
            &list,
            TaskDelta {
                tasks: vec![remote_task(
                    &list,
                    "Latest user edit",
                    &new_uid,
                    Some(&href),
                    Some("e2"),
                )],
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            task_count(&service).await,
            0,
            "losing the real UID receipt made the pending DELETE tombstone derived-only; \
             the acknowledged real object resurrected immediately"
        );
    }
}
