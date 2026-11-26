//! PIM-owned persistence: provider calendar mapping and reminder bookkeeping.
//!
//! These tables live in the `pim` migration namespace (see
//! `migrations/0001_pim.sql`) and never duplicate core data.

use chrono::Utc;
use nuncio_core::repo::{dt_string, now_string, parse_dt_opt};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::Result;

pub const PIM_NAMESPACE: &str = "pim";
pub const PIM_MIGRATIONS: &[(u32, &str)] = &[
    (1, include_str!("../migrations/0001_pim.sql")),
    (2, include_str!("../migrations/0002_contacts_tasks.sql")),
    (3, include_str!("../migrations/0003_meta.sql")),
    (
        4,
        include_str!("../migrations/0004_contact_delete_intent.sql"),
    ),
    (
        5,
        include_str!("../migrations/0005_contact_delete_tombstone.sql"),
    ),
    (
        6,
        include_str!("../migrations/0006_contact_delete_intent_owner.sql"),
    ),
    (
        7,
        include_str!("../migrations/0007_contact_delete_intent_evidence.sql"),
    ),
    (
        8,
        include_str!("../migrations/0008_contact_receipt_release_quarantine.sql"),
    ),
    (
        9,
        include_str!("../migrations/0009_contact_identity_source_and_quarantine_seq.sql"),
    ),
    (
        10,
        include_str!("../migrations/0010_contact_identity_source_on_row.sql"),
    ),
    (
        11,
        include_str!("../migrations/0011_task_delete_tombstone.sql"),
    ),
    (
        12,
        include_str!("../migrations/0012_task_identity_source.sql"),
    ),
    (
        13,
        include_str!("../migrations/0013_task_delete_intent.sql"),
    ),
    (
        14,
        include_str!("../migrations/0014_task_content_revision.sql"),
    ),
    (
        15,
        include_str!("../migrations/0015_contact_identity_state.sql"),
    ),
    (
        16,
        include_str!("../migrations/0016_task_receipt_and_move_origin.sql"),
    ),
    (
        17,
        include_str!("../migrations/0017_task_write_sequence.sql"),
    ),
];

/// How long a successfully settled delete keeps suppressing a delta batch
/// that was already fetched when the remote object was removed.
///
/// One sync window is enough for such an in-flight batch to land; the extra
/// margin covers slow providers and retried sync runs. Pending/unknown
/// intents are never expired: their cleanup has not completed.
pub const CONTACT_DELETE_TOMBSTONE_TTL_SECONDS: i64 = 24 * 60 * 60;

/// How long an unaddressable delete intent keeps its book-level isolation
/// after its owning operation stopped being outstanding.
///
/// An intent with neither a remote id nor an href cannot be acted on: the
/// create may have committed server-side with its receipt lost. It isolates
/// the book (new objects are not imported) but never holds the provider
/// cursor. Release is wall-clock based and independent of cursor movement: a
/// blocked batch that advances the cursor cannot count as evidence (that was
/// the deadlock this replaces). After this TTL the intent is recorded as
/// `released` (`released_at`) and the isolation is lifted. The trade-off is
/// deliberate: an object that did commit may be imported after the TTL, but
/// the book resumes importing auditable and bounded.
pub const CONTACT_DELETE_UNKNOWN_RELEASE_TTL_SECONDS: i64 = 24 * 60 * 60;

/// How many sync windows an intent with a known identity may keep matching an
/// incoming object ambiguously while its owning operation is terminal.
///
/// A CardDAV resource can be renamed: the same UID then arrives at a different
/// href and every batch is replayed at the same cursor. Once the owner can no
/// longer act (terminal failure), waiting cannot resolve the ambiguity. After
/// this many windows the intent is released so the renamed object is imported
/// and the cursor moves again. The release is recorded (`released_at`).
pub const CONTACT_DELETE_AMBIGUOUS_RELEASE_WINDOWS: u32 = 3;

/// Provider calendar identity for one local [`nuncio_core::models::Calendar`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarRemote {
    pub calendar_id: String,
    pub account_id: String,
    pub provider: String,
    pub remote_id: String,
    pub href: Option<String>,
}

impl CalendarRemote {
    pub fn new(
        calendar_id: impl Into<String>,
        account_id: impl Into<String>,
        provider: impl Into<String>,
        remote_id: impl Into<String>,
        href: Option<String>,
    ) -> Self {
        Self {
            calendar_id: calendar_id.into(),
            account_id: account_id.into(),
            provider: provider.into(),
            remote_id: remote_id.into(),
            href,
        }
    }
}

