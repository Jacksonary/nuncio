//! Account repository.

use chrono::{DateTime, Utc};
use rusqlite::types::Value;
use rusqlite::{params, Connection, Row};

use crate::error::AppError;
use crate::models::{
    Account, AccountPatch, AccountState, AuthKind, OAuthProvider, ProviderCapabilities,
    ProviderKind, ServerConfig,
};
use crate::repo::{
    conflict, dt_string, get_bool, get_dt, get_opt_string, get_string, json_opt, json_text,
    now_string,
};
use crate::Result;

const COLUMNS: &str = "\
    id, provider, email, display_name, color, auth_kind, oauth_provider,\
    imap_json, smtp_json, pop3_json, caldav_url, carddav_url,\
    sync_window_days, mail_enabled, calendar_enabled, contacts_enabled, tasks_enabled,\
    notifications_enabled, include_in_unified, sort_order, state, state_message,\
    capabilities_json, created_at";

fn row_to_account(row: &Row<'_>) -> Result<Account> {
    let provider: String = get_string(row, "provider")?;
    let provider = provider.parse::<ProviderKind>()?;
    let auth_kind: String = get_string(row, "auth_kind")?;
    let auth_kind = auth_kind.parse::<AuthKind>()?;
    let oauth_provider = get_opt_string(row, "oauth_provider")?
        .and_then(|value| value.parse::<OAuthProvider>().ok());
    let state: String = get_string(row, "state")?;
    let state = state.parse::<AccountState>().unwrap_or(AccountState::Ok);
    let imap: Option<ServerConfig> =
        json_opt(get_opt_string(row, "imap_json")?.as_deref(), "imap_json");
    let smtp: Option<ServerConfig> =
        json_opt(get_opt_string(row, "smtp_json")?.as_deref(), "smtp_json");
    let pop3: Option<ServerConfig> =
        json_opt(get_opt_string(row, "pop3_json")?.as_deref(), "pop3_json");
    let capabilities: ProviderCapabilities = json_opt(
        get_opt_string(row, "capabilities_json")?.as_deref(),
        "capabilities_json",
    )
    .unwrap_or_else(|| ProviderCapabilities::defaults_for(provider));

    Ok(Account {
        id: get_string(row, "id")?,
        provider,
        email: get_string(row, "email")?,
        display_name: get_string(row, "display_name")?,
        color: get_string(row, "color")?,
        auth_kind,
        oauth_provider,
        imap,
        smtp,
        pop3,
        caldav_url: get_opt_string(row, "caldav_url")?,
        carddav_url: get_opt_string(row, "carddav_url")?,
        sync_window_days: row.get::<_, i64>("sync_window_days")?.max(0) as u32,
        mail_enabled: get_bool(row, "mail_enabled")?,
        calendar_enabled: get_bool(row, "calendar_enabled")?,
        contacts_enabled: get_bool(row, "contacts_enabled")?,
        tasks_enabled: get_bool(row, "tasks_enabled")?,
        notifications_enabled: get_bool(row, "notifications_enabled")?,
        include_in_unified: get_bool(row, "include_in_unified")?,
        sort_order: row.get("sort_order")?,
        state,
        state_message: get_opt_string(row, "state_message")?,
        capabilities,
        created_at: get_dt(row, "created_at")?,
    })
}

pub fn list(conn: &Connection) -> Result<Vec<Account>> {
    let sql = format!("SELECT {COLUMNS} FROM account ORDER BY sort_order ASC, created_at ASC");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut accounts = Vec::new();
    while let Some(row) = rows.next()? {
        accounts.push(row_to_account(row)?);
    }
    Ok(accounts)
}

pub fn list_mail_accounts(conn: &Connection) -> Result<Vec<Account>> {
    Ok(list(conn)?
        .into_iter()
        .filter(|account| account.mail_enabled && !account.provider.is_local())
        .collect())
}

pub fn list_unified(conn: &Connection) -> Result<Vec<Account>> {
    Ok(list(conn)?
        .into_iter()
        .filter(|account| account.include_in_unified)
        .collect())
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Account>> {
    let sql = format!("SELECT {COLUMNS} FROM account WHERE id = ?1");
    crate::repo::query_one(conn, &sql, params![id], row_to_account)
}

pub fn get_required(conn: &Connection, id: &str) -> Result<Account> {
    get(conn, id)?.ok_or_else(|| AppError::NotFound(format!("account {id} not found")))
}

pub fn local(conn: &Connection) -> Result<Account> {
    get_required(conn, "local")
}

pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM account", [], |row| row.get(0))?)
}

