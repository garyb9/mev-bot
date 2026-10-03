//! `hl nonce reset`: rewrite a corrupt persisted nonce (SPEC-0002 H-6).

use crate::*;

/// Reset a corrupt persisted nonce (SPEC-0002 H-6).
///
/// Opens the configured SQLite directly — no agent key and no signer are needed
/// to rewrite a `meta` row. Refuses unless the stored value is corrupt by the
/// same rule the bot boots with (beyond `now +` the venue's future window), so
/// it can never force a reuse while the bot is trading. Mirrors the in-process
/// reset's new value: `now +` the write-behind lease.
pub(crate) fn nonce_reset(network: Option<NetworkArg>) -> Result<()> {
    let config = Config::load(ConfigOverrides {
        network: network.map(Into::into),
        ..Default::default()
    })?;
    reset_nonce_row(&config.db_path)
}

/// The testable core of `hl nonce reset`: rewrite `db_path`'s nonce row only if
/// it is corrupt, printing the old and new values.
pub(crate) fn reset_nonce_row(db_path: &std::path::Path) -> Result<()> {
    use hl_arb_client::nonce::{DEFAULT_NONCE_LEASE_MS, VENUE_MAX_FUTURE_MS};

    // Refuse while a bot holds the database: it may rewrite the row, and the
    // reset must not race a live send (SPEC-0002 H-6).
    let _lock = db_lock::DbLock::acquire(db_path)?;

    let db = Db::open(db_path)?;
    let now = SystemClock.now_ms();
    let old = db.nonce_last()?.unwrap_or(0);
    let ceiling = now.saturating_add(VENUE_MAX_FUTURE_MS);
    if old <= ceiling {
        anyhow::bail!(
            "refusing to reset: persisted nonce {old} is not beyond the venue future \
             window ({ceiling}); the bot is not in the fail-closed corruption state"
        );
    }
    let new = now.saturating_add(DEFAULT_NONCE_LEASE_MS);
    db.set_nonce_last(new)?;
    println!("nonce reset: old={old} new={new} ({})", db_path.display());
    println!("the venue may reject a few orders until its seen-nonce window rolls past {old}");
    Ok(())
}