pub fn get_by_calendar(conn: &Connection, calendar_id: &str) -> Result<Option<CalendarRemote>> {
    let row: Option<(String, String, String, String, Option<String>)> = conn
        .query_row(
            "SELECT calendar_id, account_id, provider, remote_id, href
             FROM pim_calendar_map WHERE calendar_id = ?1",
            params![calendar_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(
        |(calendar_id, account_id, provider, remote_id, href)| CalendarRemote {
            calendar_id,
            account_id,
            provider,
            remote_id,
            href,
        },
    ))
}

pub fn get_by_remote(
    conn: &Connection,
    account_id: &str,
    provider: &str,
    remote_id: &str,
) -> Result<Option<CalendarRemote>> {
    let row: Option<(String, String, String, String, Option<String>)> = conn
        .query_row(
            "SELECT calendar_id, account_id, provider, remote_id, href
             FROM pim_calendar_map
             WHERE account_id = ?1 AND provider = ?2 AND remote_id = ?3",
            params![account_id, provider, remote_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(
        |(calendar_id, account_id, provider, remote_id, href)| CalendarRemote {
            calendar_id,
            account_id,
            provider,
            remote_id,
            href,
        },
    ))
}

pub fn set(conn: &Connection, mapping: &CalendarRemote) -> Result<()> {
    conn.execute(
        "INSERT INTO pim_calendar_map
             (calendar_id, account_id, provider, remote_id, href, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
         ON CONFLICT(calendar_id) DO UPDATE SET
             account_id = excluded.account_id,
             provider = excluded.provider,
             remote_id = excluded.remote_id,
             href = excluded.href,
             updated_at = excluded.updated_at",
        params![
            mapping.calendar_id,
            mapping.account_id,
            mapping.provider,
            mapping.remote_id,
            mapping.href,
            now_string()
        ],
    )?;
    Ok(())
}

pub fn delete(conn: &Connection, calendar_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM pim_calendar_map WHERE calendar_id = ?1",
        params![calendar_id],
    )?;
    Ok(())
}

pub fn list_for_account(conn: &Connection, account_id: &str) -> Result<Vec<CalendarRemote>> {
    let mut stmt = conn.prepare(
        "SELECT calendar_id, account_id, provider, remote_id, href
         FROM pim_calendar_map WHERE account_id = ?1 ORDER BY created_at ASC",
    )?;
    let mut rows = stmt.query(params![account_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(CalendarRemote {
            calendar_id: row.get(0)?,
            account_id: row.get(1)?,
            provider: row.get(2)?,
            remote_id: row.get(3)?,
            href: row.get(4)?,
        });
    }
    Ok(out)
}

/* ───────────────── address book / task list mapping ─────────────────── */

/// Provider identity for one local [`nuncio_core::models::AddressBook`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressBookRemote {
    pub address_book_id: String,
    pub account_id: String,
    pub provider: String,
    pub remote_id: String,
    pub href: Option<String>,
}

impl AddressBookRemote {
    pub fn new(
        address_book_id: impl Into<String>,
        account_id: impl Into<String>,
        provider: impl Into<String>,
        remote_id: impl Into<String>,
        href: Option<String>,
    ) -> Self {
        Self {
            address_book_id: address_book_id.into(),
            account_id: account_id.into(),
            provider: provider.into(),
            remote_id: remote_id.into(),
            href,
        }
    }
}

pub fn get_book_by_local(
    conn: &Connection,
    address_book_id: &str,
) -> Result<Option<AddressBookRemote>> {
    let row: Option<(String, String, String, String, Option<String>)> = conn
        .query_row(
            "SELECT address_book_id, account_id, provider, remote_id, href
             FROM pim_address_book_map WHERE address_book_id = ?1",
            params![address_book_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(
        |(address_book_id, account_id, provider, remote_id, href)| AddressBookRemote {
            address_book_id,
            account_id,
            provider,
            remote_id,
            href,
        },
    ))
}

pub fn get_book_by_remote(
    conn: &Connection,
    account_id: &str,
    provider: &str,
    remote_id: &str,
) -> Result<Option<AddressBookRemote>> {
    let row: Option<(String, String, String, String, Option<String>)> = conn
        .query_row(
            "SELECT address_book_id, account_id, provider, remote_id, href
             FROM pim_address_book_map
             WHERE account_id = ?1 AND provider = ?2 AND remote_id = ?3",
            params![account_id, provider, remote_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(
        |(address_book_id, account_id, provider, remote_id, href)| AddressBookRemote {
            address_book_id,
            account_id,
            provider,
            remote_id,
            href,
        },
    ))
}

pub fn set_book(conn: &Connection, mapping: &AddressBookRemote) -> Result<()> {
    conn.execute(
        "INSERT INTO pim_address_book_map
             (address_book_id, account_id, provider, remote_id, href, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
         ON CONFLICT(address_book_id) DO UPDATE SET
             account_id = excluded.account_id,
             provider = excluded.provider,
             remote_id = excluded.remote_id,
             href = excluded.href,
             updated_at = excluded.updated_at",
        params![
            mapping.address_book_id,
            mapping.account_id,
            mapping.provider,
            mapping.remote_id,
            mapping.href,
            now_string()
        ],
    )?;
    Ok(())
}

pub fn delete_book(conn: &Connection, address_book_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM pim_address_book_map WHERE address_book_id = ?1",
        params![address_book_id],
    )?;
    Ok(())
}

pub fn list_books_for_account(
    conn: &Connection,
    account_id: &str,
) -> Result<Vec<AddressBookRemote>> {
    let mut stmt = conn.prepare(
        "SELECT address_book_id, account_id, provider, remote_id, href
         FROM pim_address_book_map WHERE account_id = ?1 ORDER BY created_at ASC",
    )?;
    let mut rows = stmt.query(params![account_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(AddressBookRemote {
            address_book_id: row.get(0)?,
            account_id: row.get(1)?,
            provider: row.get(2)?,
            remote_id: row.get(3)?,
            href: row.get(4)?,
        });
    }
    Ok(out)
}

/// Provider identity for one local [`nuncio_core::models::TaskList`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskListRemote {
    pub task_list_id: String,
    pub account_id: String,
    pub provider: String,
    pub remote_id: String,
    pub href: Option<String>,
}

impl TaskListRemote {
    pub fn new(
        task_list_id: impl Into<String>,
        account_id: impl Into<String>,
        provider: impl Into<String>,
        remote_id: impl Into<String>,
        href: Option<String>,
    ) -> Self {
        Self {
            task_list_id: task_list_id.into(),
            account_id: account_id.into(),
            provider: provider.into(),
            remote_id: remote_id.into(),
            href,
        }
    }
}

pub fn get_task_list_by_local(
    conn: &Connection,
    task_list_id: &str,
) -> Result<Option<TaskListRemote>> {
    let row: Option<(String, String, String, String, Option<String>)> = conn
        .query_row(
            "SELECT task_list_id, account_id, provider, remote_id, href
             FROM pim_task_list_map WHERE task_list_id = ?1",
            params![task_list_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(
        |(task_list_id, account_id, provider, remote_id, href)| TaskListRemote {
            task_list_id,
            account_id,
            provider,
            remote_id,
            href,
        },
    ))
}

pub fn get_task_list_by_remote(
    conn: &Connection,
    account_id: &str,
    provider: &str,
    remote_id: &str,
) -> Result<Option<TaskListRemote>> {
    let row: Option<(String, String, String, String, Option<String>)> = conn
        .query_row(
            "SELECT task_list_id, account_id, provider, remote_id, href
             FROM pim_task_list_map
             WHERE account_id = ?1 AND provider = ?2 AND remote_id = ?3",
            params![account_id, provider, remote_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(
        |(task_list_id, account_id, provider, remote_id, href)| TaskListRemote {
            task_list_id,
            account_id,
            provider,
            remote_id,
            href,
        },
    ))
}

pub fn set_task_list(conn: &Connection, mapping: &TaskListRemote) -> Result<()> {
    conn.execute(
        "INSERT INTO pim_task_list_map
             (task_list_id, account_id, provider, remote_id, href, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
         ON CONFLICT(task_list_id) DO UPDATE SET
             account_id = excluded.account_id,
             provider = excluded.provider,
             remote_id = excluded.remote_id,
             href = excluded.href,
             updated_at = excluded.updated_at",
        params![
            mapping.task_list_id,
            mapping.account_id,
            mapping.provider,
            mapping.remote_id,
            mapping.href,
            now_string()
        ],
    )?;
    Ok(())
}

pub fn delete_task_list(conn: &Connection, task_list_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM pim_task_list_map WHERE task_list_id = ?1",
        params![task_list_id],
    )?;
    Ok(())
}

pub fn list_task_lists_for_account(
    conn: &Connection,
    account_id: &str,
) -> Result<Vec<TaskListRemote>> {
    let mut stmt = conn.prepare(
        "SELECT task_list_id, account_id, provider, remote_id, href
         FROM pim_task_list_map WHERE account_id = ?1 ORDER BY created_at ASC",
    )?;
    let mut rows = stmt.query(params![account_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(TaskListRemote {
            task_list_id: row.get(0)?,
            account_id: row.get(1)?,
            provider: row.get(2)?,
            remote_id: row.get(3)?,
            href: row.get(4)?,
        });
    }
    Ok(out)
}

/* ─────────────────────── task step remote mapping ──────────────────── */

pub fn step_local_id(conn: &Connection, task_id: &str, remote_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT step_id FROM pim_task_step_map WHERE task_id = ?1 AND remote_id = ?2 LIMIT 1",
            params![task_id, remote_id],
            |row| row.get(0),
        )
        .optional()?)
}

pub fn set_step_mapping(
    conn: &Connection,
    step_id: &str,
    task_id: &str,
    remote_id: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO pim_task_step_map (step_id, task_id, remote_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(step_id) DO UPDATE SET
             task_id = excluded.task_id,
             remote_id = excluded.remote_id,
             updated_at = excluded.updated_at",
        params![step_id, task_id, remote_id, now_string()],
    )?;
    Ok(())
}

pub fn step_remote_id(conn: &Connection, step_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT remote_id FROM pim_task_step_map WHERE step_id = ?1",
            params![step_id],
            |row| row.get(0),
        )
        .optional()?)
}

pub fn delete_step_mapping(conn: &Connection, step_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM pim_task_step_map WHERE step_id = ?1",
        params![step_id],
    )?;
    Ok(())
}

pub fn step_mappings_for_task(conn: &Connection, task_id: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT step_id, remote_id FROM pim_task_step_map WHERE task_id = ?1 ORDER BY created_at ASC",
    )?;
    let mut rows = stmt.query(params![task_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((row.get(0)?, row.get(1)?));
    }
    Ok(out)
}

/// Owner slot value for a writer that is not a pending operation.
///
/// `NULL`/no row means "written before the revision scheme" (unknown writer,
/// never claimable); an empty string is a known non-operation write such as a
/// delta upsert.
pub const EXTERNAL_TASK_WRITER: &str = "";

/// Record a write that changed the task fields a save rollback restores.
///
/// Lives in `pim_task_content_state` (not in the pending queue) so the
/// evidence survives `prune_finished`. Must be called in the same transaction
/// as the `task` write.
pub fn bump_task_content(conn: &Connection, task_id: &str, owner: Option<&str>) -> Result<()> {
    conn.execute(
        "INSERT INTO pim_task_content_state
             (task_id, content_rev, content_op_id, row_rev, row_op_id, updated_at)
         VALUES (?1, 1, ?2, 1, ?2, ?3)
         ON CONFLICT(task_id) DO UPDATE SET
             content_rev = content_rev + 1,
             content_op_id = excluded.content_op_id,
             row_rev = row_rev + 1,
             row_op_id = excluded.row_op_id,
             updated_at = excluded.updated_at",
        params![task_id, owner.unwrap_or(EXTERNAL_TASK_WRITER), now_string()],
    )?;
    Ok(())
}

/// Record a row write that does not change content (a move).
pub fn bump_task_row(conn: &Connection, task_id: &str, owner: Option<&str>) -> Result<()> {
    conn.execute(
        "INSERT INTO pim_task_content_state
             (task_id, content_rev, content_op_id, row_rev, row_op_id, updated_at)
         VALUES (?1, 0, NULL, 1, ?2, ?3)
         ON CONFLICT(task_id) DO UPDATE SET
             row_rev = row_rev + 1,
             row_op_id = excluded.row_op_id,
             updated_at = excluded.updated_at",
        params![task_id, owner.unwrap_or(EXTERNAL_TASK_WRITER), now_string()],
    )?;
    Ok(())
}

/// Record a provider save receipt as a row write.
///
/// A receipt carries identity (UID/ETag), not content, so the content owner is
/// untouched. It must not steal the row from a newer operation either: a late
/// receipt from an older save arrives after a newer local write and the newer
/// writer stays authoritative.
///
/// The identity the receipt acknowledges is stored separately
/// (`receipt_op_id`/`receipt_op_sequence`/`receipt_*`). Unlike `row_op_id`, a
/// delta upsert does not clear these columns, so the last real acknowledgement
/// remains visible even after a sync rewrote the row writer.
pub fn record_task_receipt_writer(
    conn: &Connection,
    task_id: &str,
    op_id: &str,
    op_sequence: Option<i64>,
    remote_id: Option<&str>,
    href: Option<&str>,
    uid_is_derived: bool,
) -> Result<()> {
    conn.execute(
        "INSERT INTO pim_task_content_state
             (task_id, content_rev, content_op_id, row_rev, row_op_id,
              receipt_op_id, receipt_op_sequence, receipt_remote_id, receipt_href,
              receipt_uid_is_derived, updated_at)
         VALUES (?1, 0, NULL, 1, ?2, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(task_id) DO UPDATE SET
             row_rev = row_rev + 1,
             row_op_id = CASE WHEN row_op_id IS NULL OR row_op_id = excluded.row_op_id
                              THEN excluded.row_op_id ELSE row_op_id END,
             receipt_op_id = excluded.receipt_op_id,
             receipt_op_sequence = excluded.receipt_op_sequence,
             receipt_remote_id = excluded.receipt_remote_id,
             receipt_href = excluded.receipt_href,
             receipt_uid_is_derived = excluded.receipt_uid_is_derived,
             updated_at = excluded.updated_at",
        params![
            task_id,
            op_id,
            op_sequence,
            remote_id,
            href,
            i64::from(uid_is_derived),
            now_string()
        ],
    )?;
    Ok(())
}

/// Allocate the next durable sequence for a PIM task operation.
///
/// Unlike `pending_op.seq` (`MAX(seq) + 1`), this high-water mark lives in
/// `pim_write_seq` and is never touched by `prune_finished`. The caller writes
/// the returned value to the queued operation's `seq`, so a receipt recorded
/// for that operation can be compared with a later operation's sequence even
/// after every settled queue row has been pruned.
///
/// `MAX(seq)` is consulted as well: another crate still allocates with
/// `MAX(seq) + 1` (the generic core queue), and a task operation must sort
/// after anything already queued. `value + 1` keeps the counter growing when
/// the queue is empty, which is exactly the case pruning creates.
pub fn next_task_write_seq(conn: &Connection) -> Result<i64> {
    conn.execute(
        "UPDATE pim_write_seq
            SET value = MAX(value + 1, (SELECT COALESCE(MAX(seq), 0) FROM pending_op))
          WHERE id = 1",
        [],
    )?;
    let value = conn.query_row("SELECT value FROM pim_write_seq WHERE id = 1", [], |row| {
        row.get(0)
    })?;
    Ok(value)
}

/// The last identity a provider save receipt acknowledged for a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskConfirmedReceipt {
    /// The operation whose save the provider acknowledged.
    pub op_id: String,
    /// Queue order of that operation at receipt time. `None` for rows written
    /// before migration 0016 or by a caller that is not a queued operation.
    pub op_sequence: Option<i64>,
    pub remote_id: Option<String>,
    pub href: Option<String>,
    pub uid_is_derived: bool,
}

impl TaskConfirmedReceipt {
    /// Whether `(remote_id, href, uid_is_derived)` names the identity this
    /// receipt confirmed. Identity kind is compared before any value, exactly
    /// like the task row and the delete tombstones.
    pub fn same_identity(
        &self,
        remote_id: Option<&str>,
        href: Option<&str>,
        uid_is_derived: bool,
    ) -> bool {
        if self.uid_is_derived != uid_is_derived {
            return false;
        }
        if let (Some(left), Some(right)) = (self.remote_id.as_deref(), remote_id) {
            return !left.is_empty() && left == right;
        }
        match (self.href.as_deref(), href) {
            (Some(left), Some(right)) => crate::dav::href_key(left) == crate::dav::href_key(right),
            _ => false,
        }
    }
}

/// The last identity a save receipt acknowledged, if any.
pub fn task_confirmed_receipt(
    conn: &Connection,
    task_id: &str,
) -> Result<Option<TaskConfirmedReceipt>> {
    Ok(conn
        .query_row(
            "SELECT receipt_op_id, receipt_op_sequence, receipt_remote_id,
                    receipt_href, receipt_uid_is_derived
               FROM pim_task_content_state WHERE task_id = ?1",
            params![task_id],
            |row| {
                let op_id: Option<String> = row.get(0)?;
                let op_sequence: Option<i64> = row.get(1)?;
                let remote_id: Option<String> = row.get(2)?;
                let href: Option<String> = row.get(3)?;
                let uid_is_derived: Option<i64> = row.get(4)?;
                Ok(op_id.map(|op_id| TaskConfirmedReceipt {
                    op_id,
                    op_sequence,
                    remote_id,
                    href,
                    uid_is_derived: uid_is_derived.unwrap_or(0) != 0,
                }))
            },
        )
        .optional()?
        .flatten())
}

/// The operation that wrote the task's current content, or `None` when no
/// state row exists (or its owner is SQL `NULL`).
pub fn task_content_writer(conn: &Connection, task_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT content_op_id FROM pim_task_content_state WHERE task_id = ?1",
            params![task_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// The operation that last wrote the task row for any reason, or `None` when
/// no state row exists (or its owner is SQL `NULL`).
pub fn task_row_writer(conn: &Connection, task_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT row_op_id FROM pim_task_content_state WHERE task_id = ?1",
            params![task_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/* ───────────────── contact identity ownership ─────────────────── */

/// Owner slot value for a contact identity write that is not a pending
/// operation (a delta upsert, or a local-only account).
pub const EXTERNAL_CONTACT_WRITER: &str = "";

/// Record that an operation just wrote (or confirmed) the contact's remote
/// identity locally.
///
/// The write takes ownership unconditionally: a newer local save supersedes an
/// older writer, and the row must be attributed to the operation that will
/// eventually confirm it. Must be called in the same transaction as the
/// `contact` write or the queued operation, so the evidence and the row commit
/// together.
pub fn bump_contact_identity(
    conn: &Connection,
    contact_id: &str,
    owner: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO pim_contact_identity_state
             (contact_id, identity_rev, identity_op_id, updated_at)
         VALUES (?1, 1, ?2, ?3)
         ON CONFLICT(contact_id) DO UPDATE SET
             identity_rev = identity_rev + 1,
             identity_op_id = excluded.identity_op_id,
             updated_at = excluded.updated_at",
        params![
            contact_id,
            owner.unwrap_or(EXTERNAL_CONTACT_WRITER),
            now_string()
        ],
    )?;
    Ok(())
}

/// Record a provider receipt as an identity write without stealing ownership.
///
/// A receipt may legitimately arrive while a newer save is still queued (that
/// save has not confirmed anything yet, so this receipt is the newest
/// confirmation). The newer operation stays authoritative so its own receipt
/// can still write afterwards; only when the row has no owner (or already
/// belongs to this operation) is the receipt's operation recorded.
pub fn record_contact_identity_receipt_writer(
    conn: &Connection,
    contact_id: &str,
    op_id: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO pim_contact_identity_state
             (contact_id, identity_rev, identity_op_id, updated_at)
         VALUES (?1, 1, ?2, ?3)
         ON CONFLICT(contact_id) DO UPDATE SET
             identity_rev = identity_rev + 1,
             identity_op_id = CASE
                 WHEN identity_op_id IS NULL OR identity_op_id = excluded.identity_op_id
                 THEN excluded.identity_op_id
                 ELSE identity_op_id
             END,
             updated_at = excluded.updated_at",
        params![contact_id, op_id, now_string()],
    )?;
    Ok(())
}

/// The operation that last wrote the contact's remote identity, or `None` when
/// no state row exists (or its owner is SQL `NULL`).
pub fn contact_identity_writer(conn: &Connection, contact_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT identity_op_id FROM pim_contact_identity_state WHERE contact_id = ?1",
            params![contact_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

pub fn clear_step_mappings(conn: &Connection, task_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM pim_task_step_map WHERE task_id = ?1",
        params![task_id],
    )?;
    Ok(())
}

/* ───────────────────────── task resource hrefs ─────────────────────── */

pub fn set_task_href(conn: &Connection, task_id: &str, href: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO pim_task_resource (task_id, href, updated_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(task_id) DO UPDATE SET href = excluded.href, updated_at = excluded.updated_at",
        params![task_id, href, now_string()],
    )?;
    Ok(())
}

pub fn task_href(conn: &Connection, task_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT href FROM pim_task_resource WHERE task_id = ?1",
            params![task_id],
            |row| row.get(0),
        )
        .optional()?)
}

/// Whether a stored task's `remote_id` is a href-derived placeholder rather
/// than a provider UID. Rows written before the identity source was persisted
/// default to a real UID (the migration default).
pub fn task_uid_is_derived(conn: &Connection, task_id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT uid_is_derived FROM task WHERE id = ?1",
            params![task_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

/// Record where the identity of a stored task came from.
pub fn set_task_uid_is_derived(
    conn: &Connection,
    task_id: &str,
    uid_is_derived: bool,
) -> Result<()> {
    conn.execute(
        "UPDATE task SET uid_is_derived = ?2 WHERE id = ?1",
        params![task_id, i64::from(uid_is_derived)],
    )?;
    Ok(())
}

/// Stored rows at an href, exact spelling first and then the RFC 3986 §6.2.2
/// identity key. Each entry carries the identity kind so the caller can prefer
/// a row of the incoming kind when historical duplicates exist.
pub fn task_candidates_by_href(
    conn: &Connection,
    list_id: &str,
    href: &str,
) -> Result<Vec<(String, bool)>> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT t.id, t.uid_is_derived
         FROM task t JOIN pim_task_resource r ON r.task_id = t.id
         WHERE t.list_id = ?1 AND r.href = ?2",
    )?;
    let mut rows = stmt.query(params![list_id, href])?;
    while let Some(row) = rows.next()? {
        out.push((row.get(0)?, row.get(1)?));
    }
    drop(rows);
    drop(stmt);
    if !out.is_empty() {
        return Ok(out);
    }
    let key = crate::dav::href_key(href);
    let mut stmt = conn.prepare(
        "SELECT t.id, t.uid_is_derived, r.href
         FROM task t JOIN pim_task_resource r ON r.task_id = t.id
         WHERE t.list_id = ?1 AND r.href IS NOT NULL",
    )?;
    let mut rows = stmt.query(params![list_id])?;
    while let Some(row) = rows.next()? {
        let stored: String = row.get(2)?;
        if crate::dav::href_key(&stored) == key {
            out.push((row.get(0)?, row.get(1)?));
        }
    }
    Ok(out)
}

/// Every stored task row at an href, regardless of identity kind. One URL is
/// one resource, so a provider deletion must remove all historical duplicates.
pub fn task_ids_by_href(conn: &Connection, list_id: &str, href: &str) -> Result<Vec<String>> {
    Ok(task_candidates_by_href(conn, list_id, href)?
        .into_iter()
        .map(|(id, _)| id)
        .collect())
}

/// Task row addressed by a provider id of one identity kind.
///
/// A derived placeholder and a real UID that share the same spelling are
/// different resources: callers must name the kind they mean.
pub fn task_by_remote_id_kind(
    conn: &Connection,
    list_id: &str,
    remote_id: &str,
    uid_is_derived: bool,
) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT id FROM task
             WHERE list_id = ?1 AND remote_id = ?2 AND uid_is_derived = ?3
             LIMIT 1",
            params![list_id, remote_id, i64::from(uid_is_derived)],
            |row| row.get(0),
        )
        .optional()?)
}

