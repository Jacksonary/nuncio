//! To-do list and task commands.

use tauri::State;

use nuncio_core::models::{Importance, SmartListCounts, Task, TaskInput, TaskList, TaskQuery};

use crate::state::AppState;

#[tauri::command]
pub async fn tasklist_list(state: State<'_, AppState>) -> nuncio_core::Result<Vec<TaskList>> {
    state.pim.tasklist_list().await
}

#[tauri::command]
pub async fn tasklist_create(
    state: State<'_, AppState>,
    account_id: String,
    name: String,
    color: String,
) -> nuncio_core::Result<TaskList> {
    state.pim.tasklist_create(&account_id, &name, &color).await
}

#[tauri::command]
pub async fn tasklist_update(
    state: State<'_, AppState>,
    id: String,
    name: Option<String>,
    color: Option<String>,
) -> nuncio_core::Result<TaskList> {
    state
        .pim
        .tasklist_update(&id, name.as_deref(), color.as_deref())
        .await
}

#[tauri::command]
pub async fn tasklist_delete(state: State<'_, AppState>, id: String) -> nuncio_core::Result<()> {
    state.pim.tasklist_delete(&id).await
}

#[tauri::command]
pub async fn task_smart_counts(
    state: State<'_, AppState>,
    today: String,
) -> nuncio_core::Result<SmartListCounts> {
    state.pim.task_smart_counts(today).await
}

#[tauri::command]
pub async fn task_query(
    state: State<'_, AppState>,
    query: TaskQuery,
) -> nuncio_core::Result<Vec<Task>> {
    state.pim.task_query(query).await
}

#[tauri::command]
pub async fn task_get(state: State<'_, AppState>, id: String) -> nuncio_core::Result<Task> {
    state.pim.task_get(&id).await
}

#[tauri::command]
pub async fn task_create(
    state: State<'_, AppState>,
    list_id: String,
    input: TaskInput,
) -> nuncio_core::Result<Task> {
    state.pim.task_create(&list_id, input).await
}

#[tauri::command]
pub async fn task_update(
    state: State<'_, AppState>,
    id: String,
    input: TaskInput,
) -> nuncio_core::Result<Task> {
    state.pim.task_update(&id, input).await
}

#[tauri::command]
pub async fn task_set_completed(
    state: State<'_, AppState>,
    id: String,
    value: bool,
) -> nuncio_core::Result<Task> {
    state.pim.task_set_completed(&id, value).await
}

#[tauri::command]
pub async fn task_set_importance(
    state: State<'_, AppState>,
    id: String,
    importance: Importance,
) -> nuncio_core::Result<Task> {
    state.pim.task_set_importance(&id, importance).await
}

#[tauri::command]
pub async fn task_set_my_day(
    state: State<'_, AppState>,
    id: String,
    date: Option<String>,
) -> nuncio_core::Result<Task> {
    state.pim.task_set_my_day(&id, date.as_deref()).await
}

#[tauri::command]
pub async fn task_move(
    state: State<'_, AppState>,
    id: String,
    list_id: String,
) -> nuncio_core::Result<Task> {
    state.pim.task_move(&id, &list_id).await
}

#[tauri::command]
pub async fn task_reorder(
    state: State<'_, AppState>,
    list_id: String,
    ids: Vec<String>,
) -> nuncio_core::Result<()> {
    state.pim.task_reorder(&list_id, ids).await
}

#[tauri::command]
pub async fn task_delete(state: State<'_, AppState>, id: String) -> nuncio_core::Result<()> {
    state.pim.task_delete(&id).await
}

#[tauri::command]
pub async fn task_search(
    state: State<'_, AppState>,
    text: String,
) -> nuncio_core::Result<Vec<Task>> {
    state.pim.task_search(&text).await
}

#[tauri::command]
pub async fn task_dismiss_reminder(
    state: State<'_, AppState>,
    id: String,
) -> nuncio_core::Result<()> {
    state.pim.task_dismiss_reminder(&id).await
}

#[tauri::command]
pub async fn task_snooze_reminder(
    state: State<'_, AppState>,
    id: String,
    minutes: u32,
) -> nuncio_core::Result<()> {
    state.pim.task_snooze_reminder(&id, minutes).await
}

#[tauri::command]
pub async fn message_create_task(
    state: State<'_, AppState>,
    id: String,
) -> nuncio_core::Result<Task> {
    state.pim.message_create_task(&id).await
}
