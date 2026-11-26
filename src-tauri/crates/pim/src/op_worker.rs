//! Shared worker for the `pim.*` pending-operation queue.
//!
//! The calendar worker in [`crate::service`] predates this module; contacts and
//! tasks share the logic here: drain due operations for every non-local
//! account, mark them in progress, execute, and either complete them or apply
//! the normal retry / permanent-failure policy (rollback + `op://failed`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use nuncio_core::events::emit_op_failed;
use nuncio_core::models::{PendingOp, PendingOpState, SyncModule};
use nuncio_core::repo::{self, pending_ops};
use nuncio_core::CoreContext;
use tokio::sync::Notify;
use tracing::{debug, warn};

use crate::error::{AppError, Result};

/// Maximum attempts before a retryable failure becomes permanent.
pub const MAX_ATTEMPTS: u32 = 5;

/// One module's operation executor.
#[async_trait]
pub trait PimOpExecutor: Send + Sync {
    /// e.g. `"pim.contact."`; only matching operations are handled.
    fn kind_prefix(&self) -> &'static str;

    fn module(&self) -> SyncModule;

    async fn execute(&self, op: &PendingOp) -> Result<()>;

    /// Restore local state after a permanent remote failure.
    async fn rollback(&self, op: &PendingOp) -> Result<()>;

    /// `true` when [`Self::rollback_in_tx`] is implemented.
    ///
    /// Executors whose rollback needs async work keep the default (`false`) and
    /// the worker settles the operation in two steps. Executors that can roll
    /// back synchronously opt in so `mark_failed` and the restore commit in one
    /// transaction; a delta can then never observe the failure in between.
    fn supports_atomic_rollback(&self) -> bool {
        false
    }

    /// Synchronous rollback to be run in the same transaction as
    /// `pending_ops::mark_failed`.
    fn rollback_in_tx(&self, _conn: &rusqlite::Connection, _op: &PendingOp) -> Result<()> {
        Err(AppError::unsupported(
            "this executor cannot roll back inside a transaction",
        ))
    }

    /// Hook invoked after a successful write (used to refresh sync state).
    fn on_success(&self, _ctx: &CoreContext, _op: &PendingOp) {}

    /// Hook invoked after a permanent failure has been settled and its rollback
    /// committed.
    ///
    /// The rollback itself may run inside the worker's transaction, where an
    /// event must not be emitted before the commit; executors use this to
    /// announce the restored rows to the UI.
    fn on_failure(&self, _ctx: &CoreContext, _op: &PendingOp) {}
}

/// Run the worker until `shutdown` is set.
pub async fn run_worker(
    ctx: CoreContext,
    notify: Arc<Notify>,
    shutdown: Arc<AtomicBool>,
    executor: Arc<dyn PimOpExecutor>,
) {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        if let Err(error) = process_pass(&ctx, executor.clone()).await {
            warn!(module = executor.module().as_str(), %error, "pim op pass failed");
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            _ = notify.notified() => {}
        }
    }
}

/// Recover operations interrupted by a crash for this module.
pub fn recover_interrupted(ctx: &CoreContext, prefix: &str) -> Result<()> {
    let like = format!("{prefix}%");
    ctx.db.write_sync(|conn| {
        conn.execute(
            "UPDATE pending_op SET state = 'pending'
             WHERE kind LIKE ?1 AND state = 'inProgress'",
            rusqlite::params![like],
        )?;
        Ok(())
    })
}

