//! Time-zone name resolution.
//!
//! iCalendar files in the wild carry three flavours of TZID:
//! * real IANA names (`Asia/Shanghai`);
//! * Windows names (`China Standard Time`, `Pacific Standard Time`, …);
//! * vendor names backed by a `VTIMEZONE` component (`Customized Time Zone`).
//!
//! [`resolve_iana`] covers the first two. [`VTimezone`] stores the offsets of
//! the third so the parser can still produce correct instants.

use std::collections::HashMap;

use chrono::{FixedOffset, NaiveDate};

use crate::value::parse_offset;

/// Map a Windows time-zone display name to an IANA identifier.
///
/// The table covers every zone used by current Windows releases as well as the
/// historical names Outlook still emits. Lookup is case-insensitive.
pub fn windows_to_iana(name: &str) -> Option<&'static str> {
    let key = name.trim();
    let key = key
        .strip_prefix("(UTC")
        .and_then(|rest| rest.split_once(')').map(|(_, tail)| tail.trim()))
        .unwrap_or(key);
    let upper = key.to_ascii_uppercase();
    let iana = match upper.as_str() {
        "DATELINE STANDARD TIME" => "Etc/GMT+12",
        "UTC-11" => "Etc/GMT+11",
        "ALEUTIAN STANDARD TIME" => "America/Adak",
        "HAWAIIAN STANDARD TIME" => "Pacific/Honolulu",
        "MARQUESAS STANDARD TIME" => "Pacific/Marquesas",
        "ALASKAN STANDARD TIME" => "America/Anchorage",
        "UTC-09" => "Etc/GMT+9",
        "PACIFIC STANDARD TIME (MEXICO)" => "America/Tijuana",
        "UTC-08" => "Etc/GMT+8",
        "PACIFIC STANDARD TIME" => "America/Los_Angeles",
        "US MOUNTAIN STANDARD TIME" => "America/Phoenix",
        "MOUNTAIN STANDARD TIME (MEXICO)" => "America/Chihuahua",
        "MOUNTAIN STANDARD TIME" => "America/Denver",
        "CENTRAL AMERICA STANDARD TIME" => "America/Guatemala",
        "CENTRAL STANDARD TIME" => "America/Chicago",
        "EASTER ISLAND STANDARD TIME" => "Pacific/Easter",
        "CENTRAL STANDARD TIME (MEXICO)" => "America/Mexico_City",
        "CANADA CENTRAL STANDARD TIME" => "America/Regina",
        "SA PACIFIC STANDARD TIME" => "America/Bogota",
        "EASTERN STANDARD TIME (MEXICO)" => "America/Cancun",
        "EASTERN STANDARD TIME" => "America/New_York",
        "HAITI STANDARD TIME" => "America/Port-au-Prince",
        "CUBA STANDARD TIME" => "America/Havana",
        "US EASTERN STANDARD TIME" => "America/Indiana/Indianapolis",
        "TURKS AND CAICOS STANDARD TIME" => "America/Grand_Turk",
        "PARAGUAY STANDARD TIME" => "America/Asuncion",
        "ATLANTIC STANDARD TIME" => "America/Halifax",
        "VENEZUELA STANDARD TIME" => "America/Caracas",
        "CENTRAL BRAZILIAN STANDARD TIME" => "America/Cuiaba",
        "SA WESTERN STANDARD TIME" => "America/La_Paz",
        "PACIFIC SA STANDARD TIME" => "America/Santiago",
        "NEWFOUNDLAND STANDARD TIME" => "America/St_Johns",
        "TOCANTINS STANDARD TIME" => "America/Araguaina",
        "E. SOUTH AMERICA STANDARD TIME" => "America/Sao_Paulo",
        "SA EASTERN STANDARD TIME" => "America/Cayenne",
        "ARGENTINA STANDARD TIME" => "America/Argentina/Buenos_Aires",
        "GREENLAND STANDARD TIME" => "America/Nuuk",
        "MONTEVIDEO STANDARD TIME" => "America/Montevideo",
        "MAGALLANES STANDARD TIME" => "America/Punta_Arenas",
        "SAINT PIERRE STANDARD TIME" => "America/Miquelon",
        "BAHIA STANDARD TIME" => "America/Bahia",
        "UTC-02" => "Etc/GMT+2",
        "MID-ATLANTIC STANDARD TIME" => "Etc/GMT+2",
        "AZORES STANDARD TIME" => "Atlantic/Azores",
        "CAPE VERDE STANDARD TIME" => "Atlantic/Cape_Verde",
        "UTC" | "GMT" | "Z" | "COORDINATED UNIVERSAL TIME" => "UTC",
        "GMT STANDARD TIME" => "Europe/London",
        "GREENWICH STANDARD TIME" => "Atlantic/Reykjavik",
        "SAO TOME STANDARD TIME" => "Africa/Sao_Tome",
        "MOROCCO STANDARD TIME" => "Africa/Casablanca",
        "W. EUROPE STANDARD TIME" => "Europe/Berlin",
        "CENTRAL EUROPE STANDARD TIME" => "Europe/Budapest",
        "ROMANCE STANDARD TIME" => "Europe/Paris",
        "CENTRAL EUROPEAN STANDARD TIME" => "Europe/Warsaw",
        "W. CENTRAL AFRICA STANDARD TIME" => "Africa/Lagos",
        "JORDAN STANDARD TIME" => "Asia/Amman",
        "GTB STANDARD TIME" => "Europe/Bucharest",
        "MIDDLE EAST STANDARD TIME" => "Asia/Beirut",
        "EGYPT STANDARD TIME" => "Africa/Cairo",
        "E. EUROPE STANDARD TIME" => "Europe/Chisinau",
        "SYRIA STANDARD TIME" => "Asia/Damascus",
        "WEST BANK STANDARD TIME" => "Asia/Hebron",
        "SOUTH AFRICA STANDARD TIME" => "Africa/Johannesburg",
        "FLE STANDARD TIME" => "Europe/Kyiv",
        "ISRAEL STANDARD TIME" => "Asia/Jerusalem",
        "SOUTH SUDAN STANDARD TIME" => "Africa/Juba",
        "KALININGRAD STANDARD TIME" => "Europe/Kaliningrad",
        "SUDAN STANDARD TIME" => "Africa/Khartoum",
        "LIBYA STANDARD TIME" => "Africa/Tripoli",
        "NAMIBIA STANDARD TIME" => "Africa/Windhoek",
        "ARABIC STANDARD TIME" => "Asia/Baghdad",
        "TURKEY STANDARD TIME" => "Europe/Istanbul",
        "ARAB STANDARD TIME" => "Asia/Riyadh",
        "BELARUS STANDARD TIME" => "Europe/Minsk",
        "RUSSIAN STANDARD TIME" => "Europe/Moscow",
        "E. AFRICA STANDARD TIME" => "Africa/Nairobi",
        "IRAN STANDARD TIME" => "Asia/Tehran",
        "ARABIAN STANDARD TIME" => "Asia/Dubai",
        "ASTRAKHAN STANDARD TIME" => "Europe/Astrakhan",
        "AZERBAIJAN STANDARD TIME" => "Asia/Baku",
        "RUSSIA TIME ZONE 3" => "Europe/Samara",
        "MAURITIUS STANDARD TIME" => "Indian/Mauritius",
        "SARATOV STANDARD TIME" => "Europe/Saratov",
        "GEORGIAN STANDARD TIME" => "Asia/Tbilisi",
        "VOLGOGRAD STANDARD TIME" => "Europe/Volgograd",
        "CAUCASUS STANDARD TIME" => "Asia/Yerevan",
        "AFGHANISTAN STANDARD TIME" => "Asia/Kabul",
        "WEST ASIA STANDARD TIME" => "Asia/Tashkent",
        "EKATERINBURG STANDARD TIME" => "Asia/Yekaterinburg",
        "PAKISTAN STANDARD TIME" => "Asia/Karachi",
        "QYZYLORDA STANDARD TIME" => "Asia/Qyzylorda",
        "INDIA STANDARD TIME" => "Asia/Kolkata",
        "SRI LANKA STANDARD TIME" => "Asia/Colombo",
        "NEPAL STANDARD TIME" => "Asia/Kathmandu",
        "CENTRAL ASIA STANDARD TIME" => "Asia/Almaty",
        "BANGLADESH STANDARD TIME" => "Asia/Dhaka",
        "OMSK STANDARD TIME" => "Asia/Omsk",
        "MYANMAR STANDARD TIME" => "Asia/Yangon",
        "SE ASIA STANDARD TIME" => "Asia/Bangkok",
        "ALTAI STANDARD TIME" => "Asia/Barnaul",
        "W. MONGOLIA STANDARD TIME" => "Asia/Hovd",
        "NORTH ASIA STANDARD TIME" => "Asia/Krasnoyarsk",
        "N. CENTRAL ASIA STANDARD TIME" => "Asia/Novosibirsk",
        "TOMSK STANDARD TIME" => "Asia/Tomsk",
        "CHINA STANDARD TIME" => "Asia/Shanghai",
        "NORTH ASIA EAST STANDARD TIME" => "Asia/Irkutsk",
        "SINGAPORE STANDARD TIME" => "Asia/Singapore",
        "W. AUSTRALIA STANDARD TIME" => "Australia/Perth",
        "TAIPEI STANDARD TIME" => "Asia/Taipei",
        "ULAANBAATAR STANDARD TIME" => "Asia/Ulaanbaatar",
        "AUS CENTRAL W. STANDARD TIME" => "Australia/Eucla",
        "TRANSBAIKAL STANDARD TIME" => "Asia/Chita",
        "TOKYO STANDARD TIME" => "Asia/Tokyo",
        "NORTH KOREA STANDARD TIME" => "Asia/Pyongyang",
        "KOREA STANDARD TIME" => "Asia/Seoul",
        "YAKUTSK STANDARD TIME" => "Asia/Yakutsk",
        "CEN. AUSTRALIA STANDARD TIME" => "Australia/Adelaide",
        "AUS CENTRAL STANDARD TIME" => "Australia/Darwin",
        "E. AUSTRALIA STANDARD TIME" => "Australia/Brisbane",
        "AUS EASTERN STANDARD TIME" => "Australia/Sydney",
        "WEST PACIFIC STANDARD TIME" => "Pacific/Port_Moresby",
        "TASMANIA STANDARD TIME" => "Australia/Hobart",
        "VLADIVOSTOK STANDARD TIME" => "Asia/Vladivostok",
        "LORD HOWE STANDARD TIME" => "Australia/Lord_Howe",
        "BOUGAINVILLE STANDARD TIME" => "Pacific/Bougainville",
        "RUSSIA TIME ZONE 10" => "Asia/Srednekolymsk",
        "MAGADAN STANDARD TIME" => "Asia/Magadan",
        "NORFOLK STANDARD TIME" => "Pacific/Norfolk",
        "SAKHALIN STANDARD TIME" => "Asia/Sakhalin",
        "CENTRAL PACIFIC STANDARD TIME" => "Pacific/Guadalcanal",
        "RUSSIA TIME ZONE 11" => "Asia/Kamchatka",
        "NEW ZEALAND STANDARD TIME" => "Pacific/Auckland",
        "UTC+12" => "Etc/GMT-12",
        "FIJI STANDARD TIME" => "Pacific/Fiji",
        "CHATHAM ISLANDS STANDARD TIME" => "Pacific/Chatham",
        "UTC+13" => "Etc/GMT-13",
        "TONGA STANDARD TIME" => "Pacific/Tongatapu",
        "SAMOA STANDARD TIME" => "Pacific/Apia",
        "LINE ISLANDS STANDARD TIME" => "Pacific/Kiritimati",
        _ => return None,
    };
    Some(iana)
}

