//! Folder discovery over Microsoft Graph.
//!
//! Graph exposes the mailbox tree as `/me/mailFolders` plus
//! `/me/mailFolders/{id}/childFolders`, each paginated through
//! `@odata.nextLink`.  This module flattens that tree into the same
//! [`RemoteFolder`] values the IMAP path produces, so the reconciliation in
//! `sync::folders` runs unchanged for Microsoft accounts.
//!
//! Graph has no `\Noselect` concept: every mail folder it returns can be
//! selected.  Role information is carried the way the shared reconciliation
//! already understands it -- as IMAP SPECIAL-USE attributes derived from
//! `wellKnownName` -- which keeps Microsoft folders role-identical to an IMAP
//! account even when the display name is localised.

use std::collections::HashSet;

use nuncio_core::{CoreContext, Result};
use serde::Deserialize;
use tracing::{debug, warn};

use crate::graph::GraphMailClient;
use crate::sync::folders::RemoteFolder;

/// Graph's documented maximum page size for folder collections.  Larger pages
/// mean fewer round trips; `@odata.nextLink` still covers bigger mailboxes.
const PAGE_SIZE: usize = 100;

/// Defensive bound on pagination.  A buggy or malicious `nextLink` loop must
/// not keep an account actor alive forever; 100 pages is 10k folders per
/// level, far beyond anything a real mailbox has.
const MAX_PAGES: usize = 100;

/// One folder object from the Graph folder collections.
///
/// Only the fields this module uses are modelled; Graph returns more
/// (`totalItemCount`, `unreadItemCount`, …) that `RemoteFolder` cannot carry
/// anyway (counts are refreshed by the message sync).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphFolder {
    /// Opaque Graph folder id, needed to list children.
    id: String,
    /// Nullable in the Graph schema; treated as empty below.
    display_name: Option<String>,
    /// Number of direct children; `0` lets us skip the child request entirely.
    #[serde(default)]
    child_folder_count: i64,
    /// Stable role marker such as `inbox` or `sentitems`.
    well_known_name: Option<String>,
}

/// One page of a Graph folder collection.
#[derive(Debug, Deserialize)]
struct FolderPage {
    #[serde(default)]
    value: Vec<GraphFolder>,
    /// Absolute URL for the next page; absent on the last page.
    #[serde(rename = "@odata.nextLink", default)]
    next_link: Option<String>,
}

/// Result of [`list_folders`]: the folders that were read plus whether the walk
/// actually saw the whole tree.
///
/// `sync::folders::reconcile` prunes every local folder that is absent from the
/// listing, together with its messages and favourite state.  That is only sound
/// when the listing is authoritative.  One transient `childFolders` failure (or
/// a pagination cap) yields a partial tree, so the caller must be able to tell
/// the two cases apart and skip pruning on a partial one.
#[derive(Debug, Clone)]
pub struct FolderListing {
    /// Every folder that could be read, parents before children.
    pub folders: Vec<RemoteFolder>,
    /// `true` only when the whole tree was walked without any failure or
    /// truncation.  Callers must not prune local folders when this is `false`.
    pub complete: bool,
}

