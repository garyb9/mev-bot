//! Observability: `tracing` initialization, Prometheus metrics, and health.
//!
//! See SPEC-0000 §9 and SPEC-0006.

pub mod health;
pub mod logging;
pub mod prometheus;

/// Metric names used across the system, kept in one place to avoid drift.
pub mod names {
    /// Process startups.
    pub const STARTUPS: &str = "hl_startups_total";
    /// Process uptime in seconds.
    pub const UPTIME_SECONDS: &str = "hl_uptime_seconds";
    /// Runtime heartbeat counter.
    pub const HEARTBEATS: &str = "hl_heartbeats_total";
    /// WebSocket messages decoded, tagged by channel.
    pub const WS_MSGS: &str = "hl_ws_msgs_total";
    /// Market feed staleness in seconds, tagged by feed.
    pub const FEED_STALENESS_SECONDS: &str = "hl_feed_staleness_seconds";
    /// WebSocket reconnect counter, tagged by reason.
    pub const WS_RECONNECTS: &str = "hl_ws_reconnects_total";
    /// WebSocket frames that failed to decode.
    pub const WS_PARSE_ERRORS: &str = "hl_ws_parse_errors_total";
    /// Currently connected WebSocket sockets.
    pub const WS_CONNECTED: &str = "hl_ws_connected";
    /// Order submission latency histogram, tagged by transport.
    pub const ORDER_SUBMIT_SECONDS: &str = "hl_order_submit_seconds";
    /// Order rejects, tagged by exchange status.
    pub const ORDER_REJECTS: &str = "hl_order_rejects_total";
    /// Nonce errors encountered.
    pub const NONCE_ERRORS: &str = "hl_nonce_errors_total";
    /// Risk breaker trips, tagged by trigger.
    pub const BREAKER_TRIPS: &str = "hl_breaker_trips_total";
}
