//! Small time / formatting helpers shared by the PIM providers.
//!
//! The core model stores event start/end as **strings**: `YYYY-MM-DD` for
//! all-day events and RFC 3339 with an offset for timed events.  Provider APIs
//! use a variety of representations (naive wall-clock + time zone, UTC naive,
//! compact iCalendar values).  Everything is normalised here so provider code
//! does not grow its own ad-hoc parsers.

use chrono::{
    DateTime, Duration, FixedOffset, Local, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc,
};
use chrono_tz::Tz;
use nuncio_core::models::CalendarEvent;

use crate::error::{invalid, Result};

/// Parse a core instant (`RFC3339` or `YYYY-MM-DD`, the latter in UTC).
pub fn parse_instant(value: &str) -> Result<DateTime<Utc>> {
    let value = value.trim();
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Ok(parsed.with_timezone(&Utc));
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        if let Some(naive) = date.and_hms_opt(0, 0, 0) {
            return Ok(Utc.from_utc_datetime(&naive));
        }
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(value, format) {
            return Ok(Utc.from_utc_datetime(&naive));
        }
    }
    Err(invalid(format!("invalid instant: {value}")))
}

pub fn parse_instant_opt(value: &str) -> Option<DateTime<Utc>> {
    parse_instant(value).ok()
}

/// Format an instant the way the core model expects (`...Z`, second precision).
pub fn format_instant(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn format_offset(value: DateTime<FixedOffset>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, false)
}

/// Return the date part of a core instant string.
pub fn date_part(value: &str) -> Option<String> {
    let value = value.trim();
    if NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok() {
        return Some(value.to_string());
    }
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|parsed| parsed.date_naive().format("%Y-%m-%d").to_string())
}

/// Resolve an IANA / Windows time-zone name.  Returns `None` for unknown names
/// (callers fall back to UTC).
pub fn resolve_zone(name: &str) -> Option<Tz> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(iana) = nuncio_ical::tz::windows_to_iana(trimmed) {
        return iana.parse::<Tz>().ok();
    }
    trimmed.parse::<Tz>().ok()
}

/// Normalise a provider time-zone name to an IANA name.  Unknown names become
/// `UTC` so that recurrence expansion stays deterministic.
pub fn normalize_zone(name: &str) -> String {
    match resolve_zone(name) {
        Some(tz) => tz.name().to_string(),
        None => "UTC".to_string(),
    }
}

/// Convert a naive wall-clock time in `zone` into an RFC 3339 string.
pub fn wall_to_rfc3339(naive: NaiveDateTime, zone: &str) -> Result<String> {
    let utc = Utc.from_utc_datetime(&naive);
    if let Some(tz) = resolve_zone(zone) {
        if tz == chrono_tz::UTC {
            return Ok(format_instant(utc));
        }
        if let Some(local) = tz
            .from_local_datetime(&naive)
            .single()
            .or_else(|| tz.from_local_datetime(&naive).earliest())
        {
            return Ok(local.to_rfc3339_opts(SecondsFormat::Secs, false));
        }
    }
    Ok(format_instant(utc))
}

/// Format an instant in `zone` as RFC 3339 with offset.
pub fn instant_in_zone(value: DateTime<Utc>, zone: &str) -> String {
    match resolve_zone(zone) {
        Some(tz) if tz == chrono_tz::UTC => format_instant(value),
        Some(tz) => value
            .with_timezone(&tz)
            .to_rfc3339_opts(SecondsFormat::Secs, false),
        None => format_instant(value),
    }
}

/// Convert a date-only value into the same format the model uses.
pub fn normalize_date(value: &str) -> Option<String> {
    let value = value.trim();
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Some(date.format("%Y-%m-%d").to_string());
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y%m%d") {
        return Some(date.format("%Y-%m-%d").to_string());
    }
    None
}

/// Normalise an iCalendar EXDATE/RDATE value to a core instant string.
pub fn normalize_ical_date(value: &str, zone: &str) -> Option<String> {
    let value = value.trim();
    if let Some(date) = normalize_date(value) {
        return Some(date);
    }
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Some(format_instant(parsed.with_timezone(&Utc)));
    }
    if let Ok(naive) = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ") {
        return Some(format_instant(Utc.from_utc_datetime(&naive)));
    }
    if let Ok(naive) = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S") {
        return wall_to_rfc3339(naive, zone).ok();
    }
    None
}

/// Format a core EXDATE value as an iCalendar `DATE`/`DATE-TIME` value.
///
/// Returns `(is_date_only, value)`.
pub fn ical_date_value(value: &str, zone: &str) -> (bool, String) {
    let value = value.trim();
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return (true, date.format("%Y%m%d").to_string());
    }
    if let Ok(instant) = DateTime::parse_from_rfc3339(value) {
        let instant = instant.with_timezone(&Utc);
        if let Some(tz) = resolve_zone(zone) {
            let local = instant.with_timezone(&tz);
            return (false, local.format("%Y%m%dT%H%M%S").to_string());
        }
        return (false, instant.format("%Y%m%dT%H%M%SZ").to_string());
    }
    (false, value.to_string())
}