/// Task row addressed by a **real** provider UID. Used for delete events and
/// real-UID upserts, which must never consume a derived placeholder.
pub fn task_by_real_remote_id(
    conn: &Connection,
    list_id: &str,
    remote_id: &str,
) -> Result<Option<String>> {
    task_by_remote_id_kind(conn, list_id, remote_id, false)
}

pub fn task_by_href(conn: &Connection, list_id: &str, href: &str) -> Result<Option<String>> {
    // Exact spelling first (indexed); only a miss falls back to the RFC 3986
    // §6.2.2 identity key, so a provider that spells `A.vtodo` after a round
    // that stored `%41.vtodo` still finds the same row.
    let exact: Option<String> = conn
        .query_row(
            "SELECT t.id FROM task t JOIN pim_task_resource r ON r.task_id = t.id
             WHERE t.list_id = ?1 AND r.href = ?2 LIMIT 1",
            params![list_id, href],
            |row| row.get(0),
        )
        .optional()?;
    if exact.is_some() {
        return Ok(exact);
    }
    let key = crate::dav::href_key(href);
    let mut stmt = conn.prepare(
        "SELECT t.id, r.href FROM task t JOIN pim_task_resource r ON r.task_id = t.id
         WHERE t.list_id = ?1 AND r.href IS NOT NULL",
    )?;
    let mut rows = stmt.query(params![list_id])?;
    while let Some(row) = rows.next()? {
        let stored: String = row.get(1)?;
        if crate::dav::href_key(&stored) == key {
            return Ok(Some(row.get(0)?));
        }
    }
    Ok(None)
}

/// `(href, etag)` pairs for a CalDAV task collection.
pub fn task_etags(conn: &Connection, list_id: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT r.href, t.etag FROM task t JOIN pim_task_resource r ON r.task_id = t.id
         WHERE t.list_id = ?1 AND t.etag IS NOT NULL",
    )?;
    let mut rows = stmt.query(params![list_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((row.get(0)?, row.get(1)?));
    }
    Ok(out)
}

/* ───────────────────── task delete tombstones ─────────────────── */

/// How long a deleted task keeps suppressing a delta batch.
///
/// One sync window is enough for a batch fetched before the remote DELETE to
/// land; the extra margin covers slow providers and retried sync runs. The
/// window is renewed when the DELETE succeeds.
pub const TASK_DELETE_TOMBSTONE_TTL_SECONDS: i64 = 24 * 60 * 60;

/// How long an intent with no remote identity keeps a list's imports deferred.
///
/// Such an intent belongs to a create whose receipt was lost: the provider may
/// have committed an object that cannot be matched by identity. Importing new
/// objects would risk resurrecting the deleted task, so the list's cursor is
/// held while the owner operation may still fix the ambiguity. Once the owner
/// is terminal and this window has elapsed, the intent is released; replaying
/// the batch can never resolve an identity the provider never told us.
pub const TASK_DELETE_UNKNOWN_RELEASE_TTL_SECONDS: i64 = 24 * 60 * 60;

/// Tombstone origin: a user delete (or a create whose receipt was lost).
pub const TASK_TOMBSTONE_ORIGIN_DELETE: &str = "delete";
/// Tombstone origin: a successful move removed the task from this list.
pub const TASK_TOMBSTONE_ORIGIN_MOVE: &str = "move";

/// A remote identity whose re-import must be suppressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDeleteTombstone {
    pub remote_id: Option<String>,
    pub href: Option<String>,
    /// `remote_id` is a href-derived placeholder, not a real provider UID.
    pub uid_is_derived: bool,
}

impl TaskDeleteTombstone {
    /// Whether a provider task describes the same resource as this tombstone.
    ///
    /// Identity **kind** is compared before any value: a real UID tombstone can
    /// only match a real UID, and an href-derived placeholder can only match a
    /// UID-less object at an equivalent href. A real tombstone whose UID differs
    /// from the incoming one must not suppress it, even when the resource href
    /// was reused: a new VTODO can be created at the same URL with a fresh UID.
    pub fn matches(
        &self,
        remote_id: Option<&str>,
        href: Option<&str>,
        uid_is_derived: bool,
    ) -> bool {
        let tombstone_is_derived = self.uid_is_derived || self.remote_id.is_none();
        if tombstone_is_derived {
            if !uid_is_derived {
                return false;
            }
            let tombstone_key = self
                .href
                .as_deref()
                .filter(|value| !value.is_empty())
                .map(crate::dav::href_key)
                .or_else(|| {
                    self.remote_id
                        .as_deref()
                        .filter(|value| !value.is_empty())
                        .map(crate::dav::href_key)
                });
            let incoming_key = href
                .filter(|value| !value.is_empty())
                .map(crate::dav::href_key)
                .or_else(|| {
                    remote_id
                        .filter(|value| !value.is_empty())
                        .map(crate::dav::href_key)
                });
            return matches!(
                (tombstone_key, incoming_key),
                (Some(left), Some(right)) if left == right
            );
        }
        if uid_is_derived {
            return false;
        }
        matches!(
            (self.remote_id.as_deref(), remote_id),
            (Some(left), Some(right)) if left == right
        )
    }
}

/// Record that a task was deleted locally and must not be re-imported.
///
/// The row starts in `pending`: the queued DELETE has not reached the server
/// yet, so it must protect the object for as long as it takes. The wall-clock
/// TTL starts only when the DELETE is settled ([`settle_task_delete_tombstones`]).
pub fn insert_task_delete_tombstone(
    conn: &Connection,
    account_id: &str,
    task_list_id: &str,
    remote_id: Option<&str>,
    href: Option<&str>,
    uid_is_derived: bool,
) -> Result<()> {
    let now = now_string();
    conn.execute(
        "INSERT INTO pim_task_delete_tombstone
             (id, account_id, task_list_id, remote_id, href, uid_is_derived,
              state, origin, suppress_until, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8, ?8, ?8)",
        params![
            nuncio_core::models::new_id(),
            account_id,
            task_list_id,
            remote_id,
            href,
            i64::from(uid_is_derived),
            TASK_TOMBSTONE_ORIGIN_DELETE,
            now
        ],
    )?;
    Ok(())
}

/// Record that a successful move removed the task from its **source** list.
///
/// Unlike [`insert_task_delete_tombstone`] (a queued DELETE that may still be
/// outstanding), the row is written `settled` with a wall-clock suppression
/// window. Used when the remote object left its list as a side effect of
/// another operation — a move relinks the task to the target list or the
/// default implementation deletes the source copy — so a delta for the source
/// list that was captured before the move cannot re-import it there.
///
/// The `move` origin lets a later successful move back into this list drop
/// this marker: the task is present again, so it is no longer a "left here"
/// object. Pending user-delete tombstones are never touched by that cleanup.
pub fn insert_settled_task_move_tombstone(
    conn: &Connection,
    account_id: &str,
    task_list_id: &str,
    remote_id: Option<&str>,
    href: Option<&str>,
    uid_is_derived: bool,
    suppress_until: chrono::DateTime<Utc>,
) -> Result<()> {
    let now = now_string();
    conn.execute(
        "INSERT INTO pim_task_delete_tombstone
             (id, account_id, task_list_id, remote_id, href, uid_is_derived,
              state, origin, suppress_until, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'settled', ?7, ?8, ?9, ?9)",
        params![
            nuncio_core::models::new_id(),
            account_id,
            task_list_id,
            remote_id,
            href,
            i64::from(uid_is_derived),
            TASK_TOMBSTONE_ORIGIN_MOVE,
            dt_string(suppress_until),
            now
        ],
    )?;
    Ok(())
}

/// Tombstones for one list that still suppress an import.
///
/// A `pending` tombstone is always active: its remote DELETE has not settled,
/// so no elapsed wall time can prove the object is gone. A `settled` tombstone
/// is active only until its suppression window elapses.
pub fn active_task_delete_tombstones(
    conn: &Connection,
    task_list_id: &str,
    now: chrono::DateTime<Utc>,
) -> Result<Vec<TaskDeleteTombstone>> {
    let mut stmt = conn.prepare(
        "SELECT remote_id, href, uid_is_derived FROM pim_task_delete_tombstone
         WHERE task_list_id = ?1
           AND (state = 'pending' OR suppress_until > ?2)",
    )?;
    let mut rows = stmt.query(params![task_list_id, dt_string(now)])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(TaskDeleteTombstone {
            remote_id: row.get(0)?,
            href: row.get(1)?,
            uid_is_derived: row.get(2)?,
        });
    }
    Ok(out)
}

/// Ids of tombstones for one identity kind at `href`.
///
/// `href` is compared through [`crate::dav::href_key`], so equivalent
/// spellings (`A.ics` / `%41.ics`) settle the same rows. SQLite has no
/// built-in percent-decoder and a generated column would need the same Rust
/// function registered on every connection, so the comparison happens here,
/// over the rows of one list. That list is bounded by its few days of local
/// deletes, so the scan is small.
fn task_delete_tombstone_ids_at(
    conn: &Connection,
    task_list_id: &str,
    remote_id: Option<&str>,
    href: Option<&str>,
    uid_is_derived: bool,
) -> Result<Vec<String>> {
    task_tombstone_ids_at(
        conn,
        task_list_id,
        remote_id,
        href,
        uid_is_derived,
        None,
    )
}

/// Ids of settled move tombstones for one identity at `href`.
fn move_task_tombstone_ids_at(
    conn: &Connection,
    task_list_id: &str,
    remote_id: Option<&str>,
    href: Option<&str>,
    uid_is_derived: bool,
) -> Result<Vec<String>> {
    task_tombstone_ids_at(
        conn,
        task_list_id,
        remote_id,
        href,
        uid_is_derived,
        Some(TASK_TOMBSTONE_ORIGIN_MOVE),
    )
}

fn task_tombstone_ids_at(
    conn: &Connection,
    task_list_id: &str,
    remote_id: Option<&str>,
    href: Option<&str>,
    uid_is_derived: bool,
    origin: Option<&str>,
) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT id, href FROM pim_task_delete_tombstone
         WHERE task_list_id = ?1
           AND uid_is_derived = ?2
           AND remote_id IS ?3
           AND (?4 IS NULL OR (origin = ?4 AND state = 'settled'))",
    )?;
    let mut rows = stmt.query(params![
        task_list_id,
        i64::from(uid_is_derived),
        remote_id,
        origin
    ])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let stored: Option<String> = row.get(1)?;
        let matches = match (stored.as_deref(), href) {
            (None, None) => true,
            (Some(stored), Some(href)) => {
                crate::dav::href_key(stored) == crate::dav::href_key(href)
            }
            _ => false,
        };
        if matches {
            out.push(row.get(0)?);
        }
    }
    Ok(out)
}

/// Ids of href-derived tombstones at `href`.
fn derived_task_delete_tombstone_ids_at(
    conn: &Connection,
    task_list_id: &str,
    href: &str,
) -> Result<Vec<String>> {
    derived_tombstone_ids_at(conn, task_list_id, href, None)
}

fn derived_move_tombstone_ids_at(
    conn: &Connection,
    task_list_id: &str,
    href: &str,
) -> Result<Vec<String>> {
    derived_tombstone_ids_at(
        conn,
        task_list_id,
        href,
        Some(TASK_TOMBSTONE_ORIGIN_MOVE),
    )
}

