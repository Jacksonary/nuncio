//! IMAP protocol layer.

pub mod conn;
pub mod fetch;

pub use conn::{
    connect, connect_account, connect_account_with_timeouts, connect_with_timeouts, resolve,
    test_config, ConnectTimeouts, ImapConnection, ImapSession, ImapTransport, ServerCapabilities,
    TimeoutStream, COMMAND_TIMEOUT, FETCH_TIMEOUT, IO_READ_TIMEOUT, IO_WRITE_TIMEOUT,
};
pub use fetch::{FlagSnapshot, MessageMeta};
