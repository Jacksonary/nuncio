//! CalDAV VTODO task provider.
//!
//! Task lists are calendar collections advertising the `VTODO` component.
//! `nuncio-ical` handles the VTODO payload (including `PRIORITY`, `VALARM`,
//! `RRULE` and `X-NUNCIO-STEP` sub-steps); this module only maps the collection
//! semantics and the remote identity (UID + href + ETag).

use async_trait::async_trait;
use nuncio_core::models::{Account, Task};
use tracing::debug;

use crate::dav::{DavClient, DavKind};
use crate::error::{AppError, Result};
use crate::http::HttpClient;
use crate::provider::CalendarCursor;
use crate::tasks::provider::{
    RemoteTask, RemoteTaskList, RemoteTaskRef, RemoteWriteResult, TaskCapabilities, TaskCursor,
    TaskDelta, TaskProvider,
};

const VTODO_TYPE: &str = "text/calendar; charset=utf-8";

#[derive(Clone)]
pub struct CalDavTaskProvider {
    http: HttpClient,
}

impl std::fmt::Debug for CalDavTaskProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CalDavTaskProvider").finish()
    }
}

impl CalDavTaskProvider {
    pub fn new(http: HttpClient) -> Self {
        Self { http }
    }

    fn client(&self, account: &Account) -> DavClient {
        DavClient::new(self.http.clone(), account.clone())
    }
}

fn collection(list: &RemoteTaskList) -> &str {
    list.href.as_deref().unwrap_or(&list.remote_id)
}

fn list_from_remote(remote: crate::provider::RemoteCalendar) -> RemoteTaskList {
    RemoteTaskList {
        local_id: String::new(),
        remote_id: remote.remote_id,
        href: remote.href,
        name: remote.name,
        color: remote.color,
        read_only: remote.read_only,
        well_known_list_name: nuncio_core::models::WELL_KNOWN_NONE.to_string(),
    }
}

