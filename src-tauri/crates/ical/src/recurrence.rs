//! RRULE conversion and occurrence expansion.
//!
//! Expansion is delegated to the [`rrule`] crate. The important detail is that
//! we expand in the event's **wall-clock** time zone: a weekly 09:00 meeting in
//! `America/New_York` stays at 09:00 local across the DST switch, while its UTC
//! instant moves by one hour.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc, Weekday as ChronoWeekday};
use nuncio_core::models::{
    new_id, CalendarEvent, EventStatus, RecurrenceFreq, RecurrenceRule, StoredEvent, Weekday,
};
use nuncio_core::{AppError, Result};
use rrule::{Frequency, NWeekday, RRule, RRuleSet, Tz as RuleTz, Unvalidated};

use crate::ics::parse_calendar;
use crate::value::{format_date, format_local, parse_date_value, parse_instant, TzSpec};

/// One expanded occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    /// Formatted like `CalendarEvent::start`.
    pub start: String,
    /// Formatted like `CalendarEvent::end`.
    pub end: String,
    /// The occurrence's original start (RECURRENCE-ID), even when moved.
    pub original_start: String,
}

/// The two halves produced by a "this and following" edit.
#[derive(Debug, Clone)]
pub struct SeriesSplit {
    /// The original series, truncated before the split instance.
    pub master: CalendarEvent,
    /// The new series that starts at the split instance.
    pub following: CalendarEvent,
}

impl SeriesSplit {
    /// Destructure into `(master, following)`.
    pub fn into_parts(self) -> (CalendarEvent, CalendarEvent) {
        (self.master, self.following)
    }
}

/// Convert a core recurrence rule into an RRULE property value
/// (`FREQ=WEEKLY;BYDAY=MO` — no `RRULE:` prefix).
pub fn to_rrule_string(rule: &RecurrenceRule) -> String {
    let mut parts = Vec::new();
    let freq = match rule.freq {
        RecurrenceFreq::Daily => "DAILY",
        RecurrenceFreq::Weekly => "WEEKLY",
        RecurrenceFreq::Monthly => "MONTHLY",
        RecurrenceFreq::Yearly => "YEARLY",
    };
    parts.push(format!("FREQ={freq}"));
    if rule.interval > 1 {
        parts.push(format!("INTERVAL={}", rule.interval));
    }
    if !rule.by_weekday.is_empty() {
        let days = rule
            .by_weekday
            .iter()
            .map(|day| day.as_str())
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!("BYDAY={days}"));
    }
    if !rule.by_month_day.is_empty() {
        let days = rule
            .by_month_day
            .iter()
            .map(|day| day.to_string())
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!("BYMONTHDAY={days}"));
    }
    if !rule.by_set_pos.is_empty() {
        let positions = rule
            .by_set_pos
            .iter()
            .map(|position| position.to_string())
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!("BYSETPOS={positions}"));
    }
    if !rule.by_month.is_empty() {
        let months = rule
            .by_month
            .iter()
            .map(|month| month.to_string())
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!("BYMONTH={months}"));
    }
    if let Some(count) = rule.count {
        if count > 0 {
            parts.push(format!("COUNT={count}"));
        }
    } else if let Some(until) = &rule.until {
        if let Some(parsed) = parse_date_value(until) {
            parts.push(format!("UNTIL={}", parsed.date.format("%Y%m%d")));
        }
    }
    parts.join(";")
}

/// A parsed RRULE with fidelity information.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedRule {
    pub rule: RecurrenceRule,
    /// The RRULE text cannot be represented exactly by [`RecurrenceRule`]
    /// (for example `BYDAY=1MO,3WE` mixes per-weekday ordinals).  Callers
    /// must keep the original text around instead of overwriting it.
    pub lossy: bool,
}

/// Parse an RRULE property value (a leading `RRULE:` is tolerated).
///
/// Unknown rule parts are ignored so that vendor extensions do not drop the
/// whole rule. Returns `None` when the value is empty or has no `FREQ`.
pub fn from_rrule_string(value: &str) -> Option<RecurrenceRule> {
    from_rrule_string_detailed(value).map(|parsed| parsed.rule)
}

/// Like [`from_rrule_string`], but reports whether the structured rule is a
/// lossy approximation of the input.
pub fn from_rrule_string_detailed(value: &str) -> Option<ParsedRule> {
    from_rrule_string_with_zone_detailed(value, None)
}

/// Like [`from_rrule_string`], but reinterprets a DATE-TIME `UNTIL` in the
/// event's zone so the stored date is the local date the user meant.
pub(crate) fn from_rrule_string_with_zone(
    value: &str,
    zone: Option<TzSpec>,
) -> Option<RecurrenceRule> {
    from_rrule_string_with_zone_detailed(value, zone).map(|parsed| parsed.rule)
}

