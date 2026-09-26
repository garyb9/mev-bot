//! Orchestration binary for the Hyperliquid-first trading system.
//!
//! Milestone M0.2 (SPEC-0000): workspace scaffold only. Config, modes,
//! observability, and the client wiring arrive in later milestones.

use mev_core::{NAME, VERSION};
use mev_hl_client::Network;

fn main() {
    let network = Network::Mainnet;
    println!("{NAME} {VERSION} — scaffold");
    println!("  REST: {}", network.rest_url());
    println!("  WS:   {}", network.ws_url());
}