/// List every mail folder in the account as IMAP-shaped [`RemoteFolder`]s.
///
/// The tree is walked depth-first so a parent always appears before its
/// children, mirroring IMAP `LIST` output.  A failure to list the top level is
/// returned to the caller (the account has no usable folder state), while a
/// failure on one subtree is logged and skipped so the rest of the mailbox
/// still syncs.  Such a partial walk is reported through
/// [`FolderListing::complete`], because the skipped subtree's folders are
/// missing from `folders` without having been deleted on the server.
pub async fn list_folders(
    client: &GraphMailClient,
    ctx: &CoreContext,
    account_id: &str,
) -> Result<FolderListing> {
    let root_url = client.url(&format!("/me/mailFolders?$top={PAGE_SIZE}"));
    let (roots, mut complete) = fetch_all(client, ctx, account_id, root_url).await?;
    debug!(
        account = account_id,
        folders = roots.len(),
        complete,
        "listed top-level Graph mail folders"
    );

    let mut out = Vec::new();
    // Graph never returns the same folder twice under one parent, but the
    // remote path is the local identity: a duplicate would silently overwrite
    // another folder in `upsert_remote`, so detect it and keep the first.
    let mut seen_paths = HashSet::new();

    // An iterative DFS avoids boxing a recursive `async fn`.  Entries hold the
    // already-escaped parent path so each folder can build its own path.
    let mut stack: Vec<(GraphFolder, Option<String>)> = roots
        .into_iter()
        .rev()
        .map(|folder| (folder, None))
        .collect();

    while let Some((folder, parent_path)) = stack.pop() {
        // Skipped before the children are queued, so the whole subtree goes:
        // `syncissues` carries conflicts/serverfailures/localfailures and
        // `conversationhistory` has a child of its own.
        if is_hidden_system_folder(&folder) {
            continue;
        }
        let remote = map_folder(&folder, parent_path.as_deref());
        if !seen_paths.insert(remote.path.clone()) {
            warn!(
                account = account_id,
                folder_id = %folder.id,
                path = %remote.path,
                "duplicate Graph folder path; keeping the first folder"
            );
            continue;
        }
        out.push(remote.clone());

        if folder.child_folder_count <= 0 {
            continue;
        }
        let url = client.url(&format!(
            "/me/mailFolders/{}/childFolders?$top={PAGE_SIZE}",
            GraphMailClient::encode(&folder.id)
        ));
        match fetch_all(client, ctx, account_id, url).await {
            Ok((children, children_complete)) => {
                // A truncated child page list means this subtree is only
                // partially known, even though the request itself succeeded.
                complete &= children_complete;
                // Reverse so the natural server order is preserved when the
                // stack pops them.
                for child in children.into_iter().rev() {
                    stack.push((child, Some(remote.path.clone())));
                }
            }
            Err(error) => {
                // One unreadable subtree must not hide the rest of the
                // mailbox; the folder id is the only actionable diagnostic.
                // The listing is now partial: the skipped folders are absent
                // but still exist on the server.
                complete = false;
                warn!(
                    account = account_id,
                    folder_id = %folder.id,
                    %error,
                    "cannot list Graph child folders; skipping this subtree and marking the listing incomplete"
                );
            }
        }
    }

    Ok(FolderListing {
        folders: out,
        complete,
    })
}

/// Follow `@odata.nextLink` until the collection is exhausted.
///
/// The boolean reports whether the collection was read in full: `false` means
/// the defensive page cap truncated it, so callers must treat the returned
/// folders as a partial view rather than the authoritative list.
async fn fetch_all(
    client: &GraphMailClient,
    ctx: &CoreContext,
    account_id: &str,
    first_url: String,
) -> Result<(Vec<GraphFolder>, bool)> {
    let mut folders = Vec::new();
    let mut next = Some(first_url);
    let mut pages = 0;
    let mut complete = true;

    while let Some(url) = next.take() {
        pages += 1;
        if pages > MAX_PAGES {
            warn!(
                account = account_id,
                url = %url,
                "Graph folder pagination limit reached; truncating"
            );
            complete = false;
            break;
        }
        let page: FolderPage = client.get(ctx, account_id, &url).await?;
        folders.extend(page.value);
        next = page.next_link;
    }

    Ok((folders, complete))
}

/// Server folders that must not appear in the sidebar.
///
/// Two groups, both identified by `wellKnownName` so the filter does not
/// depend on the mailbox language:
///
/// * Exchange diagnostics -- `syncissues` (with its conflicts / server
///   failures / local failures children) and `conversationhistory`.  Outlook's
///   own folder pane hides them; they appear on an Outlook.com mailbox as soon
///   as a desktop client has been connected, and never hold real mail.
/// * `outbox` -- mail queued for delivery.  Nuncio sends through
///   `/me/sendMail`, so this folder is always empty for these accounts, and the
///   local outbox (backed by the outbox table) is what the sidebar shows.  An
///   empty server folder that looks exactly like the real outbox is worse than
///   no folder at all.
fn is_hidden_system_folder(folder: &GraphFolder) -> bool {
    matches!(
        folder
            .well_known_name
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some(
            "syncissues"
                | "conflicts"
                | "serverfailures"
                | "localfailures"
                | "conversationhistory"
                | "outbox"
        )
    )
}

