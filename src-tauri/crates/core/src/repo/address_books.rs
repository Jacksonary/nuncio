//! Address book repository.

use rusqlite::{params, Connection, Row};

use crate::error::AppError;
use crate::models::{new_id, AddressBook};
use crate::repo::{get_bool, get_string, now_string};
use crate::Result;

const COLUMNS: &str = "id, account_id, name, is_read_only, is_default";

fn row_to_book(row: &Row<'_>) -> Result<AddressBook> {
    Ok(AddressBook {
        id: get_string(row, "id")?,
        account_id: get_string(row, "account_id")?,
        name: get_string(row, "name")?,
        is_read_only: get_bool(row, "is_read_only")?,
        is_default: get_bool(row, "is_default")?,
    })
}

pub fn list(conn: &Connection, account_id: Option<&str>) -> Result<Vec<AddressBook>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM address_book WHERE (?1 IS NULL OR account_id = ?1)
         ORDER BY is_default DESC, name COLLATE NOCASE ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![account_id])?;
    let mut books = Vec::new();
    while let Some(row) = rows.next()? {
        books.push(row_to_book(row)?);
    }
    Ok(books)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<AddressBook>> {
    let sql = format!("SELECT {COLUMNS} FROM address_book WHERE id = ?1");
    crate::repo::query_one(conn, &sql, params![id], row_to_book)
}

pub fn get_required(conn: &Connection, id: &str) -> Result<AddressBook> {
    get(conn, id)?.ok_or_else(|| AppError::NotFound(format!("address book {id} not found")))
}

pub fn default_for_account(conn: &Connection, account_id: &str) -> Result<Option<AddressBook>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM address_book WHERE account_id = ?1 ORDER BY is_default DESC, name ASC LIMIT 1"
    );
    crate::repo::query_one(conn, &sql, params![account_id], row_to_book)
}

pub fn insert(conn: &Connection, book: &mut AddressBook) -> Result<AddressBook> {
    if book.id.trim().is_empty() {
        book.id = new_id();
    }
    conn.execute(
        "INSERT INTO address_book (id, account_id, name, is_read_only, is_default, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
         ON CONFLICT(id) DO UPDATE SET
             account_id = excluded.account_id,
             name = excluded.name,
             is_read_only = excluded.is_read_only,
             is_default = excluded.is_default,
             updated_at = excluded.updated_at",
        params![
            book.id,
            book.account_id,
            book.name,
            book.is_read_only as i64,
            book.is_default as i64,
            now_string()
        ],
    )?;
    Ok(book.clone())
}

pub fn set_default(conn: &Connection, id: &str) -> Result<()> {
    let book = get_required(conn, id)?;
    conn.execute(
        "UPDATE address_book SET is_default = 0 WHERE account_id = ?1",
        params![book.account_id],
    )?;
    conn.execute(
        "UPDATE address_book SET is_default = 1 WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    let changed = conn.execute("DELETE FROM address_book WHERE id = ?1", params![id])?;
    if changed == 0 {
        return Err(AppError::NotFound(format!("address book {id} not found")));
    }
    Ok(())
}
