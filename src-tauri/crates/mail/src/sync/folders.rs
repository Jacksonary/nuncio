//! Folder discovery and role mapping (`LIST`, SPECIAL-USE / XLIST).
//!
//! Gmail exposes labels as folders and also returns `[Gmail]/All Mail`,
//! `[Gmail]/Important` and `[Gmail]/Starred`. All Mail and Important are hidden
//! from favourites by default; the regular Inbox/Sent/Drafts stay pinned.

use std::collections::{HashMap, HashSet};

use async_imap::imap_proto::NameAttribute;
use futures::TryStreamExt;
use nuncio_core::events;
use nuncio_core::models::{Account, Folder, FolderRole};
use nuncio_core::{AppError, CoreContext, Result};
use tracing::{debug, info, warn};

use crate::graph::{self, GraphMailClient};
use crate::imap::conn::{ImapConnection, COMMAND_TIMEOUT};
use crate::util::imap_error;

/// Remote paths reserved for folders that only exist locally (Outbox, and a
/// Drafts folder for accounts whose server has none).
pub const LOCAL_FOLDER_PREFIX: &str = "local:";

#[derive(Debug, Clone)]
pub struct RemoteFolder {
    pub path: String,
    pub delimiter: Option<String>,
    pub name: String,
    pub attributes: Vec<String>,
    pub selectable: bool,
    /// Provider-side identity when it differs from the IMAP-style `path`.
    ///
    /// IMAP names a mailbox by its path, but Graph addresses a folder by an
    /// opaque id even though its tree still needs human paths locally.  The id
    /// is recorded in `sync_state` (`graph_folder:{path}`) and resolved by
    /// [`graph_folder_id`], so `Folder::remote_path` can keep the readable
    /// hierarchy the rest of the app derives parents from.
    pub provider_id: Option<String>,
}

/// Look up the Graph folder id recorded for a local folder path.
///
/// Returns `None` for IMAP folders (no mapping was written) and for a Graph
/// folder that has not been reconciled yet.
pub async fn graph_folder_id(
    ctx: &CoreContext,
    account_id: &str,
    remote_path: &str,
) -> Result<Option<String>> {
    let key = graph_folder_key(remote_path);
    let account_id = account_id.to_string();
    ctx.db
        .read(move |db| nuncio_core::repo::sync_state::get(db, &account_id, "mail", &key))
        .await
}

fn graph_folder_key(remote_path: &str) -> String {
    format!("graph_folder:{remote_path}")
}

/// Synchronize the local folder tree with the server's folder list.
///
/// This is the only folder-sync entry point the IMAP worker should use.  A
/// Microsoft OAuth account is dispatched to Graph before any IMAP command is
/// issued (see [`graph::uses_graph`]); every other account keeps the existing
/// `LIST` path byte-for-byte.  Both paths then feed the exact same
/// [`reconcile`] function, so roles, parent links, favourites and sort order
/// cannot diverge between providers.
///
/// Returns all local folders for the account after reconciliation.
pub async fn sync(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account_id: &str,
) -> Result<Vec<Folder>> {
    let account = load_account(ctx, account_id).await?;
    if graph::uses_graph(&account) {
        // The connection argument is intentionally unused: a Graph account
        // must not open an IMAP session at all (Exchange rejects it).
        return sync_graph(ctx, &account).await;
    }
    let remotes = list_remote(conn).await?;
    // IMAP mailbox names arrive in modified UTF-7.  `LIST` is all-or-nothing:
    // a failure anywhere propagates above, so a returned list is the whole
    // mailbox and pruning is safe.
    reconcile(ctx, account_id, remotes, true, true).await
}

/// Synchronize folders for a Microsoft Graph account without an IMAP
/// connection.
///
/// Used by the Graph account loop and by account commands that have no worker
/// connection.  It shares [`reconcile`] with the IMAP path.
pub async fn sync_graph(ctx: &CoreContext, account: &Account) -> Result<Vec<Folder>> {
    let listing = graph::folders::list_folders(&GraphMailClient::new(), ctx, &account.id).await?;
    if !listing.complete {
        // A transient childFolders failure or the pagination cap makes the
        // listing partial.  The upserts below are still safe, but deleting
        // folders missing from it would destroy a whole subtree locally.
        warn!(
            account = %account.id,
            folders = listing.folders.len(),
            "Graph folder listing is incomplete; keeping locally known folders that are missing from it"
        );
    }
    // Graph display names are already Unicode; running them through the IMAP
    // modified-UTF-7 decoder would corrupt a name such as "A&B-2026".
    reconcile(ctx, &account.id, listing.folders, false, listing.complete).await
}

async fn load_account(ctx: &CoreContext, account_id: &str) -> Result<Account> {
    let account_id = account_id.to_string();
    ctx.db
        .read(move |db| nuncio_core::repo::accounts::get_required(db, &account_id))
        .await
}

