//! SQLite persistence (SPEC-0002 §5, SPEC-0003 §10, SPEC-0004 §9).
//!
//! A single connection guarded by a mutex from one writer task; WAL mode for
//! concurrent reads. This module owns the base schema (behind versioned
//! migrations), the durable `meta` key/value table used for high-water marks
//! such as the agent nonce, and the append-only event/record tables that back
//! deterministic replay. Writes from the hot path go through the
//! [`writer::DbWriter`] actor so they are serialized off the latency-critical
//! path.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::clock::{Clock, SystemClock};
use crate::error::{Error, Result};

pub mod writer;

/// Key under which the agent nonce high-water mark is stored.
pub const NONCE_LAST_KEY: &str = "nonce.last";

/// Schema migrations, applied in order. Index `i` is version `i + 1`. Every
/// statement is idempotent (`IF NOT EXISTS`) so pre-migration databases adopt
/// cleanly.
const MIGRATIONS: &[&str] = &[
    // v1 — meta, sessions, event log, and trading/accounting records.
    "
    CREATE TABLE IF NOT EXISTS meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS sessions (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        started_at_ms INTEGER NOT NULL,
        network       TEXT NOT NULL,
        mode          TEXT NOT NULL,
        note          TEXT
    );

    -- Input event log for deterministic replay. `seq` is assigned by the
    -- single writer, so ordering never depends on timestamps.
    CREATE TABLE IF NOT EXISTS events (
        session_id INTEGER NOT NULL REFERENCES sessions(id),
        seq        INTEGER NOT NULL,
        ts_ms      INTEGER NOT NULL,
        kind       TEXT NOT NULL,
        payload    TEXT NOT NULL,
        PRIMARY KEY (session_id, seq)
    );

    CREATE TABLE IF NOT EXISTS orders (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id  INTEGER NOT NULL REFERENCES sessions(id),
        ts_ms       INTEGER NOT NULL,
        strategy    TEXT,
        coin        TEXT NOT NULL,
        side        TEXT NOT NULL,
        kind        TEXT NOT NULL,
        cloid       TEXT,
        oid         INTEGER,
        px          TEXT,
        sz          TEXT,
        reduce_only INTEGER,
        rationale   TEXT,
        status      TEXT
    );

    CREATE TABLE IF NOT EXISTS fills (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id  INTEGER NOT NULL REFERENCES sessions(id),
        ts_ms       INTEGER NOT NULL,
        tid         INTEGER,
        oid         INTEGER,
        coin        TEXT NOT NULL,
        side        TEXT NOT NULL,
        px          TEXT NOT NULL,
        sz          TEXT NOT NULL,
        fee         TEXT,
        builder_fee TEXT,
        closed_pnl  TEXT,
        strategy    TEXT
    );

    CREATE TABLE IF NOT EXISTS funding (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id INTEGER NOT NULL REFERENCES sessions(id),
        ts_ms      INTEGER NOT NULL,
        coin       TEXT NOT NULL,
        usdc       TEXT NOT NULL,
        rate       TEXT,
        szi        TEXT
    );

    CREATE TABLE IF NOT EXISTS positions_snapshot (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id      INTEGER NOT NULL REFERENCES sessions(id),
        ts_ms           INTEGER NOT NULL,
        coin            TEXT NOT NULL,
        szi             TEXT NOT NULL,
        entry_px        TEXT,
        position_value  TEXT NOT NULL,
        unrealized_pnl  TEXT NOT NULL,
        margin_used     TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS open_orders_snapshot (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id  INTEGER NOT NULL REFERENCES sessions(id),
        ts_ms       INTEGER NOT NULL,
        coin        TEXT NOT NULL,
        oid         INTEGER,
        cloid       TEXT,
        side        TEXT NOT NULL,
        limit_px    TEXT NOT NULL,
        sz          TEXT NOT NULL,
        reduce_only INTEGER NOT NULL
    );

    CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id, seq);
    CREATE INDEX IF NOT EXISTS idx_orders_session ON orders(session_id, id);
    CREATE INDEX IF NOT EXISTS idx_fills_session  ON fills(session_id, id);
    ",
];

/// A SQLite handle with the bot's schema applied.
pub struct Db {
    conn: Connection,
}

/// One row of the input event log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRow {
    /// Monotonic sequence within the session.
    pub seq: u64,
    /// Event time in milliseconds.
    pub ts_ms: u64,
    /// Event kind tag (e.g. `book`, `ctx`, `account`, `timer`).
    pub kind: String,
    /// JSON-encoded payload.
    pub payload: String,
}

