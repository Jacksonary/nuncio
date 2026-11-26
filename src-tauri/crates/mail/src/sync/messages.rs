//! Metadata-first message synchronization.
//!
//! Strategy (see PLAN §6.1):
//! * initial sync: newest-first UID batches of 200 inside the account's sync
//!   window, metadata only, previews for the first text part (2 KiB);
//! * incremental sync: CONDSTORE `CHANGEDSINCE` for flags when available,
//!   otherwise a UID/FLAGS diff; new messages from `uidnext:*`; expunge
//!   detection via `UID SEARCH`;
//! * UIDVALIDITY changes clear the folder and force a fresh initial sync;
//! * a full flag reconcile runs every 12 hours.

use std::collections::{HashMap, HashSet};

use async_imap::imap_proto::{AttributeValue, MessageSection, Response, SectionPath, Status};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use nuncio_core::models::SyncModule;
use nuncio_core::models::{Folder, FolderRole, PendingOpState};
use nuncio_core::{events, repo, AppError, CoreContext, Result};
use rusqlite::OptionalExtension;
use tracing::{debug, warn};

use crate::imap::conn::ImapConnection;
use crate::imap::fetch::{meta_from_fetch, FlagSnapshot};
use crate::ops::OpMessageState;
use crate::util::{imap_date, imap_error, uid_set};

const BATCH_SIZE: usize = 200;
/// `load_older` is a user-initiated single step, not a background sweep. A
/// smaller batch keeps the first "load older" responsive on slow providers
/// (163).
const LOAD_OLDER_BATCH_SIZE: usize = 50;
const FLAG_BATCH_SIZE: usize = 500;
const PREVIEW_BYTES: usize = 2048;
const FULL_RECONCILE_HOURS: i64 = 12;

/// Result of syncing one folder.
#[derive(Debug, Default)]
pub struct SyncOutcome {
    pub new_messages: Vec<nuncio_core::models::MessageSummary>,
    pub deleted_ids: Vec<String>,
    pub initial: bool,
    pub folder_id: String,
}

/// `sync_state` key prefixes written by builds that still requested a
/// server-side `$filter` for Graph delta.
///
/// Those tokens silently stop at Graph's 5,000-result cap, so resuming one
/// would keep skipping changes the truncated scan never saw. The v2 keys
/// deliberately orphan them; this cleanup removes the dead rows instead of
/// letting them accumulate. The unprefixed variants are the tokens written
/// before the versioned rename. None of these prefixes can match a live key:
/// `graph_delta_v2:` does not start with `graph_delta:`, and likewise for the
/// pending marker.
const LEGACY_GRAPH_CURSOR_PREFIXES: &[&str] = &[
    "graph_delta_v1:",
    "graph_pending_v1:",
    "graph_delta:",
    "graph_pending:",
];

/// Best-effort removal of Graph delta cursors from before the unfiltered scan.
///
/// Idempotent and scoped to `account_id` + the `mail` module, so it can run on
/// every account sync. A failure is logged and ignored: stale rows are inert,
/// and cleanup must never block message synchronization.
pub async fn cleanup_legacy_graph_cursors(ctx: &CoreContext, account_id: &str) {
    let account_id = account_id.to_string();
    let scope = account_id.clone();
    let result = ctx
        .db
        .write(move |db| {
            for entry in repo::sync_state::list_module(db, &scope, "mail")? {
                if LEGACY_GRAPH_CURSOR_PREFIXES
                    .iter()
                    .any(|prefix| entry.key.starts_with(prefix))
                {
                    repo::sync_state::delete_key(db, &scope, "mail", &entry.key)?;
                }
            }
            Ok(())
        })
        .await;
    if let Err(error) = result {
        warn!(
            %error,
            account = %account_id,
            "cannot remove legacy Graph delta cursors; continuing"
        );
    }
}

/// Synchronize one folder. Callers own the sync-status lifecycle
/// (`begin`/`finish`/`fail`) except for batch progress updates.
pub async fn sync_folder(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
) -> Result<SyncOutcome> {
    if !folder.is_selectable {
        return Ok(SyncOutcome {
            folder_id: folder.id.clone(),
            ..SyncOutcome::default()
        });
    }

    if crate::graph::uses_graph(account) {
        // Clear early dispatch: a Microsoft account never touches the IMAP
        // session that was opened for it (Exchange refuses the auth anyway).
        return sync_folder_graph(ctx, account, folder).await;
    }

    let (stored_validity, stored_uidnext, stored_modseq) = ctx
        .db
        .read({
            let folder_id = folder.id.clone();
            move |db| repo::folders::sync_flags(db, &folder_id)
        })
        .await?;

    let mailbox = if conn.caps.condstore {
        conn.select_condstore(&folder.remote_path).await
    } else {
        conn.select(&folder.remote_path).await
    }
    .map_err(|error| {
        if error.code() == "protocol" {
            AppError::not_found(format!("mailbox {} is not available", folder.remote_path))
        } else {
            error
        }
    })?;

    let current_validity = mailbox.uid_validity.map(|value| value as i64);
    let current_uidnext = mailbox.uid_next.map(|value| value as i64);
    let current_modseq = mailbox.highest_modseq.map(|value| value as i64);

    let mut initial = stored_uidnext.is_none();
    if let (Some(stored), Some(current)) = (stored_validity, current_validity) {
        if stored != current {
            warn!(
                folder = %folder.remote_path,
                stored, current,
                "UIDVALIDITY changed; clearing local folder"
            );
            let folder_id = folder.id.clone();
            ctx.db
                .write(move |db| {
                    repo::messages::clear_folder(db, &folder_id)?;
                    repo::folders::set_sync_flags(db, &folder_id, None, None, None)?;
                    repo::folders::recompute_counts(db, &folder_id)
                })
                .await?;
            initial = true;
        }
    }

    let mut outcome = SyncOutcome {
        initial,
        folder_id: folder.id.clone(),
        ..SyncOutcome::default()
    };

    if initial {
        initial_sync(
            ctx,
            conn,
            account,
            folder,
            current_validity,
            current_uidnext,
            &mut outcome,
        )
        .await?;
    } else {
        incremental_sync(
            ctx,
            conn,
            account,
            folder,
            current_validity,
            stored_uidnext,
            stored_modseq,
            &mut outcome,
        )
        .await?;
    }

    // Rows a failed operation left without a trusted folder-local identity are
    // resolved here, without the sync window: a UID that the server still
    // reports is confirmed, otherwise a Message-ID header search rebinds the
    // row (or proves it is gone). Best effort: a failed lookup only leaves the
    // marker for the next pass.
    if let Err(error) =
        reconcile_pending_identities(ctx, conn, account, folder, current_validity).await
    {
        warn!(
            %error,
            folder = %folder.remote_path,
            "pending identity reconciliation failed"
        );
    }

    let folder_id = folder.id.clone();
    let last_sync = Utc::now();
    ctx.db
        .write(move |db| {
            // Some servers (NetEase 163) never report UIDNEXT. Without a stored
            // value every later sync would look like a first sync: a full
            // window scan each time, and no increments. The highest UID we
            // hold, plus one, is a safe stand-in because UIDs only grow within
            // one UIDVALIDITY.
            //
            // With nothing stored (an empty folder, or only mail older than the
            // sync window) there is nothing to derive it from: keep it unset
            // rather than store 1, which would make the next incremental sync
            // request `UID 1:*` and pull the whole history past the window.
            let uidnext = match current_uidnext {
                Some(value) => Some(value),
                None => db
                    .query_row(
                        "SELECT MAX(uid) + 1 FROM message
                         WHERE folder_id = ?1 AND uid IS NOT NULL",
                        rusqlite::params![folder_id],
                        |row| row.get::<_, Option<i64>>(0),
                    )
                    .ok()
                    .flatten(),
            };
            repo::folders::set_sync_flags(
                db,
                &folder_id,
                current_validity,
                uidnext,
                current_modseq,
            )?;
            repo::folders::set_last_sync_at(db, &folder_id, last_sync)?;
            repo::folders::recompute_counts(db, &folder_id)
        })
        .await?;

    Ok(outcome)
}

/// Synchronize one folder through Microsoft Graph, without an IMAP connection.
///
/// The transport and delta-token recovery live in [`crate::graph::messages`];
/// this function is the bridge into the same local model the IMAP path writes:
/// every delta entry goes through `repo::messages::upsert`, deletions through
/// `repo::messages::delete_local`, and folder counts are recomputed.  Reusing
/// the repo layer (instead of a Graph-specific table writer) is what keeps
/// listing, search, threading, rules and notifications provider agnostic.
///
/// Graph has no IMAP UID.  The existing service layer only queues server
/// operations for rows that carry one, so each Graph message is given a
/// deterministic synthetic UID derived from its immutable Graph id.  The
/// executor resolves that UID back to the Graph id through the durable
/// `graph_remote:{folder_id}:{uid}` mapping written here; the value never
/// leaves the local database and is not sent to Microsoft.
pub async fn sync_folder_graph(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    folder: &Folder,
) -> Result<SyncOutcome> {
    sync_folder_graph_with(ctx, account, folder, &crate::graph::GraphMailClient::new()).await
}

/// [`sync_folder_graph`] against an explicit client; tests use a loopback
/// server so the delta request and its projection can be observed without
/// talking to Microsoft.
pub async fn sync_folder_graph_with(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    client: &crate::graph::GraphMailClient,
) -> Result<SyncOutcome> {
    if !folder.is_selectable {
        return Ok(SyncOutcome {
            folder_id: folder.id.clone(),
            ..SyncOutcome::default()
        });
    }

    let cursor_key = crate::graph::messages::delta_cursor_key(&folder.id);
    let cursor = {
        let account_id = account.id.clone();
        let cursor_key = cursor_key.clone();
        ctx.db
            .read(move |db| repo::sync_state::get(db, &account_id, "mail", &cursor_key))
            .await?
    };
    // No stored cursor means this folder has not completed a first delta scan
    // yet. That pass now walks the whole folder without a `$filter`, so a large
    // mailbox can take several rounds (see `graph::messages::MAX_PAGES`); the
    // marker tells the UI this is expected work rather than a stalled sync. A
    // resumed truncated pass has a pending page but still no cursor, so it
    // stays marked until the scan finally produces one.
    let full_sync = cursor.is_none();
    // The marker owns the visible flag until this future finishes. It lives in
    // a guard rather than an explicit clear so that a dropped future (the
    // account worker aborted while the request is on the wire) still clears
    // it; the guard also refuses to clear a flag a newer pass has set.
    let full_sync_marker = if full_sync {
        ctx.sync_status
            .mark_full_sync(&account.id, SyncModule::Mail)
    } else {
        None
    };

    let result = async {
        // `Folder::remote_path` is the readable local path; Graph delta and
        // operation endpoints need the opaque folder id recorded at folder
        // sync.
        let graph_path =
            crate::sync::folders::graph_folder_id(ctx, &account.id, &folder.remote_path)
                .await?
                .ok_or_else(|| {
                    AppError::not_found(format!(
                        "Graph folder id is unknown for {}; refresh folders first",
                        folder.remote_path
                    ))
                })?;
        let mut graph_folder = folder.clone();
        graph_folder.remote_path = graph_path;

        let delta = crate::graph::messages::sync_messages(
            client,
            ctx,
            &account.id,
            &graph_folder,
            cursor.as_deref(),
            account.sync_window_days,
        )
        .await?;
        apply_graph_delta(ctx, account, folder, delta).await
    }
    .await;
    drop(full_sync_marker);
    result
}

