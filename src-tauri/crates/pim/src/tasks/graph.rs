//! Microsoft Graph To Do provider (`/me/todo/lists`).
//!
//! Full support for importance, reminders, recurrence and checklist items
//! (mapped onto task steps).  Sync uses `tasks/delta` per list; Graph creates
//! the next occurrence of a completed recurring task server-side, which is
//! advertised through [`TaskCapabilities::server_recurring_completion`].

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nuncio_core::models::{Account, Task, TaskStatus, TaskStep};
use nuncio_core::traits::TokenResource;
use serde::Deserialize;
use tracing::debug;

use crate::error::{invalid, AppError, Result};
use crate::graph::map::{map_recurrence, recurrence_body, GraphRecurrence};
use crate::graph::GRAPH_BASE;
use crate::http::HttpClient;
use crate::tasks::map::{date_part, graph_datetime, parse_naive_utc, status_from_graph};
use crate::tasks::provider::{
    RemoteTask, RemoteTaskList, RemoteTaskRef, RemoteWriteResult, TaskCapabilities, TaskCursor,
    TaskDelta, TaskProvider,
};

fn encode(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

/// Low-level Graph To Do client.
#[derive(Clone)]
pub struct GraphTasksClient {
    http: HttpClient,
    base: String,
}

impl std::fmt::Debug for GraphTasksClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphTasksClient")
            .field("base", &self.base)
            .finish()
    }
}

impl GraphTasksClient {
    pub fn new(http: HttpClient) -> Self {
        Self {
            http,
            base: GRAPH_BASE.to_string(),
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

    pub async fn list_lists(&self, account_id: &str) -> Result<Vec<GTodoList>> {
        let mut items = Vec::new();
        let mut next: Option<String> = Some(self.url("/me/todo/lists?$top=100"));
        let mut guard = 0;
        while let Some(url) = next.take() {
            guard += 1;
            if guard > 50 {
                break;
            }
            let response = self
                .http
                .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                    self.http.raw().get(&url).bearer_auth(token)
                })
                .await?;
            if !response.status().is_success() {
                return Err(Self::failure(response).await);
            }
            let page: GTodoListPage = response
                .json()
                .await
                .map_err(|error| invalid(format!("invalid Graph todo lists JSON: {error}")))?;
            items.extend(page.value);
            next = page.next_link;
        }
        Ok(items)
    }