/// Flatten one Graph folder into the shape `sync::folders` consumes.
fn map_folder(folder: &GraphFolder, parent_path: Option<&str>) -> RemoteFolder {
    let display = folder.display_name.as_deref().unwrap_or_default().trim();
    // A blank display name would produce an empty path segment; fall back to
    // the opaque id so the folder still has a stable identity.
    let name = if display.is_empty() {
        folder.id.as_str()
    } else {
        display
    };
    let segment = escape_segment(name);
    let path = match parent_path {
        Some(parent) if !parent.is_empty() => format!("{parent}/{segment}"),
        _ => segment,
    };

    RemoteFolder {
        path,
        // Graph is hierarchical by parent links, but the local model stores a
        // delimiter and derives parents by splitting the path.  `/` is the
        // only separator we build, so it is the delimiter.
        delimiter: Some("/".to_string()),
        name: name.to_string(),
        attributes: attributes_for(folder.well_known_name.as_deref()),
        // Graph has no \Noselect equivalent; every returned mail folder is
        // selectable.
        selectable: true,
        // The local tree is keyed by the readable path, but every Graph
        // request (delta, move, mark-read) needs this opaque id.
        provider_id: Some(folder.id.clone()),
    }
}

/// Escape one display-name segment for `RemoteFolder::path`.
///
/// `Folder.remote_path` is the upsert identity and `sync::folders` splits it on
/// `/` to resolve parents, so a literal `/` inside a folder name would invent a
/// hierarchy level.  Percent-encoding `/` also requires encoding `%`, otherwise
/// a folder literally named `a%2Fb` would collide with a folder named `a/b`;
/// with both escaped the mapping is injective.  Ordinary names are untouched.
fn escape_segment(name: &str) -> String {
    name.replace('%', "%25").replace('/', "%2F")
}

