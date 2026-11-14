//! Contacts, groups and contact-book queries.

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::error::AppError;
use crate::models::{
    new_id, Address, Contact, ContactField, ContactGroup, ContactListQuery, ContactPage,
    ContactSummary, PageCursor, PostalAddress,
};
use crate::repo::{
    get_bool, get_opt_dt, get_opt_string, get_string, json_or_default, json_text, now_string,
};
use crate::Result;

pub const CONTACT_COLUMNS: &str = "\
    id, address_book_id, account_id, display_name, given_name, family_name, middle_name, nickname,\
    company, job_title, department, emails_json, phones_json, addresses_json, websites_json,\
    birthday, notes, photo, is_favorite, updated_at";

fn row_to_contact(row: &Row<'_>) -> Result<Contact> {
    let emails: Option<String> = row.get("emails_json")?;
    let phones: Option<String> = row.get("phones_json")?;
    let addresses: Option<String> = row.get("addresses_json")?;
    let websites: Option<String> = row.get("websites_json")?;
    Ok(Contact {
        id: get_string(row, "id")?,
        address_book_id: get_string(row, "address_book_id")?,
        account_id: get_string(row, "account_id")?,
        display_name: get_string(row, "display_name")?,
        given_name: get_string(row, "given_name")?,
        family_name: get_string(row, "family_name")?,
        middle_name: get_string(row, "middle_name")?,
        nickname: get_string(row, "nickname")?,
        company: get_string(row, "company")?,
        job_title: get_string(row, "job_title")?,
        department: get_string(row, "department")?,
        emails: json_or_default::<Vec<ContactField>>(emails.as_deref(), "emails_json"),
        phones: json_or_default::<Vec<ContactField>>(phones.as_deref(), "phones_json"),
        addresses: json_or_default::<Vec<PostalAddress>>(addresses.as_deref(), "addresses_json"),
        websites: json_or_default::<Vec<String>>(websites.as_deref(), "websites_json"),
        birthday: get_opt_string(row, "birthday")?,
        notes: get_string(row, "notes")?,
        photo: get_opt_string(row, "photo")?,
        is_favorite: get_bool(row, "is_favorite")?,
        group_ids: Vec::new(),
        updated_at: get_opt_dt(row, "updated_at")?,
    })
}

fn primary_value(fields: &[ContactField]) -> Option<String> {
    fields
        .iter()
        .find(|field| field.is_primary)
        .or_else(|| fields.first())
        .map(|field| field.value.clone())
}

fn row_to_summary(row: &Row<'_>) -> Result<ContactSummary> {
    let emails: Option<String> = row.get("emails_json")?;
    let phones: Option<String> = row.get("phones_json")?;
    let emails: Vec<ContactField> = json_or_default(emails.as_deref(), "emails_json");
    let phones: Vec<ContactField> = json_or_default(phones.as_deref(), "phones_json");
    Ok(ContactSummary {
        id: get_string(row, "id")?,
        account_id: get_string(row, "account_id")?,
        address_book_id: get_string(row, "address_book_id")?,
        display_name: get_string(row, "display_name")?,
        primary_email: primary_value(&emails),
        primary_phone: primary_value(&phones),
        company: get_string(row, "company")?,
        photo: get_opt_string(row, "photo")?,
        is_favorite: get_bool(row, "is_favorite")?,
    })
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Contact>> {
    let sql = format!("SELECT {CONTACT_COLUMNS} FROM contact WHERE id = ?1");
    let Some(contact) = crate::repo::query_one(conn, &sql, params![id], row_to_contact)? else {
        return Ok(None);
    };
    let mut contact = contact;
    contact.group_ids = group_ids_for_contact(conn, id)?;
    Ok(Some(contact))
}

/// Sync metadata kept for CardDAV / Graph (not part of the IPC `Contact`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContactSyncMeta {
    pub etag: Option<String>,
    pub remote_id: Option<String>,
    pub href: Option<String>,
    pub raw_vcard: Option<String>,
}

pub fn sync_meta(conn: &Connection, id: &str) -> Result<Option<ContactSyncMeta>> {
    Ok(conn
        .query_row(
            "SELECT etag, remote_id, href, raw_vcard FROM contact WHERE id = ?1",
            params![id],
            |row| {
                Ok(ContactSyncMeta {
                    etag: row.get(0)?,
                    remote_id: row.get(1)?,
                    href: row.get(2)?,
                    raw_vcard: row.get(3)?,
                })
            },
        )
        .optional()?)
}

pub fn get_required(conn: &Connection, id: &str) -> Result<Contact> {
    get(conn, id)?.ok_or_else(|| AppError::NotFound(format!("contact {id} not found")))
}

