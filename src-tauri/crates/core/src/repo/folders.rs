//! Mail folder repository: server folder upsert, tree, counts, favorites.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::error::AppError;
use crate::models::{Folder, FolderRole, FolderTree, UnifiedCounts};
use crate::repo::{conflict, get_bool, get_opt_dt, get_opt_string, get_string, now_string};
use crate::Result;

const COLUMNS: &str = "\
    id, account_id, parent_id, remote_path, name, role, unread_count, total_count,\
    is_favorite, sync_enabled, is_selectable, sort_order, last_sync_at";

fn row_to_folder(row: &Row<'_>) -> Result<Folder> {
    let role: String = get_string(row, "role")?;
    Ok(Folder {
        id: get_string(row, "id")?,
        account_id: get_string(row, "account_id")?,
        parent_id: get_opt_string(row, "parent_id")?,
        remote_path: get_string(row, "remote_path")?,
        name: get_string(row, "name")?,
        role: role.parse().unwrap_or(FolderRole::Other),
        unread_count: row.get("unread_count")?,
        total_count: row.get("total_count")?,
        is_favorite: get_bool(row, "is_favorite")?,
        sync_enabled: get_bool(row, "sync_enabled")?,
        is_selectable: get_bool(row, "is_selectable")?,
        sort_order: row.get("sort_order")?,
        last_sync_at: get_opt_dt(row, "last_sync_at")?,
    })
}

pub fn list(conn: &Connection, account_id: &str) -> Result<Vec<Folder>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM folder WHERE account_id = ?1 ORDER BY sort_order ASC, name COLLATE NOCASE ASC"
    ))?;
    let mut rows = stmt.query(params![account_id])?;
    let mut folders = Vec::new();
    while let Some(row) = rows.next()? {
        folders.push(row_to_folder(row)?);
    }
    Ok(folders)
}

pub fn list_all(conn: &Connection) -> Result<Vec<Folder>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM folder ORDER BY account_id ASC, sort_order ASC, name COLLATE NOCASE ASC"
    ))?;
    let mut rows = stmt.query([])?;
    let mut folders = Vec::new();
    while let Some(row) = rows.next()? {
        folders.push(row_to_folder(row)?);
    }
    Ok(folders)
}

/// Build the folder tree for an account. Orphaned folders become roots.
pub fn tree(conn: &Connection, account_id: &str) -> Result<Vec<FolderTree>> {
    let folders = list(conn, account_id)?;
    Ok(build_tree(&folders))
}

