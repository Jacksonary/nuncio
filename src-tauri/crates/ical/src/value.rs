//! Shared value helpers: date/time parsing & formatting, TEXT escaping,
//! line folding, DURATION handling and small percent-codec used by
//! `X-NUNCIO-*` properties.

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

/// `YYYY-MM-DD`
pub const DATE_FMT: &str = "%Y-%m-%d";
/// `YYYY-MM-DDTHH:MM:SS+HH:MM`
pub const OFFSET_DT_FMT: &str = "%Y-%m-%dT%H:%M:%S%:z";

/// A time zone resolved from a TZID string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TzSpec {
    /// A real IANA zone (DST aware).
    Named(Tz),
    /// A fixed offset (from a VTIMEZONE definition or an explicit offset).
    Fixed(FixedOffset),
}

impl TzSpec {
    pub fn utc() -> Self {
        Self::Named(Tz::UTC)
    }

    pub fn name(&self) -> String {
        match self {
            Self::Named(tz) => tz.name().to_string(),
            Self::Fixed(offset) => offset.to_string(),
        }
    }

    /// Convert a UTC instant into this zone (wall clock + offset).
    pub fn local_time(&self, utc: DateTime<Utc>) -> DateTime<FixedOffset> {
        match self {
            Self::Named(tz) => utc.with_timezone(tz).fixed_offset(),
            Self::Fixed(offset) => utc.with_timezone(offset),
        }
    }

    /// Resolve a local wall-clock time into an instant.
    ///
    /// During a DST gap (`LocalResult::None`) we move forward in 15 minute
    /// steps until a valid instant exists; during an overlap we pick the
    /// earlier instant, which is what calendar clients do.
    pub fn resolve_local(&self, naive: NaiveDateTime) -> DateTime<FixedOffset> {
        match self {
            Self::Fixed(offset) => offset
                .from_local_datetime(&naive)
                .single()
                .unwrap_or_else(|| offset.from_utc_datetime(&naive)),
            Self::Named(tz) => match tz.from_local_datetime(&naive) {
                chrono::LocalResult::Single(dt) => dt.fixed_offset(),
                chrono::LocalResult::Ambiguous(earlier, _later) => earlier.fixed_offset(),
                chrono::LocalResult::None => {
                    let mut probe = naive;
                    for _ in 0..16 {
                        probe += chrono::Duration::minutes(15);
                        if let Some(dt) = tz.from_local_datetime(&probe).earliest() {
                            return dt.fixed_offset();
                        }
                    }
                    tz.from_utc_datetime(&naive).fixed_offset()
                }
            },
        }
    }
}

/// The kind of zone a parsed date-time carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneKind {
    /// No zone information (floating local time).
    Floating,
    /// Trailing `Z`.
    Utc,
    /// Explicit `+HHMM` / `-HHMM` (tolerated even though iCalendar forbids it).
    Offset(FixedOffset),
}

/// A parsed DATE or DATE-TIME property value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedDate {
    pub date: NaiveDate,
    pub time: Option<NaiveTime>,
    pub zone: ZoneKind,
}

impl ParsedDate {
    pub fn is_date_only(self) -> bool {
        self.time.is_none()
    }

    pub fn naive(self) -> NaiveDateTime {
        self.date.and_time(
            self.time
                .unwrap_or_else(|| NaiveTime::from_hms_opt(0, 0, 0).unwrap_or_default()),
        )
    }

    /// Resolve to an instant. Explicit offsets win over `tz`.
    pub fn resolve(self, tz: TzSpec) -> DateTime<FixedOffset> {
        match self.zone {
            ZoneKind::Offset(offset) => offset
                .from_local_datetime(&self.naive())
                .single()
                .unwrap_or_else(|| offset.from_utc_datetime(&self.naive())),
            ZoneKind::Utc => Utc
                .from_local_datetime(&self.naive())
                .single()
                .unwrap_or_else(|| Utc.from_utc_datetime(&self.naive()))
                .fixed_offset(),
            ZoneKind::Floating => tz.resolve_local(self.naive()),
        }
    }
}

