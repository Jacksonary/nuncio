//! Calendar event repository.
//!
//! Recurrence is **not** expanded in the database: the repository returns the
//! series masters (plus detached exceptions overlapping the range) and the
//! `nuncio-ical` crate expands RRULEs for the requested window.

use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::types::Value;
use rusqlite::{params, Connection, Row};

use crate::error::AppError;
use crate::models::{
    new_id, Attendee, AttendeeStatus, CalendarEvent, EventStatus, RecurrenceRule, ShowAs,
    StoredEvent,
};
use crate::repo::{
    get_bool, get_opt_dt, get_opt_string, get_string, int, json_opt, json_or_default, json_text,
    now_string, text,
};
use crate::Result;

const COLUMNS: &str = "\
    id, calendar_id, account_id, uid, title, location, description, start_at, end_at, start_utc, end_utc,\
    all_day, time_zone, recurrence_json, rrule, exdates_json, recurrence_id, series_id, status, show_as,\
    is_private, organizer_json, attendees_json, reminders_json, is_organizer, my_response,\
    online_meeting_url, color, raw_ics, etag, remote_id, href, updated_at";

/// Normalize an event instant to a UTC RFC3339 string for range comparisons.
/// Accepts RFC3339 (with any offset) and `YYYY-MM-DD` date-only values.
pub fn to_utc(value: &str) -> String {
    let trimmed = value.trim();
    if let Ok(parsed) = DateTime::parse_from_rfc3339(trimmed) {
        return parsed
            .with_timezone(&Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    }
    if let Ok(date) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        if let Some(naive) = date.and_hms_opt(0, 0, 0) {
            return DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        }
    }
    // Last resort: keep the value, it is still lexicographically stable.
    trimmed.to_string()
}

fn row_to_stored(row: &Row<'_>) -> Result<StoredEvent> {
    let recurrence: Option<String> = row.get("recurrence_json")?;
    let organizer: Option<String> = row.get("organizer_json")?;
    let attendees: Option<String> = row.get("attendees_json")?;
    let reminders: Option<String> = row.get("reminders_json")?;
    let exdates: Option<String> = row.get("exdates_json")?;
    let status: String = get_string(row, "status")?;
    let show_as: String = get_string(row, "show_as")?;
    let my_response: Option<String> = get_opt_string(row, "my_response")?;

    let event = CalendarEvent {
        id: get_string(row, "id")?,
        calendar_id: get_string(row, "calendar_id")?,
        account_id: get_string(row, "account_id")?,
        uid: get_string(row, "uid")?,
        title: get_string(row, "title")?,
        location: get_string(row, "location")?,
        description: get_string(row, "description")?,
        start: get_string(row, "start_at")?,
        end: get_string(row, "end_at")?,
        all_day: get_bool(row, "all_day")?,
        time_zone: get_string(row, "time_zone")?,
        recurrence: json_opt::<RecurrenceRule>(recurrence.as_deref(), "recurrence_json"),
        series_id: get_opt_string(row, "series_id")?,
        recurrence_id: get_opt_string(row, "recurrence_id")?,
        status: status.parse().unwrap_or(EventStatus::Confirmed),
        show_as: show_as.parse().unwrap_or(ShowAs::Busy),
        is_private: get_bool(row, "is_private")?,
        organizer: json_opt::<Attendee>(organizer.as_deref(), "organizer_json"),
        attendees: json_or_default::<Vec<Attendee>>(attendees.as_deref(), "attendees_json"),
        reminders: json_or_default::<Vec<i32>>(reminders.as_deref(), "reminders_json"),
        is_organizer: get_bool(row, "is_organizer")?,
        my_response: my_response.and_then(|value| value.parse::<AttendeeStatus>().ok()),
        online_meeting_url: get_opt_string(row, "online_meeting_url")?,
        color: get_opt_string(row, "color")?,
        updated_at: get_opt_dt(row, "updated_at")?,
    };
    Ok(StoredEvent {
        event,
        raw_ics: get_opt_string(row, "raw_ics")?,
        etag: get_opt_string(row, "etag")?,
        remote_id: get_opt_string(row, "remote_id")?,
        href: get_opt_string(row, "href")?,
        rrule: get_opt_string(row, "rrule")?,
        exdates: json_or_default(exdates.as_deref(), "exdates_json"),
    })
}