pub(crate) fn from_rrule_string_with_zone_detailed(
    value: &str,
    zone: Option<TzSpec>,
) -> Option<ParsedRule> {
    let raw = value
        .trim()
        .strip_prefix("RRULE:")
        .or_else(|| value.trim().strip_prefix("rrule:"))
        .unwrap_or_else(|| value.trim());
    if raw.is_empty() {
        return None;
    }

    let mut rule = RecurrenceRule {
        freq: RecurrenceFreq::Weekly,
        interval: 1,
        by_weekday: Vec::new(),
        by_set_pos: Vec::new(),
        by_month_day: Vec::new(),
        by_month: Vec::new(),
        count: None,
        until: None,
    };
    // BYDAY ordinals and BYSETPOS are collected independently, then reconciled
    // once the whole rule is known (BYDAY may appear before or after BYSETPOS).
    let mut byday_ordinals: Vec<i32> = Vec::new();
    let mut byday_has_plain_entry = false;
    let mut explicit_set_pos: Vec<i32> = Vec::new();
    let mut lossy = false;

    for part in raw.split(';') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_uppercase();
        let value = value.trim();
        match key.as_str() {
            "FREQ" => {
                rule.freq = match value.to_ascii_uppercase().as_str() {
                    "DAILY" => RecurrenceFreq::Daily,
                    "MONTHLY" => RecurrenceFreq::Monthly,
                    "YEARLY" => RecurrenceFreq::Yearly,
                    _ => RecurrenceFreq::Weekly,
                };
            }
            "INTERVAL" => {
                if let Ok(interval) = value.parse::<u32>() {
                    rule.interval = interval.max(1);
                }
            }
            "COUNT" => {
                if let Ok(count) = value.parse::<u32>() {
                    rule.count = Some(count);
                }
            }
            "UNTIL" => {
                if let Some(parsed) = parse_date_value(value) {
                    let date = if parsed.time.is_some() {
                        match zone {
                            Some(spec) => {
                                let instant = parsed.resolve(TzSpec::utc()).with_timezone(&Utc);
                                spec.local_time(instant).date_naive()
                            }
                            None => parsed.date,
                        }
                    } else {
                        parsed.date
                    };
                    rule.until = Some(format_date(date));
                }
            }
            "BYDAY" => {
                for item in value.split(',') {
                    let item = item.trim();
                    if item.is_empty() {
                        continue;
                    }
                    let (ordinal_text, day_text) = if item.len() > 2 {
                        item.split_at(item.len() - 2)
                    } else {
                        ("", item)
                    };
                    if let Ok(day) = day_text.parse::<Weekday>() {
                        if !rule.by_weekday.contains(&day) {
                            rule.by_weekday.push(day);
                        }
                    }
                    match ordinal_text.parse::<i32>() {
                        Ok(ordinal) if ordinal != 0 => {
                            if !byday_ordinals.contains(&ordinal) {
                                byday_ordinals.push(ordinal);
                            }
                        }
                        _ => byday_has_plain_entry = true,
                    }
                }
            }
            "BYSETPOS" => {
                for item in value.split(',') {
                    if let Ok(position) = item.trim().parse::<i32>() {
                        if position != 0 && !explicit_set_pos.contains(&position) {
                            explicit_set_pos.push(position);
                        }
                    }
                }
            }
            "BYMONTHDAY" => {
                for item in value.split(',') {
                    if let Ok(day) = item.trim().parse::<i32>() {
                        if (1..=31).contains(&day.abs()) && !rule.by_month_day.contains(&day) {
                            rule.by_month_day.push(day);
                        }
                    }
                }
            }
            "BYMONTH" => {
                for item in value.split(',') {
                    if let Ok(month) = item.trim().parse::<i32>() {
                        if (1..=12).contains(&month) && !rule.by_month.contains(&month) {
                            rule.by_month.push(month);
                        }
                    }
                }
            }
            // WKST, BYHOUR, BYMINUTE, RSCALE, SKIP … are intentionally ignored.
            _ => {}
        }
    }

    if byday_ordinals.is_empty() {
        rule.by_set_pos = explicit_set_pos;
    } else if !byday_has_plain_entry
        && byday_ordinals.len() == 1
        && (explicit_set_pos.is_empty() || explicit_set_pos == byday_ordinals)
    {
        // A single ordinal shared by every BYDAY entry is exactly
        // `byWeekday` + `bySetPos=[N]`.
        rule.by_set_pos = byday_ordinals;
    } else {
        // Mixed ordinals (`1MO,3WE`) cannot be represented by the structured
        // rule.  Keep a documented best-effort approximation and tell the
        // caller to preserve the original RRULE text.
        let mut positions = byday_ordinals;
        for position in explicit_set_pos {
            if !positions.contains(&position) {
                positions.push(position);
            }
        }
        rule.by_set_pos = positions;
        lossy = true;
    }

    Some(ParsedRule { rule, lossy })
}

// ──────────────────────────── Expansion ────────────────────────────────

/// Expand a recurring event into occurrences overlapping the requested range.
///
/// `overrides` are detached instances (`RECURRENCE-ID` set); moved and
/// cancelled instances are honoured. EXDATEs are not carried by
/// [`CalendarEvent`]; use [`expand_stored`] or [`expand_with_exdates`] when
/// they are available.
pub fn expand(
    event: &CalendarEvent,
    overrides: &[CalendarEvent],
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    max: usize,
) -> Vec<Occurrence> {
    expand_internal(
        event,
        &[],
        &[],
        None,
        overrides,
        range_start,
        range_end,
        max,
    )
}

