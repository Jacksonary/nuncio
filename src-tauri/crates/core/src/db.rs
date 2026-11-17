//! SQLite connection pool, pragmas, migrations and async helpers.
//!
//! * One shared pool per process, opened with WAL/pragmas suited for a desktop
//!   app (`cache_size=-8000`, `foreign_keys=ON`, `busy_timeout=5000`).
//! * Reads run concurrently on pool connections; writes are serialized through
//!   an async-friendly mutex and wrapped in a transaction.
//! * `migrate` keeps a `schema_migrations(namespace, version)` ledger so each
//!   crate can own its own migrations without touching core's.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OpenFlags, Transaction};
use tracing::{debug, info};

use crate::error::AppError;
use crate::Result;

/// Core migration namespace.
pub const CORE_NAMESPACE: &str = "core";

/// Core migrations (schema for every module + built-in seed rows).
pub const CORE_MIGRATIONS: &[(u32, &str)] = &[
    (1, include_str!("../migrations/0001_core.sql")),
    (
        2,
        include_str!("../migrations/0002_reminder_state_kind.sql"),
    ),
    (
        3,
        include_str!("../migrations/0003_remote_images_default.sql"),
    ),
    (
        4,
        include_str!("../migrations/0004_outbox_sending_lease.sql"),
    ),
    (5, include_str!("../migrations/0005_account_sent_copy.sql")),
];

const PRAGMAS: &str = "\
    PRAGMA journal_mode=WAL;\
    PRAGMA synchronous=NORMAL;\
    PRAGMA foreign_keys=ON;\
    PRAGMA busy_timeout=5000;\
    PRAGMA cache_size=-8000;\
    PRAGMA temp_store=FILE;\
";

fn init_connection(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.execute_batch(PRAGMAS)
}

/// Cheap clonable handle to the shared SQLite pool.
#[derive(Clone)]
pub struct Db {
    pool: Arc<Pool<SqliteConnectionManager>>,
    write_lock: Arc<Mutex<()>>,
    /// Keeps the shared in-memory database alive (ignored for file databases).
    anchor: Option<Arc<Mutex<Connection>>>,
    path: Option<PathBuf>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("path", &self.path)
            .field("in_memory", &self.anchor.is_some())
            .finish()
    }
}

