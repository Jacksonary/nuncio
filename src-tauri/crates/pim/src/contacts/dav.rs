//! CardDAV contact provider.
//!
//! Reuses the generic DAV client (discovery, `sync-collection`, `ctag`/ETag
//! fallback, `addressbook-multiget`) and `nuncio-ical` for vCard 2.1/3.0/4.0
//! parsing and vCard 3.0 writing.  Groups are the vCard `CATEGORIES` values.

use async_trait::async_trait;
use nuncio_core::models::{Account, Contact};
use nuncio_core::providers::DavService;
use tracing::debug;

use crate::contacts::map::sanitize_photo;
use crate::contacts::provider::{
    ContactCursor, ContactDelta, ContactProvider, RemoteAddressBook, RemoteContact,
    RemoteContactRef, RemoteWriteResult,
};
use crate::dav::{DavClient, DavKind};
use crate::error::{invalid, AppError, Result};
use crate::http::HttpClient;
use crate::provider::CalendarCursor;

const VCARD_TYPE: &str = "text/vcard; charset=utf-8";

#[derive(Clone)]
pub struct CardDavProvider {
    http: HttpClient,
}

impl std::fmt::Debug for CardDavProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CardDavProvider").finish()
    }
}

impl CardDavProvider {
    pub fn new(http: HttpClient) -> Self {
        Self { http }
    }

    fn client(&self, account: &Account) -> DavClient {
        DavClient::for_service(self.http.clone(), account.clone(), DavService::Contacts)
    }
}

fn collection_url(book: &RemoteAddressBook) -> &str {
    book.href.as_deref().unwrap_or(&book.remote_id)
}

#[async_trait]
impl ContactProvider for CardDavProvider {
    fn name(&self) -> &'static str {
        "carddav"
    }

    async fn list_address_books(&self, account: &Account) -> Result<Vec<RemoteAddressBook>> {
        let client = self.client(account);
        let books = client.list_address_books().await?;
        Ok(books
            .into_iter()
            .map(|book| RemoteAddressBook {
                local_id: String::new(),
                remote_id: book.remote_id,
                href: book.href,
                name: book.name,
                read_only: book.read_only,
            })
            .collect())
    }

    async fn sync_contacts(
        &self,
        account: &Account,
        book: &RemoteAddressBook,
        cursor: &ContactCursor,
    ) -> Result<ContactDelta> {
        let client = self.client(account);
        let dav_cursor = CalendarCursor {
            local_calendar_id: cursor.local_address_book_id.clone(),
            sync_token: cursor.sync_token.clone(),
            ctag: cursor.ctag.clone(),
            etags: cursor.etags.clone(),
        };
        let delta = client
            .sync_resources(DavKind::AddressBook, collection_url(book), &dav_cursor)
            .await?;
        let mut contacts = Vec::new();
        for resource in delta.changed {
            let parsed = nuncio_ical::vcard::parse_vcards(&resource.data);
            let Some(first) = parsed.into_iter().next() else {
                debug!(href = %resource.href, "CardDAV resource has no vCard");
                continue;
            };
            let remote_id = if first.uid.trim().is_empty() {
                resource
                    .href
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_string()
            } else {
                first.uid.clone()
            };
            let mut contact = first.contact;
            contact.photo = sanitize_photo(contact.photo);
            if contact.display_name.trim().is_empty() {
                contact.display_name = contact.label();
            }
            let mut group_names = first.group_names;
            group_names.retain(|name| !name.trim().is_empty());
            contacts.push(RemoteContact {
                contact,
                remote_id: Some(remote_id),
                href: Some(resource.href),
                etag: resource.etag,
                raw_vcard: Some(resource.data),
                group_names,
            });
        }
        Ok(ContactDelta {
            contacts,
            deleted_remote_ids: Vec::new(),
            deleted_hrefs: delta.deleted_hrefs,
            groups: Vec::new(),
            groups_full: false,
            favorites_authoritative: false,
            sync_token: delta.sync_token,
            ctag: delta.ctag,
            full_sync: delta.full_sync,
        })
    }

    async fn create_contact(
        &self,
        account: &Account,
        book: &RemoteAddressBook,
        contact: &Contact,
        group_names: &[String],
    ) -> Result<RemoteWriteResult> {
        let client = self.client(account);
        let uid = if contact.id.trim().is_empty() {
            nuncio_core::models::new_id()
        } else {
            contact.id.clone()
        };
        let url = DavClient::resource_url_for(collection_url(book), &uid, "vcf");
        let body = nuncio_ical::vcard::write_vcard(contact, group_names);
        let etag = client
            .put_resource(&url, &body, VCARD_TYPE, None, true)
            .await?;
        Ok(RemoteWriteResult {
            remote_id: Some(uid),
            etag,
            href: Some(url),
        })
    }

    async fn update_contact(
        &self,
        account: &Account,
        book: &RemoteAddressBook,
        current: &RemoteContactRef,
        contact: &Contact,
        group_names: &[String],
    ) -> Result<RemoteWriteResult> {
        let client = self.client(account);
        let uid = current
            .remote_id
            .clone()
            .ok_or_else(|| AppError::conflict("CardDAV contact has no UID"))?;
        let url = current
            .href
            .clone()
            .unwrap_or_else(|| DavClient::resource_url_for(collection_url(book), &uid, "vcf"));
        let mut outbound = contact.clone();
        // The vCard UID must stay stable across updates.
        outbound.id = uid.clone();
        let body = nuncio_ical::vcard::write_vcard(&outbound, group_names);
        let etag = client
            .put_resource(&url, &body, VCARD_TYPE, current.etag.as_deref(), false)
            .await?;
        Ok(RemoteWriteResult {
            remote_id: Some(uid),
            etag,
            href: Some(url),
        })
    }

    async fn delete_contact(
        &self,
        account: &Account,
        book: &RemoteAddressBook,
        current: &RemoteContactRef,
    ) -> Result<()> {
        let client = self.client(account);
        let url = match (&current.href, &current.remote_id) {
            (Some(href), _) => href.clone(),
            (None, Some(uid)) => DavClient::resource_url_for(collection_url(book), uid, "vcf"),
            (None, None) => return Ok(()),
        };
        client.delete_resource(&url, current.etag.as_deref()).await
    }
}

