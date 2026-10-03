//! Unit tests for the recorder module (SPEC-0008).
//!
//! Kept in a sibling file so `record.rs` stays within the size budget; the
//! module path is still `record::tests`.

use super::clock::*;
use super::config::*;
use super::connection::*;
use super::inspect::*;
use super::monitor::*;
use super::probe::*;
use super::runner::*;
use super::sources::*;
use super::*;
use hl_arb_recorder::{MountProbe, Subscription};
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn temp_dir(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("hl-arb-bot-record-{tag}-"))
        .tempdir()
        .unwrap()
}

fn segment_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_segments(root, &mut files).unwrap();
    files.sort();
    files
}

/// A protocol that dials a fixed local mock URL.
struct TestProtocol {
    url: String,
}

impl Protocol for TestProtocol {
    fn name(&self) -> &'static str {
        "test"
    }
    fn url(&self) -> String {
        self.url.clone()
    }
    fn subscribe_frame(&self, sub: &str) -> String {
        sub.to_string()
    }
}

#[test]
fn base64_matches_known_vectors() {
    assert_eq!(base64_encode(b""), "");
    assert_eq!(base64_encode(b"f"), "Zg==");
    assert_eq!(base64_encode(b"fo"), "Zm8=");
    assert_eq!(base64_encode(b"foo"), "Zm9v");
    assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
    assert_eq!(base64_encode(&[0, 1, 2, 3]), "AAECAw==");
}

#[test]
fn authority_handles_default_ports() {
    assert_eq!(
        authority("https://api.hyperliquid.xyz").unwrap(),
        ("api.hyperliquid.xyz".to_string(), 443)
    );
    assert_eq!(
        authority("wss://api.hyperliquid.xyz/ws").unwrap(),
        ("api.hyperliquid.xyz".to_string(), 443)
    );
    assert_eq!(
        authority("http://127.0.0.1:8080/info").unwrap(),
        ("127.0.0.1".to_string(), 8080)
    );
}

#[test]
fn chrony_tracking_reads_system_time_column() {
    // Realistic `chronyc -c tracking` order (V-1): RefID, RefName, Stratum,
    // RefTime, SystemTime, LastOffset, …. SystemTime is column 4.
    let line = "C0000000,time.cloudflare.com,3,1790000000.123,0.000012345,\
                    0.000004,0.000006,12.345,0.001,-0.000123,0.012345,0.006789,8.0,Normal";
    let (offset_ns, stratum) = parse_chrony_tracking(line).expect("well-formed line");
    assert_eq!(offset_ns, Some(12_345));
    assert_eq!(stratum, Some(3));
}

#[test]
fn chrony_tracking_rejects_short_lines() {
    assert_eq!(
        parse_chrony_tracking("C0000000,time.cloudflare.com,3"),
        None
    );
    assert_eq!(parse_chrony_tracking(""), None);
}

#[test]
fn request_weight_matches_v1_weights() {
    // fundingHistory: `20 + 1 per 20 items`, pre-charged at the 500-item max.
    assert_eq!(
        request_weight(&serde_json::json!({
            "type": "fundingHistory",
            "coin": "BTC",
            "startTime": 0,
        })),
        20 + 25
    );
    // candleSnapshot: `20 + 1 per 60 items` over the requested window; 6 h of
    // 1m is 360 candles -> 26.
    assert_eq!(
        request_weight(&serde_json::json!({
            "type": "candleSnapshot",
            "req": {
                "coin": "BTC",
                "interval": "1m",
                "startTime": 0,
                "endTime": 21_600_000,
            },
        })),
        26
    );
    // 6 h of 1h is 6 candles -> base weight only.
    assert_eq!(
        request_weight(&serde_json::json!({
            "type": "candleSnapshot",
            "req": { "interval": "1h", "startTime": 0, "endTime": 21_600_000 },
        })),
        20
    );
    assert_eq!(request_weight(&serde_json::json!({ "type": "meta" })), 20);
}

/// Parse a recorder config from a TOML string (no file or env), validating.
fn parse_toml(text: &str) -> Result<RecordConfig> {
    parse_config(Figment::new().merge(Toml::string(text)))
}

#[test]
fn require_mount_defaults_to_unset() {
    let config = parse_toml("[profile.default]\nnetwork = \"mainnet\"\n").unwrap();
    assert_eq!(config.profile.get("default").unwrap().require_mount, None);
}

#[test]
fn require_mount_parses_an_absolute_path() {
    let config = parse_toml(
        "[profile.default]\n\
             out_dir = \"/mnt/e/mev-rec\"\n\
             require_mount = \"/mnt/e\"\n",
    )
    .unwrap();
    assert_eq!(
        config
            .profile
            .get("default")
            .unwrap()
            .require_mount
            .as_deref(),
        Some(Path::new("/mnt/e"))
    );
}

