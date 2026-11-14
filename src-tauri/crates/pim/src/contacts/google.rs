//! Google People API contact provider.
//!
//! Incremental sync uses `people.connections.list` with `syncToken`; Google
//! expires tokens regularly, in which case the documented full resync is
//! performed.  Group membership is managed through `contactGroups`, the
//! read-only `otherContacts` feed feeds recipient suggestions only and photos
//! are fetched from the (signed) Google CDN and capped at 200 KB.

use async_trait::async_trait;
use nuncio_core::models::{Account, Address, Contact, PostalAddress};
use nuncio_core::traits::TokenResource;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::contacts::map::{
    birthday, contact_field, decode_photo, display_name_from_parts, ensure_primary, field_type,
    parse_birthday, photo_data_url, sanitize_photo,
};
use crate::contacts::provider::{
    ContactCursor, ContactDelta, ContactProvider, RemoteAddressBook, RemoteContact,
    RemoteContactRef, RemoteGroup, RemoteWriteResult,
};
use crate::error::{invalid, AppError, Result};
use crate::http::HttpClient;

pub const PEOPLE_BASE: &str = "https://people.googleapis.com/v1";

/// Field mask used for reads and writes.  Kept in one place so create and
/// update stay consistent.
pub const PERSON_FIELDS: &str = "names,nicknames,emailAddresses,phoneNumbers,addresses,organizations,birthdays,biographies,photos,urls,memberships,metadata";

/// Fields accepted by `updateContact`.  Memberships and photos are managed
/// through their own endpoints and must not appear here.
pub const WRITE_FIELDS: &str = "names,nicknames,emailAddresses,phoneNumbers,addresses,organizations,birthdays,biographies,urls";

/// Google's built-in "starred" group (used for the local favourite flag).
pub const STARRED_GROUP: &str = "contactGroups/starred";

/// Photos downloaded per sync pass (keeps large address books light).
const PHOTO_BUDGET: usize = 100;

/// Low-level People API client.
#[derive(Clone)]
pub struct PeopleClient {
    http: HttpClient,
    base: String,
}

impl std::fmt::Debug for PeopleClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeopleClient")
            .field("base", &self.base)
            .finish()
    }
}

