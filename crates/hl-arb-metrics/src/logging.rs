//! `tracing` initialization.
//!
//! Level is controlled by `RUST_LOG`; set `HL_LOG_JSON=true` for structured
//! JSON logs (production).

use tracing_subscriber::EnvFilter;

/// Initialize the global subscriber. Safe to call once; subsequent calls are
/// ignored rather than panicking.
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let json = std::env::var("HL_LOG_JSON")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);

    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    let result = if json {
        builder.json().try_init()
    } else {
        builder.try_init()
    };

    // A previously-installed subscriber is not an error.
    let _ = result;
}