#[test]
fn require_mount_source_defaults_to_unset() {
    let config = parse_toml("[profile.default]\nnetwork = \"mainnet\"\n").unwrap();
    assert_eq!(
        config.profile.get("default").unwrap().require_mount_source,
        None
    );
}

#[test]
fn require_mount_source_parses_a_literal_windows_source() {
    let config = parse_toml(
        "[profile.default]\n\
             require_mount = \"/mnt/e\"\n\
             require_mount_source = 'E:\\'\n",
    )
    .unwrap();
    assert_eq!(
        config
            .profile
            .get("default")
            .unwrap()
            .require_mount_source
            .as_deref(),
        Some("E:\\")
    );
}

/// A tripped guard forces `/readyz` not-ready even while the connections
/// look healthy (R-14 §2).
#[tokio::test]
async fn readiness_monitor_is_not_ready_when_the_mount_is_tripped() {
    let clock: Arc<dyn EnvelopeClock> = Arc::new(hl_arb_recorder::FixedEnvelopeClock::new(1, 0));
    let health = Health::new();
    health.set_ready(true);
    let guard = Arc::new(MountGuard::unguarded());
    guard.trip();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(readiness_monitor(
        RecorderHealth::new(Vec::new(), Vec::new()),
        health.clone(),
        clock,
        guard,
        shutdown_rx,
    ));
    for _ in 0..1_000 {
        tokio::task::yield_now().await;
        if !health.is_ready() {
            break;
        }
    }
    let _ = shutdown_tx.send(true);
    let _ = task.await;
    assert!(
        !health.is_ready(),
        "a tripped mount must make /readyz not ready"
    );
}

/// CEX reference streams are reported but never gate readiness: a dead CEX
/// stream is `cex_down` while `ready` stays true when the gating streams are
/// healthy (R-8 fix1).
#[test]
fn dead_cex_stream_is_reported_but_does_not_gate_readiness() {
    let clock = hl_arb_recorder::FixedEnvelopeClock::new(1, 0);
    let hl = ConnState::new();
    hl.set_connected(true);
    let cex = ConnState::new();
    let watchdog_ns = READY_WATCHDOG.as_nanos() as u64;
    let now = watchdog_ns * 100;
    // The CEX stream last produced data well beyond the watchdog window.
    clock.set_mono_ns(now - watchdog_ns * 10);
    cex.touch(&clock);
    // The gating HL stream is fresh.
    clock.set_mono_ns(now - watchdog_ns / 2);
    hl.touch(&clock);
    let health = RecorderHealth::new(
        vec![("hl-ws-01".to_string(), hl)],
        vec![("bybit-linear".to_string(), cex)],
    );
    assert_eq!(health.cex_down(now, watchdog_ns), Some("bybit-linear"));
    assert!(
        health.ready(now, watchdog_ns, REST_READY_STALE.as_nanos() as u64),
        "a dead CEX stream must not gate readiness"
    );
}

/// The mount watchdog fires once the required mount is gone, which is what
/// makes `run` return an error (process exit non-zero).
#[tokio::test(start_paused = true)]
async fn mount_watchdog_fires_when_the_required_mount_is_not_a_mount() {
    let tmp = temp_dir("watchdog-run");
    let guard = Arc::new(MountGuard::new(
        Some(tmp.path().to_path_buf()),
        Arc::new(SystemMountProbe),
    ));
    tokio::time::timeout(Duration::from_secs(10), wait_for_mount_stop(guard.clone()))
        .await
        .expect("watchdog did not fire");
    assert!(guard.is_tripped());
}

/// With no `require_mount`, the watchdog never fires (current behaviour).
#[tokio::test(start_paused = true)]
async fn mount_watchdog_stays_pending_when_unguarded() {
    let guard = Arc::new(MountGuard::unguarded());
    let result =
        tokio::time::timeout(Duration::from_secs(60), wait_for_mount_stop(guard.clone())).await;
    assert!(result.is_err(), "unguarded watchdog must never complete");
    assert!(!guard.is_tripped());
}

#[test]
fn segment_config_carries_the_mount_guard() {
    let guard = Arc::new(MountGuard::unguarded());
    let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
    let config = segment_config(
        &Profile::default(),
        Network::Mainnet,
        "hl-ws",
        "hl-ws-01",
        &SegmentOpenMeta::default(),
        clock,
        1,
        guard.clone(),
    );
    assert!(Arc::ptr_eq(&config.mount_guard, &guard));
}

// -- R-14 fix1: startup, exit and probe hardening ------------------------

/// A probe whose mount device can be flipped, mirroring the recorder crate's
/// test fake (which is not visible from this crate).
struct FlipProbe {
    mount: PathBuf,
    mount_dev: u64,
    host_dev: u64,
    mounted: AtomicBool,
}

impl FlipProbe {
    fn new(mount: &Path) -> Self {
        Self {
            mount: std::fs::canonicalize(mount).unwrap_or_else(|_| mount.to_path_buf()),
            mount_dev: 7,
            host_dev: 3,
            mounted: AtomicBool::new(true),
        }
    }

