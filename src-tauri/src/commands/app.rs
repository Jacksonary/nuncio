//! Application-level commands: info, storage, cache, windows, backup, quit.

use std::collections::HashMap;
use std::path::PathBuf;

use tauri::State;
use tracing::{debug, info, warn};

use nuncio_core::events;
use nuncio_core::models::{
    AddressBook, AppInfo, Calendar, Contact, Folder, Platform, StorageUsage, StoredEvent, Task,
    TaskList,
};
use nuncio_core::repo::pim_maps::PimMapRecord;
use nuncio_core::{repo, AppError};

use crate::backup::{self, BackupDocument, RestorePlan};
use crate::pending::{self, PendingOpen};
use crate::state::AppState;
use crate::windows;

#[tauri::command]
pub async fn app_info(state: State<'_, AppState>) -> nuncio_core::Result<AppInfo> {
    let app = &state.app;
    let info = app.package_info();
    let platform = if cfg!(target_os = "windows") {
        Platform::Windows
    } else if cfg!(target_os = "macos") {
        Platform::Macos
    } else {
        Platform::Linux
    };
    Ok(AppInfo {
        version: info.version.to_string(),
        platform,
        data_dir: state.ctx.paths.root().to_string_lossy().into_owned(),
        log_dir: state.ctx.paths.logs_dir().to_string_lossy().into_owned(),
        build_has_google_client: crate::build_info::has_google_client(),
        build_has_microsoft_client: crate::build_info::has_microsoft_client(),
    })
}

fn file_size(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

#[tauri::command]
pub async fn app_storage_usage(state: State<'_, AppState>) -> nuncio_core::Result<StorageUsage> {
    let paths = &state.ctx.paths;
    let db = paths.db_file();
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    let mut shm = db.as_os_str().to_owned();
    shm.push("-shm");
    let database_bytes = file_size(&db)
        + file_size(std::path::Path::new(&wal))
        + file_size(std::path::Path::new(&shm));
    Ok(StorageUsage {
        database_bytes,
        mime_cache_bytes: nuncio_core::paths::directory_size(&paths.mime_root()),
        attachment_bytes: nuncio_core::paths::directory_size(&paths.attachments_dir()),
    })
}

#[tauri::command]
pub async fn app_clear_cache(state: State<'_, AppState>) -> nuncio_core::Result<()> {
    let paths = state.ctx.paths.clone();
    // POP3 mailboxes are fetched once and the server copy may be gone, so their
    // stored bodies are the only local original. Everything else can be fetched
    // again over IMAP, so only those rows count as cache. `trim_cache` applies
    // the same rule; the manual button must not destroy what it protects.
    let accounts = state.ctx.db.read(repo::accounts::list).await?;
    let permanent_sql = nuncio_core::repo::messages::permanent_body_accounts_sql(&accounts);

    // Collect the files first, then clear the rows in one statement. Doing it in
    // this order means a failure part-way through leaves the database pointing
    // at files that still exist, which is the harmless direction.
    // Draft projections (`staging/<draft>/draft.eml`) are the only copy of an
    // unsent message until its APPEND runs, so they are never cache.
    // Paths are stored with the platform separator; on Windows that is `\\`, so
    // match on the directory itself rather than a hard-coded `/` prefix.
    let staging_like = format!(
        "{}%",
        paths
            .staging_dir()
            .display()
            .to_string()
            .replace('\'', "''")
    );
    let body_sql = format!(
        "SELECT body_path FROM message
         WHERE body_path IS NOT NULL
           AND account_id NOT IN ({permanent_sql})
           AND body_path NOT LIKE '{staging_like}'"
    );
    let body_files: Vec<String> = state
        .ctx
        .db
        .read(move |conn| {
            let mut stmt = conn.prepare(&body_sql)?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut paths = Vec::new();
            for row in rows {
                paths.push(row?);
            }
            Ok(paths)
        })
        .await?;

    let attachment_sql = format!(
        "SELECT DISTINCT a.path FROM attachment a
         JOIN message m ON m.id = a.message_id
         WHERE a.path IS NOT NULL AND m.account_id NOT IN ({permanent_sql})"
    );
    let attachment_files: Vec<String> = state
        .ctx
        .db
        .read(move |conn| {
            let mut stmt = conn.prepare(&attachment_sql)?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut paths = Vec::new();
            for row in rows {
                paths.push(row?);
            }
            Ok(paths)
        })
        .await?;

    let update_sql = format!(
        "UPDATE message SET body_path = NULL, body_cached_at = NULL
         WHERE body_path IS NOT NULL
           AND account_id NOT IN ({permanent_sql})
           AND body_path NOT LIKE '{staging_like}';
         UPDATE attachment SET path = NULL, is_downloaded = 0
         WHERE path IS NOT NULL AND message_id IN (
             SELECT m.id FROM message m WHERE m.account_id NOT IN ({permanent_sql})
         );"
    );
    state
        .ctx
        .db
        .write(move |conn| {
            conn.execute_batch(&update_sql)?;
            Ok(())
        })
        .await?;

    for path in body_files.into_iter().chain(attachment_files) {
        if let Err(error) = tokio::fs::remove_file(&path).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                debug!(path = %path, %error, "cannot remove cached file");
            }
        }
    }

    // Drop the directories that just became empty. Roots are kept and recreated
    // by `ensure_dirs` below.
    prune_empty_dirs(&paths.mime_root()).await;
    prune_empty_dirs(&paths.attachments_dir()).await;
    if let Err(error) = tokio::fs::remove_dir_all(paths.tmp_dir()).await {
        if error.kind() != std::io::ErrorKind::NotFound {
            warn!(path = %paths.tmp_dir().display(), %error, "cannot clear the temporary folder");
        }
    }
    paths.ensure_dirs()?;

    for account in accounts {
        events::emit_mail_changed(
            state.ctx.events.as_ref(),
            account.id,
            Vec::new(),
            Vec::new(),
            events::MailChangeKind::Updated,
        );
    }
    info!("caches cleared (POP3 originals kept)");
    Ok(())
}