/// A recorded order lifecycle entry (intent, submit, cancel, or reject).
#[derive(Debug, Clone, Default)]
pub struct OrderRecord {
    /// Event time in milliseconds.
    pub ts_ms: u64,
    /// Owning strategy, if any.
    pub strategy: Option<String>,
    /// Coin.
    pub coin: String,
    /// `buy` or `sell`.
    pub side: String,
    /// `intent`, `submitted`, `cancel`, or `reject`.
    pub kind: String,
    /// Client order id.
    pub cloid: Option<String>,
    /// Exchange order id.
    pub oid: Option<u64>,
    /// Limit price as a decimal string.
    pub px: Option<String>,
    /// Size as a decimal string.
    pub sz: Option<String>,
    /// Reduce-only flag.
    pub reduce_only: Option<bool>,
    /// Audit rationale.
    pub rationale: Option<String>,
    /// Exchange status, if known.
    pub status: Option<String>,
}

/// A recorded fill.
#[derive(Debug, Clone, Default)]
pub struct FillRecord {
    /// Event time in milliseconds.
    pub ts_ms: u64,
    /// Trade id.
    pub tid: Option<u64>,
    /// Order id.
    pub oid: Option<u64>,
    /// Coin.
    pub coin: String,
    /// `buy` or `sell`.
    pub side: String,
    /// Fill price as a decimal string.
    pub px: String,
    /// Fill size as a decimal string.
    pub sz: String,
    /// Taker/maker fee as a decimal string.
    pub fee: Option<String>,
    /// Builder fee as a decimal string.
    pub builder_fee: Option<String>,
    /// Realized PnL as a decimal string.
    pub closed_pnl: Option<String>,
    /// Owning strategy, if any.
    pub strategy: Option<String>,
}

/// A recorded funding payment.
#[derive(Debug, Clone, Default)]
pub struct FundingRecord {
    /// Event time in milliseconds.
    pub ts_ms: u64,
    /// Coin.
    pub coin: String,
    /// Signed USDC amount as a decimal string.
    pub usdc: String,
    /// Funding rate as a decimal string.
    pub rate: Option<String>,
    /// Position size at settlement.
    pub szi: Option<String>,
}

/// A recorded perp position snapshot row.
#[derive(Debug, Clone, Default)]
pub struct PositionRecord {
    /// Coin.
    pub coin: String,
    /// Signed size as a decimal string.
    pub szi: String,
    /// Entry price.
    pub entry_px: Option<String>,
    /// Position value in USD.
    pub position_value: String,
    /// Unrealized PnL.
    pub unrealized_pnl: String,
    /// Margin used.
    pub margin_used: String,
}