    fn set_mounted(&self, mounted: bool) {
        self.mounted.store(mounted, Ordering::SeqCst);
    }
}

impl MountProbe for FlipProbe {
    fn stat(&self, path: &Path) -> std::io::Result<(u64, bool)> {
        let meta = std::fs::metadata(path)?;
        if !self.mounted.load(Ordering::SeqCst) {
            return Ok((self.host_dev, meta.is_dir()));
        }
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let device = if canonical.starts_with(&self.mount) {
            self.mount_dev
        } else {
            self.host_dev
        };
        Ok((device, meta.is_dir()))
    }
}

/// A probe that reports `mount` as a healthy mount but blocks on every
/// call, to exercise the bounded-wait path in isolation. If the timeout
/// were removed the probe would eventually report the mount as healthy.
struct SlowProbe {
    mount: PathBuf,
    delay: Duration,
}

impl MountProbe for SlowProbe {
    fn stat(&self, path: &Path) -> std::io::Result<(u64, bool)> {
        std::thread::sleep(self.delay);
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let device = if canonical.starts_with(&self.mount) {
            7
        } else {
            3
        };
        Ok((device, true))
    }
}

#[test]
fn guarded_create_out_dir_rechecks_and_creates_a_direct_child() {
    let mount_tmp = temp_dir("create-ok");
    let mount = mount_tmp.path().to_path_buf();
    let out = mount.join("mev-rec");
    let probe = Arc::new(FlipProbe::new(&mount));
    let guard = MountGuard::new(Some(mount.clone()), probe);
    guard.validate_startup(&out).unwrap();

    create_out_dir(&out, &guard).unwrap();
    assert!(out.is_dir(), "out_dir was not created");
    // An existing directory is fine.
    create_out_dir(&out, &guard).unwrap();
    assert!(!guard.is_tripped());
}

#[test]
fn guarded_create_out_dir_refuses_when_the_mount_flips_after_validation() {
    let mount_tmp = temp_dir("create-flip");
    let mount = mount_tmp.path().to_path_buf();
    let out = mount.join("mev-rec");
    let probe = Arc::new(FlipProbe::new(&mount));
    let guard = MountGuard::new(Some(mount.clone()), probe.clone());
    guard.validate_startup(&out).unwrap();

    // The mount disappears between the startup validation and the create.
    probe.set_mounted(false);
    let err = create_out_dir(&out, &guard).unwrap_err();
    assert!(
        format!("{err:#}").contains("require_mount"),
        "unexpected error: {err:#}"
    );
    assert!(!out.exists(), "out_dir must not be created after a flip");
    assert!(guard.is_tripped());
}

#[test]
fn guarded_create_out_dir_never_creates_parents() {
    let mount_tmp = temp_dir("create-deep");
    let mount = mount_tmp.path().to_path_buf();
    let out = mount.join("a/b");
    let probe = Arc::new(FlipProbe::new(&mount));
    let guard = MountGuard::new(Some(mount.clone()), probe);
    guard.validate_startup(&out).unwrap();

    let err = create_out_dir(&out, &guard).unwrap_err();
    assert!(
        format!("{err:#}").contains("direct child"),
        "unexpected error: {err:#}"
    );
    assert!(!mount.join("a").exists(), "parents must not be created");
}

#[test]
fn create_out_dir_is_unchanged_when_unguarded() {
    let tmp = temp_dir("create-unguarded");
    let out = tmp.path().join("nested/rec");
    create_out_dir(&out, &MountGuard::unguarded()).unwrap();
    assert!(out.is_dir());
}

/// Exit is non-zero whenever the guard tripped, including a write that
/// tripped it inside a writer's shutdown finalize (R-14 fix1 §2).
#[test]
fn exit_is_non_zero_when_the_guard_tripped_during_shutdown() {
    let guard = MountGuard::unguarded();
    assert!(recorder_exit(false, &guard).is_ok());
    assert!(recorder_exit(true, &guard).is_err(), "explicit trip");

    // A writer trips after the SIGTERM decision was taken: the post-join
    // re-read must still fail the run.
    guard.trip();
    let err = recorder_exit(false, &guard).unwrap_err();
    assert!(
        err.to_string().contains("no longer a mount"),
        "unexpected error: {err}"
    );
}

/// A probe that outruns its bound is a failed check and trips the guard
/// (R-14 fix1 §4).
#[tokio::test]
async fn probe_timeout_trips_the_guard() {
    let tmp = temp_dir("probe-timeout");
    let mount = std::fs::canonicalize(tmp.path()).unwrap();
    let guard = Arc::new(MountGuard::new(
        Some(mount.clone()),
        Arc::new(SlowProbe {
            mount,
            delay: Duration::from_millis(150),
        }),
    ));
    // The probe itself would report the mount healthy; only the bound makes
    // this a failure.
    assert!(!probe_mount(guard.clone(), Duration::from_millis(20)).await);
    assert!(guard.is_tripped());
}

