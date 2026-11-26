//! Message listing, conversations, optimistic actions with undo recording and
//! mail-rule execution.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use nuncio_core::events::MailChangeKind;
use nuncio_core::models::{
    Address, Attachment, Folder, FolderRole, Importance, MailFilter, MailRule, MailScope,
    MessageAction, MessageListQuery, MessagePage, MessageSummary, NewPendingOp, PageCursor,
    ProviderKind, SearchCoverage, SearchQuery, StoredMessage,
};
use nuncio_core::{events, repo, AppError, Result};
use rusqlite::{types::Value, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::ops::{
    ArchiveOp, DeleteOp, FlagRollback, MarkJunkOp, MoveOp, SetFlagsOp, KIND_ARCHIVE, KIND_DELETE,
    KIND_MARK_JUNK, KIND_MOVE, KIND_SET_FLAGS,
};

use super::MailService;

pub(crate) fn is_local_only(provider: ProviderKind) -> bool {
    matches!(provider, ProviderKind::Local | ProviderKind::Pop3)
}

/// Whether a delete must erase the message instead of moving it to Trash.
///
/// The backend owns this decision: it sees each message's real folder, while a
/// client only sees one selected folder (which is wrong for search results).
/// A draft is always erased outright — "discard draft" must not leave a copy in
/// Trash, and a draft keeps `is_draft` set when merely moved, so the Drafts
/// count would never drop. The Drafts folder role is checked as well because a
/// server flag can lag behind the folder a message actually sits in.
fn delete_is_permanent(requested: bool, source_role: FolderRole, is_draft: bool) -> bool {
    requested
        || is_draft
        || matches!(
            source_role,
            FolderRole::Trash | FolderRole::Junk | FolderRole::Drafts
        )
}

/// Serializable copy of [`StoredMessage`] (core keeps that type serde-free).
///
/// Embedded in [`crate::ops::DeleteOp`] so a permanent delete can be rolled
/// back after the local row was removed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSnapshot {
    pub summary: MessageSummary,
    pub bcc: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub message_id_header: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub uid: Option<i64>,
    pub uidvalidity: Option<i64>,
    pub gm_msgid: Option<String>,
    pub gm_thrid: Option<String>,
    pub labels: Vec<String>,
    pub raw_headers: Option<String>,
    pub body_path: Option<String>,
    pub body_cached_at: Option<chrono::DateTime<chrono::Utc>>,
    pub internal_date: Option<chrono::DateTime<chrono::Utc>>,
    pub is_deleted_local: bool,
}

impl From<&StoredMessage> for StoredSnapshot {
    fn from(stored: &StoredMessage) -> Self {
        Self {
            summary: stored.summary.clone(),
            bcc: stored.bcc.clone(),
            reply_to: stored.reply_to.clone(),
            message_id_header: stored.message_id_header.clone(),
            in_reply_to: stored.in_reply_to.clone(),
            references: stored.references.clone(),
            uid: stored.uid,
            uidvalidity: stored.uidvalidity,
            gm_msgid: stored.gm_msgid.clone(),
            gm_thrid: stored.gm_thrid.clone(),
            labels: stored.labels.clone(),
            raw_headers: stored.raw_headers.clone(),
            body_path: stored.body_path.clone(),
            body_cached_at: stored.body_cached_at,
            internal_date: stored.internal_date,
            is_deleted_local: stored.is_deleted_local,
        }
    }
}

impl StoredSnapshot {
    pub(crate) fn into_stored(self) -> StoredMessage {
        StoredMessage {
            summary: self.summary,
            bcc: self.bcc,
            reply_to: self.reply_to,
            message_id_header: self.message_id_header,
            in_reply_to: self.in_reply_to,
            references: self.references,
            uid: self.uid,
            uidvalidity: self.uidvalidity,
            gm_msgid: self.gm_msgid,
            gm_thrid: self.gm_thrid,
            labels: self.labels,
            raw_headers: self.raw_headers,
            body_path: self.body_path,
            body_cached_at: self.body_cached_at,
            body_identity: None,
            internal_date: self.internal_date,
            is_deleted_local: self.is_deleted_local,
        }
    }
}

/// Local snapshot recorded with an undo entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UndoEntryState {
    pub id: String,
    pub account_id: String,
    pub folder_id: String,
    pub target_folder_id: Option<String>,
    pub uid: Option<i64>,
    pub permanent: bool,
    pub seen: bool,
    pub flagged: bool,
    #[serde(default)]
    pub answered: bool,
    #[serde(default)]
    pub forwarded: bool,
    pub is_deleted_local: bool,
    pub snapshot: StoredSnapshot,
    pub attachments: Vec<Attachment>,
    /// Owner token of this action's local flag writes. Remote actions also
    /// record their queued operation id; local/POP3 actions have no operation,
    /// so this token is the only proof that an undo still owns the field.
    #[serde(default)]
    pub flag_writer_id: Option<String>,
}

/// Undo payload persisted in `undo_log.payload_json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UndoPayload {
    pub action: String,
    pub value: Option<bool>,
    pub entries: Vec<UndoEntryState>,
    pub op_ids: Vec<String>,
}

/// How long a paged search may reuse its server result.
const SEARCH_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(120);

/// Cached outcome of the last server search (see `cached_server_search`).
pub(crate) struct SearchCacheEntry {
    key: String,
    stored: std::time::Instant,
    ids: Vec<String>,
    coverage: SearchCoverage,
}

#[derive(Debug, Clone)]
struct Target {
    message: StoredMessage,
    account: nuncio_core::models::Account,
    target_folder: Option<String>,
    permanent: bool,
}

/// One queued server operation prepared before the state-changing transaction.
///
/// The local optimistic change and the queue row are written in the same
/// transaction, so a failure to queue cannot leave a row marked
/// `target_pending` with no operation that will ever release it.
struct PreparedOp {
    account_id: String,
    kind: &'static str,
    payload: serde_json::Value,
    description: String,
}

/// Folder-local identity a payload was built from. Re-read inside the write
/// transaction to make sure a concurrent sync/action did not move the row or
/// replace its UID generation before the operation is queued.
#[derive(Debug, Clone)]
struct RemoteExpectation {
    id: String,
    account_id: String,
    folder_id: String,
    uid: Option<i64>,
    uidvalidity: Option<i64>,
    /// Provider identity the target was read with (Graph only). The queue
    /// transaction re-checks both the row and the `graph_remote` mapping, so a
    /// sync that rebound the stable row/UID pair to another message cannot
    /// slip a delete for the wrong copy into the queue.
    remote_id: Option<String>,
    /// Whether this account addresses messages by provider id (Graph). Such an
    /// operation must never be queued without a pinned id: the executor could
    /// only re-resolve it from the mutable mapping, which is exactly the copy a
    /// sync may have swapped underneath the user.
    graph: bool,
}

/// `(uids, rollback, message_ids, remote_ids)`; the last three align with
/// `uids`. `remote_ids` pins the Graph copy each UID addressed when the action
/// was queued, so a later undo never re-resolves a mutable UID mapping.
type FlagGroups =
    HashMap<(String, String, Option<i64>), (Vec<i64>, Vec<FlagRollback>, Vec<String>, Vec<String>)>;
type MoveGroups = HashMap<(String, String, String, Option<i64>), (Vec<i64>, Vec<String>)>;
type ArchiveGroups =
    HashMap<(String, String, Option<i64>), (Vec<i64>, Vec<String>, Option<String>)>;
