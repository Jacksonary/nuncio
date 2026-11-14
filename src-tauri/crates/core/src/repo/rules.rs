//! Local mail rule repository.

use rusqlite::{params, Connection, Row};

use crate::error::AppError;
use crate::models::{new_id, MailRule, RuleAction, RuleCondition};
use crate::repo::{get_bool, get_opt_string, get_string, json_or_default, json_text, now_string};
use crate::Result;

const COLUMNS: &str =
    "id, account_id, name, enabled, match_all, conditions_json, actions_json, stop_processing, sort_order";

fn row_to_rule(row: &Row<'_>) -> Result<MailRule> {
    let conditions: Option<String> = row.get("conditions_json")?;
    let actions: Option<String> = row.get("actions_json")?;
    Ok(MailRule {
        id: get_string(row, "id")?,
        account_id: get_opt_string(row, "account_id")?,
        name: get_string(row, "name")?,
        enabled: get_bool(row, "enabled")?,
        match_all: get_bool(row, "match_all")?,
        conditions: json_or_default::<Vec<RuleCondition>>(conditions.as_deref(), "conditions_json"),
        actions: json_or_default::<Vec<RuleAction>>(actions.as_deref(), "actions_json"),
        stop_processing: get_bool(row, "stop_processing")?,
        sort_order: row.get("sort_order")?,
    })
}

pub fn list(conn: &Connection) -> Result<Vec<MailRule>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM mail_rule ORDER BY sort_order ASC, name COLLATE NOCASE ASC"
    ))?;
    let mut rows = stmt.query([])?;
    let mut rules = Vec::new();
    while let Some(row) = rows.next()? {
        rules.push(row_to_rule(row)?);
    }
    Ok(rules)
}

/// Enabled rules that apply to an account (account-specific + global), sorted.
pub fn list_enabled(conn: &Connection, account_id: &str) -> Result<Vec<MailRule>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM mail_rule
         WHERE enabled = 1 AND (account_id = ?1 OR account_id IS NULL)
         ORDER BY sort_order ASC, name COLLATE NOCASE ASC"
    ))?;
    let mut rows = stmt.query(params![account_id])?;
    let mut rules = Vec::new();
    while let Some(row) = rows.next()? {
        rules.push(row_to_rule(row)?);
    }
    Ok(rules)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<MailRule>> {
    let sql = format!("SELECT {COLUMNS} FROM mail_rule WHERE id = ?1");
    crate::repo::query_one(conn, &sql, params![id], row_to_rule)
}

pub fn get_required(conn: &Connection, id: &str) -> Result<MailRule> {
    get(conn, id)?.ok_or_else(|| AppError::NotFound(format!("rule {id} not found")))
}

pub fn upsert(conn: &Connection, rule: &MailRule) -> Result<MailRule> {
    let mut rule = rule.clone();
    if rule.id.trim().is_empty() {
        rule.id = new_id();
    }
    if rule.name.trim().is_empty() {
        rule.name = "Rule".to_string();
    }
    conn.execute(
        "INSERT INTO mail_rule (id, account_id, name, enabled, match_all, conditions_json, actions_json,
                                stop_processing, sort_order, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
         ON CONFLICT(id) DO UPDATE SET
             account_id = excluded.account_id,
             name = excluded.name,
             enabled = excluded.enabled,
             match_all = excluded.match_all,
             conditions_json = excluded.conditions_json,
             actions_json = excluded.actions_json,
             stop_processing = excluded.stop_processing,
             sort_order = excluded.sort_order,
             updated_at = excluded.updated_at",
        params![
            rule.id,
            rule.account_id,
            rule.name,
            rule.enabled as i64,
            rule.match_all as i64,
            json_text(&rule.conditions)?,
            json_text(&rule.actions)?,
            rule.stop_processing as i64,
            rule.sort_order,
            now_string()
        ],
    )?;
    Ok(rule)
}

pub fn set_enabled(conn: &Connection, id: &str, enabled: bool) -> Result<()> {
    conn.execute(
        "UPDATE mail_rule SET enabled = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, enabled as i64, now_string()],
    )?;
    Ok(())
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    let changed = conn.execute("DELETE FROM mail_rule WHERE id = ?1", params![id])?;
    if changed == 0 {
        return Err(AppError::NotFound(format!("rule {id} not found")));
    }
    Ok(())
}

pub fn next_sort_order(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM mail_rule",
        [],
        |row| row.get(0),
    )?)
}
