//! Local mail rule evaluation and application.
//!
//! Rules are pure: `evaluate` takes a rule, a message summary and an optional
//! body text. `apply_rules` is the side-effecting wrapper used by the sync
//! engine after new mail is stored; it reuses the same evaluator so the
//! command-facing service (`rule_run_now`) can call it directly.

use std::collections::HashSet;

use nuncio_core::models::{
    Address, FolderRole, MailRule, MessageSummary, NewPendingOp, RuleActionKind, RuleField, RuleOp,
    StoredMessage,
};
use nuncio_core::{repo, AppError, CoreContext, Result};
use regex::RegexBuilder;
use tracing::{debug, warn};

use crate::ops::{
    DeleteOp, MoveOp, SetFlagsOp, KIND_ARCHIVE, KIND_DELETE, KIND_MOVE, KIND_SET_FLAGS,
};

/// Side effects of applying rules, consumed by the notification step.
#[derive(Debug, Default)]
pub struct RuleEffects {
    /// Message ids explicitly configured not to notify.
    pub skip_notify: HashSet<String>,
    /// Message ids whose state changed locally.
    pub changed: Vec<String>,
}

/// Evaluate one rule against a message.
pub fn evaluate(rule: &MailRule, summary: &MessageSummary, body_text: Option<&str>) -> bool {
    if rule.conditions.is_empty() {
        return false;
    }
    if rule.match_all {
        rule.conditions
            .iter()
            .all(|condition| evaluate_condition(condition, summary, body_text))
    } else {
        rule.conditions
            .iter()
            .any(|condition| evaluate_condition(condition, summary, body_text))
    }
}

fn evaluate_condition(
    condition: &nuncio_core::models::RuleCondition,
    summary: &MessageSummary,
    body_text: Option<&str>,
) -> bool {
    let subject = field_value(condition.field, summary, body_text);
    match condition.op {
        RuleOp::IsTrue => match condition.field {
            RuleField::HasAttachment => summary.has_attachments,
            _ => parse_bool(&condition.value).unwrap_or(false),
        },
        _ => {
            if condition.value.is_empty() {
                return false;
            }
            let haystack = subject.to_lowercase();
            let needle = condition.value.to_lowercase();
            match condition.op {
                RuleOp::Contains => haystack.contains(&needle),
                RuleOp::NotContains => !haystack.contains(&needle),
                RuleOp::Equals => haystack == needle,
                RuleOp::StartsWith => haystack.starts_with(&needle),
                RuleOp::EndsWith => haystack.ends_with(&needle),
                RuleOp::MatchesRegex => match RegexBuilder::new(&condition.value)
                    .case_insensitive(true)
                    .build()
                {
                    Ok(regex) => regex.is_match(&subject),
                    Err(error) => {
                        warn!(%error, "invalid mail rule regex");
                        false
                    }
                },
                RuleOp::IsTrue => unreachable!(),
            }
        }
    }
}

fn field_value(field: RuleField, summary: &MessageSummary, body_text: Option<&str>) -> String {
    match field {
        RuleField::From => summary
            .r#from
            .as_ref()
            .map(format_address)
            .unwrap_or_default(),
        RuleField::To => format_addresses(&summary.to),
        RuleField::Cc => format_addresses(&summary.cc),
        RuleField::Subject => summary.subject.clone(),
        RuleField::Body => body_text.unwrap_or_default().to_string(),
        RuleField::HasAttachment => summary.has_attachments.to_string(),
        RuleField::AnyRecipient => {
            format_addresses(&summary.to) + " " + &format_addresses(&summary.cc)
        }
    }
}

fn format_address(address: &Address) -> String {
    match &address.name {
        Some(name) if !name.is_empty() => format!("{name} <{}>", address.email),
        _ => address.email.clone(),
    }
}

fn format_addresses(addresses: &[Address]) -> String {
    addresses
        .iter()
        .map(format_address)
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Some(true),
        "false" | "0" | "no" => Some(false),
        _ => None,
    }
}

