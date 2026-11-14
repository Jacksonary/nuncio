//! `nuncio-pim` — Google Calendar, Microsoft Graph and CalDAV providers,
//! calendar commands, meeting invitations, webcal subscriptions and reminders.
//!
//! # Architecture
//!
//! * [`http`] — one rustls/reqwest client with bearer retry, rate-limit backoff
//!   and error mapping shared by every provider.
//! * [`google`], [`graph`], [`dav`] — provider clients and JSON/iCalendar →
//!   [`nuncio_core::models::CalendarEvent`] mapping.  Each also implements
//!   [`provider::CalendarProvider`].
//! * [`service::PimService`] — the command facade used by the app crate.  Reads
//!   come from SQLite; writes update the database optimistically and enqueue a
//!   `pim.*` pending operation (retry + rollback + `op://failed`).
//! * [`reminders`] — a 30-second loop with an extensible [`ReminderSource`]
//!   trait; stage 2 adds task reminders without touching the engine.
//!
//! Stage 2 (contacts/tasks) reuses [`http::HttpClient`], the provider delta
//! shapes ([`provider::EventDelta`] / [`provider::CalendarCursor`]), the DAV
//! XML parser ([`dav::xml`]) and the pending-operation queue.

pub mod contacts;
pub mod dav;
pub mod error;
pub mod google;
pub mod graph;
pub mod http;
pub mod op_worker;
pub mod ops;
pub mod provider;
pub mod reminders;
pub mod service;
pub mod store;
pub mod tasks;
pub mod util;
pub mod webcal;

pub use contacts::ContactsService;
pub use dav::{DavClient, DavKind, DavProvider};
pub use error::{AppError, Result};
pub use google::{GoogleClient, GoogleProvider};
pub use graph::{GraphClient, GraphProvider};
pub use http::HttpClient;
pub use provider::{
    CalendarCursor, CalendarProvider, EventDelta, EventWindow, RemoteCalendar, RemoteEvent,
};
pub use reminders::{
    CalendarReminderSource, ReminderEngine, ReminderHit, ReminderKind, ReminderSource,
};
pub use service::PimService;
pub use store::{CalendarRemote, PIM_MIGRATIONS, PIM_NAMESPACE};
pub use tasks::TasksService;
