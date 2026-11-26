//! nuncio-mail — IMAP/POP3 synchronization, SMTP submission, the persistent
//! operation queue and the local rule engine.
//!
//! Public entry points:
//! * [`Engine`] — per-account IMAP/SMTP/POP3 actors, synchronization, the
//!   persistent operation queue and the local rule engine.
//! * [`MailService`] — the command-facing API used by the Tauri command layer.
//!   It maps one-to-one to the `folder_*`, `message_*`, `attachment_*`,
//!   `compose_*` and `rule_run_now` commands in `src/ipc/api.ts`.
//!
//! See `docs/ARCHITECTURE.md` §5/§6 and `docs/PLAN.md` §6.1 for the behavior
//! contract.
//!
//! ```no_run
//! # async fn run(ctx: nuncio_core::CoreContext) -> nuncio_core::Result<()> {
//! use nuncio_mail::{Engine, MailService};
//! let engine = Engine::new(ctx.clone());
//! engine.start().await?;
//! engine.sync_now(None, None).await?;
//! engine.shutdown().await;
//!
//! let service = MailService::new(ctx);
//! let folders = service.folder_list("account-id").await?;
//! # let _ = folders;
//! # Ok(())
//! # }
//! ```

pub mod engine;
pub mod graph;
pub mod imap;
pub mod ops;
pub mod pop3;
pub mod rules;
pub mod service;
pub mod smtp;
pub mod sync;
pub mod util;

pub use engine::{poll_interval, set_poll_override_secs, Engine, EngineConfig};
pub use rules::{evaluate as evaluate_rule, matching_rules};
pub use service::MailService;
pub use sync::folders::role_for as folder_role_for;
pub use sync::messages::imap_search_criteria;

/// Convenience re-export of the operation payload kinds for the service layer.
pub mod operation_kinds {
    pub use crate::ops::{
        AppendDraftOp, AppendToSentOp, ArchiveOp, DeleteOp, DeleteRemoteDraftOp, FolderCreateOp,
        FolderDeleteOp, FolderMarkReadOp, FolderRenameOp, FolderSubscribeOp, MarkJunkOp, MoveOp,
        RemoteDraftRef, SendOp, SetFlagsOp,
    };
}

/// Re-export the core context so downstream crates only depend on this crate
/// when wiring the mail engine.
pub use nuncio_core::CoreContext;
