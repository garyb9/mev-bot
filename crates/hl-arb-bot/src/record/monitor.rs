//! Background disk and readiness monitors (SPEC-0008 §12.2).

use super::connection::RecorderHealth;
use super::*;

// ---------------------------------------------------------------------------
// Background monitors
// ---------------------------------------------------------------------------

/// Sample the free-disk gauge until shutdown.
pub(super) async fn disk_monitor(out_dir: PathBuf, mut shutdown: watch::Receiver<bool>) {
    let disk = SystemDiskSpace;
    let mut tick = tokio::time::interval(DISK_SAMPLE_INTERVAL);
    loop {
        tokio::select! {
            _ = tick.tick() => match disk.free_bytes(&out_dir) {
                Ok(free) => metrics::gauge!(names::REC_DISK_FREE_BYTES).set(free as f64),
                Err(err) => debug!(error = %err, "disk free check failed"),
            },
            _ = shutdown.changed() => break,
        }
    }
}

/// Update `/readyz` from the connection/REST liveness state until shutdown.
///
/// A tripped [`MountGuard`] forces not-ready even while the sockets are still
/// healthy, so `/readyz` reflects that the recorder can no longer write.
///
/// Non-gating CEX streams are reported here (a transition WARN; the
/// `hl_ws_connected{src}` gauge is maintained by `RawWsConn`) but never affect
/// `/readyz`, so a dead reference feed cannot take the recorder out of rotation.
pub(super) async fn readiness_monitor(
    recorder: Arc<RecorderHealth>,
    health: Health,
    clock: Arc<dyn EnvelopeClock>,
    mount_guard: Arc<MountGuard>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(READY_SAMPLE_INTERVAL);
    let mut cex_degraded = false;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let now_ns = clock.mono_ns();
                let ready = !mount_guard.is_tripped() && recorder.ready(
                    now_ns,
                    READY_WATCHDOG.as_nanos() as u64,
                    REST_READY_STALE.as_nanos() as u64,
                );
                health.set_ready(ready);
                match recorder.cex_down(now_ns, READY_WATCHDOG.as_nanos() as u64) {
                    Some(src) if !cex_degraded => {
                        warn!(src, "cex reference stream is stale (non-gating)");
                        cex_degraded = true;
                    }
                    None if cex_degraded => {
                        info!("cex reference streams recovered");
                        cex_degraded = false;
                    }
                    _ => {}
                }
            }
            _ = shutdown.changed() => break,
        }
    }
    health.set_ready(false);
}