/// Expand an event with its EXDATE list.
pub fn expand_with_exdates(
    event: &CalendarEvent,
    exdates: &[String],
    overrides: &[CalendarEvent],
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    max: usize,
) -> Vec<Occurrence> {
    expand_internal(
        event,
        exdates,
        &[],
        None,
        overrides,
        range_start,
        range_end,
        max,
    )
}

/// Expand an event with both EXDATE and RDATE lists.
///
/// RDATEs are best effort: each value is added to the rule set and deduplicated
/// with the generated occurrences.
pub fn expand_with_rdates(
    event: &CalendarEvent,
    exdates: &[String],
    rdates: &[String],
    overrides: &[CalendarEvent],
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    max: usize,
) -> Vec<Occurrence> {
    expand_internal(
        event,
        exdates,
        rdates,
        None,
        overrides,
        range_start,
        range_end,
        max,
    )
}

/// Expand a stored event, reading EXDATE and RDATE metadata.
///
/// RDATEs are recovered from `raw_ics` when available (the database keeps the
/// raw component so that nothing is lost).
pub fn expand_stored(
    stored: &StoredEvent,
    overrides: &[CalendarEvent],
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    max: usize,
) -> Vec<Occurrence> {
    let mut rdates = Vec::new();
    let mut exdates = stored.exdates.clone();
    if let Some(raw) = &stored.raw_ics {
        if let Ok(calendar) = parse_calendar(raw) {
            let master = calendar
                .events
                .iter()
                .find(|event| event.uid == stored.event.uid && event.recurrence_id.is_none())
                .cloned();
            let master = master.or_else(|| {
                calendar
                    .events
                    .into_iter()
                    .find(|event| event.uid == stored.event.uid)
            });
            if let Some(parsed) = master {
                rdates = parsed.rdates;
                if exdates.is_empty() {
                    exdates = parsed.exdates;
                }
            }
        }
    }
    expand_internal(
        &stored.event,
        &exdates,
        &rdates,
        stored.rrule.as_deref(),
        overrides,
        range_start,
        range_end,
        max,
    )
}

struct StartInfo {
    spec: TzSpec,
    all_day: bool,
    start_utc: DateTime<Utc>,
    duration: Duration,
    start_date: NaiveDate,
    duration_days: i64,
}

fn start_info(event: &CalendarEvent) -> Option<StartInfo> {
    if event.all_day {
        let start_date = parse_date_value(&event.start)?.date;
        let end_date = parse_date_value(&event.end)
            .map(|parsed| parsed.date)
            .unwrap_or(start_date);
        let duration_days = (end_date - start_date).num_days().max(1);
        let start_utc = Utc.from_utc_datetime(&start_date.and_hms_opt(0, 0, 0)?);
        Some(StartInfo {
            spec: TzSpec::utc(),
            all_day: true,
            start_utc,
            duration: Duration::days(duration_days),
            start_date,
            duration_days,
        })
    } else {
        let start = parse_instant(&event.start)?;
        let end = parse_instant(&event.end).unwrap_or(start);
        let duration = if end > start {
            end - start
        } else {
            Duration::zero()
        };
        let spec = crate::tz::resolve_iana(&event.time_zone)
            .map(TzSpec::Named)
            .unwrap_or_else(TzSpec::utc);
        Some(StartInfo {
            spec,
            all_day: false,
            start_utc: start,
            duration,
            start_date: spec.local_time(start).date_naive(),
            duration_days: 0,
        })
    }
}