impl Db {
    /// Open (or create) the database file at `path`, creating parent folders.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let manager = SqliteConnectionManager::file(path).with_init(init_connection);
        let pool = Pool::builder()
            .max_size(8)
            .min_idle(Some(1))
            .build(manager)
            .map_err(AppError::from)?;
        info!(path = %path.display(), "opened database");
        Ok(Self {
            pool: Arc::new(pool),
            write_lock: Arc::new(Mutex::new(())),
            anchor: None,
            path: Some(path.to_path_buf()),
        })
    }

    /// Open a private in-memory database (for tests and ephemeral work).
    ///
    /// All pooled connections share the same memory database through SQLite's
    /// shared cache; an anchor connection keeps it alive for the lifetime of
    /// the `Db`.
    pub fn open_in_memory() -> Result<Self> {
        let name = format!("nuncio-mem-{}", uuid::Uuid::new_v4());
        let uri = format!("file:{name}?mode=memory&cache=shared");
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;

        let mut anchor = Connection::open_with_flags(&uri, flags)?;
        init_connection(&mut anchor)?;

        let manager = SqliteConnectionManager::file(&uri)
            .with_flags(flags)
            .with_init(init_connection);
        let pool = Pool::builder()
            .max_size(4)
            .min_idle(Some(1))
            .build(manager)
            .map_err(AppError::from)?;

        debug!("opened in-memory database");
        Ok(Self {
            pool: Arc::new(pool),
            write_lock: Arc::new(Mutex::new(())),
            anchor: Some(Arc::new(Mutex::new(anchor))),
            path: None,
        })
    }

    /// Run every core migration (idempotent).
    pub fn migrate_core(&self) -> Result<()> {
        let applied = self.migrate(CORE_NAMESPACE, CORE_MIGRATIONS)?;
        if !applied.is_empty() {
            info!(versions = ?applied, "applied core migrations");
        }
        Ok(())
    }

    /// Apply pending migrations for `namespace`.
    ///
    /// `migrations` is a list of `(version, sql)` pairs; each version is applied
    /// at most once and recorded in `schema_migrations`.
    pub fn migrate(&self, namespace: &str, migrations: &[(u32, &str)]) -> Result<Vec<u32>> {
        let _guard = self.lock_writes();
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                 namespace TEXT NOT NULL,
                 version INTEGER NOT NULL,
                 applied_at TEXT NOT NULL,
                 PRIMARY KEY (namespace, version)
             );",
        )?;

        let mut ordered: Vec<&(u32, &str)> = migrations.iter().collect();
        ordered.sort_by_key(|(version, _)| *version);

        let mut applied = Vec::new();
        for (version, sql) in ordered {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE namespace = ?1 AND version = ?2)",
                rusqlite::params![namespace, version],
                |row| row.get(0),
            )?;
            if exists {
                continue;
            }
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT INTO schema_migrations (namespace, version, applied_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![namespace, version, crate::repo::now_string()],
            )?;
            applied.push(*version);
        }
        tx.commit()?;
        Ok(applied)
    }

    /// Check whether a migration version has been applied.
    pub fn migration_applied(&self, namespace: &str, version: u32) -> Result<bool> {
        let conn = self.connection()?;
        ensure_migration_table(&conn)?;
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE namespace = ?1 AND version = ?2)",
            rusqlite::params![namespace, version],
            |row| row.get(0),
        )?;
        Ok(exists)
    }

    /// Checkout a pooled connection (blocks until one is available).
    pub fn connection(&self) -> Result<PooledConnection<SqliteConnectionManager>> {
        self.pool.get().map_err(AppError::from)
    }

    /// Location of the database file, or `None` for in-memory databases.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn is_in_memory(&self) -> bool {
        self.anchor.is_some()
    }

    fn lock_writes(&self) -> std::sync::MutexGuard<'_, ()> {
        // A panic inside a write closure should not poison every later write.
        self.write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Run a synchronous read closure on the current thread.
    pub fn read_sync<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.connection()?;
        f(&conn)
    }

    /// Run a synchronous write closure in a transaction on the current thread.
    pub fn write_sync<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let _guard = self.lock_writes();
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    /// Run a synchronous read closure inside an explicit transaction (read
    /// snapshot across several queries).
    pub fn read_tx_sync<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    /// Run an explicit write transaction on the current thread.
    pub fn transaction<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let _guard = self.lock_writes();
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    /// Async read helper: the closure runs on a blocking thread.
    pub async fn read<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get().map_err(AppError::from)?;
            f(&conn)
        })
        .await
        .map_err(AppError::from)?
    }

    /// Async write helper: writes are serialized and run in a transaction.
    pub async fn write<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let pool = self.pool.clone();
        let lock = self.write_lock.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut conn = pool.get().map_err(AppError::from)?;
            let tx = conn.transaction()?;
            let value = f(&tx)?;
            tx.commit()?;
            Ok(value)
        })
        .await
        .map_err(AppError::from)?
    }

    /// Async helper running `f` inside a read-only transaction.
    pub async fn read_tx<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Transaction<'_>) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(AppError::from)?;
            let tx = conn.transaction()?;
            let value = f(&tx)?;
            tx.commit()?;
            Ok(value)
        })
        .await
        .map_err(AppError::from)?
    }
}

