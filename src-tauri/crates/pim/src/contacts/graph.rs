//! Microsoft Graph contacts provider (`/me/contacts` + `/me/contactFolders`).
//!
//! Sync uses `contacts/delta` per folder, following `@odata.nextLink` until the
//! `@odata.deltaLink` is returned.  Graph has no real contact groups; categories
//! (`categories`) are surfaced as groups, matching the task/spec decision.

use async_trait::async_trait;
use nuncio_core::models::{Account, Contact, ContactFieldType, PostalAddress};
use nuncio_core::traits::TokenResource;
use serde::Deserialize;
use tracing::debug;

use crate::contacts::map::{
    birthday, contact_field, decode_photo, display_name_from_parts, ensure_primary,
    fields_from_values, parse_birthday, photo_data_url, single_primary, MAX_PHOTO_BYTES,
};
use crate::contacts::provider::{
    ContactCursor, ContactDelta, ContactProvider, RemoteAddressBook, RemoteContact,
    RemoteContactRef, RemoteWriteResult,
};
use crate::error::{invalid, AppError, Result};
use crate::graph::GRAPH_BASE;
use crate::http::HttpClient;

/// Default (unnamed) contact folder.  Graph addresses it as `/me/contacts`.
pub const DEFAULT_FOLDER: &str = "contacts";

// `CONTACT_SELECT` was removed with the `$select` on the delta request: change
// tracking over 'Contacts' rejects it, and the delta feed returns the full
// contact shape anyway.

/// Photos downloaded per sync pass (keeps large address books light).
const PHOTO_BUDGET: usize = 100;

fn encode(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

/// Low-level Graph contacts client.
#[derive(Clone)]
pub struct GraphContactsClient {
    http: HttpClient,
    base: String,
}

impl std::fmt::Debug for GraphContactsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphContactsClient")
            .field("base", &self.base)
            .finish()
    }
}

impl GraphContactsClient {
    pub fn new(http: HttpClient) -> Self {
        Self {
            http,
            base: GRAPH_BASE.to_string(),
        }
    }

    pub fn with_base(http: HttpClient, base: impl Into<String>) -> Self {
        Self {
            http,
            base: base.into().trim_end_matches('/').to_string(),
        }
    }

