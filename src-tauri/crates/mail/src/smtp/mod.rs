//! SMTP transports and the outbox sender.

pub mod outbox;
pub mod send;

pub use outbox::{process_due, process_one};
pub use send::{
    send_account_message, send_account_message_within, send_raw, test_smtp, SEND_DEADLINE,
};
