//! Google Tasks API provider.
//!
//! Google Tasks has no sync token: incremental sync uses `updatedMin` +
//! `showDeleted` + `showHidden`, which the contract in `docs/PLAN.md` §6.2
//! prescribes.  Importance, reminders, recurrence and My Day are local-only
//! (reported through [`TaskCapabilities`]) and preserved by the sync engine.
//! Subtasks are mapped onto the parent task's steps.

use async_trait::async_trait;
use chrono::Utc;
use nuncio_core::models::{Account, Task, TaskStatus, TaskStep};
use nuncio_core::traits::TokenResource;
use serde::Deserialize;
use tracing::debug;

use crate::error::{invalid, AppError, Result};
use crate::http::HttpClient;
use crate::tasks::map::{date_part, parse_naive_utc, position_to_order};
use crate::tasks::provider::{
    RemoteTask, RemoteTaskList, RemoteTaskRef, RemoteWriteResult, TaskCapabilities, TaskCursor,
    TaskDelta, TaskProvider,
};

pub const TASKS_BASE: &str = "https://tasks.googleapis.com/tasks/v1";

fn encode(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

/// Low-level Google Tasks client.
#[derive(Clone)]
pub struct GoogleTasksClient {
    http: HttpClient,
    base: String,
}

impl std::fmt::Debug for GoogleTasksClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleTasksClient")
            .field("base", &self.base)
            .finish()
    }
}

impl GoogleTasksClient {
    pub fn new(http: HttpClient) -> Self {
        Self {
            http,
            base: TASKS_BASE.to_string(),
        }
    }

    pub fn with_base(http: HttpClient, base: impl Into<String>) -> Self {
        Self {
            http,
            base: base.into().trim_end_matches('/').to_string(),
        }
    }

