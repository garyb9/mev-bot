//! Prometheus recorder installation and rendering for `/metrics`.

use metrics_exporter_prometheus::PrometheusBuilder;
pub use metrics_exporter_prometheus::PrometheusHandle;

/// Install the global Prometheus recorder and return a handle whose
/// [`render`](PrometheusHandle::render) output is served at `/metrics`.
///
/// Panics only if a recorder was already installed by other means.
pub fn install_recorder() -> PrometheusHandle {
    PrometheusBuilder::new()
        .install_recorder()
        .expect("failed to install Prometheus recorder")
}