/// Remove empty directories under `root`, keeping `root` itself.
async fn prune_empty_dirs(root: &std::path::Path) {
    let mut stack = vec![root.to_path_buf()];
    let mut dirs = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry
                .file_type()
                .await
                .map(|kind| kind.is_dir())
                .unwrap_or(false)
            {
                stack.push(entry.path());
            }
        }
        dirs.push(dir);
    }
    // Deepest first so parents can become empty.
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
    for dir in dirs {
        if dir == root {
            continue;
        }
        let _ = tokio::fs::remove_dir(&dir).await;
    }
}

#[tauri::command]
pub async fn app_open_window(
    state: State<'_, AppState>,
    route: String,
    title: String,
) -> nuncio_core::Result<()> {
    // Async so the command does not run on the main thread: creating a
    // webview from a synchronous Windows command deadlocks.
    windows::open_route_window(&state.app, &route, &title)
}

#[tauri::command]
pub fn app_take_pending_open(state: State<'_, AppState>) -> Vec<PendingOpen> {
    pending::take(&state.app)
}

#[tauri::command]
pub async fn app_open_log_dir(state: State<'_, AppState>) -> nuncio_core::Result<()> {
    use tauri_plugin_opener::OpenerExt;

    let dir = state.ctx.paths.logs_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|error| AppError::io(format!("cannot create the log directory: {error}")))?;
    if let Err(error) = state.app.opener().reveal_item_in_dir(&dir) {
        debug!(%error, "cannot reveal the log directory, opening it instead");
        state
            .app
            .opener()
            .open_path(dir.to_string_lossy().into_owned(), None::<String>)
            .map_err(|error| AppError::io(format!("cannot open the log directory: {error}")))?;
    }
    Ok(())
}

/// PIM data that travels in the same encrypted document as the accounts.
struct BackupSections {
    address_books: Vec<AddressBook>,
    contact_groups: Vec<backup::ContactGroup>,
    contacts: Vec<Contact>,
    calendars: Vec<Calendar>,
    events: Vec<backup::BackupEvent>,
    task_lists: Vec<TaskList>,
    tasks: Vec<Task>,
    folders: Vec<Folder>,
    pim_maps: Vec<backup::PimMap>,
}