fn build_tree(folders: &[Folder]) -> Vec<FolderTree> {
    let mut children_by_parent: BTreeMap<Option<String>, Vec<&Folder>> = BTreeMap::new();
    let known: std::collections::HashSet<&str> = folders.iter().map(|f| f.id.as_str()).collect();
    for folder in folders {
        let parent = folder
            .parent_id
            .clone()
            .filter(|parent_id| known.contains(parent_id.as_str()) && parent_id != &folder.id);
        children_by_parent.entry(parent).or_default().push(folder);
    }
    for values in children_by_parent.values_mut() {
        values.sort_by(|a, b| {
            a.sort_order
                .cmp(&b.sort_order)
                .then_with(|| a.name.cmp(&b.name))
        });
    }
    fn build(
        parent: Option<String>,
        map: &BTreeMap<Option<String>, Vec<&Folder>>,
    ) -> Vec<FolderTree> {
        map.get(&parent)
            .map(|folders| {
                folders
                    .iter()
                    .map(|folder| FolderTree {
                        folder: (*folder).clone(),
                        children: build(Some(folder.id.clone()), map),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    build(None, &children_by_parent)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Folder>> {
    let sql = format!("SELECT {COLUMNS} FROM folder WHERE id = ?1");
    crate::repo::query_one(conn, &sql, params![id], row_to_folder)
}

pub fn get_required(conn: &Connection, id: &str) -> Result<Folder> {
    get(conn, id)?.ok_or_else(|| AppError::NotFound(format!("folder {id} not found")))
}

pub fn by_role(conn: &Connection, account_id: &str, role: FolderRole) -> Result<Option<Folder>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM folder WHERE account_id = ?1 AND role = ?2 ORDER BY sort_order ASC LIMIT 1"
    );
    crate::repo::query_one(
        conn,
        &sql,
        params![account_id, role.as_str()],
        row_to_folder,
    )
}

pub fn by_remote_path(
    conn: &Connection,
    account_id: &str,
    remote_path: &str,
) -> Result<Option<Folder>> {
    let sql = format!("SELECT {COLUMNS} FROM folder WHERE account_id = ?1 AND remote_path = ?2");
    crate::repo::query_one(conn, &sql, params![account_id, remote_path], row_to_folder)
}

pub fn list_sync_enabled(conn: &Connection, account_id: &str) -> Result<Vec<Folder>> {
    Ok(list(conn, account_id)?
        .into_iter()
        .filter(|folder| folder.sync_enabled && folder.is_selectable)
        .collect())
}

pub fn list_favorites(conn: &Connection, account_id: &str) -> Result<Vec<Folder>> {
    Ok(list(conn, account_id)?
        .into_iter()
        .filter(|folder| folder.is_favorite)
        .collect())
}

/// Insert/update a folder discovered on the server.
///
/// Matches on `(account_id, remote_path)`; keeps the local id, favorite and
/// sync flags when the folder is already known.
pub fn upsert_remote(conn: &Connection, folder: &Folder) -> Result<Folder> {
    if folder.remote_path.trim().is_empty() {
        return Err(AppError::invalid_input(
            "folder remote path must not be empty",
        ));
    }
    let now = now_string();
    let id = if folder.id.trim().is_empty() {
        crate::models::new_id()
    } else {
        folder.id.clone()
    };
    conn.execute(
        "INSERT INTO folder (
            id, account_id, parent_id, remote_path, name, role, unread_count, total_count,
            is_favorite, sync_enabled, is_selectable, sort_order, last_sync_at, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14)
         ON CONFLICT(account_id, remote_path) DO UPDATE SET
            parent_id = excluded.parent_id,
            name = excluded.name,
            role = excluded.role,
            is_selectable = excluded.is_selectable,
            sort_order = excluded.sort_order,
            updated_at = excluded.updated_at",
        params![
            id,
            folder.account_id,
            folder.parent_id,
            folder.remote_path,
            folder.name,
            folder.role.as_str(),
            folder.unread_count,
            folder.total_count,
            folder.is_favorite as i64,
            folder.sync_enabled as i64,
            folder.is_selectable as i64,
            folder.sort_order,
            folder.last_sync_at.map(crate::repo::dt_string),
            now
        ],
    )?;
    by_remote_path(conn, &folder.account_id, &folder.remote_path)?
        .ok_or_else(|| AppError::Db("folder upsert did not persist".into()))
}

/// Create a local folder (the mail engine reconciles the remote name later).
pub fn create(
    conn: &Connection,
    account_id: &str,
    parent_id: Option<&str>,
    name: &str,
) -> Result<Folder> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::invalid_input("folder name must not be empty"));
    }
    if by_remote_path(conn, account_id, name)?.is_some() {
        return Err(conflict(format!("folder {name} already exists")));
    }
    if let Some(parent_id) = parent_id {
        let parent = get_required(conn, parent_id)?;
        if parent.account_id != account_id {
            return Err(AppError::invalid_input(
                "parent folder belongs to another account",
            ));
        }
    }
    let sort_order: i64 = conn.query_row(
        "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM folder
         WHERE account_id = ?1 AND (parent_id IS ?2 OR (parent_id IS NULL AND ?2 IS NULL))",
        params![account_id, parent_id],
        |row| row.get(0),
    )?;
    let folder = Folder {
        id: crate::models::new_id(),
        account_id: account_id.to_string(),
        parent_id: parent_id.map(str::to_string),
        remote_path: name.to_string(),
        name: name.to_string(),
        role: FolderRole::Other,
        unread_count: 0,
        total_count: 0,
        is_favorite: false,
        sync_enabled: true,
        is_selectable: true,
        sort_order,
        last_sync_at: None,
    };
    insert(conn, &folder)?;
    Ok(folder)
}

pub fn insert(conn: &Connection, folder: &Folder) -> Result<()> {
    let now = now_string();
    conn.execute(
        "INSERT INTO folder (
            id, account_id, parent_id, remote_path, name, role, unread_count, total_count,
            is_favorite, sync_enabled, is_selectable, sort_order, last_sync_at, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14)",
        params![
            folder.id,
            folder.account_id,
            folder.parent_id,
            folder.remote_path,
            folder.name,
            folder.role.as_str(),
            folder.unread_count,
            folder.total_count,
            folder.is_favorite as i64,
            folder.sync_enabled as i64,
            folder.is_selectable as i64,
            folder.sort_order,
            folder.last_sync_at.map(crate::repo::dt_string),
            now
        ],
    )?;
    Ok(())
}