#[test]
fn deribit_defaults_to_disabled() {
    let config = parse_toml("[profile.default]\nnetwork = \"mainnet\"\n").unwrap();
    let deribit = &config.profile.get("default").unwrap().deribit;
    assert!(!deribit.enabled, "deribit is on unless explicitly enabled");
    assert_eq!(
        deribit.currencies,
        vec!["BTC".to_string(), "ETH".to_string()]
    );
    assert_eq!(deribit.base_url, DEFAULT_BASE_URL);
}

#[test]
fn deribit_enabled_parses_currencies_and_base_url() {
    let config = parse_toml(
        "[profile.default.deribit]\n\
             enabled = true\n\
             currencies = [\"SOL\"]\n\
             base_url = \"https://example.test/api/v2\"\n",
    )
    .unwrap();
    let deribit = &config.profile.get("default").unwrap().deribit;
    assert!(deribit.enabled);
    assert_eq!(deribit.currencies, vec!["SOL".to_string()]);
    assert_eq!(deribit.base_url, "https://example.test/api/v2");
}

#[test]
fn deribit_enabled_with_empty_currencies_is_rejected() {
    let err = parse_toml(
        "[profile.default.deribit]\n\
             enabled = true\n\
             currencies = []\n",
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("currencies"),
        "unexpected error: {err:#}"
    );
}

/// End-to-end wiring: an enabled deribit profile polls a mock server and
/// leaves a finalized `deribit` segment holding `rest` envelopes.
#[tokio::test]
async fn deribit_enabled_writes_rest_segment() {
    let tmp = temp_dir("deribit");
    let dir = tmp.path();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{\"result\":[]}"))
        .mount(&server)
        .await;

    let profile = Profile {
        out_dir: dir.to_path_buf(),
        deribit: DeribitSection {
            enabled: true,
            currencies: vec!["BTC".to_string()],
            base_url: server.uri(),
        },
        ..Profile::default()
    };
    profile.validate().unwrap();

    let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
    let writer = Arc::new(
        SegmentWriter::spawn(segment_config(
            &profile,
            Network::Testnet,
            "deribit",
            "deribit",
            &SegmentOpenMeta::default(),
            clock.clone(),
            1,
            Arc::new(MountGuard::unguarded()),
        ))
        .unwrap(),
    );
    let health = RecorderHealth::new(Vec::new(), Vec::new());
    let shutdown = Arc::new(Notify::new());
    let task = tokio::spawn(run_deribit(
        deribit_config(&profile),
        writer,
        clock,
        health,
        shutdown.clone(),
    ));

    // One round is the options summary plus the index price for one currency.
    let mut served = 0;
    for _ in 0..200 {
        served = server
            .received_requests()
            .await
            .map(|requests| requests.len())
            .unwrap_or(0);
        if served >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(served >= 2, "deribit source did not poll the mock");
    // Give the source a moment to consume the responses and write the sink.
    tokio::time::sleep(Duration::from_millis(50)).await;

    shutdown.notify_one();
    task.await.unwrap();

    let files = segment_files(dir);
    assert!(!files.is_empty(), "no deribit segment was written");
    let report = reader::inspect(&files).unwrap();
    assert!(
        report.by_src.get("deribit").copied().unwrap_or(0) > 0,
        "segment has no deribit records: {:#?}",
        report.by_src
    );
    assert!(
        report.by_kind.get("rest").copied().unwrap_or(0) >= 2,
        "expected rest envelopes: {:#?}",
        report.by_kind
    );
    assert!(
        report.by_kind.get("segment_close").copied().unwrap_or(0) == 1,
        "segment did not finalize: {:#?}",
        report.by_kind
    );
}

/// `cex_venues` maps the three `[cex]` lists to their source ids.
#[test]
fn cex_venues_map_to_the_spec_srcs() {
    let profile = Profile {
        cex: CexSection {
            binance_usdm: vec!["BTCUSDT".to_string()],
            binance_spot: vec![],
            bybit_linear: vec!["ETHUSDT".to_string()],
        },
        ..Profile::default()
    };
    let venues = cex_venues(&profile);
    assert_eq!(venues[0].0.src(), "binance-usdm");
    assert_eq!(venues[1].0.src(), "binance-spot");
    assert_eq!(venues[2].0.src(), "bybit-linear");
    assert_eq!(venues[0].1, &vec!["BTCUSDT".to_string()]);
    assert!(venues[1].1.is_empty());
    assert_eq!(venues[2].1, &vec!["ETHUSDT".to_string()]);
}

/// The default profile enables all three CEX venues (SPEC-0008 §7.3).
#[test]
fn default_profile_has_non_empty_cex_lists() {
    // `Profile::default()` has empty lists; the §7.3 symbols live in the
    // repository's `config/record.toml`. Parse that file to prove the
    // wiring sees them (guards against a silent rename of the `[cex]` keys).
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("config/record.toml");
    let config = parse_toml(&std::fs::read_to_string(path).unwrap()).unwrap();
    let default = config.profile.get("default").unwrap();
    assert!(!default.cex.binance_usdm.is_empty());
    assert!(!default.cex.binance_spot.is_empty());
    assert!(!default.cex.bybit_linear.is_empty());
}

