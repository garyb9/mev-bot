//! `hl probe latency` measurement (SPEC-0008 §12.1).

use super::*;

// ---------------------------------------------------------------------------
// `hl probe latency`
// ---------------------------------------------------------------------------

/// Measure TCP connect, TLS handshake, WS ping→pong, and `/info` `allMids` RTT
/// (SPEC-0008 §12.1, used by V-4).
pub async fn probe_latency(network: Option<Network>, count: u32) -> Result<()> {
    let network = network.unwrap_or(Network::Mainnet);
    let count = count.max(1);
    let (host, port) = authority(network.rest_url())?;
    let ws_url = network.ws_url().to_string();
    let http = HttpInfo::new(network);
    let tls = tls_measurement_config();

    let mut tcp_us = Vec::new();
    let mut tls_us = Vec::new();
    let mut ws_us = Vec::new();
    let mut info_us = Vec::new();

    for iteration in 0..count {
        match measure_tcp_tls(&host, port, tls.clone()).await {
            Ok((tcp, handshake)) => {
                tcp_us.push(tcp as f64);
                tls_us.push(handshake as f64);
            }
            Err(err) => warn!(iteration, error = %err, "tcp/tls probe failed"),
        }
        match measure_ws_ping(&ws_url).await {
            Ok(rtt) => ws_us.push(rtt as f64),
            Err(err) => warn!(iteration, error = %err, "ws ping probe failed"),
        }
        let started = Instant::now();
        match http.all_mids().await {
            Ok(_) => info_us.push(started.elapsed().as_micros() as f64),
            Err(err) => warn!(iteration, error = %err, "info probe failed"),
        }
    }

    if tcp_us.is_empty() && ws_us.is_empty() && info_us.is_empty() {
        bail!("all latency probes failed; is the network reachable?");
    }

    println!("target: {host}:{port} (websocket {ws_url})");
    println!("samples: {} of {count}", info_us.len().max(tcp_us.len()));
    print_latency("tcp connect", &tcp_us);
    print_latency("tls handshake", &tls_us);
    print_latency("ws ping->pong", &ws_us);
    print_latency("info allMids", &info_us);
    Ok(())
}

/// Print p50/p90/max in milliseconds for a microsecond sample set.
pub(super) fn print_latency(label: &str, samples_us: &[f64]) {
    if samples_us.is_empty() {
        println!("  {label:<16} (no samples)");
        return;
    }
    let mut sorted = samples_us.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    println!(
        "  {label:<16} p50={:.3} ms  p90={:.3} ms  max={:.3} ms  (n={})",
        percentile(&sorted, 0.5) / 1_000.0,
        percentile(&sorted, 0.9) / 1_000.0,
        sorted[sorted.len() - 1] / 1_000.0,
        sorted.len()
    );
}

/// Nearest-rank percentile over a sorted slice.
pub(super) fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

/// Split a URL into `(host, port)` with default ports for http(s)/ws(s).
pub(super) fn authority(url: &str) -> Result<(String, u16)> {
    let (scheme, rest) = url
        .split_once("://")
        .with_context(|| format!("malformed url `{url}`"))?;
    let host_port = rest.split('/').next().unwrap_or(rest);
    let default_port = if scheme.ends_with('s') { 443 } else { 80 };
    match host_port.rsplit_once(':') {
        Some((host, port)) => Ok((host.to_string(), port.parse()?)),
        None => Ok((host_port.to_string(), default_port)),
    }
}

/// Connect and complete a rustls handshake, returning `(tcp_us, tls_us)`.
pub(super) async fn measure_tcp_tls(
    host: &str,
    port: u16,
    config: Arc<rustls::ClientConfig>,
) -> Result<(u64, u64)> {
    let host = host.to_string();
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let socket = std::net::TcpStream::connect((host.as_str(), port))?;
        let tcp_us = started.elapsed().as_micros() as u64;
        let server_name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|err| anyhow::anyhow!("invalid server name: {err}"))?;
        let connection = rustls::ClientConnection::new(config, server_name)
            .map_err(|err| anyhow::anyhow!("tls client: {err}"))?;
        let mut stream = rustls::StreamOwned::new(connection, socket);
        let started = Instant::now();
        while stream.conn.is_handshaking() {
            stream.conn.complete_io(&mut stream.sock)?;
        }
        Ok::<_, anyhow::Error>((tcp_us, started.elapsed().as_micros() as u64))
    })
    .await?
}

/// Open a WS connection, send one app ping, and time the pong.
pub(super) async fn measure_ws_ping(ws_url: &str) -> Result<u64> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (mut socket, _) = tokio_tungstenite::connect_async(ws_url).await?;
    let started = Instant::now();
    socket
        .send(Message::Text(r#"{"method":"ping"}"#.to_string().into()))
        .await?;
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => {
                let is_pong = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("channel")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .as_deref()
                    == Some("pong");
                if is_pong {
                    return Ok(started.elapsed().as_micros() as u64);
                }
            }
            Some(Ok(_)) => {}
            Some(Err(err)) => return Err(err.into()),
            None => bail!("websocket closed before pong"),
        }
    }
}

/// A rustls config that skips certificate verification for the latency probe.
///
/// The probe transmits no application data over the TLS connection; it only
/// times the handshake. Using the system root store would add a dependency for
/// no benefit here (SPEC-0008 §12.1).
pub(super) fn tls_measurement_config() -> Arc<rustls::ClientConfig> {
    let provider = rustls::crypto::ring::default_provider();
    let _ = provider.clone().install_default();
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert(Arc::new(provider))))
        .with_no_client_auth();
    Arc::new(config)
}

/// Certificate verifier that accepts everything (probe only).
pub(super) struct AcceptAnyServerCert(Arc<rustls::crypto::CryptoProvider>);

impl std::fmt::Debug for AcceptAnyServerCert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcceptAnyServerCert")
            .finish_non_exhaustive()
    }
}

impl rustls::client::danger::ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _certificate: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _certificate: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