pub fn list(conn: &Connection, query: &ContactListQuery) -> Result<ContactPage> {
    let limit = query.limit.clamp(1, 500) as i64;
    let mut clauses: Vec<String> = Vec::new();
    let mut values: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(book) = &query.address_book_id {
        clauses.push("address_book_id = ?".into());
        values.push(crate::repo::text(book));
    }
    if let Some(group) = &query.group_id {
        clauses
            .push("id IN (SELECT contact_id FROM contact_group_member WHERE group_id = ?)".into());
        values.push(crate::repo::text(group));
    }
    if query.favorites_only {
        clauses.push("is_favorite = 1".into());
    }
    if let Some(text) = query
        .query
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        clauses.push(
            "(lower(display_name) LIKE ? OR lower(emails_json) LIKE ? OR lower(company) LIKE ? OR lower(nickname) LIKE ?)"
                .into(),
        );
        let pattern = format!("%{}%", text.to_lowercase());
        for _ in 0..4 {
            values.push(crate::repo::text(pattern.clone()));
        }
    }
    let where_sql = if clauses.is_empty() {
        "1 = 1".to_string()
    } else {
        clauses.join(" AND ")
    };

    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM contact WHERE {where_sql}"),
        crate::repo::query_params(&values).as_slice(),
        |row| row.get(0),
    )?;

    let mut params = values;
    let mut cursor_clause = String::new();
    if let Some(raw) = &query.cursor {
        let cursor = PageCursor::decode(raw)?;
        cursor_clause =
            " AND (lower(display_name) > lower(?) OR (lower(display_name) = lower(?) AND id > ?))"
                .to_string();
        params.push(crate::repo::text(cursor.key.clone()));
        params.push(crate::repo::text(cursor.key));
        params.push(crate::repo::text(cursor.id));
    }
    params.push(crate::repo::int(limit + 1));

    let sql = format!(
        "SELECT {CONTACT_COLUMNS} FROM contact WHERE {where_sql}{cursor_clause}
         ORDER BY lower(display_name) ASC, id ASC LIMIT ?"
    );
    let mut stmt = conn.prepare(&sql)?;
    let refs = crate::repo::query_params(&params);
    let mut rows = stmt.query(refs.as_slice())?;
    let mut items = Vec::new();
    while let Some(row) = rows.next()? {
        items.push(row_to_summary(row)?);
    }
    let has_more = items.len() as i64 > limit;
    if has_more {
        items.truncate(limit as usize);
    }
    let next_cursor = if has_more {
        items
            .last()
            .map(|item| PageCursor::contact(item.display_name.clone(), item.id.clone()).encode())
    } else {
        None
    };
    Ok(ContactPage {
        items,
        next_cursor,
        total,
    })
}

/// Name/e-mail search used by autocomplete and the command palette.
pub fn search(conn: &Connection, text: &str, limit: u32) -> Result<Vec<ContactSummary>> {
    let pattern = format!("%{}%", text.trim().to_lowercase());
    let sql = format!(
        "SELECT {CONTACT_COLUMNS} FROM contact
         WHERE lower(display_name) LIKE ?1 OR lower(emails_json) LIKE ?1 OR lower(company) LIKE ?1
         ORDER BY is_favorite DESC, lower(display_name) ASC LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![pattern, limit.clamp(1, 200) as i64])?;
    let mut items = Vec::new();
    while let Some(row) = rows.next()? {
        items.push(row_to_summary(row)?);
    }
    Ok(items)
}

pub fn find_by_email(conn: &Connection, email: &str) -> Result<Option<Contact>> {
    let email = email.trim().to_lowercase();
    if email.is_empty() {
        return Ok(None);
    }
    let pattern = format!("%{email}%");
    let mut stmt = conn.prepare(
        "SELECT id FROM contact WHERE lower(emails_json) LIKE ?1 ORDER BY is_favorite DESC, rowid ASC LIMIT 10",
    )?;
    let mut rows = stmt.query(params![pattern])?;
    let mut ids = Vec::new();
    while let Some(row) = rows.next()? {
        ids.push(row.get::<_, String>(0)?);
    }
    drop(rows);
    drop(stmt);
    for id in ids {
        if let Some(contact) = get(conn, &id)? {
            if contact
                .emails
                .iter()
                .any(|field| field.value.trim().to_lowercase() == email)
            {
                return Ok(Some(contact));
            }
        }
    }
    Ok(None)
}

/// Address for a contact, preferring the primary e-mail.
pub fn address_for(contact: &Contact) -> Option<Address> {
    contact.primary_email().map(|email| Address {
        name: Some(contact.label()).filter(|name| !name.is_empty() && name != &email),
        email,
    })
}

