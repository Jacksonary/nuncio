//! `nuncio-ical` — iCalendar (RFC 5545), vCard (RFC 6350) and iTIP (RFC 5546).
//!
//! The crate is deliberately **pure**: every public function is a synchronous
//! transformation of text/values. It owns no I/O and never touches the
//! database. Callers (the PIM service) persist results through `nuncio-core`.
//!
//! Modules:
//! * [`ics`] — VCALENDAR/VEVENT/VTODO parsing and writing.
//! * [`recurrence`] — RRULE string conversion and occurrence expansion.
//! * [`itip`] — iTIP request/reply/cancel payloads and the MIME e-mail that
//!   carries them.
//! * [`vcard`] — vCard 2.1/3.0/4.0 parsing and vCard 3.0 writing.
//! * [`tz`] — Windows time-zone names → IANA and VTIMEZONE offsets.
//!
//! Text values are always unescaped to plain UTF-8; output lines are CRLF
//! folded at 75 octets as required by RFC 5545.

pub mod ics;
pub mod itip;
pub mod recurrence;
pub mod tz;
pub mod vcard;

mod value;

pub use ics::{
    parse_calendar, parse_todos, write_calendar, write_event, write_event_with_exdates,
    write_stored_event, write_task, write_tasks, ParsedCalendar, ParsedEvent, ParsedTodo,
};
pub use recurrence::{
    add_exdate, expand, expand_stored, expand_with_exdates, expand_with_rdates, from_rrule_string,
    from_rrule_string_detailed, split_series_at, to_rrule_string, Occurrence, ParsedRule,
    SeriesSplit,
};
pub use vcard::{parse_vcards, write_vcard, ParsedContact};

/// Crate-wide result alias.
pub type Result<T> = nuncio_core::Result<T>;