/// Parse a `.vcf` document into contacts (used by `contact_import_vcf`).
pub fn parse_vcf(path_content: &str) -> Vec<(Contact, Vec<String>)> {
    nuncio_ical::vcard::parse_vcards(path_content)
        .into_iter()
        .map(|parsed| {
            let mut contact = parsed.contact;
            if contact.display_name.trim().is_empty() {
                contact.display_name = contact.label();
            }
            (contact, parsed.group_names)
        })
        .filter(|(contact, _)| {
            !contact.display_name.trim().is_empty() || !contact.emails.is_empty()
        })
        .collect()
}

/// Render contacts as one concatenated vCard document.
pub fn write_vcf(contacts: &[Contact], groups: &[Vec<String>]) -> String {
    let mut out = String::new();
    for (index, contact) in contacts.iter().enumerate() {
        let names = groups.get(index).map(Vec::as_slice).unwrap_or(&[]);
        out.push_str(&nuncio_ical::vcard::write_vcard(contact, names));
        if !out.ends_with("\r\n") {
            out.push_str("\r\n");
        }
    }
    out
}

/// Guard used by the service when a vCard has no usable content.
pub fn require_contact(contact: &Contact) -> Result<()> {
    if contact.display_name.trim().is_empty() && contact.emails.is_empty() {
        return Err(invalid("contact needs a name or an e-mail address"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VCARD: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:card-1\r\nFN:Alan Turing\r\nN:Turing;Alan;;;\r\nEMAIL;TYPE=WORK:alan@example.com\r\nTEL;TYPE=CELL:+44 1234\r\nADR;TYPE=HOME:;;1 High St;London;;N1;UK\r\nCATEGORIES:Friends,Mathematicians\r\nNOTE:Computing pioneer\r\nEND:VCARD\r\n";

    #[test]
    fn parses_vcf_document() {
        let contacts = parse_vcf(VCARD);
        assert_eq!(contacts.len(), 1);
        let (contact, groups) = &contacts[0];
        assert_eq!(contact.display_name, "Alan Turing");
        assert_eq!(contact.primary_email().as_deref(), Some("alan@example.com"));
        assert_eq!(
            groups,
            &vec!["Friends".to_string(), "Mathematicians".to_string()]
        );
        assert_eq!(contact.addresses[0].city, "London");
    }

    #[test]
    fn round_trips_vcards() {
        let contacts = parse_vcf(VCARD);
        let (contact, groups) = &contacts[0];
        let rendered = write_vcf(std::slice::from_ref(contact), std::slice::from_ref(groups));
        let reparsed = parse_vcf(&rendered);
        assert_eq!(reparsed.len(), 1);
        assert_eq!(reparsed[0].0.display_name, "Alan Turing");
        assert_eq!(reparsed[0].1.len(), 2);
    }

    #[test]
    fn skips_empty_cards() {
        let contacts = parse_vcf("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:\r\nEND:VCARD\r\n");
        assert!(contacts.is_empty());
    }

    #[test]
    fn keeps_small_photos_only() {
        let huge = format!(
            "data:image/jpeg;base64,{}",
            "A".repeat(crate::contacts::map::MAX_PHOTO_BYTES * 2)
        );
        assert!(crate::contacts::map::sanitize_photo(Some(huge)).is_none());
        let small = "data:image/png;base64,AAAA".to_string();
        assert_eq!(
            crate::contacts::map::sanitize_photo(Some(small.clone())),
            Some(small)
        );
    }
}