/// Rules that match a message, honouring `stopProcessing` and rule order.
pub fn matching_rules<'a>(
    rules: &'a [MailRule],
    summary: &MessageSummary,
    body_text: Option<&str>,
) -> Vec<&'a MailRule> {
    let mut matched = Vec::new();
    for rule in rules.iter().filter(|rule| rule.enabled) {
        if evaluate(rule, summary, body_text) {
            matched.push(rule);
            if rule.stop_processing {
                break;
            }
        }
    }
    matched
}

/// Apply enabled rules to freshly synced messages.
///
/// Local state is updated optimistically and matching server actions are
/// enqueued on the persistent operation queue.
pub async fn apply_rules(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    messages: &[MessageSummary],
) -> Result<RuleEffects> {
    let effects = RuleEffects::default();
    if messages.is_empty() {
        return Ok(effects);
    }
    let account_id = account.id.clone();
    let rules = ctx
        .db
        .read(move |db| repo::rules::list_enabled(db, &account_id))
        .await?;
    if rules.is_empty() {
        return Ok(effects);
    }
    apply_rule_set(ctx, account, &rules, messages).await
}

/// Apply exactly one rule to a list of messages (used by `rule_run_now`).
/// Returns the number of messages whose local state changed.
pub async fn apply_rule(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    rule: &MailRule,
    messages: &[MessageSummary],
) -> Result<usize> {
    if messages.is_empty() {
        return Ok(0);
    }
    let effects = apply_rule_set(ctx, account, std::slice::from_ref(rule), messages).await?;
    let _ = effects.skip_notify;
    Ok(effects.changed.len())
}

async fn apply_rule_set(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    rules: &[MailRule],
    messages: &[MessageSummary],
) -> Result<RuleEffects> {
    let mut effects = RuleEffects::default();
    let needs_body = rules.iter().any(|rule| {
        rule.conditions
            .iter()
            .any(|condition| condition.field == RuleField::Body)
    });

    let folders = ctx
        .db
        .read({
            let account_id = account.id.clone();
            move |db| repo::folders::list(db, &account_id)
        })
        .await?;
    let find_role = |role: FolderRole| {
        folders
            .iter()
            .find(|folder| folder.role == role)
            .map(|folder| folder.id.clone())
    };
    let trash = find_role(FolderRole::Trash);
    let junk = find_role(FolderRole::Junk);
    let archive = find_role(FolderRole::Archive);
    let all = find_role(FolderRole::All);

    for summary in messages {
        let stored = ctx
            .db
            .read({
                let id = summary.id.clone();
                move |db| repo::messages::get(db, &id)
            })
            .await?;
        let Some(stored) = stored else {
            continue;
        };
        let body_text = if needs_body {
            ctx.db
                .read({
                    let id = summary.id.clone();
                    move |db| {
                        Ok(db
                            .query_row(
                                "SELECT body FROM fts_mail WHERE message_id = ?1 LIMIT 1",
                                rusqlite::params![id],
                                |row| row.get::<_, String>(0),
                            )
                            .ok())
                    }
                })
                .await
                .ok()
                .flatten()
        } else {
            None
        };

        let matched = matching_rules(rules, summary, body_text.as_deref());
        if matched.is_empty() {
            continue;
        }
        let mut changed = false;
        // Priority within one rule chain: actions run in the order the user
        // configured them, and the **first action that changes the message's
        // folder wins**. Every later action in the chain would have to address
        // the source UID, which is no longer valid once the optimistic move
        // landed: a second Move would enqueue the same source UID twice (the
        // remote copy already left that folder, so the second command cannot
        // do what the user asked), and a Delete after a Move would target a
        // message that is no longer there. So a Move/Archive/MarkJunk/Delete
        // after a successful folder action is skipped, and so is any flag
        // action after it (the flag command is UID-addressed too; put
        // "mark as read"/"flag" before the move if both are wanted).
        let mut folder_claimed = false;
        for rule in matched {
            debug!(rule = %rule.name, message = %summary.id, "mail rule matched");
            for action in &rule.actions {
                match action.kind {
                    RuleActionKind::SkipNotify => {
                        effects.skip_notify.insert(summary.id.clone());
                    }
                    RuleActionKind::Notify => {}
                    RuleActionKind::MarkRead => {
                        if folder_claimed {
                            debug!(
                                message = %summary.id,
                                "rule action skipped: an earlier action already changed the folder"
                            );
                        } else if apply_seen(ctx, &stored, account).await? {
                            changed = true;
                        }
                    }
                    RuleActionKind::Flag => {
                        if folder_claimed {
                            debug!(
                                message = %summary.id,
                                "rule action skipped: an earlier action already changed the folder"
                            );
                        } else if apply_flagged(ctx, &stored, account).await? {
                            changed = true;
                        }
                    }
                    RuleActionKind::Move => {
                        if folder_claimed {
                            debug!(
                                message = %summary.id,
                                "rule move skipped: an earlier action already changed the folder"
                            );
                        } else if let Some(target) = action.folder_id.clone() {
                            if apply_move(ctx, &stored, account, &target).await? {
                                changed = true;
                                folder_claimed = true;
                            }
                        }
                    }
                    RuleActionKind::Delete => {
                        if folder_claimed {
                            debug!(
                                message = %summary.id,
                                "rule delete skipped: an earlier action already changed the folder"
                            );
                        } else if apply_delete(ctx, &stored, account, trash.as_deref()).await? {
                            changed = true;
                            folder_claimed = true;
                        }
                    }
                    RuleActionKind::MarkJunk => {
                        if folder_claimed {
                            debug!(
                                message = %summary.id,
                                "rule junk move skipped: an earlier action already changed the folder"
                            );
                        } else if let Some(target) = junk.clone() {
                            if apply_move(ctx, &stored, account, &target).await? {
                                changed = true;
                                folder_claimed = true;
                            }
                        }
                    }
                    RuleActionKind::Archive => {
                        if folder_claimed {
                            debug!(
                                message = %summary.id,
                                "rule archive skipped: an earlier action already changed the folder"
                            );
                        } else if apply_archive(
                            ctx,
                            &stored,
                            account,
                            archive.as_deref(),
                            all.as_deref(),
                        )
                        .await?
                        {
                            changed = true;
                            folder_claimed = true;
                        }
                    }
                }
            }
        }
        if changed {
            effects.changed.push(summary.id.clone());
        }
    }

    // Keep the local inbox counts correct after moves.
    let folder_ids: HashSet<String> = folders.into_iter().map(|folder| folder.id).collect();
    let account_id = account.id.clone();
    ctx.db
        .write(move |db| {
            for folder_id in folder_ids {
                repo::folders::recompute_counts(db, &folder_id)?;
            }
            let _ = account_id;
            Ok(())
        })
        .await?;
    Ok(effects)
}