    pub async fn create_list(&self, account_id: &str, name: &str) -> Result<GTodoList> {
        let url = self.url("/me/todo/lists");
        let body = serde_json::json!({ "displayName": name });
        let response = self
            .http
            .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                self.http.raw().post(&url).bearer_auth(token).json(&body)
            })
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid Graph todo list JSON: {error}")))
    }

    pub async fn patch_list(&self, account_id: &str, id: &str, name: &str) -> Result<()> {
        let url = self.url(&format!("/me/todo/lists/{}", encode(id)));
        let body = serde_json::json!({ "displayName": name });
        let response = self
            .http
            .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                self.http.raw().patch(&url).bearer_auth(token).json(&body)
            })
            .await?;
        HttpClient::ensure_success(response).await
    }

    pub async fn delete_list(&self, account_id: &str, id: &str) -> Result<()> {
        let url = self.url(&format!("/me/todo/lists/{}", encode(id)));
        let response = self
            .http
            .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                self.http.raw().delete(&url).bearer_auth(token)
            })
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        HttpClient::ensure_success(response).await
    }

    /// Fetch one delta page chain.  Returns `(tasks, deltaLink, full_sync)`.
    pub async fn tasks_delta(
        &self,
        account_id: &str,
        list_id: &str,
        delta_link: Option<&str>,
    ) -> Result<(Vec<GTodoTask>, Option<String>, bool)> {
        let mut next = Some(match delta_link {
            Some(link) => link.to_string(),
            None => self.url(&format!(
                // `$top` is not allowed with change tracking; the page size
                // goes in a `Prefer: odata.maxpagesize` header instead.
                "/me/todo/lists/{}/tasks/delta?$expand=checklistItems",
                encode(list_id)
            )),
        });
        let mut tasks = Vec::new();
        let mut delta: Option<String> = None;
        let mut guard = 0;
        let mut include_expand = delta_link.is_none();
        while let Some(url) = next.take() {
            guard += 1;
            if guard > 300 {
                return Err(invalid("Graph To Do delta did not terminate"));
            }
            let response = self
                .http
                .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                    self.http
                        .raw()
                        .get(&url)
                        .bearer_auth(token)
                        .header("Prefer", "odata.maxpagesize=100")
                })
                .await?;
            if !response.status().is_success() && include_expand {
                // Some tenants reject `$expand` on delta; retry without it.
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                debug!(%status, body_len = body.len(), "Graph To Do delta rejected $expand; retrying plainly");
                drop(body);
                include_expand = false;
                next = Some(self.url(&format!("/me/todo/lists/{}/tasks/delta", encode(list_id))));
                let _ = status;
                continue;
            }
            if !response.status().is_success() {
                return Err(Self::failure(response).await);
            }
            let page: GTodoTaskPage = response
                .json()
                .await
                .map_err(|error| invalid(format!("invalid Graph todo delta JSON: {error}")))?;
            tasks.extend(page.value);
            if let Some(link) = page.next_link {
                next = Some(link);
                continue;
            }
            delta = page.delta_link;
            break;
        }
        Ok((tasks, delta, delta_link.is_none()))
    }

    /// Checklist items for one task (used when delta omits `$expand`).
    pub async fn checklist_items(
        &self,
        account_id: &str,
        list_id: &str,
        task_id: &str,
    ) -> Result<Vec<GChecklistItem>> {
        let url = self.url(&format!(
            "/me/todo/lists/{}/tasks/{}/checklistItems",
            encode(list_id),
            encode(task_id)
        ));
        let response = self
            .http
            .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                self.http.raw().get(&url).bearer_auth(token)
            })
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        let page: GChecklistPage = response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid Graph checklist JSON: {error}")))?;
        Ok(page.value)
    }

    /* ───────────────────────── task attachments ───────────────────── */

    fn attachments_url(list_id: &str, task_id: &str) -> String {
        format!(
            "/me/todo/lists/{}/tasks/{}/attachments",
            encode(list_id),
            encode(task_id)
        )
    }

    pub async fn list_task_attachments(
        &self,
        account_id: &str,
        list_id: &str,
        task_id: &str,
    ) -> Result<Vec<GTaskAttachment>> {
        let url = self.url(&Self::attachments_url(list_id, task_id));
        let response = self
            .http
            .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                self.http.raw().get(&url).bearer_auth(token)
            })
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        let page: GAttachmentPage = response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid Graph attachment JSON: {error}")))?;
        Ok(page.value)
    }

    pub async fn create_task_attachment(
        &self,
        account_id: &str,
        list_id: &str,
        task_id: &str,
        name: &str,
        content_type: &str,
        content_bytes: &str,
    ) -> Result<GTaskAttachment> {
        let url = self.url(&Self::attachments_url(list_id, task_id));
        let body = serde_json::json!({
            "@odata.type": "#microsoft.graph.taskFileAttachment",
            "name": name,
            "contentType": content_type,
            "contentBytes": content_bytes,
        });
        let response = self
            .http
            .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                self.http.raw().post(&url).bearer_auth(token).json(&body)
            })
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid Graph attachment JSON: {error}")))
    }

    pub async fn delete_task_attachment(
        &self,
        account_id: &str,
        list_id: &str,
        task_id: &str,
        attachment_id: &str,
    ) -> Result<()> {
        let url = self.url(&format!(
            "{}/{}",
            Self::attachments_url(list_id, task_id),
            encode(attachment_id)
        ));
        let response = self
            .http
            .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                self.http.raw().delete(&url).bearer_auth(token)
            })
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        HttpClient::ensure_success(response).await
    }

    pub async fn download_task_attachment(
        &self,
        account_id: &str,
        list_id: &str,
        task_id: &str,
        attachment_id: &str,
    ) -> Result<Vec<u8>> {
        let url = self.url(&format!(
            "{}/{}/$value",
            Self::attachments_url(list_id, task_id),
            encode(attachment_id)
        ));
        let response = self
            .http
            .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                self.http.raw().get(&url).bearer_auth(token)
            })
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|error| invalid(format!("cannot read the attachment body: {error}")))
    }

    pub async fn create_task(
        &self,
        account_id: &str,
        list_id: &str,
        body: &serde_json::Value,
    ) -> Result<GTodoTask> {
        let url = self.url(&format!("/me/todo/lists/{}/tasks", encode(list_id)));
        let response = self
            .http
            .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                self.http.raw().post(&url).bearer_auth(token).json(body)
            })
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid Graph todo JSON: {error}")))
    }

    pub async fn get_task(
        &self,
        account_id: &str,
        list_id: &str,
        task_id: &str,
    ) -> Result<GTodoTask> {
        let url = self.url(&format!(
            "/me/todo/lists/{}/tasks/{}",
            encode(list_id),
            encode(task_id)
        ));
        let response = self
            .http
            .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                self.http.raw().get(&url).bearer_auth(token)
            })
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid Graph todo JSON: {error}")))
    }

    pub async fn update_task(
        &self,
        account_id: &str,
        list_id: &str,
        task_id: &str,
        etag: Option<&str>,
        body: &serde_json::Value,
    ) -> Result<GTodoTask> {
        let url = self.url(&format!(
            "/me/todo/lists/{}/tasks/{}",
            encode(list_id),
            encode(task_id)
        ));
        // The stored ETag goes stale whenever something else touches the task:
        // another client, or a checklist item write, which rewrites the parent.
        // The op worker retries with the same payload and therefore the same
        // ETag, so a conflict would repeat until the operation failed for good.
        // Refresh once and retry; the updated task we return carries the new
        // ETag, which the caller writes back.
        let mut current_etag = etag.map(str::to_string);
        let mut refreshed = false;
        loop {
            let response = self
                .http
                .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                    let request = self.http.raw().patch(&url).bearer_auth(token).json(body);
                    match current_etag.as_deref() {
                        Some(etag) if !etag.trim().is_empty() => request.header("If-Match", etag),
                        _ => request,
                    }
                })
                .await?;
            let status = response.status();
            let conflict = status == reqwest::StatusCode::CONFLICT
                || status == reqwest::StatusCode::PRECONDITION_FAILED;
            if conflict && !refreshed {
                refreshed = true;
                match self.get_task(account_id, list_id, task_id).await {
                    Ok(fresh) => {
                        current_etag = fresh.etag;
                        continue;
                    }
                    Err(error) => {
                        tracing::debug!(
                            %error,
                            "cannot refresh the Graph task ETag after a conflict"
                        );
                        return Err(Self::failure(response).await);
                    }
                }
            }
            if !status.is_success() {
                return Err(Self::failure(response).await);
            }
            return response
                .json()
                .await
                .map_err(|error| invalid(format!("invalid Graph todo JSON: {error}")));
        }
    }

    pub async fn delete_task(
        &self,
        account_id: &str,
        list_id: &str,
        task_id: &str,
        _etag: Option<&str>,
    ) -> Result<()> {
        let url = self.url(&format!(
            "/me/todo/lists/{}/tasks/{}",
            encode(list_id),
            encode(task_id)
        ));
        // No `If-Match`: a delete is unconditional intent, and a stale ETag
        // would otherwise keep the row alive until the op failed permanently.
        // A task id is never reused, so there is nothing to protect.
        let response = self
            .http
            .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                self.http.raw().delete(&url).bearer_auth(token)
            })
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        HttpClient::ensure_success(response).await
    }

    pub async fn create_checklist_item(
        &self,
        account_id: &str,
        list_id: &str,
        task_id: &str,
        body: &serde_json::Value,
    ) -> Result<GChecklistItem> {
        let url = self.url(&format!(
            "/me/todo/lists/{}/tasks/{}/checklistItems",
            encode(list_id),
            encode(task_id)
        ));
        let response = self
            .http
            .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                self.http.raw().post(&url).bearer_auth(token).json(body)
            })
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid Graph checklist JSON: {error}")))
    }

    pub async fn update_checklist_item(
        &self,
        account_id: &str,
        list_id: &str,
        task_id: &str,
        item_id: &str,
        body: &serde_json::Value,
    ) -> Result<()> {
        let url = self.url(&format!(
            "/me/todo/lists/{}/tasks/{}/checklistItems/{}",
            encode(list_id),
            encode(task_id),
            encode(item_id)
        ));
        let response = self
            .http
            .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                self.http.raw().patch(&url).bearer_auth(token).json(body)
            })
            .await?;
        HttpClient::ensure_success(response).await
    }

    pub async fn delete_checklist_item(
        &self,
        account_id: &str,
        list_id: &str,
        task_id: &str,
        item_id: &str,
    ) -> Result<()> {
        let url = self.url(&format!(
            "/me/todo/lists/{}/tasks/{}/checklistItems/{}",
            encode(list_id),
            encode(task_id),
            encode(item_id)
        ));
        let response = self
            .http
            .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                self.http.raw().delete(&url).bearer_auth(token)
            })
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        HttpClient::ensure_success(response).await
    }
}

