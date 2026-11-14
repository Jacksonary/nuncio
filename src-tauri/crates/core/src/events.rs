//! Event names and payload structs mirroring `EventPayloads` in `src/ipc/api.ts`.

use serde::Serialize;
use serde_json::Value;
use tracing::warn;

use crate::models::{Address, Draft, Id, MailScope, NavigateTarget, SyncStatus};
use crate::traits::EventSink;

pub const SYNC_STATUS: &str = "sync://status";
pub const ACCOUNTS_CHANGED: &str = "accounts://changed";
pub const SETTINGS_CHANGED: &str = "settings://changed";
pub const MAIL_CHANGED: &str = "mail://changed";
pub const CALENDAR_CHANGED: &str = "calendar://changed";
pub const CONTACTS_CHANGED: &str = "contacts://changed";
pub const TASKS_CHANGED: &str = "tasks://changed";
pub const OP_FAILED: &str = "op://failed";
pub const REMINDER_DUE: &str = "reminder://due";
pub const NAVIGATE: &str = "app://navigate";
pub const OPEN_REQUESTED: &str = "app://open-requested";
pub const COMPOSE: &str = "app://compose";
/// Sent to the main window before its webview is destroyed: flush unsaved
/// drafts, then acknowledge with `compose_flush_done`.
///
/// The payload is the handshake id. It must be echoed in the acknowledgement:
/// a composer that finished saving for an *earlier* close would otherwise
/// release the newer one, which may still be writing.
pub const FLUSH_DRAFTS: &str = "app://flush-drafts";
/// The close handshake was abandoned (drafts could not be flushed): the window
/// stays open and the composer must unfreeze.
pub const CLOSE_CANCELLED: &str = "app://close-cancelled";

/// `mail://changed` kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MailChangeKind {
    New,
    Updated,
    Deleted,
    Folders,
}

