//! `webcal://` subscription support.
//!
//! A subscription is a **read-only** calendar owned by the built-in `local`
//! account with `calendar.subscription_url` set.  Refreshes replace the
//! events of that calendar (UID is the stable key).

use nuncio_core::models::{new_id, StoredEvent};
use url::Url;

use crate::error::{invalid, network, protocol, Result};
use crate::http::{request_error, HttpClient};
use crate::provider::RemoteEvent;
use crate::util::{find_self_index, is_email};

/// Normalise `webcal://`, `webcals://` and `http(s)://` subscription URLs.
pub fn normalize_url(input: &str) -> Result<String> {
    let input = input.trim();
    if input.is_empty() {
        return Err(invalid("subscription URL must not be empty"));
    }
    let mut url =
        Url::parse(input).map_err(|error| invalid(format!("invalid subscription URL: {error}")))?;
    match url.scheme().to_ascii_lowercase().as_str() {
        "webcal" | "webcals" => {
            let host = url
                .host_str()
                .ok_or_else(|| invalid("subscription URL has no host"))?;
            let port = url
                .port()
                .map(|port| format!(":{port}"))
                .unwrap_or_default();
            let mut rebuilt = format!("https://{host}{port}{}", url.path());
            if let Some(query) = url.query() {
                rebuilt.push('?');
                rebuilt.push_str(query);
            }
            url = Url::parse(&rebuilt)
                .map_err(|error| invalid(format!("cannot convert webcal URL: {error}")))?;
        }
        "http" | "https" => {}
        other => return Err(invalid(format!("unsupported subscription scheme: {other}"))),
    }
    if url.host_str().unwrap_or_default().is_empty() {
        return Err(invalid("subscription URL has no host"));
    }
    Ok(url.to_string())
}

