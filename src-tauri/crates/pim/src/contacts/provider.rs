//! Provider-neutral contact abstraction.
//!
//! Google People, Microsoft Graph and CardDAV all expose "a set of address
//! books containing contacts, optionally organised in groups".  The shapes
//! below mirror [`crate::provider`] for calendars so the sync engine can treat
//! every backend the same way.

use async_trait::async_trait;
use nuncio_core::models::{Account, Address, Contact};

use crate::error::Result;

/// A provider-side address book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAddressBook {
    /// Local `address_book.id`; filled in by the sync engine.
    pub local_id: String,
    /// Provider identifier (Google `people/me`, Graph folder id or the
    /// absolute CardDAV collection URL).
    pub remote_id: String,
    /// Collection URL for CardDAV.
    pub href: Option<String>,
    pub name: String,
    pub read_only: bool,
}

impl RemoteAddressBook {
    pub fn new(remote_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            local_id: String::new(),
            remote_id: remote_id.into(),
            href: None,
            name: name.into(),
            read_only: false,
        }
    }
}

/// Incremental sync cursor for one address book.
#[derive(Debug, Clone, Default)]
pub struct ContactCursor {
    pub local_address_book_id: String,
    /// Google `nextSyncToken` (People sync tokens expire).
    pub sync_token: Option<String>,
    /// CardDAV `getctag` fallback.
    pub ctag: Option<String>,
    /// `(href, etag)` pairs used by the CardDAV ctag fallback.
    pub etags: Vec<(String, String)>,
}

/// A provider-side group (Google contact groups; Graph/CardDAV derive groups
/// from contact categories so they never appear here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteGroup {
    pub remote_id: String,
    pub name: String,
}

/// One contact fetched from a provider.
#[derive(Debug, Clone)]
pub struct RemoteContact {
    pub contact: Contact,
    pub remote_id: Option<String>,
    pub href: Option<String>,
    pub etag: Option<String>,
    /// `remote_id` was derived from the href because the vCard has no UID
    /// (CardDAV). It is a placeholder, not a real provider UID, and must never
    /// be matched against a real UID of another card.
    pub uid_is_derived: bool,
    /// Original vCard (CardDAV only).
    pub raw_vcard: Option<String>,
    /// Full desired group membership as names.
    pub group_names: Vec<String>,
}

/// A batch of contact changes.
#[derive(Debug, Clone, Default)]
pub struct ContactDelta {
    pub contacts: Vec<RemoteContact>,
    /// Provider ids deleted entirely.
    pub deleted_remote_ids: Vec<String>,
    /// CardDAV hrefs deleted.
    pub deleted_hrefs: Vec<String>,
    /// Complete href set from a full server listing, including objects whose
    /// ETag was unchanged and therefore omitted from `contacts`.
    ///
    /// A `full_sync` batch must treat these as present: pruning only by
    /// `contacts` would mistake "not re-downloaded" for "deleted on the
    /// server". Empty when the provider did not list the whole collection.
    pub listed_hrefs: Vec<String>,
    /// Full group list; only providers with real groups fill this in.
    pub groups: Vec<RemoteGroup>,
    /// `true` when `groups` is the complete set (stale groups are pruned).
    pub groups_full: bool,
    /// `true` when the provider also owns the favourite flag (Google's
    /// "starred" group); otherwise the local flag wins.
    pub favorites_authoritative: bool,
    pub sync_token: Option<String>,
    pub ctag: Option<String>,
    /// `true` when the provider performed a full resync.
    pub full_sync: bool,
    /// `true` when the server listed an object whose identity could not be
    /// read this round (for example a per-resource `503` without an ETag).
    ///
    /// The object is present and must not be pruned, but the sync cursor must
    /// not advance: the next pass has to retry its identity.
    pub incomplete: bool,
    /// When the request that produced this batch started.
    ///
    /// See [`crate::provider::EventDelta::window_start`]. `None` (direct
    /// unit-test calls) treats every queued or settled operation as a potential
    /// race, which is the safe default for that helper.
    pub window_start: Option<chrono::DateTime<chrono::Utc>>,
}