impl PeopleClient {
    pub fn new(http: HttpClient) -> Self {
        Self {
            http,
            base: PEOPLE_BASE.to_string(),
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

    /* ─────────────────────────── connections ─────────────────────────── */

    /// Fetch every page of `people.connections.list`.  Returns
    /// `(people, next_sync_token)`.
    pub async fn list_connections(
        &self,
        account_id: &str,
        _book_remote_id: &str,
        sync_token: Option<&str>,
    ) -> Result<(Vec<GPerson>, Option<String>)> {
        let mut people = Vec::new();
        let mut page_token: Option<String> = None;
        let next_sync: Option<String>;
        let mut guard = 0;
        loop {
            guard += 1;
            if guard > 200 {
                return Err(invalid("Google People pagination did not terminate"));
            }
            let mut query: Vec<(String, String)> = vec![
                ("personFields".into(), PERSON_FIELDS.into()),
                ("pageSize".into(), "1000".into()),
                ("sortOrder".into(), "LAST_MODIFIED_DESCENDING".into()),
            ];
            match sync_token {
                Some(token) => query.push(("syncToken".into(), token.to_string())),
                None => query.push(("requestSyncToken".into(), "true".into())),
            }
            if let Some(token) = &page_token {
                query.push(("pageToken".into(), token.clone()));
            }
            let url = self.url("/people/me/connections");
            let response = self
                .http
                .send_authed(account_id, TokenResource::GoogleApis, |token| {
                    self.http.raw().get(&url).bearer_auth(token).query(&query)
                })
                .await?;
            if !response.status().is_success() {
                return Err(Self::failure(response).await);
            }
            let page: GConnectionsPage = response
                .json()
                .await
                .map_err(|error| invalid(format!("invalid People connections JSON: {error}")))?;
            people.extend(page.connections.unwrap_or_default());
            match page.next_page_token {
                Some(token) if !token.is_empty() => page_token = Some(token),
                _ => {
                    next_sync = page.next_sync_token;
                    break;
                }
            }
        }
        Ok((people, next_sync))
    }

    /* ───────────────────────────── people ────────────────────────────── */

    pub async fn create_contact(
        &self,
        account_id: &str,
        body: &serde_json::Value,
    ) -> Result<GPerson> {
        let url = self.url("/people:createContact");
        let response = self
            .http
            .send_authed(account_id, TokenResource::GoogleApis, |token| {
                self.http
                    .raw()
                    .post(&url)
                    .bearer_auth(token)
                    .query(&[("personFields", PERSON_FIELDS)])
                    .json(body)
            })
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid People createContact JSON: {error}")))
    }

    pub async fn update_contact(
        &self,
        account_id: &str,
        resource_name: &str,
        body: &serde_json::Value,
    ) -> Result<GPerson> {
        let url = self.url(&format!(
            "/{}:updateContact",
            resource_name.trim_start_matches('/')
        ));
        let response = self
            .http
            .send_authed(account_id, TokenResource::GoogleApis, |token| {
                self.http
                    .raw()
                    .patch(&url)
                    .bearer_auth(token)
                    .query(&[
                        ("updatePersonFields", WRITE_FIELDS),
                        ("personFields", PERSON_FIELDS),
                    ])
                    .json(body)
            })
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid People updateContact JSON: {error}")))
    }

    /// Replace a contact's photo (`people.updateContactPhoto`).
    pub async fn update_contact_photo(
        &self,
        account_id: &str,
        resource_name: &str,
        bytes: &[u8],
    ) -> Result<()> {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine as _;
        let url = self.url(&format!(
            "/{}:updateContactPhoto",
            resource_name.trim_start_matches('/')
        ));
        let body = serde_json::json!({
            "photoBytes": STANDARD.encode(bytes),
            "personFields": "metadata",
        });
        let response = self
            .http
            .send_authed(account_id, TokenResource::GoogleApis, |token| {
                self.http.raw().patch(&url).bearer_auth(token).json(&body)
            })
            .await?;
        HttpClient::ensure_success(response).await
    }

    pub async fn delete_contact(&self, account_id: &str, resource_name: &str) -> Result<()> {
        let url = self.url(&format!(
            "/{}:deleteContact",
            resource_name.trim_start_matches('/')
        ));
        let response = self
            .http
            .send_authed(account_id, TokenResource::GoogleApis, |token| {
                self.http.raw().delete(&url).bearer_auth(token)
            })
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        HttpClient::ensure_success(response).await
    }

    /* ───────────────────────────── groups ────────────────────────────── */

    pub async fn list_groups(&self, account_id: &str) -> Result<Vec<GGroup>> {
        let mut groups = Vec::new();
        let mut page_token: Option<String> = None;
        let mut guard = 0;
        loop {
            guard += 1;
            if guard > 50 {
                break;
            }
            let mut query: Vec<(String, String)> = vec![
                ("pageSize".into(), "1000".into()),
                (
                    "groupFields".into(),
                    "name,groupType,memberCount,metadata".into(),
                ),
            ];
            if let Some(token) = &page_token {
                query.push(("pageToken".into(), token.clone()));
            }
            let url = self.url("/contactGroups");
            let response = self
                .http
                .send_authed(account_id, TokenResource::GoogleApis, |token| {
                    self.http.raw().get(&url).bearer_auth(token).query(&query)
                })
                .await?;
            if !response.status().is_success() {
                return Err(Self::failure(response).await);
            }
            let page: GGroupPage = response
                .json()
                .await
                .map_err(|error| invalid(format!("invalid People contactGroups JSON: {error}")))?;
            groups.extend(page.contact_groups.unwrap_or_default());
            match page.next_page_token {
                Some(token) if !token.is_empty() => page_token = Some(token),
                _ => break,
            }
        }
        Ok(groups)
    }

    pub async fn get_group(&self, account_id: &str, resource_name: &str) -> Result<Option<GGroup>> {
        let url = self.url(&format!(
            "/contactGroups/{}",
            resource_name.trim_start_matches("contactGroups/")
        ));
        let response = self
            .http
            .send_authed(account_id, TokenResource::GoogleApis, |token| {
                self.http.raw().get(&url).bearer_auth(token)
            })
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        response
            .json()
            .await
            .map(Some)
            .map_err(|error| invalid(format!("invalid People contactGroup JSON: {error}")))
    }

    pub async fn create_group(&self, account_id: &str, name: &str) -> Result<GGroup> {
        let url = self.url("/contactGroups");
        let body = serde_json::json!({ "contactGroup": { "name": name } });
        let response = self
            .http
            .send_authed(account_id, TokenResource::GoogleApis, |token| {
                self.http.raw().post(&url).bearer_auth(token).json(&body)
            })
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response).await);
        }
        response
            .json()
            .await
            .map_err(|error| invalid(format!("invalid People createGroup JSON: {error}")))
    }

    pub async fn rename_group(
        &self,
        account_id: &str,
        resource_name: &str,
        name: &str,
    ) -> Result<()> {
        let group = self
            .get_group(account_id, resource_name)
            .await?
            .ok_or_else(|| AppError::not_found("Google contact group not found"))?;
        let url = self.url(&format!(
            "/contactGroups/{}",
            resource_name.trim_start_matches("contactGroups/")
        ));
        let body = serde_json::json!({
            "contactGroup": {
                "resourceName": group.resource_name,
                "etag": group.etag,
                "name": name,
            }
        });
        let response = self
            .http
            .send_authed(account_id, TokenResource::GoogleApis, |token| {
                self.http.raw().put(&url).bearer_auth(token).json(&body)
            })
            .await?;
        HttpClient::ensure_success(response).await
    }

    pub async fn delete_group(&self, account_id: &str, resource_name: &str) -> Result<()> {
        let url = self.url(&format!(
            "/contactGroups/{}",
            resource_name.trim_start_matches("contactGroups/")
        ));
        let response = self
            .http
            .send_authed(account_id, TokenResource::GoogleApis, |token| {
                self.http
                    .raw()
                    .delete(&url)
                    .bearer_auth(token)
                    .query(&[("deleteContacts", "false")])
            })
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        HttpClient::ensure_success(response).await
    }

    pub async fn modify_group_members(
        &self,
        account_id: &str,
        resource_name: &str,
        add: &[String],
        remove: &[String],
    ) -> Result<()> {
        if add.is_empty() && remove.is_empty() {
            return Ok(());
        }
        let url = self.url(&format!(
            "/contactGroups/{}/members:modify",
            resource_name.trim_start_matches("contactGroups/")
        ));
        let body = serde_json::json!({
            "resourceNamesToAdd": add,
            "resourceNamesToRemove": remove,
        });
        let response = self
            .http
            .send_authed(account_id, TokenResource::GoogleApis, |token| {
                self.http.raw().post(&url).bearer_auth(token).json(&body)
            })
            .await?;
        HttpClient::ensure_success(response).await
    }

    /* ─────────────────────────── other contacts ──────────────────────── */

    pub async fn list_other_contacts(&self, account_id: &str) -> Result<Vec<GPerson>> {
        let mut people = Vec::new();
        let mut page_token: Option<String> = None;
        let mut guard = 0;
        loop {
            guard += 1;
            if guard > 50 {
                break;
            }
            let mut query: Vec<(String, String)> = vec![
                ("readMask".into(), "names,emailAddresses".into()),
                ("pageSize".into(), "1000".into()),
            ];
            if let Some(token) = &page_token {
                query.push(("pageToken".into(), token.clone()));
            }
            let url = self.url("/otherContacts");
            let response = self
                .http
                .send_authed(account_id, TokenResource::GoogleApis, |token| {
                    self.http.raw().get(&url).bearer_auth(token).query(&query)
                })
                .await?;
            if !response.status().is_success() {
                return Err(Self::failure(response).await);
            }
            let page: GOtherContactsPage = response
                .json()
                .await
                .map_err(|error| invalid(format!("invalid People otherContacts JSON: {error}")))?;
            people.extend(page.other_contacts.unwrap_or_default());
            match page.next_page_token {
                Some(token) if !token.is_empty() => page_token = Some(token),
                _ => break,
            }
        }
        Ok(people)
    }

    /// Download a profile photo (signed CDN URL, no bearer token needed).
    pub async fn fetch_photo(&self, url: &str) -> Option<String> {
        let response = self.http.raw().get(url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("image/jpeg")
            .to_string();
        let bytes = response.bytes().await.ok()?;
        photo_data_url(&content_type, &bytes)
    }
}