fn derived_tombstone_ids_at(
    conn: &Connection,
    task_list_id: &str,
    href: &str,
    origin: Option<&str>,
) -> Result<Vec<String>> {
    let key = crate::dav::href_key(href);
    let mut stmt = conn.prepare(
        "SELECT id, href FROM pim_task_delete_tombstone
         WHERE task_list_id = ?1 AND uid_is_derived = 1
           AND (?2 IS NULL OR (origin = ?2 AND state = 'settled'))",
    )?;
    let mut rows = stmt.query(params![task_list_id, origin])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let stored: Option<String> = row.get(1)?;
        if stored
            .as_deref()
            .is_some_and(|stored| crate::dav::href_key(stored) == key)
        {
            out.push(row.get(0)?);
        }
    }
    Ok(out)
}

/// Turn a pending tombstone into a settled one after the remote DELETE
/// succeeded, starting its suppression window **now**.
///
/// The exact identity (kind + remote id + href) is used so a tombstone for a
/// different resource is never renewed. The href is compared through
/// [`crate::dav::href_key`], so a DELETE captured under one spelling also
/// settles the row recorded under an equivalent one — otherwise a rewrite
/// that changed only the href spelling would leave the old row `pending`
/// forever. When a real UID is settled, any href-derived tombstone for the
/// *same href* is settled too: that is the identity the resource carried
/// before an in-flight rewrite upgraded it, and it currently protects the same
/// object.
pub fn settle_task_delete_tombstones(
    conn: &Connection,
    task_list_id: &str,
    remote_id: Option<&str>,
    href: Option<&str>,
    uid_is_derived: bool,
    suppress_until: chrono::DateTime<Utc>,
) -> Result<()> {
    let mut ids =
        task_delete_tombstone_ids_at(conn, task_list_id, remote_id, href, uid_is_derived)?;
    if !uid_is_derived {
        if let Some(href) = href.filter(|value| !value.is_empty()) {
            ids.extend(derived_task_delete_tombstone_ids_at(
                conn,
                task_list_id,
                href,
            )?);
        }
    }
    ids.sort_unstable();
    ids.dedup();
    let mut stmt = conn.prepare(
        "UPDATE pim_task_delete_tombstone
         SET state = 'settled', suppress_until = ?2, updated_at = ?3
         WHERE id = ?1",
    )?;
    for id in ids {
        stmt.execute(params![id, dt_string(suppress_until), now_string()])?;
    }
    Ok(())
}

/// Drop the tombstone when a failed DELETE was rolled back and the local task
/// is restored: the object is meant to stay.
///
/// The href is compared through [`crate::dav::href_key`] for the same reason
/// settlement is: a rolled-back rewrite must release every row that protects
/// the object, whichever spelling it was recorded under.
pub fn remove_task_delete_tombstones(
    conn: &Connection,
    task_list_id: &str,
    remote_id: Option<&str>,
    href: Option<&str>,
    uid_is_derived: bool,
) -> Result<usize> {
    let mut ids =
        task_delete_tombstone_ids_at(conn, task_list_id, remote_id, href, uid_is_derived)?;
    // A rolled-back rewrite restores the object, so the pre-upgrade href
    // tombstone that now shares this href must go too.
    if !uid_is_derived {
        if let Some(href) = href.filter(|value| !value.is_empty()) {
            ids.extend(derived_task_delete_tombstone_ids_at(
                conn,
                task_list_id,
                href,
            )?);
        }
    }
    ids.sort_unstable();
    ids.dedup();
    let mut stmt = conn.prepare("DELETE FROM pim_task_delete_tombstone WHERE id = ?1")?;
    let mut changed = 0;
    for id in ids {
        changed += stmt.execute(params![id])?;
    }
    Ok(changed)
}

/// Drop settled **move** tombstones for an object that just came back to this
/// list.
///
/// A move A -> B leaves a tombstone in A. Moving the same resource back B -> A
/// (Google relinks it and keeps the provider id) makes that marker stale: the
/// task is in A again, so it must not be hidden from deltas or treated as
/// missing by a full listing. Only `move` tombstones are removed; a pending
/// DELETE still owed to the provider keeps suppressing its object.
pub fn remove_task_move_tombstones(
    conn: &Connection,
    task_list_id: &str,
    remote_id: Option<&str>,
    href: Option<&str>,
    uid_is_derived: bool,
) -> Result<usize> {
    let mut ids = move_task_tombstone_ids_at(conn, task_list_id, remote_id, href, uid_is_derived)?;
    if !uid_is_derived {
        if let Some(href) = href.filter(|value| !value.is_empty()) {
            ids.extend(derived_move_tombstone_ids_at(conn, task_list_id, href)?);
        }
    }
    ids.sort_unstable();
    ids.dedup();
    let mut stmt = conn.prepare("DELETE FROM pim_task_delete_tombstone WHERE id = ?1")?;
    let mut changed = 0;
    for id in ids {
        changed += stmt.execute(params![id])?;
    }
    Ok(changed)
}
///
/// Pending tombstones are never pruned: their deletion has not completed and
/// they must keep protecting the object. Pruning them here would also make the
/// later settle UPDATE match no row.
pub fn prune_expired_task_delete_tombstones(
    conn: &Connection,
    now: chrono::DateTime<Utc>,
) -> Result<usize> {
    let changed = conn.execute(
        "DELETE FROM pim_task_delete_tombstone
         WHERE state = 'settled' AND suppress_until <= ?1",
        params![dt_string(now)],
    )?;
    Ok(changed)
}

/// A local delete whose remote cleanup still depends on an in-flight save.
///
/// `remote_id`/`href`/`etag` may start empty (a create that has not produced
/// an object yet) and are backfilled by the save's receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDeleteIntent {
    pub task_id: String,
    pub account_id: String,
    pub task_list_id: String,
    pub remote_id: Option<String>,
    pub href: Option<String>,
    pub etag: Option<String>,
    pub uid_is_derived: bool,
    /// The operation that may have produced the remote object.
    pub owner_op_id: Option<String>,
    pub owner_attempt: Option<u32>,
}

/// Remember that a save racing this delete must clean up whatever it creates.
///
/// Recording twice keeps the addressable identity already attached by an
/// earlier receipt; only missing fields are filled in.
#[allow(clippy::too_many_arguments)]
pub fn record_task_delete_intent(
    conn: &Connection,
    task_id: &str,
    account_id: &str,
    task_list_id: &str,
    remote_id: Option<&str>,
    href: Option<&str>,
    etag: Option<&str>,
    uid_is_derived: bool,
    owner_op_id: Option<&str>,
    owner_attempt: Option<u32>,
) -> Result<()> {
    let now = now_string();
    conn.execute(
        "INSERT INTO pim_task_delete_intent
             (task_id, account_id, task_list_id, remote_id, href, etag,
              uid_is_derived, owner_op_id, owner_attempt, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
         ON CONFLICT(task_id) DO UPDATE SET
             account_id = excluded.account_id,
             task_list_id = excluded.task_list_id,
             remote_id = COALESCE(excluded.remote_id, pim_task_delete_intent.remote_id),
             href = COALESCE(excluded.href, pim_task_delete_intent.href),
             etag = COALESCE(excluded.etag, pim_task_delete_intent.etag),
             uid_is_derived = CASE
                 WHEN excluded.remote_id IS NOT NULL THEN excluded.uid_is_derived
                 ELSE pim_task_delete_intent.uid_is_derived
             END,
             owner_op_id = COALESCE(pim_task_delete_intent.owner_op_id, excluded.owner_op_id),
             owner_attempt = COALESCE(pim_task_delete_intent.owner_attempt, excluded.owner_attempt),
             updated_at = excluded.updated_at",
        params![
            task_id,
            account_id,
            task_list_id,
            remote_id,
            href,
            etag,
            i64::from(uid_is_derived),
            owner_op_id,
            owner_attempt.map(i64::from),
            now
        ],
    )?;
    Ok(())
}

pub fn task_delete_intent(conn: &Connection, task_id: &str) -> Result<Option<TaskDeleteIntent>> {
    Ok(conn
        .query_row(
            "SELECT task_id, account_id, task_list_id, remote_id, href, etag,
                    uid_is_derived, owner_op_id, owner_attempt
               FROM pim_task_delete_intent WHERE task_id = ?1",
            params![task_id],
            |row| {
                Ok(TaskDeleteIntent {
                    task_id: row.get(0)?,
                    account_id: row.get(1)?,
                    task_list_id: row.get(2)?,
                    remote_id: row.get(3)?,
                    href: row.get(4)?,
                    etag: row.get(5)?,
                    uid_is_derived: row.get::<_, i64>(6)? != 0,
                    owner_op_id: row.get(7)?,
                    owner_attempt: row.get::<_, Option<i64>>(8)?.map(|value| value as u32),
                })
            },
        )
        .optional()?)
}

/// Backfill the identity a successful save receipt returned for a deleted
/// task. Returns the updated intent, or `None` when the delete was already
/// reconciled (deleted/rolled back) and there is no one left to address.
pub fn attach_task_delete_intent(
    conn: &Connection,
    task_id: &str,
    remote_id: Option<&str>,
    href: Option<&str>,
    etag: Option<&str>,
    uid_is_derived: bool,
) -> Result<Option<TaskDeleteIntent>> {
    let changed = conn.execute(
        "UPDATE pim_task_delete_intent
            SET remote_id = COALESCE(?2, remote_id),
                href = COALESCE(?3, href),
                etag = COALESCE(?4, etag),
                uid_is_derived = CASE WHEN ?2 IS NOT NULL THEN ?5 ELSE uid_is_derived END,
                updated_at = ?6
          WHERE task_id = ?1",
        params![
            task_id,
            remote_id,
            href,
            etag,
            i64::from(uid_is_derived),
            now_string()
        ],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    task_delete_intent(conn, task_id)
}

pub fn remove_task_delete_intent(conn: &Connection, task_id: &str) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM pim_task_delete_intent WHERE task_id = ?1",
        params![task_id],
    )?)
}

/// Add the identity a save receipt just confirmed next to the tombstone the
/// delete captured.
///
/// The queued DELETE was captured with the identity the task had when the user
/// deleted it. A save that was already on the wire can replace a derived
/// placeholder with a real UID (and a fresh ETag), so the acknowledged identity
/// needs a tombstone too or a delta echo re-imports the task.
///
/// The old identity must **not** be overwritten: a delta captured before the
/// rewrite can still deliver the resource under its derived placeholder (and a
/// re-delivery may keep arriving while both identities refer to the same
/// href). Both tombstones therefore live side by side and are settled/expired
/// independently by [`settle_task_delete_tombstones`] and the prune window.
///
/// Returns the number of tombstones that matched the *old* identity. `0` means
/// the delete had no identity yet and the caller must create a fresh pending
/// tombstone for the confirmed one.
#[allow(clippy::too_many_arguments)]
pub fn rekey_task_delete_tombstones(
    conn: &Connection,
    task_list_id: &str,
    old_remote_id: Option<&str>,
    old_href: Option<&str>,
    old_uid_is_derived: bool,
    remote_id: Option<&str>,
    href: Option<&str>,
    uid_is_derived: bool,
) -> Result<usize> {
    let old_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pim_task_delete_tombstone
          WHERE task_list_id = ?1
            AND uid_is_derived = ?4
            AND remote_id IS ?2
            AND href IS ?3",
        params![
            task_list_id,
            old_remote_id,
            old_href,
            i64::from(old_uid_is_derived)
        ],
        |row| row.get(0),
    )?;
    if old_count == 0 {
        return Ok(0);
    }
    let changed =
        old_uid_is_derived != uid_is_derived || old_remote_id != remote_id || old_href != href;
    if changed && (remote_id.is_some() || href.is_some()) {
        let already_covered: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pim_task_delete_tombstone
              WHERE task_list_id = ?1
                AND uid_is_derived = ?4
                AND remote_id IS ?2
                AND href IS ?3",
            params![task_list_id, remote_id, href, i64::from(uid_is_derived)],
            |row| row.get(0),
        )?;
        if already_covered == 0 {
            // Copy the old row's lifecycle (pending vs. settled, its `origin`
            // and its `suppress_until`) so the new identity is released the
            // same way as the identity it is derived from.
            conn.execute(
                "INSERT INTO pim_task_delete_tombstone
                     (id, account_id, task_list_id, remote_id, href, uid_is_derived,
                      state, origin, suppress_until, created_at, updated_at)
                 SELECT ?5, account_id, task_list_id, ?6, ?7, ?8,
                        state, origin, suppress_until, ?9, ?9
                   FROM pim_task_delete_tombstone
                  WHERE task_list_id = ?1
                    AND uid_is_derived = ?2
                    AND remote_id IS ?3
                    AND href IS ?4
                  LIMIT 1",
                params![
                    task_list_id,
                    i64::from(old_uid_is_derived),
                    old_remote_id,
                    old_href,
                    nuncio_core::models::new_id(),
                    remote_id,
                    href,
                    i64::from(uid_is_derived),
                    now_string()
                ],
            )?;
        }
    }
    Ok(old_count as usize)
}

