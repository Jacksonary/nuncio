//! Message listing, conversations, optimistic actions with undo recording and
//! mail-rule execution.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use nuncio_core::events::MailChangeKind;
use nuncio_core::models::{
    Address, Attachment, Folder, FolderRole, Importance, MailFilter, MailRule, MailScope,
    MessageAction, MessageListQuery, MessagePage, MessageSummary, PageCursor, ProviderKind,
    SearchCoverage,
    SearchQuery, StoredMessage,
};
use nuncio_core::{events, repo, AppError, Result};
use rusqlite::{types::Value, Row};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::ops::{
    self, ArchiveOp, DeleteOp, FlagRollback, MarkJunkOp, MoveOp, SetFlagsOp, KIND_ARCHIVE,
    KIND_DELETE, KIND_MARK_JUNK, KIND_MOVE, KIND_SET_FLAGS,
};

use super::MailService;

fn is_local_only(provider: ProviderKind) -> bool {
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredSnapshot {
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
            *self.inner.search_cache.lock().expect("search cache lock") =
                Some(SearchCacheEntry {
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
    pub(crate) async fn mark_seen_on_open(&self, stored: &mut StoredMessage) -> Result<()> {
        if stored.summary.seen {
            return Ok(());
        }
        let previous = stored.uid.map(|uid| FlagRollback {
            uid,
            seen: stored.summary.seen,
            flagged: stored.summary.flagged,
            answered: stored.summary.answered,
            forwarded: stored.summary.forwarded,
        });
        stored.summary.seen = true;
        let id = stored.summary.id.clone();
        let folder_id = stored.summary.folder_id.clone();
        self.inner
            .ctx
            .db
            .write(move |db| {
                repo::messages::set_flags(db, &[id], Some(true), None, None, None)?;
                Ok(())
            })
            .await?;
        let account = self.account(&stored.summary.account_id).await?;
        self.enqueue_seen(&account, stored, true, previous, "Mark message as read")
            .await?;
        self.emit_changed(
            &account.id,
            vec![folder_id],
            vec![stored.summary.id.clone()],
            MailChangeKind::Updated,
        );
        self.update_badge().await;
        Ok(())
    }

    async fn enqueue_seen(
        &self,
        account: &nuncio_core::models::Account,
        stored: &StoredMessage,
        seen: bool,
        previous: Option<FlagRollback>,
        description: &str,
    ) -> Result<Option<String>> {
        if is_local_only(account.provider) {
            return Ok(None);
        }
        let Some(uid) = stored.uid else {
            return Ok(None);
        };
        let payload = SetFlagsOp {
            folder_id: stored.summary.folder_id.clone(),
            uids: vec![uid],
            seen: Some(seen),
            flagged: None,
            answered: None,
            forwarded: None,
            previous: previous.into_iter().collect(),
        };
        let op = ops::enqueue_operation(
            &self.inner.ctx,
            &account.id,
            KIND_SET_FLAGS,
            serde_json::to_value(payload)
                .map_err(|error| AppError::internal(format!("cannot encode setFlags: {error}")))?,
            description,
        )
        .await?;
        Ok(Some(op.id))
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
                    target_folder = self
                        .role_folder(&account, FolderRole::Archive, &mut role_cache)
                        .await?;
                    if target_folder.is_none() {
                        target_folder = self
                            .role_folder(&account, FolderRole::All, &mut role_cache)
                            .await?;
                    }
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

        // ── local changes (single transaction) ────────────────────────
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

        let mut permanent_ids: Vec<String> = Vec::new();
        let mut hidden_ids: Vec<String> = Vec::new();
        let mut moves: HashMap<String, Vec<String>> = HashMap::new();
        let mut flag_ids: Vec<String> = Vec::new();
        for target in &targets {
            match action_kind {
                "setSeen" | "setFlagged" => flag_ids.push(target.message.summary.id.clone()),
                "delete" if target.permanent => {
                    permanent_ids.push(target.message.summary.id.clone())
                }
                "delete" => {
                    if let Some(folder_id) = &target.target_folder {
                        moves
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
                        moves
                            .entry(folder_id.clone())
                            .or_default()
                            .push(target.message.summary.id.clone());
                    }
                }
            }
        }
        let seen_value = match action {
            MessageAction::SetSeen { value } => Some(*value),
            _ => None,
        };
        let flagged_value = match action {
            MessageAction::SetFlagged { value } => Some(*value),
            _ => None,
        };
        let flags = flag_ids.clone();
        let deletes = permanent_ids.clone();
        let hidden = hidden_ids.clone();
        let move_groups = moves.clone();
        self.inner
            .ctx
            .db
            .write(move |db| {
                if !flags.is_empty() {
                    repo::messages::set_flags(db, &flags, seen_value, flagged_value, None, None)?;
                }
                if !deletes.is_empty() {
                    repo::messages::delete_local(db, &deletes, true)?;
                }
                if !hidden.is_empty() {
                    repo::messages::delete_local(db, &hidden, false)?;
                }
                for (folder_id, ids) in move_groups {
                    repo::messages::move_to_folder(db, &ids, &folder_id)?;
                }
                Ok(())
            })
            .await?;

        // ── server operations ────────────────────────────────────────
        let mut op_ids: Vec<String> = Vec::new();
        match action {
            MessageAction::SetSeen { value } => {
                op_ids.extend(self.queue_flag_ops(&targets, Some(*value), None).await?);
            }
            MessageAction::SetFlagged { value } => {
                op_ids.extend(self.queue_flag_ops(&targets, None, Some(*value)).await?);
            }
            MessageAction::Move { .. } => {
                op_ids.extend(self.queue_move_ops(&targets, "Move message").await?);
            }
            MessageAction::Archive => {
                op_ids.extend(self.queue_archive_ops(&targets).await?);
            }
            MessageAction::Delete { .. } => {
                op_ids.extend(self.queue_delete_ops(&targets).await?);
            }
            MessageAction::MarkJunk { value } => {
                op_ids.extend(self.queue_junk_ops(&targets, *value).await?);
            }
        }

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

    async fn queue_flag_ops(
        &self,
        targets: &[Target],
        seen: Option<bool>,
        flagged: Option<bool>,
    ) -> Result<Vec<String>> {
        let mut grouped: HashMap<(String, String), (Vec<i64>, Vec<FlagRollback>)> = HashMap::new();
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
        }
        let mut ids = Vec::new();
        for ((account_id, folder_id), (uids, previous)) in grouped {
            let payload = SetFlagsOp {
                folder_id,
                uids,
                seen,
                flagged,
                answered: None,
                forwarded: None,
                previous,
            };
            let op = ops::enqueue_operation(
                &self.inner.ctx,
                &account_id,
                KIND_SET_FLAGS,
                serde_json::to_value(payload).map_err(|error| {
                    AppError::internal(format!("cannot encode setFlags: {error}"))
                })?,
                if seen == Some(true) {
                    "Mark as read"
                } else if seen == Some(false) {
                    "Mark as unread"
                } else if flagged == Some(true) {
                    "Flag message"
                } else {
                    "Unflag message"
                },
            )
            .await?;
            ids.push(op.id);
        }
        Ok(ids)
    }

    async fn queue_move_ops(&self, targets: &[Target], description: &str) -> Result<Vec<String>> {
        let mut grouped: HashMap<(String, String, String), Vec<i64>> = HashMap::new();
        for target in targets {
            if is_local_only(target.account.provider) {
                continue;
            }
            let (Some(uid), Some(target_folder)) =
                (target.message.uid, target.target_folder.clone())
            else {
                continue;
            };
            grouped
                .entry((
                    target.account.id.clone(),
                    target.message.summary.folder_id.clone(),
                    target_folder,
                ))
                .or_default()
                .push(uid);
        }
        let mut ids = Vec::new();
        for ((account_id, folder_id, target_folder_id), uids) in grouped {
            let payload = MoveOp {
                folder_id,
                target_folder_id,
                uids,
            };
            let op = ops::enqueue_operation(
                &self.inner.ctx,
                &account_id,
                KIND_MOVE,
                serde_json::to_value(payload)
                    .map_err(|error| AppError::internal(format!("cannot encode move: {error}")))?,
                description,
            )
            .await?;
            ids.push(op.id);
        }
        Ok(ids)
    }

    async fn queue_archive_ops(&self, targets: &[Target]) -> Result<Vec<String>> {
        let mut grouped: HashMap<(String, String), (Vec<i64>, Option<String>)> = HashMap::new();
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
                ))
                .or_default();
            entry.0.push(uid);
            if entry.1.is_none() {
                entry.1 = target.target_folder.clone();
            }
        }
        let mut ids = Vec::new();
        for ((account_id, folder_id), (uids, target_folder_id)) in grouped {
            let payload = ArchiveOp {
                folder_id,
                uids,
                target_folder_id,
            };
            let op = ops::enqueue_operation(
                &self.inner.ctx,
                &account_id,
                KIND_ARCHIVE,
                serde_json::to_value(payload).map_err(|error| {
                    AppError::internal(format!("cannot encode archive: {error}"))
                })?,
                "Archive message",
            )
            .await?;
            ids.push(op.id);
        }
        Ok(ids)
    }

    async fn queue_delete_ops(&self, targets: &[Target]) -> Result<Vec<String>> {
        let mut grouped: HashMap<(String, String, bool, Option<String>), Vec<i64>> = HashMap::new();
        for target in targets {
            if is_local_only(target.account.provider) {
                continue;
            }
            let Some(uid) = target.message.uid else {
                continue;
            };
            grouped
                .entry((
                    target.account.id.clone(),
                    target.message.summary.folder_id.clone(),
                    target.permanent,
                    target.target_folder.clone(),
                ))
                .or_default()
                .push(uid);
        }
        let mut ids = Vec::new();
        for ((account_id, folder_id, permanent, target_folder_id), uids) in grouped {
            let payload = DeleteOp {
                folder_id,
                uids,
                permanent,
                soft_hidden: !permanent && target_folder_id.is_none(),
                target_folder_id,
            };
            let op = ops::enqueue_operation(
                &self.inner.ctx,
                &account_id,
                KIND_DELETE,
                serde_json::to_value(payload).map_err(|error| {
                    AppError::internal(format!("cannot encode delete: {error}"))
                })?,
                if permanent {
                    "Delete message permanently"
                } else {
                    "Delete message"
                },
            )
            .await?;
            ids.push(op.id);
        }
        Ok(ids)
    }

    async fn queue_junk_ops(&self, targets: &[Target], value: bool) -> Result<Vec<String>> {
        let mut grouped: HashMap<(String, String), (Vec<i64>, Option<String>)> = HashMap::new();
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
                ))
                .or_default();
            entry.0.push(uid);
            if entry.1.is_none() {
                entry.1 = target.target_folder.clone();
            }
        }
        let mut ids = Vec::new();
        for ((account_id, folder_id), (uids, target_folder_id)) in grouped {
            let payload = MarkJunkOp {
                folder_id,
                uids,
                value,
                target_folder_id,
            };
            let op = ops::enqueue_operation(
                &self.inner.ctx,
                &account_id,
                KIND_MARK_JUNK,
                serde_json::to_value(payload).map_err(|error| {
                    AppError::internal(format!("cannot encode markJunk: {error}"))
                })?,
                if value {
                    "Mark as junk"
                } else {
                    "Mark as not junk"
                },
            )
            .await?;
            ids.push(op.id);
        }
        Ok(ids)
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
}