#[async_trait]
impl TaskProvider for CalDavTaskProvider {
    fn name(&self) -> &'static str {
        "caldav"
    }

    fn capabilities(&self) -> TaskCapabilities {
        TaskCapabilities::all()
    }

    async fn list_task_lists(&self, account: &Account) -> Result<Vec<RemoteTaskList>> {
        let client = self.client(account);
        Ok(client
            .list_calendars_with_component("VTODO")
            .await?
            .into_iter()
            .map(list_from_remote)
            .collect())
    }

    async fn create_task_list(
        &self,
        account: &Account,
        name: &str,
        color: &str,
    ) -> Result<RemoteTaskList> {
        let client = self.client(account);
        let remote = client.create_task_list(name, Some(color)).await?;
        Ok(list_from_remote(remote))
    }

    async fn update_task_list(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        name: Option<&str>,
        color: Option<&str>,
    ) -> Result<()> {
        let client = self.client(account);
        client.update_calendar(collection(list), name, color).await
    }

    async fn delete_task_list(&self, account: &Account, list: &RemoteTaskList) -> Result<()> {
        let client = self.client(account);
        client.delete_calendar(collection(list)).await
    }

    async fn sync_tasks(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        cursor: &TaskCursor,
    ) -> Result<TaskDelta> {
        let client = self.client(account);
        let dav_cursor = CalendarCursor {
            local_calendar_id: cursor.local_list_id.clone(),
            sync_token: cursor.sync_token.clone(),
            ctag: cursor.ctag.clone(),
            etags: cursor.etags.clone(),
        };
        let delta = client
            .sync_resources(DavKind::Calendar, collection(list), &dav_cursor)
            .await?;
        let mut tasks = Vec::new();
        for resource in delta.changed {
            let parsed = nuncio_ical::ics::parse_todos(&resource.data).unwrap_or_else(|error| {
                debug!(href = %resource.href, %error, "skipping invalid VTODO resource");
                Vec::new()
            });
            let Some(first) = parsed.into_iter().next() else {
                continue;
            };
            let uid = if first.uid.trim().is_empty() {
                resource
                    .href
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .trim_end_matches(".ics")
                    .to_string()
            } else {
                first.uid.clone()
            };
            let mut task = first.into_task(String::new(), &list.local_id, &account.id);
            if task.title.trim().is_empty() {
                task.title = "任务".to_string();
            }
            tasks.push(RemoteTask {
                task,
                remote_id: Some(uid),
                href: Some(resource.href),
                etag: resource.etag,
                parent_remote_id: None,
                steps_complete: true,
            });
        }
        Ok(TaskDelta {
            tasks,
            deleted_remote_ids: Vec::new(),
            deleted_hrefs: delta.deleted_hrefs,
            sync_token: delta.sync_token,
            ctag: delta.ctag,
            full_sync: delta.full_sync,
        })
    }

    async fn create_task(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        task: &Task,
    ) -> Result<RemoteWriteResult> {
        let client = self.client(account);
        let uid = if task.id.trim().is_empty() {
            nuncio_core::models::new_id()
        } else {
            task.id.clone()
        };
        let mut outbound = task.clone();
        outbound.id = uid.clone();
        let url = DavClient::resource_url_for(collection(list), &uid, "ics");
        let body = nuncio_ical::ics::write_task(&outbound);
        let etag = client
            .put_resource(&url, &body, VTODO_TYPE, None, true)
            .await?;
        Ok(RemoteWriteResult {
            remote_id: Some(uid),
            etag,
            href: Some(url),
        })
    }

    async fn update_task(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        current: &RemoteTaskRef,
        task: &Task,
    ) -> Result<RemoteWriteResult> {
        let client = self.client(account);
        let uid = current
            .remote_id
            .clone()
            .ok_or_else(|| AppError::conflict("VTODO has no UID"))?;
        let mut outbound = task.clone();
        outbound.id = uid.clone();
        let url = current
            .href
            .clone()
            .unwrap_or_else(|| DavClient::resource_url_for(collection(list), &uid, "ics"));
        let body = nuncio_ical::ics::write_task(&outbound);
        let etag = client
            .put_resource(&url, &body, VTODO_TYPE, current.etag.as_deref(), false)
            .await?;
        Ok(RemoteWriteResult {
            remote_id: Some(uid),
            etag,
            href: Some(url),
        })
    }

    async fn delete_task(
        &self,
        account: &Account,
        list: &RemoteTaskList,
        current: &RemoteTaskRef,
    ) -> Result<()> {
        let client = self.client(account);
        let url = match (&current.href, &current.remote_id) {
            (Some(href), _) => href.clone(),
            (None, Some(uid)) => DavClient::resource_url_for(collection(list), uid, "ics"),
            (None, None) => return Ok(()),
        };
        client.delete_resource(&url, current.etag.as_deref()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VTODO: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Nuncio//EN\r\nBEGIN:VTODO\r\nUID:todo-1\r\nSUMMARY:Buy milk\r\nDESCRIPTION:2 litres\r\nDUE:20261112\r\nSTATUS:NEEDS-ACTION\r\nPRIORITY:1\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT30M\r\nEND:VALARM\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";

    #[test]
    fn maps_vtodo() {
        let todos = nuncio_ical::ics::parse_todos(VTODO).expect("todos");
        assert_eq!(todos.len(), 1);
        let task = todos.into_iter().next().unwrap().into_task("t", "l", "a");
        assert_eq!(task.title, "Buy milk");
        assert_eq!(task.notes, "2 litres");
        assert_eq!(task.due_date.as_deref(), Some("2026-11-12"));
        assert_eq!(task.importance, nuncio_core::models::Importance::High);
        assert!(task.reminder_at.is_some());
    }

    #[test]
    fn builds_collection_url() {
        let list = RemoteTaskList {
            local_id: String::new(),
            remote_id: "https://dav/tasks/".into(),
            href: Some("https://dav/tasks/".into()),
            name: "Tasks".into(),
            color: None,
            read_only: false,
            well_known_list_name: nuncio_core::models::WELL_KNOWN_NONE.to_string(),
        };
        assert_eq!(collection(&list), "https://dav/tasks/");
        assert_eq!(
            DavClient::resource_url_for(collection(&list), "uid 1", "ics"),
            "https://dav/tasks/uid%201.ics"
        );
    }
}
