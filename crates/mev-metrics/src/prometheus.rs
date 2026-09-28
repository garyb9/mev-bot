//! Prometheus recorder installation, upkeep, and rendering for `/metrics`.
//!
//! `PrometheusBuilder::install_recorder` deliberately does **not** start the
//! recorder's upkeep task (metrics-exporter-prometheus 0.16): histogram samples
//! buffer in memory and are only folded into their distributions when a scrape
//! renders the metrics, so in a long run that is never scraped the buffers grow
//! without bound. We therefore run the upkeep on a background thread here.

use std::time::Duration;

use metrics_exporter_prometheus::PrometheusBuilder;
pub use metrics_exporter_prometheus::PrometheusHandle;

/// How often the recorder's upkeep runs.
pub const UPKEEP_INTERVAL: Duration = Duration::from_secs(5);

/// Install the global Prometheus recorder and return a handle whose
/// [`render`](PrometheusHandle::render) output is served at `/metrics`.
///
/// Also starts a background upkeep task ([`spawn_periodic`]) that calls
/// [`PrometheusHandle::run_upkeep`] every [`UPKEEP_INTERVAL`], so histogram
/// buffers cannot grow unboundedly in a run that is never scraped.
///
/// Panics only if a recorder was already installed by other means.
pub fn install_recorder() -> PrometheusHandle {
    let handle = PrometheusBuilder::new()
        .install_recorder()
        .expect("failed to install Prometheus recorder");
    let upkeep = handle.clone();
    spawn_periodic(UPKEEP_INTERVAL, move || upkeep.run_upkeep());
    handle
}

/// Spawn a detached background thread that calls `task` every `interval`.
///
/// The first call happens after one interval. The thread runs for the process
/// lifetime; a detached `std` thread does not keep the process alive.
pub(crate) fn spawn_periodic<F>(interval: Duration, mut task: F)
where
    F: FnMut() + Send + 'static,
{
    std::thread::Builder::new()
        .name("mev-metrics-upkeep".to_string())
        .spawn(move || {
            loop {
                std::thread::sleep(interval);
                task();
            }
        })
        .expect("spawn metrics upkeep thread");
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    use super::*;

    #[test]
    fn periodic_task_runs_repeatedly() {
        let runs = Arc::new(AtomicU64::new(0));
        let counter = runs.clone();
        spawn_periodic(Duration::from_millis(1), move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });

        // Non-flaky: a 2 s deadline for a 1 ms task only needs the scheduler.
        let deadline = Instant::now() + Duration::from_secs(2);
        while runs.load(Ordering::SeqCst) < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            runs.load(Ordering::SeqCst) >= 3,
            "periodic task did not run repeatedly"
        );
    }
}