/// Insert a folder on the restore path.
///
/// A restored folder is a preference carrier: the next server sync reconciles
/// it by `(account_id, remote_path)`. On a conflict (for example a local
/// account folder that already exists on this device) keep the existing id —
/// messages reference it — and apply the restored preference fields. Counters
/// and `last_sync_at` are deliberately left alone here and are zeroed by the
/// caller.
pub fn restore(conn: &Connection, folder: &Folder) -> Result<()> {
    if folder.remote_path.trim().is_empty() {
        return Err(AppError::invalid_input(
            "folder remote path must not be empty",
        ));
    }
    let now = now_string();
    conn.execute(
        "INSERT INTO folder (
            id, account_id, parent_id, remote_path, name, role, unread_count, total_count,
            is_favorite, sync_enabled, is_selectable, sort_order, last_sync_at, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0, ?7, ?8, ?9, ?10, NULL, ?11, ?11)
         ON CONFLICT DO UPDATE SET
            parent_id = excluded.parent_id,
            remote_path = excluded.remote_path,
            name = excluded.name,
            role = excluded.role,
            is_favorite = excluded.is_favorite,
            sync_enabled = excluded.sync_enabled,
            is_selectable = excluded.is_selectable,
            sort_order = excluded.sort_order,
            updated_at = excluded.updated_at",
        params![
            folder.id,
            folder.account_id,
            folder.parent_id,
            folder.remote_path,
            folder.name,
            folder.role.as_str(),
            folder.is_favorite as i64,
            folder.sync_enabled as i64,
            folder.is_selectable as i64,
            folder.sort_order,
            now
        ],
    )?;
    Ok(())
}

pub fn rename(conn: &Connection, id: &str, name: &str) -> Result<Folder> {
    let changed = conn.execute(
        "UPDATE folder SET name = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, name, now_string()],
    )?;
    if changed == 0 {
        return Err(AppError::NotFound(format!("folder {id} not found")));
    }
    get_required(conn, id)
}

pub fn move_folder(conn: &Connection, id: &str, new_parent_id: Option<&str>) -> Result<Folder> {
    if let Some(parent_id) = new_parent_id {
        if parent_id == id {
            return Err(AppError::invalid_input("a folder cannot be its own parent"));
        }
        let folder = get_required(conn, id)?;
        let parent = get_required(conn, parent_id)?;
        if parent.account_id != folder.account_id {
            return Err(AppError::invalid_input(
                "parent folder belongs to another account",
            ));
        }
        // Reject cycles: walk up from the new parent.
        let mut cursor = Some(parent_id.to_string());
        let mut hops = 0;
        while let Some(current) = cursor {
            if current == id {
                return Err(AppError::invalid_input(
                    "cannot move a folder into its own subtree",
                ));
            }
            hops += 1;
            if hops > 1000 {
                break;
            }
            cursor = get(conn, &current)?.and_then(|folder| folder.parent_id);
        }
    }
    let changed = conn.execute(
        "UPDATE folder SET parent_id = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, new_parent_id, now_string()],
    )?;
    if changed == 0 {
        return Err(AppError::NotFound(format!("folder {id} not found")));
    }
    get_required(conn, id)
}

pub fn set_favorite(conn: &Connection, id: &str, value: bool) -> Result<()> {
    conn.execute(
        "UPDATE folder SET is_favorite = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, value as i64, now_string()],
    )?;
    Ok(())
}

pub fn set_sync_enabled(conn: &Connection, id: &str, value: bool) -> Result<()> {
    conn.execute(
        "UPDATE folder SET sync_enabled = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, value as i64, now_string()],
    )?;
    Ok(())
}

pub fn set_role(conn: &Connection, id: &str, role: FolderRole) -> Result<()> {
    conn.execute(
        "UPDATE folder SET role = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, role.as_str(), now_string()],
    )?;
    Ok(())
}

pub fn set_sort_order(conn: &Connection, id: &str, sort_order: i64) -> Result<()> {
    conn.execute(
        "UPDATE folder SET sort_order = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, sort_order, now_string()],
    )?;
    Ok(())
}