/// Row identity a rule matched against, re-checked inside the transaction
/// that records each action.
///
/// Rules are evaluated outside the write transaction, and in between the user
/// (or a sync) can move, re-sync or otherwise reassign the row. A UID is
/// folder-local and only valid for one generation, so acting on the stale
/// snapshot could send a command about a different message. Every action
/// therefore re-reads the row and requires all of these to be unchanged.
struct MatchedRow {
    id: String,
    account_id: String,
    folder_id: String,
    uid: Option<i64>,
    uidvalidity: Option<i64>,
}

impl MatchedRow {
    fn from_stored(stored: &StoredMessage) -> Self {
        Self {
            id: stored.summary.id.clone(),
            account_id: stored.summary.account_id.clone(),
            folder_id: stored.summary.folder_id.clone(),
            uid: stored.uid,
            uidvalidity: stored.uidvalidity,
        }
    }
}

/// True when the row still has the identity the rule matched against and is
/// not an optimistic move waiting for its target UID.
fn still_matches(db: &rusqlite::Connection, expected: &MatchedRow) -> Result<bool> {
    let Some(current) = repo::messages::get(db, &expected.id)? else {
        return Ok(false);
    };
    if current.summary.account_id != expected.account_id
        || current.summary.folder_id != expected.folder_id
        || current.uid != expected.uid
        || current.uidvalidity != expected.uidvalidity
    {
        return Ok(false);
    }
    Ok(repo::messages::target_pending_ids(db, std::slice::from_ref(&expected.id))?.is_empty())
}