/// Parse an iCalendar DATE / DATE-TIME value.
///
/// Accepts `YYYYMMDD`, `YYYY-MM-DD`, `YYYYMMDDTHHMMSS[Z]`,
/// `YYYYMMDDTHHMM[Z]` and (tolerantly) RFC3339.
pub fn parse_date_value(value: &str) -> Option<ParsedDate> {
    let raw = value.trim();
    if raw.is_empty() {
        return None;
    }
    // Tolerate full RFC3339 produced by some vendors.
    if raw.len() >= 19 && (raw.contains('-') && raw.contains(':') && raw.contains('T')) {
        if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
            return Some(ParsedDate {
                date: dt.date_naive(),
                time: Some(dt.time()),
                zone: ZoneKind::Offset(*dt.offset()),
            });
        }
    }

    let (date_part, rest) = match raw.split_once('T').or_else(|| raw.split_once('t')) {
        Some((d, t)) => (d, Some(t)),
        None => (raw, None),
    };

    let date = parse_compact_date(date_part)?;
    let Some(time_part) = rest else {
        return Some(ParsedDate {
            date,
            time: None,
            zone: ZoneKind::Floating,
        });
    };

    let mut time_part = time_part.trim();
    let mut zone = ZoneKind::Floating;
    if let Some(stripped) = time_part
        .strip_suffix('Z')
        .or_else(|| time_part.strip_suffix('z'))
    {
        time_part = stripped;
        zone = ZoneKind::Utc;
    } else if let Some((body, offset)) = split_trailing_offset(time_part) {
        time_part = body;
        zone = ZoneKind::Offset(offset);
    }

    let time = parse_compact_time(time_part)?;
    Some(ParsedDate {
        date,
        time: Some(time),
        zone,
    })
}

fn parse_compact_date(value: &str) -> Option<NaiveDate> {
    if value.len() == 8 && value.bytes().all(|b| b.is_ascii_digit()) {
        NaiveDate::parse_from_str(value, "%Y%m%d").ok()
    } else {
        NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .ok()
            .or_else(|| NaiveDate::parse_from_str(value, "%Y%m%d").ok())
    }
}

fn parse_compact_time(value: &str) -> Option<NaiveTime> {
    let v = value.trim();
    NaiveTime::parse_from_str(v, "%H%M%S")
        .ok()
        .or_else(|| NaiveTime::parse_from_str(v, "%H:%M:%S").ok())
        .or_else(|| NaiveTime::parse_from_str(v, "%H%M").ok())
        .or_else(|| NaiveTime::parse_from_str(v, "%H:%M").ok())
}

/// Split a trailing `+0800` / `-05:00` from a time value.
fn split_trailing_offset(value: &str) -> Option<(&str, FixedOffset)> {
    let bytes = value.as_bytes();
    if bytes.len() < 5 {
        return None;
    }
    // Find a '+' or '-' after the hour digits (never at index 0).
    let idx = value[1..].rfind(['+', '-']).map(|i| i + 1)?;
    let (body, offset) = value.split_at(idx);
    let offset = parse_offset(offset)?;
    Some((body, offset))
}

/// Parse `+HHMM`, `-HHMM`, `+HH:MM`, `+HHMMSS`.
pub fn parse_offset(value: &str) -> Option<FixedOffset> {
    let mut chars = value.chars();
    let sign = match chars.next()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    let rest: String = chars.filter(|c| *c != ':').collect();
    if rest.len() != 4 && rest.len() != 6 {
        return None;
    }
    let hours: i32 = rest.get(0..2)?.parse().ok()?;
    let minutes: i32 = rest.get(2..4)?.parse().ok()?;
    let seconds: i32 = if rest.len() == 6 {
        rest.get(4..6)?.parse().ok()?
    } else {
        0
    };
    if hours > 23 || minutes > 59 || seconds > 59 {
        return None;
    }
    FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60 + seconds))
}

/// Format a local (offset-aware) instant the way `nuncio-core` expects:
/// `YYYY-MM-DDTHH:MM:SS+HH:MM`.
pub fn format_local(dt: DateTime<FixedOffset>) -> String {
    dt.format(OFFSET_DT_FMT).to_string()
}

/// Format an all-day date.
pub fn format_date(date: NaiveDate) -> String {
    date.format(DATE_FMT).to_string()
}

/// Format a UTC instant converted to `tz`.
pub fn format_in_zone(utc: DateTime<Utc>, tz: TzSpec) -> String {
    format_local(tz.local_time(utc))
}

/// `YYYYMMDDTHHMMSSZ` for DTSTAMP / LAST-MODIFIED / COMPLETED.
pub fn format_utc_compact(utc: DateTime<Utc>) -> String {
    utc.format("%Y%m%dT%H%M%SZ").to_string()
}

/// `YYYYMMDD` for DTSTART;VALUE=DATE.
pub fn format_compact_date(date: NaiveDate) -> String {
    date.format("%Y%m%d").to_string()
}

/// `YYYYMMDDTHHMMSS` for a TZID-qualified property (no `Z`).
pub fn format_compact_local(dt: DateTime<FixedOffset>) -> String {
    dt.format("%Y%m%dT%H%M%S").to_string()
}

