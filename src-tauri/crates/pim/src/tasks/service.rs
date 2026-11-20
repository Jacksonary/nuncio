//! Task sync engine, command facade and pending-operation executor.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use nuncio_core::events::{emit_mail_changed, emit_tasks_changed, MailChangeKind};
use nuncio_core::models::{
    new_id, Account, Importance, NewPendingOp, PendingOp, SmartListCounts, SyncModule, Task,
    TaskInput, TaskList, TaskQuery, TaskStatus, TaskStep,
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
use crate::tasks::graph::GraphTasksProvider;
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
            .write(move |conn| {
                apply_delta(conn, &account_id, &list_clone, caps, &delta)?;
                save_cursor(
                    conn,
                    &list_clone.id,
                    delta.sync_token.as_deref(),
                    delta.ctag.as_deref(),
                )
            })
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
                    if messages_repo::get(conn, &message_id)?.is_some() {
                        existing.insert(message_id);
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
        let (task, account_id) = self
            .ctx
            .db
            .write({
                let input = input.clone();
                move |conn| {
                    let list = task_lists::get_required(conn, &list_id)?;
                    if list.is_read_only {
                        return Err(invalid("task list is read-only"));
                    }
                    // New tasks are prepended (sort_order = MIN - 1).
                    let task = tasks_repo::create(conn, &list_id, &input)?;
                    let task = tasks_repo::get_required(conn, &task.id)?;
                    Ok((task, list.account_id))
                }
            })
            .await?;
        let account = self.load_account(&account_id).await?;
        if self.is_provider(&account) {
            let op = TaskOp::Save {
                task_id: task.id.clone(),
                snapshot: None,
            };
            self.enqueue_op(&account.id, &op, format!("创建任务「{}」", task.title))
                .await?;
        }
        emit_tasks_changed(self.ctx.events.as_ref(), vec![task.list_id.clone()]);
        Ok(task)
    }

    pub async fn task_update(&self, id: &str, input: TaskInput) -> Result<Task> {
        if input.title.trim().is_empty() {
            return Err(invalid("task title must not be empty"));
        }
        let id = id.to_string();
        let (snapshot, task) = self
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
                    let task = tasks_repo::update(conn, &id, &input)?;
                    Ok((snapshot, task))
                }
            })
            .await?;
        let account = self.load_account(&task.account_id).await?;
        if self.is_provider(&account) {
            let op = TaskOp::Save {
                task_id: task.id.clone(),
                snapshot: Some(snapshot),
            };
            self.enqueue_op(&account.id, &op, format!("更新任务「{}」", task.title))
                .await?;
        }
        emit_tasks_changed(self.ctx.events.as_ref(), vec![task.list_id.clone()]);
        Ok(task)
    }

    pub async fn task_set_completed(&self, id: &str, value: bool) -> Result<Task> {
        let id = id.to_string();
        let current = self.task_get(&id).await?;
        let account = self.load_account(&current.account_id).await?;
        let provider = self.provider_for(&account);
        let server_recurs = provider
            .as_ref()
            .map(|provider| provider.capabilities().server_recurring_completion)
            .unwrap_or(false);
        let today_date = today();
        let (snapshot, task, next_task) = self
            .ctx
            .db
            .write(move |conn| {
                let current = tasks_repo::get_required(conn, &id)?;
                let was_completed = current.status == TaskStatus::Completed;
                let snapshot = snapshot_task(conn, &id)?
                    .ok_or_else(|| AppError::not_found("task not found"))?;
                let mut updated = tasks_repo::set_completed(conn, &id, value)?;
                let next =
                    if value && !was_completed && !server_recurs && current.recurrence.is_some() {
                        let next = next_occurrence(&current, &today_date)
                            .map(|input| tasks_repo::create(conn, &current.list_id, &input))
                            .transpose()?;
                        if next.is_some() {
                            // The completed row is the old occurrence, not a
                            // running series; the new task carries the rule.
                            let mut cleared = input_from_task(&updated);
                            cleared.recurrence = None;
                            updated = tasks_repo::update(conn, &id, &cleared)?;
                        }
                        next
                    } else {
                        None
                    };
                Ok((snapshot, updated, next))
            })
            .await?;
        if self.is_provider(&account) {
            let op = TaskOp::Save {
                task_id: task.id.clone(),
                snapshot: Some(snapshot),
            };
            self.enqueue_op(&account.id, &op, format!("完成任务「{}」", task.title))
                .await?;
            if let Some(next) = &next_task {
                let op = TaskOp::Save {
                    task_id: next.id.clone(),
                    snapshot: None,
                };
                self.enqueue_op(
                    &account.id,
                    &op,
                    format!("创建下一期任务「{}」", next.title),
                )
                .await?;
            }
        }
        emit_tasks_changed(self.ctx.events.as_ref(), vec![task.list_id.clone()]);
        self.notify();
        Ok(task)
    }

    pub async fn task_set_importance(&self, id: &str, importance: Importance) -> Result<Task> {
        let id = id.to_string();
        let (snapshot, task) = self
            .ctx
            .db
            .write(move |conn| {
                let snapshot = snapshot_task(conn, &id)?
                    .ok_or_else(|| AppError::not_found("task not found"))?;
                let task = tasks_repo::set_importance(conn, &id, importance)?;
                Ok((snapshot, task))
            })
            .await?;
        self.enqueue_save_for_task(&task, snapshot).await?;
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
        let (snapshot, task, from_list) = self
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
                let task = tasks_repo::move_task(conn, &id, &target)?;
                Ok((snapshot, task, from_list))
            })
            .await?;
        let account = self.load_account(&task.account_id).await?;
        if self.is_provider(&account) {
            let op = TaskOp::Move {
                task_id: task.id.clone(),
                from_list_id: from_list.clone(),
                snapshot,
            };
            self.enqueue_op(&account.id, &op, format!("移动任务「{}」", task.title))
                .await?;
        }
        emit_tasks_changed(
            self.ctx.events.as_ref(),
            vec![from_list, task.list_id.clone()],
        );
        Ok(task)
    }

    pub async fn task_reorder(&self, list_id: &str, ids: Vec<String>) -> Result<()> {
        let list_id = list_id.to_string();
        let id_for_db = list_id.clone();
        let ids_for_db = ids.clone();
        self.ctx
            .db
            .write(move |conn| tasks_repo::reorder(conn, &id_for_db, &ids_for_db))
            .await?;
        // Push the order to providers that expose one.  Google's `tasks.move`
        // is the only backend with a server-side position; Graph/CalDAV keep
        // the local order (see `TaskCapabilities::ordering`).
        let list = self
            .ctx
            .db
            .read({
                let id = list_id.clone();
                move |conn| task_lists::get_required(conn, &id)
            })
            .await?;
        let account = self.load_account(&list.account_id).await?;
        if self.is_provider(&account) {
            if let Some(provider) = self.provider_for(&account) {
                if provider.capabilities().ordering {
                    let op = TaskOp::Reorder {
                        list_id: list_id.clone(),
                        ids,
                    };
                    self.enqueue_op(&account.id, &op, "调整任务顺序".to_string())
                        .await?;
                }
            }
        }
        emit_tasks_changed(self.ctx.events.as_ref(), vec![list_id]);
        Ok(())
    }

    pub async fn task_delete(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        let snapshot = self
            .ctx
            .db
            .write(move |conn| {
                let snapshot = snapshot_task(conn, &id)?
                    .ok_or_else(|| AppError::not_found("task not found"))?;
                tasks_repo::delete(conn, &id)?;
                Ok(snapshot)
            })
            .await?;
        let account = self.load_account(&snapshot.task.account_id).await?;
        if self.is_provider(&account) && snapshot.remote_id.is_some() {
            let op = TaskOp::Delete {
                snapshot: snapshot.clone(),
            };
            self.enqueue_op(
                &account.id,
                &op,
                format!("删除任务「{}」", snapshot.task.title),
            )
            .await?;
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
    pub async fn message_create_task(&self, message_id: &str) -> Result<Task> {
        let id = message_id.to_string();
        let message = self
            .ctx
            .db
            .read(move |conn| messages_repo::get_required(conn, &id))
            .await?;
        let summary = message.summary.clone();
        let title = if summary.subject.trim().is_empty() {
            "(无主题)".to_string()
        } else {
            summary.subject.clone()
        };
        let sender = summary
            .r#from
            .as_ref()
            .map(|address| address.to_string())
            .unwrap_or_default();
        let notes = format!(
            "发件人：{}\n日期：{}",
            sender,
            summary.date.format("%Y-%m-%d %H:%M")
        );
        let account_id = summary.account_id.clone();
        let list_id = self.default_list_for(&account_id).await?;
        let input = TaskInput {
            title,
            notes,
            importance: Importance::Normal,
            due_date: Some(today()),
            reminder_at: None,
            recurrence: None,
            my_day_date: Some(today()),
            steps: Vec::new(),
        };
        let task = self.task_create(&list_id, input).await?;
        let task_id = task.id.clone();
        let message_for_link = summary.id.clone();
        self.ctx
            .db
            .write(move |conn| {
                tasks_repo::set_linked_message(conn, &task_id, Some(&message_for_link))
            })
            .await?;
        // Flag the source message locally and through the mail op queue.
        if !summary.flagged {
            let message_ids = vec![summary.id.clone()];
            let folder_id = summary.folder_id.clone();
            let db_id = summary.id.clone();
            self.ctx
                .db
                .write(move |conn| {
                    messages_repo::set_flags(conn, &[db_id], None, Some(true), None, None)
                })
                .await?;
            let account = self.load_account(&account_id).await?;
            if !account.provider.is_local() {
                if let Some(uid) = message.uid {
                    let payload = serde_json::json!({
                        "folderId": folder_id,
                        "uids": [uid],
                        "seen": serde_json::Value::Null,
                        "flagged": true,
                        "answered": serde_json::Value::Null,
                        "forwarded": serde_json::Value::Null,
                    });
                    let op =
                        NewPendingOp::new(&account.id, "setFlags", payload, "标记邮件".to_string());
                    self.ctx
                        .db
                        .write(move |conn| repo::pending_ops::enqueue(conn, op))
                        .await?;
                }
            }
            emit_mail_changed(
                self.ctx.events.as_ref(),
                account_id.clone(),
                vec![folder_id],
                message_ids,
                MailChangeKind::Updated,
            );
        }
        let task = self.task_get(&task.id).await?;
        emit_tasks_changed(self.ctx.events.as_ref(), vec![task.list_id.clone()]);
        Ok(task)
    }

    /* ───────────────────────── helpers ───────────────────────────── */

    async fn enqueue_save_for_task(&self, task: &Task, snapshot: TaskSnapshot) -> Result<()> {
        let account = self.load_account(&task.account_id).await?;
        if self.is_provider(&account) {
            let op = TaskOp::Save {
                task_id: task.id.clone(),
                snapshot: Some(snapshot),
            };
            self.enqueue_op(&account.id, &op, format!("更新任务「{}」", task.title))
                .await?;
        }
        Ok(())
    }

    async fn default_list_for(&self, account_id: &str) -> Result<String> {
        let account_id = account_id.to_string();
        let list = self
            .ctx
            .db
            .read(move |conn| {
                if let Some(list) = task_lists::default_for_account(conn, &account_id)? {
                    return Ok(Some(list));
                }
                // Fall back to the built-in local list.
                task_lists::default_for_account(conn, nuncio_core::models::LOCAL_ACCOUNT_ID)
            })
            .await?;
        list.map(|list| list.id)
            .ok_or_else(|| AppError::not_found("no task list available"))
    }

    async fn enqueue_op(&self, account_id: &str, op: &TaskOp, description: String) -> Result<()> {
        let payload = ops::to_value(op)?;
        let new_op = NewPendingOp::new(account_id, op.kind(), payload, description);
        self.ctx
            .db
            .write(move |conn| repo::pending_ops::enqueue(conn, new_op))
            .await?;
        self.notify.notify_one();
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
        }
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
        };
        let current: HashSet<String> = task.steps.iter().map(|step| step.id.clone()).collect();
        for step in &task.steps {
            match mappings.iter().find(|(step_id, _)| step_id == &step.id) {
                Some((_, remote_step)) => {
                    provider
                        .update_step(account, list, &parent, remote_step, step)
                        .await?;
                }
                None => {
                    if let Some(result) = provider.create_step(account, list, &parent, step).await?
                    {
                        if let Some(remote_step) = result.remote_id {
                            let step_id = step.id.clone();
                            let task_id = task.id.clone();
                            self.ctx
                                .db
                                .write(move |conn| {
                                    store::set_step_mapping(conn, &step_id, &task_id, &remote_step)
                                })
                                .await?;
                        }
                    }
                }
            }
        }
        for (step_id, remote_step) in mappings {
            if current.contains(&step_id) {
                continue;
            }
            provider
                .delete_step(account, list, &parent, &remote_step)
                .await?;
            let step_id_clone = step_id.clone();
            self.ctx
                .db
                .write(move |conn| store::delete_step_mapping(conn, &step_id_clone))
                .await?;
        }
        Ok(())
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
            TaskOp::Save { task_id, .. } => {
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
                };
                let created = current.remote_id.is_none() && current.href.is_none();
                let result = if !created {
                    provider
                        .update_task(&account, &list, &current, &task)
                        .await?
                } else {
                    provider.create_task(&account, &list, &task).await?
                };
                let id = task.id.clone();
                let mapping = self
                    .ctx
                    .db
                    .write({
                        let remote_id = result.remote_id.clone();
                        let etag = result.etag.clone();
                        let href = result.href.clone();
                        move |conn| {
                            conn.execute(
                                "UPDATE task SET remote_id = COALESCE(?2, remote_id),
                                                 etag = COALESCE(?3, etag),
                                                 updated_at = ?4
                                 WHERE id = ?1",
                                rusqlite::params![id, remote_id, etag, repo::now_string()],
                            )?;
                            if let Some(href) = &href {
                                store::set_task_href(conn, &id, href)?;
                            }
                            Ok(())
                        }
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
                };
                provider.delete_task(&account, &list, &current).await?;
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
                self.ctx
                    .db
                    .write({
                        let remote_id = result.remote_id.clone();
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
                            if let Some(href) = &href {
                                store::set_task_href(conn, &id, href)?;
                            }
                            if clear_steps {
                                store::clear_step_mappings(conn, &id)?;
                            }
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
                provider
                    .update_task_list(&account, &remote, Some(&list.name), Some(&list.color))
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
        let decoded = ops::decode(op)?;
        // Rolling back mutates exactly the rows the UI is showing, and the
        // failure path already told the user something went wrong. Without a
        // changed event the list keeps rendering the rolled-back values until
        // the next sync, so the user sees an error toast over state that no
        // longer matches the database.
        let restore = match decoded {
            TaskOp::Save {
                task_id,
                snapshot,
            } => match snapshot {
                Some(snapshot) => {
                    self.ctx
                        .db
                        .write(move |conn| restore_task(conn, &snapshot))
                        .await
                }
                None => {
                    self.ctx
                        .db
                        .write(move |conn| {
                            let _ = tasks_repo::delete(conn, &task_id);
                            Ok(())
                        })
                        .await
                }
            },
            TaskOp::Delete { snapshot } => {
                self.ctx
                    .db
                    .write(move |conn| restore_task(conn, &snapshot))
                    .await
            }
            TaskOp::Move {
                task_id,
                from_list_id,
                snapshot,
            } => {
                self.ctx
                    .db
                    .write(move |conn| {
                        let _ = tasks_repo::move_task(conn, &task_id, &from_list_id);
                        restore_task(conn, &snapshot)
                    })
                    .await
            }
            TaskOp::Reorder { .. } => Ok(()),
            TaskOp::ListCreate { list_id } => {
                self.ctx
                    .db
                    .write(move |conn| {
                        let _ = task_lists::delete(conn, &list_id);
                        Ok(())
                    })
                    .await
            }
            TaskOp::ListUpdate {
                list_id,
                previous_name,
                previous_color,
            } => {
                self.ctx
                    .db
                    .write(move |conn| {
                        if task_lists::get(conn, &list_id)?.is_some() {
                            task_lists::update(
                                conn,
                                &list_id,
                                Some(&previous_name),
                                Some(&previous_color),
                            )?;
                        }
                        Ok(())
                    })
                    .await
            }
            TaskOp::ListDelete { snapshot } => {
                self.ctx
                    .db
                    .write(move |conn| {
                        conn.execute(
                            "INSERT OR IGNORE INTO task_list
                                 (id, account_id, name, color, is_default, is_read_only, sort_order, created_at, updated_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
                            rusqlite::params![
                                snapshot.list.id,
                                snapshot.list.account_id,
                                snapshot.list.name,
                                snapshot.list.color,
                                snapshot.list.is_default as i64,
                                snapshot.list.is_read_only as i64,
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
                            restore_task(conn, task)?;
                        }
                        Ok(())
                    })
                    .await
            }
        };
        if restore.is_ok() {
            // An empty list means "anything may have changed"; the frontend
            // invalidates every task query for it.
            emit_tasks_changed(self.ctx.events.as_ref(), Vec::new());
        }
        restore
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
    Ok(list)
}

fn apply_delta(
    conn: &rusqlite::Connection,
    account_id: &str,
    list: &TaskList,
    caps: TaskCapabilities,
    delta: &TaskDelta,
) -> Result<()> {
    // Deletions: task rows first, then child steps (Google subtasks).
    for remote_id in &delta.deleted_remote_ids {
        if let Some(task) = tasks_repo::by_remote_id(conn, &list.id, remote_id)? {
            tasks_repo::delete(conn, &task.id)?;
            continue;
        }
        if let Some((step_id, task_id, _)) = store::step_lookup_by_remote(conn, remote_id)? {
            if let Some(mut parent) = tasks_repo::get(conn, &task_id)? {
                parent.steps.retain(|step| step.id != step_id);
                let input = input_from_task(&parent);
                tasks_repo::update(conn, &parent.id, &input)?;
                store::delete_step_mapping(conn, &step_id)?;
            }
        }
    }
    for href in &delta.deleted_hrefs {
        if let Some(task_id) = store::task_by_href(conn, &list.id, href)? {
            let _ = tasks_repo::delete(conn, &task_id);
        }
    }

    let mut seen_remote: HashSet<String> = HashSet::new();
    let mut seen_steps: HashSet<String> = HashSet::new();
    let mut incomplete_steps: HashSet<String> = HashSet::new();
    let mut local_by_remote: HashMap<String, String> = HashMap::new();
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
        seen_remote.insert(remote_id.clone());
        let existing = find_task(conn, list, remote)?;
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
            if caps.steps && !remote.steps_complete {
                task.steps = existing.steps.clone();
            }
        }
        let mut pending_step_mappings: Vec<(String, String)> = Vec::new();
        if caps.steps && remote.steps_complete {
            let has_mappings = !store::step_mappings_for_task(conn, &id)?.is_empty();
            if remote.task.steps.is_empty() && has_mappings {
                // A delta response can omit unchanged subtasks.  Never drop
                // local steps (and their mappings) in that case: doing so
                // would make the next reconcile delete them remotely.
                task.steps = existing
                    .as_ref()
                    .map(|task| task.steps.clone())
                    .unwrap_or_default();
            } else {
                for step in &remote.task.steps {
                    if !step.id.trim().is_empty() {
                        seen_steps.insert(step.id.clone());
                    }
                }
                task.steps = map_steps(conn, &id, &task.steps, &mut pending_step_mappings)?;
            }
        } else if !caps.steps {
            task.steps = existing
                .as_ref()
                .map(|task| task.steps.clone())
                .unwrap_or_default();
        }
        let saved =
            tasks_repo::upsert_remote(conn, &task, remote.etag.as_deref(), Some(&remote_id))?;
        for (local_step, remote_step) in pending_step_mappings {
            store::set_step_mapping(conn, &local_step, &saved.id, &remote_step)?;
        }
        if let Some(href) = &remote.href {
            store::set_task_href(conn, &saved.id, href)?;
        }
        if caps.steps && !remote.steps_complete {
            incomplete_steps.insert(saved.id.clone());
        }
        local_by_remote.insert(remote_id, saved.id);
    }

    for remote in children {
        let Some(remote_id) = remote.remote_id.clone() else {
            continue;
        };
        let Some(parent_remote) = remote.parent_remote_id.clone() else {
            continue;
        };
        let parent_id = local_by_remote.get(&parent_remote).cloned().or_else(|| {
            tasks_repo::by_remote_id(conn, &list.id, &parent_remote)
                .ok()
                .flatten()
                .map(|task| task.id)
        });
        match parent_id {
            Some(parent_id) => {
                seen_remote.insert(remote_id.clone());
                seen_steps.insert(remote_id.clone());
                let step_id =
                    store::step_local_id(conn, &parent_id, &remote_id)?.unwrap_or_else(new_id);
                store::set_step_mapping(conn, &step_id, &parent_id, &remote_id)?;
                let mut parent = tasks_repo::get_required(conn, &parent_id)?;
                let step = TaskStep {
                    id: step_id.clone(),
                    title: remote.task.title.clone(),
                    is_completed: remote.task.status == TaskStatus::Completed,
                    sort_order: remote.task.sort_order,
                };
                parent.steps.retain(|existing| existing.id != step_id);
                parent.steps.push(step);
                let meta = tasks_repo::sync_meta(conn, &parent_id)?;
                tasks_repo::upsert_remote(
                    conn,
                    &parent,
                    meta.as_ref().and_then(|meta| meta.etag.as_deref()),
                    meta.as_ref().and_then(|meta| meta.remote_id.as_deref()),
                )?;
            }
            None => {
                // Parent not synced yet: keep the child as a top-level task
                // rather than dropping data; a later full sync attaches it.
                let existing = find_task(conn, list, remote)?;
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
                local_by_remote.insert(remote_id.clone(), saved.id.clone());
                seen_remote.insert(remote_id);
            }
        }
    }

    if delta.full_sync {
        let mut stmt = conn.prepare("SELECT id, remote_id FROM task WHERE list_id = ?1")?;
        let mut rows = stmt.query(rusqlite::params![list.id])?;
        let mut stale = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let remote_id: Option<String> = row.get(1)?;
            if let Some(remote_id) = remote_id {
                if !seen_remote.contains(&remote_id) && !seen_steps.contains(&remote_id) {
                    stale.push(id);
                }
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
                if incomplete_steps.contains(&task.id) {
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
                for (step_id, _) in missing {
                    store::delete_step_mapping(conn, &step_id)?;
                }
            }
        }
    }
    Ok(())
}

/// Assign local ids to provider steps and remember the mapping.
fn map_steps(
    conn: &rusqlite::Connection,
    task_id: &str,
    steps: &[TaskStep],
    mappings: &mut Vec<(String, String)>,
) -> Result<Vec<TaskStep>> {
    let mut out = Vec::with_capacity(steps.len());
    for (index, step) in steps.iter().enumerate() {
        let remote_id = step.id.trim();
        if remote_id.is_empty() {
            out.push(TaskStep {
                id: new_id(),
                title: step.title.clone(),
                is_completed: step.is_completed,
                sort_order: step.sort_order,
            });
            continue;
        }
        let local_id = store::step_local_id(conn, task_id, remote_id)?.unwrap_or_else(new_id);
        // Mappings are written after the task row exists (FK); collecting them
        // here also covers brand-new tasks with embedded checklist items.
        mappings.push((local_id.clone(), remote_id.to_string()));
        out.push(TaskStep {
            id: local_id,
            title: step.title.clone(),
            is_completed: step.is_completed,
            sort_order: if step.sort_order == 0 {
                index as i64
            } else {
                step.sort_order
            },
        });
    }
    Ok(out)
}

fn find_task(
    conn: &rusqlite::Connection,
    list: &TaskList,
    remote: &RemoteTask,
) -> Result<Option<Task>> {
    if let Some(remote_id) = &remote.remote_id {
        if let Some(task) = tasks_repo::by_remote_id(conn, &list.id, remote_id)? {
            return Ok(Some(task));
        }
    }
    if let Some(href) = &remote.href {
        if let Some(id) = store::task_by_href(conn, &list.id, href)? {
            return tasks_repo::get(conn, &id);
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
    }))
}

fn restore_task(conn: &rusqlite::Connection, snapshot: &TaskSnapshot) -> Result<()> {
    tasks_repo::upsert_remote(
        conn,
        &snapshot.task,
        snapshot.etag.as_deref(),
        snapshot.remote_id.as_deref(),
    )?;
    if let Some(href) = &snapshot.href {
        store::set_task_href(conn, &snapshot.task.id, href)?;
    }
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
                },
                RemoteTask {
                    task: child,
                    remote_id: Some("c1".into()),
                    href: None,
                    etag: None,
                    parent_remote_id: Some("p1".into()),
                    steps_complete: true,
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
}