/// Identity needed to address a stored contact on the provider.
#[derive(Debug, Clone, Default)]
pub struct RemoteContactRef {
    pub remote_id: Option<String>,
    pub href: Option<String>,
    pub etag: Option<String>,
    pub raw_vcard: Option<String>,
    /// `remote_id` is a href-derived placeholder, not a real provider UID.
    /// A provider that rewrites the object must **not** write the placeholder
    /// into the vCard as a real `UID`: it can collide with a genuine UID of
    /// the same spelling (RFC 6352 requires UIDs to be unique per address
    /// book). The rewrite instead receives a freshly assigned real UID through
    /// `update_contact`'s `assigned_uid` and returns `uid_is_derived = false`.
    pub uid_is_derived: bool,
}

/// Metadata returned after a successful remote write.
#[derive(Debug, Clone, Default)]
pub struct RemoteWriteResult {
    pub remote_id: Option<String>,
    pub etag: Option<String>,
    pub href: Option<String>,
    /// `remote_id` is a href-derived placeholder (the written object has no
    /// real UID). Persisted so a later delete tombstone matches only the same
    /// kind of identity.
    pub uid_is_derived: bool,
}

/// Contact backend.
#[async_trait]
pub trait ContactProvider: Send + Sync {
    /// Stable provider key stored in `pim_address_book_map.provider`.
    fn name(&self) -> &'static str;

    /// `true` when the provider has real, addressable groups.
    fn supports_groups(&self) -> bool {
        false
    }

    /// `true` when group membership is managed through group endpoints
    /// (Google).  Otherwise membership is written on the contact itself
    /// (categories).  The service uses this to choose the write path.
    fn groups_via_membership(&self) -> bool {
        false
    }

    async fn list_address_books(&self, account: &Account) -> Result<Vec<RemoteAddressBook>>;

    async fn sync_contacts(
        &self,
        account: &Account,
        book: &RemoteAddressBook,
        cursor: &ContactCursor,
    ) -> Result<ContactDelta>;

    async fn create_contact(
        &self,
        account: &Account,
        book: &RemoteAddressBook,
        contact: &Contact,
        group_names: &[String],
    ) -> Result<RemoteWriteResult>;

    /// Rewrite an existing contact.
    ///
    /// `assigned_uid` is a real UID the service already persisted for a card
    /// whose stored identity was derived from its href. Providers that address
    /// objects by a real UID must write this value instead of the placeholder;
    /// providers with provider-side ids ignore it.
    async fn update_contact(
        &self,
        account: &Account,
        book: &RemoteAddressBook,
        current: &RemoteContactRef,
        contact: &Contact,
        group_names: &[String],
        assigned_uid: Option<&str>,
    ) -> Result<RemoteWriteResult>;

    async fn delete_contact(
        &self,
        account: &Account,
        book: &RemoteAddressBook,
        current: &RemoteContactRef,
    ) -> Result<()>;

    /// Complete group list (Google).  Empty for category-based providers.
    async fn list_groups(
        &self,
        _account: &Account,
        _book: &RemoteAddressBook,
    ) -> Result<Vec<RemoteGroup>> {
        Ok(Vec::new())
    }

    async fn create_group(
        &self,
        _account: &Account,
        _book: &RemoteAddressBook,
        _name: &str,
    ) -> Result<RemoteGroup> {
        Err(crate::error::unsupported(
            "this provider does not support contact groups",
        ))
    }

    async fn rename_group(
        &self,
        _account: &Account,
        _group_remote_id: &str,
        _name: &str,
    ) -> Result<()> {
        Err(crate::error::unsupported(
            "this provider does not support contact groups",
        ))
    }

    async fn delete_group(&self, _account: &Account, _group_remote_id: &str) -> Result<()> {
        Err(crate::error::unsupported(
            "this provider does not support contact groups",
        ))
    }

    /// Add/remove members of a real group (Google).
    async fn modify_group_members(
        &self,
        _account: &Account,
        _group_remote_id: &str,
        _add: &[String],
        _remove: &[String],
    ) -> Result<()> {
        Err(crate::error::unsupported(
            "this provider does not support contact groups",
        ))
    }

    /// Toggle Google's "starred" system group (used for favourites).
    async fn set_starred(
        &self,
        _account: &Account,
        _person_remote_id: &str,
        _value: bool,
    ) -> Result<()> {
        Ok(())
    }

    /// Read-only "other contacts" used for recipient suggestions.
    async fn other_contacts(&self, _account: &Account) -> Result<Vec<Address>> {
        Ok(Vec::new())
    }
}
