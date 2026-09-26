//! Observability: `tracing` initialization, metric names, and health state.
//!
//! See SPEC-0000 §9 and SPEC-0006. Concrete setup lands in milestone M0.5.

/// Metric names used across the system, kept in one place to avoid drift.
pub mod names {
    /// WebSocket messages decoded per second, tagged by channel.
    pub const WS_MSGS: &str = "hl_ws_msgs_total";
    /// Market feed staleness in seconds, tagged by feed.
    pub const FEED_STALENESS_SECONDS: &str = "hl_feed_staleness_seconds";
    /// WebSocket reconnect counter, tagged by reason.
    pub const WS_RECONNECTS: &str = "hl_ws_reconnects_total";
    /// Order submission latency histogram, tagged by transport.
    pub const ORDER_SUBMIT_SECONDS: &str = "hl_order_submit_seconds";
    /// Order rejects, tagged by exchange status.
    pub const ORDER_REJECTS: &str = "hl_order_rejects_total";
    /// Nonce errors encountered.
    pub const NONCE_ERRORS: &str = "hl_nonce_errors_total";
    /// Risk breaker trips, tagged by trigger.
    pub const BREAKER_TRIPS: &str = "hl_breaker_trips_total";
}