/// Shared reconciliation of a remote folder list into the local folder tree.
///
/// `remotes` may come from IMAP `LIST` or from Graph's `mailFolders` tree; the
/// shape is identical (`RemoteFolder`) and everything below is provider
/// agnostic.  `decode_names` is the only provider difference: IMAP mailbox
/// names are modified UTF-7, Graph display names are already Unicode.
///
/// `prune_missing` says whether `remotes` is the authoritative full tree.  Only
/// then may local folders absent from it be deleted: a partial listing means
/// "not read this time", not "deleted on the server", and deleting would drop
/// the folder's messages and favourite state until the next sync recreates it
/// under a new id.  IMAP `LIST` is all-or-nothing and always passes `true`.
async fn reconcile(
    ctx: &CoreContext,
    account_id: &str,
    remotes: Vec<RemoteFolder>,
    decode_names: bool,
    prune_missing: bool,
) -> Result<Vec<Folder>> {
    debug!(
        count = remotes.len(),
        account = account_id,
        "listed remote folders"
    );
    if remotes.is_empty() {
        warn!(account = account_id, "server returned no folders");
    }

    let local_before = ctx
        .db
        .read({
            let account_id = account_id.to_string();
            move |db| nuncio_core::repo::folders::list(db, &account_id)
        })
        .await?;

    let mut local_by_path: HashMap<String, Folder> = local_before
        .into_iter()
        .map(|folder| (folder.remote_path.clone(), folder))
        .collect();

    // First pass: decide local ids so parent links can be resolved by path.
    let mut id_by_path: HashMap<String, String> = HashMap::new();
    for remote in &remotes {
        let id = local_by_path
            .get(&remote.path)
            .map(|folder| folder.id.clone())
            .unwrap_or_else(nuncio_core::models::new_id);
        id_by_path.insert(remote.path.clone(), id);
    }

    let remote_paths: HashSet<&str> = remotes.iter().map(|folder| folder.path.as_str()).collect();
    let mut upserted: Vec<Folder> = Vec::new();

    // One role per folder, resolved across the whole listing: several folders
    // can look like the same system folder, and `by_role` must not be able to
    // pick a different one on every sort.
    let roles = resolve_role_conflicts(&remotes);

    for (index, remote) in remotes.iter().enumerate() {
        let existing = local_by_path.remove(&remote.path);
        let role = roles[index];
        let parent_path = remote
            .delimiter
            .as_deref()
            .and_then(|delimiter| parent_path(&remote.path, delimiter));
        let parent_id = parent_path
            .as_deref()
            .and_then(|path| id_by_path.get(path))
            .filter(|id| Some(*id) != id_by_path.get(&remote.path))
            .cloned();

        let is_gmail_virtual = is_gmail_all_or_important(&remote.path);
        let default_favorite = matches!(
            role,
            FolderRole::Inbox | FolderRole::Sent | FolderRole::Drafts
        ) && !is_gmail_virtual;
        let default_sync = matches!(
            role,
            FolderRole::Inbox
                | FolderRole::Sent
                | FolderRole::Drafts
                | FolderRole::Trash
                | FolderRole::Junk
                | FolderRole::Archive
                | FolderRole::Flagged
                | FolderRole::Important
        );

        // A folder that loses a system role must not keep the star that role
        // granted it.  `resolve_role_conflicts` can demote a folder that a
        // previous sync auto-starred as the account's Sent/Trash (an Outlook.com
        // mailbox carries a stray `发件箱` next to its real `已发送邮件`); without
        // this it stayed pinned at the top of the sidebar as an empty folder.
        let lost_system_role = existing
            .as_ref()
            .map(|folder| {
                folder.is_favorite
                    && role == FolderRole::Other
                    && matches!(
                        folder.role,
                        FolderRole::Inbox | FolderRole::Sent | FolderRole::Drafts
                    )
            })
            .unwrap_or(false);

        let folder = Folder {
            id: id_by_path.get(&remote.path).cloned().unwrap_or_default(),
            account_id: account_id.to_string(),
            parent_id,
            remote_path: remote.path.clone(),
            // IMAP folder names arrive in modified UTF-7 (and occasionally as
            // RFC 2047 encoded words); store the readable form.  Graph names
            // are already Unicode, so they are used verbatim.
            name: if decode_names {
                crate::util::decode_mailbox_name(&remote.name)
            } else {
                remote.name.clone()
            },
            role,
            unread_count: existing.as_ref().map(|f| f.unread_count).unwrap_or(0),
            total_count: existing.as_ref().map(|f| f.total_count).unwrap_or(0),
            is_favorite: existing
                .as_ref()
                .map(|f| f.is_favorite)
                .unwrap_or(default_favorite),
            sync_enabled: existing
                .as_ref()
                .map(|f| f.sync_enabled)
                .unwrap_or(default_sync),
            is_selectable: remote.selectable,
            sort_order: existing
                .as_ref()
                .map(|f| f.sort_order)
                .unwrap_or(index as i64),
            last_sync_at: existing.as_ref().and_then(|f| f.last_sync_at),
        };

        // Graph and IMAP differ in one place: Graph's server-side identity is
        // an opaque id while the local tree is keyed by the readable path.
        // Persist the id next to the folder so message sync and the executor
        // can address the right mailbox.
        let provider_id = remote.provider_id.clone();
        let provider_path = remote.path.clone();
        let account_for_map = account_id.to_string();
        let stored = ctx
            .db
            .write(move |db| {
                let stored = nuncio_core::repo::folders::upsert_remote(db, &folder)?;
                // `upsert_remote` never touches `is_favorite` on an existing
                // row (it is the user's choice), so the stale auto-star has to
                // be cleared explicitly rather than through the upsert.
                let stored = if lost_system_role {
                    nuncio_core::repo::folders::set_favorite(db, &stored.id, false)
                        .map(|()| Folder {
                            is_favorite: false,
                            ..stored
                        })?
                } else {
                    stored
                };
                if let Some(provider_id) = provider_id {
                    let key = graph_folder_key(&provider_path);
                    nuncio_core::repo::sync_state::set(
                        db,
                        &account_for_map,
                        "mail",
                        &key,
                        &provider_id,
                    )?;
                }
                Ok(stored)
            })
            .await?;
        upserted.push(stored);
    }

    // Remove folders that vanished on the server. Virtual/local roles are kept.
    //
    // This step is destructive: `folders::delete` cascades to the folder's
    // messages and discards favourite/sync choices.  It must therefore run only
    // when the listing is the authoritative tree; on a partial listing the
    // upserts above remain, but nothing is pruned.
    if prune_missing {
        for folder in local_by_path.values() {
            if folder.role == FolderRole::Outbox
                || folder.remote_path.starts_with(LOCAL_FOLDER_PREFIX)
            {
                continue;
            }
            if !remote_paths.contains(folder.remote_path.as_str()) {
                let id = folder.id.clone();
                let map_account = account_id.to_string();
                let map_key = graph_folder_key(&folder.remote_path);
                ctx.db
                    .write(move |db| {
                        nuncio_core::repo::folders::delete(db, &id)?;
                        // The provider-id mapping must not outlive the folder.
                        nuncio_core::repo::sync_state::delete_key(
                            db,
                            &map_account,
                            "mail",
                            &map_key,
                        )
                    })
                    .await?;
                debug!(folder = %folder.remote_path, "removed folder no longer on server");
            }
        }
    } else {
        // `local_by_path` now holds every local folder that was not in the
        // partial listing (including local-only ones such as Outbox), i.e. the
        // folders whose deletion was skipped.
        warn!(
            account = account_id,
            listed = remotes.len(),
            unresolved_local = local_by_path.len(),
            "incomplete folder listing; skipped folder reconciliation pruning so locally known folders (and their messages) are kept"
        );
    }

    // A server Drafts folder supersedes the local placeholder the composer
    // created when the account had none. Reconcile is the only place that sees
    // that folder appear (for IMAP and Graph alike) and it already owns folder
    // lifecycle, while the composer may never run again. Until the migration
    // the placeholder keeps its messages and the sidebar renders two "Drafts"
    // rows, one of them permanently out of sync.
    let is_server_drafts = |folder: &Folder| {
        folder.role == FolderRole::Drafts && !folder.remote_path.starts_with(LOCAL_FOLDER_PREFIX)
    };
    // A partial listing leaves the server folder in `local_by_path` instead;
    // it is still a real Drafts folder and the placeholder must yield to it.
    let real_drafts = upserted
        .iter()
        .find(|folder| is_server_drafts(folder))
        .cloned()
        .or_else(|| {
            local_by_path
                .values()
                .find(|folder| is_server_drafts(folder))
                .cloned()
        });
    let orphan_drafts = local_by_path
        .values()
        .find(|folder| {
            folder.role == FolderRole::Drafts && folder.remote_path.starts_with(LOCAL_FOLDER_PREFIX)
        })
        .cloned();
    if let (Some(real), Some(orphan)) = (real_drafts, orphan_drafts) {
        let real_id = real.id.clone();
        let orphan_id = orphan.id.clone();
        let migrated = ctx
            .db
            .write(move |db| {
                let ids = nuncio_core::repo::messages::ids_in_folder(db, &orphan_id)?;
                if !ids.is_empty() {
                    // Both folders carry the drafts role, so the flag survives.
                    nuncio_core::repo::messages::move_to_folder(db, &ids, &real_id)?;
                }
                nuncio_core::repo::folders::delete(db, &orphan_id)?;
                Ok(ids)
            })
            .await?;
        if !migrated.is_empty() {
            events::emit_mail_changed(
                ctx.events.as_ref(),
                account_id.to_string(),
                vec![real.id.clone(), orphan.id.clone()],
                migrated,
                events::MailChangeKind::Updated,
            );
        }
        debug!(account = account_id, folder = %real.id, "migrated local Drafts into the server folder");
    }

    // Stable ordering: Inbox first, then role order, then name.
    upserted.sort_by(|a, b| {
        role_rank(a.role)
            .cmp(&role_rank(b.role))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    for (index, folder) in upserted.iter().enumerate() {
        if folder.sort_order != index as i64 {
            let id = folder.id.clone();
            let order = index as i64;
            ctx.db
                .write(move |db| nuncio_core::repo::folders::set_sort_order(db, &id, order))
                .await?;
        }
    }

    let mut refreshed = ctx
        .db
        .read({
            let account_id = account_id.to_string();
            move |db| nuncio_core::repo::folders::list(db, &account_id)
        })
        .await?;
    refreshed.sort_by(|a, b| {
        role_rank(a.role)
            .cmp(&role_rank(b.role))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    let folder_ids: Vec<String> = refreshed.iter().map(|folder| folder.id.clone()).collect();
    events::emit_folders_changed(ctx.events.as_ref(), account_id.to_string(), folder_ids);
    info!(
        account = account_id,
        folders = refreshed.len(),
        "folder sync finished"
    );
    Ok(refreshed)
}

async fn list_remote(conn: &mut ImapConnection) -> Result<Vec<RemoteFolder>> {
    let session = &mut conn.session;
    let result = tokio::time::timeout(COMMAND_TIMEOUT, async {
        let mut stream = session
            .list(Some(""), Some("\"*\""))
            .await
            .map_err(imap_error)?;
        let mut names = Vec::new();
        while let Some(name) = stream.try_next().await.map_err(imap_error)? {
            let path = name.name().to_string();
            if path.is_empty() {
                continue;
            }
            let delimiter = name.delimiter().map(str::to_string);
            let attributes: Vec<String> = name.attributes().iter().map(attribute_name).collect();
            let selectable = !attributes.iter().any(|value| value == "\\Noselect");
            let display = display_name(&path, delimiter.as_deref());
            names.push(RemoteFolder {
                path,
                delimiter,
                name: display,
                attributes,
                selectable,
                // IMAP addresses the mailbox by this same path.
                provider_id: None,
            });
        }
        Ok::<Vec<RemoteFolder>, AppError>(names)
    })
    .await
    .map_err(|_| AppError::network("LIST timed out"))??;
    Ok(result)
}

fn attribute_name(attribute: &NameAttribute<'_>) -> String {
    match attribute {
        NameAttribute::NoInferiors => "\\NoInferiors".into(),
        NameAttribute::NoSelect => "\\Noselect".into(),
        NameAttribute::Marked => "\\Marked".into(),
        NameAttribute::Unmarked => "\\Unmarked".into(),
        NameAttribute::All => "\\All".into(),
        NameAttribute::Archive => "\\Archive".into(),
        NameAttribute::Drafts => "\\Drafts".into(),
        NameAttribute::Flagged => "\\Flagged".into(),
        NameAttribute::Junk => "\\Junk".into(),
        NameAttribute::Sent => "\\Sent".into(),
        NameAttribute::Trash => "\\Trash".into(),
        NameAttribute::Extension(value) => value.to_string(),
        other => format!("{other:?}"),
    }
}

/// Map a remote folder (SPECIAL-USE attributes first, then well-known names).
///
/// Attribute names from both RFC 6154 (`\Sent`, `\Junk`, …) and the older
/// Gmail XLIST extension (`\SentMail`, `\AllMail`, `\Starred`, `\Spam`) are
/// accepted: servers that only implement XLIST otherwise fall through to the
/// name table, where a localised name can still lose its role.
pub fn role_for(path: &str, delimiter: Option<&str>, attributes: &[String]) -> FolderRole {
    if let Some(role) = role_from_attributes(attributes) {
        return role;
    }

    let lower_path = crate::util::decode_mailbox_name(path).to_lowercase();
    let leaf = leaf_name(&lower_path, delimiter);
    let gmail = lower_path.starts_with("[gmail]") || lower_path.starts_with("[google mail]");

    if gmail {
        return match leaf.as_str() {
            "all mail" | "所有邮件" => FolderRole::All,
            "important" | "重要" => FolderRole::Important,
            "sent mail" | "已发邮件" | "已发送邮件" => FolderRole::Sent,
            "drafts" | "草稿" | "草稿箱" => FolderRole::Drafts,
            "trash" | "已删除邮件" | "已删除" => FolderRole::Trash,
            "spam" | "junk" | "垃圾邮件" => FolderRole::Junk,
            "starred" | "已加星标" | "星标" => FolderRole::Flagged,
            // A localised Gmail container name that the table does not know:
            // fall through to the generic table rather than giving up.
            other => generic_leaf_role(other),
        };
    }

    let separator = delimiter.filter(|value| !value.is_empty());
    let nested = separator
        .map(|delimiter| lower_path.contains(delimiter))
        .unwrap_or(false)
        || lower_path.contains('/');
    let parent_is_inbox = lower_path
        .rsplit_once('/')
        .or_else(|| separator.and_then(|delimiter| lower_path.rsplit_once(delimiter)))
        .map(|(parent, _)| parent.eq_ignore_ascii_case("inbox"))
        .unwrap_or(false);
    if nested && !parent_is_inbox {
        return FolderRole::Other;
    }

    generic_leaf_role(&leaf)
}

/// The role a folder declares through a SPECIAL-USE attribute, if any.
///
/// This is the authoritative signal: the server itself says "this is the
/// trash".  `None` means the role can only be guessed from the name, which is
/// what [`resolve_role_conflicts`] treats as the weaker claim.
fn role_from_attributes(attributes: &[String]) -> Option<FolderRole> {
    for attribute in attributes {
        let role = match attribute.to_lowercase().as_str() {
            "\\all" | "\\allmail" => FolderRole::All,
            "\\archive" => FolderRole::Archive,
            "\\drafts" => FolderRole::Drafts,
            "\\flagged" | "\\starred" => FolderRole::Flagged,
            "\\junk" | "\\spam" => FolderRole::Junk,
            "\\sent" | "\\sentmail" => FolderRole::Sent,
            "\\trash" => FolderRole::Trash,
            "\\important" => FolderRole::Important,
            "\\inbox" => FolderRole::Inbox,
            _ => continue,
        };
        return Some(role);
    }
    None
}

/// Give every role to at most one folder.
///
/// Servers regularly return several folders whose names all look like the same
/// system folder.  An Outlook.com mailbox, for instance, commonly carries a
/// plain `Trash` created by some other mail client next to its real
/// `已删除邮件`.  Classifying each folder independently gave all of them the
/// `trash` role, and `repo::folders::by_role` then returned whichever one
/// happened to sort first -- so "delete" moved messages into the wrong folder.
///
/// The rule is that a folder which *declares* the role through a SPECIAL-USE
/// attribute beats one that was only inferred from its name.  Remaining
/// duplicates are demoted to [`FolderRole::Other`], which leaves them listed
/// as ordinary folders instead of hijacking the system folder.  With no
/// explicit claimant at all the first folder in server order keeps the role,
/// which reproduces the previous behaviour for servers that publish no
/// SPECIAL-USE at all.
fn resolve_role_conflicts(remotes: &[RemoteFolder]) -> Vec<FolderRole> {
    let claimed: Vec<FolderRole> = remotes
        .iter()
        .map(|remote| role_for(&remote.path, remote.delimiter.as_deref(), &remote.attributes))
        .collect();

    // Winner index per role, preferring an explicit attribute.
    let mut winner: HashMap<FolderRole, usize> = HashMap::new();
    for (index, role) in claimed.iter().enumerate() {
        if *role == FolderRole::Other {
            continue;
        }
        let explicit = role_from_attributes(&remotes[index].attributes) == Some(*role);
        match winner.get(role) {
            None => {
                winner.insert(*role, index);
            }
            Some(&current) => {
                let current_explicit =
                    role_from_attributes(&remotes[current].attributes) == Some(*role);
                if explicit && !current_explicit {
                    winner.insert(*role, index);
                }
            }
        }
    }

    claimed
        .into_iter()
        .enumerate()
        .map(|(index, role)| {
            if role == FolderRole::Other || winner.get(&role) == Some(&index) {
                role
            } else {
                FolderRole::Other
            }
        })
        .collect()
}

/// Well-known leaf names, shared by Gmail-style containers and flat folders.
///
/// Besides English and Chinese this carries the German and Russian names that
/// GMX and Yandex/mail.ru actually return: those servers localise the mailbox
/// names per account language and do not always attach SPECIAL-USE
/// attributes, so a name table is the only way to find the core folders.
///
/// `Outbox` is deliberately absent: it is a local-only role backed by the
/// outbox table, and mapping a server folder onto it would show queued mail
/// under a folder that the list query never reads.
fn generic_leaf_role(leaf: &str) -> FolderRole {
    match leaf {
        "inbox" | "收件箱" | "posteingang" | "входящие" => FolderRole::Inbox,
        "sent"
        | "sent mail"
        | "sent messages"
        | "sent items"
        | "已发送"
        | "已发送邮件"
        | "发件箱"
        | "寄件备份"
        | "gesendet"
        | "отправленные" => FolderRole::Sent,
        "drafts" | "draft" | "草稿" | "草稿箱" | "entwürfe" | "entwuerfe" | "черновики" => {
            FolderRole::Drafts
        }
        "trash" | "deleted" | "deleted mail" | "deleted messages" | "deleted items" | "bin"
        | "已删除" | "已删除邮件" | "垃圾箱" | "废纸篓" | "回收站" | "papierkorb" | "gelöscht"
        | "geloescht" | "корзина" => FolderRole::Trash,
        "junk" | "junk e-mail" | "junk email" | "junk mail" | "spam" | "bulk mail" | "垃圾邮件"
        | "广告邮件" | "spamverdacht" | "спам" => FolderRole::Junk,
        "archive" | "archives" | "归档" | "存档" | "archiv" | "архив" => {
            FolderRole::Archive
        }
        "all" | "all mail" => FolderRole::All,
        "flagged" | "starred" | "星标" => FolderRole::Flagged,
        "important" | "重要" => FolderRole::Important,
        _ => FolderRole::Other,
    }
}

/// Leaf name, dropping a trailing delimiter and quotes.
fn leaf_name(path: &str, delimiter: Option<&str>) -> String {
    let trimmed = path.trim_end_matches(['/', '\\', '.']);
    let leaf = match delimiter {
        Some(delimiter) if !delimiter.is_empty() => {
            trimmed.rsplit(delimiter).next().unwrap_or(trimmed)
        }
        _ => trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed),
    };
    leaf.trim_matches('"').trim().to_lowercase()
}

fn display_name(path: &str, delimiter: Option<&str>) -> String {
    let trimmed = path.trim_end_matches(['/', '\\', '.']);
    let leaf = match delimiter {
        Some(delimiter) if !delimiter.is_empty() => {
            trimmed.rsplit(delimiter).next().unwrap_or(trimmed)
        }
        _ => trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed),
    };
    let leaf = leaf.trim_matches('"').trim();
    if leaf.is_empty() {
        trimmed.to_string()
    } else {
        leaf.to_string()
    }
}

fn parent_path(path: &str, delimiter: &str) -> Option<String> {
    let trimmed = path.trim_end_matches(delimiter);
    let (parent, _) = trimmed.rsplit_once(delimiter)?;
    if parent.is_empty() {
        None
    } else {
        Some(parent.to_string())
    }
}

fn is_gmail_all_or_important(path: &str) -> bool {
    let lower = path.to_lowercase();
    let leaf = leaf_name(&lower, Some("/"));
    let in_container = lower.starts_with("[gmail]/") || lower.starts_with("[google mail]/");
    in_container && matches!(leaf.as_str(), "all mail" | "important")
}

fn role_rank(role: FolderRole) -> u8 {
    match role {
        FolderRole::Inbox => 0,
        FolderRole::Flagged => 1,
        FolderRole::Drafts => 2,
        FolderRole::Sent => 3,
        FolderRole::Archive => 4,
        FolderRole::Important => 5,
        FolderRole::All => 6,
        FolderRole::Junk => 7,
        FolderRole::Trash => 8,
        FolderRole::Outbox => 9,
        FolderRole::Other => 10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(path: &str, delimiter: Option<&str>, attributes: &[&str]) -> FolderRole {
        role_for(
            path,
            delimiter,
            &attributes
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn special_use_wins_over_name() {
        assert_eq!(
            role_for("Misc/Things", Some("/"), &["\\Sent".into()]),
            FolderRole::Sent
        );
    }

    #[test]
    fn xlist_attributes_are_understood() {
        // Gmail's older XLIST names, still emitted by some servers.
        assert_eq!(
            role_for("Whatever", None, &["\\SentMail".into()]),
            FolderRole::Sent
        );
        assert_eq!(
            role_for("Whatever", None, &["\\AllMail".into()]),
            FolderRole::All
        );
        assert_eq!(
            role_for("Whatever", None, &["\\Starred".into()]),
            FolderRole::Flagged
        );
        assert_eq!(
            role_for("Whatever", None, &["\\Spam".into()]),
            FolderRole::Junk
        );
        assert_eq!(
            role_for("Whatever", None, &["\\Drafts".into()]),
            FolderRole::Drafts
        );
    }

    #[test]
    fn recognises_common_and_chinese_names() {
        assert_eq!(role_for("INBOX", None, &[]), FolderRole::Inbox);
        assert_eq!(role_for("已发送", None, &[]), FolderRole::Sent);
        assert_eq!(role_for("草稿箱", None, &[]), FolderRole::Drafts);
        assert_eq!(role_for("已删除邮件", None, &[]), FolderRole::Trash);
        assert_eq!(role_for("垃圾邮件", None, &[]), FolderRole::Junk);
        assert_eq!(role_for("归档", None, &[]), FolderRole::Archive);
        assert_eq!(role_for("Sent Messages", None, &[]), FolderRole::Sent);
        assert_eq!(role_for("Deleted Messages", None, &[]), FolderRole::Trash);
    }

    #[test]
    fn outlook_names() {
        for (path, expected) in [
            ("INBOX", FolderRole::Inbox),
            ("Sent Items", FolderRole::Sent),
            ("Deleted Items", FolderRole::Trash),
            ("Junk Email", FolderRole::Junk),
            ("Drafts", FolderRole::Drafts),
            ("Archive", FolderRole::Archive),
        ] {
            assert_eq!(role(path, Some("/"), &[]), expected, "{path}");
        }
        // Mailbox-like folders stay plain folders.
        for path in ["Notes", "Outbox", "Conversation History"] {
            assert_eq!(role(path, Some("/"), &[]), FolderRole::Other, "{path}");
        }
    }

    #[test]
    fn icloud_names() {
        for (path, expected) in [
            ("INBOX", FolderRole::Inbox),
            ("Sent Messages", FolderRole::Sent),
            ("Deleted Messages", FolderRole::Trash),
            ("Junk", FolderRole::Junk),
            ("Drafts", FolderRole::Drafts),
            ("Archive", FolderRole::Archive),
        ] {
            assert_eq!(role(path, Some("/"), &[]), expected, "{path}");
        }
    }

    #[test]
    fn yahoo_gmx_aol_mailru_names() {
        assert_eq!(role("Bulk Mail", Some("/"), &[]), FolderRole::Junk);
        assert_eq!(role("Draft", Some("/"), &[]), FolderRole::Drafts);
        assert_eq!(role("Spam", Some("/"), &[]), FolderRole::Junk);
        assert_eq!(role("Junk Mail", Some("/"), &[]), FolderRole::Junk);
        assert_eq!(role("Trash", Some("/"), &[]), FolderRole::Trash);
        // AOL/Yandex keep deleted mail here.
        assert_eq!(role("Recently Deleted", Some("/"), &[]), FolderRole::Other);
        // Yandex calls Trash "Deleted Items" (already covered above).
        assert_eq!(role("Templates", Some("/"), &[]), FolderRole::Other);
    }

    #[test]
    fn chinese_provider_names() {
        // QQ / Exmail / Aliyun / Sina / Sohu / 139.
        assert_eq!(role("已发送邮件", None, &[]), FolderRole::Sent);
        assert_eq!(role("发件箱", None, &[]), FolderRole::Sent);
        assert_eq!(role("寄件备份", None, &[]), FolderRole::Sent);
        assert_eq!(role("草稿", None, &[]), FolderRole::Drafts);
        assert_eq!(role("已删除", None, &[]), FolderRole::Trash);
        assert_eq!(role("回收站", None, &[]), FolderRole::Trash);
        assert_eq!(role("已删除邮件", None, &[]), FolderRole::Trash);
        // 163/139 ship a second junk-ish folder.
        assert_eq!(role("广告邮件", None, &[]), FolderRole::Junk);
        assert_eq!(role("垃圾邮件", None, &[]), FolderRole::Junk);
        assert_eq!(role("收件箱", None, &[]), FolderRole::Inbox);
        // A user folder stays a folder.
        assert_eq!(role("订阅邮件", None, &[]), FolderRole::Other);
        assert_eq!(role("广告合作", None, &[]), FolderRole::Other);
    }

    #[test]
    fn modified_utf7_paths_are_decoded_before_matching() {
        // QQ returns the Chinese names as modified UTF-7.
        assert_eq!(role("&XfJT0ZAB-", None, &[]), FolderRole::Sent);
        assert_eq!(role("&g0l6P3ux-", None, &[]), FolderRole::Drafts);
        assert_eq!(role("&XfJSIJZk-", None, &[]), FolderRole::Trash);
        assert_eq!(role("&V4NXPpCuTvY-", None, &[]), FolderRole::Junk);
        assert_eq!(role("INBOX", None, &[]), FolderRole::Inbox);
        // Nested, with a UTF-7 leaf under a UTF-7 parent.
        assert_eq!(
            role("&mHlM0Gs-/&V4NXPpCuTvY2A-", Some("/"), &[]),
            FolderRole::Other
        );
    }

    #[test]
    fn gmail_folders_and_google_mail_alias() {
        assert_eq!(role("[Gmail]/All Mail", Some("/"), &[]), FolderRole::All);
        assert_eq!(
            role("[Gmail]/Important", Some("/"), &[]),
            FolderRole::Important
        );
        assert_eq!(role("[Gmail]/Sent Mail", Some("/"), &[]), FolderRole::Sent);
        assert_eq!(role("[Gmail]/Starred", Some("/"), &[]), FolderRole::Flagged);
        assert_eq!(
            role("[Google Mail]/Sent Mail", Some("/"), &[]),
            FolderRole::Sent
        );
        assert_eq!(role("[Gmail]/Spam", Some("/"), &[]), FolderRole::Junk);
        assert_eq!(role("[Gmail]/Trash", Some("/"), &[]), FolderRole::Trash);
        assert_eq!(role("[Gmail]/Drafts", Some("/"), &[]), FolderRole::Drafts);
        assert_eq!(role("[Gmail]/My Label", Some("/"), &[]), FolderRole::Other);
        assert!(is_gmail_all_or_important("[Gmail]/All Mail"));
        assert!(is_gmail_all_or_important("[Google Mail]/Important"));
    }

    #[test]
    fn gmail_localised_system_folders() {
        // A Gmail UI set to Chinese still exposes the same mailbox names, but
        // Gmail Workspace tenants can rename them; keep the mapping working.
        assert_eq!(role("[Gmail]/已发邮件", Some("/"), &[]), FolderRole::Sent);
        assert_eq!(role("[Gmail]/垃圾邮件", Some("/"), &[]), FolderRole::Junk);
        assert_eq!(role("[Gmail]/草稿", Some("/"), &[]), FolderRole::Drafts);
    }

    #[test]
    fn hierarchy_is_split_by_delimiter() {
        assert_eq!(
            role_for("Projects/Archive", Some("/"), &[]),
            FolderRole::Other
        );
        assert_eq!(parent_path("a/b/c", "/").as_deref(), Some("a/b"));
        assert_eq!(parent_path("a", "/"), None);
    }

    #[test]
    fn hierarchy_respects_other_delimiters() {
        assert_eq!(role("Projects.Archive", Some("."), &[]), FolderRole::Other);
        assert_eq!(role("Sent.2026", Some("."), &[]), FolderRole::Other);
        assert_eq!(parent_path("a.b.c", ".").as_deref(), Some("a.b"));
    }

    #[test]
    fn subfolders_of_inbox_keep_their_own_role() {
        // Outlook/163 keep rules-based folders under INBOX.
        assert_eq!(role("INBOX/Sent", Some("/"), &[]), FolderRole::Sent);
        assert_eq!(role("INBOX/Drafts", Some("/"), &[]), FolderRole::Drafts);
        assert_eq!(role("INBOX/Newsletters", Some("/"), &[]), FolderRole::Other);
    }

    #[test]
    fn proton_bridge_folders() {
        // Proton Bridge groups user folders under these containers.
        assert_eq!(role("Folders", Some("/"), &[]), FolderRole::Other);
        assert_eq!(role("Labels", Some("/"), &[]), FolderRole::Other);
        assert_eq!(role("Folders/Receipts", Some("/"), &[]), FolderRole::Other);
        assert_eq!(role("Labels/Travel", Some("/"), &[]), FolderRole::Other);
        // Bridge still exposes these as top-level system folders.
        assert_eq!(role("All Mail", Some("/"), &[]), FolderRole::All);
        assert_eq!(role("Starred", Some("/"), &[]), FolderRole::Flagged);
        assert_eq!(role("Spam", Some("/"), &[]), FolderRole::Junk);
    }

    #[test]
    fn gmx_german_names() {
        // GMX localises the mailbox names per account language and does not
        // always attach SPECIAL-USE, which is why clients create duplicates.
        assert_eq!(role("Posteingang", Some("/"), &[]), FolderRole::Inbox);
        assert_eq!(role("Gesendet", Some("/"), &[]), FolderRole::Sent);
        assert_eq!(role("Entwürfe", Some("/"), &[]), FolderRole::Drafts);
        assert_eq!(role("Papierkorb", Some("/"), &[]), FolderRole::Trash);
        // Older GMX accounts still have "Gelöscht".
        assert_eq!(role("Gelöscht", Some("/"), &[]), FolderRole::Trash);
        assert_eq!(role("Spamverdacht", Some("/"), &[]), FolderRole::Junk);
        assert_eq!(role("Archiv", Some("/"), &[]), FolderRole::Archive);
        // Umlauts arrive in modified UTF-7: "Entw&APw-rfe".
        assert_eq!(role("Entw&APw-rfe", Some("/"), &[]), FolderRole::Drafts);
        assert_eq!(role("Gel&APY-scht", Some("/"), &[]), FolderRole::Trash);
    }

    #[test]
    fn yandex_and_mail_ru_russian_names() {
        for (path, expected) in [
            ("Входящие", FolderRole::Inbox),
            ("Отправленные", FolderRole::Sent),
            ("Черновики", FolderRole::Drafts),
            ("Корзина", FolderRole::Trash),
            ("Спам", FolderRole::Junk),
            ("Архив", FolderRole::Archive),
        ] {
            assert_eq!(role(path, Some("/"), &[]), expected, "{path}");
        }
        // On the wire they are modified UTF-7.
        assert_eq!(
            role("&BB4EQgQ,BEAEMAQyBDsENQQ9BD0ESwQ1-", Some("/"), &[]),
            FolderRole::Sent
        );
        assert_eq!(
            role("&BBoEPgRABDcEOAQ9BDA-", Some("/"), &[]),
            FolderRole::Trash
        );
    }

    #[test]
    fn rfc2047_encoded_names_are_decoded() {
        // Some servers double-encode the name instead of using UTF-7.
        let encoded = "=?UTF-8?B?5bey5Y+R6YCB?=";
        assert_eq!(crate::util::decode_mailbox_name(encoded), "已发送");
        assert_eq!(role(encoded, None, &[]), FolderRole::Sent);
    }

    #[test]
    fn display_name_uses_leaf() {
        assert_eq!(display_name("Projects/2026", Some("/")), "2026");
        assert_eq!(display_name("INBOX", Some("/")), "INBOX");
    }
}

#[cfg(test)]
mod reconcile_tests {
    use super::*;
    use nuncio_core::models::{AccountState, AuthKind, ProviderCapabilities, ProviderKind};

    async fn test_ctx() -> CoreContext {
        let dir = std::env::temp_dir().join(format!(
            "nuncio-mail-folder-reconcile-{}",
            nuncio_core::models::new_id()
        ));
        CoreContext::for_test(nuncio_core::Paths::new(dir))
            .await
            .expect("context")
    }

    async fn seed_account(ctx: &CoreContext) {
        let account = Account {
            id: "acc".into(),
            provider: ProviderKind::Outlook,
            email: "me@example.com".into(),
            display_name: "Me".into(),
            color: "#000000".into(),
            auth_kind: AuthKind::Oauth,
            oauth_provider: Some(nuncio_core::models::OAuthProvider::Microsoft),
            imap: None,
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
            notifications_enabled: false,
            include_in_unified: true,
            sort_order: 0,
            state: AccountState::Ok,
            state_message: None,
            capabilities: ProviderCapabilities::defaults_for(ProviderKind::Outlook),
            created_at: nuncio_core::models::now(),
        };
        ctx.db
            .write(move |db| nuncio_core::repo::accounts::insert(db, &account))
            .await
            .expect("account");
    }

    fn remote(path: &str) -> RemoteFolder {
        RemoteFolder {
            path: path.to_string(),
            delimiter: Some("/".to_string()),
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            attributes: Vec::new(),
            selectable: true,
            provider_id: None,
        }
    }

    fn remote_with(path: &str, attributes: &[&str]) -> RemoteFolder {
        let mut folder = remote(path);
        folder.attributes = attributes.iter().map(|value| value.to_string()).collect();
        folder
    }

    #[test]
    fn explicit_special_use_wins_a_duplicate_role() {
        // An Outlook.com mailbox: a plain `Trash` left behind by another client
        // next to the real Deleted Items.  Only the latter carries `\Trash`.
        let remotes = vec![
            remote_with("Trash", &[]),
            remote_with("已删除", &[]),
            remote_with("已删除邮件", &["\\Trash"]),
        ];
        assert_eq!(
            resolve_role_conflicts(&remotes),
            vec![FolderRole::Other, FolderRole::Other, FolderRole::Trash]
        );
    }

    #[test]
    fn outbox_never_takes_the_sent_role_from_sent_items() {
        // `发件箱` is the server's Outbox and is matched to `Sent` by name;
        // `已发送邮件` declares `\Sent`.  The declared folder must win, or
        // opening "Sent" would show the empty outbox.
        let remotes = vec![remote("发件箱"), remote_with("已发送邮件", &["\\Sent"])];
        assert_eq!(
            resolve_role_conflicts(&remotes),
            vec![FolderRole::Other, FolderRole::Sent]
        );
    }

    #[test]
    fn duplicate_roles_without_special_use_keep_the_first_folder() {
        // Servers that publish no SPECIAL-USE at all still need one folder per
        // role; the first in server order keeps it.
        let remotes = vec![remote("Trash"), remote("已删除邮件")];
        assert_eq!(
            resolve_role_conflicts(&remotes),
            vec![FolderRole::Trash, FolderRole::Other]
        );
    }

    #[test]
    fn distinct_roles_are_left_alone() {
        let remotes = vec![
            remote_with("INBOX", &["\\Inbox"]),
            remote("已发送邮件"),
            remote("垃圾邮件"),
        ];
        assert_eq!(
            resolve_role_conflicts(&remotes),
            vec![FolderRole::Inbox, FolderRole::Sent, FolderRole::Junk]
        );
    }

    async fn local_paths(ctx: &CoreContext) -> Vec<String> {
        ctx.db
            .read(|db| nuncio_core::repo::folders::list(db, "acc"))
            .await
            .expect("folders")
            .into_iter()
            .map(|folder| folder.remote_path)
            .collect()
    }

    async fn local_folder(ctx: &CoreContext, path: &str) -> Folder {
        ctx.db
            .read(|db| nuncio_core::repo::folders::list(db, "acc"))
            .await
            .expect("folders")
            .into_iter()
            .find(|folder| folder.remote_path == path)
            .unwrap_or_else(|| panic!("folder {path} is missing"))
    }

    /// The real-world case: an Outlook.com mailbox carries a stray `发件箱`
    /// (the server Outbox) next to its real `已发送邮件`.  Before roles were
    /// resolved across the listing, `发件箱` sorted first and took the `sent`
    /// role -- so "Sent" opened an empty folder and deletes/archives picked the
    /// wrong target.  Once the real Sent appears, the stray folder must lose
    /// both the role and the star it was given automatically.
    #[tokio::test]
    async fn the_real_system_folder_takes_over_from_a_name_guess() {
        let ctx = test_ctx().await;
        seed_account(&ctx).await;

        // First sync: `发件箱` is the only Sent-looking folder.
        reconcile(&ctx, "acc", vec![remote("INBOX"), remote("发件箱")], false, true)
            .await
            .expect("first reconcile");
        let stray = local_folder(&ctx, "发件箱").await;
        assert_eq!(stray.role, FolderRole::Sent);
        assert!(stray.is_favorite, "the Sent default stars the folder");

        // Second sync: the real Sent folder shows up, declaring `\Sent`.
        reconcile(
            &ctx,
            "acc",
            vec![
                remote("INBOX"),
                remote("发件箱"),
                remote_with("已发送邮件", &["\\Sent"]),
            ],
            false,
            true,
        )
        .await
        .expect("second reconcile");

        let real = local_folder(&ctx, "已发送邮件").await;
        assert_eq!(real.role, FolderRole::Sent);
        assert!(real.is_favorite);

        let stray = local_folder(&ctx, "发件箱").await;
        assert_eq!(stray.role, FolderRole::Other);
        assert!(
            !stray.is_favorite,
            "a folder that lost its system role must not stay pinned in the sidebar"
        );
    }

    /// The regression this guards: a partial Graph listing (one failed
    /// `childFolders` call) must not delete the whole missing subtree.
    #[tokio::test]
    async fn incomplete_listing_keeps_folders_missing_from_it() {
        let ctx = test_ctx().await;
        seed_account(&ctx).await;
        reconcile(
            &ctx,
            "acc",
            vec![remote("INBOX"), remote("Projects"), remote("Projects/2026")],
            false,
            true,
        )
        .await
        .expect("initial reconcile");

        // The Projects subtree was not read this round: incomplete listing.
        reconcile(&ctx, "acc", vec![remote("INBOX")], false, false)
            .await
            .expect("partial reconcile");

        let paths = local_paths(&ctx).await;
        assert!(paths.iter().any(|path| path == "Projects"));
        assert!(paths.iter().any(|path| path == "Projects/2026"));
    }

    /// The composer creates a `local:drafts` placeholder when an account has no
    /// server Drafts folder. Once one appears, its drafts must move there and
    /// the placeholder must go, otherwise its messages are counted twice and
    /// two rows are both labelled "Drafts".
    #[tokio::test]
    async fn local_drafts_folder_is_reclaimed_when_the_server_provides_drafts() {
        let ctx = test_ctx().await;
        seed_account(&ctx).await;

        let orphan = Folder {
            id: "local-drafts".into(),
            account_id: "acc".into(),
            parent_id: None,
            remote_path: "local:drafts".into(),
            name: "Drafts".into(),
            role: FolderRole::Drafts,
            unread_count: 0,
            total_count: 0,
            is_favorite: false,
            sync_enabled: false,
            is_selectable: true,
            sort_order: 2,
            last_sync_at: None,
        };
        let mut summary = nuncio_core::models::MessageSummary::new(
            "draft-1",
            "acc",
            "local-drafts",
            "thread:draft-1",
            nuncio_core::models::now(),
        );
        summary.seen = true;
        summary.is_draft = true;
        let stored = nuncio_core::models::StoredMessage::new(summary);
        ctx.db
            .write(move |db| {
                nuncio_core::repo::folders::insert(db, &orphan)?;
                nuncio_core::repo::messages::upsert(db, &stored)?;
                Ok(())
            })
            .await
            .expect("seed local drafts");

        reconcile(
            &ctx,
            "acc",
            vec![remote("INBOX"), remote("Drafts")],
            false,
            true,
        )
        .await
        .expect("reconcile");

        let folders = ctx
            .db
            .read(|db| nuncio_core::repo::folders::list(db, "acc"))
            .await
            .expect("folders");
        assert!(
            !folders.iter().any(|folder| folder.remote_path == "local:drafts"),
            "the placeholder must be deleted"
        );
        let real = folders
            .iter()
            .find(|folder| folder.role == FolderRole::Drafts)
            .expect("server drafts folder");
        assert_eq!(real.remote_path, "Drafts");

        let message = ctx
            .db
            .read(|db| nuncio_core::repo::messages::get_required(db, "draft-1"))
            .await
            .expect("draft message");
        assert_eq!(message.summary.folder_id, real.id);
        assert!(
            message.summary.is_draft,
            "moving between drafts folders must not demote the draft"
        );
    }

    /// A complete listing must still prune, otherwise folders deleted on the
    /// server would linger forever.
    #[tokio::test]
    async fn complete_listing_prunes_folders_missing_from_it() {
        let ctx = test_ctx().await;
        seed_account(&ctx).await;
        reconcile(
            &ctx,
            "acc",
            vec![remote("INBOX"), remote("Projects")],
            false,
            true,
        )
        .await
        .expect("initial reconcile");

        reconcile(&ctx, "acc", vec![remote("INBOX")], false, true)
            .await
            .expect("complete reconcile");

        let paths = local_paths(&ctx).await;
        assert!(!paths.iter().any(|path| path == "Projects"));
    }
}
