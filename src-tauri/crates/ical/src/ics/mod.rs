//! iCalendar (RFC 5545) parsing and writing.
//!
//! ## Task steps
//!
//! `TaskStep`s are encoded as one `X-NUNCIO-STEP` property per step inside the
//! VTODO, with four `;`-separated fields:
//! `<completed 0|1>;<sortOrder>;<percent-encoded id>;<percent-encoded title>`.
//! This keeps every step in the master VTODO (no extra related components) and
//! survives servers that only understand VTODO text properties. `%`, `;`, CR
//! and LF are percent-encoded inside the two text fields.

pub(crate) mod lines;
mod parse;
mod types;
mod write;

pub use parse::{events_to_models, parse_calendar, parse_first_event, parse_todos};
pub use types::{ParsedCalendar, ParsedEvent, ParsedTodo};
pub use write::{
    write_calendar, write_event, write_event_with_exdates, write_stored_calendar,
    write_stored_event, write_task, write_tasks,
};

pub(crate) use write::write_vevent_ext;