fn skipped_changed(expected: &MatchedRow, action: &str) {
    warn!(
        message = %expected.id,
        action,
        "rule action skipped: the message changed since the rule matched"
    );
}

async fn apply_seen(
    ctx: &CoreContext,
    stored: &StoredMessage,
    account: &nuncio_core::models::Account,
) -> Result<bool> {
    let expected = MatchedRow::from_stored(stored);
    let account_id = account.id.clone();
    let payload = if account.provider.uses_imap() {
        stored
            .uid
            .map(|uid| {
                serde_json::to_value(SetFlagsOp {
                    folder_id: stored.summary.folder_id.clone(),
                    uids: vec![uid],
                    seen: Some(true),
                    flagged: None,
                    answered: None,
                    forwarded: None,
                    source_uidvalidity: stored.uidvalidity,
                    previous: Vec::new(),
                    message_ids: vec![stored.summary.id.clone()],
                    outcomes: Vec::new(),
                    remote_ids: Vec::new(),
                })
                .map_err(|error| {
                    AppError::internal(format!("cannot encode rule flag change: {error}"))
                })
            })
            .transpose()?
    } else {
        None
    };
    ctx.db
        .write(move |db| {
            if !still_matches(db, &expected)? {
                skipped_changed(&expected, "markRead");
                return Ok(false);
            }
            repo::messages::set_flags(
                db,
                std::slice::from_ref(&expected.id),
                Some(true),
                None,
                None,
                None,
            )?;
            if let Some(payload) = payload {
                let op = repo::pending_ops::enqueue(
                    db,
                    NewPendingOp::new(account_id, KIND_SET_FLAGS, payload, "Rule: mark as read"),
                )?;
                // The executor no longer treats a missing writer as permission
                // for a payload that carries message ids. Record the owner in
                // the same transaction as the queue row.
                repo::messages::record_flag_writers(
                    db,
                    &expected.id,
                    &op.id,
                    "setSeen",
                    Some(true),
                    None,
                    None,
                    None,
                )?;
            }
            Ok(true)
        })
        .await
}

async fn apply_flagged(
    ctx: &CoreContext,
    stored: &StoredMessage,
    account: &nuncio_core::models::Account,
) -> Result<bool> {
    let expected = MatchedRow::from_stored(stored);
    let account_id = account.id.clone();
    let payload = if account.provider.uses_imap() {
        stored
            .uid
            .map(|uid| {
                serde_json::to_value(SetFlagsOp {
                    folder_id: stored.summary.folder_id.clone(),
                    uids: vec![uid],
                    seen: None,
                    flagged: Some(true),
                    answered: None,
                    forwarded: None,
                    source_uidvalidity: stored.uidvalidity,
                    previous: Vec::new(),
                    message_ids: vec![stored.summary.id.clone()],
                    outcomes: Vec::new(),
                    remote_ids: Vec::new(),
                })
                .map_err(|error| {
                    AppError::internal(format!("cannot encode rule flag change: {error}"))
                })
            })
            .transpose()?
    } else {
        None
    };
    ctx.db
        .write(move |db| {
            if !still_matches(db, &expected)? {
                skipped_changed(&expected, "flag");
                return Ok(false);
            }
            repo::messages::set_flags(
                db,
                std::slice::from_ref(&expected.id),
                None,
                Some(true),
                None,
                None,
            )?;
            if let Some(payload) = payload {
                let op = repo::pending_ops::enqueue(
                    db,
                    NewPendingOp::new(account_id, KIND_SET_FLAGS, payload, "Rule: flag message"),
                )?;
                repo::messages::record_flag_writers(
                    db,
                    &expected.id,
                    &op.id,
                    "setFlagged",
                    None,
                    Some(true),
                    None,
                    None,
                )?;
            }
            Ok(true)
        })
        .await
}

