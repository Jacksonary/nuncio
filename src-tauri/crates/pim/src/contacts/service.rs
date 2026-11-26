//! Contact sync engine, command facade and pending-operation executor.
//!
//! Reads come straight from SQLite.  Writes update the database first and then
//! enqueue a `pim.contact.*` operation; the shared [`crate::op_worker`] drains
//! it with retry/rollback and emits `contacts://changed` after local changes.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use nuncio_core::events::emit_contacts_changed;
use nuncio_core::models::{
    new_id, Account, AddressBook, Contact, ContactGroup, ContactListQuery, ContactPage,
    NewPendingOp, PendingOp, PendingOpState, SyncModule,
};
use nuncio_core::providers::DavService;
use nuncio_core::repo::{self, contacts as contacts_repo, sync_state};
use nuncio_core::CoreContext;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
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
use crate::dav::href_key;
use crate::error::{invalid, AppError, Result};
use crate::http::HttpClient;
use crate::op_worker::{self, PimOpExecutor};
use crate::status::record_failure;

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
                record_failure(
                    &self.ctx.sync_status,
                    &account.id,
                    SyncModule::Contacts,
                    &error,
                );
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
        // Anything written after this instant is invisible to the delta request
        // below; the apply transaction protects those objects instead of
        // overwriting them with the older server copy.
        let fetch_start = Utc::now();
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
        let mut delta = provider.sync_contacts(account, remote, &cursor).await?;
        delta.window_start = Some(fetch_start);
        // A release of book-level isolation is only observable through the
        // quarantine replay, which may import objects even though this batch is
        // empty. Report the book as changed so the UI refreshes.
        let pending_replay = self
            .ctx
            .db
            .read({
                let account_id = account.id.clone();
                let book_id = book.id.clone();
                move |conn| crate::store::has_quarantined_contacts(conn, &account_id, &book_id)
            })
            .await?;
        let changed = delta.full_sync
            || !delta.contacts.is_empty()
            || !delta.deleted_remote_ids.is_empty()
            || !delta.deleted_hrefs.is_empty()
            || delta.groups_full
            || pending_replay;
        let account_id = account.id.clone();
        let book_clone = book.clone();
        let delta_for_db = delta.clone();
        self.ctx
            .db
            .write(move |conn| {
                apply_delta_and_cursor(conn, &account_id, &book_clone, &delta_for_db)
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
        let book = self
            .ctx
            .db
            .read({
                let book_id = book_id.clone();
                move |conn| repo::address_books::get_required(conn, &book_id)
            })
            .await?;
        let account = self.load_account(&book.account_id).await?;
        let is_provider_account = self.provider_name(&account).is_some();
        let account_id = account.id.clone();
        let saved = self
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
                    // Insert and enqueue in one transaction: an enqueue
                    // failure must not leave a row no operation owns.
                    if is_provider_account {
                        let op_id = new_id();
                        let op = ContactOp::Save {
                            contact_id: saved.id.clone(),
                            assigned_uid: None,
                            snapshot: None,
                        };
                        enqueue_op_with_id_sync(
                            conn,
                            &op_id,
                            &account_id,
                            &op,
                            format!("创建联系人「{}」", saved.label()),
                        )?;
                        // Claim the identity so a late receipt from an older
                        // save cannot overwrite the identity this operation
                        // confirms.
                        crate::store::bump_contact_identity(conn, &saved.id, Some(op_id.as_str()))?;
                    }
                    Ok(saved)
                }
            })
            .await?;
        if is_provider_account {
            self.notify.notify_one();
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
        let previous = self.contact_get(&id).await?;
        let account = self.load_account(&previous.account_id).await?;
        let is_provider_account = self.provider_name(&account).is_some();
        let account_id = account.id.clone();
        let saved = self
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
                    // The optimistic row and the upload that owns it must land
                    // together; otherwise an older delta can overwrite the
                    // edit before the operation exists.
                    if is_provider_account {
                        let op_id = new_id();
                        let op = ContactOp::Save {
                            contact_id: saved.id.clone(),
                            assigned_uid: None,
                            snapshot,
                        };
                        enqueue_op_with_id_sync(
                            conn,
                            &op_id,
                            &account_id,
                            &op,
                            format!("更新联系人「{}」", saved.label()),
                        )?;
                        // This save is now the newest identity writer: an older
                        // operation's receipt that lands later must not replace
                        // the identity this one confirms.
                        crate::store::bump_contact_identity(conn, &saved.id, Some(op_id.as_str()))?;
                    }
                    Ok(saved)
                }
            })
            .await?;
        if is_provider_account {
            self.notify.notify_one();
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
        // Provider capability is account configuration, not contact state, so
        // it is safe to read before the write lock. The contact snapshots
        // themselves are re-read inside the delete transaction: a create
        // receipt may land in between, and the delete must then address the
        // remote object instead of treating the contact as never uploaded.
        let accounts = self.ctx.db.read(repo::accounts::list).await?;
        let provider_accounts: HashMap<String, bool> = accounts
            .iter()
            .map(|account| (account.id.clone(), self.provider_name(account).is_some()))
            .collect();
        // Test-only rendezvous: lets the interleaving test land a create
        // receipt after the caller-side reads but before this transaction, so
        // only an in-transaction snapshot can see the remote id.
        #[cfg(test)]
        {
            for id in &ids {
                crate::test_support::wait_for_write(id).await;
            }
        }
        let (books, notify_accounts) = self
            .ctx
            .db
            .write(move |conn| {
                let mut snapshots = Vec::new();
                let mut books = Vec::new();
                for id in &ids {
                    if let Some(snapshot) = snapshot_contact(conn, id)? {
                        if !books.contains(&snapshot.contact.address_book_id) {
                            books.push(snapshot.contact.address_book_id.clone());
                        }
                        snapshots.push(snapshot);
                    }
                }
                if snapshots.is_empty() {
                    return Ok((books, Vec::new()));
                }
                let delete_ids: Vec<String> = snapshots
                    .iter()
                    .map(|snapshot| snapshot.contact.id.clone())
                    .collect();
                // Group the remote deletions by account: a bulk delete may
                // span books (and accounts).
                let mut by_account: HashMap<String, Vec<ContactSnapshot>> = HashMap::new();
                for snapshot in snapshots {
                    let account_id = snapshot.contact.account_id.clone();
                    if !provider_accounts.get(&account_id).copied().unwrap_or(false) {
                        continue;
                    }
                    if snapshot.remote_id.is_none() && snapshot.href.is_none() {
                        // A create may still be uploading and has no remote
                        // identity in this transaction. Only an operation that
                        // could already have reached the provider needs a
                        // cleanup intent. A queued create without a persisted
                        // send intent provably never left the client: cancel
                        // it instead, or a delete that raced it would keep an
                        // `unknown` intent that blocks the whole book's delta.
                        match reconcile_outstanding_creates(
                            conn,
                            &account_id,
                            &snapshot.contact.id,
                        )? {
                            OutstandingCreate::PossiblySent {
                                owner_op_id,
                                owner_attempt,
                            } => {
                                // Bind the cleanup to the operation that may
                                // have produced the object. Another operation
                                // whose local row vanished (a newer save that
                                // never sent) must not be able to delete it.
                                crate::store::record_contact_delete_intent_owned(
                                    conn,
                                    &snapshot.contact.id,
                                    &account_id,
                                    &snapshot.contact.address_book_id,
                                    Some(&owner_op_id),
                                    Some(owner_attempt),
                                )?;
                            }
                            OutstandingCreate::Unsent | OutstandingCreate::None => {}
                        }
                        continue;
                    }
                    by_account.entry(account_id).or_default().push(snapshot);
                }
                contacts_repo::delete_many(conn, &delete_ids)?;
                for (account_id, owned) in &by_account {
                    let op = ContactOp::Delete {
                        contacts: owned.clone(),
                    };
                    enqueue_op_sync(conn, account_id, &op, "删除联系人".to_string())?;
                }
                Ok((books, by_account.into_keys().collect()))
            })
            .await?;
        for _ in notify_accounts {
            self.notify.notify_one();
        }
        emit_contacts_changed(self.ctx.events.as_ref(), books);
        Ok(())
    }

    pub async fn contact_set_favorite(&self, id: &str, value: bool) -> Result<()> {
        let id = id.to_string();
        let existing = self.contact_get(&id).await?;
        let account = self.load_account(&existing.account_id).await?;
        let enqueue = backend_for(&account, DavService::Contacts) == Backend::Google;
        let account_id = account.id.clone();
        let contact = self
            .ctx
            .db
            .write(move |conn| {
                let contact = contacts_repo::get_required(conn, &id)?;
                let previous = contact.is_favorite;
                contacts_repo::set_favorite(conn, &id, value)?;
                // Keep the flag and its queued upload atomic for the same
                // reason as the full contact update.
                if enqueue {
                    let op = ContactOp::Favorite {
                        contact_id: contact.id.clone(),
                        value,
                        previous,
                    };
                    enqueue_op_sync(
                        conn,
                        &account_id,
                        &op,
                        format!("更新联系人「{}」的收藏状态", contact.label()),
                    )?;
                }
                Ok(contact)
            })
            .await?;
        if enqueue {
            self.notify.notify_one();
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
        let (group, previous, current) = self
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
                    // Re-read instead of trusting the input: ids from another
                    // address book are filtered out, and the remote diff must
                    // not mention them.
                    let current_members: Vec<String> = contacts_repo::members(conn, &group_id)?
                        .into_iter()
                        .map(|contact| contact.id)
                        .collect();
                    Ok((group, previous_members, current_members))
                }
            })
            .await?;
        let current: HashSet<String> = current.into_iter().collect();
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
                    assigned_uid: None,
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
        // A save enqueued outside `contact_create`/`contact_update` (an import
        // or a group-membership refresh) is still the newest identity writer:
        // record it so a late receipt from an older save cannot overwrite the
        // identity this operation confirms.
        let save_contact_id = match op {
            ContactOp::Save { contact_id, .. } => Some(contact_id.clone()),
            _ => None,
        };
        self.ctx
            .db
            .write(move |conn| {
                let queued = repo::pending_ops::enqueue(conn, new_op)?;
                if let Some(contact_id) = save_contact_id {
                    // The contact may have been deleted between the caller's
                    // snapshot and this write; there is then no identity to
                    // own and the save's executor will skip it.
                    if contacts_repo::get(conn, &contact_id)?.is_some() {
                        crate::store::bump_contact_identity(
                            conn,
                            &contact_id,
                            Some(queued.id.as_str()),
                        )?;
                    }
                }
                Ok(())
            })
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
            assigned_uid: None,
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

    /// Clean up a remote object left behind by a create whose local row was
    /// deleted before its receipt landed.
    #[cfg(test)]
    async fn settle_deleted_contact_create(
        &self,
        account: &Account,
        provider: &dyn ContactProvider,
        contact_id: &str,
    ) -> Result<()> {
        let id = contact_id.to_string();
        let intent = self
            .ctx
            .db
            .read(move |conn| crate::store::contact_delete_intent(conn, &id))
            .await?;
        let Some(intent) = intent else {
            return Ok(());
        };
        self.settle_contact_delete_intent(account, provider, intent)
            .await
    }

    /// Clean up the intent of an operation that provably never sent anything.
    ///
    /// The intent is only touched when this operation still owns the claim and
    /// the claim is current. An intent recorded for another operation (a
    /// concurrently deleted create that may already be on the server) is left
    /// alone: deleting it here would orphan that object and let the next delta
    /// re-import the contact.
    async fn clean_up_intent_for_unsent_op(
        &self,
        account: &Account,
        provider: &dyn ContactProvider,
        contact_id: &str,
        op: &PendingOp,
    ) -> Result<()> {
        let id = contact_id.to_string();
        let op_id = op.id.clone();
        let owned = self
            .ctx
            .db
            .write(move |conn| {
                let stored = repo::pending_ops::get_required(conn, &op_id)?;
                if stored.state != PendingOpState::InProgress {
                    return Ok(None);
                }
                let Some(intent) = crate::store::contact_delete_intent(conn, &id)? else {
                    return Ok(None);
                };
                if intent
                    .owner_op_id
                    .as_deref()
                    .is_some_and(|owner| owner != stored.id.as_str())
                {
                    // Another operation may already have reached the provider;
                    // only that operation may settle or remove its intent.
                    warn!(
                        contact_id = %id,
                        intent_owner = ?intent.owner_op_id,
                        op = %stored.id,
                        "leaving a contact delete intent owned by another operation"
                    );
                    return Ok(None);
                }
                let has_pending_receipt = crate::store::contact_delete_receipts(conn, &id)?
                    .iter()
                    .any(|receipt| {
                        receipt.state == crate::store::ContactDeleteReceiptState::Pending
                    });
                if intent.remote_id.is_none() && intent.href.is_none() && !has_pending_receipt {
                    // This unsent operation cannot have produced an object, so
                    // an unaddressable intent it owns is pure overhead. A
                    // pending receipt would still name an addressable object,
                    // so it must keep the intent alive.
                    crate::store::remove_contact_delete_intent_if_owned(
                        conn,
                        &id,
                        &stored.id,
                        stored.attempts,
                    )?;
                    return Ok(None);
                }
                // A legacy (owner-less) intent may still name an object that
                // another operation created before receipts were tracked.
                Ok(Some(intent))
            })
            .await?;
        if let Some(intent) = owned {
            self.settle_contact_delete_intent(account, provider, intent)
                .await?;
        }
        Ok(())
    }

    /// Settle the intent owned by `op`, refusing to act on another operation's
    /// intent.
    async fn settle_deleted_contact_create_for_op(
        &self,
        account: &Account,
        provider: &dyn ContactProvider,
        contact_id: &str,
        op: &PendingOp,
    ) -> Result<()> {
        let id = contact_id.to_string();
        let op_id = op.id.clone();
        let (proceed, intent) = self
            .ctx
            .db
            .write(move |conn| {
                let stored = repo::pending_ops::get_required(conn, &op_id)?;
                if stored.state != PendingOpState::InProgress {
                    return Ok((false, None));
                }
                let Some(intent) = crate::store::contact_delete_intent(conn, &id)? else {
                    return Ok((true, None));
                };
                if intent
                    .owner_op_id
                    .as_deref()
                    .is_some_and(|owner| owner != stored.id.as_str())
                {
                    warn!(
                        contact_id = %id,
                        intent_owner = ?intent.owner_op_id,
                        op = %stored.id,
                        "not settling a contact delete intent owned by another operation"
                    );
                    // The intent belongs to another operation, but this one
                    // may still have recorded its own receipt. Cleanup of
                    // those receipts must not be skipped.
                    return Ok((true, None));
                }
                Ok((true, Some(intent)))
            })
            .await?;
        if let Some(intent) = intent {
            self.settle_contact_delete_intent(account, provider, intent)
                .await?;
        } else if proceed {
            self.settle_contact_delete_receipts(account, provider, contact_id)
                .await?;
        }
        Ok(())
    }

    /// Remove the provider object named by a pending delete intent.
    ///
    /// The intent is only settled (and turned into a short-lived suppression
    /// tombstone) after the delete succeeds, so a retryable failure is retried
    /// by the next execution of the operation. CardDAV can address the object
    /// by the contact id it used as UID even when no receipt was ever
    /// persisted.
    async fn settle_contact_delete_intent(
        &self,
        account: &Account,
        provider: &dyn ContactProvider,
        intent: crate::store::ContactDeleteIntent,
    ) -> Result<()> {
        // A settled intent already had its remote object removed. Retrying the
        // operation (e.g. after `mark_done` failed) must not delete again, but
        // a per-operation receipt recorded by a later execution still has to
        // be cleaned.
        if intent.state == crate::store::ContactDeleteIntentState::Settled {
            self.settle_contact_delete_receipts(account, provider, &intent.contact_id)
                .await?;
            return Ok(());
        }
        let Some(mapping) = self.mapping_for(&intent.address_book_id).await? else {
            // The book no longer has a provider mapping, so no remote object
            // can be addressed; drop the intent instead of retrying forever.
            self.remove_contact_delete_intent(&intent.contact_id)
                .await?;
            return Ok(());
        };
        // Every operation that reached the provider may have produced its own
        // object. Remove those first; the intent's own identity is handled
        // below.
        let receipts = self
            .ctx
            .db
            .read({
                let id = intent.contact_id.clone();
                move |conn| crate::store::contact_delete_receipts(conn, &id)
            })
            .await?;
        self.settle_receipts_for_mapping(account, provider, &mapping, receipts)
            .await?;
        let remote_id = intent
            .remote_id
            .clone()
            .or_else(|| (provider.name() == "carddav").then(|| intent.contact_id.clone()));
        if remote_id.is_none() && intent.href.is_none() {
            // The POST may have committed while its response was lost, so an
            // object can exist even though we never learned how to address it.
            // Never treat this as a completed cleanup: keep the intent (state
            // `unknown`) so a delta still suppresses the book, and fail this
            // attempt so the worker retries and the failure is visible for
            // reconciliation.
            let id = intent.contact_id.clone();
            self.ctx
                .db
                .write(move |conn| crate::store::mark_contact_delete_intent_unknown(conn, &id))
                .await?;
            return Err(AppError::conflict(
                "contact delete intent has no remote identity; the create receipt was lost",
            ));
        }
        let book = self.book_from_mapping(&mapping);
        // The fallback id (`intent.contact_id`) used to address a CardDAV
        // object is a real UID, never an href-derived placeholder.
        let uid_is_derived = intent.remote_id.is_some() && intent.uid_is_derived;
        let current = RemoteContactRef {
            remote_id: remote_id.clone(),
            href: intent.href.clone(),
            etag: intent.etag.clone(),
            raw_vcard: None,
            uid_is_derived,
        };
        provider.delete_contact(account, &book, &current).await?;
        // Keep a tombstone instead of dropping the row: a delta request that
        // started before this DELETE can still name the object, and importing
        // it would resurrect the contact the user deleted. The tombstone
        // expires after one sync window.
        let id = intent.contact_id.clone();
        let href = intent.href.clone();
        let suppress_until = Utc::now()
            + chrono::Duration::seconds(crate::store::CONTACT_DELETE_TOMBSTONE_TTL_SECONDS);
        self.ctx
            .db
            .write(move |conn| {
                crate::store::settle_contact_delete_intent(
                    conn,
                    &id,
                    remote_id.as_deref(),
                    href.as_deref(),
                    suppress_until,
                    uid_is_derived,
                )
            })
            .await
    }

    async fn remove_contact_delete_intent(&self, contact_id: &str) -> Result<()> {
        let id = contact_id.to_string();
        self.ctx
            .db
            .write(move |conn| crate::store::remove_contact_delete_intent(conn, &id))
            .await
    }

    /// Persist the UID assigned to a derived-identity card before the first
    /// wire attempt.
    ///
    /// The value lives on the operation payload (not only in memory) so a
    /// retry of the *same* operation reuses it. Rewriting it per attempt would
    /// change the vCard `UID` after an ambiguous failure and could leave the
    /// server with one identity while the client believes in another.
    async fn persist_assigned_uid(&self, op: &PendingOp, uid: &str) -> Result<()> {
        let mut payload = ops::decode(op)?;
        match &mut payload {
            ContactOp::Save { assigned_uid, .. } => *assigned_uid = Some(uid.to_string()),
            _ => return Ok(()),
        }
        let value = ops::to_value(&payload)?;
        let id = op.id.clone();
        self.ctx
            .db
            .write(move |conn| repo::pending_ops::update_payload(conn, &id, &value))
            .await?;
        Ok(())
    }

    /// Persist (before the provider call) that this operation is about to
    /// issue a wire command.
    ///
    /// A no-op update means the operation was cancelled concurrently (a delete
    /// racing the create), so the caller must not send anything.
    async fn mark_send_intent(&self, op_id: &str) -> Result<()> {
        let id = op_id.to_string();
        let changed = self
            .ctx
            .db
            .write(move |conn| repo::pending_ops::mark_send_intent(conn, &id))
            .await?;
        if changed == 0 {
            return Err(AppError::conflict(
                "contact operation was cancelled before it was sent",
            ));
        }
        Ok(())
    }

    /// Persist a create receipt returned by the provider.
    ///
    /// When the local row still exists the receipt is written to it. When the
    /// user deleted the contact while the POST was in flight, the receipt is
    /// kept on the delete intent. A later Save can upload its own object while
    /// an earlier operation owns the intent (its receipt was lost); that
    /// operation's identity is recorded per operation instead of being
    /// discarded.
    async fn bind_create_receipt(
        &self,
        contact_id: &str,
        op_id: &str,
        receipt: &CreateReceiptIdentity,
    ) -> Result<CreateReceiptOutcome> {
        let id = contact_id.to_string();
        let op_id = op_id.to_string();
        let receipt = receipt.clone();
        self.ctx
            .db
            .write(move |conn| {
                // Only the operation that still owns the contact's identity may
                // write it. A newer save claimed the row when it enqueued its
                // operation (or confirmed it with its own receipt), so a late
                // receipt from an older save must not replace the `remote_id`
                // and identity kind the newer writer confirmed.
                let existing_remote_id: Option<Option<String>> = conn
                    .query_row(
                        "SELECT remote_id FROM contact WHERE id = ?1",
                        rusqlite::params![id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(existing_remote_id) = existing_remote_id {
                    let writer = crate::store::contact_identity_writer(conn, &id)?;
                    // This operation owns the identity. A different operation
                    // that is still queued or on the wire has not confirmed
                    // anything yet, so this receipt is the newest confirmation
                    // and a later failure of that operation must keep it.
                    //
                    // With no usable evidence (a row written before the state
                    // table existed, or a delta upsert) the conservative rule
                    // is: never overwrite an already confirmed remote id.
                    let may_write = match writer.as_deref() {
                        Some(owner) if owner == op_id => true,
                        Some("") => false,
                        Some(owner) => repo::pending_ops::get(conn, owner)?
                            .is_some_and(|op| repo::pending_ops::is_outstanding(&op)),
                        None => existing_remote_id.is_none(),
                    };
                    if !may_write {
                        warn!(
                            contact = %id,
                            op = %op_id,
                            ?writer,
                            "dropping a late create receipt; a newer writer already confirmed \
                             the contact identity"
                        );
                        return Ok(CreateReceiptOutcome::RowPresent);
                    }
                    conn.execute(
                        "UPDATE contact SET remote_id = COALESCE(?2, remote_id),
                                            etag = COALESCE(?3, etag),
                                            href = COALESCE(?4, href),
                                            updated_at = ?5
                         WHERE id = ?1",
                        rusqlite::params![
                            id,
                            receipt.remote_id,
                            receipt.etag,
                            receipt.href,
                            repo::now_string()
                        ],
                    )?;
                    // The row still exists: its remote id is now the provider's
                    // identity. Remember whether it is a real UID or an
                    // href-derived placeholder, and that this operation owns it
                    // so an older receipt arriving later is rejected.
                    if receipt.remote_id.is_some() {
                        set_contact_uid_is_derived(conn, &id, receipt.uid_is_derived)?;
                        // Do not steal the row from a newer operation that is
                        // still queued: it has not confirmed anything yet, and
                        // its own receipt must remain able to write.
                        crate::store::record_contact_identity_receipt_writer(
                            conn,
                            &id,
                            op_id.as_str(),
                        )?;
                    }
                    return Ok(CreateReceiptOutcome::RowPresent);
                }
                let Some(intent) = crate::store::contact_delete_intent(conn, &id)? else {
                    // The row is gone but no cleanup is owed (the delete had
                    // nothing to remove). Nothing to record.
                    return Ok(CreateReceiptOutcome::RowGone { owned_intent: None });
                };
                let owned = intent
                    .owner_op_id
                    .as_deref()
                    .is_none_or(|owner| owner == op_id);
                if owned {
                    let owned_intent = crate::store::attach_contact_delete_intent(
                        conn,
                        &id,
                        receipt.remote_id.as_deref(),
                        receipt.href.as_deref(),
                        receipt.etag.as_deref(),
                        receipt.uid_is_derived,
                    )?
                    .map(Box::new);
                    Ok(CreateReceiptOutcome::RowGone { owned_intent })
                } else {
                    if receipt.remote_id.is_some() || receipt.href.is_some() {
                        // An earlier operation owns the intent, but this
                        // operation did reach the provider and produced its own
                        // addressable object. Keep the accurate identity so the
                        // object can be deleted instead of orphaned.
                        crate::store::record_contact_delete_receipt(
                            conn,
                            &id,
                            &op_id,
                            receipt.remote_id.as_deref(),
                            receipt.href.as_deref(),
                            receipt.etag.as_deref(),
                            receipt.uid_is_derived,
                        )?;
                    }
                    Ok(CreateReceiptOutcome::RowGone { owned_intent: None })
                }
            })
            .await
    }

    /// Delete every provider object named by a per-operation create receipt
    /// of this contact.
    ///
    /// Receipts belong to operations that do not own the delete intent: an
    /// earlier create lost its receipt while a later Save uploaded its own
    /// object. Cleanup is independent of intent ownership.
    async fn settle_contact_delete_receipts(
        &self,
        account: &Account,
        provider: &dyn ContactProvider,
        contact_id: &str,
    ) -> Result<()> {
        let id = contact_id.to_string();
        let (intent, receipts) = self
            .ctx
            .db
            .read(move |conn| {
                let intent = crate::store::contact_delete_intent(conn, &id)?;
                let receipts = crate::store::contact_delete_receipts(conn, &id)?;
                Ok((intent, receipts))
            })
            .await?;
        let Some(intent) = intent else {
            return Ok(());
        };
        if !receipts
            .iter()
            .any(|receipt| receipt.state == crate::store::ContactDeleteReceiptState::Pending)
        {
            return Ok(());
        }
        let Some(mapping) = self.mapping_for(&intent.address_book_id).await? else {
            // No provider mapping means no object can be addressed; drop the
            // intent (receipts cascade) instead of retrying forever.
            self.remove_contact_delete_intent(contact_id).await?;
            return Ok(());
        };
        self.settle_receipts_for_mapping(account, provider, &mapping, receipts)
            .await
    }

    async fn settle_receipts_for_mapping(
        &self,
        account: &Account,
        provider: &dyn ContactProvider,
        mapping: &crate::store::AddressBookRemote,
        receipts: Vec<crate::store::ContactDeleteReceipt>,
    ) -> Result<()> {
        let book = self.book_from_mapping(mapping);
        for receipt in receipts {
            if receipt.state != crate::store::ContactDeleteReceiptState::Pending {
                continue;
            }
            if receipt.remote_id.is_none() && receipt.href.is_none() {
                continue;
            }
            let current = RemoteContactRef {
                remote_id: receipt.remote_id.clone(),
                href: receipt.href.clone(),
                etag: receipt.etag.clone(),
                raw_vcard: None,
                uid_is_derived: receipt.uid_is_derived,
            };
            provider.delete_contact(account, &book, &current).await?;
            // Keep the identity as a short-lived tombstone: a delta fetched
            // before this DELETE can still name the object.
            let contact_id = receipt.contact_id.clone();
            let op_id = receipt.op_id.clone();
            let suppress_until = Utc::now()
                + chrono::Duration::seconds(crate::store::CONTACT_DELETE_TOMBSTONE_TTL_SECONDS);
            self.ctx
                .db
                .write(move |conn| {
                    crate::store::settle_contact_delete_receipt(
                        conn,
                        &contact_id,
                        &op_id,
                        suppress_until,
                    )
                })
                .await?;
        }
        Ok(())
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

/// Remote identity returned by a create/update POST.
#[derive(Debug, Clone, Default)]
struct CreateReceiptIdentity {
    remote_id: Option<String>,
    href: Option<String>,
    etag: Option<String>,
    /// The provider returned a href-derived placeholder instead of a real UID.
    uid_is_derived: bool,
}

/// Where a create receipt was written.
enum CreateReceiptOutcome {
    /// The local row still exists; the receipt was written to it.
    RowPresent,
    /// The local row was deleted while the upload was in flight.
    RowGone {
        /// Set when this operation owns the delete intent (or the intent is a
        /// legacy owner-less row). `None` means the receipt (if addressable)
        /// was recorded per operation instead.
        owned_intent: Option<Box<crate::store::ContactDeleteIntent>>,
    },
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
        // Re-read the row before doing anything: the caller may hold an older
        // snapshot, and a delete racing the create may have cancelled it in
        // between. A cancelled/settled operation is never executed and must
        // not touch a delete intent.
        let current = {
            let id = op.id.clone();
            self.ctx
                .db
                .read(move |conn| repo::pending_ops::get_required(conn, &id))
                .await?
        };
        if current.state != PendingOpState::InProgress {
            debug!(
                op = %current.id,
                state = current.state.as_str(),
                "not executing a contact operation that is no longer claimed"
            );
            return Ok(());
        }
        let op = &current;
        let decoded = ops::decode(op)?;
        let account = self.load_account(&op.account_id).await?;
        let Some(provider) = self.provider_for(&account) else {
            return Ok(());
        };
        match &decoded {
            ContactOp::Save {
                contact_id,
                assigned_uid: sticky_uid,
                snapshot: previous_snapshot,
            } => {
                let snapshot = self.snapshot_contact(contact_id).await?;
                let Some(snapshot) = snapshot else {
                    // The local row vanished while this create was queued or
                    // uploading (the user deleted it). A persisted intent says
                    // whether a remote object still has to be removed; this
                    // invocation must not create a new one.
                    //
                    // An operation that never persisted a send intent (in any
                    // attempt) and whose outcome is not ambiguous cannot have
                    // produced a server object. A concurrent delete may have
                    // recorded an intent only because it saw this claim as
                    // `inProgress`; drop that unaddressable intent instead of
                    // turning it into a permanent `unknown` tombstone.
                    if !op.send_intent && !op.outcome_unknown {
                        self.clean_up_intent_for_unsent_op(
                            &account,
                            provider.as_ref(),
                            contact_id,
                            op,
                        )
                        .await?;
                        return Ok(());
                    }
                    self.settle_deleted_contact_create_for_op(
                        &account,
                        provider.as_ref(),
                        contact_id,
                        op,
                    )
                    .await?;
                    return Ok(());
                };
                let contact = snapshot.contact.clone();
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
                    uid_is_derived: snapshot.uid_is_derived,
                };
                let is_update = current.remote_id.is_some() || current.href.is_some();
                // A card whose local identity was derived from its href has no
                // real UID yet. Writing the href-derived placeholder back as a
                // UID collides with a genuine UID of the same spelling, so the
                // first rewrite assigns a fresh UID. The value is persisted on
                // the operation payload *before* the wire command: a retry
                // after an ambiguous failure must reuse it instead of writing
                // a different UID on every attempt.
                let assigned_uid = if is_update && snapshot.uid_is_derived {
                    let uid = match sticky_uid {
                        Some(uid) if !uid.trim().is_empty() => uid.clone(),
                        _ => {
                            let uid = nuncio_core::models::new_id();
                            self.persist_assigned_uid(op, &uid).await?;
                            uid
                        }
                    };
                    Some(uid)
                } else {
                    None
                };
                // Persist the send intent before the wire command. A delete
                // racing this create may only persist a cleanup intent (and
                // therefore block the delta) when it can see that the create
                // might already have reached the provider.
                self.mark_send_intent(&op.id).await?;
                let result = if is_update {
                    provider
                        .update_contact(
                            &account,
                            &book,
                            &current,
                            &contact,
                            &group_names,
                            assigned_uid.as_deref(),
                        )
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
                let receipt = CreateReceiptIdentity {
                    remote_id: result.remote_id.clone(),
                    href: result.href.clone(),
                    etag: result.etag.clone(),
                    uid_is_derived: result.uid_is_derived,
                };
                match self
                    .bind_create_receipt(&contact.id, &op.id, &receipt)
                    .await?
                {
                    CreateReceiptOutcome::RowPresent => {}
                    CreateReceiptOutcome::RowGone {
                        owned_intent: Some(intent),
                    } => {
                        self.settle_contact_delete_intent(&account, provider.as_ref(), *intent)
                            .await?;
                    }
                    CreateReceiptOutcome::RowGone { owned_intent: None } => {
                        // This operation's receipt (if it produced an
                        // addressable object) was recorded per operation, even
                        // when an earlier create owns the intent. Remove the
                        // object now instead of leaving it on the server.
                        self.settle_contact_delete_receipts(
                            &account,
                            provider.as_ref(),
                            &contact.id,
                        )
                        .await?;
                    }
                }
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
                        uid_is_derived: snapshot.uid_is_derived,
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
        let op_row = op.clone();
        let restore = self
            .ctx
            .db
            .write(move |conn| rollback_conn(conn, &op_row))
            .await;
        if restore.is_ok() {
            // An empty list means "anything may have changed".
            emit_contacts_changed(self.ctx.events.as_ref(), Vec::new());
        }
        restore
    }

    fn supports_atomic_rollback(&self) -> bool {
        true
    }

    fn rollback_in_tx(&self, conn: &rusqlite::Connection, op: &PendingOp) -> Result<()> {
        rollback_conn(conn, op)
    }
}

/// Undo a permanently failed contact operation on an existing transaction.
///
/// The shared worker calls this in the same transaction as `mark_failed`;
/// otherwise a delta could see "operation gone, optimistic row still present"
/// between the two writes, delete the row, and the snapshot restore would
/// immediately re-insert it.
fn rollback_conn(conn: &rusqlite::Connection, op: &PendingOp) -> Result<()> {
    let decoded = ops::decode(op)?;
    // Same reason as the task executor: the rollback rewrites the very rows
    // the contact list is rendering, and the failure toast alone leaves the
    // UI showing data that no longer exists in the database.
    match decoded {
        ContactOp::Save {
            contact_id,
            snapshot,
            ..
        } => match snapshot {
            Some(snapshot) => {
                // A newer local edit must win over an older snapshot;
                // restoring it would silently undo the user's latest change.
                if !contacts_unchanged_since(conn, op, std::slice::from_ref(&contact_id))? {
                    warn!(
                        op = %op.id,
                        contact_id = %contact_id,
                        "keeping local contact: it was edited after the failed save was queued"
                    );
                    return Ok(());
                }
                // The save never removes its own row. If it is gone now, a
                // delta deleted it while the operation was queued; re-inserting
                // the snapshot would resurrect data the server removed.
                if contacts_repo::get(conn, &contact_id)?.is_none() {
                    warn!(
                        op = %op.id,
                        contact_id = %contact_id,
                        "not restoring contact: the row no longer exists"
                    );
                    return Ok(());
                }
                restore_contact_content(conn, &snapshot)
            }
            None => {
                if !contacts_unchanged_since(conn, op, std::slice::from_ref(&contact_id))? {
                    warn!(
                        op = %op.id,
                        contact_id = %contact_id,
                        "keeping local contact: it was edited after the failed create was queued"
                    );
                    return Ok(());
                }
                let _ = contacts_repo::delete_many(conn, &[contact_id]);
                Ok(())
            }
        },
        ContactOp::Delete { contacts } => {
            for snapshot in &contacts {
                if !contacts_unchanged_since(conn, op, std::slice::from_ref(&snapshot.contact.id))?
                {
                    warn!(
                        op = %op.id,
                        contact_id = %snapshot.contact.id,
                        "keeping local contact: it changed after the failed delete was queued"
                    );
                    continue;
                }
                // Restoring a failed delete is the point: the row is expected
                // to be absent locally.
                restore_contact(conn, snapshot)?;
            }
            Ok(())
        }
        ContactOp::Favorite {
            contact_id,
            previous,
            ..
        } => {
            if !contacts_unchanged_since(conn, op, std::slice::from_ref(&contact_id))? {
                warn!(
                    op = %op.id,
                    contact_id = %contact_id,
                    "keeping local contact: it changed after the failed favourite was queued"
                );
                return Ok(());
            }
            if contacts_repo::get(conn, &contact_id)?.is_none() {
                warn!(
                    op = %op.id,
                    contact_id = %contact_id,
                    "not restoring favourite: the row no longer exists"
                );
                return Ok(());
            }
            contacts_repo::set_favorite(conn, &contact_id, previous)
        }
        ContactOp::GroupCreate { group_id } => {
            let _ = contacts_repo::delete_group(conn, &group_id);
            Ok(())
        }
        ContactOp::GroupRename {
            group_id,
            previous_name,
        } => {
            let _ = contacts_repo::rename_group(conn, &group_id, &previous_name);
            Ok(())
        }
        ContactOp::GroupDelete { snapshot } => {
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
                    rusqlite::params![snapshot.group.id, contact_id, repo::now_string()],
                )?;
            }
            Ok(())
        }
        ContactOp::GroupMembers {
            group_id,
            added,
            removed,
        } => {
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
        }
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

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

/// ETag baseline for the CardDAV ctag/ETag fallback.
///
/// The baseline must describe every object we believe the server holds — not
/// only the rows currently visible in `contact`. Objects hidden by book-level
/// delete isolation live in `pim_contact_quarantine`, but the server still
/// lists them. Leaving their href out of the diff means an authoritative
/// listing that no longer contains the object yields no `deleted_hrefs`, so
/// the cursor advances and the quarantine replay later resurrects a contact
/// the server already deleted. Include the quarantined hrefs so absence is
/// observable. A quarantine snapshot may carry no ETag; the empty sentinel
/// still makes absence produce a deletion and makes a listed object re-fetched.
fn local_etags(conn: &rusqlite::Connection, book_id: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    // The visible row is the live copy, so it wins if an href is somehow
    // present in both places.
    let mut seen: HashSet<String> = HashSet::new();
    {
        let mut stmt = conn.prepare(
            "SELECT href, etag FROM contact
             WHERE address_book_id = ?1 AND href IS NOT NULL AND etag IS NOT NULL",
        )?;
        let mut rows = stmt.query(rusqlite::params![book_id])?;
        while let Some(row) = rows.next()? {
            let href: String = row.get(0)?;
            let etag: String = row.get(1)?;
            // Deduplicate equivalent spellings of one resource; the emitted
            // baseline keeps the raw href the database holds.
            if seen.insert(href_key(&href)) {
                out.push((href, etag));
            }
        }
    }
    let mut stmt = conn.prepare(
        "SELECT href, payload FROM pim_contact_quarantine
         WHERE address_book_id = ?1 AND href IS NOT NULL AND href <> ''",
    )?;
    let mut rows = stmt.query(rusqlite::params![book_id])?;
    while let Some(row) = rows.next()? {
        let href: String = row.get(0)?;
        if !seen.insert(href_key(&href)) {
            continue;
        }
        let payload: String = row.get(1)?;
        let record: QuarantinedRemoteContact = serde_json::from_str(&payload).map_err(|error| {
            AppError::internal(format!("invalid quarantined contact payload: {error}"))
        })?;
        out.push((href, record.etag.unwrap_or_default()));
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

/// Local contacts owned by a write that has not reached the provider.
///
/// `local_ids` covers rows that still exist, `remote_ids`/`hrefs` cover rows a
/// queued delete removed locally: a delta must not resurrect them either.
#[derive(Default)]
struct PendingContactWrites {
    local_ids: HashSet<String>,
    /// Remote identities owned by outstanding (or recently settled) local
    /// writes, keyed by address book. The book scope matters for CardDAV: its
    /// remote id is the vCard UID, which is only unique inside one book.
    remote_ids: HashMap<String, HashSet<String>>,
    hrefs: HashMap<String, HashSet<String>>,
    /// Address books that contain a create whose local row has no remote
    /// identity yet. A delta cannot attribute a newly listed server object to
    /// that row, so importing it would create a second contact sharing one
    /// remote identity once the upload receipt is written. Such books delay
    /// the import (cursor held) until the create is attributed. This is a
    /// duplicate-prevention rule of an *outstanding local write*, not a
    /// delete-intent isolation: it resolves as soon as the receipt binds.
    unattributed_create_books: HashSet<String>,
    /// Address books isolated by a persisted delete intent that has no
    /// remote identity at all, keyed by address book. A new server object
    /// cannot be attributed to (or ruled out as) the possibly-created object,
    /// so objects not matched by a precise tombstone stay hidden. This is
    /// book-level **isolation, not a cursor barrier**: the batch is still
    /// committed, because replaying it cannot resolve the intent (release is
    /// wall-clock based).
    book_blocked_deletes: HashSet<String>,
    /// Remote objects named by *outstanding* deletes, keyed by address book.
    /// The local row is gone, so a full response cannot list it; its absence
    /// must still hold the cursor until the delete settles.
    deleted_remote_ids: HashMap<String, HashSet<String>>,
    deleted_hrefs: HashMap<String, HashSet<String>>,
    /// Remote identities named by persisted delete intents, keyed by address
    /// book. A listed contact carrying one of these must never be imported
    /// back, but it must not hold the cursor: the tombstone already records
    /// that the object was deleted (or that cleanup is terminal), so replaying
    /// the whole batch forever cannot help. Keying by book is essential: a
    /// CardDAV UID is only unique per book, so a tombstone in book A must not
    /// hide a different contact with the same UID in book B.
    ///
    /// Both fields are kept together so matching can be exact. A tombstone
    /// whose UID differs from the incoming record must not suppress it even
    /// when the old href is reused: a fresh vCard can be created at the same
    /// resource URL with a new UID.
    suppressed: HashMap<String, Vec<SuppressedIdentity>>,
}

/// One persisted tombstone identity. Either field may be missing (an intent
/// settled without a receipt stores only what it knows).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SuppressedIdentity {
    /// Intent this identity belongs to. For a per-operation receipt this is
    /// the owning intent's contact id.
    contact_id: String,
    /// Set when the identity came from a per-operation receipt. Its ambiguous
    /// windows are counted and released independently of its parent intent.
    op_id: Option<String>,
    remote_id: Option<String>,
    /// `remote_id` was derived from the href (UID-less vCard) rather than a
    /// real provider UID. Derived and real identities never match each other.
    uid_is_derived: bool,
    href: Option<String>,
    /// A bounded ambiguity release already fired for this identity. A precise
    /// match still hides the object, but a conflicting href no longer holds
    /// the cursor.
    ambiguity_released: bool,
}

/// Where an ambiguous tombstone match came from, so the correct counter can be
/// advanced. Receipts must be released even after their parent intent was.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum AmbiguousTombstone {
    Intent(String),
    Receipt { contact_id: String, op_id: String },
}

/// Outcome of matching an incoming remote object against delete tombstones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Suppression {
    /// No tombstone describes this object.
    #[default]
    None,
    /// The object is the deleted one; hide it and let the cursor advance.
    Suppress,
    /// One identity field matches but the other conflicts or cannot be
    /// verified. Do not import now and do not advance the cursor: the batch
    /// must be replayed once the tombstone expires or the conflict resolves.
    Ambiguous,
}

/// Full result of evaluating every tombstone of a book against one object.
#[derive(Debug, Default)]
struct SuppressionMatch {
    outcome: Suppression,
    /// The tombstone identity that produced a definite `Suppress`. Cleanup
    /// must use *this* identity, never the generic upsert lookup: an href may
    /// have been reused by a different vCard with another UID.
    matched: Option<SuppressedIdentity>,
    /// Identities that matched ambiguously. Their bounded counters are
    /// advanced independently (intents and receipts separately).
    ambiguous: HashSet<AmbiguousTombstone>,
}

/// Compare a tombstone identity with an incoming object.
///
/// Identity *kind* is compared before any value: a real UID tombstone can only
/// match a real UID, and an href-derived placeholder can only match a UID-less
/// record at the same resource. This keeps a placeholder like `A.vcf` from
/// swallowing a new card whose real UID happens to be `A.vcf`.
///
/// Within the real-UID kind:
/// * a UID present on both sides must be equal — a different UID is a
///   different vCard even if the resource href was reused;
/// * if the UID matches but the hrefs differ, the object may have moved or
///   been replaced: ambiguous, replay the batch.
///
/// Within the derived kind the normalized href *is* the identity, so an
/// equivalent spelling (`%41.vcf` / `A.vcf`) still matches. A record that
/// carries a real UID, or a href that is not equivalent, is not suppressed.
fn match_suppressed_identity(identity: &SuppressedIdentity, remote: &RemoteContact) -> Suppression {
    // An identity with no remote id at all is a href-only tombstone: it can
    // only ever describe a UID-less object.
    let tombstone_is_derived = identity.remote_id.is_none() || identity.uid_is_derived;
    if tombstone_is_derived {
        // Only a record without a real UID can share the derived identity.
        if !remote.uid_is_derived {
            return Suppression::None;
        }
        let tombstone_key = identity
            .href
            .as_deref()
            .filter(|href| !href.is_empty())
            .map(href_key)
            .or_else(|| identity.remote_id.as_deref().map(href_key));
        let incoming_key = remote
            .href
            .as_deref()
            .filter(|href| !href.is_empty())
            .map(href_key)
            .or_else(|| remote.remote_id.as_deref().map(href_key));
        return match (tombstone_key, incoming_key) {
            (Some(tombstone), Some(incoming)) if tombstone == incoming => Suppression::Suppress,
            _ => Suppression::None,
        };
    }

    // Real UID tombstone: only a real incoming UID can match it.
    if remote.uid_is_derived {
        return Suppression::None;
    }
    let Some(tombstone_uid) = identity.remote_id.as_deref() else {
        return Suppression::None;
    };
    let uid_matches = remote.remote_id.as_deref() == Some(tombstone_uid);
    if !uid_matches {
        return Suppression::None;
    }
    let href_conflicts = match (&identity.href, &remote.href) {
        (Some(tombstone), Some(incoming)) => href_key(tombstone) != href_key(incoming),
        _ => false,
    };
    if href_conflicts {
        Suppression::Ambiguous
    } else {
        Suppression::Suppress
    }
}

impl PendingContactWrites {
    fn contains_local(&self, id: &str) -> bool {
        self.local_ids.contains(id)
    }

    /// Remote identity still owned by a local write that may roll back. A
    /// match must hold the cursor until the write settles.
    fn matches_remote(&self, book_id: &str, remote: &RemoteContact) -> bool {
        remote.remote_id.as_ref().is_some_and(|id| {
            self.remote_ids
                .get(book_id)
                .is_some_and(|ids| ids.contains(id))
        }) || remote.href.as_ref().is_some_and(|href| {
            let key = href_key(href);
            self.hrefs
                .get(book_id)
                .is_some_and(|hrefs| hrefs.contains(&key))
        })
    }

    /// Match an incoming object against the persisted delete tombstones of
    /// this book. A definite match only suppresses the import; it never holds
    /// the cursor. An ambiguous match must hold the cursor (see [`Suppression`])
    /// and returns the intents it came from so their bounded release can be
    /// counted.
    fn suppresses_import(&self, book_id: &str, remote: &RemoteContact) -> SuppressionMatch {
        let Some(identities) = self.suppressed.get(book_id) else {
            return SuppressionMatch::default();
        };
        let mut result = SuppressionMatch::default();
        for identity in identities {
            let matched = match match_suppressed_identity(identity, remote) {
                Suppression::Ambiguous if identity.ambiguity_released => Suppression::None,
                other => other,
            };
            match matched {
                Suppression::Suppress => {
                    result.outcome = Suppression::Suppress;
                    result.matched = Some(identity.clone());
                    // A definite hide wins over an ambiguous sibling: the
                    // object is the deleted one either way.
                    result.ambiguous.clear();
                    return result;
                }
                Suppression::Ambiguous => {
                    result.ambiguous.insert(match &identity.op_id {
                        Some(op_id) => AmbiguousTombstone::Receipt {
                            contact_id: identity.contact_id.clone(),
                            op_id: op_id.clone(),
                        },
                        None => AmbiguousTombstone::Intent(identity.contact_id.clone()),
                    });
                }
                Suppression::None => {}
            }
        }
        if !result.ambiguous.is_empty() {
            result.outcome = Suppression::Ambiguous;
        }
        result
    }

    fn has_unattributed_create(&self, book_id: &str) -> bool {
        self.unattributed_create_books.contains(book_id)
    }

    fn has_book_blocked_delete(&self, book_id: &str) -> bool {
        self.book_blocked_deletes.contains(book_id)
    }

    /// Protect the objects named by persisted delete intents.
    ///
    /// This intentionally does not go through pending operations: the worker
    /// stops executing an operation once it reaches a terminal failure, but the
    /// remote object may still exist. The intent table is the durable record of
    /// what must not be imported back.
    ///
    /// The model has two independent levels:
    /// * an **exact identity** (the intent's or a per-operation receipt's
    ///   remote id/href) hides that object and always lets the cursor advance;
    /// * an intent with **no identity at all** isolates the book (unattributed
    ///   new objects stay hidden) but never holds the cursor.
    ///
    /// Receipts are settled **before** any release decision: a released parent
    /// must not drop the exact identity of a receipt whose object was never
    /// removed.
    fn add_delete_intents(&mut self, conn: &rusqlite::Connection, account_id: &str) -> Result<()> {
        let now = Utc::now();
        // Settled tombstones only suppress for their TTL. Expired ones are
        // dropped here so they cannot block the cursor forever.
        crate::store::prune_expired_contact_delete_tombstones(conn, now)?;
        let intents = crate::store::contact_delete_intents(conn, account_id)?;

        // Per-operation receipts are independent of the parent intent's
        // state. Read and protect them first: an intent may be released (its
        // identity-less isolation lifted) while a later Save's receipt still
        // names an exact object that has to be removed.
        for intent in &intents {
            let book_id = intent.address_book_id.clone();
            for receipt in crate::store::contact_delete_receipts(conn, &intent.contact_id)? {
                let released = receipt.released_at.is_some();
                let settled = receipt.state == crate::store::ContactDeleteReceiptState::Settled;
                // A settled receipt is a tombstone only inside its window. A
                // pending receipt protects until it is settled or released by
                // its own bounded ambiguity counter.
                let active = match receipt.state {
                    crate::store::ContactDeleteReceiptState::Pending => !released,
                    crate::store::ContactDeleteReceiptState::Settled => {
                        !released && receipt.suppress_until.is_some_and(|until| until > now)
                    }
                };
                if !active && !released {
                    continue;
                }
                if receipt.remote_id.is_none() && receipt.href.is_none() {
                    continue;
                }
                self.suppressed
                    .entry(book_id.clone())
                    .or_default()
                    .push(SuppressedIdentity {
                        contact_id: intent.contact_id.clone(),
                        op_id: Some(receipt.op_id.clone()),
                        remote_id: receipt.remote_id.clone(),
                        uid_is_derived: receipt.uid_is_derived,
                        href: receipt.href.clone(),
                        // A released receipt keeps suppressing only a precise
                        // identity; it must no longer hold the cursor on an
                        // ambiguous (moved/renamed) match. Settled tombstones
                        // are terminal for the same reason.
                        ambiguity_released: released || settled,
                    });
                // The user deleted the object this receipt names. A delta may
                // have re-imported it while the parent intent was not
                // protecting it; remove that local copy by its precise
                // identity before the intent can be released. A row that a
                // newer local operation still owns is an edit target and must
                // survive a late empty delta, so it is left untouched.
                let ids = local_ids_precise_identity(
                    conn,
                    &book_id,
                    receipt.remote_id.as_deref(),
                    receipt.href.as_deref(),
                    receipt.uid_is_derived,
                )?;
                let deletable: Vec<String> = ids
                    .into_iter()
                    .filter(|id| !self.local_ids.contains(id))
                    .collect();
                if !deletable.is_empty() {
                    contacts_repo::delete_many(conn, &deletable)?;
                }
            }
        }

        // Identity-less intents are released by wall-clock TTL, never by
        // cursor advancement (a blocked batch may be unable to move it). The
        // precise receipt identities collected above stay protected.
        let released = crate::store::release_expired_contact_delete_intents(
            conn,
            account_id,
            crate::store::CONTACT_DELETE_UNKNOWN_RELEASE_TTL_SECONDS,
            now,
        )?;
        if released > 0 {
            warn!(
                account = account_id,
                released,
                "released unaddressable contact delete intents after their wall-clock TTL"
            );
        }
        let intents = if released > 0 {
            crate::store::contact_delete_intents(conn, account_id)?
        } else {
            intents
        };

        for intent in intents {
            // A released intent no longer isolates the book and its own
            // identity (if it ever had one) must become importable again: that
            // is the point of the bounded ambiguous release.
            if intent.state == crate::store::ContactDeleteIntentState::Released {
                continue;
            }
            let protect_identity = intent.state != crate::store::ContactDeleteIntentState::Settled
                || intent.suppress_until.is_some_and(|until| until > now);
            if !protect_identity {
                continue;
            }
            let book_id = intent.address_book_id.clone();
            if intent.remote_id.is_none() && intent.href.is_none() {
                // The create may have committed server-side without a receipt
                // (state `unknown`). The object cannot be matched by identity,
                // so keep new objects of this book hidden — without making the
                // batch uncommittable.
                self.book_blocked_deletes.insert(book_id);
            } else {
                self.suppressed
                    .entry(book_id)
                    .or_default()
                    .push(SuppressedIdentity {
                        contact_id: intent.contact_id,
                        op_id: None,
                        remote_id: intent.remote_id,
                        uid_is_derived: intent.uid_is_derived,
                        href: intent.href,
                        ambiguity_released: false,
                    });
            }
        }
        Ok(())
    }
}

/// Queue a contact operation on an existing transaction so the local row and
/// its upload intent commit atomically.
fn enqueue_op_sync(
    conn: &rusqlite::Connection,
    account_id: &str,
    op: &ContactOp,
    description: String,
) -> Result<()> {
    let new_op = NewPendingOp::new(account_id, op.kind(), ops::to_value(op)?, description);
    repo::pending_ops::enqueue(conn, new_op)?;
    Ok(())
}

/// Queue a contact operation under an id chosen before the local row was
/// written, on an existing transaction.
///
/// The row records that id as its identity owner, so a late receipt from an
/// older save can be told apart from the operation that most recently
/// confirmed the provider identity. The row write, the owner state and this
/// insert must commit or roll back together.
fn enqueue_op_with_id_sync(
    conn: &rusqlite::Connection,
    op_id: &str,
    account_id: &str,
    op: &ContactOp,
    description: String,
) -> Result<()> {
    let new_op = NewPendingOp::new(account_id, op.kind(), ops::to_value(op)?, description);
    repo::pending_ops::enqueue_with_id(conn, op_id, new_op)?;
    Ok(())
}

/// Contact ids a `pim.contact.*` payload refers to.
fn contact_op_ids(op: &ContactOp) -> Vec<String> {
    match op {
        ContactOp::Save { contact_id, .. } | ContactOp::Favorite { contact_id, .. } => {
            vec![contact_id.clone()]
        }
        ContactOp::Delete { contacts } => contacts
            .iter()
            .map(|snapshot| snapshot.contact.id.clone())
            .collect(),
        ContactOp::GroupCreate { .. }
        | ContactOp::GroupRename { .. }
        | ContactOp::GroupDelete { .. }
        | ContactOp::GroupMembers { .. } => Vec::new(),
    }
}

fn writes_from_ops(ops: impl IntoIterator<Item = PendingOp>) -> Result<PendingContactWrites> {
    let mut writes = PendingContactWrites::default();
    for op in ops {
        let Ok(decoded) = ops::decode(&op) else {
            continue;
        };
        writes.local_ids.extend(contact_op_ids(&decoded));
        match &decoded {
            ContactOp::Save {
                snapshot: Some(snapshot),
                ..
            } => {
                let book = snapshot.contact.address_book_id.clone();
                if let Some(remote_id) = &snapshot.remote_id {
                    writes
                        .remote_ids
                        .entry(book.clone())
                        .or_default()
                        .insert(remote_id.clone());
                }
                if let Some(href) = &snapshot.href {
                    writes.hrefs.entry(book).or_default().insert(href_key(href));
                }
            }
            ContactOp::Delete { contacts } => {
                for snapshot in contacts {
                    let book = snapshot.contact.address_book_id.clone();
                    if let Some(remote_id) = &snapshot.remote_id {
                        writes
                            .remote_ids
                            .entry(book.clone())
                            .or_default()
                            .insert(remote_id.clone());
                        writes
                            .deleted_remote_ids
                            .entry(book.clone())
                            .or_default()
                            .insert(remote_id.clone());
                    }
                    if let Some(href) = &snapshot.href {
                        let key = href_key(href);
                        writes
                            .hrefs
                            .entry(book.clone())
                            .or_default()
                            .insert(key.clone());
                        writes.deleted_hrefs.entry(book).or_default().insert(key);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(writes)
}

fn merge_identity_maps(
    target: &mut HashMap<String, HashSet<String>>,
    source: HashMap<String, HashSet<String>>,
) {
    for (book_id, identities) in source {
        target.entry(book_id).or_default().extend(identities);
    }
}

/// Objects a delta must not overwrite.
///
/// Outstanding operations are always protected. Operations that changed
/// (`created_at`/`updated_at`) at or after the request start are protected too:
/// a `done` upload already replaced the server value, but a batch requested
/// before it finished still describes the older state. `window_start = None`
/// (direct helper calls/tests) treats every operation as a possible race.
fn contact_writes_to_protect(
    conn: &rusqlite::Connection,
    account_id: &str,
    window_start: Option<DateTime<Utc>>,
) -> Result<PendingContactWrites> {
    let outstanding: Vec<PendingOp> = repo::pending_ops::list(conn, Some(account_id))?
        .into_iter()
        .filter(repo::pending_ops::is_outstanding)
        .collect();
    let mut writes = writes_from_ops(outstanding.iter().cloned())?;
    // Window writes are collected *before* intent cleanup: a local edit that
    // was enqueued after the delta request started is still an edit target.
    // A late empty delta must not let a receipt's pre-cleanup delete it.
    let recent = match window_start {
        Some(since) => repo::pending_ops::list_touched_since(conn, Some(account_id), since)?,
        None => repo::pending_ops::list(conn, Some(account_id))?,
    };
    let recent = writes_from_ops(recent)?;
    writes.local_ids.extend(recent.local_ids);
    merge_identity_maps(&mut writes.remote_ids, recent.remote_ids);
    merge_identity_maps(&mut writes.hrefs, recent.hrefs);
    // Persisted delete intents are authoritative and survive a terminal
    // failure of their operation, so they must be read directly rather than
    // through the outstanding-operation set. Receipt pre-cleanup uses the
    // outstanding + window `local_ids` collected above.
    writes.add_delete_intents(conn, account_id)?;
    // Any queued Save without remote identity will execute as a create, no
    // matter what its rollback snapshot says: an older failed create can leave
    // a newer edit (`snapshot: Some`) that still has to POST. Until the upload
    // receipt binds a remote id/href a delta cannot tell whether a newly
    // listed server object is that row, so the whole book must defer importing
    // new objects to avoid a duplicate.
    for op in &outstanding {
        let Ok(ContactOp::Save { contact_id, .. }) = ops::decode(op) else {
            continue;
        };
        let Some(contact) = contacts_repo::get(conn, &contact_id)? else {
            // The row was deleted while its create is still queued or in
            // flight. `add_delete_intents` below protects the server object;
            // nothing more to attribute here.
            continue;
        };
        let meta = contacts_repo::sync_meta(conn, &contact_id)?;
        let has_remote = meta
            .as_ref()
            .is_some_and(|meta| meta.remote_id.is_some() || meta.href.is_some());
        if !has_remote {
            writes
                .unattributed_create_books
                .insert(contact.address_book_id);
        }
    }
    // `deleted_*` stay limited to outstanding operations: a settled (failed)
    // delete whose snapshot rollback restored the row must not keep its
    // absence protected forever.
    Ok(writes)
}

/// May a failed operation restore its snapshot?
///
/// Only when every referenced local row still holds the value the operation
/// left behind: no later operation for the same contact exists and its
/// `updated_at` has not advanced past the failed operation's enqueue time.
/// Otherwise a second edit would be silently reverted to an older snapshot.
fn contacts_unchanged_since(
    conn: &rusqlite::Connection,
    op: &PendingOp,
    ids: &[String],
) -> Result<bool> {
    let ids: HashSet<&str> = ids
        .iter()
        .map(String::as_str)
        .filter(|id| !id.is_empty())
        .collect();
    if ids.is_empty() {
        return Ok(true);
    }
    for other in repo::pending_ops::list(conn, Some(&op.account_id))? {
        if other.id == op.id || other.sequence <= op.sequence {
            continue;
        }
        let Ok(decoded) = ops::decode(&other) else {
            continue;
        };
        if contact_op_ids(&decoded)
            .iter()
            .any(|id| ids.contains(id.as_str()))
        {
            return Ok(false);
        }
    }
    for id in &ids {
        if let Some(contact) = contacts_repo::get(conn, id)? {
            if contact
                .updated_at
                .is_some_and(|updated| updated > op.created_at)
            {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Local rows that one tombstone identity names *precisely*.
///
/// Identity **kind** is compared before any value, exactly like
/// [`match_suppressed_identity`]: a real UID tombstone may only consume a row
/// whose stored identity is a real UID, and a derived placeholder tombstone may
/// only consume a row whose identity was also derived. Without this a derived
/// tombstone `A.vcf` would delete a replacement card whose *real* UID happens
/// to be `A.vcf` at the same href.
///
/// Within one kind the rule is deliberately strict so a tombstone can never
/// consume a different vCard:
/// * real identity, both UID and href known: both must match;
/// * real identity, only the UID known: match by UID (Graph/Google);
/// * derived identity: the normalized href **is** the key. A row that stores
///   no UID at all (`remote_id IS NULL`) is the same no-UID kind and is matched
///   by href too.
fn local_ids_precise_identity(
    conn: &rusqlite::Connection,
    book_id: &str,
    remote_id: Option<&str>,
    href: Option<&str>,
    uid_is_derived: bool,
) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    // An identity without any remote id is a href-only tombstone. It describes
    // a UID-less object, so it belongs to the derived kind (same rule as the
    // tombstone matcher).
    if uid_is_derived || remote_id.is_none() {
        let Some(key) = href
            .filter(|value| !value.is_empty())
            .map(href_key)
            .or_else(|| remote_id.filter(|value| !value.is_empty()).map(href_key))
        else {
            return Ok(ids);
        };
        let mut stmt = conn.prepare(
            "SELECT id, remote_id, href FROM contact
             WHERE address_book_id = ?1 AND (uid_is_derived = 1 OR remote_id IS NULL)",
        )?;
        let mut rows = stmt.query(rusqlite::params![book_id])?;
        while let Some(row) = rows.next()? {
            let stored_remote: Option<String> = row.get(1)?;
            let stored_href: Option<String> = row.get(2)?;
            let stored_key = stored_href
                .as_deref()
                .filter(|value| !value.is_empty())
                .map(href_key)
                .or_else(|| {
                    stored_remote
                        .as_deref()
                        .filter(|value| !value.is_empty())
                        .map(href_key)
                });
            if stored_key.as_deref() == Some(key.as_str()) {
                ids.push(row.get(0)?);
            }
        }
        return Ok(ids);
    }

    let Some(remote_id) = remote_id.filter(|value| !value.is_empty()) else {
        return Ok(ids);
    };
    if let Some(href) = href.filter(|value| !value.is_empty()) {
        // Identities persisted by earlier builds may spell the href
        // differently; equality is by RFC 3986 §6.2.2 key.
        let key = href_key(href);
        let mut stmt = conn.prepare(
            "SELECT id, href FROM contact
             WHERE address_book_id = ?1 AND remote_id = ?2
               AND uid_is_derived = 0 AND href IS NOT NULL",
        )?;
        let mut rows = stmt.query(rusqlite::params![book_id, remote_id])?;
        while let Some(row) = rows.next()? {
            let stored: String = row.get(1)?;
            if href_key(&stored) == key {
                ids.push(row.get(0)?);
            }
        }
    } else {
        let mut stmt = conn.prepare(
            "SELECT id FROM contact
             WHERE address_book_id = ?1 AND remote_id = ?2 AND uid_is_derived = 0",
        )?;
        let mut rows = stmt.query(rusqlite::params![book_id, remote_id])?;
        while let Some(row) = rows.next()? {
            ids.push(row.get(0)?);
        }
    }
    Ok(ids)
}

/// Whether a stored contact's `remote_id` is a href-derived placeholder rather
/// than a provider UID. Rows written before the identity source was persisted
/// default to a real UID.
fn contact_uid_is_derived(conn: &rusqlite::Connection, contact_id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT uid_is_derived FROM contact WHERE id = ?1",
            rusqlite::params![contact_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

/// Record where the identity of a stored contact came from.
fn set_contact_uid_is_derived(
    conn: &rusqlite::Connection,
    contact_id: &str,
    uid_is_derived: bool,
) -> Result<()> {
    conn.execute(
        "UPDATE contact SET uid_is_derived = ?2 WHERE id = ?1",
        rusqlite::params![contact_id, i64::from(uid_is_derived)],
    )?;
    Ok(())
}

/// Identity of one remote object: the remote id, its normalized href and the
/// kind of the remote id.
///
/// Keeping `uid_is_derived` here is what stops a *placeholder* (`A.vcf`
/// derived from a UID-less vCard's href) from being treated as a real UID that
/// happens to spell the same string. The quarantine table stores the raw
/// `remote_id`/`href` columns; the kind is part of the JSON payload (see
/// [`QuarantinedRemoteContact`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteIdentity {
    remote_id: Option<String>,
    /// Normalized with [`href_key`], so equivalent spellings compare equal.
    href: Option<String>,
    uid_is_derived: bool,
}

impl RemoteIdentity {
    /// `None` means the object has no usable identity.
    fn new(remote_id: Option<&str>, href: Option<&str>, uid_is_derived: bool) -> Option<Self> {
        let remote_id = remote_id
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let href = href.filter(|value| !value.is_empty()).map(href_key);
        if remote_id.is_none() && href.is_none() {
            return None;
        }
        Some(Self {
            remote_id,
            href,
            uid_is_derived,
        })
    }
}

/// Whether two identities name the same provider resource.
///
/// The normalized href **is** the resource key: one URL is one object even
/// when its identity kind changes (a vCard gaining or losing its UID), so a
/// snapshot of the UID-less card must be superseded by the same href's
/// real-UID delivery.
///
/// Without a shared href only a **real** UID is an identity, and it survives a
/// resource rename. A derived id is a placeholder built from the href, so two
/// equal placeholders at *different* hrefs (`other/A.vcf` and `A.vcf` both
/// derive `A.vcf`) are different objects and must never be collapsed. Comparing
/// the bare strings is what deleted a real-UID snapshot whose UID happened to
/// equal another card's placeholder.
fn same_remote_resource(a: &RemoteIdentity, b: &RemoteIdentity) -> bool {
    if a.href.is_some() && a.href == b.href {
        return true;
    }
    !a.uid_is_derived && !b.uid_is_derived && a.remote_id.is_some() && a.remote_id == b.remote_id
}

/// Whether two identities name the same *snapshot* for quarantine settlement.
///
/// A shared real UID is deliberately **not** enough here. After a resource
/// rename the old href's snapshot is settled by a tombstone while the new
/// href's snapshot is still `Ambiguous` (same UID, different resource). The
/// cursor that delivered the new snapshot has already advanced, so consuming
/// it because another href's snapshot of the same UID was imported/hidden
/// would delete its only copy. Only an exact kind + normalized-href match may
/// consume a snapshot; the fallback for an identity without an href is the
/// exact provider id within the same kind.
fn same_snapshot_identity(a: &RemoteIdentity, b: &RemoteIdentity) -> bool {
    if a.uid_is_derived != b.uid_is_derived {
        return false;
    }
    match (&a.href, &b.href) {
        (Some(left), Some(right)) => left == right,
        (None, None) => a.remote_id.is_some() && a.remote_id == b.remote_id,
        _ => false,
    }
}

/// Result of applying one provider batch.
#[derive(Debug, Default)]
struct ApplyOutcome {
    /// Some change had to be skipped, so the cursor must not advance.
    skipped: bool,
    /// Remote identities this batch handled definitively: imported, or hidden
    /// by a durable tombstone. Quarantine replay uses this to consume only the
    /// snapshots that will not need another attempt, instead of retaining the
    /// whole batch because one record is still blocked.
    handled: Vec<RemoteIdentity>,
}

/// Apply a provider batch and report whether anything had to be skipped.
///
/// `true` means the caller must **not** advance the sync cursor: the skipped
/// change must be re-delivered once the blocking local write settles, or it
/// would be lost forever.
fn apply_delta(
    conn: &rusqlite::Connection,
    account_id: &str,
    book: &AddressBook,
    delta: &ContactDelta,
) -> Result<bool> {
    Ok(apply_delta_inner(conn, account_id, book, delta, true)?.skipped)
}

/// [`apply_delta`] with the quarantine replay switch.
///
/// Replaying quarantined objects re-enters this function with a synthetic
/// batch; `replay_quarantine = false` keeps that call from recursing.
fn apply_delta_inner(
    conn: &rusqlite::Connection,
    account_id: &str,
    book: &AddressBook,
    delta: &ContactDelta,
    replay_quarantine: bool,
) -> Result<ApplyOutcome> {
    // A delta describes the server, which does not know about a write that is
    // still queued, nor about one that finished uploading after this batch was
    // requested. The local row must keep the user's value in both cases.
    let protected = contact_writes_to_protect(conn, account_id, delta.window_start)?;
    let mut skipped = false;
    let mut handled: Vec<RemoteIdentity> = Vec::new();
    // Tombstones that could not be matched unambiguously with this batch. They
    // hold the cursor, so count one bounded window for each after the batch has
    // been applied. Intents and per-operation receipts are counted separately:
    // a receipt must still be released after its parent intent was.
    let mut ambiguous_tombstones: HashSet<AmbiguousTombstone> = HashSet::new();
    // Deletions first.
    for remote_id in &delta.deleted_remote_ids {
        // `deleted_remote_ids` always names a **real** provider UID (CardDAV
        // reports its UID-less resources through `deleted_hrefs`). Restrict the
        // lookup to the same identity kind so a delete cannot consume a
        // UID-less row whose href-derived placeholder happens to spell the
        // deleted UID.
        if let Some(contact) = contact_by_real_remote_id(conn, &book.id, remote_id)? {
            // The server deleted this object. Forget every quarantined
            // snapshot of it in this same transaction (the one that commits
            // the cursor): a later quarantine release must not resurrect it.
            let meta = contacts_repo::sync_meta(conn, &contact.id)?;
            crate::store::remove_quarantined_contacts_matching(
                conn,
                account_id,
                &book.id,
                Some(remote_id),
                meta.as_ref().and_then(|meta| meta.href.as_deref()),
            )?;
            if protected.contains_local(&contact.id) {
                skipped = true;
                continue;
            }
            contacts_repo::delete_many(conn, &[contact.id])?;
            continue;
        }
        // No local row (for example a pending delete already removed it): the
        // identity itself is enough to forget its quarantined snapshot.
        crate::store::remove_quarantined_contacts_matching(
            conn,
            account_id,
            &book.id,
            Some(remote_id),
            None,
        )?;
        // A queued delete removed the local row before the upload settled.
        // The notification cannot be matched against a row anymore, but the
        // pending snapshot names the same remote object; treat it as skipped
        // so the cursor keeps pointing before it and a failed upload can
        // replay the deletion.
        if protected
            .remote_ids
            .get(&book.id)
            .is_some_and(|ids| ids.contains(remote_id))
        {
            skipped = true;
        }
    }
    for href in &delta.deleted_hrefs {
        // An href names one resource regardless of identity kind, and earlier
        // builds could have left two rows for it (a real-UID row plus a
        // UID-less one). Delete every row at the normalized href; deleting only
        // the first would leak the other forever, because the server will not
        // report this deletion again.
        let rows = contact_rows_by_href(conn, &book.id, href)?;
        // Same as the remote-id path: the provider's deletion is authoritative
        // for the quarantined snapshots at this resource. Match by href only;
        // a snapshot that merely shares a bare remote id at another href is a
        // different object.
        crate::store::remove_quarantined_contacts_matching(
            conn,
            account_id,
            &book.id,
            None,
            Some(href),
        )?;
        if rows.is_empty() {
            if protected
                .hrefs
                .get(&book.id)
                .is_some_and(|hrefs| hrefs.contains(&href_key(href)))
            {
                skipped = true;
            }
            continue;
        }
        let mut deletable = Vec::new();
        for row in rows {
            if protected.contains_local(&row.id) {
                // Keep the row (its local write still owns it) but hold the
                // cursor so the deletion is re-delivered after the write
                // settles.
                skipped = true;
            } else {
                deletable.push(row.id);
            }
        }
        if !deletable.is_empty() {
            contacts_repo::delete_many(conn, &deletable)?;
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
    // Kind-aware view of the same set: the prune and the quarantine replay must
    // compare identities exactly like the upsert does.
    let mut seen_identities: Vec<RemoteIdentity> = Vec::new();
    for remote in &delta.contacts {
        if let Some(remote_id) = &remote.remote_id {
            seen_remote.insert(remote_id.clone());
        }
        if let Some(href) = &remote.href {
            seen_href.insert(href_key(href));
        }
        if let Some(identity) = RemoteIdentity::new(
            remote.remote_id.as_deref(),
            remote.href.as_deref(),
            remote.uid_is_derived,
        ) {
            seen_identities.push(identity);
        }
        let existing = find_existing_contact(conn, &book.id, remote)?;
        // Object-level suppression is evaluated first and independently of
        // the book-level rules below: a persisted tombstone or an exact
        // per-operation receipt identity always hides its object and always
        // lets the cursor advance (even while the book is isolated by an
        // identity-less intent or an unattributed create).
        let suppression = protected.suppresses_import(&book.id, remote);
        ambiguous_tombstones.extend(suppression.ambiguous);
        match suppression.outcome {
            Suppression::Suppress => {
                // A persisted tombstone names this exact object (same book and
                // a matching UID, or a matching href when no UID contradicts
                // it). The object stays hidden, but the tombstone is durable
                // and will never become importable again, so replaying the
                // batch cannot change anything: skip the object and let the
                // cursor advance. A copy that was imported before the
                // tombstone existed is removed by **the identity that matched**
                // — never by the generic upsert lookup, whose href fallback may
                // point at a different vCard that reused the same URL.
                if let Some(identity) = suppression.matched.as_ref() {
                    let ids = local_ids_precise_identity(
                        conn,
                        &book.id,
                        identity.remote_id.as_deref(),
                        identity.href.as_deref(),
                        identity.uid_is_derived,
                    )?;
                    let mut deletable = Vec::new();
                    for id in ids {
                        if protected.contains_local(&id) {
                            // A local write still owns this row (it may roll
                            // back and restore the snapshot); do not delete it.
                            skipped = true;
                        } else {
                            deletable.push(id);
                        }
                    }
                    if !deletable.is_empty() {
                        contacts_repo::delete_many(conn, &deletable)?;
                    }
                }
                // A durable tombstone handles this object for good; a
                // quarantined snapshot of it has nothing left to retry.
                if let Some(identity) = RemoteIdentity::new(
                    remote.remote_id.as_deref(),
                    remote.href.as_deref(),
                    remote.uid_is_derived,
                ) {
                    handled.push(identity);
                }
                continue;
            }
            Suppression::Ambiguous => {
                // One identity field matches while the other conflicts or
                // cannot be verified (e.g. the href was reused by a vCard with
                // another UID). Importing may lose the new object and
                // suppressing may hide it forever; hold the cursor so the
                // change is replayed once the tombstone expires or the
                // identity converges.
                skipped = true;
                continue;
            }
            Suppression::None => {}
        }
        if let Some(previous) = &existing {
            if protected.contains_local(&previous.id) {
                // Keep the optimistic local edit (and its group membership);
                // the remote ids above are already marked as seen so a full
                // sync will not delete the row either.
                skipped = true;
                continue;
            }
        } else if protected.matches_remote(&book.id, remote)
            || protected.has_unattributed_create(&book.id)
        {
            // A queued delete removed the row locally; re-inserting the server
            // copy would undo that delete before it is uploaded. Likewise, an
            // unattributed create may already be this server object: importing
            // it now would leave two rows sharing one remote identity once the
            // receipt binds it to the optimistic row. Both cases need the
            // whole batch replayed, so hold the cursor.
            skipped = true;
            continue;
        } else if protected.has_book_blocked_delete(&book.id) {
            // An intent without any identity (its create receipt was lost)
            // may or may not be this object. Keep the object hidden, but do
            // not hold the cursor: the intent's release is wall-clock based,
            // so replaying the batch can never resolve it. The object is
            // persisted instead: the cursor advances past it, so the provider
            // will not deliver it again. On release the quarantine is replayed
            // and unrelated contacts come back.
            quarantine_remote_contact(conn, account_id, &book.id, remote)?;
            continue;
        }
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
        if remote
            .remote_id
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        {
            // The row now carries this object's identity; record where it came
            // from so later cleanup and upsert cannot cross the real/derived
            // boundary.
            set_contact_uid_is_derived(conn, &contact.id, remote.uid_is_derived)?;
        }
        if let Some(identity) = RemoteIdentity::new(
            remote.remote_id.as_deref(),
            remote.href.as_deref(),
            remote.uid_is_derived,
        ) {
            handled.push(identity);
        }
    }

    if delta.full_sync {
        // A provider that performs a full listing does not necessarily
        // re-deliver unchanged objects: the CardDAV ETag fallback only fetches
        // hrefs whose ETag moved. `seen_href` therefore describes what entered
        // `delta.contacts`, not what the server still holds. Treat every href
        // from the authoritative listing as present, or an unchanged object
        // (visible row or quarantine snapshot) is pruned as deleted.
        let listed_hrefs: HashSet<String> = delta
            .listed_hrefs
            .iter()
            .map(|href| href_key(href))
            .collect();
        let mut stmt = conn.prepare(
            "SELECT id, remote_id, href, uid_is_derived FROM contact
             WHERE address_book_id = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![book.id])?;
        let mut stale = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let remote_id: Option<String> = row.get(1)?;
            let href: Option<String> = row.get(2)?;
            let uid_is_derived: bool = row.get(3)?;
            // Compare identities the same way the upsert does: a listed href
            // is present for both identity kinds, and a real UID is only
            // matched by another real UID. A derived placeholder that merely
            // spells the same string as some other card's real UID must not
            // keep this row alive.
            let row_identity =
                RemoteIdentity::new(remote_id.as_deref(), href.as_deref(), uid_is_derived);
            let seen = href
                .as_ref()
                .is_some_and(|href| listed_hrefs.contains(&href_key(href)))
                || row_identity.as_ref().is_some_and(|identity| {
                    seen_identities
                        .iter()
                        .any(|seen| same_remote_resource(seen, identity))
                });
            let has_remote = remote_id.is_some() || href.is_some();
            // A pending local write may be missing from the server response
            // simply because it has not been uploaded yet; deleting the row
            // would drop the user's edit.
            if has_remote && !seen {
                if protected.contains_local(&id) {
                    skipped = true;
                } else {
                    stale.push(id);
                }
            }
        }
        drop(rows);
        drop(stmt);
        for id in stale {
            contacts_repo::delete_many(conn, &[id])?;
        }
        // The full listing is authoritative for the whole book: a quarantined
        // snapshot whose object is no longer listed (by remote id or href)
        // belongs to an object the server deleted. Drop it in this same
        // transaction as the cursor commit; a later release must not import it
        // again. Snapshots that *are* listed are dropped as superseded by the
        // replay step below.
        let quarantined = crate::store::quarantined_contacts(conn, account_id, &book.id)?;
        for record in quarantined {
            let listed_href = record
                .href
                .as_ref()
                .is_some_and(|href| listed_hrefs.contains(&href_key(href)));
            // The identity kind lives in the payload. If it cannot be read,
            // keep the snapshot: a false "seen" only retains data, while a
            // false "gone" deletes the only copy.
            let seen = if listed_href {
                true
            } else {
                match serde_json::from_str::<QuarantinedRemoteContact>(&record.payload) {
                    Ok(payload) => RemoteIdentity::new(
                        record.remote_id.as_deref(),
                        record.href.as_deref(),
                        payload.uid_is_derived,
                    )
                    .is_none_or(|identity| {
                        seen_identities
                            .iter()
                            .any(|seen| same_remote_resource(seen, &identity))
                    }),
                    Err(_) => true,
                }
            };
            if !seen {
                crate::store::remove_quarantined_contact(conn, &record.id)?;
            }
        }
        // A pending delete already removed its local row, so the full response
        // cannot list it and the loop above cannot observe the absence. Hold
        // the cursor until the delete settles: if its upload fails and the
        // rollback restores the row, a later incremental batch must still see
        // that the server does not have the object.
        if protected
            .deleted_remote_ids
            .get(&book.id)
            .is_some_and(|ids| ids.iter().any(|id| !seen_remote.contains(id)))
        {
            skipped = true;
        }
        if protected.deleted_hrefs.get(&book.id).is_some_and(|hrefs| {
            hrefs
                .iter()
                .any(|href| !seen_href.contains(&href_key(href)))
        }) {
            skipped = true;
        }
    }
    // An identified tombstone that keeps matching the same batch ambiguously
    // (CardDAV UID unchanged after a resource rename) would otherwise hold the
    // cursor forever. Once its operation is terminal, no future action can
    // resolve the conflict, so the block is lifted after a bounded number of
    // windows. Intents and per-operation receipts are counted independently:
    // a released parent must not keep a pending child receipt blocking.
    for tombstone in ambiguous_tombstones {
        match tombstone {
            AmbiguousTombstone::Intent(contact_id) => {
                let released = crate::store::advance_ambiguous_contact_delete_intents(
                    conn,
                    &contact_id,
                    crate::store::CONTACT_DELETE_AMBIGUOUS_RELEASE_WINDOWS,
                    Utc::now(),
                )?;
                if released > 0 {
                    warn!(
                        account = account_id,
                        book = %book.id,
                        contact_id = %contact_id,
                        "released an ambiguous contact delete intent after its bounded sync windows"
                    );
                }
            }
            AmbiguousTombstone::Receipt { contact_id, op_id } => {
                let released = crate::store::advance_ambiguous_contact_delete_receipts(
                    conn,
                    &contact_id,
                    &op_id,
                    crate::store::CONTACT_DELETE_AMBIGUOUS_RELEASE_WINDOWS,
                    Utc::now(),
                )?;
                if released > 0 {
                    warn!(
                        account = account_id,
                        book = %book.id,
                        contact_id = %contact_id,
                        op_id = %op_id,
                        "released an ambiguous contact delete receipt after its bounded sync windows"
                    );
                }
            }
        }
    }
    // A round in which a listed object's identity could not be read is
    // uncertain: do not replay quarantined snapshots against it (that would
    // import a stale copy as if it were confirmed), and keep the cursor before
    // the batch so the next sync retries. Eviction and replay are separate:
    // a snapshot that this very batch replaced with a newer value will never
    // be needed again, because the next round matches the new ETag and does
    // not re-deliver the object. Retaining it would let the old snapshot
    // overwrite the new value on that next round.
    if replay_quarantine {
        if delta.incomplete {
            evict_superseded_quarantined_contacts(conn, account_id, book, &handled)?;
        } else if replay_quarantined_contacts(conn, account_id, book, &protected, &seen_identities)?
        {
            // A newest snapshot could not be applied (it is still `Ambiguous`,
            // or a local write owns its object). Keep the cursor before this
            // batch so the provider redelivers it once the conflict resolves;
            // otherwise the snapshot would be the only remaining copy and
            // nothing would ever trigger its retry.
            skipped = true;
        }
    }
    if delta.incomplete {
        skipped = true;
    }
    Ok(ApplyOutcome { skipped, handled })
}

/// Serialized form of a [`RemoteContact`] kept in `pim_contact_quarantine`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuarantinedRemoteContact {
    contact: Contact,
    remote_id: Option<String>,
    href: Option<String>,
    etag: Option<String>,
    /// Payloads written before the identity kind was tracked default to a real
    /// UID, matching how such tombstones were persisted.
    #[serde(default)]
    uid_is_derived: bool,
    raw_vcard: Option<String>,
    group_names: Vec<String>,
}

impl QuarantinedRemoteContact {
    fn from_remote(remote: &RemoteContact) -> Self {
        Self {
            contact: remote.contact.clone(),
            remote_id: remote.remote_id.clone(),
            href: remote.href.clone(),
            etag: remote.etag.clone(),
            uid_is_derived: remote.uid_is_derived,
            raw_vcard: remote.raw_vcard.clone(),
            group_names: remote.group_names.clone(),
        }
    }

    fn into_remote(self) -> RemoteContact {
        RemoteContact {
            contact: self.contact,
            remote_id: self.remote_id,
            href: self.href,
            etag: self.etag,
            uid_is_derived: self.uid_is_derived,
            raw_vcard: self.raw_vcard,
            group_names: self.group_names,
        }
    }
}

/// Persist an object dropped by book-level isolation so it can be replayed
/// after the isolating intent is released.
fn quarantine_remote_contact(
    conn: &rusqlite::Connection,
    account_id: &str,
    book_id: &str,
    remote: &RemoteContact,
) -> Result<()> {
    let payload =
        serde_json::to_string(&QuarantinedRemoteContact::from_remote(remote)).map_err(|error| {
            AppError::internal(format!("cannot encode quarantined contact: {error}"))
        })?;
    crate::store::quarantine_contact(
        conn,
        account_id,
        book_id,
        remote.remote_id.as_deref(),
        remote.href.as_deref(),
        &payload,
    )
}

/// Drop quarantine snapshots that this batch definitively replaced.
///
/// `handled` names the remote identities the batch imported (or hid behind a
/// durable tombstone). An object that was *newly* quarantined in this batch is
/// deliberately absent from `handled`, so its freshly written snapshot is not
/// dropped here. Supersession uses the same rule as the upsert: the normalized
/// href, or a real UID (which survives a resource rename). A derived
/// placeholder that merely spells the same string as a real UID at another href
/// is a different object and must keep its snapshot.
fn evict_superseded_quarantined_contacts(
    conn: &rusqlite::Connection,
    account_id: &str,
    book: &AddressBook,
    handled: &[RemoteIdentity],
) -> Result<()> {
    if handled.is_empty() {
        return Ok(());
    }
    for record in crate::store::quarantined_contacts(conn, account_id, &book.id)? {
        // The kind is serialized in the payload; without it the identity is
        // incomplete and the snapshot is retained (never lose the only copy).
        let Ok(payload) = serde_json::from_str::<QuarantinedRemoteContact>(&record.payload) else {
            continue;
        };
        let Some(identity) = RemoteIdentity::new(
            record.remote_id.as_deref(),
            record.href.as_deref(),
            payload.uid_is_derived,
        ) else {
            continue;
        };
        let superseded = handled
            .iter()
            .any(|handled| same_remote_resource(handled, &identity));
        if superseded {
            crate::store::remove_quarantined_contact(conn, &record.id)?;
        }
    }
    Ok(())
}

/// Replay objects that book-level isolation discarded once no active intent
/// isolates the book any more.
///
/// Release is wall-clock based while the batch that dropped the object already
/// advanced the cursor, so provider re-delivery cannot be relied on. Replaying
/// through the normal import path restores the unrelated contacts (and applies
/// every current protection again). Records that cannot be imported yet (the
/// book is still blocked, an unattributed create owns the identity) are kept
/// for the next sync.
///
/// Returns `true` when a newest snapshot had to be retained (for example an
/// `Ambiguous` tombstone match). The caller must then keep its own batch
/// cursor before the batch so the provider re-delivers it; otherwise nothing
/// would ever retry the retained snapshot.
fn replay_quarantined_contacts(
    conn: &rusqlite::Connection,
    account_id: &str,
    book: &AddressBook,
    protected: &PendingContactWrites,
    current: &[RemoteIdentity],
) -> Result<bool> {
    if protected.has_book_blocked_delete(&book.id) || protected.has_unattributed_create(&book.id) {
        return Ok(false);
    }
    let records = crate::store::quarantined_contacts(conn, account_id, &book.id)?;
    if records.is_empty() {
        return Ok(false);
    }
    let mut decoded: Vec<Option<QuarantinedRemoteContact>> = Vec::with_capacity(records.len());
    let mut identities: Vec<Option<RemoteIdentity>> = Vec::with_capacity(records.len());
    let mut groups: Vec<String> = Vec::with_capacity(records.len());
    let mut superseded: HashSet<String> = HashSet::new();
    for (index, record) in records.iter().enumerate() {
        // The batch that is being applied is newer than the snapshot it
        // itself delivered to quarantine earlier. If it already handled the
        // same *resource* (normalized href, or the same real UID after a
        // rename), replaying the old snapshot would overwrite the new value
        // *and* its ETag. Forget the snapshot instead; the current batch is
        // authoritative.
        //
        // The kind is needed to reach that decision: a bare remote-id match
        // would treat a real UID and another card's href-derived placeholder as
        // the same object and delete the only copy of the real card. The cursor
        // has already advanced past the batch that produced the snapshot, so
        // that loss would be unrecoverable.
        let payload: QuarantinedRemoteContact =
            serde_json::from_str(&record.payload).map_err(|error| {
                AppError::internal(format!("invalid quarantined contact payload: {error}"))
            })?;
        let identity = RemoteIdentity::new(
            record.remote_id.as_deref(),
            record.href.as_deref(),
            payload.uid_is_derived,
        );
        let replaced = identity.as_ref().is_some_and(|identity| {
            current
                .iter()
                .any(|seen| same_remote_resource(seen, identity))
        });
        if replaced {
            superseded.insert(record.id.clone());
            decoded.push(None);
            identities.push(None);
            groups.push(String::new());
            continue;
        }
        groups.push(quarantine_replay_group_key(&payload, index));
        decoded.push(Some(payload));
        identities.push(identity);
    }
    // A normalized href names one resource, and the provider delivers its
    // successive versions in write order (`records` is ordered by ascending
    // `seq`). Replaying an older version after a newer one would overwrite the
    // new value; replaying an older version *instead of* a newest one that is
    // blocked (for example by an `Ambiguous` tombstone) would resurrect a card
    // the resource no longer holds. Only the newest version of each resource
    // may be replayed; keep it if the replay cannot apply it, and discard the
    // older versions outright because they can never become current again.
    let mut newest_index: HashMap<String, usize> = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        if superseded.contains(&record.id) {
            continue;
        }
        newest_index.insert(groups[index].clone(), index);
    }
    let newest: HashSet<usize> = newest_index.values().copied().collect();
    let mut contacts = Vec::with_capacity(newest.len());
    for (index, record) in records.iter().enumerate() {
        if superseded.contains(&record.id) {
            continue;
        }
        if !newest.contains(&index) {
            // An older version of a resource whose newest version is already
            // known: it must never be applied again.
            superseded.insert(record.id.clone());
            continue;
        }
        if let Some(payload) = decoded[index].take() {
            contacts.push(payload.into_remote());
        }
    }
    for id in &superseded {
        crate::store::remove_quarantined_contact(conn, id)?;
    }
    if contacts.is_empty() {
        return Ok(false);
    }
    let replay = ContactDelta {
        contacts,
        window_start: Some(Utc::now()),
        ..Default::default()
    };
    // Do not recurse into another replay: the synthetic batch is only the
    // persisted objects, and their destination is decided by this call.
    let outcome = apply_delta_inner(conn, account_id, book, &replay, false)?;
    // Consume snapshots **per record**, in the same transaction as the import.
    // A single blocked record (for example an `Ambiguous` tombstone match)
    // must not keep the snapshots of objects that were imported successfully:
    // on the next empty incremental that stale snapshot would overwrite the
    // newer local edit with the value it had when it was quarantined. A newest
    // snapshot that could not be applied stays so the next sync can retry it.
    //
    // Consumption must match the *exact* identity of the record. The same real
    // UID at another href is a different resource after a rename: the old
    // href's snapshot may be handled precisely by a tombstone while the new
    // href's snapshot is still `Ambiguous`. Treating the shared UID as a match
    // would delete the only copy of the new resource, and the incremental
    // cursor that delivered it has already advanced.
    let mut retained = false;
    for (index, record) in records.iter().enumerate() {
        if superseded.contains(&record.id) {
            continue;
        }
        let handled = identities[index].as_ref().is_some_and(|identity| {
            outcome
                .handled
                .iter()
                .any(|handled| same_snapshot_identity(handled, identity))
        });
        if !outcome.skipped || handled {
            crate::store::remove_quarantined_contact(conn, &record.id)?;
        } else {
            retained = true;
        }
    }
    Ok(retained)
}

/// Resource key used to collapse quarantine snapshots before replay.
///
/// The normalized href is the resource identity, so two deliveries of one
/// resource under different provider ids (a card gaining or losing its UID) are
/// one group and only the newest is replayed. Without an href the provider id
/// is the identity; a snapshot with neither is replayed on its own.
fn quarantine_replay_group_key(remote: &QuarantinedRemoteContact, index: usize) -> String {
    if let Some(href) = remote.href.as_deref().filter(|value| !value.is_empty()) {
        return format!("href:{}", href_key(href));
    }
    if let Some(remote_id) = remote
        .remote_id
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        return format!("id:{remote_id}");
    }
    format!("row:{index}")
}

/// Apply a batch and advance the cursor only when nothing was skipped.
///
/// A skipped deletion/update must stay reachable: if the cursor advanced past
/// it, the next sync would never deliver it again, so a pending edit that
/// later fails and rolls back would leave the object locally forever.
fn apply_delta_and_cursor(
    conn: &rusqlite::Connection,
    account_id: &str,
    book: &AddressBook,
    delta: &ContactDelta,
) -> Result<bool> {
    let skipped = apply_delta(conn, account_id, book, delta)?;
    if skipped {
        return Ok(true);
    }
    // Book-level delete isolation and exact tombstones do not set `skipped`:
    // their batches are committed. Release of identity-less intents is
    // wall-clock based (`release_expired_contact_delete_intents`) and never
    // depends on this cursor moving.
    save_cursor(
        conn,
        &book.id,
        delta.sync_token.as_deref(),
        delta.ctag.as_deref(),
    )?;
    Ok(false)
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

/// A local row addressed by href, with the identity kind needed to prefer the
/// row that matches an incoming delivery.
struct HrefContactRow {
    id: String,
    uid_is_derived: bool,
}

/// Rows whose href is spelled exactly like `href` (the common case and an
/// indexed lookup).
fn exact_contact_rows_by_href(
    conn: &rusqlite::Connection,
    book_id: &str,
    href: &str,
) -> Result<Vec<HrefContactRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, uid_is_derived FROM contact
         WHERE address_book_id = ?1 AND href = ?2",
    )?;
    let mut rows = stmt.query(rusqlite::params![book_id, href])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(HrefContactRow {
            id: row.get(0)?,
            uid_is_derived: row.get(1)?,
        });
    }
    Ok(out)
}

/// **Every** row at the normalized href, regardless of identity kind.
///
/// RFC 3986 §6.2.2 equivalent spellings (`A.vcf` / `%41.vcf`) are one resource.
/// Two rows can legitimately exist for one resource in databases written by
/// earlier builds (a real-UID row plus a UID-less one); both must be found so a
/// provider deletion does not leak one of them forever.
fn contact_rows_by_href(
    conn: &rusqlite::Connection,
    book_id: &str,
    href: &str,
) -> Result<Vec<HrefContactRow>> {
    let key = href_key(href);
    let mut stmt = conn.prepare(
        "SELECT id, href, uid_is_derived FROM contact
         WHERE address_book_id = ?1 AND href IS NOT NULL",
    )?;
    let mut rows = stmt.query(rusqlite::params![book_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let stored: String = row.get(1)?;
        if href_key(&stored) == key {
            out.push(HrefContactRow {
                id: row.get(0)?,
                uid_is_derived: row.get(2)?,
            });
        }
    }
    Ok(out)
}

/// The local row with this **real** remote id.
///
/// `deleted_remote_ids` and the real-UID upsert branch must never compare the
/// bare string against a derived placeholder.
fn contact_by_real_remote_id(
    conn: &rusqlite::Connection,
    book_id: &str,
    remote_id: &str,
) -> Result<Option<Contact>> {
    let id: Option<String> = conn
        .query_row(
            "SELECT id FROM contact
             WHERE address_book_id = ?1 AND remote_id = ?2 AND uid_is_derived = 0
             ORDER BY updated_at DESC LIMIT 1",
            rusqlite::params![book_id, remote_id],
            |row| row.get(0),
        )
        .optional()?;
    match id {
        Some(id) => contacts_repo::get(conn, &id),
        None => Ok(None),
    }
}

/// Find the local row an incoming object updates.
///
/// The href lookup is a *resource* match, not an identity match: the same URL
/// is one object even when the identity **kind changes**. A real-UID card may
/// be replaced by a UID-less copy at the same href, or the reverse; updating
/// the existing row in place (and rewriting `uid_is_derived`) keeps one local
/// row per resource. Inserting a second row would also make the eventual
/// provider `404` ambiguous.
///
/// The href is checked **before** the provider id. Once a href-derived card has
/// been written with its derived id as the vCard UID (RFC 6352 §5.1), the next
/// sync delivers that value as a *real* UID. A genuine UID of the same spelling
/// on another resource then exists in the same book; matching by the bare id
/// first would route one card's update into the other's row and overwrite both.
/// Only when the resource URL is unknown must the provider id decide, and then
/// only within the same identity **kind**.
fn find_existing_contact(
    conn: &rusqlite::Connection,
    book_id: &str,
    remote: &RemoteContact,
) -> Result<Option<Contact>> {
    if let Some(href) = remote.href.as_deref().filter(|value| !value.is_empty()) {
        let mut rows = exact_contact_rows_by_href(conn, book_id, href)?;
        if rows.is_empty() {
            // Equivalent spelling stored by an earlier build or provider.
            rows = contact_rows_by_href(conn, book_id, href)?;
        }
        // Prefer a row of the incoming kind when historical duplicates exist;
        // either way this is the same resource, so update it in place rather
        // than inserting a second row.
        let row = rows
            .iter()
            .find(|row| row.uid_is_derived == remote.uid_is_derived)
            .or_else(|| rows.first());
        if let Some(row) = row {
            return contacts_repo::get(conn, &row.id);
        }
    }
    // A derived id is the normalized href, so without an href it names no
    // resource and must never match a real provider UID.
    if !remote.uid_is_derived {
        if let Some(remote_id) = remote
            .remote_id
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            if let Some(contact) = contact_by_real_remote_id(conn, book_id, remote_id)? {
                return Ok(Some(contact));
            }
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
        uid_is_derived: contact_uid_is_derived(conn, id)?,
    }))
}

/// Whether an outstanding `Save` for a contact may already have created a
/// remote object.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OutstandingCreate {
    /// No create exists for the contact.
    None,
    /// Every create is provably unsent and was just cancelled (or already
    /// settled without ever being sent).
    Unsent,
    /// At least one create may have reached the provider before its local row
    /// was deleted; a cleanup intent is required, bound to that operation.
    PossiblySent {
        owner_op_id: String,
        owner_attempt: u32,
    },
}

/// Classify (and cancel) the creates queued for a contact that is being
/// deleted before its upload receipt landed.
///
/// Mirrors the executor's rule: a save without remote identity is a create.
/// `send_intent` is persisted *before* the provider call, so a create without
/// it (and without an ambiguous outcome) cannot have produced a server object.
/// Cancelling such outstanding operations is what keeps a create-then-delete
/// from leaving an unaddressable `unknown` intent behind.
///
/// A terminal failure is **not** proof that nothing was sent: a create whose
/// receipt was lost is marked failed after its retries are exhausted while the
/// server object may well exist. A failed (or otherwise settled) save that
/// carries `send_intent`/`outcome_unknown` therefore still requires an intent.
fn reconcile_outstanding_creates(
    conn: &rusqlite::Connection,
    account_id: &str,
    contact_id: &str,
) -> Result<OutstandingCreate> {
    let mut unsent = false;
    let mut owner: Option<(String, u32)> = None;
    for op in repo::pending_ops::list(conn, Some(account_id))? {
        let Ok(ContactOp::Save { contact_id: id, .. }) = ops::decode(&op) else {
            continue;
        };
        if id != contact_id {
            continue;
        }
        if op.send_intent || op.outcome_unknown {
            // The operation may have issued a wire command, no matter whether
            // it is still queued or already reached a terminal failure. Keep
            // the earliest such operation as the owner: it is the one whose
            // object the cleanup must address.
            if owner.is_none() {
                owner = Some((op.id.clone(), op.attempts));
            }
            continue;
        }
        if !repo::pending_ops::is_outstanding(&op) {
            // Settled without ever sending: nothing to cancel or clean up.
            continue;
        }
        // The operation has not issued (or may not have issued) any wire
        // command in any attempt, so no remote object can exist. Cancel it
        // unconditionally: the worker may have claimed it (`inProgress`) but
        // has not marked a send intent, and its executor aborts if the row is
        // gone.
        repo::pending_ops::cancel(conn, &op.id)?;
        unsent = true;
    }
    Ok(match owner {
        Some((owner_op_id, owner_attempt)) => OutstandingCreate::PossiblySent {
            owner_op_id,
            owner_attempt,
        },
        None if unsent => OutstandingCreate::Unsent,
        None => OutstandingCreate::None,
    })
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
    // Restore the identity source as well: a rolled-back delete must not turn a
    // href-derived identity into a real UID (or the reverse).
    set_contact_uid_is_derived(conn, &contact.id, snapshot.uid_is_derived)?;
    Ok(())
}

/// Roll back a failed save's content, keeping the remote identity the current
/// row holds.
///
/// The snapshot is taken when the save is queued, but an earlier save for the
/// same contact may have its upload confirmed afterwards (a second edit within
/// the same second is enough for [`contacts_unchanged_since`] to accept the
/// stale snapshot). Restoring the snapshot's `remote_id`/`href`/`etag` would
/// then forget the UID and ETag the provider already acknowledged — and
/// `uid_is_derived` would flip back to `true`, so the next edit would allocate
/// a *second* UID for the same card.
///
/// Those columns describe the server object, not local content: the current
/// row always wins. Only the content fields (and group membership) are rolled
/// back from the snapshot.
fn restore_contact_content(conn: &rusqlite::Connection, snapshot: &ContactSnapshot) -> Result<()> {
    let Some(current) = snapshot_contact(conn, &snapshot.contact.id)? else {
        return Ok(());
    };
    contacts_repo::upsert_remote(
        conn,
        &snapshot.contact,
        current.etag.as_deref(),
        current.remote_id.as_deref(),
        current.href.as_deref(),
        snapshot.raw_vcard.as_deref(),
    )?;
    set_contact_uid_is_derived(conn, &snapshot.contact.id, current.uid_is_derived)?;
    Ok(())
}

fn group_names_for(conn: &rusqlite::Connection, contact_id: &str) -> Result<Vec<String>> {
    // Only groups from the contact's own address book; a cross-book membership
    // must never be pushed to the provider as a category.
    let mut stmt = conn.prepare(
        "SELECT g.name FROM contact_group g
         JOIN contact_group_member m ON m.group_id = g.id
         JOIN contact c ON c.id = m.contact_id
         WHERE m.contact_id = ?1 AND c.address_book_id = g.address_book_id
         ORDER BY g.name ASC",
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
    async fn cross_book_group_membership_is_ignored_by_count_and_list() {
        let ctx = ctx().await;
        let service = service(&ctx);
        let other_id = "other-contacts";
        let other = AddressBook {
            id: other_id.into(),
            account_id: "local".into(),
            name: "Other".into(),
            is_read_only: false,
            is_default: false,
        };
        ctx.db
            .write(move |conn| repo::address_books::insert(conn, &mut other.clone()))
            .await
            .expect("book");

        let in_book = service
            .contact_create(contact("local-contacts", "Ada Lovelace", "ada@example.com"))
            .await
            .expect("create");
        let other_contact = service
            .contact_create(contact(other_id, "Grace Hopper", "grace@example.com"))
            .await
            .expect("create");
        let group = service
            .group_create("local-contacts", "Mathematicians")
            .await
            .expect("group");

        // The group editor is asked to add a contact from another book.
        service
            .group_set_members(
                &group.id,
                vec![in_book.id.clone(), other_contact.id.clone()],
            )
            .await
            .expect("members");

        let groups = service
            .group_list(Some("local-contacts".into()))
            .await
            .expect("groups");
        let team = groups
            .iter()
            .find(|candidate| candidate.id == group.id)
            .expect("group row");
        assert_eq!(
            team.member_count, 1,
            "cross-book member must not be counted"
        );

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
        assert_eq!(page.total, 1);
        assert_eq!(page.items.len(), 1, "group count and list must agree");

        // The editor path (`contact_update` -> `replace_groups`) ignores it too.
        let mut edited = other_contact;
        edited.group_ids = vec![group.id.clone()];
        service.contact_update(edited).await.expect("update");
        let groups = service
            .group_list(Some("local-contacts".into()))
            .await
            .expect("groups");
        let team = groups
            .iter()
            .find(|candidate| candidate.id == group.id)
            .expect("group row");
        assert_eq!(team.member_count, 1);
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
            uid_is_derived: false,
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

    /// A provider may spell the same vCard resource differently across rounds
    /// (RFC 3986 §6.2.2). Its `404` must delete the row stored under the other
    /// equivalent spelling.
    #[tokio::test]
    async fn deletes_contact_spelled_with_an_equivalent_href() {
        let ctx = ctx().await;
        let account = ctx.db.read(repo::accounts::local).await.expect("local");
        let book = AddressBook {
            id: "book-href".into(),
            account_id: account.id.clone(),
            name: "Contacts".into(),
            is_read_only: false,
            is_default: true,
        };
        let mut remote = remote_contact("Grace Hopper", "grace@example.com", "remote-1", "");
        remote.href = Some("https://dav.example.com/ab/%41.vcf".into());
        let seed = ContactDelta {
            contacts: vec![remote],
            ..Default::default()
        };
        let book_for_db = book.clone();
        ctx.db
            .write(move |conn| {
                repo::address_books::insert(conn, &mut book_for_db.clone())?;
                apply_delta(conn, "local", &book_for_db, &seed)
            })
            .await
            .expect("seed");
        assert!(ctx
            .db
            .read(|conn| contacts_repo::by_remote_id(conn, "book-href", "remote-1"))
            .await
            .expect("query")
            .is_some());

        let deletion = ContactDelta {
            deleted_hrefs: vec!["https://dav.example.com/ab/A.vcf".into()],
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta(conn, "local", &book_for_db, &deletion))
            .await
            .expect("deletion");
        assert!(!skipped, "nothing blocks the deletion");
        assert!(
            ctx.db
                .read(|conn| contacts_repo::by_remote_id(conn, "book-href", "remote-1"))
                .await
                .expect("query")
                .is_none(),
            "the equivalent spelling must delete the stored contact"
        );
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
                uid_is_derived: false,
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
                uid_is_derived: false,
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

    async fn provider_contacts() -> (CoreContext, ContactsService, AddressBook) {
        let ctx = ctx().await;
        let mut account = ctx.db.read(repo::accounts::local).await.expect("local");
        account.id = "dav1".into();
        account.provider = nuncio_core::models::ProviderKind::Imap;
        account.auth_kind = nuncio_core::models::AuthKind::Password;
        account.email = "me@example.com".into();
        account.oauth_provider = None;
        account.carddav_url = Some("https://dav.example.com/carddav/".into());
        account.contacts_enabled = true;
        ctx.db
            .write(move |conn| repo::accounts::insert(conn, &account))
            .await
            .expect("account");
        let book = AddressBook {
            id: "dav-book".into(),
            account_id: "dav1".into(),
            name: "Contacts".into(),
            is_read_only: false,
            is_default: true,
        };
        let book_for_db = book.clone();
        ctx.db
            .write(move |conn| repo::address_books::insert(conn, &mut book_for_db.clone()))
            .await
            .expect("book");
        let service = service(&ctx);
        (ctx, service, book)
    }

    async fn map_book(ctx: &CoreContext, book: &AddressBook) {
        let mapping = crate::store::AddressBookRemote::new(
            book.id.clone(),
            book.account_id.clone(),
            "carddav",
            "https://dav.example.com/ab/",
            Some("https://dav.example.com/ab/".into()),
        );
        ctx.db
            .write(move |conn| crate::store::set_book(conn, &mapping))
            .await
            .expect("book mapping");
    }

    fn remote_contact(name: &str, email: &str, remote_id: &str, company: &str) -> RemoteContact {
        let mut contact = Contact::new("", "", "dav1");
        contact.display_name = name.into();
        contact.company = company.into();
        contact.emails = vec![nuncio_core::models::ContactField {
            r#type: nuncio_core::models::ContactFieldType::Work,
            value: email.into(),
            is_primary: true,
        }];
        RemoteContact {
            contact,
            remote_id: Some(remote_id.into()),
            href: None,
            etag: Some("etag-1".into()),
            uid_is_derived: false,
            raw_vcard: None,
            group_names: Vec::new(),
        }
    }

    async fn seed(ctx: &CoreContext, book: &AddressBook, contacts: Vec<RemoteContact>) {
        let delta = ContactDelta {
            contacts,
            ..Default::default()
        };
        let book = book.clone();
        ctx.db
            .write(move |conn| apply_delta(conn, "dav1", &book, &delta))
            .await
            .expect("seed delta");
    }

    /// Hide one remote object in the quarantine by making the book isolated
    /// with an identity-less delete intent. The intent stays active, so the
    /// object is not released to the next batch.
    async fn quarantine_via_isolation_blocked(
        ctx: &CoreContext,
        book: &AddressBook,
        remote: RemoteContact,
        intent_id: &str,
    ) {
        ctx.db
            .write({
                let book_id = book.id.clone();
                let intent_id = intent_id.to_string();
                move |conn| {
                    crate::store::record_contact_delete_intent(conn, &intent_id, "dav1", &book_id)?;
                    crate::store::mark_contact_delete_intent_unknown(conn, &intent_id)?;
                    save_cursor(conn, &book_id, Some("token-0"), None)
                }
            })
            .await
            .expect("blocking intent");
        let delta = ContactDelta {
            contacts: vec![remote],
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("blocked delta");
        assert!(!skipped, "book isolation must not hold the cursor");
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            0,
            "the isolated object is not imported yet"
        );
        assert_eq!(quarantine_count(ctx).await, 1);
    }

    /// Age the identity-less intent past its wall-clock TTL so the object is
    /// released for the next batch.
    async fn release_contact_delete_intent(ctx: &CoreContext, intent_id: &str) {
        let aged = repo::dt_string(
            Utc::now()
                - chrono::Duration::seconds(
                    crate::store::CONTACT_DELETE_UNKNOWN_RELEASE_TTL_SECONDS + 60,
                ),
        );
        ctx.db
            .write({
                let intent_id = intent_id.to_string();
                move |conn| {
                    conn.execute(
                        "UPDATE pim_contact_delete_intent SET updated_at = ?2
                         WHERE contact_id = ?1",
                        rusqlite::params![intent_id, aged],
                    )?;
                    Ok(())
                }
            })
            .await
            .expect("age the intent");
    }

    /// Hide one remote object in the quarantine and immediately make it
    /// releasable on the next batch.
    async fn quarantine_via_isolation(
        ctx: &CoreContext,
        book: &AddressBook,
        remote: RemoteContact,
        intent_id: &str,
    ) {
        quarantine_via_isolation_blocked(ctx, book, remote, intent_id).await;
        release_contact_delete_intent(ctx, intent_id).await;
    }

    async fn quarantine_count(ctx: &CoreContext) -> i64 {
        ctx.db
            .read(|conn| {
                conn.query_row("SELECT COUNT(*) FROM pim_contact_quarantine", [], |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(|error| AppError::db(format!("quarantine count: {error}")))
            })
            .await
            .expect("quarantine count")
    }

    async fn cursor_token(ctx: &CoreContext, book: &AddressBook) -> Option<String> {
        let book_id = book.id.clone();
        ctx.db
            .read(move |conn| load_cursor(conn, &book_id))
            .await
            .expect("cursor")
            .and_then(|cursor| cursor.sync_token)
    }

    /// Regression ①: the batch that lifts book isolation may also deliver a
    /// newer value/ETag for an object that is still quarantined. The stale
    /// snapshot must be evicted instead of overwriting the fresh delivery.
    /// Regression: quarantine keys its snapshots by the *normalized* href.
    /// The same resource delivered as `A` (v1), `%41` (v2) and `A` (v3) must
    /// leave one row and replay v3; keying by the raw href kept two rows and
    /// replayed them in insertion order, so the older v2 overwrote v3.
    #[tokio::test]
    async fn quarantine_merges_equivalent_href_spellings_and_replays_the_newest() {
        let (ctx, _service, book) = provider_contacts().await;
        let intent_id = "quarantine-merge";
        ctx.db
            .write({
                let book_id = book.id.clone();
                move |conn| {
                    crate::store::record_contact_delete_intent(conn, intent_id, "dav1", &book_id)?;
                    crate::store::mark_contact_delete_intent_unknown(conn, intent_id)
                }
            })
            .await
            .expect("blocking intent");

        let plain = "https://dav.example.com/ab/A.vcf";
        let encoded = "https://dav.example.com/ab/%41.vcf";
        for (version, href) in [("V1", plain), ("V2", encoded), ("V3", plain)] {
            let mut remote =
                remote_contact(version, "shared@example.com", "remote-shared", version);
            remote.href = Some(href.into());
            remote.etag = Some(format!("etag-{version}"));
            let delta = ContactDelta {
                contacts: vec![remote],
                sync_token: Some(format!("token-{version}")),
                window_start: Some(Utc::now()),
                ..Default::default()
            };
            let book_for_db = book.clone();
            let skipped = ctx
                .db
                .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
                .await
                .expect("blocked delta");
            assert!(!skipped, "book isolation must not hold the cursor");
        }
        assert_eq!(
            quarantine_count(&ctx).await,
            1,
            "equivalent spellings are one quarantined resource"
        );

        release_contact_delete_intent(&ctx, intent_id).await;
        // An empty batch releases the intent and replays the quarantine.
        let book_for_db = book.clone();
        let delta = ContactDelta {
            sync_token: Some("token-release".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("release delta");
        assert!(!skipped);

        let book_id = book.id.clone();
        let stored = ctx
            .db
            .read(move |conn| contacts_repo::by_remote_id(conn, &book_id, "remote-shared"))
            .await
            .expect("lookup")
            .expect("the replayed contact is imported");
        assert_eq!(
            stored.company, "V3",
            "the newest version must win the replay"
        );
        assert_eq!(quarantine_count(&ctx).await, 0, "the snapshot is consumed");
    }

    #[tokio::test]
    async fn released_quarantine_does_not_overwrite_a_newer_delivery() {
        let (ctx, _service, book) = provider_contacts().await;
        let href = "https://dav.example.com/ab/shared.vcf";
        let mut old = remote_contact("Shared", "shared@example.com", "remote-shared", "Old");
        old.href = Some(href.into());
        old.etag = Some("etag-old".into());
        quarantine_via_isolation(&ctx, &book, old, "lost-newer").await;

        let mut new = remote_contact("Shared", "shared@example.com", "remote-shared", "New");
        new.href = Some(href.into());
        new.etag = Some("etag-new".into());
        let delta = ContactDelta {
            contacts: vec![new],
            sync_token: Some("token-2".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("release delta");
        assert!(!skipped);

        let book_id = book.id.clone();
        let stored = ctx
            .db
            .read(move |conn| contacts_repo::by_remote_id(conn, &book_id, "remote-shared"))
            .await
            .expect("lookup")
            .expect("the fresh delivery is imported");
        assert_eq!(
            stored.company, "New",
            "the stale quarantine snapshot must not overwrite the newer value"
        );
        let contact_id = stored.id.clone();
        let meta = ctx
            .db
            .read(move |conn| contacts_repo::sync_meta(conn, &contact_id))
            .await
            .expect("meta")
            .expect("sync meta");
        assert_eq!(meta.etag.as_deref(), Some("etag-new"));
        assert_eq!(quarantine_count(&ctx).await, 0, "the snapshot is consumed");
        assert_eq!(cursor_token(&ctx, &book).await.as_deref(), Some("token-2"));
    }

    /// Regression ②a: an explicit remote-id deletion must also forget the
    /// quarantined snapshot of that object, in the same transaction.
    #[tokio::test]
    async fn explicit_remote_id_delete_clears_quarantine() {
        let (ctx, _service, book) = provider_contacts().await;
        let remote = remote_contact("Gone", "gone@example.com", "remote-gone", "");
        quarantine_via_isolation(&ctx, &book, remote, "lost-remote-id").await;

        let deletion = ContactDelta {
            deleted_remote_ids: vec!["remote-gone".into()],
            sync_token: Some("token-2".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &deletion))
            .await
            .expect("deletion");
        assert!(!skipped);
        assert_eq!(quarantine_count(&ctx).await, 0);

        // A later empty delta must not replay the deleted object.
        let empty = ContactDelta {
            sync_token: Some("token-3".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &empty))
            .await
            .expect("empty delta");
        assert!(!skipped);
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            0,
            "a deleted contact must not be resurrected from quarantine"
        );
        assert_eq!(cursor_token(&ctx, &book).await.as_deref(), Some("token-3"));
    }

    /// Regression ②b: same as ②a through the CardDAV href path.
    #[tokio::test]
    async fn explicit_href_delete_clears_quarantine() {
        let (ctx, _service, book) = provider_contacts().await;
        let href = "https://dav.example.com/ab/gone.vcf";
        let mut remote = remote_contact("Gone", "gone@example.com", "remote-gone-href", "");
        remote.href = Some(href.into());
        quarantine_via_isolation(&ctx, &book, remote, "lost-href").await;

        let deletion = ContactDelta {
            deleted_hrefs: vec![href.into()],
            sync_token: Some("token-2".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &deletion))
            .await
            .expect("deletion");
        assert!(!skipped);
        assert_eq!(quarantine_count(&ctx).await, 0);

        let empty = ContactDelta {
            sync_token: Some("token-3".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &empty))
            .await
            .expect("empty delta");
        assert!(!skipped);
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            0,
            "a deleted contact must not be resurrected from quarantine"
        );
        assert_eq!(cursor_token(&ctx, &book).await.as_deref(), Some("token-3"));
    }

    /// Regression ②c: a full reconciliation that no longer lists the object
    /// clears its quarantine snapshot too.
    #[tokio::test]
    async fn full_sync_absence_clears_quarantine() {
        let (ctx, _service, book) = provider_contacts().await;
        let remote = remote_contact("Gone", "gone@example.com", "remote-gone-full", "");
        quarantine_via_isolation(&ctx, &book, remote, "lost-full").await;

        let full = ContactDelta {
            full_sync: true,
            sync_token: Some("token-2".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &full))
            .await
            .expect("full sync");
        assert!(!skipped);
        assert_eq!(
            quarantine_count(&ctx).await,
            0,
            "the full listing is authoritative for quarantined objects too"
        );

        let empty = ContactDelta {
            sync_token: Some("token-3".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &empty))
            .await
            .expect("empty delta");
        assert!(!skipped);
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            0,
            "a deleted contact must not be resurrected from quarantine"
        );
        assert_eq!(cursor_token(&ctx, &book).await.as_deref(), Some("token-3"));
    }

    /// Regression: the CardDAV ctag/ETag fallback baseline must include objects
    /// hidden in the quarantine. The server no longer lists the object, so an
    /// authoritative empty listing has to produce a delete; otherwise the
    /// quarantine replay resurrects a contact the server already removed.
    #[tokio::test]
    async fn empty_authoritative_etag_listing_does_not_replay_quarantine() {
        let (ctx, _service, book) = provider_contacts().await;
        let href = "https://dav.example.com/ab/gone-again.vcf";
        let mut remote = remote_contact(
            "Gone Again",
            "gone-again@example.com",
            "remote-gone-again",
            "",
        );
        remote.href = Some(href.into());
        remote.etag = Some("etag-gone-again".into());
        quarantine_via_isolation(&ctx, &book, remote, "lost-gone-again").await;

        // This is exactly what the ctag fallback diffs the server listing
        // against.
        let book_id = book.id.clone();
        let baseline = ctx
            .db
            .read(move |conn| local_etags(conn, &book_id))
            .await
            .expect("baseline");
        assert!(
            baseline
                .iter()
                .any(|(local_href, etag)| local_href == href && etag == "etag-gone-again"),
            "the quarantined object must stay in the ETag baseline"
        );

        // An empty authoritative listing means every baseline href is gone.
        let deleted_hrefs: Vec<String> = baseline.into_iter().map(|(href, _)| href).collect();
        let delta = ContactDelta {
            deleted_hrefs,
            sync_token: Some("token-2".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("empty listing");
        assert!(!skipped);
        assert_eq!(
            quarantine_count(&ctx).await,
            0,
            "the authoritative absence must clear the snapshot before replay"
        );

        // No later incremental may resurrect it.
        let empty = ContactDelta {
            sync_token: Some("token-3".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &empty))
            .await
            .expect("empty delta");
        assert!(!skipped);
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            0,
            "a contact deleted by the authoritative listing must not come back"
        );
    }

    /// Regression: a CardDAV server that advertises neither ctag nor
    /// sync-token always takes the ETag fallback, so the batch is marked
    /// `full_sync`. Only hrefs whose ETag moved enter `delta.contacts`;
    /// unchanged objects exist solely in the authoritative listing. The
    /// full-sync prune must not treat "not re-downloaded" as "deleted on the
    /// server": a quarantined object that is still listed keeps its snapshot,
    /// and releasing the isolation restores it.
    #[tokio::test]
    async fn etag_fallback_full_sync_keeps_unchanged_quarantine() {
        let (ctx, _service, book) = provider_contacts().await;
        let href = "https://dav.example.com/ab/kept.vcf";
        let mut kept = remote_contact("Kept", "kept@example.com", "remote-kept", "");
        kept.href = Some(href.into());
        kept.etag = Some("etag-kept".into());
        quarantine_via_isolation_blocked(&ctx, &book, kept, "lost-etag-unchanged").await;

        // What the ETag fallback produces when nothing changed: the server
        // lists the object, but it is not fetched into `contacts` because its
        // ETag matches the local baseline.
        let full = ContactDelta {
            full_sync: true,
            listed_hrefs: vec![href.into()],
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &full))
            .await
            .expect("full listing");
        assert!(!skipped);
        assert_eq!(
            quarantine_count(&ctx).await,
            1,
            "the snapshot of a listed, unchanged object must survive the full-sync prune"
        );

        // Release the isolation: the snapshot is replayed, not lost.
        release_contact_delete_intent(&ctx, "lost-etag-unchanged").await;
        let empty = ContactDelta {
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &empty))
            .await
            .expect("release");
        assert!(!skipped);
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            1,
            "the released contact must be restored from the quarantine"
        );
        assert_eq!(quarantine_count(&ctx).await, 0, "the snapshot is consumed");
    }

    /// Regression: when the listing names an object whose ETag could not be
    /// read (per-resource `503`), the round is uncertain. The href is still in
    /// `listed_hrefs`, so the prune keeps it; the snapshot must not be replayed
    /// against that unconfirmed state; and the cursor must stay put so the next
    /// sync retries the identity.
    #[tokio::test]
    async fn incomplete_round_keeps_quarantine_and_holds_cursor() {
        let (ctx, _service, book) = provider_contacts().await;
        let href = "https://dav.example.com/ab/unreadable.vcf";
        let mut remote = remote_contact("Unreadable", "u@example.com", "remote-unreadable", "");
        remote.href = Some(href.into());
        remote.etag = Some("etag-old".into());
        quarantine_via_isolation(&ctx, &book, remote, "lost-incomplete").await;

        let uncertain = ContactDelta {
            full_sync: true,
            listed_hrefs: vec![href.into()],
            incomplete: true,
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &uncertain))
            .await
            .expect("uncertain listing");
        assert!(skipped, "an uncertain round must not advance the cursor");
        assert_eq!(
            quarantine_count(&ctx).await,
            1,
            "the snapshot of a listed-but-unreadable object must survive"
        );
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            0,
            "an unconfirmed snapshot must not be replayed"
        );
    }

    /// Regression: the same incomplete round can still deliver a *new* value
    /// for another object. That object's old quarantine snapshot is superseded
    /// and must be evicted even though the round cannot replay the rest. If it
    /// survives, the next round matches the new ETag (so the server re-sends no
    /// body) and the stale snapshot is replayed over the fresh value.
    #[tokio::test]
    async fn incomplete_round_evicts_a_snapshot_superseded_by_the_batch() {
        let (ctx, _service, book) = provider_contacts().await;
        let href = "https://dav.example.com/ab/shared.vcf";
        let mut old = remote_contact("Shared", "shared@example.com", "remote-shared", "Old");
        old.href = Some(href.into());
        old.etag = Some("etag-old".into());
        quarantine_via_isolation(&ctx, &book, old, "lost-incomplete-newer").await;

        let mut new = remote_contact("Shared", "shared@example.com", "remote-shared", "New");
        new.href = Some(href.into());
        new.etag = Some("etag-new".into());
        let delta = ContactDelta {
            contacts: vec![new],
            sync_token: Some("token-2".into()),
            incomplete: true,
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("incomplete delta");
        assert!(skipped, "an incomplete round must not advance the cursor");
        assert_eq!(
            cursor_token(&ctx, &book).await.as_deref(),
            Some("token-1"),
            "the cursor must still point before the batch"
        );

        let book_id = book.id.clone();
        let stored = ctx
            .db
            .read(move |conn| contacts_repo::by_remote_id(conn, &book_id, "remote-shared"))
            .await
            .expect("lookup")
            .expect("the fresh delivery is imported");
        assert_eq!(stored.company, "New");
        assert_eq!(
            quarantine_count(&ctx).await,
            0,
            "a snapshot superseded by this batch must be evicted even when the round is incomplete"
        );

        // The next complete round delivers no body for this object (its ETag
        // already matches). The evicted snapshot must not resurrect "Old".
        let empty = ContactDelta {
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &empty))
            .await
            .expect("release");
        assert!(!skipped);
        let book_id = book.id.clone();
        let stored = ctx
            .db
            .read(move |conn| contacts_repo::by_remote_id(conn, &book_id, "remote-shared"))
            .await
            .expect("lookup")
            .expect("the contact is still there");
        assert_eq!(
            stored.company, "New",
            "the stale snapshot must not be replayed after the incomplete round"
        );
    }

    /// Companion regression: the same ETag fallback also omits *visible*
    /// unchanged contacts from `contacts`. A `full_sync` prune that ignores
    /// the server listing would delete them as if the server had removed them.
    #[tokio::test]
    async fn etag_fallback_full_sync_keeps_unchanged_visible_contact() {
        let (ctx, _service, book) = provider_contacts().await;
        let href = "https://dav.example.com/ab/visible.vcf";
        let mut visible = remote_contact("Visible", "visible@example.com", "remote-visible", "");
        visible.href = Some(href.into());
        visible.etag = Some("etag-visible".into());
        seed(&ctx, &book, vec![visible]).await;

        let full = ContactDelta {
            full_sync: true,
            listed_hrefs: vec![href.into()],
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &full))
            .await
            .expect("full listing");
        assert!(!skipped);
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            1,
            "an unchanged contact that the server still lists must not be pruned"
        );
    }

    /// Regression: one `Ambiguous` record must not keep the whole replay batch
    /// in the quarantine. Snapshots of contacts that were imported are consumed
    /// per record, so they cannot overwrite a later edit on the next empty
    /// incremental.
    #[tokio::test]
    async fn partial_replay_consumes_only_the_imported_snapshots() {
        let (ctx, service, book) = provider_contacts().await;

        // A lost create receipt isolates the book and hides both objects.
        ctx.db
            .write({
                let book_id = book.id.clone();
                move |conn| {
                    crate::store::record_contact_delete_intent(
                        conn,
                        "lost-create",
                        "dav1",
                        &book_id,
                    )?;
                    crate::store::mark_contact_delete_intent_unknown(conn, "lost-create")?;
                    save_cursor(conn, &book_id, Some("token-0"), None)
                }
            })
            .await
            .expect("blocking intent");

        let a_href = "https://dav.example.com/ab/a.vcf";
        let mut alpha = remote_contact("Alpha", "alpha@example.com", "uid-a", "Old company");
        alpha.href = Some(a_href.into());
        let mut beta = remote_contact("Beta", "beta@example.com", "uid-b", "Beta company");
        beta.href = Some("https://dav.example.com/ab/b-current.vcf".into());
        let blocked = ContactDelta {
            contacts: vec![alpha, beta],
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &blocked))
            .await
            .expect("blocked delta");
        assert!(!skipped);
        assert_eq!(quarantine_count(&ctx).await, 2);

        // Release the identity-less intent, and add a tombstone that matches
        // Beta's UID at an older href: same UID + different href is ambiguous.
        let aged = repo::dt_string(
            Utc::now()
                - chrono::Duration::seconds(
                    crate::store::CONTACT_DELETE_UNKNOWN_RELEASE_TTL_SECONDS + 60,
                ),
        );
        ctx.db
            .write({
                let book_id = book.id.clone();
                move |conn| {
                    conn.execute(
                        "UPDATE pim_contact_delete_intent SET updated_at = ?2
                         WHERE contact_id = ?1",
                        rusqlite::params!["lost-create", aged],
                    )?;
                    crate::store::record_contact_delete_intent(
                        conn,
                        "still-deleted",
                        "dav1",
                        &book_id,
                    )?;
                    crate::store::attach_contact_delete_intent(
                        conn,
                        "still-deleted",
                        Some("uid-b"),
                        Some("https://dav.example.com/ab/b-old.vcf"),
                        Some("etag-b-old"),
                        false,
                    )?;
                    Ok(())
                }
            })
            .await
            .expect("ambiguous tombstone");

        // The replay imports Alpha and keeps Beta blocked.
        let release = ContactDelta {
            sync_token: Some("token-2".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        // Beta's snapshot stays blocked, so this batch's cursor is held and
        // the next delivery retries the replay.
        ctx.db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &release))
            .await
            .expect("release delta");
        assert_eq!(
            quarantine_count(&ctx).await,
            1,
            "only the blocked snapshot may remain"
        );
        let stored = ctx
            .db
            .read({
                let book_id = book.id.clone();
                move |conn| contacts_repo::by_remote_id(conn, &book_id, "uid-a")
            })
            .await
            .expect("lookup")
            .expect("Alpha must be imported");
        assert_eq!(stored.company, "Old company");

        // A later edit of Alpha settles; the next empty incremental must not
        // replay a stale snapshot of Alpha over it.
        let mut edited = stored.clone();
        edited.company = "New company".into();
        service.contact_update(edited).await.expect("edit");
        let op = save_op(&ctx).await;
        let op_id = op.id.clone();
        ctx.db
            .write(move |conn| repo::pending_ops::mark_done(conn, &op_id))
            .await
            .expect("settle edit");

        let empty = ContactDelta {
            sync_token: Some("token-3".into()),
            window_start: Some(Utc::now() + chrono::Duration::seconds(1)),
            ..Default::default()
        };
        let book_for_db = book.clone();
        ctx.db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &empty))
            .await
            .expect("empty delta");

        let reloaded = service.contact_get(&stored.id).await.expect("get");
        assert_eq!(
            reloaded.company, "New company",
            "an imported snapshot must not overwrite the newer edit"
        );
    }

    #[tokio::test]
    async fn pending_contact_edit_survives_stale_delta() {
        let (ctx, service, book) = provider_contacts().await;
        seed(
            &ctx,
            &book,
            vec![remote_contact(
                "Grace Hopper",
                "grace@example.com",
                "remote-1",
                "Navy",
            )],
        )
        .await;
        let stored = ctx
            .db
            .read({
                let book_id = book.id.clone();
                move |conn| contacts_repo::by_remote_id(conn, &book_id, "remote-1")
            })
            .await
            .expect("query")
            .expect("contact");
        let mut edited = stored.clone();
        edited.company = "Local edit".into();
        service.contact_update(edited).await.expect("update");

        // An older server copy arrives while the save is still queued.
        seed(
            &ctx,
            &book,
            vec![remote_contact(
                "Grace Hopper",
                "grace@example.com",
                "remote-1",
                "Navy",
            )],
        )
        .await;

        let reloaded = service.contact_get(&stored.id).await.expect("get");
        assert_eq!(
            reloaded.company, "Local edit",
            "a queued local edit must not be overwritten by an older delta"
        );
    }

    #[tokio::test]
    async fn contact_delta_without_pending_still_applies() {
        let (ctx, _service, book) = provider_contacts().await;
        seed(
            &ctx,
            &book,
            vec![
                remote_contact("Grace Hopper", "grace@example.com", "remote-1", "Navy"),
                remote_contact("Ada Lovelace", "ada@example.com", "remote-2", "Analytical"),
            ],
        )
        .await;

        seed(
            &ctx,
            &book,
            vec![remote_contact(
                "Ada Lovelace",
                "ada@example.com",
                "remote-2",
                "From remote",
            )],
        )
        .await;

        let stored = ctx
            .db
            .read({
                let book_id = book.id.clone();
                move |conn| contacts_repo::by_remote_id(conn, &book_id, "remote-2")
            })
            .await
            .expect("query")
            .expect("contact");
        assert_eq!(stored.company, "From remote");
    }

    #[tokio::test]
    async fn failed_save_rollback_keeps_newer_local_edit() {
        let (ctx, service, book) = provider_contacts().await;
        seed(
            &ctx,
            &book,
            vec![remote_contact(
                "Grace Hopper",
                "grace@example.com",
                "remote-1",
                "Navy",
            )],
        )
        .await;
        let stored = ctx
            .db
            .read({
                let book_id = book.id.clone();
                move |conn| contacts_repo::by_remote_id(conn, &book_id, "remote-1")
            })
            .await
            .expect("query")
            .expect("contact");

        let mut first = stored.clone();
        first.company = "First edit".into();
        service.contact_update(first).await.expect("first update");
        let mut second = stored.clone();
        second.company = "Second edit".into();
        service.contact_update(second).await.expect("second update");

        let first_op = ctx
            .db
            .read(|conn| repo::pending_ops::list(conn, Some("dav1")))
            .await
            .expect("ops")
            .into_iter()
            .find(|op| op.kind == ops::KIND_SAVE)
            .expect("save op");
        service.rollback(&first_op).await.expect("rollback");

        let reloaded = service.contact_get(&stored.id).await.expect("get");
        assert_eq!(
            reloaded.company, "Second edit",
            "rolling back the older op must not revert the newer edit"
        );
    }

    #[tokio::test]
    async fn failed_save_rollback_restores_when_unchanged() {
        let (ctx, service, book) = provider_contacts().await;
        seed(
            &ctx,
            &book,
            vec![remote_contact(
                "Grace Hopper",
                "grace@example.com",
                "remote-1",
                "Navy",
            )],
        )
        .await;
        let stored = ctx
            .db
            .read({
                let book_id = book.id.clone();
                move |conn| contacts_repo::by_remote_id(conn, &book_id, "remote-1")
            })
            .await
            .expect("query")
            .expect("contact");
        let mut edited = stored.clone();
        edited.company = "Failed edit".into();
        service.contact_update(edited).await.expect("update");

        let save_op = ctx
            .db
            .read(|conn| repo::pending_ops::list(conn, Some("dav1")))
            .await
            .expect("ops")
            .into_iter()
            .find(|op| op.kind == ops::KIND_SAVE)
            .expect("save op");
        service.rollback(&save_op).await.expect("rollback");

        let reloaded = service.contact_get(&stored.id).await.expect("get");
        assert_eq!(reloaded.company, "Navy");
    }

    /// Same-second saves: the first upload confirms a real UID and a fresh
    /// ETag, then the second fails and rolls back. The rollback may restore
    /// the queued content, but the remote identity is a server fact — replaying
    /// the older snapshot's `remote_id`/`href`/`etag`/`uid_is_derived` would
    /// forget the confirmed UID and make the next edit allocate a second one
    /// for the same card.
    #[tokio::test]
    async fn failed_save_rollback_keeps_the_confirmed_remote_identity() {
        let (ctx, service, book) = provider_contacts().await;
        // Start from an href-derived identity: the placeholder is not a real
        // UID yet, so the first successful rewrite assigns one.
        let href = "https://dav.example.com/ab/grace.vcf";
        let mut remote = remote_contact("Grace Hopper", "grace@example.com", href, "Navy");
        remote.href = Some(href.into());
        remote.uid_is_derived = true;
        seed(&ctx, &book, vec![remote]).await;
        let stored = ctx
            .db
            .read({
                let book_id = book.id.clone();
                move |conn| contacts_repo::by_remote_id(conn, &book_id, href)
            })
            .await
            .expect("query")
            .expect("contact");

        let first = service.contact_get(&stored.id).await.expect("get");
        let mut first_edit = first;
        first_edit.company = "First edit".into();
        service
            .contact_update(first_edit)
            .await
            .expect("first update");
        let first_op = save_op(&ctx).await;

        // The second edit is queued before the first upload's receipt lands,
        // so its rollback snapshot still carries the href-derived identity.
        let mut second_edit = service.contact_get(&stored.id).await.expect("get");
        second_edit.company = "Second edit".into();
        service
            .contact_update(second_edit)
            .await
            .expect("second update");
        let second_op = ctx
            .db
            .read(|conn| repo::pending_ops::list(conn, Some("dav1")))
            .await
            .expect("ops")
            .into_iter()
            .rfind(|op| op.kind == ops::KIND_SAVE)
            .expect("second save op");
        assert_ne!(first_op.id, second_op.id);

        // The first upload succeeds and the provider confirms a real UID and a
        // new ETag. Its receipt does not advance `updated_at`, so the
        // same-second "unchanged" check still lets the second rollback run.
        let contact_id = stored.id.clone();
        let first_op_id = first_op.id.clone();
        ctx.db
            .write(move |conn| {
                conn.execute(
                    "UPDATE contact SET remote_id = 'real-uid-1', etag = 'etag-2',
                                        uid_is_derived = 0
                     WHERE id = ?1",
                    rusqlite::params![contact_id],
                )?;
                repo::pending_ops::mark_done(conn, &first_op_id)
            })
            .await
            .expect("first upload receipt");

        service.rollback(&second_op).await.expect("rollback");

        let rolled = ctx
            .db
            .read({
                let id = stored.id.clone();
                move |conn| snapshot_contact(conn, &id)
            })
            .await
            .expect("snapshot")
            .expect("contact row");
        assert_eq!(
            rolled.contact.company, "First edit",
            "the failed save must still restore the content captured when it was queued"
        );
        assert_eq!(
            rolled.remote_id.as_deref(),
            Some("real-uid-1"),
            "the UID the provider confirmed must survive the rollback"
        );
        assert_eq!(
            rolled.etag.as_deref(),
            Some("etag-2"),
            "the ETag the provider confirmed must survive the rollback"
        );
        assert!(
            !rolled.uid_is_derived,
            "the confirmed identity must stay a real UID, not the href placeholder"
        );

        // The next edit must treat the card as an existing real-UID object and
        // must not allocate another UID.
        let mut third_edit = service.contact_get(&stored.id).await.expect("get");
        third_edit.company = "Third edit".into();
        service
            .contact_update(third_edit)
            .await
            .expect("third update");
        let third_op = ctx
            .db
            .read(|conn| repo::pending_ops::list(conn, Some("dav1")))
            .await
            .expect("ops")
            .into_iter()
            .rfind(|op| op.kind == ops::KIND_SAVE)
            .expect("third save op");
        match ops::decode(&third_op).expect("decode") {
            ContactOp::Save {
                assigned_uid,
                snapshot,
                ..
            } => {
                assert!(
                    assigned_uid.is_none(),
                    "no UID may be assigned before the wire command"
                );
                let snapshot = snapshot.expect("update snapshot");
                assert_eq!(snapshot.remote_id.as_deref(), Some("real-uid-1"));
                assert_eq!(snapshot.etag.as_deref(), Some("etag-2"));
                assert!(
                    !snapshot.uid_is_derived,
                    "the next edit must not classify the confirmed UID as derived"
                );
            }
            other => panic!("unexpected operation: {other:?}"),
        }
    }

    async fn seed_one(ctx: &CoreContext, book: &AddressBook) -> Contact {
        seed(
            ctx,
            book,
            vec![remote_contact(
                "Grace Hopper",
                "grace@example.com",
                "remote-1",
                "Navy",
            )],
        )
        .await;
        ctx.db
            .read({
                let book_id = book.id.clone();
                move |conn| contacts_repo::by_remote_id(conn, &book_id, "remote-1")
            })
            .await
            .expect("query")
            .expect("contact")
    }

    async fn save_op(ctx: &CoreContext) -> PendingOp {
        ctx.db
            .read(|conn| repo::pending_ops::list(conn, Some("dav1")))
            .await
            .expect("ops")
            .into_iter()
            .find(|op| op.kind == ops::KIND_SAVE)
            .expect("save op")
    }

    /// Simulate the worker having claimed the create and persisted its send
    /// intent right before the provider POST.
    async fn mark_save_sent(ctx: &CoreContext) {
        let op = save_op(ctx).await;
        ctx.db
            .write(move |conn| {
                repo::pending_ops::mark_in_progress(conn, &op.id)?;
                repo::pending_ops::mark_send_intent(conn, &op.id).map(|_| ())
            })
            .await
            .expect("mark save sent");
    }

    /// Issue 1: a failed queue insert must roll the optimistic row back too.
    #[tokio::test]
    async fn contact_update_and_enqueue_commit_atomically() {
        let (ctx, service, book) = provider_contacts().await;
        let stored = seed_one(&ctx, &book).await;
        ctx.db
            .write(|conn| {
                conn.execute_batch(
                    "CREATE TRIGGER block_pending_insert
                     BEFORE INSERT ON pending_op
                     BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
                )?;
                Ok(())
            })
            .await
            .expect("trigger");

        let mut edited = stored.clone();
        edited.company = "Must not land".into();
        let result = service.contact_update(edited).await;
        assert!(result.is_err(), "enqueue failure must fail the update");

        let reloaded = service.contact_get(&stored.id).await.expect("get");
        assert_eq!(
            reloaded.company, "Navy",
            "the local write must roll back with the failed enqueue"
        );
        let ops = ctx
            .db
            .read(|conn| repo::pending_ops::list(conn, Some("dav1")))
            .await
            .expect("ops");
        assert!(ops.is_empty(), "no operation may be left behind");
    }

    /// Issue 2: a delta fetched before a successful upload landed must not
    /// overwrite the local value just because the operation now says `done`.
    #[tokio::test]
    async fn late_delta_after_successful_upload_keeps_local_contact() {
        let (ctx, service, book) = provider_contacts().await;
        let stored = seed_one(&ctx, &book).await;

        let mut edited = stored.clone();
        edited.company = "Uploaded edit".into();
        service.contact_update(edited).await.expect("update");
        let op = save_op(&ctx).await;
        let contact_id = stored.id.clone();

        // Simulate the worker: the remote write succeeded and the row metadata
        // (etag/updated_at) was refreshed. The operation is no longer pending.
        ctx.db
            .write(move |conn| {
                repo::pending_ops::mark_done(conn, &op.id)?;
                conn.execute(
                    "UPDATE contact SET etag = 'etag-2', updated_at = ?2 WHERE id = ?1",
                    rusqlite::params![contact_id, repo::now_string()],
                )?;
                Ok(())
            })
            .await
            .expect("settle upload");

        // The delta request started before the upload settled; its payload is
        // the old server state.
        let delta = ContactDelta {
            contacts: vec![remote_contact(
                "Grace Hopper",
                "grace@example.com",
                "remote-1",
                "Navy",
            )],
            window_start: Some(Utc::now() - chrono::Duration::seconds(1)),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("delta");
        assert!(skipped, "the stale row must be reported as skipped");

        let reloaded = service.contact_get(&stored.id).await.expect("get");
        assert_eq!(
            reloaded.company, "Uploaded edit",
            "a stale delta must not revert an already uploaded edit"
        );
    }

    /// Issue 3: a deletion hidden behind a pending edit must be replayed after
    /// that edit fails and rolls back instead of being lost with the cursor.
    #[tokio::test]
    async fn skipped_deletion_is_replayed_after_failed_edit_rolls_back() {
        let (ctx, service, book) = provider_contacts().await;
        let stored = seed_one(&ctx, &book).await;
        let mut edited = stored.clone();
        edited.company = "Local edit".into();
        service.contact_update(edited).await.expect("update");
        let op = save_op(&ctx).await;

        // A cursor from the previous successful sync.
        let book_id = book.id.clone();
        ctx.db
            .write(move |conn| save_cursor(conn, &book_id, Some("token-0"), Some("ctag-0")))
            .await
            .expect("seed cursor");

        let deletion = ContactDelta {
            deleted_remote_ids: vec!["remote-1".into()],
            sync_token: Some("token-1".into()),
            ctag: Some("ctag-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &deletion))
            .await
            .expect("delta");
        assert!(
            skipped,
            "the deletion must be skipped while the edit is queued"
        );
        assert_eq!(
            service.contact_get(&stored.id).await.expect("get").company,
            "Local edit",
            "the queued edit still owns the row"
        );
        let cursor = ctx
            .db
            .read({
                let book_id = book.id.clone();
                move |conn| load_cursor(conn, &book_id)
            })
            .await
            .expect("cursor")
            .expect("cursor row");
        assert_eq!(
            cursor.sync_token.as_deref(),
            Some("token-0"),
            "a skipped batch must not advance the cursor"
        );

        // The upload fails permanently; the local edit is rolled back.
        let op_id = op.id.clone();
        ctx.db
            .write(move |conn| repo::pending_ops::mark_failed(conn, &op_id, "boom"))
            .await
            .expect("mark failed");
        service.rollback(&op).await.expect("rollback");
        assert_eq!(
            service.contact_get(&stored.id).await.expect("get").company,
            "Navy"
        );

        // The next sync (after the failure settled) re-delivers the deletion.
        let replay = ContactDelta {
            deleted_remote_ids: vec!["remote-1".into()],
            sync_token: Some("token-2".into()),
            ctag: Some("ctag-2".into()),
            window_start: Some(Utc::now() + chrono::Duration::seconds(1)),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &replay))
            .await
            .expect("replay");
        assert!(!skipped, "nothing blocks the replayed deletion now");
        let cursor = ctx
            .db
            .read({
                let book_id = book.id.clone();
                move |conn| load_cursor(conn, &book_id)
            })
            .await
            .expect("cursor")
            .expect("cursor row");
        assert_eq!(
            cursor.sync_token.as_deref(),
            Some("token-2"),
            "an applied batch advances the cursor"
        );
        assert!(
            ctx.db
                .read({
                    let book_id = book.id.clone();
                    move |conn| contacts_repo::by_remote_id(conn, &book_id, "remote-1")
                })
                .await
                .expect("query")
                .is_none(),
            "the skipped deletion must be applied after the rollback"
        );
    }

    /// Issue 5: a rollback must not resurrect a row the server already deleted.
    #[tokio::test]
    async fn failed_save_rollback_does_not_reinsert_missing_contact() {
        let (ctx, service, book) = provider_contacts().await;
        let stored = seed_one(&ctx, &book).await;
        let mut edited = stored.clone();
        edited.company = "Failed edit".into();
        service.contact_update(edited).await.expect("update");
        let op = save_op(&ctx).await;

        // A delta deleted the row after the operation settled as failed; the
        // snapshot must not bring it back.
        let id = stored.id.clone();
        ctx.db
            .write(move |conn| contacts_repo::delete_many(conn, &[id]))
            .await
            .expect("delete");
        service.rollback(&op).await.expect("rollback");

        assert!(
            ctx.db
                .read({
                    let id = stored.id.clone();
                    move |conn| contacts_repo::get(conn, &id)
                })
                .await
                .expect("get")
                .is_none(),
            "rollback must not re-insert a contact that no longer exists"
        );
    }

    /// A failed queue insert must keep the optimistic delete and its snapshot
    /// in place: returning an error for a row that is already gone would leave
    /// the contact on the server forever.
    #[tokio::test]
    async fn contact_delete_and_enqueue_commit_atomically() {
        let (ctx, service, book) = provider_contacts().await;
        let stored = seed_one(&ctx, &book).await;
        ctx.db
            .write(|conn| {
                conn.execute_batch(
                    "CREATE TRIGGER block_pending_insert
                     BEFORE INSERT ON pending_op
                     BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
                )?;
                Ok(())
            })
            .await
            .expect("trigger");

        let result = service.contact_delete(vec![stored.id.clone()]).await;
        assert!(result.is_err(), "enqueue failure must fail the delete");

        assert!(
            service.contact_get(&stored.id).await.is_ok(),
            "the local row must roll back with the failed enqueue"
        );
        let ops = ctx
            .db
            .read(|conn| repo::pending_ops::list(conn, Some("dav1")))
            .await
            .expect("ops");
        assert!(ops.is_empty(), "no operation may be left behind");
    }

    /// When a queued delete already removed the local row, a remote deletion
    /// notification has no row to match. It must still be recognised through
    /// the pending snapshot, otherwise the cursor advances and the notification
    /// is lost if the upload later fails and restores the snapshot.
    #[tokio::test]
    async fn deleted_notification_for_already_removed_row_holds_the_cursor() {
        let (ctx, service, book) = provider_contacts().await;
        let stored = seed_one(&ctx, &book).await;
        service
            .contact_delete(vec![stored.id.clone()])
            .await
            .expect("delete");
        assert!(
            service.contact_get(&stored.id).await.is_err(),
            "the pending delete removes the row locally"
        );

        let book_id = book.id.clone();
        ctx.db
            .write(move |conn| save_cursor(conn, &book_id, Some("token-0"), Some("ctag-0")))
            .await
            .expect("seed cursor");

        let deletion = ContactDelta {
            deleted_remote_ids: vec!["remote-1".into()],
            sync_token: Some("token-1".into()),
            ctag: Some("ctag-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &deletion))
            .await
            .expect("delta");
        assert!(
            skipped,
            "the deletion must be skipped even though the local row is gone"
        );

        let book_id = book.id.clone();
        let cursor = ctx
            .db
            .read(move |conn| load_cursor(conn, &book_id))
            .await
            .expect("cursor")
            .expect("cursor exists");
        assert_eq!(
            cursor.sync_token.as_deref(),
            Some("token-0"),
            "the cursor must keep pointing before the skipped deletion"
        );
    }

    /// A create that is still uploading has no remote identity, so a delta
    /// that already lists the new server object cannot match it. Importing it
    /// would create a second row; once the receipt binds the remote id, both
    /// rows would share one identity.
    #[tokio::test]
    async fn delta_defers_import_until_the_create_is_attributed() {
        let (ctx, service, book) = provider_contacts().await;
        let created = service
            .contact_create(contact("dav-book", "New Person", "new@example.com"))
            .await
            .expect("create");

        let mut remote = remote_contact("New Person", "new@example.com", "remote-new", "");
        let href = "https://dav.example.com/carddav/new.vcf".to_string();
        remote.href = Some(href.clone());
        let delta = ContactDelta {
            contacts: vec![remote.clone()],
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("delta");
        assert!(
            skipped,
            "an unattributed create must delay importing the server object"
        );

        let count: i64 = ctx.db.read(contacts_repo::count).await.expect("count");
        assert_eq!(count, 1, "the delta must not insert a duplicate row");
        assert!(service.contact_get(&created.id).await.is_ok());

        // The upload receipt arrives and binds the remote identity.
        let op = save_op(&ctx).await;
        let contact_id = created.id.clone();
        let remote_id = "remote-new".to_string();
        let href_for_db = href.clone();
        ctx.db
            .write(move |conn| {
                conn.execute(
                    "UPDATE contact SET remote_id = ?2, href = ?3, etag = 'etag-2' WHERE id = ?1",
                    rusqlite::params![contact_id, remote_id, href_for_db],
                )?;
                repo::pending_ops::mark_done(conn, &op.id)
            })
            .await
            .expect("bind receipt");

        // A later delta (requested after settlement) updates the same row.
        let replay = ContactDelta {
            contacts: vec![remote],
            sync_token: Some("token-2".into()),
            window_start: Some(Utc::now() + chrono::Duration::seconds(1)),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta(conn, "dav1", &book_for_db, &replay))
            .await
            .expect("replay");
        assert!(!skipped, "nothing blocks the replay after attribution");
        let count: i64 = ctx.db.read(contacts_repo::count).await.expect("count");
        assert_eq!(count, 1, "the replayed delta must update, not duplicate");
        let book_id = book.id.clone();
        let matched = ctx
            .db
            .read(move |conn| contacts_repo::by_remote_id(conn, &book_id, "remote-new"))
            .await
            .expect("lookup")
            .expect("contact");
        assert_eq!(
            matched.id, created.id,
            "the server object must stay bound to the optimistic row"
        );
    }

    struct RecordingProvider {
        /// Provider key used to derive the remote identity (`carddav` can use
        /// the contact id as UID; Graph/Google cannot).
        provider_name: &'static str,
        deleted: Arc<Mutex<Vec<RemoteContactRef>>>,
    }

    impl Default for RecordingProvider {
        fn default() -> Self {
            Self {
                provider_name: "carddav",
                deleted: Arc::default(),
            }
        }
    }

    #[async_trait::async_trait]
    impl ContactProvider for RecordingProvider {
        fn name(&self) -> &'static str {
            self.provider_name
        }

        async fn list_address_books(&self, _account: &Account) -> Result<Vec<RemoteAddressBook>> {
            Ok(Vec::new())
        }

        async fn sync_contacts(
            &self,
            _account: &Account,
            _book: &RemoteAddressBook,
            _cursor: &ContactCursor,
        ) -> Result<ContactDelta> {
            Ok(ContactDelta::default())
        }

        async fn create_contact(
            &self,
            _account: &Account,
            _book: &RemoteAddressBook,
            _contact: &Contact,
            _group_names: &[String],
        ) -> Result<crate::contacts::provider::RemoteWriteResult> {
            Err(AppError::unsupported("test provider"))
        }

        async fn update_contact(
            &self,
            _account: &Account,
            _book: &RemoteAddressBook,
            _current: &RemoteContactRef,
            _contact: &Contact,
            _group_names: &[String],
            _assigned_uid: Option<&str>,
        ) -> Result<crate::contacts::provider::RemoteWriteResult> {
            Err(AppError::unsupported("test provider"))
        }

        async fn delete_contact(
            &self,
            _account: &Account,
            _book: &RemoteAddressBook,
            current: &RemoteContactRef,
        ) -> Result<()> {
            self.deleted
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(current.clone());
            Ok(())
        }
    }

    /// The delete snapshot must be read inside the delete transaction: a
    /// create receipt may land between the caller's read and the commit, and
    /// using the earlier copy would skip the remote deletion entirely.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn contact_delete_snapshots_the_row_inside_the_transaction() {
        let (ctx, service, book) = provider_contacts().await;
        let created = service
            .contact_create(contact(&book.id, "Racy", "racy@example.com"))
            .await
            .expect("create");

        // Park the delete immediately before it takes the write lock. The
        // signal fires only after its caller-side account read has completed,
        // so the receipt lands strictly before the snapshot transaction.
        let (gate, entered) = crate::test_support::WriteGate::new();
        let _gate = crate::test_support::install(created.id.clone(), gate.clone());

        let delete_service = service.clone();
        let delete_id = created.id.clone();
        let deleting =
            tokio::spawn(async move { delete_service.contact_delete(vec![delete_id]).await });

        tokio::time::timeout(std::time::Duration::from_secs(10), entered)
            .await
            .expect("the delete must reach the write gate")
            .expect("write gate sender");

        // The delete is parked, so the create receipt can be committed now.
        // Only an in-transaction snapshot can see its remote id afterwards.
        let holder_id = created.id.clone();
        ctx.db
            .write(move |conn| {
                conn.execute(
                    "UPDATE contact SET remote_id = 'remote-race',
                         href = 'https://dav.example.com/ab/racy.vcf' WHERE id = ?1",
                    rusqlite::params![holder_id],
                )?;
                Ok(())
            })
            .await
            .expect("concurrent create receipt");

        gate.release();
        deleting.await.expect("delete join").expect("delete");
        let ops = ctx
            .db
            .read(|conn| repo::pending_ops::list(conn, Some("dav1")))
            .await
            .expect("ops");
        let delete_op = ops
            .iter()
            .find(|op| op.kind == ops::KIND_DELETE)
            .expect("a remote delete must be queued for the receipt's remote id");
        match ops::decode(delete_op).expect("decode") {
            ContactOp::Delete { contacts } => assert_eq!(
                contacts[0].remote_id.as_deref(),
                Some("remote-race"),
                "the delete must use the row read inside its transaction"
            ),
            other => panic!("unexpected op: {other:?}"),
        }
    }

    /// Deleting a contact whose create is already uploading must persist an
    /// intent: the create receipt later finds no local row, and without the
    /// intent the server object would be orphaned (and re-imported by the next
    /// delta).
    #[tokio::test]
    async fn contact_delete_during_an_inflight_create_records_cleanup_intent() {
        let (ctx, service, book) = provider_contacts().await;
        let created = service
            .contact_create(contact(&book.id, "In Flight", "flight@example.com"))
            .await
            .expect("create");
        // The worker claimed the create and persisted the send intent, so the
        // POST may have reached the provider before the delete commits.
        mark_save_sent(&ctx).await;
        service
            .contact_delete(vec![created.id.clone()])
            .await
            .expect("delete");

        let id = created.id.clone();
        let intent = ctx
            .db
            .read(move |conn| crate::store::contact_delete_intent(conn, &id))
            .await
            .expect("intent read")
            .expect("cleanup intent");
        assert_eq!(intent.address_book_id, book.id);
        assert!(intent.remote_id.is_none());

        let ops = ctx
            .db
            .read(|conn| repo::pending_ops::list(conn, Some("dav1")))
            .await
            .expect("ops");
        assert!(
            !ops.iter().any(|op| op.kind == ops::KIND_DELETE),
            "there is no remote identity to enqueue a Delete for"
        );
    }

    /// While the create of a deleted contact is still settling, its server
    /// object must not be imported back into the (now empty) local book.
    #[tokio::test]
    async fn delta_does_not_reimport_a_contact_deleted_mid_create() {
        let (ctx, service, book) = provider_contacts().await;
        let created = service
            .contact_create(contact(&book.id, "In Flight", "flight@example.com"))
            .await
            .expect("create");
        mark_save_sent(&ctx).await;
        service
            .contact_delete(vec![created.id.clone()])
            .await
            .expect("delete");

        let mut remote = remote_contact("In Flight", "flight@example.com", "remote-flight", "");
        remote.href = Some("https://dav.example.com/ab/flight.vcf".into());
        let delta = ContactDelta {
            contacts: vec![remote],
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("delta");
        assert!(
            !skipped,
            "book-level isolation must not hold the cursor for an identity-less intent"
        );
        let count = ctx.db.read(contacts_repo::count).await.expect("count");
        assert_eq!(count, 0, "the deleted contact must not reappear");
    }

    /// The create receipt of a contact deleted mid-upload must remove the
    /// object it just created and then drop the intent.
    #[tokio::test]
    async fn create_receipt_cleans_up_the_object_of_a_deleted_contact() {
        let (ctx, service, book) = provider_contacts().await;
        let mapping = crate::store::AddressBookRemote::new(
            book.id.clone(),
            "dav1",
            "carddav",
            "https://dav.example.com/ab/",
            Some("https://dav.example.com/ab/".into()),
        );
        ctx.db
            .write(move |conn| crate::store::set_book(conn, &mapping))
            .await
            .expect("book mapping");

        let created = service
            .contact_create(contact(&book.id, "Receipt", "receipt@example.com"))
            .await
            .expect("create");
        mark_save_sent(&ctx).await;
        service
            .contact_delete(vec![created.id.clone()])
            .await
            .expect("delete");

        // The upload's receipt arrives after the delete committed.
        let id = created.id.clone();
        ctx.db
            .write({
                let id = id.clone();
                move |conn| {
                    crate::store::attach_contact_delete_intent(
                        conn,
                        &id,
                        Some("remote-created"),
                        Some("https://dav.example.com/ab/new.vcf"),
                        Some("etag-9"),
                        false,
                    )
                    .map(|_| ())
                }
            })
            .await
            .expect("receipt attached");

        let account = ctx
            .db
            .read(|conn| repo::accounts::get_required(conn, "dav1"))
            .await
            .expect("account");
        let provider = RecordingProvider::default();
        service
            .settle_deleted_contact_create(&account, &provider, &id)
            .await
            .expect("cleanup");

        let deleted = provider
            .deleted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(deleted.len(), 1, "the created object must be removed");
        assert_eq!(deleted[0].remote_id.as_deref(), Some("remote-created"));
        assert_eq!(
            deleted[0].href.as_deref(),
            Some("https://dav.example.com/ab/new.vcf")
        );

        let intent = ctx
            .db
            .read(move |conn| crate::store::contact_delete_intent(conn, &id))
            .await
            .expect("intent read")
            .expect("the settled tombstone must be kept");
        assert_eq!(
            intent.state,
            crate::store::ContactDeleteIntentState::Settled,
            "the cleanup intent must become a suppression tombstone"
        );
        assert_eq!(intent.remote_id.as_deref(), Some("remote-created"));
        assert!(
            intent.suppress_until.is_some(),
            "the tombstone must carry an expiry"
        );
    }

    /// Scenario 1: the create POST committed server-side but its response was
    /// lost, so the intent has no remote identity. Cleanup must not be treated
    /// as done: the intent survives in the `unknown` state, the attempt fails
    /// visibly, and a delta must not import the object back.
    #[tokio::test]
    async fn lost_create_receipt_keeps_the_intent_and_blocks_reimport() {
        let (ctx, service, book) = provider_contacts().await;
        map_book(&ctx, &book).await;
        let contact_id = "lost-receipt";
        ctx.db
            .write({
                let contact_id = contact_id.to_string();
                let book_id = book.id.clone();
                move |conn| {
                    crate::store::record_contact_delete_intent(conn, &contact_id, "dav1", &book_id)
                }
            })
            .await
            .expect("intent");

        let account = ctx
            .db
            .read(|conn| repo::accounts::get_required(conn, "dav1"))
            .await
            .expect("account");
        // Graph/Google cannot derive an identity from the local contact id.
        let provider = RecordingProvider {
            provider_name: "graph",
            deleted: Arc::default(),
        };
        let intent = ctx
            .db
            .read({
                let contact_id = contact_id.to_string();
                move |conn| crate::store::contact_delete_intent(conn, &contact_id)
            })
            .await
            .expect("intent read")
            .expect("intent");
        let result = service
            .settle_contact_delete_intent(&account, &provider, intent)
            .await;
        assert!(
            result.is_err(),
            "a cleanup without any remote identity must not report success"
        );
        assert!(
            provider
                .deleted
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty(),
            "there is no addressable object, so no DELETE may be attempted"
        );

        let intent = ctx
            .db
            .read({
                let contact_id = contact_id.to_string();
                move |conn| crate::store::contact_delete_intent(conn, &contact_id)
            })
            .await
            .expect("intent read")
            .expect("the unknown intent must be kept for reconciliation");
        assert_eq!(
            intent.state,
            crate::store::ContactDeleteIntentState::Unknown
        );

        // No outstanding operation exists (the lost create is not queued), so
        // the protection must come from the intent table alone.
        let mut remote = remote_contact("Lost", "lost@example.com", "remote-lost", "");
        remote.href = Some("https://dav.example.com/ab/lost.vcf".into());
        let delta = ContactDelta {
            contacts: vec![remote],
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("delta");
        assert!(
            !skipped,
            "book-level isolation hides new objects without holding the cursor"
        );
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            0,
            "the deleted contact must not be imported back"
        );
    }

    /// Scenario 2: after the remote DELETE succeeded, a delta batch that was
    /// requested before it landed can still name the object. The settled
    /// tombstone hides it without holding the cursor: the tombstone is durable,
    /// so replaying the batch cannot change the outcome.
    #[tokio::test]
    async fn settled_tombstone_suppresses_a_late_delta_without_holding_the_cursor() {
        let (ctx, service, book) = provider_contacts().await;
        map_book(&ctx, &book).await;
        let contact_id = "late-delta";
        ctx.db
            .write({
                let contact_id = contact_id.to_string();
                let book_id = book.id.clone();
                move |conn| {
                    crate::store::record_contact_delete_intent(conn, &contact_id, "dav1", &book_id)
                }
            })
            .await
            .expect("intent");
        ctx.db
            .write({
                let contact_id = contact_id.to_string();
                move |conn| {
                    crate::store::attach_contact_delete_intent(
                        conn,
                        &contact_id,
                        Some("remote-late"),
                        Some("https://dav.example.com/ab/late.vcf"),
                        Some("etag-late"),
                        false,
                    )
                    .map(|_| ())
                }
            })
            .await
            .expect("receipt");

        let account = ctx
            .db
            .read(|conn| repo::accounts::get_required(conn, "dav1"))
            .await
            .expect("account");
        let provider = RecordingProvider::default();
        let intent = ctx
            .db
            .read({
                let contact_id = contact_id.to_string();
                move |conn| crate::store::contact_delete_intent(conn, &contact_id)
            })
            .await
            .expect("intent read")
            .expect("intent");
        service
            .settle_contact_delete_intent(&account, &provider, intent)
            .await
            .expect("cleanup");

        let book_id = book.id.clone();
        ctx.db
            .write(move |conn| save_cursor(conn, &book_id, Some("token-0"), Some("ctag-0")))
            .await
            .expect("seed cursor");

        let mut remote = remote_contact("Late", "late@example.com", "remote-late", "");
        remote.href = Some("https://dav.example.com/ab/late.vcf".into());
        let delta = ContactDelta {
            contacts: vec![remote],
            sync_token: Some("token-late".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("late delta");
        assert!(
            !skipped,
            "a known tombstone hides its object without holding the cursor"
        );
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            0,
            "the late delta must not resurrect the contact"
        );

        let book_id = book.id.clone();
        let cursor = ctx
            .db
            .read(move |conn| load_cursor(conn, &book_id))
            .await
            .expect("cursor")
            .expect("cursor row");
        assert_eq!(
            cursor.sync_token.as_deref(),
            Some("token-late"),
            "a suppressed object must not stop the cursor from advancing"
        );
    }

    /// Scenario 3: the cleanup DELETE exhausted its retries and the worker no
    /// longer runs the Save. The persisted remote id must still hide the
    /// object, but it must not stall the cursor: the tombstone is durable, so
    /// replaying the batch can never import it.
    #[tokio::test]
    async fn terminal_failed_cleanup_hides_the_object_and_advances_the_cursor() {
        let (ctx, service, book) = provider_contacts().await;
        let created = service
            .contact_create(contact(&book.id, "Terminal", "terminal@example.com"))
            .await
            .expect("create");
        mark_save_sent(&ctx).await;
        service
            .contact_delete(vec![created.id.clone()])
            .await
            .expect("delete");
        let contact_id = created.id.clone();
        ctx.db
            .write({
                let contact_id = contact_id.clone();
                move |conn| {
                    crate::store::attach_contact_delete_intent(
                        conn,
                        &contact_id,
                        Some("remote-terminal"),
                        Some("https://dav.example.com/ab/terminal.vcf"),
                        Some("etag-terminal"),
                        false,
                    )
                    .map(|_| ())
                }
            })
            .await
            .expect("receipt");

        // The cleanup failed permanently: no outstanding Save remains.
        let save = save_op(&ctx).await;
        let op_id = save.id.clone();
        ctx.db
            .write(move |conn| repo::pending_ops::mark_failed(conn, &op_id, "boom"))
            .await
            .expect("mark failed");
        let outstanding = ctx
            .db
            .read(|conn| repo::pending_ops::list(conn, Some("dav1")))
            .await
            .expect("ops")
            .into_iter()
            .filter(repo::pending_ops::is_outstanding)
            .count();
        assert_eq!(outstanding, 0, "the worker must no longer run the Save");
        assert!(
            ctx.db
                .read({
                    let contact_id = contact_id.clone();
                    move |conn| crate::store::contact_delete_intent(conn, &contact_id)
                })
                .await
                .expect("intent")
                .is_some(),
            "the cleanup intent must survive the terminal failure"
        );

        let book_id = book.id.clone();
        ctx.db
            .write(move |conn| save_cursor(conn, &book_id, Some("token-0"), Some("ctag-0")))
            .await
            .expect("seed cursor");

        // Two consecutive batches re-deliver the object. Neither may stall the
        // cursor (the regression stalled every batch at token-0).
        for token in ["token-terminal-1", "token-terminal-2"] {
            let mut remote =
                remote_contact("Terminal", "terminal@example.com", "remote-terminal", "");
            remote.href = Some("https://dav.example.com/ab/terminal.vcf".into());
            let delta = ContactDelta {
                contacts: vec![remote],
                sync_token: Some(token.into()),
                window_start: Some(Utc::now()),
                ..Default::default()
            };
            let book_for_db = book.clone();
            let skipped = ctx
                .db
                .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
                .await
                .expect("delta");
            assert!(
                !skipped,
                "a known tombstone must hide its object without holding the cursor"
            );
            assert_eq!(
                ctx.db.read(contacts_repo::count).await.expect("count"),
                0,
                "the object of a terminal-failed cleanup must not be re-imported"
            );
            let book_id = book.id.clone();
            let cursor = ctx
                .db
                .read(move |conn| load_cursor(conn, &book_id))
                .await
                .expect("cursor")
                .expect("cursor row");
            assert_eq!(
                cursor.sync_token.as_deref(),
                Some(token),
                "the cursor must advance past the suppressed object"
            );
        }
    }

    /// A later Save can still create when the first create failed: it carries
    /// `snapshot: Some` but the row has no remote identity. The duplicate
    /// protection must use the executor's create rule, not the snapshot shape.
    #[tokio::test]
    async fn delta_defers_import_when_a_later_save_will_create() {
        let (ctx, service, book) = provider_contacts().await;
        let created = service
            .contact_create(contact(&book.id, "Late", "late@example.com"))
            .await
            .expect("create");
        let mut edited = created.clone();
        edited.company = "Edited".into();
        service.contact_update(edited).await.expect("update");

        // The first create fails and rolls back; the newer edit survives.
        let first_create = ctx
            .db
            .read(|conn| repo::pending_ops::list(conn, Some("dav1")))
            .await
            .expect("ops")
            .into_iter()
            .find(|op| op.kind == ops::KIND_SAVE)
            .expect("create op");
        let op_id = first_create.id.clone();
        ctx.db
            .write(move |conn| repo::pending_ops::mark_failed(conn, &op_id, "boom"))
            .await
            .expect("mark failed");
        service.rollback(&first_create).await.expect("rollback");
        assert_eq!(
            service.contact_get(&created.id).await.expect("get").company,
            "Edited",
            "the newer edit must survive the failed create rollback"
        );

        let mut remote = remote_contact("Late", "late@example.com", "remote-late", "");
        remote.href = Some("https://dav.example.com/ab/late.vcf".into());
        let delta = ContactDelta {
            contacts: vec![remote],
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("delta");
        assert!(
            skipped,
            "a queued Save that will create must defer importing the server object"
        );
        let count = ctx.db.read(contacts_repo::count).await.expect("count");
        assert_eq!(count, 1, "the delta must not insert a duplicate row");
    }

    /// A full sync cannot list a locally deleted contact (its row is gone).
    /// Its absence must still hold the cursor until the delete settles, or a
    /// failed delete rollback would restore a row the next delta can never
    /// correct.
    #[tokio::test]
    async fn full_sync_holds_cursor_for_a_pending_delete_absent_from_response() {
        let (ctx, service, book) = provider_contacts().await;
        let stored = seed_one(&ctx, &book).await;
        service
            .contact_delete(vec![stored.id.clone()])
            .await
            .expect("delete");

        let book_id = book.id.clone();
        ctx.db
            .write(move |conn| save_cursor(conn, &book_id, Some("token-0"), Some("ctag-0")))
            .await
            .expect("seed cursor");

        let delta = ContactDelta {
            full_sync: true,
            contacts: Vec::new(),
            sync_token: Some("token-1".into()),
            ctag: Some("ctag-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("full sync");
        assert!(
            skipped,
            "a pending delete missing from a full response must hold the cursor"
        );

        let book_id = book.id.clone();
        let cursor = ctx
            .db
            .read(move |conn| load_cursor(conn, &book_id))
            .await
            .expect("cursor")
            .expect("cursor row");
        assert_eq!(
            cursor.sync_token.as_deref(),
            Some("token-0"),
            "the cursor must not advance past the pending delete"
        );
    }

    /// Regression 1: deleting a contact whose create was never sent (the worker
    /// has not marked a send intent) must cancel the queued Save instead of
    /// leaving an unaddressable cleanup intent. Otherwise every later contact
    /// of the book is blocked behind an `unknown` intent forever.
    #[tokio::test]
    async fn deleting_an_unsent_create_cancels_the_save_and_does_not_block_the_book() {
        let (ctx, service, book) = provider_contacts().await;
        let created = service
            .contact_create(contact(&book.id, "Never Sent", "never@example.com"))
            .await
            .expect("create");
        let contact_id = created.id.clone();
        service
            .contact_delete(vec![contact_id.clone()])
            .await
            .expect("delete");

        let intent = ctx
            .db
            .read({
                let contact_id = contact_id.clone();
                move |conn| crate::store::contact_delete_intent(conn, &contact_id)
            })
            .await
            .expect("intent read");
        assert!(
            intent.is_none(),
            "a provably unsent create needs no cleanup intent"
        );

        let ops = ctx
            .db
            .read(|conn| repo::pending_ops::list(conn, Some("dav1")))
            .await
            .expect("ops");
        let save = ops
            .iter()
            .find(|op| op.kind == ops::KIND_SAVE)
            .expect("save op");
        assert_eq!(
            save.state,
            nuncio_core::models::PendingOpState::Cancelled,
            "the pending create must be cancelled, not retried"
        );
        assert!(
            !repo::pending_ops::is_outstanding(save),
            "no create may keep running after the local row was deleted"
        );

        // An unrelated new contact in the same book must import normally and
        // the cursor must advance.
        let book_id = book.id.clone();
        ctx.db
            .write(move |conn| save_cursor(conn, &book_id, Some("token-0"), Some("ctag-0")))
            .await
            .expect("seed cursor");
        let delta = ContactDelta {
            contacts: vec![remote_contact(
                "Next Person",
                "next@example.com",
                "remote-next",
                "",
            )],
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("delta");
        assert!(
            !skipped,
            "a cancelled unsent create must not defer the whole book"
        );
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            1,
            "the unrelated contact must be imported"
        );
        let book_id = book.id.clone();
        let cursor = ctx
            .db
            .read(move |conn| load_cursor(conn, &book_id))
            .await
            .expect("cursor")
            .expect("cursor row");
        assert_eq!(
            cursor.sync_token.as_deref(),
            Some("token-1"),
            "the cursor must advance"
        );
    }

    /// Regression 1 (claim race): the worker claimed the create but has not
    /// persisted a send intent yet. The delete still runs before any POST, so
    /// cancelling it is correct and no intent may be recorded.
    #[tokio::test]
    async fn deleting_a_claimed_but_unsent_create_records_no_intent() {
        let (ctx, service, book) = provider_contacts().await;
        let created = service
            .contact_create(contact(&book.id, "Claimed", "claimed@example.com"))
            .await
            .expect("create");
        let op = save_op(&ctx).await;
        let op_id = op.id.clone();
        ctx.db
            .write(move |conn| repo::pending_ops::mark_in_progress(conn, &op_id))
            .await
            .expect("claim");
        let contact_id = created.id.clone();
        service
            .contact_delete(vec![contact_id.clone()])
            .await
            .expect("delete");

        assert!(
            ctx.db
                .read({
                    let contact_id = contact_id.clone();
                    move |conn| crate::store::contact_delete_intent(conn, &contact_id)
                })
                .await
                .expect("intent read")
                .is_none(),
            "an inProgress operation without a send intent never reached the wire"
        );
        let stored = ctx
            .db
            .read({
                let op_id = op.id.clone();
                move |conn| repo::pending_ops::get_required(conn, &op_id)
            })
            .await
            .expect("op");
        assert_eq!(
            stored.state,
            nuncio_core::models::PendingOpState::Cancelled,
            "the claimed but unsent create must be cancelled"
        );
        assert!(!stored.send_intent);
    }

    /// Regression 3: CardDAV's remote id is the vCard UID, which is only
    /// unique inside one address book. A tombstone in book A must not hide a
    /// different copy with the same UID in book B (nor hold B's cursor).
    #[tokio::test]
    async fn delete_tombstone_in_one_book_does_not_block_the_same_uid_in_another_book() {
        let (ctx, _service, book_a) = provider_contacts().await;
        let book_b = AddressBook {
            id: "dav-book-b".into(),
            account_id: "dav1".into(),
            name: "Other".into(),
            is_read_only: false,
            is_default: false,
        };
        ctx.db
            .write({
                let mut book_b = book_b.clone();
                move |conn| repo::address_books::insert(conn, &mut book_b)
            })
            .await
            .expect("book b");

        // A settled tombstone in book A for UID `shared-uid`.
        let contact_id = "deleted-in-a";
        ctx.db
            .write({
                let contact_id = contact_id.to_string();
                let book_a = book_a.id.clone();
                move |conn| {
                    crate::store::record_contact_delete_intent(conn, &contact_id, "dav1", &book_a)
                }
            })
            .await
            .expect("intent");
        let suppress_until = Utc::now()
            + chrono::Duration::seconds(crate::store::CONTACT_DELETE_TOMBSTONE_TTL_SECONDS);
        ctx.db
            .write({
                let contact_id = contact_id.to_string();
                move |conn| {
                    crate::store::settle_contact_delete_intent(
                        conn,
                        &contact_id,
                        Some("shared-uid"),
                        Some("https://dav.example.com/ab-a/shared.vcf"),
                        suppress_until,
                        false,
                    )
                }
            })
            .await
            .expect("settle tombstone");

        let book_id = book_b.id.clone();
        ctx.db
            .write(move |conn| save_cursor(conn, &book_id, Some("token-0"), Some("ctag-0")))
            .await
            .expect("seed cursor");

        let mut copy = remote_contact("Copy", "copy@example.com", "shared-uid", "");
        copy.href = Some("https://dav.example.com/ab-b/shared.vcf".into());
        let delta = ContactDelta {
            contacts: vec![copy],
            sync_token: Some("token-b".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book_b.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("delta b");
        assert!(
            !skipped,
            "book A's tombstone must not suppress book B's UID"
        );
        let book_b_id = book_b.id.clone();
        assert!(
            ctx.db
                .read(move |conn| contacts_repo::by_remote_id(conn, &book_b_id, "shared-uid"))
                .await
                .expect("lookup")
                .is_some(),
            "the same-UID contact in book B must be imported"
        );
        let book_id = book_b.id.clone();
        let cursor = ctx
            .db
            .read(move |conn| load_cursor(conn, &book_id))
            .await
            .expect("cursor")
            .expect("cursor row");
        assert_eq!(
            cursor.sync_token.as_deref(),
            Some("token-b"),
            "book B's cursor must advance"
        );

        // Sanity: book A's own delta still hides the tombstoned object.
        let mut deleted = remote_contact("Deleted", "deleted@example.com", "shared-uid", "");
        deleted.href = Some("https://dav.example.com/ab-a/shared.vcf".into());
        let delta_a = ContactDelta {
            contacts: vec![deleted],
            sync_token: Some("token-a".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_a_for_db = book_a.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta(conn, "dav1", &book_a_for_db, &delta_a))
            .await
            .expect("delta a");
        assert!(!skipped, "the tombstoned object is hidden, not replayed");
        let book_a_id = book_a.id.clone();
        assert!(
            ctx.db
                .read(move |conn| contacts_repo::by_remote_id(conn, &book_a_id, "shared-uid"))
                .await
                .expect("lookup")
                .is_none(),
            "book A must not import its own deleted contact"
        );
    }

    /// Regression 1: a Save cancelled by the delete must not be resurrected by
    /// a worker holding an older snapshot, and it must not delete the intent
    /// that belongs to the create which may already be on the wire.
    #[tokio::test]
    async fn cancelled_save_does_not_execute_or_delete_another_operations_intent() {
        let (ctx, service, book) = provider_contacts().await;
        let created = service
            .contact_create(contact(&book.id, "Race", "race@example.com"))
            .await
            .expect("create");
        // Op A was claimed and may have reached the provider.
        mark_save_sent(&ctx).await;

        let mut edited = created.clone();
        edited.company = "Edited".into();
        service.contact_update(edited).await.expect("update"); // op B, unsent

        let ops = ctx
            .db
            .read(|conn| repo::pending_ops::list(conn, Some("dav1")))
            .await
            .expect("ops");
        let sent = ops
            .iter()
            .find(|op| op.kind == ops::KIND_SAVE && op.send_intent)
            .expect("sent save")
            .clone();
        let stale = ops
            .iter()
            .find(|op| op.kind == ops::KIND_SAVE && !op.send_intent)
            .expect("unsent save")
            .clone();

        service
            .contact_delete(vec![created.id.clone()])
            .await
            .expect("delete");

        let id = created.id.clone();
        let intent = ctx
            .db
            .read(move |conn| crate::store::contact_delete_intent(conn, &id))
            .await
            .expect("intent read")
            .expect("cleanup intent");
        assert_eq!(intent.owner_op_id.as_deref(), Some(sent.id.as_str()));
        assert_eq!(intent.owner_attempt, Some(sent.attempts));

        // The worker's claim is conditional: a cancelled row stays cancelled.
        let stale_id = stale.id.clone();
        let claimed = ctx
            .db
            .write(move |conn| repo::pending_ops::claim_for_execution(conn, &stale_id))
            .await
            .expect("claim");
        assert_eq!(
            claimed, 0,
            "a cancelled operation must not be claimed again"
        );

        // Even if an old snapshot reaches the executor, it must neither run
        // nor modify the intent recorded for the other operation.
        service
            .execute(&stale)
            .await
            .expect("a cancelled operation is a no-op");

        let stale_id = stale.id.clone();
        let stored = ctx
            .db
            .read(move |conn| repo::pending_ops::get_required(conn, &stale_id))
            .await
            .expect("op");
        assert_eq!(stored.state, PendingOpState::Cancelled);
        assert!(!stored.send_intent);

        let id = created.id.clone();
        let intent = ctx
            .db
            .read(move |conn| crate::store::contact_delete_intent(conn, &id))
            .await
            .expect("intent read")
            .expect("the other operation's intent must survive");
        assert_eq!(
            intent.state,
            crate::store::ContactDeleteIntentState::Pending
        );
        assert_eq!(intent.owner_op_id.as_deref(), Some(sent.id.as_str()));
    }

    /// Regression 2: a terminal failure is not proof that nothing was sent.
    /// Deleting after the failed create must still record an intent, and a
    /// delta carrying the possibly-created object must not import it back.
    #[tokio::test]
    async fn delete_after_a_failed_create_records_an_intent_and_blocks_reimport() {
        let (ctx, service, book) = provider_contacts().await;
        let created = service
            .contact_create(contact(&book.id, "Failed", "failed@example.com"))
            .await
            .expect("create");
        mark_save_sent(&ctx).await;
        let sent = save_op(&ctx).await;
        let sent_id = sent.id.clone();
        ctx.db
            .write(move |conn| repo::pending_ops::mark_failed(conn, &sent_id, "receipt lost"))
            .await
            .expect("mark failed");

        let mut edited = created.clone();
        edited.company = "Edited".into();
        service.contact_update(edited).await.expect("update");

        service
            .contact_delete(vec![created.id.clone()])
            .await
            .expect("delete");

        let id = created.id.clone();
        let intent = ctx
            .db
            .read(move |conn| crate::store::contact_delete_intent(conn, &id))
            .await
            .expect("intent read")
            .expect("a failed create with a send intent still needs cleanup");
        assert_eq!(intent.owner_op_id.as_deref(), Some(sent.id.as_str()));
        assert!(
            intent.remote_id.is_none(),
            "the lost receipt left no addressable identity"
        );

        let mut remote = remote_contact("Failed", "failed@example.com", "remote-failed", "");
        remote.href = Some("https://dav.example.com/ab/failed.vcf".into());
        let delta = ContactDelta {
            contacts: vec![remote],
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("delta");
        assert!(
            !skipped,
            "the unaddressable intent isolates the book without holding the cursor"
        );
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            0,
            "the possibly-created object must not be re-imported"
        );
    }

    /// Regression 3: a reused CardDAV href with a different UID is a different
    /// vCard. The old tombstone must not consume it (and then let it expire).
    #[tokio::test]
    async fn reused_href_with_a_different_uid_is_imported() {
        let (ctx, _service, book) = provider_contacts().await;
        let deleted_id = "deleted-uid";
        let old_href = "https://dav.example.com/ab/reused.vcf";
        ctx.db
            .write({
                let book_id = book.id.clone();
                move |conn| {
                    crate::store::record_contact_delete_intent(conn, deleted_id, "dav1", &book_id)?;
                    crate::store::settle_contact_delete_intent(
                        conn,
                        deleted_id,
                        Some("old-uid"),
                        Some(old_href),
                        Utc::now()
                            + chrono::Duration::seconds(
                                crate::store::CONTACT_DELETE_TOMBSTONE_TTL_SECONDS,
                            ),
                        false,
                    )
                }
            })
            .await
            .expect("settled tombstone");

        let book_id = book.id.clone();
        ctx.db
            .write(move |conn| save_cursor(conn, &book_id, Some("token-0"), Some("ctag-0")))
            .await
            .expect("seed cursor");

        let mut replacement = remote_contact("Replacement", "new@example.com", "new-uid", "");
        replacement.href = Some(old_href.into());
        let delta = ContactDelta {
            contacts: vec![replacement],
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("delta");
        assert!(
            !skipped,
            "a different UID at the old href is a new contact, not the tombstone"
        );
        let book_id = book.id.clone();
        assert!(
            ctx.db
                .read(move |conn| contacts_repo::by_remote_id(conn, &book_id, "new-uid"))
                .await
                .expect("lookup")
                .is_some(),
            "the new contact must be imported"
        );
        let book_id = book.id.clone();
        let cursor = ctx
            .db
            .read(move |conn| load_cursor(conn, &book_id))
            .await
            .expect("cursor")
            .expect("cursor row");
        assert_eq!(cursor.sync_token.as_deref(), Some("token-1"));
    }

    /// Regression: a UID-less CardDAV contact is identified by its href. Its
    /// derived remote id must use the normalized spelling; otherwise a
    /// tombstone recorded for `%41.vcf` is bypassed when the provider
    /// re-delivers `A.vcf` and the deleted contact is imported again.
    #[tokio::test]
    async fn no_uid_tombstone_survives_equivalent_href_spellings() {
        let (ctx, _service, book) = provider_contacts().await;
        let encoded = "https://dav.example.com/ab/%41.vcf";
        let plain = "https://dav.example.com/ab/A.vcf";
        let deleted_id = "deleted-no-uid";
        // A tombstone persisted by an older build derived the placeholder from
        // the *raw* spelling (`%41.vcf`). It must still match the UID-less card
        // re-delivered as `A.vcf`, because both spellings are one identity.
        let old_tombstone_id = "%41.vcf";
        assert_eq!(
            crate::contacts::dav::contact_remote_id("", encoded),
            "A.vcf",
            "new derived ids use the normalized href"
        );
        ctx.db
            .write({
                let book_id = book.id.clone();
                let remote_id = old_tombstone_id.to_string();
                move |conn| {
                    crate::store::record_contact_delete_intent(conn, deleted_id, "dav1", &book_id)?;
                    crate::store::settle_contact_delete_intent(
                        conn,
                        deleted_id,
                        Some(&remote_id),
                        Some(encoded),
                        Utc::now()
                            + chrono::Duration::seconds(
                                crate::store::CONTACT_DELETE_TOMBSTONE_TTL_SECONDS,
                            ),
                        true,
                    )
                }
            })
            .await
            .expect("settled tombstone");

        let book_id = book.id.clone();
        ctx.db
            .write(move |conn| save_cursor(conn, &book_id, Some("token-0"), Some("ctag-0")))
            .await
            .expect("seed cursor");

        // The provider re-delivers the same resource under the other spelling.
        let incoming_id = crate::contacts::dav::contact_remote_id("", plain);
        let mut remote = remote_contact("No UID", "no-uid@example.com", &incoming_id, "");
        remote.href = Some(plain.into());
        remote.uid_is_derived = true;
        let delta = ContactDelta {
            contacts: vec![remote],
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("delta");
        assert!(
            !skipped,
            "a durable tombstone hides its object without holding the cursor"
        );
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            0,
            "an equivalent spelling must not bypass the delete tombstone"
        );
    }

    /// Regression ② end-to-end: a derived tombstone (`A.vcf` from a UID-less
    /// card) must not swallow a new card whose *real* UID is `A.vcf`.
    #[tokio::test]
    async fn real_uid_card_is_not_swallowed_by_a_derived_tombstone() {
        let (ctx, _service, book) = provider_contacts().await;
        let encoded = "https://dav.example.com/ab/%41.vcf";
        let plain = "https://dav.example.com/ab/A.vcf";
        let deleted_id = "deleted-no-uid-2";
        ctx.db
            .write({
                let book_id = book.id.clone();
                move |conn| {
                    crate::store::record_contact_delete_intent(conn, deleted_id, "dav1", &book_id)?;
                    crate::store::settle_contact_delete_intent(
                        conn,
                        deleted_id,
                        Some("A.vcf"),
                        Some(encoded),
                        Utc::now()
                            + chrono::Duration::seconds(
                                crate::store::CONTACT_DELETE_TOMBSTONE_TTL_SECONDS,
                            ),
                        // The deleted card had no UID; its id was href-derived.
                        true,
                    )
                }
            })
            .await
            .expect("settled tombstone");

        let book_id = book.id.clone();
        ctx.db
            .write(move |conn| save_cursor(conn, &book_id, Some("token-0"), Some("ctag-0")))
            .await
            .expect("seed cursor");

        // A new vCard at the same resource URL with the real UID `A.vcf`.
        let mut replacement = remote_contact("Replacement", "new@example.com", "A.vcf", "");
        replacement.href = Some(plain.into());
        assert!(!replacement.uid_is_derived);
        let delta = ContactDelta {
            contacts: vec![replacement],
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("delta");
        assert!(
            !skipped,
            "a real UID is a different identity, not an ambiguous match"
        );
        let book_id = book.id.clone();
        assert!(
            ctx.db
                .read(move |conn| contacts_repo::by_remote_id(conn, &book_id, "A.vcf"))
                .await
                .expect("lookup")
                .is_some(),
            "the new card with the real UID must be imported"
        );
        assert_eq!(quarantine_count(&ctx).await, 0);
        let book_id = book.id.clone();
        let cursor = ctx
            .db
            .read(move |conn| load_cursor(conn, &book_id))
            .await
            .expect("cursor")
            .expect("cursor row");
        assert_eq!(cursor.sync_token.as_deref(), Some("token-1"));
    }

    /// Regression ① end-to-end: a resource delivered as UID X -> Y -> X inside
    /// the same second must replay in write order, so the released local row is
    /// the newest X, not the stale Y.
    #[tokio::test]
    async fn quarantine_release_applies_same_second_uid_changes_in_write_order() {
        let (ctx, _service, book) = provider_contacts().await;
        let href_plain = "https://dav.example.com/ab/A.vcf";
        let href_encoded = "https://dav.example.com/ab/%41.vcf";

        let build = |name: &str, remote_id: &str, href: &str| {
            let mut remote =
                remote_contact(name, &format!("{remote_id}@example.com"), remote_id, "");
            remote.href = Some(href.into());
            remote
        };
        let snapshots = [
            build("X-old", "uid-x", href_plain),
            build("Y", "uid-y", href_encoded),
            build("X-new", "uid-x", href_plain),
        ];
        let account_id = book.account_id.clone();
        let book_id = book.id.clone();
        ctx.db
            .write(move |conn| {
                for remote in &snapshots {
                    let payload =
                        serde_json::to_string(&QuarantinedRemoteContact::from_remote(remote))
                            .expect("serialize snapshot");
                    crate::store::quarantine_contact(
                        conn,
                        &account_id,
                        &book_id,
                        remote.remote_id.as_deref(),
                        remote.href.as_deref(),
                        &payload,
                    )?;
                }
                Ok(())
            })
            .await
            .expect("quarantine snapshots");
        assert_eq!(quarantine_count(&ctx).await, 2);

        // An empty batch triggers the quarantine replay on release.
        let book_for_db = book.clone();
        ctx.db
            .write(move |conn| {
                apply_delta_and_cursor(conn, "dav1", &book_for_db, &ContactDelta::default())
            })
            .await
            .expect("replay");

        let book_id = book.id.clone();
        let page = ctx
            .db
            .read(move |conn| contacts_repo::list(conn, &list_query_for_book(&book_id)))
            .await
            .expect("contacts");
        assert_eq!(page.items.len(), 1, "the equivalent href is one contact");
        assert_eq!(
            page.items[0].display_name, "X-new",
            "the newest X must win over the stale Y"
        );
        let contact_id = page.items[0].id.clone();
        let meta = ctx
            .db
            .read(move |conn| contacts_repo::sync_meta(conn, &contact_id))
            .await
            .expect("meta")
            .expect("sync meta");
        assert_eq!(meta.remote_id.as_deref(), Some("uid-x"));
    }

    /// Regression ①: cleanup driven by a *derived* tombstone must not delete a
    /// replacement card whose real UID happens to equal the derived
    /// placeholder at the same href. Identity kind is compared, not only the
    /// string values.
    #[tokio::test]
    async fn derived_tombstone_cleanup_keeps_a_real_uid_replacement() {
        let (ctx, _service, book) = provider_contacts().await;
        let href = "https://dav.example.com/ab/A.vcf";
        let deleted_id = "deleted-derived-cleanup";
        ctx.db
            .write({
                let book_id = book.id.clone();
                move |conn| {
                    // The replacement vCard: same URL, real UID `A.vcf`.
                    let mut replacement = Contact::new("replacement-real", &book_id, "dav1");
                    replacement.display_name = "Replacement".into();
                    contacts_repo::upsert_remote(
                        conn,
                        &replacement,
                        Some("e-new"),
                        Some("A.vcf"),
                        Some(href),
                        None,
                    )?;
                    // The deleted card carried no UID; its identity was
                    // derived from the href and is not a real UID.
                    crate::store::record_contact_delete_intent(conn, deleted_id, "dav1", &book_id)?;
                    crate::store::settle_contact_delete_intent(
                        conn,
                        deleted_id,
                        Some("A.vcf"),
                        Some(href),
                        Utc::now()
                            + chrono::Duration::seconds(
                                crate::store::CONTACT_DELETE_TOMBSTONE_TTL_SECONDS,
                            ),
                        true,
                    )
                }
            })
            .await
            .expect("settled derived tombstone");

        // The UID-less card is re-delivered at the same resource: the derived
        // tombstone hides it and runs its cleanup. Only rows of the derived
        // kind may be deleted.
        let mut uid_less = remote_contact("No UID", "no-uid@example.com", "A.vcf", "");
        uid_less.href = Some(href.into());
        uid_less.uid_is_derived = true;
        let delta = ContactDelta {
            contacts: vec![uid_less],
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("delta");
        assert!(
            !skipped,
            "a definite tombstone match never holds the cursor"
        );
        assert!(
            ctx.db
                .read(|conn| contacts_repo::get(conn, "replacement-real"))
                .await
                .expect("lookup")
                .is_some(),
            "a real-UID replacement at the same href must survive derived cleanup"
        );
        assert_eq!(ctx.db.read(contacts_repo::count).await.expect("count"), 1);
    }

    /// Regression ②: a real UID and a derived placeholder with the same string
    /// but *different* hrefs are different objects. The upsert must insert the
    /// UID-less card instead of merging it into the real-UID row.
    #[tokio::test]
    async fn real_uid_and_derived_identity_with_different_hrefs_are_not_merged() {
        let (ctx, _service, book) = provider_contacts().await;
        let real_href = "https://dav.example.com/ab/other/A.vcf";
        let derived_href = "https://dav.example.com/ab/A.vcf";
        ctx.db
            .write({
                let book_id = book.id.clone();
                move |conn| {
                    let mut real = Contact::new("real-uid-card", &book_id, "dav1");
                    real.display_name = "Real".into();
                    real.company = "Keep".into();
                    contacts_repo::upsert_remote(
                        conn,
                        &real,
                        Some("e-real"),
                        Some("A.vcf"),
                        Some(real_href),
                        None,
                    )
                }
            })
            .await
            .expect("real-uid row");

        let mut derived = remote_contact("Derived", "derived@example.com", "A.vcf", "Derived");
        derived.href = Some(derived_href.into());
        derived.uid_is_derived = true;
        let delta = ContactDelta {
            contacts: vec![derived],
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        ctx.db
            .write(move |conn| apply_delta(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("delta");

        let (total, real_company, derived_rows): (i64, String, i64) = ctx
            .db
            .read(move |conn| {
                let total = conn.query_row("SELECT COUNT(*) FROM contact", [], |row| row.get(0))?;
                let real_company = conn.query_row(
                    "SELECT company FROM contact WHERE id = 'real-uid-card'",
                    [],
                    |row| row.get(0),
                )?;
                let derived_rows = conn.query_row(
                    "SELECT COUNT(*) FROM contact WHERE href = ?1 AND uid_is_derived = 1",
                    rusqlite::params![derived_href],
                    |row| row.get(0),
                )?;
                Ok((total, real_company, derived_rows))
            })
            .await
            .expect("rows");
        assert_eq!(total, 2, "the two identities must not merge");
        assert_eq!(
            real_company, "Keep",
            "the real-UID row must not be overwritten"
        );
        assert_eq!(derived_rows, 1, "the derived card needs its own row");
    }

    /// After the derived card's PUT writes the derived id back as its UID
    /// (RFC 6352 §5.1), a later sync delivers that value as a **real** UID.
    /// The href resource key must still keep it separate from a genuine
    /// same-named UID at another href; matching by the bare UID would let the
    /// two cards overwrite each other.
    #[tokio::test]
    async fn derived_href_and_same_named_real_uid_stay_separate_across_syncs() {
        let (ctx, _service, book) = provider_contacts().await;
        let derived_href = "https://dav.example.com/ab/A.vcf";
        let real_href = "https://dav.example.com/ab/other/A.vcf";
        ctx.db
            .write({
                let book_id = book.id.clone();
                move |conn| {
                    let mut derived = Contact::new("derived-local", &book_id, "dav1");
                    derived.display_name = "Derived Old".into();
                    derived.company = "Old".into();
                    contacts_repo::upsert_remote(
                        conn,
                        &derived,
                        Some("e-derived"),
                        Some("A.vcf"),
                        Some(derived_href),
                        None,
                    )?;
                    conn.execute(
                        "UPDATE contact SET uid_is_derived = 1 WHERE id = 'derived-local'",
                        [],
                    )?;
                    let mut real = Contact::new("real-local", &book_id, "dav1");
                    real.display_name = "Real Keep".into();
                    real.company = "Keep".into();
                    contacts_repo::upsert_remote(
                        conn,
                        &real,
                        Some("e-real"),
                        Some("A.vcf"),
                        Some(real_href),
                        None,
                    )
                }
            })
            .await
            .expect("seed rows");

        // Both resources now report the same real UID. Only the href tells
        // them apart.
        let mut real = remote_contact("Real Keep", "real@example.com", "A.vcf", "Keep");
        real.href = Some(real_href.into());
        let mut derived = remote_contact("Derived New", "derived@example.com", "A.vcf", "After");
        derived.href = Some(derived_href.into());
        assert!(!derived.uid_is_derived);
        let delta = ContactDelta {
            contacts: vec![real, derived],
            ..Default::default()
        };
        let book_for_db = book.clone();
        ctx.db
            .write(move |conn| apply_delta(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("delta");

        let (total, derived_row, real_row): (i64, (String, String), (String, String)) = ctx
            .db
            .read(|conn| {
                let total = conn.query_row("SELECT COUNT(*) FROM contact", [], |row| row.get(0))?;
                let derived_row = conn.query_row(
                    "SELECT company, href FROM contact WHERE id = 'derived-local'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                let real_row = conn.query_row(
                    "SELECT company, href FROM contact WHERE id = 'real-local'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                Ok((total, derived_row, real_row))
            })
            .await
            .expect("rows");
        assert_eq!(total, 2, "the two hrefs must stay two rows");
        assert_eq!(
            derived_row,
            ("After".to_string(), derived_href.to_string()),
            "the card at the derived href must receive its own update"
        );
        assert_eq!(
            real_row,
            ("Keep".to_string(), real_href.to_string()),
            "the genuine same-named UID at another href must stay untouched"
        );
    }

    /// Regression ① (kind transition): the same href delivered first with a
    /// real UID and then without one is **one** resource. The second delivery
    /// must update the existing row and rewrite `uid_is_derived` instead of
    /// inserting a second row; a full sync must keep exactly one row, and the
    /// eventual `404` must remove it completely.
    #[tokio::test]
    async fn same_href_real_uid_then_no_uid_keeps_one_row_and_404_removes_it() {
        let (ctx, _service, book) = provider_contacts().await;
        let href = "https://dav.example.com/ab/A.vcf";

        let mut real = remote_contact("Real", "real@example.com", "uid-real", "Real");
        real.href = Some(href.into());
        seed(&ctx, &book, vec![real]).await;
        assert_eq!(ctx.db.read(contacts_repo::count).await.expect("count"), 1);

        // The provider replaces the vCard with one that has no UID. Its
        // derived id (`A.vcf`, the href basename) is a different kind.
        let mut uid_less = remote_contact("No UID", "no-uid@example.com", "A.vcf", "No UID");
        uid_less.href = Some(href.into());
        uid_less.uid_is_derived = true;
        uid_less.etag = Some("etag-2".into());
        let delta = ContactDelta {
            contacts: vec![uid_less],
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        ctx.db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("kind transition delta");
        let (total, remote_id, derived): (i64, String, bool) = ctx
            .db
            .read(move |conn| {
                let total = conn.query_row("SELECT COUNT(*) FROM contact", [], |row| row.get(0))?;
                let (remote_id, derived): (String, bool) = conn.query_row(
                    "SELECT remote_id, uid_is_derived FROM contact WHERE href = ?1",
                    rusqlite::params![href],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                Ok((total, remote_id, derived))
            })
            .await
            .expect("rows");
        assert_eq!(total, 1, "the same href is one resource, not a second row");
        assert_eq!(remote_id, "A.vcf");
        assert!(derived, "the row must adopt the incoming identity kind");

        // A provider full listing only re-delivers changed ETags, but the
        // listing still names the href: the single row must survive.
        let full = ContactDelta {
            full_sync: true,
            listed_hrefs: vec![href.into()],
            sync_token: Some("token-2".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &full))
            .await
            .expect("full sync");
        assert!(!skipped);
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            1,
            "a full sync keeps the single row"
        );

        // The server later deletes the resource. The href `404` must leave no
        // row behind (an equivalent spelling is accepted too).
        let deletion = ContactDelta {
            deleted_hrefs: vec!["https://dav.example.com/ab/%41.vcf".into()],
            sync_token: Some("token-3".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &deletion))
            .await
            .expect("404");
        assert!(!skipped);
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            0,
            "the 404 removes the resource completely"
        );
    }

    /// Regression ② (reverse kind transition): the same href delivered first
    /// without a UID and then with a real one must also stay one row, adopting
    /// the real identity.
    #[tokio::test]
    async fn same_href_no_uid_then_real_uid_keeps_one_row() {
        let (ctx, _service, book) = provider_contacts().await;
        let href = "https://dav.example.com/ab/A.vcf";

        let mut uid_less = remote_contact("No UID", "no-uid@example.com", "A.vcf", "No UID");
        uid_less.href = Some(href.into());
        uid_less.uid_is_derived = true;
        seed(&ctx, &book, vec![uid_less]).await;

        let mut real = remote_contact("Real", "real@example.com", "uid-real", "Real");
        real.href = Some(href.into());
        real.etag = Some("etag-2".into());
        let delta = ContactDelta {
            contacts: vec![real],
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        ctx.db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("kind transition delta");

        let (total, remote_id, derived): (i64, String, bool) = ctx
            .db
            .read(move |conn| {
                let total = conn.query_row("SELECT COUNT(*) FROM contact", [], |row| row.get(0))?;
                let (remote_id, derived): (String, bool) = conn.query_row(
                    "SELECT remote_id, uid_is_derived FROM contact WHERE href = ?1",
                    rusqlite::params![href],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                Ok((total, remote_id, derived))
            })
            .await
            .expect("rows");
        assert_eq!(total, 1, "the same href is one resource");
        assert_eq!(remote_id, "uid-real");
        assert!(!derived, "the row must adopt the real-UID kind");
    }

    /// Regression ④: a provider href deletion must remove **every** local row
    /// at that resource, including the two kinds of identity a database written
    /// by an earlier build can hold (a real-UID row plus a UID-less one). The
    /// deletion is delivered once; a leaked row would stay forever.
    #[tokio::test]
    async fn href_delete_removes_local_rows_of_both_identity_kinds() {
        let (ctx, _service, book) = provider_contacts().await;
        let href = "https://dav.example.com/ab/A.vcf";
        ctx.db
            .write({
                let book_id = book.id.clone();
                move |conn| {
                    let mut real = Contact::new("legacy-real", &book_id, "dav1");
                    real.display_name = "Legacy real".into();
                    contacts_repo::upsert_remote(
                        conn,
                        &real,
                        Some("e-real"),
                        Some("uid-legacy"),
                        Some(href),
                        None,
                    )?;
                    let mut uid_less = Contact::new("legacy-derived", &book_id, "dav1");
                    uid_less.display_name = "Legacy derived".into();
                    contacts_repo::upsert_remote(
                        conn,
                        &uid_less,
                        Some("e-derived"),
                        Some("A.vcf"),
                        Some(href),
                        None,
                    )?;
                    set_contact_uid_is_derived(conn, "legacy-derived", true)
                }
            })
            .await
            .expect("legacy duplicate rows");
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            2,
            "the fixture holds both identity kinds at one href"
        );

        let deletion = ContactDelta {
            deleted_hrefs: vec!["https://dav.example.com/ab/%41.vcf".into()],
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &deletion))
            .await
            .expect("href delete");
        assert!(!skipped);
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            0,
            "both rows at the normalized href must be deleted"
        );
    }

    /// Regression ③ (replay eviction): a real-UID snapshot at href `B` whose
    /// UID happens to spell a UID-less card's derived placeholder at href `A`
    /// is a **different object**. The replay must keep and apply it; matching
    /// on the bare remote id would delete the only copy after the cursor has
    /// already advanced (unrecoverable).
    #[tokio::test]
    async fn replay_keeps_a_real_uid_snapshot_that_matches_another_cards_placeholder() {
        let (ctx, _service, book) = provider_contacts().await;
        let real_href = "https://dav.example.com/ab/other/B.vcf";
        let derived_href = "https://dav.example.com/ab/A.vcf";

        // Quarantine a real-UID card whose UID literally is `A.vcf`, at a
        // different href; its batch advanced the cursor.
        let mut real = remote_contact("Real UID", "real@example.com", "A.vcf", "Real");
        real.href = Some(real_href.into());
        quarantine_via_isolation(&ctx, &book, real, "release-real-uid-snapshot").await;
        assert_eq!(quarantine_count(&ctx).await, 1);

        // The next batch delivers a UID-less card at another href; its
        // href-derived placeholder is also `A.vcf`.
        let mut derived = remote_contact("Derived", "derived@example.com", "A.vcf", "Derived");
        derived.href = Some(derived_href.into());
        derived.uid_is_derived = true;
        let delta = ContactDelta {
            contacts: vec![derived],
            sync_token: Some("token-2".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("release delta");
        assert!(!skipped);
        assert_eq!(
            quarantine_count(&ctx).await,
            0,
            "the released snapshot is replayed and consumed"
        );

        let (total, real_rows, derived_rows): (i64, i64, i64) = ctx
            .db
            .read(move |conn| {
                let total = conn.query_row("SELECT COUNT(*) FROM contact", [], |row| row.get(0))?;
                let real_rows = conn.query_row(
                    "SELECT COUNT(*) FROM contact WHERE href = ?1 AND uid_is_derived = 0",
                    rusqlite::params![real_href],
                    |row| row.get(0),
                )?;
                let derived_rows = conn.query_row(
                    "SELECT COUNT(*) FROM contact WHERE href = ?1 AND uid_is_derived = 1",
                    rusqlite::params![derived_href],
                    |row| row.get(0),
                )?;
                Ok((total, real_rows, derived_rows))
            })
            .await
            .expect("rows");
        assert_eq!(
            real_rows, 1,
            "the real-UID snapshot must survive a placeholder collision"
        );
        assert_eq!(derived_rows, 1);
        assert_eq!(total, 2, "the two hrefs are different resources");
    }

    /// Regression ③: while a resource's *newest* quarantined version cannot be
    /// applied (an `Ambiguous` tombstone), no older version of the same href
    /// may be replayed in its place. Only the newest snapshot is retained for a
    /// later retry; older ones can never become current again.
    #[tokio::test]
    async fn blocked_newest_quarantine_snapshot_does_not_replay_an_older_version() {
        let (ctx, _service, book) = provider_contacts().await;
        let intent_id = "isolation-for-stale-replay";
        ctx.db
            .write({
                let book_id = book.id.clone();
                move |conn| {
                    crate::store::record_contact_delete_intent(conn, intent_id, "dav1", &book_id)?;
                    crate::store::mark_contact_delete_intent_unknown(conn, intent_id)?;
                    save_cursor(conn, &book_id, Some("token-0"), None)
                }
            })
            .await
            .expect("isolating intent");

        let plain = "https://dav.example.com/ab/A.vcf";
        let encoded = "https://dav.example.com/ab/%41.vcf";
        // Two versions of one resource: an old UID, then a new one under an
        // equivalent href spelling. They are distinct quarantine rows (the
        // provider id differs) but the same normalized href.
        let mut old = remote_contact("Old", "old@example.com", "uid-old", "");
        old.href = Some(plain.into());
        let mut new = remote_contact("New", "new@example.com", "uid-new", "");
        new.href = Some(encoded.into());
        for (remote, token) in [(old, "token-1"), (new, "token-2")] {
            let delta = ContactDelta {
                contacts: vec![remote],
                sync_token: Some(token.into()),
                window_start: Some(Utc::now()),
                ..Default::default()
            };
            let book_for_db = book.clone();
            let skipped = ctx
                .db
                .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
                .await
                .expect("isolated delta");
            assert!(!skipped, "book isolation must not hold the cursor");
        }
        assert_eq!(quarantine_count(&ctx).await, 2);

        // A tombstone names the newest card's UID at a different href, so the
        // newest snapshot matches ambiguously (same UID, moved resource).
        ctx.db
            .write({
                let book_id = book.id.clone();
                move |conn| {
                    crate::store::record_contact_delete_intent(
                        conn,
                        "renamed-new",
                        "dav1",
                        &book_id,
                    )?;
                    crate::store::settle_contact_delete_intent(
                        conn,
                        "renamed-new",
                        Some("uid-new"),
                        Some("https://dav.example.com/ab/renamed.vcf"),
                        Utc::now()
                            + chrono::Duration::seconds(
                                crate::store::CONTACT_DELETE_TOMBSTONE_TTL_SECONDS,
                            ),
                        false,
                    )
                }
            })
            .await
            .expect("ambiguous tombstone");

        release_contact_delete_intent(&ctx, intent_id).await;
        let delta = ContactDelta {
            sync_token: Some("token-3".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        ctx.db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("release delta");

        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            0,
            "the older version must not be imported while the newest is blocked"
        );
        assert_eq!(
            quarantine_count(&ctx).await,
            1,
            "only the newest (blocked) snapshot may remain"
        );
    }

    /// Regression: per-record settlement must consume a snapshot only by its
    /// **exact** identity (kind + normalized href). A real UID is not enough:
    /// after a rename, the old href's snapshot is handled by a tombstone while
    /// the new href's snapshot is still `Ambiguous`. Deleting the latter
    /// because they share the UID would remove the only copy of the new
    /// resource, and the incremental cursor that delivered it had already
    /// advanced. The retained snapshot must also hold this batch's cursor.
    #[tokio::test]
    async fn ambiguous_snapshot_at_another_href_is_not_consumed_by_a_shared_uid() {
        let (ctx, _service, book) = provider_contacts().await;
        let old_href = "https://dav.example.com/ab/old.vcf";
        let new_href = "https://dav.example.com/ab/new.vcf";
        let uid = "uid-shared";
        let account_id = book.account_id.clone();
        let book_id = book.id.clone();

        // Two snapshots of one real UID at different resources, as produced by
        // a resource rename; both were quarantined by earlier batches whose
        // cursor already advanced.
        let mut old = remote_contact("Moved", "moved@example.com", uid, "Old");
        old.href = Some(old_href.into());
        let mut new = remote_contact("Moved", "moved@example.com", uid, "New");
        new.href = Some(new_href.into());
        ctx.db
            .write(move |conn| {
                for remote in [&old, &new] {
                    let payload =
                        serde_json::to_string(&QuarantinedRemoteContact::from_remote(remote))
                            .expect("serialize snapshot");
                    crate::store::quarantine_contact(
                        conn,
                        &account_id,
                        &book_id,
                        remote.remote_id.as_deref(),
                        remote.href.as_deref(),
                        &payload,
                    )?;
                }
                save_cursor(conn, &book_id, Some("token-0"), None)
            })
            .await
            .expect("quarantine snapshots");
        assert_eq!(quarantine_count(&ctx).await, 2);

        // A pending tombstone names the *old* href exactly. It handles that
        // snapshot, while the new href sees the same UID at a different
        // resource and stays ambiguous.
        ctx.db
            .write({
                let book_id = book.id.clone();
                move |conn| {
                    crate::store::record_contact_delete_intent(conn, "deleted", "dav1", &book_id)?;
                    crate::store::attach_contact_delete_intent(
                        conn,
                        "deleted",
                        Some(uid),
                        Some(old_href),
                        Some("etag-old"),
                        false,
                    )
                }
            })
            .await
            .expect("tombstone");

        let empty = ContactDelta {
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let delta_for_db = empty.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta_for_db))
            .await
            .expect("replay");
        assert!(
            skipped,
            "a still-ambiguous snapshot must keep the batch cursor before it"
        );
        assert_eq!(cursor_token(&ctx, &book).await.as_deref(), Some("token-0"));
        assert_eq!(
            quarantine_count(&ctx).await,
            1,
            "only the old href's snapshot may be consumed"
        );
        let remaining = ctx
            .db
            .read({
                let account_id = book.account_id.clone();
                let book_id = book.id.clone();
                move |conn| crate::store::quarantined_contacts(conn, &account_id, &book_id)
            })
            .await
            .expect("quarantine");
        assert_eq!(
            remaining[0].href.as_deref(),
            Some(new_href),
            "the new href's snapshot is the one that must survive"
        );
        assert_eq!(ctx.db.read(contacts_repo::count).await.expect("count"), 0);

        // Once the tombstone is gone the retained snapshot imports: nothing
        // was lost by keeping it.
        ctx.db
            .write(|conn| crate::store::remove_contact_delete_intent(conn, "deleted"))
            .await
            .expect("drop tombstone");
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &empty))
            .await
            .expect("replay after release");
        assert!(!skipped, "nothing blocks the retained snapshot any more");
        assert_eq!(cursor_token(&ctx, &book).await.as_deref(), Some("token-1"));
        assert_eq!(quarantine_count(&ctx).await, 0);
        let book_id = book.id.clone();
        let stored = ctx
            .db
            .read(move |conn| contacts_repo::by_remote_id(conn, &book_id, uid))
            .await
            .expect("lookup")
            .expect("the retained snapshot must import");
        assert_eq!(stored.company, "New");
        let meta = ctx
            .db
            .read(move |conn| contacts_repo::sync_meta(conn, &stored.id))
            .await
            .expect("meta")
            .expect("sync meta");
        assert_eq!(meta.href.as_deref(), Some(new_href));
    }

    #[test]
    fn tombstone_identity_matching_is_precise() {
        let identity = SuppressedIdentity {
            contact_id: "deleted-a".into(),
            remote_id: Some("uid-a".into()),
            href: Some("https://dav.example.com/ab/a.vcf".into()),
            ..Default::default()
        };
        let mut remote = remote_contact("Same", "same@example.com", "uid-a", "");
        remote.href = Some("https://dav.example.com/ab/a.vcf".into());
        assert_eq!(
            match_suppressed_identity(&identity, &remote),
            Suppression::Suppress
        );

        // Same UID, moved href: ambiguous — neither import nor advance.
        remote.href = Some("https://dav.example.com/ab/moved.vcf".into());
        assert_eq!(
            match_suppressed_identity(&identity, &remote),
            Suppression::Ambiguous
        );

        // Different UID at the same href: a different vCard.
        remote.remote_id = Some("uid-b".into());
        remote.href = Some("https://dav.example.com/ab/a.vcf".into());
        assert_eq!(
            match_suppressed_identity(&identity, &remote),
            Suppression::None
        );

        // An href-only tombstone describes a UID-less object: a record with a
        // real UID is a different identity, never a match.
        let href_only = SuppressedIdentity {
            contact_id: "deleted-b".into(),
            remote_id: None,
            href: Some("https://dav.example.com/ab/a.vcf".into()),
            ..Default::default()
        };
        assert_eq!(
            match_suppressed_identity(&href_only, &remote),
            Suppression::None
        );

        // Regression ②: a tombstone whose id was derived from the href
        // (`A.vcf`) must not swallow a card whose *real* UID is `A.vcf`.
        let derived = SuppressedIdentity {
            contact_id: "deleted-derived".into(),
            remote_id: Some("A.vcf".into()),
            uid_is_derived: true,
            href: Some("https://dav.example.com/ab/%41.vcf".into()),
            ..Default::default()
        };
        let mut real_uid_replacement =
            remote_contact("Replacement", "new@example.com", "A.vcf", "");
        real_uid_replacement.href = Some("https://dav.example.com/ab/A.vcf".into());
        assert!(!real_uid_replacement.uid_is_derived);
        assert_eq!(
            match_suppressed_identity(&derived, &real_uid_replacement),
            Suppression::None,
            "a real UID is never matched by a derived placeholder"
        );

        // Regression ③: an old derived tombstone (`%41.vcf`) still hides the
        // UID-less card re-delivered as `A.vcf`.
        let old_derived = SuppressedIdentity {
            contact_id: "deleted-old-derived".into(),
            remote_id: Some("%41.vcf".into()),
            uid_is_derived: true,
            href: Some("https://dav.example.com/ab/%41.vcf".into()),
            ..Default::default()
        };
        let mut uid_less = remote_contact("No UID", "no-uid@example.com", "A.vcf", "");
        uid_less.href = Some("https://dav.example.com/ab/A.vcf".into());
        uid_less.uid_is_derived = true;
        assert_eq!(
            match_suppressed_identity(&old_derived, &uid_less),
            Suppression::Suppress,
            "equivalent spellings are one derived identity"
        );

        // A real-UID tombstone must not match a UID-less card at the same href.
        assert_eq!(
            match_suppressed_identity(&identity, &uid_less),
            Suppression::None
        );

        // The same derived tombstone does not match a UID-less card at another
        // href: the normalized href is the identity.
        uid_less.href = Some("https://dav.example.com/ab/other.vcf".into());
        assert_eq!(
            match_suppressed_identity(&old_derived, &uid_less),
            Suppression::None
        );
    }

    /// Regression: an intent with no identity isolates the book but must
    /// never hold the cursor. Replaying the batch is not release evidence
    /// (the old model deadlocked exactly here); release is wall-clock based.
    #[tokio::test]
    async fn book_isolation_advances_the_cursor_and_releases_after_the_ttl() {
        let (ctx, _service, book) = provider_contacts().await;
        let lost_id = "lost-receipt";
        ctx.db
            .write({
                let book_id = book.id.clone();
                move |conn| {
                    crate::store::record_contact_delete_intent(conn, lost_id, "dav1", &book_id)?;
                    crate::store::mark_contact_delete_intent_unknown(conn, lost_id)
                }
            })
            .await
            .expect("unknown intent");

        let book_id = book.id.clone();
        ctx.db
            .write(move |conn| save_cursor(conn, &book_id, Some("token-0"), Some("ctag-0")))
            .await
            .expect("seed cursor");

        let mut lost = remote_contact("Lost", "lost@example.com", "remote-lost", "");
        lost.href = Some("https://dav.example.com/ab/lost.vcf".into());
        let unrelated = remote_contact("Other", "other@example.com", "remote-other", "");

        // Every replayed/advancing batch stays importable: the objects are
        // hidden, the cursor still commits.
        for window in 1..=3 {
            let delta = ContactDelta {
                contacts: vec![lost.clone(), unrelated.clone()],
                sync_token: Some(format!("token-{window}")),
                ctag: Some(format!("ctag-{window}")),
                window_start: Some(Utc::now()),
                ..Default::default()
            };
            let book_for_db = book.clone();
            let skipped = ctx
                .db
                .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
                .await
                .expect("delta");
            assert!(
                !skipped,
                "window {window}: book-level isolation must not hold the cursor"
            );
            assert_eq!(
                ctx.db.read(contacts_repo::count).await.expect("count"),
                0,
                "window {window}: unattributed objects stay hidden"
            );
            let book_id = book.id.clone();
            let cursor = ctx
                .db
                .read(move |conn| load_cursor(conn, &book_id))
                .await
                .expect("cursor")
                .expect("cursor row");
            assert_eq!(
                cursor.sync_token.as_deref(),
                Some(format!("token-{window}").as_str()),
                "window {window}: the cursor must advance"
            );
        }
        let id = lost_id.to_string();
        let intent = ctx
            .db
            .read(move |conn| crate::store::contact_delete_intent(conn, &id))
            .await
            .expect("intent read")
            .expect("intent");
        assert_eq!(
            intent.state,
            crate::store::ContactDeleteIntentState::Unknown,
            "a few batches are not the wall-clock TTL"
        );

        // Age the intent past the TTL. Release no longer needs any cursor
        // evidence: it happens on the next apply, before importing.
        let id = lost_id.to_string();
        let aged = repo::dt_string(
            Utc::now()
                - chrono::Duration::seconds(
                    crate::store::CONTACT_DELETE_UNKNOWN_RELEASE_TTL_SECONDS + 60,
                ),
        );
        ctx.db
            .write(move |conn| {
                conn.execute(
                    "UPDATE pim_contact_delete_intent SET updated_at = ?2 WHERE contact_id = ?1",
                    rusqlite::params![id, aged],
                )?;
                Ok(())
            })
            .await
            .expect("age the intent");

        let delta = ContactDelta {
            contacts: vec![lost],
            sync_token: Some("token-released".into()),
            ctag: Some("ctag-released".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("delta after TTL");
        assert!(
            !skipped,
            "the TTL release must let the batch commit and import"
        );
        let book_id = book.id.clone();
        assert!(
            ctx.db
                .read(move |conn| contacts_repo::by_remote_id(conn, &book_id, "remote-lost"))
                .await
                .expect("lookup")
                .is_some(),
            "the object imports once the wall-clock TTL releases the intent"
        );
        let id = lost_id.to_string();
        let intent = ctx
            .db
            .read(move |conn| crate::store::contact_delete_intent(conn, &id))
            .await
            .expect("intent read")
            .expect("the released intent is kept as an audit record");
        assert_eq!(
            intent.state,
            crate::store::ContactDeleteIntentState::Released
        );
        assert!(intent.released_at.is_some());
        let book_id = book.id.clone();
        let cursor = ctx
            .db
            .read(move |conn| load_cursor(conn, &book_id))
            .await
            .expect("cursor")
            .expect("cursor row");
        assert_eq!(cursor.sync_token.as_deref(), Some("token-released"));
    }

    /// Regression 2: a later Save can upload its own object while an earlier
    /// operation (whose receipt was lost) owns the delete intent. Its accurate
    /// remote identity must not be discarded: the object has to be removed.
    #[tokio::test]
    async fn later_save_receipt_removes_its_object_despite_intent_ownership() {
        let (ctx, service, book) = provider_contacts().await;
        map_book(&ctx, &book).await;
        let created = service
            .contact_create(contact(&book.id, "Racy", "racy@example.com"))
            .await
            .expect("create");

        // A: its POST may have committed, but the receipt never arrived.
        mark_save_sent(&ctx).await;
        let a = save_op(&ctx).await;
        let a_id = a.id.clone();
        ctx.db
            .write(move |conn| repo::pending_ops::mark_failed(conn, &a_id, "receipt lost"))
            .await
            .expect("fail A");

        // B: a later Save that reaches the provider while the local row still
        // exists.
        let mut edited = created.clone();
        edited.company = "B edit".into();
        service.contact_update(edited).await.expect("update B");
        let b = ctx
            .db
            .read(|conn| repo::pending_ops::list(conn, Some("dav1")))
            .await
            .expect("ops")
            .into_iter()
            .find(|op| op.kind == ops::KIND_SAVE && !op.send_intent)
            .expect("unsent B");
        let b_id = b.id.clone();
        ctx.db
            .write(move |conn| {
                repo::pending_ops::mark_in_progress(conn, &b_id)?;
                repo::pending_ops::mark_send_intent(conn, &b_id).map(|_| ())
            })
            .await
            .expect("mark B sent");

        // The user deletes before B's receipt lands.
        service
            .contact_delete(vec![created.id.clone()])
            .await
            .expect("delete");
        let id = created.id.clone();
        let intent = ctx
            .db
            .read(move |conn| crate::store::contact_delete_intent(conn, &id))
            .await
            .expect("intent read")
            .expect("cleanup intent");
        assert_eq!(
            intent.owner_op_id.as_deref(),
            Some(a.id.as_str()),
            "the earliest possibly-sent operation owns the intent"
        );
        assert!(intent.remote_id.is_none());

        // B finishes and returns its own accurate remote identity.
        let receipt = CreateReceiptIdentity {
            remote_id: Some("remote-b".into()),
            href: Some("https://dav.example.com/ab/b.vcf".into()),
            etag: Some("etag-b".into()),
            uid_is_derived: false,
        };
        let outcome = service
            .bind_create_receipt(&created.id, &b.id, &receipt)
            .await
            .expect("bind B receipt");
        assert!(
            matches!(
                outcome,
                CreateReceiptOutcome::RowGone { owned_intent: None }
            ),
            "B must not settle an intent owned by A, but its receipt must be kept"
        );
        let id = created.id.clone();
        let receipts = ctx
            .db
            .read(move |conn| crate::store::contact_delete_receipts(conn, &id))
            .await
            .expect("receipts");
        assert_eq!(receipts.len(), 1, "B's identity must not be discarded");
        assert_eq!(receipts[0].op_id, b.id);
        assert_eq!(receipts[0].remote_id.as_deref(), Some("remote-b"));

        // This is the cleanup the executor runs right after binding.
        let account = ctx
            .db
            .read(|conn| repo::accounts::get_required(conn, "dav1"))
            .await
            .expect("account");
        let provider = RecordingProvider::default();
        service
            .settle_contact_delete_receipts(&account, &provider, &created.id)
            .await
            .expect("cleanup B");
        let deleted = provider
            .deleted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(deleted.len(), 1, "B's server object must be removed");
        assert_eq!(deleted[0].remote_id.as_deref(), Some("remote-b"));
        assert_eq!(
            deleted[0].href.as_deref(),
            Some("https://dav.example.com/ab/b.vcf")
        );

        // A's unaddressable object is still tracked by its intent; only B's
        // receipt settled.
        let id = created.id.clone();
        let intent = ctx
            .db
            .read(move |conn| crate::store::contact_delete_intent(conn, &id))
            .await
            .expect("intent read")
            .expect("intent");
        assert_eq!(
            intent.state,
            crate::store::ContactDeleteIntentState::Pending
        );
        assert_eq!(intent.owner_op_id.as_deref(), Some(a.id.as_str()));
        let id = created.id.clone();
        let receipts = ctx
            .db
            .read(move |conn| crate::store::contact_delete_receipts(conn, &id))
            .await
            .expect("receipts");
        assert_eq!(
            receipts[0].state,
            crate::store::ContactDeleteReceiptState::Settled
        );
    }

    /// Regression 3: a CardDAV resource rename (same UID, different href)
    /// keeps matching ambiguously. With a terminal owner the book must not be
    /// held forever: the intent is released after a bounded number of windows
    /// and the cursor resumes.
    #[tokio::test]
    async fn ambiguous_carddav_intent_is_released_after_a_terminal_owner() {
        let (ctx, service, book) = provider_contacts().await;
        let _created = service
            .contact_create(contact(&book.id, "Renamed", "renamed@example.com"))
            .await
            .expect("create");
        mark_save_sent(&ctx).await;
        let owner = save_op(&ctx).await;
        let owner_id = owner.id.clone();
        let owner_attempt = owner.attempts;
        ctx.db
            .write(move |conn| repo::pending_ops::mark_failed(conn, &owner_id, "terminal"))
            .await
            .expect("terminal owner");

        let book_id = book.id.clone();
        let owner_id_for_intent = owner.id.clone();
        ctx.db
            .write(move |conn| {
                crate::store::record_contact_delete_intent_owned(
                    conn,
                    "renamed-contact",
                    "dav1",
                    &book_id,
                    Some(&owner_id_for_intent),
                    Some(owner_attempt),
                )?;
                crate::store::attach_contact_delete_intent(
                    conn,
                    "renamed-contact",
                    Some("uid-renamed"),
                    Some("https://dav.example.com/ab/old.vcf"),
                    Some("etag-old"),
                    false,
                )?;
                save_cursor(conn, &book_id, Some("token-0"), Some("ctag-0"))
            })
            .await
            .expect("intent with identity");

        let mut renamed = remote_contact("Renamed", "renamed@example.com", "uid-renamed", "");
        renamed.href = Some("https://dav.example.com/ab/new.vcf".into());
        let windows = crate::store::CONTACT_DELETE_AMBIGUOUS_RELEASE_WINDOWS;
        assert!(windows > 1, "the release must be bounded, not immediate");
        for window in 1..=windows {
            let delta = ContactDelta {
                contacts: vec![renamed.clone()],
                sync_token: Some("token-1".into()),
                ctag: Some("ctag-1".into()),
                window_start: Some(Utc::now()),
                ..Default::default()
            };
            let book_for_db = book.clone();
            let skipped = ctx
                .db
                .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
                .await
                .expect("ambiguous delta");
            assert!(
                skipped,
                "window {window}: the ambiguous identity must hold the cursor"
            );
            if window < windows {
                let id = "renamed-contact".to_string();
                let intent = ctx
                    .db
                    .read(move |conn| crate::store::contact_delete_intent(conn, &id))
                    .await
                    .expect("intent read")
                    .expect("intent");
                assert_eq!(
                    intent.state,
                    crate::store::ContactDeleteIntentState::Pending,
                    "the intent must not be released before the bound"
                );
            }
        }

        let id = "renamed-contact".to_string();
        let intent = ctx
            .db
            .read(move |conn| crate::store::contact_delete_intent(conn, &id))
            .await
            .expect("intent read")
            .expect("intent");
        assert_eq!(
            intent.state,
            crate::store::ContactDeleteIntentState::Released,
            "a terminal owner plus persistent ambiguity must be released"
        );
        assert!(intent.released_at.is_some());
        assert_eq!(intent.ambiguous_windows, windows);

        // The next delivery of the same batch imports the renamed object and
        // restores the cursor.
        let delta = ContactDelta {
            contacts: vec![renamed],
            sync_token: Some("token-1".into()),
            ctag: Some("ctag-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("delta after release");
        assert!(
            !skipped,
            "the released intent must let the renamed object import"
        );
        let book_id = book.id.clone();
        assert!(
            ctx.db
                .read(move |conn| contacts_repo::by_remote_id(conn, &book_id, "uid-renamed"))
                .await
                .expect("lookup")
                .is_some(),
            "the renamed contact must import once the intent is released"
        );
        let book_id = book.id.clone();
        let cursor = ctx
            .db
            .read(move |conn| load_cursor(conn, &book_id))
            .await
            .expect("cursor")
            .expect("cursor row");
        assert_eq!(cursor.sync_token.as_deref(), Some("token-1"));
    }

    /// Regression ①: a released parent intent must not discard the exact
    /// identity of a per-operation receipt. Receipts are settled by identity
    /// first; only then is the parent released.
    #[tokio::test]
    async fn released_parent_still_cleans_up_child_receipts_before_releasing() {
        let (ctx, service, book) = provider_contacts().await;
        map_book(&ctx, &book).await;
        let contact_id = "released-parent";
        let receipt_remote = "remote-child";
        let receipt_href = "https://dav.example.com/ab/child.vcf";
        ctx.db
            .write({
                let book_id = book.id.clone();
                let contact_id = contact_id.to_string();
                move |conn| {
                    crate::store::record_contact_delete_intent(
                        conn,
                        &contact_id,
                        "dav1",
                        &book_id,
                    )?;
                    crate::store::mark_contact_delete_intent_unknown(conn, &contact_id)?;
                    crate::store::record_contact_delete_receipt(
                        conn,
                        &contact_id,
                        "op-child",
                        Some(receipt_remote),
                        Some(receipt_href),
                        None,
                        false,
                    )?;
                    // A delta re-imported the child object before the parent
                    // was released.
                    let mut copy = Contact::new("reimported-copy", &book_id, "dav1");
                    copy.display_name = "Child".into();
                    contacts_repo::upsert_remote(
                        conn,
                        &copy,
                        None,
                        Some(receipt_remote),
                        Some(receipt_href),
                        None,
                    )?;
                    Ok(())
                }
            })
            .await
            .expect("setup");

        // Age the parent past its wall-clock TTL so this apply releases it.
        let aged = repo::dt_string(
            Utc::now()
                - chrono::Duration::seconds(
                    crate::store::CONTACT_DELETE_UNKNOWN_RELEASE_TTL_SECONDS + 60,
                ),
        );
        let id = contact_id.to_string();
        ctx.db
            .write(move |conn| {
                conn.execute(
                    "UPDATE pim_contact_delete_intent SET updated_at = ?2 WHERE contact_id = ?1",
                    rusqlite::params![id, aged],
                )?;
                Ok(())
            })
            .await
            .expect("age the intent");

        let book_id = book.id.clone();
        ctx.db
            .write(move |conn| save_cursor(conn, &book_id, Some("token-0"), Some("ctag-0")))
            .await
            .expect("seed cursor");

        let mut child = remote_contact("Child", "child@example.com", receipt_remote, "");
        child.href = Some(receipt_href.into());
        let delta = ContactDelta {
            contacts: vec![child],
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("delta");
        assert!(
            !skipped,
            "the receipt identity must hide its object without holding the cursor"
        );
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            0,
            "the re-imported copy must be removed and the object must not be re-imported"
        );
        let book_id = book.id.clone();
        let cursor = ctx
            .db
            .read(move |conn| load_cursor(conn, &book_id))
            .await
            .expect("cursor")
            .expect("cursor row");
        assert_eq!(cursor.sync_token.as_deref(), Some("token-1"));

        let id = contact_id.to_string();
        let intent = ctx
            .db
            .read(move |conn| crate::store::contact_delete_intent(conn, &id))
            .await
            .expect("intent read")
            .expect("intent");
        assert_eq!(
            intent.state,
            crate::store::ContactDeleteIntentState::Released
        );
        assert!(intent.released_at.is_some());

        // The receipt still exists and is still pending: releasing the parent
        // must not silently complete the child's cleanup.
        let id = contact_id.to_string();
        let receipts = ctx
            .db
            .read(move |conn| crate::store::contact_delete_receipts(conn, &id))
            .await
            .expect("receipts");
        assert_eq!(receipts.len(), 1);
        assert_eq!(
            receipts[0].state,
            crate::store::ContactDeleteReceiptState::Pending
        );

        // The exact identity is still actionable: the executor's cleanup
        // removes the server object after the release.
        let account = ctx
            .db
            .read(|conn| repo::accounts::get_required(conn, "dav1"))
            .await
            .expect("account");
        let provider = RecordingProvider::default();
        service
            .settle_contact_delete_receipts(&account, &provider, contact_id)
            .await
            .expect("cleanup child receipt");
        let deleted = provider
            .deleted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0].remote_id.as_deref(), Some(receipt_remote));
        let id = contact_id.to_string();
        let receipts = ctx
            .db
            .read(move |conn| crate::store::contact_delete_receipts(conn, &id))
            .await
            .expect("receipts");
        assert_eq!(
            receipts[0].state,
            crate::store::ContactDeleteReceiptState::Settled
        );
    }

    /// Regression ③: an object with an exact receipt identity must bypass the
    /// book-level isolation of an identity-less sibling intent. It is hidden,
    /// the cursor advances, and it is never imported.
    #[tokio::test]
    async fn precise_receipt_object_bypasses_book_level_isolation() {
        let (ctx, _service, book) = provider_contacts().await;
        let contact_id = "parent-unknown";
        let receipt_remote = "remote-known";
        let receipt_href = "https://dav.example.com/ab/known.vcf";
        ctx.db
            .write({
                let book_id = book.id.clone();
                move |conn| {
                    crate::store::record_contact_delete_intent(conn, contact_id, "dav1", &book_id)?;
                    crate::store::mark_contact_delete_intent_unknown(conn, contact_id)?;
                    crate::store::record_contact_delete_receipt(
                        conn,
                        contact_id,
                        "op-known",
                        Some(receipt_remote),
                        Some(receipt_href),
                        None,
                        false,
                    )
                }
            })
            .await
            .expect("parent + receipt");

        let book_id = book.id.clone();
        ctx.db
            .write(move |conn| save_cursor(conn, &book_id, Some("token-0"), Some("ctag-0")))
            .await
            .expect("seed cursor");

        let mut known = remote_contact("Known", "known@example.com", receipt_remote, "");
        known.href = Some(receipt_href.into());

        // The batch contains only the exact receipt object. The old order hit
        // the identity-less book rule first and reported `skipped = true`.
        let delta = ContactDelta {
            contacts: vec![known.clone()],
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("delta");
        assert!(
            !skipped,
            "an exact receipt identity must bypass the book-level block"
        );
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            0,
            "the exact receipt object must not be imported"
        );
        let book_id = book.id.clone();
        let cursor = ctx
            .db
            .read(move |conn| load_cursor(conn, &book_id))
            .await
            .expect("cursor")
            .expect("cursor row");
        assert_eq!(cursor.sync_token.as_deref(), Some("token-1"));

        // A mixed batch: the exact object is hidden by identity, the
        // unrelated object by book isolation. Neither holds the cursor.
        let delta = ContactDelta {
            contacts: vec![
                known,
                remote_contact("Other", "other@example.com", "remote-other", ""),
            ],
            sync_token: Some("token-2".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("mixed delta");
        assert!(!skipped, "book isolation must not hold the cursor");
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            0,
            "neither the exact object nor the unattributed object may import"
        );
        let book_id = book.id.clone();
        let cursor = ctx
            .db
            .read(move |conn| load_cursor(conn, &book_id))
            .await
            .expect("cursor")
            .expect("cursor row");
        assert_eq!(cursor.sync_token.as_deref(), Some("token-2"));

        let id = contact_id.to_string();
        let intent = ctx
            .db
            .read(move |conn| crate::store::contact_delete_intent(conn, &id))
            .await
            .expect("intent read")
            .expect("intent");
        assert_eq!(
            intent.state,
            crate::store::ContactDeleteIntentState::Unknown,
            "the parent stays isolated until its own TTL"
        );
    }

    /// Regression ①: a late delta of the *old* UID at a reused href hits the
    /// tombstone, but cleanup must not reuse the generic href lookup — that
    /// would delete the new contact that now owns the same resource URL.
    #[tokio::test]
    async fn late_tombstone_delta_does_not_delete_a_new_contact_at_the_same_href() {
        let (ctx, _service, book) = provider_contacts().await;
        let shared_href = "https://dav.example.com/ab/shared.vcf";
        let old_uid = "old-uid";
        let new_uid = "new-uid";
        ctx.db
            .write({
                let book_id = book.id.clone();
                move |conn| {
                    // The replacement vCard: same URL, different UID.
                    let mut replacement = Contact::new("new-contact", &book_id, "dav1");
                    replacement.display_name = "New".into();
                    contacts_repo::upsert_remote(
                        conn,
                        &replacement,
                        Some("e-new"),
                        Some(new_uid),
                        Some(shared_href),
                        None,
                    )?;
                    // The deleted old vCard, settled as a tombstone.
                    crate::store::record_contact_delete_intent(
                        conn,
                        "old-contact",
                        "dav1",
                        &book_id,
                    )?;
                    crate::store::settle_contact_delete_intent(
                        conn,
                        "old-contact",
                        Some(old_uid),
                        Some(shared_href),
                        Utc::now()
                            + chrono::Duration::seconds(
                                crate::store::CONTACT_DELETE_TOMBSTONE_TTL_SECONDS,
                            ),
                        false,
                    )?;
                    save_cursor(conn, &book_id, Some("token-0"), None)
                }
            })
            .await
            .expect("setup");

        // A batch captured before the delete still names the old object: same
        // UID and the same href the replacement now uses.
        let mut late_old = remote_contact("Old", "old@example.com", old_uid, "");
        late_old.href = Some(shared_href.into());
        let delta = ContactDelta {
            contacts: vec![late_old],
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("delta");
        assert!(
            !skipped,
            "a definite tombstone match never holds the cursor"
        );

        let book_id = book.id.clone();
        assert!(
            ctx.db
                .read(move |conn| contacts_repo::by_remote_id(conn, &book_id, new_uid))
                .await
                .expect("lookup")
                .is_some(),
            "the new contact that reused the href must survive the old tombstone"
        );
        let book_id = book.id.clone();
        let cursor = ctx
            .db
            .read(move |conn| load_cursor(conn, &book_id))
            .await
            .expect("cursor")
            .expect("cursor row");
        assert_eq!(cursor.sync_token.as_deref(), Some("token-1"));
    }

    /// Regression ②: the receipt pre-cleanup must not delete a row that a
    /// newer local operation still owns. An empty delta racing a queued edit
    /// used to wipe the edit target.
    #[tokio::test]
    async fn child_receipt_cleanup_keeps_a_pending_edit_target() {
        let (ctx, service, book) = provider_contacts().await;
        let contact_id = "edit-target";
        let remote_id = "remote-target";
        let href = "https://dav.example.com/ab/target.vcf";
        ctx.db
            .write({
                let book_id = book.id.clone();
                move |conn| {
                    crate::store::record_contact_delete_intent(
                        conn,
                        "old-deleted",
                        "dav1",
                        &book_id,
                    )?;
                    crate::store::record_contact_delete_receipt(
                        conn,
                        "old-deleted",
                        "op-child",
                        Some(remote_id),
                        Some(href),
                        None,
                        false,
                    )?;
                    let mut stored = Contact::new(contact_id, &book_id, "dav1");
                    stored.display_name = "Editable".into();
                    stored.company = "Server".into();
                    contacts_repo::upsert_remote(
                        conn,
                        &stored,
                        Some("e0"),
                        Some(remote_id),
                        Some(href),
                        None,
                    )?;
                    Ok(())
                }
            })
            .await
            .expect("setup");

        let stored = ctx
            .db
            .read(move |conn| contacts_repo::get(conn, contact_id))
            .await
            .expect("get")
            .expect("row");
        let mut edited = stored.clone();
        edited.company = "Local edit".into();
        service.contact_update(edited).await.expect("queue edit");

        // The delta is empty and describes the server before the edit landed.
        let delta = ContactDelta {
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("empty delta");
        assert!(!skipped);

        let reloaded = ctx
            .db
            .read(move |conn| contacts_repo::get(conn, contact_id))
            .await
            .expect("reload")
            .expect("the edit target must survive the receipt pre-cleanup");
        assert_eq!(reloaded.company, "Local edit");
    }

    /// Regression ③: objects hidden by book-level isolation are persisted and
    /// replayed when the intent is released. The provider never redelivers the
    /// batch because its cursor already advanced.
    #[tokio::test]
    async fn isolated_book_quarantines_and_replays_unrelated_contacts_on_release() {
        let (ctx, _service, book) = provider_contacts().await;
        let lost_id = "lost-receipt";
        ctx.db
            .write({
                let book_id = book.id.clone();
                move |conn| {
                    crate::store::record_contact_delete_intent(conn, lost_id, "dav1", &book_id)?;
                    crate::store::mark_contact_delete_intent_unknown(conn, lost_id)?;
                    save_cursor(conn, &book_id, Some("token-0"), None)
                }
            })
            .await
            .expect("intent");

        let mut lost = remote_contact("Lost", "lost@example.com", "remote-lost", "");
        lost.href = Some("https://dav.example.com/ab/lost.vcf".into());
        let unrelated = remote_contact("Other", "other@example.com", "remote-other", "");
        let delta = ContactDelta {
            contacts: vec![lost.clone(), unrelated.clone()],
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("blocked delta");
        assert!(!skipped, "book isolation must not hold the cursor");
        assert_eq!(
            ctx.db.read(contacts_repo::count).await.expect("count"),
            0,
            "isolated objects stay hidden until release"
        );
        let quarantined: i64 = ctx
            .db
            .read(|conn| {
                conn.query_row("SELECT COUNT(*) FROM pim_contact_quarantine", [], |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(|error| AppError::db(format!("quarantine count: {error}")))
            })
            .await
            .expect("quarantine count");
        assert_eq!(quarantined, 2, "both dropped objects are persisted");

        // Age the intent past the wall-clock TTL.
        let id = lost_id.to_string();
        let aged = repo::dt_string(
            Utc::now()
                - chrono::Duration::seconds(
                    crate::store::CONTACT_DELETE_UNKNOWN_RELEASE_TTL_SECONDS + 60,
                ),
        );
        ctx.db
            .write(move |conn| {
                conn.execute(
                    "UPDATE pim_contact_delete_intent SET updated_at = ?2 WHERE contact_id = ?1",
                    rusqlite::params![id, aged],
                )?;
                Ok(())
            })
            .await
            .expect("age the intent");

        // The provider delivers nothing new: recovery must come from the
        // quarantine replay, not from a re-delivery.
        let delta = ContactDelta {
            sync_token: Some("token-2".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("release delta");
        assert!(!skipped);
        let book_id = book.id.clone();
        assert!(
            ctx.db
                .read(move |conn| contacts_repo::by_remote_id(conn, &book_id, "remote-other"))
                .await
                .expect("other lookup")
                .is_some(),
            "the unrelated contact must be restored from quarantine"
        );
        let book_id = book.id.clone();
        assert!(
            ctx.db
                .read(move |conn| contacts_repo::by_remote_id(conn, &book_id, "remote-lost"))
                .await
                .expect("lost lookup")
                .is_some(),
            "the possibly-created object is restored once isolation is lifted"
        );
        let quarantined: i64 = ctx
            .db
            .read(|conn| {
                conn.query_row("SELECT COUNT(*) FROM pim_contact_quarantine", [], |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(|error| AppError::db(format!("quarantine count: {error}")))
            })
            .await
            .expect("quarantine count");
        assert_eq!(quarantined, 0, "replayed records are consumed");
        let book_id = book.id.clone();
        let cursor = ctx
            .db
            .read(move |conn| load_cursor(conn, &book_id))
            .await
            .expect("cursor")
            .expect("cursor row");
        assert_eq!(cursor.sync_token.as_deref(), Some("token-2"));
    }

    /// Regression ④: a pending receipt must be able to release its own
    /// ambiguity block even when its parent intent is already released.
    #[tokio::test]
    async fn released_parent_child_receipt_stops_blocking_the_cursor_after_the_bound() {
        let (ctx, _service, book) = provider_contacts().await;
        let contact_id = "released-parent";
        let uid = "uid-child";
        let old_href = "https://dav.example.com/ab/old.vcf";
        ctx.db
            .write({
                let book_id = book.id.clone();
                move |conn| {
                    crate::store::record_contact_delete_intent(conn, contact_id, "dav1", &book_id)?;
                    crate::store::attach_contact_delete_intent(
                        conn,
                        contact_id,
                        Some(uid),
                        Some(old_href),
                        Some("e-old"),
                        false,
                    )?;
                    crate::store::record_contact_delete_receipt(
                        conn,
                        contact_id,
                        "op-child",
                        Some(uid),
                        Some(old_href),
                        None,
                        false,
                    )?;
                    conn.execute(
                        "UPDATE pim_contact_delete_intent
                         SET state = 'released', released_at = ?2
                         WHERE contact_id = ?1",
                        rusqlite::params![contact_id, repo::now_string()],
                    )?;
                    save_cursor(conn, &book_id, Some("token-0"), None)
                }
            })
            .await
            .expect("setup");

        // Same UID, different href: ambiguous against the child receipt.
        let mut moved = remote_contact("Moved", "moved@example.com", uid, "");
        moved.href = Some("https://dav.example.com/ab/new.vcf".into());
        let windows = crate::store::CONTACT_DELETE_AMBIGUOUS_RELEASE_WINDOWS;
        for window in 1..=windows {
            let delta = ContactDelta {
                contacts: vec![moved.clone()],
                sync_token: Some("token-1".into()),
                window_start: Some(Utc::now()),
                ..Default::default()
            };
            let book_for_db = book.clone();
            let skipped = ctx
                .db
                .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
                .await
                .expect("ambiguous delta");
            assert!(
                skipped,
                "window {window}: the receipt ambiguity must hold the cursor"
            );
        }
        let id = contact_id.to_string();
        let receipts = ctx
            .db
            .read(move |conn| crate::store::contact_delete_receipts(conn, &id))
            .await
            .expect("receipts");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].ambiguous_windows, windows);
        assert!(
            receipts[0].released_at.is_some(),
            "the child receipt must be released independently of its parent"
        );

        // The next delivery imports the renamed object and the cursor moves.
        let delta = ContactDelta {
            contacts: vec![moved],
            sync_token: Some("token-1".into()),
            window_start: Some(Utc::now()),
            ..Default::default()
        };
        let book_for_db = book.clone();
        let skipped = ctx
            .db
            .write(move |conn| apply_delta_and_cursor(conn, "dav1", &book_for_db, &delta))
            .await
            .expect("delta after receipt release");
        assert!(!skipped);
        let book_id = book.id.clone();
        assert!(
            ctx.db
                .read(move |conn| contacts_repo::by_remote_id(conn, &book_id, uid))
                .await
                .expect("lookup")
                .is_some(),
            "the renamed contact must import once the receipt is released"
        );
        let book_id = book.id.clone();
        let cursor = ctx
            .db
            .read(move |conn| load_cursor(conn, &book_id))
            .await
            .expect("cursor")
            .expect("cursor row");
        assert_eq!(cursor.sync_token.as_deref(), Some("token-1"));
    }

    /// Regression ②: a late create receipt from an older save must not
    /// overwrite the `remote_id` and identity kind a newer save confirmed.
    #[tokio::test]
    async fn late_create_receipt_does_not_overwrite_confirmed_remote_id() {
        let (ctx, service, book) = provider_contacts().await;
        let created = service
            .contact_create(contact(&book.id, "Grace Hopper", "grace@example.com"))
            .await
            .expect("create");
        let id = created.id.clone();

        // The older create's operation is the first queued save.
        let old_op = save_op(&ctx).await;

        let mut edited = created.clone();
        edited.company = "Navy".into();
        service.contact_update(edited).await.expect("update");
        let new_op = ctx
            .db
            .read(|conn| repo::pending_ops::list(conn, Some("dav1")))
            .await
            .expect("ops")
            .into_iter()
            .filter(|op| op.kind == ops::KIND_SAVE)
            .find(|op| op.id != old_op.id)
            .expect("newer save op");

        // The newer save confirms a real UID first.
        service
            .bind_create_receipt(
                &id,
                &new_op.id,
                &CreateReceiptIdentity {
                    remote_id: Some("new-uid".into()),
                    href: Some("https://dav.example.com/ab/new.vcf".into()),
                    etag: Some("etag-new".into()),
                    uid_is_derived: false,
                },
            )
            .await
            .expect("new receipt");
        // The worker completes the newer save, so its confirmation is the
        // newest one on record.
        ctx.db
            .write({
                let op_id = new_op.id.clone();
                move |conn| repo::pending_ops::mark_done(conn, &op_id)
            })
            .await
            .expect("mark newer save done");

        // The older create's receipt arrives late and must be dropped.
        service
            .bind_create_receipt(
                &id,
                &old_op.id,
                &CreateReceiptIdentity {
                    remote_id: Some("old-uid".into()),
                    href: Some("https://dav.example.com/ab/old.vcf".into()),
                    etag: Some("etag-old".into()),
                    uid_is_derived: true,
                },
            )
            .await
            .expect("old receipt");

        let stored = ctx
            .db
            .read({
                let id = id.clone();
                move |conn| snapshot_contact(conn, &id)
            })
            .await
            .expect("snapshot")
            .expect("contact row");
        assert_eq!(
            stored.remote_id.as_deref(),
            Some("new-uid"),
            "the late receipt must not replace the newer confirmed remote id"
        );
        assert_eq!(
            stored.href.as_deref(),
            Some("https://dav.example.com/ab/new.vcf")
        );
        assert_eq!(stored.etag.as_deref(), Some("etag-new"));
        assert!(
            !stored.uid_is_derived,
            "the identity kind confirmed by the newer save must survive"
        );
    }
}