fn event_values(stored: &StoredEvent) -> Result<Vec<Value>> {
    let event = &stored.event;
    let recurrence = match &event.recurrence {
        Some(rule) => Value::Text(json_text(rule)?),
        None => Value::Null,
    };
    let organizer = match &event.organizer {
        Some(attendee) => Value::Text(json_text(attendee)?),
        None => Value::Null,
    };
    Ok(vec![
        text(&event.id),
        text(&event.calendar_id),
        text(&event.account_id),
        text(&event.uid),
        text(&event.title),
        text(&event.location),
        text(&event.description),
        text(&event.start),
        text(&event.end),
        text(to_utc(&event.start)),
        text(to_utc(&event.end)),
        int(event.all_day as i64),
        text(&event.time_zone),
        recurrence,
        stored.rrule.clone().map(Value::Text).unwrap_or(Value::Null),
        text(json_text(&stored.exdates)?),
        event
            .recurrence_id
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        event
            .series_id
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        text(event.status.as_str()),
        text(event.show_as.as_str()),
        int(event.is_private as i64),
        organizer,
        text(json_text(&event.attendees)?),
        text(json_text(&event.reminders)?),
        int(event.is_organizer as i64),
        event
            .my_response
            .map(|status| text(status.as_str()))
            .unwrap_or(Value::Null),
        event
            .online_meeting_url
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        event.color.clone().map(Value::Text).unwrap_or(Value::Null),
        stored
            .raw_ics
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        stored.etag.clone().map(Value::Text).unwrap_or(Value::Null),
        stored
            .remote_id
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        stored.href.clone().map(Value::Text).unwrap_or(Value::Null),
    ])
}

const UPSERT_SQL: &str = "\
INSERT INTO event (
    id, calendar_id, account_id, uid, title, location, description, start_at, end_at, start_utc, end_utc,
    all_day, time_zone, recurrence_json, rrule, exdates_json, recurrence_id, series_id, status, show_as,
    is_private, organizer_json, attendees_json, reminders_json, is_organizer, my_response,
    online_meeting_url, color, raw_ics, etag, remote_id, href, created_at, updated_at
) VALUES (
    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20,
    ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?33
)
ON CONFLICT(id) DO UPDATE SET
    calendar_id = excluded.calendar_id,
    account_id = excluded.account_id,
    uid = excluded.uid,
    title = excluded.title,
    location = excluded.location,
    description = excluded.description,
    start_at = excluded.start_at,
    end_at = excluded.end_at,
    start_utc = excluded.start_utc,
    end_utc = excluded.end_utc,
    all_day = excluded.all_day,
    time_zone = excluded.time_zone,
    recurrence_json = excluded.recurrence_json,
    rrule = excluded.rrule,
    exdates_json = excluded.exdates_json,
    recurrence_id = excluded.recurrence_id,
    series_id = excluded.series_id,
    status = excluded.status,
    show_as = excluded.show_as,
    is_private = excluded.is_private,
    organizer_json = excluded.organizer_json,
    attendees_json = excluded.attendees_json,
    reminders_json = excluded.reminders_json,
    is_organizer = excluded.is_organizer,
    my_response = excluded.my_response,
    online_meeting_url = excluded.online_meeting_url,
    color = excluded.color,
    raw_ics = excluded.raw_ics,
    etag = excluded.etag,
    remote_id = excluded.remote_id,
    href = excluded.href,
    updated_at = excluded.updated_at";

pub fn upsert(conn: &Connection, stored: &StoredEvent) -> Result<StoredEvent> {
    let mut stored = stored.clone();
    if stored.event.id.trim().is_empty() {
        stored.event.id = new_id();
    }
    let mut values = event_values(&stored)?;
    values.push(text(now_string()));
    let refs = crate::repo::query_params(&values);
    conn.execute(UPSERT_SQL, refs.as_slice())?;
    get_required(conn, &stored.event.id)
}

pub fn insert(conn: &Connection, stored: &StoredEvent) -> Result<StoredEvent> {
    upsert(conn, stored)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<StoredEvent>> {
    let sql = format!("SELECT {COLUMNS} FROM event WHERE id = ?1");
    crate::repo::query_one(conn, &sql, params![id], row_to_stored)
}

pub fn get_required(conn: &Connection, id: &str) -> Result<StoredEvent> {
    get(conn, id)?.ok_or_else(|| AppError::NotFound(format!("event {id} not found")))
}

pub fn by_uid(conn: &Connection, calendar_id: &str, uid: &str) -> Result<Option<StoredEvent>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM event WHERE calendar_id = ?1 AND uid = ?2 ORDER BY (series_id IS NULL) DESC LIMIT 1"
    );
    crate::repo::query_one(conn, &sql, params![calendar_id, uid], row_to_stored)
}

pub fn by_remote_id(
    conn: &Connection,
    calendar_id: &str,
    remote_id: &str,
) -> Result<Option<StoredEvent>> {
    let sql =
        format!("SELECT {COLUMNS} FROM event WHERE calendar_id = ?1 AND remote_id = ?2 LIMIT 1");
    crate::repo::query_one(conn, &sql, params![calendar_id, remote_id], row_to_stored)
}

pub fn by_href(conn: &Connection, calendar_id: &str, href: &str) -> Result<Option<StoredEvent>> {
    let sql = format!("SELECT {COLUMNS} FROM event WHERE calendar_id = ?1 AND href = ?2 LIMIT 1");
    crate::repo::query_one(conn, &sql, params![calendar_id, href], row_to_stored)
}