/// One drain pass (at most 20 operations per pass).
pub async fn process_pass(ctx: &CoreContext, executor: Arc<dyn PimOpExecutor>) -> Result<()> {
    let accounts = ctx.db.read(repo::accounts::list).await?;
    let now = Utc::now();
    let mut processed = 0u32;
    for account in accounts {
        if account.provider.is_local() {
            continue;
        }
        let account_id = account.id.clone();
        let ops = ctx
            .db
            .read(move |conn| pending_ops::list(conn, Some(account_id.as_str())))
            .await?;
        for op in ops {
            if processed >= 20 {
                return Ok(());
            }
            // `inProgress` rows are ours to reclaim: the previous pass either
            // crashed or failed to record completion, and the remote write used a
            // stable idempotency key, so re-running it is safe. Without this the
            // row would sit in `inProgress` forever.
            let runnable = matches!(
                op.state,
                PendingOpState::Pending | PendingOpState::InProgress
            );
            if !runnable || !op.kind.starts_with(executor.kind_prefix()) {
                continue;
            }
            if op.next_retry_at.is_some_and(|retry| retry > now) {
                continue;
            }
            processed += 1;
            let op_id = op.id.clone();
            // Claiming the operation must succeed before touching the provider:
            // if the write fails another worker could claim it too, and a remote
            // side effect without a claim cannot be tracked or rolled back.
            // The transition is conditional: an operation cancelled between the
            // snapshot above and this claim (a create-then-delete race) must
            // stay cancelled instead of being resurrected and executed.
            let claimed = match ctx
                .db
                .write({
                    let id = op_id.clone();
                    move |conn| pending_ops::claim_for_execution(conn, &id)
                })
                .await
            {
                Ok(claimed) => claimed,
                Err(error) => {
                    warn!(op = %op.kind, %error, "could not claim a pim operation; skipping this pass");
                    continue;
                }
            };
            if claimed == 0 {
                debug!(op = %op.kind, "skipping a pim operation settled by another actor");
                continue;
            }
            // Re-read the row after claiming: the in-memory snapshot may be
            // stale (the user may have cancelled the operation or enqueued a
            // newer attempt). Only the row as it exists now may be executed.
            let current = match ctx
                .db
                .read({
                    let id = op_id.clone();
                    move |conn| pending_ops::get_required(conn, &id)
                })
                .await
            {
                Ok(current) => current,
                Err(error) => {
                    warn!(op = %op.kind, %error, "could not re-read a claimed pim operation");
                    continue;
                }
            };
            if !matches!(current.state, PendingOpState::InProgress) {
                debug!(op = %op.kind, "pim operation was settled before execution");
                continue;
            }
            match executor.execute(&current).await {
                Ok(()) => {
                    let id = op_id.clone();
                    // The remote write already happened. If recording it fails we
                    // must not pretend success: the row stays in progress and the
                    // next pass retries, which providers treat idempotently via
                    // the stored remote id. A cancelled row is not completed:
                    // the cancellation is the authoritative outcome.
                    match ctx
                        .db
                        .write(move |conn| pending_ops::finish(conn, &id))
                        .await
                    {
                        Ok(0) => {
                            debug!(op = %current.kind, "pim operation was settled during execution");
                            continue;
                        }
                        Ok(_) => executor.on_success(ctx, &current),
                        Err(error) => {
                            warn!(op = %current.kind, %error, "could not record a completed pim operation");
                            continue;
                        }
                    }
                }
                Err(error) => {
                    handle_failure(ctx, &executor, &current, error).await;
                }
            }
        }
    }
    Ok(())
}

async fn handle_failure(
    ctx: &CoreContext,
    executor: &Arc<dyn PimOpExecutor>,
    op: &PendingOp,
    error: AppError,
) {
    let auth = matches!(&error, AppError::Auth(_));
    let retryable = matches!(
        &error,
        AppError::Network(_) | AppError::Service(_) | AppError::Conflict(_) | AppError::Io(_)
    ) && op.attempts < MAX_ATTEMPTS;
    if auth {
        ctx.sync_status
            .needs_reauth(&op.account_id, executor.module());
    }
    if retryable || auth {
        if auth && op.attempts >= MAX_ATTEMPTS {
            fail_op(ctx, executor, op, &error).await;
            return;
        }
        let op_id = op.id.clone();
        let message = error.message();
        let message_for_db = message.clone();
        let now = Utc::now();
        if let Err(error) = ctx
            .db
            .write(move |conn| pending_ops::mark_retry(conn, &op_id, &message_for_db, now))
            .await
        {
            warn!(op = %op.kind, %error, "could not schedule a pim operation retry");
        }
        debug!(
            module = executor.module().as_str(),
            op = %op.kind,
            %message,
            "pim operation will be retried"
        );
        return;
    }
    fail_op(ctx, executor, op, &error).await;
}