/// Download an ICS subscription.  The response is capped to keep memory usage
/// bounded (calendar feeds are text and rarely exceed a few MB).
pub async fn fetch(http: &HttpClient, url: &str) -> Result<String> {
    let url = normalize_url(url)?;
    let response = http
        .raw()
        .get(&url)
        .send()
        .await
        .map_err(|error| request_error("webcal download", error))?;
    let status = response.status();
    if !status.is_success() {
        return Err(network(format!("webcal returned {status}")));
    }
    // Read the body in chunks and stop at the limit: `bytes()` would buffer the
    // whole response first, so a hostile or broken feed could exhaust memory
    // before the size check ever ran. The cap applies to the decompressed bytes
    // we actually accumulate.
    const MAX_FEED_BYTES: usize = 32 * 1024 * 1024;
    if let Some(length) = response.content_length() {
        if length as usize > MAX_FEED_BYTES {
            return Err(protocol("webcal feed is larger than 32 MB"));
        }
    }
    let mut response = response;
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| request_error("webcal body", error))?
    {
        if bytes.len() + chunk.len() > MAX_FEED_BYTES {
            return Err(protocol("webcal feed is larger than 32 MB"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    if !trimmed.starts_with("BEGIN:VCALENDAR") {
        return Err(protocol("webcal feed is not an iCalendar document"));
    }
    Ok(text)
}

/// Build a tiny iCalendar document that only carries a master's RDATE values.
///
/// Recurrence expansion (`nuncio_ical::recurrence::expand_stored`) recovers
/// RDATEs by re-parsing `StoredEvent::raw_ics`.  Storing the whole feed on
/// every master makes both storage and each expansion O(events × feed size);
/// this helper keeps just the lines expansion needs.  Returns `None` when the
/// event has no RDATE (no raw copy is needed at all).
pub fn rdates_only_ics(uid: &str, all_day: bool, start: &str, rdates: &[String]) -> Option<String> {
    if rdates.is_empty() {
        return None;
    }
    let start_line = if all_day {
        let date = start.get(0..10).unwrap_or(start).replace('-', "");
        format!("DTSTART;VALUE=DATE:{date}")
    } else {
        let instant = crate::util::parse_instant(start).ok()?;
        format!("DTSTART:{}", instant.format("%Y%m%dT%H%M%SZ"))
    };
    let values: Vec<String> = if all_day {
        rdates
            .iter()
            .filter_map(|value| {
                let date = crate::util::date_part(value)?;
                Some(date.replace('-', ""))
            })
            .collect()
    } else {
        rdates
            .iter()
            .filter_map(|value| {
                crate::util::parse_instant(value)
                    .ok()
                    .map(|instant| instant.format("%Y%m%dT%H%M%SZ").to_string())
            })
            .collect()
    };
    if values.is_empty() {
        return None;
    }
    let mut out = String::new();
    out.push_str("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Nuncio//EN\r\n");
    out.push_str("BEGIN:VEVENT\r\n");
    out.push_str(&format!("UID:{}\r\n", uid.replace(['\r', '\n'], "")));
    out.push_str(&start_line);
    out.push_str("\r\n");
    out.push_str(&format!("RDATE:{}\r\n", values.join(",")));
    out.push_str("END:VEVENT\r\nEND:VCALENDAR\r\n");
    Some(out)
}

/// Parse a downloaded feed into remote events ready for the sync engine.
pub fn parse_subscription(
    ics: &str,
    calendar_id: &str,
    account_id: &str,
    account_email: &str,
) -> Result<Vec<RemoteEvent>> {
    let calendar = nuncio_ical::ics::parse_calendar(ics)
        .map_err(|error| protocol(format!("invalid webcal feed: {error}")))?;
    let mut parsed_events = calendar.events;
    // Feed the sync engine each series' master before its exception overrides.
    // A feed normally holds several independent UIDs, so this has to be ordered
    // per series rather than hoisting a single "first" master to the front.
    parsed_events.sort_by_key(|parsed| (parsed.uid.clone(), parsed.recurrence_id.is_some()));
    let mut remote = Vec::new();
    // `is_master` is per UID: a RECURRENCE-ID override belongs to its own
    // series, so a second UID's master is still a master. Tracking one flag for
    // the whole feed dropped every RDATE after the first series.
    let mut masters_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for parsed in parsed_events {
        let recurrence_id = parsed.recurrence_id.clone();
        let is_master = recurrence_id.is_none() && masters_seen.insert(parsed.uid.clone());
        let uid = parsed.uid.clone();
        let rrule = parsed.rrule.clone();
        let exdates = parsed.exdates.clone();
        let rdates = parsed.rdates.clone();
        let all_day = parsed.all_day;
        let start = parsed.start.clone();
        let mut stored = parsed.into_stored(new_id(), calendar_id, account_id);
        stored.remote_id = Some(if is_master {
            uid.clone()
        } else {
            match &recurrence_id {
                Some(value) => format!("{uid}|{value}"),
                None => uid.clone(),
            }
        });
        // Keep only the RDATE values the recurrence expander needs instead of
        // duplicating the whole feed on every master.
        stored.raw_ics = if is_master {
            rdates_only_ics(&uid, all_day, &start, &rdates)
        } else {
            None
        };
        stored.rrule = rrule;
        stored.exdates = exdates;
        stored.etag = None;
        stored.href = None;
        enrich(&mut stored, account_email);
        remote.push(RemoteEvent::from_stored(&stored, None));
    }
    Ok(remote)
}

fn enrich(stored: &mut StoredEvent, account_email: &str) {
    stored.event.is_organizer = stored
        .event
        .organizer
        .as_ref()
        .is_some_and(|organizer| is_email(&organizer.email, account_email));
    if let Some(index) = find_self_index(&stored.event, account_email) {
        stored.event.my_response = Some(stored.event.attendees[index].status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_webcal_scheme() {
        assert_eq!(
            normalize_url("webcal://example.com/cal.ics").expect("url"),
            "https://example.com/cal.ics"
        );
        assert!(normalize_url("ftp://example.com/cal.ics").is_err());
        assert!(normalize_url("not a url").is_err());
    }

    #[test]
    fn parses_subscription_events() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//T//EN\r\nBEGIN:VEVENT\r\nUID:feed-1\r\nSUMMARY:Holiday\r\nDTSTART;VALUE=DATE:20261225\r\nDTEND;VALUE=DATE:20261226\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_subscription(ics, "cal", "local", "me@example.com").expect("parse");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.uid, "feed-1");
        assert!(events[0].event.all_day);
        assert_eq!(events[0].remote_id.as_deref(), Some("feed-1"));
        assert!(events[0].raw_ics.is_none(), "no RDATE means no raw copy");
    }

    #[test]
    fn keeps_only_rdate_values_for_recurring_feeds() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//T//EN\r\nBEGIN:VEVENT\r\nUID:feed-2\r\nSUMMARY:Weekly\r\nDTSTART:20261105T090000Z\r\nDTEND:20261105T100000Z\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\nRDATE:20261201T090000Z,20261208T090000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_subscription(ics, "cal", "local", "me@example.com").expect("parse");
        assert_eq!(events.len(), 1);
        let raw = events[0].raw_ics.as_deref().expect("rdate raw");
        assert!(raw.len() < ics.len());
        assert!(raw.contains("RDATE:"));
        assert!(!raw.contains("SUMMARY:"));
    }
}
