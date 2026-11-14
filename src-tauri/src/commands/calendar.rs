//! Calendar and event commands (delegated to `nuncio-pim`).

use tauri::State;

use nuncio_core::models::{
    AttendeeStatus, Calendar, CalendarEvent, EventInput, EventInstance, RecurrenceEditScope,
};

use crate::state::AppState;

#[tauri::command]
pub async fn calendar_list(state: State<'_, AppState>) -> nuncio_core::Result<Vec<Calendar>> {
    state.pim.calendar_list().await
}

#[tauri::command]
pub async fn calendar_create(
    state: State<'_, AppState>,
    account_id: String,
    name: String,
    color: String,
) -> nuncio_core::Result<Calendar> {
    state.pim.calendar_create(&account_id, &name, &color).await
}

#[tauri::command]
pub async fn calendar_update(
    state: State<'_, AppState>,
    id: String,
    name: Option<String>,
    color: Option<String>,
) -> nuncio_core::Result<Calendar> {
    state
        .pim
        .calendar_update(&id, name.as_deref(), color.as_deref())
        .await
}

#[tauri::command]
pub async fn calendar_set_visible(
    state: State<'_, AppState>,
    id: String,
    visible: bool,
) -> nuncio_core::Result<()> {
    state.pim.calendar_set_visible(&id, visible).await
}

#[tauri::command]
pub async fn calendar_delete(state: State<'_, AppState>, id: String) -> nuncio_core::Result<()> {
    state.pim.calendar_delete(&id).await
}

#[tauri::command]
pub async fn calendar_subscribe(
    state: State<'_, AppState>,
    url: String,
    name: String,
    color: String,
) -> nuncio_core::Result<Calendar> {
    state.pim.calendar_subscribe(&url, &name, &color).await
}

#[tauri::command]
pub async fn calendar_import_ics(
    state: State<'_, AppState>,
    path: String,
    calendar_id: String,
) -> nuncio_core::Result<u32> {
    state.pim.calendar_import_ics(&path, &calendar_id).await
}

#[tauri::command]
pub async fn calendar_export_ics(
    state: State<'_, AppState>,
    calendar_id: String,
    path: String,
) -> nuncio_core::Result<()> {
    state.pim.calendar_export_ics(&calendar_id, &path).await
}

#[tauri::command]
pub async fn event_range(
    state: State<'_, AppState>,
    from: String,
    to: String,
    calendar_ids: Option<Vec<String>>,
) -> nuncio_core::Result<Vec<EventInstance>> {
    state.pim.event_range(&from, &to, calendar_ids).await
}

#[tauri::command]
pub async fn event_get(
    state: State<'_, AppState>,
    event_id: String,
    instance_start: Option<String>,
) -> nuncio_core::Result<CalendarEvent> {
    state
        .pim
        .event_get(&event_id, instance_start.as_deref())
        .await
}

#[tauri::command]
pub async fn event_create(
    state: State<'_, AppState>,
    input: EventInput,
) -> nuncio_core::Result<CalendarEvent> {
    state.pim.event_create(input).await
}

#[tauri::command]
pub async fn event_update(
    state: State<'_, AppState>,
    event_id: String,
    input: EventInput,
    scope: RecurrenceEditScope,
    instance_start: Option<String>,
) -> nuncio_core::Result<CalendarEvent> {
    state
        .pim
        .event_update(&event_id, input, scope, instance_start.as_deref())
        .await
}

#[tauri::command]
pub async fn event_delete(
    state: State<'_, AppState>,
    event_id: String,
    scope: RecurrenceEditScope,
    instance_start: Option<String>,
    notify_attendees: bool,
) -> nuncio_core::Result<()> {
    state
        .pim
        .event_delete(
            &event_id,
            scope,
            instance_start.as_deref(),
            notify_attendees,
        )
        .await
}

#[tauri::command]
pub async fn event_respond(
    state: State<'_, AppState>,
    event_id: String,
    response: AttendeeStatus,
    comment: Option<String>,
    send_reply: bool,
) -> nuncio_core::Result<()> {
    state
        .pim
        .event_respond(&event_id, response, comment.as_deref(), send_reply)
        .await
}

#[tauri::command]
pub async fn event_search(
    state: State<'_, AppState>,
    text: String,
    limit: u32,
) -> nuncio_core::Result<Vec<EventInstance>> {
    state.pim.event_search(&text, limit).await
}

#[tauri::command]
pub async fn invite_respond(
    state: State<'_, AppState>,
    message_id: String,
    response: AttendeeStatus,
    comment: Option<String>,
) -> nuncio_core::Result<()> {
    state
        .pim
        .invite_respond(&message_id, response, comment.as_deref())
        .await
}

#[tauri::command]
pub async fn event_dismiss_reminder(
    state: State<'_, AppState>,
    event_id: String,
    instance_start: String,
) -> nuncio_core::Result<()> {
    state
        .pim
        .event_dismiss_reminder(&event_id, &instance_start)
        .await
}

#[tauri::command]
pub async fn event_snooze_reminder(
    state: State<'_, AppState>,
    event_id: String,
    instance_start: String,
    minutes: u32,
) -> nuncio_core::Result<()> {
    state
        .pim
        .event_snooze_reminder(&event_id, &instance_start, minutes)
        .await
}
