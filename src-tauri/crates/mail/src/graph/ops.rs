//! Microsoft Graph equivalents of the IMAP server operations.
//!
//! The IMAP path performs user actions through [`crate::ops::executor`]: it
//! stores flags, moves messages, appends copies and so on.  Accounts synced
//! through Graph need the same actions expressed as Graph calls, so this module
//! mirrors one IMAP executor operation at a time.
//!
//! Deliberate differences from IMAP, because Graph's data model differs:
//!
//! * `\Answered` and `$Forwarded` have no Graph message flag.  [`set_flags`]
//!   therefore only carries `isRead` and the follow-up flag; a change that
//!   touches *only* the other two flags is a no-op and sends no request.
//! * Graph assigns a **new message id** when a message is moved.  The caller
//!   must re-sync the affected folders and rewrite the local id mapping;
//!   expecting the old id to keep working leaves later operations on a stale
//!   id.
//! * `DELETE /me/messages/{id}` is a soft delete: it moves the message to
//!   Deleted Items.  [`permanent_delete_message`] performs the hard delete.
//! * Sending through Graph saves the copy in Sent Items automatically, so the
//!   `appendToSent` operation must not run for a Graph account.
//! * Graph folders have no IMAP subscription concept, so `folderSubscribe`
//!   has no counterpart here and stays a no-op.
//!
//! The transport (auth injection, 401 retry, error mapping) lives in
//! [`super::GraphMailClient`]; this module only shapes requests and bodies.

use base64::Engine as _;
use nuncio_core::{AppError, CoreContext, Result};
use reqwest::header::CONTENT_TYPE;
use reqwest::{Response, StatusCode};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tracing::{debug, warn};

use super::GraphMailClient;

/// Media type Graph's MIME submission path expects.
///
/// The v1.0 reference for `user: sendMail` and `Create message` specifies
/// `text/plain` for a MIME body (the body itself is the base64-encoded
/// RFC822 message).  `text/rfc822`, which older samples used, is not the
/// documented value; Graph distinguishes MIME from JSON by this header, so it
/// must stay a text media type.
const MIME_CONTENT_TYPE: &str = "text/plain";

/// Page size used when enumerating messages for a folder-wide flag change.
const FOLDER_PAGE_SIZE: usize = 100;

/// Hard bound on folder pagination, so a broken `@odata.nextLink` loop cannot
/// spin forever.  500 pages × 100 messages is far above any real folder and
/// simply stops the operation instead of hammering the service.
const MAX_FOLDER_PAGES: usize = 500;

/// Minimal shape of a Graph message returned by a create/move call.
#[derive(Debug, Deserialize)]
struct IdResponse {
    id: String,
}

/// One page of `GET /me/mailFolders/{id}/messages`.
#[derive(Debug, Deserialize)]
struct MessagePage {
    #[serde(default)]
    value: Vec<MessageRef>,
    /// Absolute URL for the next page; absent on the last page.
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

/// The only fields needed to mark a whole folder as read.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageRef {
    id: String,
    #[serde(default)]
    is_read: bool,
}

/// Build the PATCH body for [`set_flags`], or `None` when nothing would change.
///
/// Pure so the three request shapes (read-only, flagged-only, both) are
/// unit-testable without a network.  Graph's `flagStatus` values are
/// `flagged` / `notFlagged`; note the lower-case `n` in `notFlagged`.
fn flags_body(read: Option<bool>, flagged: Option<bool>) -> Option<Value> {
    let mut body = Map::new();
    if let Some(read) = read {
        body.insert("isRead".to_string(), Value::Bool(read));
    }
    if let Some(flagged) = flagged {
        let status = if flagged { "flagged" } else { "notFlagged" };
        body.insert("flag".to_string(), json!({ "flagStatus": status }));
    }
    if body.is_empty() {
        return None;
    }
    Some(Value::Object(body))
}

/// Build the body for `POST /me/mailFolders`.
///
/// `parentFolderId` is omitted rather than set to null for a top-level folder:
/// Graph then creates it directly under the message root.
fn create_folder_body(name: &str, parent_id: Option<&str>) -> Value {
    let mut body = json!({ "displayName": name });
    if let Some(parent_id) = parent_id {
        body["parentFolderId"] = json!(parent_id);
    }
    body
}

/// Base64-encode an RFC822 message for Graph's MIME submission endpoints.
///
/// Graph expects the *whole* message, headers and attachments included, as one
/// base64 string.  Nothing is parsed or rebuilt here, which is what preserves
/// headers the structured JSON message shape cannot express.
fn encode_mime(rfc822: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(rfc822)
}

/// Turn a non-2xx response into the project's error type, preserving the body
/// snippet that Graph uses to explain mailbox and permission problems.
async fn expect_success(response: Response) -> Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(GraphMailClient::failure(status, &body))
}

