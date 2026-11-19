//! Typed repositories for every table group.
//!
//! All functions are synchronous and take a `&rusqlite::Connection`, so they can
//! be used from unit tests with an in-memory database and from the async `Db`
//! helpers (`Db::read`, `Db::write`) inside the app.
//!
//! Convention:
//! * `list`/`get`/`find` are read-only helpers,
//! * `upsert`/`insert`/`update_*`/`delete` write helpers,
//! * JSON columns are decoded defensively: corrupt values log a warning and fall
//!   back to the type default instead of failing the whole view.

pub mod accounts;
pub mod address_books;
pub mod attachments;
pub mod calendars;
pub mod contacts;
pub mod drafts;
pub mod events;
pub mod folders;
pub mod identities;
pub mod messages;
pub mod outbox;
pub mod pending_ops;
pub mod pim_maps;
pub mod recipient_history;
pub mod reminder_state;
pub mod rules;
pub mod settings;
pub mod signatures;
pub mod sync_state;
pub mod task_lists;
pub mod tasks;
pub mod trusted_senders;
pub mod undo;

use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use rusqlite::types::Value;
use rusqlite::{Connection, Row, ToSql};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tracing::warn;

use crate::error::AppError;
use crate::Result;

/// Current instant as the canonical storage string (`...Z`, second precision).
pub fn now_string() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Format an instant for storage.
pub fn dt_string(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn dt_string_opt(value: Option<DateTime<Utc>>) -> Option<String> {
    value.map(dt_string)
}

/// Parse a stored timestamp. Accepts RFC3339 and naive `YYYY-MM-DDTHH:MM:SS`.
pub fn parse_dt(value: &str) -> Result<DateTime<Utc>> {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Ok(parsed.with_timezone(&Utc));
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(value, format) {
            return Ok(Utc.from_utc_datetime(&naive));
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        if let Some(naive) = date.and_hms_opt(0, 0, 0) {
            return Ok(Utc.from_utc_datetime(&naive));
        }
    }
    Err(AppError::Db(format!(
        "invalid timestamp in database: {value}"
    )))
}

pub fn parse_dt_opt(value: Option<String>) -> Result<Option<DateTime<Utc>>> {
    match value {
        Some(value) if !value.trim().is_empty() => parse_dt(&value).map(Some),
        _ => Ok(None),
    }
}

/// Parse a plain `YYYY-MM-DD` date string.
pub fn parse_date(value: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
        .map_err(|_| AppError::InvalidInput(format!("invalid date: {value}")))
}

pub fn json_text<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|e| AppError::internal(format!("JSON encode failed: {e}")))
}

pub fn json_text_or(value: Option<&str>, fallback: &str) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| fallback.to_string())
}

pub fn json_parse<T: DeserializeOwned>(value: &str) -> Result<T> {
    serde_json::from_str(value).map_err(|e| AppError::Db(format!("invalid JSON in database: {e}")))
}

/// Decode a JSON column, falling back to the default with a warning.
pub fn json_or_default<T: DeserializeOwned + Default>(value: Option<&str>, column: &str) -> T {
    match value {
        Some(text) if !text.trim().is_empty() => {
            serde_json::from_str(text).unwrap_or_else(|error| {
                warn!(column, %error, "ignoring corrupt JSON column");
                T::default()
            })
        }
        _ => T::default(),
    }
}

/// Decode an optional JSON column (`null` stays `None`).
pub fn json_opt<T: DeserializeOwned>(value: Option<&str>, column: &str) -> Option<T> {
    match value {
        Some(text) if !text.trim().is_empty() && text.trim() != "null" => {
            serde_json::from_str(text).unwrap_or_else(|error| {
                warn!(column, %error, "ignoring corrupt JSON column");
                None
            })
        }
        _ => None,
    }
}

pub fn get_bool(row: &Row<'_>, column: &str) -> rusqlite::Result<bool> {
    Ok(row.get::<_, i64>(column)? != 0)
}

pub fn get_opt_bool(row: &Row<'_>, column: &str) -> rusqlite::Result<Option<bool>> {
    Ok(row.get::<_, Option<i64>>(column)?.map(|value| value != 0))
}

pub fn get_string(row: &Row<'_>, column: &str) -> rusqlite::Result<String> {
    row.get::<_, String>(column)
}

pub fn get_opt_string(row: &Row<'_>, column: &str) -> rusqlite::Result<Option<String>> {
    row.get::<_, Option<String>>(column)
}

pub fn get_dt(row: &Row<'_>, column: &str) -> Result<DateTime<Utc>> {
    let value: String = row.get(column)?;
    parse_dt(&value)
}

pub fn get_opt_dt(row: &Row<'_>, column: &str) -> Result<Option<DateTime<Utc>>> {
    let value: Option<String> = row.get(column)?;
    parse_dt_opt(value)
}

/// Helper for building dynamic SQL with bound values.
pub fn text(value: impl Into<String>) -> Value {
    Value::Text(value.into())
}

pub fn int(value: i64) -> Value {
    Value::Integer(value)
}

/// Run a statement with `rusqlite::types::Value` parameters.
pub fn query_params(values: &[Value]) -> Vec<&dyn ToSql> {
    values.iter().map(|v| v as &dyn ToSql).collect()
}

/// Run a single-row query whose mapper returns an [`AppError`].
pub fn query_one<T, P>(
    conn: &Connection,
    sql: &str,
    params: P,
    f: impl FnOnce(&Row<'_>) -> Result<T>,
) -> Result<Option<T>>
where
    P: rusqlite::Params,
{
    use rusqlite::OptionalExtension;
    conn.query_row(sql, params, |row| {
        f(row).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Null,
                Box::new(error),
            )
        })
    })
    .optional()
    .map_err(AppError::from)
}

pub(crate) fn conflict(message: impl Into<String>) -> AppError {
    AppError::Conflict(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_round_trip() {
        let value = Utc.with_ymd_and_hms(2026, 11, 12, 8, 0, 0).unwrap();
        let encoded = dt_string(value);
        assert_eq!(encoded, "2026-11-12T08:00:00Z");
        assert_eq!(parse_dt(&encoded).unwrap(), value);
        assert_eq!(
            parse_dt("2026-11-12T08:00:00+08:00").unwrap(),
            Utc.with_ymd_and_hms(2026, 11, 12, 0, 0, 0).unwrap()
        );
        assert!(parse_dt("not a date").is_err());
    }

    #[test]
    fn date_parsing() {
        assert_eq!(parse_date("2026-11-12").unwrap().to_string(), "2026-11-12");
        assert!(parse_date("2026-13-01").is_err());
    }

    #[test]
    fn json_helpers_are_defensive() {
        let parsed: Vec<String> = json_or_default(Some("[\"a\"]"), "test");
        assert_eq!(parsed, vec!["a".to_string()]);
        let broken: Vec<String> = json_or_default(Some("not json"), "test");
        assert!(broken.is_empty());
        let none: Option<serde_json::Value> = json_opt(None, "test");
        assert!(none.is_none());
    }
}