/// Whether a list has a delete intent that never learned a remote identity.
///
/// Such an intent belongs to a create whose receipt was lost. The server may
/// have committed an object that cannot be matched by UID or href, so new
/// objects in this list cannot be imported until the ambiguity is resolved
/// (or the bounded release window elapses).
pub fn has_unattributed_task_delete_intent(conn: &Connection, task_list_id: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pim_task_delete_intent
          WHERE task_list_id = ?1
            AND remote_id IS NULL
            AND href IS NULL",
        params![task_list_id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Release intents that never learned an identity once their owner operation
/// can no longer resolve them and the ambiguity window has elapsed.
///
/// Deleting the row lifts the list-level import deferral. This is a *bounded*
/// release, not a conclusion that nothing was created: a committed object may
/// become importable again, exactly like the contacts release path. An owner
/// that is still queued or in flight keeps the intent.
pub fn release_expired_unknown_task_delete_intents(
    conn: &Connection,
    account_id: &str,
    ttl_seconds: i64,
    now: chrono::DateTime<Utc>,
) -> Result<usize> {
    let cutoff = now - chrono::Duration::seconds(ttl_seconds.max(0));
    Ok(conn.execute(
        "DELETE FROM pim_task_delete_intent
          WHERE account_id = ?1
            AND remote_id IS NULL
            AND href IS NULL
            AND updated_at <= ?2
            AND NOT EXISTS (
                SELECT 1 FROM pending_op p
                WHERE p.id = pim_task_delete_intent.owner_op_id
                  AND p.state IN ('pending', 'inProgress')
            )",
        params![account_id, dt_string(cutoff)],
    )?)
}

/// Map a provider child-task id back to its local step and parent task.
pub fn step_lookup_by_remote(
    conn: &Connection,
    remote_id: &str,
) -> Result<Option<(String, String, String)>> {
    Ok(conn
        .query_row(
            "SELECT step_id, task_id, remote_id FROM pim_task_step_map WHERE remote_id = ?1 LIMIT 1",
            params![remote_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?)
}

/* ─────────────────────── task reminder bookkeeping ─────────────────── */

pub fn task_reminder_fired(conn: &Connection, task_id: &str, key: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pim_task_reminder_fired WHERE task_id = ?1 AND reminder_key = ?2",
        params![task_id, key],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

pub fn mark_task_reminder_fired(conn: &Connection, task_id: &str, key: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO pim_task_reminder_fired (task_id, reminder_key, fired_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(task_id, reminder_key) DO UPDATE SET fired_at = excluded.fired_at",
        params![task_id, key, now_string()],
    )?;
    Ok(())
}

/// Allow a task reminder to fire again (used after snoozing).
pub fn clear_task_reminder_fired(conn: &Connection, task_id: &str, key: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM pim_task_reminder_fired WHERE task_id = ?1 AND reminder_key = ?2",
        params![task_id, key],
    )?;
    Ok(())
}

/// True when a reminder for this occurrence was already delivered.
pub fn reminder_fired(conn: &Connection, event_id: &str, instance_start: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pim_reminder_fired
         WHERE event_id = ?1 AND instance_start = ?2",
        params![event_id, instance_start],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Record a delivered reminder, keeping one row per occurrence.
pub fn mark_reminder_fired(conn: &Connection, event_id: &str, instance_start: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO pim_reminder_fired (event_id, instance_start, fired_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(event_id, instance_start) DO UPDATE SET fired_at = excluded.fired_at",
        params![event_id, instance_start, now_string()],
    )?;
    Ok(())
}

/// Allow an occurrence to fire again (used after snoozing).
pub fn clear_reminder_fired(conn: &Connection, event_id: &str, instance_start: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM pim_reminder_fired WHERE event_id = ?1 AND instance_start = ?2",
        params![event_id, instance_start],
    )?;
    Ok(())
}

/// Remove fired markers for events that no longer exist.
pub fn prune_reminder_fired(conn: &Connection) -> Result<usize> {
    let cutoff = dt_string(Utc::now() - chrono::Duration::days(35));
    let removed = conn.execute(
        "DELETE FROM pim_reminder_fired
         WHERE event_id NOT IN (SELECT id FROM event) OR fired_at < ?1",
        params![cutoff],
    )?;
    let removed_tasks = conn.execute(
        "DELETE FROM pim_task_reminder_fired
         WHERE task_id NOT IN (SELECT id FROM task) OR fired_at < ?1",
        params![cutoff],
    )?;
    Ok(removed + removed_tasks)
}

/// Read the last webcal refresh timestamp from a calendar sync marker.
pub fn webcal_last_refresh(
    conn: &Connection,
    calendar_id: &str,
) -> Result<Option<chrono::DateTime<Utc>>> {
    let value: Option<String> = conn
        .query_row(
            "SELECT sync_token FROM calendar WHERE id = ?1",
            params![calendar_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    match value {
        Some(value) => Ok(parse_dt_opt(Some(value))?),
        None => Ok(None),
    }
}

/// Store the webcal refresh timestamp in `calendar.sync_token`.
pub fn set_webcal_last_refresh(
    conn: &Connection,
    calendar_id: &str,
    at: chrono::DateTime<Utc>,
) -> Result<()> {
    conn.execute(
        "UPDATE calendar SET sync_token = ?2, updated_at = ?2 WHERE id = ?1",
        params![calendar_id, dt_string(at)],
    )?;
    Ok(())
}

/// Lifecycle of a persisted contact delete intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactDeleteIntentState {
    /// Cleanup is still owed; the remote id may already be known.
    Pending,
    /// A create may have committed server-side, but no receipt arrived, so the
    /// object cannot be addressed. Keep the intent for reconciliation.
    Unknown,
    /// The remote DELETE succeeded. The row is a tombstone until
    /// `suppress_until` so a late delta cannot re-import the contact.
    Settled,
    /// The intent's protection was lifted: either it never became addressable
    /// and its owner is no longer outstanding (wall-clock TTL, or explicit
    /// reconciliation), or a still-identified intent kept matching incoming
    /// objects ambiguously after its owner became terminal. Kept as an audit
    /// record; it no longer suppresses or isolates imports (per-operation
    /// receipts are settled independently of this state).
    Released,
}

impl ContactDeleteIntentState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Unknown => "unknown",
            Self::Settled => "settled",
            Self::Released => "released",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "unknown" => Self::Unknown,
            "settled" => Self::Settled,
            "released" => Self::Released,
            _ => Self::Pending,
        }
    }
}

/// A contact local delete that may have raced an in-flight create upload.
///
/// The local row is already gone, so the queued `pim.contact.save` receipt has
/// no row to write. The intent lets the executor remove the remote object the
/// create produced (and survives a retry so the cleanup converges).
///
/// A settled intent is deliberately kept for one sync window: a delta request
/// that started before the DELETE can still name the object afterwards, and
/// importing it would resurrect a contact the user deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactDeleteIntent {
    pub contact_id: String,
    pub account_id: String,
    pub address_book_id: String,
    pub remote_id: Option<String>,
    pub href: Option<String>,
    pub etag: Option<String>,
    /// `remote_id` was derived from the href (the vCard had no UID) instead of
    /// being a real provider UID. Real and derived identities never match each
    /// other: see [`crate::contacts::service`]'s tombstone matching.
    pub uid_is_derived: bool,
    pub state: ContactDeleteIntentState,
    pub suppress_until: Option<chrono::DateTime<Utc>>,
    /// The `pim.contact.save` operation that may have produced the remote
    /// object. Only that operation (or a legacy row without an owner) may
    /// settle or clean the intent.
    pub owner_op_id: Option<String>,
    /// The owner's attempt counter when the intent was recorded.
    pub owner_attempt: Option<u32>,
    /// Legacy diagnostic counter, kept for schema compatibility. Release of
    /// unaddressable intents is wall-clock based
    /// ([`release_expired_contact_delete_intents`]); it no longer depends on
    /// sync windows or cursor movement.
    pub unknown_windows: u32,
    /// Legacy cursor evidence, kept for schema compatibility. It is no longer
    /// required to release an unaddressable intent.
    pub evidence_cursor: Option<String>,
    /// Sync windows elapsed while a still-identified intent kept matching an
    /// incoming object ambiguously and its owner was already terminal.
    pub ambiguous_windows: u32,
    /// When an unaddressable or persistently ambiguous intent was released.
    /// `state` is then `released`.
    pub released_at: Option<chrono::DateTime<Utc>>,
}

/// One create receipt produced by a `pim.contact.save` operation.
///
/// Multiple operations can each have created a server object for the same
/// local contact (a retry after a lost receipt plus a later Save). Keeping a
/// row per operation means a later Save's accurate remote identity is never
/// discarded because the delete intent itself is owned by an earlier one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactDeleteReceipt {
    pub contact_id: String,
    pub op_id: String,
    pub remote_id: Option<String>,
    pub href: Option<String>,
    pub etag: Option<String>,
    /// `remote_id` was derived from the href instead of being a real UID.
    pub uid_is_derived: bool,
    pub state: ContactDeleteReceiptState,
    pub suppress_until: Option<chrono::DateTime<Utc>>,
    /// Sync windows elapsed while this receipt kept matching an incoming
    /// object ambiguously and its operation was already terminal.
    pub ambiguous_windows: u32,
    /// When bounded ambiguity release fired for this receipt. A released
    /// receipt stops holding the cursor, but its exact identity is still a
    /// tombstone until the normal prune window elapses.
    pub released_at: Option<chrono::DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactDeleteReceiptState {
    /// The object may exist server-side and still has to be removed.
    Pending,
    /// The remote DELETE succeeded; kept as a tombstone until
    /// `suppress_until`.
    Settled,
}

impl ContactDeleteReceiptState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Settled => "settled",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "settled" => Self::Settled,
            _ => Self::Pending,
        }
    }
}

/// Persist the intent to remove a provider object created by an in-flight
/// create. Must run in the same transaction as the local delete.
pub fn record_contact_delete_intent(
    conn: &Connection,
    contact_id: &str,
    account_id: &str,
    address_book_id: &str,
) -> Result<()> {
    record_contact_delete_intent_owned(conn, contact_id, account_id, address_book_id, None, None)
}

/// Record a delete intent and bind it to the operation that may have produced
/// the remote object.
///
/// Cleanup is authorized by this identity: an unsent operation whose local row
/// vanished must not delete an intent that belongs to another operation.
pub fn record_contact_delete_intent_owned(
    conn: &Connection,
    contact_id: &str,
    account_id: &str,
    address_book_id: &str,
    owner_op_id: Option<&str>,
    owner_attempt: Option<u32>,
) -> Result<()> {
    let now = now_string();
    conn.execute(
        "INSERT INTO pim_contact_delete_intent
             (contact_id, account_id, address_book_id, state, owner_op_id,
              owner_attempt, unknown_windows, created_at, updated_at)
         VALUES (?1, ?2, ?3, 'pending', ?4, ?5, 0, ?6, ?6)
         ON CONFLICT(contact_id) DO UPDATE SET
             account_id = excluded.account_id,
             address_book_id = excluded.address_book_id,
             remote_id = NULL,
             href = NULL,
             etag = NULL,
             uid_is_derived = 0,
             state = 'pending',
             suppress_until = NULL,
             owner_op_id = excluded.owner_op_id,
             owner_attempt = excluded.owner_attempt,
             unknown_windows = 0,
             evidence_cursor = NULL,
             ambiguous_windows = 0,
             released_at = NULL,
             updated_at = excluded.updated_at",
        params![
            contact_id,
            account_id,
            address_book_id,
            owner_op_id,
            owner_attempt.map(i64::from),
            now
        ],
    )?;
    Ok(())
}

pub fn contact_delete_intent(
    conn: &Connection,
    contact_id: &str,
) -> Result<Option<ContactDeleteIntent>> {
    read_contact_delete_intent(
        conn.query_row(
            "SELECT account_id, address_book_id, contact_id, remote_id, href, etag,
                    state, suppress_until, owner_op_id, owner_attempt,
                    unknown_windows, released_at, ambiguous_windows, evidence_cursor,
                    uid_is_derived
             FROM pim_contact_delete_intent WHERE contact_id = ?1",
            params![contact_id],
            contact_delete_intent_row,
        )
        .optional()?,
    )
}

struct ContactDeleteIntentRow {
    account_id: String,
    address_book_id: String,
    contact_id: String,
    remote_id: Option<String>,
    href: Option<String>,
    etag: Option<String>,
    uid_is_derived: bool,
    state: String,
    suppress_until: Option<String>,
    owner_op_id: Option<String>,
    owner_attempt: Option<i64>,
    unknown_windows: i64,
    released_at: Option<String>,
    ambiguous_windows: i64,
    evidence_cursor: Option<String>,
}

fn contact_delete_intent_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ContactDeleteIntentRow> {
    Ok(ContactDeleteIntentRow {
        account_id: row.get(0)?,
        address_book_id: row.get(1)?,
        contact_id: row.get(2)?,
        remote_id: row.get(3)?,
        href: row.get(4)?,
        etag: row.get(5)?,
        state: row.get(6)?,
        suppress_until: row.get(7)?,
        owner_op_id: row.get(8)?,
        owner_attempt: row.get(9)?,
        unknown_windows: row.get(10)?,
        released_at: row.get(11)?,
        ambiguous_windows: row.get(12)?,
        evidence_cursor: row.get(13)?,
        uid_is_derived: row.get(14)?,
    })
}

fn read_contact_delete_intent(
    row: Option<ContactDeleteIntentRow>,
) -> Result<Option<ContactDeleteIntent>> {
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(ContactDeleteIntent {
        contact_id: row.contact_id,
        account_id: row.account_id,
        address_book_id: row.address_book_id,
        remote_id: row.remote_id,
        href: row.href,
        etag: row.etag,
        uid_is_derived: row.uid_is_derived,
        state: ContactDeleteIntentState::parse(&row.state),
        suppress_until: parse_dt_opt(row.suppress_until)?,
        owner_op_id: row.owner_op_id,
        owner_attempt: row.owner_attempt.map(|attempt| attempt.max(0) as u32),
        unknown_windows: row.unknown_windows.max(0) as u32,
        evidence_cursor: row.evidence_cursor,
        ambiguous_windows: row.ambiguous_windows.max(0) as u32,
        released_at: parse_dt_opt(row.released_at)?,
    }))
}