/// A CEX source pointed at a mock server writes a finalized `bybit-linear`
/// segment with `frame` envelopes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cex_source_writes_a_bybit_linear_segment() {
    let tmp = temp_dir("cex-bybit");
    let dir = tmp.path();

    // Mock WS server: accept one connection, push frames, and read the
    // subscribe message.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let _ = socket.next().await; // the subscribe frame
        let frame = r#"{"topic":"orderbook.1.BTCUSDT","data":{"b":[["1","1"]]}}"#;
        loop {
            if socket.send(Message::Text(frame.into())).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
    let writer = Arc::new(
        SegmentWriter::spawn(segment_config(
            &Profile {
                out_dir: dir.to_path_buf(),
                ..Profile::default()
            },
            Network::Testnet,
            "bybit-linear",
            "bybit-linear",
            &SegmentOpenMeta::default(),
            clock.clone(),
            1,
            Arc::new(MountGuard::unguarded()),
        ))
        .unwrap(),
    );
    let sink = Arc::new(CountingSink {
        writer: writer.clone(),
        clock: clock.clone(),
        health: RecorderHealth::new(Vec::new(), Vec::new()),
        metrics: ConnMetrics::new("bybit-linear", "bybit-linear"),
        state: ConnState::new(),
        account_hl_rest: false,
    });
    let config = CexConfig {
        base_url: format!("ws://{addr}"),
        ..CexConfig::new(CexKind::BybitLinear, vec!["BTCUSDT".to_string()])
    };
    let source = CexSource::new(config, sink, clock);
    let shutdown = Arc::new(Notify::new());
    let task = tokio::spawn(run_cex(source, shutdown.clone()));

    // Wait for at least one frame to reach the segment writer.
    for _ in 0..400 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        let has_bytes = segment_files(dir)
            .first()
            .and_then(|path| std::fs::metadata(path).ok())
            .map(|meta| meta.len())
            .unwrap_or(0)
            > 0;
        if has_bytes {
            break;
        }
    }
    shutdown.notify_one();
    task.await.unwrap();
    server.abort();
    drop(writer);

    let files = segment_files(dir);
    assert!(!files.is_empty(), "no bybit-linear segment was written");
    let report = reader::inspect(&files).unwrap();
    assert!(
        report.by_src.get("bybit-linear").copied().unwrap_or(0) > 0,
        "segment has no bybit-linear records: {:#?}",
        report.by_src
    );
    assert!(
        report.by_kind.get("frame").copied().unwrap_or(0) > 0,
        "expected frame envelopes: {:#?}",
        report.by_kind
    );
    assert!(
        report.by_kind.get("segment_close").copied().unwrap_or(0) == 1,
        "segment did not finalize: {:#?}",
        report.by_kind
    );
}