fn collect_backup_sections(conn: &rusqlite::Connection) -> nuncio_core::Result<BackupSections> {
    Ok(BackupSections {
        address_books: repo::address_books::list(conn, None)?,
        contact_groups: repo::contacts::list_groups_all(conn)?
            .into_iter()
            .map(Into::into)
            .collect(),
        contacts: repo::contacts::list_all(conn)?,
        calendars: repo::calendars::list(conn)?,
        events: repo::events::list_all(conn)?
            .into_iter()
            .map(|stored| backup::BackupEvent {
                event: stored.event,
                rrule: stored.rrule,
                exdates: stored.exdates,
            })
            .collect(),
        task_lists: repo::task_lists::list(conn)?,
        tasks: repo::tasks::list_all(conn)?,
        // `folder_preferences` zeroes the counters and the sync cursor: they
        // describe this device's cache, not the user's preferences.
        folders: repo::folders::list_all(conn)?
            .iter()
            .map(backup::folder_preferences)
            .collect(),
        pim_maps: repo::pim_maps::list_all(conn)?
            .into_iter()
            .map(Into::into)
            .collect(),
    })
}

#[tauri::command]
pub async fn app_backup_export(
    state: State<'_, AppState>,
    path: String,
    password: String,
    include_credentials: bool,
) -> nuncio_core::Result<()> {
    if password.trim().is_empty() {
        return Err(AppError::invalid_input("the backup password is required"));
    }
    let ctx = &state.ctx;
    let mut settings = ctx.settings.get().await;
    // The OAuth client secret lives in the keyring and is never part of the
    // settings table (nor of the exported document).
    settings.advanced.google_client_secret = None;
    let accounts = ctx.db.read(repo::accounts::list).await?;
    let identities = ctx
        .db
        .read(|conn| repo::identities::list(conn, None))
        .await?;
    let signatures = ctx.db.read(repo::signatures::list).await?;
    let rules = ctx.db.read(repo::rules::list).await?;
    let sections = ctx.db.read(collect_backup_sections).await?;

    let secrets = if include_credentials {
        let ids: Vec<String> = accounts.iter().map(|account| account.id.clone()).collect();
        Some(state.auth.export_account_secrets(&ids)?)
    } else {
        None
    };

    let mut document = BackupDocument::new(settings, accounts, identities, signatures, rules);
    document.address_books = sections.address_books;
    document.contact_groups = sections.contact_groups;
    document.contacts = sections.contacts;
    document.calendars = sections.calendars;
    document.events = sections.events;
    document.task_lists = sections.task_lists;
    document.tasks = sections.tasks;
    document.folders = sections.folders;
    document.pim_maps = sections.pim_maps;
    document.secrets = secrets;
    let bytes = backup::encrypt(&document, &password)?;

    let target = PathBuf::from(path);
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                AppError::io(format!("cannot create the backup folder: {error}"))
            })?;
        }
    }
    tokio::fs::write(&target, bytes)
        .await
        .map_err(|error| AppError::io(format!("cannot write the backup: {error}")))?;
    info!(path = %target.display(), "backup exported");
    Ok(())
}

