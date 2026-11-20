//! Encrypted application backup.
//!
//! File layout: `NUNCIOBK1` magic, 16-byte salt, 12-byte nonce, then the
//! AES-256-GCM ciphertext (which includes the tag). The key is derived from the
//! user password with Argon2id.
//!
//! Version 2 carries the PIM data (address books, contacts, calendars, tasks)
//! and the folder preferences that used to require separate `.vcf` / `.ics`
//! exports, so moving to a new machine is one restore.

use std::collections::{HashMap, HashSet};

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use nuncio_core::models::{
    Account, AddressBook, AppSettings, Calendar, CalendarEvent, Contact, Folder, Identity,
    MailRule, Signature, Task, TaskList,
};
use nuncio_core::repo::contacts::ContactGroupRecord;
use nuncio_core::repo::pim_maps::PimMapRecord;
use nuncio_core::AppError;

pub const MAGIC: &[u8; 9] = b"NUNCIOBK1";
pub const FORMAT: &str = "nuncio-backup";
pub const VERSION: u32 = 2;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

/// One `contact_group` row (the IPC model exposes only a derived member count).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContactGroup {
    pub id: String,
    pub address_book_id: String,
    pub account_id: String,
    pub name: String,
    pub remote_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<ContactGroupRecord> for ContactGroup {
    fn from(record: ContactGroupRecord) -> Self {
        Self {
            id: record.id,
            address_book_id: record.address_book_id,
            account_id: record.account_id,
            name: record.name,
            remote_id: record.remote_id,
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

impl From<ContactGroup> for ContactGroupRecord {
    fn from(group: ContactGroup) -> Self {
        Self {
            id: group.id,
            address_book_id: group.address_book_id,
            account_id: group.account_id,
            name: group.name,
            remote_id: group.remote_id,
            created_at: group.created_at,
            updated_at: group.updated_at,
        }
    }
}

/// Provider mapping for a calendar (`kind = "calendar"`), address book
/// (`"books"`) or task list (`"tasks"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PimMap {
    pub kind: String,
    pub local_id: String,
    pub account_id: String,
    pub provider: String,
    pub remote_id: String,
    pub href: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<PimMapRecord> for PimMap {
    fn from(record: PimMapRecord) -> Self {
        Self {
            kind: record.kind,
            local_id: record.local_id,
            account_id: record.account_id,
            provider: record.provider,
            remote_id: record.remote_id,
            href: record.href,
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

impl From<PimMap> for PimMapRecord {
    fn from(map: PimMap) -> Self {
        Self {
            kind: map.kind,
            local_id: map.local_id,
            account_id: map.account_id,
            provider: map.provider,
            remote_id: map.remote_id,
            href: map.href,
            created_at: map.created_at,
            updated_at: map.updated_at,
        }
    }
}

/// Decrypted backup document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupDocument {
    pub format: String,
    pub version: u32,
    pub exported_at: String,
    pub settings: AppSettings,
    pub accounts: Vec<Account>,
    pub identities: Vec<Identity>,
    pub signatures: Vec<Signature>,
    pub rules: Vec<MailRule>,
    pub address_books: Vec<AddressBook>,
    pub contact_groups: Vec<ContactGroup>,
    pub contacts: Vec<Contact>,
    pub calendars: Vec<Calendar>,
    pub events: Vec<BackupEvent>,
    pub task_lists: Vec<TaskList>,
    pub tasks: Vec<Task>,
    /// Preference carriers: stars, sync toggles and sort order. Volatile
    /// counters are zeroed before export (see [`folder_preferences`]).
    pub folders: Vec<Folder>,
    pub pim_maps: Vec<PimMap>,
    /// `account:<id>:<suffix>` -> secret. Present only when requested.
    #[serde(default)]
    pub secrets: Option<HashMap<String, String>>,
}

impl BackupDocument {
    pub fn new(
        settings: AppSettings,
        accounts: Vec<Account>,
        identities: Vec<Identity>,
        signatures: Vec<Signature>,
        rules: Vec<MailRule>,
    ) -> Self {
        Self {
            format: FORMAT.to_string(),
            version: VERSION,
            exported_at: nuncio_core::models::now().to_rfc3339(),
            settings,
            accounts,
            identities,
            signatures,
            rules,
            address_books: Vec::new(),
            contact_groups: Vec::new(),
            contacts: Vec::new(),
            calendars: Vec::new(),
            events: Vec::new(),
            task_lists: Vec::new(),
            tasks: Vec::new(),
            folders: Vec::new(),
            pim_maps: Vec::new(),
            secrets: None,
        }
    }

    pub fn validate(&self) -> nuncio_core::Result<()> {
        if self.format != FORMAT {
            return Err(AppError::invalid_input("this file is not a Nuncio backup"));
        }
        if self.version != VERSION {
            return Err(AppError::invalid_input(format!(
                "unsupported backup version {}",
                self.version
            )));
        }
        Ok(())
    }
}

/// Copy a folder leaving out everything the receiving device recomputes.
///
/// `unread_count`, `total_count` and `last_sync_at` describe this machine's
/// cache and sync cursor. Carrying them over would show wrong badges on the new
/// device and could suppress the first sync. The IMAP uid state
/// (`uidvalidity` / `uidnext` / `highest_modseq`) is the same kind of state; it
/// is not part of the `Folder` model, and [`nuncio_core::repo::folders::restore`]
/// never writes it. Only `is_favorite`, `sync_enabled` and `sort_order` survive
/// as the user's preferences.
pub fn folder_preferences(folder: &Folder) -> Folder {
    Folder {
        unread_count: 0,
        total_count: 0,
        last_sync_at: None,
        ..folder.clone()
    }
}

/// The rows of a document that may actually be written to this database.
///
/// A calendar event plus the stored fields the event model does not carry.
///
/// `exdates` are the occurrences of a recurring event the user deleted, and
/// `rrule` is the stored recurrence text. Both are user data: without them a
/// restored local calendar brings deleted occurrences back. Sync metadata
/// (`etag`, `href`, `remote_id`, `raw_ics`) is deliberately left out; it is
/// device state that the first sync rebuilds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupEvent {
    #[serde(flatten)]
    pub event: CalendarEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rrule: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exdates: Vec<String>,
}

/// A row is dropped when its owning account was not restored, or when the
/// parent it points at (address book, calendar, task list) was dropped. This is
/// what keeps one corrupt entry in a backup from aborting the whole restore.
#[derive(Debug, Clone, Default)]
pub struct RestorePlan {
    pub address_books: Vec<AddressBook>,
    pub contact_groups: Vec<ContactGroup>,
    pub contacts: Vec<Contact>,
    pub calendars: Vec<Calendar>,
    pub events: Vec<BackupEvent>,
    pub task_lists: Vec<TaskList>,
    pub tasks: Vec<Task>,
    pub folders: Vec<Folder>,
    pub pim_maps: Vec<PimMap>,
}

impl RestorePlan {
    pub fn from_document(document: &BackupDocument, accounts: &HashSet<String>) -> Self {
        let address_books: Vec<AddressBook> = document
            .address_books
            .iter()
            .filter(|book| accounts.contains(&book.account_id))
            .cloned()
            .collect();
        let book_ids: HashSet<&str> = address_books.iter().map(|book| book.id.as_str()).collect();

        let contact_groups: Vec<ContactGroup> = document
            .contact_groups
            .iter()
            .filter(|group| {
                accounts.contains(&group.account_id)
                    && book_ids.contains(group.address_book_id.as_str())
            })
            .cloned()
            .collect();
        let group_ids: HashSet<&str> = contact_groups
            .iter()
            .map(|group| group.id.as_str())
            .collect();

        let contacts: Vec<Contact> = document
            .contacts
            .iter()
            .filter(|contact| {
                accounts.contains(&contact.account_id)
                    && book_ids.contains(contact.address_book_id.as_str())
            })
            .map(|contact| {
                let mut contact = contact.clone();
                contact
                    .group_ids
                    .retain(|group_id| group_ids.contains(group_id.as_str()));
                contact
            })
            .collect();

        let calendars: Vec<Calendar> = document
            .calendars
            .iter()
            .filter(|calendar| accounts.contains(&calendar.account_id))
            .cloned()
            .collect();
        let calendar_ids: HashSet<&str> = calendars
            .iter()
            .map(|calendar| calendar.id.as_str())
            .collect();

        let events: Vec<BackupEvent> = document
            .events
            .iter()
            .filter(|backup| {
                accounts.contains(&backup.event.account_id)
                    && calendar_ids.contains(backup.event.calendar_id.as_str())
            })
            .cloned()
            .collect();

        let task_lists: Vec<TaskList> = document
            .task_lists
            .iter()
            .filter(|list| accounts.contains(&list.account_id))
            .cloned()
            .collect();
        let list_ids: HashSet<&str> = task_lists.iter().map(|list| list.id.as_str()).collect();

        let tasks: Vec<Task> = document
            .tasks
            .iter()
            .filter(|task| {
                accounts.contains(&task.account_id) && list_ids.contains(task.list_id.as_str())
            })
            .cloned()
            .collect();

        let folders: Vec<Folder> = document
            .folders
            .iter()
            .filter(|folder| accounts.contains(&folder.account_id))
            .map(folder_preferences)
            .collect();

        let pim_maps: Vec<PimMap> = document
            .pim_maps
            .iter()
            .filter(|map| {
                if !accounts.contains(&map.account_id) {
                    return false;
                }
                match map.kind.as_str() {
                    "calendar" => calendar_ids.contains(map.local_id.as_str()),
                    "books" => book_ids.contains(map.local_id.as_str()),
                    "tasks" => list_ids.contains(map.local_id.as_str()),
                    _ => false,
                }
            })
            .cloned()
            .collect();

        Self {
            address_books,
            contact_groups,
            contacts,
            calendars,
            events,
            task_lists,
            tasks,
            folders,
            pim_maps,
        }
    }
}

fn derive_key(password: &str, salt: &[u8]) -> nuncio_core::Result<[u8; KEY_LEN]> {
    if password.is_empty() {
        return Err(AppError::invalid_input("the backup password is required"));
    }
    let params = Params::new(65_536, 3, 1, Some(KEY_LEN))
        .map_err(|error| AppError::internal(format!("argon2 parameters: {error}")))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; KEY_LEN];
    argon
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|error| AppError::internal(format!("key derivation failed: {error}")))?;
    Ok(key)
}

/// Serialize and encrypt a backup document.
pub fn encrypt(document: &BackupDocument, password: &str) -> nuncio_core::Result<Vec<u8>> {
    let plain = serde_json::to_vec(document).map_err(AppError::from)?;
    let mut salt = [0u8; SALT_LEN];
    let mut nonce_bytes = [0u8; NONCE_LEN];
    let mut rng = rand::rngs::OsRng;
    rng.fill_bytes(&mut salt);
    rng.fill_bytes(&mut nonce_bytes);

    let key = derive_key(password, &salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|error| AppError::internal(format!("cipher setup failed: {error}")))?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plain.as_ref())
        .map_err(|error| AppError::internal(format!("encryption failed: {error}")))?;

    let mut out = Vec::with_capacity(MAGIC.len() + SALT_LEN + NONCE_LEN + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypt and parse a backup file.
pub fn decrypt(bytes: &[u8], password: &str) -> nuncio_core::Result<BackupDocument> {
    let header = MAGIC.len() + SALT_LEN + NONCE_LEN;
    if bytes.len() <= header || &bytes[..MAGIC.len()] != MAGIC {
        return Err(AppError::invalid_input(
            "this file is not a Nuncio backup (bad header)",
        ));
    }
    let salt = &bytes[MAGIC.len()..MAGIC.len() + SALT_LEN];
    let nonce_bytes = &bytes[MAGIC.len() + SALT_LEN..header];
    let ciphertext = &bytes[header..];

    let key = derive_key(password, salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|error| AppError::internal(format!("cipher setup failed: {error}")))?;
    let nonce = Nonce::from_slice(nonce_bytes);
    let plain = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| AppError::invalid_input("wrong password or corrupted backup data"))?;
    let document: BackupDocument = serde_json::from_slice(&plain)
        .map_err(|_| AppError::invalid_input("wrong password or corrupted backup data"))?;
    document.validate()?;
    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::models::{Contact, ContactField, ContactFieldType, FolderRole};

    fn document() -> BackupDocument {
        BackupDocument::new(
            AppSettings::default(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    }

    fn stamp(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .expect("timestamp")
            .with_timezone(&Utc)
    }

    fn sample_document() -> BackupDocument {
        let mut document = document();

        document.address_books.push(AddressBook {
            id: "book-1".into(),
            account_id: "acc-1".into(),
            name: "Work".into(),
            is_read_only: true,
            is_default: true,
        });

        document.contact_groups.push(ContactGroup {
            id: "group-1".into(),
            address_book_id: "book-1".into(),
            account_id: "acc-1".into(),
            name: "Team".into(),
            remote_id: Some("remote-group".into()),
            created_at: stamp("2026-01-02T03:04:05Z"),
            updated_at: stamp("2026-02-03T04:05:06Z"),
        });

        let mut contact = Contact::new("contact-1", "book-1", "acc-1");
        contact.display_name = "Ada Lovelace".into();
        contact.emails.push(ContactField {
            r#type: ContactFieldType::Work,
            value: "ada@example.com".into(),
            is_primary: true,
        });
        contact.group_ids = vec!["group-1".into()];
        contact.is_favorite = true;
        contact.updated_at = Some(stamp("2026-03-04T05:06:07Z"));
        document.contacts.push(contact);

        document.calendars.push(Calendar {
            id: "cal-1".into(),
            account_id: "acc-1".into(),
            name: "Personal".into(),
            color: "#123456".into(),
            is_read_only: false,
            is_visible: true,
            is_default: true,
            subscription_url: None,
        });

        let mut event = CalendarEvent::new(
            "event-1",
            "cal-1",
            "acc-1",
            "uid-1",
            "Standup",
            "2026-04-01T09:00:00Z",
            "2026-04-01T09:30:00Z",
        );
        event.location = "Room 1".into();
        event.reminders = vec![10, 30];
        event.updated_at = Some(stamp("2026-04-01T08:00:00Z"));
        document.events.push(BackupEvent {
            event,
            rrule: Some("FREQ=WEEKLY;BYDAY=MO".into()),
            exdates: vec!["2026-04-08T09:00:00Z".into()],
        });

        document.task_lists.push(TaskList {
            id: "list-1".into(),
            account_id: "acc-1".into(),
            name: "Errands".into(),
            color: "#654321".into(),
            is_default: true,
            is_read_only: false,
            well_known_list_name: "none".into(),
            incomplete_count: 3,
            sort_order: 2,
        });

        let task = Task {
            id: "task-1".into(),
            list_id: "list-1".into(),
            account_id: "acc-1".into(),
            title: "Buy milk".into(),
            notes: "Two litres".into(),
            status: nuncio_core::models::TaskStatus::NotStarted,
            importance: nuncio_core::models::Importance::High,
            due_date: Some("2026-05-01".into()),
            reminder_at: Some(stamp("2026-05-01T08:00:00Z")),
            recurrence: None,
            my_day_date: Some("2026-05-01".into()),
            completed_at: None,
            created_at: stamp("2026-04-01T00:00:00Z"),
            updated_at: stamp("2026-04-02T00:00:00Z"),
            steps: vec![nuncio_core::models::TaskStep {
                id: "step-1".into(),
                title: "Check the fridge".into(),
                is_completed: false,
                sort_order: 0,
            }],
            sort_order: 1,
            linked_message_id: Some("message-1".into()),
        };
        document.tasks.push(task);

        let mut folder = Folder::new("folder-1", "acc-1", "Inbox", FolderRole::Inbox);
        folder.is_favorite = true;
        folder.sync_enabled = false;
        folder.sort_order = 7;
        document.folders.push(folder);

        document.pim_maps.push(PimMap {
            kind: "calendar".into(),
            local_id: "cal-1".into(),
            account_id: "acc-1".into(),
            provider: "caldav".into(),
            remote_id: "remote-cal".into(),
            href: Some("https://dav/cal/".into()),
            created_at: stamp("2026-01-01T00:00:00Z"),
            updated_at: stamp("2026-01-02T00:00:00Z"),
        });

        document
    }

    #[test]
    fn v2_round_trip_preserves_every_section() {
        let original = sample_document();
        let encrypted = encrypt(&original, "correct horse").expect("encrypt");
        assert_eq!(&encrypted[..MAGIC.len()], MAGIC);
        let restored = decrypt(&encrypted, "correct horse").expect("decrypt");
        assert_eq!(restored.format, FORMAT);
        assert_eq!(restored.version, VERSION);
        assert_eq!(restored.address_books, original.address_books);
        assert_eq!(restored.contact_groups, original.contact_groups);
        assert_eq!(restored.contacts, original.contacts);
        assert_eq!(restored.calendars, original.calendars);
        assert_eq!(restored.events, original.events);
        assert_eq!(restored.task_lists, original.task_lists);
        assert_eq!(restored.tasks, original.tasks);
        assert_eq!(restored.folders, original.folders);
        assert_eq!(restored.pim_maps, original.pim_maps);
    }

    #[test]
    fn wrong_password_fails() {
        let encrypted = encrypt(&document(), "one").expect("encrypt");
        assert!(decrypt(&encrypted, "two").is_err());
    }

    #[test]
    fn rejects_non_backup_files() {
        assert!(decrypt(b"hello world", "x").is_err());
    }

    #[test]
    fn validate_rejects_other_versions() {
        for version in [1, 3] {
            let mut document = document();
            document.version = version;
            assert!(document.validate().is_err(), "version {version} must fail");
        }
        assert!(document().validate().is_ok());
    }

    #[test]
    fn folder_preferences_drop_volatile_state() {
        let mut folder = Folder::new("f", "a", "Inbox", FolderRole::Inbox);
        folder.unread_count = 12;
        folder.total_count = 99;
        folder.last_sync_at = Some(stamp("2026-01-01T00:00:00Z"));
        folder.is_favorite = true;
        folder.sort_order = 4;

        let restored = folder_preferences(&folder);
        assert_eq!(restored.unread_count, 0);
        assert_eq!(restored.total_count, 0);
        assert!(restored.last_sync_at.is_none());
        assert!(restored.is_favorite);
        assert_eq!(restored.sort_order, 4);
    }

    #[test]
    fn restore_plan_skips_orphans() {
        let mut document = sample_document();
        // A known contact in an address book that is missing from the document.
        let mut orphan_contact = Contact::new("orphan-contact", "missing-book", "acc-1");
        orphan_contact.display_name = "Orphan".into();
        document.contacts.push(orphan_contact);
        // A row owned by an account that was not restored.
        let mut foreign_calendar = document.calendars[0].clone();
        foreign_calendar.id = "foreign-cal".into();
        foreign_calendar.account_id = "unknown-account".into();
        document.calendars.push(foreign_calendar);
        // A map pointing at a local id that survived nowhere.
        document.pim_maps.push(PimMap {
            kind: "calendar".into(),
            local_id: "missing-cal".into(),
            account_id: "acc-1".into(),
            provider: "caldav".into(),
            remote_id: "remote".into(),
            href: None,
            created_at: stamp("2026-01-01T00:00:00Z"),
            updated_at: stamp("2026-01-01T00:00:00Z"),
        });

        let accounts: HashSet<String> = ["acc-1".to_string()].into_iter().collect();
        let plan = RestorePlan::from_document(&document, &accounts);

        assert_eq!(plan.calendars.len(), 1);
        assert_eq!(plan.contacts.len(), 1);
        assert_eq!(plan.pim_maps.len(), 1);
        assert_eq!(plan.address_books.len(), 1);
        assert_eq!(plan.events.len(), 1);
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.folders.len(), 1);
    }
}
