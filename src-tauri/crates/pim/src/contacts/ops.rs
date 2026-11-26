//! Serializable payloads for `pim.contact.*` pending operations.
//!
//! Every payload embeds the rows as they were before the optimistic local
//! change so a permanent remote failure can be rolled back exactly.

use nuncio_core::models::{Contact, ContactGroup, PendingOp as CorePendingOp};
use serde::{Deserialize, Serialize};

use crate::error::{internal, Result};

pub const KIND_PREFIX: &str = "pim.contact.";

pub const KIND_SAVE: &str = "pim.contact.save";
pub const KIND_DELETE: &str = "pim.contact.delete";
pub const KIND_FAVORITE: &str = "pim.contact.favorite";
pub const KIND_GROUP_CREATE: &str = "pim.contact.group.create";
pub const KIND_GROUP_RENAME: &str = "pim.contact.group.rename";
pub const KIND_GROUP_DELETE: &str = "pim.contact.group.delete";
pub const KIND_GROUP_MEMBERS: &str = "pim.contact.group.members";

/// A contact plus its provider sync metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContactSnapshot {
    pub contact: Contact,
    pub etag: Option<String>,
    pub remote_id: Option<String>,
    pub href: Option<String>,
    pub raw_vcard: Option<String>,
    /// `remote_id` was derived from the href rather than being a real provider
    /// UID. A snapshot written by an older build defaults to a real UID, the
    /// same rule the tombstone/receipt columns use.
    #[serde(default)]
    pub uid_is_derived: bool,
}

impl ContactSnapshot {
    pub fn new(contact: Contact) -> Self {
        Self {
            contact,
            etag: None,
            remote_id: None,
            href: None,
            raw_vcard: None,
            uid_is_derived: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupSnapshot {
    pub group: ContactGroup,
    pub remote_id: Option<String>,
    pub member_ids: Vec<String>,
}

/// A queued contacts write.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
#[allow(clippy::large_enum_variant)]
pub enum ContactOp {
    /// Create or update: executed as an update when the contact has provider
    /// metadata, otherwise as a create.
    Save {
        contact_id: String,
        /// A real provider UID assigned to a card whose local identity was
        /// derived from its href (`uid_is_derived`).
        ///
        /// Generated and persisted **before** the first wire attempt so a
        /// retry reuses the same value instead of writing a different UID on
        /// every attempt. `None` until the executor needs one (and forever for
        /// cards that already carry a real UID).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        assigned_uid: Option<String>,
        /// `None` for a create (rollback deletes the row).
        snapshot: Option<ContactSnapshot>,
    },
    Delete {
        contacts: Vec<ContactSnapshot>,
    },
    Favorite {
        contact_id: String,
        value: bool,
        previous: bool,
    },
    GroupCreate {
        group_id: String,
    },
    GroupRename {
        group_id: String,
        previous_name: String,
    },
    GroupDelete {
        snapshot: GroupSnapshot,
    },
    GroupMembers {
        group_id: String,
        added: Vec<String>,
        removed: Vec<String>,
    },
}

impl ContactOp {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Save { .. } => KIND_SAVE,
            Self::Delete { .. } => KIND_DELETE,
            Self::Favorite { .. } => KIND_FAVORITE,
            Self::GroupCreate { .. } => KIND_GROUP_CREATE,
            Self::GroupRename { .. } => KIND_GROUP_RENAME,
            Self::GroupDelete { .. } => KIND_GROUP_DELETE,
            Self::GroupMembers { .. } => KIND_GROUP_MEMBERS,
        }
    }

    pub fn description(&self, label: &str) -> String {
        match self {
            Self::Save { snapshot: None, .. } => format!("创建联系人「{label}」"),
            Self::Save { .. } => format!("更新联系人「{label}」"),
            Self::Delete { .. } => format!("删除联系人「{label}」"),
            Self::Favorite { .. } => format!("更新联系人「{label}」的收藏状态"),
            Self::GroupCreate { .. } => "创建联系人分组".to_string(),
            Self::GroupRename { .. } => "重命名联系人分组".to_string(),
            Self::GroupDelete { .. } => "删除联系人分组".to_string(),
            Self::GroupMembers { .. } => "更新联系人分组成员".to_string(),
        }
    }
}

pub fn decode(op: &CorePendingOp) -> Result<ContactOp> {
    serde_json::from_value(op.payload.clone())
        .map_err(|error| internal(format!("invalid contact pending op payload: {error}")))
}

pub fn to_value(op: &ContactOp) -> Result<serde_json::Value> {
    serde_json::to_value(op)
        .map_err(|error| internal(format!("cannot encode contact pending op: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_decodes() {
        let mut contact = Contact::new("c1", "book", "acc");
        contact.display_name = "Ada".into();
        let op = ContactOp::Save {
            contact_id: "c1".into(),
            assigned_uid: None,
            snapshot: None,
        };
        let value = to_value(&op).unwrap();
        assert_eq!(value["op"], "save");
        let decoded: ContactOp = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.kind(), KIND_SAVE);
        assert_eq!(decoded.description("Ada"), "创建联系人「Ada」");
        let snapshot = ContactSnapshot::new(contact);
        assert_eq!(snapshot.contact.display_name, "Ada");
    }

    #[test]
    fn kinds_are_namespaced() {
        for kind in [
            KIND_SAVE,
            KIND_DELETE,
            KIND_FAVORITE,
            KIND_GROUP_CREATE,
            KIND_GROUP_RENAME,
            KIND_GROUP_DELETE,
            KIND_GROUP_MEMBERS,
        ] {
            assert!(kind.starts_with(KIND_PREFIX));
        }
    }
}
