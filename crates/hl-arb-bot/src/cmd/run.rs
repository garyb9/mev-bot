//! `hl run`: the engine loop, feed wiring, and shutdown.

use crate::*;

pub(crate) async fn run(
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

    // Hold the exclusive nonce-database lock for the process lifetime, so
    // `hl nonce reset` cannot rewrite the row under a running bot (SPEC-0002
    // H-6). Released when the process exits.
    let _db_lock = db_lock::DbLock::acquire(&config.db_path)?;

    let selector = selector_for(config.network, &config.watchlist).await?;
    let watchlist: Vec<String> = selector
        .resolve_all(&config.watchlist)?
        .iter()
        .map(|market| market.coin.clone())
        .collect();

    let metrics = hl_arb_metrics::prometheus::install_recorder();
    metrics::counter!(hl_arb_metrics::names::STARTUPS).increment(1);

    info!(
        network = ?config.network,
        mode = ?config.mode,
        autonomy = ?config.autonomy,
        watchlist = ?watchlist,
        "starting"
    );

    // Strategies run in `simulate` and `live`; `observe` stays read-only.
    let plan = if config.mode == Mode::Observe {
        None
    } else {
        Some(engine::build(&config, &selector)?)
    };

    let (subscriptions, book_coins, ctx_coins) = build_subscriptions(&watchlist, plan.as_ref());

    // Health-only market snapshot: it drives `/readyz` and feed staleness
    // metrics and never gates a decision (the engine owns trading state).
    let health_state = Arc::new(RwLock::new(MarketState::new(Tolerance::default())));
    if let Ok(mut guard) = health_state.write() {
        for coin in &book_coins {
            guard.expect_book(coin);
        }
        for coin in &ctx_coins {
            guard.expect_ctx(coin);
        }
    }
    let health = Health::new();

    // Simulate/live record their inputs to SQLite for deterministic replay.
    let session = if config.mode == Mode::Observe {
        None
    } else {
        let db = Db::open(&config.db_path)?;
        let session_id = db.create_session(
            &format!("{:?}", config.network),
            &format!("{:?}", config.mode),
            None,
            SystemClock.now_ms(),
        )?;
        info!(session_id, "recording session");
        Some((Arc::new(DbWriter::spawn(db, 4096)), session_id))
    };
    let recorder = session
        .as_ref()
        .map(|(writer, session_id)| engine::Recorder::new(writer.clone(), *session_id));

    let info: Arc<dyn InfoApi> = Arc::new(HttpInfo::new(config.network));
    let exchange = live_exchange(&config)?;

    // Interned coins: the strategy universe when strategies run, else the
    // watchlist. The market decoder resolves names through this registry.
    let registry = plan.as_ref().map_or_else(
        || CoinRegistry::from_coins(&watchlist),
        |build| build.registry.clone(),
    );
    let coin_count = registry.len();

    // Inbound channels (market lossy, account lossless) and the loop stop flag.
    let (handles, inputs) = inputs(MARKET_CHANNEL_CAP, ACCOUNT_CHANNEL_CAP);
    let (stop_tx, stop_rx) = crossbeam_channel::bounded(1);

    // SPEC-0004 K-3: if the kill flag file exists at startup, begin killed so no
    // order can go out before the first control poll.
    live::initial_kill(&handles, &config.kill_file);

    // Exec backend: the paper backend fills in-process for `simulate`; `live`
    // hands unsigned posts to a WS writer; `observe` has no backend.
    let mut exec_writer = None;
    let exec: Option<Box<dyn hl_arb_engine::exec::ExecBackend + Send>> = match config.mode {
        Mode::Live => {
            let exchange = exchange
                .clone()
                .context("live mode requires a configured exchange")?;
            let (out, posts) = outbound::<UnsignedPost>(ACCOUNT_CHANNEL_CAP);
            exec_writer = Some(live::spawn_exec_writer(
                posts,
                exchange,
                info.clone(),
                config.account_address.clone(),
                handles.clone(),
            ));
            Some(Box::new(out))
        }
        _ => None,
    };

    let paper = if config.mode == Mode::Simulate {
        let instruments: BTreeMap<String, Instrument> = plan
            .as_ref()
            .map(|build| build.instruments.clone())
            .unwrap_or_default();
        let seeded = AccountView {
            account_value: Decimal::from(100_000),
            fees: FeeRates::PERP,
            ..Default::default()
        };
        Some(PaperExec::new(
            PaperConfig::default(),
            seeded,
            instruments,
            FeeRates::PERP,
            FeeRates::SPOT,
        ))
    } else {
        None
    };

    let strategies = plan.map(|build| build.strategies).unwrap_or_default();
    let table = AssetTable::from_selector(&registry, &selector);
    let risk = RiskGate::from_settings(&config.risk);
    let dispatcher_config = DispatcherConfig {
        max_slippage_bps: Decimal::from(config.strategy.max_slippage_bps),
    };
    let mut dispatcher = StrategyDispatcher::new(
        strategies,
        registry.clone(),
        table,
        risk,
        exec,
        dispatcher_config,
    );
    if let Some(paper) = paper {
        dispatcher = dispatcher.with_paper(paper);
    }
    let resting = dispatcher.resting_order_counter();
    let market_drops = dispatcher.market_drop_counter();

    // The loop is synchronous and owns all trading state: run it on its own
    // thread, stopped through the crossbeam channel on shutdown.
    let spin_us = config.engine.spin_us;
    let engine_handle = std::thread::Builder::new()
        .name("hl-arb-engine".to_string())
        .spawn(move || {
            let loop_config = LoopConfig {
                spin_us,
                coin_count,
            };
            let engine = EngineLoop::with_dispatcher(inputs, dispatcher, loop_config, stop_rx);
            let _ = engine.run();
        })
        .context("spawning the engine thread")?;

    let heartbeat = tokio::spawn(heartbeat());
    // The health snapshot and the SQLite replay log are off the engine path:
    // the ingest task hands every raw frame to this sidecar and never waits.
    let (side_tx, side_rx) = tokio::sync::mpsc::channel::<SideFrame>(SIDE_CHANNEL_CAP);
    let sidecar_task = tokio::spawn(recording_sidecar(side_rx, health_state.clone(), recorder));
    let ingest_task = tokio::spawn(ingest(
        handles.clone(),
        subscriptions,
        registry.clone(),
        config.network,
        side_tx,
        market_drops,
    ));
    let monitor_task = tokio::spawn(monitor(health_state, health.clone()));
    // SPEC-0010 §15: the H-3 account stream is the source of truth for own
    // orders and fills; the REST reconciler is the 30 s backstop.
    let reconciler = config.account_address.clone().map(|address| {
        tokio::spawn(engine::account_reconciler(
            info.clone(),
            address,
            registry.clone(),
            handles.clone(),
        ))
    });
    let account_stream_task = config.account_address.as_ref().map(|address| {
        tokio::spawn(live::account_stream(
            handles.clone(),
            vec![
                Subscription::OrderUpdates {
                    user: address.clone(),
                },
                Subscription::UserFills {
                    user: address.clone(),
                },
                Subscription::UserEvents {
                    user: address.clone(),
                },
            ],
            registry.clone(),
            config.network,
        ))
    });
    let control_task = tokio::spawn(live::control(handles.clone(), config.kill_file.clone()));
    let deadman_task = exchange.clone().map(|exchange| {
        tokio::spawn(deadman(
            exchange,
            info.clone(),
            config.account_address.clone(),
            resting.clone(),
            handles.clone(),
            config.schedule_cancel_ttl_ms,
        ))
    });

    info!("waiting for feeds to become ready");
    serve(health, metrics, config.http_port).await?;

    // Stop the engine first, then tear the I/O tasks down.
    let _ = stop_tx.send(());
    let _ = engine_handle.join();
    ingest_task.abort();
    sidecar_task.abort();
    monitor_task.abort();
    heartbeat.abort();
    control_task.abort();
    if let Some(account_stream) = account_stream_task {
        account_stream.abort();
    }
    if let Some(reconciler) = reconciler {
        reconciler.abort();
    }
    if let Some(writer) = exec_writer {
        writer.abort();
    }
    // The dead-man task disarms `scheduleCancel` on the same shutdown signal;
    // give it a moment to submit before the process exits.
    if let Some(deadman) = deadman_task {
        let _ = tokio::time::timeout(Duration::from_secs(5), deadman).await;
    }
    info!("shutdown complete");
    Ok(())
}