fn account_values(account: &Account) -> Result<Vec<Value>> {
    let to_json = |value: &Option<ServerConfig>| -> Result<Value> {
        match value {
            Some(config) => Ok(Value::Text(json_text(config)?)),
            None => Ok(Value::Null),
        }
    };
    Ok(vec![
        Value::Text(account.id.clone()),
        Value::Text(account.provider.as_str().to_string()),
        Value::Text(account.email.clone()),
        Value::Text(account.display_name.clone()),
        Value::Text(account.color.clone()),
        Value::Text(account.auth_kind.as_str().to_string()),
        match account.oauth_provider {
            Some(provider) => Value::Text(provider.as_str().to_string()),
            None => Value::Null,
        },
        to_json(&account.imap)?,
        to_json(&account.smtp)?,
        to_json(&account.pop3)?,
        account
            .caldav_url
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        account
            .carddav_url
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        Value::Integer(account.sync_window_days as i64),
        Value::Integer(account.mail_enabled as i64),
        Value::Integer(account.calendar_enabled as i64),
        Value::Integer(account.contacts_enabled as i64),
        Value::Integer(account.tasks_enabled as i64),
        Value::Integer(account.notifications_enabled as i64),
        Value::Integer(account.include_in_unified as i64),
        Value::Integer(account.sort_order),
        Value::Text(account.state.as_str().to_string()),
        account
            .state_message
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        Value::Text(json_text(&account.capabilities)?),
        Value::Text(dt_string(account.created_at)),
    ])
}

/// Insert a new account. Fails with `Conflict` if the id already exists.
pub fn insert(conn: &Connection, account: &Account) -> Result<()> {
    let values = account_values(account)?;
    let sql = "INSERT INTO account (
        id, provider, email, display_name, color, auth_kind, oauth_provider,
        imap_json, smtp_json, pop3_json, caldav_url, carddav_url,
        sync_window_days, mail_enabled, calendar_enabled, contacts_enabled, tasks_enabled,
        notifications_enabled, include_in_unified, sort_order, state, state_message,
        capabilities_json, created_at, updated_at
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)";
    let mut all = values;
    all.push(Value::Text(now_string()));
    let refs: Vec<&dyn rusqlite::ToSql> = all.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
    match conn.execute(sql, refs.as_slice()) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(error, _))
            if error.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Err(conflict(format!("account {} already exists", account.id)))
        }
        Err(error) => Err(error.into()),
    }
}

/// Insert or replace an account by id.
pub fn upsert(conn: &Connection, account: &Account) -> Result<()> {
    let values = account_values(account)?;
    let sql = "INSERT INTO account (
        id, provider, email, display_name, color, auth_kind, oauth_provider,
        imap_json, smtp_json, pop3_json, caldav_url, carddav_url,
        sync_window_days, mail_enabled, calendar_enabled, contacts_enabled, tasks_enabled,
        notifications_enabled, include_in_unified, sort_order, state, state_message,
        capabilities_json, created_at, updated_at
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)
    ON CONFLICT(id) DO UPDATE SET
        provider = excluded.provider,
        email = excluded.email,
        display_name = excluded.display_name,
        color = excluded.color,
        auth_kind = excluded.auth_kind,
        oauth_provider = excluded.oauth_provider,
        imap_json = excluded.imap_json,
        smtp_json = excluded.smtp_json,
        pop3_json = excluded.pop3_json,
        caldav_url = excluded.caldav_url,
        carddav_url = excluded.carddav_url,
        sync_window_days = excluded.sync_window_days,
        mail_enabled = excluded.mail_enabled,
        calendar_enabled = excluded.calendar_enabled,
        contacts_enabled = excluded.contacts_enabled,
        tasks_enabled = excluded.tasks_enabled,
        notifications_enabled = excluded.notifications_enabled,
        include_in_unified = excluded.include_in_unified,
        sort_order = excluded.sort_order,
        state = excluded.state,
        state_message = excluded.state_message,
        capabilities_json = excluded.capabilities_json,
        updated_at = excluded.updated_at";
    let mut all = values;
    all.push(Value::Text(now_string()));
    let refs: Vec<&dyn rusqlite::ToSql> = all.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
    conn.execute(sql, refs.as_slice())?;
    Ok(())
}