/// Re-check, inside the queue transaction, that the row still carries the
/// identity the payload was built from.
///
/// Besides the folder/UID generation this also pins a Graph provider id: a
/// sync may reuse the same local row (and therefore the same synthetic UID)
/// for a different provider message, so the UID alone cannot prove the delete
/// would address the copy the user selected.
fn remote_expectation_matches(db: &Connection, expected: &RemoteExpectation) -> Result<bool> {
    let Some(stored) = repo::messages::get(db, &expected.id)? else {
        return Ok(false);
    };
    if stored.summary.account_id != expected.account_id
        || stored.summary.folder_id != expected.folder_id
        || stored.uid != expected.uid
        || stored.uidvalidity != expected.uidvalidity
    {
        return Ok(false);
    }
    let Some(remote) = expected
        .remote_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    else {
        // A Graph command may only address the provider id the user acted on.
        // With no pinned id the executor would have to re-resolve the UID
        // through the mutable mapping, which a sync may already have rebound.
        // Refuse to queue instead of risking a delete/flag on another message.
        return Ok(!expected.graph);
    };
    if stored.gm_msgid.as_deref() != Some(remote) {
        return Ok(false);
    }
    // The executor resolves provider ids through this mapping. If a sync has
    // already rewritten it to another id, the row is no longer the copy the
    // user acted on.
    if let Some(uid) = expected.uid {
        let key = format!("graph_remote:{}:{uid}", expected.folder_id);
        if let Some(mapped) = repo::sync_state::get(db, &expected.account_id, "mail", &key)? {
            if mapped != remote {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

type DeleteGroups = HashMap<
    (String, String, bool, Option<String>, Option<i64>),
    (Vec<i64>, Vec<String>, Vec<StoredSnapshot>, Vec<String>),
>;
type JunkGroups = HashMap<(String, String, Option<i64>), (Vec<i64>, Vec<String>, Option<String>)>;

impl MailService {
    /// List messages for a folder / unified / search scope.
    ///
    /// Search scopes with `includeServer` ask the IMAP server first; the
    /// returned ids are merged with the local FTS hits so body-only matches are
    /// visible instead of being dropped by the local preview index.
    pub async fn message_list(&self, query: &MessageListQuery) -> Result<MessagePage> {
        // Outbox rows never live in the message table: project them directly.
        if let MailScope::Unified {
            role: FolderRole::Outbox,
        } = &query.scope
        {
            return self.outbox_page(query, None).await;
        }
        if let MailScope::Folder { folder_id } = &query.scope {
            let folder = self.folder(folder_id).await?;
            if folder.role == FolderRole::Outbox {
                return self.outbox_page(query, Some(&folder)).await;
            }
            if let Some(mode) = self.gmail_label_mode(&folder).await? {
                let query_owned = query.clone();
                return self
                    .inner
                    .ctx
                    .db
                    .read(move |db| list_label_messages(db, &query_owned, &folder, &mode))
                    .await;
            }
        }

        let mut server_ids: Vec<String> = Vec::new();
        let mut coverage = None;
        if let MailScope::Search { query: search } = &query.scope {
            coverage = Some(SearchCoverage::LocalOnly);
            if search.include_server {
                match self
                    .cached_server_search(search, query.cursor.is_some())
                    .await
                {
                    Ok((ids, found)) => {
                        server_ids = ids;
                        coverage = Some(found);
                    }
                    Err(error) => {
                        warn!(%error, "server-side search failed");
                        coverage = Some(SearchCoverage::ServerUnavailable {
                            message: error.to_string(),
                        });
                    }
                }
            }
        }
        let query = query.clone();
        if !server_ids.is_empty() {
            if let MailScope::Search { query: search } = &query.scope {
                if !search.text.trim().is_empty() {
                    let mut page = self.merged_search_page(&query, &server_ids, search).await?;
                    page.coverage = coverage;
                    return Ok(page);
                }
            }
        }
        let mut page = self
            .inner
            .ctx
            .db
            .read(move |db| repo::messages::list(db, &query))
            .await?;
        page.coverage = coverage;
        Ok(page)
    }

    /// [`Self::server_search`], reusing the previous result while the same
    /// query is paged (a header scan costs many round trips).
    async fn cached_server_search(
        &self,
        search: &SearchQuery,
        paging: bool,
    ) -> Result<(Vec<String>, SearchCoverage)> {
        let key = serde_json::to_string(search).unwrap_or_default();
        if paging {
            let cache = self.inner.search_cache.lock().expect("search cache lock");
            if let Some(entry) = cache.as_ref() {
                if entry.key == key && entry.stored.elapsed() < SEARCH_CACHE_TTL {
                    return Ok((entry.ids.clone(), entry.coverage.clone()));
                }
            }
        }
        let cancel = {
            let mut current = self.inner.scan_cancel.lock().expect("scan cancel lock");
            current.store(true, std::sync::atomic::Ordering::Relaxed);
            let next = Arc::new(std::sync::atomic::AtomicBool::new(false));
            *current = next.clone();
            next
        };
        let (ids, coverage) = self.server_search(search, &cancel).await?;
        if !cancel.load(std::sync::atomic::Ordering::Relaxed) {
            *self.inner.search_cache.lock().expect("search cache lock") = Some(SearchCacheEntry {
                key,
                stored: std::time::Instant::now(),
                ids: ids.clone(),
                coverage: coverage.clone(),
            });
        }
        Ok((ids, coverage))
    }

    /// Outbox rows for a unified outbox or the virtual per-account folder.
    async fn outbox_page(
        &self,
        query: &MessageListQuery,
        folder: Option<&Folder>,
    ) -> Result<MessagePage> {
        if matches!(
            query.filter,
            MailFilter::Unread | MailFilter::Flagged | MailFilter::MentionsMe
        ) {
            return Ok(MessagePage {
                items: Vec::new(),
                next_cursor: None,
                total: 0,
                coverage: None,
            });
        }
        let account_ids: Vec<String> = match folder {
            Some(folder) => vec![folder.account_id.clone()],
            None => {
                self.inner
                    .ctx
                    .db
                    .read(|db| {
                        let mut statement =
                            db.prepare("SELECT id FROM account WHERE include_in_unified = 1")?;
                        let mut rows = statement.query([])?;
                        let mut ids = Vec::new();
                        while let Some(row) = rows.next()? {
                            ids.push(row.get::<_, String>(0)?);
                        }
                        Ok(ids)
                    })
                    .await?
            }
        };
        let accounts: HashMap<String, nuncio_core::models::Account> = {
            let ids = account_ids.clone();
            self.inner
                .ctx
                .db
                .read(move |db| {
                    let mut map = HashMap::new();
                    for id in ids {
                        if let Some(account) = repo::accounts::get(db, &id)? {
                            map.insert(id, account);
                        }
                    }
                    Ok(map)
                })
                .await?
        };
        let folder_ids: HashMap<String, String> = {
            let ids = account_ids.clone();
            self.inner
                .ctx
                .db
                .read(move |db| {
                    let mut map = HashMap::new();
                    for id in ids {
                        if let Some(folder) = repo::folders::by_role(db, &id, FolderRole::Outbox)? {
                            map.insert(id, folder.id);
                        }
                    }
                    Ok(map)
                })
                .await?
        };
        let mut items = self.outbox_items(account_ids).await?;
        if matches!(query.filter, MailFilter::Attachments) {
            items.clear();
        }
        items.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        let limit = query.limit.clamp(1, 500) as usize;
        let cursor = query
            .cursor
            .as_deref()
            .map(PageCursor::decode)
            .transpose()?;
        let mut summaries: Vec<MessageSummary> = Vec::new();
        for item in &items {
            let Some(account) = accounts.get(&item.account_id) else {
                continue;
            };
            let folder_id = folder
                .map(|folder| folder.id.clone())
                .or_else(|| folder_ids.get(&item.account_id).cloned())
                .unwrap_or_else(|| "outbox".to_string());
            summaries.push(super::outbox::summary_from_item(item, account, &folder_id));
        }
        let total = summaries.len() as i64;
        if let Some(cursor) = &cursor {
            summaries.retain(|item| {
                let key = nuncio_core::repo::dt_string(item.date);
                key < cursor.key || (key == cursor.key && item.id < cursor.id)
            });
        }
        let has_more = summaries.len() > limit;
        summaries.truncate(limit);
        let next_cursor = if has_more {
            summaries
                .last()
                .map(|item| PageCursor::message(item.date, item.id.clone()).encode())
        } else {
            None
        };
        Ok(MessagePage {
            items: summaries,
            next_cursor,
            total,
            coverage: None,
        })
    }

    /// Gmail exposes custom labels as folders. Messages carry every label in
    /// `labels_json`, so a label folder must match label membership rather than
    /// `folder_id` (a message has exactly one primary folder).
    async fn gmail_label_mode(&self, folder: &Folder) -> Result<Option<LabelMode>> {
        let account = self.account(&folder.account_id).await?;
        if !account.capabilities.gmail_extensions && account.provider != ProviderKind::Gmail {
            return Ok(None);
        }
        match folder.role {
            FolderRole::Other => Ok(Some(LabelMode::Label(folder.remote_path.clone()))),
            FolderRole::All => Ok(Some(LabelMode::AllAccount)),
            _ => Ok(None),
        }
    }

    /// Run `UID SEARCH` on the relevant account/folder(s) and cache results.
    /// Accounts whose server cannot search content are header-scanned instead.
    /// Returns the local ids of every server hit and how they were produced.
    async fn server_search(
        &self,
        query: &nuncio_core::models::SearchQuery,
        cancel: &Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<(Vec<String>, SearchCoverage)> {
        let Some(criteria) = crate::sync::messages::imap_search_criteria(query) else {
            return Ok((Vec::new(), SearchCoverage::LocalOnly));
        };
        let scan_terms = crate::sync::messages::ScanTerms::from_query(query);
        let (mut scanned_total, mut scanned_of, mut scanned_any, mut native_any) =
            (0u32, 0u32, false, false);
        let accounts = match query.account_id.as_deref() {
            Some(account_id) => vec![self.account(account_id).await?],
            None => {
                self.inner
                    .ctx
                    .db
                    .read(repo::accounts::list_mail_accounts)
                    .await?
            }
        };
        let mut first_error = None;
        let mut all_ids: Vec<String> = Vec::new();
        for account in accounts {
            if account.imap.is_none() || is_local_only(account.provider) {
                continue;
            }
            // Some servers accept UID SEARCH but cannot answer it: QQ and
            // NetEase return no hits, 139 ignores the criteria and returns
            // every message. Those accounts get a header scan below (content
            // terms only) instead of a SEARCH.
            let scan_instead = !nuncio_core::providers::server_text_search(&account);
            if scan_instead && scan_terms.is_empty() {
                continue;
            }
            let folder = match query.folder_id.as_deref() {
                Some(folder_id) => match self.folder(folder_id).await {
                    Ok(folder) if folder.account_id == account.id => Some(folder),
                    Ok(_) => continue,
                    Err(error) => {
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                        continue;
                    }
                },
                None => {
                    let account_id = account.id.clone();
                    self.inner
                        .ctx
                        .db
                        .read(move |db| repo::folders::by_role(db, &account_id, FolderRole::Inbox))
                        .await?
                }
            };
            let Some(folder) = folder else { continue };
            if scan_instead {
                let scan = match self
                    .inner
                    .engine
                    .server_scan(&account.id, &folder.id, query, cancel.clone())
                    .await
                {
                    Err(error) if matches!(error.code(), "notFound" | "internal") => {
                        match crate::imap::connect_account(&self.inner.ctx, &account).await {
                            Ok(mut connection) => {
                                crate::sync::messages::server_scan(
                                    &self.inner.ctx,
                                    &mut connection,
                                    &account,
                                    &folder,
                                    query,
                                    cancel,
                                )
                                .await
                            }
                            Err(error) => Err(error),
                        }
                    }
                    other => other,
                };
                match scan {
                    Ok(outcome) => {
                        debug!(
                            account = %account.id,
                            folder = %folder.id,
                            scanned = outcome.scanned,
                            total = outcome.total,
                            found = outcome.ids.len(),
                            "header scan finished"
                        );
                        scanned_any = true;
                        scanned_total += outcome.scanned;
                        scanned_of += outcome.total;
                        all_ids.extend(outcome.ids);
                    }
                    Err(error) => {
                        warn!(account = %account.id, %error, "header scan failed");
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                    }
                }
                continue;
            }
            let result = match self
                .inner
                .engine
                .server_search(&account.id, Some(&folder.id), &criteria)
                .await
            {
                Ok(ids) => Ok(ids),
                Err(error) if matches!(error.code(), "notFound" | "internal") => {
                    let mut connection =
                        crate::imap::connect_account(&self.inner.ctx, &account).await?;
                    crate::sync::messages::server_search(
                        &self.inner.ctx,
                        &mut connection,
                        &account,
                        &folder,
                        &criteria,
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            match result {
                Ok(ids) => {
                    debug!(
                        account = %account.id,
                        folder = %folder.id,
                        found = ids.len(),
                        "server search cached messages"
                    );
                    native_any = true;
                    all_ids.extend(ids);
                }
                Err(error) => {
                    warn!(account = %account.id, %error, "server search failed");
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        all_ids.sort();
        all_ids.dedup();
        let coverage = if let Some(error) = first_error {
            SearchCoverage::ServerUnavailable {
                message: error.to_string(),
            }
        } else if scanned_any {
            SearchCoverage::Scanned {
                scanned: scanned_total,
                total: scanned_of,
            }
        } else if native_any {
            SearchCoverage::Server
        } else {
            SearchCoverage::LocalOnly
        };
        Ok((all_ids, coverage))
    }

    /// Merge server search hits with local FTS hits. Used when the local
    /// preview index cannot know that a body-only match exists (the message
    /// body is downloaded lazily).
    async fn merged_search_page(
        &self,
        query: &MessageListQuery,
        server_ids: &[String],
        search: &SearchQuery,
    ) -> Result<MessagePage> {
        let fts = nuncio_core::text::fts_query(&search.text);
        let server: Vec<String> = server_ids.to_vec();
        let search = search.clone();
        let query = query.clone();
        self.inner
            .ctx
            .db
            .read(move |db| {
                let mut ids: Vec<String> = Vec::new();
                let mut seen: HashSet<String> = HashSet::new();
                for id in &server {
                    if seen.insert(id.clone()) {
                        ids.push(id.clone());
                    }
                }
                if !fts.is_empty() {
                    let mut statement =
                        db.prepare("SELECT message_id FROM fts_mail WHERE fts_mail MATCH ?1")?;
                    let mut rows = statement.query(rusqlite::params![fts])?;
                    while let Some(row) = rows.next()? {
                        let id: String = row.get(0)?;
                        if seen.insert(id.clone()) {
                            ids.push(id);
                        }
                    }
                }
                // Bound the in-memory merge; normal searches are far smaller.
                ids.truncate(MAX_MERGED_CANDIDATES);

                let account_emails: HashMap<String, String> = {
                    let mut statement = db.prepare("SELECT id, lower(email) FROM account")?;
                    let mut rows = statement.query([])?;
                    let mut map = HashMap::new();
                    while let Some(row) = rows.next()? {
                        map.insert(row.get::<_, String>(0)?, row.get::<_, String>(1)?);
                    }
                    map
                };
                let mut items = fetch_summaries(db, &ids)?;
                items.retain(|summary| {
                    let account_email = account_emails.get(&summary.account_id);
                    search_matches(
                        summary,
                        &search,
                        query.filter,
                        account_email.map(String::as_str),
                    )
                });
                items.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| b.id.cmp(&a.id)));

                let limit = query.limit.clamp(1, 500) as usize;
                let cursor = query
                    .cursor
                    .as_deref()
                    .map(PageCursor::decode)
                    .transpose()?;

                if query.conversation {
                    let mut threads: HashMap<(String, String), Vec<MessageSummary>> =
                        HashMap::new();
                    for item in items {
                        threads
                            .entry((item.account_id.clone(), item.thread_id.clone()))
                            .or_default()
                            .push(item);
                    }
                    let mut representatives: Vec<MessageSummary> = Vec::new();
                    for (_, members) in threads {
                        let count = members.len() as i64;
                        let unread = members.iter().filter(|item| !item.seen).count() as i64;
                        if let Some(mut representative) = members.into_iter().next() {
                            representative.thread_count = count;
                            representative.thread_unread = unread;
                            representatives.push(representative);
                        }
                    }
                    representatives
                        .sort_by(|a, b| b.date.cmp(&a.date).then_with(|| b.id.cmp(&a.id)));
                    if let Some(cursor) = &cursor {
                        representatives.retain(|item| after_cursor(item, cursor));
                    }
                    let total = representatives.len() as i64;
                    let has_more = representatives.len() > limit;
                    representatives.truncate(limit);
                    let next_cursor = if has_more {
                        representatives
                            .last()
                            .map(|item| PageCursor::message(item.date, item.id.clone()).encode())
                    } else {
                        None
                    };
                    Ok(MessagePage {
                        items: representatives,
                        next_cursor,
                        total,
                        coverage: None,
                    })
                } else {
                    if let Some(cursor) = &cursor {
                        items.retain(|item| after_cursor(item, cursor));
                    }
                    let total = items.len() as i64;
                    let has_more = items.len() > limit;
                    items.truncate(limit);
                    let next_cursor = if has_more {
                        items
                            .last()
                            .map(|item| PageCursor::message(item.date, item.id.clone()).encode())
                    } else {
                        None
                    };
                    Ok(MessagePage {
                        items,
                        next_cursor,
                        total,
                        coverage: None,
                    })
                }
            })
            .await
    }

    /// Conversation members, oldest first.
    pub async fn message_thread(
        &self,
        account_id: &str,
        thread_id: &str,
    ) -> Result<Vec<MessageSummary>> {
        let account_id = account_id.to_string();
        let thread_id = thread_id.to_string();
        self.inner
            .ctx
            .db
            .read(move |db| repo::messages::thread(db, &account_id, &thread_id))
            .await
    }

    /// Mark a message as seen when it is opened.
    ///
    /// The local value, the queued command and the durable writer are one
    /// transaction. Splitting them let a user action that committed in between
    /// be overtaken by this (older) intent: the writer then named the automatic
    /// operation even though the user had already chosen another value.
    ///
    /// Both the provider copy and the message's flag revision observed when
    /// the open started are re-checked inside the transaction. If either
    /// changed, a sync rebound the row or a newer action ran first, and this
    /// open must not overwrite it.
    ///
    /// The revision is message-level, not the per-field writer `seq`: an undo
    /// with no predecessor deletes the writer row, so comparing `Option<seq>`
    /// would see `None == None` and overwrite the value the undo just restored.
    pub(crate) async fn mark_seen_on_open(
        &self,
        stored: &mut StoredMessage,
        opened_copy: Option<String>,
        observed_flag_revision: Option<i64>,
    ) -> Result<()> {
        if stored.summary.seen {
            return Ok(());
        }
        let account = self.account(&stored.summary.account_id).await?;
        let message_id = stored.summary.id.clone();
        let folder_id = stored.summary.folder_id.clone();
        let account_id = account.id.clone();
        let local_only = is_local_only(account.provider);
        let local_owner = local_only.then(nuncio_core::models::new_id);
        let message_for_tx = message_id.clone();
        let outcome = self
            .inner
            .ctx
            .db
            .write(move |db| {
                let Some(current) = repo::messages::get(db, &message_for_tx)? else {
                    return Ok(None);
                };
                if repo::messages::body_identity_key(&current) != opened_copy {
                    // The row now names another provider copy: a sync rebound
                    // it while the body was loading. The user opened the copy
                    // observed at the start, so marking this one read on its
                    // behalf would write a flag for a message they never read.
                    return Ok(Some(false));
                }
                if current.summary.seen {
                    // Already read (a sync or a newer action got there first).
                    return Ok(Some(true));
                }
                let current_revision = repo::messages::flag_revision(db, &message_for_tx)?;
                if current_revision != observed_flag_revision {
                    // A newer user action committed while this message was
                    // being opened. Its intent wins; leave the row alone.
                    return Ok(Some(false));
                }
                let previous = current.uid.map(|uid| FlagRollback {
                    uid,
                    seen: current.summary.seen,
                    flagged: current.summary.flagged,
                    answered: current.summary.answered,
                    forwarded: current.summary.forwarded,
                });
                repo::messages::set_flags(
                    db,
                    std::slice::from_ref(&message_for_tx),
                    Some(true),
                    None,
                    None,
                    None,
                )?;
                let uid = current.uid;
                if local_only || uid.is_none() {
                    // No server command, but the local write still owns the
                    // field so a later undo cannot clobber a newer action.
                    let owner = local_owner.unwrap_or_else(nuncio_core::models::new_id);
                    repo::messages::record_flag_writers(
                        db,
                        &message_for_tx,
                        &owner,
                        "setSeen",
                        Some(true),
                        None,
                        None,
                        None,
                    )?;
                    return Ok(Some(true));
                }
                let payload = SetFlagsOp {
                    folder_id: current.summary.folder_id.clone(),
                    uids: vec![uid.unwrap_or_default()],
                    seen: Some(true),
                    flagged: None,
                    answered: None,
                    forwarded: None,
                    source_uidvalidity: current.uidvalidity,
                    previous: previous.into_iter().collect(),
                    message_ids: vec![message_for_tx.clone()],
                    outcomes: Vec::new(),
                    // Pin the Graph copy this open addressed; undo must not
                    // re-resolve the synthetic UID to whatever the row names
                    // after a rebind.
                    remote_ids: current
                        .gm_msgid
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(|value| vec![value.to_string()])
                        .unwrap_or_default(),
                };
                let op = repo::pending_ops::enqueue(
                    db,
                    NewPendingOp::new(
                        account_id,
                        KIND_SET_FLAGS,
                        serde_json::to_value(payload).map_err(|error| {
                            AppError::internal(format!("cannot encode setFlags: {error}"))
                        })?,
                        "Mark message as read",
                    ),
                )?;
                repo::messages::record_flag_writers(
                    db,
                    &message_for_tx,
                    &op.id,
                    "setSeen",
                    Some(true),
                    None,
                    None,
                    None,
                )?;
                Ok(Some(true))
            })
            .await?;
        match outcome {
            Some(true) => {
                stored.summary.seen = true;
                self.emit_changed(
                    &account.id,
                    vec![folder_id],
                    vec![stored.summary.id.clone()],
                    MailChangeKind::Updated,
                );
                self.update_badge().await;
            }
            Some(false) => {
                // Reflect the value the newer action left behind.
                let id = stored.summary.id.clone();
                if let Some(fresh) = self
                    .inner
                    .ctx
                    .db
                    .read(move |db| repo::messages::get(db, &id))
                    .await?
                {
                    stored.summary.seen = fresh.summary.seen;
                }
            }
            None => {}
        }
        Ok(())
    }

    /// Apply an action to one or more messages: local state changes are
    /// immediate, the server work is queued, and an undo entry is recorded.
    ///
    /// Returns the undo entry id, or an empty string when the action recorded
    /// nothing reversible (for example discarding a queued outbox item).
    pub async fn message_act(&self, ids: &[String], action: &MessageAction) -> Result<String> {
        if ids.is_empty() {
            return Ok(String::new());
        }
        // Synthetically projected outbox rows are not in the message table.
        // Deleting one discards the queued message; flags/moves do not apply.
        let outbox_ids: Vec<&String> = ids
            .iter()
            .filter(|id| Self::outbox_item_id(id).is_some())
            .collect();
        if !outbox_ids.is_empty() {
            if outbox_ids.len() != ids.len() {
                return Err(AppError::invalid_input(
                    "outbox items cannot be mixed with normal messages",
                ));
            }
            match action {
                MessageAction::Delete { .. } => {
                    for id in outbox_ids {
                        self.outbox_discard(id).await?;
                    }
                    return Ok(String::new());
                }
                MessageAction::SetSeen { .. } | MessageAction::SetFlagged { .. } => {
                    // Outbox rows are always "read" and cannot be flagged.
                    return Ok(String::new());
                }
                _ => {
                    return Err(AppError::invalid_input(
                        "queued outbox messages can only be discarded",
                    ))
                }
            }
        }
        let mut messages: Vec<StoredMessage> = Vec::new();
        for id in ids {
            let id_owned = id.clone();
            if let Some(stored) = self
                .inner
                .ctx
                .db
                .read(move |db| repo::messages::get(db, &id_owned))
                .await?
            {
                messages.push(stored);
            }
        }
        if messages.is_empty() {
            return Err(AppError::not_found("none of the messages exist"));
        }

        // Load accounts once.
        let mut accounts: HashMap<String, nuncio_core::models::Account> = HashMap::new();
        for message in &messages {
            if !accounts.contains_key(&message.summary.account_id) {
                let account = self.account(&message.summary.account_id).await?;
                accounts.insert(account.id.clone(), account);
            }
        }

        let mut role_cache: HashMap<(String, FolderRole), Option<String>> = HashMap::new();
        let mut targets: Vec<Target> = Vec::with_capacity(messages.len());
        for message in messages {
            let account = accounts
                .get(&message.summary.account_id)
                .cloned()
                .ok_or_else(|| AppError::internal("account vanished during action"))?;
            let mut target_folder = None;
            let mut permanent = false;
            match action {
                MessageAction::SetSeen { .. } | MessageAction::SetFlagged { .. } => {}
                MessageAction::Move { folder_id } => {
                    let folder = self.folder(folder_id).await?;
                    if folder.account_id != account.id {
                        return Err(AppError::invalid_input(
                            "cannot move a message to another account",
                        ));
                    }
                    if folder.id != message.summary.folder_id {
                        target_folder = Some(folder.id);
                    }
                }
                MessageAction::Archive => {
                    let archive = self
                        .role_folder(&account, FolderRole::Archive, &mut role_cache)
                        .await?;
                    let all = self
                        .role_folder(&account, FolderRole::All, &mut role_cache)
                        .await?;
                    target_folder = crate::ops::archive_target(
                        account.capabilities.gmail_extensions,
                        archive.as_deref(),
                        all.as_deref(),
                    );
                    if target_folder.as_deref() == Some(message.summary.folder_id.as_str()) {
                        target_folder = None;
                    }
                }
                MessageAction::Delete {
                    permanent: requested,
                } => {
                    let source_role = self
                        .inner
                        .ctx
                        .db
                        .read({
                            let folder_id = message.summary.folder_id.clone();
                            move |db| {
                                Ok(repo::folders::get(db, &folder_id)?.map(|folder| folder.role))
                            }
                        })
                        .await?
                        .unwrap_or(FolderRole::Other);
                    permanent =
                        delete_is_permanent(*requested, source_role, message.summary.is_draft);
                    if !permanent {
                        target_folder = self
                            .role_folder(&account, FolderRole::Trash, &mut role_cache)
                            .await?;
                        if target_folder.as_deref() == Some(message.summary.folder_id.as_str()) {
                            permanent = true;
                            target_folder = None;
                        }
                    }
                }
                MessageAction::MarkJunk { value } => {
                    let role = if *value {
                        FolderRole::Junk
                    } else {
                        FolderRole::Inbox
                    };
                    let found = self.role_folder(&account, role, &mut role_cache).await?;
                    let Some(found) = found else {
                        return Err(AppError::unsupported(if *value {
                            "this account has no junk folder"
                        } else {
                            "this account has no inbox folder"
                        }));
                    };
                    if found == message.summary.folder_id {
                        continue;
                    }
                    target_folder = Some(found);
                }
            }
            if matches!(action, MessageAction::Move { .. }) && target_folder.is_none() {
                // Moving to the same folder is a no-op.
                continue;
            }
            targets.push(Target {
                message,
                account,
                target_folder,
                permanent,
            });
        }
        if targets.is_empty() {
            return Ok(String::new());
        }

        // ── prepare local groups and server payloads ─────────────────
        let action_kind = match action {
            MessageAction::SetSeen { .. } => "setSeen",
            MessageAction::SetFlagged { .. } => "setFlagged",
            MessageAction::Move { .. } => "move",
            MessageAction::Archive => "archive",
            MessageAction::Delete { .. } => "delete",
            MessageAction::MarkJunk { .. } => "markJunk",
        };
        let action_value = match action {
            MessageAction::SetSeen { value } | MessageAction::SetFlagged { value } => Some(*value),
            MessageAction::MarkJunk { value } => Some(*value),
            _ => None,
        };

        let seen_value = match action {
            MessageAction::SetSeen { value } => Some(*value),
            _ => None,
        };
        let flagged_value = match action {
            MessageAction::SetFlagged { value } => Some(*value),
            _ => None,
        };
        // A flag action on a local/POP3 row, or on a row without a server UID,
        // queues no operation. Its local write still needs a durable owner so a
        // later undo cannot clobber a newer action's value.
        let local_flag_ids: Vec<String> = if matches!(action_kind, "setSeen" | "setFlagged") {
            targets
                .iter()
                .filter(|target| {
                    is_local_only(target.account.provider) || target.message.uid.is_none()
                })
                .map(|target| target.message.summary.id.clone())
                .collect()
        } else {
            Vec::new()
        };
        let flag_writer_id =
            matches!(action_kind, "setSeen" | "setFlagged").then(nuncio_core::models::new_id);

        // Only accounts that queue a remote operation get the optimistic
        // move treatment (UID cleared, `target_pending` set). Local/POP3
        // messages, and remote rows without a server UID (Graph, never
        // uploaded drafts), are moved plainly: there is no target UID to wait
        // for, and marking them would block every later action forever.
        // Permanent deletes split by whether a server command will be queued:
        // a remote row is only hidden (full state kept) until the command is
        // acknowledged, while a row with no server identity is erased at once.
        let mut permanent_ids: Vec<String> = Vec::new();
        let mut pending_permanent_ids: Vec<String> = Vec::new();
        let mut hidden_ids: Vec<String> = Vec::new();
        let mut local_moves: HashMap<String, Vec<String>> = HashMap::new();
        let mut remote_moves: HashMap<String, Vec<String>> = HashMap::new();
        let mut flag_ids: Vec<String> = Vec::new();
        for target in &targets {
            let queues_remote_move =
                !is_local_only(target.account.provider) && target.message.uid.is_some();
            match action_kind {
                "setSeen" | "setFlagged" => flag_ids.push(target.message.summary.id.clone()),
                "delete" if target.permanent => {
                    if queues_remote_move {
                        pending_permanent_ids.push(target.message.summary.id.clone());
                    } else {
                        permanent_ids.push(target.message.summary.id.clone());
                    }
                }
                "delete" => {
                    if let Some(folder_id) = &target.target_folder {
                        let group = if queues_remote_move {
                            &mut remote_moves
                        } else {
                            &mut local_moves
                        };
                        group
                            .entry(folder_id.clone())
                            .or_default()
                            .push(target.message.summary.id.clone());
                    } else {
                        // No Trash folder: hide the row until the server
                        // confirms the permanent delete.
                        hidden_ids.push(target.message.summary.id.clone());
                    }
                }
                _ => {
                    if let Some(folder_id) = &target.target_folder {
                        let group = if queues_remote_move {
                            &mut remote_moves
                        } else {
                            &mut local_moves
                        };
                        group
                            .entry(folder_id.clone())
                            .or_default()
                            .push(target.message.summary.id.clone());
                    }
                }
            }
        }

        let prepared: Vec<PreparedOp> = match action {
            MessageAction::SetSeen { value } => self.prepare_flag_ops(
                &targets,
                Some(*value),
                None,
                if *value {
                    "Mark as read"
                } else {
                    "Mark as unread"
                },
            )?,
            MessageAction::SetFlagged { value } => self.prepare_flag_ops(
                &targets,
                None,
                Some(*value),
                if *value {
                    "Flag message"
                } else {
                    "Unflag message"
                },
            )?,
            MessageAction::Move { .. } => self.prepare_move_ops(&targets, "Move message")?,
            MessageAction::Archive => self.prepare_archive_ops(&targets)?,
            MessageAction::Delete { permanent } => self.prepare_delete_ops(
                &targets,
                if *permanent {
                    "Delete message permanently"
                } else {
                    "Delete message"
                },
            )?,
            MessageAction::MarkJunk { value } => self.prepare_junk_ops(
                &targets,
                *value,
                if *value {
                    "Mark as junk"
                } else {
                    "Mark as not junk"
                },
            )?,
        };

        // The queue check, the local change, the optimistic move and the
        // queue rows all run in one write transaction. A row that enters
        // `target_pending` always has the operation that will release it, and
        // two concurrent actions cannot both pass the pending check with the
        // same stale source identity.
        let pending_ids: Vec<String> = targets
            .iter()
            .filter(|target| !is_local_only(target.account.provider))
            .map(|target| target.message.summary.id.clone())
            .collect();
        // Only rows that will actually queue a server command need a pinned
        // provider identity. A row without a UID (for example an unsynced
        // Graph draft) is changed locally and must stay actionable.
        let expectations: Vec<RemoteExpectation> = targets
            .iter()
            .filter(|target| {
                !is_local_only(target.account.provider) && target.message.uid.is_some()
            })
            .map(|target| RemoteExpectation {
                id: target.message.summary.id.clone(),
                account_id: target.account.id.clone(),
                folder_id: target.message.summary.folder_id.clone(),
                uid: target.message.uid,
                uidvalidity: target.message.uidvalidity,
                graph: crate::graph::uses_graph(&target.account),
                remote_id: crate::graph::uses_graph(&target.account)
                    .then(|| target.message.gm_msgid.clone())
                    .flatten()
                    .filter(|value| !value.trim().is_empty()),
            })
            .collect();
        let flags = flag_ids;
        let deletes = permanent_ids.clone();
        let pending_deletes = pending_permanent_ids.clone();
        let hidden = hidden_ids.clone();
        let local_move_groups = local_moves;
        let remote_move_groups = remote_moves;
        let local_flag_ids_for_tx = local_flag_ids.clone();
        let flag_writer_id_for_tx = flag_writer_id.clone();
        let op_ids = self
            .inner
            .ctx
            .db
            .write(move |db| {
                // A row marked `target_pending` is an optimistic move whose
                // server copy has not been mapped yet. Refuse inside the
                // transaction: otherwise the local change would report success
                // while the remote queue silently skipped it (and the next
                // sync would resurrect the message in its old folder).
                let pending = repo::messages::target_pending_ids(db, &pending_ids)?;
                if let Some(id) = pending.first() {
                    return Err(AppError::Conflict(format!(
                        "message {id} is still being moved to its new folder; retry after the sync finishes"
                    )));
                }
                // The payloads were built from rows read before this
                // transaction. Re-check that none of them moved or changed
                // generation in the meantime, so the queued UID really
                // addresses the message the user acted on.
                for expected in &expectations {
                    if !remote_expectation_matches(db, expected)? {
                        return Err(AppError::Conflict(format!(
                            "message {} changed while the action was being prepared; retry",
                            expected.id
                        )));
                    }
                }
                if !flags.is_empty() {
                    repo::messages::set_flags(db, &flags, seen_value, flagged_value, None, None)?;
                }
                if !deletes.is_empty() {
                    repo::messages::delete_local(db, &deletes, true)?;
                }
                if !pending_deletes.is_empty() {
                    // Keep the row, its attachments and its FTS document until
                    // the server acknowledges the delete; a refused command
                    // unhides it again instead of rebuilding it.
                    repo::messages::hide_for_permanent_delete(db, &pending_deletes)?;
                }
                if !hidden.is_empty() {
                    repo::messages::delete_local(db, &hidden, false)?;
                }
                for (folder_id, ids) in local_move_groups {
                    repo::messages::move_to_folder(db, &ids, &folder_id)?;
                }
                for (folder_id, ids) in remote_move_groups {
                    repo::messages::move_to_folder_pending(db, &ids, &folder_id)?;
                }
                // Record the per-field writer in the same transaction as the
                // local value and the queue row. Execution and rollback only
                // restore a field this exact owner still holds.
                if let Some(owner) = flag_writer_id_for_tx.as_deref() {
                    for id in &local_flag_ids_for_tx {
                        repo::messages::record_flag_writers(
                            db,
                            id,
                            owner,
                            action_kind,
                            seen_value,
                            flagged_value,
                            None,
                            None,
                        )?;
                    }
                }
                let mut op_ids = Vec::with_capacity(prepared.len());
                for op in prepared {
                    let kind = op.kind;
                    let payload = op.payload.clone();
                    let stored = repo::pending_ops::enqueue(
                        db,
                        NewPendingOp::new(op.account_id, op.kind, payload.clone(), op.description),
                    )?;
                    // The optimistic move above already put the row in its
                    // destination. Record this operation as the owner of the
                    // identity it is waiting for, so a retry of an earlier
                    // batch cannot fill the slot with a stale destination.
                    crate::ops::claim_move_target_owner(
                        db,
                        &stored.account_id,
                        kind,
                        &payload,
                        &stored.id,
                        stored.sequence,
                    )?;
                    if kind == KIND_SET_FLAGS {
                        if let Ok(payload) = serde_json::from_value::<SetFlagsOp>(payload) {
                            for message_id in &payload.message_ids {
                                repo::messages::record_flag_writers(
                                    db,
                                    message_id,
                                    &stored.id,
                                    action_kind,
                                    payload.seen,
                                    payload.flagged,
                                    payload.answered,
                                    payload.forwarded,
                                )?;
                            }
                        }
                    }
                    op_ids.push(stored.id);
                }
                Ok(op_ids)
            })
            .await?;

        // ── undo entry ───────────────────────────────────────────────
        let attachment_ids: Vec<String> = targets
            .iter()
            .map(|target| target.message.summary.id.clone())
            .collect();
        let mut attachment_map: HashMap<String, Vec<Attachment>> = self
            .inner
            .ctx
            .db
            .read(move |db| {
                let mut map = HashMap::new();
                for id in attachment_ids {
                    map.insert(id.clone(), repo::attachments::list(db, &id)?);
                }
                Ok(map)
            })
            .await?;
        let entries: Vec<UndoEntryState> = targets
            .iter()
            .map(|target| UndoEntryState {
                id: target.message.summary.id.clone(),
                account_id: target.account.id.clone(),
                folder_id: target.message.summary.folder_id.clone(),
                target_folder_id: target.target_folder.clone(),
                uid: target.message.uid,
                permanent: target.permanent,
                seen: target.message.summary.seen,
                flagged: target.message.summary.flagged,
                answered: target.message.summary.answered,
                forwarded: target.message.summary.forwarded,
                is_deleted_local: target.message.is_deleted_local,
                snapshot: StoredSnapshot::from(&target.message),
                flag_writer_id: flag_writer_id.clone(),
                attachments: attachment_map
                    .remove(&target.message.summary.id)
                    .unwrap_or_default(),
            })
            .collect();
        let primary_account = entries
            .first()
            .map(|entry| entry.account_id.clone())
            .unwrap_or_default();
        let payload = UndoPayload {
            action: action_kind.to_string(),
            value: action_value,
            entries: entries.clone(),
            op_ids: op_ids.clone(),
        };
        let description = match action {
            MessageAction::SetSeen { value } => {
                if *value {
                    "Mark as read".to_string()
                } else {
                    "Mark as unread".to_string()
                }
            }
            MessageAction::SetFlagged { value } => {
                if *value {
                    "Flag message".to_string()
                } else {
                    "Unflag message".to_string()
                }
            }
            MessageAction::Move { .. } => "Move message".to_string(),
            MessageAction::Archive => "Archive message".to_string(),
            MessageAction::Delete { permanent } => {
                if *permanent {
                    "Delete message permanently".to_string()
                } else {
                    "Delete message".to_string()
                }
            }
            MessageAction::MarkJunk { value } => {
                if *value {
                    "Mark as junk".to_string()
                } else {
                    "Mark as not junk".to_string()
                }
            }
        };
        let account_for_undo = primary_account.clone();
        let description_for_db = description.clone();
        let value_for_db = serde_json::to_value(&payload)
            .map_err(|error| AppError::internal(format!("cannot encode undo entry: {error}")))?;
        let undo_id = self
            .inner
            .ctx
            .db
            .write(move |db| {
                Ok(repo::undo::push(
                    db,
                    &account_for_undo,
                    action_kind,
                    &description_for_db,
                    &value_for_db,
                )?
                .id)
            })
            .await?;

        // ── events ───────────────────────────────────────────────────
        let mut per_account: HashMap<String, (HashSet<String>, Vec<String>)> = HashMap::new();
        for entry in &entries {
            let bucket = per_account.entry(entry.account_id.clone()).or_default();
            bucket.0.insert(entry.folder_id.clone());
            if let Some(target) = &entry.target_folder_id {
                bucket.0.insert(target.clone());
            }
            bucket.1.push(entry.id.clone());
        }
        let kind = if permanent_ids.is_empty() && hidden_ids.is_empty() {
            MailChangeKind::Updated
        } else {
            MailChangeKind::Deleted
        };
        for (account_id, (folders, message_ids)) in per_account {
            events::emit_mail_changed(
                self.inner.ctx.events.as_ref(),
                account_id,
                folders.into_iter().collect(),
                message_ids,
                kind,
            );
        }
        self.update_badge().await;
        debug!(
            count = entries.len(),
            kind = action_kind,
            ops = op_ids.len(),
            "applied message action"
        );
        Ok(undo_id)
    }

    async fn role_folder(
        &self,
        account: &nuncio_core::models::Account,
        role: FolderRole,
        cache: &mut HashMap<(String, FolderRole), Option<String>>,
    ) -> Result<Option<String>> {
        if let Some(value) = cache.get(&(account.id.clone(), role)) {
            return Ok(value.clone());
        }
        let account_id = account.id.clone();
        let folder = self
            .inner
            .ctx
            .db
            .read(move |db| repo::folders::by_role(db, &account_id, role))
            .await?;
        let value = folder.map(|folder| folder.id);
        cache.insert((account.id.clone(), role), value.clone());
        Ok(value)
    }

    fn prepare_flag_ops(
        &self,
        targets: &[Target],
        seen: Option<bool>,
        flagged: Option<bool>,
        description: &str,
    ) -> Result<Vec<PreparedOp>> {
        // Group by (account, folder, UIDVALIDITY): a payload must never carry
        // UIDs from two generations, or a valid row would vouch for a stale
        // one. Rows with no generation form their own group and are refused by
        // the executor (which reconciles the folder).
        let mut grouped: FlagGroups = HashMap::new();
        for target in targets {
            if is_local_only(target.account.provider) {
                continue;
            }
            let Some(uid) = target.message.uid else {
                continue;
            };
            let entry = grouped
                .entry((
                    target.account.id.clone(),
                    target.message.summary.folder_id.clone(),
                    target.message.uidvalidity,
                ))
                .or_default();
            entry.0.push(uid);
            entry.1.push(FlagRollback {
                uid,
                seen: target.message.summary.seen,
                flagged: target.message.summary.flagged,
                answered: target.message.summary.answered,
                forwarded: target.message.summary.forwarded,
            });
            entry.2.push(target.message.summary.id.clone());
            entry.3.push(
                target
                    .message
                    .gm_msgid
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or_default()
                    .to_string(),
            );
        }
        let mut prepared = Vec::new();
        for (
            (account_id, folder_id, source_uidvalidity),
            (uids, previous, message_ids, remote_ids),
        ) in grouped
        {
            let payload = SetFlagsOp {
                folder_id,
                uids,
                seen,
                flagged,
                answered: None,
                forwarded: None,
                source_uidvalidity,
                previous,
                message_ids,
                outcomes: Vec::new(),
                remote_ids,
            };
            prepared.push(PreparedOp {
                account_id,
                kind: KIND_SET_FLAGS,
                payload: serde_json::to_value(payload).map_err(|error| {
                    AppError::internal(format!("cannot encode setFlags: {error}"))
                })?,
                description: description.to_string(),
            });
        }
        Ok(prepared)
    }

    fn prepare_move_ops(&self, targets: &[Target], description: &str) -> Result<Vec<PreparedOp>> {
        let mut grouped: MoveGroups = HashMap::new();
        for target in targets {
            if is_local_only(target.account.provider) {
                continue;
            }
            let (Some(uid), Some(target_folder)) =
                (target.message.uid, target.target_folder.clone())
            else {
                continue;
            };
            let entry = grouped
                .entry((
                    target.account.id.clone(),
                    target.message.summary.folder_id.clone(),
                    target_folder,
                    target.message.uidvalidity,
                ))
                .or_default();
            entry.0.push(uid);
            entry.1.push(target.message.summary.id.clone());
        }
        let mut prepared = Vec::new();
        for ((account_id, folder_id, target_folder_id, source_uidvalidity), (uids, message_ids)) in
            grouped
        {
            let payload = MoveOp {
                folder_id,
                target_folder_id,
                uids,
                message_ids,
                source_uidvalidity,
                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            prepared.push(PreparedOp {
                account_id,
                kind: KIND_MOVE,
                payload: serde_json::to_value(payload)
                    .map_err(|error| AppError::internal(format!("cannot encode move: {error}")))?,
                description: description.to_string(),
            });
        }
        Ok(prepared)
    }

    fn prepare_archive_ops(&self, targets: &[Target]) -> Result<Vec<PreparedOp>> {
        let mut grouped: ArchiveGroups = HashMap::new();
        for target in targets {
            if is_local_only(target.account.provider) {
                continue;
            }
            let Some(uid) = target.message.uid else {
                continue;
            };
            let entry = grouped
                .entry((
                    target.account.id.clone(),
                    target.message.summary.folder_id.clone(),
                    target.message.uidvalidity,
                ))
                .or_default();
            entry.0.push(uid);
            entry.1.push(target.message.summary.id.clone());
            if entry.2.is_none() {
                entry.2 = target.target_folder.clone();
            }
        }
        let mut prepared = Vec::new();
        for ((account_id, folder_id, source_uidvalidity), (uids, message_ids, target_folder_id)) in
            grouped
        {
            let payload = ArchiveOp {
                folder_id,
                uids,
                target_folder_id,
                soft_hidden: false,
                message_ids,
                source_uidvalidity,
                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            prepared.push(PreparedOp {
                account_id,
                kind: KIND_ARCHIVE,
                payload: serde_json::to_value(payload).map_err(|error| {
                    AppError::internal(format!("cannot encode archive: {error}"))
                })?,
                description: "Archive message".to_string(),
            });
        }
        Ok(prepared)
    }

    fn prepare_delete_ops(&self, targets: &[Target], description: &str) -> Result<Vec<PreparedOp>> {
        let mut grouped: DeleteGroups = HashMap::new();
        for target in targets {
            if is_local_only(target.account.provider) {
                continue;
            }
            let Some(uid) = target.message.uid else {
                continue;
            };
            let entry = grouped
                .entry((
                    target.account.id.clone(),
                    target.message.summary.folder_id.clone(),
                    target.permanent,
                    target.target_folder.clone(),
                    target.message.uidvalidity,
                ))
                .or_default();
            entry.0.push(uid);
            entry.1.push(target.message.summary.id.clone());
            if target.permanent {
                // The optimistic permanent delete now hides the row instead of
                // removing it, but keep its contents as a fallback: if the row
                // disappears for any other reason, a refused command can still
                // rebuild it even when it is older than the sync window.
                entry.2.push(StoredSnapshot::from(&target.message));
            }
            // Pin the exact provider id this target was read with. Graph ids
            // change without changing the local row (and its synthetic UID),
            // so execution must address this copy, never a re-resolved one.
            entry.3.push(if crate::graph::uses_graph(&target.account) {
                target.message.gm_msgid.clone().unwrap_or_default()
            } else {
                String::new()
            });
        }
        let mut prepared = Vec::new();
        for (
            (account_id, folder_id, permanent, target_folder_id, source_uidvalidity),
            (uids, message_ids, snapshot, remote_ids),
        ) in grouped
        {
            let payload = DeleteOp {
                folder_id,
                uids,
                permanent,
                soft_hidden: !permanent && target_folder_id.is_none(),
                target_folder_id,
                message_ids,
                source_uidvalidity,
                pending_delete: permanent,
                deleted_flag_set: false,
                snapshot,
                outcomes: Vec::new(),
                remote_ids,
            };
            prepared.push(PreparedOp {
                account_id,
                kind: KIND_DELETE,
                payload: serde_json::to_value(payload).map_err(|error| {
                    AppError::internal(format!("cannot encode delete: {error}"))
                })?,
                description: description.to_string(),
            });
        }
        Ok(prepared)
    }

    fn prepare_junk_ops(
        &self,
        targets: &[Target],
        value: bool,
        description: &str,
    ) -> Result<Vec<PreparedOp>> {
        let mut grouped: JunkGroups = HashMap::new();
        for target in targets {
            if is_local_only(target.account.provider) {
                continue;
            }
            let Some(uid) = target.message.uid else {
                continue;
            };
            let entry = grouped
                .entry((
                    target.account.id.clone(),
                    target.message.summary.folder_id.clone(),
                    target.message.uidvalidity,
                ))
                .or_default();
            entry.0.push(uid);
            entry.1.push(target.message.summary.id.clone());
            if entry.2.is_none() {
                entry.2 = target.target_folder.clone();
            }
        }
        let mut prepared = Vec::new();
        for ((account_id, folder_id, source_uidvalidity), (uids, message_ids, target_folder_id)) in
            grouped
        {
            let payload = MarkJunkOp {
                folder_id,
                uids,
                value,
                target_folder_id,
                message_ids,
                source_uidvalidity,
                outcomes: Vec::new(),
                remote_ids: Vec::new(),
            };
            prepared.push(PreparedOp {
                account_id,
                kind: KIND_MARK_JUNK,
                payload: serde_json::to_value(payload).map_err(|error| {
                    AppError::internal(format!("cannot encode markJunk: {error}"))
                })?,
                description: description.to_string(),
            });
        }
        Ok(prepared)
    }

    /// Apply an action to every message of the given conversations.
    ///
    /// Returns the undo entry id like [`Self::message_act`].
    pub async fn message_act_threads(
        &self,
        account_id: &str,
        thread_ids: &[String],
        action: &MessageAction,
    ) -> Result<String> {
        if thread_ids.is_empty() {
            return Ok(String::new());
        }
        let mut ids: Vec<String> = Vec::new();
        for thread_id in thread_ids {
            let account_id_owned = account_id.to_string();
            let thread_id_owned = thread_id.clone();
            let mut found = self
                .inner
                .ctx
                .db
                .read(move |db| {
                    repo::messages::thread_message_ids(db, &account_id_owned, &thread_id_owned)
                })
                .await?;
            ids.append(&mut found);
        }
        ids.sort();
        ids.dedup();
        self.message_act(&ids, action).await
    }

    /// Revert the most recent undoable action. Returns `false` when there is
    /// nothing to undo (or the action can no longer be reverted).
    pub async fn message_undo_last(&self) -> Result<bool> {
        self.undo_last().await
    }

    /// Revert one specific action by the undo id returned from `message_act`.
    pub async fn message_undo(&self, undo_id: &str) -> Result<bool> {
        self.undo_by_id(undo_id).await
    }

    /// Re-run the targeted reconciliation for messages the UI marks as
    /// `reconcileReason`.
    ///
    /// This is the safe retry path for a message that could not be resolved
    /// automatically (an unreliable HEADER search, a missing Message-ID, an
    /// ambiguous copy). It never deletes anything by itself: it re-synchronizes
    /// the owning folder(s), and the sync's reconciliation pass applies the
    /// same evidence rules as before. Returns how many rows were retried.
    pub async fn message_reconcile(&self, ids: &[String]) -> Result<usize> {
        let mut folders: HashMap<String, String> = HashMap::new();
        let mut retried = 0;
        for id in ids {
            let info = self
                .inner
                .ctx
                .db
                .read({
                    let id = id.clone();
                    move |db| {
                        Ok(db
                            .query_row(
                                "SELECT folder_id, account_id, reconcile_reason,
                                        NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0'),
                                        target_pending, awaiting_reconcile
                                   FROM message WHERE id = ?1",
                                rusqlite::params![id],
                                |row| {
                                    Ok((
                                        row.get::<_, String>(0)?,
                                        row.get::<_, String>(1)?,
                                        row.get::<_, Option<String>>(2)?,
                                        row.get::<_, Option<String>>(3)?,
                                        row.get::<_, i64>(4)?,
                                        row.get::<_, i64>(5)?,
                                    ))
                                },
                            )
                            .optional()?)
                    }
                })
                .await?;
            let Some((folder_id, account_id, reason, op_id, target_pending, awaiting)) = info
            else {
                continue;
            };
            if reason.is_none() && target_pending == 0 && awaiting == 0 {
                continue;
            }
            retried += 1;
            folders.insert(folder_id.clone(), account_id.clone());
            let Some(op_id) = op_id else {
                continue;
            };
            let target = self
                .inner
                .ctx
                .db
                .read(move |db| {
                    Ok(repo::pending_ops::get(db, &op_id)?.and_then(|op| {
                        op.payload
                            .get("targetFolderId")
                            .and_then(|value| value.as_str())
                            .map(str::to_string)
                    }))
                })
                .await?;
            if let Some(target) = target {
                folders.entry(target).or_insert(account_id);
            }
        }
        for (folder, account) in folders {
            self.inner
                .engine
                .sync_now(Some(&account), Some(&folder))
                .await?;
        }
        Ok(retried)
    }

    /// Run one local rule immediately against a folder; returns the number of
    /// affected messages.
    pub async fn rule_run_now(&self, rule_id: &str, folder_id: &str) -> Result<usize> {
        let rule: MailRule = {
            let id = rule_id.to_string();
            self.inner
                .ctx
                .db
                .read(move |db| repo::rules::get_required(db, &id))
                .await?
        };
        let folder = self.folder(folder_id).await?;
        let account = self.account(&folder.account_id).await?;
        let mut summaries: Vec<MessageSummary> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..20 {
            let query = MessageListQuery::new(MailScope::folder(folder.id.clone()))
                .with_conversation(false)
                .with_cursor(cursor.clone())
                .with_limit(500);
            let page = self
                .inner
                .ctx
                .db
                .read(move |db| repo::messages::list(db, &query))
                .await?;
            summaries.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        let affected =
            crate::rules::apply_rule(&self.inner.ctx, &account, &rule, &summaries).await?;
        if affected > 0 {
            let folder_id_owned = folder.id.clone();
            self.inner
                .ctx
                .db
                .write(move |db| {
                    repo::folders::recompute_counts(db, &folder_id_owned)?;
                    Ok(())
                })
                .await?;
            let ids = summaries.iter().map(|summary| summary.id.clone()).collect();
            self.emit_changed(
                &account.id,
                vec![folder.id.clone()],
                ids,
                MailChangeKind::Updated,
            );
            self.update_badge().await;
        }
        Ok(affected)
    }

    /// Resolve trusted-sender status for the reader UI.
    pub async fn is_sender_trusted(&self, email: &str) -> Result<bool> {
        let email = nuncio_core::text::normalize_email(email);
        let domain = nuncio_core::text::email_domain(&email).unwrap_or_default();
        self.inner
            .ctx
            .db
            .read(move |db| repo::trusted_senders::is_trusted(db, &email, &domain))
            .await
    }

    /// Remember a sender e-mail address or `@domain` as trusted for remote
    /// content.
    pub async fn message_trust_sender(&self, value: &str) -> Result<()> {
        let value = value.trim().to_lowercase();
        if value.is_empty() || (!value.starts_with('@') && !value.contains('@')) {
            return Err(AppError::invalid_input(
                "trusted sender must be an e-mail address or @domain",
            ));
        }
        self.inner
            .ctx
            .db
            .write(move |db| repo::trusted_senders::add(db, &value, None))
            .await
    }
}

/* ─────────────────── Gmail label folders and search merge ──────────────── */

const MAX_MERGED_CANDIDATES: usize = 5000;

#[derive(Debug, Clone)]
enum LabelMode {
    /// Custom Gmail label (`Work`, `Projects/2026`, ...).
    Label(String),
    /// `[Gmail]/All Mail`: every message of the account.
    AllAccount,
}

fn addresses_from(row: &Row<'_>, column: &str) -> Result<Vec<Address>> {
    let value: Option<String> = row.get(column)?;
    Ok(nuncio_core::repo::json_or_default(value.as_deref(), column))
}

/// Row mapper matching `repo::messages::row_to_summary` (which is private).
fn row_to_summary(row: &Row<'_>) -> Result<MessageSummary> {
    use nuncio_core::repo::{get_bool, get_dt, get_string, json_opt};
    let from: Option<String> = row.get("from_json")?;
    let importance: String = get_string(row, "importance")?;
    Ok(MessageSummary {
        id: get_string(row, "id")?,
        account_id: get_string(row, "account_id")?,
        folder_id: get_string(row, "folder_id")?,
        thread_id: get_string(row, "thread_id")?,
        subject: get_string(row, "subject")?,
        r#from: json_opt(from.as_deref(), "from_json"),
        to: addresses_from(row, "to_json")?,
        cc: addresses_from(row, "cc_json")?,
        date: get_dt(row, "date")?,
        preview: get_string(row, "preview")?,
        seen: get_bool(row, "seen")?,
        flagged: get_bool(row, "flagged")?,
        answered: get_bool(row, "answered")?,
        forwarded: get_bool(row, "forwarded")?,
        is_draft: get_bool(row, "is_draft")?,
        has_attachments: get_bool(row, "has_attachments")?,
        importance: importance.parse().unwrap_or(Importance::Normal),
        size: row.get::<_, i64>("size")?.max(0) as u64,
        thread_count: 1,
        thread_unread: 0,
        reconcile_reason: nuncio_core::repo::get_opt_string(row, "reconcile_reason")?,
        draft_sync_state: None,
    })
}

fn fetch_summaries(conn: &rusqlite::Connection, ids: &[String]) -> Result<Vec<MessageSummary>> {
    use rusqlite::ToSql;
    let mut summaries = Vec::new();
    for chunk in ids.chunks(300) {
        if chunk.is_empty() {
            continue;
        }
        let placeholders = (1..=chunk.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {} FROM message m WHERE m.id IN ({placeholders})",
            repo::messages::MESSAGE_COLUMNS
        );
        let values: Vec<&dyn ToSql> = chunk.iter().map(|id| id as &dyn ToSql).collect();
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(values.as_slice())?;
        while let Some(row) = rows.next()? {
            summaries.push(row_to_summary(row)?);
        }
    }
    repo::messages::attach_draft_sync_state(conn, &mut summaries)?;
    Ok(summaries)
}

fn after_cursor(item: &MessageSummary, cursor: &PageCursor) -> bool {
    let key = nuncio_core::repo::dt_string(item.date);
    key < cursor.key || (key == cursor.key && item.id < cursor.id)
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// Apply the non-text part of a [`SearchQuery`] (and the list filter) to a
/// summary. Used by the merged server/local search path.
fn search_matches(
    summary: &MessageSummary,
    search: &SearchQuery,
    filter: MailFilter,
    account_email: Option<&str>,
) -> bool {
    if let Some(account_id) = search.account_id.as_deref() {
        if summary.account_id != account_id {
            return false;
        }
    }
    if let Some(folder_id) = search.folder_id.as_deref() {
        if summary.folder_id != folder_id {
            return false;
        }
    }
    if let Some(from) = search
        .from
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        let matches = summary
            .r#from
            .as_ref()
            .map(|address| {
                contains_ci(&address.email, from)
                    || address
                        .name
                        .as_deref()
                        .map(|name| contains_ci(name, from))
                        .unwrap_or(false)
            })
            .unwrap_or(false);
        if !matches {
            return false;
        }
    }
    if let Some(to) = search
        .to
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        let matches = summary
            .to
            .iter()
            .chain(summary.cc.iter())
            .any(|address| contains_ci(&address.email, to));
        if !matches {
            return false;
        }
    }
    if let Some(subject) = search
        .subject
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        if !contains_ci(&summary.subject, subject) {
            return false;
        }
    }
    if let Some(value) = search.has_attachment {
        if summary.has_attachments != value {
            return false;
        }
    }
    if let Some(unread) = search.unread {
        if summary.seen == unread {
            return false;
        }
    }
    if let Some(flagged) = search.flagged {
        if summary.flagged != flagged {
            return false;
        }
    }
    let date = nuncio_core::repo::dt_string(summary.date);
    if let Some(from) = search
        .date_from
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        if date.as_str() < from {
            return false;
        }
    }
    if let Some(to) = search.date_to.as_deref().filter(|value| !value.is_empty()) {
        if date.as_str() > to {
            return false;
        }
    }
    match filter {
        MailFilter::All => true,
        MailFilter::Unread => !summary.seen,
        MailFilter::Flagged => summary.flagged,
        MailFilter::Attachments => summary.has_attachments,
        MailFilter::MentionsMe => account_email
            .map(|email| {
                summary
                    .to
                    .iter()
                    .chain(summary.cc.iter())
                    .any(|address| nuncio_core::text::normalize_email(&address.email) == email)
            })
            .unwrap_or(false),
    }
}

/// Paged listing for a Gmail label folder / `All Mail`. Mirrors
/// `repo::messages::list` but matches label membership for custom labels.
fn list_label_messages(
    conn: &rusqlite::Connection,
    query: &MessageListQuery,
    folder: &Folder,
    mode: &LabelMode,
) -> Result<MessagePage> {
    use nuncio_core::repo::{int, text};
    let limit = query.limit.clamp(1, 500) as i64;
    let mut clauses: Vec<String> = vec!["m.is_deleted_local = 0".to_string()];
    let mut values: Vec<Value> = Vec::new();
    match mode {
        LabelMode::Label(label) => {
            clauses.push("m.account_id = ?".into());
            values.push(text(folder.account_id.clone()));
            clauses.push(
                "(m.folder_id = ? OR EXISTS (SELECT 1 FROM json_each(COALESCE(m.labels_json, '[]')) WHERE json_each.value = ?))"
                    .into(),
            );
            values.push(text(folder.id.clone()));
            values.push(text(label.clone()));
        }
        LabelMode::AllAccount => {
            clauses.push("m.account_id = ?".into());
            values.push(text(folder.account_id.clone()));
        }
    }
    match query.filter {
        MailFilter::All => {}
        MailFilter::Unread => clauses.push("m.seen = 0".into()),
        MailFilter::Flagged => clauses.push("m.flagged = 1".into()),
        MailFilter::Attachments => clauses.push("m.has_attachments = 1".into()),
        MailFilter::MentionsMe => clauses.push(
            "EXISTS (SELECT 1 FROM account a WHERE a.id = m.account_id
             AND (lower(m.to_json) LIKE '%' || lower(a.email) || '%'
                  OR lower(m.cc_json) LIKE '%' || lower(a.email) || '%'))"
                .into(),
        ),
    }
    let where_sql = clauses.join(" AND ");
    let total_sql = if query.conversation {
        format!("SELECT COUNT(DISTINCT m.thread_id) FROM message m WHERE {where_sql}")
    } else {
        format!("SELECT COUNT(*) FROM message m WHERE {where_sql}")
    };
    let refs = nuncio_core::repo::query_params(&values);
    let total: i64 = conn.query_row(&total_sql, refs.as_slice(), |row| row.get(0))?;

    let mut params = values.clone();
    let mut cursor_clause = String::new();
    if let Some(raw) = &query.cursor {
        let cursor = PageCursor::decode(raw)?;
        if query.conversation {
            cursor_clause = " AND (date < ? OR (date = ? AND id < ?))".to_string();
        } else {
            cursor_clause = " AND (m.date < ? OR (m.date = ? AND m.id < ?))".to_string();
        }
        params.push(text(cursor.key.clone()));
        params.push(text(cursor.key));
        params.push(text(cursor.id));
    }
    params.push(int(limit + 1));

    let mut items: Vec<MessageSummary> = Vec::new();
    if query.conversation {
        let sql = format!(
            "WITH filtered AS (
                 SELECT m.* FROM message m WHERE {where_sql}
             ),
             ranked AS (
                 SELECT *,
                     ROW_NUMBER() OVER (PARTITION BY thread_id ORDER BY date DESC, id DESC) AS rn,
                     COUNT(*) OVER (PARTITION BY thread_id) AS tcount,
                     SUM(CASE WHEN seen = 0 THEN 1 ELSE 0 END) OVER (PARTITION BY thread_id) AS tunread
                 FROM filtered
             )
             SELECT * FROM ranked WHERE rn = 1{cursor_clause}
             ORDER BY date DESC, id DESC LIMIT ?"
        );
        let refs = nuncio_core::repo::query_params(&params);
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(refs.as_slice())?;
        while let Some(row) = rows.next()? {
            let mut summary = row_to_summary(row)?;
            summary.thread_count = row.get::<_, Option<i64>>("tcount")?.unwrap_or(1).max(1);
            summary.thread_unread = row.get::<_, Option<i64>>("tunread")?.unwrap_or(0).max(0);
            items.push(summary);
        }
    } else {
        let sql = format!(
            "SELECT {}, 1 AS tcount, CASE WHEN m.seen = 0 THEN 1 ELSE 0 END AS tunread
             FROM message m WHERE {where_sql}{cursor_clause}
             ORDER BY m.date DESC, m.id DESC LIMIT ?",
            repo::messages::MESSAGE_COLUMNS
        );
        let refs = nuncio_core::repo::query_params(&params);
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(refs.as_slice())?;
        while let Some(row) = rows.next()? {
            let mut summary = row_to_summary(row)?;
            summary.thread_count = 1;
            summary.thread_unread = if summary.seen { 0 } else { 1 };
            items.push(summary);
        }
    }

    let has_more = items.len() as i64 > limit;
    if has_more {
        items.truncate(limit as usize);
    }
    let next_cursor = if has_more {
        items
            .last()
            .map(|item| PageCursor::message(item.date, item.id.clone()).encode())
    } else {
        None
    };
    Ok(MessagePage {
        items,
        next_cursor,
        total,
        coverage: None,
    })
}

#[cfg(test)]
mod tests {
    use super::delete_is_permanent;
    use nuncio_core::models::FolderRole;
    use nuncio_core::repo;

    #[test]
    fn a_discarded_draft_is_deleted_rather_than_trashed() {
        // The Drafts folder is a normal folder, so a role-only rule would move
        // the draft to Trash and leave is_draft set.
        let requested = false;
        let is_draft = true;
        assert!(delete_is_permanent(requested, FolderRole::Other, is_draft));
        // The folder role is enough even when the server flag is missing.
        let is_draft = false;
        assert!(delete_is_permanent(requested, FolderRole::Drafts, is_draft));
    }

    #[test]
    fn trash_and_junk_are_already_permanent() {
        assert!(delete_is_permanent(false, FolderRole::Trash, false));
        assert!(delete_is_permanent(false, FolderRole::Junk, false));
        assert!(!delete_is_permanent(false, FolderRole::Inbox, false));
        assert!(!delete_is_permanent(false, FolderRole::Archive, false));
    }

    #[test]
    fn an_explicit_request_wins() {
        assert!(delete_is_permanent(true, FolderRole::Inbox, false));
    }

    /// The ABA case behind the message-level fallback revision.
    ///
    /// An open observes "no writer". While the body loads, the user marks the
    /// message read and undoes it; because the action has no predecessor the
    /// undo *deletes* the writer row. Comparing `Option<writer.seq>` would see
    /// `None == None` and mark the restored unread message read again. The
    /// message revision must have moved, so the automatic write is skipped.
    #[tokio::test]
    async fn auto_mark_read_does_not_overwrite_a_value_restored_after_writer_clear() {
        use nuncio_core::models::{
            Account, AccountState, AuthKind, FolderRole, MessageSummary, ProviderKind, Security,
            ServerConfig, StoredMessage,
        };
        use nuncio_core::{CoreContext, Paths};

        let dir = std::env::temp_dir().join(format!(
            "nuncio-mail-seen-{}",
            nuncio_core::models::new_id()
        ));
        let ctx = CoreContext::for_test(Paths::new(dir))
            .await
            .expect("context");
        let now = nuncio_core::models::now();
        let account = Account {
            id: "acc".into(),
            provider: ProviderKind::Imap,
            email: "me@example.com".into(),
            display_name: "Me".into(),
            color: "#000000".into(),
            auth_kind: AuthKind::Password,
            oauth_provider: None,
            imap: Some(ServerConfig::new(
                "imap.example.com",
                993,
                Security::Tls,
                "me@example.com",
            )),
            smtp: None,
            pop3: None,
            caldav_url: None,
            carddav_url: None,
            sent_copy: None,
            sync_window_days: 90,
            mail_enabled: true,
            calendar_enabled: false,
            contacts_enabled: false,
            tasks_enabled: false,
            notifications_enabled: true,
            include_in_unified: true,
            sort_order: 0,
            state: AccountState::Ok,
            state_message: None,
            capabilities: Default::default(),
            created_at: now,
        };
        ctx.db
            .write({
                let account = account.clone();
                move |db| {
                    repo::accounts::insert(db, &account)?;
                    let folder =
                        nuncio_core::models::Folder::new("fld", "acc", "Inbox", FolderRole::Inbox);
                    repo::folders::insert(db, &folder)
                }
            })
            .await
            .expect("account and folder");
        let summary = MessageSummary::new("m1", "acc", "fld", "t1", now);
        let mut stored = StoredMessage::new(summary);
        stored.uid = Some(7);
        stored.uidvalidity = Some(1);
        ctx.db
            .write(move |db| repo::messages::upsert(db, &stored))
            .await
            .expect("message");

        // Snapshot the open's baseline: the provider copy and the revision.
        let mut opened = ctx
            .db
            .read(|db| repo::messages::get_required(db, "m1"))
            .await
            .expect("stored");
        let opened_copy = repo::messages::body_identity_key(&opened);
        let observed_revision = ctx
            .db
            .read(|db| repo::messages::flag_revision(db, "m1"))
            .await
            .expect("revision");
        assert_eq!(observed_revision, Some(0));

        // Mark read, then undo with no predecessor: the writer row disappears.
        ctx.db
            .write(|db| {
                repo::messages::set_flags(db, &["m1".to_string()], Some(true), None, None, None)?;
                repo::messages::record_flag_writer(
                    db,
                    "m1",
                    "seen",
                    "op-1",
                    Some(true),
                    "setSeen",
                )?;
                repo::messages::set_flags(db, &["m1".to_string()], Some(false), None, None, None)?;
                repo::messages::reassign_flag_writer(db, "m1", "seen", "op-1", None, None, None)
            })
            .await
            .expect("mark read and undo");
        assert!(
            ctx.db
                .read(|db| repo::messages::flag_writer(db, "m1", "seen"))
                .await
                .expect("writer")
                .is_none(),
            "the undo removed the last writer"
        );

        let service = crate::MailService::new(ctx.clone());
        service
            .mark_seen_on_open(&mut opened, opened_copy, observed_revision)
            .await
            .expect("auto mark read");

        let seen = ctx
            .db
            .read(|db| repo::messages::get_required(db, "m1"))
            .await
            .expect("stored")
            .summary
            .seen;
        assert!(
            !seen,
            "the automatic mark must not overwrite the value the undo restored"
        );
        let ops = ctx
            .db
            .read(|db| repo::pending_ops::list(db, Some("acc")))
            .await
            .expect("ops");
        assert!(
            ops.is_empty(),
            "no setFlags may be queued after the concurrent change"
        );
    }

    /// A sync can rebind the stable row (and its synthetic UID) to another
    /// Graph message before the queue transaction runs. The transaction must
    /// reject the stale payload instead of queueing a delete for the new copy.
    #[test]
    fn a_rebound_graph_copy_fails_the_queue_expectation() {
        use super::{remote_expectation_matches, RemoteExpectation};
        use nuncio_core::models::{MessageSummary, StoredMessage};

        let db = nuncio_core::Db::open_in_memory().expect("db");
        db.migrate_core().expect("migrate");
        db.write_sync(|conn| {
            let now = nuncio_core::models::now().to_rfc3339();
            conn.execute(
                "INSERT INTO account (id, provider, email, auth_kind, created_at, updated_at)
                 VALUES ('acc', 'outlook', 'me@example.com', 'oauth', ?1, ?1)",
                rusqlite::params![now],
            )?;
            conn.execute(
                "INSERT INTO folder (id, account_id, remote_path, name, role, created_at, updated_at)
                 VALUES ('fld', 'acc', 'Inbox', 'Inbox', 'inbox', ?1, ?1)",
                rusqlite::params![now],
            )?;
            let summary = MessageSummary::new(
                "row",
                "acc",
                "fld",
                "thread",
                nuncio_core::models::now(),
            );
            let mut stored = StoredMessage::new(summary);
            stored.uid = Some(7);
            stored.uidvalidity = Some(1);
            stored.gm_msgid = Some("copy-a".into());
            repo::messages::upsert(conn, &stored)?;
            repo::sync_state::set(conn, "acc", "mail", "graph_remote:fld:7", "copy-a")?;

            let expected = RemoteExpectation {
                id: "row".into(),
                account_id: "acc".into(),
                folder_id: "fld".into(),
                uid: Some(7),
                uidvalidity: Some(1),
                graph: true,
                remote_id: Some("copy-a".into()),
            };
            assert!(remote_expectation_matches(conn, &expected)?);

            // The same local row/UID now names a different provider message.
            stored.gm_msgid = Some("copy-b".into());
            repo::messages::upsert(conn, &stored)?;
            repo::sync_state::set(conn, "acc", "mail", "graph_remote:fld:7", "copy-b")?;
            assert!(
                !remote_expectation_matches(conn, &expected)?,
                "a rebound provider id must fail the queue check"
            );

            // No pinned provider id at all: the executor could only re-resolve
            // it from the mutable mapping, so the queue must refuse.
            let unpinned = RemoteExpectation {
                remote_id: None,
                ..expected
            };
            assert!(
                !remote_expectation_matches(conn, &unpinned)?,
                "a Graph action without a provider id must be refused"
            );
            let imap_unpinned = RemoteExpectation {
                graph: false,
                remote_id: None,
                ..unpinned
            };
            assert!(
                remote_expectation_matches(conn, &imap_unpinned)?,
                "an IMAP action has no provider id to pin"
            );
            Ok(())
        })
        .expect("expectation");
    }
}