/// Attach a create receipt's remote identity to an existing delete intent.
///
/// Returns the intent when one was waiting; `None` means the local row was
/// still present when the receipt landed (no delete raced the upload).
pub fn attach_contact_delete_intent(
    conn: &Connection,
    contact_id: &str,
    remote_id: Option<&str>,
    href: Option<&str>,
    etag: Option<&str>,
    uid_is_derived: bool,
) -> Result<Option<ContactDeleteIntent>> {
    let changed = conn.execute(
        "UPDATE pim_contact_delete_intent
         SET remote_id = COALESCE(?2, remote_id),
             href = COALESCE(?3, href),
             etag = COALESCE(?4, etag),
             uid_is_derived = CASE
                 WHEN ?2 IS NOT NULL THEN ?6
                 ELSE uid_is_derived
             END,
             state = CASE
                 WHEN state = 'settled' THEN state
                 WHEN COALESCE(?2, remote_id) IS NOT NULL
                   OR COALESCE(?3, href) IS NOT NULL THEN 'pending'
                 ELSE state
             END,
             unknown_windows = CASE WHEN state = 'unknown' THEN 0 ELSE unknown_windows END,
             evidence_cursor = CASE WHEN state = 'unknown' THEN NULL ELSE evidence_cursor END,
             ambiguous_windows = CASE WHEN state = 'unknown' THEN 0 ELSE ambiguous_windows END,
             updated_at = ?5
         WHERE contact_id = ?1",
        params![
            contact_id,
            remote_id,
            href,
            etag,
            now_string(),
            i64::from(uid_is_derived)
        ],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    contact_delete_intent(conn, contact_id)
}

/// Record that a cleanup attempt has no addressable remote identity.
///
/// The intent stays (state `unknown`) on purpose: a create may have committed
/// server-side before its response was lost, so dropping the row would let the
/// next delta import an object the user deleted.
pub fn mark_contact_delete_intent_unknown(conn: &Connection, contact_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE pim_contact_delete_intent
         SET state = 'unknown',
             unknown_windows = CASE WHEN state = 'unknown' THEN unknown_windows ELSE 0 END,
             evidence_cursor = CASE WHEN state = 'unknown' THEN evidence_cursor ELSE NULL END,
             ambiguous_windows = CASE WHEN state = 'unknown' THEN ambiguous_windows ELSE 0 END,
             released_at = NULL,
             updated_at = ?2
         WHERE contact_id = ?1 AND state <> 'settled' AND state <> 'released'",
        params![contact_id, now_string()],
    )?;
    Ok(())
}

/// Turn an intent into a suppression tombstone after the remote DELETE
/// succeeded, keeping it until `suppress_until`.
pub fn settle_contact_delete_intent(
    conn: &Connection,
    contact_id: &str,
    remote_id: Option<&str>,
    href: Option<&str>,
    suppress_until: chrono::DateTime<Utc>,
    uid_is_derived: bool,
) -> Result<()> {
    conn.execute(
        "UPDATE pim_contact_delete_intent
         SET remote_id = COALESCE(?2, remote_id),
             href = COALESCE(?3, href),
             uid_is_derived = CASE
                 WHEN ?2 IS NOT NULL THEN ?6
                 ELSE uid_is_derived
             END,
             state = 'settled',
             suppress_until = ?4,
             updated_at = ?5
         WHERE contact_id = ?1",
        params![
            contact_id,
            remote_id,
            href,
            dt_string(suppress_until),
            now_string(),
            i64::from(uid_is_derived)
        ],
    )?;
    Ok(())
}

/// Every delete intent of an account, regardless of whether its operation is
/// still outstanding. A delta must protect these objects even after the worker
/// gave up on the operation (terminal failure), because the server object may
/// still exist.
pub fn contact_delete_intents(
    conn: &Connection,
    account_id: &str,
) -> Result<Vec<ContactDeleteIntent>> {
    let mut stmt = conn.prepare(
        "SELECT account_id, address_book_id, contact_id, remote_id, href, etag,
                state, suppress_until, owner_op_id, owner_attempt,
                unknown_windows, released_at, ambiguous_windows, evidence_cursor,
                uid_is_derived
         FROM pim_contact_delete_intent
         WHERE account_id = ?1
         ORDER BY created_at ASC",
    )?;
    let mut rows = stmt.query(params![account_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        if let Some(intent) = read_contact_delete_intent(Some(contact_delete_intent_row(row)?))? {
            out.push(intent);
        }
    }
    Ok(out)
}

/// Remove settled tombstones whose suppression window has elapsed.
///
/// Pending and unknown intents are never pruned: their cleanup has not
/// completed and they must keep protecting the remote object. Settled
/// per-operation receipts expire exactly like settled intents.
pub fn prune_expired_contact_delete_tombstones(
    conn: &Connection,
    now: chrono::DateTime<Utc>,
) -> Result<usize> {
    let receipts = conn.execute(
        "DELETE FROM pim_contact_delete_intent_receipt
         WHERE state = 'settled'
           AND (suppress_until IS NULL OR suppress_until <= ?1)",
        params![dt_string(now)],
    )?;
    // A receipt released by bounded ambiguity may keep suppressing its exact
    // identity for the normal tombstone window, then it expires like any other
    // settled tombstone.
    let released_receipts = conn.execute(
        "DELETE FROM pim_contact_delete_intent_receipt
         WHERE released_at IS NOT NULL
           AND released_at <= ?1",
        params![dt_string(
            now - chrono::Duration::seconds(CONTACT_DELETE_TOMBSTONE_TTL_SECONDS)
        )],
    )?;
    let intents = conn.execute(
        "DELETE FROM pim_contact_delete_intent
         WHERE state = 'settled'
           AND (suppress_until IS NULL OR suppress_until <= ?1)",
        params![dt_string(now)],
    )?;
    Ok(intents + receipts + released_receipts)
}

/// Release every unaddressable intent of an account whose wall-clock TTL has
/// elapsed and whose owner is no longer outstanding.
///
/// An intent with neither a remote id nor an href cannot be acted on: the
/// create may have committed server-side with its receipt lost. It isolates
/// the book (new objects are hidden) but never holds the cursor, so cursor
/// advancement cannot be used as release evidence — a blocked batch may never
/// move it, which deadlocked the previous model. Release is therefore
/// wall-clock based: once `updated_at` is older than `ttl_seconds` the intent
/// becomes `released` (`released_at`) and stops isolating the book.
///
/// Server-side deletion evidence releases an intent through
/// [`settle_contact_delete_intent`] (the executor's DELETE succeeded) or
/// [`remove_contact_delete_intent`] (explicit reconciliation); those paths do
/// not wait for this TTL.
///
/// Only intents without any identity are touched: an addressable intent can
/// still be cleaned up and must keep protecting its object. An owner that is
/// still `pending`/`inProgress` may yet produce a receipt, so its intent is
/// not released. Per-operation receipts do not block the release: their exact
/// identities stay protected independently of the parent intent's state.
pub fn release_expired_contact_delete_intents(
    conn: &Connection,
    account_id: &str,
    ttl_seconds: i64,
    now: chrono::DateTime<Utc>,
) -> Result<usize> {
    let cutoff = now - chrono::Duration::seconds(ttl_seconds.max(0));
    let changed = conn.execute(
        "UPDATE pim_contact_delete_intent
         SET state = 'released',
             released_at = ?3,
             updated_at = ?3
         WHERE account_id = ?1
           AND remote_id IS NULL
           AND href IS NULL
           AND state IN ('pending', 'unknown')
           AND updated_at <= ?2
           AND NOT EXISTS (
               SELECT 1 FROM pending_op p
               WHERE p.id = pim_contact_delete_intent.owner_op_id
                 AND p.state IN ('pending', 'inProgress')
           )",
        params![account_id, dt_string(cutoff), dt_string(now)],
    )?;
    Ok(changed)
}

/// Count one ambiguous sync window for an intent whose owner is no longer
/// outstanding, releasing it after `window_limit` windows.
///
/// A tombstone identity (the intent's own UID/href or a pending per-operation
/// receipt's) can keep matching an incoming object ambiguously forever
/// (CardDAV resource rename: same UID, different href). While an operation
/// could still act the batch is held so the object is not lost, but a terminal
/// owner can never resolve the conflict. After the bound the intent is
/// released (`state = 'released'`, `released_at`) so the renamed object is
/// imported and the book's cursor moves again.
pub fn advance_ambiguous_contact_delete_intents(
    conn: &Connection,
    contact_id: &str,
    window_limit: u32,
    now: chrono::DateTime<Utc>,
) -> Result<usize> {
    let limit = window_limit.max(1) as i64;
    let changed = conn.execute(
        "UPDATE pim_contact_delete_intent
         SET ambiguous_windows = ambiguous_windows + 1,
             state = CASE
                 WHEN ambiguous_windows + 1 >= ?2 THEN 'released'
                 ELSE state
             END,
             released_at = CASE
                 WHEN ambiguous_windows + 1 >= ?2 THEN ?3
                 ELSE released_at
             END,
             updated_at = ?3
         WHERE contact_id = ?1
           AND state IN ('pending', 'unknown')
           AND NOT EXISTS (
               SELECT 1 FROM pending_op p
               WHERE p.id = pim_contact_delete_intent.owner_op_id
                 AND p.state IN ('pending', 'inProgress')
           )
           AND NOT EXISTS (
               SELECT 1 FROM pim_contact_delete_intent_receipt r
               JOIN pending_op p ON p.id = r.op_id
               WHERE r.contact_id = pim_contact_delete_intent.contact_id
                 AND r.state = 'pending'
                 AND p.state IN ('pending', 'inProgress')
           )",
        params![contact_id, limit, dt_string(now)],
    )?;
    Ok(changed)
}

/// Count one ambiguous sync window for one *per-operation receipt*,
/// independently of its parent intent, releasing it after `window_limit`.
///
/// A parent intent can already be `released` while its receipt still names an
/// object that keeps arriving ambiguously (same UID, different href). The
/// parent's counter no longer runs, so without this the receipt would hold the
/// book's cursor forever. Release is recorded per receipt (`released_at`); its
/// exact identity remains a tombstone until the normal prune window, so a
/// genuinely deleted object is still never re-imported.
///
/// A receipt whose operation is still outstanding is not released: that
/// operation may yet converge the identity.
pub fn advance_ambiguous_contact_delete_receipts(
    conn: &Connection,
    contact_id: &str,
    op_id: &str,
    window_limit: u32,
    now: chrono::DateTime<Utc>,
) -> Result<usize> {
    let limit = window_limit.max(1) as i64;
    let changed = conn.execute(
        "UPDATE pim_contact_delete_intent_receipt
         SET ambiguous_windows = ambiguous_windows + 1,
             released_at = CASE
                 WHEN ambiguous_windows + 1 >= ?3 THEN ?4
                 ELSE released_at
             END,
             updated_at = ?4
         WHERE contact_id = ?1
           AND op_id = ?2
           AND state = 'pending'
           AND released_at IS NULL
           AND NOT EXISTS (
               SELECT 1 FROM pending_op p
               WHERE p.id = pim_contact_delete_intent_receipt.op_id
                 AND p.state IN ('pending', 'inProgress')
           )",
        params![contact_id, op_id, limit, dt_string(now)],
    )?;
    Ok(changed)
}
///
/// Must only be called while the delete intent exists (the receipt table has a
/// foreign key to it). Re-recording the same operation keeps a settled receipt
/// settled: the object was already removed and a retry must not resurrect the
/// cleanup.
pub fn record_contact_delete_receipt(
    conn: &Connection,
    contact_id: &str,
    op_id: &str,
    remote_id: Option<&str>,
    href: Option<&str>,
    etag: Option<&str>,
    uid_is_derived: bool,
) -> Result<()> {
    let now = now_string();
    conn.execute(
        "INSERT INTO pim_contact_delete_intent_receipt
             (contact_id, op_id, remote_id, href, etag, uid_is_derived,
              state, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?7, 'pending', ?6, ?6)
         ON CONFLICT(contact_id, op_id) DO UPDATE SET
             remote_id = COALESCE(excluded.remote_id, remote_id),
             href = COALESCE(excluded.href, href),
             etag = COALESCE(excluded.etag, etag),
             uid_is_derived = CASE
                 WHEN excluded.remote_id IS NOT NULL THEN excluded.uid_is_derived
                 ELSE uid_is_derived
             END,
             updated_at = excluded.updated_at",
        params![
            contact_id,
            op_id,
            remote_id,
            href,
            etag,
            now,
            i64::from(uid_is_derived)
        ],
    )?;
    Ok(())
}

/// Every create receipt recorded for a contact, oldest first.
pub fn contact_delete_receipts(
    conn: &Connection,
    contact_id: &str,
) -> Result<Vec<ContactDeleteReceipt>> {
    let mut stmt = conn.prepare(
        "SELECT contact_id, op_id, remote_id, href, etag, state, suppress_until,
                ambiguous_windows, released_at, uid_is_derived
         FROM pim_contact_delete_intent_receipt
         WHERE contact_id = ?1
         ORDER BY created_at ASC",
    )?;
    let mut rows = stmt.query(params![contact_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(ContactDeleteReceipt {
            contact_id: row.get(0)?,
            op_id: row.get(1)?,
            remote_id: row.get(2)?,
            href: row.get(3)?,
            etag: row.get(4)?,
            state: ContactDeleteReceiptState::parse(&row.get::<_, String>(5)?),
            suppress_until: parse_dt_opt(row.get(6)?)?,
            ambiguous_windows: row.get::<_, i64>(7)?.max(0) as u32,
            released_at: parse_dt_opt(row.get(8)?)?,
            uid_is_derived: row.get(9)?,
        });
    }
    Ok(out)
}

/// Mark one operation's receipt as removed, keeping it as a short-lived
/// suppression tombstone.
pub fn settle_contact_delete_receipt(
    conn: &Connection,
    contact_id: &str,
    op_id: &str,
    suppress_until: chrono::DateTime<Utc>,
) -> Result<()> {
    conn.execute(
        "UPDATE pim_contact_delete_intent_receipt
         SET state = 'settled',
             suppress_until = ?3,
             updated_at = ?4
         WHERE contact_id = ?1 AND op_id = ?2",
        params![contact_id, op_id, dt_string(suppress_until), now_string()],
    )?;
    Ok(())
}

/// Remove an intent only when the calling operation owns it.
///
/// The attempt must match too: a worker that read an old snapshot of the
/// operation must not delete an intent recorded for a later attempt. Legacy
/// rows without an owner are claimable by the operation that reaches them.
pub fn remove_contact_delete_intent_if_owned(
    conn: &Connection,
    contact_id: &str,
    op_id: &str,
    attempt: u32,
) -> Result<bool> {
    let changed = conn.execute(
        "DELETE FROM pim_contact_delete_intent
         WHERE contact_id = ?1
           AND (owner_op_id IS NULL
                OR (owner_op_id = ?2 AND owner_attempt = ?3))",
        params![contact_id, op_id, i64::from(attempt)],
    )?;
    Ok(changed > 0)
}

pub fn remove_contact_delete_intent(conn: &Connection, contact_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM pim_contact_delete_intent WHERE contact_id = ?1",
        params![contact_id],
    )?;
    Ok(())
}

/// One remote object hidden by book-level delete isolation.
///
/// The object was dropped while the cursor advanced, so the provider will not
/// deliver it again. It is persisted here (`payload` is the serialized
/// `RemoteContact`) and replayed when the isolating intent is released.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantinedContact {
    pub id: String,
    pub account_id: String,
    pub address_book_id: String,
    pub remote_id: Option<String>,
    pub href: Option<String>,
    pub payload: String,
}