/// Map a failed JSON decode to a protocol error.
///
/// `reqwest::Response::json` reports decode failures as `reqwest::Error`, so
/// this takes any displayable error rather than a concrete serde type.
fn invalid_json(what: &str, error: impl std::fmt::Display) -> AppError {
    AppError::protocol(format!("invalid Graph {what} JSON: {error}"))
}

/// Set the read and/or follow-up flag of one message.
///
/// The IMAP executor maps `\Seen` and `\Flagged` onto this call.
/// `answered`/`forwarded` have no Graph equivalent and are intentionally not
/// parameters; passing both `None` here is a no-op so a queue entry that only
/// changed those flags still completes instead of retrying forever.
pub async fn set_flags(
    client: &GraphMailClient,
    ctx: &CoreContext,
    account_id: &str,
    remote_id: &str,
    read: Option<bool>,
    flagged: Option<bool>,
) -> Result<()> {
    let Some(body) = flags_body(read, flagged) else {
        return Ok(());
    };
    let url = client.url(&format!(
        "/me/messages/{}",
        GraphMailClient::encode(remote_id)
    ));
    let response = client
        .send_authed(ctx, account_id, |http| http.patch(&url).json(&body))
        .await?;
    expect_success(response).await?;
    debug!(
        account = account_id,
        remote_id,
        read = ?read,
        flagged = ?flagged,
        "updated Graph message flags"
    );
    Ok(())
}

/// Move one message to another folder and return the id Graph assigned to the
/// moved copy.
///
/// Archive and "mark as junk" are both this operation: the caller resolves the
/// role folder locally and passes its Graph folder id (the `remote_path` of the
/// local folder row).  Graph returns the moved message under a **new id**, so
/// the caller must both re-sync the source and destination folders and rewrite
/// the local id mapping; the old `remote_id` is invalid afterwards.  Returning
/// the new id is what lets the executor keep later queued operations (mark
/// read, delete) working against the moved message instead of a stale id that
/// Graph answers with 404.
pub async fn move_message(
    client: &GraphMailClient,
    ctx: &CoreContext,
    account_id: &str,
    remote_id: &str,
    destination_folder_id: &str,
) -> Result<String> {
    let url = client.url(&format!(
        "/me/messages/{}/move",
        GraphMailClient::encode(remote_id)
    ));
    let body = json!({ "destinationId": destination_folder_id });
    let response = client
        .send_authed(ctx, account_id, |http| http.post(&url).json(&body))
        .await?;
    let response = expect_success(response).await?;
    let moved: IdResponse = response
        .json()
        .await
        .map_err(|error| invalid_json("moved message", error))?;
    debug!(
        account = account_id,
        remote_id,
        destination_folder_id,
        new_id = %moved.id,
        "moved Graph message"
    );
    Ok(moved.id)
}

/// Delete one message by moving it to Deleted Items.
///
/// Graph's `DELETE /me/messages/{id}` is a soft delete; there is no per-call
/// "hard" variant on this verb.  A 404 is treated as success because the
/// operation is idempotent and the message being gone is the desired result.
/// Use [`permanent_delete_message`] when the user asked to bypass the trash.
pub async fn delete_message(
    client: &GraphMailClient,
    ctx: &CoreContext,
    account_id: &str,
    remote_id: &str,
) -> Result<()> {
    let url = client.url(&format!(
        "/me/messages/{}",
        GraphMailClient::encode(remote_id)
    ));
    let response = client
        .send_authed(ctx, account_id, |http| http.delete(&url))
        .await?;
    if response.status() == StatusCode::NOT_FOUND {
        debug!(account = account_id, remote_id, "message already gone");
        return Ok(());
    }
    expect_success(response).await?;
    debug!(account = account_id, remote_id, "deleted Graph message");
    Ok(())
}

/// Permanently delete one message (the IMAP permanent-delete / expunge path).
///
/// Graph exposes this as the `permanentDelete` action, not as a second DELETE:
/// it moves the item into the mailbox dumpster's `purges` folder, where mail
/// clients can no longer see it.  The request has no body and answers 204.
/// A 404 is success for the same idempotency reason as [`delete_message`].
pub async fn permanent_delete_message(
    client: &GraphMailClient,
    ctx: &CoreContext,
    account_id: &str,
    remote_id: &str,
) -> Result<()> {
    let url = client.url(&format!(
        "/me/messages/{}/permanentDelete",
        GraphMailClient::encode(remote_id)
    ));
    let response = client
        .send_authed(ctx, account_id, |http| http.post(&url))
        .await?;
    if response.status() == StatusCode::NOT_FOUND {
        debug!(account = account_id, remote_id, "message already gone");
        return Ok(());
    }
    expect_success(response).await?;
    debug!(
        account = account_id,
        remote_id, "permanently deleted Graph message"
    );
    Ok(())
}