/* ──────────────────────────── JSON shapes ──────────────────────────── */

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GTodoListPage {
    #[serde(default)]
    pub value: Vec<GTodoList>,
    #[serde(default, rename = "@odata.nextLink")]
    pub next_link: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GTaskAttachment {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub size: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GAttachmentPage {
    #[serde(default)]
    value: Vec<GTaskAttachment>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GTodoList {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub wellknown_list_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GTodoTaskPage {
    #[serde(default)]
    pub value: Vec<GTodoTask>,
    #[serde(default, rename = "@odata.nextLink")]
    pub next_link: Option<String>,
    #[serde(default, rename = "@odata.deltaLink")]
    pub delta_link: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GTodoDateTime {
    #[serde(default)]
    pub date_time: String,
    #[serde(default)]
    pub time_zone: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GTodoBody {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GChecklistItem {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub is_checked: bool,
    #[serde(default)]
    pub created_date_time: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GChecklistPage {
    #[serde(default)]
    pub value: Vec<GChecklistItem>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GTodoTask {
    #[serde(default)]
    pub id: String,
    #[serde(default, rename = "@odata.etag")]
    pub etag: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<GTodoBody>,
    #[serde(default)]
    pub importance: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub due_date_time: Option<GTodoDateTime>,
    #[serde(default)]
    pub start_date_time: Option<GTodoDateTime>,
    #[serde(default)]
    pub reminder_date_time: Option<GTodoDateTime>,
    #[serde(default)]
    pub is_reminder_on: Option<bool>,
    #[serde(default)]
    pub recurrence: Option<GraphRecurrence>,
    #[serde(default)]
    pub completed_date_time: Option<GTodoDateTime>,
    #[serde(default)]
    pub created_date_time: Option<String>,
    #[serde(default)]
    pub last_modified_date_time: Option<String>,
    #[serde(default)]
    pub checklist_items: Vec<GChecklistItem>,
    #[serde(default, rename = "@removed")]
    pub removed: Option<GChecklistItemRemoved>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GChecklistItemRemoved {
    #[serde(default)]
    pub reason: Option<String>,
}

impl GTodoTask {
    pub fn is_removed(&self) -> bool {
        self.removed.is_some()
    }
}

/* ───────────────────────────── mapping ────────────────────────────── */

fn instant(value: Option<&GTodoDateTime>) -> Option<DateTime<Utc>> {
    let value = value?;
    parse_naive_utc(&value.date_time)
}

/// Convert a Graph To Do task into the contract [`Task`].
pub fn task_from_graph(item: &GTodoTask, list_id: &str, account_id: &str) -> Task {
    let now = Utc::now();
    let status = status_from_graph(item.status.as_deref().unwrap_or("notStarted"));
    let importance =
        crate::tasks::map::importance_from(item.importance.as_deref().unwrap_or("normal"));
    let steps = item
        .checklist_items
        .iter()
        .enumerate()
        .map(|(index, step)| TaskStep {
            id: step.id.clone(),
            title: step.display_name.clone().unwrap_or_default(),
            is_completed: step.is_checked,
            sort_order: index as i64,
        })
        .collect();
    Task {
        id: String::new(),
        list_id: list_id.to_string(),
        account_id: account_id.to_string(),
        title: item.title.clone().unwrap_or_default(),
        notes: item
            .body
            .as_ref()
            .and_then(|body| body.content.clone())
            .unwrap_or_default(),
        status,
        importance,
        due_date: item
            .due_date_time
            .as_ref()
            .and_then(|value| date_part(&value.date_time)),
        reminder_at: if item.is_reminder_on.unwrap_or(false) {
            instant(item.reminder_date_time.as_ref())
        } else {
            None
        },
        recurrence: item.recurrence.as_ref().and_then(map_recurrence),
        my_day_date: None,
        completed_at: instant(item.completed_date_time.as_ref()),
        created_at: item
            .created_date_time
            .as_deref()
            .and_then(parse_naive_utc)
            .unwrap_or(now),
        updated_at: item
            .last_modified_date_time
            .as_deref()
            .and_then(parse_naive_utc)
            .unwrap_or(now),
        steps,
        sort_order: 0,
        linked_message_id: None,
    }
}

fn graph_datetime_value(value: DateTime<Utc>) -> serde_json::Value {
    serde_json::json!({
        "dateTime": graph_datetime(value),
        "timeZone": "UTC",
    })
}

/// Body for `POST /me/todo/lists/{id}/tasks` and `PATCH .../tasks/{id}`.
pub fn task_body(task: &Task, for_create: bool) -> serde_json::Value {
    let mut body = serde_json::json!({
        "title": task.title,
        "body": { "content": task.notes, "contentType": "text" },
        "importance": task.importance.as_str(),
        "status": if task.status == TaskStatus::Completed { "completed" } else { "notStarted" },
        "isReminderOn": task.reminder_at.is_some(),
    });
    match task.due_date.as_deref().and_then(date_part) {
        Some(due) => {
            body["dueDateTime"] = serde_json::json!({
                "dateTime": format!("{due}T00:00:00.0000000"),
                "timeZone": "UTC",
            });
            body["startDateTime"] = serde_json::json!({
                "dateTime": format!("{due}T00:00:00.0000000"),
                "timeZone": "UTC",
            });
        }
        None => {
            body["dueDateTime"] = serde_json::Value::Null;
            if !for_create {
                body["startDateTime"] = serde_json::Value::Null;
            }
        }
    }
    if let Some(reminder) = task.reminder_at {
        body["reminderDateTime"] = graph_datetime_value(reminder);
    } else {
        body["reminderDateTime"] = serde_json::Value::Null;
    }
    match &task.recurrence {
        Some(rule) => {
            let start = task.due_date.as_deref().unwrap_or("1970-01-01");
            body["recurrence"] = recurrence_body(rule, start, "UTC");
        }
        None => body["recurrence"] = serde_json::Value::Null,
    }
    if for_create {
        if let Some(completed) = task.completed_at {
            if task.status == TaskStatus::Completed {
                body["completedDateTime"] = graph_datetime_value(completed);
            }
        }
    }
    body
}

/// Map a Graph list, keeping its built-in marker.
///
/// Graph reports To Do's own lists (`defaultList`, `flaggedEmails`) alongside
/// user lists. They cannot be renamed or deleted, so the marker has to survive
/// into the local row; importing them as ordinary lists is what produced a
/// "Flagged Emails" entry that the web UI does not show and that failed on
/// every delete.
fn list_from_graph(item: &GTodoList) -> RemoteTaskList {
    // `unknownFutureValue` is Graph's evolution sentinel: it names no known
    // built-in list, so it is treated as an ordinary one rather than blocking
    // operations on a list we cannot reason about.
    let well_known_list_name = match item.wellknown_list_name.as_deref() {
        Some(nuncio_core::models::WELL_KNOWN_DEFAULT) => nuncio_core::models::WELL_KNOWN_DEFAULT,
        Some(nuncio_core::models::WELL_KNOWN_FLAGGED_EMAILS) => {
            nuncio_core::models::WELL_KNOWN_FLAGGED_EMAILS
        }
        _ => nuncio_core::models::WELL_KNOWN_NONE,
    }
    .to_string();
    RemoteTaskList {
        local_id: String::new(),
        remote_id: item.id.clone(),
        href: None,
        name: if item.display_name.trim().is_empty() {
            "任务".to_string()
        } else {
            item.display_name.clone()
        },
        color: None,
        read_only: false,
        well_known_list_name,
    }
}

/* ──────────────────────────── provider ────────────────────────────── */

#[derive(Clone)]
pub struct GraphTasksProvider {
    client: GraphTasksClient,
}

impl std::fmt::Debug for GraphTasksProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphTasksProvider").finish()
    }
}

impl GraphTasksProvider {
    pub fn new(http: HttpClient) -> Self {
        Self {
            client: GraphTasksClient::new(http),
        }
    }

    pub fn with_base(http: HttpClient, base: impl Into<String>) -> Self {
        Self {
            client: GraphTasksClient::with_base(http, base),
        }
    }

    pub fn client(&self) -> &GraphTasksClient {
        &self.client
    }
}

#[async_trait]
impl TaskProvider for GraphTasksProvider {
    fn name(&self) -> &'static str {
        "graph"
    }

    fn capabilities(&self) -> TaskCapabilities {
        TaskCapabilities {
            importance: true,
            reminders: true,
            steps: true,
            recurrence: true,
            ordering: false,
            server_recurring_completion: true,
        }
    }

    async fn list_task_lists(&self, account: &Account) -> Result<Vec<RemoteTaskList>> {
        Ok(self
            .client
            .list_lists(&account.id)
            .await?
            .iter()
            .map(list_from_graph)
            .collect())
    }

    async fn create_task_list(
        &self,
        account: &Account,
        name: &str,
        _color: &str,
    ) -> Result<RemoteTaskList> {
        let item = self.client.create_list(&account.id, name).await?;
        Ok(list_from_graph(&item))
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
                .patch_list(&account.id, &list.remote_id, name)
                .await?;
        }
        Ok(())
    }

    async fn delete_task_list(&self, account: &Account, list: &RemoteTaskList) -> Result<()> {
        self.client.delete_list(&account.id, &list.remote_id).await
    }

    async fn sync_tasks(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        cursor: &TaskCursor,
    ) -> Result<TaskDelta> {
        let (items, delta_link, full_sync) = match self
            .client
            .tasks_delta(&account.id, &list.remote_id, cursor.sync_token.as_deref())
            .await
        {
            Ok(result) => result,
            Err(error @ (AppError::NotFound(_) | AppError::InvalidInput(_)))
                if cursor.sync_token.is_some() =>
            {
                debug!(%error, "Graph To Do delta token expired; resyncing list");
                self.client
                    .tasks_delta(&account.id, &list.remote_id, None)
                    .await?
            }
            Err(error) => return Err(error),
        };
        let mut tasks = Vec::new();
        let mut deleted_remote_ids = Vec::new();
        let mut fetched_steps = 0usize;
        for item in &items {
            if item.is_removed() {
                if !item.id.is_empty() {
                    deleted_remote_ids.push(item.id.clone());
                }
                continue;
            }
            if item.id.trim().is_empty() {
                continue;
            }
            let mut task = task_from_graph(item, &list.local_id, &account.id);
            // Delta usually expands checklistItems; fetch them for small
            // lists when a tenant rejected `$expand`.  Large lists keep local
            // steps rather than silently dropping them.
            let mut steps_complete = !task.steps.is_empty() || items.len() <= 25;
            if task.steps.is_empty() && fetched_steps < 25 && items.len() <= 25 {
                fetched_steps += 1;
                match self
                    .client
                    .checklist_items(&account.id, &list.remote_id, &item.id)
                    .await
                {
                    Ok(steps) => {
                        task.steps = steps
                            .iter()
                            .enumerate()
                            .map(|(index, step)| TaskStep {
                                id: step.id.clone(),
                                title: step.display_name.clone().unwrap_or_default(),
                                is_completed: step.is_checked,
                                sort_order: index as i64,
                            })
                            .collect();
                        steps_complete = true;
                    }
                    Err(error) => {
                        debug!(%error, "could not fetch Graph checklist items");
                        steps_complete = false;
                    }
                }
            }
            tasks.push(RemoteTask {
                task,
                remote_id: Some(item.id.clone()),
                href: None,
                etag: item.etag.clone(),
                parent_remote_id: None,
                steps_complete,
                // Graph ids are real provider identifiers, never href-derived.
                uid_is_derived: false,
            });
        }
        Ok(TaskDelta {
            tasks,
            deleted_remote_ids,
            deleted_hrefs: Vec::new(),
            // The delta endpoint is not an href listing; a full sync
            // re-delivers every task, so presence comes from `seen_remote`.
            listed_hrefs: Vec::new(),
            incomplete: false,
            sync_token: delta_link,
            ctag: None,
            full_sync,
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
            .create_task(&account.id, &list.remote_id, &body)
            .await?;
        Ok(RemoteWriteResult {
            remote_id: Some(created.id).filter(|id| !id.is_empty()),
            etag: created.etag,
            href: None,
            uid_is_derived: false,
        })
    }

    async fn update_task(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        current: &RemoteTaskRef,
        task: &Task,
        _assigned_uid: Option<&str>,
    ) -> Result<RemoteWriteResult> {
        let Some(id) = current.remote_id.clone() else {
            return Err(AppError::conflict("task has no Graph id"));
        };
        let body = task_body(task, false);
        let updated = self
            .client
            .update_task(
                &account.id,
                &list.remote_id,
                &id,
                current.etag.as_deref(),
                &body,
            )
            .await?;
        Ok(RemoteWriteResult {
            remote_id: Some(updated.id).filter(|id| !id.is_empty()).or(Some(id)),
            etag: updated.etag,
            href: None,
            uid_is_derived: false,
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
            .delete_task(&account.id, &list.remote_id, id, current.etag.as_deref())
            .await
    }

    async fn create_step(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        parent: &RemoteTaskRef,
        _previous: Option<&str>,
        step: &TaskStep,
    ) -> Result<Option<RemoteWriteResult>> {
        let Some(parent_id) = parent.remote_id.as_deref() else {
            return Ok(None);
        };
        let body = serde_json::json!({
            "displayName": step.title,
            "isChecked": step.is_completed,
        });
        let created = self
            .client
            .create_checklist_item(&account.id, &list.remote_id, parent_id, &body)
            .await?;
        Ok(Some(RemoteWriteResult {
            remote_id: Some(created.id).filter(|id| !id.is_empty()),
            etag: None,
            href: None,
            uid_is_derived: false,
        }))
    }

    async fn update_step(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        parent: &RemoteTaskRef,
        remote_step_id: &str,
        step: &TaskStep,
    ) -> Result<()> {
        let Some(parent_id) = parent.remote_id.as_deref() else {
            return Ok(());
        };
        let body = serde_json::json!({
            "displayName": step.title,
            "isChecked": step.is_completed,
        });
        self.client
            .update_checklist_item(
                &account.id,
                &list.remote_id,
                parent_id,
                remote_step_id,
                &body,
            )
            .await
    }

    async fn delete_step(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        parent: &RemoteTaskRef,
        remote_step_id: &str,
    ) -> Result<()> {
        let Some(parent_id) = parent.remote_id.as_deref() else {
            return Ok(());
        };
        self.client
            .delete_checklist_item(&account.id, &list.remote_id, parent_id, remote_step_id)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::models::{Importance, RecurrenceFreq};

    const TASK: &str = r##"{
      "id": "task-1",
      "@odata.etag": "W/\"abc\"",
      "title": "Buy milk",
      "body": {"content": "2 litres", "contentType": "text"},
      "importance": "high",
      "status": "inProgress",
      "dueDateTime": {"dateTime": "2026-11-12T00:00:00.0000000", "timeZone": "UTC"},
      "reminderDateTime": {"dateTime": "2026-11-12T08:00:00.0000000", "timeZone": "UTC"},
      "isReminderOn": true,
      "recurrence": {"pattern": {"type": "daily", "interval": 1}, "range": {"type": "noEnd", "startDate": "2026-11-12"}},
      "createdDateTime": "2026-11-01T08:00:00Z",
      "lastModifiedDateTime": "2026-11-10T08:00:00Z",
      "checklistItems": [
        {"id": "s1", "displayName": "Check fridge", "isChecked": true},
        {"id": "s2", "displayName": "Take bag", "isChecked": false}
      ]
    }"##;

    #[test]
    fn maps_graph_task() {
        let item: GTodoTask = serde_json::from_str(TASK).expect("task");
        let task = task_from_graph(&item, "list-1", "acc");
        assert_eq!(task.title, "Buy milk");
        assert_eq!(task.notes, "2 litres");
        assert_eq!(task.importance, Importance::High);
        assert_eq!(task.status, TaskStatus::NotStarted);
        assert_eq!(task.due_date.as_deref(), Some("2026-11-12"));
        assert!(task.reminder_at.is_some());
        assert_eq!(
            task.recurrence.as_ref().map(|rule| rule.freq),
            Some(RecurrenceFreq::Daily)
        );
        assert_eq!(task.steps.len(), 2);
        assert!(task.steps[0].is_completed);
        assert_eq!(task.steps[1].id, "s2");
        assert_eq!(item.etag.as_deref(), Some("W/\"abc\""));
    }

    #[test]
    fn maps_removed_task() {
        let item: GTodoTask =
            serde_json::from_str(r#"{"id":"1","@removed":{"reason":"deleted"}}"#).expect("task");
        assert!(item.is_removed());
    }

    #[test]
    fn builds_graph_body() {
        let item: GTodoTask = serde_json::from_str(TASK).expect("task");
        let task = task_from_graph(&item, "list-1", "acc");
        let body = task_body(&task, false);
        assert_eq!(body["title"], "Buy milk");
        assert_eq!(body["importance"], "high");
        assert_eq!(
            body["dueDateTime"]["dateTime"],
            "2026-11-12T00:00:00.0000000"
        );
        assert_eq!(body["isReminderOn"], true);
        assert_eq!(body["recurrence"]["pattern"]["type"], "daily");
        assert_eq!(body["status"], "notStarted");
    }

    #[test]
    fn clears_optional_fields() {
        let mut task = task_from_graph(&serde_json::from_str(TASK).expect("task"), "l", "a");
        task.reminder_at = None;
        task.recurrence = None;
        let body = task_body(&task, false);
        assert!(body["reminderDateTime"].is_null());
        assert!(body["recurrence"].is_null());
        assert_eq!(body["isReminderOn"], false);
    }

    #[test]
    fn maps_wellknown_lists() {
        let parse = |json: &str| {
            let item: GTodoList = serde_json::from_str(json).expect("list");
            list_from_graph(&item)
        };
        let tasks = parse(r#"{"id":"l1","displayName":"Tasks","wellknownListName":"defaultList"}"#);
        assert_eq!(tasks.name, "Tasks");
        assert_eq!(
            tasks.well_known_list_name,
            nuncio_core::models::WELL_KNOWN_DEFAULT
        );
        let flagged = parse(
            r#"{"id":"l2","displayName":"Flagged Emails","wellknownListName":"flaggedEmails"}"#,
        );
        assert_eq!(
            flagged.well_known_list_name,
            nuncio_core::models::WELL_KNOWN_FLAGGED_EMAILS
        );
        // A user list, an unknown sentinel and a missing field all stay
        // ordinary lists so nothing is locked down by accident.
        for json in [
            r#"{"id":"l3","displayName":"Errands","wellknownListName":"none"}"#,
            r#"{"id":"l4","displayName":"Errands","wellknownListName":"unknownFutureValue"}"#,
            r#"{"id":"l5","displayName":"Errands"}"#,
        ] {
            assert_eq!(
                parse(json).well_known_list_name,
                nuncio_core::models::WELL_KNOWN_NONE,
                "{json}"
            );
        }
    }
}