/// Store IMAP sync markers for a folder.
pub fn set_sync_flags(
    conn: &Connection,
    id: &str,
    uidvalidity: Option<i64>,
    uidnext: Option<i64>,
    highest_modseq: Option<i64>,
) -> Result<()> {
    conn.execute(
        "UPDATE folder SET uidvalidity = ?2, uidnext = ?3, highest_modseq = ?4, updated_at = ?5 WHERE id = ?1",
        params![id, uidvalidity, uidnext, highest_modseq, now_string()],
    )?;
    Ok(())
}

/// UIDVALIDITY generation the folder was last synced with. Used to decide
/// whether a restored UID still addresses the same message.
pub fn uidvalidity(conn: &Connection, id: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT uidvalidity FROM folder WHERE id = ?1",
            params![id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten())
}

pub fn set_last_sync_at(
    conn: &Connection,
    id: &str,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    conn.execute(
        "UPDATE folder SET last_sync_at = ?2, updated_at = ?2 WHERE id = ?1",
        params![id, crate::repo::dt_string(at)],
    )?;
    Ok(())
}

/// Read (uidvalidity, uidnext, highest_modseq).
pub fn sync_flags(conn: &Connection, id: &str) -> Result<(Option<i64>, Option<i64>, Option<i64>)> {
    Ok(conn.query_row(
        "SELECT uidvalidity, uidnext, highest_modseq FROM folder WHERE id = ?1",
        params![id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?)
}

/// Recompute unread/total counts from the message table.
pub fn recompute_counts(conn: &Connection, folder_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE folder SET
             total_count = (SELECT COUNT(*) FROM message m WHERE m.folder_id = ?1 AND m.is_deleted_local = 0),
             unread_count = (SELECT COUNT(*) FROM message m WHERE m.folder_id = ?1 AND m.is_deleted_local = 0 AND m.seen = 0)
         WHERE id = ?1",
        params![folder_id],
    )?;
    Ok(())
}

pub fn recompute_counts_for_account(conn: &Connection, account_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE folder SET
             total_count = (SELECT COUNT(*) FROM message m WHERE m.folder_id = folder.id AND m.is_deleted_local = 0),
             unread_count = (SELECT COUNT(*) FROM message m WHERE m.folder_id = folder.id AND m.is_deleted_local = 0 AND m.seen = 0)
         WHERE account_id = ?1",
        params![account_id],
    )?;
    Ok(())
}

/// Counts for the unified ("Linked") inbox.
pub fn unified_counts(conn: &Connection) -> Result<UnifiedCounts> {
    let inbox: i64 = conn.query_row(
        "SELECT COUNT(*) FROM message m
         JOIN folder f ON f.id = m.folder_id
         JOIN account a ON a.id = m.account_id
         WHERE a.include_in_unified = 1 AND a.mail_enabled = 1 AND f.role = 'inbox' AND m.is_deleted_local = 0 AND m.seen = 0",
        [],
        |row| row.get(0),
    )?;
    let flagged: i64 = conn.query_row(
        "SELECT COUNT(*) FROM message m
         JOIN account a ON a.id = m.account_id
         WHERE a.include_in_unified = 1 AND a.mail_enabled = 1 AND m.is_deleted_local = 0 AND m.flagged = 1",
        [],
        |row| row.get(0),
    )?;
    let drafts: i64 = conn.query_row(
        "SELECT COUNT(*) FROM message m
         JOIN account a ON a.id = m.account_id
         WHERE a.include_in_unified = 1 AND a.mail_enabled = 1 AND m.is_deleted_local = 0 AND m.is_draft = 1",
        [],
        |row| row.get(0),
    )?;
    let outbox: i64 = conn.query_row(
        "SELECT COUNT(*) FROM outbox o JOIN account a ON a.id = o.account_id
         WHERE a.include_in_unified = 1 AND a.mail_enabled = 1 AND o.state <> 'sent'",
        [],
        |row| row.get(0),
    )?;
    Ok(UnifiedCounts {
        inbox,
        flagged,
        drafts,
        outbox,
    })
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM fts_mail WHERE message_id IN (SELECT id FROM message WHERE folder_id = ?1)",
        params![id],
    )?;
    let changed = conn.execute("DELETE FROM folder WHERE id = ?1", params![id])?;
    if changed == 0 {
        return Err(AppError::NotFound(format!("folder {id} not found")));
    }
    Ok(())
}

