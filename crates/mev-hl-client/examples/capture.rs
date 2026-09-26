//! Record live WS frames into `benches/fixtures/*.jsonl` for deterministic
//! decode benchmarks (SPEC-0001 §10).
//!
//! Usage: `cargo run -p mev-hl-client --example capture [seconds]`

use std::{collections::HashMap, fs::File, io::Write, time::Duration};

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};

const CHANNELS: [&str; 4] = ["l2Book", "trades", "activeAssetCtx", "allMids"];
const PER_CHANNEL: usize = 4000;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    let (mut socket, _) = connect_async("wss://api.hyperliquid.xyz/ws").await?;
    socket
        .send(Message::Text(
            r#"{"method":"subscribe","subscription":{"type":"allMids"}}"#.into(),
        ))
        .await?;
    for coin in ["BTC", "ETH", "SOL", "xyz:TSLA"] {
        for kind in ["l2Book", "trades", "activeAssetCtx"] {
            let sub = format!(
                r#"{{"method":"subscribe","subscription":{{"type":"{kind}","coin":"{coin}"}}}}"#
            );
            socket.send(Message::Text(sub.into())).await?;
        }
    }

    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/benches/fixtures");
    std::fs::create_dir_all(dir)?;

    let mut files: HashMap<String, File> = HashMap::new();
    let mut counts: HashMap<String, usize> = HashMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);

    loop {
        if all_full(&counts) {
            break;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        let text = match tokio::time::timeout(remaining, socket.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => text,
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_))) | Ok(None) | Err(_) => break,
        };

        let channel = serde_json::from_str::<serde_json::Value>(text.as_str())
            .ok()
            .and_then(|value| value.get("channel")?.as_str().map(str::to_owned));
        let Some(channel) = channel else { continue };
        if !CHANNELS.contains(&channel.as_str()) {
            continue;
        }

        let count = counts.entry(channel.clone()).or_default();
        if *count >= PER_CHANNEL {
            continue;
        }
        let file = match files.get_mut(&channel) {
            Some(file) => file,
            None => files
                .entry(channel.clone())
                .or_insert(File::create(format!("{dir}/{channel}.jsonl"))?),
        };
        writeln!(file, "{text}")?;
        *count += 1;
    }

    for channel in CHANNELS {
        println!(
            "{channel}: {} frames",
            counts.get(channel).copied().unwrap_or(0)
        );
    }
    Ok(())
}

fn all_full(counts: &HashMap<String, usize>) -> bool {
    CHANNELS
        .iter()
        .all(|channel| counts.get(*channel).copied().unwrap_or(0) >= PER_CHANNEL)
}
