//! Deterministic replay of recorder segments through the v2 engine
//! (SPEC-0010 §13/§14, task E-7 part 2).
//!
//! The driver reads SPEC-0008 segments (R-7), decodes their frames with the
//! same typed ingest decoders the live path uses, and drives
//! `EngineLoop<StrategyDispatcher>` with a [`ReplayClock`] advanced to each
//! event's recorded time. The exec backend is [`PaperExec`] and the cloid prefix
//! is pinned, so the action journal is byte-identical across runs.
//!
//! The recorder and the engine are separate processes; replay opens no sockets
//! and loads no keys.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use hl_arb_client::MarketSelector;
use hl_arb_core::config::{Config, Network};
use hl_arb_engine::builder::AssetTable;
use hl_arb_engine::channels::inputs;
use hl_arb_engine::clock::ReplayClock;
use hl_arb_engine::dispatch::{DispatcherConfig, StrategyDispatcher};
use hl_arb_engine::ingest::Ingest;
use hl_arb_engine::journal::{JournalEntry, SharedSink};
use hl_arb_engine::paper_exec::{PaperConfig, PaperExec};
use hl_arb_engine::risk::RiskGate;
use hl_arb_engine::run::{EngineLoop, LoopConfig};
use hl_arb_engine::types::{ConnId, MarketUpdate, Stamp};
use hl_arb_recorder::envelope::Kind;
use hl_arb_recorder::reader::merge_segments_iter;
use hl_arb_strategy::{AccountView, FeeRates};
use rust_decimal::Decimal;
use tracing::info;

use crate::record;

/// One segment-replay request (SPEC-0010 §14).
pub struct ReplayRequest {
    /// Recorder output root (`data/rec`).
    pub out_dir: PathBuf,
    /// Inclusive UTC start date, `YYYY-MM-DD`.
    pub from: String,
    /// Inclusive UTC end date, `YYYY-MM-DD`.
    pub to: String,
    /// Where to write the action journal as JSONL, if anywhere.
    pub out: Option<PathBuf>,
    /// Deterministic cloid prefix (default 0).
    pub cloid_prefix: u64,
    /// Simulated one-way latency in milliseconds.
    pub latency_ms: u64,
    /// Starting paper account value.
    pub account_value: Decimal,
}

/// What a segment replay produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayOutcome {
    /// Segment files read.
    pub segments: usize,
    /// Text frames decoded.
    pub frames: usize,
    /// Journal entries (approved actions + fills).
    pub entries: usize,
    /// FNV-1a fingerprint of the serialized action journal.
    pub fingerprint: u64,
}

/// Replay recorded segments through the real engine and return a summary.
pub fn replay_segments(
    cfg: &Config,
    selector: &MarketSelector,
    req: &ReplayRequest,
) -> Result<ReplayOutcome> {
    let build = crate::engine::build(cfg, selector)?;
    let registry = build.registry.clone();
    let coin_count = registry.len();
    let table = AssetTable::from_selector(&registry, selector);

    let paper = PaperExec::new(
        PaperConfig {
            latency_ms: req.latency_ms,
            maker_fills: true,
        },
        AccountView {
            account_value: req.account_value,
            fees: FeeRates::PERP,
            ..Default::default()
        },
        build.instruments.clone(),
        FeeRates::PERP,
        FeeRates::SPOT,
    );
    let journal = SharedSink::new();
    let clock = Arc::new(ReplayClock::new());

    let dispatcher = StrategyDispatcher::new(
        build.strategies,
        registry.clone(),
        table,
        RiskGate::from_settings(&cfg.risk),
        None,
        DispatcherConfig {
            max_slippage_bps: Decimal::from(cfg.strategy.max_slippage_bps),
        },
    )
    .with_paper(paper)
    .with_clock(clock.clone())
    .with_journal(Box::new(journal.clone()))
    .with_cloid_prefix(req.cloid_prefix);

    let segments =
        hl_arb_recorder::segments_for(&req.out_dir, network_dir(cfg.network), &req.from, &req.to)
            .with_context(|| format!("enumerating segments under {}", req.out_dir.display()))?;

    let (handles, inputs) = inputs(65_536, 16_384);
    let (_stop_tx, stop_rx) = crossbeam_channel::bounded(1);
    let mut engine = EngineLoop::with_dispatcher(
        inputs,
        dispatcher,
        LoopConfig {
            spin_us: 0,
            coin_count,
        },
        stop_rx,
    )
    .with_clock(clock.clone());

    let ingest = Ingest::new(ConnId(0), registry);
    let mut conns: BTreeMap<String, ConnId> = BTreeMap::new();
    let mut frames = 0usize;
    let mut last_mono = 0u64;

    for envelope in merge_segments_iter(&segments)? {
        let envelope = envelope?;
        let conn = match conns.get(&envelope.conn) {
            Some(conn) => *conn,
            None => {
                let conn = ConnId(conns.len() as u16);
                conns.insert(envelope.conn.clone(), conn);
                conn
            }
        };
        // Keep monotonic time non-decreasing across process restarts, so
        // replayed timers still fire after a recorder restart.
        let mono = envelope.mono_ns.max(last_mono);
        last_mono = mono;
        let stamp = Stamp {
            t_recv_ns: envelope.t_ns,
            mono_ns: mono,
            ts_exch_ms: 0,
        };
        match envelope.kind {
            Kind::Frame => {
                if let Some(raw) = envelope.raw.as_deref() {
                    frames += 1;
                    if let Ok(Some(update)) = ingest.decode(raw, stamp) {
                        let _ = handles.send_market(update);
                    }
                }
            }
            Kind::GapStart => {
                let _ = handles.send_market(MarketUpdate::Gap {
                    conn,
                    stamp,
                    open: true,
                });
            }
            Kind::GapEnd => {
                let _ = handles.send_market(MarketUpdate::Gap {
                    conn,
                    stamp,
                    open: false,
                });
            }
            // REST bodies, subscribe/connect/clock lines and binary frames are
            // not part of the v1 market replay (SPEC-0010 §14).
            _ => {}
        }
        clock.set(stamp);
        engine.iterate(stamp.mono_ns);
    }

    let entries = journal.entries();
    let bytes = serialize_journal(&entries);
    let fingerprint = fnv1a(&bytes);
    if let Some(path) = &req.out {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    }

    info!(
        segments = segments.len(),
        frames,
        entries = entries.len(),
        fingerprint = format!("0x{fingerprint:016x}"),
        "segment replay complete"
    );
    Ok(ReplayOutcome {
        segments: segments.len(),
        frames,
        entries: entries.len(),
        fingerprint,
    })
}

