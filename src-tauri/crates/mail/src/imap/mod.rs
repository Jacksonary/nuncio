//! IMAP protocol layer.

pub mod conn;
pub mod fetch;

pub use conn::{
    connect, connect_account, connect_with_timeouts, resolve, test_config, ConnectTimeouts,
    ImapConnection, ImapTransport, ServerCapabilities, COMMAND_TIMEOUT, FETCH_TIMEOUT,
};
pub use fetch::{FlagSnapshot, MessageMeta};