/// Persist (or refresh) one object hidden by book-level isolation.
///
/// The identity is `remote id` + the normalized href key inside the book. The
/// href is normalized exactly like [`crate::dav::href_key`] (the same identity
/// rule DAV deltas use), so equivalent spellings of one resource (`A.vcf` and
/// `%41.vcf`) collapse into one row. Re-delivery overwrites the previous
/// snapshot: the last write is the newest version the provider delivered.
pub fn quarantine_contact(
    conn: &Connection,
    account_id: &str,
    address_book_id: &str,
    remote_id: Option<&str>,
    href: Option<&str>,
    payload: &str,
) -> Result<()> {
    let href_key = href
        .filter(|value| !value.is_empty())
        .map(crate::dav::href_key)
        .unwrap_or_default();
    let identity = format!("{}\u{1f}{}", remote_id.unwrap_or_default(), href_key);
    let now = now_string();
    conn.execute(
        "INSERT INTO pim_contact_quarantine
             (id, account_id, address_book_id, identity, remote_id, href,
              payload, created_at, updated_at, seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8,
                 (SELECT COALESCE(MAX(seq), 0) + 1 FROM pim_contact_quarantine))
         ON CONFLICT(address_book_id, identity) DO UPDATE SET
             remote_id = excluded.remote_id,
             href = excluded.href,
             payload = excluded.payload,
             updated_at = excluded.updated_at,
             seq = excluded.seq",
        params![
            nuncio_core::models::new_id(),
            account_id,
            address_book_id,
            identity,
            remote_id,
            href,
            payload,
            now
        ],
    )?;
    Ok(())
}

