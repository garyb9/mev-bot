//! Single-writer actor for the SQLite store (SPEC-0004 §9).
//!
//! The hot path only ever hands a command to a bounded channel; a dedicated
//! thread owns the write connection and processes commands in order, assigning
//! event sequence numbers itself so replay ordering is independent of
//! timestamps. A full channel drops the write (and logs) rather than applying
//! backpressure to trading.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::{self, JoinHandle};

use tracing::{debug, error, warn};

use super::{Db, FillRecord, FundingRecord, OpenOrderRecord, OrderRecord, PositionRecord};
use crate::error::Result;

/// A command processed by the single writer thread.
#[derive(Debug)]
pub enum WriteCmd {
    /// Append an input event to the replay log.
    Event {
        /// Session id.
        session_id: i64,
        /// Event time in milliseconds.
        ts_ms: u64,
        /// Event kind tag.
        kind: String,
        /// JSON payload.
        payload: String,
    },
    /// Record an order lifecycle entry.
    Order {
        /// Session id.
        session_id: i64,
        /// The record.
        record: OrderRecord,
    },
    /// Record a fill.
    Fill {
        /// Session id.
        session_id: i64,
        /// The record.
        record: FillRecord,
    },
    /// Record a funding payment.
    Funding {
        /// Session id.
        session_id: i64,
        /// The record.
        record: FundingRecord,
    },
    /// Record a perp position snapshot.
    Positions {
        /// Session id.
        session_id: i64,
        /// Snapshot time in milliseconds.
        ts_ms: u64,
        /// Position rows.
        records: Vec<PositionRecord>,
    },
    /// Record an open-orders snapshot.
    OpenOrders {
        /// Session id.
        session_id: i64,
        /// Snapshot time in milliseconds.
        ts_ms: u64,
        /// Open-order rows.
        records: Vec<OpenOrderRecord>,
    },
    /// Stop the writer after draining everything queued ahead of it.
    Shutdown,
}

/// Handle to the writer thread. Dropping it does not stop the thread; call
/// [`DbWriter::shutdown`] for a clean, fully-drained stop.
pub struct DbWriter {
    tx: SyncSender<WriteCmd>,
    handle: Option<JoinHandle<()>>,
}

impl DbWriter {
    /// Spawn the writer with a bounded queue of `capacity` commands.
    pub fn spawn(db: Db, capacity: usize) -> Self {
        let (tx, rx) = sync_channel(capacity.max(1));
        let handle = thread::Builder::new()
            .name("db-writer".to_string())
            .spawn(move || run(db, &rx))
            .expect("spawn db-writer thread");
        Self {
            tx,
            handle: Some(handle),
        }
    }

    /// Enqueue a command without blocking. Returns `false` if the queue was full
    /// (command dropped) or the writer has stopped.
    pub fn try_send(&self, cmd: WriteCmd) -> bool {
        match self.tx.try_send(cmd) {
            Ok(()) => true,
            Err(TrySendError::Full(cmd)) => {
                warn!(
                    kind = cmd_kind(&cmd),
                    "db writer queue full; dropping write"
                );
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                error!("db writer thread has stopped");
                false
            }
        }
    }

    /// Enqueue a command, blocking until space is available. Use only off the
    /// latency-critical path.
    pub fn send(&self, cmd: WriteCmd) -> bool {
        self.tx.send(cmd).is_ok()
    }