async fn apply_move(
    ctx: &CoreContext,
    stored: &StoredMessage,
    account: &nuncio_core::models::Account,
    target_folder: &str,
) -> Result<bool> {
    let expected = MatchedRow::from_stored(stored);
    let account_id = account.id.clone();
    let target = target_folder.to_string();
    let payload = if account.provider.uses_imap() {
        stored
            .uid
            .map(|uid| {
                serde_json::to_value(MoveOp {
                    folder_id: stored.summary.folder_id.clone(),
                    target_folder_id: target.clone(),
                    uids: vec![uid],
                    message_ids: vec![stored.summary.id.clone()],
                    source_uidvalidity: stored.uidvalidity,
                    outcomes: Vec::new(),
                    remote_ids: Vec::new(),
                })
                .map_err(|error| AppError::internal(format!("cannot encode rule move: {error}")))
            })
            .transpose()?
    } else {
        None
    };
    ctx.db
        .write(move |db| {
            if !still_matches(db, &expected)? {
                skipped_changed(&expected, "move");
                return Ok(false);
            }
            if expected.folder_id == target {
                return Ok(false);
            }
            let ids = vec![expected.id.clone()];
            match payload {
                // No server identity (Local/POP3, or a Graph row without a
                // UID mapping): move the row plainly and queue nothing.
                None => {
                    repo::messages::move_to_folder(db, &ids, &target)?;
                }
                // The optimistic move and the queue row are written together:
                // a queued move always has a `target_pending` row and vice
                // versa.
                Some(payload) => {
                    repo::messages::move_to_folder_pending(db, &ids, &target)?;
                    let op = repo::pending_ops::enqueue(
                        db,
                        NewPendingOp::new(
                            account_id,
                            KIND_MOVE,
                            payload.clone(),
                            "Rule: move message",
                        ),
                    )?;
                    // The rule-created trip owns the destination slot from the
                    // moment it is projected. Without this claim a retry of an
                    // earlier batch could write its stale destination onto the
                    // new projection before this operation confirms, and the
                    // old id's `@removed` would then delete it.
                    crate::ops::claim_move_target_owner(
                        db,
                        &op.account_id,
                        KIND_MOVE,
                        &payload,
                        &op.id,
                        op.sequence,
                    )?;
                }
            }
            Ok(true)
        })
        .await
}

async fn apply_delete(
    ctx: &CoreContext,
    stored: &StoredMessage,
    account: &nuncio_core::models::Account,
    trash: Option<&str>,
) -> Result<bool> {
    let Some(trash) = trash else {
        // No Trash folder: hide the row locally until the server confirms the
        // delete, and queue the UID command (when there is one). `soft_hidden`
        // records the local projection so a refused command unhides the row;
        // a permanent server delete is what the executor does when the source
        // has no Trash role.
        let expected = MatchedRow::from_stored(stored);
        let account_id = account.id.clone();
        let payload = if account.provider.uses_imap() {
            stored
                .uid
                .map(|uid| {
                    serde_json::to_value(DeleteOp {
                        folder_id: stored.summary.folder_id.clone(),
                        uids: vec![uid],
                        permanent: false,
                        target_folder_id: None,
                        soft_hidden: true,
                        message_ids: vec![stored.summary.id.clone()],
                        source_uidvalidity: stored.uidvalidity,
                        pending_delete: false,
                        deleted_flag_set: false,
                        snapshot: Vec::new(),
                        outcomes: Vec::new(),
                        remote_ids: Vec::new(),
                    })
                    .map_err(|error| {
                        AppError::internal(format!("cannot encode rule delete: {error}"))
                    })
                })
                .transpose()?
        } else {
            None
        };
        return ctx
            .db
            .write(move |db| {
                if !still_matches(db, &expected)? {
                    skipped_changed(&expected, "delete");
                    return Ok(false);
                }
                repo::messages::delete_local(db, std::slice::from_ref(&expected.id), false)?;
                if let Some(payload) = payload {
                    repo::pending_ops::enqueue(
                        db,
                        NewPendingOp::new(account_id, KIND_DELETE, payload, "Rule: delete message"),
                    )?;
                }
                Ok(true)
            })
            .await;
    };
    apply_move(ctx, stored, account, trash).await
}