/// Every object currently quarantined for one book, oldest version first.
///
/// A replay applies the returned snapshots in sequence, so the newest version
/// of an object must be last or a stale duplicate would overwrite it.
/// Ordering is therefore by the monotonic write `seq`, never by the one-second
/// `updated_at` or the insertion `rowid`: a resource delivered as UID
/// X -> Y -> X inside one second updates X's row twice, so its `seq` is the
/// newest and it is applied last.
pub fn quarantined_contacts(
    conn: &Connection,
    account_id: &str,
    address_book_id: &str,
) -> Result<Vec<QuarantinedContact>> {
    let mut stmt = conn.prepare(
        "SELECT id, account_id, address_book_id, remote_id, href, payload
         FROM pim_contact_quarantine
         WHERE account_id = ?1 AND address_book_id = ?2
         ORDER BY seq ASC, rowid ASC",
    )?;
    let mut rows = stmt.query(params![account_id, address_book_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(QuarantinedContact {
            id: row.get(0)?,
            account_id: row.get(1)?,
            address_book_id: row.get(2)?,
            remote_id: row.get(3)?,
            href: row.get(4)?,
            payload: row.get(5)?,
        });
    }
    Ok(out)
}

/// Whether a book currently has objects waiting for a quarantine replay.
pub fn has_quarantined_contacts(
    conn: &Connection,
    account_id: &str,
    address_book_id: &str,
) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pim_contact_quarantine
         WHERE account_id = ?1 AND address_book_id = ?2",
        params![account_id, address_book_id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Drop one replayed (or no longer relevant) quarantine row.
pub fn remove_quarantined_contact(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM pim_contact_quarantine WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

/// Drop quarantine snapshots that name a remote object deleted by the
/// provider.
///
/// Either identity component that is present matches: a single provider
/// deletion must forget every snapshot of that object, otherwise a later
/// release would resurrect a contact the server no longer has.
pub fn remove_quarantined_contacts_matching(
    conn: &Connection,
    account_id: &str,
    address_book_id: &str,
    remote_id: Option<&str>,
    href: Option<&str>,
) -> Result<usize> {
    let remote_id = remote_id.filter(|value| !value.is_empty());
    let href_key = href
        .filter(|value| !value.is_empty())
        .map(crate::dav::href_key);
    if remote_id.is_none() && href_key.is_none() {
        return Ok(0);
    }
    // Remote ids are exact (a UID). Hrefs are compared by identity key, so a
    // provider deletion reported as `A.vcf` also forgets a snapshot stored as
    // `%41.vcf`; otherwise a later quarantine release would resurrect the
    // deleted contact.
    let mut stmt = conn.prepare(
        "SELECT id, remote_id, href FROM pim_contact_quarantine
         WHERE account_id = ?1 AND address_book_id = ?2",
    )?;
    let mut rows = stmt.query(params![account_id, address_book_id])?;
    let mut matched = Vec::new();
    while let Some(row) = rows.next()? {
        let stored_remote: Option<String> = row.get(1)?;
        let stored_href: Option<String> = row.get(2)?;
        let remote_matches = remote_id.is_some() && stored_remote.as_deref() == remote_id;
        let href_matches = href_key.as_deref().is_some_and(|key| {
            stored_href
                .as_deref()
                .is_some_and(|stored| crate::dav::href_key(stored) == key)
        });
        if remote_matches || href_matches {
            matched.push(row.get::<_, String>(0)?);
        }
    }
    drop(rows);
    drop(stmt);
    let removed = matched.len();
    for id in matched {
        remove_quarantined_contact(conn, &id)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::{Db, CORE_MIGRATIONS, CORE_NAMESPACE};

    fn db() -> Db {
        let db = Db::open_in_memory().expect("in-memory db");
        db.migrate(CORE_NAMESPACE, CORE_MIGRATIONS)
            .expect("core schema");
        db.migrate(PIM_NAMESPACE, PIM_MIGRATIONS)
            .expect("pim schema");
        db
    }

    /// The durable task write sequence must start above any receipt persisted
    /// by a previous build and keep growing when the pending queue is empty
    /// (the state `prune_finished` leaves behind).
    #[test]
    fn write_sequence_survives_pruning_and_continues_above_a_stored_receipt() {
        let db = Db::open_in_memory().expect("in-memory db");
        db.migrate(CORE_NAMESPACE, CORE_MIGRATIONS)
            .expect("core schema");
        // Stop before 0017 so the migration's seeding can be observed.
        db.migrate(PIM_NAMESPACE, &PIM_MIGRATIONS[..16])
            .expect("pim schema before 0017");
        db.write_sync(|conn| {
            conn.execute(
                "INSERT INTO task (id, list_id, account_id, title, created_at, updated_at)
                 VALUES ('t1', 'local-tasks', 'local', 't',
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                [],
            )?;
            // A receipt whose operation row was already pruned: only the
            // persisted sequence remains.
            conn.execute(
                "INSERT INTO pim_task_content_state
                     (task_id, content_rev, row_rev, receipt_op_id,
                      receipt_op_sequence, updated_at)
                 VALUES ('t1', 0, 1, 'old-op', 42, '2026-01-01T00:00:00Z')",
                [],
            )?;
            Ok(())
        })
        .expect("stored receipt");
        db.migrate(PIM_NAMESPACE, PIM_MIGRATIONS)
            .expect("task write sequence migration");
        db.write_sync(|conn| {
            let first = next_task_write_seq(conn)?;
            assert!(
                first > 42,
                "a new operation must sort after a receipt persisted before the upgrade"
            );
            let second = next_task_write_seq(conn)?;
            assert!(
                second > first,
                "an empty queue must not make the sequence rewind"
            );
            Ok(())
        })
        .expect("durable sequence");
    }

    #[test]
    fn calendar_mapping_round_trip() {
        let db = db();
        db.write_sync(|conn| {
            let calendar = nuncio_core::repo::calendars::create(conn, "local", "Test", "#0078D4")?;
            let mapping = CalendarRemote::new(
                calendar.id.clone(),
                "local",
                "caldav",
                "https://dav/cal/",
                Some("https://dav/cal/".into()),
            );
            set(conn, &mapping)?;
            assert_eq!(get_by_calendar(conn, &calendar.id)?, Some(mapping.clone()));
            assert_eq!(
                get_by_remote(conn, "local", "caldav", "https://dav/cal/")?,
                Some(mapping)
            );
            Ok(())
        })
        .expect("round trip");
    }

    #[test]
    fn quarantine_removal_matches_equivalent_href_spellings() {
        let db = db();
        db.write_sync(|conn| {
            let account = nuncio_core::repo::accounts::local(conn)?;
            let book = nuncio_core::repo::address_books::get_required(conn, "local-contacts")?;
            quarantine_contact(
                conn,
                &account.id,
                &book.id,
                Some("uid-1"),
                Some("https://dav/ab/%41.vcf"),
                "{}",
            )?;
            quarantine_contact(
                conn,
                &account.id,
                &book.id,
                Some("uid-2"),
                Some("https://dav/ab/b.vcf"),
                "{}",
            )?;

            // `%41` and `A` are the same resource; the other vCard must stay.
            let removed = remove_quarantined_contacts_matching(
                conn,
                &account.id,
                &book.id,
                None,
                Some("https://dav/ab/A.vcf"),
            )?;
            assert_eq!(removed, 1);
            let left = quarantined_contacts(conn, &account.id, &book.id)?;
            assert_eq!(left.len(), 1);
            assert_eq!(left[0].href.as_deref(), Some("https://dav/ab/b.vcf"));
            Ok(())
        })
        .expect("quarantine removal");
    }

    #[test]
    fn quarantine_merges_equivalent_spellings_and_replays_the_newest() {
        let db = db();
        db.write_sync(|conn| {
            let account = nuncio_core::repo::accounts::local(conn)?;
            let book = nuncio_core::repo::address_books::get_required(conn, "local-contacts")?;
            // The same resource delivered as `A` (v1), `%41` (v2), `A` (v3).
            for (href, payload) in [
                ("https://dav/ab/A.vcf", "v1"),
                ("https://dav/ab/%41.vcf", "v2"),
                ("https://dav/ab/A.vcf", "v3"),
            ] {
                quarantine_contact(
                    conn,
                    &account.id,
                    &book.id,
                    Some("uid-1"),
                    Some(href),
                    payload,
                )?;
            }
            let rows = quarantined_contacts(conn, &account.id, &book.id)?;
            assert_eq!(rows.len(), 1, "equivalent spellings are one resource");
            assert_eq!(rows[0].payload, "v3", "the last delivery overwrites");
            assert_eq!(
                rows[0].href.as_deref(),
                Some("https://dav/ab/A.vcf"),
                "the stored snapshot keeps the newest raw spelling"
            );

            // A row written before identity normalization is still possible.
            // Replay must consume versions oldest first, so the newest snapshot
            // is applied last instead of winning only by insertion order.
            conn.execute(
                "INSERT INTO pim_contact_quarantine
                     (id, account_id, address_book_id, identity, remote_id, href,
                      payload, created_at, updated_at)
                 VALUES ('legacy', ?1, ?2, ?3, 'uid-1',
                         'https://dav/ab/%41.vcf', 'v0', ?4, ?4)",
                params![
                    account.id,
                    book.id,
                    "uid-1\u{1f}https://dav/ab/%41.vcf",
                    "2000-01-01T00:00:00Z"
                ],
            )?;
            let rows = quarantined_contacts(conn, &account.id, &book.id)?;
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].payload, "v0", "the older version replays first");
            assert_eq!(rows[1].payload, "v3", "the newest version replays last");
            Ok(())
        })
        .expect("quarantine merge");
    }

    /// Regression ① at the storage layer: the same resource delivered as
    /// UID X -> Y -> X within one second used to keep two rows ordered by
    /// `updated_at`/`rowid`, so the stale Y replayed *after* the newest X.
    #[test]
    fn quarantine_orders_same_second_uid_changes_by_write_sequence() {
        let db = db();
        db.write_sync(|conn| {
            let account = nuncio_core::repo::accounts::local(conn)?;
            let book = nuncio_core::repo::address_books::get_required(conn, "local-contacts")?;
            for (remote_id, href, payload) in [
                ("uid-x", "https://dav/ab/A.vcf", "x-old"),
                ("uid-y", "https://dav/ab/%41.vcf", "y"),
                ("uid-x", "https://dav/ab/A.vcf", "x-new"),
            ] {
                quarantine_contact(
                    conn,
                    &account.id,
                    &book.id,
                    Some(remote_id),
                    Some(href),
                    payload,
                )?;
            }
            let rows = quarantined_contacts(conn, &account.id, &book.id)?;
            assert_eq!(rows.len(), 2, "the two UIDs are two snapshots");
            assert_eq!(
                rows[0].payload, "y",
                "Y is older than the last write even though X's row was inserted first"
            );
            assert_eq!(
                rows.last().map(|row| row.payload.as_str()),
                Some("x-new"),
                "the newest X is applied last"
            );
            assert_eq!(
                rows.last().and_then(|row| row.remote_id.as_deref()),
                Some("uid-x")
            );
            Ok(())
        })
        .expect("quarantine ordering");
    }

    #[test]
    fn task_lookup_matches_equivalent_href_spellings() {
        let db = db();
        db.write_sync(|conn| {
            let list = nuncio_core::repo::task_lists::create(conn, "local", "Tasks", "#0078D4")?;
            let input = nuncio_core::models::TaskInput {
                title: "Task".into(),
                ..Default::default()
            };
            let task = nuncio_core::repo::tasks::create(conn, &list.id, &input)?;
            set_task_href(conn, &task.id, "https://dav/tasks/%41.ics")?;

            assert_eq!(
                task_by_href(conn, &list.id, "https://dav/tasks/A.ics")?,
                Some(task.id.clone())
            );
            // `%7e` and `~` are the same resource too.
            set_task_href(conn, &task.id, "https://dav/tasks/%7e.ics")?;
            assert_eq!(
                task_by_href(conn, &list.id, "https://dav/tasks/~.ics")?,
                Some(task.id.clone())
            );
            assert_eq!(
                task_by_href(conn, &list.id, "https://dav/tasks/other.ics")?,
                None
            );
            Ok(())
        })
        .expect("task href lookup");
    }

    #[test]
    fn contact_delete_intent_round_trip() {
        let db = db();
        db.write_sync(|conn| {
            record_contact_delete_intent(conn, "c1", "local", "local-contacts")?;
            let intent = contact_delete_intent(conn, "c1")?.expect("intent");
            assert_eq!(intent.account_id, "local");
            assert_eq!(intent.remote_id, None);
            assert_eq!(intent.state, ContactDeleteIntentState::Pending);

            let attached = attach_contact_delete_intent(
                conn,
                "c1",
                Some("remote-1"),
                None,
                Some("e1"),
                false,
            )?
            .expect("receipt attached");
            assert_eq!(attached.remote_id.as_deref(), Some("remote-1"));
            assert_eq!(attached.etag.as_deref(), Some("e1"));
            assert!(
                attach_contact_delete_intent(conn, "unknown", Some("x"), None, None, false)?
                    .is_none()
            );

            remove_contact_delete_intent(conn, "c1")?;
            assert!(contact_delete_intent(conn, "c1")?.is_none());
            Ok(())
        })
        .expect("delete intent");
    }

    #[test]
    fn unknown_intent_survives_and_settled_intent_expires() {
        let db = db();
        db.write_sync(|conn| {
            record_contact_delete_intent(conn, "c1", "local", "local-contacts")?;
            mark_contact_delete_intent_unknown(conn, "c1")?;
            let intent = contact_delete_intent(conn, "c1")?.expect("unknown intent");
            assert_eq!(intent.state, ContactDeleteIntentState::Unknown);
            assert!(intent.suppress_until.is_none());

            // A known identity puts the intent back into the pending state so
            // the next executor pass tries the DELETE again.
            let attached =
                attach_contact_delete_intent(conn, "c1", Some("remote-1"), None, None, false)?
                    .expect("attached");
            assert_eq!(attached.state, ContactDeleteIntentState::Pending);

            let now = Utc::now();
            settle_contact_delete_intent(conn, "c1", Some("remote-1"), None, now, false)?;
            let settled = contact_delete_intent(conn, "c1")?.expect("settled");
            assert_eq!(settled.state, ContactDeleteIntentState::Settled);

            // Still inside the window: not pruned.
            assert_eq!(
                prune_expired_contact_delete_tombstones(conn, now - chrono::Duration::seconds(1))?,
                0
            );
            assert!(contact_delete_intent(conn, "c1")?.is_some());
            // Window elapsed: the tombstone expires automatically.
            assert_eq!(
                prune_expired_contact_delete_tombstones(conn, now + chrono::Duration::seconds(1))?,
                1
            );
            assert!(contact_delete_intent(conn, "c1")?.is_none());
            Ok(())
        })
        .expect("intent lifecycle");
    }

    #[test]
    fn unaddressable_intent_is_released_after_the_wall_clock_ttl() {
        let db = db();
        db.write_sync(|conn| {
            record_contact_delete_intent(conn, "c1", "local", "local-contacts")?;
            mark_contact_delete_intent_unknown(conn, "c1")?;
            let now = Utc::now();
            let ttl = CONTACT_DELETE_UNKNOWN_RELEASE_TTL_SECONDS;

            // Inside the TTL nothing is released.
            assert_eq!(
                release_expired_contact_delete_intents(conn, "local", ttl, now)?,
                0
            );
            let intent = contact_delete_intent(conn, "c1")?.expect("intent");
            assert_eq!(intent.state, ContactDeleteIntentState::Unknown);

            // Exactly at the TTL boundary the intent is released with an
            // auditable `released_at`, independent of any cursor movement.
            let later = now + chrono::Duration::seconds(ttl);
            assert_eq!(
                release_expired_contact_delete_intents(conn, "local", ttl, later)?,
                1
            );
            let released = contact_delete_intent(conn, "c1")?.expect("released");
            assert_eq!(released.state, ContactDeleteIntentState::Released);
            assert_eq!(
                released.released_at.map(|at| at.timestamp()),
                Some(later.timestamp())
            );

            // A released intent is not released twice, and an addressable
            // intent is never released by the TTL: its cleanup can still run
            // and it must keep protecting its object.
            assert_eq!(
                release_expired_contact_delete_intents(
                    conn,
                    "local",
                    ttl,
                    later + chrono::Duration::seconds(1),
                )?,
                0
            );
            record_contact_delete_intent(conn, "c2", "local", "local-contacts")?;
            attach_contact_delete_intent(conn, "c2", Some("remote-2"), Some("h2"), None, false)?;
            conn.execute(
                "UPDATE pim_contact_delete_intent SET updated_at = ?1 WHERE contact_id = 'c2'",
                params![dt_string(now - chrono::Duration::seconds(ttl * 2))],
            )?;
            assert_eq!(
                release_expired_contact_delete_intents(
                    conn,
                    "local",
                    ttl,
                    later + chrono::Duration::seconds(2),
                )?,
                0
            );

            // An owner that is still outstanding may yet produce a receipt:
            // its intent is not released even after the TTL.
            record_contact_delete_intent(conn, "c3", "local", "local-contacts")?;
            mark_contact_delete_intent_unknown(conn, "c3")?;
            let op = nuncio_core::repo::pending_ops::enqueue(
                conn,
                nuncio_core::models::NewPendingOp::new(
                    "local",
                    "pim.contact.save",
                    serde_json::json!({}),
                    "test",
                ),
            )?;
            conn.execute(
                "UPDATE pim_contact_delete_intent
                 SET owner_op_id = ?1, updated_at = ?2
                 WHERE contact_id = 'c3'",
                params![op.id, dt_string(now - chrono::Duration::seconds(ttl * 2))],
            )?;
            assert_eq!(
                release_expired_contact_delete_intents(
                    conn,
                    "local",
                    ttl,
                    later + chrono::Duration::seconds(3),
                )?,
                0
            );
            nuncio_core::repo::pending_ops::mark_failed(conn, &op.id, "terminal")?;
            assert_eq!(
                release_expired_contact_delete_intents(
                    conn,
                    "local",
                    ttl,
                    later + chrono::Duration::seconds(4),
                )?,
                1
            );
            Ok(())
        })
        .expect("release");
    }

    #[test]
    fn ambiguous_intent_is_released_after_the_window_limit() {
        let db = db();
        db.write_sync(|conn| {
            record_contact_delete_intent(conn, "c1", "local", "local-contacts")?;
            attach_contact_delete_intent(conn, "c1", Some("uid-1"), Some("h1"), None, false)?;
            let now = Utc::now();
            for window in 1..CONTACT_DELETE_AMBIGUOUS_RELEASE_WINDOWS {
                assert_eq!(
                    advance_ambiguous_contact_delete_intents(
                        conn,
                        "c1",
                        CONTACT_DELETE_AMBIGUOUS_RELEASE_WINDOWS,
                        now,
                    )?,
                    1
                );
                let intent = contact_delete_intent(conn, "c1")?.expect("intent");
                assert_eq!(intent.state, ContactDeleteIntentState::Pending);
                assert_eq!(intent.ambiguous_windows, window);
            }
            assert_eq!(
                advance_ambiguous_contact_delete_intents(
                    conn,
                    "c1",
                    CONTACT_DELETE_AMBIGUOUS_RELEASE_WINDOWS,
                    now,
                )?,
                1
            );
            let released = contact_delete_intent(conn, "c1")?.expect("released");
            assert_eq!(released.state, ContactDeleteIntentState::Released);
            assert!(released.released_at.is_some());

            // A pending receipt whose operation is still outstanding keeps
            // the intent alive even when the intent itself has no identity.
            record_contact_delete_intent(conn, "c2", "local", "local-contacts")?;
            mark_contact_delete_intent_unknown(conn, "c2")?;
            let op = nuncio_core::repo::pending_ops::enqueue(
                conn,
                nuncio_core::models::NewPendingOp::new(
                    "local",
                    "pim.contact.save",
                    serde_json::json!({}),
                    "test",
                ),
            )?;
            record_contact_delete_receipt(conn, "c2", &op.id, Some("remote-2"), None, None, false)?;
            assert_eq!(
                advance_ambiguous_contact_delete_intents(
                    conn,
                    "c2",
                    CONTACT_DELETE_AMBIGUOUS_RELEASE_WINDOWS,
                    now,
                )?,
                0,
                "an outstanding receipt operation can still resolve the conflict"
            );
            nuncio_core::repo::pending_ops::mark_failed(conn, &op.id, "terminal")?;
            assert_eq!(
                advance_ambiguous_contact_delete_intents(
                    conn,
                    "c2",
                    CONTACT_DELETE_AMBIGUOUS_RELEASE_WINDOWS,
                    now,
                )?,
                1
            );
            Ok(())
        })
        .expect("ambiguous release");
    }

    #[test]
    fn create_receipts_are_per_operation_and_cascade() {
        let db = db();
        db.write_sync(|conn| {
            record_contact_delete_intent(conn, "c1", "local", "local-contacts")?;
            record_contact_delete_receipt(conn, "c1", "op-a", None, Some("ha"), None, false)?;
            record_contact_delete_receipt(conn, "c1", "op-b", Some("rb"), Some("hb"), None, false)?;
            let receipts = contact_delete_receipts(conn, "c1")?;
            assert_eq!(receipts.len(), 2, "each operation keeps its own receipt");
            assert_eq!(
                receipts
                    .iter()
                    .find(|receipt| receipt.op_id == "op-b")
                    .expect("op-b receipt")
                    .remote_id
                    .as_deref(),
                Some("rb")
            );
            let now = Utc::now();
            settle_contact_delete_receipt(conn, "c1", "op-a", now)?;
            let receipts = contact_delete_receipts(conn, "c1")?;
            assert_eq!(
                receipts
                    .iter()
                    .find(|receipt| receipt.op_id == "op-a")
                    .expect("op-a receipt")
                    .state,
                ContactDeleteReceiptState::Settled
            );
            assert_eq!(
                receipts
                    .iter()
                    .find(|receipt| receipt.op_id == "op-b")
                    .expect("op-b receipt")
                    .state,
                ContactDeleteReceiptState::Pending
            );
            remove_contact_delete_intent(conn, "c1")?;
            assert!(
                contact_delete_receipts(conn, "c1")?.is_empty(),
                "receipts must cascade with their intent"
            );
            Ok(())
        })
        .expect("receipts");
    }

    #[test]
    fn reminder_markers_are_idempotent() {
        let db = db();
        db.write_sync(|conn| {
            assert!(!reminder_fired(conn, "e1", "2026-11-12")?);
            mark_reminder_fired(conn, "e1", "2026-11-12")?;
            mark_reminder_fired(conn, "e1", "2026-11-12")?;
            assert!(reminder_fired(conn, "e1", "2026-11-12")?);
            clear_reminder_fired(conn, "e1", "2026-11-12")?;
            assert!(!reminder_fired(conn, "e1", "2026-11-12")?);
            Ok(())
        })
        .expect("markers");
    }
}
