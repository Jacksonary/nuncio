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

    /// Hook invoked after a successful write (used to refresh sync state).
    fn on_success(&self, _ctx: &CoreContext, _op: &PendingOp) {}
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
        if let Err(error) = process_pass(&ctx, executor.as_ref()).await {
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
pub async fn process_pass(ctx: &CoreContext, executor: &dyn PimOpExecutor) -> Result<()> {
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
            if let Err(error) = ctx
                .db
                .write({
                    let id = op_id.clone();
                    move |conn| pending_ops::mark_in_progress(conn, &id)
                })
                .await
            {
                warn!(op = %op.kind, %error, "could not claim a pim operation; skipping this pass");
                continue;
            }
            match executor.execute(&op).await {
                Ok(()) => {
                    let id = op_id.clone();
                    // The remote write already happened. If recording it fails we
                    // must not pretend success: the row stays in progress and the
                    // next pass retries, which providers treat idempotently via
                    // the stored remote id.
                    if let Err(error) = ctx
                        .db
                        .write(move |conn| pending_ops::mark_done(conn, &id))
                        .await
                    {
                        warn!(op = %op.kind, %error, "could not record a completed pim operation");
                        continue;
                    }
                    executor.on_success(ctx, &op);
                }
                Err(error) => {
                    handle_failure(ctx, executor, &op, error).await;
                }
            }
        }
    }
    Ok(())
}

async fn handle_failure(
    ctx: &CoreContext,
    executor: &dyn PimOpExecutor,
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
    executor: &dyn PimOpExecutor,
    op: &PendingOp,
    error: &AppError,
) {
    let op_id = op.id.clone();
    let message = error.message();
    if let Err(record_error) = ctx
        .db
        .write(move |conn| pending_ops::mark_failed(conn, &op_id, &message))
        .await
    {
        warn!(op = %op.kind, %record_error, "could not mark a pim operation as failed");
    }
    if let Err(rollback_error) = executor.rollback(op).await {
        warn!(
            module = executor.module().as_str(),
            op = %op.kind,
            %rollback_error,
            "could not roll back a failed pim operation"
        );
    }
    emit_op_failed(
        ctx.events.as_ref(),
        op.account_id.clone(),
        &op.description,
        &error.message(),
    );
}