fn expand_internal(
    event: &CalendarEvent,
    exdates: &[String],
    rdates: &[String],
    original_rrule: Option<&str>,
    overrides: &[CalendarEvent],
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    max: usize,
) -> Vec<Occurrence> {
    if max == 0 || range_end <= range_start || event.status == EventStatus::Cancelled {
        return Vec::new();
    }
    let Some(info) = start_info(event) else {
        tracing::debug!(event_id = %event.id, "cannot expand event without a parsable start");
        return Vec::new();
    };

    let canonical = |value: &str| canonical_key(value, event);

    // Detached instances keyed by their normalised RECURRENCE-ID.
    let mut override_map: HashMap<String, &CalendarEvent> = HashMap::new();
    for over in overrides {
        if let Some(recurrence_id) = &over.recurrence_id {
            if let Some(key) = canonical(recurrence_id) {
                override_map.insert(key, over);
            }
        }
    }

    let exdate_keys: HashSet<String> = exdates
        .iter()
        .filter_map(|value| canonical(value))
        .collect();
    let exdate_dates: HashSet<NaiveDate> = exdates
        .iter()
        .filter_map(|value| parse_date_value(value))
        .filter(|parsed| parsed.time.is_none())
        .map(|parsed| parsed.date)
        .collect();

    let Some(rule) = &event.recurrence else {
        if event.all_day {
            let start = format_date(info.start_date);
            let end = format_date(info.start_date + Duration::days(info.duration_days));
            return if overlaps_all_day(info.start_date, info.duration_days, range_start, range_end)
            {
                vec![Occurrence {
                    start,
                    end,
                    original_start: format_date(info.start_date),
                }]
            } else {
                Vec::new()
            };
        }
        let end = info.start_utc + info.duration;
        return if overlaps(info.start_utc, end, range_start, range_end) {
            vec![Occurrence {
                start: format_in_spec(info.start_utc, info.spec),
                end: format_in_spec(end, info.spec),
                original_start: format_in_spec(info.start_utc, info.spec),
            }]
        } else {
            Vec::new()
        };
    };

    let (dtstart, rule_tz) = match info.spec {
        TzSpec::Named(tz) if !info.all_day => (
            info.start_utc.with_timezone(&RuleTz::Tz(tz)),
            RuleTz::Tz(tz),
        ),
        _ => (info.start_utc.with_timezone(&RuleTz::UTC), RuleTz::UTC),
    };

    // Prefer the original RRULE text: the structured `RecurrenceRule` is a
    // lossy approximation for rules such as `BYDAY=1MO,3WE`.
    let built = original_rrule
        .and_then(|text| build_rule_from_text(text, dtstart))
        .or_else(|| build_rule(rule, dtstart, &info));
    let Some(built) = built else {
        tracing::debug!(event_id = %event.id, "invalid RRULE; returning the master occurrence only");
        return Vec::new();
    };
    let mut set = built;
    for rdate in rdates {
        if let Some(dt) = rdate_in_tz(rdate, &info, rule_tz) {
            set = set.rdate(dt);
        }
    }

    // Ask for candidates slightly before the range so long events that start
    // before `range_start` are still considered.
    let after = if info.all_day {
        (range_start - Duration::days(info.duration_days + 1)).with_timezone(&rule_tz)
    } else {
        (range_start - info.duration - Duration::minutes(1)).with_timezone(&rule_tz)
    };
    let before = if info.all_day {
        (range_end + Duration::days(1)).with_timezone(&rule_tz)
    } else {
        (range_end + Duration::minutes(1)).with_timezone(&rule_tz)
    };
    let limit = max
        .saturating_add(overrides.len())
        .saturating_add(8)
        .clamp(1, u16::MAX as usize) as u16;
    let generated = set.after(after).before(before).all(limit);

    let mut out: Vec<Occurrence> = Vec::new();
    let mut used_overrides: HashSet<String> = HashSet::new();

    for date in generated.dates {
        let (start, end, start_utc, end_utc, key) = occurrence_from_rule(date, &info);
        if !overlaps(start_utc, end_utc, range_start, range_end) {
            continue;
        }
        let local_date = if info.all_day {
            start_utc.date_naive()
        } else {
            info.spec.local_time(start_utc).date_naive()
        };
        let excluded = exdate_keys.contains(&key) || exdate_dates.contains(&local_date);
        if let Some(over) = override_map.get(&key).copied() {
            used_overrides.insert(key.clone());
            if excluded || over.status == EventStatus::Cancelled {
                continue;
            }
            if let Some(occurrence) = override_occurrence(over, &key, range_start, range_end) {
                out.push(occurrence);
            }
            continue;
        }
        if excluded {
            continue;
        }
        out.push(Occurrence {
            start,
            end,
            original_start: key,
        });
    }

    // Detached occurrences moved in from outside the generated window.
    for over in overrides {
        let Some(recurrence_id) = &over.recurrence_id else {
            continue;
        };
        if over.status == EventStatus::Cancelled {
            continue;
        }
        let key = canonical(recurrence_id).unwrap_or_else(|| recurrence_id.clone());
        if used_overrides.contains(&key) {
            continue;
        }
        if let Some(occurrence) = override_occurrence(over, &key, range_start, range_end) {
            out.push(occurrence);
        }
    }

    out.sort_by(|left, right| {
        sort_key(&left.start)
            .cmp(&sort_key(&right.start))
            .then_with(|| left.start.cmp(&right.start))
    });
    out.dedup_by(|left, right| {
        left.start == right.start
            && left.end == right.end
            && left.original_start == right.original_start
    });
    out.truncate(max);
    out
}

/// Build an [`RRuleSet`] from the original iCalendar RRULE text.
///
/// The `rrule` crate models per-weekday ordinals natively, so unlike the
/// structured [`RecurrenceRule`] this preserves `BYDAY=1MO,3WE`.
fn build_rule_from_text(value: &str, dtstart: DateTime<RuleTz>) -> Option<RRuleSet> {
    let raw = value
        .trim()
        .strip_prefix("RRULE:")
        .or_else(|| value.trim().strip_prefix("rrule:"))
        .unwrap_or_else(|| value.trim());
    if raw.is_empty() {
        return None;
    }
    match raw.parse::<RRule<Unvalidated>>() {
        Ok(unvalidated) => match unvalidated.validate(dtstart) {
            Ok(valid) => Some(RRuleSet::new(dtstart).rrule(valid)),
            Err(error) => {
                tracing::debug!(%error, "original RRULE failed validation; using structured rule");
                None
            }
        },
        Err(error) => {
            tracing::debug!(%error, "cannot parse the original RRULE text; using structured rule");
            None
        }
    }
}