/// Parse the naive date-time used by Microsoft Graph (`2026-11-12T08:00:00.0000000`).
pub fn parse_graph_datetime(value: &str) -> Option<NaiveDateTime> {
    let value = value.trim();
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(value, format) {
            return Some(naive);
        }
    }
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|parsed| parsed.naive_utc())
}

/// Convert a Graph date-time + time zone into a core instant string.
pub fn graph_to_instant(date_time: &str, zone: &str) -> Option<String> {
    let naive = parse_graph_datetime(date_time)?;
    wall_to_rfc3339(naive, zone).ok()
}

/// Convert a core instant into the UTC naive form Graph expects.
pub fn instant_to_graph(value: &str) -> Result<String> {
    let instant = parse_instant(value)?;
    Ok(instant.format("%Y-%m-%dT%H:%M:%S.0000000").to_string())
}

/// Duration of an event.  All-day events return whole days.
pub fn event_duration(event: &CalendarEvent) -> Duration {
    if event.all_day {
        let start = NaiveDate::parse_from_str(event.start.trim(), "%Y-%m-%d").ok();
        let end = NaiveDate::parse_from_str(event.end.trim(), "%Y-%m-%d").ok();
        return match (start, end) {
            (Some(start), Some(end)) if end > start => Duration::days((end - start).num_days()),
            _ => Duration::days(1),
        };
    }
    match (parse_instant(&event.start), parse_instant(&event.end)) {
        (Ok(start), Ok(end)) if end > start => end - start,
        _ => Duration::zero(),
    }
}

/// Add a duration to a core instant string, preserving its representation.
pub fn shift_instant(value: &str, amount: Duration) -> Result<String> {
    let value = value.trim();
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        let days = amount.num_days();
        let shifted = date + Duration::days(days);
        return Ok(shifted.format("%Y-%m-%d").to_string());
    }
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|_| invalid(format!("invalid instant: {value}")))?;
    Ok((parsed + amount).to_rfc3339_opts(SecondsFormat::Secs, false))
}

/// Convert an instant to a naive wall-clock string in `zone` using the
/// Microsoft Graph `dateTime` shape (`YYYY-MM-DDTHH:MM:SS.0000000`).
pub fn instant_to_graph_in_zone(value: &str, zone: &str) -> Result<String> {
    let instant = parse_instant(value)?;
    let naive = match resolve_zone(zone) {
        Some(tz) => instant.with_timezone(&tz).naive_local(),
        None => instant.naive_utc(),
    };
    Ok(naive.format("%Y-%m-%dT%H:%M:%S.0000000").to_string())
}

/// Start of an all-day occurrence (`YYYY-MM-DD`) as a UTC instant using the
/// event's time zone.  Falls back to the system local zone when the zone name
/// is unknown, then to UTC midnight as a last resort.
pub fn date_start_instant(value: &str, zone: &str) -> Option<DateTime<Utc>> {
    let date = NaiveDate::parse_from_str(&date_part(value)?, "%Y-%m-%d").ok()?;
    let naive = date.and_hms_opt(0, 0, 0)?;
    let resolved = resolve_zone(zone)
        .and_then(|tz| tz.from_local_datetime(&naive).earliest())
        .map(|local| local.with_timezone(&Utc))
        .or_else(|| {
            Local
                .from_local_datetime(&naive)
                .earliest()
                .map(|local| local.with_timezone(&Utc))
        });
    Some(resolved.unwrap_or_else(|| Utc.from_utc_datetime(&naive)))
}

/// Parse a core event start into an instant honouring all-day semantics.
pub fn event_start_instant(event: &CalendarEvent, value: &str) -> Option<DateTime<Utc>> {
    if event.all_day {
        date_start_instant(value, &event.time_zone)
    } else {
        parse_instant(value).ok()
    }
}

/// Parse a core event end into an instant honouring all-day semantics.
pub fn event_end_instant(event: &CalendarEvent, value: &str) -> Option<DateTime<Utc>> {
    if event.all_day {
        date_start_instant(value, &event.time_zone)
    } else {
        parse_instant(value).ok()
    }
}

/// End of an occurrence given the master's start/end and the occurrence start.
pub fn occurrence_end(event: &CalendarEvent, occurrence_start: &str) -> Result<String> {
    shift_instant(occurrence_start, event_duration(event))
}