/// Merge the watchlist feeds with any strategy-required feeds, de-duplicated.
pub(crate) fn build_subscriptions(
    watchlist: &[String],
    plan: Option<&engine::EngineBuild>,
) -> (Vec<Subscription>, BTreeSet<String>, BTreeSet<String>) {
    let mut book_coins = BTreeSet::new();
    let mut ctx_coins = BTreeSet::new();
    let mut subs = Vec::new();

    for coin in watchlist {
        book_coins.insert(coin.clone());
        ctx_coins.insert(coin.clone());
        subs.push(Subscription::L2Book { coin: coin.clone() });
        subs.push(Subscription::ActiveAssetCtx { coin: coin.clone() });
        subs.push(Subscription::Trades { coin: coin.clone() });
    }
    if let Some(plan) = plan {
        for sub in &plan.subscriptions {
            match sub {
                Subscription::L2Book { coin } => {
                    book_coins.insert(coin.clone());
                }
                Subscription::ActiveAssetCtx { coin } => {
                    ctx_coins.insert(coin.clone());
                }
                _ => {}
            }
            subs.push(sub.clone());
        }
    }

    let mut seen = BTreeSet::new();
    subs.retain(|sub| {
        let key = serde_json::to_string(sub).unwrap_or_default();
        seen.insert(key)
    });
    (subs, book_coins, ctx_coins)
}