fn build_rule(
    rule: &RecurrenceRule,
    dtstart: DateTime<RuleTz>,
    info: &StartInfo,
) -> Option<RRuleSet> {
    let frequency = match rule.freq {
        RecurrenceFreq::Daily => Frequency::Daily,
        RecurrenceFreq::Weekly => Frequency::Weekly,
        RecurrenceFreq::Monthly => Frequency::Monthly,
        RecurrenceFreq::Yearly => Frequency::Yearly,
    };
    let mut rrule: RRule<Unvalidated> = RRule::new(frequency);
    rrule = rrule.interval(rule.interval.clamp(1, u16::MAX as u32) as u16);

    if let Some(count) = rule.count {
        if count > 0 {
            rrule = rrule.count(count.min(100_000));
        }
    } else if let Some(until) = &rule.until {
        if let Some(date) = parse_date_value(until).map(|parsed| parsed.date) {
            if let Some(until_dt) = until_utc(date, info, dtstart.timezone()) {
                rrule = rrule.until(until_dt);
            }
        }
    }

    if !rule.by_weekday.is_empty() {
        let weekdays = rule
            .by_weekday
            .iter()
            .map(|day| NWeekday::Every(chrono_weekday(*day)))
            .collect();
        rrule = rrule.by_weekday(weekdays);
    }
    if !rule.by_month_day.is_empty() {
        let days: Vec<i8> = rule
            .by_month_day
            .iter()
            .filter(|day| (1..=31).contains(&day.abs()))
            .map(|day| *day as i8)
            .collect();
        if !days.is_empty() {
            rrule = rrule.by_month_day(days);
        }
    }
    if !rule.by_set_pos.is_empty() {
        rrule = rrule.by_set_pos(rule.by_set_pos.clone());
    }
    if !rule.by_month.is_empty() {
        let months: Vec<chrono::Month> = rule
            .by_month
            .iter()
            .filter_map(|month| u8::try_from(*month).ok())
            .filter_map(|month| chrono::Month::try_from(month).ok())
            .collect();
        if !months.is_empty() {
            rrule = rrule.by_month(&months);
        }
    }

    match rrule.validate(dtstart) {
        Ok(valid) => Some(RRuleSet::new(dtstart).rrule(valid)),
        Err(error) => {
            tracing::debug!(%error, "rrule validation failed");
            None
        }
    }
}

fn until_utc(date: NaiveDate, info: &StartInfo, rule_tz: RuleTz) -> Option<DateTime<RuleTz>> {
    let naive = date.and_hms_opt(23, 59, 59)?;
    match rule_tz {
        RuleTz::Tz(tz) if !info.all_day => tz
            .from_local_datetime(&naive)
            .earliest()
            .map(|dt| dt.with_timezone(&RuleTz::UTC)),
        _ => Some(Utc.from_utc_datetime(&naive).with_timezone(&RuleTz::UTC)),
    }
}

fn rdate_in_tz(value: &str, info: &StartInfo, rule_tz: RuleTz) -> Option<DateTime<RuleTz>> {
    if info.all_day {
        let date = parse_date_value(value)?.date;
        let naive = date.and_hms_opt(0, 0, 0)?;
        return Some(Utc.from_utc_datetime(&naive).with_timezone(&RuleTz::UTC));
    }
    let instant = parse_instant(value)?;
    Some(match rule_tz {
        RuleTz::Tz(tz) => instant.with_timezone(&RuleTz::Tz(tz)),
        _ => instant.with_timezone(&RuleTz::UTC),
    })
}

fn occurrence_from_rule(
    date: DateTime<RuleTz>,
    info: &StartInfo,
) -> (String, String, DateTime<Utc>, DateTime<Utc>, String) {
    if info.all_day {
        let date = date.date_naive();
        let start = format_date(date);
        let end = format_date(date + Duration::days(info.duration_days));
        let start_utc = Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap_or_default());
        let end_utc = start_utc + Duration::days(info.duration_days);
        let key = start.clone();
        (start, end, start_utc, end_utc, key)
    } else {
        let start_utc = date.with_timezone(&Utc);
        let end_utc = start_utc + info.duration;
        let start = format_in_spec(start_utc, info.spec);
        let end = format_in_spec(end_utc, info.spec);
        (start.clone(), end, start_utc, end_utc, start)
    }
}

fn override_occurrence(
    over: &CalendarEvent,
    key: &str,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
) -> Option<Occurrence> {
    if over.status == EventStatus::Cancelled {
        return None;
    }
    if over.all_day {
        let start_date = parse_date_value(&over.start)?.date;
        let end_date = parse_date_value(&over.end)
            .map(|parsed| parsed.date)
            .unwrap_or(start_date);
        let days = (end_date - start_date).num_days().max(0);
        if !overlaps_all_day(start_date, days, range_start, range_end) {
            return None;
        }
    } else {
        let start = parse_instant(&over.start)?;
        let end = parse_instant(&over.end).unwrap_or(start);
        if !overlaps(start, end, range_start, range_end) {
            return None;
        }
    }
    Some(Occurrence {
        start: over.start.clone(),
        end: over.end.clone(),
        original_start: key.to_string(),
    })
}

fn canonical_key(value: &str, event: &CalendarEvent) -> Option<String> {
    let parsed = parse_date_value(value)?;
    if event.all_day {
        return Some(format_date(parsed.date));
    }
    let spec = crate::tz::resolve_iana(&event.time_zone)
        .map(TzSpec::Named)
        .unwrap_or_else(TzSpec::utc);
    let instant = parsed.resolve(spec).with_timezone(&Utc);
    Some(format_in_spec(instant, spec))
}

fn format_in_spec(instant: DateTime<Utc>, spec: TzSpec) -> String {
    format_local(spec.local_time(instant))
}

