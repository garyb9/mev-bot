//! `hl` — orchestration binary for the Hyperliquid-first trading system.
//!
//! Milestone M0.4 (SPEC-0000): layered config, execution modes, and the CLI
//! surface. The runtime loop (M0.5/M0.7) and Hyperliquid wiring (SPEC-0001+)
//! arrive in later milestones.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use mev_core::{
    config::{Config, ConfigOverrides, Mode, Network},
    error::Result,
};

#[derive(Parser)]
#[command(name = "hl", version, about = "Hyperliquid-first trading system")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Launch the bot with the configured watchlist.
    Run {
        /// Execution mode (default: observe).
        #[arg(long, value_enum)]
        mode: Option<ModeArg>,
        /// Network (default: mainnet).
        #[arg(long, value_enum)]
        network: Option<NetworkArg>,
        /// Comma-separated coin override, e.g. `--coins BTC,ETH`.
        #[arg(long, value_delimiter = ',')]
        coins: Option<Vec<String>>,
        /// SQLite database path override.
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Inspect resolved configuration.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// List/search markets (SPEC-0001).
    Markets {
        /// Optional filter substring.
        query: Option<String>,
    },
    /// Show a live book snapshot (SPEC-0001).
    Book {
        /// Market symbol.
        coin: String,
        /// Number of book levels per side.
        #[arg(long, default_value_t = 5)]
        levels: usize,
    },
    /// Stream market data (SPEC-0001).
    Watch {
        /// Market symbols.
        coins: Vec<String>,
    },
    /// Edit the persisted watchlist (SPEC-0001).
    Select {
        /// Coins to add.
        #[arg(long)]
        add: Vec<String>,
        /// Coins to remove.
        #[arg(long)]
        remove: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the resolved config with secrets redacted.
    Show,
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    Observe,
    Simulate,
    Live,
}

impl From<ModeArg> for Mode {
    fn from(value: ModeArg) -> Self {
        match value {
            ModeArg::Observe => Mode::Observe,
            ModeArg::Simulate => Mode::Simulate,
            ModeArg::Live => Mode::Live,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum NetworkArg {
    Mainnet,
    Testnet,
}

impl From<NetworkArg> for Network {
    fn from(value: NetworkArg) -> Self {
        match value {
            NetworkArg::Mainnet => Network::Mainnet,
            NetworkArg::Testnet => Network::Testnet,
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let command = cli
        .command
        .unwrap_or(Command::Run { mode: None, network: None, coins: None, db: None });

    if let Err(err) = dispatch(command) {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Run { mode, network, coins, db } => run(mode, network, coins, db),
        Command::Config { cmd } => match cmd {
            ConfigCmd::Show => show_config(),
        },
        Command::Markets { query } => not_yet("markets", query.as_deref()),
        Command::Book { coin, levels } => not_yet("book", Some(&format!("{coin} x{levels}"))),
        Command::Watch { coins } => not_yet("watch", Some(&coins.join(","))),
        Command::Select { add, remove } => {
            not_yet("select", Some(&format!("+{:?} -{:?}", add, remove)))
        }
    }
}

fn run(
    mode: Option<ModeArg>,
    network: Option<NetworkArg>,
    coins: Option<Vec<String>>,
    db: Option<PathBuf>,
) -> Result<()> {
    let overrides = ConfigOverrides {
        mode: mode.map(Into::into),
        network: network.map(Into::into),
        coins,
        db_path: db,
    };
    let config = Config::load(overrides)?;
    println!("[config]\n{}", config.summary());
    println!("\n[run] runtime loop arrives in milestone M0.7 (observe mode).");
    Ok(())
}

fn show_config() -> Result<()> {
    let config = Config::load(ConfigOverrides::default())?;
    println!("{}", config.summary());
    Ok(())
}

fn not_yet(what: &str, detail: Option<&str>) -> Result<()> {
    match detail {
        Some(detail) => println!("`{what} {detail}` is not implemented yet (arrives with SPEC-0001)."),
        None => println!("`{what}` is not implemented yet (arrives with SPEC-0001)."),
    }
    Ok(())
}
