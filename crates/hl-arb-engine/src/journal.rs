//! Optional action journal for replay (SPEC-0010 §14, E-7).
//!
//! In `simulate`/`replay` the dispatcher can be handed an [`ActionSink`]; it
//! records every risk-approved action with its cloid, price, and size, plus
//! every fill. That journal is the determinism artifact: the same input must
//! produce byte-identical entries (G-6). Live runs leave the sink `None`, so the
//! hot path is unchanged.
//!
//! The engine never does I/O here: it hands typed entries to the sink, and the
//! replay driver is free to buffer or serialize them.

use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::types::{Cloid, Px, Sz};

/// One journal line (SPEC-0010 §14).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalEntry {
    /// A place approved by risk, after cloid assignment and any resize.
    Place {
        /// Assigned client order id (`0x`-prefixed hex).
        cloid: String,
        /// Canonical coin name.
        coin: String,
        /// `buy` or `sell`.
        side: String,
        /// Limit price, or `null` for an aggressive (marketable) order.
        limit_px: Option<Px>,
        /// Order size.
        size: Sz,
    },
    /// A cancel approved by risk.
    Cancel {
        /// Client order id.
        cloid: String,
    },
    /// A modify approved by risk.
    Modify {
        /// Client order id.
        cloid: String,
        /// New limit price.
        px: Px,
        /// New size.
        sz: Sz,
    },
    /// A fill (from the paper backend in replay).
    Fill {
        /// Client order id, when the fill maps to one.
        cloid: Option<String>,
        /// Canonical coin name.
        coin: String,
        /// `buy` or `sell`.
        side: String,
        /// Fill price.
        px: Px,
        /// Fill size.
        sz: Sz,
        /// Fee paid.
        fee: Px,
    },
}

/// A consumer of journal entries (replay).
pub trait ActionSink: Send {
    /// Record one entry, in engine operation order.
    fn record(&mut self, entry: &JournalEntry);
}

/// An in-memory sink (tests and drivers that fingerprint or serialize later).
#[derive(Debug, Default)]
pub struct MemorySink {
    /// Entries in the order they were recorded.
    pub entries: Vec<JournalEntry>,
}

impl ActionSink for MemorySink {
    fn record(&mut self, entry: &JournalEntry) {
        self.entries.push(entry.clone());
    }
}

/// A shareable in-memory sink, so a driver or test can inspect the journal
/// after handing it to the dispatcher.
#[derive(Debug, Clone, Default)]
pub struct SharedSink {
    entries: Arc<Mutex<Vec<JournalEntry>>>,
}

impl SharedSink {
    /// An empty shared sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// A snapshot of the recorded entries.
    pub fn entries(&self) -> Vec<JournalEntry> {
        self.entries.lock().expect("journal mutex").clone()
    }
}

impl ActionSink for SharedSink {
    fn record(&mut self, entry: &JournalEntry) {
        self.entries
            .lock()
            .expect("journal mutex")
            .push(entry.clone());
    }
}

/// A convenience cloid renderer for sinks and tests.
pub fn cloid_hex(cloid: Cloid) -> String {
    cloid.to_hex()
}