fn sort_key(value: &str) -> DateTime<Utc> {
    parse_instant(value).unwrap_or(DateTime::<Utc>::MIN_UTC)
}

fn overlaps(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
) -> bool {
    start < range_end && end.max(start) > range_start
}

fn overlaps_all_day(
    start_date: NaiveDate,
    duration_days: i64,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
) -> bool {
    let start = Utc.from_utc_datetime(&start_date.and_hms_opt(0, 0, 0).unwrap_or_default());
    let end = start + Duration::days(duration_days.max(1));
    start < range_end && end > range_start
}

fn chrono_weekday(day: Weekday) -> ChronoWeekday {
    match day {
        Weekday::Mo => ChronoWeekday::Mon,
        Weekday::Tu => ChronoWeekday::Tue,
        Weekday::We => ChronoWeekday::Wed,
        Weekday::Th => ChronoWeekday::Thu,
        Weekday::Fr => ChronoWeekday::Fri,
        Weekday::Sa => ChronoWeekday::Sat,
        Weekday::Su => ChronoWeekday::Sun,
    }
}

// ──────────────────────── Series editing helpers ────────────────────────

/// Split a recurring event at `instance_start` for a "this and following"
/// edit.
///
/// The master keeps its UID and gets an `UNTIL` on the day before the split;
/// the returned `following` series gets a new id and a derived UID (and keeps
/// the original rule minus the truncation).
pub fn split_series_at(event: &CalendarEvent, instance_start: &str) -> Result<SeriesSplit> {
    let parsed = parse_date_value(instance_start).ok_or_else(|| {
        AppError::invalid_input(format!("invalid instance start: {instance_start}"))
    })?;
    let rule = event
        .recurrence
        .clone()
        .ok_or_else(|| AppError::invalid_input("event is not recurring"))?;

    let mut master = event.clone();
    let mut master_rule = rule.clone();
    let previous = parsed.date.pred_opt().unwrap_or(parsed.date);
    master_rule.until = Some(format_date(previous));
    master_rule.count = None;
    master.recurrence = Some(master_rule);

    let mut following = event.clone();
    following.id = new_id();
    following.uid = format!("{}.{}", event.uid, instance_start.trim());
    following.series_id = None;
    following.recurrence_id = None;
    following.start = instance_start.to_string();
    following.end = shifted_end(event, &parsed).unwrap_or_else(|| event.end.clone());
    following.recurrence = Some(rule);
    following.status = EventStatus::Confirmed;

    Ok(SeriesSplit { master, following })
}

fn shifted_end(event: &CalendarEvent, instance: &crate::value::ParsedDate) -> Option<String> {
    if event.all_day {
        let start = parse_date_value(&event.start)?.date;
        let end = parse_date_value(&event.end)?.date;
        let days = (end - start).num_days().max(1);
        return Some(format_date(instance.date + Duration::days(days)));
    }
    let start = parse_instant(&event.start)?;
    let end = parse_instant(&event.end)?;
    let duration = if end > start {
        end - start
    } else {
        Duration::zero()
    };
    let instant = instance.resolve(
        crate::tz::resolve_iana(&event.time_zone)
            .map(TzSpec::Named)
            .unwrap_or_else(TzSpec::utc),
    );
    Some(format_in_spec(
        instant.with_timezone(&Utc) + duration,
        crate::tz::resolve_iana(&event.time_zone)
            .map(TzSpec::Named)
            .unwrap_or_else(TzSpec::utc),
    ))
}