/// Translate Graph's `wellKnownName` into the SPECIAL-USE attributes that
/// `sync::folders::role_for` already recognises.
///
/// This is what makes Sent/Drafts/Trash/Junk/Archive work for a localised
/// Outlook mailbox: Graph localises `displayName` but keeps `wellKnownName`
/// stable.  `Outbox` is deliberately absent, because the local Outbox role is
/// backed by the outbox table and must not be reassigned to a server folder.
/// Unknown and missing names yield no attributes, so the shared name table
/// still gets a chance to classify the folder.
fn attributes_for(well_known_name: Option<&str>) -> Vec<String> {
    let attribute = match well_known_name.map(str::to_ascii_lowercase).as_deref() {
        Some("inbox") => "\\Inbox",
        Some("sentitems") => "\\Sent",
        Some("drafts") => "\\Drafts",
        Some("deleteditems") => "\\Trash",
        Some("junkemail") => "\\Junk",
        Some("archive") => "\\Archive",
        _ => return Vec::new(),
    };
    vec![attribute.to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::models::FolderRole;

    fn graph_folder(
        id: &str,
        name: &str,
        well_known: Option<&str>,
        child_count: i64,
    ) -> GraphFolder {
        GraphFolder {
            id: id.to_string(),
            display_name: Some(name.to_string()),
            child_folder_count: child_count,
            well_known_name: well_known.map(str::to_string),
        }
    }

    fn role_of(remote: &RemoteFolder) -> FolderRole {
        crate::sync::folders::role_for(
            &remote.path,
            remote.delimiter.as_deref(),
            &remote.attributes,
        )
    }

    #[test]
    fn exchange_diagnostics_are_hidden_but_real_folders_are_not() {
        for well_known in [
            "syncissues",
            "conflicts",
            "serverfailures",
            "localfailures",
            "conversationhistory",
            "outbox",
        ] {
            assert!(
                is_hidden_system_folder(&graph_folder("id", "x", Some(well_known), 0)),
                "{well_known} must be hidden"
            );
        }
        // Case does not matter.
        assert!(is_hidden_system_folder(&graph_folder(
            "id",
            "Sync Issues",
            Some("SyncIssues"),
            3
        )));
        // Ordinary folders, and folders Graph gives no wellKnownName, stay.
        for well_known in [None, Some("inbox"), Some("archive"), Some("deleteditems")] {
            assert!(
                !is_hidden_system_folder(&graph_folder("id", "x", well_known, 0)),
                "{well_known:?} must stay visible"
            );
        }
        // `RSS 源` is the legacy RSS folder; Graph reports no wellKnownName for
        // it, so it must not be caught by a name-based rule.
        assert!(!is_hidden_system_folder(&graph_folder(
            "id",
            "RSS 源",
            None,
            0
        )));
    }

    #[test]
    fn escape_segment_keeps_ordinary_names() {
        assert_eq!(escape_segment("Inbox"), "Inbox");
        assert_eq!(escape_segment("Projects 2026"), "Projects 2026");
        assert_eq!(escape_segment("收件箱"), "收件箱");
        assert_eq!(escape_segment("Sent Items"), "Sent Items");
    }

    #[test]
    fn escape_segment_removes_the_path_delimiter_and_stays_injective() {
        assert_eq!(escape_segment("Q1/2026"), "Q1%2F2026");
        assert_eq!(escape_segment("a%2Fb"), "a%252Fb");
        assert_eq!(escape_segment("100%"), "100%25");
        // The two names that would otherwise flatten to the same path differ.
        assert_ne!(escape_segment("a/b"), escape_segment("a%2Fb"));
        // No escaped segment contains a literal delimiter.
        assert!(!escape_segment("a/b/c").contains('/'));
    }

    #[test]
    fn path_is_hierarchical() {
        let parent = map_folder(&graph_folder("id-parent", "Projects", None, 1), None);
        assert_eq!(parent.path, "Projects");
        let child = map_folder(
            &graph_folder("id-child", "2026", None, 0),
            Some(&parent.path),
        );
        assert_eq!(child.path, "Projects/2026");
        assert_eq!(child.name, "2026");
        assert_eq!(child.delimiter.as_deref(), Some("/"));
        assert!(child.selectable);
    }

    #[test]
    fn escaped_names_do_not_create_parent_links() {
        let remote = map_folder(&graph_folder("id", "Q1/2026", None, 0), None);
        assert_eq!(remote.path, "Q1%2F2026");
        // The IMAP reconciliation derives a parent by splitting on `/`; an
        // escaped segment must not yield one.
        assert!(!remote.path.contains('/'));
    }

    #[test]
    fn well_known_names_map_to_imap_roles() {
        for (well_known, expected) in [
            ("inbox", FolderRole::Inbox),
            ("sentitems", FolderRole::Sent),
            ("drafts", FolderRole::Drafts),
            ("deleteditems", FolderRole::Trash),
            ("junkemail", FolderRole::Junk),
            ("archive", FolderRole::Archive),
        ] {
            // A localised display name must not matter: the attribute wins.
            let remote = map_folder(
                &graph_folder("id", "some-localised-name", Some(well_known), 0),
                None,
            );
            assert_eq!(role_of(&remote), expected, "{well_known}");
        }
    }

    #[test]
    fn well_known_name_is_case_insensitive() {
        let remote = map_folder(&graph_folder("id", "Posteingang", Some("Inbox"), 0), None);
        assert_eq!(role_of(&remote), FolderRole::Inbox);
    }

    #[test]
    fn unknown_and_missing_well_known_names_fall_back_to_the_name_table() {
        let unknown = map_folder(&graph_folder("id", "Sent Items", Some("notes"), 0), None);
        assert!(unknown.attributes.is_empty());
        assert_eq!(role_of(&unknown), FolderRole::Sent);

        let missing = map_folder(&graph_folder("id", "Notes", None, 0), None);
        assert!(missing.attributes.is_empty());
        assert_eq!(role_of(&missing), FolderRole::Other);
    }

    #[test]
    fn outbox_and_non_mail_well_known_names_are_not_roles() {
        assert!(attributes_for(Some("outbox")).is_empty());
        assert!(attributes_for(Some("msgfolderroot")).is_empty());
        assert!(attributes_for(Some("conversationhistory")).is_empty());
    }

    #[test]
    fn blank_display_name_falls_back_to_the_folder_id() {
        let mut folder = graph_folder("AAMkAD%3D", "Inbox", None, 0);
        folder.display_name = Some("   ".to_string());
        let remote = map_folder(&folder, None);
        // The id is escaped too, so a `%` in it cannot forge a path.
        assert_eq!(remote.path, "AAMkAD%253D");
        assert_eq!(remote.name, "AAMkAD%3D");
    }
}