/// Build the live write client when running `live` with an agent key.
///
/// Returns `None` in `observe`/`simulate`, so no socket is opened and no key is
/// needed. The returned client shares the SQLite nonce high-water mark.
pub(crate) fn live_exchange(config: &Config) -> Result<Option<Arc<dyn ExchangeApi>>> {
    if config.mode != Mode::Live {
        return Ok(None);
    }
    let Some(key) = config.agent_key() else {
        return Ok(None);
    };
    let signer = AgentSigner::from_hex(key, config.network == Network::Mainnet)?;
    let db = Arc::new(Mutex::new(Db::open(&config.db_path)?));
    let exchange = WsExchange::new(config.network, config.mode, Some(signer))?.with_nonce_db(db)?;
    Ok(Some(Arc::new(exchange)))
}

/// Keep `scheduleCancel` armed while (and only while) orders rest, and fail
/// closed on any arm/refresh error (SPEC-0002 H-4).
///
/// Each cycle the task reads the engine's resting-order count (an atomic the
/// dispatcher refreshes per iteration; reading it never blocks the order path).
/// The switch (a) arms when the first order rests, (b) refreshes at half the
/// TTL, and (c) disarms when the last order leaves — so an idle bot does not
/// spend address rate-limit budget. If arming/refreshing fails while orders
/// rest, a fail-closed `Control::KillSwitch` is sent through the engine's
/// account channel (halting dispatch and cancelling working orders). The task
/// also polls `userRateLimit` every 60 s and exposes the remaining address
/// budget as a metric.
pub(crate) async fn deadman(
    exchange: Arc<dyn ExchangeApi>,
    info: Arc<dyn InfoApi>,
    address: Option<String>,
    resting: Arc<AtomicUsize>,
    handles: InputHandles,
    ttl_ms: u64,
) {
    let mut switch = DeadMansSwitch::new(ttl_ms);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.tick().await;
    let mut rate = tokio::time::interval(Duration::from_secs(60));

    loop {
        tokio::select! {
            _ = shutdown_signal() => break,
            _ = rate.tick() => {
                if let Some(address) = &address {
                    match info.user_rate_limit(address).await {
                        Ok(budget) => {
                            metrics::gauge!(
                                hl_arb_metrics::names::RATE_BUDGET_REMAINING,
                                "kind" => "address",
                            )
                            .set(budget.remaining() as f64);
                        }
                        Err(err) => {
                            tracing::debug!(error = %err, "userRateLimit poll failed");
                        }
                    }
                }
            }
            _ = tick.tick() => {
                // The engine refreshes this count once per iteration; reading
                // it never blocks the order path.
                let resting_orders = resting.load(Ordering::Relaxed);
                if let Some(action) = switch.update(now_ms(), resting_orders) {
                    let arming = resting_orders > 0;
                    match exchange.submit(&action).await {
                        Ok(_) => {
                            if arming {
                                metrics::counter!(hl_arb_metrics::names::DEADMAN_REFRESHES).increment(1);
                            } else {
                                info!("dead-man switch disarmed (no resting orders)");
                            }
                        }
                        Err(err) if arming => {
                            error!(error = %err, "dead-man arm/refresh failed; halting trading");
                            metrics::counter!(hl_arb_metrics::names::DEADMAN_FAILURES).increment(1);
                            // Fail closed: halt the v2 dispatcher through the
                            // lossless account channel (it also cancels working
                            // orders via Control::KillSwitch).
                            if !handles.send_account(AccountUpdate::Control(Control::KillSwitch)) {
                                break;
                            }
                        }
                        Err(err) => {
                            // Disarm failed; the venue expires the schedule anyway.
                            tracing::debug!(error = %err, "dead-man disarm failed");
                        }
                    }
                }
                metrics::gauge!(hl_arb_metrics::names::DEADMAN_ARMED)
                    .set(if switch.is_armed() { 1.0 } else { 0.0 });
            }
        }
    }

    if let Some(action) = switch.disarm() {
        match exchange.submit(&action).await {
            Ok(_) => info!("dead-man switch disarmed"),
            Err(err) => tracing::debug!(error = %err, "dead-man disarm failed"),
        }
    }
    metrics::gauge!(hl_arb_metrics::names::DEADMAN_ARMED).set(0.0);
}