#[tauri::command]
pub async fn app_backup_import(
    state: State<'_, AppState>,
    path: String,
    password: String,
) -> nuncio_core::Result<()> {
    // A backup is an encrypted JSON document of settings and accounts; anything
    // in the hundreds of megabytes is not one, and reading it whole would spike
    // memory for no benefit.
    const MAX_BACKUP_BYTES: u64 = 64 * 1024 * 1024;
    let size = tokio::fs::metadata(&path)
        .await
        .map_err(|error| AppError::io(format!("cannot read the backup: {error}")))?
        .len();
    if size > MAX_BACKUP_BYTES {
        return Err(AppError::invalid_input(
            "the backup file is larger than 64 MB; that is not a Nuncio backup",
        ));
    }
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|error| AppError::io(format!("cannot read the backup: {error}")))?;
    if bytes.len() as u64 > MAX_BACKUP_BYTES {
        return Err(AppError::invalid_input(
            "the backup file is larger than 64 MB; that is not a Nuncio backup",
        ));
    }
    let document = backup::decrypt(&bytes, &password)?;
    document.validate()?;

    let ctx = &state.ctx;
    let local_id = nuncio_core::models::LOCAL_ACCOUNT_ID.to_string();
    // Snapshot the accounts that are running right now; after the transaction
    // this table holds the restored set instead.
    let previous_accounts = ctx.db.read(repo::accounts::list).await?;

    // Decide once which PIM rows can land: the owning account must be part of
    // the restored set, and the parent row (address book / calendar / task
    // list) must survive too. Orphans from a corrupt file are simply dropped.
    let restored_accounts: std::collections::HashSet<String> = document
        .accounts
        .iter()
        .map(|account| account.id.clone())
        .chain(std::iter::once(local_id.clone()))
        .collect();
    let plan = RestorePlan::from_document(&document, &restored_accounts);

    // A backup may carry a legacy plaintext advanced Google client secret;
    // strip it before the settings row is written and hand it to the keyring
    // afterwards, so the settings table never stores a secret.
    let mut imported_settings = document.settings;
    let imported_secret = imported_settings.advanced.google_client_secret.take();

    // Everything that touches the database happens in one transaction: the old
    // configuration is only replaced if every row lands. Restoring half of a
    // backup after deleting the previous accounts would leave the user with
    // neither their old setup nor the new one.
    let accounts = document.accounts.clone();
    let identities = document.identities.clone();
    let signatures = document.signatures.clone();
    let rules = document.rules.clone();
    let local_id_for_tx = local_id.clone();
    let settings_for_tx = imported_settings.clone();
    let account_ids: Vec<String> = ctx
        .db
        .write(move |conn| {
            // `accounts::delete` cascades to folders, messages, drafts, outbox
            // rows and pending operations, so this is a real replacement.
            for account in repo::accounts::list(conn)? {
                if account.id == local_id_for_tx {
                    continue;
                }
                repo::accounts::delete(conn, &account.id)?;
            }
            let mut ids: Vec<String> = Vec::new();
            for account in &accounts {
                if account.id == local_id_for_tx {
                    ids.push(local_id_for_tx.clone());
                    continue;
                }
                repo::accounts::upsert(conn, account)?;
                ids.push(account.id.clone());
            }
            let known: std::collections::HashSet<&String> = ids.iter().collect();
            for identity in &identities {
                if !known.contains(&identity.account_id) {
                    continue;
                }
                let mut identity = identity.clone();
                repo::identities::upsert(conn, &mut identity)?;
            }
            for signature in &signatures {
                if let Some(account_id) = &signature.account_id {
                    if !known.contains(account_id) {
                        continue;
                    }
                }
                let mut signature = signature.clone();
                signature.html = nuncio_mime::sanitize_fragment(&signature.html);
                repo::signatures::upsert(conn, &mut signature)?;
            }
            for rule in &rules {
                if let Some(account_id) = &rule.account_id {
                    if !known.contains(account_id) {
                        continue;
                    }
                }
                repo::rules::upsert(conn, rule)?;
            }

            // Parent rows first, so the foreign keys and the child filters see
            // them. Ids are preserved because PIM mappings, folder preferences
            // and task/list links reference them.
            for book in &plan.address_books {
                let mut book = book.clone();
                repo::address_books::insert(conn, &mut book)?;
            }
            for group in &plan.contact_groups {
                let record = nuncio_core::repo::contacts::ContactGroupRecord::from(group.clone());
                repo::contacts::insert_group_record(conn, &record)?;
            }
            for contact in &plan.contacts {
                // `create` preserves the id and writes `contact_group_member`,
                // with group ids already filtered by `RestorePlan`.
                repo::contacts::create(conn, contact)?;
            }
            for calendar in &plan.calendars {
                let mut calendar = calendar.clone();
                repo::calendars::insert(conn, &mut calendar)?;
            }
            for backup in &plan.events {
                let stored = StoredEvent {
                    rrule: backup.rrule.clone(),
                    exdates: backup.exdates.clone(),
                    ..StoredEvent::new(backup.event.clone())
                };
                repo::events::upsert(conn, &stored)?;
            }
            for list in &plan.task_lists {
                repo::task_lists::insert(conn, list)?;
            }
            for task in &plan.tasks {
                repo::tasks::upsert_remote(conn, task, None, None)?;
            }
            for folder in &plan.folders {
                repo::folders::restore(conn, folder)?;
            }
            for map in &plan.pim_maps {
                let record = PimMapRecord::from(map.clone());
                repo::pim_maps::insert(conn, &record)?;
            }

            // Settings belong to the same commit: a restore that replaced the
            // accounts but failed to store its settings would leave a mixed
            // configuration behind.
            repo::settings::save(conn, &settings_for_tx)?;
            Ok(ids)
        })
        .await?;

    // The database now holds the restored configuration. From here on the work
    // is side-effectful and idempotent (cache, keyring, engines), so a failure
    // is reported but no longer destroys the previous state.
    //
    // Stop the engines of the accounts that existed *before* the swap: the list
    // is captured above, because after the transaction the table already holds
    // the restored accounts.
    for account in previous_accounts {
        if account.id == local_id {
            continue;
        }
        if let Err(error) = state.mail.on_account_removed(&account.id).await {
            debug!(account = %account.id, %error, "cannot stop mail for the account");
        }
        state.pim.on_account_removed(&account.id).await;
        state.auth.on_account_removed(&account.id);
        ctx.sync_status.remove_account(&account.id);
    }

    ctx.settings.adopt_persisted(imported_settings).await;
    // Same rule as the per-account secrets below: a keychain failure must not
    // abort a restore whose database work already committed.
    let advanced_secret_result = match imported_secret.as_deref().map(str::trim) {
        Some("********") | None => Ok(()),
        Some("") => state.auth.set_google_client_secret(None).await,
        Some(secret) => {
            state
                .auth
                .set_google_client_secret(Some(secret.to_string()))
                .await
        }
    };

    let known: std::collections::HashSet<&String> = account_ids.iter().collect();

    // Credentials live outside the database, so they cannot join the transaction.
    // They are applied before any engine starts: a failure here leaves an empty
    // account that the user can re-authenticate, instead of a half-restored
    // profile whose engines are already running with stale keys.
    let secrets_result = match document.secrets {
        Some(secrets) => {
            let filtered: HashMap<String, String> = secrets
                .into_iter()
                .filter(|(key, _)| {
                    key.strip_prefix("account:")
                        .and_then(|rest| rest.split(':').next())
                        .is_some_and(|id| known.contains(&id.to_string()))
                })
                .collect();
            state.auth.import_account_secrets(&filtered)
        }
        None => Ok(()),
    };

    for account in document.accounts.iter() {
        if account.id == local_id {
            continue;
        }
        state.auth.on_account_added(account);
        if let Err(error) = state.mail.on_account_added(&account.id).await {
            warn!(account = %account.id, %error, "cannot start mail after restore");
        }
        state.pim.on_account_added(&account.id).await;
    }

    let settings = ctx.settings.current();
    crate::commands::settings::apply_side_effects(&state, &settings);
    events::emit_accounts_changed(ctx.events.as_ref(), account_ids);
    info!("backup imported");
    // Report after the engines are running: the configuration itself is restored,
    // and the message tells the user exactly which part needs re-authenticating.
    advanced_secret_result.and(secrets_result).map_err(|error| {
        AppError::io(format!(
            "the configuration was restored, but its saved credentials could not be \
             written to the system keychain; re-authenticate the accounts affected: {error}"
        ))
    })
}

#[tauri::command]
pub async fn app_quit(state: State<'_, AppState>) -> nuncio_core::Result<()> {
    let app = state.app.clone();
    windows::shutdown_and_exit(&app).await;
    Ok(())
}