/// Create a mail folder and return its Graph id.
///
/// Graph creates the folder under the message root when `parent_id` is absent.
/// The returned id is what the local folder row should store as `remote_path`,
/// matching the IMAP path's use of the mailbox name there.
pub async fn create_folder(
    client: &GraphMailClient,
    ctx: &CoreContext,
    account_id: &str,
    name: &str,
    parent_id: Option<&str>,
) -> Result<String> {
    let url = client.url("/me/mailFolders");
    let body = create_folder_body(name, parent_id);
    let response = client
        .send_authed(ctx, account_id, |http| http.post(&url).json(&body))
        .await?;
    let response = expect_success(response).await?;
    let created: IdResponse = response
        .json()
        .await
        .map_err(|error| invalid_json("created folder", error))?;
    debug!(
        account = account_id,
        folder = %created.id,
        name,
        "created Graph folder"
    );
    Ok(created.id)
}

/// Rename a folder; only its display name changes, so its id and contents stay
/// put (unlike IMAP, where the mailbox path itself is the identity).
pub async fn rename_folder(
    client: &GraphMailClient,
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
    name: &str,
) -> Result<()> {
    let url = client.url(&format!(
        "/me/mailFolders/{}",
        GraphMailClient::encode(folder_id)
    ));
    let body = json!({ "displayName": name });
    let response = client
        .send_authed(ctx, account_id, |http| http.patch(&url).json(&body))
        .await?;
    expect_success(response).await?;
    debug!(
        account = account_id,
        folder = folder_id,
        name,
        "renamed Graph folder"
    );
    Ok(())
}

/// Delete a folder.
///
/// Like message deletion this is soft: Graph moves the folder and its contents
/// to Deleted Items.  That still matches the user's action (the folder leaves
/// the tree), and the retention window is the server's policy.  A 404 counts as
/// success for idempotency.
pub async fn delete_folder(
    client: &GraphMailClient,
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
) -> Result<()> {
    let url = client.url(&format!(
        "/me/mailFolders/{}",
        GraphMailClient::encode(folder_id)
    ));
    let response = client
        .send_authed(ctx, account_id, |http| http.delete(&url))
        .await?;
    if response.status() == StatusCode::NOT_FOUND {
        debug!(
            account = account_id,
            folder = folder_id,
            "folder already gone"
        );
        return Ok(());
    }
    expect_success(response).await?;
    debug!(
        account = account_id,
        folder = folder_id,
        "deleted Graph folder"
    );
    Ok(())
}

/// Mark every unread message in a folder as read.
///
/// Graph has no "flag all messages" command and JSON batching is not required
/// here, so the folder is paged and each unread message is PATCHed.  Messages
/// that are already read are skipped to keep the request count proportional to
/// the actual work.  Returns the number of messages updated.
pub async fn mark_folder_read(
    client: &GraphMailClient,
    ctx: &CoreContext,
    account_id: &str,
    folder_id: &str,
) -> Result<usize> {
    let mut next = Some(client.url(&format!(
        "/me/mailFolders/{}/messages?%24select=id%2CisRead&%24top={FOLDER_PAGE_SIZE}",
        GraphMailClient::encode(folder_id)
    )));
    let mut updated = 0usize;
    let mut pages = 0usize;
    while let Some(url) = next.take() {
        pages += 1;
        if pages > MAX_FOLDER_PAGES {
            warn!(
                account = account_id,
                folder = folder_id,
                "Graph folder pagination limit reached; some messages stay unread"
            );
            break;
        }
        let page: MessagePage = client.get(ctx, account_id, &url).await?;
        for message in page.value {
            if message.is_read {
                continue;
            }
            set_flags(client, ctx, account_id, &message.id, Some(true), None).await?;
            updated += 1;
        }
        next = page.next_link;
    }
    debug!(
        account = account_id,
        folder = folder_id,
        updated,
        "marked Graph folder as read"
    );
    Ok(updated)
}

/// Send an already-built RFC822 message through Graph.
///
/// Graph wants MIME, not a parsed JSON body: the bytes are base64-encoded and
/// POSTed to `/me/sendMail` with a `text/plain` content type.  Building the
/// message as JSON instead would silently drop headers and attachments.
///
/// Two consequences the caller must respect:
/// * Graph takes the recipients from the MIME headers, so unlike the SMTP path
///   the message must still carry its `Bcc` header when there are Bcc
///   recipients.  Stage Graph sends with `build_message_with_bcc`.
/// * Graph saves the sent copy in Sent Items itself, so the `appendToSent`
///   operation must be skipped for Graph accounts.
pub async fn send_raw(
    client: &GraphMailClient,
    ctx: &CoreContext,
    account_id: &str,
    rfc822: &[u8],
) -> Result<()> {
    if rfc822.is_empty() {
        return Err(AppError::invalid_input("cannot send an empty message"));
    }
    let url = client.url("/me/sendMail");
    let body = encode_mime(rfc822);
    let size = rfc822.len();
    let response = client
        .send_authed(ctx, account_id, |http| {
            http.post(&url)
                .header(CONTENT_TYPE, MIME_CONTENT_TYPE)
                .body(body)
        })
        .await?;
    expect_success(response).await?;
    debug!(
        account = account_id,
        bytes = size,
        "sent message through Graph"
    );
    Ok(())
}