/// All events of a calendar (used by the CalDAV/Graph sync engines).
pub fn list_for_calendar(conn: &Connection, calendar_id: &str) -> Result<Vec<StoredEvent>> {
    let sql = format!("SELECT {COLUMNS} FROM event WHERE calendar_id = ?1 ORDER BY start_utc ASC");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![calendar_id])?;
    let mut events = Vec::new();
    while let Some(row) = rows.next()? {
        events.push(row_to_stored(row)?);
    }
    Ok(events)
}

/// Every event row (backup export / full restore).
pub fn list_all(conn: &Connection) -> Result<Vec<StoredEvent>> {
    let sql =
        format!("SELECT {COLUMNS} FROM event ORDER BY calendar_id ASC, start_utc ASC, id ASC");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut events = Vec::new();
    while let Some(row) = rows.next()? {
        events.push(row_to_stored(row)?);
    }
    Ok(events)
}

/// Events overlapping `[from, to)` plus every recurring master.
///
/// `calendar_ids = None` means "all visible calendars".
///
/// Every detached exception (`series_id`/`recurrence_id` set) is returned
/// regardless of where it currently sits. An occurrence can be moved outside
/// the window by a detached override while its original slot stays inside: the
/// expander needs the override to suppress that original slot, otherwise it
/// would emit a ghost occurrence (and schedule a reminder) at the old time.
/// The expander filters overrides by the window itself, so extra rows are
/// harmless for rendering.
pub fn range(
    conn: &Connection,
    from: &str,
    to: &str,
    calendar_ids: Option<&[String]>,
) -> Result<Vec<StoredEvent>> {
    let from_utc = to_utc(from);
    let to_utc_value = to_utc(to);
    let mut values: Vec<Value> = Vec::new();
    let calendar_clause = match calendar_ids {
        Some(ids) if !ids.is_empty() => {
            let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            for id in ids {
                values.push(text(id));
            }
            format!("calendar_id IN ({placeholders})")
        }
        Some(_) => "1 = 0".to_string(),
        None => "calendar_id IN (SELECT id FROM calendar WHERE is_visible = 1)".to_string(),
    };
    // The SQL clause reads `start_utc < to AND end_utc > from`.
    values.push(text(to_utc_value));
    values.push(text(from_utc));
    let sql = format!(
        "SELECT {COLUMNS} FROM event
         WHERE {calendar_clause}
           AND (rrule IS NOT NULL
                OR series_id IS NOT NULL
                OR recurrence_id IS NOT NULL
                OR (start_utc < ? AND end_utc > ?))
         ORDER BY start_utc ASC, id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let refs = crate::repo::query_params(&values);
    let mut rows = stmt.query(refs.as_slice())?;
    let mut events = Vec::new();
    while let Some(row) = rows.next()? {
        events.push(row_to_stored(row)?);
    }
    Ok(events)
}

/// Full-text-ish search over title/location/description (case-insensitive).
pub fn search(conn: &Connection, query: &str, limit: u32) -> Result<Vec<StoredEvent>> {
    let pattern = format!("%{}%", query.trim().to_lowercase());
    let sql = format!(
        "SELECT {COLUMNS} FROM event
         WHERE lower(title) LIKE ?1 OR lower(location) LIKE ?1 OR lower(description) LIKE ?1
         ORDER BY start_utc DESC LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![pattern, limit.clamp(1, 200) as i64])?;
    let mut events = Vec::new();
    while let Some(row) = rows.next()? {
        events.push(row_to_stored(row)?);
    }
    Ok(events)
}

pub fn set_my_response(conn: &Connection, id: &str, response: AttendeeStatus) -> Result<()> {
    conn.execute(
        "UPDATE event SET my_response = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, response.as_str(), now_string()],
    )?;
    Ok(())
}

pub fn set_status(conn: &Connection, id: &str, status: EventStatus) -> Result<()> {
    conn.execute(
        "UPDATE event SET status = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, status.as_str(), now_string()],
    )?;
    Ok(())
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM event WHERE id = ?1", params![id])?;
    Ok(())
}

/// Delete a whole series: the master plus every detached exception.
pub fn delete_series(conn: &Connection, master_id: &str) -> Result<usize> {
    let master = get(conn, master_id)?;
    let uid = master.as_ref().map(|stored| stored.event.uid.clone());
    let calendar = master
        .as_ref()
        .map(|stored| stored.event.calendar_id.clone());
    match (uid, calendar) {
        (Some(uid), Some(calendar_id)) => Ok(conn.execute(
            "DELETE FROM event WHERE calendar_id = ?1 AND (uid = ?2 OR series_id = ?3)",
            params![calendar_id, uid, master_id],
        )?),
        _ => Ok(0),
    }
}

pub fn delete_for_calendar(conn: &Connection, calendar_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM event WHERE calendar_id = ?1",
        params![calendar_id],
    )?;
    Ok(())
}

pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM event", [], |row| row.get(0))?)
}

/// Events for one day (used by the reminder scheduler to find upcoming ones).
pub fn upcoming(conn: &Connection, from: &str, to: &str) -> Result<Vec<StoredEvent>> {
    range(conn, from, to, None)
}