/// Resolve a TZID to an IANA zone.
///
/// Handles Windows names, vendor prefixes such as
/// `/freeassociation.sourceforge.net/Tzfile/America/New_York` and plain IANA
/// names. Returns `None` when the zone is only described by a VTIMEZONE.
pub fn resolve_iana(tzid: &str) -> Option<chrono_tz::Tz> {
    let trimmed = tzid.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(iana) = windows_to_iana(trimmed) {
        if let Ok(tz) = iana.parse::<chrono_tz::Tz>() {
            return Some(tz);
        }
    }
    if let Ok(tz) = trimmed.parse::<chrono_tz::Tz>() {
        return Some(tz);
    }

    // Strip URL-ish prefixes commonly stored in TZID.
    let stripped = trimmed.trim_start_matches('/');
    if let Ok(tz) = stripped.parse::<chrono_tz::Tz>() {
        return Some(tz);
    }
    for prefix in [
        "freeassociation.sourceforge.net/Tzfile/",
        "mozilla.org/20050126_1/",
        "citadel.org/",
    ] {
        if let Some(rest) = stripped.strip_prefix(prefix) {
            if let Ok(tz) = rest.parse::<chrono_tz::Tz>() {
                return Some(tz);
            }
        }
    }
    // Last resort: try every slash-separated suffix.
    let mut start = 0;
    while let Some(idx) = stripped[start..].find('/') {
        let candidate = &stripped[start + idx + 1..];
        if let Ok(tz) = candidate.parse::<chrono_tz::Tz>() {
            return Some(tz);
        }
        start += idx + 1;
    }
    None
}