    pub fn http(&self) -> &HttpClient {
        &self.http
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    async fn failure(response: reqwest::Response) -> AppError {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        crate::http::status_error(status, &body)
    }

    fn folder_path(folder: &str) -> String {
        if folder == DEFAULT_FOLDER {
            "/me/contacts".to_string()
        } else {
            format!("/me/contactFolders/{}/contacts", encode(folder))
        }
    }

    pub async fn list_folders(&self, account_id: &str) -> Result<Vec<GraphContactFolder>> {
        let mut folders = Vec::new();
        let mut next: Option<String> = Some(self.url("/me/contactFolders?$top=100"));
        let mut guard = 0;
        while let Some(url) = next.take() {
            guard += 1;
            if guard > 50 {
                break;
            }
            let response = self
                .http
                .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                    self.http.raw().get(&url).bearer_auth(token)
                })
                .await?;
            if !response.status().is_success() {
                return Err(Self::failure(response).await);
            }
            let page: GraphContactFolderPage = response
                .json()
                .await
                .map_err(|error| invalid(format!("invalid Graph contactFolders JSON: {error}")))?;
            folders.extend(page.value);
            next = page.next_link;
        }
        Ok(folders)
    }

    /// Fetch one delta page.
    pub async fn contacts_delta(
        &self,
        account_id: &str,
        folder: &str,
        delta_link: Option<&str>,
    ) -> Result<(Vec<GraphContact>, Option<String>, bool)> {
        let mut next = Some(match delta_link {
            Some(link) => link.to_string(),
            None => self.url(&format!(
                // Change tracking over 'Contacts' rejects `$select` and `$top`
                // outright ("The following parameters are not supported with
                // change tracking over the 'Contacts' resource"). The delta feed
                // always returns the full shape, and page size is requested with
                // `Prefer: odata.maxpagesize` on the request itself.
                "{}/delta",
                Self::folder_path(folder)
            )),
        });
        let mut contacts = Vec::new();
        let mut delta: Option<String> = None;
        let mut guard = 0;
        while let Some(url) = next.take() {
            guard += 1;
            if guard > 200 {
                return Err(invalid("Graph contacts delta did not terminate"));
            }
            let response = self
                .http
                .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                    self.http
                        .raw()
                        .get(&url)
                        .bearer_auth(token)
                        .header("Prefer", "odata.maxpagesize=100")
                })
                .await?;
            if !response.status().is_success() {
                return Err(Self::failure(response).await);
            }
            let page: GraphContactPage = response
                .json()
                .await
                .map_err(|error| invalid(format!("invalid Graph contacts delta JSON: {error}")))?;
            contacts.extend(page.value);
            if let Some(link) = page.next_link {
                next = Some(link);
                continue;
            }
            delta = page.delta_link;
            break;
        }
        Ok((contacts, delta, delta_link.is_none()))
    }

    pub async fn create_contact(
        &self,
        account_id: &str,
        folder: &str,
        body: &serde_json::Value,
    ) -> Result<GraphContact> {
        let url = self.url(&Self::folder_path(folder));
        let response = self
            .http
            .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                self.http.raw().post(&url).bearer_auth(token).json(body)
            })
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid Graph createContact JSON: {error}")))
    }

    pub async fn update_contact(
        &self,
        account_id: &str,
        folder: &str,
        id: &str,
        etag: Option<&str>,
        body: &serde_json::Value,
    ) -> Result<GraphContact> {
        let url = self.url(&format!("{}/{}", Self::folder_path(folder), encode(id)));
        let response = self
            .http
            .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                let request = self.http.raw().patch(&url).bearer_auth(token).json(body);
                match etag {
                    Some(etag) if !etag.trim().is_empty() => request.header("If-Match", etag),
                    _ => request,
                }
            })
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid Graph updateContact JSON: {error}")))
    }

    pub async fn delete_contact(
        &self,
        account_id: &str,
        folder: &str,
        id: &str,
        etag: Option<&str>,
    ) -> Result<()> {
        let url = self.url(&format!("{}/{}", Self::folder_path(folder), encode(id)));
        let response = self
            .http
            .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                let request = self.http.raw().delete(&url).bearer_auth(token);
                match etag {
                    Some(etag) if !etag.trim().is_empty() => request.header("If-Match", etag),
                    _ => request,
                }
            })
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        HttpClient::ensure_success(response).await
    }

    /// Download a contact photo as a small `data:` URL (`photo/$value`).
    pub async fn fetch_photo(&self, account_id: &str, folder: &str, id: &str) -> Option<String> {
        let url = self.url(&format!(
            "{}/{}/photo/$value",
            Self::folder_path(folder),
            encode(id)
        ));
        let response = self
            .http
            .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                self.http.raw().get(&url).bearer_auth(token)
            })
            .await
            .ok()?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            // 404 is the normal "this contact has no photo" response.
            return None;
        }
        if !response.status().is_success() {
            debug!(
                status = %response.status(),
                "contact photo download failed"
            );
            return None;
        }
        if response
            .content_length()
            .is_some_and(|size| size as usize > MAX_PHOTO_BYTES)
        {
            debug!("skipping oversized contact photo");
            return None;
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("image/jpeg")
            .to_string();
        let bytes = response.bytes().await.ok()?;
        if bytes.len() > MAX_PHOTO_BYTES {
            debug!(size = bytes.len(), "skipping oversized contact photo");
            return None;
        }
        photo_data_url(&content_type, &bytes)
    }

    /// Upload a contact photo (`PUT photo/$value`).
    pub async fn put_photo(
        &self,
        account_id: &str,
        folder: &str,
        id: &str,
        mime: &str,
        bytes: Vec<u8>,
    ) -> Result<()> {
        let url = self.url(&format!(
            "{}/{}/photo/$value",
            Self::folder_path(folder),
            encode(id)
        ));
        let response = self
            .http
            .send_authed(account_id, TokenResource::MicrosoftGraph, |token| {
                self.http
                    .raw()
                    .put(&url)
                    .bearer_auth(token)
                    .header(reqwest::header::CONTENT_TYPE, mime.to_string())
                    .body(bytes.clone())
            })
            .await?;
        HttpClient::ensure_success(response).await
    }
}

