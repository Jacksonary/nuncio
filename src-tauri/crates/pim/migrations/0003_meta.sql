-- nuncio-pim: small key/value store for one-off data migrations that need
-- Rust-side parsing (raw iCalendar compaction).
CREATE TABLE IF NOT EXISTS pim_meta (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
