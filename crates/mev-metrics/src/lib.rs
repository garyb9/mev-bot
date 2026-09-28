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

    /// Frames dropped on the full ingest→health/recording sidecar channel.
    pub const SIDECAR_DROPS: &str = "hl_sidecar_drops_total";
    /// Currently connected WebSocket sockets.
    pub const WS_CONNECTED: &str = "hl_ws_connected";
    /// Order submission latency histogram, tagged by transport.
    pub const ORDER_SUBMIT_SECONDS: &str = "hl_order_submit_seconds";

    /// Time from an exec post being received to its frame being enqueued on the
    /// socket (SPEC-0002 H-1, SPEC-0010 §12). The reply wait is not included.
    pub const EXEC_QUEUE_SECONDS: &str = "hl_exec_queue_seconds";
    /// Order rejects, tagged by exchange status.
    pub const ORDER_REJECTS: &str = "hl_order_rejects_total";
    /// Unknown order outcomes resolved via `orderStatus`, tagged by resolution.
    pub const ORDER_RECONCILE: &str = "hl_order_reconcile_total";
    /// Nonce errors encountered.
    pub const NONCE_ERRORS: &str = "hl_nonce_errors_total";
    /// Nonce self-heal resets, tagged by reason.
    pub const NONCE_RESETS: &str = "hl_nonce_resets_total";
    /// Sends refused because the reserved nonce was not covered by a confirmed
    /// durable high-water mark (SPEC-0002 H-6).
    pub const NONCE_LEASE_REFUSALS_TOTAL: &str = "hl_nonce_lease_refusals_total";
    /// Write-behind nonce persists dropped because the writer queue was full or
    /// the writer was gone (SPEC-0002 H-6).
    pub const NONCE_PERSIST_DROPPED_TOTAL: &str = "hl_nonce_persist_dropped_total";
    /// Restored nonce leases beyond the venue's future window at resume: the bot
    /// fails closed until an operator resets the nonce (SPEC-0002 H-6).
    pub const NONCE_RESUME_CORRUPT_TOTAL: &str = "hl_nonce_resume_corrupt_total";
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
    /// WS frame decode time, measured at the ingest site (SPEC-0002 H-7),
    /// tagged by `kind` (`market`/`account`).
    ///
    /// Distinct from [`ENGINE_DECODE_SECONDS`], which is the per-decision
    /// engine span from the internal `LatencyRecorder`; this is the decode of
    /// one frame as it happens in the ingest task.
    pub const DECODE_SECONDS: &str = "hl_decode_seconds";
    /// Order build + msgpack + EIP-712 sign time (SPEC-0002 H-7).
    ///
    /// Measured inside `WriteCore::prepare`, where signing actually happens,
    /// unlike the engine's `hl_engine_sign_seconds` span.
    pub const SIGN_SECONDS: &str = "hl_sign_seconds";
    /// Engine handoff span: `t_written − t_signed` (SPEC-0010 §17).
    pub const ENGINE_HANDOFF_SECONDS: &str = "hl_engine_handoff_seconds";
    /// Headline internal latency, timed by the live exec path: market frame read
    /// → order frame written to the socket (SPEC-0002 H-7, GOAL §5.2).
    pub const TICK_TO_ORDER_SECONDS: &str = "hl_tick_to_order_seconds";
    /// Submit-to-ack span: `t_ack − t_written`, measured at the transport
    /// (SPEC-0002 H-7), tagged by `transport`.
    pub const SUBMIT_ACK_SECONDS: &str = "hl_submit_ack_seconds";
    /// Tick-to-order samples skipped, tagged by `reason` (SPEC-0002 H-7).
    pub const TICK_TO_ORDER_SKIPPED_TOTAL: &str = "hl_tick_to_order_skipped_total";
    /// `hl_tick_to_order_skipped_total{reason}`: the post carried no market
    /// read time (e.g. a timer-driven decision).
    pub const TICK_TO_ORDER_SKIP_UNSET: &str = "unset";
    /// `hl_tick_to_order_skipped_total{reason}`: the market read stamp was not
    /// before now on the socket clock (foreign/future clock).
    pub const TICK_TO_ORDER_SKIP_CLOCK: &str = "clock";
    /// Engine-side headline span `t_written − t_recv` from the in-process
    /// `LatencyRecorder`, not yet published (SPEC-0010 §17). Distinct from
    /// [`TICK_TO_ORDER_SECONDS`], the H-7 live exec path.
    pub const ENGINE_TICK_TO_ORDER_SECONDS: &str = "hl_engine_tick_to_order_seconds";
    /// Engine-side submit span `t_ack − t_written` from the in-process
    /// `LatencyRecorder`, not yet published (SPEC-0010 §17). Distinct from
    /// [`SUBMIT_ACK_SECONDS`], the H-7 transport histogram.
    pub const ENGINE_SUBMIT_ACK_SECONDS: &str = "hl_engine_submit_ack_seconds";
    /// Engine loop iteration duration in seconds (SPEC-0010 §17).
    pub const ENGINE_ITERATION_SECONDS: &str = "hl_engine_iteration_seconds";
    /// Market events drained per engine iteration (SPEC-0010 §17).
    pub const ENGINE_EVENTS_PER_ITERATION: &str = "hl_engine_events_per_iteration";
    /// Market updates dropped on a full channel, tagged by coin (SPEC-0010 §17).
    pub const ENGINE_MARKET_DROPS_TOTAL: &str = "hl_engine_market_drops_total";
    /// Fraction of engine iterations that drained no events (SPEC-0010 §17).
    pub const ENGINE_IDLE_RATIO: &str = "hl_engine_idle_ratio";

    /// Recorder: envelopes written, tagged by `src`, `conn`, and `kind`
    /// (SPEC-0008 §12.2).
    pub const REC_RECORDS_TOTAL: &str = "hl_rec_records_total";
    /// Recorder: uncompressed envelope bytes written, tagged by `src`
    /// (SPEC-0008 §12.2).
    pub const REC_BYTES_RAW_TOTAL: &str = "hl_rec_bytes_raw_total";
    /// Recorder: compressed bytes written, tagged by `src` (SPEC-0008 §12.2).
    pub const REC_BYTES_ZST_TOTAL: &str = "hl_rec_bytes_zst_total";
    /// Recorder: envelopes dropped on a full writer queue, tagged by `src` and
    /// `conn` (SPEC-0008 §12.2).
    pub const REC_DROPPED_TOTAL: &str = "hl_rec_dropped_total";
    /// Recorder: seconds spent in a data gap, tagged by `src`, `conn`, and
    /// `reason` (SPEC-0008 §12.2).
    pub const REC_GAP_SECONDS_TOTAL: &str = "hl_rec_gap_seconds_total";
    /// Recorder: segment-writer queue depth, tagged by `src` (SPEC-0008 §12.2).
    pub const REC_CHANNEL_DEPTH: &str = "hl_rec_channel_depth";
    /// Recorder: segment rotations, tagged by `src` (SPEC-0008 §12.2).
    pub const REC_SEGMENT_ROTATIONS_TOTAL: &str = "hl_rec_segment_rotations_total";
    /// Recorder: free disk bytes (SPEC-0008 §12.2).
    pub const REC_DISK_FREE_BYTES: &str = "hl_rec_disk_free_bytes";
    /// Recorder: chrony clock offset in nanoseconds (SPEC-0008 §12.2).
    pub const REC_CLOCK_OFFSET_NS: &str = "hl_rec_clock_offset_ns";
    /// Recorder: REST weight spent, tagged by `src` (SPEC-0008 §12.2).
    pub const REST_WEIGHT_USED_TOTAL: &str = "hl_rest_weight_used_total";
}