/// End-to-end wiring test: a mock WS server and a mock REST server feed the
/// real connection/snapshotter tasks for ~2 s; the resulting segments must
/// contain `segment_open`, `sub`, `frame`, and `segment_close`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recorder_writes_segments_from_mock_feeds() {
    let tmp = temp_dir("e2e");
    let dir = tmp.path();

    // Mock WS server: accept one connection, then push a frame every 50 ms.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_addr = listener.local_addr().unwrap();
    let (ws_shutdown_tx, mut ws_shutdown_rx) = watch::channel(false);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        loop {
            tokio::select! {
                _ = ws_shutdown_rx.changed() => break,
                _ = tick.tick() => {
                    let frame = r#"{"channel":"bbo","data":{"coin":"BTC"}}"#;
                    if socket.send(Message::Text(frame.to_string().into())).await.is_err() {
                        break;
                    }
                }
                message = socket.next() => {
                    if message.is_none() { break; }
                }
            }
        }
    });

    // Mock REST server.
    let rest = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&rest)
        .await;

    let network = Network::Testnet;
    let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let rest_shutdown = Arc::new(Notify::new());
    let health = RecorderHealth::new(vec![("hl-ws-01".to_string(), ConnState::new())], Vec::new());

    // WS connection task.
    let conn = Connection {
        id: "hl-ws-01".to_string(),
        subs: vec![Subscription {
            stream: Stream::Bbo,
            coin: Some("BTC".to_string()),
            dex: None,
        }],
    };
    let writer = SegmentWriter::spawn(segment_config(
        &Profile {
            out_dir: dir.to_path_buf(),
            ..Profile::default()
        },
        network,
        "hl-ws",
        "hl-ws-01",
        &SegmentOpenMeta::default(),
        clock.clone(),
        1,
        Arc::new(MountGuard::unguarded()),
    ))
    .unwrap();
    let ws_url = format!("ws://{ws_addr}");
    let ws_task = tokio::spawn(run_ws_conn(
        conn,
        Box::new(move || {
            Box::new(TestProtocol {
                url: ws_url.clone(),
            }) as Box<dyn Protocol>
        }),
        writer,
        clock.clone(),
        ConnState::new(),
        shutdown_rx.clone(),
        Duration::ZERO,
    ));

    // REST task.
    let rest_writer = Arc::new(
        SegmentWriter::spawn(segment_config(
            &Profile {
                out_dir: dir.to_path_buf(),
                ..Profile::default()
            },
            network,
            "hl-rest",
            "hl-rest",
            &SegmentOpenMeta::default(),
            clock.clone(),
            1,
            Arc::new(MountGuard::unguarded()),
        ))
        .unwrap(),
    );
    let snapshotter = SnapshotterConfig {
        base_url: rest.uri(),
        out_dir: dir.to_path_buf(),
        funding_coins: Vec::new(),
        candle_coins: Vec::new(),
        meta_refresh: Duration::from_secs(3600),
        ctx_interval: Duration::from_secs(3600),
        predicted_fundings_interval: Duration::from_secs(3600),
        ..SnapshotterConfig::default()
    };
    let rest_task = tokio::spawn(run_rest(
        snapshotter,
        rest_writer,
        clock.clone(),
        health,
        rest_shutdown.clone(),
    ));

    tokio::time::sleep(Duration::from_millis(2_000)).await;
    let _ = shutdown_tx.send(true);
    let _ = ws_shutdown_tx.send(true);
    rest_shutdown.notify_one();
    let _ = ws_task.await;
    let _ = rest_task.await;
    let _ = server.await;

    let files = segment_files(dir);
    assert!(!files.is_empty(), "no segment files were written");
    let report = reader::inspect(&files).unwrap();
    for kind in ["segment_open", "sub", "frame", "segment_close"] {
        assert!(
            report.by_kind.get(kind).copied().unwrap_or(0) > 0,
            "missing {kind} in {:#?}",
            report.by_kind
        );
    }
    assert!(report.records >= 4);
}

