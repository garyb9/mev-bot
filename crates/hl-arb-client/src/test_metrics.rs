//! Test-only helpers for asserting on recorded metrics.
//!
//! `metrics-util`'s debugging recorder is a dev-dependency; this module is
//! compiled only under `cfg(test)` and shared by the crate's test modules.

use metrics_util::debugging::{DebugValue, Snapshot};

/// Every histogram sample recorded for `name`, optionally filtered to a single
/// `label` (`key = value`).
pub(crate) fn histogram_values(
    snapshot: Snapshot,
    name: &str,
    label: Option<(&str, &str)>,
) -> Vec<f64> {
    snapshot
        .into_vec()
        .into_iter()
        .filter_map(|(key, _, _, value)| {
            if key.key().name() != name {
                return None;
            }
            if let Some((want_key, want_value)) = label {
                let matches = key
                    .key()
                    .labels()
                    .any(|l| l.key() == want_key && l.value() == want_value);
                if !matches {
                    return None;
                }
            }
            match value {
                DebugValue::Histogram(values) => Some(
                    values
                        .into_iter()
                        .map(|v| v.into_inner())
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            }
        })
        .flatten()
        .collect()
}

/// Count the histogram samples recorded for `name`, optionally filtered to a
/// single `label` (`key = value`).
pub(crate) fn histogram_samples(
    snapshot: Snapshot,
    name: &str,
    label: Option<(&str, &str)>,
) -> usize {
    histogram_values(snapshot, name, label).len()
}

/// The value of the counter `name`, optionally filtered to a single `label`
/// (`key = value`), in a debugging-recorder snapshot (0 when absent).
pub(crate) fn counter_value(snapshot: Snapshot, name: &str, label: Option<(&str, &str)>) -> u64 {
    snapshot
        .into_vec()
        .into_iter()
        .filter_map(|(key, _, _, value)| {
            if key.key().name() != name {
                return None;
            }
            if let Some((want_key, want_value)) = label
                && !key
                    .key()
                    .labels()
                    .any(|l| l.key() == want_key && l.value() == want_value)
            {
                return None;
            }
            match value {
                DebugValue::Counter(value) => Some(value),
                _ => None,
            }
        })
        .sum()
}