/// A parsed VTIMEZONE component (best effort).
#[derive(Debug, Clone, Default)]
pub struct VTimezone {
    pub tzid: String,
    pub standard_offset: Option<FixedOffset>,
    pub daylight_offset: Option<FixedOffset>,
    /// Approximate `(month, day)` when DST starts.
    pub daylight_start: Option<(u32, u32)>,
    /// Approximate `(month, day)` when standard time resumes.
    pub standard_start: Option<(u32, u32)>,
}

impl VTimezone {
    /// Offset in effect on `date`.
    pub fn offset_at(&self, date: NaiveDate) -> Option<FixedOffset> {
        match (self.standard_offset, self.daylight_offset) {
            (None, None) => None,
            (Some(std), None) => Some(std),
            (None, Some(dst)) => Some(dst),
            (Some(std), Some(dst)) => match (self.daylight_start, self.standard_start) {
                (Some(start), Some(end)) => {
                    let today = date.month_day();
                    if start < end {
                        if today >= start && today < end {
                            Some(dst)
                        } else {
                            Some(std)
                        }
                    } else if start > end {
                        // Southern hemisphere: DST spans the new year.
                        if today >= start || today < end {
                            Some(dst)
                        } else {
                            Some(std)
                        }
                    } else {
                        Some(std)
                    }
                }
                _ => Some(std),
            },
        }
    }
}