/// A raw frame handed off the ingest task for health/recording.
pub(crate) struct SideFrame {
    /// Exact text received.
    text: String,
    /// Local receive time in wall-clock ms.
    ts_ms: u64,
}

/// Bounded queue from the ingest task to the health/SQLite sidecar.
pub(crate) const SIDE_CHANNEL_CAP: usize = 16_384;

/// Apply a raw market-stream gap to the engine, returning `true` when the
/// ingest task should stop (clean shutdown).
///
/// The gap's stale cut must be the moment the drop was detected, so this carries
/// [`hl_arb_client::RawEvent::Gap::disconnect_ns`] (process-monotonic, the same
/// clock that stamps frames) into the engine's control channel. Discarding it
/// and letting the engine use its iteration start would let a pre-gap book that
/// was still queued when the socket died clear staleness and pass the risk gate
/// on pre-disconnect depth.
pub(crate) fn signal_raw_gap(handles: &InputHandles, event: &RawEvent) -> bool {
    let RawEvent::Gap {
        reason,
        detail,
        disconnect_ns,
        ..
    } = event
    else {
        return false;
    };
    tracing::warn!(reason, detail, "market feed gap");
    if reason == "shutdown" {
        return true;
    }
    let _ = handles.signal_gap(*disconnect_ns);
    false
}

