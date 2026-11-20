//! Tasks: providers (Google Tasks, Microsoft Graph To Do, CalDAV VTODO), sync
//! engine, command facade, pending operations and reminders.

pub mod dav;
pub mod google;
pub mod graph;
pub mod map;
pub mod ops;
pub mod provider;
pub mod reminders;
pub mod service;

pub use dav::CalDavTaskProvider;
pub use google::{GoogleTasksClient, GoogleTasksProvider};
pub use graph::{GraphTasksClient, GraphTasksProvider};
pub use ops::{TaskListSnapshot, TaskOp, TaskSnapshot};
pub use provider::{
    RemoteTask, RemoteTaskList, RemoteTaskRef, RemoteWriteResult, TaskCapabilities, TaskCursor,
    TaskDelta, TaskProvider,
};
pub use reminders::{task_is_all_day, task_reminder_key, TaskReminderSource};
pub use service::TasksService;

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::models::{ProviderCapabilities, ProviderKind};
    use nuncio_core::{CoreContext, Paths};

    /// Two hand-maintained capability matrices decide whether a task field is
    /// synced or kept locally: the account-level `ProviderCapabilities` the UI
    /// reads (to show "仅保存在本机") and each provider's `TaskCapabilities`
    /// (which the engine uses to preserve local values). If they drift, the UI
    /// promises a sync the engine refuses, or hides a field it does sync.
    #[tokio::test]
    async fn provider_capabilities_match_account_defaults() {
        let dir =
            std::env::temp_dir().join(format!("nuncio-pim-caps-{}", nuncio_core::models::new_id()));
        let ctx = CoreContext::for_test(Paths::new(dir)).await.expect("ctx");
        let http = crate::http::HttpClient::new(ctx.creds.clone()).expect("http");

        let cases = [
            (
                ProviderKind::Gmail,
                GoogleTasksProvider::new(http.clone()).capabilities(),
            ),
            (
                ProviderKind::Outlook,
                GraphTasksProvider::new(http.clone()).capabilities(),
            ),
            (ProviderKind::Local, TaskCapabilities::local()),
        ];
        for (kind, caps) in cases {
            let account = ProviderCapabilities::defaults_for(kind);
            assert_eq!(
                account.task_importance, caps.importance,
                "{kind:?}: importance"
            );
            assert_eq!(
                account.task_reminders, caps.reminders,
                "{kind:?}: reminders"
            );
            assert_eq!(account.task_steps, caps.steps, "{kind:?}: steps");
            assert_eq!(
                account.task_recurrence, caps.recurrence,
                "{kind:?}: recurrence"
            );
        }
    }
}
