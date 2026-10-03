//! `hl replay`: replay SQLite sessions or recorder segments (SPEC-0010 §14).

use crate::*;

/// Deterministically replay a recorded session (SQLite) or recorder segments.
pub(crate) async fn replay(network: Option<NetworkArg>, args: ReplayArgs) -> Result<()> {
    let network = resolve_network(network)?;
    let overrides = ConfigOverrides {
        network: Some(network),
        db_path: args.db.clone(),
        ..Default::default()
    };
    let mut config = Config::load(overrides)?;

    // `--from`/`--to` select the recorder-segment driver (SPEC-0010 E-7).
    if let (Some(from), Some(to)) = (args.from.clone(), args.to.clone()) {
        if !args.strategies.is_empty() {
            config.strategy.enabled = args.strategies.clone();
        }
        let selector = selector_for(config.network, &config.watchlist).await?;
        let account_value = Decimal::from_str(&args.account_value)
            .with_context(|| format!("parsing --account-value `{}`", args.account_value))?;
        let request = replay::ReplayRequest {
            out_dir: args.rec_dir.clone(),
            from,
            to,
            out: args.out.clone(),
            cloid_prefix: args.cloid_prefix,
            latency_ms: args.latency_ms,
            account_value,
        };
        let outcome = replay::replay_segments(&config, &selector, &request)?;
        println!(
            "replay: segments={} frames={} entries={} fingerprint=0x{:016x}",
            outcome.segments, outcome.frames, outcome.entries, outcome.fingerprint
        );
        return Ok(());
    }

    let selector = selector_for(config.network, &config.watchlist).await?;
    let outcome = engine::replay(&config, &selector, args.session, &config.db_path).await?;
    println!(
        "replay: events={} intents={} fingerprint=0x{:016x}",
        outcome.events, outcome.intents, outcome.fingerprint
    );
    Ok(())
}