/// Archive one message, reusing the provider-aware `ArchiveOp` semantics:
/// Gmail drops the Inbox label, a regular IMAP mailbox moves to `\Archive` and
/// only falls back to `\All` when no Archive folder exists.
///
/// The local projection uses the same target the executor will use. When the
/// account has neither role, the row is hidden in place and the payload records
/// `soft_hidden`, so a refused command unhides it by local id (the move
/// rollback would look in a target folder that never existed).
async fn apply_archive(
    ctx: &CoreContext,
    stored: &StoredMessage,
    account: &nuncio_core::models::Account,
    archive: Option<&str>,
    all: Option<&str>,
) -> Result<bool> {
    let expected = MatchedRow::from_stored(stored);
    let account_id = account.id.clone();
    let imap = account.provider.uses_imap();
    let archive = archive.map(str::to_string);
    let all = all.map(str::to_string);
    let uid = stored.uid;
    let source_folder_id = stored.summary.folder_id.clone();
    let uidvalidity = stored.uidvalidity;
    let gmail = account.capabilities.gmail_extensions;
    ctx.db
        .write(move |db| {
            if !still_matches(db, &expected)? {
                skipped_changed(&expected, "archive");
                return Ok(false);
            }
            // Provider-aware target shared with the manual archive action:
            // Gmail drops the Inbox label (All Mail), everyone else moves to a
            // real `\Archive` (created by the executor when missing).
            let target = crate::ops::archive_target(gmail, archive.as_deref(), all.as_deref());
            // Already filed in that target: there is nothing to archive.
            if target.as_deref() == Some(expected.folder_id.as_str())
                || (gmail && all.as_deref() == Some(expected.folder_id.as_str()))
            {
                return Ok(false);
            }
            let ids = vec![expected.id.clone()];
            let message_ids = ids.clone();
            match target {
                Some(target) => {
                    let payload = if imap {
                        uid.map(|uid| {
                            serde_json::to_value(crate::ops::ArchiveOp {
                                folder_id: source_folder_id.clone(),
                                uids: vec![uid],
                                target_folder_id: Some(target.clone()),
                                soft_hidden: false,
                                message_ids: message_ids.clone(),
                                source_uidvalidity: uidvalidity,
                                outcomes: Vec::new(),
                                remote_ids: Vec::new(),
                            })
                        })
                        .transpose()
                        .map_err(|error| {
                            AppError::internal(format!("cannot encode rule archive: {error}"))
                        })?
                    } else {
                        None
                    };
                    match payload {
                        Some(payload) => {
                            // The projection and the queued command now need the
                            // same target folder.
                            repo::messages::move_to_folder_pending(db, &ids, &target)?;
                            let op = repo::pending_ops::enqueue(
                                db,
                                NewPendingOp::new(
                                    account_id,
                                    KIND_ARCHIVE,
                                    payload.clone(),
                                    "Rule: archive message",
                                ),
                            )?;
                            // Same ownership rule as `apply_move`: the queued
                            // archive is the only operation allowed to fill
                            // this projection's destination slot.
                            crate::ops::claim_move_target_owner(
                                db,
                                &op.account_id,
                                KIND_ARCHIVE,
                                &payload,
                                &op.id,
                                op.sequence,
                            )?;
                        }
                        None => {
                            repo::messages::move_to_folder(db, &ids, &target)?;
                        }
                    }
                }
                None => {
                    // No Archive or All folder to project into. Only hide when
                    // a server command will actually resolve the state; without
                    // a UID there is no command and the row would stay hidden.
                    let Some(uid) = uid.filter(|_| imap) else {
                        return Ok(false);
                    };
                    let payload = serde_json::to_value(crate::ops::ArchiveOp {
                        folder_id: source_folder_id.clone(),
                        uids: vec![uid],
                        target_folder_id: None,
                        soft_hidden: true,
                        message_ids,
                        source_uidvalidity: uidvalidity,
                        outcomes: Vec::new(),
                        remote_ids: Vec::new(),
                    })
                    .map_err(|error| {
                        AppError::internal(format!("cannot encode rule archive: {error}"))
                    })?;
                    repo::messages::delete_local(db, &ids, false)?;
                    repo::pending_ops::enqueue(
                        db,
                        NewPendingOp::new(
                            account_id,
                            KIND_ARCHIVE,
                            payload,
                            "Rule: archive message",
                        ),
                    )?;
                }
            }
            Ok(true)
        })
        .await
}

