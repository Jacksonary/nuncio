//! Temporary audit probes: each assertion describes the required behavior.
mod support;

use std::sync::Arc;
use chrono::{Duration, Utc};
use nuncio_core::models::{Address, OutboxItem, OutboxState, Security, ServerConfig};
use nuncio_core::{repo, CoreContext};
use nuncio_mail::{ops, MailService};
use support::{account, context, insert_account, RecordingNotifier, StaticCreds};
use support::mock_imap::MockImap;
use support::mock_pop3::MockPop3;
use support::mock_smtp::MockSmtp;

fn server(port: u16) -> ServerConfig {
    ServerConfig::new("127.0.0.1", port, Security::None, "test@example.com")
}

async fn ctx() -> CoreContext {
    context(StaticCreds::password("test@example.com", "secret"), Arc::new(RecordingNotifier::default())).await
}

#[tokio::test]
async fn unsaved_send_must_queue_sent_copy() {
    let imap = MockImap::start_with_folders(&["UIDPLUS"], &[("INBOX", &["\\Inbox"]), ("Sent", &["\\Sent"])]).await;
    let smtp = MockSmtp::start().await;
    let ctx = ctx().await;
    let account = account("acc", "test@example.com", Some(server(imap.addr.port())), Some(server(smtp.addr.port())), None);
    insert_account(&ctx, &account).await;
    let mut connection = nuncio_mail::imap::connect(account.imap.as_ref().unwrap(), &StaticCreds::password("test@example.com", "secret").auth).await.unwrap();
    nuncio_mail::sync::sync_account(&ctx, &mut connection, &account).await.unwrap();
    let service = MailService::new(ctx.clone());
    let mut draft = service.compose_create(Some("acc")).await.unwrap();
    assert!(draft.id.is_none());
    draft.to = vec![Address::new("recipient@example.com")];
    draft.subject = "Unsaved send audit".into();
    draft.html = "<p>Body</p>".into();
    service.compose_send(&draft).await.unwrap();
    assert_eq!(smtp.messages.lock().unwrap().len(), 1, "SMTP accepted the message");
    let ops = ctx.db.read(|db| repo::pending_ops::list(db, Some("acc"))).await.unwrap();
    assert!(ops.iter().any(|op| op.kind == ops::KIND_APPEND_TO_SENT), "sent copy was not queued for an unsaved draft");
}

#[tokio::test]
async fn another_account_pass_must_not_recover_active_send() {
    let ctx = ctx().await;
    let a = account("a", "a@example.com", None, None, None);
    let b = account("b", "b@example.com", None, None, None);
    insert_account(&ctx, &a).await;
    insert_account(&ctx, &b).await;
    let mut item = OutboxItem::new("a", None, vec![Address::new("recipient@example.com")], "Old queued mail", "/unused.eml");
    item.created_at = Utc::now() - Duration::hours(1);
    item.state = OutboxState::Sending;
    let id = item.id.clone();
    ctx.db.write(move |db| repo::outbox::enqueue(db, &item)).await.unwrap();
    nuncio_mail::smtp::outbox::process_due(&ctx, &b).await.unwrap();
    let item = ctx.db.read(move |db| repo::outbox::get_required(db, &id)).await.unwrap();
    assert_eq!(item.state, OutboxState::Sending, "an unrelated account reset the live send to pending");
}

#[tokio::test]
async fn discard_must_reject_sending_item() {
    let ctx = ctx().await;
    let a = account("a", "a@example.com", None, None, None);
    insert_account(&ctx, &a).await;
    let mut item = OutboxItem::new("a", None, vec![Address::new("recipient@example.com")], "In flight", "/unused.eml");
    item.state = OutboxState::Sending;
    let id = item.id.clone();
    ctx.db.write(move |db| repo::outbox::enqueue(db, &item)).await.unwrap();
    let result = MailService::new(ctx).outbox_discard(&format!("outbox:{id}")).await;
    assert!(result.is_err(), "a sending item was discarded successfully");
}

#[tokio::test]
async fn pop3_must_be_readable_after_current_clear_cache_steps() {
    let mock = MockPop3::start().await;
    mock.add("uid-a", &support::message_raw("sender@example.com", "test@example.com", "POP audit", "Only body", "audit@x"));
    let ctx = ctx().await;
    let account = account("pop", "test@example.com", None, None, Some(server(mock.addr.port())));
    insert_account(&ctx, &account).await;
    nuncio_mail::pop3::sync::sync_account(&ctx, &account).await.unwrap();
    let id: String = ctx.db.read(|db| Ok(db.query_row("SELECT id FROM message WHERE account_id = 'pop'", [], |row| row.get(0))?)).await.unwrap();
    let service = MailService::new(ctx.clone());
    service.message_get(&id, false).await.unwrap();
    // Exact database/filesystem steps in app_clear_cache, without OS wiring.
    ctx.db.write(|db| {
        db.execute("UPDATE message SET body_path = NULL, body_cached_at = NULL", [])?;
        db.execute("UPDATE attachment SET path = NULL, is_downloaded = 0", [])?;
        Ok(())
    }).await.unwrap();
    for dir in [ctx.paths.mime_root(), ctx.paths.attachments_dir(), ctx.paths.tmp_dir()] {
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
    ctx.paths.ensure_dirs().unwrap();
    nuncio_mail::pop3::sync::sync_account(&ctx, &account).await.unwrap();
    let result = service.message_get(&id, false).await;
    assert!(result.is_ok(), "POP3 body remains unreadable after sync: {:?}", result.err());
}
