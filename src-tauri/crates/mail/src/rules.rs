//! Local mail rule evaluation and application.
//!
//! Rules are pure: `evaluate` takes a rule, a message summary and an optional
//! body text. `apply_rules` is the side-effecting wrapper used by the sync
//! engine after new mail is stored; it reuses the same evaluator so the
//! command-facing service (`rule_run_now`) can call it directly.

use std::collections::HashSet;

use nuncio_core::models::{
    Address, FolderRole, MailRule, MessageSummary, RuleActionKind, RuleField, RuleOp,
};
use nuncio_core::{repo, AppError, CoreContext, Result};
use regex::RegexBuilder;
use tracing::{debug, warn};

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
        for rule in matched {
            debug!(rule = %rule.name, message = %summary.id, "mail rule matched");
            for action in &rule.actions {
                match action.kind {
                    RuleActionKind::SkipNotify => {
                        effects.skip_notify.insert(summary.id.clone());
                    }
                    RuleActionKind::Notify => {}
                    RuleActionKind::MarkRead => {
                        apply_seen(ctx, &stored, &account.id).await?;
                        changed = true;
                    }
                    RuleActionKind::Flag => {
                        apply_flagged(ctx, &stored, &account.id).await?;
                        changed = true;
                    }
                    RuleActionKind::Move => {
                        if let Some(target) = action.folder_id.clone() {
                            if target != summary.folder_id {
                                apply_move(ctx, &stored, &account.id, &target).await?;
                                changed = true;
                            }
                        }
                    }
                    RuleActionKind::Delete => {
                        let target = trash.clone();
                        apply_delete(ctx, &stored, &account.id, target.as_deref()).await?;
                        changed = true;
                    }
                    RuleActionKind::MarkJunk => {
                        if let Some(target) = junk.clone() {
                            apply_move(ctx, &stored, &account.id, &target).await?;
                            changed = true;
                        }
                    }
                    RuleActionKind::Archive => {
                        apply_archive(ctx, &stored, &account.id, all.as_deref()).await?;
                        changed = true;
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

async fn apply_seen(
    ctx: &CoreContext,
    stored: &nuncio_core::models::StoredMessage,
    account_id: &str,
) -> Result<()> {
    let id = stored.summary.id.clone();
    ctx.db
        .write(move |db| repo::messages::set_flags(db, &[id], Some(true), None, None, None))
        .await?;
    if let Some(uid) = stored.uid {
        crate::ops::enqueue_set_flags(
            ctx,
            account_id,
            &stored.summary.folder_id,
            &[uid],
            Some(true),
            None,
            None,
            None,
            "Rule: mark as read",
        )
        .await?;
    }
    Ok(())
}

async fn apply_flagged(
    ctx: &CoreContext,
    stored: &nuncio_core::models::StoredMessage,
    account_id: &str,
) -> Result<()> {
    let id = stored.summary.id.clone();
    ctx.db
        .write(move |db| repo::messages::set_flags(db, &[id], None, Some(true), None, None))
        .await?;
    if let Some(uid) = stored.uid {
        crate::ops::enqueue_set_flags(
            ctx,
            account_id,
            &stored.summary.folder_id,
            &[uid],
            None,
            Some(true),
            None,
            None,
            "Rule: flag message",
        )
        .await?;
    }
    Ok(())
}

async fn apply_move(
    ctx: &CoreContext,
    stored: &nuncio_core::models::StoredMessage,
    account_id: &str,
    target_folder: &str,
) -> Result<()> {
    let ids = vec![stored.summary.id.clone()];
    let target = target_folder.to_string();
    ctx.db
        .write(move |db| repo::messages::move_to_folder(db, &ids, &target))
        .await?;
    let uids: Vec<i64> = stored.uid.into_iter().collect();
    if !uids.is_empty() {
        crate::ops::enqueue_move(
            ctx,
            account_id,
            &stored.summary.folder_id,
            target_folder,
            &uids,
            "Rule: move message",
        )
        .await?;
    }
    Ok(())
}

async fn apply_delete(
    ctx: &CoreContext,
    stored: &nuncio_core::models::StoredMessage,
    account_id: &str,
    trash: Option<&str>,
) -> Result<()> {
    if let Some(trash) = trash {
        apply_move(ctx, stored, account_id, trash).await
    } else {
        let ids = vec![stored.summary.id.clone()];
        ctx.db
            .write(move |db| repo::messages::delete_local(db, &ids, false))
            .await?;
        let uids: Vec<i64> = stored.uid.into_iter().collect();
        crate::ops::enqueue_delete(
            ctx,
            account_id,
            &stored.summary.folder_id,
            &uids,
            false,
            "Rule: delete message",
        )
        .await
    }
}

async fn apply_archive(
    ctx: &CoreContext,
    stored: &nuncio_core::models::StoredMessage,
    account_id: &str,
    all: Option<&str>,
) -> Result<()> {
    if let Some(all) = all {
        apply_move(ctx, stored, account_id, all).await?;
    } else {
        let ids = vec![stored.summary.id.clone()];
        ctx.db
            .write(move |db| repo::messages::delete_local(db, &ids, false))
            .await?;
    }
    let uids: Vec<i64> = stored.uid.into_iter().collect();
    crate::ops::enqueue_archive(
        ctx,
        account_id,
        &stored.summary.folder_id,
        &uids,
        "Rule: archive message",
    )
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
