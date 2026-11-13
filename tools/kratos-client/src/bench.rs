use std::time::Duration;

use kratos_protocol::{CreateWorkspace, RequiredText};
use serde::Serialize;

/// Latency percentiles in milliseconds, printed as one JSON line.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchReport {
    pub messages: usize,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

pub(crate) fn temp_workspace(name: &str) -> CreateWorkspace {
    CreateWorkspace {
        name: RequiredText::new(name).expect("the name is not blank"),
        path: RequiredText::new(&std::env::temp_dir().to_string_lossy())
            .expect("the temporary folder has a path"),
    }
}

impl BenchReport {
    /// Nearest-rank percentiles of a non-empty list of samples: the p-th percentile of n samples is
    /// the sample at rank `ceil(p / 100 * n)`, counting from 1 in sorted order.
    pub fn from_latencies(mut latencies: Vec<Duration>) -> BenchReport {
        latencies.sort();
        let milliseconds = |percent: usize| {
            let rank = (percent * latencies.len()).div_ceil(100);
            let microseconds = latencies[rank - 1].as_secs_f64() * 1_000_000.0;
            microseconds.round() / 1_000.0
        };
        BenchReport {
            messages: latencies.len(),
            p50_ms: milliseconds(50),
            p90_ms: milliseconds(90),
            p95_ms: milliseconds(95),
            p99_ms: milliseconds(99),
            max_ms: milliseconds(100),
        }
    }
}