/// A recorded open-order snapshot row.
#[derive(Debug, Clone, Default)]
pub struct OpenOrderRecord {
    /// Coin.
    pub coin: String,
    /// Order id.
    pub oid: Option<u64>,
    /// Client order id.
    pub cloid: Option<String>,
    /// `buy` or `sell`.
    pub side: String,
    /// Limit price as a decimal string.
    pub limit_px: String,
    /// Remaining size as a decimal string.
    pub sz: String,
    /// Reduce-only flag.
    pub reduce_only: bool,
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
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|e| Error::Config(format!("setting busy timeout: {e}")))?;
        apply_migrations(&conn)?;
        Ok(Self { conn })
    }

    /// The highest applied schema migration version.
    pub fn schema_version(&self) -> Result<u32> {
        self.conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |row| row.get::<_, u32>(0),
            )
            .map_err(|e| Error::Config(format!("reading schema version: {e}")))
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

    /// Open a new session and return its id.
    pub fn create_session(
        &self,
        network: &str,
        mode: &str,
        note: Option<&str>,
        started_at_ms: u64,
    ) -> Result<i64> {
        self.conn
            .execute(
                "INSERT INTO sessions (started_at_ms, network, mode, note) VALUES (?1, ?2, ?3, ?4)",
                params![started_at_ms as i64, network, mode, note],
            )
            .map_err(|e| Error::Config(format!("creating session: {e}")))?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The most recently created session id, if any.
    pub fn latest_session(&self) -> Result<Option<i64>> {
        self.conn
            .query_row("SELECT MAX(id) FROM sessions", [], |row| {
                row.get::<_, Option<i64>>(0)
            })
            .map_err(|e| Error::Config(format!("reading latest session: {e}")))
    }

    /// The highest recorded event sequence for a session, if any.
    pub fn max_event_seq(&self, session_id: i64) -> Result<Option<u64>> {
        self.conn
            .query_row(
                "SELECT MAX(seq) FROM events WHERE session_id = ?1",
                params![session_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map(|value| value.map(|v| v as u64))
            .map_err(|e| Error::Config(format!("reading max event seq: {e}")))
    }

    /// Append an event at an explicit sequence number.
    pub fn append_event(
        &self,
        session_id: i64,
        seq: u64,
        ts_ms: u64,
        kind: &str,
        payload: &str,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO events (session_id, seq, ts_ms, kind, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![session_id, seq as i64, ts_ms as i64, kind, payload],
            )
            .map_err(|e| Error::Config(format!("appending event: {e}")))?;
        Ok(())
    }

    /// Read a session's events in sequence order.
    pub fn read_events(&self, session_id: i64) -> Result<Vec<EventRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT seq, ts_ms, kind, payload FROM events
                 WHERE session_id = ?1 ORDER BY seq ASC",
            )
            .map_err(|e| Error::Config(format!("preparing event read: {e}")))?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok(EventRow {
                    seq: row.get::<_, i64>(0)? as u64,
                    ts_ms: row.get::<_, i64>(1)? as u64,
                    kind: row.get(2)?,
                    payload: row.get(3)?,
                })
            })
            .map_err(|e| Error::Config(format!("reading events: {e}")))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| Error::Config(format!("decoding events: {e}")))
    }

    /// Insert an order lifecycle record.
    pub fn insert_order(&self, session_id: i64, record: &OrderRecord) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO orders
                    (session_id, ts_ms, strategy, coin, side, kind, cloid, oid, px, sz, reduce_only, rationale, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    session_id,
                    record.ts_ms as i64,
                    record.strategy,
                    record.coin,
                    record.side,
                    record.kind,
                    record.cloid,
                    record.oid.map(|v| v as i64),
                    record.px,
                    record.sz,
                    record.reduce_only.map(|v| v as i64),
                    record.rationale,
                    record.status,
                ],
            )
            .map_err(|e| Error::Config(format!("inserting order: {e}")))?;
        Ok(())
    }

    /// Insert a fill record.
    pub fn insert_fill(&self, session_id: i64, record: &FillRecord) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO fills
                    (session_id, ts_ms, tid, oid, coin, side, px, sz, fee, builder_fee, closed_pnl, strategy)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    session_id,
                    record.ts_ms as i64,
                    record.tid.map(|v| v as i64),
                    record.oid.map(|v| v as i64),
                    record.coin,
                    record.side,
                    record.px,
                    record.sz,
                    record.fee,
                    record.builder_fee,
                    record.closed_pnl,
                    record.strategy,
                ],
            )
            .map_err(|e| Error::Config(format!("inserting fill: {e}")))?;
        Ok(())
    }

    /// Insert a funding-payment record.
    pub fn insert_funding(&self, session_id: i64, record: &FundingRecord) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO funding (session_id, ts_ms, coin, usdc, rate, szi)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    session_id,
                    record.ts_ms as i64,
                    record.coin,
                    record.usdc,
                    record.rate,
                    record.szi,
                ],
            )
            .map_err(|e| Error::Config(format!("inserting funding: {e}")))?;
        Ok(())
    }

    /// Insert a position snapshot atomically.
    pub fn insert_positions(
        &self,
        session_id: i64,
        ts_ms: u64,
        records: &[PositionRecord],
    ) -> Result<()> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| Error::Config(format!("begin positions snapshot: {e}")))?;
        for record in records {
            tx.execute(
                "INSERT INTO positions_snapshot
                    (session_id, ts_ms, coin, szi, entry_px, position_value, unrealized_pnl, margin_used)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    session_id,
                    ts_ms as i64,
                    record.coin,
                    record.szi,
                    record.entry_px,
                    record.position_value,
                    record.unrealized_pnl,
                    record.margin_used,
                ],
            )
            .map_err(|e| Error::Config(format!("inserting position snapshot: {e}")))?;
        }
        tx.commit()
            .map_err(|e| Error::Config(format!("commit positions snapshot: {e}")))?;
        Ok(())
    }

    /// Insert an open-orders snapshot atomically.
    pub fn insert_open_orders(
        &self,
        session_id: i64,
        ts_ms: u64,
        records: &[OpenOrderRecord],
    ) -> Result<()> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| Error::Config(format!("begin open-orders snapshot: {e}")))?;
        for record in records {
            tx.execute(
                "INSERT INTO open_orders_snapshot
                    (session_id, ts_ms, coin, oid, cloid, side, limit_px, sz, reduce_only)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    session_id,
                    ts_ms as i64,
                    record.coin,
                    record.oid.map(|v| v as i64),
                    record.cloid,
                    record.side,
                    record.limit_px,
                    record.sz,
                    record.reduce_only as i64,
                ],
            )
            .map_err(|e| Error::Config(format!("inserting open-order snapshot: {e}")))?;
        }
        tx.commit()
            .map_err(|e| Error::Config(format!("commit open-orders snapshot: {e}")))?;
        Ok(())
    }
}