/// Persist one Graph batch (delta pass or older-mail page) into the local
/// model and emit the matching events. Shared by [`sync_folder_graph`] and
/// [`load_older_graph`] so both write rows exactly the same way.
async fn apply_graph_delta(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    delta: crate::graph::messages::MessageDelta,
) -> Result<SyncOutcome> {
    let initial = delta.full_sync;
    // Snapshot the ids before the messages move into the write closure.  A
    // complete delta scan is the Graph equivalent of an IMAP expunge pass: any
    // local Graph row that the server did not return is gone.
    let seen_ids: HashSet<String> = delta
        .messages
        .iter()
        .map(|stored| stored.summary.id.clone())
        .collect();

    let account_id = account.id.clone();
    let folder_id = folder.id.clone();
    // Copy out of the borrowed account for the `'static` write closure.
    let sync_window_days = account.sync_window_days;
    let messages = delta.messages;
    // A confirmed move destination must update the optimistic projection that
    // already lives in this folder. The projection has no provider id, so this
    // is the only association the writer has; without it the new Graph id
    // becomes a second row and the original one stays unbound forever.
    let adopted_local_ids = delta.adopted_local_ids;
    let removed_ids = delta.removed_remote_ids;
    // The cursor and pending page belong to exactly these rows. They are
    // persisted inside the same write transaction below: committing a cursor
    // without its rows would make the retry skip the unstored changes.
    let delta_link = delta.delta_link;
    let pending_cursor = delta.pending_cursor;
    let resync_folder = folder.id.clone();

    let (inserted, updated, removed_ids) = ctx
        .db
        .write(move |db| {
            let mut inserted = Vec::new();
            let mut updated = Vec::new();
            // A delta page only carries identities for the Graph ids it
            // actually returned. A confirmed move destination that has not
            // appeared in this batch must still protect its projection: an
            // update or removal carrying the row's *old* id would otherwise
            // rewrite it and clear `target_pending`, and an unrelated copy
            // sharing the Message-ID could be folded into it. Derive the
            // complete mapping from the durable operation queue instead,
            // independent of the page's contents. It is computed inside the
            // write transaction, so it is fresher than the page's mapping (an
            // undo may have moved the row back) and wins where both name the
            // same Graph id.
            let mut adopted_local_ids = adopted_local_ids;
            // The page mapping was captured when the delta was requested, so it
            // predates this transaction. A newer trip may have claimed the row
            // since (an undo that moved it out and back again is the common
            // case), and a claim alone owns the identity slot even before its
            // destination is confirmed. Only the current owner's confirmed
            // destination may still alias the row; every other page entry is
            // historical evidence and must not resolve (or delete) the
            // projection below.
            let current_owners =
                repo::messages::move_target_owners(db, &account_id, &folder_id)?;
            // The current owner's confirmed destination, if it has one. Kept
            // outside the `retain` below so the Message-ID heuristic and the
            // primary-key upsert guard can also tell the current trip's own
            // binding from a stale one.
            let current_destinations: HashMap<String, String> = if current_owners.is_empty() {
                HashMap::new()
            } else {
                repo::messages::move_target_identities(db, &account_id, &folder_id)?
                    .into_iter()
                    .map(|identity| (identity.local_id, identity.remote_id))
                    .collect()
            };
            if !current_owners.is_empty() {
                adopted_local_ids.retain(|remote, local| match current_owners.get(local) {
                    // A claimed row is owned by a current trip. Its own
                    // confirmed destination is the only alias that may stand;
                    // with a claim-only trip there is none, so every page
                    // alias is stale.
                    Some(_) => current_destinations.get(local) == Some(remote),
                    None => true,
                });
            }
            for (remote, local) in confirmed_move_targets(db, &account_id, &folder_id)? {
                // One local row may only be adopted for one destination id.
                // The row's own identity is authoritative, so a page-provided
                // stale alias for the same row is dropped rather than kept
                // alongside it (an `@removed` for the old id would otherwise
                // resolve the projection and delete it).
                adopted_local_ids.retain(|existing_remote, existing_local| {
                    existing_local != &local || existing_remote == &remote
                });
                adopted_local_ids.insert(remote, local);
            }
            // Local rows this batch's confirmed move receipts already own. A
            // pending projection in this set may only be resolved by its
            // adopted Graph id: another copy sharing the Message-ID, or a
            // stale id that happens to equal its primary key, must not consume
            // it.
            let adopted_local_id_set: HashSet<&str> =
                adopted_local_ids.values().map(String::as_str).collect();
            // Same protection as the IMAP metadata path: a delta can still
            // report a message whose permanent delete is queued, and upserting
            // it would undo the optimistic hide.
            let pending_delete_rows: HashSet<String> =
                repo::messages::pending_delete_ids_in_folder(db, &folder_id)?
                    .into_iter()
                    .collect();
            let pending_delete_remote: HashSet<String> = {
                let mut statement = db.prepare(
                    "SELECT gm_msgid FROM message
                      WHERE folder_id = ?1 AND pending_delete = 1 AND gm_msgid IS NOT NULL",
                )?;
                let mut rows = statement.query(rusqlite::params![folder_id])?;
                let mut values = HashSet::new();
                while let Some(row) = rows.next()? {
                    values.insert(row.get::<_, String>(0)?);
                }
                values
            };
            // Resolve every removal to the local row it names before deleting.
            // A confirmed binding keeps the local primary key and mirrors the
            // provider id into `gm_msgid` (`id=m-old, gm_msgid=m-new`), so
            // deleting by the Graph id alone would miss the row while the
            // cursor advanced past the deletion. The local primary key remains
            // the fallback for rows that never went through a binding.
            //
            // The resolved local id is kept together with the Graph id that
            // named it: the row may still be reused by another entry of this
            // same batch, and deleting it then would remove the live
            // replacement. The apply-time check below re-validates every
            // target against the binding it had when it was resolved.
            //
            // The same ids must also suppress this batch's upserts: a page can
            // report a message before it reports its removal (the local row
            // does not exist yet at resolution time), and writing it after the
            // deletion evidence would resurrect it with the cursor advanced
            // past both events.
            let removed_remote_set: HashSet<String> = removed_ids.iter().cloned().collect();
            // `None` is a full-scan prune target (no Graph id names it); the
            // apply-time check only applies to removal events.
            let mut removed_targets: Vec<(String, Option<String>)> = Vec::new();
            for remote in &removed_ids {
                match local_row_for_graph_id(db, &account_id, &folder_id, remote)? {
                    Some((local_id, pending_delete)) => {
                        if pending_delete != 0 {
                            // The row is hidden for an unconfirmed permanent
                            // delete; the queued operation owns its removal.
                            debug!(
                                message = %local_id,
                                remote = %remote,
                                folder = %folder_id,
                                "Graph removal left a row hidden for an unconfirmed permanent delete"
                            );
                            continue;
                        }
                        removed_targets.push((local_id, Some(remote.clone())));
                    }
                    None => {
                        // A confirmed move destination has no `gm_msgid` on its
                        // projection yet, so only the confirmed mapping can
                        // name it. Deleting it here is what lets the cursor
                        // advance safely; without this the removal matches
                        // nothing and the projection is lost with the cursor.
                        match adopted_local_ids.get(remote) {
                            Some(local_id)
                                if is_pending_projection(
                                    db,
                                    &account_id,
                                    &folder_id,
                                    local_id,
                                )? =>
                            {
                                removed_targets.push((local_id.clone(), Some(remote.clone())));
                            }
                            Some(local_id) => {
                                // The pass promised this row, but it is no
                                // longer the projection the confirmation
                                // described (an undo may have moved it back).
                                // Deleting a guess is unsafe, and advancing the
                                // cursor would discard the only evidence that
                                // names it: fail the batch so the next run
                                // replays from the same cursor.
                                return Err(AppError::conflict(format!(
                                    "Graph removal {remote} names a confirmed move projection \
                                     ({local_id}) that is no longer pending in folder {folder_id}"
                                )));
                            }
                            None => {
                                // An id with no local row and no confirmed
                                // identity (already pruned, another folder, or
                                // a copy that never reached the store) is not a
                                // failure: the rest of the batch still applies
                                // and the cursor still advances, but the event
                                // is recorded.
                                debug!(
                                    remote = %remote,
                                    folder = %folder_id,
                                    "Graph removal did not match a local row"
                                );
                            }
                        }
                    }
                }
            }
            // A delta may contain two copies of the same mail (same Message-ID,
            // different Graph ids). Count them before any row is reused.
            let mut header_counts: HashMap<String, usize> = HashMap::new();
            for stored in &messages {
                if let Some(header) = stored.message_id_header.as_deref() {
                    *header_counts.entry(header.to_string()).or_default() += 1;
                }
            }
            for mut stored in messages {
                // Graph, like IMAP, can hand out a new provider id for a
                // message whose content did not change: every draft save
                // deletes the old server copy and APPENDs a new one, and a
                // move/copy also changes the id.  The Message-ID header is
                // stable across that churn, so adopt the row that already
                // represents it in this folder -- the same lookup the IMAP
                // metadata path performs.  Without this the composer's local
                // projection row and the APPEND echo become two rows for one
                // draft.
                let remote_id = stored.summary.id.clone();
                // The same batch removed this Graph id: deletion evidence wins
                // over its upsert, even when the local row did not exist yet
                // when the removals were resolved above. Writing it here would
                // recreate the message the server already deleted.
                if removed_remote_set.contains(&remote_id) {
                    debug!(
                        remote = %remote_id,
                        folder = %folder_id,
                        "Graph batch both upserted and removed this id; skipping the write"
                    );
                    continue;
                }
                // Re-check the projection inside the write: an undo may have
                // moved the row back while the delta request was in flight.
                let adopted_local_id = match adopted_local_ids.get(&remote_id) {
                    Some(local_id)
                        if is_pending_projection(db, &account_id, &folder_id, local_id)? =>
                    {
                        Some(local_id.clone())
                    }
                    _ => None,
                };
                let resolved_by_adoption = adopted_local_id.is_some();
                if let Some(local_id) = adopted_local_id {
                    // The destination of an optimistic move: the row was moved
                    // locally already, so apply the delta onto it. This takes
                    // precedence over the Message-ID heuristic because the
                    // executor confirmed this exact local row.
                    stored.summary.id = local_id;
                } else if let Some((local_id, _)) =
                    local_row_for_graph_id(db, &account_id, &folder_id, &remote_id)?
                {
                    // The row is already bound to this provider id but keeps a
                    // different local primary key. This must come before the
                    // Message-ID heuristic: after a confirmed move or a draft
                    // re-APPEND the stored header may lag behind (or be absent),
                    // so a repeated delivery of the same Graph id would
                    // otherwise insert a second row and advance the cursor.
                    stored.summary.id = local_id;
                } else if let Some(header) = stored.message_id_header.as_deref() {
                    let incoming = header_counts.get(header).copied().unwrap_or(1);
                    if let Some(existing_id) =
                        local_id_for_message_id(db, &account_id, &folder_id, header, incoming)?
                    {
                        // A row already bound to a different Graph id may
                        // only be reused when this batch also removes that
                        // binding: a re-APPENDed draft or a replacement copy
                        // reports the old id removed next to the new one.
                        // Otherwise the row keeps the live binding and this
                        // entry must not be folded into it.
                        //
                        // Refusing that *reuse* is not refusing the *import*:
                        // a copy has its own Graph id but shares the original
                        // Message-ID, so the entry is a legitimate second
                        // message. Leaving `summary.id` as the incoming Graph
                        // id makes the insert below create a row for it.
                        // Skipping the entry instead would lose the copy
                        // forever while the cursor advanced past it. Only a
                        // repeated delivery of the same Graph id (resolved
                        // above by `local_row_for_graph_id`) is a duplicate.
                        //
                        // A row that a current move trip owns (a claim alone or
                        // a confirmed destination) may only be bound by that
                        // trip's own confirmed destination. This incoming
                        // entry is not part of that trip, so reusing the row
                        // would clear `target_pending` and its owner while the
                        // move is still waiting. Import a genuinely new copy
                        // under its own Graph id instead; when the entry names
                        // the row's own primary key there is no separate row to
                        // import, so ignore it and let the trip settle.
                        if current_owners.contains_key(existing_id.as_str()) {
                            if existing_id == remote_id {
                                debug!(
                                    message = %existing_id,
                                    remote = %remote_id,
                                    folder = %folder_id,
                                    "Graph update names a row owned by its current move trip; ignored"
                                );
                                continue;
                            }
                            debug!(
                                message = %existing_id,
                                remote = %remote_id,
                                folder = %folder_id,
                                "Graph message shares a Message-ID with a row owned by its current move trip; importing it as a new copy"
                            );
                        } else if adopted_local_id_set.contains(existing_id.as_str())
                            && is_pending_projection(
                                db,
                                &account_id,
                                &folder_id,
                                &existing_id,
                            )?
                        {
                            debug!(
                                message = %existing_id,
                                remote = %remote_id,
                                folder = %folder_id,
                                "Graph message shares a Message-ID with a pending projection owned by a confirmed target; importing it as a new copy"
                            );
                        } else {
                            match bound_graph_id(db, &account_id, &folder_id, &existing_id)? {
                                Some(bound)
                                    if bound != remote_id
                                        && !removed_remote_set.contains(&bound) =>
                                {
                                    debug!(
                                        message = %existing_id,
                                        remote = %remote_id,
                                        bound = %bound,
                                        folder = %folder_id,
                                        "Graph message shares a Message-ID with a row bound to another live id; importing it as a new copy"
                                    );
                                }
                                _ => stored.summary.id = existing_id,
                            }
                        }
                    }
                }
                // The service layer needs a UID to group queue entries; derive
                // it from the row id that is actually used so it stays stable
                // when a re-APPEND changes the Graph id of a draft.
                if stored.uid.is_none() {
                    stored.uid = Some(synthetic_uid(&stored.summary.id));
                }
                // The primary-key fallback needs the same guard as the
                // deletion path. When no identity lookup resolved this entry,
                // `stored.summary.id` is still the raw Graph id, and the
                // upsert below would hit `id = remote_id` by primary key even
                // though that row was re-bound to another Graph id
                // (`id=m-old, gm_msgid=m-new`). `ON CONFLICT(id)` would then
                // overwrite the live binding with the superseded id. Only a
                // row that is unbound or still bound to this exact id may
                // receive the write; a stale id is ignored.
                if stored.summary.id == remote_id {
                    if let Some(bound) =
                        bound_graph_id(db, &account_id, &folder_id, &stored.summary.id)?
                    {
                        if bound != remote_id {
                            debug!(
                                message = %stored.summary.id,
                                remote = %remote_id,
                                bound = %bound,
                                folder = %folder_id,
                                "Graph update names a row re-bound to another id; ignored"
                            );
                            continue;
                        }
                    }
                    // The row at this primary key may be an optimistic move
                    // projection that a confirmed receipt in this batch already
                    // owns (`adopted_local_ids`). Its key is the id it had in
                    // the folder it is moving back into, so this stale entry
                    // must not bind it back and clear `target_pending`: only the
                    // adopted Graph id may resolve it. `resolved_by_adoption`
                    // keeps a legitimate adoption that happens to share the
                    // key from being rejected here.
                    if !resolved_by_adoption
                        && (adopted_local_id_set.contains(stored.summary.id.as_str())
                            || current_owners.contains_key(stored.summary.id.as_str()))
                        && is_pending_projection(
                            db,
                            &account_id,
                            &folder_id,
                            &stored.summary.id,
                        )?
                    {
                        debug!(
                            message = %stored.summary.id,
                            remote = %remote_id,
                            folder = %folder_id,
                            "Graph update names a pending move projection owned by another trip; ignored"
                        );
                        continue;
                    }
                }
                // Keep the upsert strictly inside this folder. When the entry
                // still carries the raw Graph id and that id already belongs
                // to a row in another folder, the row is an optimistic move
                // projection (or a move this account already applied): the
                // primary-key upsert would rewrite its `folder_id` and clear
                // `target_pending`. Hand the update to the folder that owns
                // the row instead.
                if let Some((owner_folder, target_pending)) =
                    foreign_folder_row(db, &account_id, &folder_id, &stored.summary.id)?
                {
                    debug!(
                        message = %stored.summary.id,
                        remote = %remote_id,
                        folder = %folder_id,
                        owner = %owner_folder,
                        target_pending,
                        "Graph update names a row owned by another folder; ignored here"
                    );
                    continue;
                }
                if pending_delete_rows.contains(&stored.summary.id)
                    || pending_delete_remote.contains(&remote_id)
                {
                    debug!(
                        message = %stored.summary.id,
                        folder = %folder_id,
                        "Graph sync left a row hidden for an unconfirmed permanent delete"
                    );
                    continue;
                }
                // Graph has no per-message answered/forwarded flag; both are
                // local-only (set when the user replies or forwards). Carry
                // the stored values into this upsert so a delta pass cannot
                // reset them. IMAP is unaffected: FETCH returns real
                // `\Answered`/`\Forwarded` flags and its path keeps calling
                // `repo::messages::upsert` directly.
                let existed = preserve_local_flags(db, &mut stored)?;
                // Adopt a local ancestor conversation when one already exists,
                // exactly like the IMAP metadata path does.
                resolve_thread_id(db, &account_id, &mut stored)?;
                repo::messages::upsert(db, &stored)?;
                // Persist the uid -> Graph id mapping independently of the
                // message row.  Optimistic actions move or even delete the row
                // before the queued operation runs (permanent delete removes
                // it outright), so the executor cannot always recover the
                // Graph id from `message`.  The value must stay the Graph id,
                // never the local row id: the executor sends it back to
                // Microsoft, and the two differ after the reuse above.
                if let Some(uid) = stored.uid {
                    let key = format!("graph_remote:{folder_id}:{uid}");
                    repo::sync_state::set(db, &account_id, "mail", &key, &remote_id)?;
                }
                if existed {
                    updated.push(stored.summary.id.clone());
                } else {
                    inserted.push(stored.summary.clone());
                }
            }

            if initial {
                // A completed scan is authoritative for every synced row in
                // the folder, including rows left over from the IMAP era.
                removed_targets.extend(
                    graph_prune_ids(
                        db,
                        &account_id,
                        &folder_id,
                        &seen_ids,
                        sync_window_days,
                    )?
                    .into_iter()
                    .map(|id| (id, None)),
                );
            }
            // Re-validate every removal at apply time. A row resolved before
            // the upserts above may have been reused by another entry of this
            // same batch (a re-APPENDed draft or a replacement copy under a
            // new Graph id). Deleting it now would drop the live replacement
            // while the server still holds it. Only a row that still exists
            // and is still bound to the removed Graph id (or never bound,
            // NULL) may be deleted; a row now bound to a different id is the
            // replacement and stays.
            let mut removed_local_ids: Vec<String> = Vec::new();
            for (local_id, remote) in removed_targets {
                let Some(remote) = remote else {
                    // Full-scan prune: no Graph id names it and it was derived
                    // from the post-upsert state, so it is safe to delete.
                    removed_local_ids.push(local_id);
                    continue;
                };
                let binding: Option<Option<String>> = db
                    .query_row(
                        "SELECT gm_msgid FROM message
                          WHERE id = ?1 AND account_id = ?2 AND folder_id = ?3",
                        rusqlite::params![local_id, &account_id, &folder_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                match binding {
                    Some(Some(bound)) if bound != remote => {
                        debug!(
                            message = %local_id,
                            removed = %remote,
                            bound = %bound,
                            folder = %folder_id,
                            "Graph removal skipped: the row was reused by another id in this batch"
                        );
                    }
                    Some(_) => removed_local_ids.push(local_id),
                    None => {
                        debug!(
                            message = %local_id,
                            removed = %remote,
                            folder = %folder_id,
                            "Graph removal target no longer exists"
                        );
                    }
                }
            }
            removed_local_ids.sort();
            removed_local_ids.dedup();
            if !removed_local_ids.is_empty() {
                // Drop the uid mapping together with the row.  The mapping is
                // keyed by that row's uid and a later sync derives a fresh uid
                // for a new row, so a stale entry could only mislead.
                for id in &removed_local_ids {
                    let uid: Option<Option<i64>> = db
                        .query_row(
                            "SELECT uid FROM message WHERE id = ?1",
                            rusqlite::params![id],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if let Some(Some(uid)) = uid {
                        let key = format!("graph_remote:{folder_id}:{uid}");
                        repo::sync_state::delete_key(db, &account_id, "mail", &key)?;
                    }
                }
                repo::messages::delete_local(db, &removed_local_ids, true)?;
            }
            repo::folders::set_last_sync_at(db, &resync_folder, Utc::now())?;
            repo::folders::recompute_counts(db, &resync_folder)?;
            // Heal rows earlier builds stored without subject/from even
            // though their raw headers carried both.
            crate::graph::messages::heal_missing_metadata(db, &account_id)?;
            // Same transaction as the messages above. If any write fails, the
            // cursor is rolled back with them, so a retry replays this batch
            // instead of resuming past changes that were never stored.
            crate::graph::messages::persist_cursor_update(
                db,
                &account_id,
                &folder_id,
                delta_link.as_deref(),
                &pending_cursor,
            )?;
            Ok((inserted, updated, removed_local_ids))
        })
        .await?;

    let inserted_ids: Vec<String> = inserted.iter().map(|item| item.id.clone()).collect();
    if !inserted_ids.is_empty() {
        events::emit_mail_new(
            ctx.events.as_ref(),
            account.id.clone(),
            folder.id.clone(),
            inserted_ids,
        );
    }
    if !updated.is_empty() {
        events::emit_mail_changed(
            ctx.events.as_ref(),
            account.id.clone(),
            vec![folder.id.clone()],
            updated,
            events::MailChangeKind::Updated,
        );
    }
    let deleted_ids = removed_ids.clone();
    if !removed_ids.is_empty() {
        events::emit_mail_changed(
            ctx.events.as_ref(),
            account.id.clone(),
            vec![folder.id.clone()],
            removed_ids,
            events::MailChangeKind::Deleted,
        );
    }

    debug!(
        account = %account.id,
        folder = %folder.remote_path,
        inserted = inserted.len(),
        initial,
        "Graph message sync applied"
    );

    Ok(SyncOutcome {
        new_messages: inserted,
        deleted_ids,
        initial,
        folder_id: folder.id.clone(),
    })
}

/// Deterministic synthetic UID for a Graph message.
///
/// Graph ids are opaque strings; the local queue keys flag/move operations by
/// `(folder_id, uid)`.  A v5 UUID of the Graph id gives a stable positive
/// 63-bit integer: stable across restarts (unlike `DefaultHasher`, whose keys
/// are randomised per process) and effectively collision-free for one folder.
pub fn synthetic_uid(remote_id: &str) -> i64 {
    let uuid = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, remote_id.as_bytes());
    let mut value = 0u64;
    for byte in &uuid.as_bytes()[..8] {
        value = (value << 8) | u64::from(*byte);
    }
    // Keep the sign bit clear so the value is a normal positive UID; 0 is
    // reserved so it can never look like "no uid" in a numeric check.
    let value = value & (i64::MAX as u64);
    if value == 0 {
        1
    } else {
        value as i64
    }
}

/// Carry the local-only flags of an existing row into a Graph upsert.
///
/// Microsoft Graph exposes no per-message `answered` or `forwarded` state, so
/// [`crate::graph::messages::map_message`] always maps them as `false`. The
/// columns are set locally when the user replies or forwards
/// (`service::compose`), and `repo::messages::upsert` deliberately takes the
/// incoming value for them. Without this step the next delta pass would erase
/// those flags.
///
/// This lives in the Graph bridge on purpose. The IMAP path gets real
/// `\Answered` and `\Forwarded` flags from FETCH, so it must keep writing the
/// fetched values through `repo::messages::upsert` unchanged.
///
/// Returns whether the row already existed, which the caller uses to classify
/// the event (insert vs update).
fn preserve_local_flags(
    conn: &rusqlite::Connection,
    stored: &mut nuncio_core::models::StoredMessage,
) -> Result<bool> {
    let existing: Option<(i64, i64)> = conn
        .query_row(
            "SELECT answered, forwarded FROM message WHERE id = ?1",
            rusqlite::params![stored.summary.id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match existing {
        Some((answered, forwarded)) => {
            stored.summary.answered = answered != 0;
            stored.summary.forwarded = forwarded != 0;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Local row id already representing `message_id_header` in `folder_id`.
///
/// Provider ids are not stable (Graph changes a message's id when a draft is
/// re-APPENDed or a message is copied/moved) while the Message-ID is, so the
/// sync updates the existing row instead of inserting a second one.  This is
/// the same lookup the IMAP metadata path performs before its upsert.
fn local_id_for_message_id(
    conn: &rusqlite::Connection,
    account_id: &str,
    folder_id: &str,
    header: &str,
    incoming_count: usize,
) -> Result<Option<String>> {
    // Copies share a Message-ID but have distinct provider ids. Only a single
    // candidate on each side may be folded into one row.
    repo::messages::unambiguous_message_id_row(conn, account_id, folder_id, header, incoming_count)
}

/// Local row in `folder_id` already bound to a Graph id.
///
/// A confirmed move or a draft re-APPEND reuses the local row and mirrors the
/// provider id into `gm_msgid`, so the row key and the remote id differ
/// (`id=m-old, gm_msgid=m-new`). Every Graph delta entry and `@removed` event
/// must resolve that binding first: the Message-ID heuristic is not reliable
/// after the provider rewrote the row's headers, and deleting by the Graph id
/// alone would miss the row while the cursor still advanced. The local primary
/// key is the fallback only for rows that were **never rebound**: an unbound
/// `gm_msgid` (NULL) or one that still equals `remote_id`. A row already bound
/// to a different Graph id is not a match — a late `@removed` carrying the old
/// id must not delete the message now bound to the new one. The lookup stays
/// inside this account and folder so a coincidental id elsewhere is never
/// touched.
///
/// An optimistic move projection (`uid IS NULL AND target_pending = 1`) is
/// never resolved by that primary-key fallback. It has no identity yet, and
/// after a move back into its original folder its primary key is the *old*
/// Graph id of that folder: trusting the key would let a removal delete the
/// projection, or an update bind it back to the id the move is leaving (and
/// clear `target_pending`) while the cursor advances. Only the current
/// operation's adopted mapping may resolve such a row (see
/// [`apply_graph_delta`]).
///
/// Returns the row id and its `pending_delete` flag. An ambiguous binding (more
/// than one row carrying the same provider id) is not resolved: acting on a
/// guess could update or delete the wrong message.
fn local_row_for_graph_id(
    conn: &rusqlite::Connection,
    account_id: &str,
    folder_id: &str,
    remote_id: &str,
) -> Result<Option<(String, i64)>> {
    let mut statement = conn.prepare(
        "SELECT id, pending_delete FROM message
          WHERE account_id = ?1 AND folder_id = ?2 AND gm_msgid = ?3",
    )?;
    let mut rows = statement.query(rusqlite::params![account_id, folder_id, remote_id])?;
    let mut candidates: Vec<(String, i64)> = Vec::new();
    while let Some(row) = rows.next()? {
        candidates.push((row.get(0)?, row.get(1)?));
    }
    match candidates.len() {
        1 => return Ok(candidates.pop()),
        0 => {}
        _ => return Ok(None),
    }
    // A row whose primary key happens to equal `remote_id` but is bound to a
    // different Graph id has been re-bound (`id=m-old, gm_msgid=m-new`). The
    // old id is stale evidence: resolving it here would delete the live
    // message. A pending move projection is excluded too: its identity is not
    // decided yet and the key may be the id it had before it left this folder,
    // so only the current operation's adopted mapping may name it.
    Ok(conn
        .query_row(
            "SELECT id, pending_delete FROM message
              WHERE id = ?1 AND account_id = ?2 AND folder_id = ?3
                AND (gm_msgid IS NULL OR gm_msgid = ?1)
                AND NOT (uid IS NULL AND target_pending = 1)",
            rusqlite::params![remote_id, account_id, folder_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

/// `gm_msgid` of the local row `id` in `folder_id`, when it is set.
///
/// The upsert writer uses this for its primary-key fallback: a row already
/// bound to a *different* Graph id has been re-bound by an earlier delta (a
/// draft re-APPEND or a provider-side id change) and must not receive an entry
/// that still carries the superseded id. `None` means either the row does not
/// exist or its `gm_msgid` is NULL, both of which are safe to write.
fn bound_graph_id(
    conn: &rusqlite::Connection,
    account_id: &str,
    folder_id: &str,
    id: &str,
) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT gm_msgid FROM message
              WHERE id = ?1 AND account_id = ?2 AND folder_id = ?3",
            rusqlite::params![id, account_id, folder_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// Folder and `target_pending` of a row with `id` that lives in a *different*
/// folder of the same account.
///
/// Graph ids are mailbox-wide, and an optimistic move keeps the local primary
/// key while changing `folder_id`. A late delta entry for the source folder
/// therefore names a row the destination folder now owns. Upserting it would
/// pull the projection back to the source and clear `target_pending`, so the
/// writer treats it as foreign and ignores it here; only the destination
/// folder (or its adopted projection) may update it.
fn foreign_folder_row(
    conn: &rusqlite::Connection,
    account_id: &str,
    folder_id: &str,
    id: &str,
) -> Result<Option<(String, i64)>> {
    Ok(conn
        .query_row(
            "SELECT folder_id, target_pending FROM message
              WHERE id = ?1 AND account_id = ?2",
            rusqlite::params![id, account_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .filter(|(existing, _)| existing != folder_id))
}

/// Whether `id` is still an optimistic-move projection waiting for its
/// destination identity in `folder_id`.
///
/// The confirmed destination was resolved before the delta request; an undo
/// can move the row back while that request is in flight, so the writer
/// re-checks instead of rebinding a row that is no longer this operation's.
fn is_pending_projection(
    conn: &rusqlite::Connection,
    account_id: &str,
    folder_id: &str,
    id: &str,
) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM message
              WHERE id = ?1 AND account_id = ?2 AND folder_id = ?3
                AND uid IS NULL AND target_pending = 1)",
        rusqlite::params![id, account_id, folder_id],
        |row| row.get(0),
    )?)
}

/// Confirmed move destinations whose projection is still pending in
/// `folder_id`.
///
/// [`crate::graph::messages::MessageDelta::adopted_local_ids`] only names the
/// Graph ids the delta page returned. A confirmed destination that has not
/// reached the page yet has no entry there, so an update or removal carrying
/// the row's *old* id could bind (or delete) the projection and clear
/// `target_pending`, and a different copy sharing the Message-ID could be
/// folded into it.
///
/// The projection row itself is the primary source: the executor writes the
/// confirmed destination *and its owner* onto it, so the identity survives
/// pruning of the operation queue. Operation payloads are a supplement for
/// receipts recorded before that column existed. A retained receipt may
/// describe an earlier trip of the same row, so it is accepted only when it is
/// the identity's owner (or, for rows written before ownership was recorded,
/// when it is the newest operation that moved the row into this folder)
/// ([`crate::graph::messages::latest_owner`]), the row is still an unbound
/// projection here, and the row does not already carry a different confirmed
/// identity. Those re-checks also drop a mapping whose row an undo already
/// moved back.
fn confirmed_move_targets(
    db: &rusqlite::Connection,
    account_id: &str,
    folder_id: &str,
) -> Result<HashMap<String, String>> {
    let row_identities = repo::messages::move_target_identities(db, account_id, folder_id)?;
    let mut targets: HashMap<String, String> = HashMap::new();
    // One local row may only be adopted for one destination id; the row's own
    // identity always wins over a retained receipt.
    let mut adopted_by_message: HashMap<String, String> = HashMap::new();
    let mut row_owner: HashMap<String, (String, Option<i64>)> = HashMap::new();
    for identity in &row_identities {
        targets
            .entry(identity.remote_id.clone())
            .or_insert_with(|| identity.local_id.clone());
        adopted_by_message
            .entry(identity.local_id.clone())
            .or_insert_with(|| identity.remote_id.clone());
        if let Some(owner) = identity.owner_op_id.clone() {
            row_owner.insert(identity.local_id.clone(), (owner, identity.owner_sequence));
        }
    }
    // Claims without a destination yet still own the row: a retained receipt
    // from another operation is historical and must not resolve it.
    for (message_id, owner) in repo::messages::move_target_owners(db, account_id, folder_id)? {
        row_owner.insert(message_id, owner);
    }
    let scopes = crate::graph::messages::move_operation_scopes(db, account_id)?;
    let latest_owner = crate::graph::messages::latest_owner(&scopes, folder_id);
    for scope in &scopes {
        for outcome in &scope.outcomes {
            if outcome.state != OpMessageState::Confirmed
                || outcome.target_folder_id.as_deref() != Some(folder_id)
            {
                continue;
            }
            let Some(remote) = outcome
                .target_remote_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            // The recorded owner is authoritative. Only its receipt may
            // contribute; a historical operation must not back another
            // destination for the same row.
            if let Some((owner_id, _)) = row_owner.get(&outcome.message_id) {
                if owner_id != &scope.id {
                    continue;
                }
            } else if !adopted_by_message.contains_key(&outcome.message_id)
                && latest_owner.get(&outcome.message_id).copied() != Some(scope.sequence)
            {
                continue;
            }
            if !is_pending_projection(db, account_id, folder_id, &outcome.message_id)? {
                continue;
            }
            // The row already carries the identity of its *current* pending
            // projection. An operation receipt from an earlier trip must not
            // add a second, stale destination for the same row.
            if let Some(existing) = adopted_by_message.get(&outcome.message_id) {
                if existing != remote {
                    continue;
                }
            }
            targets
                .entry(remote.to_string())
                .or_insert_with(|| outcome.message_id.clone());
            adopted_by_message
                .entry(outcome.message_id.clone())
                .or_insert_with(|| remote.to_string());
        }
    }
    Ok(targets)
}

/// Local ids a completed Graph full scan must delete.
///
/// The scan is authoritative for every row that has a remote counterpart, but
/// rows without a `uid` are never candidates:
///
/// * a draft projection written by `compose_save` before its APPEND ran (the
///   composer owns that row and deletes it on send/discard);
/// * a row that was never synced at all.
///
/// Everything else is compared against the Graph ids the scan returned.  That
/// deliberately includes rows from a previous IMAP sync, which carry a real
/// `uid` but a NULL `gm_msgid`: they can never match a Graph id and must be
/// removed, otherwise switching a Microsoft account from IMAP to Graph leaves
/// every message listed twice.  A projection that was already synced but is
/// being re-APPENDed (a pending `appendDraft` operation exists) is kept,
/// because the disappearance of its old remote copy is expected and the new
/// copy has not necessarily been echoed yet.
///
/// `sync_window_days` matters because a Graph full scan only stores rows inside
/// that window (the window is applied locally, not as a `$filter`): a row older
/// than the window is simply absent from the returned set, so it cannot be
/// treated as deleted.  Only rows clearly inside the window (one extra day of
/// slack for clock-skew) are judged.
fn graph_prune_ids(
    conn: &rusqlite::Connection,
    account_id: &str,
    folder_id: &str,
    seen_ids: &HashSet<String>,
    sync_window_days: u32,
) -> Result<Vec<String>> {
    let cutoff = (sync_window_days > 0)
        .then(|| Utc::now() - ChronoDuration::days(i64::from(sync_window_days).saturating_add(1)));
    let awaiting_append = pending_append_draft_ids(conn, account_id)?;
    // Rows hidden for an unconfirmed permanent delete are not pruned: their
    // absence from the scan may simply mean the server already applied the
    // queued delete, and the executor still has to confirm it.
    let mut statement = conn.prepare(
        "SELECT id, gm_msgid, is_draft, message_id_header, date FROM message
         WHERE folder_id = ?1 AND uid IS NOT NULL AND pending_delete = 0",
    )?;
    let mut rows = statement.query(rusqlite::params![folder_id])?;
    let mut removed = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let remote_id: Option<String> = row.get(1)?;
        if remote_id
            .as_deref()
            .is_some_and(|remote| seen_ids.contains(remote))
        {
            continue;
        }
        if let Some(cutoff) = cutoff {
            let date: String = row.get(4)?;
            let in_window = DateTime::parse_from_rfc3339(&date)
                .map(|date| date.with_timezone(&Utc) >= cutoff)
                .unwrap_or(false);
            if !in_window {
                continue;
            }
        }
        let is_draft: i64 = row.get(2)?;
        let header: Option<String> = row.get(3)?;
        let awaiting = is_draft != 0
            && header
                .as_deref()
                .and_then(crate::util::draft_id_from_header)
                .is_some_and(|draft_id| awaiting_append.contains(draft_id));
        if !awaiting {
            removed.push(id);
        }
    }
    Ok(removed)
}

/// Draft ids whose server APPEND is still queued or running.
///
/// `compose_save` writes the projection row before it queues the APPEND, so a
/// pending operation is the durable marker that the row's missing (or
/// soon-to-be-replaced) server copy is temporary rather than a remote deletion
/// to mirror locally.
fn pending_append_draft_ids(
    conn: &rusqlite::Connection,
    account_id: &str,
) -> Result<HashSet<String>> {
    let mut result = HashSet::new();
    for op in repo::pending_ops::list(conn, Some(account_id))? {
        if op.kind != crate::ops::KIND_APPEND_DRAFT
            || !matches!(
                op.state,
                PendingOpState::Pending | PendingOpState::InProgress
            )
        {
            continue;
        }
        if let Ok(payload) = repo::pending_ops::payload::<crate::ops::AppendDraftOp>(&op) {
            result.insert(payload.draft_id);
        }
    }
    Ok(result)
}

async fn initial_sync(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    uidvalidity: Option<i64>,
    uidnext: Option<i64>,
    outcome: &mut SyncOutcome,
) -> Result<()> {
    let mut uids = search_uids(conn, account.sync_window_days).await?;
    uids.sort_unstable_by(|a, b| b.cmp(a)); // newest first
    let total = uids.len() as u64;

    ctx.sync_status
        .begin(&account.id, SyncModule::Mail, Some(total.max(1)));

    let mut done = 0u64;
    for chunk in uids.chunks(BATCH_SIZE) {
        let metas = fetch_metadata(conn, account, folder, uidvalidity, chunk).await?;
        let new = persist_metadata(ctx, conn, account, folder, metas, true).await?;
        let ids: Vec<String> = new.iter().map(|item| item.id.clone()).collect();
        if !ids.is_empty() {
            events::emit_mail_new(
                ctx.events.as_ref(),
                account.id.clone(),
                folder.id.clone(),
                ids,
            );
        }
        outcome.new_messages.extend(new);
        done += chunk.len() as u64;
        ctx.sync_status
            .progress(&account.id, SyncModule::Mail, done, total.max(1));
        debug!(
            folder = %folder.remote_path,
            done, total, "initial sync batch stored"
        );
    }

    // Record the server's uidnext even if the folder was empty.
    if uidnext.is_none() && total == 0 {
        debug!(folder = %folder.remote_path, "initial sync: empty folder");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn incremental_sync(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    uidvalidity: Option<i64>,
    uidnext: Option<i64>,
    stored_modseq: Option<i64>,
    outcome: &mut SyncOutcome,
) -> Result<()> {
    let full_reconcile = needs_full_reconcile(ctx, &account.id, &folder.id).await?;

    if conn.caps.condstore && !full_reconcile && stored_modseq.is_some() {
        if let Some(modseq) = stored_modseq {
            match fetch_changed_flags(conn, modseq as u64).await {
                Ok(changes) => {
                    apply_flag_updates(ctx, folder, &changes).await?;
                }
                Err(error) => {
                    warn!(%error, folder = %folder.remote_path, "CHANGEDSINCE failed; falling back to UID diff");
                    let changes = fetch_all_flags(conn, None).await?;
                    apply_flag_updates(ctx, folder, &changes).await?;
                }
            }
        }
    } else {
        let changes = fetch_all_flags(conn, None).await?;
        apply_flag_updates(ctx, folder, &changes).await?;
        mark_full_reconcile(ctx, &account.id, &folder.id).await?;
    }

    // New messages (do not apply the sync window here: any new UID is recent).
    if let Some(uidnext) = uidnext {
        let start = uidnext.max(1);
        let uids = uid_range(conn, start).await?;
        if !uids.is_empty() {
            for chunk in uids.chunks(BATCH_SIZE) {
                let metas = fetch_metadata(conn, account, folder, uidvalidity, chunk).await?;
                let new = persist_metadata(ctx, conn, account, folder, metas, false).await?;
                let ids: Vec<String> = new.iter().map(|item| item.id.clone()).collect();
                if !ids.is_empty() {
                    events::emit_mail_new(
                        ctx.events.as_ref(),
                        account.id.clone(),
                        folder.id.clone(),
                        ids,
                    );
                }
                outcome.new_messages.extend(new);
            }
        }
    }

    // Expunge detection: compare every server UID with the local set.
    let server_uids: HashSet<i64> = search_all_uids(conn)
        .await?
        .into_iter()
        .map(|uid| uid as i64)
        .collect();
    let local_uids = ctx
        .db
        .read({
            let folder_id = folder.id.clone();
            move |db| local_uids(db, &folder_id)
        })
        .await?;
    let mut removed_ids: Vec<String> = Vec::new();
    for (id, uid) in local_uids {
        if !server_uids.contains(&uid) {
            removed_ids.push(id);
        }
    }
    if !removed_ids.is_empty() {
        let ids = removed_ids.clone();
        ctx.db
            .write(move |db| repo::messages::delete_local(db, &ids, true))
            .await?;
        events::emit_mail_changed(
            ctx.events.as_ref(),
            account.id.clone(),
            vec![folder.id.clone()],
            removed_ids.clone(),
            events::MailChangeKind::Deleted,
        );
        outcome.deleted_ids.extend(removed_ids);
    }

    Ok(())
}

/// First UID range width tried by [`older_uid_batch`]; widened x4 while the
/// mailbox is sparse so a big folder never costs a full `UID SEARCH ALL`.
const OLDER_RANGE_START: u32 = 1000;

/// Load the next batch (up to [`LOAD_OLDER_BATCH_SIZE`]) of messages OLDER than
/// anything stored for an IMAP folder, ignoring the sync window.
///
/// The cursor is derived, not stored: it is the minimum UID already in the
/// local database for the folder (or the server `UIDNEXT` when the window
/// excluded everything). Rows go through the same `fetch_metadata` /
/// `persist_metadata` pair as the initial sync (`BODY.PEEK`, so no message is
/// ever marked read). Folder sync flags (UIDVALIDITY/UIDNEXT/MODSEQ) are not
/// touched, so the incremental sync is unaffected.
pub async fn load_older_imap(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
) -> Result<nuncio_core::models::LoadOlderResult> {
    use nuncio_core::models::LoadOlderResult;
    if !folder.is_selectable {
        return Ok(LoadOlderResult::default());
    }
    let (stored_validity, stored_uidnext, _) = ctx
        .db
        .read({
            let folder_id = folder.id.clone();
            move |db| repo::folders::sync_flags(db, &folder_id)
        })
        .await?;
    let mailbox = conn.select(&folder.remote_path).await.map_err(|error| {
        if error.code() == "protocol" {
            AppError::not_found(format!("mailbox {} is not available", folder.remote_path))
        } else {
            error
        }
    })?;
    let current_validity = mailbox.uid_validity.map(|value| value as i64);
    if let (Some(stored), Some(current)) = (stored_validity, current_validity) {
        if stored != current {
            // Nothing is cleared here: the next regular sync owns that reset.
            return Err(AppError::conflict(
                "UIDVALIDITY changed on the server; sync the folder before loading older mail",
            ));
        }
    }

    let min_uid: Option<i64> = ctx
        .db
        .read({
            let folder_id = folder.id.clone();
            move |db| {
                Ok(db.query_row(
                    "SELECT MIN(uid) FROM message WHERE folder_id = ?1 AND uid IS NOT NULL",
                    rusqlite::params![folder_id],
                    |row| row.get::<_, Option<i64>>(0),
                )?)
            }
        })
        .await?;
    // Exclusive upper bound: everything strictly below it is "older".
    // The cursor is the lowest UID we already hold; it needs no stored UIDNEXT
    // (163 does not report one). With nothing stored the server's UIDNEXT, or a
    // previously stored one, bounds the first batch; with none of those the
    // folder simply has not been synced.
    let Some(upper) = min_uid
        .or(mailbox.uid_next.map(|value| value as i64))
        .or(stored_uidnext)
    else {
        return Err(AppError::conflict(
            "the folder has not been synced yet; sync it before loading older mail",
        ));
    };
    if upper <= 1 {
        return Ok(LoadOlderResult::default());
    }
    let upper = upper.min(i64::from(u32::MAX)) as u32;

    let (uids, has_more) = older_uid_batch(conn, upper - 1).await?;
    if uids.is_empty() {
        return Ok(LoadOlderResult {
            loaded: 0,
            has_more: false,
        });
    }
    let metas = fetch_metadata(conn, account, folder, current_validity, &uids).await?;
    let new = persist_metadata(ctx, conn, account, folder, metas, true).await?;
    let ids: Vec<String> = new.iter().map(|item| item.id.clone()).collect();
    if !ids.is_empty() {
        events::emit_mail_new(
            ctx.events.as_ref(),
            account.id.clone(),
            folder.id.clone(),
            ids,
        );
    }
    let folder_id = folder.id.clone();
    ctx.db
        .write(move |db| repo::folders::recompute_counts(db, &folder_id))
        .await?;
    debug!(
        folder = %folder.remote_path,
        loaded = new.len(),
        has_more,
        "older mail loaded"
    );
    Ok(LoadOlderResult {
        loaded: new.len() as u32,
        has_more,
    })
}

/// Up to [`LOAD_OLDER_BATCH_SIZE`] UIDs at or below `hi`, newest first, plus
/// whether more exist below them. Uses bounded `UID SEARCH UID lo:hi` ranges
/// that widen x4 until the batch is full (one surplus UID proves `has_more`) or
/// UID 1 is reached (an exhaustive answer), so exhaustion is exact without ever
/// listing a large mailbox.
async fn older_uid_batch(conn: &mut ImapConnection, hi: u32) -> Result<(Vec<u32>, bool)> {
    let mut width = OLDER_RANGE_START;
    loop {
        let lo = hi.saturating_sub(width - 1).max(1);
        let query = format!("UID {lo}:{hi}");
        let mut found: Vec<u32> = conn.uid_search(&query).await?.into_iter().collect();
        if found.len() > LOAD_OLDER_BATCH_SIZE || lo == 1 {
            found.sort_unstable_by(|a, b| b.cmp(a));
            let has_more = found.len() > LOAD_OLDER_BATCH_SIZE;
            found.truncate(LOAD_OLDER_BATCH_SIZE);
            return Ok((found, has_more));
        }
        width = width.saturating_mul(4);
    }
}

/// Microsoft Graph counterpart of [`load_older_imap`]: one batch of messages
/// received at or before the oldest locally stored one, persisted through the
/// same writer as [`sync_folder_graph`].
pub async fn load_older_graph(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    folder: &Folder,
) -> Result<nuncio_core::models::LoadOlderResult> {
    load_older_graph_with(ctx, &crate::graph::GraphMailClient::new(), account, folder).await
}

/// [`load_older_graph`] with an explicit client (loopback mock in tests).
pub async fn load_older_graph_with(
    ctx: &CoreContext,
    client: &crate::graph::GraphMailClient,
    account: &nuncio_core::models::Account,
    folder: &Folder,
) -> Result<nuncio_core::models::LoadOlderResult> {
    use nuncio_core::models::LoadOlderResult;
    if !folder.is_selectable {
        return Ok(LoadOlderResult::default());
    }
    let graph_path = crate::sync::folders::graph_folder_id(ctx, &account.id, &folder.remote_path)
        .await?
        .ok_or_else(|| {
            AppError::not_found(format!(
                "Graph folder id is unknown for {}; refresh folders first",
                folder.remote_path
            ))
        })?;
    let mut graph_folder = folder.clone();
    graph_folder.remote_path = graph_path;

    let oldest: Option<String> = ctx
        .db
        .read({
            let folder_id = folder.id.clone();
            move |db| {
                Ok(db
                    .query_row(
                        "SELECT date FROM message WHERE folder_id = ?1 AND uid IS NOT NULL
                         ORDER BY date ASC LIMIT 1",
                        rusqlite::params![folder_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?)
            }
        })
        .await?;
    let before = oldest
        .and_then(|date| DateTime::parse_from_rfc3339(&date).ok())
        .map(|date| date.with_timezone(&Utc));

    let page =
        crate::graph::messages::list_older(client, ctx, &account.id, &graph_folder, before).await?;
    let has_more = page.has_more;
    let delta = crate::graph::messages::MessageDelta {
        messages: page.messages,
        delta_link: None,
        // Loading older mail must not disturb delta pagination state.
        pending_cursor: crate::graph::messages::PendingCursor::Keep,
        adopted_local_ids: page.adopted_local_ids,
        full_sync: false,
        removed_remote_ids: Vec::new(),
    };
    let outcome = apply_graph_delta(ctx, account, folder, delta).await?;
    Ok(LoadOlderResult {
        loaded: outcome.new_messages.len() as u32,
        has_more,
    })
}

async fn search_uids(conn: &mut ImapConnection, window_days: u32) -> Result<Vec<u32>> {
    let criteria = if window_days == 0 {
        "ALL".to_string()
    } else {
        let since = Utc::now() - ChronoDuration::days(window_days as i64);
        format!("SINCE {}", imap_date(since))
    };
    conn.uid_search(&criteria)
        .await
        .map(|set| set.into_iter().collect())
}

async fn search_all_uids(conn: &mut ImapConnection) -> Result<Vec<u32>> {
    conn.uid_search("ALL")
        .await
        .map(|set| set.into_iter().collect())
}

async fn uid_range(conn: &mut ImapConnection, start: i64) -> Result<Vec<u32>> {
    let query = format!("UID {}:*", start.max(1));
    conn.uid_search(&query)
        .await
        .map(|set| set.into_iter().collect())
}

struct FlagChange {
    uid: i64,
    flags: FlagSnapshot,
}

async fn fetch_changed_flags(conn: &mut ImapConnection, modseq: u64) -> Result<Vec<FlagChange>> {
    let query = format!("(UID FLAGS MODSEQ) (CHANGEDSINCE {modseq})");
    let fetches = conn.uid_fetch_all("1:*", &query).await?;
    Ok(fetches
        .iter()
        .filter_map(|fetch| {
            fetch.uid.map(|uid| FlagChange {
                uid: uid as i64,
                flags: FlagSnapshot::from_fetch(fetch),
            })
        })
        .collect())
}

async fn fetch_all_flags(
    conn: &mut ImapConnection,
    limit: Option<usize>,
) -> Result<Vec<FlagChange>> {
    let uids = search_all_uids(conn).await?;
    let mut all = Vec::new();
    let total = limit.unwrap_or(uids.len()).min(uids.len());
    for chunk in uids[..total].chunks(FLAG_BATCH_SIZE) {
        let set = uid_set(&chunk.iter().map(|uid| *uid as i64).collect::<Vec<_>>());
        let fetches = conn.uid_fetch_all(&set, "(UID FLAGS)").await?;
        all.extend(fetches.iter().filter_map(|fetch| {
            fetch.uid.map(|uid| FlagChange {
                uid: uid as i64,
                flags: FlagSnapshot::from_fetch(fetch),
            })
        }));
    }
    Ok(all)
}

async fn fetch_metadata(
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    uidvalidity: Option<i64>,
    uids: &[u32],
) -> Result<Vec<crate::imap::fetch::MessageMeta>> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let set = uid_set(&uids.iter().map(|uid| *uid as i64).collect::<Vec<_>>());
    let mut query = String::from(
        "(UID FLAGS INTERNALDATE ENVELOPE BODYSTRUCTURE RFC822.SIZE \
         BODY.PEEK[HEADER.FIELDS (REFERENCES IN-REPLY-TO IMPORTANCE X-PRIORITY LIST-UNSUBSCRIBE CONTENT-TYPE)])",
    );
    if conn.caps.gmail {
        // `X-GM-THRID` is the exact thread key, `X-GM-MSGID` dedupes label
        // copies, and `X-GM-LABELS` keeps the label set for the list and
        // move/archive ops.
        query.pop();
        query.push_str(" X-GM-THRID X-GM-MSGID X-GM-LABELS)");
    }
    let fetches = conn.uid_fetch_all(&set, &query).await?;

    // async-imap 0.12 does not expose parsed `X-GM-THRID` values, so ask for
    // them with a small raw FETCH and keep the parsed attributes. Without this
    // Gmail conversations fall back to References/subject matching.
    let gm_threads = if conn.caps.gmail {
        match fetch_gm_thread_ids(conn, uids).await {
            Ok(ids) => ids,
            Err(error) => {
                // A timeout poisons the connection; continuing would read the
                // previous command's tail as this command's response.
                if crate::util::is_command_timeout(&error) {
                    return Err(error);
                }
                debug!(%error, "X-GM-THRID fetch failed");
                HashMap::new()
            }
        }
    } else {
        HashMap::new()
    };

    let now = Utc::now();
    let mut metas = Vec::new();
    for fetch in &fetches {
        let gm_thrid = fetch
            .uid
            .and_then(|uid| gm_threads.get(&(uid as i64)))
            .map(String::as_str);
        match meta_from_fetch(fetch, &account.id, &folder.id, uidvalidity, now, gm_thrid) {
            Ok(Some(meta)) => metas.push(meta),
            Ok(None) => {}
            Err(error) => warn!(%error, "skipping malformed FETCH response"),
        }
    }
    Ok(metas)
}

/// Fetch `X-GM-THRID` for a UID set. async-imap filters Gmail attributes out
/// of `Fetch`, but the raw response still carries `AttributeValue::GmailThrId`.
async fn fetch_gm_thread_ids(
    conn: &mut ImapConnection,
    uids: &[u32],
) -> Result<HashMap<i64, String>> {
    if uids.is_empty() {
        return Ok(HashMap::new());
    }
    let set = uid_set(&uids.iter().map(|uid| *uid as i64).collect::<Vec<_>>());
    let command = format!("UID FETCH {set} (UID X-GM-THRID)");
    let budget = conn.fetch_timeout();
    conn.with_session(budget, "X-GM-THRID fetch", move |session| {
        Box::pin(async move {
            let tag = session.run_command(&command).await.map_err(imap_error)?;
            let mut result = HashMap::new();
            loop {
                let response = session.read_response().await.map_err(|error| {
                    AppError::network(format!("X-GM-THRID read failed: {error}"))
                })?;
                let Some(data) = response else {
                    break;
                };
                match data.parsed() {
                    Response::Fetch(_, attributes) => {
                        let mut uid = None;
                        let mut thrid = None;
                        for attribute in attributes {
                            match attribute {
                                AttributeValue::Uid(value) => uid = Some(*value as i64),
                                AttributeValue::GmailThrId(value) => {
                                    thrid = Some(value.to_string())
                                }
                                _ => {}
                            }
                        }
                        if let (Some(uid), Some(thrid)) = (uid, thrid) {
                            result.insert(uid, thrid);
                        }
                    }
                    Response::Done {
                        tag: done, status, ..
                    } if done.0 == tag.0 => {
                        if !matches!(status, Status::Ok) {
                            debug!(?status, "server rejected X-GM-THRID fetch");
                        }
                        break;
                    }
                    _ => {}
                }
            }
            Ok(result)
        })
    })
    .await
}

/// Insert/update fetched metadata, deduplicating Gmail messages across labels.
async fn persist_metadata(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    metas: Vec<crate::imap::fetch::MessageMeta>,
    initial: bool,
) -> Result<Vec<nuncio_core::models::MessageSummary>> {
    if metas.is_empty() {
        return Ok(Vec::new());
    }
    let account_id = account.id.clone();
    let folder_id = folder.id.clone();
    let incoming_role = folder.role;
    let gmail = account.capabilities.gmail_extensions;
    let account_email = account.email.to_lowercase();
    let metas_for_write = metas.clone();
    let result = ctx
        .db
        .write(move |db| {
            let mut inserted = Vec::new();
            // Rows hidden for an unacknowledged permanent delete keep their
            // state exactly as the optimistic action left it. A metadata pass
            // may still see the message on the server (the delete is queued or
            // in flight); upserting it would unhide the row and, on Gmail,
            // reset its labels before the command runs.
            let pending_deletes: HashSet<String> =
                repo::messages::pending_delete_ids_in_folder(db, &folder_id)?
                    .into_iter()
                    .collect();
            // A Message-ID does not identify a copy uniquely. Count the ones in
            // this batch first: two incoming copies with the same header must
            // never be folded into one local row.
            let mut header_counts: HashMap<String, usize> = HashMap::new();
            for meta in &metas_for_write {
                if let Some(header) = meta.stored.message_id_header.as_deref() {
                    *header_counts.entry(header.to_string()).or_default() += 1;
                }
            }
            for meta in &metas_for_write {
                let mut stored = meta.stored.clone();
                // Reuse the local id of a row that already represents this
                // Message-ID in the same folder (UIDs change after moves/copies),
                // but only when both sides are unambiguous. A row hidden for a
                // pending delete is never a candidate: upserting it would undo
                // the optimistic hide. A row waiting for a targeted
                // reconciliation is also protected: with a second local
                // candidate or a second server copy the lookup returns None,
                // so a different copy can never clear its markers.
                if let Some(header) = stored.message_id_header.as_deref() {
                    let incoming = header_counts.get(header).copied().unwrap_or(1);
                    // UIDs are folder-local and two provider copies of one
                    // letter share a Message-ID. Reuse only the row bound to
                    // this exact UID/UIDVALIDITY (or a single identity-less
                    // projection), never a stable copy under another UID: that
                    // would overwrite its identity and its Bcc.
                    if let Some(existing_id) =
                        repo::messages::unambiguous_message_id_row_for_identity(
                            db,
                            &account_id,
                            &stored.summary.folder_id,
                            header,
                            incoming,
                            stored.uid,
                            stored.uidvalidity,
                        )?
                    {
                        stored.summary.id = existing_id;
                    }
                }
                if gmail {
                    if let Some(gm_msgid) = stored.gm_msgid.as_deref() {
                        if let Some((existing_id, existing_folder, existing_labels)) = db
                            .query_row(
                                "SELECT id, folder_id, labels_json FROM message WHERE account_id = ?1 AND gm_msgid = ?2 LIMIT 1",
                                rusqlite::params![account_id, gm_msgid],
                                |row| {
                                    Ok((
                                        row.get::<_, String>(0)?,
                                        row.get::<_, String>(1)?,
                                        row.get::<_, Option<String>>(2)?,
                                    ))
                                },
                            )
                            .optional()?
                        {
                            if existing_folder == stored.summary.folder_id {
                                // Same label and message, but the UID may have moved.
                                stored.summary.id = existing_id;
                            } else {
                                let existing_role = repo::folders::get(db, &existing_folder)?
                                    .map(|f| f.role)
                                    .unwrap_or(FolderRole::Other);
                                if folder_priority(incoming_role) < folder_priority(existing_role) {
                                    // The incoming label is more specific (e.g. Inbox
                                    // before a custom label); make it the primary
                                    // folder while keeping every label.
                                    if repo::messages::by_uid(db, &folder_id, stored.uid.unwrap_or(0))?.is_some() {
                                        // Defensive: a stale row already owns this UID.
                                        continue;
                                    }
                                    stored.summary.id = existing_id;
                                    let existing_labels: Vec<String> =
                                        nuncio_core::repo::json_or_default(
                                            existing_labels.as_deref(),
                                            "labels_json",
                                        );
                                    stored.labels = union_labels(&existing_labels, &stored.labels);
                                } else {
                                    // Already owned by an equal or more specific
                                    // folder; merge labels only. Gmail label
                                    // folders query these labels, so nothing is lost.
                                    merge_labels(db, &existing_id, &stored.labels)?;
                                    continue;
                                }
                            }
                        }
                    }
                }
                if pending_deletes.contains(&stored.summary.id) {
                    debug!(
                        message = %stored.summary.id,
                        folder = %folder_id,
                        "sync left a row hidden for an unconfirmed permanent delete"
                    );
                    continue;
                }
                // Reuse the parent conversation when it is already local; the
                // derived key only applies when no ancestor has been stored yet.
                resolve_thread_id(db, &account_id, &mut stored)?;
                let existed: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM message WHERE id = ?1)",
                    rusqlite::params![stored.summary.id],
                    |row| row.get(0),
                )?;
                repo::messages::upsert(db, &stored)?;
                // A late parent may arrive after its children; move those
                // children into the now-authoritative conversation.
                reconcile_threads(
                    db,
                    &account_id,
                    &stored.summary.thread_id,
                    stored.message_id_header.as_deref(),
                )?;
                if !existed {
                    inserted.push(stored.summary.clone());
                }
            }
            // Messages authored by this account feed recipient suggestions.
            let sent: Vec<nuncio_core::models::Address> = metas_for_write
                .iter()
                .filter(|meta| {
                    meta.stored
                        .summary
                        .r#from
                        .as_ref()
                        .map(|from| from.email.to_lowercase() == account_email)
                        .unwrap_or(false)
                })
                .flat_map(|meta| {
                    let mut recipients = meta.stored.summary.to.clone();
                    recipients.extend(meta.stored.summary.cc.clone());
                    recipients
                })
                .collect();
            if !sent.is_empty() {
                repo::recipient_history::record(db, &sent)?;
            }
            repo::folders::recompute_counts(db, &folder_id)?;
            Ok(inserted)
        })
        .await?;

    // Best-effort previews for the newly inserted rows. The full body fetch
    // later replaces these with the exact text.
    if !result.is_empty() {
        if let Err(error) = fill_previews(ctx, conn, account, folder, &metas, &result).await {
            debug!(%error, "preview prefetch failed");
        }
    }
    debug!(
        folder = %folder.remote_path,
        inserted = result.len(),
        initial,
        "metadata persisted"
    );
    Ok(result)
}

/// Ranking used to decide which Gmail label becomes a message's primary
/// folder. Lower wins; `All Mail` is the least specific because every message
/// is in it.
fn folder_priority(role: FolderRole) -> u8 {
    match role {
        FolderRole::Inbox => 0,
        FolderRole::Drafts => 1,
        FolderRole::Sent => 2,
        FolderRole::Junk => 3,
        FolderRole::Trash => 4,
        FolderRole::Archive => 5,
        FolderRole::Flagged => 6,
        FolderRole::Important => 7,
        FolderRole::Other => 8,
        FolderRole::All => 9,
        FolderRole::Outbox => 10,
    }
}

fn union_labels(existing: &[String], incoming: &[String]) -> Vec<String> {
    let mut result: Vec<String> = existing.to_vec();
    for label in incoming {
        if !result.iter().any(|current| current == label) {
            result.push(label.clone());
        }
    }
    result
}

fn merge_labels(conn: &rusqlite::Connection, id: &str, incoming: &[String]) -> Result<()> {
    if incoming.is_empty() {
        return Ok(());
    }
    let existing: Option<String> = conn
        .query_row(
            "SELECT labels_json FROM message WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let existing: Vec<String> =
        nuncio_core::repo::json_or_default(existing.as_deref(), "labels_json");
    let merged = union_labels(&existing, incoming);
    if merged != existing {
        repo::messages::set_labels(conn, id, &merged)?;
    }
    Ok(())
}

/// Prefer the conversation of a local ancestor over the key derived from the
/// headers. This keeps chains together when the local copy of a parent arrived
/// with a truncated `References` header.
pub(crate) fn resolve_thread_id(
    conn: &rusqlite::Connection,
    account_id: &str,
    stored: &mut nuncio_core::models::StoredMessage,
) -> Result<()> {
    if stored.summary.thread_id.starts_with("gm:") {
        return Ok(());
    }
    let own_id = stored.message_id_header.clone().unwrap_or_default();
    let mut candidates: Vec<String> = Vec::new();
    let mut push_candidate = |value: String| {
        if !value.is_empty()
            && value != own_id
            && !candidates.iter().any(|current| current == &value)
        {
            candidates.push(value);
        }
    };
    if let Some(parent) = stored.in_reply_to.as_deref() {
        push_candidate(nuncio_mime::normalize_message_id(parent));
    }
    // The nearest reference is the last one.
    for reference in stored.references.iter().rev() {
        push_candidate(nuncio_mime::normalize_message_id(reference));
    }
    if candidates.is_empty() {
        return Ok(());
    }
    let mut thread_by_header: HashMap<String, String> = HashMap::new();
    for chunk in candidates.chunks(200) {
        let placeholders = (2..=chunk.len() + 1)
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT message_id_header, thread_id FROM message
             WHERE account_id = ?1 AND message_id_header IN ({placeholders})
             ORDER BY date ASC"
        );
        let mut values: Vec<rusqlite::types::Value> =
            vec![nuncio_core::repo::text(account_id.to_string())];
        values.extend(chunk.iter().cloned().map(rusqlite::types::Value::Text));
        let refs = nuncio_core::repo::query_params(&values);
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(refs.as_slice())?;
        while let Some(row) = rows.next()? {
            let header: String = row.get(0)?;
            let thread: String = row.get(1)?;
            if !thread.trim().is_empty() {
                thread_by_header.entry(header).or_insert(thread);
            }
        }
    }
    for candidate in candidates {
        if let Some(thread) = thread_by_header.get(&candidate) {
            stored.summary.thread_id = thread.clone();
            return Ok(());
        }
    }
    Ok(())
}

/// Move every local descendant that references `message_id_header` into
/// `thread_id`, walking the reference tree so a reply chain is repaired even
/// when the root arrives last.
pub(crate) fn reconcile_threads(
    conn: &rusqlite::Connection,
    account_id: &str,
    thread_id: &str,
    message_id_header: Option<&str>,
) -> Result<()> {
    let Some(header) = message_id_header
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };
    if thread_id.is_empty() || thread_id.starts_with("subj:") || thread_id == "unthreaded" {
        return Ok(());
    }
    let mut frontier = vec![header.to_string()];
    let mut visited: HashSet<String> = frontier.iter().cloned().collect();
    for _ in 0..32 {
        if frontier.is_empty() {
            break;
        }
        let mut next = Vec::new();
        for parent in frontier.drain(..) {
            let quoted = serde_json::to_string(&parent).unwrap_or_default();
            let mut statement = conn.prepare(
                "SELECT id, thread_id, message_id_header FROM message
                 WHERE account_id = ?1
                   AND (in_reply_to = ?2 OR instr(COALESCE(references_json, ''), ?3) > 0)",
            )?;
            let mut rows = statement.query(rusqlite::params![account_id, parent, quoted])?;
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let child_thread: String = row.get(1)?;
                let child_header: Option<String> = row.get(2)?;
                if child_thread != thread_id {
                    repo::messages::set_thread(conn, &id, thread_id)?;
                }
                if let Some(child_header) = child_header {
                    if !child_header.trim().is_empty() && visited.insert(child_header.clone()) {
                        next.push(child_header);
                    }
                }
            }
        }
        frontier = next;
    }
    Ok(())
}

/// Store the server's flags for the messages that changed and announce them.
///
/// Flags are changed by other mail clients too, so the local cache update is
/// invisible to the UI unless it is published: `unread_count` is derived from
/// these rows, hence the recompute inside the write before the event goes out.
async fn apply_flag_updates(
    ctx: &CoreContext,
    folder: &Folder,
    changes: &[FlagChange],
) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let folder_id = folder.id.clone();
    let changes: Vec<(i64, FlagSnapshot)> = changes
        .iter()
        .map(|change| (change.uid, change.flags))
        .collect();
    let updated = ctx
        .db
        .write(move |db| {
            let mut updated = Vec::new();
            for (uid, flags) in changes {
                let current = db
                    .query_row(
                        "SELECT id, seen, flagged, answered, forwarded, is_draft
                         FROM message WHERE folder_id = ?1 AND uid = ?2",
                        rusqlite::params![folder_id, uid],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)? != 0,
                                row.get::<_, i64>(2)? != 0,
                                row.get::<_, i64>(3)? != 0,
                                row.get::<_, i64>(4)? != 0,
                                row.get::<_, i64>(5)? != 0,
                            ))
                        },
                    )
                    .optional()?;
                let Some((id, seen, flagged, answered, forwarded, is_draft)) = current else {
                    continue;
                };
                if seen == flags.seen
                    && flagged == flags.flagged
                    && answered == flags.answered
                    && forwarded == flags.forwarded
                    && is_draft == flags.is_draft
                {
                    continue;
                }
                repo::messages::set_flags(
                    db,
                    std::slice::from_ref(&id),
                    Some(flags.seen),
                    Some(flags.flagged),
                    Some(flags.answered),
                    Some(flags.forwarded),
                )?;
                if is_draft != flags.is_draft {
                    db.execute(
                        "UPDATE message SET is_draft = ?2, updated_at = ?3 WHERE id = ?1",
                        rusqlite::params![id, flags.is_draft as i64, Utc::now().to_rfc3339()],
                    )?;
                }
                updated.push(id);
            }
            if !updated.is_empty() {
                // The unread/total counts are snapshots of the message rows, so
                // they have to be consistent before the UI reacts to the event.
                repo::folders::recompute_counts(db, &folder_id)?;
            }
            Ok(updated)
        })
        .await?;

    if !updated.is_empty() {
        events::emit_mail_changed(
            ctx.events.as_ref(),
            folder.account_id.clone(),
            vec![folder.id.clone()],
            updated,
            events::MailChangeKind::Updated,
        );
    }
    Ok(())
}

/// Message ids referenced by operations that are still pending or running.
/// Such a row belongs to the operation that will release it: a sync must not
/// converge it on its own.
fn open_operation_message_ids(
    conn: &rusqlite::Connection,
    account_id: &str,
) -> Result<HashSet<String>> {
    let mut ids = HashSet::new();
    for op in repo::pending_ops::list(conn, Some(account_id))? {
        if !matches!(
            op.state,
            PendingOpState::Pending | PendingOpState::InProgress
        ) {
            continue;
        }
        if let Some(values) = op
            .payload
            .get("messageIds")
            .and_then(|value| value.as_array())
        {
            for value in values {
                if let Some(id) = value.as_str() {
                    ids.insert(id.to_string());
                }
            }
        }
    }
    Ok(ids)
}

/// Row state re-read immediately before a reconciliation write. The tuple is
/// account id, folder id, reconcile reason, reconcile op id and pending_delete.
type ReconcileRowState = (
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    Option<String>,
);

/// Resolve rows left without a trusted folder-local identity by failed
/// optimistic operations.
///
/// Normal syncs only cover the sync window and incremental UID ranges, so a
/// restored row older than the window, or a projection whose target folder has
/// no server copy, would otherwise stay mis-addressed forever. This pass asks
/// the server directly, and only acts on positive evidence:
///
/// * a retained UID still present in the current generation is confirmed;
/// * otherwise `UID SEARCH HEADER Message-ID` (independent of the window) finds
///   the row and the metadata pass rebinds it, but only when both sides are
///   unambiguous;
/// * a `pendingDelete` row is erased only when the queued UID is definitively
///   absent from the same generation — never from an empty HEADER result;
/// * a `moveRollback` row leaves the target only after the target search is
///   empty *and* the source still reports the pre-move UID.
async fn reconcile_pending_identities(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    uidvalidity: Option<i64>,
) -> Result<()> {
    let folder_id = folder.id.clone();
    let account_id = account.id.clone();
    let rows = ctx
        .db
        .read({
            let folder_id = folder_id.clone();
            move |db| repo::messages::pending_identity_rows(db, &folder_id)
        })
        .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let open = ctx
        .db
        .read({
            let account_id = account_id.clone();
            move |db| open_operation_message_ids(db, &account_id)
        })
        .await?;
    // QQ/NetEase accept `UID SEARCH` but answer content keys with no hits. An
    // empty HEADER result is then not evidence of anything.
    let header_search_reliable = nuncio_core::providers::server_text_search(account);

    enum Action {
        /// The row is addressable again. `uid` is `Some` when the server
        /// confirmed the pre-operation number and it must be written back.
        Confirm {
            uid: Option<i64>,
            uidvalidity: Option<i64>,
        },
        /// A permanent delete was positively confirmed for the exact queued
        /// generation (the UID is gone). This is the only path that erases a
        /// row, and it never relies on an empty HEADER result.
        Delete,
        /// The failed move's source still holds the message: put the row back.
        Restore {
            source: String,
            uid: i64,
            uidvalidity: i64,
        },
    }
    let mut actions: Vec<(String, String, Option<String>, Action)> = Vec::new();

    for row in rows {
        if open.contains(&row.id) {
            continue;
        }
        let reason = row.reason().to_string();
        let op_id = row.reconcile_op_id.clone();

        // 1. Exact identity check. `reconcile_uid` is the number the operation
        //    was queued with; it may only be interpreted when the live folder
        //    still reports that generation. Per RFC 9051 §2.3.1.1 a UID is
        //    never reused inside one UIDVALIDITY, so a confirmed absence here
        //    is meaningful for a permanent delete.
        // A move rollback row lives in its *target* folder, so its recorded UID
        // belongs to another folder and must never be searched here; the
        // source side is checked by `move_rollback_source` below.
        let mut uid_state: Option<bool> = None;
        if reason != repo::messages::RECONCILE_MOVE_ROLLBACK {
            if let (Some(uid), Some(generation)) = (row.reconcile_uid, row.reconcile_uidvalidity) {
                if uidvalidity == Some(generation) {
                    match run_search(conn, &format!("UID {uid}")).await {
                        Ok(found) => {
                            uid_state = Some(found.iter().any(|found| i64::from(*found) == uid));
                        }
                        Err(error) => {
                            debug!(%error, message = %row.id, "reconcile: UID presence check failed");
                        }
                    }
                }
            }
        }
        if uid_state == Some(true) {
            actions.push((
                row.id,
                reason,
                op_id,
                Action::Confirm {
                    uid: row.reconcile_uid,
                    uidvalidity: row.reconcile_uidvalidity,
                },
            ));
            continue;
        }
        if may_erase_pending_delete(
            &reason,
            uid_state,
            row.reconcile_uidvalidity.is_some() && row.reconcile_uidvalidity == uidvalidity,
        ) {
            // The exact queued UID is absent from the same generation: the
            // permanent delete reached the server. (A UID cannot be reused in
            // this generation, so this is not an ambiguous observation.)
            actions.push((row.id, reason, op_id, Action::Delete));
            continue;
        }

        // 2. A targeted header search is not limited by the sync window. An
        //    empty result is never enough to delete a row: the provider may not
        //    implement HEADER search, the header may be missing, or a copy may
        //    live elsewhere.
        let Some(header) = row.message_id_header.clone() else {
            debug!(
                message = %row.id,
                reason = %reason,
                "reconcile: no identity and no Message-ID; leaving the row marked"
            );
            continue;
        };
        let escaped = header.replace('\\', "\\\\").replace('"', "\\\"");
        let hits = match run_search(conn, &format!("HEADER Message-ID \"{escaped}\"")).await {
            Ok(hits) => hits,
            Err(error) => {
                debug!(%error, message = %row.id, "reconcile: header search failed");
                continue;
            }
        };
        if !hits.is_empty() {
            // Some servers treat HEADER as a content search and answer with a
            // superset; only accept messages that really carry the header.
            let mut matching = Vec::new();
            for chunk in hits.chunks(BATCH_SIZE) {
                let metas = fetch_metadata(conn, account, folder, uidvalidity, chunk).await?;
                for meta in metas {
                    if meta.stored.message_id_header.as_deref() == Some(header.as_str()) {
                        matching.push(meta);
                    }
                }
            }
            // Several hits mean several copies: binding one of them to the
            // single waiting row would clear its markers and let a later
            // delete address the wrong copy.
            if matching.len() != 1 {
                if matching.len() > 1 {
                    debug!(
                        message = %row.id,
                        hits = matching.len(),
                        "reconcile: Message-ID is ambiguous; leaving the row marked"
                    );
                }
                continue;
            }
            let incoming_uid = matching[0].stored.uid;
            let incoming_uidvalidity = matching[0].stored.uidvalidity;
            let candidates = ctx
                .db
                .read({
                    let account_id = account_id.clone();
                    let folder_id = folder_id.clone();
                    let header = header.clone();
                    move |db| {
                        repo::messages::unambiguous_message_id_row_for_identity(
                            db,
                            &account_id,
                            &folder_id,
                            &header,
                            1,
                            incoming_uid,
                            incoming_uidvalidity,
                        )
                    }
                })
                .await?;
            if candidates.as_deref() != Some(row.id.as_str()) {
                debug!(
                    message = %row.id,
                    "reconcile: the header is owned by another local row; leaving it marked"
                );
                continue;
            }
            persist_metadata(ctx, conn, account, folder, matching, false).await?;
            let bound = ctx
                .db
                .read({
                    let id = row.id.clone();
                    move |db| {
                        Ok(db
                            .query_row(
                                "SELECT uid FROM message WHERE id = ?1",
                                rusqlite::params![id],
                                |row| row.get::<_, Option<i64>>(0),
                            )
                            .optional()?
                            .flatten()
                            .is_some())
                    }
                })
                .await?;
            if bound {
                actions.push((
                    row.id,
                    reason,
                    op_id,
                    Action::Confirm {
                        uid: None,
                        uidvalidity: None,
                    },
                ));
            }
            continue;
        }

        // 3. No hit in this folder. Only a move rollback can still be resolved:
        //    the op knows its source folder, and both ends must agree (absent
        //    here, present there) before the projection moves back.
        if reason != repo::messages::RECONCILE_MOVE_ROLLBACK {
            if !header_search_reliable {
                debug!(
                    message = %row.id,
                    "reconcile: header search is unreliable for this provider; keeping the row marked"
                );
            }
            continue;
        }
        let Some(source) = move_rollback_source(ctx, conn, account, folder, &row).await? else {
            continue;
        };
        actions.push((
            row.id,
            reason,
            op_id,
            Action::Restore {
                source: source.0,
                uid: source.1,
                uidvalidity: source.2,
            },
        ));
    }

    if actions.is_empty() {
        return Ok(());
    }
    let event_account = account_id.clone();
    let event_folder = folder_id.clone();
    let write_account = account_id.clone();
    let write_folder = folder_id.clone();
    let (confirmed_ids, deleted_ids, moved_back_by_source): (
        Vec<String>,
        Vec<String>,
        HashMap<String, Vec<String>>,
    ) = ctx
        .db
        .write(move |db| {
            let mut confirmed_ids: Vec<String> = Vec::new();
            let mut deleted_ids: Vec<String> = Vec::new();
            let mut moved_back_by_source: HashMap<String, Vec<String>> = HashMap::new();
            for (id, reason, op_id, action) in actions {
                // Re-check ownership and the exact state before writing: a
                // concurrent action may have claimed the row between the
                // server lookup and this pass.
                let current: Option<ReconcileRowState> = db
                    .query_row(
                        "SELECT account_id, folder_id, reconcile_reason,
                                -- Blank tokens mean no owner (legacy rollback);
                                -- they must not look like a current trip here.
                                NULLIF(NULLIF(TRIM(reconcile_op_id), ''), '0'),
                                pending_delete,
                                NULLIF(NULLIF(TRIM(move_target_op_id), ''), '0')
                           FROM message WHERE id = ?1",
                        rusqlite::params![id],
                        |row| {
                            Ok((
                                row.get(0)?,
                                row.get(1)?,
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                                row.get::<_, Option<String>>(5)?,
                            ))
                        },
                    )
                    .optional()?;
                let Some((account, folder, stored_reason, stored_op, pending_delete, move_owner)) =
                    current
                else {
                    continue;
                };
                // A legacy row has no explicit reason; `reason()` then reports
                // the safe `rebind` default, which is what was checked above.
                let reason_matches = stored_reason.as_deref() == Some(reason.as_str())
                    || (stored_reason.is_none() && reason == repo::messages::RECONCILE_REBIND);
                // A newer move trip may have claimed this projection while the
                // reconcile actions were being planned. That trip's identity
                // owns the row now; this pass must not confirm or move it.
                let move_owner_conflicts = match (&move_owner, &op_id) {
                    (Some(owner), Some(op)) => owner != op,
                    (Some(_), None) => true,
                    _ => false,
                };
                if account != write_account
                    || folder != write_folder
                    || pending_delete != 0
                    || !reason_matches
                    || stored_op != op_id
                    || move_owner_conflicts
                {
                    debug!(message = %id, "reconcile: the row changed while it was being checked");
                    continue;
                }
                match action {
                    Action::Confirm { uid, uidvalidity } => {
                        match (uid, uidvalidity) {
                            (Some(uid), Some(uidvalidity)) => {
                                repo::messages::set_uid_identity(
                                    db,
                                    &id,
                                    Some(uid),
                                    Some(uidvalidity),
                                )?;
                                repo::messages::confirm_pending_identity(db, &id)?;
                            }
                            _ => repo::messages::confirm_pending_identity(db, &id)?,
                        }
                        confirmed_ids.push(id);
                    }
                    Action::Delete => {
                        repo::messages::delete_local(db, std::slice::from_ref(&id), true)?;
                        deleted_ids.push(id);
                    }
                    Action::Restore {
                        source,
                        uid,
                        uidvalidity,
                    } => {
                        let restored = repo::messages::move_to_folder_rollback(
                            db,
                            std::slice::from_ref(&id),
                            &source,
                            &write_folder,
                            &write_account,
                            op_id.as_deref().unwrap_or(""),
                        )?;
                        if restored.contains(&id) {
                            repo::messages::set_uid_identity(
                                db,
                                &id,
                                Some(uid),
                                Some(uidvalidity),
                            )?;
                            moved_back_by_source
                                .entry(source.clone())
                                .or_default()
                                .push(id);
                        }
                    }
                }
            }
            Ok((confirmed_ids, deleted_ids, moved_back_by_source))
        })
        .await?;
    if !confirmed_ids.is_empty() {
        events::emit_mail_changed(
            ctx.events.as_ref(),
            event_account.clone(),
            vec![event_folder.clone()],
            confirmed_ids,
            events::MailChangeKind::Updated,
        );
    }
    if !deleted_ids.is_empty() {
        events::emit_mail_changed(
            ctx.events.as_ref(),
            event_account.clone(),
            vec![event_folder.clone()],
            deleted_ids,
            events::MailChangeKind::Deleted,
        );
    }
    for (source, ids) in moved_back_by_source {
        events::emit_mail_changed(
            ctx.events.as_ref(),
            event_account.clone(),
            vec![source, event_folder.clone()],
            ids,
            events::MailChangeKind::Updated,
        );
    }
    Ok(())
}

/// Whether a row may be erased after a refused permanent delete.
///
/// The only positive evidence is a successful `UID SEARCH` in the *same*
/// UIDVALIDITY generation answering without the queued UID (RFC 9051 §2.3.1.1:
/// a UID is never reused inside one generation). An empty `HEADER Message-ID`
/// result — or a search that could not run — never authorizes erasure.
fn may_erase_pending_delete(
    reason: &str,
    uid_state: Option<bool>,
    generation_confirmed: bool,
) -> bool {
    reason == repo::messages::RECONCILE_PENDING_DELETE
        && generation_confirmed
        && uid_state == Some(false)
}

/// Verify that a `moveRollback` projection which is absent from the target
/// folder still exists in its source folder.
///
/// Both ends are checked: the caller has already searched the target for the
/// Message-ID, and this function selects the source (recorded by the owning
/// operation) and asks for the pre-move UID in that same generation. Anything
/// short of a positive hit leaves the projection where it is.
async fn move_rollback_source(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    _account: &nuncio_core::models::Account,
    folder: &Folder,
    row: &repo::messages::PendingIdentity,
) -> Result<Option<(String, i64, i64)>> {
    let Some(op_id) = row.reconcile_op_id.clone() else {
        return Ok(None);
    };
    let Some(uid) = row.reconcile_uid else {
        return Ok(None);
    };
    let Some(expected) = row.reconcile_uidvalidity else {
        return Ok(None);
    };
    let op = ctx
        .db
        .read(move |db| repo::pending_ops::get(db, &op_id))
        .await?;
    let Some(op) = op else {
        return Ok(None);
    };
    let payload = &op.payload;
    let source_id = payload
        .get("folderId")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let target_id = payload
        .get("targetFolderId")
        .and_then(|value| value.as_str());
    let Some(source_id) = source_id else {
        return Ok(None);
    };
    if target_id != Some(folder.id.as_str()) || source_id == folder.id {
        return Ok(None);
    }
    let source = ctx
        .db
        .read({
            let source_id = source_id.clone();
            move |db| repo::folders::get(db, &source_id)
        })
        .await?;
    let Some(source) = source else {
        return Ok(None);
    };
    let mailbox = conn.select(&source.remote_path).await;
    let live = mailbox
        .ok()
        .and_then(|mailbox| mailbox.uid_validity.map(|value| value as i64));
    let present = if live == Some(expected) {
        run_search(conn, &format!("UID {uid}")).await.ok()
    } else {
        None
    };
    // Restore the folder this sync pass is processing before returning; the
    // caller may still need the connection (and leaving another mailbox
    // selected would be surprising).
    if let Err(error) = conn.select(&folder.remote_path).await {
        debug!(%error, folder = %folder.remote_path, "reconcile: could not re-select folder");
    }
    let confirmed = present.is_some_and(|found| found.iter().any(|found| i64::from(*found) == uid));
    Ok(confirmed.then_some((source_id, uid, expected)))
}

fn local_uids(db: &rusqlite::Connection, folder_id: &str) -> Result<Vec<(String, i64)>> {
    // A row waiting for a permanent delete to be confirmed is not authoritative
    // of the server's state yet: the expunge/prune pass must not drop it.
    let mut statement = db.prepare(
        "SELECT id, uid FROM message
          WHERE folder_id = ?1 AND uid IS NOT NULL AND pending_delete = 0",
    )?;
    let mut rows = statement.query(rusqlite::params![folder_id])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        result.push((row.get(0)?, row.get(1)?));
    }
    Ok(result)
}

async fn needs_full_reconcile(
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
) -> Result<bool> {
    let key = format!("full_reconcile:{folder_id}");
    let account_id = account_id.to_string();
    let value = ctx
        .db
        .read(move |db| repo::sync_state::get(db, &account_id, "mail", &key))
        .await?;
    let Some(value) = value else {
        return Ok(true);
    };
    Ok(DateTime::parse_from_rfc3339(&value)
        .map(|date| {
            Utc::now() - date.with_timezone(&Utc) > ChronoDuration::hours(FULL_RECONCILE_HOURS)
        })
        .unwrap_or(true))
}

async fn mark_full_reconcile(ctx: &CoreContext, account_id: &str, folder_id: &str) -> Result<()> {
    let key = format!("full_reconcile:{folder_id}");
    let value = Utc::now().to_rfc3339();
    let account_id = account_id.to_string();
    ctx.db
        .write(move |db| repo::sync_state::set(db, &account_id, "mail", &key, &value))
        .await
}

/// Fetch the first 2 KiB of the first text part for newly inserted messages.
async fn fill_previews(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    _account: &nuncio_core::models::Account,
    folder: &Folder,
    metas: &[crate::imap::fetch::MessageMeta],
    inserted: &[nuncio_core::models::MessageSummary],
) -> Result<()> {
    let inserted_ids: HashSet<&str> = inserted.iter().map(|item| item.id.as_str()).collect();
    // Section -> (uids, transfer encoding, charset). Messages that share a
    // section layout usually share the encoding too, but keep the first
    // observed value per message by storing them alongside the uid.
    let mut groups: HashMap<String, (Vec<i64>, HashMap<i64, (Option<String>, Option<String>)>)> =
        HashMap::new();
    for meta in metas {
        if !inserted_ids.contains(meta.stored.summary.id.as_str()) {
            continue;
        }
        if let (Some(uid), Some(part)) = (meta.stored.uid, meta.text_part.as_deref()) {
            let entry = groups
                .entry(part.to_string())
                .or_insert_with(|| (Vec::new(), HashMap::new()));
            entry.0.push(uid);
            entry.1.insert(
                uid,
                (
                    meta.text_part_encoding.clone(),
                    meta.text_part_charset.clone(),
                ),
            );
        }
    }
    for (part, (uids, part_meta)) in groups {
        let Some(section) = section_path(&part) else {
            continue;
        };
        for chunk in uids.chunks(50) {
            let set = uid_set(chunk);
            let query = format!("(UID BODY.PEEK[{part}]<0.{PREVIEW_BYTES}>)");
            let fetches = match conn.uid_fetch_all(&set, &query).await {
                Ok(fetches) => fetches,
                Err(error) if crate::util::is_command_timeout(&error) => {
                    // The connection is poisoned; later chunks cannot be read.
                    return Err(error);
                }
                Err(error) => {
                    debug!(%error, "preview fetch failed");
                    continue;
                }
            };
            let mut updates = Vec::new();
            for fetch in &fetches {
                if let Some(uid) = fetch.uid {
                    let bytes = if part.eq_ignore_ascii_case("TEXT") {
                        fetch.text()
                    } else {
                        fetch.section(&section)
                    };
                    if let Some(bytes) = bytes {
                        let (encoding, charset) = part_meta
                            .get(&(uid as i64))
                            .map(|(encoding, charset)| (encoding.clone(), charset.clone()))
                            .unwrap_or((None, None));
                        let text = nuncio_mime::decode_preview_part(
                            bytes,
                            encoding.as_deref(),
                            charset.as_deref(),
                        );
                        if !text.trim().is_empty() {
                            updates.push((uid as i64, nuncio_core::text::trim_preview(&text, 200)));
                        }
                    }
                }
            }
            if updates.is_empty() {
                continue;
            }
            let folder_id = folder.id.clone();
            let updated = ctx
                .db
                .write(move |db| {
                    let mut updated = Vec::new();
                    for (uid, preview) in updates {
                        if let Some(stored) = repo::messages::by_uid(db, &folder_id, uid)? {
                            repo::messages::update_preview(db, &stored.summary.id, &preview)?;
                            updated.push(stored.summary.id);
                        }
                    }
                    Ok(updated)
                })
                .await?;
            if !updated.is_empty() {
                events::emit_mail_changed(
                    ctx.events.as_ref(),
                    folder.account_id.clone(),
                    vec![folder.id.clone()],
                    updated,
                    events::MailChangeKind::Updated,
                );
            }
        }
    }
    Ok(())
}

fn section_path(part: &str) -> Option<SectionPath> {
    if part.eq_ignore_ascii_case("TEXT") {
        return Some(SectionPath::Full(MessageSection::Text));
    }
    let parts: Option<Vec<u32>> = part
        .split('.')
        .map(|segment| segment.parse::<u32>().ok())
        .collect();
    parts.map(|parts| SectionPath::Part(parts, None))
}

/// Build an IMAP search criteria string from a [`SearchQuery`].
///
/// `hasAttachment` has no portable IMAP equivalent and is intentionally left
/// to the local index (the server result is a superset). Strings are quoted and
/// escaped per RFC 3501; non-ASCII keys are sent as UTF-8 under an explicit
/// `CHARSET UTF-8` directive (the IMAP client library has no literal writer),
/// and [`server_search`] retries without the directive when a server rejects it.
pub fn imap_search_criteria(query: &nuncio_core::models::SearchQuery) -> Option<String> {
    fn escape(value: &str) -> String {
        value.replace('\\', "\\\\").replace('"', "\\\"")
    }
    let mut parts = Vec::new();
    let mut non_ascii = false;
    let push_string = |parts: &mut Vec<String>, key: &str, value: &str, non_ascii: &mut bool| {
        if value.chars().any(|character| !character.is_ascii()) {
            *non_ascii = true;
        }
        parts.push(format!("{key} \"{}\"", escape(value)));
    };
    if !query.text.trim().is_empty() {
        push_string(&mut parts, "TEXT", query.text.trim(), &mut non_ascii);
    }
    if let Some(from) = query
        .from
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        push_string(&mut parts, "FROM", from.trim(), &mut non_ascii);
    }
    if let Some(to) = query.to.as_deref().filter(|value| !value.trim().is_empty()) {
        push_string(&mut parts, "TO", to.trim(), &mut non_ascii);
    }
    if let Some(subject) = query
        .subject
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        push_string(&mut parts, "SUBJECT", subject.trim(), &mut non_ascii);
    }
    if query.unread == Some(true) {
        parts.push("UNSEEN".into());
    }
    if query.flagged == Some(true) {
        parts.push("FLAGGED".into());
    }
    if let Some(from) = query.date_from.as_deref() {
        if let Ok(date) = chrono::NaiveDate::parse_from_str(from, "%Y-%m-%d") {
            let value = date
                .and_hms_opt(0, 0, 0)
                .map(|naive| chrono::DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
                .unwrap_or_else(Utc::now);
            parts.push(format!("SINCE {}", imap_date(value)));
        }
    }
    if let Some(to) = query.date_to.as_deref() {
        if let Ok(date) = chrono::NaiveDate::parse_from_str(to, "%Y-%m-%d") {
            let value = date
                .and_hms_opt(0, 0, 0)
                .map(|naive| chrono::DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
                .unwrap_or_else(Utc::now)
                + chrono::Duration::days(1);
            parts.push(format!("BEFORE {}", imap_date(value)));
        }
    }
    if parts.is_empty() {
        None
    } else {
        let criteria = parts.join(" ");
        // RFC 3501 §6.4.4: non-ASCII search keys require an explicit charset.
        // Servers that reject CHARSET cause the caller to fall back to the
        // local index, which always supports UTF-8 text.
        if non_ascii {
            Some(format!("CHARSET UTF-8 {criteria}"))
        } else {
            Some(criteria)
        }
    }
}

/// The charset directive prepended to non-ASCII search criteria.
const UTF8_CHARSET_PREFIX: &str = "CHARSET UTF-8 ";

/// Criteria with the `CHARSET UTF-8` directive removed, for servers that do not
/// implement RFC 3501 §6.4.4.
pub fn search_without_charset(criteria: &str) -> Option<&str> {
    criteria.strip_prefix(UTF8_CHARSET_PREFIX)
}

/// Run a `UID SEARCH`, retrying once without `CHARSET UTF-8` when the server
/// rejects the charset. A server that rejects both is treated as "no server
/// hits" so callers can still use the local UTF-8 FTS index.
async fn search_with_fallback(conn: &mut ImapConnection, criteria: &str) -> Result<Vec<u32>> {
    match run_search(conn, criteria).await {
        Ok(uids) => Ok(uids),
        Err(error) => {
            let Some(fallback) = search_without_charset(criteria) else {
                return Err(error);
            };
            if error.code() != "protocol" {
                return Err(error);
            }
            debug!(%error, "server rejected CHARSET UTF-8; retrying without it");
            match run_search(conn, fallback).await {
                Ok(uids) => Ok(uids),
                Err(error) => {
                    debug!(
                        %error,
                        "server search without CHARSET failed; using the local index only"
                    );
                    Ok(Vec::new())
                }
            }
        }
    }
}

async fn run_search(conn: &mut ImapConnection, criteria: &str) -> Result<Vec<u32>> {
    conn.uid_search(criteria)
        .await
        .map(|set| set.into_iter().collect())
}

/// Run an IMAP `UID SEARCH` and cache metadata for any matching messages that
/// are not stored locally yet. Returns local message ids.
pub async fn server_search(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    criteria: &str,
) -> Result<Vec<String>> {
    let mailbox = if conn.caps.condstore {
        conn.select_condstore(&folder.remote_path).await
    } else {
        conn.select(&folder.remote_path).await
    }?;
    let uidvalidity = mailbox.uid_validity.map(|value| value as i64);
    let found = search_with_fallback(conn, criteria).await?;
    cache_hits(ctx, conn, account, folder, uidvalidity, found).await
}

/// Fetch and persist metadata for server hits that are not stored locally and
/// return the local ids of every hit.
async fn cache_hits(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    uidvalidity: Option<i64>,
    found: Vec<u32>,
) -> Result<Vec<String>> {
    if found.is_empty() {
        return Ok(Vec::new());
    }

    let folder_id = folder.id.clone();
    let known = ctx
        .db
        .read({
            let folder_id = folder_id.clone();
            move |db| {
                let rows = local_uids(db, &folder_id)?;
                Ok(rows
                    .into_iter()
                    .map(|(id, uid)| (uid, id))
                    .collect::<HashMap<_, _>>())
            }
        })
        .await?;
    let known_uids: HashSet<i64> = known.keys().copied().collect();
    let missing: Vec<u32> = found
        .iter()
        .copied()
        .filter(|uid| !known_uids.contains(&(*uid as i64)))
        .collect();
    for chunk in missing.chunks(BATCH_SIZE) {
        let metas = fetch_metadata(conn, account, folder, uidvalidity, chunk).await?;
        persist_metadata(ctx, conn, account, folder, metas, false).await?;
    }

    let found_set: HashSet<i64> = found.iter().map(|uid| *uid as i64).collect();
    let ids = ctx
        .db
        .read({
            let folder_id = folder_id.clone();
            move |db| {
                let rows = local_uids(db, &folder_id)?;
                Ok(rows
                    .into_iter()
                    .filter(|(_, uid)| found_set.contains(uid))
                    .map(|(id, _)| id)
                    .collect::<Vec<_>>())
            }
        })
        .await?;
    Ok(ids)
}

/// Newest-first cap on how many message headers a fallback scan reads. 5000
/// is 25 round trips of [`BATCH_SIZE`] and roughly 5 MB of header text: a
/// few seconds on a home connection while still covering several years of a
/// typical personal mailbox. Larger folders report `scanned < total`.
pub const SCAN_CAP: usize = 5000;

/// Header fields requested by the fallback scan.
const SCAN_FIELDS: &str = "(UID BODY.PEEK[HEADER.FIELDS (FROM TO CC SUBJECT DATE MESSAGE-ID)])";

/// Outcome of [`server_scan`].
#[derive(Debug, Clone, Default)]
pub struct ScanOutcome {
    /// Local ids of every matched message (persisted when not yet stored).
    pub ids: Vec<String>,
    /// Headers actually read.
    pub scanned: u32,
    /// Candidate messages the server reported (before the cap).
    pub total: u32,
}

/// Terms used by the header matcher of the fallback scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanTerms {
    /// Lower-cased free-text terms; every term must hit some header field.
    pub text: Vec<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub subject: Option<String>,
}

impl ScanTerms {
    pub fn from_query(query: &nuncio_core::models::SearchQuery) -> Self {
        let clean = |value: &Option<String>| {
            value
                .as_deref()
                .map(|value| value.trim().to_lowercase())
                .filter(|value| !value.is_empty())
        };
        Self {
            text: query
                .text
                .split_whitespace()
                .map(|term| term.trim_matches('"').to_lowercase())
                .filter(|term| !term.is_empty())
                .collect(),
            from: clean(&query.from),
            to: clean(&query.to),
            subject: clean(&query.subject),
        }
    }

    /// True when there is anything a header scan can match on.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.from.is_none() && self.to.is_none() && self.subject.is_none()
    }
}