/// Delete all folders of an account (used after UIDVALIDITY reset, or when a
/// server no longer returns a folder). Messages cascade.
pub fn delete_for_account(conn: &Connection, account_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM folder WHERE account_id = ?1",
        params![account_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::models::{MessageSummary, StoredMessage};

    fn setup() -> Db {
        let db = Db::open_in_memory().expect("db");
        db.migrate_core().expect("migrate");
        db
    }

    #[test]
    fn seeded_local_folders_and_tree() {
        let db = setup();
        db.write_sync(|conn| {
            let inbox = Folder {
                id: "f-inbox".into(),
                account_id: "local".into(),
                parent_id: None,
                remote_path: "INBOX".into(),
                name: "Inbox".into(),
                role: FolderRole::Inbox,
                ..Folder::new("f-inbox", "local", "Inbox", FolderRole::Inbox)
            };
            upsert_remote(conn, &inbox)?;
            let archive = Folder::new("f-arch", "local", "Archive", FolderRole::Archive);
            upsert_remote(conn, &archive)?;
            let sub = Folder {
                parent_id: Some("f-arch".into()),
                ..Folder::new("f-arch-2024", "local", "2024", FolderRole::Other)
            };
            upsert_remote(conn, &sub)?;
            let tree = tree(conn, "local")?;
            assert_eq!(tree.len(), 2);
            let archive_node = tree
                .iter()
                .find(|node| node.folder.id == "f-arch")
                .expect("archive");
            assert_eq!(archive_node.children.len(), 1);
            Ok(())
        })
        .expect("folders");
    }

    /// The unified drafts count reads `is_draft`, so the flag must stop being
    /// true once the message leaves a Drafts-role folder.
    #[test]
    fn leaving_drafts_clears_the_draft_flag_and_unified_count() {
        let db = setup();
        db.write_sync(|conn| {
            // The built-in local account is calendar/contacts/tasks only
            // (mail_enabled = 0 in the seed). A real mail account that
            // contributes to the linked inbox has mail turned on, and the
            // unified views now test both flags, so the fixture has to model
            // an actual mail account rather than a disabled one.
            let mut account = crate::repo::accounts::get_required(conn, "local")?;
            account.include_in_unified = true;
            account.mail_enabled = true;
            crate::repo::accounts::upsert(conn, &account)?;

            let drafts = upsert_remote(
                conn,
                &Folder::new("f-drafts", "local", "Drafts", FolderRole::Drafts),
            )?;
            let other_drafts = upsert_remote(
                conn,
                &Folder::new("f-drafts-2", "local", "Drafts 2", FolderRole::Drafts),
            )?;
            let trash = upsert_remote(
                conn,
                &Folder::new("f-trash", "local", "Trash", FolderRole::Trash),
            )?;

            let mut summary =
                MessageSummary::new("m1", "local", &drafts.id, "thread:m1", crate::models::now());
            summary.seen = true;
            summary.is_draft = true;
            crate::repo::messages::upsert(conn, &StoredMessage::new(summary))?;
            assert_eq!(unified_counts(conn)?.drafts, 1);

            // Migrating between two Drafts-role folders keeps the flag.
            crate::repo::messages::move_to_folder(conn, &["m1".to_string()], &other_drafts.id)?;
            assert!(
                crate::repo::messages::get_required(conn, "m1")?
                    .summary
                    .is_draft
            );
            assert_eq!(unified_counts(conn)?.drafts, 1);

            // Leaving Drafts for Trash clears it, and the unified badge follows.
            crate::repo::messages::move_to_folder(conn, &["m1".to_string()], &trash.id)?;
            assert!(
                !crate::repo::messages::get_required(conn, "m1")?
                    .summary
                    .is_draft
            );
            assert_eq!(unified_counts(conn)?.drafts, 0);
            Ok(())
        })
        .expect("draft move");
    }

    #[test]
    fn upsert_preserves_local_id_and_favorite() {
        let db = setup();
        db.write_sync(|conn| {
            let mut folder = Folder::new("", "local", "Inbox", FolderRole::Inbox);
            folder.remote_path = "INBOX".into();
            let first = upsert_remote(conn, &folder)?;
            set_favorite(conn, &first.id, true)?;
            let again = upsert_remote(conn, &folder)?;
            assert_eq!(again.id, first.id);
            assert!(again.is_favorite);
            Ok(())
        })
        .expect("upsert");
    }
}
