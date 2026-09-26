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
    /// Unknown order outcomes resolved via `orderStatus`, tagged by resolution.
    pub const ORDER_RECONCILE: &str = "hl_order_reconcile_total";
    /// Nonce errors encountered.
    pub const NONCE_ERRORS: &str = "hl_nonce_errors_total";
    /// Nonce self-heal resets, tagged by reason.
    pub const NONCE_RESETS: &str = "hl_nonce_resets_total";
    /// Whether the dead-man's switch is currently armed (0/1).
    pub const DEADMAN_ARMED: &str = "hl_deadman_armed";
    /// Dead-man's switch arm/refresh submissions.
    pub const DEADMAN_REFRESHES: &str = "hl_deadman_refreshes_total";
    /// Dead-man's switch submission failures.
    pub const DEADMAN_FAILURES: &str = "hl_deadman_failures_total";
    /// Remaining address rate-limit budget, tagged by kind.
    pub const RATE_BUDGET_REMAINING: &str = "hl_rate_budget_remaining";
    /// Risk breaker trips, tagged by trigger.
    pub const BREAKER_TRIPS: &str = "hl_breaker_trips_total";
    /// Order intents proposed by strategies, tagged by strategy.
    pub const STRATEGY_INTENTS: &str = "hl_strategy_intents_total";
    /// Intents gated by risk, tagged by strategy and decision/reason.
    pub const STRATEGY_GATES: &str = "hl_strategy_gates_total";
    /// Fills (paper or real), tagged by strategy.
    pub const STRATEGY_FILLS: &str = "hl_strategy_fills_total";
    /// Expected net edge in bps at decision time, tagged by strategy.
    pub const STRATEGY_EDGE_BPS: &str = "hl_strategy_edge_bps";
    /// Paper trading fees paid in USD.
    pub const PAPER_FEES_PAID: &str = "hl_paper_fees_paid_usd";
    /// Engine decode span: `t_decoded − t_recv` (SPEC-0010 §17).
    pub const ENGINE_DECODE_SECONDS: &str = "hl_engine_decode_seconds";
    /// Engine queue span: `t_dequeued − t_decoded` (SPEC-0010 §17).
    pub const ENGINE_QUEUE_SECONDS: &str = "hl_engine_queue_seconds";
    /// Engine decide span: `t_decided − t_dequeued` (SPEC-0010 §17).
    pub const ENGINE_DECIDE_SECONDS: &str = "hl_engine_decide_seconds";
    /// Engine risk span: `t_risked − t_decided` (SPEC-0010 §17).
    pub const ENGINE_RISK_SECONDS: &str = "hl_engine_risk_seconds";
    /// Engine sign span: `t_signed − t_risked` (SPEC-0010 §17).
    pub const ENGINE_SIGN_SECONDS: &str = "hl_engine_sign_seconds";
    /// Engine handoff span: `t_written − t_signed` (SPEC-0010 §17).
    pub const ENGINE_HANDOFF_SECONDS: &str = "hl_engine_handoff_seconds";
    /// Headline internal latency: `t_written − t_recv` (SPEC-0010 §17, GOAL §5.2).
    pub const TICK_TO_ORDER_SECONDS: &str = "hl_tick_to_order_seconds";
    /// Submit-to-ack span: `t_ack − t_written` (SPEC-0010 §17).
    pub const SUBMIT_ACK_SECONDS: &str = "hl_submit_ack_seconds";
    /// Engine loop iteration duration in seconds (SPEC-0010 §17).
    pub const ENGINE_ITERATION_SECONDS: &str = "hl_engine_iteration_seconds";
    /// Market events drained per engine iteration (SPEC-0010 §17).
    pub const ENGINE_EVENTS_PER_ITERATION: &str = "hl_engine_events_per_iteration";
    /// Market updates dropped on a full channel, tagged by coin (SPEC-0010 §17).
    pub const ENGINE_MARKET_DROPS_TOTAL: &str = "hl_engine_market_drops_total";
    /// Fraction of engine iterations that drained no events (SPEC-0010 §17).
    pub const ENGINE_IDLE_RATIO: &str = "hl_engine_idle_ratio";
}