    /// Drain the queue, stop the thread, and join it.
    pub fn shutdown(mut self) {
        let _ = self.tx.send(WriteCmd::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for DbWriter {
    fn drop(&mut self) {
        // Best-effort: request shutdown and join if the caller forgot.
        let _ = self.tx.send(WriteCmd::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn cmd_kind(cmd: &WriteCmd) -> &'static str {
    match cmd {
        WriteCmd::Event { .. } => "event",
        WriteCmd::Order { .. } => "order",
        WriteCmd::Fill { .. } => "fill",
        WriteCmd::Funding { .. } => "funding",
        WriteCmd::Positions { .. } => "positions",
        WriteCmd::OpenOrders { .. } => "open_orders",
        WriteCmd::Shutdown => "shutdown",
    }
}

fn run(db: Db, rx: &Receiver<WriteCmd>) {
    let mut seqs: HashMap<i64, u64> = HashMap::new();
    while let Ok(cmd) = rx.recv() {
        if matches!(cmd, WriteCmd::Shutdown) {
            break;
        }
        if let Err(err) = apply(&db, &mut seqs, cmd) {
            error!(error = %err, "db write failed");
        }
    }
    debug!("db writer stopped");
}

fn apply(db: &Db, seqs: &mut HashMap<i64, u64>, cmd: WriteCmd) -> Result<()> {
    match cmd {
        WriteCmd::Event {
            session_id,
            ts_ms,
            kind,
            payload,
        } => {
            let seq = match seqs.get(&session_id) {
                Some(seq) => *seq,
                None => {
                    let next = db.max_event_seq(session_id)?.map_or(0, |max| max + 1);
                    seqs.insert(session_id, next);
                    next
                }
            };
            db.append_event(session_id, seq, ts_ms, &kind, &payload)?;
            seqs.insert(session_id, seq + 1);
        }
        WriteCmd::Order { session_id, record } => db.insert_order(session_id, &record)?,
        WriteCmd::Fill { session_id, record } => db.insert_fill(session_id, &record)?,
        WriteCmd::Funding { session_id, record } => db.insert_funding(session_id, &record)?,
        WriteCmd::Positions {
            session_id,
            ts_ms,
            records,
        } => db.insert_positions(session_id, ts_ms, &records)?,
        WriteCmd::OpenOrders {
            session_id,
            ts_ms,
            records,
        } => db.insert_open_orders(session_id, ts_ms, &records)?,
        WriteCmd::Shutdown => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("mev-writer-{tag}-"))
            .tempdir()
            .unwrap();
        let path = dir.path().join("hlbot.db");
        (dir, path)
    }

    #[test]
    fn assigns_monotonic_sequences_and_drains_on_shutdown() {
        let (_dir, path) = temp_path("seq");
        let db = Db::open(&path).unwrap();
        let session = db.create_session("Testnet", "simulate", None, 0).unwrap();
        let writer = DbWriter::spawn(db, 8);

        for ts in 0..5 {
            assert!(writer.try_send(WriteCmd::Event {
                session_id: session,
                ts_ms: ts,
                kind: "book".into(),
                payload: format!(r#"{{"n":{ts}}}"#),
            }));
        }
        writer.shutdown();

        let db = Db::open(&path).unwrap();
        let events = db.read_events(session).unwrap();
        assert_eq!(events.len(), 5);
        for (i, event) in events.iter().enumerate() {
            assert_eq!(event.seq, i as u64);
            assert_eq!(event.ts_ms, i as u64);
        }
    }

    #[test]
    fn resumes_sequence_after_restart() {
        let (_dir, path) = temp_path("resume");
        let db = Db::open(&path).unwrap();
        let session = db.create_session("Testnet", "simulate", None, 0).unwrap();
        let writer = DbWriter::spawn(db, 4);
        writer.try_send(WriteCmd::Event {
            session_id: session,
            ts_ms: 1,
            kind: "book".into(),
            payload: "{}".into(),
        });
        writer.try_send(WriteCmd::Event {
            session_id: session,
            ts_ms: 2,
            kind: "book".into(),
            payload: "{}".into(),
        });
        writer.shutdown();

        // A fresh writer on the same session must continue past the last seq.
        let db = Db::open(&path).unwrap();
        let writer = DbWriter::spawn(db, 4);
        writer.try_send(WriteCmd::Event {
            session_id: session,
            ts_ms: 3,
            kind: "ctx".into(),
            payload: "{}".into(),
        });
        writer.shutdown();

        let db = Db::open(&path).unwrap();
        let events = db.read_events(session).unwrap();
        assert_eq!(
            events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(events[2].kind, "ctx");
    }

    #[test]
    fn records_trading_rows() {
        let (_dir, path) = temp_path("rows");
        let db = Db::open(&path).unwrap();
        let session = db.create_session("Mainnet", "live", None, 0).unwrap();
        let writer = DbWriter::spawn(db, 4);
        assert!(writer.try_send(WriteCmd::Fill {
            session_id: session,
            record: FillRecord {
                ts_ms: 9,
                coin: "BTC".into(),
                side: "sell".into(),
                px: "100".into(),
                sz: "0.5".into(),
                ..Default::default()
            },
        }));
        writer.shutdown();
    }
}