/// Add an EXDATE (deduplicating); returns `true` when it was new.
pub fn add_exdate(exdates: &mut Vec<String>, instance_start: &str) -> bool {
    let value = instance_start.trim().to_string();
    if value.is_empty() || exdates.iter().any(|existing| existing == &value) {
        return false;
    }
    exdates.push(value);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn rule_weekly() -> RecurrenceRule {
        RecurrenceRule {
            freq: RecurrenceFreq::Weekly,
            interval: 1,
            by_weekday: vec![Weekday::Mo, Weekday::We],
            by_set_pos: vec![],
            by_month_day: vec![],
            by_month: vec![],
            count: Some(4),
            until: None,
        }
    }

    #[test]
    fn rrule_round_trip() {
        let rule = rule_weekly();
        let text = to_rrule_string(&rule);
        assert!(text.contains("FREQ=WEEKLY"));
        assert!(text.contains("BYDAY=MO,WE"));
        assert!(text.contains("COUNT=4"));
        let parsed = from_rrule_string(&text).unwrap();
        assert_eq!(parsed.freq, RecurrenceFreq::Weekly);
        assert_eq!(parsed.by_weekday, vec![Weekday::Mo, Weekday::We]);
        assert_eq!(parsed.count, Some(4));
    }

    #[test]
    fn parses_ordinal_byday() {
        let rule = from_rrule_string("FREQ=MONTHLY;BYDAY=2TU").unwrap();
        assert_eq!(rule.by_weekday, vec![Weekday::Tu]);
        assert_eq!(rule.by_set_pos, vec![2]);
        let detailed = from_rrule_string_detailed("FREQ=MONTHLY;BYDAY=2TU").unwrap();
        assert!(!detailed.lossy);
    }

    #[test]
    fn single_ordinal_set_maps_to_one_set_pos() {
        // 1MO,1WE means the first Monday/ Wednesday: expressible as
        // byWeekday + bySetPos=[1].
        let detailed = from_rrule_string_detailed("FREQ=MONTHLY;BYDAY=1MO,1WE").expect("rule");
        assert!(!detailed.lossy);
        assert_eq!(detailed.rule.by_weekday, vec![Weekday::Mo, Weekday::We]);
        assert_eq!(detailed.rule.by_set_pos, vec![1]);
    }

    #[test]
    fn multi_ordinal_byday_is_flagged_lossy_and_not_rewritten() {
        let text = "FREQ=MONTHLY;BYDAY=1MO,3WE";
        let detailed = from_rrule_string_detailed(text).expect("rule");
        assert!(
            detailed.lossy,
            "mixed ordinals cannot be represented exactly"
        );
        assert_eq!(detailed.rule.by_weekday, vec![Weekday::Mo, Weekday::We]);
        assert_eq!(detailed.rule.by_set_pos, vec![1, 3]);

        // Expansion from the original RRULE text keeps the exact semantics:
        // the first Monday and the third Wednesday of each month.
        let mut event = CalendarEvent::new(
            "e1",
            "c1",
            "a1",
            "uid-multi",
            "Mixed",
            "2026-01-05T10:00:00Z",
            "2026-01-05T11:00:00Z",
        );
        event.time_zone = "UTC".into();
        event.recurrence = Some(detailed.rule.clone());
        let mut stored = StoredEvent::new(event);
        stored.rrule = Some(text.to_string());
        let from = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let to = Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap();
        let occurrences = expand_stored(&stored, &[], from, to, 20);
        let starts: Vec<&str> = occurrences.iter().map(|o| o.start.as_str()).collect();
        // 2026-01-05 is the first Monday; 2026-01-21 is the third Wednesday.
        assert!(starts.contains(&"2026-01-05T10:00:00+00:00"));
        assert!(starts.contains(&"2026-01-21T10:00:00+00:00"));
        // The lossy structured form would also emit the first Wednesday and
        // the third Monday, which the original text must not.
        assert!(!starts.contains(&"2026-01-07T10:00:00+00:00"));
        assert!(!starts.contains(&"2026-01-19T10:00:00+00:00"));
    }

    #[test]
    fn parses_bysetpos_form() {
        let rule = from_rrule_string("FREQ=MONTHLY;BYDAY=TU;BYSETPOS=2;BYMONTHDAY=-1").unwrap();
        assert_eq!(rule.by_weekday, vec![Weekday::Tu]);
        assert_eq!(rule.by_set_pos, vec![2]);
        assert_eq!(rule.by_month_day, vec![-1]);
        let text = to_rrule_string(&rule);
        assert!(text.contains("BYDAY=TU"));
        assert!(text.contains("BYSETPOS=2"));
        assert!(text.contains("BYMONTHDAY=-1"));
    }

    #[test]
    fn expands_four_weekly_occurrences() {
        let mut event = CalendarEvent::new(
            "e1",
            "c1",
            "a1",
            "uid-1",
            "Standup",
            "2026-11-02T09:00:00-05:00",
            "2026-11-02T09:30:00-05:00",
        );
        event.time_zone = "America/New_York".into();
        event.recurrence = Some(RecurrenceRule {
            freq: RecurrenceFreq::Weekly,
            interval: 1,
            by_weekday: vec![Weekday::Mo, Weekday::We],
            by_set_pos: vec![],
            by_month_day: vec![],
            by_month: vec![],
            count: Some(4),
            until: None,
        });
        let start = Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap();
        let occurrences = expand(&event, &[], start, end, 100);
        assert_eq!(occurrences.len(), 4);
        assert_eq!(occurrences[0].start, "2026-11-02T09:00:00-05:00");
        assert_eq!(occurrences[1].start, "2026-11-04T09:00:00-05:00");
    }

    #[test]
    fn monthly_second_tuesday() {
        let mut event = CalendarEvent::new(
            "e1",
            "c1",
            "a1",
            "uid-2",
            "Review",
            "2026-01-13T14:00:00+01:00",
            "2026-01-13T15:00:00+01:00",
        );
        event.time_zone = "Europe/Berlin".into();
        event.recurrence = Some(RecurrenceRule {
            freq: RecurrenceFreq::Monthly,
            interval: 1,
            by_weekday: vec![Weekday::Tu],
            by_set_pos: vec![2],
            by_month_day: vec![],
            by_month: vec![],
            count: Some(3),
            until: None,
        });
        let start = Utc.with_ymd_and_hms(2025, 12, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap();
        let occurrences = expand(&event, &[], start, end, 100);
        assert_eq!(occurrences.len(), 3);
        assert!(occurrences[0].start.starts_with("2026-01-13T14:00:00"));
        assert!(occurrences[1].start.starts_with("2026-02-10T14:00:00"));
        assert!(occurrences[2].start.starts_with("2026-03-10T14:00:00"));
    }

    #[test]
    fn yearly_until_date() {
        let mut event = CalendarEvent::new(
            "e1",
            "c1",
            "a1",
            "uid-3",
            "Birthday",
            "2024-05-01T10:00:00+00:00",
            "2024-05-01T11:00:00+00:00",
        );
        event.time_zone = "UTC".into();
        event.recurrence = Some(RecurrenceRule {
            freq: RecurrenceFreq::Yearly,
            interval: 1,
            by_weekday: vec![],
            by_set_pos: vec![],
            by_month_day: vec![],
            by_month: vec![],
            count: None,
            until: Some("2026-05-01".into()),
        });
        let start = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap();
        let occurrences = expand(&event, &[], start, end, 100);
        assert_eq!(occurrences.len(), 3);
    }

    #[test]
    fn exdates_and_overrides() {
        let mut event = CalendarEvent::new(
            "e1",
            "c1",
            "a1",
            "uid-4",
            "Weekly",
            "2026-11-02T09:00:00+01:00",
            "2026-11-02T10:00:00+01:00",
        );
        event.time_zone = "Europe/Berlin".into();
        event.recurrence = Some(RecurrenceRule {
            freq: RecurrenceFreq::Weekly,
            interval: 1,
            by_weekday: vec![Weekday::Mo],
            by_set_pos: vec![],
            by_month_day: vec![],
            by_month: vec![],
            count: None,
            until: None,
        });
        let exdates = vec!["2026-11-09T09:00:00+01:00".to_string()];
        let mut moved = event.clone();
        moved.id = "override".into();
        moved.start = "2026-11-16T11:00:00+01:00".into();
        moved.end = "2026-11-16T12:00:00+01:00".into();
        moved.recurrence_id = Some("2026-11-16T09:00:00+01:00".into());
        let start = Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 11, 30, 0, 0, 0).unwrap();
        let occurrences = expand_with_exdates(&event, &exdates, &[moved], start, end, 100);
        let starts: Vec<&str> = occurrences.iter().map(|o| o.start.as_str()).collect();
        assert!(!starts.contains(&"2026-11-09T09:00:00+01:00"));
        assert!(starts.contains(&"2026-11-16T11:00:00+01:00"));
        let moved_occurrence = occurrences
            .iter()
            .find(|o| o.start == "2026-11-16T11:00:00+01:00")
            .unwrap();
        assert_eq!(moved_occurrence.original_start, "2026-11-16T09:00:00+01:00");
    }

    #[test]
    fn dst_keeps_wall_clock() {
        let mut event = CalendarEvent::new(
            "e1",
            "c1",
            "a1",
            "uid-5",
            "DST",
            "2026-03-01T09:00:00-05:00",
            "2026-03-01T10:00:00-05:00",
        );
        event.time_zone = "America/New_York".into();
        event.recurrence = Some(RecurrenceRule {
            freq: RecurrenceFreq::Weekly,
            interval: 1,
            by_weekday: vec![Weekday::Su],
            by_set_pos: vec![],
            by_month_day: vec![],
            by_month: vec![],
            count: Some(4),
            until: None,
        });
        let start = Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap();
        let occurrences = expand(&event, &[], start, end, 100);
        assert_eq!(occurrences.len(), 4);
        assert_eq!(occurrences[0].start, "2026-03-01T09:00:00-05:00");
        assert_eq!(occurrences[1].start, "2026-03-08T09:00:00-04:00");
        assert_eq!(occurrences[2].start, "2026-03-15T09:00:00-04:00");
        assert_eq!(occurrences[3].start, "2026-03-22T09:00:00-04:00");
    }

    #[test]
    fn all_day_expansion() {
        let mut event = CalendarEvent::new(
            "e1",
            "c1",
            "a1",
            "uid-6",
            "Holiday",
            "2026-11-10",
            "2026-11-12",
        );
        event.all_day = true;
        event.recurrence = Some(RecurrenceRule {
            freq: RecurrenceFreq::Daily,
            interval: 2,
            by_weekday: vec![],
            by_set_pos: vec![],
            by_month_day: vec![],
            by_month: vec![],
            count: Some(3),
            until: None,
        });
        let start = Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap();
        let occurrences = expand(&event, &[], start, end, 100);
        assert_eq!(occurrences.len(), 3);
        assert_eq!(occurrences[0].start, "2026-11-10");
        assert_eq!(occurrences[0].end, "2026-11-12");
        assert_eq!(occurrences[1].start, "2026-11-12");
    }

    #[test]
    fn split_series() {
        let mut event = CalendarEvent::new(
            "e1",
            "c1",
            "a1",
            "uid-7",
            "Meeting",
            "2026-11-02T09:00:00+01:00",
            "2026-11-02T10:00:00+01:00",
        );
        event.time_zone = "Europe/Berlin".into();
        event.recurrence = Some(rule_weekly());
        let split = split_series_at(&event, "2026-11-16T09:00:00+01:00").unwrap();
        assert_eq!(
            split.master.recurrence.as_ref().unwrap().until.as_deref(),
            Some("2026-11-15")
        );
        assert_ne!(split.following.id, split.master.id);
        assert_eq!(split.following.start, "2026-11-16T09:00:00+01:00");
        assert_eq!(split.following.end, "2026-11-16T10:00:00+01:00");
    }

    #[test]
    fn exdate_helper() {
        let mut exdates = vec![];
        assert!(add_exdate(&mut exdates, "2026-01-01"));
        assert!(!add_exdate(&mut exdates, "2026-01-01"));
        assert_eq!(exdates, vec!["2026-01-01"]);
    }
}