impl MailChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Updated => "updated",
            Self::Deleted => "deleted",
            Self::Folders => "folders",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountsChanged {
    pub account_ids: Vec<Id>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailChanged {
    pub account_id: Id,
    pub folder_ids: Vec<Id>,
    pub message_ids: Vec<Id>,
    pub kind: MailChangeKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarChanged {
    pub calendar_ids: Vec<Id>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContactsChanged {
    pub address_book_ids: Vec<Id>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TasksChanged {
    pub list_ids: Vec<Id>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpFailed {
    pub account_id: Id,
    pub description: String,
    pub error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ReminderKind {
    Event,
    Task,
}

impl ReminderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Event => "event",
            Self::Task => "task",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReminderDue {
    pub kind: ReminderKind,
    pub id: Id,
    pub instance_start: Option<String>,
    pub title: String,
    pub start: Option<String>,
    /// True for all-day events and for date-only task reminders.
    pub all_day: bool,
    pub location: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposeRequest {
    pub draft: Draft,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailRecipients {
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
}

impl MailRecipients {
    pub fn empty() -> Self {
        Self {
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
        }
    }
}

/// Serialize `payload` and emit it, logging (never panicking) on failure.
fn emit<T: Serialize>(events: &dyn EventSink, name: &str, payload: &T) {
    match serde_json::to_value(payload) {
        Ok(value) => events.emit(name, value),
        Err(error) => warn!(event = name, %error, "failed to serialize event payload"),
    }
}

pub fn emit_sync_status(events: &dyn EventSink, status: &SyncStatus) {
    emit(events, SYNC_STATUS, status);
}

pub fn emit_accounts_changed(events: &dyn EventSink, account_ids: Vec<Id>) {
    emit(events, ACCOUNTS_CHANGED, &AccountsChanged { account_ids });
}

pub fn emit_settings_changed(events: &dyn EventSink, settings: &crate::models::AppSettings) {
    emit(events, SETTINGS_CHANGED, settings);
}

pub fn emit_mail_changed(
    events: &dyn EventSink,
    account_id: impl Into<Id>,
    folder_ids: Vec<Id>,
    message_ids: Vec<Id>,
    kind: MailChangeKind,
) {
    emit(
        events,
        MAIL_CHANGED,
        &MailChanged {
            account_id: account_id.into(),
            folder_ids,
            message_ids,
            kind,
        },
    );
}

/// Convenience for freshly arrived messages.
pub fn emit_mail_new(
    events: &dyn EventSink,
    account_id: impl Into<Id>,
    folder_id: impl Into<Id>,
    message_ids: Vec<Id>,
) {
    emit_mail_changed(
        events,
        account_id,
        vec![folder_id.into()],
        message_ids,
        MailChangeKind::New,
    );
}

pub fn emit_folders_changed(
    events: &dyn EventSink,
    account_id: impl Into<Id>,
    folder_ids: Vec<Id>,
) {
    emit_mail_changed(
        events,
        account_id,
        folder_ids,
        Vec::new(),
        MailChangeKind::Folders,
    );
}

pub fn emit_calendar_changed(events: &dyn EventSink, calendar_ids: Vec<Id>) {
    emit(events, CALENDAR_CHANGED, &CalendarChanged { calendar_ids });
}

pub fn emit_contacts_changed(events: &dyn EventSink, address_book_ids: Vec<Id>) {
    emit(
        events,
        CONTACTS_CHANGED,
        &ContactsChanged { address_book_ids },
    );
}

pub fn emit_tasks_changed(events: &dyn EventSink, list_ids: Vec<Id>) {
    emit(events, TASKS_CHANGED, &TasksChanged { list_ids });
}

pub fn emit_op_failed(
    events: &dyn EventSink,
    account_id: impl Into<Id>,
    description: &str,
    error: &str,
) {
    emit(
        events,
        OP_FAILED,
        &OpFailed {
            account_id: account_id.into(),
            description: description.to_string(),
            error: error.to_string(),
        },
    );
}

pub fn emit_reminder_due(events: &dyn EventSink, reminder: ReminderDue) {
    emit(events, REMINDER_DUE, &reminder);
}

pub fn emit_navigate(events: &dyn EventSink, target: &NavigateTarget) {
    emit(events, NAVIGATE, target);
}

pub fn emit_open_requested(events: &dyn EventSink) {
    events.emit(OPEN_REQUESTED, Value::Object(serde_json::Map::new()));
}

pub fn emit_compose(events: &dyn EventSink, draft: &Draft) {
    emit(
        events,
        COMPOSE,
        &ComposeRequest {
            draft: draft.clone(),
        },
    );
}

/// Parse a `mail://changed` payload (helper for tests / IPC bridges).
pub fn parse_mail_changed(value: &Value) -> Option<(String, Vec<String>, Vec<String>, String)> {
    Some((
        value.get("accountId")?.as_str()?.to_string(),
        value
            .get("folderIds")?
            .as_array()?
            .iter()
            .filter_map(|id| id.as_str().map(str::to_string))
            .collect(),
        value
            .get("messageIds")?
            .as_array()?
            .iter()
            .filter_map(|id| id.as_str().map(str::to_string))
            .collect(),
        value.get("kind")?.as_str()?.to_string(),
    ))
}

/// Search scopes are used by the app's navigation payloads; re-exported for
/// convenience so callers do not need to depend on `models` directly.
pub fn scope_account(scope: &MailScope) -> Option<&str> {
    match scope {
        MailScope::Folder { .. } => None,
        MailScope::Unified { .. } => None,
        MailScope::Search { query } => query.account_id.as_deref(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::testing::RecordingEventSink;
    use serde_json::json;

    #[test]
    fn mail_changed_payload_shape() {
        let sink = RecordingEventSink::new();
        emit_mail_changed(
            &sink,
            "a1",
            vec!["f1".into()],
            vec!["m1".into()],
            MailChangeKind::New,
        );
        let (event, payload) = sink.last().expect("event");
        assert_eq!(event, MAIL_CHANGED);
        assert_eq!(payload["accountId"], "a1");
        assert_eq!(payload["folderIds"], json!(["f1"]));
        assert_eq!(payload["messageIds"], json!(["m1"]));
        assert_eq!(payload["kind"], "new");
    }

    #[test]
    fn mail_new_and_folders_helpers() {
        let sink = RecordingEventSink::new();
        emit_mail_new(&sink, "a1", "f1", vec!["m1".into()]);
        emit_folders_changed(&sink, "a1", vec!["f2".into()]);
        let events = sink.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].1["kind"], "new");
        assert_eq!(events[1].1["kind"], "folders");
    }

    #[test]
    fn navigate_and_reminder_payloads() {
        let sink = RecordingEventSink::new();
        emit_navigate(
            &sink,
            &NavigateTarget::mail("m1", Some("a".into()), Some("f".into())),
        );
        let (event, payload) = sink.last().expect("navigate");
        assert_eq!(event, NAVIGATE);
        assert_eq!(payload["module"], "mail");

        emit_reminder_due(
            &sink,
            ReminderDue {
                kind: ReminderKind::Event,
                id: "e1".into(),
                instance_start: Some("2026-11-12T09:00:00Z".into()),
                title: "Standup".into(),
                start: Some("2026-11-12T09:00:00Z".into()),
                all_day: false,
                location: None,
            },
        );
        let (event, payload) = sink.last().expect("reminder");
        assert_eq!(event, REMINDER_DUE);
        assert_eq!(payload["kind"], "event");
        assert_eq!(payload["allDay"], false);
        assert_eq!(payload["location"], Value::Null);
    }

    #[test]
    fn open_requested_is_empty_object() {
        let sink = RecordingEventSink::new();
        emit_open_requested(&sink);
        assert_eq!(sink.last().expect("event").1, json!({}));
    }

    #[test]
    fn mail_change_round_trip_helper() {
        let sink = RecordingEventSink::new();
        emit_mail_new(&sink, "a", "f", vec!["m".into()]);
        let parsed = parse_mail_changed(&sink.last().expect("event").1).expect("parse");
        assert_eq!(parsed.0, "a");
        assert_eq!(parsed.1, vec!["f".to_string()]);
        assert_eq!(parsed.2, vec!["m".to_string()]);
        assert_eq!(parsed.3, "new");
    }
}
