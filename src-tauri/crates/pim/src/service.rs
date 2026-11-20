//! `PimService` — the command-facing PIM facade.
//!
//! It implements the calendar commands from `src/ipc/api.ts`, the optimistic
//! write queue (`pim.*` pending operations), per-account calendar sync loops,
//! webcal subscriptions and the reminder loop.  Contacts and tasks (stage 2)
//! plug into the same HTTP client, pending-op queue and reminder engine.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{Duration, Utc};
use nuncio_core::events::{emit_calendar_changed, emit_op_failed};
use nuncio_core::models::{
    new_id, Account, Address, Attendee, AttendeeRole, AttendeeStatus, Calendar, CalendarEvent,
    EventInput, EventInstance, EventStatus, NewPendingOp, PendingOp, PendingOpState,
    RecurrenceEditScope, StoredEvent, SyncModule,
};
use nuncio_core::providers::DavService;
use nuncio_core::repo::{self, events as events_repo, pending_ops, reminder_state};
use nuncio_core::traits::MailSender;
use nuncio_core::CoreContext;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::backend::{backend_for, is_implicit_dav_auth_failure, Backend};
use crate::contacts::ContactsService;
use crate::dav::{DavClient, DavKind, DavProvider};
use crate::error::{invalid, unsupported, AppError, Result};
use crate::google::GoogleProvider;
use crate::graph::GraphProvider;
use crate::http::HttpClient;
use crate::ops::{self, CalendarSnapshot, EventSnapshot, PimOp};
use crate::provider::{CalendarCursor, CalendarProvider, EventWindow, RemoteCalendar, RemoteEvent};
use crate::reminders::ReminderEngine;
use crate::status::record_failure;
use crate::store::{self, CalendarRemote};
use crate::tasks::{task_reminder_key, TaskReminderSource, TasksService};
use crate::util::{
    date_part, event_color, normalize_color, normalize_zone, parse_instant, same_instant,
    MAX_OCCURRENCES_PER_SERIES,
};
use crate::webcal;

struct Inner {
    ctx: CoreContext,
    http: HttpClient,
    mail_sender: Arc<dyn MailSender>,
    reminders: ReminderEngine,
    contacts_service: ContactsService,
    tasks_service: TasksService,
    shutdown: AtomicBool,
    started: AtomicBool,
    tasks: Mutex<TaskHandles>,
    sync_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    ops_notify: Arc<Notify>,
    wake: Arc<Notify>,
}

#[derive(Default)]
struct TaskHandles {
    accounts: HashMap<String, JoinHandle<()>>,
    contacts: HashMap<String, JoinHandle<()>>,
    tasks: HashMap<String, JoinHandle<()>>,
    op_worker: Option<JoinHandle<()>>,
    reminder_loop: Option<JoinHandle<()>>,
}

/// Cloneable PIM service.
#[derive(Clone)]
pub struct PimService {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for PimService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PimService").finish_non_exhaustive()
    }
}

impl PimService {
    /// Create the service, apply the `pim` migration namespace and build the
    /// provider HTTP client.
    pub fn new(ctx: CoreContext, mail_sender: Arc<dyn MailSender>) -> Result<Self> {
        ctx.migrate(store::PIM_NAMESPACE, store::PIM_MIGRATIONS)?;
        let http = HttpClient::new(ctx.creds.clone())?;
        let contacts = ContactsService::new(ctx.clone(), http.clone());
        let tasks = TasksService::new(ctx.clone(), http.clone());
        let reminders = ReminderEngine::new(ctx.clone());
        reminders.add_source(Arc::new(TaskReminderSource));
        Ok(Self {
            inner: Arc::new(Inner {
                ctx,
                http,
                mail_sender,
                reminders,
                contacts_service: contacts,
                tasks_service: tasks,
                shutdown: AtomicBool::new(false),
                started: AtomicBool::new(false),
                tasks: Mutex::new(TaskHandles::default()),
                sync_locks: Mutex::new(HashMap::new()),
                ops_notify: Arc::new(Notify::new()),
                wake: Arc::new(Notify::new()),
            }),
        })
    }

    pub fn context(&self) -> &CoreContext {
        &self.inner.ctx
    }

    /// Expose the reminder engine so stage 2 can register task sources.
    pub fn reminder_engine(&self) -> &ReminderEngine {
        &self.inner.reminders
    }

    /// Register an additional reminder source.
    pub fn add_reminder_source(&self, source: Arc<dyn crate::reminders::ReminderSource>) {
        self.inner.reminders.add_source(source);
    }

    /// Contacts sub-service (providers, sync, command facade).
    pub fn contacts(&self) -> &ContactsService {
        &self.inner.contacts_service
    }

    /// Tasks sub-service (providers, sync, command facade).
    pub fn tasks(&self) -> &TasksService {
        &self.inner.tasks_service
    }

    /* ───────────────────────── life-cycle ───────────────────────── */

    pub async fn start(&self) -> Result<()> {
        if self.inner.started.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.inner.shutdown.store(false, Ordering::SeqCst);
        if let Err(error) = self.compact_raw_ics_once().await {
            warn!(%error, "raw iCalendar compaction failed");
        }
        // Recover operations that were interrupted by a crash.
        let _ = self.inner.ctx.db.write_sync(|conn| {
            conn.execute(
                "UPDATE pending_op SET state = 'pending'
                 WHERE kind LIKE 'pim.%' AND state = 'inProgress'",
                [],
            )?;
            Ok(())
        });

        self.inner.contacts_service.start();
        self.inner.tasks_service.start();
        let reminder_engine = self.inner.reminders.clone();
        let reminder_handle = tokio::spawn(async move { reminder_engine.run().await });
        let worker = self.clone();
        let worker_handle = tokio::spawn(async move { worker.op_worker().await });

        {
            let mut tasks = self
                .inner
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            tasks.reminder_loop = Some(reminder_handle);
            tasks.op_worker = Some(worker_handle);
        }

        let accounts = self.inner.ctx.db.read(repo::accounts::list).await?;
        for account in accounts {
            if account.calendar_enabled {
                self.spawn_account_loop(account.id.clone());
            }
            if account.contacts_enabled {
                self.spawn_contacts_loop(account.id.clone());
            }
            if account.tasks_enabled {
                self.spawn_tasks_loop(account.id.clone());
            }
        }
        info!("nuncio-pim started");
        Ok(())
    }

