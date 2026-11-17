//! Contact sync engine, command facade and pending-operation executor.
//!
//! Reads come straight from SQLite.  Writes update the database first and then
//! enqueue a `pim.contact.*` operation; the shared [`crate::op_worker`] drains
//! it with retry/rollback and emits `contacts://changed` after local changes.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use nuncio_core::events::emit_contacts_changed;
use nuncio_core::models::{
    new_id, Account, AddressBook, Contact, ContactGroup, ContactListQuery, ContactPage,
    NewPendingOp, PendingOp, SyncModule,
};
use nuncio_core::providers::DavService;
use nuncio_core::repo::{self, contacts as contacts_repo, sync_state};
use nuncio_core::CoreContext;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::backend::{backend_for, is_implicit_dav_auth_failure, Backend};
use crate::contacts::dav::{parse_vcf, write_vcf, CardDavProvider};
use crate::contacts::google::GoogleContactsProvider;
use crate::contacts::graph::GraphContactsProvider;
use crate::contacts::ops::{self, ContactOp, ContactSnapshot, GroupSnapshot};
use crate::contacts::provider::{
    ContactCursor, ContactDelta, ContactProvider, RemoteAddressBook, RemoteContact,
    RemoteContactRef,
};
use crate::error::{invalid, AppError, Result};
use crate::http::HttpClient;
use crate::op_worker::{self, PimOpExecutor};

/// `sync_state.module` used for contact cursors.
pub const SYNC_MODULE: &str = "contacts";

/// Decode a vCard file to text.
///
/// Supports UTF-8 (with or without BOM), UTF-16 LE/BE with a BOM, an explicit
/// `CHARSET=` hint, and falls back to GB18030 (a superset of GBK/GB2312) for
/// legacy Chinese exports.
fn decode_vcf_bytes(bytes: &[u8]) -> Result<String> {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return std::str::from_utf8(&bytes[3..])
            .map(str::to_string)
            .map_err(|error| invalid(format!("invalid UTF-8 vCard: {error}")));
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return decode_utf16(&bytes[2..], true);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return decode_utf16(&bytes[2..], false);
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(text.to_string());
    }
    let encoding = charset_hint(bytes).unwrap_or(encoding_rs::GB18030);
    let (text, _used, had_errors) = encoding.decode(bytes);
    if had_errors {
        debug!(
            encoding = encoding.name(),
            "vCard import had undecodable byte sequences"
        );
    }
    Ok(text.into_owned())
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> Result<String> {
    if bytes.len() % 2 != 0 {
        return Err(invalid("truncated UTF-16 vCard file"));
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        })
        .collect();
    String::from_utf16(&units).map_err(|error| invalid(format!("invalid UTF-16 vCard: {error}")))
}

/// Read a `CHARSET=` parameter from the leading bytes of a vCard.
fn charset_hint(bytes: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    let sample = &bytes[..bytes.len().min(8192)];
    let text = String::from_utf8_lossy(sample);
    let lower = text.to_ascii_lowercase();
    let index = lower.find("charset=")? + "charset=".len();
    let rest = &text[index..];
    let label = rest
        .split(|character: char| {
            character.is_whitespace()
                || matches!(character, ';' | ':' | ',' | '"' | '\'' | '\r' | '\n')
        })
        .next()?
        .trim();
    if label.is_empty() {
        return None;
    }
    encoding_rs::Encoding::for_label(label.as_bytes())
}

/// Contact service handle shared by `PimService`.
#[derive(Clone)]
pub struct ContactsService {
    ctx: CoreContext,
    http: HttpClient,
    notify: Arc<Notify>,
    shutdown: Arc<AtomicBool>,
    worker: Arc<Mutex<Option<JoinHandle<()>>>>,
    locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl std::fmt::Debug for ContactsService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContactsService").finish_non_exhaustive()
    }
}