/* ──────────────────────────── JSON shapes ──────────────────────────── */

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphContactFolderPage {
    #[serde(default)]
    pub value: Vec<GraphContactFolder>,
    #[serde(default, rename = "@odata.nextLink")]
    pub next_link: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphContactFolder {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub display_name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphContactPage {
    #[serde(default)]
    pub value: Vec<GraphContact>,
    #[serde(default, rename = "@odata.nextLink")]
    pub next_link: Option<String>,
    #[serde(default, rename = "@odata.deltaLink")]
    pub delta_link: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphEmailAddress {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphPhysicalAddress {
    #[serde(default)]
    pub street: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub postal_code: Option<String>,
    #[serde(default)]
    pub country_or_region: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphRemoved {
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphContact {
    #[serde(default)]
    pub id: String,
    #[serde(default, rename = "@odata.etag")]
    pub etag: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub given_name: Option<String>,
    #[serde(default)]
    pub surname: Option<String>,
    #[serde(default)]
    pub middle_name: Option<String>,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub company_name: Option<String>,
    #[serde(default)]
    pub job_title: Option<String>,
    #[serde(default)]
    pub department: Option<String>,
    #[serde(default)]
    pub email_addresses: Vec<GraphEmailAddress>,
    #[serde(default)]
    pub business_phones: Vec<String>,
    #[serde(default)]
    pub home_phones: Vec<String>,
    #[serde(default)]
    pub mobile_phone: Option<String>,
    #[serde(default)]
    pub business_home_page: Option<String>,
    #[serde(default)]
    pub home_address: Option<GraphPhysicalAddress>,
    #[serde(default)]
    pub business_address: Option<GraphPhysicalAddress>,
    #[serde(default)]
    pub other_address: Option<GraphPhysicalAddress>,
    #[serde(default)]
    pub birthday: Option<String>,
    #[serde(default)]
    pub personal_notes: Option<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub change_key: Option<String>,
    #[serde(default, rename = "@removed")]
    pub removed: Option<GraphRemoved>,
}

impl GraphContact {
    pub fn is_removed(&self) -> bool {
        self.removed.is_some()
    }
}

/* ───────────────────────────── mapping ────────────────────────────── */

fn address_with(
    address: Option<&GraphPhysicalAddress>,
    kind: ContactFieldType,
) -> Option<PostalAddress> {
    let address = address?;
    let postal = PostalAddress {
        r#type: kind,
        street: address.street.clone().unwrap_or_default(),
        city: address.city.clone().unwrap_or_default(),
        region: address.state.clone().unwrap_or_default(),
        postal_code: address.postal_code.clone().unwrap_or_default(),
        country: address.country_or_region.clone().unwrap_or_default(),
    };
    if postal.street.is_empty()
        && postal.city.is_empty()
        && postal.region.is_empty()
        && postal.postal_code.is_empty()
        && postal.country.is_empty()
    {
        return None;
    }
    Some(postal)
}

/// Convert a Graph contact into the contract [`Contact`].
pub fn contact_from_graph(item: &GraphContact) -> Contact {
    let mut contact = Contact::new("", "", "");
    contact.display_name = item.display_name.clone().unwrap_or_default();
    contact.given_name = item.given_name.clone().unwrap_or_default();
    contact.family_name = item.surname.clone().unwrap_or_default();
    contact.middle_name = item.middle_name.clone().unwrap_or_default();
    if contact.display_name.trim().is_empty() {
        contact.display_name = display_name_from_parts(&contact.given_name, &contact.family_name);
    }
    contact.nickname = item.nickname.clone().unwrap_or_default();
    contact.company = item.company_name.clone().unwrap_or_default();
    contact.job_title = item.job_title.clone().unwrap_or_default();
    contact.department = item.department.clone().unwrap_or_default();

    let mut emails: Vec<_> = item
        .email_addresses
        .iter()
        .filter_map(|email| {
            let value = email.address.as_deref()?.trim();
            if value.is_empty() {
                return None;
            }
            Some(contact_field(value, "other", false))
        })
        .collect();
    if let Some(first) = emails.first_mut() {
        first.is_primary = true;
    }
    contact.emails = ensure_primary(emails);

    let mut phones = fields_from_values(&item.business_phones, "work");
    let home = fields_from_values(&item.home_phones, "home");
    if !phones.is_empty() {
        phones.extend(home);
    } else {
        phones = home;
    }
    if contacts_mobile(&item.mobile_phone) {
        let primary = phones.is_empty();
        phones.insert(
            0,
            contact_field(
                item.mobile_phone.clone().unwrap_or_default(),
                "mobile",
                primary,
            ),
        );
    }
    contact.phones = single_primary(phones);

    contact.addresses = [
        address_with(item.home_address.as_ref(), ContactFieldType::Home),
        address_with(item.business_address.as_ref(), ContactFieldType::Work),
        address_with(item.other_address.as_ref(), ContactFieldType::Other),
    ]
    .into_iter()
    .flatten()
    .collect();

    if let Some(url) = item
        .business_home_page
        .clone()
        .filter(|url| !url.trim().is_empty())
    {
        contact.websites = vec![url];
    }
    contact.birthday = item
        .birthday
        .as_deref()
        .filter(|value| !value.trim_start().starts_with("0001-"))
        .and_then(parse_birthday);
    contact.notes = item.personal_notes.clone().unwrap_or_default();
    contact
}

fn contacts_mobile(value: &Option<String>) -> bool {
    value
        .as_deref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

fn address_body(address: &PostalAddress) -> serde_json::Value {
    serde_json::json!({
        "street": address.street,
        "city": address.city,
        "state": address.region,
        "postalCode": address.postal_code,
        "countryOrRegion": address.country,
    })
}

/// Build the Graph contact body shared by create and update.
pub fn contact_body(contact: &Contact, group_names: &[String]) -> serde_json::Value {
    let display = if contact.display_name.trim().is_empty() {
        contact.label()
    } else {
        contact.display_name.clone()
    };
    let home = contact
        .addresses
        .iter()
        .find(|address| address.r#type == ContactFieldType::Home);
    let work = contact
        .addresses
        .iter()
        .find(|address| address.r#type == ContactFieldType::Work);
    let other = contact
        .addresses
        .iter()
        .find(|address| address.r#type == ContactFieldType::Other);
    let business_phones: Vec<&String> = contact
        .phones
        .iter()
        .filter(|phone| phone.r#type == ContactFieldType::Work)
        .map(|phone| &phone.value)
        .collect();
    let home_phones: Vec<&String> = contact
        .phones
        .iter()
        .filter(|phone| phone.r#type == ContactFieldType::Home)
        .map(|phone| &phone.value)
        .collect();
    let mobile = contact
        .phones
        .iter()
        .find(|phone| phone.r#type == ContactFieldType::Mobile)
        .map(|phone| phone.value.clone())
        .unwrap_or_default();
    let mut body = serde_json::json!({
        "displayName": display,
        "givenName": contact.given_name,
        "surname": contact.family_name,
        "middleName": contact.middle_name,
        "nickname": contact.nickname,
        "companyName": contact.company,
        "jobTitle": contact.job_title,
        "department": contact.department,
        "emailAddresses": contact.emails.iter().map(|email| {
            serde_json::json!({ "address": email.value, "name": display })
        }).collect::<Vec<_>>(),
        "businessPhones": business_phones,
        "homePhones": home_phones,
        "mobilePhone": mobile,
        "businessHomePage": contact.websites.first().cloned().unwrap_or_default(),
        "personalNotes": contact.notes,
        "categories": group_names,
    });
    if let Some(address) = home {
        body["homeAddress"] = address_body(address);
    }
    if let Some(address) = work {
        body["businessAddress"] = address_body(address);
    }
    if let Some(address) = other {
        body["otherAddress"] = address_body(address);
    }
    if let Some(birthday) = contact.birthday.as_deref().and_then(graph_birthday) {
        body["birthday"] = serde_json::json!(birthday);
    }
    body
}

fn graph_birthday(value: &str) -> Option<String> {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix("--") {
        // Graph cannot express "year unknown"; encode it in the leap-safe year
        // 1904 so the month/day survive a round trip.
        let (month, day) = rest.split_once('-')?;
        let month: u32 = month.parse().ok()?;
        let day: u32 = day.parse().ok()?;
        birthday(None, month, day)?;
        return Some(format!("1904-{month:02}-{day:02}T00:00:00Z"));
    }
    let parsed = parse_birthday(value)?;
    Some(format!("{parsed}T00:00:00Z"))
}

/* ──────────────────────────── provider ────────────────────────────── */

#[derive(Clone)]
pub struct GraphContactsProvider {
    client: GraphContactsClient,
}

impl std::fmt::Debug for GraphContactsProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphContactsProvider").finish()
    }
}

impl GraphContactsProvider {
    pub fn new(http: HttpClient) -> Self {
        Self {
            client: GraphContactsClient::new(http),
        }
    }

    pub fn with_base(http: HttpClient, base: impl Into<String>) -> Self {
        Self {
            client: GraphContactsClient::with_base(http, base),
        }
    }

    pub fn client(&self) -> &GraphContactsClient {
        &self.client
    }
}

#[async_trait]
impl ContactProvider for GraphContactsProvider {
    fn name(&self) -> &'static str {
        "graph"
    }

    async fn list_address_books(&self, account: &Account) -> Result<Vec<RemoteAddressBook>> {
        let mut books = vec![RemoteAddressBook::new(DEFAULT_FOLDER, "联系人")];
        for folder in self.client.list_folders(&account.id).await? {
            if folder.id.trim().is_empty() {
                continue;
            }
            let name = if folder.display_name.trim().is_empty() {
                "联系人".to_string()
            } else {
                folder.display_name.clone()
            };
            books.push(RemoteAddressBook::new(folder.id, name));
        }
        Ok(books)
    }

    async fn sync_contacts(
        &self,
        account: &Account,
        book: &RemoteAddressBook,
        cursor: &ContactCursor,
    ) -> Result<ContactDelta> {
        let (items, delta_link, full_sync) = match self
            .client
            .contacts_delta(&account.id, &book.remote_id, cursor.sync_token.as_deref())
            .await
        {
            Ok(result) => result,
            Err(error @ (AppError::NotFound(_) | AppError::InvalidInput(_)))
                if cursor.sync_token.is_some() =>
            {
                debug!(%error, "Graph contacts delta token expired; resyncing folder");
                self.client
                    .contacts_delta(&account.id, &book.remote_id, None)
                    .await?
            }
            Err(error) => return Err(error),
        };

        let mut contacts = Vec::new();
        let mut deleted_remote_ids = Vec::new();
        let mut photo_budget = PHOTO_BUDGET;
        for item in &items {
            if item.is_removed() {
                if !item.id.is_empty() {
                    deleted_remote_ids.push(item.id.clone());
                }
                continue;
            }
            if item.id.trim().is_empty() {
                continue;
            }
            let mut contact = contact_from_graph(item);
            if contact.photo.is_none() && photo_budget > 0 {
                // Graph never returns the photo inline, so a delta contact
                // lacks one until it is downloaded.  Count requests against
                // the budget (not only successes) so address books where most
                // contacts have no photo stay bounded per pass.
                photo_budget -= 1;
                if let Some(photo) = self
                    .client
                    .fetch_photo(&account.id, &book.remote_id, &item.id)
                    .await
                {
                    contact.photo = Some(photo);
                }
            }
            let mut group_names = item.categories.clone();
            group_names.retain(|name| !name.trim().is_empty());
            group_names.sort();
            group_names.dedup();
            contacts.push(RemoteContact {
                contact,
                remote_id: Some(item.id.clone()),
                href: None,
                etag: item.etag.clone().or_else(|| item.change_key.clone()),
                raw_vcard: None,
                group_names,
            });
        }

        Ok(ContactDelta {
            contacts,
            deleted_remote_ids,
            deleted_hrefs: Vec::new(),
            groups: Vec::new(),
            groups_full: false,
            favorites_authoritative: false,
            sync_token: delta_link,
            ctag: None,
            full_sync,
        })
    }

    async fn create_contact(
        &self,
        account: &Account,
        book: &RemoteAddressBook,
        contact: &Contact,
        group_names: &[String],
    ) -> Result<RemoteWriteResult> {
        let body = contact_body(contact, group_names);
        let created = self
            .client
            .create_contact(&account.id, &book.remote_id, &body)
            .await?;
        let remote_id = Some(created.id.clone()).filter(|id| !id.is_empty());
        if let (Some(remote_id), Some((mime, bytes))) = (
            remote_id.as_deref(),
            contact.photo.as_deref().and_then(decode_photo),
        ) {
            self.client
                .put_photo(&account.id, &book.remote_id, remote_id, &mime, bytes)
                .await?;
        }
        Ok(RemoteWriteResult {
            remote_id,
            etag: created.etag.or(created.change_key),
            href: None,
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
        let Some(id) = current.remote_id.clone() else {
            return Err(AppError::conflict("contact has no Graph id"));
        };
        let body = contact_body(contact, group_names);
        let updated = self
            .client
            .update_contact(
                &account.id,
                &book.remote_id,
                &id,
                current.etag.as_deref(),
                &body,
            )
            .await?;
        if let Some((mime, bytes)) = contact.photo.as_deref().and_then(decode_photo) {
            self.client
                .put_photo(&account.id, &book.remote_id, &id, &mime, bytes)
                .await?;
        }
        Ok(RemoteWriteResult {
            remote_id: Some(updated.id).filter(|id| !id.is_empty()).or(Some(id)),
            etag: updated.etag.or(updated.change_key),
            href: None,
        })
    }

    async fn delete_contact(
        &self,
        account: &Account,
        book: &RemoteAddressBook,
        current: &RemoteContactRef,
    ) -> Result<()> {
        let Some(id) = current.remote_id.as_deref() else {
            return Ok(());
        };
        self.client
            .delete_contact(&account.id, &book.remote_id, id, current.etag.as_deref())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::models::ContactFieldType;

    const CONTACT: &str = r#"{
      "id": "AAMkAD",
      "@odata.etag": "W/\"EQAAABY\"",
      "displayName": "Grace Hopper",
      "givenName": "Grace",
      "surname": "Hopper",
      "nickname": "Amazing Grace",
      "companyName": "US Navy",
      "jobTitle": "Rear Admiral",
      "department": "Computing",
      "emailAddresses": [
        {"name": "Grace Hopper", "address": "grace@example.com"},
        {"name": "Work", "address": "grace@navy.example"}
      ],
      "businessPhones": ["+1 555 0100"],
      "homePhones": ["+1 555 0199"],
      "mobilePhone": "+1 555 0188",
      "businessHomePage": "https://example.com/grace",
      "homeAddress": {"street": "1 Main St", "city": "Arlington", "state": "VA", "postalCode": "22201", "countryOrRegion": "US"},
      "birthday": "1906-12-09T00:00:00Z",
      "personalNotes": "COBOL",
      "categories": ["Friends", "Work"]
    }"#;

    #[test]
    fn maps_graph_contact() {
        let item: GraphContact = serde_json::from_str(CONTACT).expect("contact");
        let contact = contact_from_graph(&item);
        assert_eq!(contact.display_name, "Grace Hopper");
        assert_eq!(contact.family_name, "Hopper");
        assert_eq!(
            contact.primary_email().as_deref(),
            Some("grace@example.com")
        );
        assert_eq!(contact.phones.len(), 3);
        assert_eq!(contact.phones[0].r#type, ContactFieldType::Mobile);
        assert_eq!(contact.addresses[0].city, "Arlington");
        assert_eq!(contact.birthday.as_deref(), Some("1906-12-09"));
        assert_eq!(contact.notes, "COBOL");
        assert_eq!(item.categories, vec!["Friends", "Work"]);
        assert_eq!(item.etag.as_deref(), Some("W/\"EQAAABY\""));
    }

    #[test]
    fn builds_graph_body() {
        let item: GraphContact = serde_json::from_str(CONTACT).expect("contact");
        let contact = contact_from_graph(&item);
        let body = contact_body(&contact, &["Friends".into()]);
        assert_eq!(body["givenName"], "Grace");
        assert_eq!(body["emailAddresses"][0]["address"], "grace@example.com");
        assert_eq!(body["mobilePhone"], "+1 555 0188");
        assert_eq!(body["categories"][0], "Friends");
        assert_eq!(body["homeAddress"]["city"], "Arlington");
        assert_eq!(body["birthday"], "1906-12-09T00:00:00Z");
    }

    #[test]
    fn detects_removed() {
        let item: GraphContact =
            serde_json::from_str(r#"{"id":"1","@removed":{"reason":"deleted"}}"#).expect("item");
        assert!(item.is_removed());
    }

    #[test]
    fn handles_unknown_birthday() {
        assert_eq!(parse_birthday("0001-01-01T00:00:00Z"), None);
        assert_eq!(
            graph_birthday("--04-15").as_deref(),
            Some("1904-04-15T00:00:00Z")
        );
    }
}