fn ensure_migration_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             namespace TEXT NOT NULL,
             version INTEGER NOT NULL,
             applied_at TEXT NOT NULL,
             PRIMARY KEY (namespace, version)
         );",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    #[test]
    fn opens_in_memory_and_runs_pragmas() {
        let db = Db::open_in_memory().expect("db");
        let (mode, fk, cache): (String, i64, i64) = db
            .read_sync(|conn| {
                Ok((
                    conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?,
                    conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?,
                    conn.query_row("PRAGMA cache_size", [], |r| r.get(0))?,
                ))
            })
            .expect("pragmas");
        assert_eq!(fk, 1);
        assert_eq!(cache, -8000);
        assert!(mode == "memory" || mode == "wal");
    }

    #[test]
    fn migrations_are_idempotent_and_namespaced() {
        let db = Db::open_in_memory().expect("db");
        let migrations = &[(1u32, "CREATE TABLE demo (id TEXT PRIMARY KEY);")];
        assert_eq!(db.migrate("demo", migrations).expect("first"), vec![1]);
        assert!(db.migrate("demo", migrations).expect("second").is_empty());
        assert!(db.migration_applied("demo", 1).expect("applied"));
        assert!(!db.migration_applied("other", 1).expect("not applied"));

        // A different namespace applies the same version independently.
        let other = &[(1u32, "CREATE TABLE other_demo (id TEXT PRIMARY KEY);")];
        assert_eq!(db.migrate("other", other).expect("other"), vec![1]);
    }

    #[test]
    fn write_sync_is_atomic() {
        let db = Db::open_in_memory().expect("db");
        db.write_sync(|conn| {
            conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL);")?;
            Ok(())
        })
        .expect("schema");

        let failed: Result<()> = db.write_sync(|conn| {
            conn.execute("INSERT INTO t (id, v) VALUES (1, 'a')", [])?;
            Err(AppError::invalid_input("rollback"))
        });
        assert!(failed.is_err());
        let count: i64 = db
            .read_sync(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))?))
            .expect("count");
        assert_eq!(count, 0);
    }

    #[test]
    fn writes_are_serialized_across_threads() {
        let db = Db::open_in_memory().expect("db");
        db.write_sync(|conn| {
            conn.execute_batch("CREATE TABLE counter (id INTEGER PRIMARY KEY, n INTEGER NOT NULL); INSERT INTO counter (id, n) VALUES (1, 0);")?;
            Ok(())
        })
        .expect("schema");

        let mut handles = Vec::new();
        for _ in 0..8 {
            let db = db.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..25 {
                    db.write_sync(|conn| {
                        conn.execute("UPDATE counter SET n = n + 1 WHERE id = 1", [])?;
                        Ok(())
                    })
                    .expect("update");
                }
            }));
        }
        for handle in handles {
            handle.join().expect("join");
        }
        let n: i64 = db
            .read_sync(|conn| {
                Ok(conn.query_row("SELECT n FROM counter WHERE id = 1", [], |r| r.get(0))?)
            })
            .expect("n");
        assert_eq!(n, 200);
    }

    #[test]
    fn file_database_round_trip() {
        let dir = std::env::temp_dir().join(format!("nuncio-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("nuncio.db");
        {
            let db = Db::open(&path).expect("open");
            db.write_sync(|conn| {
                conn.execute_batch("CREATE TABLE demo (id TEXT PRIMARY KEY, v INTEGER);")?;
                conn.execute("INSERT INTO demo (id, v) VALUES (?1, ?2)", params!["a", 1])?;
                Ok(())
            })
            .expect("write");
        }
        {
            let db = Db::open(&path).expect("reopen");
            let v: i64 = db
                .read_sync(|conn| {
                    Ok(conn.query_row("SELECT v FROM demo WHERE id = 'a'", [], |r| r.get(0))?)
                })
                .expect("read");
            assert_eq!(v, 1);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn async_helpers_work() {
        let db = Db::open_in_memory().expect("db");
        db.write(|conn| {
            conn.execute_batch("CREATE TABLE demo (id TEXT PRIMARY KEY, v INTEGER);")?;
            Ok(())
        })
        .await
        .expect("schema");
        db.write(|conn| {
            conn.execute("INSERT INTO demo (id, v) VALUES ('x', 42)", [])?;
            Ok(())
        })
        .await
        .expect("insert");
        let v: i64 = db
            .read(|conn| {
                Ok(conn.query_row("SELECT v FROM demo WHERE id = 'x'", [], |row| row.get(0))?)
            })
            .await
            .expect("read");
        assert_eq!(v, 42);
    }
}