/// Helper trait for month/day comparisons.
trait MonthDay {
    fn month_day(&self) -> (u32, u32);
}

impl MonthDay for NaiveDate {
    fn month_day(&self) -> (u32, u32) {
        use chrono::Datelike;
        (self.month(), self.day())
    }
}

/// Parse the `TZOFFSETTO` value of a STANDARD/DAYLIGHT component.
pub fn parse_offset_to(value: &str) -> Option<FixedOffset> {
    parse_offset(value.trim())
}

/// Parse a TZID-keyed map of VTIMEZONE definitions.
pub type TimezoneMap = HashMap<String, VTimezone>;

/// Look up a VTIMEZONE definition case-insensitively.
pub fn lookup<'a>(map: &'a TimezoneMap, tzid: &str) -> Option<&'a VTimezone> {
    if let Some(found) = map.get(tzid) {
        return Some(found);
    }
    let needle = tzid.trim().to_ascii_lowercase();
    map.iter()
        .find(|(key, _)| key.to_ascii_lowercase() == needle)
        .map(|(_, value)| value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_windows_names() {
        assert_eq!(
            windows_to_iana("China Standard Time"),
            Some("Asia/Shanghai")
        );
        assert_eq!(
            windows_to_iana("Pacific Standard Time"),
            Some("America/Los_Angeles")
        );
        assert_eq!(
            windows_to_iana("W. Europe Standard Time"),
            Some("Europe/Berlin")
        );
        assert_eq!(windows_to_iana("Tokyo Standard Time"), Some("Asia/Tokyo"));
        assert_eq!(windows_to_iana("India Standard Time"), Some("Asia/Kolkata"));
        assert_eq!(windows_to_iana("Nope"), None);
    }

    #[test]
    fn resolves_prefixed_iana() {
        assert!(resolve_iana("/freeassociation.sourceforge.net/Tzfile/Europe/Paris").is_some());
        assert!(resolve_iana("Asia/Shanghai").is_some());
        assert!(resolve_iana("Not/AZone").is_none());
    }

    #[test]
    fn vtimezone_offset_selection() {
        let vtz = VTimezone {
            tzid: "Custom".into(),
            standard_offset: parse_offset("-0800"),
            daylight_offset: parse_offset("-0700"),
            daylight_start: Some((3, 8)),
            standard_start: Some((11, 1)),
        };
        let winter = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        let summer = NaiveDate::from_ymd_opt(2026, 7, 15).unwrap();
        assert_eq!(vtz.offset_at(winter), parse_offset("-0800"));
        assert_eq!(vtz.offset_at(summer), parse_offset("-0700"));
    }
}