impl ContactsService {
    pub fn new(ctx: CoreContext, http: HttpClient) -> Self {
        Self {
            ctx,
            http,
            notify: Arc::new(Notify::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            worker: Arc::new(Mutex::new(None)),
            locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn context(&self) -> &CoreContext {
        &self.ctx
    }

    /// Wake the op worker (after a local write) and any pending loops.
    pub fn notify(&self) {
        self.notify.notify_waiters();
    }

    pub fn start(&self) {
        if self
            .worker
            .lock()
            .map(|slot| slot.is_some())
            .unwrap_or(false)
        {
            return;
        }
        self.shutdown.store(false, Ordering::SeqCst);
        let _ = op_worker::recover_interrupted(&self.ctx, ops::KIND_PREFIX);
        let executor: Arc<dyn PimOpExecutor> = Arc::new(self.clone());
        let ctx = self.ctx.clone();
        let notify = self.notify.clone();
        let shutdown = self.shutdown.clone();
        let handle = tokio::spawn(async move {
            op_worker::run_worker(ctx, notify, shutdown, executor).await;
        });
        if let Ok(mut slot) = self.worker.lock() {
            *slot = Some(handle);
        }
    }

    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
        if let Ok(mut slot) = self.worker.lock() {
            if let Some(handle) = slot.take() {
                handle.abort();
            }
        }
    }

    fn provider_for(&self, account: &Account) -> Option<Box<dyn ContactProvider>> {
        match backend_for(account, DavService::Contacts) {
            Backend::Google => Some(Box::new(GoogleContactsProvider::new(self.http.clone()))),
            Backend::Graph => Some(Box::new(GraphContactsProvider::new(self.http.clone()))),
            Backend::Dav => Some(Box::new(CardDavProvider::new(self.http.clone()))),
            Backend::Local | Backend::Unsupported => None,
        }
    }

    fn provider_name(&self, account: &Account) -> Option<&'static str> {
        match backend_for(account, DavService::Contacts) {
            Backend::Google => Some("google"),
            Backend::Graph => Some("graph"),
            Backend::Dav => Some("carddav"),
            Backend::Local | Backend::Unsupported => None,
        }
    }

    /* ───────────────────────────── sync ───────────────────────────── */

    /// Sync every address book of one account.
    pub async fn sync_account(&self, account_id: &str, _force: bool) -> Result<()> {
        let lock = {
            let mut locks = self
                .locks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            locks
                .entry(account_id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;

        let account = self
            .ctx
            .db
            .read({
                let id = account_id.to_string();
                move |conn| repo::accounts::get_required(conn, &id)
            })
            .await?;
        if !account.contacts_enabled {
            return Ok(());
        }
        self.ctx
            .sync_status
            .begin(&account.id, SyncModule::Contacts, None);
        let result = self.sync_account_inner(&account).await;
        match result {
            Ok(()) => {
                self.ctx
                    .sync_status
                    .finish(&account.id, SyncModule::Contacts);
                Ok(())
            }
            Err(error) => {
                // CardDAV with a directory URL and no dedicated password is
                // implicit: a rejected mail password is expected, not a
                // sign-in problem.
                if matches!(error, AppError::Auth(_))
                    && backend_for(&account, DavService::Contacts) == Backend::Dav
                {
                    let dedicated = self
                        .ctx
                        .creds
                        .has_dav_password(&account.id, DavService::Contacts)
                        .await;
                    if is_implicit_dav_auth_failure(
                        &account,
                        DavService::Contacts,
                        dedicated,
                        &error,
                    ) {
                        debug!(
                            account_id = %account.id,
                            "contacts needs its own password for this provider; set it in account settings"
                        );
                        self.ctx
                            .sync_status
                            .finish(&account.id, SyncModule::Contacts);
                        return Ok(());
                    }
                }
                match &error {
                    AppError::Auth(_) => self
                        .ctx
                        .sync_status
                        .needs_reauth(&account.id, SyncModule::Contacts),
                    AppError::Network(_) => self
                        .ctx
                        .sync_status
                        .offline(&account.id, SyncModule::Contacts),
                    _ => self.ctx.sync_status.fail(
                        &account.id,
                        SyncModule::Contacts,
                        error.message(),
                    ),
                };
                Err(error)
            }
        }
    }

    async fn sync_account_inner(&self, account: &Account) -> Result<()> {
        let Some(provider) = self.provider_for(account) else {
            return Ok(());
        };
        let provider_name = provider.name().to_string();
        let remotes = provider.list_address_books(account).await?;
        let (books, books_changed) = self
            .upsert_remote_books(account, &provider_name, remotes)
            .await?;
        let mut changed: Vec<String> = Vec::new();
        if books_changed {
            changed.extend(books.iter().map(|(book, _)| book.id.clone()));
        }
        let mut first_error = None;
        for (book, remote) in &books {
            match self
                .sync_one_book(account, provider.as_ref(), book, remote)
                .await
            {
                Ok(true) => changed.push(book.id.clone()),
                Ok(false) => {}
                Err(error) => {
                    warn!(book = %book.id, %error, "address book sync failed");
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        // Google's "other contacts" only feed recipient suggestions.
        if backend_for(account, DavService::Contacts) == Backend::Google {
            match provider.other_contacts(account).await {
                Ok(addresses) if !addresses.is_empty() => {
                    let records = addresses;
                    let _ = self
                        .ctx
                        .db
                        .write(move |conn| repo::recipient_history::record(conn, &records))
                        .await;
                }
                Ok(_) => {}
                Err(error) => debug!(%error, "could not refresh Google other contacts"),
            }
        }
        if !changed.is_empty() {
            changed.sort();
            changed.dedup();
            emit_contacts_changed(self.ctx.events.as_ref(), changed);
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn upsert_remote_books(
        &self,
        account: &Account,
        provider_name: &str,
        remotes: Vec<RemoteAddressBook>,
    ) -> Result<(Vec<(AddressBook, RemoteAddressBook)>, bool)> {
        let account_id = account.id.clone();
        let provider_name = provider_name.to_string();
        self.ctx
            .db
            .write(move |conn| {
                let existing: Vec<crate::store::AddressBookRemote> =
                    crate::store::list_books_for_account(conn, &account_id)?
                        .into_iter()
                        .filter(|mapping| mapping.provider == provider_name)
                        .collect();
                let remote_ids: HashSet<String> =
                    remotes.iter().map(|remote| remote.remote_id.clone()).collect();
                let mut changed = false;
                let mut pairs = Vec::new();

                for remote in remotes {
                    let mut remote = remote;
                    let mapping = crate::store::get_book_by_remote(
                        conn,
                        &account_id,
                        &provider_name,
                        &remote.remote_id,
                    )?;
                    match mapping {
                        Some(mapping) => match repo::address_books::get(conn, &mapping.address_book_id)? {
                            Some(mut book) => {
                                if book.name != remote.name && !remote.name.trim().is_empty() {
                                    conn.execute(
                                        "UPDATE address_book SET name = ?2, updated_at = ?3 WHERE id = ?1",
                                        rusqlite::params![book.id, remote.name, repo::now_string()],
                                    )?;
                                    book.name = remote.name.clone();
                                    changed = true;
                                }
                                if book.is_read_only != remote.read_only {
                                    conn.execute(
                                        "UPDATE address_book SET is_read_only = ?2 WHERE id = ?1",
                                        rusqlite::params![book.id, remote.read_only as i64],
                                    )?;
                                    book.is_read_only = remote.read_only;
                                    changed = true;
                                }
                                if mapping.href != remote.href {
                                    crate::store::set_book(
                                        conn,
                                        &crate::store::AddressBookRemote {
                                            href: remote.href.clone(),
                                            ..mapping.clone()
                                        },
                                    )?;
                                }
                                remote.local_id = book.id.clone();
                                pairs.push((book, remote));
                            }
                            None => {
                                crate::store::delete_book(conn, &mapping.address_book_id)?;
                                let book = create_remote_book(conn, &account_id, &remote)?;
                                crate::store::set_book(
                                    conn,
                                    &crate::store::AddressBookRemote::new(
                                        book.id.clone(),
                                        &account_id,
                                        &provider_name,
                                        &remote.remote_id,
                                        remote.href.clone(),
                                    ),
                                )?;
                                remote.local_id = book.id.clone();
                                changed = true;
                                pairs.push((book, remote));
                            }
                        },
                        None => {
                            let book = create_remote_book(conn, &account_id, &remote)?;
                            crate::store::set_book(
                                conn,
                                &crate::store::AddressBookRemote::new(
                                    book.id.clone(),
                                    &account_id,
                                    &provider_name,
                                    &remote.remote_id,
                                    remote.href.clone(),
                                ),
                            )?;
                            remote.local_id = book.id.clone();
                            changed = true;
                            pairs.push((book, remote));
                        }
                    }
                }

                for mapping in existing {
                    if !remote_ids.contains(&mapping.remote_id) {
                        if let Err(error) =
                            repo::address_books::delete(conn, &mapping.address_book_id)
                        {
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

    async fn sync_one_book(
        &self,
        account: &Account,
        provider: &dyn ContactProvider,
        book: &AddressBook,
        remote: &RemoteAddressBook,
    ) -> Result<bool> {
        let book_id = book.id.clone();
        let state = self
            .ctx
            .db
            .read({
                let book_id = book_id.clone();
                move |conn| load_cursor(conn, &book_id)
            })
            .await?;
        let etags = if provider.name() == "carddav" {
            self.ctx
                .db
                .read({
                    let book_id = book_id.clone();
                    move |conn| local_etags(conn, &book_id)
                })
                .await?
        } else {
            Vec::new()
        };
        let cursor = ContactCursor {
            local_address_book_id: book.id.clone(),
            sync_token: state.as_ref().and_then(|state| state.sync_token.clone()),
            ctag: state.as_ref().and_then(|state| state.ctag.clone()),
            etags,
        };
        let delta = provider.sync_contacts(account, remote, &cursor).await?;
        let changed = delta.full_sync
            || !delta.contacts.is_empty()
            || !delta.deleted_remote_ids.is_empty()
            || !delta.deleted_hrefs.is_empty()
            || delta.groups_full;
        let account_id = account.id.clone();
        let book_clone = book.clone();
        let delta_for_db = delta.clone();
        self.ctx
            .db
            .write(move |conn| {
                apply_delta(conn, &account_id, &book_clone, &delta_for_db)?;
                save_cursor(
                    conn,
                    &book_clone.id,
                    delta_for_db.sync_token.as_deref(),
                    delta_for_db.ctag.as_deref(),
                )
            })
            .await?;
        Ok(changed)
    }

    /* ──────────────────────── command facade ──────────────────────── */

    pub async fn addressbook_list(&self) -> Result<Vec<AddressBook>> {
        self.ctx
            .db
            .read(|conn| repo::address_books::list(conn, None))
            .await
    }

    pub async fn contact_list(&self, query: ContactListQuery) -> Result<ContactPage> {
        self.ctx
            .db
            .read(move |conn| contacts_repo::list(conn, &query))
            .await
    }

    pub async fn contact_get(&self, id: &str) -> Result<Contact> {
        let id = id.to_string();
        self.ctx
            .db
            .read(move |conn| contacts_repo::get_required(conn, &id))
            .await
    }

    pub async fn contact_create(&self, contact: Contact) -> Result<Contact> {
        if contact.address_book_id.trim().is_empty() {
            return Err(invalid("contact requires an address book"));
        }
        require_contact(&contact)?;
        let book_id = contact.address_book_id.clone();
        let (saved, account) = self
            .ctx
            .db
            .write({
                let mut contact = contact.clone();
                move |conn| {
                    let book = repo::address_books::get_required(conn, &book_id)?;
                    if book.is_read_only {
                        return Err(invalid("address book is read-only"));
                    }
                    contact.id = new_id();
                    contact.account_id = book.account_id.clone();
                    contact.updated_at = Some(Utc::now());
                    let saved = contacts_repo::create(conn, &contact)?;
                    Ok((saved, book.account_id))
                }
            })
            .await?;
        let account = self.load_account(&account).await?;
        if self.provider_name(&account).is_some() {
            let op = ContactOp::Save {
                contact_id: saved.id.clone(),
                snapshot: None,
            };
            self.enqueue_op(&account.id, &op, format!("创建联系人「{}」", saved.label()))
                .await?;
        }
        emit_contacts_changed(
            self.ctx.events.as_ref(),
            vec![saved.address_book_id.clone()],
        );
        Ok(saved)
    }

    pub async fn contact_update(&self, contact: Contact) -> Result<Contact> {
        if contact.id.trim().is_empty() {
            return Err(invalid("contact id is required"));
        }
        require_contact(&contact)?;
        let id = contact.id.clone();
        let (snapshot, saved) = self
            .ctx
            .db
            .write({
                let mut contact = contact.clone();
                move |conn| {
                    let previous = contacts_repo::get_required(conn, &id)?;
                    let snapshot = snapshot_contact(conn, &id)?;
                    if contact.address_book_id.trim().is_empty() {
                        contact.address_book_id = previous.address_book_id.clone();
                    }
                    contact.account_id = previous.account_id.clone();
                    contact.updated_at = Some(Utc::now());
                    let book = repo::address_books::get_required(conn, &contact.address_book_id)?;
                    if book.is_read_only {
                        return Err(invalid("address book is read-only"));
                    }
                    let saved = contacts_repo::update(conn, &contact)?;
                    Ok((snapshot, saved))
                }
            })
            .await?;
        let account = self.load_account(&saved.account_id).await?;
        if self.provider_name(&account).is_some() {
            let op = ContactOp::Save {
                contact_id: saved.id.clone(),
                snapshot,
            };
            self.enqueue_op(&account.id, &op, format!("更新联系人「{}」", saved.label()))
                .await?;
        }
        emit_contacts_changed(
            self.ctx.events.as_ref(),
            vec![saved.address_book_id.clone()],
        );
        Ok(saved)
    }

    pub async fn contact_delete(&self, ids: Vec<String>) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut snapshots = Vec::new();
        let mut books = Vec::new();
        for id in &ids {
            if let Some(snapshot) = self.snapshot_contact(id).await? {
                if !books.contains(&snapshot.contact.address_book_id) {
                    books.push(snapshot.contact.address_book_id.clone());
                }
                snapshots.push(snapshot);
            }
        }
        if snapshots.is_empty() {
            return Ok(());
        }
        let delete_ids: Vec<String> = snapshots
            .iter()
            .map(|snapshot| snapshot.contact.id.clone())
            .collect();
        let db_ids = delete_ids.clone();
        self.ctx
            .db
            .write(move |conn| contacts_repo::delete_many(conn, &db_ids))
            .await?;
        // Group the remote deletions by account (bulk delete may span books).
        let mut by_account: HashMap<String, Vec<ContactSnapshot>> = HashMap::new();
        for snapshot in &snapshots {
            if snapshot.remote_id.is_none() && snapshot.href.is_none() {
                continue;
            }
            by_account
                .entry(snapshot.contact.account_id.clone())
                .or_default()
                .push(snapshot.clone());
        }
        for (account_id, owned) in by_account {
            let account = self.load_account(&account_id).await?;
            if self.provider_name(&account).is_some() {
                let op = ContactOp::Delete { contacts: owned };
                self.enqueue_op(&account.id, &op, "删除联系人".to_string())
                    .await?;
            }
        }
        emit_contacts_changed(self.ctx.events.as_ref(), books);
        Ok(())
    }

    pub async fn contact_set_favorite(&self, id: &str, value: bool) -> Result<()> {
        let id = id.to_string();
        let (contact, previous) = self
            .ctx
            .db
            .write(move |conn| {
                let contact = contacts_repo::get_required(conn, &id)?;
                let previous = contact.is_favorite;
                contacts_repo::set_favorite(conn, &id, value)?;
                Ok((contact, previous))
            })
            .await?;
        let account = self.load_account(&contact.account_id).await?;
        if backend_for(&account, DavService::Contacts) == Backend::Google {
            let op = ContactOp::Favorite {
                contact_id: contact.id.clone(),
                value,
                previous,
            };
            self.enqueue_op(
                &account.id,
                &op,
                format!("更新联系人「{}」的收藏状态", contact.label()),
            )
            .await?;
        }
        emit_contacts_changed(self.ctx.events.as_ref(), vec![contact.address_book_id]);
        Ok(())
    }

    pub async fn contact_find_by_email(&self, email: &str) -> Result<Option<Contact>> {
        let email = email.to_string();
        self.ctx
            .db
            .read(move |conn| contacts_repo::find_by_email(conn, &email))
            .await
    }

    pub async fn group_list(&self, address_book_id: Option<String>) -> Result<Vec<ContactGroup>> {
        self.ctx
            .db
            .read(move |conn| contacts_repo::list_groups(conn, address_book_id.as_deref()))
            .await
    }

    pub async fn group_create(&self, address_book_id: &str, name: &str) -> Result<ContactGroup> {
        if name.trim().is_empty() {
            return Err(invalid("group name must not be empty"));
        }
        let book_id = address_book_id.to_string();
        let name_owned = name.trim().to_string();
        let group = self
            .ctx
            .db
            .write(move |conn| contacts_repo::create_group(conn, &book_id, &name_owned))
            .await?;
        let account = self.load_account(&group.account_id).await?;
        // Category-based providers (Graph, CardDAV) only need local groups;
        // Google has real contact groups and must create them remotely.
        if backend_for(&account, DavService::Contacts) == Backend::Google {
            let op = ContactOp::GroupCreate {
                group_id: group.id.clone(),
            };
            self.enqueue_op(
                &account.id,
                &op,
                format!("创建联系人分组「{}」", group.name),
            )
            .await?;
        }
        emit_contacts_changed(
            self.ctx.events.as_ref(),
            vec![group.address_book_id.clone()],
        );
        Ok(group)
    }

    pub async fn group_rename(&self, id: &str, name: &str) -> Result<ContactGroup> {
        if name.trim().is_empty() {
            return Err(invalid("group name must not be empty"));
        }
        let id = id.to_string();
        let new_name = name.trim().to_string();
        let (previous, group) = self
            .ctx
            .db
            .write(move |conn| {
                let previous = contacts_repo::get_group(conn, &id)?
                    .ok_or_else(|| AppError::not_found("group not found"))?;
                let group = contacts_repo::rename_group(conn, &id, &new_name)?;
                Ok((previous, group))
            })
            .await?;
        let account = self.load_account(&group.account_id).await?;
        if self.provider_name(&account).is_some() {
            if backend_for(&account, DavService::Contacts) == Backend::Google {
                let op = ContactOp::GroupRename {
                    group_id: group.id.clone(),
                    previous_name: previous.name.clone(),
                };
                self.enqueue_op(
                    &account.id,
                    &op,
                    format!("重命名联系人分组「{}」", group.name),
                )
                .await?;
            } else {
                // Category-based providers store membership on the contact.
                self.enqueue_group_contact_saves(&group.id).await?;
            }
        }
        emit_contacts_changed(
            self.ctx.events.as_ref(),
            vec![group.address_book_id.clone()],
        );
        Ok(group)
    }

    pub async fn group_delete(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        let snapshot = self.group_snapshot(&id).await?;
        let Some(snapshot) = snapshot else {
            return Err(AppError::not_found("group not found"));
        };
        let db_id = id.clone();
        self.ctx
            .db
            .write(move |conn| contacts_repo::delete_group(conn, &db_id))
            .await?;
        let account = self.load_account(&snapshot.group.account_id).await?;
        if let Some(provider_name) = self.provider_name(&account) {
            if snapshot.remote_id.is_some() {
                let op = ContactOp::GroupDelete {
                    snapshot: snapshot.clone(),
                };
                self.enqueue_op(
                    &account.id,
                    &op,
                    format!("删除联系人分组「{}」", snapshot.group.name),
                )
                .await?;
            }
            if provider_name != "google" {
                for contact_id in &snapshot.member_ids {
                    self.enqueue_contact_save(&account.id, contact_id).await?;
                }
            }
        }
        emit_contacts_changed(
            self.ctx.events.as_ref(),
            vec![snapshot.group.address_book_id],
        );
        Ok(())
    }

    pub async fn group_set_members(&self, group_id: &str, contact_ids: Vec<String>) -> Result<()> {
        let group_id = group_id.to_string();
        let (group, previous) = self
            .ctx
            .db
            .write({
                let contact_ids = contact_ids.clone();
                move |conn| {
                    let group = contacts_repo::get_group(conn, &group_id)?
                        .ok_or_else(|| AppError::not_found("group not found"))?;
                    let previous_members: Vec<String> = contacts_repo::members(conn, &group_id)?
                        .into_iter()
                        .map(|contact| contact.id)
                        .collect();
                    contacts_repo::set_group_members(conn, &group_id, &contact_ids)?;
                    Ok((group, previous_members))
                }
            })
            .await?;
        let current: HashSet<String> = contact_ids.into_iter().collect();
        let previous_set: HashSet<String> = previous.into_iter().collect();
        let added: Vec<String> = current.difference(&previous_set).cloned().collect();
        let removed: Vec<String> = previous_set.difference(&current).cloned().collect();
        let account = self.load_account(&group.account_id).await?;
        if let Some(provider_name) = self.provider_name(&account) {
            if provider_name == "google" && (!added.is_empty() || !removed.is_empty()) {
                let op = ContactOp::GroupMembers {
                    group_id: group.id.clone(),
                    added: added.clone(),
                    removed: removed.clone(),
                };
                self.enqueue_op(
                    &account.id,
                    &op,
                    format!("更新联系人分组「{}」成员", group.name),
                )
                .await?;
            }
            if provider_name != "google" {
                for contact_id in added.iter().chain(removed.iter()) {
                    self.enqueue_contact_save(&account.id, contact_id).await?;
                }
            }
        }
        emit_contacts_changed(self.ctx.events.as_ref(), vec![group.address_book_id]);
        Ok(())
    }

    pub async fn contact_import_vcf(&self, path: &str, address_book_id: &str) -> Result<u32> {
        // Check the size before reading so an accidentally selected huge file
        // cannot spike memory just to be rejected afterwards.
        const MAX_VCF_BYTES: u64 = 64 * 1024 * 1024;
        let size = tokio::fs::metadata(path)
            .await
            .map_err(|error| AppError::io(format!("cannot stat {path}: {error}")))?
            .len();
        if size > MAX_VCF_BYTES {
            return Err(invalid("the vCard file is larger than 64 MB"));
        }
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|error| AppError::io(format!("cannot read {path}: {error}")))?;
        if bytes.len() as u64 > MAX_VCF_BYTES {
            return Err(invalid("the vCard file is larger than 64 MB"));
        }
        let content = decode_vcf_bytes(&bytes)?;
        let parsed = parse_vcf(&content);
        if parsed.is_empty() {
            return Err(invalid("no contacts found in the vCard file"));
        }
        let book_id = address_book_id.to_string();
        let parsed_for_db = parsed.clone();
        let saved = self
            .ctx
            .db
            .write(move |conn| {
                let book = repo::address_books::get_required(conn, &book_id)?;
                if book.is_read_only {
                    return Err(invalid("address book is read-only"));
                }
                let mut saved = Vec::new();
                for (mut contact, group_names) in parsed_for_db {
                    contact.id = new_id();
                    contact.address_book_id = book.id.clone();
                    contact.account_id = book.account_id.clone();
                    contact.group_ids = ensure_groups(conn, &book, &group_names)?;
                    contact.updated_at = Some(Utc::now());
                    saved.push(contacts_repo::create(conn, &contact)?);
                }
                Ok((saved, book))
            })
            .await?;
        let (contacts, book) = saved;
        let account = self.load_account(&book.account_id).await?;
        if self.provider_name(&account).is_some() {
            if backend_for(&account, DavService::Contacts) == Backend::Google {
                let mut group_ids: Vec<String> = contacts
                    .iter()
                    .flat_map(|contact| contact.group_ids.iter().cloned())
                    .collect();
                group_ids.sort();
                group_ids.dedup();
                for group_id in group_ids {
                    if group_remote_id(&self.ctx, &group_id).await?.is_some() {
                        continue;
                    }
                    let op = ContactOp::GroupCreate {
                        group_id: group_id.clone(),
                    };
                    self.enqueue_op(&account.id, &op, "导入联系人分组".to_string())
                        .await?;
                }
            }
            for contact in &contacts {
                let op = ContactOp::Save {
                    contact_id: contact.id.clone(),
                    snapshot: None,
                };
                self.enqueue_op(
                    &account.id,
                    &op,
                    format!("导入联系人「{}」", contact.label()),
                )
                .await?;
            }
        }
        emit_contacts_changed(self.ctx.events.as_ref(), vec![book.id]);
        Ok(contacts.len() as u32)
    }

    pub async fn contact_export_vcf(&self, ids: Vec<String>, path: &str) -> Result<()> {
        let contacts = self
            .ctx
            .db
            .read(move |conn| {
                let mut out = Vec::new();
                let mut groups = Vec::new();
                for id in ids {
                    let contact = match contacts_repo::get(conn, &id)? {
                        Some(contact) => contact,
                        None => continue,
                    };
                    let names = group_names_for(conn, &id)?;
                    out.push(contact);
                    groups.push(names);
                }
                Ok((out, groups))
            })
            .await?;
        let (contacts, groups) = contacts;
        if contacts.is_empty() {
            return Err(AppError::not_found("no contacts to export"));
        }
        let body = write_vcf(&contacts, &groups);
        tokio::fs::write(path, body)
            .await
            .map_err(|error| AppError::io(format!("cannot write {path}: {error}")))?;
        Ok(())
    }

    /* ───────────────────────── op plumbing ───────────────────────── */

    async fn enqueue_op(
        &self,
        account_id: &str,
        op: &ContactOp,
        description: String,
    ) -> Result<()> {
        let payload = ops::to_value(op)?;
        let new_op = NewPendingOp::new(account_id, op.kind(), payload, description);
        self.ctx
            .db
            .write(move |conn| repo::pending_ops::enqueue(conn, new_op))
            .await?;
        self.notify.notify_one();
        Ok(())
    }

    async fn enqueue_contact_save(&self, account_id: &str, contact_id: &str) -> Result<()> {
        let Some(snapshot) = self.snapshot_contact(contact_id).await? else {
            return Ok(());
        };
        let op = ContactOp::Save {
            contact_id: contact_id.to_string(),
            snapshot: Some(snapshot),
        };
        self.enqueue_op(
            account_id,
            &op,
            format!("同步联系人「{}」", op_label(contact_id, self).await),
        )
        .await
    }

    async fn enqueue_group_contact_saves(&self, group_id: &str) -> Result<()> {
        let group_id = group_id.to_string();
        let ids = self
            .ctx
            .db
            .read(move |conn| {
                let members = contacts_repo::members(conn, &group_id)?;
                Ok(members
                    .into_iter()
                    .map(|contact| contact.id)
                    .collect::<Vec<_>>())
            })
            .await?;
        for id in ids {
            let account_id = {
                let id = id.clone();
                self.ctx
                    .db
                    .read(move |conn| contacts_repo::get_required(conn, &id))
                    .await?
                    .account_id
            };
            self.enqueue_contact_save(&account_id, &id).await?;
        }
        Ok(())
    }

    async fn snapshot_contact(&self, id: &str) -> Result<Option<ContactSnapshot>> {
        let id = id.to_string();
        self.ctx
            .db
            .read(move |conn| snapshot_contact(conn, &id))
            .await
    }

    async fn group_snapshot(&self, id: &str) -> Result<Option<GroupSnapshot>> {
        let id = id.to_string();
        self.ctx
            .db
            .read(move |conn| group_snapshot(conn, &id))
            .await
    }

    async fn load_account(&self, id: &str) -> Result<Account> {
        let id = id.to_string();
        self.ctx
            .db
            .read(move |conn| repo::accounts::get_required(conn, &id))
            .await
    }
}

/* ─────────────────────────── executor ─────────────────────────── */

#[async_trait::async_trait]
impl PimOpExecutor for ContactsService {
    fn kind_prefix(&self) -> &'static str {
        ops::KIND_PREFIX
    }

    fn module(&self) -> SyncModule {
        SyncModule::Contacts
    }

    async fn execute(&self, op: &PendingOp) -> Result<()> {
        let decoded = ops::decode(op)?;
        let account = self.load_account(&op.account_id).await?;
        let Some(provider) = self.provider_for(&account) else {
            return Ok(());
        };
        match &decoded {
            ContactOp::Save {
                contact_id,
                snapshot: previous_snapshot,
            } => {
                let (contact, snapshot) = self
                    .ctx
                    .db
                    .read({
                        let id = contact_id.clone();
                        move |conn| {
                            let contact = contacts_repo::get_required(conn, &id)?;
                            let snapshot = snapshot_contact(conn, &id)?;
                            Ok((contact, snapshot))
                        }
                    })
                    .await?;
                let Some(snapshot) = snapshot else {
                    return Ok(());
                };
                let previous = previous_snapshot
                    .as_ref()
                    .map(|state| state.contact.clone());
                let previous_groups = previous
                    .as_ref()
                    .map(|contact| contact.group_ids.clone())
                    .unwrap_or_default();
                let previous_favorite = previous
                    .as_ref()
                    .map(|contact| contact.is_favorite)
                    .unwrap_or(false);
                let Some(mapping) = self.mapping_for(&snapshot.contact.address_book_id).await?
                else {
                    return Err(AppError::conflict("address book is not mapped yet"));
                };
                let book = self.book_from_mapping(&mapping);
                let group_names = {
                    let id = contact.id.clone();
                    self.ctx
                        .db
                        .read(move |conn| group_names_for(conn, &id))
                        .await?
                };
                let current = RemoteContactRef {
                    remote_id: snapshot.remote_id.clone(),
                    href: snapshot.href.clone(),
                    etag: snapshot.etag.clone(),
                    raw_vcard: snapshot.raw_vcard.clone(),
                };
                let result = if current.remote_id.is_some() || current.href.is_some() {
                    provider
                        .update_contact(&account, &book, &current, &contact, &group_names)
                        .await?
                } else {
                    provider
                        .create_contact(&account, &book, &contact, &group_names)
                        .await?
                };
                let remote_id = result
                    .remote_id
                    .clone()
                    .or_else(|| snapshot.remote_id.clone());
                let id = contact.id.clone();
                self.ctx
                    .db
                    .write(move |conn| {
                        conn.execute(
                            "UPDATE contact SET remote_id = COALESCE(?2, remote_id),
                                                etag = COALESCE(?3, etag),
                                                href = COALESCE(?4, href),
                                                updated_at = ?5
                             WHERE id = ?1",
                            rusqlite::params![
                                id,
                                result.remote_id,
                                result.etag,
                                result.href,
                                repo::now_string()
                            ],
                        )?;
                        Ok(())
                    })
                    .await?;
                // Google keeps group membership and the favourite flag outside
                // the person resource, so they need explicit write-back.
                if provider.name() == "google" {
                    if let Some(remote_id) = remote_id.as_deref() {
                        if contact.is_favorite != previous_favorite {
                            provider
                                .set_starred(&account, remote_id, contact.is_favorite)
                                .await?;
                        }
                    }
                }
                if provider.groups_via_membership() {
                    self.push_google_memberships(&account, &contact.id, &previous_groups)
                        .await?;
                }
            }
            ContactOp::Delete { contacts } => {
                for snapshot in contacts {
                    let Some(mapping) = self.mapping_for(&snapshot.contact.address_book_id).await?
                    else {
                        continue;
                    };
                    let book = self.book_from_mapping(&mapping);
                    let current = RemoteContactRef {
                        remote_id: snapshot.remote_id.clone(),
                        href: snapshot.href.clone(),
                        etag: snapshot.etag.clone(),
                        raw_vcard: snapshot.raw_vcard.clone(),
                    };
                    provider.delete_contact(&account, &book, &current).await?;
                }
            }
            ContactOp::Favorite {
                contact_id, value, ..
            } => {
                let snapshot = self.snapshot_contact(contact_id).await?;
                if let Some(snapshot) = snapshot {
                    if let Some(remote_id) = snapshot.remote_id {
                        provider.set_starred(&account, &remote_id, *value).await?;
                    }
                }
            }
            ContactOp::GroupCreate { group_id } => {
                let group = match self.load_group(group_id).await {
                    Ok(group) => group,
                    Err(_) => return Ok(()),
                };
                if group_remote_id(&self.ctx, group_id).await?.is_some() {
                    return Ok(());
                }
                let book = match self.default_book(&account).await {
                    Ok(book) => book,
                    // The address book has not been synced yet.  Retrying is
                    // important: returning `Ok` here would silently drop the
                    // group forever.
                    Err(_) => {
                        return Err(AppError::conflict(
                            "address book is not mapped yet; retrying group creation",
                        ))
                    }
                };
                let remote = provider.create_group(&account, &book, &group.name).await?;
                set_group_remote_id(&self.ctx, group_id, &remote.remote_id).await?;
            }
            ContactOp::GroupRename { group_id, .. } => {
                let Some(remote_id) = group_remote_id(&self.ctx, group_id).await? else {
                    return Ok(());
                };
                let group = self.load_group(group_id).await?;
                provider
                    .rename_group(&account, &remote_id, &group.name)
                    .await?;
            }
            ContactOp::GroupDelete { snapshot } => {
                if let Some(remote_id) = &snapshot.remote_id {
                    provider.delete_group(&account, remote_id).await?;
                }
            }
            ContactOp::GroupMembers {
                group_id,
                added,
                removed,
            } => {
                let Some(remote_id) = group_remote_id(&self.ctx, group_id).await? else {
                    // The group itself has not reached the provider yet.
                    return Err(AppError::conflict("contact group is not synced yet"));
                };
                let add = self.remote_ids_for(added).await?;
                let remove = self.remote_ids_for(removed).await?;
                provider
                    .modify_group_members(&account, &remote_id, &add, &remove)
                    .await?;
            }
        }
        // Let the scheduler push a follow-up sync so server-side changes
        // (etags, generated ids) are reflected quickly.
        emit_contacts_changed(
            self.ctx.events.as_ref(),
            self.changed_books(&decoded).await?,
        );
        Ok(())
    }

    async fn rollback(&self, op: &PendingOp) -> Result<()> {
        let decoded = ops::decode(op)?;
        // Same reason as the task executor: the rollback rewrites the very rows
        // the contact list is rendering, and the failure toast alone leaves the
        // UI showing data that no longer exists in the database.
        let restore = match decoded {
            ContactOp::Save {
                contact_id,
                snapshot,
            } => {
                let id = contact_id.clone();
                match snapshot {
                    Some(snapshot) => {
                        self.ctx
                            .db
                            .write(move |conn| restore_contact(conn, &snapshot))
                            .await
                    }
                    None => {
                        self.ctx
                            .db
                            .write(move |conn| {
                                let _ = contacts_repo::delete_many(conn, &[id]);
                                Ok(())
                            })
                            .await
                    }
                }
            }
            ContactOp::Delete { contacts } => {
                self.ctx
                    .db
                    .write(move |conn| {
                        for snapshot in &contacts {
                            restore_contact(conn, snapshot)?;
                        }
                        Ok(())
                    })
                    .await
            }
            ContactOp::Favorite {
                contact_id,
                previous,
                ..
            } => {
                self.ctx
                    .db
                    .write(move |conn| {
                        contacts_repo::set_favorite(conn, &contact_id, previous)
                    })
                    .await
            }
            ContactOp::GroupCreate { group_id } => {
                self.ctx
                    .db
                    .write(move |conn| {
                        let _ = contacts_repo::delete_group(conn, &group_id);
                        Ok(())
                    })
                    .await
            }
            ContactOp::GroupRename {
                group_id,
                previous_name,
            } => {
                self.ctx
                    .db
                    .write(move |conn| {
                        let _ = contacts_repo::rename_group(conn, &group_id, &previous_name);
                        Ok(())
                    })
                    .await
            }
            ContactOp::GroupDelete { snapshot } => {
                self.ctx
                    .db
                    .write(move |conn| {
                        conn.execute(
                            "INSERT OR IGNORE INTO contact_group
                                 (id, address_book_id, account_id, name, remote_id, created_at, updated_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                            rusqlite::params![
                                snapshot.group.id,
                                snapshot.group.address_book_id,
                                snapshot.group.account_id,
                                snapshot.group.name,
                                snapshot.remote_id,
                                repo::now_string()
                            ],
                        )?;
                        for contact_id in &snapshot.member_ids {
                            conn.execute(
                                "INSERT OR IGNORE INTO contact_group_member
                                     (group_id, contact_id, added_at)
                                 VALUES (?1, ?2, ?3)",
                                rusqlite::params![
                                    snapshot.group.id,
                                    contact_id,
                                    repo::now_string()
                                ],
                            )?;
                        }
                        Ok(())
                    })
                    .await
            }
            ContactOp::GroupMembers {
                group_id,
                added,
                removed,
            } => {
                self.ctx
                    .db
                    .write(move |conn| {
                        for contact_id in &added {
                            conn.execute(
                                "DELETE FROM contact_group_member WHERE group_id = ?1 AND contact_id = ?2",
                                rusqlite::params![group_id, contact_id],
                            )?;
                        }
                        for contact_id in &removed {
                            conn.execute(
                                "INSERT OR IGNORE INTO contact_group_member
                                     (group_id, contact_id, added_at)
                                 VALUES (?1, ?2, ?3)",
                                rusqlite::params![group_id, contact_id, repo::now_string()],
                            )?;
                        }
                        Ok(())
                    })
                    .await
            }
        };
        if restore.is_ok() {
            // An empty list means "anything may have changed".
            emit_contacts_changed(self.ctx.events.as_ref(), Vec::new());
        }
        restore
    }
}

/* ─────────────────────────── helpers ─────────────────────────── */

impl ContactsService {
    async fn mapping_for(&self, book_id: &str) -> Result<Option<crate::store::AddressBookRemote>> {
        let id = book_id.to_string();
        self.ctx
            .db
            .read(move |conn| crate::store::get_book_by_local(conn, &id))
            .await
    }

    fn book_from_mapping(&self, mapping: &crate::store::AddressBookRemote) -> RemoteAddressBook {
        RemoteAddressBook {
            local_id: mapping.address_book_id.clone(),
            remote_id: mapping.remote_id.clone(),
            href: mapping.href.clone(),
            name: String::new(),
            read_only: false,
        }
    }

    async fn default_book(&self, account: &Account) -> Result<RemoteAddressBook> {
        let account_id = account.id.clone();
        let mapping = self
            .ctx
            .db
            .read(move |conn| {
                let books = crate::store::list_books_for_account(conn, &account_id)?;
                Ok(books.into_iter().next())
            })
            .await?;
        mapping
            .map(|mapping| self.book_from_mapping(&mapping))
            .ok_or_else(|| AppError::conflict("account has no synced address book"))
    }

    async fn load_group(&self, id: &str) -> Result<ContactGroup> {
        let id = id.to_string();
        self.ctx
            .db
            .read(move |conn| {
                contacts_repo::get_group(conn, &id)?
                    .ok_or_else(|| AppError::not_found("group not found"))
            })
            .await
    }

    async fn remote_ids_for(&self, contact_ids: &[String]) -> Result<Vec<String>> {
        let ids = contact_ids.to_vec();
        self.ctx
            .db
            .read(move |conn| {
                let mut out = Vec::new();
                for id in ids {
                    if let Some(snapshot) = snapshot_contact(conn, &id)? {
                        if let Some(remote_id) = snapshot.remote_id {
                            out.push(remote_id);
                        }
                    }
                }
                Ok(out)
            })
            .await
    }

    async fn changed_books(&self, op: &ContactOp) -> Result<Vec<String>> {
        let mut books = Vec::new();
        match op {
            ContactOp::Save { contact_id, .. } => {
                if let Some(snapshot) = self.snapshot_contact(contact_id).await? {
                    books.push(snapshot.contact.address_book_id);
                }
            }
            ContactOp::Delete { contacts } => {
                for snapshot in contacts {
                    books.push(snapshot.contact.address_book_id.clone());
                }
            }
            ContactOp::Favorite { contact_id, .. } => {
                if let Some(snapshot) = self.snapshot_contact(contact_id).await? {
                    books.push(snapshot.contact.address_book_id);
                }
            }
            ContactOp::GroupCreate { group_id }
            | ContactOp::GroupRename { group_id, .. }
            | ContactOp::GroupMembers { group_id, .. } => {
                if let Ok(group) = self.load_group(group_id).await {
                    books.push(group.address_book_id);
                }
            }
            ContactOp::GroupDelete { snapshot } => {
                books.push(snapshot.group.address_book_id.clone());
            }
        }
        books.sort();
        books.dedup();
        Ok(books)
    }

    async fn push_google_memberships(
        &self,
        account: &Account,
        contact_id: &str,
        previous_group_ids: &[String],
    ) -> Result<()> {
        let id = contact_id.to_string();
        let previous = previous_group_ids.to_vec();
        let (current_groups, removed_groups) = self
            .ctx
            .db
            .read(move |conn| {
                let memberships = contacts_repo::group_ids_for_contact(conn, &id)?;
                let mut current = Vec::new();
                for group_id in &memberships {
                    if let Some(remote_id) = group_remote_id_conn(conn, group_id)? {
                        current.push((group_id.clone(), remote_id));
                    }
                }
                let mut removed = Vec::new();
                for group_id in &previous {
                    if memberships.contains(group_id) {
                        continue;
                    }
                    if let Some(remote_id) = group_remote_id_conn(conn, group_id)? {
                        removed.push(remote_id);
                    }
                }
                Ok((current, removed))
            })
            .await?;
        let snapshot = self.snapshot_contact(contact_id).await?;
        let Some(remote_id) = snapshot.and_then(|snapshot| snapshot.remote_id) else {
            return Ok(());
        };
        let Some(provider) = self.provider_for(account) else {
            return Ok(());
        };
        for (_, group) in &current_groups {
            provider
                .modify_group_members(account, group, std::slice::from_ref(&remote_id), &[])
                .await?;
        }
        // Memberships removed in the editor must be removed remotely too,
        // otherwise the next sync restores them.
        for group in removed_groups {
            provider
                .modify_group_members(account, &group, &[], std::slice::from_ref(&remote_id))
                .await?;
        }
        Ok(())
    }
}

async fn op_label(contact_id: &str, service: &ContactsService) -> String {
    service
        .snapshot_contact(contact_id)
        .await
        .ok()
        .flatten()
        .map(|snapshot| snapshot.contact.label())
        .unwrap_or_default()
}

/* ─────────────────────────── sync logic ─────────────────────────── */

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredCursor {
    sync_token: Option<String>,
    ctag: Option<String>,
}

fn cursor_key(book_id: &str) -> String {
    format!("book:{book_id}")
}

fn load_cursor(conn: &rusqlite::Connection, book_id: &str) -> Result<Option<StoredCursor>> {
    // Cursor rows are account scoped; the book id already uniquely identifies
    // the account, so scan every bucket by key.
    let mut stmt =
        conn.prepare("SELECT value FROM sync_state WHERE module = ?1 AND key = ?2 LIMIT 1")?;
    let mut rows = stmt.query(rusqlite::params![SYNC_MODULE, cursor_key(book_id)])?;
    if let Some(row) = rows.next()? {
        let value: String = row.get(0)?;
        return serde_json::from_str::<StoredCursor>(&value)
            .map(Some)
            .map_err(|error| AppError::db(format!("invalid contact cursor: {error}")));
    }
    Ok(None)
}

fn save_cursor(
    conn: &rusqlite::Connection,
    book_id: &str,
    sync_token: Option<&str>,
    ctag: Option<&str>,
) -> Result<()> {
    let account_id: String = conn.query_row(
        "SELECT account_id FROM address_book WHERE id = ?1",
        rusqlite::params![book_id],
        |row| row.get(0),
    )?;
    let cursor = StoredCursor {
        sync_token: sync_token.map(str::to_string),
        ctag: ctag.map(str::to_string),
    };
    let encoded = serde_json::to_string(&cursor)
        .map_err(|error| AppError::internal(format!("cannot encode contact cursor: {error}")))?;
    sync_state::set(
        conn,
        &account_id,
        SYNC_MODULE,
        &cursor_key(book_id),
        &encoded,
    )
}

fn local_etags(conn: &rusqlite::Connection, book_id: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT href, etag FROM contact
         WHERE address_book_id = ?1 AND href IS NOT NULL AND etag IS NOT NULL",
    )?;
    let mut rows = stmt.query(rusqlite::params![book_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((row.get(0)?, row.get(1)?));
    }
    Ok(out)
}

fn create_remote_book(
    conn: &rusqlite::Connection,
    account_id: &str,
    remote: &RemoteAddressBook,
) -> Result<AddressBook> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM address_book WHERE account_id = ?1",
        rusqlite::params![account_id],
        |row| row.get(0),
    )?;
    let mut book = AddressBook {
        id: new_id(),
        account_id: account_id.to_string(),
        name: remote.name.clone(),
        is_read_only: remote.read_only,
        is_default: count == 0,
    };
    repo::address_books::insert(conn, &mut book)
}

fn apply_delta(
    conn: &rusqlite::Connection,
    account_id: &str,
    book: &AddressBook,
    delta: &ContactDelta,
) -> Result<()> {
    // Deletions first.
    for remote_id in &delta.deleted_remote_ids {
        if let Some(contact) = contacts_repo::by_remote_id(conn, &book.id, remote_id)? {
            contacts_repo::delete_many(conn, &[contact.id])?;
        }
    }
    for href in &delta.deleted_hrefs {
        let id: Option<String> = conn
            .query_row(
                "SELECT id FROM contact WHERE address_book_id = ?1 AND href = ?2 LIMIT 1",
                rusqlite::params![book.id, href],
                |row| row.get(0),
            )
            .ok();
        if let Some(id) = id {
            contacts_repo::delete_many(conn, &[id])?;
        }
    }

    // Groups (Google exposes a complete list).
    let mut group_by_name: HashMap<String, String> = HashMap::new();
    for group in &delta.groups {
        let id = match group_remote_id_lookup(conn, &book.id, &group.remote_id)? {
            Some((id, _)) => {
                conn.execute(
                    "UPDATE contact_group SET name = ?2, updated_at = ?3 WHERE id = ?1",
                    rusqlite::params![id, group.name, repo::now_string()],
                )?;
                id
            }
            None => {
                let id = new_id();
                conn.execute(
                    "INSERT INTO contact_group
                         (id, address_book_id, account_id, name, remote_id, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                    rusqlite::params![
                        id,
                        book.id,
                        account_id,
                        group.name,
                        group.remote_id,
                        repo::now_string()
                    ],
                )?;
                id
            }
        };
        if !group.name.trim().is_empty() {
            group_by_name.insert(group.name.to_lowercase(), id);
        }
    }
    if delta.groups_full {
        let known: HashSet<String> = delta
            .groups
            .iter()
            .map(|group| group.remote_id.clone())
            .collect();
        let mut stmt = conn.prepare(
            "SELECT id, remote_id FROM contact_group
             WHERE address_book_id = ?1 AND remote_id IS NOT NULL",
        )?;
        let mut rows = stmt.query(rusqlite::params![book.id])?;
        let mut stale = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let remote: String = row.get(1)?;
            if !known.contains(&remote) {
                stale.push(id);
            }
        }
        drop(rows);
        drop(stmt);
        for id in stale {
            let _ = contacts_repo::delete_group(conn, &id);
        }
    }

    // Existing local group names (including groups created locally).
    let mut existing_groups: HashMap<String, String> = HashMap::new();
    for group in contacts_repo::list_groups(conn, Some(&book.id))? {
        existing_groups.insert(group.name.to_lowercase(), group.id);
    }
    existing_groups.extend(group_by_name);

    let mut seen_remote: HashSet<String> = HashSet::new();
    let mut seen_href: HashSet<String> = HashSet::new();
    for remote in &delta.contacts {
        if let Some(remote_id) = &remote.remote_id {
            seen_remote.insert(remote_id.clone());
        }
        if let Some(href) = &remote.href {
            seen_href.insert(href.clone());
        }
        let existing = find_existing_contact(conn, &book.id, remote)?;
        let id = existing
            .as_ref()
            .map(|contact| contact.id.clone())
            .unwrap_or_else(new_id);
        let mut contact = remote.contact.clone();
        contact.id = id;
        contact.address_book_id = book.id.clone();
        contact.account_id = account_id.to_string();
        contact.updated_at = Some(Utc::now());
        if let Some(previous) = &existing {
            if delta.favorites_authoritative {
                // Google's starred group is the source of truth.
                contact.is_favorite = remote.contact.is_favorite;
            } else {
                contact.is_favorite = contact.is_favorite || previous.is_favorite;
            }
            if contact.photo.is_none() {
                // Providers that did not deliver a photo in this delta (or hit
                // their download budget) must not wipe the local avatar.
                contact.photo = previous.photo.clone();
            }
        }
        // Resolve group membership by name, creating local groups on demand.
        let mut group_ids = Vec::new();
        for name in &remote.group_names {
            let key = name.to_lowercase();
            let group_id = match existing_groups.get(&key) {
                Some(id) => id.clone(),
                None => {
                    let id = new_id();
                    conn.execute(
                        "INSERT INTO contact_group
                             (id, address_book_id, account_id, name, remote_id, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5)",
                        rusqlite::params![id, book.id, account_id, name, repo::now_string()],
                    )?;
                    existing_groups.insert(key, id.clone());
                    id
                }
            };
            group_ids.push(group_id);
        }
        contact.group_ids = group_ids;
        contacts_repo::upsert_remote(
            conn,
            &contact,
            remote.etag.as_deref(),
            remote.remote_id.as_deref(),
            remote.href.as_deref(),
            remote.raw_vcard.as_deref(),
        )?;
    }

    if delta.full_sync {
        let mut stmt =
            conn.prepare("SELECT id, remote_id, href FROM contact WHERE address_book_id = ?1")?;
        let mut rows = stmt.query(rusqlite::params![book.id])?;
        let mut stale = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let remote_id: Option<String> = row.get(1)?;
            let href: Option<String> = row.get(2)?;
            let seen = remote_id
                .as_ref()
                .is_some_and(|id| seen_remote.contains(id))
                || href.as_ref().is_some_and(|href| seen_href.contains(href));
            let has_remote = remote_id.is_some() || href.is_some();
            if has_remote && !seen {
                stale.push(id);
            }
        }
        drop(rows);
        drop(stmt);
        for id in stale {
            contacts_repo::delete_many(conn, &[id])?;
        }
    }
    Ok(())
}

#[cfg(test)]
fn list_query_for_book(book_id: &str) -> ContactListQuery {
    ContactListQuery {
        address_book_id: Some(book_id.to_string()),
        group_id: None,
        query: None,
        favorites_only: false,
        cursor: None,
        limit: 50,
    }
}

fn find_existing_contact(
    conn: &rusqlite::Connection,
    book_id: &str,
    remote: &RemoteContact,
) -> Result<Option<Contact>> {
    if let Some(remote_id) = &remote.remote_id {
        if let Some(contact) = contacts_repo::by_remote_id(conn, book_id, remote_id)? {
            return Ok(Some(contact));
        }
    }
    if let Some(href) = &remote.href {
        let id: Option<String> = conn
            .query_row(
                "SELECT id FROM contact WHERE address_book_id = ?1 AND href = ?2 LIMIT 1",
                rusqlite::params![book_id, href],
                |row| row.get(0),
            )
            .ok();
        if let Some(id) = id {
            return contacts_repo::get(conn, &id);
        }
    }
    Ok(None)
}

fn snapshot_contact(conn: &rusqlite::Connection, id: &str) -> Result<Option<ContactSnapshot>> {
    let Some(contact) = contacts_repo::get(conn, id)? else {
        return Ok(None);
    };
    let meta = contacts_repo::sync_meta(conn, id)?;
    Ok(Some(ContactSnapshot {
        contact,
        etag: meta.as_ref().and_then(|meta| meta.etag.clone()),
        remote_id: meta.as_ref().and_then(|meta| meta.remote_id.clone()),
        href: meta.as_ref().and_then(|meta| meta.href.clone()),
        raw_vcard: meta.as_ref().and_then(|meta| meta.raw_vcard.clone()),
    }))
}

fn group_snapshot(conn: &rusqlite::Connection, id: &str) -> Result<Option<GroupSnapshot>> {
    let Some(group) = contacts_repo::get_group(conn, id)? else {
        return Ok(None);
    };
    let members = contacts_repo::members(conn, id)?;
    Ok(Some(GroupSnapshot {
        remote_id: group_remote_id_conn(conn, id)?,
        group,
        member_ids: members.into_iter().map(|contact| contact.id).collect(),
    }))
}

fn restore_contact(conn: &rusqlite::Connection, snapshot: &ContactSnapshot) -> Result<()> {
    let contact = &snapshot.contact;
    contacts_repo::upsert_remote(
        conn,
        contact,
        snapshot.etag.as_deref(),
        snapshot.remote_id.as_deref(),
        snapshot.href.as_deref(),
        snapshot.raw_vcard.as_deref(),
    )?;
    Ok(())
}

fn group_names_for(conn: &rusqlite::Connection, contact_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT g.name FROM contact_group g
         JOIN contact_group_member m ON m.group_id = g.id
         WHERE m.contact_id = ?1 ORDER BY g.name ASC",
    )?;
    let mut rows = stmt.query(rusqlite::params![contact_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row.get(0)?);
    }
    Ok(out)
}

fn ensure_groups(
    conn: &rusqlite::Connection,
    book: &AddressBook,
    names: &[String],
) -> Result<Vec<String>> {
    let existing = contacts_repo::list_groups(conn, Some(&book.id))?;
    let mut by_name: HashMap<String, String> = existing
        .into_iter()
        .map(|group| (group.name.to_lowercase(), group.id))
        .collect();
    let mut ids = Vec::new();
    for name in names {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = trimmed.to_lowercase();
        let id = match by_name.get(&key) {
            Some(id) => id.clone(),
            None => {
                let group = contacts_repo::create_group(conn, &book.id, trimmed)?;
                by_name.insert(key, group.id.clone());
                group.id
            }
        };
        ids.push(id);
    }
    ids.dedup();
    Ok(ids)
}

fn group_remote_id_conn(conn: &rusqlite::Connection, group_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT remote_id FROM contact_group WHERE id = ?1",
            rusqlite::params![group_id],
            |row| row.get(0),
        )
        .ok()
        .flatten())
}

fn group_remote_id_lookup(
    conn: &rusqlite::Connection,
    book_id: &str,
    remote_id: &str,
) -> Result<Option<(String, Option<String>)>> {
    let row: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT id, remote_id FROM contact_group
             WHERE address_book_id = ?1 AND remote_id = ?2 LIMIT 1",
            rusqlite::params![book_id, remote_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();
    Ok(row)
}

async fn group_remote_id(ctx: &CoreContext, group_id: &str) -> Result<Option<String>> {
    let id = group_id.to_string();
    ctx.db
        .read(move |conn| group_remote_id_conn(conn, &id))
        .await
}

async fn set_group_remote_id(ctx: &CoreContext, group_id: &str, remote_id: &str) -> Result<()> {
    let id = group_id.to_string();
    let remote = remote_id.to_string();
    ctx.db
        .write(move |conn| {
            conn.execute(
                "UPDATE contact_group SET remote_id = ?2, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![id, remote, repo::now_string()],
            )?;
            Ok(())
        })
        .await
}

fn require_contact(contact: &Contact) -> Result<()> {
    if contact.display_name.trim().is_empty() && contact.emails.is_empty() {
        return Err(invalid("contact needs a name or an e-mail address"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ctx() -> CoreContext {
        let dir = std::env::temp_dir().join(format!("nuncio-pim-contacts-{}", new_id()));
        let ctx = CoreContext::for_test(nuncio_core::Paths::new(dir))
            .await
            .expect("ctx");
        ctx.migrate(crate::store::PIM_NAMESPACE, crate::store::PIM_MIGRATIONS)
            .expect("migrations");
        ctx
    }

    fn service(ctx: &CoreContext) -> ContactsService {
        let http = HttpClient::new(ctx.creds.clone()).expect("http");
        ContactsService::new(ctx.clone(), http)
    }

    fn contact(book: &str, name: &str, email: &str) -> Contact {
        let mut contact = Contact::new("", book, "local");
        contact.display_name = name.into();
        contact.emails = vec![nuncio_core::models::ContactField {
            r#type: nuncio_core::models::ContactFieldType::Work,
            value: email.into(),
            is_primary: true,
        }];
        contact
    }

    #[tokio::test]
    async fn local_crud_and_groups() {
        let ctx = ctx().await;
        let service = service(&ctx);
        let created = service
            .contact_create(contact("local-contacts", "Ada Lovelace", "ada@example.com"))
            .await
            .expect("create");
        assert_eq!(created.account_id, "local");

        let group = service
            .group_create("local-contacts", "Mathematicians")
            .await
            .expect("group");
        service
            .group_set_members(&group.id, vec![created.id.clone()])
            .await
            .expect("members");

        let page = service
            .contact_list(ContactListQuery {
                address_book_id: Some("local-contacts".into()),
                group_id: Some(group.id.clone()),
                query: None,
                favorites_only: false,
                cursor: None,
                limit: 50,
            })
            .await
            .expect("list");
        assert_eq!(page.items.len(), 1);

        service
            .contact_set_favorite(&created.id, true)
            .await
            .expect("favorite");
        let found = service
            .contact_find_by_email("ADA@example.com")
            .await
            .expect("find")
            .expect("found");
        assert!(found.is_favorite);

        service
            .contact_delete(vec![created.id.clone()])
            .await
            .expect("delete");
        assert!(service
            .contact_find_by_email("ada@example.com")
            .await
            .expect("find")
            .is_none());
    }

    #[tokio::test]
    async fn import_export_round_trip() {
        let ctx = ctx().await;
        let service = service(&ctx);
        let dir = std::env::temp_dir().join(format!("nuncio-vcf-{}", new_id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let source = dir.join("in.vcf");
        let body = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Alan Turing\r\nEMAIL:alan@example.com\r\nCATEGORIES:Friends\r\nEND:VCARD\r\n";
        std::fs::write(&source, body).expect("write");

        let count = service
            .contact_import_vcf(source.to_str().expect("path"), "local-contacts")
            .await
            .expect("import");
        assert_eq!(count, 1);
        let page = service
            .contact_list(ContactListQuery {
                address_book_id: Some("local-contacts".into()),
                group_id: None,
                query: Some("Alan".into()),
                favorites_only: false,
                cursor: None,
                limit: 10,
            })
            .await
            .expect("list");
        assert_eq!(page.items.len(), 1);
        let groups = service
            .group_list(Some("local-contacts".into()))
            .await
            .expect("groups");
        assert!(groups.iter().any(|group| group.name == "Friends"));

        let out = dir.join("out.vcf");
        service
            .contact_export_vcf(vec![page.items[0].id.clone()], out.to_str().expect("path"))
            .await
            .expect("export");
        let text = std::fs::read_to_string(out).expect("read");
        assert!(text.contains("FN:Alan Turing"));
        assert!(text.contains("CATEGORIES:Friends"));
    }

    #[tokio::test]
    async fn applies_provider_delta() {
        let ctx = ctx().await;
        let account = ctx.db.read(repo::accounts::local).await.expect("local");
        let book = AddressBook {
            id: "book-1".into(),
            account_id: account.id.clone(),
            name: "Contacts".into(),
            is_read_only: false,
            is_default: true,
        };
        let remote = RemoteContact {
            contact: contact("", "Grace Hopper", "grace@example.com"),
            remote_id: Some("remote-1".into()),
            href: None,
            etag: Some("e1".into()),
            raw_vcard: None,
            group_names: vec!["Navy".into()],
        };
        let delta = ContactDelta {
            contacts: vec![remote],
            groups: Vec::new(),
            groups_full: false,
            ..Default::default()
        };
        ctx.db
            .write(move |conn| {
                repo::address_books::insert(conn, &mut book.clone())?;
                apply_delta(conn, "local", &book, &delta)
            })
            .await
            .expect("apply");
        let page = ctx
            .db
            .read(|conn| contacts_repo::list(conn, &list_query_for_book("book-1")))
            .await
            .expect("list");
        assert_eq!(page.items.len(), 1);
        let groups = ctx
            .db
            .read(|conn| contacts_repo::list_groups(conn, Some("book-1")))
            .await
            .expect("groups");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "Navy");
    }

    #[tokio::test]
    async fn provider_delta_without_photo_keeps_local_avatar() {
        let ctx = ctx().await;
        let account = ctx.db.read(repo::accounts::local).await.expect("local");
        let book = AddressBook {
            id: "book-2".into(),
            account_id: account.id.clone(),
            name: "Contacts".into(),
            is_read_only: false,
            is_default: true,
        };
        let mut remote_contact = contact("", "Ada Lovelace", "ada@example.com");
        remote_contact.photo = Some("data:image/png;base64,AAAA".into());
        let first = ContactDelta {
            contacts: vec![RemoteContact {
                contact: remote_contact,
                remote_id: Some("remote-2".into()),
                href: None,
                etag: Some("e1".into()),
                raw_vcard: None,
                group_names: Vec::new(),
            }],
            ..Default::default()
        };
        // A later delta omits the photo (Graph never returns it inline;
        // Google's download budget may skip it).
        let second = ContactDelta {
            contacts: vec![RemoteContact {
                contact: contact("", "Ada Lovelace", "ada@example.com"),
                remote_id: Some("remote-2".into()),
                href: None,
                etag: Some("e2".into()),
                raw_vcard: None,
                group_names: Vec::new(),
            }],
            ..Default::default()
        };
        let book_for_db = book.clone();
        ctx.db
            .write(move |conn| {
                repo::address_books::insert(conn, &mut book_for_db.clone())?;
                apply_delta(conn, "local", &book_for_db, &first)
            })
            .await
            .expect("first delta");
        let book_for_db = book.clone();
        ctx.db
            .write(move |conn| apply_delta(conn, "local", &book_for_db, &second))
            .await
            .expect("second delta");
        let stored = ctx
            .db
            .read(|conn| contacts_repo::by_remote_id(conn, "book-2", "remote-2"))
            .await
            .expect("query")
            .expect("contact");
        assert_eq!(
            stored.photo.as_deref(),
            Some("data:image/png;base64,AAAA"),
            "remote deltas without a photo must not clear the avatar"
        );
    }

    #[test]
    fn decodes_vcf_encodings() {
        let body = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:张三\r\nEND:VCARD\r\n";

        // Plain UTF-8.
        assert_eq!(decode_vcf_bytes(body.as_bytes()).expect("utf8"), body);

        // UTF-8 with BOM.
        let mut with_bom = vec![0xEF, 0xBB, 0xBF];
        with_bom.extend_from_slice(body.as_bytes());
        assert_eq!(decode_vcf_bytes(&with_bom).expect("utf8 bom"), body);

        // UTF-16 LE / BE with BOM.
        let mut utf16_le = vec![0xFF, 0xFE];
        for unit in body.encode_utf16() {
            utf16_le.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_vcf_bytes(&utf16_le).expect("utf16le"), body);

        let mut utf16_be = vec![0xFE, 0xFF];
        for unit in body.encode_utf16() {
            utf16_be.extend_from_slice(&unit.to_be_bytes());
        }
        assert_eq!(decode_vcf_bytes(&utf16_be).expect("utf16be"), body);
    }

    #[test]
    fn decodes_gb18030_and_charset_hints() {
        let body = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:李四\r\nEND:VCARD\r\n";
        let (encoded, _used, _errors) = encoding_rs::GB18030.encode(body);
        assert_eq!(
            decode_vcf_bytes(encoded.as_ref()).expect("gb18030"),
            body,
            "invalid UTF-8 without a hint falls back to GB18030"
        );

        // An explicit charset hint wins over the GB18030 fallback.
        let (big5, _used, _errors) = encoding_rs::BIG5.encode(body);
        let mut hinted = b"BEGIN:VCARD\r\nNOTE;CHARSET=big5:x\r\n".to_vec();
        hinted.extend_from_slice(big5.as_ref());
        let decoded = decode_vcf_bytes(&hinted).expect("big5");
        assert!(decoded.starts_with("BEGIN:VCARD"));
        assert!(decoded.contains("李四"), "charset hint was not honoured");
    }
}