/// The network directory name (SPEC-0008 §6).
pub(crate) fn network_dir(network: Network) -> &'static str {
    record::network_dir(network)
}

/// Serialize the journal as newline-delimited JSON (one entry per line).
pub fn serialize_journal(entries: &[JournalEntry]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for entry in entries {
        serde_json::to_writer(&mut bytes, entry).expect("journal entries serialize");
        bytes.push(b'\n');
    }
    bytes
}

/// FNV-1a 64-bit fingerprint over the serialized journal.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::Arc;

    use hl_arb_client::AssetMap;
    use hl_arb_client::types::{AssetMeta, Meta};
    use hl_arb_core::config::{Config, MmSettings, Network, StrategyConfig};
    use hl_arb_recorder::envelope::{Envelope, FixedEnvelopeClock};
    use hl_arb_recorder::segment::{SegmentConfig, SegmentWriter};

    use super::*;

    fn ds(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn temp_dir(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("mev-replay-{tag}-"))
            .tempdir()
            .unwrap()
    }

    fn btc_asset_map() -> AssetMap {
        let mut map = AssetMap::new();
        map.insert_perp_dex(
            None,
            None,
            &Meta {
                universe: vec![AssetMeta {
                    name: "BTC".into(),
                    sz_decimals: 3,
                    max_leverage: 40,
                    is_delisted: false,
                    only_isolated: false,
                }],
            },
        );
        map
    }

    fn mm_config() -> Config {
        let market_making = MmSettings {
            coins: vec!["BTC".to_string()],
            size_per_level: Decimal::ONE,
            // Never pull on a wide spread: the fixture book has a ~1% spread.
            vol_pull_bps: 100_000,
            ..MmSettings::default()
        };
        let strategy = StrategyConfig {
            enabled: vec!["market_making".to_string()],
            market_making,
            ..StrategyConfig::default()
        };
        Config {
            network: Network::Testnet,
            watchlist: vec!["BTC".to_string()],
            strategy,
            ..Config::default()
        }
    }

    fn book_frame(bid: &str, ask: &str) -> String {
        format!(
            r#"{{"channel":"l2Book","data":{{"coin":"BTC","time":1,"levels":[[{{"px":"{bid}","sz":"1","n":1}}],[{{"px":"{ask}","sz":"1","n":1}}]]}}}}"#
        )
    }

    /// Write a small l2Book fixture segment and return its directory + date.
    fn write_fixture(dir: &std::path::Path) -> String {
        let base = 1_700_000_000_000_000_000i64; // 2023-11-14 UTC
        let clock = Arc::new(FixedEnvelopeClock::new(base, 0));
        let writer = SegmentWriter::spawn(SegmentConfig {
            out_dir: dir.to_path_buf(),
            network: "testnet".into(),
            src: "hl-ws".into(),
            conn: "hl-ws-01".into(),
            clock: clock.clone(),
            ..SegmentConfig::default()
        })
        .unwrap();
        for (i, (bid, ask)) in [("100", "101"), ("100.5", "101.5"), ("101", "102")]
            .iter()
            .enumerate()
        {
            clock.set_t_ns(base + i as i64 * 1_000_000);
            assert!(writer.try_send(Envelope::frame(
                &*clock,
                "hl-ws",
                "hl-ws-01",
                i as u64,
                book_frame(bid, ask),
            )));
        }
        writer.shutdown().unwrap();
        "2023-11-14".to_string()
    }

    #[test]
    fn segment_replay_is_byte_identical_across_runs() {
        let tmp = temp_dir("determinism");
        let dir = tmp.path();
        let date = write_fixture(dir);
        let cfg = mm_config();
        let selector = MarketSelector::new(btc_asset_map());

        let first_out = dir.join("first.jsonl");
        let second_out = dir.join("second.jsonl");
        let request = |out: PathBuf| ReplayRequest {
            out_dir: dir.to_path_buf(),
            from: date.clone(),
            to: date.clone(),
            out: Some(out),
            cloid_prefix: 0,
            latency_ms: 20,
            account_value: ds("100000"),
        };

        let a = replay_segments(&cfg, &selector, &request(first_out.clone())).unwrap();
        let b = replay_segments(&cfg, &selector, &request(second_out.clone())).unwrap();
        assert_eq!(a, b, "same segments must replay identically");
        assert!(a.frames >= 3, "decoded the fixture frames: {a:?}");
        assert!(a.entries > 0, "the market maker should place orders: {a:?}");
        assert_eq!(
            std::fs::read(&first_out).unwrap(),
            std::fs::read(&second_out).unwrap(),
            "the action journal must be byte-identical"
        );
    }
}