fn insert_contact(conn: &Connection, contact: &Contact) -> Result<()> {
    let now = now_string();
    conn.execute(
        "INSERT INTO contact (
            id, address_book_id, account_id, display_name, given_name, family_name, middle_name, nickname,
            company, job_title, department, emails_json, phones_json, addresses_json, websites_json,
            birthday, notes, photo, is_favorite, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?20)
         ON CONFLICT(id) DO UPDATE SET
             address_book_id = excluded.address_book_id,
             account_id = excluded.account_id,
             display_name = excluded.display_name,
             given_name = excluded.given_name,
             family_name = excluded.family_name,
             middle_name = excluded.middle_name,
             nickname = excluded.nickname,
             company = excluded.company,
             job_title = excluded.job_title,
             department = excluded.department,
             emails_json = excluded.emails_json,
             phones_json = excluded.phones_json,
             addresses_json = excluded.addresses_json,
             websites_json = excluded.websites_json,
             birthday = excluded.birthday,
             notes = excluded.notes,
             photo = excluded.photo,
             is_favorite = excluded.is_favorite,
             updated_at = excluded.updated_at",
        params![
            contact.id,
            contact.address_book_id,
            contact.account_id,
            contact.display_name,
            contact.given_name,
            contact.family_name,
            contact.middle_name,
            contact.nickname,
            contact.company,
            contact.job_title,
            contact.department,
            json_text(&contact.emails)?,
            json_text(&contact.phones)?,
            json_text(&contact.addresses)?,
            json_text(&contact.websites)?,
            contact.birthday,
            contact.notes,
            contact.photo,
            contact.is_favorite as i64,
            now
        ],
    )?;
    Ok(())
}

pub fn create(conn: &Connection, contact: &Contact) -> Result<Contact> {
    let mut contact = contact.clone();
    if contact.id.trim().is_empty() {
        contact.id = new_id();
    }
    if contact.address_book_id.trim().is_empty() {
        return Err(AppError::invalid_input("contact requires an address book"));
    }
    insert_contact(conn, &contact)?;
    replace_groups(conn, &contact.id, &contact.group_ids)?;
    get_required(conn, &contact.id)
}

pub fn update(conn: &Connection, contact: &Contact) -> Result<Contact> {
    if contact.id.trim().is_empty() {
        return Err(AppError::invalid_input("contact id is required"));
    }
    insert_contact(conn, contact)?;
    replace_groups(conn, &contact.id, &contact.group_ids)?;
    get_required(conn, &contact.id)
}

/// Upsert a contact synced from CardDAV / Graph (keeps sync metadata).
pub fn upsert_remote(
    conn: &Connection,
    contact: &Contact,
    etag: Option<&str>,
    remote_id: Option<&str>,
    href: Option<&str>,
    raw_vcard: Option<&str>,
) -> Result<Contact> {
    let mut contact = contact.clone();
    if contact.id.trim().is_empty() {
        contact.id = new_id();
    }
    insert_contact(conn, &contact)?;
    conn.execute(
        "UPDATE contact SET etag = COALESCE(?2, etag), remote_id = COALESCE(?3, remote_id),
                            href = COALESCE(?4, href), raw_vcard = COALESCE(?5, raw_vcard)
         WHERE id = ?1",
        params![contact.id, etag, remote_id, href, raw_vcard],
    )?;
    replace_groups(conn, &contact.id, &contact.group_ids)?;
    get_required(conn, &contact.id)
}

pub fn set_favorite(conn: &Connection, id: &str, value: bool) -> Result<()> {
    conn.execute(
        "UPDATE contact SET is_favorite = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, value as i64, now_string()],
    )?;
    Ok(())
}

pub fn delete_many(conn: &Connection, ids: &[String]) -> Result<usize> {
    let mut count = 0;
    for id in ids {
        count += conn.execute("DELETE FROM contact WHERE id = ?1", params![id])?;
    }
    Ok(count)
}

pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM contact", [], |row| row.get(0))?)
}

pub fn by_remote_id(
    conn: &Connection,
    address_book_id: &str,
    remote_id: &str,
) -> Result<Option<Contact>> {
    let id: Option<String> = conn
        .query_row(
            "SELECT id FROM contact WHERE address_book_id = ?1 AND remote_id = ?2 LIMIT 1",
            params![address_book_id, remote_id],
            |row| row.get(0),
        )
        .optional()?;
    match id {
        Some(id) => get(conn, &id),
        None => Ok(None),
    }
}

/* ───────────────────────────── groups ─────────────────────────────── */