async fn fail_op(
    ctx: &CoreContext,
    executor: &Arc<dyn PimOpExecutor>,
    op: &PendingOp,
    error: &AppError,
) {
    let message = error.message();
    if executor.supports_atomic_rollback() {
        // Settle and restore in one transaction: a delta must never observe
        // "operation no longer outstanding, stale row still present", or it
        // could delete the row and the rollback would then re-insert it.
        let op_row = op.clone();
        let exec = executor.clone();
        let message_for_db = message.clone();
        match ctx
            .db
            .write(move |conn| {
                pending_ops::mark_failed(conn, &op_row.id, &message_for_db)?;
                exec.rollback_in_tx(conn, &op_row)
            })
            .await
        {
            // The transaction committed: the restored rows are visible now, so
            // the module can announce them.
            Ok(()) => executor.on_failure(ctx, op),
            Err(settle_error) => warn!(
                module = executor.module().as_str(),
                op = %op.kind,
                %settle_error,
                "could not settle and roll back a failed pim operation"
            ),
        }
    } else {
        let op_id = op.id.clone();
        let message_for_db = message.clone();
        let mut settled = false;
        if let Err(record_error) = ctx
            .db
            .write(move |conn| pending_ops::mark_failed(conn, &op_id, &message_for_db))
            .await
        {
            warn!(op = %op.kind, %record_error, "could not mark a pim operation as failed");
        } else {
            settled = true;
        }
        match executor.rollback(op).await {
            Ok(()) => {
                if settled {
                    executor.on_failure(ctx, op);
                }
            }
            Err(rollback_error) => warn!(
                module = executor.module().as_str(),
                op = %op.kind,
                %rollback_error,
                "could not roll back a failed pim operation"
            ),
        }
    }
    emit_op_failed(
        ctx.events.as_ref(),
        op.account_id.clone(),
        &op.description,
        &error.message(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::models::{new_id, NewPendingOp};
    use nuncio_core::Paths;
    use std::sync::atomic::AtomicUsize;

    struct CountingExecutor {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl PimOpExecutor for CountingExecutor {
        fn kind_prefix(&self) -> &'static str {
            "pim.contact."
        }

        fn module(&self) -> SyncModule {
            SyncModule::Contacts
        }

        async fn execute(&self, _op: &PendingOp) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn rollback(&self, _op: &PendingOp) -> Result<()> {
            Ok(())
        }
    }

    /// Regression 1 (worker side): an operation cancelled between the queue
    /// read and the claim must not be resurrected into `inProgress`, must not
    /// be handed to the executor, and must stay cancelled afterwards.
    #[tokio::test]
    async fn cancelled_operation_is_not_claimed_or_executed() {
        let dir = std::env::temp_dir().join(format!("nuncio-pim-worker-{}", new_id()));
        let ctx = CoreContext::for_test(Paths::new(dir)).await.expect("ctx");
        let mut account = ctx
            .db
            .read(repo::accounts::local)
            .await
            .expect("local account");
        account.id = "dav1".into();
        account.provider = nuncio_core::models::ProviderKind::Imap;
        account.auth_kind = nuncio_core::models::AuthKind::Password;
        account.email = "me@example.com".into();
        account.carddav_url = Some("https://dav.example.com/carddav/".into());
        account.contacts_enabled = true;
        ctx.db
            .write(move |conn| repo::accounts::insert(conn, &account))
            .await
            .expect("account");

        let op = ctx
            .db
            .write(|conn| {
                let new_op = NewPendingOp::new(
                    "dav1",
                    "pim.contact.save",
                    serde_json::json!({ "contactId": "c1" }),
                    "save",
                );
                pending_ops::enqueue(conn, new_op)
            })
            .await
            .expect("enqueue");
        let op_id = op.id.clone();
        ctx.db
            .write(move |conn| pending_ops::cancel(conn, &op_id))
            .await
            .expect("cancel");

        let op_id = op.id.clone();
        let claimed = ctx
            .db
            .write(move |conn| pending_ops::claim_for_execution(conn, &op_id))
            .await
            .expect("claim");
        assert_eq!(claimed, 0, "a cancelled row must not be claimed");

        let calls = Arc::new(AtomicUsize::new(0));
        let executor: Arc<dyn PimOpExecutor> = Arc::new(CountingExecutor {
            calls: calls.clone(),
        });
        process_pass(&ctx, executor).await.expect("pass");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "the cancelled operation must not reach the executor"
        );

        let op_id = op.id.clone();
        let stored = ctx
            .db
            .read(move |conn| pending_ops::get_required(conn, &op_id))
            .await
            .expect("op");
        assert_eq!(stored.state, PendingOpState::Cancelled);
    }

    struct FailingRollback;

    #[async_trait::async_trait]
    impl PimOpExecutor for FailingRollback {
        fn kind_prefix(&self) -> &'static str {
            "pim.contact."
        }

        fn module(&self) -> SyncModule {
            SyncModule::Contacts
        }

        async fn execute(&self, _op: &PendingOp) -> Result<()> {
            Ok(())
        }

        async fn rollback(&self, _op: &PendingOp) -> Result<()> {
            Ok(())
        }

        fn supports_atomic_rollback(&self) -> bool {
            true
        }

        fn rollback_in_tx(&self, _conn: &rusqlite::Connection, _op: &PendingOp) -> Result<()> {
            Err(AppError::invalid_input("rollback failed"))
        }
    }

    /// Issue 5: `mark_failed` and the rollback commit together. When the
    /// rollback fails, the failure marker must roll back too — otherwise a
    /// delta could delete the row in the gap and the retried rollback would
    /// resurrect it.
    #[tokio::test]
    async fn failed_rollback_does_not_leave_a_failure_marker() {
        let dir = std::env::temp_dir().join(format!("nuncio-pim-worker-{}", new_id()));
        let ctx = CoreContext::for_test(Paths::new(dir)).await.expect("ctx");
        let account_id = ctx
            .db
            .read(repo::accounts::local)
            .await
            .expect("local account")
            .id;
        let op = ctx
            .db
            .write(move |conn| {
                let new_op = NewPendingOp::new(
                    account_id,
                    "pim.contact.save",
                    serde_json::json!({ "contactId": "c1" }),
                    "save",
                );
                pending_ops::enqueue(conn, new_op)
            })
            .await
            .expect("enqueue");
        ctx.db
            .write({
                let id = op.id.clone();
                move |conn| pending_ops::mark_in_progress(conn, &id)
            })
            .await
            .expect("claim");

        let executor: Arc<dyn PimOpExecutor> = Arc::new(FailingRollback);
        fail_op(&ctx, &executor, &op, &AppError::invalid_input("boom")).await;

        let stored = ctx
            .db
            .read({
                let id = op.id.clone();
                move |conn| pending_ops::get_required(conn, &id)
            })
            .await
            .expect("op");
        assert_ne!(
            stored.state,
            PendingOpState::Failed,
            "a failed rollback must undo the failure marker in the same transaction"
        );
    }
}