    pub fn http(&self) -> &HttpClient {
        &self.http
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    async fn failure(response: reqwest::Response) -> AppError {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        crate::http::status_error(status, &body)
    }

    pub async fn list_task_lists(&self, account_id: &str) -> Result<Vec<GTaskList>> {
        let mut items = Vec::new();
        let mut page_token: Option<String> = None;
        let mut guard = 0;
        loop {
            guard += 1;
            if guard > 50 {
                break;
            }
            let mut query: Vec<(String, String)> = vec![("maxResults".into(), "1000".into())];
            if let Some(token) = &page_token {
                query.push(("pageToken".into(), token.clone()));
            }
            let url = self.url("/users/@me/lists");
            let response = self
                .http
                .send_authed(account_id, TokenResource::GoogleApis, |token| {
                    self.http.raw().get(&url).bearer_auth(token).query(&query)
                })
                .await?;
            if !response.status().is_success() {
                return Err(Self::failure(response).await);
            }
            let page: GTaskListPage = response
                .json()
                .await
                .map_err(|error| invalid(format!("invalid Google tasklists JSON: {error}")))?;
            items.extend(page.items);
            match page.next_page_token {
                Some(token) if !token.is_empty() => page_token = Some(token),
                _ => break,
            }
        }
        Ok(items)
    }

    pub async fn create_task_list(&self, account_id: &str, title: &str) -> Result<GTaskList> {
        let url = self.url("/users/@me/lists");
        let body = serde_json::json!({ "title": title });
        let response = self
            .http
            .send_authed(account_id, TokenResource::GoogleApis, |token| {
                self.http.raw().post(&url).bearer_auth(token).json(&body)
            })
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid Google tasklist JSON: {error}")))
    }

    pub async fn patch_task_list(
        &self,
        account_id: &str,
        list_id: &str,
        title: &str,
    ) -> Result<()> {
        let url = self.url(&format!("/users/@me/lists/{}", encode(list_id)));
        let body = serde_json::json!({ "title": title });
        let response = self
            .http
            .send_authed(account_id, TokenResource::GoogleApis, |token| {
                self.http.raw().patch(&url).bearer_auth(token).json(&body)
            })
            .await?;
        HttpClient::ensure_success(response).await
    }

    pub async fn delete_task_list(&self, account_id: &str, list_id: &str) -> Result<()> {
        let url = self.url(&format!("/users/@me/lists/{}", encode(list_id)));
        let response = self
            .http
            .send_authed(account_id, TokenResource::GoogleApis, |token| {
                self.http.raw().delete(&url).bearer_auth(token)
            })
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        HttpClient::ensure_success(response).await
    }

    /// Fetch all tasks of one list.  `updated_min` selects the incremental
    /// window; `None` performs a full list.
    pub async fn list_tasks(
        &self,
        account_id: &str,
        list_id: &str,
        updated_min: Option<&str>,
    ) -> Result<Vec<GTask>> {
        let mut items = Vec::new();
        let mut page_token: Option<String> = None;
        let mut guard = 0;
        loop {
            guard += 1;
            if guard > 200 {
                return Err(invalid("Google Tasks pagination did not terminate"));
            }
            let mut query: Vec<(String, String)> = vec![
                ("maxResults".into(), "100".into()),
                ("showCompleted".into(), "true".into()),
                ("showHidden".into(), "true".into()),
            ];
            if let Some(updated) = updated_min {
                query.push(("updatedMin".into(), updated.to_string()));
                query.push(("showDeleted".into(), "true".into()));
            }
            if let Some(token) = &page_token {
                query.push(("pageToken".into(), token.clone()));
            }
            let url = self.url(&format!("/lists/{}/tasks", encode(list_id)));
            let response = self
                .http
                .send_authed(account_id, TokenResource::GoogleApis, |token| {
                    self.http.raw().get(&url).bearer_auth(token).query(&query)
                })
                .await?;
            if !response.status().is_success() {
                return Err(Self::failure(response).await);
            }
            let page: GTaskPage = response
                .json()
                .await
                .map_err(|error| invalid(format!("invalid Google tasks JSON: {error}")))?;
            items.extend(page.items);
            match page.next_page_token {
                Some(token) if !token.is_empty() => page_token = Some(token),
                _ => break,
            }
        }
        Ok(items)
    }

    /// Insert a task. `parent` and `previous` are query parameters, not part of
    /// the body: `Task.parent` is read-only in the API, so a nested task must
    /// name its parent here or it is created at the top level instead.
    pub async fn insert_task(
        &self,
        account_id: &str,
        list_id: &str,
        body: &serde_json::Value,
        parent: Option<&str>,
        previous: Option<&str>,
    ) -> Result<GTask> {
        let url = self.url(&format!("/lists/{}/tasks", encode(list_id)));
        let mut params: Vec<(&str, &str)> = Vec::new();
        if let Some(parent) = parent {
            params.push(("parent", parent));
        }
        if let Some(previous) = previous {
            params.push(("previous", previous));
        }
        let response = self
            .http
            .send_authed(account_id, TokenResource::GoogleApis, |token| {
                self.http
                    .raw()
                    .post(&url)
                    .query(&params)
                    .bearer_auth(token)
                    .json(body)
            })
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid Google task JSON: {error}")))
    }

    pub async fn patch_task(
        &self,
        account_id: &str,
        list_id: &str,
        task_id: &str,
        body: &serde_json::Value,
    ) -> Result<GTask> {
        let url = self.url(&format!(
            "/lists/{}/tasks/{}",
            encode(list_id),
            encode(task_id)
        ));
        let response = self
            .http
            .send_authed(account_id, TokenResource::GoogleApis, |token| {
                self.http.raw().patch(&url).bearer_auth(token).json(body)
            })
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid Google task JSON: {error}")))
    }

    pub async fn delete_task(&self, account_id: &str, list_id: &str, task_id: &str) -> Result<()> {
        let url = self.url(&format!(
            "/lists/{}/tasks/{}",
            encode(list_id),
            encode(task_id)
        ));
        let response = self
            .http
            .send_authed(account_id, TokenResource::GoogleApis, |token| {
                self.http.raw().delete(&url).bearer_auth(token)
            })
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        HttpClient::ensure_success(response).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn move_task(
        &self,
        account_id: &str,
        from_list: &str,
        task_id: &str,
        to_list: &str,
    ) -> Result<GTask> {
        self.move_task_after(account_id, from_list, task_id, to_list, None)
            .await
    }

    /// `tasks.move` with an explicit `previous` sibling, used to apply the
    /// local custom order.  `None` moves the task to the end of the list.
    pub async fn move_task_after(
        &self,
        account_id: &str,
        from_list: &str,
        task_id: &str,
        to_list: &str,
        previous: Option<&str>,
    ) -> Result<GTask> {
        let url = self.url(&format!(
            "/lists/{}/tasks/{}/move",
            encode(from_list),
            encode(task_id)
        ));
        let mut query: Vec<(String, String)> = vec![("destinationTaskList".into(), to_list.into())];
        if let Some(previous) = previous.filter(|value| !value.trim().is_empty()) {
            query.push(("previous".into(), previous.to_string()));
        }
        let response = self
            .http
            .send_authed(account_id, TokenResource::GoogleApis, |token| {
                self.http.raw().post(&url).bearer_auth(token).query(&query)
            })
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid Google task JSON: {error}")))
    }
}

/* ──────────────────────────── JSON shapes ──────────────────────────── */

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GTaskListPage {
    #[serde(default)]
    pub items: Vec<GTaskList>,
    #[serde(default)]
    pub next_page_token: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GTaskList {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub updated: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GTaskPage {
    #[serde(default)]
    pub items: Vec<GTask>,
    #[serde(default)]
    pub next_page_token: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GTask {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    /// RFC 3339 date-time at midnight UTC.
    #[serde(default)]
    pub due: Option<String>,
    #[serde(default)]
    pub completed: Option<String>,
    #[serde(default)]
    pub deleted: Option<bool>,
    #[serde(default)]
    pub hidden: Option<bool>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub position: Option<String>,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub updated: Option<String>,
}

/* ───────────────────────────── mapping ────────────────────────────── */

/// Convert a Google task into the contract [`Task`]; ids are assigned by the
/// sync engine.
pub fn task_from_google(item: &GTask, list_id: &str, account_id: &str) -> Task {
    let now = Utc::now();
    let status = if item.status.as_deref() == Some("completed") {
        TaskStatus::Completed
    } else {
        TaskStatus::NotStarted
    };
    Task {
        id: String::new(),
        list_id: list_id.to_string(),
        account_id: account_id.to_string(),
        title: item.title.clone().unwrap_or_default(),
        notes: item.notes.clone().unwrap_or_default(),
        status,
        importance: nuncio_core::models::Importance::Normal,
        due_date: item.due.as_deref().and_then(date_part),
        reminder_at: None,
        recurrence: None,
        my_day_date: None,
        completed_at: item.completed.as_deref().and_then(parse_naive_utc),
        created_at: item
            .updated
            .as_deref()
            .and_then(parse_naive_utc)
            .unwrap_or(now),
        updated_at: item
            .updated
            .as_deref()
            .and_then(parse_naive_utc)
            .unwrap_or(now),
        steps: Vec::new(),
        sort_order: position_to_order(item.position.as_deref()),
        linked_message_id: None,
    }
}

/// Body for `tasks.insert` / `tasks.patch`.
pub fn task_body(task: &Task, for_create: bool) -> serde_json::Value {
    let mut body = serde_json::json!({
        "title": task.title,
        "notes": task.notes,
        "status": if task.status == TaskStatus::Completed { "completed" } else { "needsAction" },
    });
    match task.due_date.as_deref().and_then(date_part) {
        Some(due) => body["due"] = serde_json::json!(format!("{due}T00:00:00.000Z")),
        None if !for_create => body["due"] = serde_json::Value::Null,
        None => {}
    }
    if for_create {
        if let Some(completed) = task.completed_at {
            body["completed"] = serde_json::json!(completed.to_rfc3339());
        }
    }
    body
}

/* ──────────────────────────── provider ────────────────────────────── */

#[derive(Clone)]
pub struct GoogleTasksProvider {
    client: GoogleTasksClient,
}

impl std::fmt::Debug for GoogleTasksProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleTasksProvider").finish()
    }
}

impl GoogleTasksProvider {
    pub fn new(http: HttpClient) -> Self {
        Self {
            client: GoogleTasksClient::new(http),
        }
    }

    pub fn with_base(http: HttpClient, base: impl Into<String>) -> Self {
        Self {
            client: GoogleTasksClient::with_base(http, base),
        }
    }

    pub fn client(&self) -> &GoogleTasksClient {
        &self.client
    }
}

fn list_from_google(item: &GTaskList) -> RemoteTaskList {
    RemoteTaskList {
        local_id: String::new(),
        remote_id: item.id.clone(),
        href: None,
        name: if item.title.trim().is_empty() {
            "任务".to_string()
        } else {
            item.title.clone()
        },
        color: None,
        read_only: false,
        well_known_list_name: nuncio_core::models::WELL_KNOWN_NONE.to_string(),
    }
}

#[async_trait]
impl TaskProvider for GoogleTasksProvider {
    fn name(&self) -> &'static str {
        "google"
    }

    fn capabilities(&self) -> TaskCapabilities {
        TaskCapabilities {
            importance: false,
            reminders: false,
            steps: true,
            recurrence: false,
            ordering: true,
            server_recurring_completion: false,
        }
    }

    async fn list_task_lists(&self, account: &Account) -> Result<Vec<RemoteTaskList>> {
        Ok(self
            .client
            .list_task_lists(&account.id)
            .await?
            .iter()
            .map(list_from_google)
            .collect())
    }

    async fn create_task_list(
        &self,
        account: &Account,
        name: &str,
        _color: &str,
    ) -> Result<RemoteTaskList> {
        let item = self.client.create_task_list(&account.id, name).await?;
        Ok(list_from_google(&item))
    }

    async fn update_task_list(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        name: Option<&str>,
        _color: Option<&str>,
    ) -> Result<()> {
        if let Some(name) = name {
            self.client
                .patch_task_list(&account.id, &list.remote_id, name)
                .await?;
        }
        Ok(())
    }

    async fn delete_task_list(&self, account: &Account, list: &RemoteTaskList) -> Result<()> {
        self.client
            .delete_task_list(&account.id, &list.remote_id)
            .await
    }

    async fn sync_tasks(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        cursor: &TaskCursor,
    ) -> Result<TaskDelta> {
        let started = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let updated_min = cursor.sync_token.as_deref();
        let items = self
            .client
            .list_tasks(&account.id, &list.remote_id, updated_min)
            .await?;
        let mut tasks = Vec::new();
        let mut deleted_remote_ids = Vec::new();
        for item in &items {
            if item.id.trim().is_empty() {
                continue;
            }
            if item.deleted == Some(true) {
                deleted_remote_ids.push(item.id.clone());
                continue;
            }
            let task = task_from_google(item, &list.local_id, &account.id);
            tasks.push(RemoteTask {
                task,
                remote_id: Some(item.id.clone()),
                href: None,
                etag: item.etag.clone(),
                parent_remote_id: item.parent.clone(),
                // `updatedMin` returns only changed tasks, so subtasks that did
                // not change are absent.  Only a full sync can claim the step
                // list is complete.
                steps_complete: updated_min.is_none(),
            });
        }
        debug!(
            list = %list.local_id,
            tasks = tasks.len(),
            deleted = deleted_remote_ids.len(),
            "Google Tasks delta"
        );
        Ok(TaskDelta {
            tasks,
            deleted_remote_ids,
            deleted_hrefs: Vec::new(),
            sync_token: Some(started),
            ctag: None,
            full_sync: updated_min.is_none(),
        })
    }

    async fn create_task(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        task: &Task,
    ) -> Result<RemoteWriteResult> {
        let body = task_body(task, true);
        let created = self
            .client
            .insert_task(&account.id, &list.remote_id, &body, None, None)
            .await?;
        Ok(RemoteWriteResult {
            remote_id: Some(created.id).filter(|id| !id.is_empty()),
            etag: created.etag,
            href: None,
        })
    }

    async fn update_task(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        current: &RemoteTaskRef,
        task: &Task,
    ) -> Result<RemoteWriteResult> {
        let Some(id) = current.remote_id.clone() else {
            return Err(AppError::conflict("task has no Google id"));
        };
        let body = task_body(task, false);
        let updated = self
            .client
            .patch_task(&account.id, &list.remote_id, &id, &body)
            .await?;
        Ok(RemoteWriteResult {
            remote_id: Some(updated.id).filter(|id| !id.is_empty()).or(Some(id)),
            etag: updated.etag,
            href: None,
        })
    }

    async fn delete_task(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        current: &RemoteTaskRef,
    ) -> Result<()> {
        let Some(id) = current.remote_id.as_deref() else {
            return Ok(());
        };
        self.client
            .delete_task(&account.id, &list.remote_id, id)
            .await
    }

    async fn move_task(
        &self,
        account: &Account,
        from: &RemoteTaskList,
        to: &RemoteTaskList,
        current: &RemoteTaskRef,
        _task: &Task,
    ) -> Result<RemoteWriteResult> {
        let Some(id) = current.remote_id.clone() else {
            return Err(AppError::conflict("task has no Google id"));
        };
        let moved = self
            .client
            .move_task(&account.id, &from.remote_id, &id, &to.remote_id)
            .await?;
        Ok(RemoteWriteResult {
            remote_id: Some(moved.id).filter(|id| !id.is_empty()).or(Some(id)),
            etag: moved.etag,
            href: None,
        })
    }

    async fn reorder_tasks(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        remote_ids: &[String],
    ) -> Result<()> {
        // Placing each task right after its predecessor builds the desired
        // contiguous order; the first task keeps its absolute position.
        for pair in remote_ids.windows(2) {
            let previous = &pair[0];
            let task = &pair[1];
            self.client
                .move_task_after(
                    &account.id,
                    &list.remote_id,
                    task,
                    &list.remote_id,
                    Some(previous),
                )
                .await?;
        }
        Ok(())
    }

    async fn create_step(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        parent: &RemoteTaskRef,
        previous: Option<&str>,
        step: &TaskStep,
    ) -> Result<Option<RemoteWriteResult>> {
        let Some(parent_id) = parent.remote_id.as_deref() else {
            return Ok(None);
        };
        // `parent` is a query parameter. Passing it in the body silently
        // creates a top-level task instead of a subtask, which is why steps
        // showed up flattened (and duplicated) in the Google UI.
        let body = serde_json::json!({
            "title": step.title,
            "status": if step.is_completed { "completed" } else { "needsAction" },
        });
        let created = self
            .client
            .insert_task(
                &account.id,
                &list.remote_id,
                &body,
                Some(parent_id),
                previous,
            )
            .await?;
        Ok(Some(RemoteWriteResult {
            remote_id: Some(created.id).filter(|id| !id.is_empty()),
            etag: created.etag,
            href: None,
        }))
    }

    async fn update_step(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        _parent: &RemoteTaskRef,
        remote_step_id: &str,
        step: &TaskStep,
    ) -> Result<()> {
        let body = serde_json::json!({
            "title": step.title,
            "status": if step.is_completed { "completed" } else { "needsAction" },
        });
        self.client
            .patch_task(&account.id, &list.remote_id, remote_step_id, &body)
            .await?;
        Ok(())
    }

    async fn delete_step(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        _parent: &RemoteTaskRef,
        remote_step_id: &str,
    ) -> Result<()> {
        self.client
            .delete_task(&account.id, &list.remote_id, remote_step_id)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::models::{Importance, TaskStatus};

    const TASK: &str = r#"{
      "kind": "tasks#task",
      "id": "MTIzNDU2",
      "etag": "\"abc\"",
      "title": "Water the plants",
      "updated": "2026-11-12T08:00:00.000Z",
      "status": "needsAction",
      "due": "2026-11-12T00:00:00.000Z",
      "notes": "Kitchen and balcony",
      "position": "00000000000000000010",
      "parent": "parent-1"
    }"#;

    #[test]
    fn maps_google_task() {
        let item: GTask = serde_json::from_str(TASK).expect("task");
        let task = task_from_google(&item, "local-tasks", "local");
        assert_eq!(task.title, "Water the plants");
        assert_eq!(task.notes, "Kitchen and balcony");
        assert_eq!(task.due_date.as_deref(), Some("2026-11-12"));
        assert_eq!(task.status, TaskStatus::NotStarted);
        assert_eq!(task.importance, Importance::Normal);
        assert_eq!(task.sort_order, 10);
        assert!(task.recurrence.is_none());
        assert!(task.reminder_at.is_none());
        assert_eq!(item.parent.as_deref(), Some("parent-1"));
    }

    #[test]
    fn maps_completed_task() {
        let item: GTask = serde_json::from_str(
            r#"{"id":"1","status":"completed","completed":"2026-11-12T09:00:00.000Z"}"#,
        )
        .expect("task");
        let task = task_from_google(&item, "l", "a");
        assert_eq!(task.status, TaskStatus::Completed);
        assert!(task.completed_at.is_some());
    }

    #[test]
    fn builds_task_body() {
        let mut task = task_from_google(
            &serde_json::from_str(TASK).expect("task"),
            "local-tasks",
            "local",
        );
        task.title = "Updated".into();
        let body = task_body(&task, false);
        assert_eq!(body["title"], "Updated");
        assert_eq!(body["due"], "2026-11-12T00:00:00.000Z");
        assert_eq!(body["status"], "needsAction");
        assert!(body.get("completed").is_none());
    }

    #[test]
    fn maps_deleted_flag() {
        let item: GTask = serde_json::from_str(r#"{"id":"1","deleted":true}"#).expect("task");
        assert_eq!(item.deleted, Some(true));
    }
}