    /// One-off data migration: older builds stored the complete webcal feed /
    /// imported .ics file on every master event (`raw_ics`), making storage and
    /// recurrence expansion O(events × file size).  Rewrite those rows to keep
    /// only the RDATE values the expander needs, and drop raw copies from
    /// overrides (their master carries the data).
    async fn compact_raw_ics_once(&self) -> Result<()> {
        let done = self
            .inner
            .ctx
            .db
            .read(|conn| {
                let value: Option<String> = conn
                    .query_row(
                        "SELECT value FROM pim_meta WHERE key = 'raw_ics_compacted'",
                        [],
                        |row| row.get(0),
                    )
                    .ok();
                Ok(value.is_some())
            })
            .await?;
        if done {
            return Ok(());
        }
        self.inner
            .ctx
            .db
            .write(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, uid, start_at, all_day, raw_ics, series_id, recurrence_id
                     FROM event WHERE raw_ics IS NOT NULL",
                )?;
                let mut rows = stmt.query([])?;
                let mut updates: Vec<(String, Option<String>)> = Vec::new();
                while let Some(row) = rows.next()? {
                    let id: String = row.get(0)?;
                    let uid: String = row.get(1)?;
                    let start: String = row.get(2)?;
                    let all_day: bool = row.get(3)?;
                    let raw: String = row.get(4)?;
                    let is_override: Option<String> = row.get(5)?;
                    let recurrence_id: Option<String> = row.get(6)?;
                    if is_override.is_some() || recurrence_id.is_some() {
                        updates.push((id, None));
                        continue;
                    }
                    let rdates = nuncio_ical::ics::parse_calendar(&raw)
                        .ok()
                        .and_then(|calendar| {
                            calendar
                                .events
                                .into_iter()
                                .find(|event| {
                                    event.uid == uid && event.recurrence_id.is_none()
                                })
                                .map(|event| event.rdates)
                        })
                        .unwrap_or_default();
                    updates.push((id, webcal::rdates_only_ics(&uid, all_day, &start, &rdates)));
                }
                drop(rows);
                drop(stmt);
                for (id, raw) in updates {
                    conn.execute(
                        "UPDATE event SET raw_ics = ?2 WHERE id = ?1",
                        rusqlite::params![id, raw],
                    )?;
                }
                conn.execute(
                    "INSERT INTO pim_meta (key, value, updated_at) VALUES ('raw_ics_compacted', '1', ?1)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                    rusqlite::params![repo::now_string()],
                )?;
                Ok(())
            })
            .await
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        self.inner.wake.notify_waiters();
        self.inner.ops_notify.notify_waiters();
        self.inner.contacts_service.stop();
        self.inner.tasks_service.stop();
        let handles = {
            let mut tasks = self
                .inner
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut handles: Vec<JoinHandle<()>> = Vec::new();
            handles.extend(tasks.accounts.drain().map(|(_, handle)| handle));
            handles.extend(tasks.contacts.drain().map(|(_, handle)| handle));
            handles.extend(tasks.tasks.drain().map(|(_, handle)| handle));
            if let Some(handle) = tasks.op_worker.take() {
                handles.push(handle);
            }
            if let Some(handle) = tasks.reminder_loop.take() {
                handles.push(handle);
            }
            handles
        };
        for handle in handles {
            handle.abort();
        }
        self.inner.started.store(false, Ordering::SeqCst);
        info!("nuncio-pim stopped");
    }

    pub async fn on_account_added(&self, account_id: &str) {
        self.restart_account_loop(account_id).await;
    }

    pub async fn on_account_changed(&self, account_id: &str) {
        self.restart_account_loop(account_id).await;
    }

    pub async fn on_account_removed(&self, account_id: &str) {
        self.abort_account_loop(account_id);
        self.inner.ctx.sync_status.remove_account(account_id);
        let account = account_id.to_string();
        let _ = self
            .inner
            .ctx
            .db
            .write(move |conn| {
                // Only cancel PIM operations; mail ops share the same table.
                conn.execute(
                    "UPDATE pending_op SET state = 'cancelled'
                     WHERE account_id = ?1 AND kind LIKE 'pim.%'
                       AND state IN ('pending', 'inProgress')",
                    rusqlite::params![account],
                )?;
                conn.execute(
                    "DELETE FROM pim_address_book_map WHERE account_id = ?1",
                    rusqlite::params![account],
                )?;
                conn.execute(
                    "DELETE FROM pim_task_list_map WHERE account_id = ?1",
                    rusqlite::params![account],
                )?;
                store::prune_reminder_fired(conn)?;
                Ok(())
            })
            .await;
    }

    async fn restart_account_loop(&self, account_id: &str) {
        self.abort_account_loop(account_id);
        let account = match self
            .inner
            .ctx
            .db
            .read({
                let id = account_id.to_string();
                move |conn| repo::accounts::get(conn, &id)
            })
            .await
        {
            Ok(Some(account)) => account,
            Ok(None) => return,
            Err(error) => {
                warn!(account_id, %error, "cannot load account for PIM service");
                return;
            }
        };
        if account.calendar_enabled {
            self.spawn_account_loop(account.id.clone());
        }
        if account.contacts_enabled {
            self.spawn_contacts_loop(account.id.clone());
        }
        if account.tasks_enabled {
            self.spawn_tasks_loop(account.id.clone());
        }
        self.inner.wake.notify_waiters();
    }

    fn abort_account_loop(&self, account_id: &str) {
        let mut tasks = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(handle) = tasks.accounts.remove(account_id) {
            handle.abort();
        }
        if let Some(handle) = tasks.contacts.remove(account_id) {
            handle.abort();
        }
        if let Some(handle) = tasks.tasks.remove(account_id) {
            handle.abort();
        }
    }

    fn spawn_account_loop(&self, account_id: String) {
        if !self.inner.started.load(Ordering::SeqCst) {
            return;
        }
        let service = self.clone();
        let id = account_id.clone();
        let handle = tokio::spawn(async move { service.account_loop(id).await });
        let mut tasks = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(old) = tasks.accounts.insert(account_id, handle) {
            old.abort();
        }
    }

    async fn account_loop(&self, account_id: String) {
        loop {
            if self.inner.shutdown.load(Ordering::SeqCst) {
                break;
            }
            if let Err(error) = self.sync_calendar_account(&account_id, false).await {
                // An account whose backend cannot serve calendars at all (a
                // plain IMAP mailbox with no CalDAV URL, for instance) would
                // otherwise log the same "provider does not support this
                // operation" every poll for the lifetime of the process.
                if !matches!(error, AppError::Unsupported(_)) {
                    debug!(account_id = %account_id, %error, "periodic calendar sync failed");
                }
            }
            let minutes = self
                .inner
                .ctx
                .settings
                .current()
                .calendar
                .poll_minutes
                .clamp(1, 24 * 60);
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(u64::from(minutes) * 60)) => {}
                _ = self.inner.wake.notified() => {}
            }
        }
    }

    fn spawn_contacts_loop(&self, account_id: String) {
        if !self.inner.started.load(Ordering::SeqCst) {
            return;
        }
        let service = self.clone();
        let id = account_id.clone();
        let handle = tokio::spawn(async move { service.contacts_loop(id).await });
        let mut tasks = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(old) = tasks.contacts.insert(account_id, handle) {
            old.abort();
        }
    }

    async fn contacts_loop(&self, account_id: String) {
        loop {
            if self.inner.shutdown.load(Ordering::SeqCst) {
                break;
            }
            if let Err(error) = self
                .inner
                .contacts_service
                .sync_account(&account_id, false)
                .await
            {
                debug!(account_id = %account_id, %error, "periodic contact sync failed");
            }
            let minutes = self
                .inner
                .ctx
                .settings
                .current()
                .contacts
                .poll_minutes
                .clamp(1, 24 * 60);
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(u64::from(minutes) * 60)) => {}
                _ = self.inner.wake.notified() => {}
            }
        }
    }

    fn spawn_tasks_loop(&self, account_id: String) {
        if !self.inner.started.load(Ordering::SeqCst) {
            return;
        }
        let service = self.clone();
        let id = account_id.clone();
        let handle = tokio::spawn(async move { service.tasks_loop(id).await });
        let mut tasks = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(old) = tasks.tasks.insert(account_id, handle) {
            old.abort();
        }
    }

    async fn tasks_loop(&self, account_id: String) {
        loop {
            if self.inner.shutdown.load(Ordering::SeqCst) {
                break;
            }
            if let Err(error) = self
                .inner
                .tasks_service
                .sync_account(&account_id, false)
                .await
            {
                debug!(account_id = %account_id, %error, "periodic task sync failed");
            }
            let minutes = self
                .inner
                .ctx
                .settings
                .current()
                .tasks
                .poll_minutes
                .clamp(1, 24 * 60);
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(u64::from(minutes) * 60)) => {}
                _ = self.inner.wake.notified() => {}
            }
        }
    }

    /// Sync now.  `module = None` means every module of every enabled account.
    pub async fn sync_now(
        &self,
        account_id: Option<&str>,
        module: Option<SyncModule>,
    ) -> Result<()> {
        let accounts = self.inner.ctx.db.read(repo::accounts::list).await?;
        let mut first_error = None;
        for account in accounts {
            if let Some(id) = account_id {
                if account.id != id {
                    continue;
                }
            }
            let wants = |wanted: SyncModule| module.is_none_or(|value| value == wanted);
            if wants(SyncModule::Calendar) && account.calendar_enabled {
                if let Err(error) = self.sync_calendar_account(&account.id, true).await {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
            if wants(SyncModule::Contacts) && account.contacts_enabled {
                if let Err(error) = self
                    .inner
                    .contacts_service
                    .sync_account(&account.id, true)
                    .await
                {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
            if wants(SyncModule::Tasks) && account.tasks_enabled {
                if let Err(error) = self
                    .inner
                    .tasks_service
                    .sync_account(&account.id, true)
                    .await
                {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /* ───────────────────────── provider plumbing ───────────────────── */

    fn provider_for(&self, account: &Account) -> Option<Box<dyn CalendarProvider>> {
        let default_reminder = self
            .inner
            .ctx
            .settings
            .current()
            .calendar
            .default_reminder_minutes as i32;
        match backend_for(account, DavService::Calendar) {
            Backend::Google => Some(Box::new(GoogleProvider::new(
                self.inner.http.clone(),
                default_reminder,
            ))),
            Backend::Graph => Some(Box::new(GraphProvider::new(
                self.inner.http.clone(),
                default_reminder,
            ))),
            Backend::Dav => Some(Box::new(DavProvider::new(self.inner.http.clone()))),
            Backend::Local | Backend::Unsupported => None,
        }
    }

    fn is_provider(&self, account: &Account) -> bool {
        matches!(
            backend_for(account, DavService::Calendar),
            Backend::Google | Backend::Graph | Backend::Dav
        )
    }

    fn mapping_for(&self, calendar_id: &str) -> Result<Option<CalendarRemote>> {
        self.inner
            .ctx
            .db
            .read_sync(|conn| store::get_by_calendar(conn, calendar_id))
    }

    fn remote_for(&self, calendar: &Calendar, mapping: &CalendarRemote) -> RemoteCalendar {
        RemoteCalendar {
            local_id: calendar.id.clone(),
            remote_id: mapping.remote_id.clone(),
            href: mapping.href.clone(),
            name: calendar.name.clone(),
            color: Some(calendar.color.clone()),
            read_only: calendar.is_read_only,
            sync_token: None,
            ctag: None,
            supports_sync_collection: true,
        }
    }

    /* ───────────────────────────── sync ───────────────────────────── */

    async fn sync_calendar_account(&self, account_id: &str, force_webcal: bool) -> Result<()> {
        let lock = {
            let mut locks = self
                .inner
                .sync_locks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            locks
                .entry(account_id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;

        let account = self
            .inner
            .ctx
            .db
            .read({
                let id = account_id.to_string();
                move |conn| repo::accounts::get_required(conn, &id)
            })
            .await?;
        if !account.calendar_enabled {
            return Ok(());
        }

        self.inner
            .ctx
            .sync_status
            .begin(&account.id, SyncModule::Calendar, None);
        let result = match backend_for(&account, DavService::Calendar) {
            Backend::Local => self.sync_webcal_calendars(&account, force_webcal).await,
            Backend::Unsupported => Ok(()),
            Backend::Google | Backend::Graph | Backend::Dav => {
                self.sync_provider_calendars(&account).await
            }
        };

        match result {
            Ok(()) => {
                self.inner
                    .ctx
                    .sync_status
                    .finish(&account.id, SyncModule::Calendar);
                Ok(())
            }
            Err(error) => {
                // "This account has no calendar backend" is not a sync failure.
                // `backend_for` already returns Unsupported for most of these,
                // but the DAV layer can still surface one (a provider without a
                // directory URL, a server with no calendar collections).
                // Reporting that as an error kept the sidebar on "sync error"
                // forever for an account that will never have calendars.
                if matches!(error, AppError::Unsupported(_)) {
                    self.inner
                        .ctx
                        .sync_status
                        .finish(&account.id, SyncModule::Calendar);
                    return Ok(());
                }
                // An implicit DAV endpoint (directory URL + reused mail
                // password) may reject that password; that is not a sign-in
                // problem and must not turn the whole account red.
                if matches!(error, AppError::Auth(_))
                    && backend_for(&account, DavService::Calendar) == Backend::Dav
                {
                    let dedicated = self
                        .inner
                        .ctx
                        .creds
                        .has_dav_password(&account.id, DavService::Calendar)
                        .await;
                    if is_implicit_dav_auth_failure(
                        &account,
                        DavService::Calendar,
                        dedicated,
                        &error,
                    ) {
                        debug!(
                            account_id = %account.id,
                            "calendar needs its own password for this provider; set it in account settings"
                        );
                        self.inner
                            .ctx
                            .sync_status
                            .finish(&account.id, SyncModule::Calendar);
                        return Ok(());
                    }
                }
                record_failure(
                    &self.inner.ctx.sync_status,
                    &account.id,
                    SyncModule::Calendar,
                    &error,
                );
                Err(error)
            }
        }
    }

    async fn sync_provider_calendars(&self, account: &Account) -> Result<()> {
        let Some(provider) = self.provider_for(account) else {
            return Ok(());
        };
        let remotes = provider.list_calendars(account).await?;
        let (pairs, calendars_changed) = self
            .upsert_remote_calendars(account, provider.name(), remotes)
            .await?;
        let mut changed: Vec<String> = Vec::new();
        if calendars_changed {
            changed.extend(pairs.iter().map(|(calendar, _)| calendar.id.clone()));
        }
        let mut first_error: Option<AppError> = None;
        for (calendar, remote) in &pairs {
            match self
                .sync_one_calendar(account, provider.as_ref(), calendar, remote)
                .await
            {
                Ok(true) => changed.push(calendar.id.clone()),
                Ok(false) => {}
                Err(error) => {
                    warn!(calendar = %calendar.id, %error, "calendar sync failed");
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        if !changed.is_empty() {
            changed.sort();
            changed.dedup();
            emit_calendar_changed(self.inner.ctx.events.as_ref(), changed);
            self.inner.reminders.notify_changed();
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn upsert_remote_calendars(
        &self,
        account: &Account,
        provider_name: &str,
        remotes: Vec<RemoteCalendar>,
    ) -> Result<(Vec<(Calendar, RemoteCalendar)>, bool)> {
        let account_id = account.id.clone();
        let provider_name = provider_name.to_string();
        self.inner
            .ctx
            .db
            .write(move |conn| {
                let existing = store::list_for_account(conn, &account_id)?;
                let provider_maps: Vec<CalendarRemote> = existing
                    .iter()
                    .filter(|mapping| mapping.provider == provider_name)
                    .cloned()
                    .collect();
                let remote_ids: HashSet<String> = remotes
                    .iter()
                    .map(|remote| remote.remote_id.clone())
                    .collect();
                let mut changed = false;
                let mut pairs = Vec::new();

                for mut remote in remotes {
                    if let Some(mapping) =
                        store::get_by_remote(conn, &account_id, &provider_name, &remote.remote_id)?
                    {
                        match repo::calendars::get(conn, &mapping.calendar_id)? {
                            Some(mut calendar) => {
                                let name = if remote.name.trim().is_empty() {
                                    calendar.name.clone()
                                } else {
                                    remote.name.clone()
                                };
                                let color = remote
                                    .color
                                    .as_deref()
                                    .and_then(normalize_color)
                                    .unwrap_or_else(|| calendar.color.clone());
                                if calendar.name != name || calendar.color != color {
                                    calendar = repo::calendars::update(
                                        conn,
                                        &calendar.id,
                                        Some(&name),
                                        Some(&color),
                                    )?;
                                    changed = true;
                                }
                                if calendar.is_read_only != remote.read_only {
                                    repo::calendars::set_read_only(
                                        conn,
                                        &calendar.id,
                                        remote.read_only,
                                    )?;
                                    calendar.is_read_only = remote.read_only;
                                    changed = true;
                                }
                                if mapping.href != remote.href {
                                    let updated = CalendarRemote {
                                        href: remote.href.clone(),
                                        ..mapping.clone()
                                    };
                                    store::set(conn, &updated)?;
                                }
                                remote.local_id = calendar.id.clone();
                                pairs.push((calendar, remote));
                            }
                            None => {
                                store::delete(conn, &mapping.calendar_id)?;
                                let calendar = create_remote_calendar(conn, &account_id, &remote)?;
                                store::set(
                                    conn,
                                    &CalendarRemote::new(
                                        calendar.id.clone(),
                                        &account_id,
                                        &provider_name,
                                        &remote.remote_id,
                                        remote.href.clone(),
                                    ),
                                )?;
                                remote.local_id = calendar.id.clone();
                                changed = true;
                                pairs.push((calendar, remote));
                            }
                        }
                    } else {
                        let calendar = create_remote_calendar(conn, &account_id, &remote)?;
                        store::set(
                            conn,
                            &CalendarRemote::new(
                                calendar.id.clone(),
                                &account_id,
                                &provider_name,
                                &remote.remote_id,
                                remote.href.clone(),
                            ),
                        )?;
                        remote.local_id = calendar.id.clone();
                        changed = true;
                        pairs.push((calendar, remote));
                    }
                }

                // Remote calendars deleted on the server disappear locally.
                for mapping in provider_maps {
                    if !remote_ids.contains(&mapping.remote_id) {
                        if let Err(error) = repo::calendars::delete(conn, &mapping.calendar_id) {
                            if !error.is_not_found() {
                                return Err(error);
                            }
                        }
                        changed = true;
                    }
                }
                Ok((pairs, changed))
            })
            .await
    }

    async fn sync_one_calendar(
        &self,
        account: &Account,
        provider: &dyn CalendarProvider,
        calendar: &Calendar,
        remote: &RemoteCalendar,
    ) -> Result<bool> {
        let calendar_id = calendar.id.clone();
        let provider_name = provider.name().to_string();
        let (ctag, sync_token, etags) = self
            .inner
            .ctx
            .db
            .read(move |conn| {
                let (ctag, sync_token) = repo::calendars::sync_markers(conn, &calendar_id)?;
                let mut etags = Vec::new();
                if provider_name == "caldav" {
                    for stored in events_repo::list_for_calendar(conn, &calendar_id)? {
                        if let (Some(href), Some(etag)) = (stored.href, stored.etag) {
                            etags.push((href, etag));
                        }
                    }
                }
                Ok((ctag, sync_token, etags))
            })
            .await?;

        let cursor = CalendarCursor {
            local_calendar_id: calendar.id.clone(),
            sync_token,
            ctag,
            etags,
        };
        let window = EventWindow::default_window(Utc::now());
        let delta = provider
            .sync_events(account, remote, &cursor, window)
            .await?;
        let changed = delta.full_sync
            || !delta.events.is_empty()
            || !delta.deleted_remote_ids.is_empty()
            || !delta.deleted_hrefs.is_empty();

        self.apply_event_delta(calendar, delta.clone()).await?;

        let next_ctag = delta.ctag.clone().or(cursor.ctag);
        let next_token = delta.sync_token.clone().or(cursor.sync_token);
        let calendar_id = calendar.id.clone();
        self.inner
            .ctx
            .db
            .write(move |conn| {
                repo::calendars::set_sync_markers(
                    conn,
                    &calendar_id,
                    next_ctag.as_deref(),
                    next_token.as_deref(),
                )
            })
            .await?;
        Ok(changed)
    }

    async fn apply_event_delta(
        &self,
        calendar: &Calendar,
        delta: crate::provider::EventDelta,
    ) -> Result<()> {
        let calendar_id = calendar.id.clone();
        let account_id = calendar.account_id.clone();
        self.inner
            .ctx
            .db
            .write(move |conn| {
                for remote_id in &delta.deleted_remote_ids {
                    if let Some(stored) = events_repo::by_remote_id(conn, &calendar_id, remote_id)?
                    {
                        delete_stored(conn, &stored)?;
                        continue;
                    }
                    let all = events_repo::list_for_calendar(conn, &calendar_id)?;
                    for stored in all {
                        if stored.event.uid == *remote_id {
                            delete_stored(conn, &stored)?;
                        }
                    }
                }
                for href in &delta.deleted_hrefs {
                    let all = events_repo::list_for_calendar(conn, &calendar_id)?;
                    for stored in all {
                        if stored.href.as_deref() == Some(href.as_str()) {
                            delete_stored(conn, &stored)?;
                        }
                    }
                }

                let mut ordered = delta.events.clone();
                // Masters before detached exceptions so series ids resolve.
                ordered.sort_by_key(|remote| remote.series_remote_id.is_some());
                let mut remote_to_local: HashMap<String, String> = HashMap::new();
                let mut seen_remote: HashSet<String> = HashSet::new();
                let mut seen_by_href: HashMap<String, HashSet<String>> = HashMap::new();

                for remote in ordered {
                    if let (Some(href), Some(remote_id)) = (&remote.href, &remote.remote_id) {
                        seen_by_href
                            .entry(href.clone())
                            .or_default()
                            .insert(remote_id.clone());
                    }
                    if let Some(remote_id) = &remote.remote_id {
                        seen_remote.insert(remote_id.clone());
                    }
                    let existing = find_existing_event(conn, &calendar_id, &remote)?;
                    let id = existing
                        .as_ref()
                        .map(|stored| stored.event.id.clone())
                        .unwrap_or_else(new_id);
                    let mut series_id = existing
                        .as_ref()
                        .and_then(|stored| stored.event.series_id.clone());
                    if remote.event.recurrence_id.is_some() {
                        if let Some(series_remote) = &remote.series_remote_id {
                            if let Some(local) = remote_to_local.get(series_remote) {
                                series_id = Some(local.clone());
                            } else if let Some(master) =
                                events_repo::by_remote_id(conn, &calendar_id, series_remote)?
                            {
                                series_id = Some(master.event.id);
                            }
                        }
                        if series_id.is_none() {
                            let all = events_repo::list_for_calendar(conn, &calendar_id)?;
                            series_id = all
                                .into_iter()
                                .find(|stored| {
                                    stored.event.uid == remote.event.uid
                                        && stored.event.series_id.is_none()
                                })
                                .map(|stored| stored.event.id);
                        }
                    } else {
                        series_id = None;
                    }

                    let mut event = remote.event.clone();
                    event.id = id;
                    event.calendar_id = calendar_id.clone();
                    event.account_id = account_id.clone();
                    event.series_id = series_id;
                    let stored = StoredEvent {
                        event,
                        raw_ics: remote.raw_ics,
                        etag: remote.etag,
                        remote_id: remote.remote_id,
                        href: remote.href,
                        rrule: remote.rrule,
                        exdates: remote.exdates,
                    };
                    let saved = events_repo::upsert(conn, &stored)?;
                    if let Some(remote_id) = saved.remote_id.clone() {
                        remote_to_local.insert(remote_id, saved.event.id);
                    }
                }

                // A CalDAV resource may lose an override: drop local rows that
                // share the resource URL but were not in the new payload.
                for (href, seen_ids) in &seen_by_href {
                    let all = events_repo::list_for_calendar(conn, &calendar_id)?;
                    for stored in all {
                        if stored.href.as_deref() == Some(href.as_str()) {
                            if let Some(remote_id) = stored.remote_id.as_deref() {
                                if !seen_ids.contains(remote_id) {
                                    events_repo::delete(conn, &stored.event.id)?;
                                }
                            }
                        }
                    }
                }

                if delta.full_sync {
                    let all = events_repo::list_for_calendar(conn, &calendar_id)?;
                    for stored in all {
                        if let Some(remote_id) = stored.remote_id.as_deref() {
                            if !seen_remote.contains(remote_id) {
                                delete_stored(conn, &stored)?;
                            }
                        }
                    }
                }
                Ok(())
            })
            .await
    }

    async fn sync_webcal_calendars(&self, account: &Account, force: bool) -> Result<()> {
        let account_id = account.id.clone();
        let calendars = self
            .inner
            .ctx
            .db
            .read({
                let id = account_id.clone();
                move |conn| repo::calendars::list_for_account(conn, &id)
            })
            .await?;
        let mut first_error = None;
        let mut changed = Vec::new();
        for calendar in calendars {
            let Some(url) = calendar.subscription_url.clone() else {
                continue;
            };
            let last = self
                .inner
                .ctx
                .db
                .read({
                    let id = calendar.id.clone();
                    move |conn| store::webcal_last_refresh(conn, &id)
                })
                .await?;
            if !force {
                if let Some(last) = last {
                    if Utc::now() - last < Duration::hours(6) {
                        continue;
                    }
                }
            }
            match self.refresh_webcal(account, &calendar, &url).await {
                Ok(()) => changed.push(calendar.id),
                Err(error) => {
                    warn!(calendar = %calendar.id, url = %url, %error, "webcal refresh failed");
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        if !changed.is_empty() {
            emit_calendar_changed(self.inner.ctx.events.as_ref(), changed);
            self.inner.reminders.notify_changed();
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn refresh_webcal(
        &self,
        account: &Account,
        calendar: &Calendar,
        url: &str,
    ) -> Result<()> {
        let ics = webcal::fetch(&self.inner.http, url).await?;
        let remote_events =
            webcal::parse_subscription(&ics, &calendar.id, &account.id, &account.email)?;
        let calendar_id = calendar.id.clone();
        let account_id = calendar.account_id.clone();
        self.inner
            .ctx
            .db
            .write(move |conn| {
                let mut seen: HashSet<String> = HashSet::new();
                let mut ordered = remote_events;
                ordered.sort_by_key(|remote| remote.series_remote_id.is_some());
                let mut masters_by_uid: HashMap<String, String> = HashMap::new();
                for remote in ordered {
                    if let Some(remote_id) = &remote.remote_id {
                        seen.insert(remote_id.clone());
                    }
                    let existing = find_existing_event(conn, &calendar_id, &remote)?;
                    let id = existing
                        .as_ref()
                        .map(|stored| stored.event.id.clone())
                        .unwrap_or_else(new_id);
                    let series_id = if remote.event.recurrence_id.is_some() {
                        masters_by_uid.get(&remote.event.uid).cloned().or_else(|| {
                            events_repo::by_uid(conn, &calendar_id, &remote.event.uid)
                                .ok()
                                .flatten()
                                .map(|stored| stored.event.id)
                        })
                    } else {
                        masters_by_uid.insert(remote.event.uid.clone(), id.clone());
                        None
                    };
                    let mut event = remote.event.clone();
                    event.id = id;
                    event.calendar_id = calendar_id.clone();
                    event.account_id = account_id.clone();
                    event.series_id = series_id;
                    let stored = StoredEvent {
                        event,
                        raw_ics: remote.raw_ics,
                        etag: None,
                        remote_id: remote.remote_id,
                        href: None,
                        rrule: remote.rrule,
                        exdates: remote.exdates,
                    };
                    events_repo::upsert(conn, &stored)?;
                }
                let all = events_repo::list_for_calendar(conn, &calendar_id)?;
                for stored in all {
                    let stale = match stored.remote_id.as_deref() {
                        Some(remote_id) => !seen.contains(remote_id),
                        None => false,
                    };
                    if stale {
                        delete_stored(conn, &stored)?;
                    }
                }
                store::set_webcal_last_refresh(conn, &calendar_id, Utc::now())
            })
            .await?;
        Ok(())
    }

    /* ───────────────────────── calendar commands ───────────────────── */

    pub async fn calendar_list(&self) -> Result<Vec<Calendar>> {
        self.inner.ctx.db.read(repo::calendars::list).await
    }

    pub async fn calendar_create(
        &self,
        account_id: &str,
        name: &str,
        color: &str,
    ) -> Result<Calendar> {
        let name = name.trim();
        if name.is_empty() {
            return Err(invalid("calendar name must not be empty"));
        }
        let color = if color.trim().is_empty() {
            "#0078D4".to_string()
        } else {
            normalize_color(color).ok_or_else(|| invalid("invalid calendar color"))?
        };
        let account = self.load_account(account_id).await?;
        if backend_for(&account, DavService::Calendar) == Backend::Unsupported {
            return Err(unsupported("this account has no calendar provider"));
        }
        if !account.calendar_enabled {
            return Err(invalid("calendar is disabled for this account"));
        }
        let is_local = backend_for(&account, DavService::Calendar) == Backend::Local;
        let account_id = account.id.clone();
        let calendar_name = name.to_string();
        let calendar_color = color.clone();
        let calendar = self
            .inner
            .ctx
            .db
            .write(move |conn| {
                if is_local {
                    return repo::calendars::create(
                        conn,
                        &account_id,
                        &calendar_name,
                        &calendar_color,
                    );
                }
                let count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM calendar WHERE account_id = ?1",
                    rusqlite::params![account_id],
                    |row| row.get(0),
                )?;
                let mut calendar = Calendar {
                    id: new_id(),
                    account_id,
                    name: calendar_name,
                    color: calendar_color,
                    is_read_only: false,
                    is_visible: true,
                    is_default: count == 0,
                    subscription_url: None,
                };
                repo::calendars::insert(conn, &mut calendar)
            })
            .await?;

        if self.is_provider(&account) {
            let op = PimOp::CalendarCreate {
                calendar_id: calendar.id.clone(),
            };
            self.enqueue_op(&account.id, &op, format!("创建日历「{}」", calendar.name))
                .await?;
        }
        emit_calendar_changed(self.inner.ctx.events.as_ref(), vec![calendar.id.clone()]);
        Ok(calendar)
    }

    pub async fn calendar_update(
        &self,
        id: &str,
        name: Option<&str>,
        color: Option<&str>,
    ) -> Result<Calendar> {
        if let Some(name) = name {
            if name.trim().is_empty() {
                return Err(invalid("calendar name must not be empty"));
            }
        }
        let color = match color {
            Some(color) if !color.trim().is_empty() => {
                Some(normalize_color(color).ok_or_else(|| invalid("invalid calendar color"))?)
            }
            _ => None,
        };
        let id = id.to_string();
        let current = self.load_calendar(&id).await?;
        if current.is_read_only && current.subscription_url.is_none() {
            return Err(invalid("calendar is read-only"));
        }
        let name = name.map(|value| value.trim().to_string());
        let (previous, updated) = self
            .inner
            .ctx
            .db
            .write(move |conn| {
                let previous = repo::calendars::get_required(conn, &id)?;
                let updated =
                    repo::calendars::update(conn, &id, name.as_deref(), color.as_deref())?;
                Ok((previous, updated))
            })
            .await?;
        let account = self.load_account(&updated.account_id).await?;
        if self.is_provider(&account) {
            let op = PimOp::CalendarUpdate {
                calendar_id: updated.id.clone(),
                previous_name: previous.name,
                previous_color: previous.color,
            };
            self.enqueue_op(&account.id, &op, format!("更新日历「{}」", updated.name))
                .await?;
        }
        emit_calendar_changed(self.inner.ctx.events.as_ref(), vec![updated.id.clone()]);
        Ok(updated)
    }

    pub async fn calendar_set_visible(&self, id: &str, visible: bool) -> Result<()> {
        let id = id.to_string();
        let id_for_db = id.clone();
        self.inner
            .ctx
            .db
            .write(move |conn| repo::calendars::set_visible(conn, &id_for_db, visible))
            .await?;
        emit_calendar_changed(self.inner.ctx.events.as_ref(), vec![id]);
        Ok(())
    }

    pub async fn calendar_delete(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        let (calendar, events, mapping) = self
            .inner
            .ctx
            .db
            .write(move |conn| {
                let calendar = repo::calendars::get_required(conn, &id)?;
                let events = events_repo::list_for_calendar(conn, &id)?;
                let mapping = store::get_by_calendar(conn, &id)?;
                repo::calendars::delete(conn, &id)?;
                Ok((calendar, events, mapping))
            })
            .await?;
        let account = self.load_account(&calendar.account_id).await?;
        if self.is_provider(&account) && mapping.is_some() {
            let op = PimOp::CalendarDelete {
                calendar_id: calendar.id.clone(),
                snapshot: CalendarSnapshot::new(calendar.clone(), mapping, events),
            };
            self.enqueue_op(&account.id, &op, format!("删除日历「{}」", calendar.name))
                .await?;
        }
        emit_calendar_changed(self.inner.ctx.events.as_ref(), vec![calendar.id]);
        Ok(())
    }

    pub async fn calendar_subscribe(&self, url: &str, name: &str, color: &str) -> Result<Calendar> {
        let url = webcal::normalize_url(url)?;
        let name = name.trim();
        if name.is_empty() {
            return Err(invalid("calendar name must not be empty"));
        }
        let color = if color.trim().is_empty() {
            "#0078D4".to_string()
        } else {
            normalize_color(color).ok_or_else(|| invalid("invalid calendar color"))?
        };
        let local = self.inner.ctx.db.read(repo::accounts::local).await?;
        let local_id = local.id.clone();
        let name = name.to_string();
        let calendar = self
            .inner
            .ctx
            .db
            .write(move |conn| {
                let count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM calendar WHERE account_id = ?1",
                    rusqlite::params![local_id],
                    |row| row.get(0),
                )?;
                let mut calendar = Calendar {
                    id: new_id(),
                    account_id: local_id,
                    name,
                    color,
                    is_read_only: true,
                    is_visible: true,
                    is_default: count == 0,
                    subscription_url: Some(url),
                };
                repo::calendars::insert(conn, &mut calendar)
            })
            .await?;
        emit_calendar_changed(self.inner.ctx.events.as_ref(), vec![calendar.id.clone()]);
        if let Err(error) = self
            .refresh_webcal(
                &local,
                &calendar,
                calendar.subscription_url.as_deref().unwrap_or_default(),
            )
            .await
        {
            warn!(calendar = %calendar.id, %error, "initial webcal fetch failed");
        }
        Ok(calendar)
    }

    pub async fn calendar_import_ics(&self, path: &str, calendar_id: &str) -> Result<u32> {
        let text = tokio::fs::read_to_string(path)
            .await
            .map_err(|error| AppError::io(format!("cannot read {path}: {error}")))?;
        let parsed = nuncio_ical::ics::parse_calendar(&text)?;
        if parsed.events.is_empty() {
            return Ok(0);
        }
        let calendar = self.load_calendar(calendar_id).await?;
        if calendar.is_read_only || calendar.subscription_url.is_some() {
            return Err(invalid("calendar is read-only"));
        }
        let account = self.load_account(&calendar.account_id).await?;

        let mut parsed_events = parsed.events;
        if let Some(index) = parsed_events
            .iter()
            .position(|event| event.recurrence_id.is_none())
        {
            let master = parsed_events.remove(index);
            parsed_events.insert(0, master);
        }

        let calendar_id = calendar.id.clone();
        let account_id = calendar.account_id.clone();
        let written = self
            .inner
            .ctx
            .db
            .write(move |conn| {
                let mut master_ids: HashMap<String, String> = HashMap::new();
                let mut written: Vec<ImportedComponent> = Vec::new();
                for parsed_event in parsed_events {
                    let uid = parsed_event.uid.clone();
                    let recurrence_id = parsed_event.recurrence_id.clone();
                    let cancelled = parsed_event.status == EventStatus::Cancelled;
                    let was_master = recurrence_id.is_none();
                    let existing =
                        find_event_by_uid(conn, &calendar_id, &uid, recurrence_id.as_deref())?;
                    let was_existing = existing.is_some();
                    let id = existing
                        .as_ref()
                        .map(|stored| stored.event.id.clone())
                        .unwrap_or_else(new_id);
                    let rrule = parsed_event.rrule.clone();
                    let exdates = parsed_event.exdates.clone();
                    let rdates = parsed_event.rdates.clone();
                    let all_day = parsed_event.all_day;
                    let start = parsed_event.start.clone();
                    let mut stored =
                        parsed_event.into_stored(id.clone(), &calendar_id, &account_id);
                    // Only RDATE values are needed later; storing the entire
                    // imported file on every master is O(events × file size).
                    stored.raw_ics = if was_master {
                        webcal::rdates_only_ics(&uid, all_day, &start, &rdates)
                    } else {
                        None
                    };
                    stored.rrule = rrule;
                    stored.exdates = exdates;
                    if let Some(existing) = &existing {
                        stored.remote_id = existing.remote_id.clone();
                        stored.etag = existing.etag.clone();
                        stored.href = existing.href.clone();
                    }
                    if let Some(recurrence_id) = &recurrence_id {
                        stored.event.recurrence_id = Some(recurrence_id.clone());
                        stored.event.series_id = master_ids.get(&uid).cloned().or_else(|| {
                            find_master_by_uid(conn, &calendar_id, &uid)
                                .ok()
                                .flatten()
                                .map(|stored| stored.event.id)
                        });
                    } else {
                        master_ids.insert(uid.clone(), id.clone());
                    }
                    events_repo::upsert(conn, &stored)?;
                    written.push(ImportedComponent {
                        id,
                        uid,
                        was_existing,
                        recurrence_id,
                        cancelled,
                        was_master,
                    });
                }
                Ok(written)
            })
            .await?;

        let count = written.len() as u32;
        if self.is_provider(&account) {
            let groups = group_imported_components(written);
            let is_dav = backend_for(&account, DavService::Calendar) == Backend::Dav;
            for group in groups {
                self.enqueue_import_group(&account, &group, is_dav).await?;
            }
        }
        emit_calendar_changed(self.inner.ctx.events.as_ref(), vec![calendar.id]);
        Ok(count)
    }

    /// Queue provider writes for one imported UID group.
    ///
    /// CalDAV stores a whole series in a single resource, so master and
    /// exceptions are PUT together.  Google/Graph need the master first and
    /// then per-occurrence patches (`originalStartTime`/instance ids), which
    /// the normal scoped update/delete operations already implement.
    async fn enqueue_import_group(
        &self,
        account: &Account,
        group: &ImportedGroup,
        is_dav: bool,
    ) -> Result<()> {
        let Some(master_id) = group.master_id.as_deref() else {
            warn!(
                uid = %group.uid,
                "imported calendar resource has exceptions but no master; skipped provider write"
            );
            return Ok(());
        };
        if is_dav {
            let op = if group.master_existing {
                PimOp::EventUpdate {
                    event_id: master_id.to_string(),
                    scope: RecurrenceEditScope::All,
                    instance_start: None,
                    notify_attendees: false,
                    snapshot: Vec::new(),
                    following_id: None,
                }
            } else {
                PimOp::EventCreate {
                    event_id: master_id.to_string(),
                    notify_attendees: false,
                }
            };
            self.enqueue_op(&account.id, &op, "导入日程".to_string())
                .await?;
            return Ok(());
        }

        let master_op = if group.master_existing {
            PimOp::EventUpdate {
                event_id: master_id.to_string(),
                scope: RecurrenceEditScope::All,
                instance_start: None,
                notify_attendees: false,
                snapshot: Vec::new(),
                following_id: None,
            }
        } else {
            PimOp::EventCreate {
                event_id: master_id.to_string(),
                notify_attendees: false,
            }
        };
        self.enqueue_op(&account.id, &master_op, "导入日程".to_string())
            .await?;
        for exception in &group.overrides {
            let Some(recurrence_id) = &exception.recurrence_id else {
                continue;
            };
            let op = if exception.cancelled {
                PimOp::EventDelete {
                    event_id: master_id.to_string(),
                    scope: RecurrenceEditScope::This,
                    instance_start: Some(recurrence_id.clone()),
                    notify_attendees: false,
                    snapshot: Vec::new(),
                }
            } else {
                PimOp::EventUpdate {
                    event_id: master_id.to_string(),
                    scope: RecurrenceEditScope::This,
                    instance_start: Some(recurrence_id.clone()),
                    notify_attendees: false,
                    snapshot: Vec::new(),
                    following_id: None,
                }
            };
            self.enqueue_op(&account.id, &op, "导入日程".to_string())
                .await?;
        }
        Ok(())
    }

    pub async fn calendar_export_ics(&self, calendar_id: &str, path: &str) -> Result<()> {
        let calendar_id = calendar_id.to_string();
        let stored = self
            .inner
            .ctx
            .db
            .read(move |conn| events_repo::list_for_calendar(conn, &calendar_id))
            .await?;
        let ics = nuncio_ical::ics::write_stored_calendar(&stored);
        tokio::fs::write(path, ics)
            .await
            .map_err(|error| AppError::io(format!("cannot write {path}: {error}")))
    }

    /* ────────────────────────── event commands ─────────────────────── */

    pub async fn event_range(
        &self,
        from: &str,
        to: &str,
        calendar_ids: Option<Vec<String>>,
    ) -> Result<Vec<EventInstance>> {
        let from_instant = parse_instant(from)?;
        let to_instant = parse_instant(to)?;
        if to_instant <= from_instant {
            return Err(invalid("range end must be after the start"));
        }
        let stored = self
            .inner
            .ctx
            .db
            .read({
                let from = from.to_string();
                let to = to.to_string();
                move |conn| events_repo::range(conn, &from, &to, calendar_ids.as_deref())
            })
            .await?;
        let calendars = self.calendar_list().await?;
        let colors: HashMap<String, String> = calendars
            .iter()
            .map(|calendar| (calendar.id.clone(), calendar.color.clone()))
            .collect();

        let mut masters: Vec<StoredEvent> = Vec::new();
        let mut overrides: HashMap<String, Vec<CalendarEvent>> = HashMap::new();
        let mut master_by_uid: HashMap<(String, String), String> = HashMap::new();
        for item in &stored {
            if item.event.series_id.is_none() && item.event.recurrence_id.is_none() {
                master_by_uid.insert(
                    (item.event.calendar_id.clone(), item.event.uid.clone()),
                    item.event.id.clone(),
                );
                masters.push(item.clone());
            }
        }
        for item in &stored {
            if item.event.recurrence_id.is_none()
                || item.event.series_id.is_none() && item.event.recurrence.is_some()
            {
                continue;
            }
            let series = item
                .event
                .series_id
                .clone()
                .or_else(|| {
                    master_by_uid
                        .get(&(item.event.calendar_id.clone(), item.event.uid.clone()))
                        .cloned()
                })
                .unwrap_or_default();
            if !series.is_empty() {
                overrides
                    .entry(series)
                    .or_default()
                    .push(item.event.clone());
            }
        }

        let mut instances = Vec::new();
        for master in masters {
            let event = &master.event;
            if event.status == EventStatus::Cancelled {
                continue;
            }
            let calendar_color = colors.get(&event.calendar_id).cloned();
            let color = event_color(event, calendar_color.as_deref());
            let series_overrides = overrides.remove(&event.id).unwrap_or_default();
            if event.recurrence.is_some() {
                let occurrences = nuncio_ical::recurrence::expand_stored(
                    &master,
                    &series_overrides,
                    from_instant,
                    to_instant,
                    MAX_OCCURRENCES_PER_SERIES,
                );
                for occurrence in occurrences {
                    let source = series_overrides
                        .iter()
                        .find(|over| {
                            over.recurrence_id.as_deref().is_some_and(|value| {
                                same_instant(value, &occurrence.original_start)
                            })
                        })
                        .unwrap_or(event);
                    let instance_color = event_color(source, calendar_color.as_deref());
                    instances.push(EventInstance {
                        event_id: event.id.clone(),
                        instance_key: format!("{}:{}", event.id, occurrence.start),
                        calendar_id: event.calendar_id.clone(),
                        account_id: event.account_id.clone(),
                        title: source.title.clone(),
                        location: source.location.clone(),
                        start: occurrence.start,
                        end: occurrence.end,
                        all_day: event.all_day,
                        color: instance_color,
                        is_recurring: true,
                        show_as: source.show_as,
                        status: source.status,
                        my_response: source.my_response,
                        has_attendees: !source.attendees.is_empty(),
                        has_reminder: !source.reminders.is_empty(),
                    });
                }
            } else {
                let start = parse_instant(&event.start)?;
                let end = parse_instant(&event.end).unwrap_or(start);
                if end >= from_instant && start <= to_instant {
                    instances.push(EventInstance {
                        event_id: event.id.clone(),
                        instance_key: format!("{}:{}", event.id, event.start),
                        calendar_id: event.calendar_id.clone(),
                        account_id: event.account_id.clone(),
                        title: event.title.clone(),
                        location: event.location.clone(),
                        start: event.start.clone(),
                        end: event.end.clone(),
                        all_day: event.all_day,
                        color: color.clone(),
                        is_recurring: false,
                        show_as: event.show_as,
                        status: event.status,
                        my_response: event.my_response,
                        has_attendees: !event.attendees.is_empty(),
                        has_reminder: !event.reminders.is_empty(),
                    });
                }
            }
        }
        instances.sort_by(|left, right| left.start.cmp(&right.start));
        Ok(instances)
    }

    pub async fn event_get(
        &self,
        event_id: &str,
        instance_start: Option<&str>,
    ) -> Result<CalendarEvent> {
        let stored = self.load_event(event_id).await?;
        let Some(instance_start) = instance_start else {
            return Ok(stored.event);
        };
        let master = if let Some(series_id) = &stored.event.series_id {
            self.load_event(series_id).await.unwrap_or(stored.clone())
        } else {
            stored.clone()
        };
        let master_id = master.event.id.clone();
        let uid = master.event.uid.clone();
        let calendar_id = master.event.calendar_id.clone();
        let series = self
            .inner
            .ctx
            .db
            .read(move |conn| series_events(conn, &calendar_id, &uid))
            .await?;
        let overrides: Vec<CalendarEvent> = series
            .iter()
            .filter(|item| {
                item.event.series_id.as_deref() == Some(master_id.as_str())
                    || item.event.recurrence_id.is_some()
            })
            .map(|item| item.event.clone())
            .collect();
        // Match the occurrence both by its original start (RECURRENCE-ID) and
        // by its current start.  After an occurrence was moved, the frontend
        // opens it with the *new* start, so an override whose RECURRENCE-ID is
        // the original start would otherwise be missed and the peek would show
        // master fields.
        if let Some(override_event) = overrides.iter().find(|event| {
            event
                .recurrence_id
                .as_deref()
                .is_some_and(|value| same_instant(value, instance_start))
                || same_instant(&event.start, instance_start)
        }) {
            return Ok(override_event.clone());
        }
        if master.event.recurrence.is_none() {
            return Ok(master.event);
        }
        let instance = parse_instant(instance_start)?;
        let occurrences = nuncio_ical::recurrence::expand_stored(
            &master,
            &overrides,
            instance - Duration::days(1),
            instance + Duration::days(2),
            MAX_OCCURRENCES_PER_SERIES,
        );
        if let Some(occurrence) = occurrences.into_iter().find(|occurrence| {
            same_instant(&occurrence.original_start, instance_start)
                || same_instant(&occurrence.start, instance_start)
        }) {
            let mut view = master.event.clone();
            view.start = occurrence.start;
            view.end = occurrence.end;
            view.recurrence_id = Some(occurrence.original_start);
            view.series_id = Some(master.event.id.clone());
            return Ok(view);
        }
        Ok(master.event)
    }

    pub async fn event_create(&self, input: EventInput) -> Result<CalendarEvent> {
        let (calendar, account) = self.validate_input(&input).await?;
        let event = build_event(new_id(), new_id(), &calendar, &account, &input, None);
        let event_id = event.id.clone();
        let stored = StoredEvent::new(event);
        let insert = stored.clone();
        self.inner
            .ctx
            .db
            .write(move |conn| events_repo::upsert(conn, &insert))
            .await?;
        if self.is_provider(&account) {
            let op = PimOp::EventCreate {
                event_id: event_id.clone(),
                notify_attendees: input.notify_attendees,
            };
            self.enqueue_op(
                &account.id,
                &op,
                format!("创建日程「{}」", stored.event.title),
            )
            .await?;
        }
        emit_calendar_changed(self.inner.ctx.events.as_ref(), vec![calendar.id.clone()]);
        self.inner.reminders.notify_changed();
        Ok(stored.event)
    }

    pub async fn event_update(
        &self,
        event_id: &str,
        input: EventInput,
        scope: RecurrenceEditScope,
        instance_start: Option<&str>,
    ) -> Result<CalendarEvent> {
        let stored = self.load_event(event_id).await?;
        let (calendar, account) = self.validate_input(&input).await?;
        if calendar.id != stored.event.calendar_id
            && (backend_for(&account, DavService::Calendar) != Backend::Local
                || stored.event.recurrence.is_some())
        {
            return Err(unsupported(
                "moving events between calendars is only supported for non-recurring local events",
            ));
        }
        let master = match &stored.event.series_id {
            Some(series_id) => self.load_event(series_id).await?,
            None => stored.clone(),
        };
        let scope = if master.event.recurrence.is_none()
            || (scope == RecurrenceEditScope::ThisAndFollowing
                && instance_start.is_some_and(|value| same_instant(value, &master.event.start)))
        {
            RecurrenceEditScope::All
        } else {
            scope
        };

        let calendar_id = calendar.id.clone();
        let account_id = account.id.clone();
        let is_provider_account = self.is_provider(&account);
        let fallback_instance = stored.event.recurrence_id.clone();
        let fallback_instance_after = fallback_instance.clone();
        let instance_start_owned = instance_start.map(str::to_string);
        let instance_start_after = instance_start_owned.clone();
        let input_clone = input.clone();
        let master_id = master.event.id.clone();
        let (result, snapshot) =
            self.inner
                .ctx
                .db
                .write(move |conn| {
                    let series = series_events(conn, &master.event.calendar_id, &master.event.uid)?;
                    let snapshot = ops::snapshots(&series);
                    match scope {
                        RecurrenceEditScope::All => {
                            let mut updated = build_event(
                                master.event.id.clone(),
                                master.event.uid.clone(),
                                &calendar,
                                &account,
                                &input_clone,
                                Some(&master.event),
                            );
                            reanchor_recurring_update(&master.event, &mut updated);
                            let mut stored = master.clone();
                            let recurrence_changed = master.event.recurrence != updated.recurrence;
                            stored.event = updated.clone();
                            stored.rrule = crate::util::rrule_for_update(
                                master.rrule.as_deref(),
                                updated.recurrence.as_ref(),
                            );
                            if recurrence_changed {
                                // RDATEs belong to the previous rule.
                                stored.raw_ics = None;
                            }
                            events_repo::upsert(conn, &stored)?;
                            if updated.recurrence.is_none() {
                                for item in &series {
                                    if item.event.id != master.event.id {
                                        events_repo::delete(conn, &item.event.id)?;
                                    }
                                }
                            }
                            Ok((updated, snapshot))
                        }
                        RecurrenceEditScope::This => {
                            let instance_start = instance_start_owned
                                .clone()
                                .or_else(|| fallback_instance.clone())
                                .ok_or_else(|| invalid("instance start is required"))?;
                            let existing =
                                series.iter().find(|item| {
                                    item.event.id != master.event.id
                                        && (item.event.recurrence_id.as_deref().is_some_and(
                                            |value| same_instant(value, &instance_start),
                                        ) || same_instant(&item.event.start, &instance_start))
                                });
                            let mut exception =
                                existing.map(|item| item.event.clone()).unwrap_or_else(|| {
                                    CalendarEvent::new(
                                        new_id(),
                                        master.event.calendar_id.clone(),
                                        master.event.account_id.clone(),
                                        master.event.uid.clone(),
                                        input_clone.title.clone(),
                                        input_clone.start.clone(),
                                        input_clone.end.clone(),
                                    )
                                });
                            apply_input(&mut exception, &input_clone, &account);
                            exception.uid = master.event.uid.clone();
                            exception.series_id = Some(master.event.id.clone());
                            // Keep the original RECURRENCE-ID when the edit was
                            // started from an already-moved occurrence.
                            exception.recurrence_id = Some(
                                existing
                                    .and_then(|item| item.event.recurrence_id.clone())
                                    .unwrap_or_else(|| instance_start.clone()),
                            );
                            exception.recurrence = None;
                            exception.status = EventStatus::Confirmed;
                            let mut stored_override =
                                existing.cloned().unwrap_or_else(|| StoredEvent {
                                    event: exception.clone(),
                                    raw_ics: None,
                                    etag: None,
                                    remote_id: None,
                                    href: master.href.clone(),
                                    rrule: None,
                                    exdates: Vec::new(),
                                });
                            stored_override.event = exception.clone();
                            stored_override.href = master.href.clone();
                            events_repo::upsert(conn, &stored_override)?;
                            Ok((exception, snapshot))
                        }
                        RecurrenceEditScope::ThisAndFollowing => {
                            let instance_start = instance_start_owned
                                .clone()
                                .ok_or_else(|| invalid("instance start is required"))?;
                            let split = nuncio_ical::recurrence::split_series_at(
                                &master.event,
                                &instance_start,
                            )?;
                            let mut truncated = master.clone();
                            truncated.event = split.master;
                            truncated.exdates.retain(|value| {
                                !crate::dav::map::at_or_after(value, &instance_start)
                            });
                            truncated.raw_ics = None;
                            truncated.rrule = truncated
                                .event
                                .recurrence
                                .as_ref()
                                .map(nuncio_ical::recurrence::to_rrule_string);
                            events_repo::upsert(conn, &truncated)?;

                            let mut following = {
                                let mut event = CalendarEvent::new(
                                    new_id(),
                                    master.event.calendar_id.clone(),
                                    master.event.account_id.clone(),
                                    new_id(),
                                    input_clone.title.clone(),
                                    input_clone.start.clone(),
                                    input_clone.end.clone(),
                                );
                                apply_input(&mut event, &input_clone, &account);
                                event.recurrence = input_clone.recurrence.clone();
                                event.series_id = None;
                                event.recurrence_id = None;
                                event.uid = new_id();
                                if event.organizer.is_none() {
                                    event.organizer = master.event.organizer.clone();
                                }
                                event.is_organizer = master.event.is_organizer;
                                event.my_response = master.event.my_response;
                                event
                            };
                            following.status = EventStatus::Confirmed;
                            let following_stored = StoredEvent::new(following.clone());
                            events_repo::upsert(conn, &following_stored)?;
                            // Remove overrides that belonged to the old tail.
                            for item in &series {
                                if item.event.id == master.event.id {
                                    continue;
                                }
                                let after =
                                    item.event.recurrence_id.as_deref().is_some_and(|value| {
                                        crate::dav::map::at_or_after(value, &instance_start)
                                    });
                                if after {
                                    events_repo::delete(conn, &item.event.id)?;
                                }
                            }
                            Ok((following, snapshot))
                        }
                    }
                })
                .await?;

        if is_provider_account {
            let (scope, instance_start, following_id) = match scope {
                RecurrenceEditScope::All => (RecurrenceEditScope::All, None, None),
                RecurrenceEditScope::This => (
                    RecurrenceEditScope::This,
                    result
                        .recurrence_id
                        .clone()
                        .or_else(|| instance_start_after.clone())
                        .or_else(|| fallback_instance_after.clone()),
                    None,
                ),
                RecurrenceEditScope::ThisAndFollowing => (
                    RecurrenceEditScope::ThisAndFollowing,
                    instance_start_after.clone(),
                    Some(result.id.clone()),
                ),
            };
            let op = PimOp::EventUpdate {
                event_id: master_id.clone(),
                scope,
                instance_start,
                notify_attendees: input.notify_attendees,
                snapshot,
                following_id,
            };
            self.enqueue_op(&account_id, &op, format!("更新日程「{}」", result.title))
                .await?;
        }
        emit_calendar_changed(self.inner.ctx.events.as_ref(), vec![calendar_id]);
        self.inner.reminders.notify_changed();
        Ok(result)
    }

    pub async fn event_delete(
        &self,
        event_id: &str,
        scope: RecurrenceEditScope,
        instance_start: Option<&str>,
        notify_attendees: bool,
    ) -> Result<()> {
        let stored = self.load_event(event_id).await?;
        let master = match &stored.event.series_id {
            Some(series_id) => self.load_event(series_id).await?,
            None => stored.clone(),
        };
        let calendar = self.load_calendar(&master.event.calendar_id).await?;
        let account = self.load_account(&calendar.account_id).await?;
        let scope = if master.event.recurrence.is_none()
            || (scope == RecurrenceEditScope::ThisAndFollowing
                && instance_start
                    .or(stored.event.recurrence_id.as_deref())
                    .is_some_and(|value| same_instant(value, &master.event.start)))
        {
            RecurrenceEditScope::All
        } else {
            scope
        };
        let master_id = master.event.id.clone();
        let master_title = master.event.title.clone();
        let calendar_id = calendar.id.clone();
        let instance_owned = instance_start
            .map(str::to_string)
            .or_else(|| stored.event.recurrence_id.clone());
        let instance_for_db = instance_owned.clone();
        let snapshot = self
            .inner
            .ctx
            .db
            .write(move |conn| {
                let series = series_events(conn, &master.event.calendar_id, &master.event.uid)?;
                let snapshot = ops::snapshots(&series);
                match scope {
                    RecurrenceEditScope::All => {
                        events_repo::delete_series(conn, &master.event.id)?;
                    }
                    RecurrenceEditScope::This => {
                        let instance_start = instance_for_db
                            .as_deref()
                            .ok_or_else(|| invalid("instance start is required"))?;
                        let mut updated = master.clone();
                        nuncio_ical::recurrence::add_exdate(&mut updated.exdates, instance_start);
                        events_repo::upsert(conn, &updated)?;
                        for item in &series {
                            if item.event.id == master.event.id {
                                continue;
                            }
                            let matches = item
                                .event
                                .recurrence_id
                                .as_deref()
                                .is_some_and(|value| same_instant(value, instance_start))
                                || same_instant(&item.event.start, instance_start);
                            if matches {
                                events_repo::delete(conn, &item.event.id)?;
                            }
                        }
                    }
                    RecurrenceEditScope::ThisAndFollowing => {
                        let instance_start = instance_for_db
                            .as_deref()
                            .ok_or_else(|| invalid("instance start is required"))?;
                        let split = nuncio_ical::recurrence::split_series_at(
                            &master.event,
                            instance_start,
                        )?;
                        let mut truncated = master.clone();
                        truncated.event = split.master;
                        truncated
                            .exdates
                            .retain(|value| !crate::dav::map::at_or_after(value, instance_start));
                        truncated.raw_ics = None;
                        truncated.rrule = truncated
                            .event
                            .recurrence
                            .as_ref()
                            .map(nuncio_ical::recurrence::to_rrule_string);
                        events_repo::upsert(conn, &truncated)?;
                        for item in &series {
                            if item.event.id == master.event.id {
                                continue;
                            }
                            let after = item.event.recurrence_id.as_deref().is_some_and(|value| {
                                crate::dav::map::at_or_after(value, instance_start)
                            });
                            if after {
                                events_repo::delete(conn, &item.event.id)?;
                            }
                        }
                    }
                }
                Ok(snapshot)
            })
            .await?;

        if self.is_provider(&account) {
            let (scope, instance_start) = match scope {
                RecurrenceEditScope::All => (RecurrenceEditScope::All, None),
                _ => (scope, instance_owned),
            };
            let op = PimOp::EventDelete {
                event_id: master_id,
                scope,
                instance_start,
                notify_attendees,
                snapshot,
            };
            self.enqueue_op(&account.id, &op, format!("删除日程「{}」", master_title))
                .await?;
        }
        emit_calendar_changed(self.inner.ctx.events.as_ref(), vec![calendar_id]);
        self.inner.reminders.notify_changed();
        Ok(())
    }

    pub async fn event_respond(
        &self,
        event_id: &str,
        response: AttendeeStatus,
        comment: Option<&str>,
        send_reply: bool,
    ) -> Result<()> {
        let stored = self.load_event(event_id).await?;
        let master = match &stored.event.series_id {
            Some(series_id) => self.load_event(series_id).await?,
            None => stored.clone(),
        };
        let calendar = self.load_calendar(&master.event.calendar_id).await?;
        let account = self.load_account(&calendar.account_id).await?;
        let email = account.email.clone();
        let snapshot_series = {
            let calendar_id = master.event.calendar_id.clone();
            let uid = master.event.uid.clone();
            self.inner
                .ctx
                .db
                .read(move |conn| series_events(conn, &calendar_id, &uid))
                .await?
        };
        let snapshot = ops::snapshots(&snapshot_series);
        let master_id = master.event.id.clone();
        let master_title = master.event.title.clone();
        let email_for_db = email.clone();
        self.inner
            .ctx
            .db
            .write(move |conn| {
                let mut updated = master.clone();
                set_attendee_status(&mut updated.event, &email_for_db, response);
                events_repo::upsert(conn, &updated)
            })
            .await?;

        if self.is_provider(&account) {
            let op = PimOp::EventRespond {
                event_id: master_id,
                response,
                comment: comment.map(str::to_string),
                send_reply,
                snapshot,
            };
            self.enqueue_op(
                &account.id,
                &op,
                format!("回复日程邀请「{}」", master_title),
            )
            .await?;
        } else if send_reply && !account.provider.is_local() {
            // Local-only accounts have no mail transport; nothing to send.
        }
        let calendar_id = calendar.id;
        emit_calendar_changed(self.inner.ctx.events.as_ref(), vec![calendar_id]);
        Ok(())
    }

    pub async fn event_search(&self, text: &str, limit: u32) -> Result<Vec<EventInstance>> {
        let text = text.to_string();
        let stored = self
            .inner
            .ctx
            .db
            .read(move |conn| events_repo::search(conn, &text, limit))
            .await?;
        let calendars = self.calendar_list().await?;
        let colors: HashMap<String, String> = calendars
            .iter()
            .map(|calendar| (calendar.id.clone(), calendar.color.clone()))
            .collect();
        let mut out = Vec::new();
        for item in stored {
            if item.event.status == EventStatus::Cancelled {
                continue;
            }
            let color = event_color(
                &item.event,
                colors.get(&item.event.calendar_id).map(|s| s.as_str()),
            );
            out.push(EventInstance {
                event_id: item.event.id.clone(),
                instance_key: format!("{}:{}", item.event.id, item.event.start),
                calendar_id: item.event.calendar_id.clone(),
                account_id: item.event.account_id.clone(),
                title: item.event.title.clone(),
                location: item.event.location.clone(),
                start: item.event.start.clone(),
                end: item.event.end.clone(),
                all_day: item.event.all_day,
                color,
                is_recurring: item.event.recurrence.is_some(),
                show_as: item.event.show_as,
                status: item.event.status,
                my_response: item.event.my_response,
                has_attendees: !item.event.attendees.is_empty(),
                has_reminder: !item.event.reminders.is_empty(),
            });
        }
        Ok(out)
    }

    pub async fn event_dismiss_reminder(&self, event_id: &str, instance_start: &str) -> Result<()> {
        let event_id = event_id.to_string();
        let instance_start = instance_start.to_string();
        self.inner
            .ctx
            .db
            .write(move |conn| reminder_state::dismiss(conn, &event_id, &instance_start))
            .await?;
        Ok(())
    }

    pub async fn event_snooze_reminder(
        &self,
        event_id: &str,
        instance_start: &str,
        minutes: u32,
    ) -> Result<()> {
        let event_id = event_id.to_string();
        let instance_start = instance_start.to_string();
        self.inner
            .ctx
            .db
            .write(move |conn| {
                let until = reminder_state::snooze(
                    conn,
                    &event_id,
                    &instance_start,
                    minutes.max(1),
                    Utc::now(),
                )?;
                store::clear_reminder_fired(conn, &event_id, &instance_start)?;
                Ok(until)
            })
            .await?;
        self.inner.reminders.notify_changed();
        Ok(())
    }

    /// Dismiss the current reminder of a task (persisted, never fires again
    /// for that reminder key).
    pub async fn task_dismiss_reminder(&self, task_id: &str) -> Result<()> {
        let task_id = task_id.to_string();
        self.inner
            .ctx
            .db
            .write(move |conn| {
                let task = repo::tasks::get_required(conn, &task_id)?;
                let key = task_reminder_key(&task);
                reminder_state::dismiss_for(conn, reminder_state::KIND_TASK, &task.id, &key)
            })
            .await?;
        Ok(())
    }

    /// Snooze a task reminder; the reminder loop re-fires it after `minutes`.
    pub async fn task_snooze_reminder(&self, task_id: &str, minutes: u32) -> Result<()> {
        let task_id = task_id.to_string();
        self.inner
            .ctx
            .db
            .write(move |conn| {
                let task = repo::tasks::get_required(conn, &task_id)?;
                let key = task_reminder_key(&task);
                reminder_state::snooze_for(
                    conn,
                    reminder_state::KIND_TASK,
                    &task.id,
                    &key,
                    minutes.max(1),
                    Utc::now(),
                )?;
                store::clear_task_reminder_fired(conn, &task.id, &key)
            })
            .await?;
        self.inner.reminders.notify_changed();
        Ok(())
    }

    /* ───────────────────────── invite handling ─────────────────────── */

    pub async fn invite_respond(
        &self,
        message_id: &str,
        response: AttendeeStatus,
        comment: Option<&str>,
    ) -> Result<()> {
        let message = self
            .inner
            .ctx
            .db
            .read({
                let id = message_id.to_string();
                move |conn| repo::messages::get_required(conn, &id)
            })
            .await?;
        let Some(path) = message.body_path.as_deref() else {
            return Err(AppError::not_found(
                "the invitation message body has not been downloaded yet",
            ));
        };
        let raw = tokio::fs::read(path)
            .await
            .map_err(|error| AppError::io(format!("cannot read message body: {error}")))?;
        let parsed_message = nuncio_mime::parse(&raw)?;
        let part = parsed_message
            .calendar_parts
            .iter()
            .find(|part| matches!(part.method.as_str(), "REQUEST" | "CANCEL" | "PUBLISH"))
            .or_else(|| parsed_message.calendar_parts.first())
            .ok_or_else(|| AppError::not_found("message has no calendar attachment"))?;
        let calendar = nuncio_ical::ics::parse_calendar(&part.text)?;
        let method = calendar
            .method
            .clone()
            .unwrap_or_else(|| part.method.clone())
            .to_ascii_uppercase();
        let mut events = calendar.events;
        if events.is_empty() {
            return Err(AppError::not_found("calendar attachment has no event"));
        }
        if let Some(index) = events
            .iter()
            .position(|event| event.recurrence_id.is_none())
        {
            let master = events.remove(index);
            events.insert(0, master);
        }
        let master = events.remove(0);
        let uid = master.uid.clone();
        let account_id = message.summary.account_id.clone();
        let account = self.load_account(&account_id).await?;

        let mut target_calendar = self.find_account_calendar(&account_id).await?.unwrap_or(
            self.inner
                .ctx
                .db
                .read(repo::calendars::writable_default)
                .await?
                .ok_or_else(|| AppError::not_found("no writable calendar"))?,
        );

        if method == "CANCEL" {
            let account_for_db = account.id.clone();
            let uid_for_db = uid.clone();
            let mut changed = self
                .inner
                .ctx
                .db
                .write(move |conn| {
                    let mut changed = Vec::new();
                    for calendar in repo::calendars::list_for_account(conn, &account_for_db)? {
                        if let Some(existing) =
                            events_repo::by_uid(conn, &calendar.id, &uid_for_db)?
                        {
                            events_repo::delete_series(conn, &existing.event.id)?;
                            changed.push(calendar.id);
                        }
                    }
                    Ok(changed)
                })
                .await?;
            if changed.is_empty() {
                changed.push(target_calendar.id);
            }
            emit_calendar_changed(self.inner.ctx.events.as_ref(), changed);
            return Ok(());
        }

        // Prefer an existing local copy of the invitation.
        let existing = self.find_event_by_uid_account(&account_id, &uid).await?;
        if let Some(existing) = &existing {
            target_calendar = self.load_calendar(&existing.event.calendar_id).await?;
        }
        let remote_existing = self
            .find_remote_invite(&account, &uid, &mut target_calendar)
            .await?;

        let master_for_stored = master.clone();
        let calendar_id = target_calendar.id.clone();
        let account_for_db = account.id.clone();
        let email = account.email.clone();
        let rdates = master_for_stored.rdates.clone();
        let all_day = master_for_stored.all_day;
        let start = master_for_stored.start.clone();
        let uid_for_raw = master_for_stored.uid.clone();
        let stored = self
            .inner
            .ctx
            .db
            .write(move |conn| {
                let existing = find_event_by_uid(conn, &calendar_id, &master_for_stored.uid, None)?;
                let id = existing
                    .as_ref()
                    .map(|stored| stored.event.id.clone())
                    .unwrap_or_else(new_id);
                let rrule = master_for_stored.rrule.clone();
                let exdates = master_for_stored.exdates.clone();
                let mut stored = master_for_stored.into_stored(id, &calendar_id, &account_for_db);
                // Keep only RDATE data, not the complete invitation message.
                stored.raw_ics = webcal::rdates_only_ics(&uid_for_raw, all_day, &start, &rdates);
                stored.rrule = rrule;
                stored.exdates = exdates;
                stored.event.is_organizer = false;
                set_attendee_status(&mut stored.event, &email, response);
                if let Some(existing) = &existing {
                    stored.remote_id = existing.remote_id.clone();
                    stored.etag = existing.etag.clone();
                    stored.href = existing.href.clone();
                    stored.event.series_id = existing.event.series_id.clone();
                    stored.event.recurrence_id = existing.event.recurrence_id.clone();
                }
                if let Some(remote) = remote_existing {
                    stored.remote_id = remote.0;
                    stored.etag = remote.1;
                }
                events_repo::upsert(conn, &stored)
            })
            .await?;

        if self.is_provider(&account) {
            if stored.remote_id.is_some() {
                let op = PimOp::EventRespond {
                    event_id: stored.event.id.clone(),
                    response,
                    comment: comment.map(str::to_string),
                    send_reply: true,
                    snapshot: Vec::new(),
                };
                self.enqueue_op(
                    &account.id,
                    &op,
                    format!("回复日程邀请「{}」", stored.event.title),
                )
                .await?;
            } else {
                let op = PimOp::EventCreate {
                    event_id: stored.event.id.clone(),
                    notify_attendees: false,
                };
                self.enqueue_op(
                    &account.id,
                    &op,
                    format!("添加日程「{}」", stored.event.title),
                )
                .await?;
            }
        }
        // When the event exists on the server the queued EventRespond op sends
        // the iTIP reply; only send immediately for a brand-new local copy.
        if stored.remote_id.is_none()
            && stored.href.is_none()
            && account.mail_enabled
            && !matches!(
                backend_for(&account, DavService::Calendar),
                Backend::Google | Backend::Graph
            )
            && stored.event.organizer.is_some()
        {
            let _ = self
                .send_itip_reply(&account, &stored, response, comment)
                .await;
        }
        emit_calendar_changed(self.inner.ctx.events.as_ref(), vec![target_calendar.id]);
        self.inner.reminders.notify_changed();
        Ok(())
    }

    /* ───────────────────────── contacts ───────────────────────────── */

    pub async fn addressbook_list(&self) -> Result<Vec<nuncio_core::models::AddressBook>> {
        self.inner.contacts_service.addressbook_list().await
    }

    pub async fn contact_list(
        &self,
        query: nuncio_core::models::ContactListQuery,
    ) -> Result<nuncio_core::models::ContactPage> {
        self.inner.contacts_service.contact_list(query).await
    }

    pub async fn contact_get(&self, id: &str) -> Result<nuncio_core::models::Contact> {
        self.inner.contacts_service.contact_get(id).await
    }

    pub async fn contact_create(
        &self,
        contact: nuncio_core::models::Contact,
    ) -> Result<nuncio_core::models::Contact> {
        self.inner.contacts_service.contact_create(contact).await
    }

    pub async fn contact_update(
        &self,
        contact: nuncio_core::models::Contact,
    ) -> Result<nuncio_core::models::Contact> {
        self.inner.contacts_service.contact_update(contact).await
    }

    pub async fn contact_delete(&self, ids: Vec<String>) -> Result<()> {
        self.inner.contacts_service.contact_delete(ids).await
    }

    pub async fn contact_set_favorite(&self, id: &str, value: bool) -> Result<()> {
        self.inner
            .contacts_service
            .contact_set_favorite(id, value)
            .await
    }

    pub async fn contact_find_by_email(
        &self,
        email: &str,
    ) -> Result<Option<nuncio_core::models::Contact>> {
        self.inner
            .contacts_service
            .contact_find_by_email(email)
            .await
    }

    pub async fn group_list(
        &self,
        address_book_id: Option<String>,
    ) -> Result<Vec<nuncio_core::models::ContactGroup>> {
        self.inner
            .contacts_service
            .group_list(address_book_id)
            .await
    }

    pub async fn group_create(
        &self,
        address_book_id: &str,
        name: &str,
    ) -> Result<nuncio_core::models::ContactGroup> {
        self.inner
            .contacts_service
            .group_create(address_book_id, name)
            .await
    }

    pub async fn group_rename(
        &self,
        id: &str,
        name: &str,
    ) -> Result<nuncio_core::models::ContactGroup> {
        self.inner.contacts_service.group_rename(id, name).await
    }

    pub async fn group_delete(&self, id: &str) -> Result<()> {
        self.inner.contacts_service.group_delete(id).await
    }

    pub async fn group_set_members(&self, group_id: &str, contact_ids: Vec<String>) -> Result<()> {
        self.inner
            .contacts_service
            .group_set_members(group_id, contact_ids)
            .await
    }

    pub async fn contact_import_vcf(&self, path: &str, address_book_id: &str) -> Result<u32> {
        self.inner
            .contacts_service
            .contact_import_vcf(path, address_book_id)
            .await
    }

    pub async fn contact_export_vcf(&self, ids: Vec<String>, path: &str) -> Result<()> {
        self.inner
            .contacts_service
            .contact_export_vcf(ids, path)
            .await
    }

    /* ─────────────────────────── tasks ────────────────────────────── */

    pub async fn tasklist_list(&self) -> Result<Vec<nuncio_core::models::TaskList>> {
        self.inner.tasks_service.tasklist_list().await
    }

    pub async fn tasklist_create(
        &self,
        account_id: &str,
        name: &str,
        color: &str,
    ) -> Result<nuncio_core::models::TaskList> {
        self.inner
            .tasks_service
            .tasklist_create(account_id, name, color)
            .await
    }

    pub async fn tasklist_update(
        &self,
        id: &str,
        name: Option<&str>,
        color: Option<&str>,
    ) -> Result<nuncio_core::models::TaskList> {
        self.inner
            .tasks_service
            .tasklist_update(id, name, color)
            .await
    }

    pub async fn tasklist_delete(&self, id: &str) -> Result<()> {
        self.inner.tasks_service.tasklist_delete(id).await
    }

    pub async fn task_smart_counts(
        &self,
        today: String,
    ) -> Result<nuncio_core::models::SmartListCounts> {
        self.inner.tasks_service.task_smart_counts(today).await
    }

    pub async fn task_query(
        &self,
        query: nuncio_core::models::TaskQuery,
    ) -> Result<Vec<nuncio_core::models::Task>> {
        self.inner.tasks_service.task_query(query).await
    }

    pub async fn task_get(&self, id: &str) -> Result<nuncio_core::models::Task> {
        self.inner.tasks_service.task_get(id).await
    }

    pub async fn task_create(
        &self,
        list_id: &str,
        input: nuncio_core::models::TaskInput,
    ) -> Result<nuncio_core::models::Task> {
        self.inner.tasks_service.task_create(list_id, input).await
    }

    pub async fn task_update(
        &self,
        id: &str,
        input: nuncio_core::models::TaskInput,
    ) -> Result<nuncio_core::models::Task> {
        self.inner.tasks_service.task_update(id, input).await
    }

    pub async fn task_set_completed(
        &self,
        id: &str,
        value: bool,
    ) -> Result<nuncio_core::models::Task> {
        self.inner.tasks_service.task_set_completed(id, value).await
    }

    pub async fn task_set_importance(
        &self,
        id: &str,
        importance: nuncio_core::models::Importance,
    ) -> Result<nuncio_core::models::Task> {
        self.inner
            .tasks_service
            .task_set_importance(id, importance)
            .await
    }

    pub async fn task_set_my_day(
        &self,
        id: &str,
        date: Option<&str>,
    ) -> Result<nuncio_core::models::Task> {
        self.inner.tasks_service.task_set_my_day(id, date).await
    }

    pub async fn task_move(&self, id: &str, list_id: &str) -> Result<nuncio_core::models::Task> {
        self.inner.tasks_service.task_move(id, list_id).await
    }

    pub async fn task_reorder(&self, list_id: &str, ids: Vec<String>) -> Result<()> {
        self.inner.tasks_service.task_reorder(list_id, ids).await
    }

    pub async fn task_delete(&self, id: &str) -> Result<()> {
        self.inner.tasks_service.task_delete(id).await
    }

    pub async fn task_search(&self, text: &str) -> Result<Vec<nuncio_core::models::Task>> {
        self.inner.tasks_service.task_search(text).await
    }

    pub async fn message_create_task(
        &self,
        message_id: &str,
        options: nuncio_core::models::NewTaskFromMessage,
    ) -> Result<nuncio_core::models::Task> {
        self.inner
            .tasks_service
            .message_create_task(message_id, options)
            .await
    }

    /* ───────────────────────── pending operations ─────────────────── */

    async fn enqueue_op(&self, account_id: &str, op: &PimOp, description: String) -> Result<()> {
        let payload = ops::to_value(op)?;
        let new_op = NewPendingOp::new(account_id, op.kind(), payload, description);
        self.inner
            .ctx
            .db
            .write(move |conn| pending_ops::enqueue(conn, new_op))
            .await?;
        self.inner.ops_notify.notify_one();
        Ok(())
    }

    async fn op_worker(self) {
        loop {
            if self.inner.shutdown.load(Ordering::SeqCst) {
                break;
            }
            if let Err(error) = self.process_pending_ops().await {
                warn!(%error, "pim op worker pass failed");
            }
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                _ = self.inner.ops_notify.notified() => {}
            }
        }
    }

    async fn process_pending_ops(&self) -> Result<()> {
        let accounts = self.inner.ctx.db.read(repo::accounts::list).await?;
        let now = Utc::now();
        let mut processed = 0u32;
        for account in accounts {
            if account.provider.is_local() {
                continue;
            }
            let account_id = account.id.clone();
            let ops = self
                .inner
                .ctx
                .db
                .read(move |conn| pending_ops::list(conn, Some(account_id.as_str())))
                .await?;
            for op in ops {
                if processed >= 20 {
                    return Ok(());
                }
                if op.state != PendingOpState::Pending || !ops::is_calendar_kind(&op.kind) {
                    continue;
                }
                if op.next_retry_at.is_some_and(|retry| retry > now) {
                    continue;
                }
                processed += 1;
                let op_id = op.id.clone();
                let _ = self
                    .inner
                    .ctx
                    .db
                    .write({
                        let id = op_id.clone();
                        move |conn| pending_ops::mark_in_progress(conn, &id)
                    })
                    .await;
                match self.execute_op(&op).await {
                    Ok(()) => {
                        let id = op_id.clone();
                        let _ = self
                            .inner
                            .ctx
                            .db
                            .write(move |conn| pending_ops::mark_done(conn, &id))
                            .await;
                    }
                    Err(error) => {
                        self.handle_op_failure(&op, error).await;
                    }
                }
            }
        }
        Ok(())
    }

    async fn execute_op(&self, op: &PendingOp) -> Result<()> {
        let decoded = ops::decode(op)?;
        let account = self.load_account(&op.account_id).await?;
        if !self.is_provider(&account) {
            return Ok(());
        }
        let Some(provider) = self.provider_for(&account) else {
            return Ok(());
        };

        match &decoded {
            PimOp::CalendarCreate { calendar_id } => {
                if self.mapping_for(calendar_id)?.is_some() {
                    return Ok(());
                }
                let calendar = self.load_calendar(calendar_id).await?;
                let time_zone = "UTC";
                let remote = provider
                    .create_calendar(&account, &calendar.name, &calendar.color, time_zone)
                    .await?;
                let mapping = CalendarRemote::new(
                    calendar.id.clone(),
                    account.id.clone(),
                    provider.name(),
                    remote.remote_id,
                    remote.href,
                );
                self.inner
                    .ctx
                    .db
                    .write(move |conn| store::set(conn, &mapping))
                    .await?;
            }
            PimOp::CalendarUpdate {
                calendar_id,
                previous_name: _,
                previous_color: _,
            } => {
                let calendar = self.load_calendar(calendar_id).await?;
                let Some(mapping) = self.mapping_for(&calendar.id)? else {
                    return Err(AppError::conflict("calendar is not mapped yet"));
                };
                let remote = self.remote_for(&calendar, &mapping);
                provider
                    .update_calendar(
                        &account,
                        &remote,
                        Some(&calendar.name),
                        Some(&calendar.color),
                    )
                    .await?;
            }
            PimOp::CalendarDelete { snapshot, .. } => {
                if let Some(remote) = &snapshot.remote {
                    let remote_calendar = RemoteCalendar {
                        local_id: snapshot.calendar.id.clone(),
                        remote_id: remote.remote_id.clone(),
                        href: remote.href.clone(),
                        name: snapshot.calendar.name.clone(),
                        color: Some(snapshot.calendar.color.clone()),
                        read_only: snapshot.calendar.is_read_only,
                        sync_token: None,
                        ctag: None,
                        supports_sync_collection: true,
                    };
                    provider.delete_calendar(&account, &remote_calendar).await?;
                }
            }
            PimOp::EventCreate {
                event_id,
                notify_attendees,
            } => {
                let event = self.load_event(event_id).await?;
                if event.remote_id.is_some() || event.href.is_some() {
                    return Ok(());
                }
                let calendar = self.load_calendar(&event.event.calendar_id).await?;
                let Some(mapping) = self.mapping_for(&calendar.id)? else {
                    return Err(AppError::conflict("calendar is not mapped yet"));
                };
                let remote = self.remote_for(&calendar, &mapping);
                let mut created_series = false;
                if backend_for(&account, DavService::Calendar) == Backend::Dav {
                    let calendar_id = event.event.calendar_id.clone();
                    let uid = event.event.uid.clone();
                    let series = self
                        .inner
                        .ctx
                        .db
                        .read(move |conn| series_events(conn, &calendar_id, &uid))
                        .await?;
                    if series.len() > 1 {
                        let client = DavClient::new(self.inner.http.clone(), account.clone());
                        client.create_series(&remote, &series).await?;
                        created_series = true;
                    }
                }
                if !created_series {
                    provider
                        .create_event(&account, &remote, &event.event, *notify_attendees)
                        .await?;
                }
                // Resolve the provider id right away so queued follow-up
                // operations (recurrence exceptions, attendees) can address
                // the new event without waiting for the next sync.
                self.persist_created_remote_id(&account, &remote, &event)
                    .await;
                if *notify_attendees && backend_for(&account, DavService::Calendar) == Backend::Dav
                {
                    self.send_itip_request(&account, &event.event, false).await;
                }
            }
            PimOp::EventUpdate {
                event_id,
                scope,
                instance_start,
                notify_attendees,
                following_id,
                ..
            } => {
                let (current, series, calendar) = self.load_series_context(event_id).await?;
                let Some(mapping) = self.mapping_for(&calendar.id)? else {
                    return Err(AppError::conflict("calendar is not mapped yet"));
                };
                let remote = self.remote_for(&calendar, &mapping);
                let updated = match scope {
                    RecurrenceEditScope::All => current.event.clone(),
                    RecurrenceEditScope::This => {
                        let instance = instance_start.as_deref().ok_or_else(|| {
                            invalid("instance start is required for a single-occurrence update")
                        })?;
                        series
                            .iter()
                            .find(|stored| {
                                stored
                                    .event
                                    .recurrence_id
                                    .as_deref()
                                    .is_some_and(|value| same_instant(value, instance))
                            })
                            .map(|stored| stored.event.clone())
                            .ok_or_else(|| invalid("occurrence override not found"))?
                    }
                    RecurrenceEditScope::ThisAndFollowing => {
                        let following_id = following_id
                            .as_deref()
                            .ok_or_else(|| invalid("following event id missing"))?;
                        self.load_event(following_id).await?.event
                    }
                };
                provider
                    .update_event(
                        &account,
                        &remote,
                        &current,
                        &series,
                        &updated,
                        *scope,
                        instance_start.as_deref(),
                        *notify_attendees,
                    )
                    .await?;
                if *notify_attendees && backend_for(&account, DavService::Calendar) == Backend::Dav
                {
                    // CalDAV providers do not send iTIP themselves; deliver
                    // REQUEST/UPDATE to attendees over the account's mail.
                    if *scope == RecurrenceEditScope::This {
                        self.send_itip_request(&account, &updated, true).await;
                    } else {
                        self.send_itip_request(&account, &current.event, true).await;
                    }
                }
            }
            PimOp::EventDelete {
                event_id,
                scope,
                instance_start,
                notify_attendees,
                snapshot,
            } => {
                let (current, series, calendar) = self
                    .load_series_context_or_snapshot(event_id, snapshot)
                    .await?;
                let Some(mapping) = self.mapping_for(&calendar.id)? else {
                    return Ok(());
                };
                let remote = self.remote_for(&calendar, &mapping);
                provider
                    .delete_event(
                        &account,
                        &remote,
                        &current,
                        &series,
                        *scope,
                        instance_start.as_deref(),
                        *notify_attendees,
                    )
                    .await?;
                if *notify_attendees && backend_for(&account, DavService::Calendar) == Backend::Dav
                {
                    self.send_itip_cancel(&account, &current.event).await;
                }
            }
            PimOp::EventRespond {
                event_id,
                response,
                comment,
                send_reply,
                snapshot,
            } => {
                let (current, series, calendar) = self
                    .load_series_context_or_snapshot(event_id, snapshot)
                    .await?;
                if current.remote_id.is_none()
                    && backend_for(&account, DavService::Calendar) != Backend::Dav
                {
                    return Err(AppError::conflict("event is not synced yet"));
                }
                if current.remote_id.is_none() && current.href.is_none() {
                    return Err(AppError::conflict("event is not synced yet"));
                }
                let Some(mapping) = self.mapping_for(&calendar.id)? else {
                    return Err(AppError::conflict("calendar is not mapped yet"));
                };
                let remote = self.remote_for(&calendar, &mapping);
                provider
                    .respond_event(
                        &account,
                        &remote,
                        &current,
                        &series,
                        *response,
                        comment.as_deref(),
                        *send_reply,
                    )
                    .await?;
                if *send_reply
                    && account.mail_enabled
                    && !matches!(
                        backend_for(&account, DavService::Calendar),
                        Backend::Google | Backend::Graph
                    )
                    && current.event.organizer.is_some()
                {
                    self.send_itip_reply(&account, &current, *response, comment.as_deref())
                        .await?;
                }
            }
        }

        let calendar_id = self.op_calendar_id(&decoded).await?;
        if let Some(calendar_id) = calendar_id {
            emit_calendar_changed(self.inner.ctx.events.as_ref(), vec![calendar_id]);
        }
        self.inner.reminders.notify_changed();
        self.spawn_sync_after_write(account.id);
        Ok(())
    }

    async fn op_calendar_id(&self, op: &PimOp) -> Result<Option<String>> {
        match op {
            PimOp::CalendarCreate { calendar_id }
            | PimOp::CalendarUpdate { calendar_id, .. }
            | PimOp::CalendarDelete { calendar_id, .. } => Ok(Some(calendar_id.clone())),
            PimOp::EventCreate { event_id, .. }
            | PimOp::EventUpdate { event_id, .. }
            | PimOp::EventDelete { event_id, .. }
            | PimOp::EventRespond { event_id, .. } => {
                match self
                    .inner
                    .ctx
                    .db
                    .read({
                        let id = event_id.clone();
                        move |conn| events_repo::get(conn, &id)
                    })
                    .await?
                {
                    Some(stored) => Ok(Some(stored.event.calendar_id)),
                    None => Ok(None),
                }
            }
        }
    }

    async fn handle_op_failure(&self, op: &PendingOp, error: AppError) {
        let retryable = matches!(
            &error,
            AppError::Network(_) | AppError::Service(_) | AppError::Conflict(_) | AppError::Io(_)
        ) && op.attempts < 5;
        let auth = matches!(&error, AppError::Auth(_));
        if auth {
            self.inner
                .ctx
                .sync_status
                .needs_reauth(&op.account_id, SyncModule::Calendar);
        }
        if retryable || auth {
            if auth && op.attempts >= 5 {
                self.fail_op(op, &error).await;
                return;
            }
            let op_id = op.id.clone();
            let message = error.message();
            let now = Utc::now();
            let _ = self
                .inner
                .ctx
                .db
                .write(move |conn| pending_ops::mark_retry(conn, &op_id, &message, now))
                .await;
            return;
        }
        self.fail_op(op, &error).await;
    }

    async fn fail_op(&self, op: &PendingOp, error: &AppError) {
        let op_id = op.id.clone();
        let message = error.message();
        let _ = self
            .inner
            .ctx
            .db
            .write(move |conn| pending_ops::mark_failed(conn, &op_id, &message))
            .await;
        if let Ok(decoded) = ops::decode(op) {
            let _ = self.rollback_op(&decoded).await;
        }
        emit_op_failed(
            self.inner.ctx.events.as_ref(),
            op.account_id.clone(),
            &op.description,
            &error.message(),
        );
        self.inner.reminders.notify_changed();
    }

    async fn rollback_op(&self, op: &PimOp) -> Result<()> {
        match op {
            PimOp::EventCreate { event_id, .. } => {
                let id = event_id.clone();
                self.inner
                    .ctx
                    .db
                    .write(move |conn| {
                        if let Some(stored) = events_repo::get(conn, &id)? {
                            delete_stored(conn, &stored)?;
                        }
                        Ok(())
                    })
                    .await
            }
            PimOp::EventUpdate {
                snapshot,
                following_id,
                ..
            } => {
                let restore = ops::restore_events(snapshot);
                let following = following_id.clone();
                self.inner
                    .ctx
                    .db
                    .write(move |conn| {
                        if let Some(following) = &following {
                            events_repo::delete(conn, following)?;
                        }
                        if let Some(first) = restore.first() {
                            let calendar_id = first.event.calendar_id.clone();
                            let uids: std::collections::HashSet<String> = restore
                                .iter()
                                .map(|stored| stored.event.uid.clone())
                                .collect();
                            let ids: std::collections::HashSet<String> = restore
                                .iter()
                                .map(|stored| stored.event.id.clone())
                                .collect();
                            let current = events_repo::list_for_calendar(conn, &calendar_id)?;
                            for stored in current {
                                if uids.contains(&stored.event.uid)
                                    && !ids.contains(&stored.event.id)
                                {
                                    events_repo::delete(conn, &stored.event.id)?;
                                }
                            }
                        }
                        for stored in &restore {
                            events_repo::upsert(conn, stored)?;
                        }
                        Ok(())
                    })
                    .await
            }
            PimOp::EventDelete { snapshot, .. } | PimOp::EventRespond { snapshot, .. } => {
                let restore = ops::restore_events(snapshot);
                self.inner
                    .ctx
                    .db
                    .write(move |conn| {
                        for stored in &restore {
                            events_repo::upsert(conn, stored)?;
                        }
                        Ok(())
                    })
                    .await
            }
            PimOp::CalendarCreate { calendar_id } => {
                let id = calendar_id.clone();
                self.inner
                    .ctx
                    .db
                    .write(move |conn| {
                        let _ = repo::calendars::delete(conn, &id);
                        Ok(())
                    })
                    .await
            }
            PimOp::CalendarUpdate {
                calendar_id,
                previous_name,
                previous_color,
            } => {
                let id = calendar_id.clone();
                let name = previous_name.clone();
                let color = previous_color.clone();
                self.inner
                    .ctx
                    .db
                    .write(move |conn| {
                        if repo::calendars::get(conn, &id)?.is_some() {
                            repo::calendars::update(conn, &id, Some(&name), Some(&color))?;
                        }
                        Ok(())
                    })
                    .await
            }
            PimOp::CalendarDelete { snapshot, .. } => {
                let calendar = snapshot.calendar.clone();
                let remote = snapshot.remote.clone();
                let events = ops::restore_events(&snapshot.events);
                self.inner
                    .ctx
                    .db
                    .write(move |conn| {
                        let mut calendar = calendar;
                        repo::calendars::insert(conn, &mut calendar)?;
                        if let Some(remote) = &remote {
                            store::set(conn, remote)?;
                        }
                        for stored in &events {
                            events_repo::upsert(conn, stored)?;
                        }
                        Ok(())
                    })
                    .await
            }
        }
    }

    fn spawn_sync_after_write(&self, account_id: String) {
        let service = self.clone();
        tokio::spawn(async move {
            if let Err(error) = service.sync_calendar_account(&account_id, false).await {
                debug!(account_id = %account_id, %error, "post-write sync failed");
            }
        });
    }

    /* ───────────────────────────── helpers ─────────────────────────── */

    async fn load_account(&self, account_id: &str) -> Result<Account> {
        self.inner
            .ctx
            .db
            .read({
                let id = account_id.to_string();
                move |conn| repo::accounts::get_required(conn, &id)
            })
            .await
    }

    async fn load_calendar(&self, calendar_id: &str) -> Result<Calendar> {
        self.inner
            .ctx
            .db
            .read({
                let id = calendar_id.to_string();
                move |conn| repo::calendars::get_required(conn, &id)
            })
            .await
    }

    async fn load_event(&self, event_id: &str) -> Result<StoredEvent> {
        self.inner
            .ctx
            .db
            .read({
                let id = event_id.to_string();
                move |conn| events_repo::get_required(conn, &id)
            })
            .await
    }

    async fn load_series_context(
        &self,
        event_id: &str,
    ) -> Result<(StoredEvent, Vec<StoredEvent>, Calendar)> {
        let stored = self.load_event(event_id).await?;
        let master = if let Some(series_id) = &stored.event.series_id {
            self.load_event(series_id).await.unwrap_or(stored.clone())
        } else {
            stored.clone()
        };
        let calendar = self.load_calendar(&master.event.calendar_id).await?;
        let calendar_id = master.event.calendar_id.clone();
        let uid = master.event.uid.clone();
        let series = self
            .inner
            .ctx
            .db
            .read(move |conn| series_events(conn, &calendar_id, &uid))
            .await?;
        Ok((master, series, calendar))
    }

    async fn load_series_context_or_snapshot(
        &self,
        event_id: &str,
        snapshot: &[EventSnapshot],
    ) -> Result<(StoredEvent, Vec<StoredEvent>, Calendar)> {
        if let Ok(context) = self.load_series_context(event_id).await {
            return Ok(context);
        }
        let restore = ops::restore_events(snapshot);
        let master = restore
            .iter()
            .find(|stored| stored.event.series_id.is_none())
            .or_else(|| restore.first())
            .cloned()
            .ok_or_else(|| AppError::not_found("event snapshot is empty"))?;
        let calendar = self.load_calendar(&master.event.calendar_id).await?;
        Ok((master, restore, calendar))
    }

    async fn validate_input(&self, input: &EventInput) -> Result<(Calendar, Account)> {
        let calendar = self.load_calendar(&input.calendar_id).await?;
        if calendar.is_read_only || calendar.subscription_url.is_some() {
            return Err(invalid("calendar is read-only"));
        }
        let account = self.load_account(&calendar.account_id).await?;
        if !account.calendar_enabled {
            return Err(invalid("calendar is disabled for this account"));
        }
        if backend_for(&account, DavService::Calendar) == Backend::Unsupported {
            return Err(unsupported("this account has no calendar provider"));
        }
        if input.all_day {
            let start = date_part(&input.start)
                .ok_or_else(|| invalid("all-day event start must be a date"))?;
            let end =
                date_part(&input.end).ok_or_else(|| invalid("all-day event end must be a date"))?;
            if end <= start {
                return Err(invalid("event end must be after the start"));
            }
        } else {
            let start = parse_instant(&input.start)?;
            let end = parse_instant(&input.end)?;
            if end <= start {
                return Err(invalid("event end must be after the start"));
            }
        }
        if input.title.trim().is_empty() {
            return Err(invalid("event title must not be empty"));
        }
        Ok((calendar, account))
    }

    async fn find_account_calendar(&self, account_id: &str) -> Result<Option<Calendar>> {
        let account_id = account_id.to_string();
        self.inner
            .ctx
            .db
            .read(move |conn| {
                let calendars = repo::calendars::list_for_account(conn, &account_id)?;
                Ok(calendars
                    .into_iter()
                    .find(|calendar| !calendar.is_read_only && calendar.subscription_url.is_none()))
            })
            .await
    }

    async fn find_event_by_uid_account(
        &self,
        account_id: &str,
        uid: &str,
    ) -> Result<Option<StoredEvent>> {
        let account_id = account_id.to_string();
        let uid = uid.to_string();
        self.inner
            .ctx
            .db
            .read(move |conn| {
                let calendars = repo::calendars::list_for_account(conn, &account_id)?;
                for calendar in calendars {
                    if let Some(stored) = events_repo::by_uid(conn, &calendar.id, &uid)? {
                        return Ok(Some(stored));
                    }
                }
                Ok(None)
            })
            .await
    }

    /// Look up an invitation on the provider so the response can be sent via
    /// the calendar API.  Returns `(remote_id, etag)`.
    async fn find_remote_invite(
        &self,
        account: &Account,
        uid: &str,
        calendar: &mut Calendar,
    ) -> Result<Option<(Option<String>, Option<String>)>> {
        match backend_for(account, DavService::Calendar) {
            Backend::Google => {
                let client = crate::google::GoogleClient::new(self.inner.http.clone());
                let mappings = self.mappings_for_account(&account.id).await?;
                for mapping in mappings
                    .iter()
                    .filter(|mapping| mapping.provider == "google")
                {
                    if let Some(found) = client
                        .find_by_ical_uid(&account.id, &mapping.remote_id, uid)
                        .await?
                    {
                        if let Some(local) = self
                            .inner
                            .ctx
                            .db
                            .read({
                                let id = mapping.calendar_id.clone();
                                move |conn| repo::calendars::get(conn, &id)
                            })
                            .await?
                        {
                            *calendar = local;
                        }
                        return Ok(Some((Some(found.id), found.etag)));
                    }
                }
                Ok(None)
            }
            Backend::Graph => {
                let client = crate::graph::GraphClient::new(self.inner.http.clone());
                match client.find_by_ical_uid(&account.id, uid).await? {
                    Some(found) => Ok(Some((Some(found.id), found.change_key))),
                    None => Ok(None),
                }
            }
            _ => Ok(None),
        }
    }

    async fn mappings_for_account(&self, account_id: &str) -> Result<Vec<CalendarRemote>> {
        let id = account_id.to_string();
        self.inner
            .ctx
            .db
            .read(move |conn| store::list_for_account(conn, &id))
            .await
    }

    /// Persist the provider id of a freshly created event immediately.
    ///
    /// Queued operations for the same series (imported exceptions) run right
    /// after the create and need the remote id, which otherwise only arrives
    /// with the next sync.
    async fn persist_created_remote_id(
        &self,
        account: &Account,
        calendar: &RemoteCalendar,
        stored: &StoredEvent,
    ) {
        let found = match backend_for(account, DavService::Calendar) {
            Backend::Google => {
                let client = crate::google::GoogleClient::new(self.inner.http.clone());
                match client
                    .find_by_ical_uid(&account.id, &calendar.remote_id, &stored.event.uid)
                    .await
                {
                    Ok(Some(found)) => Some((Some(found.id), found.etag)),
                    Ok(None) => None,
                    Err(error) => {
                        debug!(%error, "could not resolve new Google event id");
                        None
                    }
                }
            }
            Backend::Graph => {
                let client = crate::graph::GraphClient::new(self.inner.http.clone());
                match client
                    .find_by_ical_uid(&account.id, &stored.event.uid)
                    .await
                {
                    Ok(Some(found)) => Some((Some(found.id), found.change_key)),
                    Ok(None) => None,
                    Err(error) => {
                        debug!(%error, "could not resolve new Graph event id");
                        None
                    }
                }
            }
            _ => None,
        };
        let Some((remote_id, etag)) = found else {
            return;
        };
        let Some(remote_id) = remote_id else {
            return;
        };
        let id = stored.event.id.clone();
        if let Err(error) = self
            .inner
            .ctx
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE event SET remote_id = ?2, etag = COALESCE(?3, etag), updated_at = ?4
                     WHERE id = ?1",
                    rusqlite::params![id, remote_id, etag, repo::now_string()],
                )?;
                Ok(())
            })
            .await
        {
            debug!(%error, "could not store new event remote id");
        }
    }

    /// Send an iTIP REQUEST/UPDATE for a CalDAV event.
    async fn send_itip_request(&self, account: &Account, event: &CalendarEvent, update: bool) {
        if !account.mail_enabled
            || account.provider.is_local()
            || !itip_can_organize(event, &account.email)
        {
            return;
        }
        let recipients = itip_recipients(event, &account.email);
        if recipients.is_empty() {
            return;
        }
        let ics = nuncio_ical::itip::build_request(event);
        let from = Address::with_name(&account.display_name, &account.email);
        let subject = format!(
            "{}{}",
            if update { "Updated: " } else { "Invitation: " },
            event.title
        );
        let body = if update {
            format!("会议已更新：{}", event.title)
        } else {
            format!("邀请您参加：{}", event.title)
        };
        let raw =
            nuncio_ical::itip::itip_email(&from, &recipients, &subject, &body, &ics, "REQUEST");
        if let Err(error) = self
            .inner
            .mail_sender
            .send_raw(&account.id, raw, true)
            .await
        {
            warn!(%error, event = %event.id, "could not send iTIP REQUEST");
        }
    }

    /// Send an iTIP CANCEL for a deleted CalDAV event.
    async fn send_itip_cancel(&self, account: &Account, event: &CalendarEvent) {
        if !account.mail_enabled
            || account.provider.is_local()
            || !itip_can_organize(event, &account.email)
        {
            return;
        }
        let recipients = itip_recipients(event, &account.email);
        if recipients.is_empty() {
            return;
        }
        let ics = nuncio_ical::itip::build_cancel(event);
        let from = Address::with_name(&account.display_name, &account.email);
        let subject = format!("Cancelled: {}", event.title);
        let body = format!("会议已取消：{}", event.title);
        let raw =
            nuncio_ical::itip::itip_email(&from, &recipients, &subject, &body, &ics, "CANCEL");
        if let Err(error) = self
            .inner
            .mail_sender
            .send_raw(&account.id, raw, true)
            .await
        {
            warn!(%error, event = %event.id, "could not send iTIP CANCEL");
        }
    }

    async fn send_itip_reply(
        &self,
        account: &Account,
        event: &StoredEvent,
        response: AttendeeStatus,
        comment: Option<&str>,
    ) -> Result<()> {
        if !account.mail_enabled || account.provider.is_local() {
            return Ok(());
        }
        let Some(organizer) = event.event.organizer.as_ref() else {
            return Ok(());
        };
        if organizer.email.trim().is_empty() {
            return Ok(());
        }
        let ics = nuncio_ical::itip::build_reply(&event.event, &account.email, response, comment);
        let from = Address::with_name(&account.display_name, &account.email);
        let to = vec![Address::new(&organizer.email)];
        let subject = format!("Re: {}", event.event.title);
        let body = comment
            .map(str::to_string)
            .unwrap_or_else(|| format!("Meeting response: {}", response.as_str()));
        let raw = nuncio_ical::itip::itip_email(&from, &to, &subject, &body, &ics, "REPLY");
        self.inner
            .mail_sender
            .send_raw(&account.id, raw, true)
            .await
    }

    /// DAV connectivity test used by the app's `account_test` command.
    ///
    /// Tests the CalDAV and/or CardDAV home discovered from the account
    /// configuration; returns `true` when at least one service answered and no
    /// configured service failed.
    pub async fn test_dav(&self, account: &Account) -> Result<bool> {
        let mut tested = false;
        if account.caldav_url.is_some() {
            let client = DavClient::new(self.inner.http.clone(), account.clone());
            client.discover_home(DavKind::Calendar).await?;
            tested = true;
        }
        if account.carddav_url.is_some() {
            let client = DavClient::for_service(
                self.inner.http.clone(),
                account.clone(),
                DavService::Contacts,
            );
            client.discover_home(DavKind::AddressBook).await?;
            tested = true;
        }
        Ok(tested)
    }

    /// CardDAV-only connectivity test (used when adding an address book).
    pub async fn test_carddav(&self, account: &Account) -> Result<bool> {
        if account.carddav_url.is_none() {
            return Ok(false);
        }
        let client = DavClient::for_service(
            self.inner.http.clone(),
            account.clone(),
            DavService::Contacts,
        );
        client
            .discover_home(DavKind::AddressBook)
            .await
            .map(|_| true)
    }

    /// Contact/task sync is implemented in stage 2; expose the account list
    /// helper so that layer can reuse it.
    pub async fn accounts(&self) -> Result<Vec<Account>> {
        self.inner.ctx.db.read(repo::accounts::list).await
    }
}

/* ───────────────────────────── free helpers ───────────────────────────── */

/// One VEVENT written by `calendar_import_ics`.
struct ImportedComponent {
    id: String,
    uid: String,
    was_existing: bool,
    recurrence_id: Option<String>,
    cancelled: bool,
    was_master: bool,
}

struct ImportedGroup {
    uid: String,
    master_id: Option<String>,
    master_existing: bool,
    overrides: Vec<ImportedComponent>,
}

fn group_imported_components(components: Vec<ImportedComponent>) -> Vec<ImportedGroup> {
    let mut groups: Vec<ImportedGroup> = Vec::new();
    for component in components {
        let index = match groups.iter().position(|group| group.uid == component.uid) {
            Some(index) => index,
            None => {
                groups.push(ImportedGroup {
                    uid: component.uid.clone(),
                    master_id: None,
                    master_existing: false,
                    overrides: Vec::new(),
                });
                groups.len() - 1
            }
        };
        let group = &mut groups[index];
        if component.was_master {
            group.master_id = Some(component.id.clone());
            group.master_existing = component.was_existing;
        } else {
            group.overrides.push(component);
        }
    }
    groups
}

fn itip_can_organize(event: &CalendarEvent, self_email: &str) -> bool {
    match &event.organizer {
        None => event.is_organizer,
        Some(organizer) => {
            organizer.email.trim().is_empty() || organizer.email.eq_ignore_ascii_case(self_email)
        }
    }
}

fn itip_recipients(event: &CalendarEvent, self_email: &str) -> Vec<Address> {
    event
        .attendees
        .iter()
        .filter(|attendee| {
            !attendee.email.trim().is_empty() && !attendee.email.eq_ignore_ascii_case(self_email)
        })
        .map(|attendee| Address::with_name(attendee.name.as_deref().unwrap_or(""), &attendee.email))
        .collect()
}

fn create_remote_calendar(
    conn: &rusqlite::Connection,
    account_id: &str,
    remote: &RemoteCalendar,
) -> Result<Calendar> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM calendar WHERE account_id = ?1",
        rusqlite::params![account_id],
        |row| row.get(0),
    )?;
    let mut calendar = Calendar {
        id: new_id(),
        account_id: account_id.to_string(),
        name: remote.name.clone(),
        color: remote
            .color
            .as_deref()
            .and_then(normalize_color)
            .unwrap_or_else(|| "#0078D4".to_string()),
        is_read_only: remote.read_only,
        is_visible: true,
        is_default: count == 0,
        subscription_url: None,
    };
    repo::calendars::insert(conn, &mut calendar)
}

fn delete_stored(conn: &rusqlite::Connection, stored: &StoredEvent) -> Result<()> {
    if stored.event.series_id.is_none() {
        events_repo::delete_series(conn, &stored.event.id)?;
    } else {
        events_repo::delete(conn, &stored.event.id)?;
    }
    Ok(())
}

fn find_existing_event(
    conn: &rusqlite::Connection,
    calendar_id: &str,
    remote: &RemoteEvent,
) -> Result<Option<StoredEvent>> {
    if let Some(remote_id) = &remote.remote_id {
        if let Some(stored) = events_repo::by_remote_id(conn, calendar_id, remote_id)? {
            return Ok(Some(stored));
        }
    }
    if let Some(href) = &remote.href {
        let all = events_repo::list_for_calendar(conn, calendar_id)?;
        let by_href: Vec<&StoredEvent> = all
            .iter()
            .filter(|stored| stored.href.as_deref() == Some(href.as_str()))
            .collect();
        if let Some(recurrence_id) = &remote.event.recurrence_id {
            if let Some(found) = by_href.iter().find(|stored| {
                stored
                    .event
                    .recurrence_id
                    .as_deref()
                    .is_some_and(|value| same_instant(value, recurrence_id))
            }) {
                return Ok(Some((*found).clone()));
            }
        } else if let Some(found) = by_href
            .iter()
            .find(|stored| stored.event.series_id.is_none())
            .or_else(|| by_href.first())
        {
            return Ok(Some((*found).clone()));
        }
    }
    let all = events_repo::list_for_calendar(conn, calendar_id)?;
    let candidate = all.into_iter().find(|stored| {
        stored.event.uid == remote.event.uid
            && match (&remote.event.recurrence_id, &stored.event.recurrence_id) {
                (Some(left), Some(right)) => same_instant(left, right),
                (None, None) => true,
                _ => false,
            }
    });
    Ok(candidate)
}

fn find_event_by_uid(
    conn: &rusqlite::Connection,
    calendar_id: &str,
    uid: &str,
    recurrence_id: Option<&str>,
) -> Result<Option<StoredEvent>> {
    let all = events_repo::list_for_calendar(conn, calendar_id)?;
    Ok(all.into_iter().find(|stored| {
        stored.event.uid == uid
            && match (recurrence_id, stored.event.recurrence_id.as_deref()) {
                (Some(left), Some(right)) => same_instant(left, right),
                (None, None) => true,
                _ => false,
            }
    }))
}

fn find_master_by_uid(
    conn: &rusqlite::Connection,
    calendar_id: &str,
    uid: &str,
) -> Result<Option<StoredEvent>> {
    let all = events_repo::list_for_calendar(conn, calendar_id)?;
    Ok(all
        .into_iter()
        .find(|stored| stored.event.uid == uid && stored.event.series_id.is_none()))
}

fn series_events(
    conn: &rusqlite::Connection,
    calendar_id: &str,
    uid: &str,
) -> Result<Vec<StoredEvent>> {
    let all = events_repo::list_for_calendar(conn, calendar_id)?;
    Ok(all
        .into_iter()
        .filter(|stored| stored.event.uid == uid)
        .collect())
}

fn set_attendee_status(event: &mut CalendarEvent, email: &str, status: AttendeeStatus) {
    if email.trim().is_empty() {
        event.my_response = Some(status);
        return;
    }
    if let Some(index) = event
        .attendees
        .iter()
        .position(|attendee| attendee.email.eq_ignore_ascii_case(email))
    {
        event.attendees[index].status = status;
    } else {
        event.attendees.push(Attendee {
            email: email.to_string(),
            name: None,
            role: AttendeeRole::Required,
            status,
        });
    }
    event.my_response = Some(status);
}

/// Build a fresh event from user input.  `existing` preserves identity fields
/// and the organizer when editing.
fn build_event(
    id: String,
    uid: String,
    calendar: &Calendar,
    account: &Account,
    input: &EventInput,
    existing: Option<&CalendarEvent>,
) -> CalendarEvent {
    let time_zone = if input.time_zone.trim().is_empty() {
        "UTC".to_string()
    } else {
        normalize_zone(&input.time_zone)
    };
    let (start, end) = if input.all_day {
        (
            date_part(&input.start).unwrap_or_else(|| input.start.clone()),
            date_part(&input.end).unwrap_or_else(|| input.end.clone()),
        )
    } else {
        (
            parse_instant(&input.start)
                .map(|instant| crate::util::instant_in_zone(instant, &time_zone))
                .unwrap_or_else(|_| input.start.clone()),
            parse_instant(&input.end)
                .map(|instant| crate::util::instant_in_zone(instant, &time_zone))
                .unwrap_or_else(|_| input.end.clone()),
        )
    };

    let mut event = CalendarEvent {
        id,
        calendar_id: calendar.id.clone(),
        account_id: account.id.clone(),
        uid,
        title: input.title.trim().to_string(),
        location: input.location.trim().to_string(),
        description: input.description.clone(),
        start,
        end,
        all_day: input.all_day,
        time_zone,
        recurrence: input.recurrence.clone(),
        series_id: existing.and_then(|event| event.series_id.clone()),
        recurrence_id: existing.and_then(|event| event.recurrence_id.clone()),
        status: existing
            .map(|event| event.status)
            .unwrap_or(EventStatus::Confirmed),
        show_as: input.show_as,
        is_private: input.is_private,
        organizer: existing.and_then(|event| event.organizer.clone()),
        attendees: input
            .attendees
            .iter()
            .filter(|attendee| !attendee.email.trim().is_empty())
            .cloned()
            .collect(),
        reminders: normalized_reminders(&input.reminders),
        is_organizer: existing.map(|event| event.is_organizer).unwrap_or(true),
        my_response: existing.and_then(|event| event.my_response),
        online_meeting_url: input.online_meeting_url.clone(),
        color: input.color.as_deref().and_then(normalize_color),
        updated_at: None,
    };
    if event.organizer.is_none() && !event.attendees.is_empty() {
        event.organizer = Some(Attendee {
            email: account.email.clone(),
            name: Some(account.display_name.clone()).filter(|name| !name.is_empty()),
            role: AttendeeRole::Chair,
            status: AttendeeStatus::Accepted,
        });
    }
    if let Some(index) = event
        .attendees
        .iter()
        .position(|attendee| attendee.email.eq_ignore_ascii_case(&account.email))
    {
        event.attendees[index].status = AttendeeStatus::Accepted;
        event.my_response = Some(AttendeeStatus::Accepted);
    }
    event
}

fn apply_input(event: &mut CalendarEvent, input: &EventInput, account: &Account) {
    let time_zone = if input.time_zone.trim().is_empty() {
        "UTC".to_string()
    } else {
        normalize_zone(&input.time_zone)
    };
    event.title = input.title.trim().to_string();
    event.location = input.location.trim().to_string();
    event.description = input.description.clone();
    event.all_day = input.all_day;
    event.time_zone = time_zone.clone();
    event.start = if input.all_day {
        date_part(&input.start).unwrap_or_else(|| input.start.clone())
    } else {
        parse_instant(&input.start)
            .map(|instant| crate::util::instant_in_zone(instant, &time_zone))
            .unwrap_or_else(|_| input.start.clone())
    };
    event.end = if input.all_day {
        date_part(&input.end).unwrap_or_else(|| input.end.clone())
    } else {
        parse_instant(&input.end)
            .map(|instant| crate::util::instant_in_zone(instant, &time_zone))
            .unwrap_or_else(|_| input.end.clone())
    };
    event.recurrence = input.recurrence.clone();
    event.show_as = input.show_as;
    event.is_private = input.is_private;
    event.attendees = input
        .attendees
        .iter()
        .filter(|attendee| !attendee.email.trim().is_empty())
        .cloned()
        .collect();
    event.reminders = normalized_reminders(&input.reminders);
    event.online_meeting_url = input.online_meeting_url.clone();
    event.color = input.color.as_deref().and_then(normalize_color);
    if event.organizer.is_none() && !event.attendees.is_empty() {
        event.organizer = Some(Attendee {
            email: account.email.clone(),
            name: Some(account.display_name.clone()).filter(|name| !name.is_empty()),
            role: AttendeeRole::Chair,
            status: AttendeeStatus::Accepted,
        });
    }
    if let Some(index) = event
        .attendees
        .iter()
        .position(|attendee| attendee.email.eq_ignore_ascii_case(&account.email))
    {
        event.attendees[index].status = AttendeeStatus::Accepted;
        event.my_response = Some(AttendeeStatus::Accepted);
    }
}

/// Keep a recurring series anchored to its original DTSTART date when an
/// "all events" edit was started from a later occurrence.
///
/// The editor opens a specific occurrence, so `input.start` carries that
/// occurrence's date.  Applying it verbatim to the master would move the
/// series start forward, dropping every earlier occurrence (and changing
/// COUNT/UNTIL semantics).  Only the time-of-day / duration from the input is
/// applied; the date is pinned back to the master anchor.
fn reanchor_recurring_update(master: &CalendarEvent, updated: &mut CalendarEvent) {
    if master.recurrence.is_none() || updated.recurrence.is_none() {
        return;
    }
    let Some(master_date) = date_part(&master.start) else {
        return;
    };
    let Some(input_date) = date_part(&updated.start) else {
        return;
    };
    if input_date <= master_date {
        // Equal date: the user edited the anchor occurrence itself.  Earlier
        // date: an explicit move of the series start; keep it.
        return;
    }
    let (Ok(master_day), Ok(input_day)) = (
        chrono::NaiveDate::parse_from_str(&master_date, "%Y-%m-%d"),
        chrono::NaiveDate::parse_from_str(&input_date, "%Y-%m-%d"),
    ) else {
        return;
    };
    let shift = Duration::days((master_day - input_day).num_days());
    if let Ok(start) = crate::util::shift_instant(&updated.start, shift) {
        updated.start = start;
    }
    if let Ok(end) = crate::util::shift_instant(&updated.end, shift) {
        updated.end = end;
    }
}

fn normalized_reminders(values: &[i32]) -> Vec<i32> {
    let mut out: Vec<i32> = values
        .iter()
        .copied()
        .filter(|minutes| (0..=10080).contains(minutes))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod import_grouping_tests {
    use super::*;

    fn component(id: &str, uid: &str, master: bool, existing: bool) -> ImportedComponent {
        ImportedComponent {
            id: id.into(),
            uid: uid.into(),
            was_existing: existing,
            recurrence_id: if master {
                None
            } else {
                Some("2026-11-19T09:00:00Z".into())
            },
            cancelled: false,
            was_master: master,
        }
    }

    #[test]
    fn groups_master_and_exceptions_even_when_ordered_last() {
        let groups = group_imported_components(vec![
            component("exception", "uid-1", false, false),
            component("master", "uid-1", true, false),
            component("other", "uid-2", true, true),
        ]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].master_id.as_deref(), Some("master"));
        assert!(!groups[0].master_existing);
        assert_eq!(groups[0].overrides.len(), 1);
        assert_eq!(groups[0].overrides[0].id, "exception");
        assert!(groups[1].master_existing);
        assert!(groups[1].overrides.is_empty());
    }

    #[test]
    fn orphan_exceptions_do_not_fake_a_master() {
        let groups = group_imported_components(vec![component("orphan", "uid-3", false, false)]);
        assert_eq!(groups.len(), 1);
        assert!(groups[0].master_id.is_none());
        assert_eq!(groups[0].overrides.len(), 1);
    }
}
