//! nuncio-core — shared domain layer for Nuncio.
//!
//! Every other crate builds on this one:
//!
//! ```
//! use nuncio_core::{Db, Paths};
//!
//! # fn main() -> nuncio_core::Result<()> {
//! let db = Db::open_in_memory()?;
//! db.migrate_core()?;
//! let paths = Paths::new(std::env::temp_dir().join("nuncio-example"));
//! assert!(paths.db_file().ends_with("nuncio.db"));
//! # Ok(())
//! # }
//! ```
//!
//! See `docs/ARCHITECTURE.md` §4 for the intended public API.

pub mod context;
pub mod db;
pub mod error;
pub mod events;
pub mod models;
pub mod paths;
pub mod providers;
pub mod repo;
pub mod settings;
pub mod sync_status;
pub mod text;
pub mod traits;

pub use context::CoreContext;
pub use db::{Db, CORE_MIGRATIONS, CORE_NAMESPACE};
pub use error::AppError;
pub use paths::Paths;
pub use settings::SettingsService;
pub use sync_status::SyncStatusHub;

/// Result type used throughout the workspace.
pub type Result<T> = std::result::Result<T, AppError>;

/// Common re-exports for domain crates.
pub mod prelude {
    pub use crate::context::CoreContext;
    pub use crate::db::{Db, CORE_MIGRATIONS, CORE_NAMESPACE};
    pub use crate::error::AppError;
    pub use crate::events;
    pub use crate::models::*;
    pub use crate::paths::Paths;
    pub use crate::repo;
    pub use crate::settings::SettingsService;
    pub use crate::sync_status::SyncStatusHub;
    pub use crate::text;
    pub use crate::traits::{
        testing, CredentialProvider, EventSink, MailAuth, MailSender, Notifier, TokenResource,
    };
    pub use crate::Result;
}