/// Compare two core instant strings (RFC 3339 with different offsets or dates).
pub fn same_instant(left: &str, right: &str) -> bool {
    match (parse_instant(left), parse_instant(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left.trim() == right.trim(),
    }
}

/// Build the display color for an event: event override first, then calendar.
pub fn event_color(event: &CalendarEvent, calendar_color: Option<&str>) -> String {
    event
        .color
        .clone()
        .or_else(|| calendar_color.map(str::to_string))
        .unwrap_or_else(|| "#0078D4".to_string())
}

/// Normalise a hex color (`#RGB`, `#RRGGBB`, `#RRGGBBAA`).
pub fn normalize_color(value: &str) -> Option<String> {
    let value = value.trim();
    let hex = value.strip_prefix('#')?;
    match hex.len() {
        3 => {
            let mut out = String::from("#");
            for ch in hex.chars() {
                out.push(ch);
                out.push(ch);
            }
            Some(out.to_ascii_uppercase())
        }
        6 => Some(format!("#{}", hex.to_ascii_uppercase())),
        8 => Some(format!("#{}", hex[..6].to_ascii_uppercase())),
        _ => None,
    }
}

/// XML escape used when writing CalDAV request bodies.
pub fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// True when an attendee matches `email` (case-insensitive).
pub fn is_email(attendee_email: &str, email: &str) -> bool {
    attendee_email.trim().eq_ignore_ascii_case(email.trim())
}

/// Find "me" among attendees by e-mail.
pub fn find_self_index(event: &CalendarEvent, email: &str) -> Option<usize> {
    event
        .attendees
        .iter()
        .position(|attendee| is_email(&attendee.email, email))
}

/// Maximal number of expanded occurrences returned per series.
pub const MAX_OCCURRENCES_PER_SERIES: usize = 2000;

/// RRULE text to persist after an edit.
///
/// When the incoming structured rule is exactly the one derived from the
/// stored original text, the original text is kept so multi-ordinal BYDAY
/// rules survive round trips.  Otherwise the text is regenerated from the
/// structured rule (the user changed the recurrence).
pub fn rrule_for_update(
    original_rrule: Option<&str>,
    incoming: Option<&nuncio_core::models::RecurrenceRule>,
) -> Option<String> {
    let incoming = incoming?;
    if let Some(original) = original_rrule
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        if nuncio_ical::recurrence::from_rrule_string(original).as_ref() == Some(incoming) {
            return Some(original.to_string());
        }
    }
    Some(nuncio_ical::recurrence::to_rrule_string(incoming))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_core_instants() {
        assert!(parse_instant("2026-11-12").is_ok());
        assert!(parse_instant("2026-11-12T08:00:00Z").is_ok());
        assert!(parse_instant("not-a-date").is_err());
    }

    #[test]
    fn normalizes_colors() {
        assert_eq!(normalize_color("#abc").as_deref(), Some("#AABBCC"));
        assert_eq!(normalize_color("#112233ff").as_deref(), Some("#112233"));
        assert_eq!(normalize_color("red"), None);
    }

    #[test]
    fn ical_values_round_trip() {
        assert_eq!(
            normalize_ical_date("20261112", "UTC").as_deref(),
            Some("2026-11-12")
        );
        assert_eq!(
            normalize_ical_date("20261112T080000Z", "UTC").as_deref(),
            Some("2026-11-12T08:00:00Z")
        );
        let (is_date, value) = ical_date_value("2026-11-12", "UTC");
        assert!(is_date);
        assert_eq!(value, "20261112");
    }

    #[test]
    fn shifts_all_day_dates() {
        assert_eq!(
            shift_instant("2026-11-12", Duration::days(1)).unwrap(),
            "2026-11-13"
        );
    }

    #[test]
    fn graph_datetime_conversion() {
        assert_eq!(
            graph_to_instant("2026-11-12T08:00:00.0000000", "UTC").as_deref(),
            Some("2026-11-12T08:00:00Z")
        );
        assert_eq!(
            instant_to_graph("2026-11-12T08:00:00+08:00").unwrap(),
            "2026-11-12T00:00:00.0000000"
        );
    }

    #[test]
    fn keeps_the_original_rrule_unless_the_rule_changed() {
        let original = "FREQ=MONTHLY;BYDAY=1MO,3WE";
        let parsed = nuncio_ical::recurrence::from_rrule_string(original).expect("rule");
        assert_eq!(
            rrule_for_update(Some(original), Some(&parsed)).as_deref(),
            Some(original),
            "an unchanged (lossy) rule must keep its original text"
        );

        let mut changed = parsed.clone();
        changed.count = Some(5);
        let regenerated = rrule_for_update(Some(original), Some(&changed)).expect("text");
        assert!(regenerated.contains("COUNT=5"));
        assert_ne!(regenerated, original);
        assert_eq!(rrule_for_update(Some(original), None), None);
    }
}