pub fn list_groups(conn: &Connection, address_book_id: Option<&str>) -> Result<Vec<ContactGroup>> {
    let sql = "SELECT g.id, g.address_book_id, g.account_id, g.name,
                      (SELECT COUNT(*) FROM contact_group_member m WHERE m.group_id = g.id) AS member_count
               FROM contact_group g
               WHERE (?1 IS NULL OR g.address_book_id = ?1)
               ORDER BY g.name COLLATE NOCASE ASC";
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(params![address_book_id])?;
    let mut groups = Vec::new();
    while let Some(row) = rows.next()? {
        groups.push(ContactGroup {
            id: row.get(0)?,
            address_book_id: row.get(1)?,
            account_id: row.get(2)?,
            name: row.get(3)?,
            member_count: row.get(4)?,
        });
    }
    Ok(groups)
}

pub fn get_group(conn: &Connection, id: &str) -> Result<Option<ContactGroup>> {
    let groups = list_groups(conn, None)?;
    Ok(groups.into_iter().find(|group| group.id == id))
}

pub fn create_group(conn: &Connection, address_book_id: &str, name: &str) -> Result<ContactGroup> {
    let book = crate::repo::address_books::get_required(conn, address_book_id)?;
    let id = new_id();
    let now = now_string();
    conn.execute(
        "INSERT INTO contact_group (id, address_book_id, account_id, name, remote_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5)",
        params![id, address_book_id, book.account_id, name, now],
    )?;
    crate::repo::contacts::get_group(conn, &id)?
        .ok_or_else(|| AppError::Db("group insert failed".into()))
}

pub fn rename_group(conn: &Connection, id: &str, name: &str) -> Result<ContactGroup> {
    let changed = conn.execute(
        "UPDATE contact_group SET name = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, name, now_string()],
    )?;
    if changed == 0 {
        return Err(AppError::NotFound(format!("group {id} not found")));
    }
    get_group(conn, id)?.ok_or_else(|| AppError::Db("group disappeared".into()))
}

pub fn delete_group(conn: &Connection, id: &str) -> Result<()> {
    let changed = conn.execute("DELETE FROM contact_group WHERE id = ?1", params![id])?;
    if changed == 0 {
        return Err(AppError::NotFound(format!("group {id} not found")));
    }
    Ok(())
}

pub fn set_group_members(conn: &Connection, group_id: &str, contact_ids: &[String]) -> Result<()> {
    if get_group(conn, group_id)?.is_none() {
        return Err(AppError::NotFound(format!("group {group_id} not found")));
    }
    conn.execute(
        "DELETE FROM contact_group_member WHERE group_id = ?1",
        params![group_id],
    )?;
    for contact_id in contact_ids {
        conn.execute(
            "INSERT OR IGNORE INTO contact_group_member (group_id, contact_id, added_at) VALUES (?1, ?2, ?3)",
            params![group_id, contact_id, now_string()],
        )?;
    }
    Ok(())
}

fn replace_groups(conn: &Connection, contact_id: &str, group_ids: &[String]) -> Result<()> {
    conn.execute(
        "DELETE FROM contact_group_member WHERE contact_id = ?1",
        params![contact_id],
    )?;
    for group_id in group_ids {
        conn.execute(
            "INSERT OR IGNORE INTO contact_group_member (group_id, contact_id, added_at) VALUES (?1, ?2, ?3)",
            params![group_id, contact_id, now_string()],
        )?;
    }
    Ok(())
}

pub fn group_ids_for_contact(conn: &Connection, contact_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT group_id FROM contact_group_member WHERE contact_id = ?1 ORDER BY group_id ASC",
    )?;
    let mut rows = stmt.query(params![contact_id])?;
    let mut ids = Vec::new();
    while let Some(row) = rows.next()? {
        ids.push(row.get(0)?);
    }
    Ok(ids)
}

/// Contacts that belong to a group (full records).
pub fn members(conn: &Connection, group_id: &str) -> Result<Vec<Contact>> {
    let mut stmt = conn.prepare(
        "SELECT c.id FROM contact c
         JOIN contact_group_member m ON m.contact_id = c.id
         WHERE m.group_id = ?1 ORDER BY lower(c.display_name) ASC",
    )?;
    let mut rows = stmt.query(params![group_id])?;
    let mut ids = Vec::new();
    while let Some(row) = rows.next()? {
        ids.push(row.get::<_, String>(0)?);
    }
    drop(rows);
    drop(stmt);
    let mut contacts = Vec::new();
    for id in ids {
        if let Some(contact) = get(conn, &id)? {
            contacts.push(contact);
        }
    }
    Ok(contacts)
}
