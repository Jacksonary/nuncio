//! Contact, group and address-book commands.

use tauri::State;

use nuncio_core::models::{AddressBook, Contact, ContactGroup, ContactListQuery, ContactPage};

use crate::state::AppState;

#[tauri::command]
pub async fn addressbook_list(state: State<'_, AppState>) -> nuncio_core::Result<Vec<AddressBook>> {
    state.pim.addressbook_list().await
}

#[tauri::command]
pub async fn contact_list(
    state: State<'_, AppState>,
    query: ContactListQuery,
) -> nuncio_core::Result<ContactPage> {
    state.pim.contact_list(query).await
}

#[tauri::command]
pub async fn contact_get(state: State<'_, AppState>, id: String) -> nuncio_core::Result<Contact> {
    state.pim.contact_get(&id).await
}

#[tauri::command]
pub async fn contact_create(
    state: State<'_, AppState>,
    contact: Contact,
) -> nuncio_core::Result<Contact> {
    state.pim.contact_create(contact).await
}

#[tauri::command]
pub async fn contact_update(
    state: State<'_, AppState>,
    contact: Contact,
) -> nuncio_core::Result<Contact> {
    state.pim.contact_update(contact).await
}

#[tauri::command]
pub async fn contact_delete(
    state: State<'_, AppState>,
    ids: Vec<String>,
) -> nuncio_core::Result<()> {
    state.pim.contact_delete(ids).await
}

#[tauri::command]
pub async fn contact_set_favorite(
    state: State<'_, AppState>,
    id: String,
    value: bool,
) -> nuncio_core::Result<()> {
    state.pim.contact_set_favorite(&id, value).await
}

#[tauri::command]
pub async fn contact_find_by_email(
    state: State<'_, AppState>,
    email: String,
) -> nuncio_core::Result<Option<Contact>> {
    state.pim.contact_find_by_email(&email).await
}

#[tauri::command]
pub async fn group_list(
    state: State<'_, AppState>,
    address_book_id: Option<String>,
) -> nuncio_core::Result<Vec<ContactGroup>> {
    state.pim.group_list(address_book_id).await
}

#[tauri::command]
pub async fn group_create(
    state: State<'_, AppState>,
    address_book_id: String,
    name: String,
) -> nuncio_core::Result<ContactGroup> {
    state.pim.group_create(&address_book_id, &name).await
}

#[tauri::command]
pub async fn group_rename(
    state: State<'_, AppState>,
    id: String,
    name: String,
) -> nuncio_core::Result<ContactGroup> {
    state.pim.group_rename(&id, &name).await
}

#[tauri::command]
pub async fn group_delete(state: State<'_, AppState>, id: String) -> nuncio_core::Result<()> {
    state.pim.group_delete(&id).await
}

#[tauri::command]
pub async fn group_set_members(
    state: State<'_, AppState>,
    group_id: String,
    contact_ids: Vec<String>,
) -> nuncio_core::Result<()> {
    state.pim.group_set_members(&group_id, contact_ids).await
}

#[tauri::command]
pub async fn contact_import_vcf(
    state: State<'_, AppState>,
    path: String,
    address_book_id: String,
) -> nuncio_core::Result<u32> {
    state.pim.contact_import_vcf(&path, &address_book_id).await
}

#[tauri::command]
pub async fn contact_export_vcf(
    state: State<'_, AppState>,
    ids: Vec<String>,
    path: String,
) -> nuncio_core::Result<()> {
    state.pim.contact_export_vcf(ids, &path).await
}