pub fn update_state(
    conn: &Connection,
    id: &str,
    state: AccountState,
    message: Option<&str>,
) -> Result<()> {
    let changed = conn.execute(
        "UPDATE account SET state = ?2, state_message = ?3, updated_at = ?4 WHERE id = ?1",
        params![id, state.as_str(), message, now_string()],
    )?;
    if changed == 0 {
        return Err(AppError::NotFound(format!("account {id} not found")));
    }
    Ok(())
}

pub fn set_capabilities(
    conn: &Connection,
    id: &str,
    capabilities: &ProviderCapabilities,
) -> Result<()> {
    conn.execute(
        "UPDATE account SET capabilities_json = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, json_text(capabilities)?, now_string()],
    )?;
    Ok(())
}

pub fn set_sort_order(conn: &Connection, id: &str, sort_order: i64) -> Result<()> {
    conn.execute(
        "UPDATE account SET sort_order = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, sort_order, now_string()],
    )?;
    Ok(())
}

/// Persist the given display order (index in `ids` becomes `sort_order`).
pub fn reorder(conn: &Connection, ids: &[String]) -> Result<()> {
    for (index, id) in ids.iter().enumerate() {
        set_sort_order(conn, id, index as i64)?;
    }
    Ok(())
}

/// Apply an `account_update` patch and return the updated account.
pub fn update_patch(conn: &Connection, id: &str, patch: &AccountPatch) -> Result<Account> {
    let mut account = get_required(conn, id)?;
    if let Some(value) = &patch.display_name {
        account.display_name = value.clone();
    }
    if let Some(value) = &patch.color {
        account.color = value.clone();
    }
    if let Some(value) = &patch.imap {
        account.imap = value.clone();
    }
    if let Some(value) = &patch.smtp {
        account.smtp = value.clone();
    }
    if let Some(value) = &patch.pop3 {
        account.pop3 = value.clone();
    }
    if let Some(value) = &patch.caldav_url {
        account.caldav_url = value.clone();
    }
    if let Some(value) = &patch.carddav_url {
        account.carddav_url = value.clone();
    }
    if let Some(value) = patch.sync_window_days {
        account.sync_window_days = value;
    }
    if let Some(value) = patch.mail_enabled {
        account.mail_enabled = value;
    }
    if let Some(value) = patch.calendar_enabled {
        account.calendar_enabled = value;
    }
    if let Some(value) = patch.contacts_enabled {
        account.contacts_enabled = value;
    }
    if let Some(value) = patch.tasks_enabled {
        account.tasks_enabled = value;
    }
    if let Some(value) = patch.notifications_enabled {
        account.notifications_enabled = value;
    }
    if let Some(value) = patch.include_in_unified {
        account.include_in_unified = value;
    }
    upsert(conn, &account)?;
    Ok(account)
}

/// Delete an account (cascades to folders, messages, calendars, contacts...).
pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    if id == "local" {
        return Err(AppError::invalid_input(
            "the built-in local account cannot be deleted",
        ));
    }
    conn.execute(
        "DELETE FROM fts_mail WHERE message_id IN (SELECT id FROM message WHERE account_id = ?1)",
        params![id],
    )?;
    conn.execute("DELETE FROM undo_log WHERE account_id = ?1", params![id])?;
    conn.execute("DELETE FROM sync_state WHERE account_id = ?1", params![id])?;
    let changed = conn.execute("DELETE FROM account WHERE id = ?1", params![id])?;
    if changed == 0 {
        return Err(AppError::NotFound(format!("account {id} not found")));
    }
    Ok(())
}

/// Next `sort_order` value for a new account.
pub fn next_sort_order(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM account",
        [],
        |row| row.get(0),
    )?)
}

/// Update the `last_sync_at` marker kept in `sync_state` (module-agnostic).
pub fn touch_synced_at(conn: &Connection, id: &str, at: DateTime<Utc>) -> Result<()> {
    crate::repo::sync_state::set(conn, id, "core", "last_sync_at", &dt_string(at))?;
    Ok(())
}