/// Parse an RFC3339-ish instant, tolerant of `YYYY-MM-DD`.
pub fn parse_instant(value: &str) -> Option<DateTime<Utc>> {
    let v = value.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(v) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Some(parsed) = parse_date_value(v) {
        let dt = parsed.resolve(TzSpec::utc());
        return Some(dt.with_timezone(&Utc));
    }
    None
}

/// Parse an ISO-8601 / iCalendar DURATION.
///
/// Supports `P[n]W`, `P[n]D`, `PT[n]H[n]M[n]S` and combinations, with an
/// optional leading sign. Months/years are rejected because they are not
/// fixed-length.
pub fn parse_duration(value: &str) -> Option<chrono::Duration> {
    let raw = value.trim();
    if raw.is_empty() {
        return None;
    }
    let (sign, body) = match raw.as_bytes()[0] {
        b'+' => (1, &raw[1..]),
        b'-' => (-1, &raw[1..]),
        _ => (1, raw),
    };
    let body = body.strip_prefix('P').or_else(|| body.strip_prefix('p'))?;
    if body.is_empty() {
        return None;
    }

    let mut seconds: i64 = 0;
    let mut in_time = false;
    let mut number = String::new();
    let mut saw_component = false;

    for ch in body.chars() {
        match ch {
            '0'..='9' => number.push(ch),
            'T' | 't' => {
                in_time = true;
                number.clear();
            }
            'W' | 'w' => {
                seconds += number.parse::<i64>().ok()? * 7 * 86_400;
                number.clear();
                saw_component = true;
            }
            'D' | 'd' => {
                seconds += number.parse::<i64>().ok()? * 86_400;
                number.clear();
                saw_component = true;
            }
            'H' | 'h' if in_time => {
                seconds += number.parse::<i64>().ok()? * 3_600;
                number.clear();
                saw_component = true;
            }
            'M' | 'm' if in_time => {
                seconds += number.parse::<i64>().ok()? * 60;
                number.clear();
                saw_component = true;
            }
            'S' | 's' if in_time => {
                seconds += number.parse::<i64>().ok()?;
                number.clear();
                saw_component = true;
            }
            _ => return None,
        }
    }

    if !saw_component || !number.is_empty() {
        return None;
    }
    Some(chrono::Duration::seconds(sign * seconds))
}

/// Format a duration the way `TRIGGER:-PT15M` expects (minutes only when it
/// divides evenly, otherwise seconds).
pub fn format_duration_minutes(minutes_before: i64) -> String {
    let sign = if minutes_before >= 0 { "-" } else { "" };
    let abs = minutes_before.unsigned_abs();
    if abs.is_multiple_of(60) && abs >= 60 {
        format!("{sign}PT{}H", abs / 60)
    } else {
        format!("{sign}PT{abs}M")
    }
}

/// Unescape an iCalendar TEXT value.
pub fn unescape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(';') => out.push(';'),
            Some(',') => out.push(','),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Escape a value for an iCalendar TEXT property.
pub fn escape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 8);
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            other => out.push(other),
        }
    }
    out
}

/// Decode RFC 6868 caret escapes (`^n`, `^^`, `^'`) used by vCard 4.0.
pub fn decode_caret(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '^' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some('^') => out.push('^'),
            Some('\'') => out.push('"'),
            Some(other) => {
                out.push('^');
                out.push(other);
            }
            None => out.push('^'),
        }
    }
    out
}

/// Split on a delimiter, ignoring delimiters that are backslash-escaped.
pub fn split_escaped(value: &str, delimiter: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            current.push('\\');
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            c if c == delimiter => {
                parts.push(std::mem::take(&mut current));
            }
            c => current.push(c),
        }
    }
    if escaped {
        current.push('\\');
    }
    parts.push(current);
    parts
}

/// Split a comma-separated value then unescape each part.
pub fn split_list(value: &str, delimiter: char) -> Vec<String> {
    split_escaped(value, delimiter)
        .into_iter()
        .map(|part| unescape_text(part.trim()))
        .filter(|part| !part.is_empty())
        .collect()
}

/// Fold a content line at 75 octets using CRLF + a single leading space.
/// Multi-byte UTF-8 characters are never split.
pub fn fold_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len() + line.len() / 70 + 4);
    let mut current = String::new();
    let mut current_len = 0usize;
    let mut first = true;

    for ch in line.chars() {
        let ch_len = ch.len_utf8();
        let limit = if first { 75 } else { 74 };
        if current_len + ch_len > limit && current_len > 0 {
            out.push_str(&current);
            out.push_str("\r\n ");
            current.clear();
            current_len = 0;
            first = false;
        }
        current.push(ch);
        current_len += ch_len;
    }
    out.push_str(&current);
    out.push_str("\r\n");
    out
}

/// Fold and push a content line into an output buffer.
pub fn push_line(out: &mut String, line: &str) {
    out.push_str(&fold_line(line));
}