/* ──────────────────────────── JSON shapes ──────────────────────────── */

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GConnectionsPage {
    #[serde(default)]
    pub connections: Option<Vec<GPerson>>,
    #[serde(default)]
    pub next_page_token: Option<String>,
    #[serde(default)]
    pub next_sync_token: Option<String>,
    #[serde(default)]
    pub total_people: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GOtherContactsPage {
    #[serde(default)]
    pub other_contacts: Option<Vec<GPerson>>,
    #[serde(default)]
    pub next_page_token: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GMetadata {
    #[serde(default)]
    pub primary: bool,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub sources: Vec<GSource>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GSource {
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub etag: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GName {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub given_name: Option<String>,
    #[serde(default)]
    pub family_name: Option<String>,
    #[serde(default)]
    pub middle_name: Option<String>,
    #[serde(default)]
    pub unstructured_name: Option<String>,
    #[serde(default)]
    pub metadata: Option<GMetadata>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GValue {
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub metadata: Option<GMetadata>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GAddress {
    #[serde(default)]
    pub street_address: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub postal_code: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub metadata: Option<GMetadata>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GOrganization {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub department: Option<String>,
    #[serde(default)]
    pub current: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GDate {
    #[serde(default)]
    pub year: Option<i32>,
    #[serde(default)]
    pub month: Option<u32>,
    #[serde(default)]
    pub day: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GBirthday {
    #[serde(default)]
    pub date: Option<GDate>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GBiography {
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GPhoto {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default, rename = "default")]
    pub is_default: Option<bool>,
    #[serde(default)]
    pub metadata: Option<GMetadata>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GGroupMembership {
    #[serde(default)]
    pub contact_group_resource_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GMembership {
    #[serde(default)]
    pub contact_group_membership: Option<GGroupMembership>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GPerson {
    #[serde(default)]
    pub resource_name: Option<String>,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub metadata: Option<GMetadata>,
    #[serde(default)]
    pub names: Option<Vec<GName>>,
    #[serde(default)]
    pub nicknames: Option<Vec<GValue>>,
    #[serde(default)]
    pub email_addresses: Option<Vec<GValue>>,
    #[serde(default)]
    pub phone_numbers: Option<Vec<GValue>>,
    #[serde(default)]
    pub addresses: Option<Vec<GAddress>>,
    #[serde(default)]
    pub organizations: Option<Vec<GOrganization>>,
    #[serde(default)]
    pub birthdays: Option<Vec<GBirthday>>,
    #[serde(default)]
    pub biographies: Option<Vec<GBiography>>,
    #[serde(default)]
    pub photos: Option<Vec<GPhoto>>,
    #[serde(default)]
    pub urls: Option<Vec<GValue>>,
    #[serde(default)]
    pub memberships: Option<Vec<GMembership>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GGroupPage {
    #[serde(default)]
    pub contact_groups: Option<Vec<GGroup>>,
    #[serde(default)]
    pub next_page_token: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GGroup {
    #[serde(default)]
    pub resource_name: Option<String>,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub group_type: Option<String>,
    #[serde(default)]
    pub member_count: Option<i32>,
    #[serde(default)]
    pub metadata: Option<GMetadata>,
}

impl GGroup {
    fn is_user_group(&self) -> bool {
        self.group_type.as_deref() == Some("USER_CONTACT_GROUP")
            && !self.metadata.as_ref().is_some_and(|meta| meta.deleted)
    }
}

/* ───────────────────────────── mapping ────────────────────────────── */

fn preferred<T>(items: &[T], primary: impl Fn(&T) -> bool) -> Option<&T> {
    items
        .iter()
        .find(|item| primary(item))
        .or_else(|| items.first())
}

fn value_is_primary(value: &GValue) -> bool {
    value.metadata.as_ref().is_some_and(|meta| meta.primary)
}

/// Convert a People `Person` into the contract [`Contact`].  `id` and the
/// address-book/account ids are assigned by the sync engine.
pub fn person_to_contact(person: &GPerson) -> Contact {
    let mut contact = Contact::new("", "", "");
    if let Some(name) = person.names.as_deref().and_then(|names| {
        preferred(names, |name| {
            name.metadata.as_ref().is_some_and(|meta| meta.primary)
        })
    }) {
        contact.display_name = name
            .display_name
            .clone()
            .or_else(|| name.unstructured_name.clone())
            .unwrap_or_default();
        contact.given_name = name.given_name.clone().unwrap_or_default();
        contact.family_name = name.family_name.clone().unwrap_or_default();
        contact.middle_name = name.middle_name.clone().unwrap_or_default();
    }
    if contact.display_name.trim().is_empty() {
        contact.display_name = display_name_from_parts(&contact.given_name, &contact.family_name);
    }
    if let Some(nickname) = person
        .nicknames
        .as_deref()
        .and_then(|values| values.first())
        .and_then(|value| value.value.clone())
    {
        contact.nickname = nickname;
    }
    if let Some(emails) = &person.email_addresses {
        contact.emails = emails
            .iter()
            .filter_map(|value| {
                let email = value.value.as_deref()?.trim();
                if email.is_empty() {
                    return None;
                }
                Some(contact_field(
                    email,
                    value.kind.as_deref().unwrap_or("other"),
                    value_is_primary(value),
                ))
            })
            .collect();
        contact.emails = ensure_primary(contact.emails);
    }
    if let Some(phones) = &person.phone_numbers {
        contact.phones = phones
            .iter()
            .filter_map(|value| {
                let phone = value.value.as_deref()?.trim();
                if phone.is_empty() {
                    return None;
                }
                Some(contact_field(
                    phone,
                    value.kind.as_deref().unwrap_or("other"),
                    value_is_primary(value),
                ))
            })
            .collect();
        contact.phones = ensure_primary(contact.phones);
    }
    if let Some(addresses) = &person.addresses {
        contact.addresses = addresses
            .iter()
            .map(|address| PostalAddress {
                r#type: field_type(address.kind.as_deref().unwrap_or("other")),
                street: address.street_address.clone().unwrap_or_default(),
                city: address.city.clone().unwrap_or_default(),
                region: address.region.clone().unwrap_or_default(),
                postal_code: address.postal_code.clone().unwrap_or_default(),
                country: address
                    .country
                    .clone()
                    .or_else(|| address.country_code.clone())
                    .unwrap_or_default(),
            })
            .filter(|address| {
                !(address.street.is_empty()
                    && address.city.is_empty()
                    && address.region.is_empty()
                    && address.postal_code.is_empty()
                    && address.country.is_empty())
            })
            .collect();
    }
    if let Some(organization) = person.organizations.as_deref().and_then(|orgs| {
        orgs.iter()
            .find(|org| org.current == Some(true))
            .or_else(|| orgs.first())
    }) {
        contact.company = organization.name.clone().unwrap_or_default();
        contact.job_title = organization.title.clone().unwrap_or_default();
        contact.department = organization.department.clone().unwrap_or_default();
    }
    if let Some(date) = person
        .birthdays
        .as_deref()
        .and_then(|items| items.iter().find_map(|item| item.date.as_ref()))
    {
        if let (Some(month), Some(day)) = (date.month, date.day) {
            contact.birthday = birthday(date.year, month, day);
        }
    }
    if let Some(note) = person
        .biographies
        .as_deref()
        .and_then(|items| {
            items
                .iter()
                .find(|item| item.content_type.as_deref() == Some("TEXT_PLAIN"))
                .or_else(|| items.first())
        })
        .and_then(|item| item.value.clone())
    {
        contact.notes = note;
    }
    if let Some(website) = person
        .urls
        .as_deref()
        .and_then(|items| items.first())
        .and_then(|item| item.value.clone())
    {
        contact.websites = vec![website];
    }
    contact.is_favorite = group_memberships(person)
        .iter()
        .any(|membership| membership == STARRED_GROUP);
    contact
}

fn group_memberships(person: &GPerson) -> Vec<String> {
    person
        .memberships
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(|membership| {
            membership
                .contact_group_membership
                .as_ref()
                .and_then(|group| group.contact_group_resource_name.clone())
        })
        .collect()
}

fn person_body(contact: &Contact) -> serde_json::Map<String, serde_json::Value> {
    let mut body = serde_json::Map::new();
    let display = if contact.display_name.trim().is_empty() {
        contact.label()
    } else {
        contact.display_name.clone()
    };
    if !display.trim().is_empty() {
        body.insert(
            "names".into(),
            serde_json::json!([{
                "displayName": display,
                "unstructuredName": display,
                "givenName": contact.given_name,
                "familyName": contact.family_name,
                "middleName": contact.middle_name,
            }]),
        );
    }
    if !contact.nickname.trim().is_empty() {
        body.insert(
            "nicknames".into(),
            serde_json::json!([{ "value": contact.nickname }]),
        );
    }
    if !contact.emails.is_empty() {
        body.insert(
            "emailAddresses".into(),
            serde_json::json!(contact
                .emails
                .iter()
                .map(|field| {
                    serde_json::json!({
                        "value": field.value,
                        "type": field.r#type.as_str(),
                        "metadata": { "primary": field.is_primary },
                    })
                })
                .collect::<Vec<_>>()),
        );
    }
    if !contact.phones.is_empty() {
        body.insert(
            "phoneNumbers".into(),
            serde_json::json!(contact
                .phones
                .iter()
                .map(|field| {
                    serde_json::json!({
                        "value": field.value,
                        "type": field.r#type.as_str(),
                        "metadata": { "primary": field.is_primary },
                    })
                })
                .collect::<Vec<_>>()),
        );
    }
    if !contact.addresses.is_empty() {
        body.insert(
            "addresses".into(),
            serde_json::json!(contact
                .addresses
                .iter()
                .map(|address| {
                    serde_json::json!({
                        "type": address.r#type.as_str(),
                        "streetAddress": address.street,
                        "city": address.city,
                        "region": address.region,
                        "postalCode": address.postal_code,
                        "country": address.country,
                    })
                })
                .collect::<Vec<_>>()),
        );
    }
    if !contact.company.is_empty()
        || !contact.job_title.is_empty()
        || !contact.department.is_empty()
    {
        body.insert(
            "organizations".into(),
            serde_json::json!([{
                "name": contact.company,
                "title": contact.job_title,
                "department": contact.department,
                "current": true,
            }]),
        );
    }
    if !contact.notes.trim().is_empty() {
        body.insert(
            "biographies".into(),
            serde_json::json!([{
                "value": contact.notes,
                "contentType": "TEXT_PLAIN",
            }]),
        );
    }
    if let Some(website) = contact
        .websites
        .first()
        .filter(|url| !url.trim().is_empty())
    {
        body.insert("urls".into(), serde_json::json!([{ "value": website }]));
    }
    if let Some(birthday) = contact.birthday.as_deref().and_then(birthday_parts) {
        body.insert(
            "birthdays".into(),
            serde_json::json!([{ "date": birthday }]),
        );
    }
    body
}

fn birthday_parts(value: &str) -> Option<serde_json::Value> {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix("--") {
        let (month, day) = rest.split_once('-')?;
        return Some(serde_json::json!({
            "month": month.parse::<u32>().ok()?,
            "day": day.parse::<u32>().ok()?,
        }));
    }
    let parsed = parse_birthday(value)?;
    let mut parts = parsed.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    Some(serde_json::json!({ "year": year, "month": month, "day": day }))
}

/* ──────────────────────────── provider ────────────────────────────── */

/// Google People provider.
#[derive(Clone)]
pub struct GoogleContactsProvider {
    client: PeopleClient,
}

impl std::fmt::Debug for GoogleContactsProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleContactsProvider").finish()
    }
}

impl GoogleContactsProvider {
    pub fn new(http: HttpClient) -> Self {
        Self {
            client: PeopleClient::new(http),
        }
    }

    pub fn with_base(http: HttpClient, base: impl Into<String>) -> Self {
        Self {
            client: PeopleClient::with_base(http, base),
        }
    }

    pub fn client(&self) -> &PeopleClient {
        &self.client
    }

    /// Fetch small photos for the first [`PHOTO_BUDGET`] contacts.
    async fn attach_photos(&self, people: &[GPerson], contacts: &mut [Contact]) {
        let mut budget = PHOTO_BUDGET;
        for (person, contact) in people.iter().zip(contacts.iter_mut()) {
            if budget == 0 || contact.photo.is_some() {
                continue;
            }
            let Some(photo) = person
                .photos
                .as_deref()
                .and_then(|photos| photos.iter().find(|photo| photo.is_default != Some(true)))
            else {
                continue;
            };
            let Some(url) = photo.url.as_deref() else {
                continue;
            };
            if thumbnail_url(url).is_none() {
                continue;
            }
            budget -= 1;
            let url = thumbnail_url(url).unwrap_or_else(|| url.to_string());
            match self.client.fetch_photo(&url).await {
                Some(data) => contact.photo = sanitize_photo(Some(data)),
                None => debug!("Google profile photo could not be downloaded"),
            }
        }
    }
}

/// Ask Google's CDN for a small square thumbnail (`=s256`).
fn thumbnail_url(url: &str) -> Option<String> {
    if !url.starts_with("https://") {
        return None;
    }
    let base = url.split('=').next().unwrap_or(url);
    Some(format!("{base}=s256-c"))
}

#[async_trait]
impl ContactProvider for GoogleContactsProvider {
    fn name(&self) -> &'static str {
        "google"
    }

    fn supports_groups(&self) -> bool {
        true
    }

    fn groups_via_membership(&self) -> bool {
        true
    }

    async fn list_address_books(&self, _account: &Account) -> Result<Vec<RemoteAddressBook>> {
        // People API exposes exactly one address book per account.
        Ok(vec![RemoteAddressBook::new("people/me", "联系人")])
    }

    async fn sync_contacts(
        &self,
        account: &Account,
        book: &RemoteAddressBook,
        cursor: &ContactCursor,
    ) -> Result<ContactDelta> {
        let groups = self.client.list_groups(&account.id).await?;
        let group_names: std::collections::HashMap<String, String> = groups
            .iter()
            .filter(|group| group.is_user_group())
            .filter_map(|group| {
                Some((
                    group.resource_name.clone()?,
                    group.name.clone().unwrap_or_default(),
                ))
            })
            .collect();

        let token = cursor.sync_token.clone();
        let mut full_sync = token.is_none();
        let (people, next_token) = match self
            .client
            .list_connections(&account.id, &book.remote_id, token.as_deref())
            .await
        {
            Ok(result) => result,
            Err(error @ (AppError::InvalidInput(_) | AppError::NotFound(_))) if token.is_some() => {
                debug!(%error, "Google People sync token expired; performing a full resync");
                full_sync = true;
                self.client
                    .list_connections(&account.id, &book.remote_id, None)
                    .await?
            }
            Err(error) => return Err(error),
        };

        let mut contacts = Vec::new();
        let mut deleted_remote_ids = Vec::new();
        let mut people_for_photos = Vec::new();
        let mut mapped_for_photos = Vec::new();
        for person in &people {
            let Some(resource_name) = person.resource_name.clone() else {
                continue;
            };
            if person.metadata.as_ref().is_some_and(|meta| meta.deleted) {
                deleted_remote_ids.push(resource_name);
                continue;
            }
            let mut contact = person_to_contact(person);
            let memberships = group_memberships(person);
            let mut names = Vec::new();
            for membership in &memberships {
                if membership == STARRED_GROUP {
                    contact.is_favorite = true;
                    continue;
                }
                if let Some(name) = group_names.get(membership) {
                    if !name.trim().is_empty() {
                        names.push(name.clone());
                    }
                }
            }
            names.sort();
            names.dedup();
            let etag = person.etag.clone().or_else(|| {
                person
                    .metadata
                    .as_ref()
                    .and_then(|meta| meta.sources.first())
                    .and_then(|source| source.etag.clone())
            });
            people_for_photos.push(person.clone());
            mapped_for_photos.push(contact.clone());
            contacts.push(RemoteContact {
                contact,
                remote_id: Some(resource_name),
                href: None,
                etag,
                raw_vcard: None,
                group_names: names,
            });
        }
        self.attach_photos(&people_for_photos, &mut mapped_for_photos)
            .await;
        for (remote, mapped) in contacts.iter_mut().zip(mapped_for_photos) {
            remote.contact.photo = mapped.photo;
        }

        Ok(ContactDelta {
            contacts,
            deleted_remote_ids,
            deleted_hrefs: Vec::new(),
            groups: groups
                .iter()
                .filter(|group| group.is_user_group())
                .filter_map(|group| {
                    Some(RemoteGroup {
                        remote_id: group.resource_name.clone()?,
                        name: group.name.clone().unwrap_or_default(),
                    })
                })
                .collect(),
            groups_full: true,
            favorites_authoritative: true,
            sync_token: next_token.or_else(|| cursor.sync_token.clone()),
            ctag: None,
            full_sync,
        })
    }

    async fn create_contact(
        &self,
        account: &Account,
        _book: &RemoteAddressBook,
        contact: &Contact,
        _group_names: &[String],
    ) -> Result<RemoteWriteResult> {
        if contact.label().trim().is_empty() && contact.emails.is_empty() {
            return Err(invalid("contact needs a name or an e-mail address"));
        }
        let body = serde_json::Value::Object(person_body(contact));
        let person = self.client.create_contact(&account.id, &body).await?;
        if let (Some(resource_name), Some((_, bytes))) = (
            person.resource_name.as_deref(),
            contact.photo.as_deref().and_then(decode_photo),
        ) {
            self.client
                .update_contact_photo(&account.id, resource_name, &bytes)
                .await?;
        }
        Ok(RemoteWriteResult {
            remote_id: person.resource_name,
            etag: person.etag,
            href: None,
        })
    }

    async fn update_contact(
        &self,
        account: &Account,
        _book: &RemoteAddressBook,
        current: &RemoteContactRef,
        contact: &Contact,
        _group_names: &[String],
    ) -> Result<RemoteWriteResult> {
        let Some(resource_name) = current.remote_id.clone() else {
            return Err(AppError::conflict("contact has no Google resource name"));
        };
        let mut body = person_body(contact);
        body.insert("resourceName".into(), serde_json::json!(resource_name));
        if let Some(etag) = &current.etag {
            body.insert("etag".into(), serde_json::json!(etag));
        }
        let person = self
            .client
            .update_contact(
                &account.id,
                &resource_name,
                &serde_json::Value::Object(body),
            )
            .await?;
        if let Some((_, bytes)) = contact.photo.as_deref().and_then(decode_photo) {
            let target = person.resource_name.as_deref().unwrap_or(&resource_name);
            self.client
                .update_contact_photo(&account.id, target, &bytes)
                .await?;
        }
        Ok(RemoteWriteResult {
            remote_id: person.resource_name.or(Some(resource_name)),
            etag: person.etag,
            href: None,
        })
    }

    async fn delete_contact(
        &self,
        account: &Account,
        _book: &RemoteAddressBook,
        current: &RemoteContactRef,
    ) -> Result<()> {
        let Some(resource_name) = current.remote_id.as_deref() else {
            return Ok(());
        };
        self.client.delete_contact(&account.id, resource_name).await
    }

    async fn list_groups(
        &self,
        account: &Account,
        _book: &RemoteAddressBook,
    ) -> Result<Vec<RemoteGroup>> {
        Ok(self
            .client
            .list_groups(&account.id)
            .await?
            .iter()
            .filter(|group| group.is_user_group())
            .filter_map(|group| {
                Some(RemoteGroup {
                    remote_id: group.resource_name.clone()?,
                    name: group.name.clone().unwrap_or_default(),
                })
            })
            .collect())
    }

    async fn create_group(
        &self,
        account: &Account,
        _book: &RemoteAddressBook,
        name: &str,
    ) -> Result<RemoteGroup> {
        let group = self.client.create_group(&account.id, name).await?;
        Ok(RemoteGroup {
            remote_id: group.resource_name.unwrap_or_default(),
            name: group.name.unwrap_or_else(|| name.to_string()),
        })
    }

    async fn rename_group(
        &self,
        account: &Account,
        group_remote_id: &str,
        name: &str,
    ) -> Result<()> {
        self.client
            .rename_group(&account.id, group_remote_id, name)
            .await
    }

    async fn delete_group(&self, account: &Account, group_remote_id: &str) -> Result<()> {
        self.client.delete_group(&account.id, group_remote_id).await
    }

    async fn modify_group_members(
        &self,
        account: &Account,
        group_remote_id: &str,
        add: &[String],
        remove: &[String],
    ) -> Result<()> {
        let prefix = |ids: &[String]| -> Vec<String> {
            ids.iter()
                .filter(|id| !id.trim().is_empty())
                .map(|id| {
                    if id.starts_with("people/") {
                        id.clone()
                    } else {
                        format!("people/{id}")
                    }
                })
                .collect()
        };
        self.client
            .modify_group_members(&account.id, group_remote_id, &prefix(add), &prefix(remove))
            .await
    }

    async fn set_starred(
        &self,
        account: &Account,
        person_remote_id: &str,
        value: bool,
    ) -> Result<()> {
        let member = vec![person_remote_id.to_string()];
        let empty: Vec<String> = Vec::new();
        if value {
            self.client
                .modify_group_members(&account.id, STARRED_GROUP, &member, &empty)
                .await
        } else {
            self.client
                .modify_group_members(&account.id, STARRED_GROUP, &empty, &member)
                .await
        }
    }

    async fn other_contacts(&self, account: &Account) -> Result<Vec<Address>> {
        let people = self.client.list_other_contacts(&account.id).await?;
        let mut out = Vec::new();
        for person in people {
            let name = person
                .names
                .as_deref()
                .and_then(|names| names.first())
                .and_then(|name| {
                    name.display_name
                        .clone()
                        .or_else(|| name.unstructured_name.clone())
                });
            for value in person.email_addresses.unwrap_or_default() {
                let Some(email) = value.value else {
                    continue;
                };
                if email.trim().is_empty() {
                    continue;
                }
                out.push(match &name {
                    Some(name) if !name.trim().is_empty() => {
                        Address::with_name(name.clone(), email)
                    }
                    _ => Address::new(email),
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERSON: &str = r##"{
      "resourceName": "people/c123",
      "etag": "%EgQBAjM3GQ==",
      "metadata": {"sources": [{"type": "CONTACT", "id": "abc", "etag": "#xyz"}]},
      "names": [{"displayName": "Ada Lovelace", "givenName": "Ada", "familyName": "Lovelace", "metadata": {"primary": true}}],
      "nicknames": [{"value": "Ada"}],
      "emailAddresses": [
        {"value": "ada@example.com", "type": "home", "metadata": {"primary": true}},
        {"value": "ada@analytical.example", "type": "work"}
      ],
      "phoneNumbers": [{"value": "+44 20 0000 0000", "type": "mobile"}],
      "addresses": [{"type": "home", "streetAddress": "12 St James's Square", "city": "London", "region": "England", "postalCode": "SW1Y 4LB", "country": "UK"}],
      "organizations": [{"name": "Analytical Engines", "title": "Mathematician", "department": "R&D", "current": true}],
      "birthdays": [{"date": {"year": 1815, "month": 12, "day": 10}}],
      "biographies": [{"value": "First programmer", "contentType": "TEXT_PLAIN"}],
      "urls": [{"value": "https://example.com/ada"}],
      "photos": [{"url": "https://lh3.googleusercontent.com/a/photo", "default": false}],
      "memberships": [
        {"contactGroupMembership": {"contactGroupResourceName": "contactGroups/friends"}},
        {"contactGroupMembership": {"contactGroupResourceName": "contactGroups/starred"}}
      ]
    }"##;

    #[test]
    fn maps_person_to_contact() {
        let person: GPerson = serde_json::from_str(PERSON).expect("person");
        let contact = person_to_contact(&person);
        assert_eq!(contact.display_name, "Ada Lovelace");
        assert_eq!(contact.given_name, "Ada");
        assert_eq!(contact.family_name, "Lovelace");
        assert_eq!(contact.nickname, "Ada");
        assert_eq!(contact.primary_email().as_deref(), Some("ada@example.com"));
        assert_eq!(
            contact.phones[0].r#type,
            nuncio_core::models::ContactFieldType::Mobile
        );
        assert_eq!(contact.addresses[0].city, "London");
        assert_eq!(contact.company, "Analytical Engines");
        assert_eq!(contact.birthday.as_deref(), Some("1815-12-10"));
        assert_eq!(contact.notes, "First programmer");
        assert_eq!(contact.websites, vec!["https://example.com/ada"]);
        assert!(contact.is_favorite, "starred group marks favourite");
        assert_eq!(
            group_memberships(&person),
            vec!["contactGroups/friends", "contactGroups/starred"]
        );
    }

    #[test]
    fn builds_person_body() {
        let person: GPerson = serde_json::from_str(PERSON).expect("person");
        let contact = person_to_contact(&person);
        let body = serde_json::Value::Object(person_body(&contact));
        assert_eq!(body["names"][0]["givenName"], "Ada");
        assert_eq!(body["emailAddresses"][0]["value"], "ada@example.com");
        assert_eq!(body["birthdays"][0]["date"]["year"], 1815);
        assert_eq!(body["organizations"][0]["name"], "Analytical Engines");
    }

    #[test]
    fn maps_deleted_person() {
        let person: GPerson =
            serde_json::from_str(r#"{"resourceName":"people/c9","metadata":{"deleted":true}}"#)
                .expect("person");
        let contact = person_to_contact(&person);
        assert!(contact.display_name.is_empty());
        assert!(person.metadata.unwrap().deleted);
    }

    #[test]
    fn builds_thumbnails() {
        assert_eq!(
            thumbnail_url("https://lh3.googleusercontent.com/a/xyz=s512").as_deref(),
            Some("https://lh3.googleusercontent.com/a/xyz=s256-c")
        );
        assert_eq!(thumbnail_url("http://insecure/photo"), None);
    }

    #[test]
    fn parses_groups() {
        let page: GGroupPage = serde_json::from_str(
            r#"{"contactGroups":[
                {"resourceName":"contactGroups/1","name":"Friends","groupType":"USER_CONTACT_GROUP"},
                {"resourceName":"contactGroups/starred","name":"Starred","groupType":"SYSTEM_CONTACT_GROUP"}
            ]}"#,
        )
        .expect("groups");
        let groups = page.contact_groups.unwrap();
        assert!(groups[0].is_user_group());
        assert!(!groups[1].is_user_group());
    }

    #[test]
    fn maps_unknown_types_to_other() {
        let field = contact_field("x", "weird", false);
        assert_eq!(field.r#type, nuncio_core::models::ContactFieldType::Other);
    }
}