/// Apply every migration newer than the recorded schema version.
fn apply_migrations(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
             version       INTEGER PRIMARY KEY,
             applied_at_ms INTEGER NOT NULL
         );",
    )
    .map_err(|e| Error::Config(format!("creating schema_version: {e}")))?;

    let current: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .map_err(|e| Error::Config(format!("reading schema version: {e}")))?;

    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = index as i64 + 1;
        if version <= current {
            continue;
        }
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| Error::Config(format!("begin migration {version}: {e}")))?;
        tx.execute_batch(sql)
            .map_err(|e| Error::Config(format!("applying migration {version}: {e}")))?;
        tx.execute(
            "INSERT INTO schema_version (version, applied_at_ms) VALUES (?1, ?2)",
            params![version, SystemClock.now_ms() as i64],
        )
        .map_err(|e| Error::Config(format!("recording migration {version}: {e}")))?;
        tx.commit()
            .map_err(|e| Error::Config(format!("commit migration {version}: {e}")))?;
    }
    Ok(())
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
    fn survives_reopen_and_migrates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hlbot.db");
        {
            let db = Db::open(&path).unwrap();
            db.set_nonce_last(42).unwrap();
            assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as u32);
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.nonce_last().unwrap(), Some(42));
        // Re-opening applies no further migrations.
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as u32);
    }

    #[test]
    fn events_round_trip_in_sequence() {
        let db = Db::open_in_memory().unwrap();
        let session = db
            .create_session("Testnet", "simulate", Some("t"), 1_000)
            .unwrap();
        assert_eq!(db.latest_session().unwrap(), Some(session));
        assert_eq!(db.max_event_seq(session).unwrap(), None);

        db.append_event(session, 0, 10, "book", r#"{"coin":"BTC"}"#)
            .unwrap();
        db.append_event(session, 1, 20, "ctx", r#"{"coin":"BTC"}"#)
            .unwrap();

        let events = db.read_events(session).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, 0);
        assert_eq!(events[0].kind, "book");
        assert_eq!(events[1].seq, 1);
        assert_eq!(db.max_event_seq(session).unwrap(), Some(1));
    }

    #[test]
    fn trading_records_insert() {
        let db = Db::open_in_memory().unwrap();
        let session = db.create_session("Mainnet", "live", None, 0).unwrap();

        db.insert_order(
            session,
            &OrderRecord {
                ts_ms: 1,
                coin: "BTC".into(),
                side: "buy".into(),
                kind: "intent".into(),
                ..Default::default()
            },
        )
        .unwrap();
        db.insert_fill(
            session,
            &FillRecord {
                ts_ms: 2,
                coin: "BTC".into(),
                side: "buy".into(),
                px: "100".into(),
                sz: "1".into(),
                ..Default::default()
            },
        )
        .unwrap();
        db.insert_funding(
            session,
            &FundingRecord {
                ts_ms: 3,
                coin: "BTC".into(),
                usdc: "0.5".into(),
                ..Default::default()
            },
        )
        .unwrap();
        db.insert_positions(
            session,
            4,
            &[PositionRecord {
                coin: "BTC".into(),
                szi: "1".into(),
                position_value: "100".into(),
                unrealized_pnl: "0".into(),
                margin_used: "10".into(),
                ..Default::default()
            }],
        )
        .unwrap();
        db.insert_open_orders(
            session,
            5,
            &[OpenOrderRecord {
                coin: "BTC".into(),
                side: "buy".into(),
                limit_px: "99".into(),
                sz: "1".into(),
                ..Default::default()
            }],
        )
        .unwrap();
    }
}