/// Structural-only criteria for the scan: the keys every server answers
/// correctly (`ALL`, `UNSEEN`, `FLAGGED`, `SINCE`, `BEFORE`). Content keys are
/// matched locally against the fetched headers.
pub fn imap_scan_criteria(query: &nuncio_core::models::SearchQuery) -> String {
    let mut structural = query.clone();
    structural.text.clear();
    structural.from = None;
    structural.to = None;
    structural.subject = None;
    imap_search_criteria(&structural).unwrap_or_else(|| "ALL".to_string())
}

/// Case-insensitive substring match of `terms` against a raw header block.
/// RFC 2047 words are decoded first so CJK subjects and display names match.
pub fn header_matches(raw_headers: &[u8], terms: &ScanTerms) -> bool {
    let Ok(headers) = nuncio_mime::parse_headers_only(raw_headers) else {
        return false;
    };
    let addresses = |list: &[nuncio_core::models::Address]| {
        list.iter()
            .map(|address| {
                format!(
                    "{} {}",
                    address.name.as_deref().unwrap_or(""),
                    address.email
                )
            })
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    let subject = headers.subject.to_lowercase();
    let from = headers
        .from
        .as_ref()
        .map(|address| addresses(std::slice::from_ref(address)))
        .unwrap_or_default();
    let to = addresses(&headers.to);
    let cc = addresses(&headers.cc);
    let message_id = headers
        .message_id
        .clone()
        .unwrap_or_default()
        .to_lowercase();
    if let Some(wanted) = &terms.from {
        if !from.contains(wanted.as_str()) {
            return false;
        }
    }
    if let Some(wanted) = &terms.to {
        if !to.contains(wanted.as_str()) && !cc.contains(wanted.as_str()) {
            return false;
        }
    }
    if let Some(wanted) = &terms.subject {
        if !subject.contains(wanted.as_str()) {
            return false;
        }
    }
    terms.text.iter().all(|term| {
        [&subject, &from, &to, &cc, &message_id]
            .iter()
            .any(|field| field.contains(term.as_str()))
    })
}

/// Fallback for servers whose content `SEARCH` is broken (NetEase, QQ):
/// `EXAMINE` the folder read-only, `UID SEARCH` structural criteria, then read
/// headers newest-first in [`BATCH_SIZE`] UID batches and match locally.
/// Matches that are not stored yet are cached through the same path as
/// [`server_search`]. `cancel` is checked between batches.
pub async fn server_scan(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    query: &nuncio_core::models::SearchQuery,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<ScanOutcome> {
    server_scan_with_cap(ctx, conn, account, folder, query, cancel, SCAN_CAP).await
}

/// [`server_scan`] with an explicit newest-first cap (tests use a tiny one).
pub async fn server_scan_with_cap(
    ctx: &CoreContext,
    conn: &mut ImapConnection,
    account: &nuncio_core::models::Account,
    folder: &Folder,
    query: &nuncio_core::models::SearchQuery,
    cancel: &std::sync::atomic::AtomicBool,
    cap: usize,
) -> Result<ScanOutcome> {
    use std::sync::atomic::Ordering;
    let terms = ScanTerms::from_query(query);
    if terms.is_empty() {
        return Ok(ScanOutcome::default());
    }
    let mailbox = conn.examine(&folder.remote_path).await?;
    let uidvalidity = mailbox.uid_validity.map(|value| value as i64);
    let mut uids = run_search(conn, &imap_scan_criteria(query)).await?;
    uids.sort_unstable_by(|a, b| b.cmp(a));
    let total = uids.len() as u32;
    uids.truncate(cap);

    let mut matched: Vec<u32> = Vec::new();
    let mut scanned = 0u32;
    for chunk in uids.chunks(BATCH_SIZE) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let set = uid_set(&chunk.iter().map(|uid| *uid as i64).collect::<Vec<_>>());
        let fetches = conn.uid_fetch_all(&set, SCAN_FIELDS).await?;
        scanned += chunk.len() as u32;
        for fetch in &fetches {
            let (Some(uid), Some(raw)) = (
                fetch.uid,
                fetch.section(&SectionPath::Full(MessageSection::Header)),
            ) else {
                continue;
            };
            if header_matches(raw, &terms) {
                matched.push(uid);
            }
        }
    }
    matched.sort_unstable_by(|a, b| b.cmp(a));
    // Caching needs a read-write view only for local state; FETCH of metadata
    // is read-only on the wire, so the EXAMINE selection stays valid.
    let ids = cache_hits(ctx, conn, account, folder, uidvalidity, matched).await?;
    Ok(ScanOutcome {
        ids,
        scanned,
        total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn legacy_graph_cursors_are_removed_without_touching_current_keys() {
        let dir = std::env::temp_dir().join(format!(
            "nuncio-legacy-cursor-cleanup-{}",
            nuncio_core::models::new_id()
        ));
        let ctx = nuncio_core::CoreContext::for_test(nuncio_core::Paths::new(dir))
            .await
            .expect("core context");
        ctx.db
            .write(|db| {
                for key in [
                    "graph_delta_v1:fld",
                    "graph_pending_v1:fld",
                    "graph_delta:fld",
                    "graph_pending:fld",
                ] {
                    repo::sync_state::set(db, "acc", "mail", key, "legacy")?;
                }
                for key in [
                    "graph_delta_v2:fld",
                    "graph_pending_v2:fld",
                    "graph_folder:Inbox",
                    "drafts_resync_pending",
                ] {
                    repo::sync_state::set(db, "acc", "mail", key, "live")?;
                }
                // Another account's legacy row must be left for its own pass.
                repo::sync_state::set(db, "other", "mail", "graph_delta_v1:fld", "legacy")?;
                Ok(())
            })
            .await
            .expect("seed");

        cleanup_legacy_graph_cursors(&ctx, "acc").await;

        let keys = ctx
            .db
            .read(|db| repo::sync_state::list_module(db, "acc", "mail"))
            .await
            .expect("list");
        let keys: Vec<&str> = keys.iter().map(|entry| entry.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "drafts_resync_pending",
                "graph_delta_v2:fld",
                "graph_folder:Inbox",
                "graph_pending_v2:fld",
            ]
        );
        assert_eq!(
            ctx.db
                .read(|db| repo::sync_state::get(db, "other", "mail", "graph_delta_v1:fld"))
                .await
                .expect("other account"),
            Some("legacy".to_string())
        );
    }

    #[test]
    fn search_criteria_are_quoted_and_combined() {
        let mut query = nuncio_core::models::SearchQuery::text("invoice \"2026\"");
        query.from = Some("billing@example.com".into());
        query.unread = Some(true);
        query.date_from = Some("2026-11-01".into());
        let criteria = imap_search_criteria(&query).expect("criteria");
        assert!(criteria.contains("TEXT \"invoice \\\"2026\\\"\""));
        assert!(criteria.contains("FROM \"billing@example.com\""));
        assert!(criteria.contains("UNSEEN"));
        assert!(criteria.contains("SINCE 01-Nov-2026"));
        assert!(imap_search_criteria(&nuncio_core::models::SearchQuery::default()).is_none());
    }

    #[test]
    fn search_criteria_declare_utf8_for_non_ascii_keys() {
        let query = nuncio_core::models::SearchQuery::text("会议纪要");
        let criteria = imap_search_criteria(&query).expect("criteria");
        assert_eq!(criteria, "CHARSET UTF-8 TEXT \"会议纪要\"");

        let mut query = nuncio_core::models::SearchQuery::text("report");
        query.subject = Some("周报".into());
        let criteria = imap_search_criteria(&query).expect("criteria");
        assert!(criteria.starts_with("CHARSET UTF-8 "));
        assert!(criteria.contains("TEXT \"report\""));
        assert!(criteria.contains("SUBJECT \"周报\""));

        // ASCII-only searches keep the classic form.
        let criteria = imap_search_criteria(&nuncio_core::models::SearchQuery::text("invoice"))
            .expect("criteria");
        assert_eq!(criteria, "TEXT \"invoice\"");
    }

    #[test]
    fn header_matcher_handles_ascii_cjk_and_encoded_words() {
        let raw = "From: =?UTF-8?B?5byg5LiJ?= <zhang@example.com>\r\nTo: me@example.com\r\n\
                   Subject: =?UTF-8?B?5Lya6K6u57qq6KaB?= Re: Budget\r\nMessage-ID: <abc@x>\r\n\r\n";
        let terms = |text: &str| ScanTerms {
            text: text.split_whitespace().map(str::to_lowercase).collect(),
            ..ScanTerms::default()
        };
        assert!(header_matches(raw.as_bytes(), &terms("budget")));
        assert!(header_matches(raw.as_bytes(), &terms("BUDGET re:")));
        assert!(header_matches(raw.as_bytes(), &terms("会议纪要")));
        assert!(header_matches(raw.as_bytes(), &terms("张三")));
        assert!(header_matches(raw.as_bytes(), &terms("zhang@example")));
        assert!(!header_matches(raw.as_bytes(), &terms("budget missing")));
        let subject_only = ScanTerms {
            subject: Some("zhang".into()),
            ..ScanTerms::default()
        };
        assert!(!header_matches(raw.as_bytes(), &subject_only));
        assert!(!header_matches(b"", &terms("x")));
    }

    #[test]
    fn scan_criteria_keep_only_structural_keys() {
        let mut query = nuncio_core::models::SearchQuery::text("会议");
        query.from = Some("a".into());
        assert_eq!(imap_scan_criteria(&query), "ALL");
        query.unread = Some(true);
        query.date_from = Some("2026-11-01".into());
        assert_eq!(imap_scan_criteria(&query), "UNSEEN SINCE 01-Nov-2026");
    }

    #[test]
    fn charset_fallback_strips_only_the_utf8_directive() {
        assert_eq!(
            search_without_charset("CHARSET UTF-8 TEXT \"会议纪要\""),
            Some("TEXT \"会议纪要\"")
        );
        assert_eq!(search_without_charset("TEXT \"invoice\""), None);
    }

    #[test]
    fn section_paths_round_trip() {
        assert_eq!(
            section_path("TEXT"),
            Some(SectionPath::Full(MessageSection::Text))
        );
        assert_eq!(
            section_path("1.2"),
            Some(SectionPath::Part(vec![1, 2], None))
        );
        assert_eq!(section_path("bad"), None);
    }

    #[test]
    fn synthetic_uids_are_stable_positive_and_distinct() {
        let id = "AAMkAD/abc== +x";
        let first = synthetic_uid(id);
        assert!(first > 0, "a synthetic UID must look like a real UID");
        assert_eq!(
            first,
            synthetic_uid(id),
            "the same Graph id must map stably"
        );
        assert_ne!(first, synthetic_uid("AAMkAD/other"));
    }

    #[test]
    fn graph_resync_keeps_local_answered_and_forwarded_flags() {
        let db = nuncio_core::db::Db::open_in_memory().expect("in-memory database");
        db.migrate_core().expect("core migrations");
        let now = nuncio_core::models::now().to_rfc3339();

        db.write_sync(|conn| {
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

            // A row exactly as the Graph mapper builds it: answered/forwarded
            // default to false because Graph has no such flags.
            let graph_row = |id: &str| {
                let summary = nuncio_core::models::MessageSummary::new(
                    id,
                    "acc",
                    "fld",
                    "thread",
                    nuncio_core::models::now(),
                );
                nuncio_core::models::StoredMessage::new(summary)
            };

            // First sync inserts the row; the user then replies and forwards,
            // which sets the local-only flags.
            let mut first = graph_row("graph-1");
            first.summary.answered = true;
            first.summary.forwarded = true;
            repo::messages::upsert(conn, &first)?;

            // Next delta pass maps the same Graph message back to false.
            let mut resynced = graph_row("graph-1");
            assert!(!resynced.summary.answered && !resynced.summary.forwarded);
            assert!(preserve_local_flags(conn, &mut resynced)?);
            repo::messages::upsert(conn, &resynced)?;

            let (answered, forwarded): (i64, i64) = conn.query_row(
                "SELECT answered, forwarded FROM message WHERE id = 'graph-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert_eq!((answered, forwarded), (1, 1));

            // A genuinely new message is not reported as existing and keeps
            // the mapped values.
            let mut fresh = graph_row("graph-2");
            assert!(!preserve_local_flags(conn, &mut fresh)?);
            assert!(!fresh.summary.answered && !fresh.summary.forwarded);
            Ok(())
        })
        .expect("seed and re-sync");
    }

    #[tokio::test]
    async fn server_flag_changes_emit_updated_event_and_refresh_counts() {
        use nuncio_core::traits::testing::{
            NullCredentialProvider, NullNotifier, RecordingEventSink,
        };

        let sink = std::sync::Arc::new(RecordingEventSink::new());
        let ctx = nuncio_core::CoreContext::new(
            nuncio_core::db::Db::open_in_memory().expect("in-memory database"),
            nuncio_core::Paths::new(std::env::temp_dir().join(nuncio_core::models::new_id())),
            sink.clone(),
            std::sync::Arc::new(NullCredentialProvider),
            std::sync::Arc::new(NullNotifier),
        )
        .await
        .expect("core context");

        ctx.db
            .write(|db| {
                seed_graph_folder(db);
                insert_message(db, "m1", Some(1), None, false, None);
                insert_message(db, "m2", Some(2), None, false, None);
                // m1 was unread locally; another client has now read it.
                db.execute("UPDATE message SET seen = 0 WHERE id = 'm1'", [])?;
                Ok(())
            })
            .await
            .expect("seed");

        let folder = ctx
            .db
            .read(|db| repo::folders::get_required(db, "fld"))
            .await
            .expect("folder");

        let read = FlagSnapshot {
            seen: true,
            ..FlagSnapshot::default()
        };
        let changes = vec![
            FlagChange {
                uid: 1,
                flags: read,
            },
            // Already stored exactly like this: it must not be reported.
            FlagChange {
                uid: 2,
                flags: read,
            },
        ];
        apply_flag_updates(&ctx, &folder, &changes)
            .await
            .expect("apply flags");

        let seen: i64 = ctx
            .db
            .read(|db| {
                Ok(
                    db.query_row("SELECT seen FROM message WHERE id = 'm1'", [], |row| {
                        row.get(0)
                    })?,
                )
            })
            .await
            .expect("seen flag");
        assert_eq!(seen, 1, "the server flag is stored");

        let (event, payload) = sink.last().expect("a change event");
        assert_eq!(event, events::MAIL_CHANGED);
        assert_eq!(payload["accountId"], "acc");
        assert_eq!(payload["folderIds"], serde_json::json!(["fld"]));
        assert_eq!(payload["messageIds"], serde_json::json!(["m1"]));
        assert_eq!(payload["kind"], "updated");

        let folder = ctx
            .db
            .read(|db| repo::folders::get_required(db, "fld"))
            .await
            .expect("folder");
        assert_eq!(folder.unread_count, 0, "both rows are read now");
        assert_eq!(
            folder.total_count, 2,
            "counts are recomputed before the event"
        );

        // No changes, no event.
        let before = sink.len();
        apply_flag_updates(&ctx, &folder, &[])
            .await
            .expect("no changes");
        assert_eq!(sink.len(), before);
    }

    /// Seed the account/folder Graph sync tests write into.
    fn seed_graph_folder(conn: &rusqlite::Connection) {
        let now = nuncio_core::models::now().to_rfc3339();
        conn.execute(
            "INSERT INTO account (id, provider, email, auth_kind, created_at, updated_at)
             VALUES ('acc', 'outlook', 'me@example.com', 'oauth', ?1, ?1)",
            rusqlite::params![now],
        )
        .expect("account");
        conn.execute(
            "INSERT INTO folder (id, account_id, remote_path, name, role, created_at, updated_at)
             VALUES ('fld', 'acc', 'Inbox', 'Inbox', 'inbox', ?1, ?1)",
            rusqlite::params![now],
        )
        .expect("folder");
    }

    /// Insert a `message` row with only the columns the prune reads.
    fn insert_message(
        conn: &rusqlite::Connection,
        id: &str,
        uid: Option<i64>,
        gm_msgid: Option<&str>,
        is_draft: bool,
        header: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO message (
                 id, account_id, folder_id, uid, gm_msgid, message_id_header, thread_id,
                 subject, date, preview, seen, flagged, answered, forwarded, is_draft,
                 has_attachments, importance, size, created_at, updated_at
             ) VALUES (?1, 'acc', 'fld', ?2, ?3, ?4, 'thread', 'subject', ?5, '', 1, 0, 0, 0,
                       ?6, 0, 'normal', 0, ?5, ?5)",
            rusqlite::params![
                id,
                uid,
                gm_msgid,
                header,
                nuncio_core::models::now().to_rfc3339(),
                is_draft as i64,
            ],
        )
        .expect("insert message");
    }

    #[test]
    fn graph_prune_preserves_draft_projections() {
        let db = nuncio_core::db::Db::open_in_memory().expect("in-memory database");
        db.migrate_core().expect("core migrations");

        db.write_sync(|conn| {
            seed_graph_folder(conn);
            // Never synced: the composer's projection for a draft whose APPEND
            // has not run yet.
            insert_message(
                conn,
                "draft-local",
                None,
                None,
                true,
                Some("nuncio-draft-abc@nuncio.local"),
            );
            // Already synced once and now being re-APPENDed: its old Graph id
            // is gone from the server, but that is expected.
            insert_message(
                conn,
                "draft-resynced",
                Some(synthetic_uid("draft-resynced")),
                Some("graph-old-draft"),
                true,
                Some("nuncio-draft-def@nuncio.local"),
            );
            repo::pending_ops::enqueue(
                conn,
                nuncio_core::models::NewPendingOp::new(
                    "acc",
                    crate::ops::KIND_APPEND_DRAFT,
                    serde_json::json!({
                        "draftId": "def",
                        "folderId": "fld",
                        "rfc822Path": "/tmp/draft.eml"
                    }),
                    "Save draft to server",
                ),
            )?;

            // The scan returned a different message; neither draft may be
            // pruned.
            let seen: HashSet<String> = ["graph-live".to_string()].into_iter().collect();
            let removed = graph_prune_ids(conn, "acc", "fld", &seen, 0)?;
            assert!(
                removed.is_empty(),
                "draft projections must survive a full scan: {removed:?}"
            );
            Ok(())
        })
        .expect("prune drafts");
    }

    #[test]
    fn graph_prune_removes_imap_era_rows() {
        let db = nuncio_core::db::Db::open_in_memory().expect("in-memory database");
        db.migrate_core().expect("core migrations");

        db.write_sync(|conn| {
            seed_graph_folder(conn);
            // A row synced over IMAP before the switch: real UID, no Graph id,
            // so it can never appear in the Graph scan.
            insert_message(
                conn,
                "imap-era",
                Some(42),
                None,
                false,
                Some("<old@example.com>"),
            );

            let seen: HashSet<String> = ["graph-live".to_string()].into_iter().collect();
            let removed = graph_prune_ids(conn, "acc", "fld", &seen, 0)?;
            assert_eq!(removed, vec!["imap-era".to_string()]);
            Ok(())
        })
        .expect("prune imap era row");
    }

    /// A full Graph scan is `$filter`ed to the sync window. A row older than
    /// the window is simply not returned, so its absence must never be read as
    /// "the server no longer has it". Only the direct receipt of a confirmed
    /// operation may remove such a projection.
    #[test]
    fn graph_prune_keeps_rows_outside_the_sync_window() {
        let db = nuncio_core::db::Db::open_in_memory().expect("in-memory database");
        db.migrate_core().expect("core migrations");

        db.write_sync(|conn| {
            seed_graph_folder(conn);
            insert_message(
                conn,
                "old-draft",
                Some(synthetic_uid("old-draft")),
                Some("retired-graph-id"),
                true,
                Some("nuncio-draft-old@nuncio.local"),
            );
            conn.execute(
                "UPDATE message SET date = ?1 WHERE id = 'old-draft'",
                rusqlite::params![(chrono::Utc::now() - chrono::Duration::days(400)).to_rfc3339()],
            )?;

            // The scan returned nothing, but the row is outside the window and
            // therefore cannot be judged.
            let seen: HashSet<String> = HashSet::new();
            let removed = graph_prune_ids(conn, "acc", "fld", &seen, 30)?;
            assert!(
                removed.is_empty(),
                "an out-of-window row must not be pruned: {removed:?}"
            );
            Ok(())
        })
        .expect("keep out-of-window row");
    }

    #[test]
    fn graph_prune_keeps_rows_the_scan_returned() {
        let db = nuncio_core::db::Db::open_in_memory().expect("in-memory database");
        db.migrate_core().expect("core migrations");

        db.write_sync(|conn| {
            seed_graph_folder(conn);
            insert_message(
                conn,
                "graph-live",
                Some(synthetic_uid("graph-live")),
                Some("graph-live"),
                false,
                Some("<live@example.com>"),
            );

            let seen: HashSet<String> = ["graph-live".to_string()].into_iter().collect();
            let removed = graph_prune_ids(conn, "acc", "fld", &seen, 0)?;
            assert!(
                removed.is_empty(),
                "a message the scan returned must be kept: {removed:?}"
            );
            Ok(())
        })
        .expect("keep live graph row");
    }

    /// Neither the IMAP expunge diff nor the Graph prune may drop a row whose
    /// permanent delete the server has not confirmed yet.
    #[test]
    fn expunge_and_prune_skip_pending_permanent_deletes() {
        let db = nuncio_core::db::Db::open_in_memory().expect("in-memory database");
        db.migrate_core().expect("core migrations");

        db.write_sync(|conn| {
            seed_graph_folder(conn);
            insert_message(
                conn,
                "pending",
                Some(7),
                None,
                false,
                Some("<pending@example.com>"),
            );
            insert_message(
                conn,
                "normal",
                Some(8),
                None,
                false,
                Some("<normal@example.com>"),
            );
            repo::messages::hide_for_permanent_delete(conn, &["pending".to_string()])?;

            assert_eq!(
                local_uids(conn, "fld")?,
                vec![("normal".to_string(), 8)],
                "the expunge diff must not see the pending row"
            );

            let seen: HashSet<String> = HashSet::new();
            assert_eq!(
                graph_prune_ids(conn, "acc", "fld", &seen, 0)?,
                vec!["normal".to_string()],
                "the Graph prune must keep the pending row"
            );
            Ok(())
        })
        .expect("pending delete sync protection");
    }

    #[test]
    fn graph_sync_reuses_the_row_with_the_same_message_id() {
        let db = nuncio_core::db::Db::open_in_memory().expect("in-memory database");
        db.migrate_core().expect("core migrations");

        db.write_sync(|conn| {
            seed_graph_folder(conn);
            insert_message(
                conn,
                "draft-local",
                None,
                None,
                true,
                Some("nuncio-draft-abc@nuncio.local"),
            );

            assert_eq!(
                local_id_for_message_id(conn, "acc", "fld", "nuncio-draft-abc@nuncio.local", 1)?,
                Some("draft-local".to_string()),
                "the APPEND echo must update the composer's projection row"
            );
            assert_eq!(
                local_id_for_message_id(conn, "acc", "fld", "<unknown@example.com>", 1)?,
                None
            );
            Ok(())
        })
        .expect("reuse message id");
    }

    /// The Graph mapper must hand the reuse lookup a bare Message-ID.
    ///
    /// Graph returns `internetMessageId` wrapped in angle brackets while the
    /// composer's projection row is written with
    /// `nuncio-draft-<uuid>@nuncio.local`. The lookup is
    /// `WHERE message_id_header = ?`, an exact string comparison, so a
    /// bracketed value from `map_message` never matches: the APPEND echo would
    /// be inserted as a second row for the same draft. This drives the real
    /// mapper with a Graph-shaped payload and asserts the reuse actually fires.
    #[test]
    fn graph_echo_with_bracketed_message_id_reuses_the_draft_projection() {
        let db = nuncio_core::db::Db::open_in_memory().expect("in-memory database");
        db.migrate_core().expect("core migrations");

        db.write_sync(|conn| {
            seed_graph_folder(conn);
            // The projection `compose_save` writes before queuing the APPEND.
            insert_message(
                conn,
                "draft-local",
                None,
                None,
                true,
                Some("nuncio-draft-abc@nuncio.local"),
            );

            // The server copy of that draft, as Graph reports it.
            let echoed = crate::graph::messages::map_graph_json(
                serde_json::json!({
                    "id": "AAMkAD-draft-server-id",
                    "subject": "Draft subject",
                    "isDraft": true,
                    "receivedDateTime": "2026-11-16T08:00:00Z",
                    "internetMessageId": "<nuncio-draft-abc@nuncio.local>",
                }),
                "acc",
                "fld",
            )
            .expect("mapped");

            let header = echoed
                .message_id_header
                .as_deref()
                .expect("a draft always carries a Message-ID");
            assert_eq!(header, "nuncio-draft-abc@nuncio.local");
            // The second line of defence: recovering the local draft id from
            // the header only works on the bare form.
            assert_eq!(crate::util::draft_id_from_header(header), Some("abc"));

            let reused = local_id_for_message_id(conn, "acc", "fld", header, 1)?
                .expect("the APPEND echo must match the projection row");
            assert_eq!(reused, "draft-local");

            // Apply the same id swap the sync performs and check no second row
            // appears.
            let mut adopted = echoed;
            adopted.summary.id = reused;
            adopted.uid = Some(synthetic_uid(&adopted.summary.id));
            repo::messages::upsert(conn, &adopted)?;
            let rows: i64 = conn.query_row(
                "SELECT COUNT(*) FROM message WHERE message_id_header = ?1",
                rusqlite::params!["nuncio-draft-abc@nuncio.local"],
                |row| row.get(0),
            )?;
            assert_eq!(rows, 1, "one draft must stay one row");

            // Ordinary (non-draft) messages are normalized the same way, so a
            // re-sync after a Graph id change also lands on the existing row.
            insert_message(
                conn,
                "mail-local",
                None,
                None,
                false,
                Some("abc@example.com"),
            );
            let mailed = crate::graph::messages::map_graph_json(
                serde_json::json!({
                    "id": "AAMkAD-mail-server-id",
                    "receivedDateTime": "2026-11-16T08:00:00Z",
                    "internetMessageId": "<abc@example.com>",
                }),
                "acc",
                "fld",
            )
            .expect("mapped");
            assert_eq!(
                local_id_for_message_id(
                    conn,
                    "acc",
                    "fld",
                    mailed.message_id_header.as_deref().expect("header"),
                    1,
                )?,
                Some("mail-local".to_string())
            );
            Ok(())
        })
        .expect("reuse the bracketed graph message id");
    }

    /// An empty HEADER result, a failed search or a different generation must
    /// never erase a row; only a definitive absence in the same generation may.
    #[test]
    fn only_a_confirmed_uid_absence_erases_a_pending_delete() {
        use repo::messages::{RECONCILE_PENDING_DELETE, RECONCILE_REBIND};
        // An empty HEADER search leaves uid_state None: no erasure.
        assert!(!may_erase_pending_delete(
            RECONCILE_PENDING_DELETE,
            None,
            true
        ));
        // The search ran and found the UID: the message still exists.
        assert!(!may_erase_pending_delete(
            RECONCILE_PENDING_DELETE,
            Some(true),
            true
        ));
        // The generation changed, so the old UID means nothing.
        assert!(!may_erase_pending_delete(
            RECONCILE_PENDING_DELETE,
            Some(false),
            false
        ));
        // A move rollback is never erased by this path.
        assert!(!may_erase_pending_delete(
            RECONCILE_REBIND,
            Some(false),
            true
        ));
        // Only this combination is positive evidence.
        assert!(may_erase_pending_delete(
            RECONCILE_PENDING_DELETE,
            Some(false),
            true
        ));
    }

    /// Copies of one mail share a Message-ID but have different UIDs. Folding
    /// both into one row would clear the wrong copy's markers.
    #[test]
    fn duplicate_message_ids_are_not_folded_into_one_row() {
        let db = nuncio_core::db::Db::open_in_memory().expect("in-memory database");
        db.migrate_core().expect("core migrations");

        db.write_sync(|conn| {
            seed_graph_folder(conn);
            insert_message(
                conn,
                "copy-a",
                Some(1),
                Some("graph-a"),
                false,
                Some("<same@example.com>"),
            );
            insert_message(
                conn,
                "copy-b",
                Some(2),
                Some("graph-b"),
                false,
                Some("<same@example.com>"),
            );

            assert_eq!(
                local_id_for_message_id(conn, "acc", "fld", "<same@example.com>", 2)?,
                None,
                "two copies must never be folded together"
            );
            assert_eq!(
                local_id_for_message_id(conn, "acc", "fld", "<same@example.com>", 1)?,
                None,
                "a single incoming copy is still ambiguous with two local rows"
            );
            conn.execute("DELETE FROM message WHERE id = 'copy-b'", [])?;
            assert_eq!(
                local_id_for_message_id(conn, "acc", "fld", "<same@example.com>", 1)?,
                Some("copy-a".to_string()),
                "one row on each side is unambiguous"
            );
            Ok(())
        })
        .expect("ambiguous message ids");
    }

    /// A refused Graph permanent delete keeps the synthetic UID, and an
    /// unchanged mailbox returns an *empty* delta. The row must stay
    /// actionable without waiting for an entry that will never arrive.
    #[tokio::test]
    async fn graph_empty_delta_keeps_a_restored_row_actionable() {
        use nuncio_core::models::{
            Account, AccountState, AuthKind, OAuthProvider, ProviderCapabilities, ProviderKind,
        };

        let dir = std::env::temp_dir().join(format!(
            "nuncio-graph-empty-delta-{}",
            nuncio_core::models::new_id()
        ));
        let ctx = nuncio_core::CoreContext::for_test(nuncio_core::Paths::new(dir))
            .await
            .expect("core context");
        ctx.db
            .write(|db| {
                seed_graph_folder(db);
                // The state `release_pending_delete_preserving_identity`
                // leaves behind: unhidden, full identity, no pending markers.
                insert_message(
                    db,
                    "m1",
                    Some(42),
                    Some("AAMk-graph-id"),
                    false,
                    Some("<m1@example.com>"),
                );
                Ok(())
            })
            .await
            .expect("seed");

        let folder = ctx
            .db
            .read(|db| repo::folders::get_required(db, "fld"))
            .await
            .expect("folder");
        let account = Account {
            id: "acc".into(),
            provider: ProviderKind::Outlook,
            email: "me@example.com".into(),
            display_name: "Me".into(),
            color: "#336699".into(),
            auth_kind: AuthKind::Oauth,
            oauth_provider: Some(OAuthProvider::Microsoft),
            imap: None,
            smtp: None,
            pop3: None,
            caldav_url: None,
            carddav_url: None,
            sent_copy: None,
            sync_window_days: 0,
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

        apply_graph_delta(
            &ctx,
            &account,
            &folder,
            crate::graph::messages::MessageDelta {
                messages: Vec::new(),
                delta_link: Some("unchanged-cursor".into()),
                pending_cursor: crate::graph::messages::PendingCursor::Keep,
                adopted_local_ids: std::collections::HashMap::new(),
                full_sync: false,
                removed_remote_ids: Vec::new(),
            },
        )
        .await
        .expect("empty delta");

        let stored = ctx
            .db
            .read(|db| repo::messages::get(db, "m1"))
            .await
            .expect("read")
            .expect("the row survives an empty delta");
        assert_eq!(stored.uid, Some(42));
        assert_eq!(stored.gm_msgid.as_deref(), Some("AAMk-graph-id"));
        assert!(stored.summary.reconcile_reason.is_none());
    }

    /// Account/folder/context for the Graph delta regressions below.
    async fn graph_delta_fixture() -> (
        nuncio_core::CoreContext,
        nuncio_core::models::Account,
        Folder,
    ) {
        use nuncio_core::models::{
            Account, AccountState, AuthKind, OAuthProvider, ProviderCapabilities, ProviderKind,
        };

        let dir = std::env::temp_dir().join(format!(
            "nuncio-graph-delta-{}",
            nuncio_core::models::new_id()
        ));
        let ctx = nuncio_core::CoreContext::for_test(nuncio_core::Paths::new(dir))
            .await
            .expect("core context");
        ctx.db
            .write(|db| {
                seed_graph_folder(db);
                Ok(())
            })
            .await
            .expect("seed");
        let folder = ctx
            .db
            .read(|db| repo::folders::get_required(db, "fld"))
            .await
            .expect("folder");
        let account = Account {
            id: "acc".into(),
            provider: ProviderKind::Outlook,
            email: "me@example.com".into(),
            display_name: "Me".into(),
            color: "#336699".into(),
            auth_kind: AuthKind::Oauth,
            oauth_provider: Some(OAuthProvider::Microsoft),
            imap: None,
            smtp: None,
            pop3: None,
            caldav_url: None,
            carddav_url: None,
            sent_copy: None,
            sync_window_days: 0,
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
        (ctx, account, folder)
    }

    /// The state an optimistic move leaves behind: the row is already in the
    /// target folder but has no folder-local identity yet.
    async fn insert_pending_projection(ctx: &nuncio_core::CoreContext, id: &str, header: &str) {
        let id = id.to_string();
        let header = header.to_string();
        ctx.db
            .write(move |db| {
                let now = nuncio_core::models::now().to_rfc3339();
                db.execute(
                    "INSERT INTO message (
                         id, account_id, folder_id, uid, gm_msgid, message_id_header, thread_id,
                         subject, date, preview, seen, flagged, answered, forwarded, is_draft,
                         has_attachments, importance, size, target_pending, awaiting_reconcile,
                         created_at, updated_at
                     ) VALUES (?1, 'acc', 'fld', NULL, NULL, ?2, 'thread', 'subject', ?3, '',
                               1, 0, 0, 0, 0, 0, 'normal', 0, 1, 1, ?3, ?3)",
                    rusqlite::params![id, header, now],
                )?;
                Ok(())
            })
            .await
            .expect("insert pending projection");
    }

    /// A completed Graph move as the executor leaves it behind: the payload
    /// retains the confirmed destination id after the queue row settles.
    ///
    /// Returns the queued operation id so a test can also settle it the way
    /// the executor does before the destination folder is synchronized.
    async fn insert_confirmed_move_op(
        ctx: &nuncio_core::CoreContext,
        message_id: &str,
        target_remote_id: &str,
    ) -> String {
        use crate::ops::{MoveOp, OpMessageOutcome, OpMessageState, KIND_MOVE};
        let payload = MoveOp {
            folder_id: "src".into(),
            target_folder_id: "fld".into(),
            uids: vec![7],
            source_uidvalidity: None,
            message_ids: vec![message_id.to_string()],
            outcomes: vec![OpMessageOutcome {
                message_id: message_id.to_string(),
                source_uid: Some(7),
                state: OpMessageState::Confirmed,
                target_folder_id: Some("fld".into()),
                target_uid: None,
                target_uidvalidity: None,
                target_remote_id: Some(target_remote_id.to_string()),
                attempt_remote_id: None,
                copy_phase: Default::default(),
                delete_flag_phase: Default::default(),
                expunge_phase: Default::default(),
            }],
            remote_ids: Vec::new(),
        };
        ctx.db
            .write(move |db| {
                let op = repo::pending_ops::enqueue(
                    db,
                    nuncio_core::models::NewPendingOp::new(
                        "acc",
                        KIND_MOVE,
                        serde_json::to_value(payload).expect("move payload"),
                        "Confirmed move",
                    ),
                )?;
                Ok(op.id)
            })
            .await
            .expect("enqueue confirmed move")
    }

    /// Settle a confirmed move the way the executor does immediately after the
    /// wire command succeeds. The projection is still `target_pending` at this
    /// point, so the protection has to outlive the queue row's `pending`
    /// state.
    async fn settle_move_op(ctx: &nuncio_core::CoreContext, op_id: &str) {
        let op_id = op_id.to_string();
        ctx.db
            .write(move |db| {
                repo::pending_ops::claim_for_execution(db, &op_id)?;
                repo::pending_ops::finish(db, &op_id)?;
                Ok(())
            })
            .await
            .expect("settle move op");
    }

    /// Mirror a confirmed move destination onto the projection row, as the
    /// executor does in the same write that records the outcome.
    async fn set_row_move_target(
        ctx: &nuncio_core::CoreContext,
        message_id: &str,
        target_remote_id: &str,
    ) {
        set_row_move_target_owned(ctx, message_id, target_remote_id, "row-owner", 1).await;
    }

    /// The same write, naming the operation that owns the identity.
    async fn set_row_move_target_owned(
        ctx: &nuncio_core::CoreContext,
        message_id: &str,
        target_remote_id: &str,
        owner_op_id: &str,
        owner_sequence: i64,
    ) {
        let message_id = message_id.to_string();
        let target_remote_id = target_remote_id.to_string();
        let owner_op_id = owner_op_id.to_string();
        ctx.db
            .write(move |db| {
                repo::messages::set_move_target_identity(
                    db,
                    "acc",
                    &message_id,
                    "fld",
                    &target_remote_id,
                    &owner_op_id,
                    owner_sequence,
                )?;
                Ok(())
            })
            .await
            .expect("persist row move target");
    }

    /// The old batch retry path: mirror a destination recorded in the
    /// operation payload onto the row. Returns the rows changed, so a caller
    /// can assert that newer evidence was not overwritten.
    async fn backfill_row_move_target(
        ctx: &nuncio_core::CoreContext,
        message_id: &str,
        target_remote_id: &str,
        owner_op_id: &str,
        owner_sequence: i64,
    ) -> usize {
        let message_id = message_id.to_string();
        let target_remote_id = target_remote_id.to_string();
        let owner_op_id = owner_op_id.to_string();
        ctx.db
            .write(move |db| {
                repo::messages::set_move_target_identity(
                    db,
                    "acc",
                    &message_id,
                    "fld",
                    &target_remote_id,
                    &owner_op_id,
                    owner_sequence,
                )
            })
            .await
            .expect("backfill row move target")
    }

    /// Publish the durable `graph_remote:{folder}:{uid}` alias the executor
    /// writes when a move command succeeds.
    async fn set_graph_alias(ctx: &nuncio_core::CoreContext, message_id: &str, remote_id: &str) {
        let key = format!(
            "graph_remote:fld:{}",
            crate::sync::messages::synthetic_uid(message_id)
        );
        let remote_id = remote_id.to_string();
        ctx.db
            .write(move |db| repo::sync_state::set(db, "acc", "mail", &key, &remote_id))
            .await
            .expect("set graph alias");
    }

    /// The number of rows matching `id` in the fixture account.
    async fn message_row_count(ctx: &nuncio_core::CoreContext, id: &str) -> i64 {
        let id = id.to_string();
        ctx.db
            .read(move |db| {
                Ok(db.query_row(
                    "SELECT COUNT(*) FROM message WHERE account_id = 'acc' AND id = ?1",
                    rusqlite::params![id],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("row count")
    }

    /// Run housekeeping far enough into the future to prune every settled
    /// queue row (subject to its retention guards).
    async fn prune_finished_ops(ctx: &nuncio_core::CoreContext) {
        ctx.db
            .write(|db| {
                repo::pending_ops::prune_finished(db, Utc::now() + chrono::Duration::days(1))?;
                Ok(())
            })
            .await
            .expect("prune finished ops");
    }

    /// Read the temporary confirmed-move identity stored on a row.
    async fn row_move_target(
        ctx: &nuncio_core::CoreContext,
        id: &str,
    ) -> (Option<String>, Option<String>) {
        let id = id.to_string();
        ctx.db
            .read(move |db| {
                Ok(db.query_row(
                    "SELECT move_target_remote_id, move_target_folder_id
                       FROM message WHERE id = ?1",
                    rusqlite::params![id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            })
            .await
            .expect("read row move target")
    }

    /// Read `(uid, gm_msgid, target_pending)` of one row.
    async fn pending_projection_state(
        ctx: &nuncio_core::CoreContext,
        id: &str,
    ) -> (Option<i64>, Option<String>, i64) {
        let id = id.to_string();
        ctx.db
            .read(move |db| {
                Ok(db.query_row(
                    "SELECT uid, gm_msgid, target_pending FROM message WHERE id = ?1",
                    rusqlite::params![id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?)
            })
            .await
            .expect("read projection state")
    }

    /// Regression: a message moved back into its original folder keeps its
    /// primary key equal to the Graph id it had there. While the row waits for
    /// the move's new target identity (`target_pending`, no uid/gm_msgid), a
    /// delta that still names that *old* id must not delete the projection.
    #[tokio::test]
    async fn graph_pending_projection_survives_stale_original_id_removal() {
        let (ctx, account, folder) = graph_delta_fixture().await;
        insert_pending_projection(&ctx, "old-graph-id", "same@example.com").await;

        apply_graph_delta(
            &ctx,
            &account,
            &folder,
            crate::graph::messages::MessageDelta {
                messages: Vec::new(),
                delta_link: Some("cursor-1".into()),
                pending_cursor: crate::graph::messages::PendingCursor::Keep,
                adopted_local_ids: std::collections::HashMap::from([(
                    "m-new".to_string(),
                    "old-graph-id".to_string(),
                )]),
                full_sync: false,
                removed_remote_ids: vec!["old-graph-id".into()],
            },
        )
        .await
        .expect("delta");

        let (uid, gm_msgid, target_pending) = pending_projection_state(&ctx, "old-graph-id").await;
        assert_eq!(
            (uid, gm_msgid, target_pending),
            (None, None, 1),
            "the old id must not resolve the projection through its primary key"
        );
        let rows: i64 = ctx
            .db
            .read(|db| {
                Ok(db.query_row(
                    "SELECT COUNT(*) FROM message WHERE account_id = 'acc'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("row count");
        assert_eq!(rows, 1, "the projection is not deleted");
        // The removal really named another copy: the cursor may advance, but
        // no local row (and therefore no message) was silently dropped.
        let cursor = ctx
            .db
            .read(|db| {
                repo::sync_state::get(
                    db,
                    "acc",
                    "mail",
                    &crate::graph::messages::delta_cursor_key("fld"),
                )
            })
            .await
            .expect("cursor");
        assert_eq!(cursor.as_deref(), Some("cursor-1"));
    }

    /// Regression: the same stale old id arriving as an *update* must not bind
    /// the projection back to it (which would clear `target_pending` and make
    /// the row forget the move it is still waiting for).
    #[tokio::test]
    async fn graph_pending_projection_ignores_stale_original_id_update() {
        let (ctx, account, folder) = graph_delta_fixture().await;
        insert_pending_projection(&ctx, "old-graph-id", "same@example.com").await;
        let stale = crate::graph::messages::map_graph_json(
            serde_json::json!({
                "id": "old-graph-id",
                "subject": "Stale copy",
                "receivedDateTime": "2026-11-16T08:00:00Z",
                "internetMessageId": "<same@example.com>",
            }),
            "acc",
            "fld",
        )
        .expect("mapped");

        apply_graph_delta(
            &ctx,
            &account,
            &folder,
            crate::graph::messages::MessageDelta {
                messages: vec![stale],
                delta_link: Some("cursor-2".into()),
                pending_cursor: crate::graph::messages::PendingCursor::Keep,
                adopted_local_ids: std::collections::HashMap::from([(
                    "m-new".to_string(),
                    "old-graph-id".to_string(),
                )]),
                full_sync: false,
                removed_remote_ids: Vec::new(),
            },
        )
        .await
        .expect("delta");

        let (uid, gm_msgid, target_pending) = pending_projection_state(&ctx, "old-graph-id").await;
        assert_eq!(
            (uid, gm_msgid, target_pending),
            (None, None, 1),
            "the stale id must not be written back onto the pending projection"
        );
        let rows: i64 = ctx
            .db
            .read(|db| {
                Ok(db.query_row(
                    "SELECT COUNT(*) FROM message WHERE account_id = 'acc'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("row count");
        assert_eq!(rows, 1, "no second row is created");
        let cursor = ctx
            .db
            .read(|db| {
                repo::sync_state::get(
                    db,
                    "acc",
                    "mail",
                    &crate::graph::messages::delta_cursor_key("fld"),
                )
            })
            .await
            .expect("cursor");
        assert_eq!(cursor.as_deref(), Some("cursor-2"));
    }

    /// Regression: another Graph copy sharing the Message-ID must not consume
    /// a pending projection that a confirmed move receipt already owns. The
    /// copy stays an independent row; the projection still waits for `m-new`.
    #[tokio::test]
    async fn graph_message_id_heuristic_keeps_an_adopted_pending_projection() {
        let (ctx, account, folder) = graph_delta_fixture().await;
        insert_pending_projection(&ctx, "local-proj", "shared@example.com").await;
        let copy = crate::graph::messages::map_graph_json(
            serde_json::json!({
                "id": "copy-2",
                "subject": "Independent copy",
                "receivedDateTime": "2026-11-16T08:00:00Z",
                "internetMessageId": "<shared@example.com>",
            }),
            "acc",
            "fld",
        )
        .expect("mapped");

        apply_graph_delta(
            &ctx,
            &account,
            &folder,
            crate::graph::messages::MessageDelta {
                messages: vec![copy],
                delta_link: Some("cursor-1".into()),
                pending_cursor: crate::graph::messages::PendingCursor::Keep,
                adopted_local_ids: std::collections::HashMap::from([(
                    "m-new".to_string(),
                    "local-proj".to_string(),
                )]),
                full_sync: false,
                removed_remote_ids: Vec::new(),
            },
        )
        .await
        .expect("delta with the other copy");

        let (uid, gm_msgid, target_pending) = pending_projection_state(&ctx, "local-proj").await;
        assert_eq!(
            (uid, gm_msgid, target_pending),
            (None, None, 1),
            "the projection still waits for its adopted id"
        );
        let (copy_uid, copy_remote, copy_pending) = pending_projection_state(&ctx, "copy-2").await;
        assert!(copy_uid.is_some(), "the copy is imported as a real row");
        assert_eq!(copy_remote.as_deref(), Some("copy-2"));
        assert_eq!(copy_pending, 0);

        // The confirmed target still binds to the projection afterwards.
        let adopted = crate::graph::messages::map_graph_json(
            serde_json::json!({
                "id": "m-new",
                "subject": "Moved message",
                "receivedDateTime": "2026-11-16T08:00:00Z",
                "internetMessageId": "<shared@example.com>",
            }),
            "acc",
            "fld",
        )
        .expect("mapped");
        apply_graph_delta(
            &ctx,
            &account,
            &folder,
            crate::graph::messages::MessageDelta {
                messages: vec![adopted],
                delta_link: Some("cursor-2".into()),
                pending_cursor: crate::graph::messages::PendingCursor::Keep,
                adopted_local_ids: std::collections::HashMap::from([(
                    "m-new".to_string(),
                    "local-proj".to_string(),
                )]),
                full_sync: false,
                removed_remote_ids: Vec::new(),
            },
        )
        .await
        .expect("delta with the adopted id");

        let (uid, gm_msgid, target_pending) = pending_projection_state(&ctx, "local-proj").await;
        assert!(uid.is_some(), "the adopted id binds the projection");
        assert_eq!(gm_msgid.as_deref(), Some("m-new"));
        assert_eq!(target_pending, 0);
        let rows: i64 = ctx
            .db
            .read(|db| {
                Ok(db.query_row(
                    "SELECT COUNT(*) FROM message WHERE account_id = 'acc'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("row count");
        assert_eq!(rows, 2, "the copy and the projection both survive");
    }

    /// Regression: the confirmed move target `m-new` has not appeared in a
    /// delta yet. An update carrying the projection's *old* Graph id arrives
    /// first. Protection must come from the durable pending-op receipt, not
    /// from the page: the row must not be rebound and `target_pending` must
    /// stay set so `m-new` can still claim it.
    #[tokio::test]
    async fn graph_confirmed_move_protects_projection_before_target_delta() {
        let (ctx, account, folder) = graph_delta_fixture().await;
        insert_pending_projection(&ctx, "old-graph-id", "same@example.com").await;
        let op_id = insert_confirmed_move_op(&ctx, "old-graph-id", "m-new").await;
        // The executor settles the row right after the move lands; the
        // destination folder is synced later, so protection must not depend on
        // the operation being outstanding.
        settle_move_op(&ctx, &op_id).await;
        let stale = crate::graph::messages::map_graph_json(
            serde_json::json!({
                "id": "old-graph-id",
                "subject": "Stale copy",
                "receivedDateTime": "2026-11-16T08:00:00Z",
                "internetMessageId": "<same@example.com>",
            }),
            "acc",
            "fld",
        )
        .expect("mapped");

        apply_graph_delta(
            &ctx,
            &account,
            &folder,
            crate::graph::messages::MessageDelta {
                messages: vec![stale],
                delta_link: Some("cursor-1".into()),
                pending_cursor: crate::graph::messages::PendingCursor::Keep,
                // `m-new` is not in this batch, so the page carries no mapping.
                adopted_local_ids: std::collections::HashMap::new(),
                full_sync: false,
                removed_remote_ids: Vec::new(),
            },
        )
        .await
        .expect("delta");

        let (uid, gm_msgid, target_pending) = pending_projection_state(&ctx, "old-graph-id").await;
        assert_eq!(
            (uid, gm_msgid, target_pending),
            (None, None, 1),
            "the stale old id must not be written back onto the projection"
        );
        let rows: i64 = ctx
            .db
            .read(|db| {
                Ok(db.query_row(
                    "SELECT COUNT(*) FROM message WHERE account_id = 'acc'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("row count");
        assert_eq!(rows, 1, "no second row is created for the stale id");
    }

    /// Regression: while the confirmed target `m-new` is still absent, the
    /// destination delta reports the projection's *old* Graph id as removed.
    /// The durable receipt must keep the projection alive; the cursor may
    /// advance, but the row (and its `target_pending` state) stays.
    #[tokio::test]
    async fn graph_confirmed_move_ignores_stale_old_id_removal_before_target_delta() {
        let (ctx, account, folder) = graph_delta_fixture().await;
        insert_pending_projection(&ctx, "old-graph-id", "same@example.com").await;
        insert_confirmed_move_op(&ctx, "old-graph-id", "m-new").await;

        apply_graph_delta(
            &ctx,
            &account,
            &folder,
            crate::graph::messages::MessageDelta {
                messages: Vec::new(),
                delta_link: Some("cursor-1".into()),
                pending_cursor: crate::graph::messages::PendingCursor::Keep,
                // `m-new` is not in this batch, so the page carries no mapping.
                adopted_local_ids: std::collections::HashMap::new(),
                full_sync: false,
                removed_remote_ids: vec!["old-graph-id".into()],
            },
        )
        .await
        .expect("delta");

        let (uid, gm_msgid, target_pending) = pending_projection_state(&ctx, "old-graph-id").await;
        assert_eq!(
            (uid, gm_msgid, target_pending),
            (None, None, 1),
            "the old id must not delete the projection through its primary key"
        );
        let rows: i64 = ctx
            .db
            .read(|db| {
                Ok(db.query_row(
                    "SELECT COUNT(*) FROM message WHERE account_id = 'acc'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("row count");
        assert_eq!(rows, 1, "the projection is not deleted");
    }

    /// Regression: with the same outstanding confirmed move (`m-new` absent
    /// from the delta), a *different* Graph id carrying the same Message-ID
    /// arrives. It must become its own row; the projection keeps waiting for
    /// `m-new` instead of being consumed by the heuristic.
    #[tokio::test]
    async fn graph_confirmed_move_keeps_independent_copy_before_target_delta() {
        let (ctx, account, folder) = graph_delta_fixture().await;
        insert_pending_projection(&ctx, "local-proj", "shared@example.com").await;
        insert_confirmed_move_op(&ctx, "local-proj", "m-new").await;
        let copy = crate::graph::messages::map_graph_json(
            serde_json::json!({
                "id": "copy-2",
                "subject": "Independent copy",
                "receivedDateTime": "2026-11-16T08:00:00Z",
                "internetMessageId": "<shared@example.com>",
            }),
            "acc",
            "fld",
        )
        .expect("mapped");

        apply_graph_delta(
            &ctx,
            &account,
            &folder,
            crate::graph::messages::MessageDelta {
                messages: vec![copy],
                delta_link: Some("cursor-1".into()),
                pending_cursor: crate::graph::messages::PendingCursor::Keep,
                // `m-new` is not in this batch, so the page carries no mapping.
                adopted_local_ids: std::collections::HashMap::new(),
                full_sync: false,
                removed_remote_ids: Vec::new(),
            },
        )
        .await
        .expect("delta with the other copy");

        let (uid, gm_msgid, target_pending) = pending_projection_state(&ctx, "local-proj").await;
        assert_eq!(
            (uid, gm_msgid, target_pending),
            (None, None, 1),
            "the projection still waits for its confirmed target m-new"
        );
        let (copy_uid, copy_remote, copy_pending) = pending_projection_state(&ctx, "copy-2").await;
        assert!(copy_uid.is_some(), "the copy is imported as a real row");
        assert_eq!(copy_remote.as_deref(), Some("copy-2"));
        assert_eq!(copy_pending, 0);
        let rows: i64 = ctx
            .db
            .read(|db| {
                Ok(db.query_row(
                    "SELECT COUNT(*) FROM message WHERE account_id = 'acc'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("row count");
        assert_eq!(rows, 2, "the copy and the projection both survive");
    }

    /// Regression: the confirmed destination lives on the projection row, so
    /// protection must outlive the operation queue. With `m-new` still absent
    /// from the delta and the confirming operation already pruned, a stale
    /// update carrying the row's old id must not bind it back.
    #[tokio::test]
    async fn graph_confirmed_move_row_identity_survives_pruned_op() {
        let (ctx, account, folder) = graph_delta_fixture().await;
        insert_pending_projection(&ctx, "old-graph-id", "same@example.com").await;
        let op_id = insert_confirmed_move_op(&ctx, "old-graph-id", "m-new").await;
        set_row_move_target(&ctx, "old-graph-id", "m-new").await;
        settle_move_op(&ctx, &op_id).await;
        prune_finished_ops(&ctx).await;
        let op = ctx
            .db
            .read({
                let op_id = op_id.clone();
                move |db| repo::pending_ops::get(db, &op_id)
            })
            .await
            .expect("read op");
        assert!(op.is_none(), "the settled operation row is pruned");

        let stale = crate::graph::messages::map_graph_json(
            serde_json::json!({
                "id": "old-graph-id",
                "subject": "Stale copy",
                "receivedDateTime": "2026-11-16T08:00:00Z",
                "internetMessageId": "<same@example.com>",
            }),
            "acc",
            "fld",
        )
        .expect("mapped");

        apply_graph_delta(
            &ctx,
            &account,
            &folder,
            crate::graph::messages::MessageDelta {
                messages: vec![stale],
                delta_link: Some("cursor-1".into()),
                pending_cursor: crate::graph::messages::PendingCursor::Keep,
                adopted_local_ids: std::collections::HashMap::new(),
                full_sync: false,
                removed_remote_ids: Vec::new(),
            },
        )
        .await
        .expect("delta");

        let (uid, gm_msgid, target_pending) = pending_projection_state(&ctx, "old-graph-id").await;
        assert_eq!(
            (uid, gm_msgid, target_pending),
            (None, None, 1),
            "the row-level receipt must protect the projection after the op is pruned"
        );
        assert_eq!(
            row_move_target(&ctx, "old-graph-id").await,
            (Some("m-new".to_string()), Some("fld".to_string())),
            "the row still waits for m-new"
        );
        let rows: i64 = ctx
            .db
            .read(|db| {
                Ok(db.query_row(
                    "SELECT COUNT(*) FROM message WHERE account_id = 'acc'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("row count");
        assert_eq!(rows, 1, "the stale id does not become a second row");
    }

    /// Regression: with the operation row pruned, a different Graph id sharing
    /// the Message-ID must still be imported as its own row. The row-level
    /// receipt keeps the projection reserved for `m-new`.
    #[tokio::test]
    async fn graph_confirmed_move_row_identity_keeps_independent_copy_after_prune() {
        let (ctx, account, folder) = graph_delta_fixture().await;
        insert_pending_projection(&ctx, "local-proj", "shared@example.com").await;
        let op_id = insert_confirmed_move_op(&ctx, "local-proj", "m-new").await;
        set_row_move_target(&ctx, "local-proj", "m-new").await;
        settle_move_op(&ctx, &op_id).await;
        prune_finished_ops(&ctx).await;
        let copy = crate::graph::messages::map_graph_json(
            serde_json::json!({
                "id": "copy-2",
                "subject": "Independent copy",
                "receivedDateTime": "2026-11-16T08:00:00Z",
                "internetMessageId": "<shared@example.com>",
            }),
            "acc",
            "fld",
        )
        .expect("mapped");

        apply_graph_delta(
            &ctx,
            &account,
            &folder,
            crate::graph::messages::MessageDelta {
                messages: vec![copy],
                delta_link: Some("cursor-1".into()),
                pending_cursor: crate::graph::messages::PendingCursor::Keep,
                adopted_local_ids: std::collections::HashMap::new(),
                full_sync: false,
                removed_remote_ids: Vec::new(),
            },
        )
        .await
        .expect("delta with the other copy");

        let (uid, gm_msgid, target_pending) = pending_projection_state(&ctx, "local-proj").await;
        assert_eq!(
            (uid, gm_msgid, target_pending),
            (None, None, 1),
            "the projection still waits for its confirmed target"
        );
        let (copy_uid, copy_remote, copy_pending) = pending_projection_state(&ctx, "copy-2").await;
        assert!(copy_uid.is_some(), "the copy is imported as a real row");
        assert_eq!(copy_remote.as_deref(), Some("copy-2"));
        assert_eq!(copy_pending, 0);
        let rows: i64 = ctx
            .db
            .read(|db| {
                Ok(db.query_row(
                    "SELECT COUNT(*) FROM message WHERE account_id = 'acc'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("row count");
        assert_eq!(rows, 2, "the copy and the projection both survive");
    }

    /// The temporary identity is cleared when `m-new` finally binds the row,
    /// and the row then behaves exactly as before: a later stale update of the
    /// old id cannot replace the live binding.
    #[tokio::test]
    async fn graph_confirmed_move_binding_clears_row_identity() {
        let (ctx, account, folder) = graph_delta_fixture().await;
        insert_pending_projection(&ctx, "old-graph-id", "same@example.com").await;
        let op_id = insert_confirmed_move_op(&ctx, "old-graph-id", "m-new").await;
        set_row_move_target(&ctx, "old-graph-id", "m-new").await;
        settle_move_op(&ctx, &op_id).await;
        prune_finished_ops(&ctx).await;

        let adopted = crate::graph::messages::map_graph_json(
            serde_json::json!({
                "id": "m-new",
                "subject": "Moved message",
                "receivedDateTime": "2026-11-16T08:00:00Z",
                "internetMessageId": "<same@example.com>",
            }),
            "acc",
            "fld",
        )
        .expect("mapped");
        apply_graph_delta(
            &ctx,
            &account,
            &folder,
            crate::graph::messages::MessageDelta {
                messages: vec![adopted],
                delta_link: Some("cursor-1".into()),
                pending_cursor: crate::graph::messages::PendingCursor::Keep,
                // Derived from the row identity, not the pruned operation.
                adopted_local_ids: std::collections::HashMap::new(),
                full_sync: false,
                removed_remote_ids: Vec::new(),
            },
        )
        .await
        .expect("delta binds the projection");

        let (uid, gm_msgid, target_pending) = pending_projection_state(&ctx, "old-graph-id").await;
        assert!(uid.is_some(), "the projection is bound");
        assert_eq!(gm_msgid.as_deref(), Some("m-new"));
        assert_eq!(target_pending, 0);
        assert_eq!(
            row_move_target(&ctx, "old-graph-id").await,
            (None, None),
            "the temporary identity is cleared with the binding"
        );

        // Post-binding behaviour is unchanged: the old id is a superseded
        // binding and must not replace the live one.
        let stale = crate::graph::messages::map_graph_json(
            serde_json::json!({
                "id": "old-graph-id",
                "subject": "Stale copy",
                "receivedDateTime": "2026-11-16T08:00:00Z",
                "internetMessageId": "<same@example.com>",
            }),
            "acc",
            "fld",
        )
        .expect("mapped");
        apply_graph_delta(
            &ctx,
            &account,
            &folder,
            crate::graph::messages::MessageDelta {
                messages: vec![stale],
                delta_link: Some("cursor-2".into()),
                pending_cursor: crate::graph::messages::PendingCursor::Keep,
                adopted_local_ids: std::collections::HashMap::new(),
                full_sync: false,
                removed_remote_ids: Vec::new(),
            },
        )
        .await
        .expect("stale delta after binding");

        let (_, gm_msgid, target_pending) = pending_projection_state(&ctx, "old-graph-id").await;
        assert_eq!(gm_msgid.as_deref(), Some("m-new"));
        assert_eq!(target_pending, 0);
        let rows: i64 = ctx
            .db
            .read(|db| {
                Ok(db.query_row(
                    "SELECT COUNT(*) FROM message WHERE account_id = 'acc'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("row count");
        assert_eq!(rows, 1, "the superseded id does not add a row");
    }

    /// Regression: the row was already claimed by a newer trip that recorded
    /// destination `m-new2`. A retry of an older batch must not backfill its
    /// stale `m-new1` over that identity, because an `@removed` for `m-new1`
    /// would then delete the projection of the new copy.
    #[tokio::test]
    async fn graph_old_batch_backfill_does_not_overwrite_newer_trip_identity() {
        let (ctx, account, folder) = graph_delta_fixture().await;
        insert_pending_projection(&ctx, "proj", "same@example.com").await;
        // The older trip still has a retained confirmed receipt for m-new1.
        let older = insert_confirmed_move_op(&ctx, "proj", "m-new1").await;
        // The newer trip owns the row and wrote m-new2 onto it.
        let newer = insert_confirmed_move_op(&ctx, "proj", "m-new2").await;
        set_row_move_target_owned(&ctx, "proj", "m-new2", &newer, 2).await;

        let changed = backfill_row_move_target(&ctx, "proj", "m-new1", &older, 1).await;
        assert_eq!(
            changed, 0,
            "the older batch must not replace the newer trip's destination"
        );
        assert_eq!(
            row_move_target(&ctx, "proj").await,
            (Some("m-new2".to_string()), Some("fld".to_string())),
            "the row still waits for the newer trip's copy"
        );
        // The collection layer must hand the writer only the newer trip's
        // destination, never both.
        let cached = crate::graph::messages::cached_identities(&ctx, "acc", "fld")
            .await
            .expect("cached identities");
        assert_eq!(
            cached.adopted_local_ids.get("m-new2").map(String::as_str),
            Some("proj")
        );
        assert!(
            !cached.adopted_local_ids.contains_key("m-new1"),
            "the old batch's destination is not adopted for the row"
        );

        // The old copy is reported removed. It must not resolve the projection.
        apply_graph_delta(
            &ctx,
            &account,
            &folder,
            crate::graph::messages::MessageDelta {
                messages: Vec::new(),
                delta_link: Some("cursor-1".into()),
                pending_cursor: crate::graph::messages::PendingCursor::Keep,
                adopted_local_ids: std::collections::HashMap::new(),
                full_sync: false,
                removed_remote_ids: vec!["m-new1".into()],
            },
        )
        .await
        .expect("delta removing the old copy");

        assert_eq!(
            message_row_count(&ctx, "proj").await,
            1,
            "the old id's @removed must not delete the new projection"
        );
        let (uid, gm_msgid, target_pending) = pending_projection_state(&ctx, "proj").await;
        assert_eq!((uid, gm_msgid, target_pending), (None, None, 1));
        assert_eq!(
            row_move_target(&ctx, "proj").await,
            (Some("m-new2".to_string()), Some("fld".to_string())),
            "the newer destination survives the stale removal"
        );
    }

    /// Regression: the newer trip's confirmation is on the row (`m-new2`) and
    /// its settled operation has since been housekept, while an older trip's
    /// confirmed receipt (`m-old`) and its `graph_remote` alias are still
    /// retained. The historical receipt must not back the alias next to the
    /// row's new target, so an `@removed` for `m-old` must not delete the
    /// projection.
    #[tokio::test]
    async fn graph_historical_receipt_does_not_back_stale_alias() {
        let (ctx, account, folder) = graph_delta_fixture().await;
        insert_pending_projection(&ctx, "proj", "same@example.com").await;
        // Historical receipt from an earlier trip into this folder.
        let older = insert_confirmed_move_op(&ctx, "proj", "m-old").await;
        // The newer trip confirmed m-new2 and wrote it onto the row.
        let newer = insert_confirmed_move_op(&ctx, "proj", "m-new2").await;
        set_row_move_target_owned(&ctx, "proj", "m-new2", &newer, 2).await;
        settle_move_op(&ctx, &newer).await;
        prune_finished_ops(&ctx).await;

        let newer_row = ctx
            .db
            .read({
                let newer = newer.clone();
                move |db| repo::pending_ops::get(db, &newer)
            })
            .await
            .expect("read newer op");
        assert!(newer_row.is_none(), "the newer receipt is housekept");
        let older_row = ctx
            .db
            .read({
                let older = older.clone();
                move |db| repo::pending_ops::get(db, &older)
            })
            .await
            .expect("read older op");
        assert!(
            older_row.is_some(),
            "the historical receipt is retained because it disagrees with the row identity"
        );

        // The durable alias still names the earlier trip's destination.
        set_graph_alias(&ctx, "proj", "m-old").await;
        assert_eq!(
            row_move_target(&ctx, "proj").await,
            (Some("m-new2".to_string()), Some("fld".to_string())),
            "the current row identity is the newer target"
        );

        // The collection layer must hand the writer only the row's identity,
        // never the historical alias alongside it.
        let cached = crate::graph::messages::cached_identities(&ctx, "acc", "fld")
            .await
            .expect("cached identities");
        assert_eq!(
            cached.adopted_local_ids.get("m-new2").map(String::as_str),
            Some("proj")
        );
        assert!(
            !cached.adopted_local_ids.contains_key("m-old"),
            "the stale alias is not adopted for the current projection"
        );
        assert!(
            !cached.remote_ids.contains("m-old"),
            "the stale alias is not presented as a cached identity"
        );

        apply_graph_delta(
            &ctx,
            &account,
            &folder,
            crate::graph::messages::MessageDelta {
                messages: Vec::new(),
                delta_link: Some("cursor-1".into()),
                pending_cursor: crate::graph::messages::PendingCursor::Keep,
                adopted_local_ids: std::collections::HashMap::new(),
                full_sync: false,
                removed_remote_ids: vec!["m-old".into()],
            },
        )
        .await
        .expect("delta removing the historical destination");

        assert_eq!(
            message_row_count(&ctx, "proj").await,
            1,
            "the historical receipt must not back the stale alias, so the projection survives"
        );
        let (uid, gm_msgid, target_pending) = pending_projection_state(&ctx, "proj").await;
        assert_eq!(
            (uid, gm_msgid, target_pending),
            (None, None, 1),
            "the projection still waits for the newer trip"
        );
        assert_eq!(
            row_move_target(&ctx, "proj").await,
            (Some("m-new2".to_string()), Some("fld".to_string())),
            "the new target survives the stale removal"
        );
    }

    /// A page mapping captured before a newer trip cannot resolve (and delete)
    /// the row after that trip claimed the identity slot, even while the claim
    /// has no confirmed destination yet.
    ///
    /// This is the exact `message_undo` sequence: the message leaves and
    /// returns to the folder, so the old receipts (and the page aliases derived
    /// from them) still name it, but the current trip owns the row.
    #[tokio::test]
    async fn graph_stale_page_mapping_does_not_survive_a_new_claim() {
        async fn confirm_latest(ctx: &nuncio_core::CoreContext, remote: &str, bind: bool) {
            let remote = remote.to_string();
            ctx.db
                .write(move |db| {
                    let op = repo::pending_ops::list(db, Some("acc"))?
                        .into_iter()
                        .max_by_key(|op| op.sequence)
                        .expect("queued move");
                    let mut payload: crate::ops::MoveOp = repo::pending_ops::payload(&op)?;
                    payload.outcomes.push(crate::ops::OpMessageOutcome {
                        message_id: "proj".into(),
                        source_uid: Some(payload.uids[0]),
                        state: crate::ops::OpMessageState::Confirmed,
                        target_folder_id: Some(payload.target_folder_id.clone()),
                        target_uid: Some(synthetic_uid(&remote)),
                        target_uidvalidity: None,
                        target_remote_id: Some(remote.clone()),
                        attempt_remote_id: None,
                        copy_phase: Default::default(),
                        delete_flag_phase: Default::default(),
                        expunge_phase: Default::default(),
                    });
                    repo::pending_ops::update_payload(
                        db,
                        &op.id,
                        &serde_json::to_value(&payload).expect("payload"),
                    )?;
                    repo::pending_ops::mark_done(db, &op.id)?;
                    repo::messages::set_move_target_identity(
                        db,
                        "acc",
                        "proj",
                        &payload.target_folder_id,
                        &remote,
                        &op.id,
                        op.sequence,
                    )?;
                    if bind {
                        repo::messages::set_graph_identity(
                            db,
                            "proj",
                            synthetic_uid("proj"),
                            &remote,
                        )?;
                    }
                    Ok(())
                })
                .await
                .expect("confirm move");
        }

        let (ctx, account, folder) = graph_delta_fixture().await;
        insert_pending_projection(&ctx, "proj", "same@example.com").await;
        ctx.db
            .write(|db| {
                repo::folders::insert(db, &Folder::new("src", "acc", "Source", FolderRole::Other))?;
                db.execute(
                    "UPDATE account SET oauth_provider = 'microsoft' WHERE id = 'acc'",
                    [],
                )?;
                repo::messages::set_graph_identity(db, "proj", synthetic_uid("proj"), "m-start")?;
                Ok(())
            })
            .await
            .expect("seed source");
        let service = crate::MailService::new(ctx.clone());
        let first = service
            .message_act(
                &["proj".to_string()],
                &nuncio_core::models::MessageAction::Move {
                    folder_id: "src".into(),
                },
            )
            .await
            .expect("first move");
        confirm_latest(&ctx, "m-src", true).await;
        let second = service
            .message_act(
                &["proj".to_string()],
                &nuncio_core::models::MessageAction::Move {
                    folder_id: "fld".into(),
                },
            )
            .await
            .expect("second move");
        confirm_latest(&ctx, "m-old", false).await;
        let cached = crate::graph::messages::cached_identities(&ctx, "acc", "fld")
            .await
            .expect("collect before the newer trip");
        assert_eq!(
            cached.adopted_local_ids.get("m-old").map(String::as_str),
            Some("proj"),
            "the page starts from the receipt of the previous trip"
        );
        // Both undos commit their inverse claim locally without waiting for
        // the account actor.
        assert!(service.message_undo(&second).await.expect("undo second"));
        assert!(service.message_undo(&first).await.expect("undo first"));
        assert_eq!(
            pending_projection_state(&ctx, "proj").await,
            (None, None, 1)
        );
        assert_eq!(row_move_target(&ctx, "proj").await, (None, None));
        let owners = ctx
            .db
            .read(|db| repo::messages::move_target_owners(db, "acc", "fld"))
            .await
            .expect("owners");
        assert!(
            owners.contains_key("proj"),
            "the second inverse owns the new pending trip"
        );
        apply_graph_delta(
            &ctx,
            &account,
            &folder,
            crate::graph::messages::MessageDelta {
                messages: Vec::new(),
                delta_link: Some("cursor-after-stale-page".into()),
                pending_cursor: crate::graph::messages::PendingCursor::Keep,
                adopted_local_ids: cached.adopted_local_ids,
                full_sync: false,
                removed_remote_ids: vec!["m-old".into()],
            },
        )
        .await
        .expect("apply stale page after a new claim");
        assert_eq!(
            message_row_count(&ctx, "proj").await,
            1,
            "a page mapping captured before the new claim cannot delete its projection"
        );
    }

    /// A rule-driven move claims the projection's identity slot in the same
    /// transaction that queues the move, so a retry of an earlier batch cannot
    /// backfill its stale destination and let `@removed` delete the new trip.
    #[tokio::test]
    async fn graph_rule_move_claims_before_an_old_backfill() {
        use nuncio_core::models::{
            MailRule, RuleAction, RuleActionKind, RuleCondition, RuleField, RuleOp,
        };
        let (ctx, account, folder) = graph_delta_fixture().await;
        insert_pending_projection(&ctx, "proj", "same@example.com").await;
        let older = insert_confirmed_move_op(&ctx, "proj", "m-old").await;
        ctx.db
            .write(|db| {
                let source = Folder::new("src", "acc", "Source", FolderRole::Other);
                repo::folders::insert(db, &source)?;
                repo::messages::move_to_folder_pending(db, &["proj".to_string()], "src")?;
                repo::messages::set_graph_identity(db, "proj", 7, "m-source")?;
                Ok(())
            })
            .await
            .expect("seed source projection");
        let summary = ctx
            .db
            .read(|db| repo::messages::get_required(db, "proj").map(|row| row.summary))
            .await
            .expect("summary");
        let rule = MailRule {
            id: "return-rule".into(),
            account_id: Some("acc".into()),
            name: "Return".into(),
            enabled: true,
            match_all: true,
            stop_processing: true,
            sort_order: 0,
            conditions: vec![RuleCondition {
                field: RuleField::Subject,
                op: RuleOp::Contains,
                value: "subject".into(),
            }],
            actions: vec![RuleAction {
                kind: RuleActionKind::Move,
                folder_id: Some("fld".into()),
            }],
        };
        assert_eq!(
            crate::rules::apply_rule(&ctx, &account, &rule, &[summary])
                .await
                .expect("apply rule"),
            1
        );
        ctx.db
            .write(move |db| {
                repo::sync_state::set(db, "acc", "mail", "graph_folder:Inbox", "remote-inbox")?;
                db.execute(
                    "UPDATE pending_op SET next_retry_at = '2099-01-01T00:00:00Z' WHERE id <> ?1",
                    rusqlite::params![older],
                )?;
                Ok(())
            })
            .await
            .expect("defer other operations");
        let client = crate::graph::GraphMailClient::with_base("http://127.0.0.1:9".to_string());
        let result = crate::ops::executor::run_due_graph_with(&ctx, &account, &client)
            .await
            .expect("retry the earlier batch");
        assert_eq!(
            result.done, 1,
            "retry the earlier confirmed batch through the real executor"
        );
        apply_graph_delta(
            &ctx,
            &account,
            &folder,
            crate::graph::messages::MessageDelta {
                messages: Vec::new(),
                delta_link: Some("cursor-rule".into()),
                pending_cursor: crate::graph::messages::PendingCursor::Keep,
                adopted_local_ids: std::collections::HashMap::new(),
                full_sync: false,
                removed_remote_ids: vec!["m-old".into()],
            },
        )
        .await
        .expect("apply removal of the old destination");
        assert_eq!(
            message_row_count(&ctx, "proj").await,
            1,
            "a rule-created newer trip must survive the old backfill and tombstone"
        );
    }

    async fn review_confirm_latest_trip(ctx: &nuncio_core::CoreContext, remote: &str, bind: bool) {
        let remote = remote.to_string();
        ctx.db
            .write(move |db| {
                let op = repo::pending_ops::list(db, Some("acc"))?
                    .into_iter()
                    .max_by_key(|op| op.sequence)
                    .unwrap();
                let mut payload: crate::ops::MoveOp = repo::pending_ops::payload(&op)?;
                payload.outcomes.push(crate::ops::OpMessageOutcome {
                    message_id: "proj".into(),
                    source_uid: Some(payload.uids[0]),
                    state: crate::ops::OpMessageState::Confirmed,
                    target_folder_id: Some(payload.target_folder_id.clone()),
                    target_uid: Some(synthetic_uid(&remote)),
                    target_uidvalidity: None,
                    target_remote_id: Some(remote.clone()),
                    attempt_remote_id: None,
                    copy_phase: Default::default(),
                    delete_flag_phase: Default::default(),
                    expunge_phase: Default::default(),
                });
                repo::pending_ops::update_payload(
                    db,
                    &op.id,
                    &serde_json::to_value(&payload).unwrap(),
                )?;
                repo::pending_ops::mark_done(db, &op.id)?;
                repo::messages::set_move_target_identity(
                    db,
                    "acc",
                    "proj",
                    &payload.target_folder_id,
                    &remote,
                    &op.id,
                    op.sequence,
                )?;
                if bind {
                    repo::messages::set_graph_identity(db, "proj", synthetic_uid("proj"), &remote)?;
                }
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn review_owner_fixture() -> (
        nuncio_core::CoreContext,
        nuncio_core::models::Account,
        Folder,
        crate::MailService,
    ) {
        let (ctx, account, folder) = graph_delta_fixture().await;
        insert_pending_projection(&ctx, "proj", "same@example.com").await;
        ctx.db
            .write(|db| {
                repo::folders::insert(db, &Folder::new("src", "acc", "Source", FolderRole::Other))?;
                db.execute(
                    "UPDATE account SET oauth_provider = 'microsoft' WHERE id = 'acc'",
                    [],
                )?;
                repo::messages::set_graph_identity(db, "proj", synthetic_uid("proj"), "m-start")?;
                Ok(())
            })
            .await
            .unwrap();
        let service = crate::MailService::new(ctx.clone());
        (ctx, account, folder, service)
    }

    /// Regression: a `NotExecuted` (non-compensating) undo restores the row to
    /// its source folder and must put back the provable Graph source identity.
    /// A retained confirmation from an earlier trip into the same folder must
    /// then not resolve the row, so its `@removed` cannot delete the restored
    /// message.
    #[tokio::test]
    async fn review_graph_not_executed_undo_does_not_adopt_a_historical_receipt() {
        use nuncio_core::models::MessageAction;
        let (ctx, account, folder, service) = review_owner_fixture().await;
        // A retained partial batch confirmed this row on a previous trip.
        let older = insert_confirmed_move_op(&ctx, "proj", "m-old").await;
        ctx.db
            .write(|db| {
                repo::messages::set_graph_identity(db, "proj", synthetic_uid("proj"), "m-old")?;
                Ok(())
            })
            .await
            .unwrap();
        service
            .message_act(
                &["proj".into()],
                &MessageAction::Move {
                    folder_id: "src".into(),
                },
            )
            .await
            .unwrap();
        review_confirm_latest_trip(&ctx, "m-out", true).await;
        service
            .message_act(
                &["proj".into()],
                &MessageAction::Move {
                    folder_id: "fld".into(),
                },
            )
            .await
            .unwrap();
        review_confirm_latest_trip(&ctx, "m-current", true).await;
        prune_finished_ops(&ctx).await;
        assert!(ctx
            .db
            .read(move |db| repo::pending_ops::get(db, &older))
            .await
            .unwrap()
            .is_some());
        // A later move is explicitly refused while verification is unavailable.
        let undo = service
            .message_act(
                &["proj".into()],
                &MessageAction::Move {
                    folder_id: "src".into(),
                },
            )
            .await
            .unwrap();
        ctx.db
            .write(|db| {
                let op = repo::pending_ops::list(db, Some("acc"))?
                    .into_iter()
                    .max_by_key(|op| op.sequence)
                    .unwrap();
                let mut payload: crate::ops::MoveOp = repo::pending_ops::payload(&op)?;
                payload.outcomes.push(crate::ops::OpMessageOutcome {
                    message_id: "proj".into(),
                    source_uid: Some(payload.uids[0]),
                    state: crate::ops::OpMessageState::NotExecuted,
                    target_folder_id: Some(payload.target_folder_id.clone()),
                    target_uid: None,
                    target_uidvalidity: None,
                    target_remote_id: None,
                    attempt_remote_id: None,
                    copy_phase: Default::default(),
                    delete_flag_phase: Default::default(),
                    expunge_phase: Default::default(),
                });
                repo::pending_ops::update_payload(
                    db,
                    &op.id,
                    &serde_json::to_value(payload).unwrap(),
                )?;
                repo::pending_ops::mark_failed(
                    db,
                    &op.id,
                    "Graph rejected the move; verification unavailable",
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(service.message_undo(&undo).await.unwrap());
        assert_eq!(
            pending_projection_state(&ctx, "proj").await,
            (
                Some(synthetic_uid("proj")),
                Some("m-current".to_string()),
                0
            ),
            "the refusal must restore the provable source identity"
        );
        assert!(ctx
            .db
            .read(|db| repo::messages::move_target_owners(db, "acc", "fld"))
            .await
            .unwrap()
            .is_empty());
        apply_graph_delta(
            &ctx,
            &account,
            &folder,
            crate::graph::messages::MessageDelta {
                messages: vec![],
                delta_link: Some("cursor-refused-undo".into()),
                pending_cursor: crate::graph::messages::PendingCursor::Keep,
                adopted_local_ids: std::collections::HashMap::new(),
                full_sync: false,
                removed_remote_ids: vec!["m-old".into()],
            },
        )
        .await
        .unwrap();
        assert_eq!(
            message_row_count(&ctx, "proj").await,
            1,
            "NotExecuted Undo must not let an older trip's receipt delete the restored message"
        );
    }

    /// Regression: an undo moves the claimed row out and back again. The page
    /// alias captured before the undo names the old trip's id; discarding it
    /// must not let the Message-ID heuristic bind that id to the claimed row
    /// (which would clear `target_pending` and the current owner).
    #[tokio::test]
    async fn review_graph_claim_only_undo_does_not_bind_stale_update() {
        use nuncio_core::models::MessageAction;
        let (ctx, account, folder, service) = review_owner_fixture().await;
        let first = service
            .message_act(
                &["proj".into()],
                &MessageAction::Move {
                    folder_id: "src".into(),
                },
            )
            .await
            .unwrap();
        review_confirm_latest_trip(&ctx, "m-out", true).await;
        let second = service
            .message_act(
                &["proj".into()],
                &MessageAction::Move {
                    folder_id: "fld".into(),
                },
            )
            .await
            .unwrap();
        review_confirm_latest_trip(&ctx, "m-old", false).await;
        let cached = crate::graph::messages::cached_identities(&ctx, "acc", "fld")
            .await
            .unwrap();
        assert!(service.message_undo(&second).await.unwrap());
        assert!(service.message_undo(&first).await.unwrap());
        assert!(ctx
            .db
            .read(|db| repo::messages::move_target_owners(db, "acc", "fld"))
            .await
            .unwrap()
            .contains_key("proj"));
        let stale = crate::graph::messages::map_graph_json(
            serde_json::json!({
                "id": "m-old", "subject": "Old trip", "receivedDateTime": "2026-11-16T08:00:00Z", "internetMessageId": "<same@example.com>"
            }),
            "acc",
            "fld",
        )
        .unwrap();
        apply_graph_delta(
            &ctx,
            &account,
            &folder,
            crate::graph::messages::MessageDelta {
                messages: vec![stale],
                delta_link: Some("cursor-stale-update".into()),
                pending_cursor: crate::graph::messages::PendingCursor::Keep,
                adopted_local_ids: cached.adopted_local_ids,
                full_sync: false,
                removed_remote_ids: vec![],
            },
        )
        .await
        .unwrap();
        assert_eq!(
            pending_projection_state(&ctx, "proj").await,
            (None, None, 1),
            "discarding the page alias must not let its old id consume the claimed row through Message-ID"
        );
    }

    /// Regression: `gm_msgid` is overloaded. On Gmail IMAP it holds
    /// `X-GM-MSGID`, so a `NotExecuted` undo must not treat it as a Graph id and
    /// fabricate a synthetic UID. After the source generation changed, the row
    /// must stay isolated (`uid IS NULL`, `target_pending = 1`) for the next
    /// sync to re-associate.
    #[tokio::test]
    async fn gmail_not_executed_undo_does_not_fabricate_a_graph_uid() {
        use nuncio_core::models::MessageAction;
        let (ctx, _, _, service) = review_owner_fixture().await;
        ctx.db
            .write(|db| {
                db.execute(
                    "UPDATE account SET provider = 'gmail', oauth_provider = 'google' WHERE id = 'acc'",
                    [],
                )?;
                repo::folders::set_sync_flags(db, "fld", Some(7), None, None)?;
                repo::messages::set_uid_and_graph_identity(
                    db,
                    "proj",
                    Some(42),
                    Some(7),
                    Some("123456789012345"),
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let undo = service
            .message_act(
                &["proj".into()],
                &MessageAction::Move {
                    folder_id: "src".into(),
                },
            )
            .await
            .unwrap();
        ctx.db
            .write(|db| {
                let op = repo::pending_ops::list(db, Some("acc"))?
                    .into_iter()
                    .max_by_key(|op| op.sequence)
                    .unwrap();
                let mut payload: crate::ops::MoveOp = repo::pending_ops::payload(&op)?;
                payload.outcomes.push(crate::ops::OpMessageOutcome {
                    message_id: "proj".into(),
                    source_uid: Some(42),
                    state: crate::ops::OpMessageState::NotExecuted,
                    target_folder_id: Some("src".into()),
                    target_uid: None,
                    target_uidvalidity: None,
                    target_remote_id: None,
                    attempt_remote_id: None,
                    copy_phase: Default::default(),
                    delete_flag_phase: Default::default(),
                    expunge_phase: Default::default(),
                });
                repo::pending_ops::update_payload(
                    db,
                    &op.id,
                    &serde_json::to_value(&payload).unwrap(),
                )?;
                repo::pending_ops::mark_failed(
                    db,
                    &op.id,
                    "COPY refused before the mailbox generation changed",
                )?;
                repo::folders::set_sync_flags(db, "fld", Some(8), None, None)?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(service.message_undo(&undo).await.unwrap());
        let row = ctx
            .db
            .read(|db| repo::messages::get_required(db, "proj"))
            .await
            .unwrap();
        let pending = pending_projection_state(&ctx, "proj").await;
        assert!(
            row.uid.is_none() && pending.2 == 1,
            "X-GM-MSGID is not a Graph id: a changed-generation IMAP row must stay unbound, got uid={:?}, uidvalidity={:?}, pending={}",
            row.uid,
            row.uidvalidity,
            pending.2
        );
    }

    /// Regression: a settled failure that persisted a send intent may already
    /// have reached the server. Undo must keep it pending/deferred and must not
    /// apply the `NotExecuted` identity restore.
    #[tokio::test]
    async fn graph_failed_send_intent_undo_stays_pending() {
        use nuncio_core::models::{MessageAction, UndoState};
        let (ctx, _, _, service) = review_owner_fixture().await;
        service
            .message_act(
                &["proj".into()],
                &MessageAction::Move {
                    folder_id: "src".into(),
                },
            )
            .await
            .unwrap();
        review_confirm_latest_trip(&ctx, "m-out", true).await;
        service
            .message_act(
                &["proj".into()],
                &MessageAction::Move {
                    folder_id: "fld".into(),
                },
            )
            .await
            .unwrap();
        review_confirm_latest_trip(&ctx, "m-current", true).await;
        let undo = service
            .message_act(
                &["proj".into()],
                &MessageAction::Move {
                    folder_id: "src".into(),
                },
            )
            .await
            .unwrap();
        ctx.db
            .write(|db| {
                let op = repo::pending_ops::list(db, Some("acc"))?
                    .into_iter()
                    .max_by_key(|op| op.sequence)
                    .unwrap();
                repo::pending_ops::mark_send_intent(db, &op.id)?;
                repo::pending_ops::mark_failed(
                    db,
                    &op.id,
                    "connection reset after the request was persisted",
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(service.message_undo(&undo).await.unwrap());
        assert_eq!(
            pending_projection_state(&ctx, "proj").await,
            (None, None, 1),
            "a possible send must stay pending; the NotExecuted identity restore must not run"
        );
        let state = ctx
            .db
            .read({
                let undo = undo.clone();
                move |db| Ok(repo::undo::get_request(db, &undo)?.map(|request| request.state))
            })
            .await
            .unwrap();
        assert!(
            matches!(state, Some(UndoState::Deferred | UndoState::Pending)),
            "a possible send keeps the request retryable, got {state:?}"
        );
    }
}