/// The HL path (`run_ws_conn`) stamps a `gap_start` at the disconnect and a
/// `gap_end` at the reopen, so the recorded pair covers the real outage
/// (SPEC-0008 RW-2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hl_ws_gap_covers_the_real_downtime() {
    const DOWNTIME: Duration = Duration::from_millis(500);
    let tmp = temp_dir("hl-gap");
    let dir = tmp.path();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_addr = listener.local_addr().unwrap();
    let (reconnected_tx, mut reconnected_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let server = tokio::spawn(async move {
        // Round 0: subscribe, one frame, then a clean close.
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let _ = socket.next().await; // subscribe
        let _ = socket
            .send(Message::Text(r#"{"channel":"bbo","data":{}}"#.into()))
            .await;
        let _ = socket.close(None).await;
        drop(socket);
        // Stay down for DOWNTIME before completing the next handshake.
        tokio::time::sleep(DOWNTIME).await;
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let _ = socket.next().await; // subscribe
        let _ = reconnected_tx.send(());
        while let Some(Ok(_)) = socket.next().await {}
    });

    let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
    let writer = SegmentWriter::spawn(segment_config(
        &Profile {
            out_dir: dir.to_path_buf(),
            ..Profile::default()
        },
        Network::Testnet,
        "hl-ws",
        "hl-ws-01",
        &SegmentOpenMeta::default(),
        clock.clone(),
        1,
        Arc::new(MountGuard::unguarded()),
    ))
    .unwrap();
    let conn = Connection {
        id: "hl-ws-01".to_string(),
        subs: vec![Subscription {
            stream: Stream::Bbo,
            coin: Some("BTC".to_string()),
            dex: None,
        }],
    };
    let ws_url = format!("ws://{ws_addr}");
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let ws_task = tokio::spawn(run_ws_conn(
        conn,
        Box::new(move || {
            Box::new(TestProtocol {
                url: ws_url.clone(),
            }) as Box<dyn Protocol>
        }),
        writer,
        clock.clone(),
        ConnState::new(),
        shutdown_rx,
        Duration::ZERO,
    ));

    // Wait for the reconnect to complete, let `gap_end` flush, then stop.
    reconnected_rx.recv().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = shutdown_tx.send(true);
    let _ = ws_task.await;
    server.abort();

    let files = segment_files(dir);
    assert!(!files.is_empty(), "no hl-ws segment was written");
    let report = reader::inspect(&files).unwrap();
    assert!(
        report.gap_total_ms >= DOWNTIME.as_millis() as u64 * 3 / 4,
        "gap_total_ms {} does not cover {DOWNTIME:?}",
        report.gap_total_ms
    );

    let mut gap_start = None;
    let mut gap_end = None;
    for file in &files {
        for env in reader::read_envelopes(file).unwrap() {
            match env.kind {
                // The first `gap_start` is the outage; the shutdown one
                // follows it.
                Kind::GapStart if gap_start.is_none() => gap_start = Some(env),
                Kind::GapEnd => gap_end = Some(env),
                _ => {}
            }
        }
    }
    let (start, end) = (
        gap_start.expect("an outage gap_start"),
        gap_end.expect("an outage gap_end"),
    );
    let recorded_ns = end.t_ns.saturating_sub(start.t_ns).max(0) as u64;
    assert!(
        recorded_ns >= DOWNTIME.as_nanos() as u64 * 3 / 4,
        "recorded gap {recorded_ns} ns does not cover {DOWNTIME:?}"
    );
    assert_eq!(
        end.meta.unwrap()["gap_ms"].as_u64().unwrap(),
        recorded_ns / 1_000_000
    );
}

/// A run clock that follows paused tokio time, so a several-minute outage
/// can be driven without sleeping for real. The wall clock starts at the
/// system time so it lines up with `RawWsConn`'s own disconnect stamps.
struct PausedEnvelopeClock {
    base_ns: i64,
    start: tokio::time::Instant,
}

impl PausedEnvelopeClock {
    fn new() -> Self {
        Self {
            base_ns: now_epoch_ms() as i64 * 1_000_000,
            start: tokio::time::Instant::now(),
        }
    }
}

impl hl_arb_core::clock::Clock for PausedEnvelopeClock {
    fn now_ms(&self) -> u64 {
        (self.t_ns().max(0) as u64) / 1_000_000
    }
}

impl EnvelopeClock for PausedEnvelopeClock {
    fn t_ns(&self) -> i64 {
        self.base_ns + self.start.elapsed().as_nanos() as i64
    }

    fn mono_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }
}

/// `run_ws_conn` races `raw.next()` against its 60 s clock tick. Across an
/// outage longer than several ticks it must still record exactly one outage
/// `gap_start` and one `gap_end`, with `gap_ms` covering the whole outage
/// (SPEC-0008 RW-3).
#[tokio::test(start_paused = true)]
async fn hl_ws_gap_is_one_pair_across_clock_ticks() {
    const REFUSAL: Duration = Duration::from_secs(240);
    let tmp = temp_dir("hl-gap-ticks");
    let dir = tmp.path();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_addr = listener.local_addr().unwrap();
    let (reconnected_tx, mut reconnected_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    tokio::spawn(async move {
        // Round 0: read the subscribe, one frame, then close.
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let _ = socket.next().await; // subscribe
        let _ = socket
            .send(Message::Text(r#"{"channel":"bbo","data":{}}"#.into()))
            .await;
        let _ = socket.close(None).await;
        drop(socket);
        // Refuse by holding each dial without completing the handshake, so
        // the 60 s tick cancels the reconnect mid-dial several times. The
        // first connection accepted at/after the deadline is handshaked.
        let deadline = tokio::time::Instant::now() + REFUSAL;
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            if tokio::time::Instant::now() < deadline {
                tokio::time::sleep_until(deadline).await;
                drop(stream);
                continue;
            }
            if let Ok(mut ws) = accept_async(stream).await {
                let _ = ws.next().await; // resubscribe => the client is up
                let _ = reconnected_tx.send(());
                while let Some(Ok(_)) = ws.next().await {}
            }
        }
    });

    let clock: Arc<dyn EnvelopeClock> = Arc::new(PausedEnvelopeClock::new());
    let writer = SegmentWriter::spawn(segment_config(
        &Profile {
            out_dir: dir.to_path_buf(),
            ..Profile::default()
        },
        Network::Testnet,
        "hl-ws",
        "hl-ws-01",
        &SegmentOpenMeta::default(),
        clock.clone(),
        1,
        Arc::new(MountGuard::unguarded()),
    ))
    .unwrap();
    let conn = Connection {
        id: "hl-ws-01".to_string(),
        subs: vec![Subscription {
            stream: Stream::Bbo,
            coin: Some("BTC".to_string()),
            dex: None,
        }],
    };
    let ws_url = format!("ws://{ws_addr}");
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let ws_task = tokio::spawn(run_ws_conn(
        conn,
        Box::new(move || {
            Box::new(TestProtocol {
                url: ws_url.clone(),
            }) as Box<dyn Protocol>
        }),
        writer,
        clock.clone(),
        ConnState::new(),
        shutdown_rx,
        Duration::ZERO,
    ));

    // The server signals after it read the reconnect's subscribe frame;
    // yield so the client processes `Opened` and writes `gap_end` before
    // the shutdown gap_start.
    reconnected_rx.recv().await.unwrap();
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    let _ = shutdown_tx.send(true);
    let _ = ws_task.await;

    let files = segment_files(dir);
    assert!(!files.is_empty(), "no hl-ws segment was written");
    let mut outage_starts = 0u32;
    let mut gap_end = None;
    for file in &files {
        for env in reader::read_envelopes(file).unwrap() {
            match env.kind {
                Kind::GapStart => {
                    let reason = env
                        .meta
                        .as_ref()
                        .and_then(|m| m.get("reason"))
                        .and_then(|r| r.as_str())
                        .unwrap_or("");
                    if reason != "shutdown" {
                        outage_starts += 1;
                    }
                }
                Kind::GapEnd => gap_end = Some(env),
                _ => {}
            }
        }
    }
    assert_eq!(
        outage_starts, 1,
        "the outage was split into {outage_starts} gap_starts across clock ticks"
    );
    let end = gap_end.expect("an outage gap_end");
    let gap_ms = end.meta.as_ref().unwrap()["gap_ms"].as_u64().unwrap();
    assert!(
        gap_ms >= Duration::from_secs(210).as_millis() as u64,
        "gap_ms {gap_ms} does not cover the 4-minute outage"
    );
}

fn empty_report() -> reader::VerifyReport {
    reader::VerifyReport {
        date: "2026-01-01".into(),
        files: Vec::new(),
        orphans: Vec::new(),
        crashed_no_manifest: Vec::new(),
        unfinalized: Vec::new(),
        corrupt_orphans: Vec::new(),
        partials: Vec::new(),
        coverage: Vec::new(),
    }
}

fn orphan_entry(file: &str) -> hl_arb_recorder::ManifestEntry {
    hl_arb_recorder::ManifestEntry {
        file: file.into(),
        src: "hl-ws".into(),
        conn: "hl-ws-01".into(),
        first_t_ns: 1,
        last_t_ns: 2,
        records: 3,
        bytes_raw: 4,
        bytes_zst: 5,
        crashed: false,
    }
}

fn unmanifested(file: &str) -> reader::UnmanifestedSegment {
    reader::UnmanifestedSegment {
        file: file.into(),
        src: "hl-ws".into(),
        conn: "hl-ws-01".into(),
        records: 3,
    }
}

/// `verify` exits non-zero for orphans unless `--allow-orphans` is passed,
/// and a `.partial` file (a recorder may be running) downgrades them to a
/// warning.
#[test]
fn verify_exit_orphans_allow_and_partial_downgrade() {
    let mut report = empty_report();
    report.orphans.push(orphan_entry(
        "testnet/hl-ws/2026-01-01/00/hl-ws-01-1.jsonl.zst",
    ));
    assert!(verify_exit(&report, false, false).is_err());
    assert!(verify_exit(&report, true, false).is_ok());

    // A `.partial` means the orphans may be in flight: do not fail.
    report
        .partials
        .push("testnet/hl-ws/2026-01-01/00/hl-ws-01-2.jsonl.zst.partial".into());
    assert!(verify_exit(&report, false, false).is_ok());

    assert!(verify_exit(&empty_report(), false, false).is_ok());
}

/// A `.crashed` segment with no manifest line warns by default and fails
/// only with `--strict`; the wording says how to recover, never "run
/// repair-manifest".
#[test]
fn verify_exit_strict_on_crashed() {
    let mut report = empty_report();
    report.crashed_no_manifest.push(unmanifested(
        "testnet/hl-ws/2026-01-01/00/hl-ws-01-9.jsonl.zst.crashed",
    ));
    assert!(
        verify_exit(&report, false, false).is_ok(),
        "a crashed segment without a manifest line must warn, not fail"
    );
    let err = verify_exit(&report, false, true)
        .expect_err("--strict must fail on a crashed segment without a manifest line");
    let msg = err.to_string();
    assert!(
        msg.contains("not repairable by repair-manifest")
            && msg.contains("recover by restarting the recorder"),
        "message should say how to recover: {msg}"
    );
    assert!(
        !msg.contains("repair-manifest --date"),
        "must not tell the operator to run repair-manifest: {msg}"
    );
}

/// An unfinalized segment with no manifest line cannot be repaired by
/// `repair-manifest`; it fails unless `--allow-orphans`.
#[test]
fn verify_exit_unfinalized_fails_unless_allowed() {
    let mut report = empty_report();
    report.unfinalized.push(unmanifested(
        "testnet/hl-ws/2026-01-01/00/hl-ws-01-8.jsonl.zst",
    ));
    let err = verify_exit(&report, false, false).expect_err("unfinalized must fail");
    assert!(
        err.to_string()
            .contains("not repairable by repair-manifest"),
        "{err}"
    );
    assert!(verify_exit(&report, true, false).is_ok());
}

/// A corrupt orphan fails by default (consistently with repair's `Skip`) and
/// passes with `--allow-orphans`.
#[test]
fn verify_exit_corrupt_orphans() {
    let mut report = empty_report();
    report.corrupt_orphans.push(reader::CorruptOrphan {
        file: "testnet/hl-ws/2026-01-01/00/hl-ws-01-7.jsonl.zst".into(),
        error: "envelope decode error".into(),
    });
    let err = verify_exit(&report, false, false).expect_err("corrupt orphan must fail");
    assert!(err.to_string().contains("--allow-orphans"), "{err}");
    assert!(verify_exit(&report, true, false).is_ok());
}