/// Ingest market data into the v2 engine.
///
/// The typed decode and the hand-off to the engine happen **first**, so the
/// engine is never queued behind the slower legacy decode (ADR-0001 put
/// `ws::decode` at 130–260 µs). The raw frame is then offered to the health /
/// SQLite sidecar on a bounded, non-blocking channel (drop on full; the engine
/// never waits on recording).
pub(crate) async fn ingest(
    handles: InputHandles,
    subscriptions: Vec<Subscription>,
    registry: CoinRegistry,
    network: Network,
    side_tx: tokio::sync::mpsc::Sender<SideFrame>,
    market_drops: Arc<AtomicU64>,
) {
    let ingester = Ingest::new(ConnId(0), registry);
    let planned: Vec<String> = subscriptions
        .iter()
        .map(|sub| serde_json::to_string(sub).unwrap_or_default())
        .collect();
    let mut backoff = Duration::from_secs(1);

    loop {
        match RawWsConn::connect(Box::new(HlProtocol::new(network)), planned.clone()).await {
            Ok(mut conn) => {
                info!(subscriptions = planned.len(), "market stream connected");
                // A reconnect does not itself clear staleness: the engine clears
                // a coin only when a fresh l2 book snapshot arrives (SPEC-0010
                // §16), so no gap-close signal is needed.
                loop {
                    match conn.next().await {
                        Ok(RawEvent::Text {
                            t_ns,
                            mono_ns,
                            text,
                        }) => {
                            let stamp = Stamp {
                                // `RawEvent::Text::t_ns` is wall-clock ms.
                                t_recv_ns: t_ns.saturating_mul(1_000_000),
                                mono_ns,
                                ts_exch_ms: 0,
                            };
                            match ingester.decode(&text, stamp) {
                                Ok(Some(update)) => {
                                    if handles.send_market(update) == MarketSend::Dropped {
                                        // Folded into `hl_engine_market_drops_total`
                                        // by the dispatcher's next flush.
                                        market_drops.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                Ok(None) => {}
                                Err(err) => {
                                    tracing::debug!(error = %err, "undecodable market frame");
                                }
                            }
                            // Off the engine path: decode again for the health
                            // snapshot and the replay log on a sidecar task. A
                            // full sidecar channel drops the frame (counted).
                            if side_tx
                                .try_send(SideFrame {
                                    text,
                                    ts_ms: SystemClock.now_ms(),
                                })
                                .is_err()
                            {
                                metrics::counter!(hl_arb_metrics::names::SIDECAR_DROPS)
                                    .increment(1);
                            }
                        }
                        Ok(gap @ RawEvent::Gap { .. }) => {
                            // Mark coins stale until a fresh book arrives, using
                            // the drop's detection time. With the R-8 fix2
                            // `RawWsConn`, this arrives the moment the drop is
                            // detected, before the reconnect. The signal uses
                            // the lossless control channel so a saturated market
                            // queue cannot swallow it.
                            if signal_raw_gap(&handles, &gap) {
                                return;
                            }
                        }
                        Ok(RawEvent::Opened { .. }) => {
                            // The reconnect succeeded; the engine clears each
                            // coin as its l2 book snapshot arrives.
                        }
                        Ok(_) => {}
                        Err(err) => {
                            tracing::warn!(error = %err, "market stream ended");
                            // No `RawEvent::Gap` carried a timestamp here: read
                            // the detection time on the shared frame clock.
                            let _ = handles.signal_gap(hl_arb_client::raw_ws::mono_ns());
                            return;
                        }
                    }
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "market stream connect failed");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// Apply health and SQLite recording to raw frames, off the ingest path.
///
/// Runs the legacy `ws::decode` here so it never delays the engine hand-off
/// (ADR-0001: 130–260 µs per frame). A dropped frame only costs health/recording
/// fidelity; it is folded into a gap by the next update.
pub(crate) async fn recording_sidecar(
    mut rx: tokio::sync::mpsc::Receiver<SideFrame>,
    health_state: Arc<RwLock<MarketState>>,
    recorder: Option<engine::Recorder>,
) {
    while let Some(frame) = rx.recv().await {
        if let Ok(Some(event)) = hl_arb_client::ws::decode(&frame.text) {
            if let Some(recorder) = &recorder {
                recorder.record(&hl_arb_strategy::Event::Market(event.clone()), frame.ts_ms);
            }
            if let Ok(mut guard) = health_state.write() {
                guard.apply(&event);
            }
        }
    }
}

/// Publish feed staleness and readiness while the process runs.
pub(crate) async fn monitor(state: Arc<RwLock<MarketState>>, health: Health) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        interval.tick().await;
        let now = Instant::now();
        let (ready, ages) = {
            let guard = state.read().expect("market state lock poisoned");
            (guard.is_ready(now), guard.ages(now))
        };
        health.set_ready(ready);
        for age in ages {
            metrics::gauge!(
                hl_arb_metrics::names::FEED_STALENESS_SECONDS,
                "feed" => age.feed,
                "coin" => age.coin,
            )
            .set(age.age_secs.min(1e9));
        }
    }
}
pub(crate) async fn heartbeat() {
    let started = Instant::now();
    let mut interval = tokio::time::interval(Duration::from_secs(15));
    loop {
        interval.tick().await;
        let uptime = started.elapsed().as_secs();
        metrics::gauge!(hl_arb_metrics::names::UPTIME_SECONDS).set(uptime as f64);
        metrics::counter!(hl_arb_metrics::names::HEARTBEATS).increment(1);
        tracing::debug!(uptime_seconds = uptime, "heartbeat");
    }
}