/// Validate a rule before persisting it (rejects invalid regex conditions).
pub fn validate_rule(rule: &MailRule) -> Result<()> {
    for condition in &rule.conditions {
        if condition.op == RuleOp::MatchesRegex {
            RegexBuilder::new(&condition.value)
                .build()
                .map_err(|error| {
                    AppError::invalid_input(format!("invalid regular expression: {error}"))
                })?;
        }
        if condition.field == RuleField::HasAttachment
            && condition.op != RuleOp::IsTrue
            && parse_bool(&condition.value).is_none()
        {
            return Err(AppError::invalid_input(
                "hasAttachment conditions need a boolean value",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::models::{RuleAction, RuleCondition};

    fn rule(conditions: Vec<RuleCondition>, actions: Vec<RuleAction>) -> MailRule {
        MailRule {
            id: "r1".into(),
            account_id: None,
            name: "test".into(),
            enabled: true,
            match_all: true,
            conditions,
            actions,
            stop_processing: false,
            sort_order: 0,
        }
    }

    fn message() -> MessageSummary {
        let mut summary =
            MessageSummary::new("m1", "acc", "inbox", "thread", nuncio_core::models::now());
        summary.subject = "Invoice from ACME".into();
        summary.r#from = Some(Address::with_name("Billing", "billing@acme.test"));
        summary.to = vec![Address::new("me@example.com")];
        summary.has_attachments = true;
        summary
    }

    fn condition(field: RuleField, op: RuleOp, value: &str) -> RuleCondition {
        RuleCondition {
            field,
            op,
            value: value.into(),
        }
    }

    #[test]
    fn matches_from_and_subject() {
        let rule = rule(
            vec![
                condition(RuleField::From, RuleOp::Contains, "acme.test"),
                condition(RuleField::Subject, RuleOp::StartsWith, "invoice"),
            ],
            vec![RuleAction {
                kind: RuleActionKind::Move,
                folder_id: Some("archive".into()),
            }],
        );
        assert!(evaluate(&rule, &message(), None));
    }

    #[test]
    fn match_any_and_negation() {
        let mut rule = rule(
            vec![
                condition(RuleField::Subject, RuleOp::Contains, "receipt"),
                condition(RuleField::From, RuleOp::NotContains, "spam"),
            ],
            Vec::new(),
        );
        rule.match_all = false;
        assert!(evaluate(&rule, &message(), None));
        rule.match_all = true;
        assert!(!evaluate(&rule, &message(), None));
    }

    #[test]
    fn has_attachment_and_regex() {
        let rule = rule(
            vec![
                condition(RuleField::HasAttachment, RuleOp::IsTrue, ""),
                condition(RuleField::Subject, RuleOp::MatchesRegex, "^invoice.*acme$"),
            ],
            Vec::new(),
        );
        assert!(evaluate(&rule, &message(), None));
    }

    #[test]
    fn body_conditions_need_body_text() {
        let rule = rule(
            vec![condition(RuleField::Body, RuleOp::Contains, "退款")],
            Vec::new(),
        );
        assert!(!evaluate(&rule, &message(), None));
        assert!(evaluate(&rule, &message(), Some("请处理退款申请")));
    }

    #[test]
    fn stop_processing_is_honoured() {
        let mut first = rule(
            vec![condition(RuleField::From, RuleOp::Contains, "acme")],
            Vec::new(),
        );
        first.stop_processing = true;
        let second = rule(
            vec![condition(RuleField::From, RuleOp::Contains, "acme")],
            Vec::new(),
        );
        let rules = vec![first, second];
        let summary = message();
        let matched = matching_rules(&rules, &summary, None);
        assert_eq!(matched.len(), 1);
    }

    #[test]
    fn invalid_regex_is_rejected() {
        let rule = rule(
            vec![condition(RuleField::Subject, RuleOp::MatchesRegex, "([")],
            Vec::new(),
        );
        assert!(validate_rule(&rule).is_err());
    }
}