/// Create a server-side draft from an RFC822 message and return its Graph id.
///
/// `POST /me/messages` with MIME content creates the draft in Drafts by
/// default, which is exactly the IMAP `APPEND ... (\Draft)` behaviour.  The
/// draft must therefore keep its `Bcc` header (the local draft is built with
/// `build_message_with_bcc`).  The returned id is what the executor records so
/// a later autosave can delete the previous remote copy.
pub async fn append_draft(
    client: &GraphMailClient,
    ctx: &CoreContext,
    account_id: &str,
    rfc822: &[u8],
) -> Result<String> {
    let url = client.url("/me/messages");
    let body = encode_mime(rfc822);
    let size = rfc822.len();
    let response = client
        .send_authed(ctx, account_id, |http| {
            http.post(&url)
                .header(CONTENT_TYPE, MIME_CONTENT_TYPE)
                .body(body)
        })
        .await?;
    let response = expect_success(response).await?;
    let created: IdResponse = response
        .json()
        .await
        .map_err(|error| invalid_json("created draft", error))?;
    debug!(
        account = account_id,
        draft = %created.id,
        bytes = size,
        "created Graph draft"
    );
    Ok(created.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_body_carries_read_only() {
        assert_eq!(
            flags_body(Some(true), None),
            Some(json!({ "isRead": true }))
        );
        assert_eq!(
            flags_body(Some(false), None),
            Some(json!({ "isRead": false }))
        );
    }

    #[test]
    fn flags_body_carries_follow_up_only() {
        assert_eq!(
            flags_body(None, Some(true)),
            Some(json!({ "flag": { "flagStatus": "flagged" } }))
        );
        // Graph spells the cleared value with a lower-case "n".
        assert_eq!(
            flags_body(None, Some(false)),
            Some(json!({ "flag": { "flagStatus": "notFlagged" } }))
        );
    }

    #[test]
    fn flags_body_combines_both_and_omits_nothing() {
        assert_eq!(
            flags_body(Some(true), Some(false)),
            Some(json!({
                "isRead": true,
                "flag": { "flagStatus": "notFlagged" }
            }))
        );
        // A queue entry that only changed \Answered/\Forwarded must not spend a
        // request: there is no Graph flag to carry.
        assert_eq!(flags_body(None, None), None);
    }

    #[test]
    fn mime_body_is_standard_base64_of_the_raw_bytes() {
        assert_eq!(encode_mime(b"Hello"), "SGVsbG8=");
        let raw = concat!(
            "From: Alice <alice@example.com>\r\n",
            "To: Bob <bob@example.com>\r\n",
            "Subject: =?UTF-8?B?5rWL6K+V?=\r\n",
            "\r\n",
            "body\r\n"
        );
        let encoded = encode_mime(raw.as_bytes());
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&encoded)
                .expect("decode"),
            raw.as_bytes()
        );
        // Standard base64 alphabet, not URL-safe: "+" and "/" survive.
        assert!(!encoded.contains('-') && !encoded.contains('_'));
    }

    #[test]
    fn create_folder_body_omits_parent_for_top_level() {
        assert_eq!(
            create_folder_body("Receipts", None),
            json!({ "displayName": "Receipts" })
        );
        assert_eq!(
            create_folder_body("2026", Some("parent-id")),
            json!({ "displayName": "2026", "parentFolderId": "parent-id" })
        );
    }

    #[test]
    fn message_page_reads_the_odata_next_link() {
        let page: MessagePage = serde_json::from_str(
            r#"{
                "value": [
                    { "id": "m1", "isRead": false },
                    { "id": "m2", "isRead": true }
                ],
                "@odata.nextLink": "https://graph.microsoft.com/v1.0/me/mailFolders/x/messages?$skip=100"
            }"#,
        )
        .expect("page");
        assert_eq!(page.value.len(), 2);
        assert!(!page.value[0].is_read);
        assert!(page.value[1].is_read);
        assert_eq!(
            page.next_link.as_deref(),
            Some("https://graph.microsoft.com/v1.0/me/mailFolders/x/messages?$skip=100")
        );
    }
}
