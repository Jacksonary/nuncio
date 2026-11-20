//! Tauri command registry. Every function in `src/ipc/api.ts` has exactly one
//! entry here (the command macro uses the Rust function name).

pub mod accounts;
pub mod app;
pub mod calendar;
pub mod contacts;
pub mod mail;
pub mod settings;
pub mod sync;
pub mod tasks;

pub use accounts::*;
pub use app::*;
pub use calendar::*;
pub use contacts::*;
pub use mail::*;
pub use settings::*;
pub use sync::*;
pub use tasks::*;

/// The single `generate_handler!` list. Keep in sync with `src/ipc/api.ts`.
///
/// Full module paths are used because `#[tauri::command]` generates its helper
/// items in the module that defines the function (a re-export is not enough).
#[macro_export]
macro_rules! nuncio_commands {
    () => {
        tauri::generate_handler![
            // app
            $crate::commands::app::app_info,
            $crate::commands::app::app_storage_usage,
            $crate::commands::app::app_clear_cache,
            $crate::commands::app::app_open_window,
            $crate::commands::app::app_take_pending_open,
            $crate::commands::app::app_backup_export,
            $crate::commands::app::app_backup_import,
            $crate::commands::app::app_open_log_dir,
            $crate::commands::app::app_quit,
            // settings
            $crate::commands::settings::settings_get,
            $crate::commands::settings::settings_update,
            // accounts
            $crate::commands::accounts::account_list,
            $crate::commands::accounts::account_discover,
            $crate::commands::accounts::account_provider_info,
            $crate::commands::accounts::account_test,
            $crate::commands::accounts::account_create,
            $crate::commands::accounts::account_update,
            $crate::commands::accounts::account_update_password,
            $crate::commands::accounts::account_set_dav_password,
            $crate::commands::accounts::account_dav_password_status,
            $crate::commands::accounts::account_reauth,
            $crate::commands::accounts::account_delete,
            $crate::commands::accounts::account_reorder,
            $crate::commands::accounts::auth_oauth_login,
            $crate::commands::accounts::auth_oauth_cancel,
            $crate::commands::accounts::identity_list,
            $crate::commands::accounts::identity_upsert,
            $crate::commands::accounts::identity_delete,
            $crate::commands::accounts::signature_list,
            $crate::commands::accounts::signature_upsert,
            $crate::commands::accounts::signature_delete,
            // sync
            $crate::commands::sync::sync_now,
            $crate::commands::sync::sync_status,
            // mail
            $crate::commands::mail::folder_list,
            $crate::commands::mail::folder_unified_counts,
            $crate::commands::mail::folder_create,
            $crate::commands::mail::folder_rename,
            $crate::commands::mail::folder_move,
            $crate::commands::mail::folder_delete,
            $crate::commands::mail::folder_set_favorite,
            $crate::commands::mail::folder_set_sync_enabled,
            $crate::commands::mail::folder_mark_all_read,
            $crate::commands::mail::folder_empty,
            $crate::commands::mail::folder_sync,
            $crate::commands::mail::folder_load_older,
            $crate::commands::mail::message_list,
            $crate::commands::mail::message_thread,
            $crate::commands::mail::message_get,
            $crate::commands::mail::message_act,
            $crate::commands::mail::message_act_threads,
            $crate::commands::mail::message_undo_last,
            $crate::commands::mail::message_save_eml,
            $crate::commands::mail::message_open_eml,
            $crate::commands::mail::message_trust_sender,
            $crate::commands::mail::message_unsubscribe,
            $crate::commands::mail::attachment_open,
            $crate::commands::mail::attachment_save,
            $crate::commands::mail::attachment_save_all,
            $crate::commands::mail::attachment_is_dangerous,
            $crate::commands::mail::compose_create,
            $crate::commands::mail::compose_from_mailto,
            $crate::commands::mail::compose_reply,
            $crate::commands::mail::compose_open_draft,
            $crate::commands::mail::compose_get_draft,
            $crate::commands::mail::compose_save,
            $crate::commands::mail::compose_discard,
            $crate::commands::mail::compose_send,
            $crate::commands::mail::compose_add_attachments,
            $crate::commands::mail::compose_add_inline_image,
            $crate::commands::mail::compose_suggest_recipients,
            $crate::commands::mail::compose_flush_done,
            $crate::commands::mail::outbox_retry,
            $crate::commands::mail::outbox_cancel,
            $crate::commands::mail::rule_list,
            $crate::commands::mail::rule_upsert,
            $crate::commands::mail::rule_delete,
            $crate::commands::mail::rule_run_now,
            // calendar
            $crate::commands::calendar::calendar_list,
            $crate::commands::calendar::calendar_create,
            $crate::commands::calendar::calendar_update,
            $crate::commands::calendar::calendar_set_visible,
            $crate::commands::calendar::calendar_delete,
            $crate::commands::calendar::calendar_subscribe,
            $crate::commands::calendar::calendar_import_ics,
            $crate::commands::calendar::calendar_export_ics,
            $crate::commands::calendar::event_range,
            $crate::commands::calendar::event_get,
            $crate::commands::calendar::event_create,
            $crate::commands::calendar::event_update,
            $crate::commands::calendar::event_delete,
            $crate::commands::calendar::event_respond,
            $crate::commands::calendar::event_search,
            $crate::commands::calendar::invite_respond,
            $crate::commands::calendar::event_dismiss_reminder,
            $crate::commands::calendar::event_snooze_reminder,
            // contacts
            $crate::commands::contacts::addressbook_list,
            $crate::commands::contacts::contact_list,
            $crate::commands::contacts::contact_get,
            $crate::commands::contacts::contact_create,
            $crate::commands::contacts::contact_update,
            $crate::commands::contacts::contact_delete,
            $crate::commands::contacts::contact_set_favorite,
            $crate::commands::contacts::contact_find_by_email,
            $crate::commands::contacts::group_list,
            $crate::commands::contacts::group_create,
            $crate::commands::contacts::group_rename,
            $crate::commands::contacts::group_delete,
            $crate::commands::contacts::group_set_members,
            $crate::commands::contacts::contact_import_vcf,
            $crate::commands::contacts::contact_export_vcf,
            // tasks
            $crate::commands::tasks::tasklist_list,
            $crate::commands::tasks::tasklist_create,
            $crate::commands::tasks::tasklist_update,
            $crate::commands::tasks::tasklist_delete,
            $crate::commands::tasks::task_smart_counts,
            $crate::commands::tasks::task_query,
            $crate::commands::tasks::task_get,
            $crate::commands::tasks::task_create,
            $crate::commands::tasks::task_update,
            $crate::commands::tasks::task_set_completed,
            $crate::commands::tasks::task_set_importance,
            $crate::commands::tasks::task_set_my_day,
            $crate::commands::tasks::task_move,
            $crate::commands::tasks::task_reorder,
            $crate::commands::tasks::task_delete,
            $crate::commands::tasks::task_search,
            $crate::commands::tasks::task_dismiss_reminder,
            $crate::commands::tasks::task_snooze_reminder,
            $crate::commands::tasks::message_create_task,
        ]
    };
}