/// Percent-encode a value for the compact `X-NUNCIO-STEP` representation:
/// `%`, `;`, CR and LF are escaped; everything else is kept as UTF-8.
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '%' => out.push_str("%25"),
            ';' => out.push_str("%3B"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            other => out.push(other),
        }
    }
    out
}

/// Decode [`percent_encode`].
pub fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'%' && idx + 2 < bytes.len() {
            let hex = &value[idx + 1..idx + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                idx += 3;
                continue;
            }
        }
        out.push(bytes[idx]);
        idx += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Strip the `mailto:` scheme and percent-decode the address.
pub fn normalize_mailto(value: &str) -> String {
    let trimmed = value.trim();
    let stripped = if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("mailto:") {
        &trimmed[7..]
    } else {
        trimmed
    };
    let decoded = percent_decode(stripped);
    decoded.trim().trim_matches('"').to_string()
}

/// Number of days in a month.
pub fn days_in_month(year: i32, month: u32) -> Option<u32> {
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let next = NaiveDate::from_ymd_opt(ny, nm, 1)?;
    Some((next - first).num_days() as u32)
}

/// Nth (or last, when negative) weekday of a month.
pub fn nth_weekday_of_month(
    year: i32,
    month: u32,
    weekday: chrono::Weekday,
    n: i32,
) -> Option<NaiveDate> {
    if n == 0 {
        return None;
    }
    if n > 0 {
        let first = NaiveDate::from_ymd_opt(year, month, 1)?;
        let offset = (weekday.num_days_from_monday() as i64
            - first.weekday().num_days_from_monday() as i64)
            .rem_euclid(7);
        let day = 1 + offset as u32 + (n as u32 - 1) * 7;
        NaiveDate::from_ymd_opt(year, month, day)
    } else {
        let last_day = days_in_month(year, month)?;
        let last = NaiveDate::from_ymd_opt(year, month, last_day)?;
        let offset = (last.weekday().num_days_from_monday() as i64
            - weekday.num_days_from_monday() as i64)
            .rem_euclid(7);
        let day = last_day as i64 - offset - (-n as i64 - 1) * 7;
        if day < 1 {
            None
        } else {
            NaiveDate::from_ymd_opt(year, month, day as u32)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    #[test]
    fn parses_compact_date_and_datetime() {
        let d = parse_date_value("20261112").unwrap();
        assert_eq!(d.date, NaiveDate::from_ymd_opt(2026, 11, 12).unwrap());
        assert!(d.time.is_none());

        let dt = parse_date_value("20261112T090000Z").unwrap();
        assert_eq!(dt.zone, ZoneKind::Utc);
        assert_eq!(dt.naive().hour(), 9);

        let dt = parse_date_value("2026-11-12T09:00:00+08:00").unwrap();
        assert!(matches!(dt.zone, ZoneKind::Offset(o) if o.local_minus_utc() == 8 * 3600));
    }

    #[test]
    fn duration_parsing() {
        assert_eq!(
            parse_duration("-PT15M"),
            Some(chrono::Duration::minutes(-15))
        );
        assert_eq!(parse_duration("P1D"), Some(chrono::Duration::days(1)));
        assert_eq!(parse_duration("P2W"), Some(chrono::Duration::weeks(2)));
        assert_eq!(
            parse_duration("PT1H30M"),
            Some(chrono::Duration::minutes(90))
        );
        assert_eq!(parse_duration("P1M"), None); // ambiguous month
    }

    #[test]
    fn escapes_and_splits() {
        assert_eq!(unescape_text(r"a\,b\;c\\d\ne"), "a,b;c\\d\ne");
        assert_eq!(escape_text("a,b;c\\d\ne"), r"a\,b\;c\\d\ne");
        assert_eq!(split_list("a,b\\,c", ','), vec!["a", "b,c"]);
    }

    #[test]
    fn folds_long_utf8_lines() {
        let line = "SUMMARY:".to_string() + &"日历".repeat(40);
        let folded = fold_line(&line);
        for part in folded.split("\r\n") {
            assert!(part.len() <= 75, "line too long: {}", part.len());
        }
        assert!(folded.contains("\r\n "));
    }

    #[test]
    fn nth_weekday() {
        // 2nd Tuesday of November 2026 is the 10th.
        assert_eq!(
            nth_weekday_of_month(2026, 11, chrono::Weekday::Tue, 2),
            NaiveDate::from_ymd_opt(2026, 11, 10)
        );
        // Last Friday of November 2026 is the 27th.
        assert_eq!(
            nth_weekday_of_month(2026, 11, chrono::Weekday::Fri, -1),
            NaiveDate::from_ymd_opt(2026, 11, 27)
        );
    }
}
