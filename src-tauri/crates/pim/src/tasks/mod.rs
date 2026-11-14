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
