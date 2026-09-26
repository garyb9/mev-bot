//! SQLite persistence (SPEC-0002 §5, SPEC-0004).
//!
//! A single connection guarded by a mutex from one writer task; WAL mode for
//! concurrent reads. This module currently owns the small `meta` key/value
//! table used for durable high-water marks such as the agent nonce; trading and
//! accounting tables arrive with SPEC-0004.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};

/// Key under which the agent nonce high-water mark is stored.
pub const NONCE_LAST_KEY: &str = "nonce.last";

/// A SQLite handle with the bot's base schema applied.
pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open (creating if needed) the database at `path` with WAL enabled.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Config(format!("creating {}: {e}", parent.display())))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| Error::Config(format!("opening {}: {e}", path.display())))?;
        Self::from_connection(conn)
    }

    /// Open an ephemeral in-memory database (tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| Error::Config(format!("opening in-memory db: {e}")))?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| Error::Config(format!("enabling WAL: {e}")))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| Error::Config(format!("enabling foreign keys: {e}")))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )
        .map_err(|e| Error::Config(format!("applying schema: {e}")))?;
        Ok(Self { conn })
    }

    /// Read a `meta` value.
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| Error::Config(format!("reading meta `{key}`: {e}")))
    }

    /// Write a `meta` value (upsert).
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(|e| Error::Config(format!("writing meta `{key}`: {e}")))?;
        Ok(())
    }

    /// Read the persisted nonce high-water mark.
    pub fn nonce_last(&self) -> Result<Option<u64>> {
        match self.meta(NONCE_LAST_KEY)? {
            Some(value) => value
                .parse::<u64>()
                .map(Some)
                .map_err(|e| Error::Config(format!("invalid stored nonce `{value}`: {e}"))),
            None => Ok(None),
        }
    }

    /// Persist the nonce high-water mark (write-ahead of the send).
    pub fn set_nonce_last(&self, nonce: u64) -> Result<()> {
        self.set_meta(NONCE_LAST_KEY, &nonce.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_round_trips_and_upserts() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.meta("k").unwrap(), None);
        db.set_meta("k", "1").unwrap();
        assert_eq!(db.meta("k").unwrap().as_deref(), Some("1"));
        db.set_meta("k", "2").unwrap();
        assert_eq!(db.meta("k").unwrap().as_deref(), Some("2"));
    }

    #[test]
    fn nonce_round_trips() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.nonce_last().unwrap(), None);
        db.set_nonce_last(1_700_000_000_000).unwrap();
        assert_eq!(db.nonce_last().unwrap(), Some(1_700_000_000_000));
    }

    #[test]
    fn survives_reopen() {
        let dir = std::env::temp_dir().join(format!("mev-db-{}", std::process::id()));
        let path = dir.join("hlbot.db");
        {
            let db = Db::open(&path).unwrap();
            db.set_nonce_last(42).unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.nonce_last().unwrap(), Some(42));
        std::fs::remove_dir_all(&dir).ok();
    }
}
