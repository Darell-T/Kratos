use std::time::Duration;

use kratos_client::BenchReport;

fn milliseconds(count: u64) -> Duration {
    Duration::from_millis(count)
}

/// 1 ms to `count` ms, each once, in an order that is not sorted: a stride coprime with `count`.
fn shuffled_milliseconds(count: u64) -> Vec<Duration> {
    (0..count)
        .map(|index| milliseconds((index * 37) % count + 1))
        .collect()
}

#[test]
fn percentiles_are_nearest_rank_whatever_order_the_samples_arrive_in() {
    // Catches a report that skips the sort, a rank that rounds down (101 samples put p50 at rank
    // 51, not 50), and one that rounds up a whole sample too far (100 samples put p95 at rank 95,
    // not 96).
    let hundred = BenchReport::from_latencies(shuffled_milliseconds(100));
    let hundred_expected = BenchReport {
        messages: 100,
        p50_ms: 50.0,
        p90_ms: 90.0,
        p95_ms: 95.0,
        p99_ms: 99.0,
        max_ms: 100.0,
    };
    assert_eq!(hundred, hundred_expected);

    let hundred_and_one = BenchReport::from_latencies(shuffled_milliseconds(101));
    let hundred_and_one_expected = BenchReport {
        messages: 101,
        p50_ms: 51.0,
        p90_ms: 91.0,
        p95_ms: 96.0,
        p99_ms: 100.0,
        max_ms: 101.0,
    };
    assert_eq!(hundred_and_one, hundred_and_one_expected);
}

#[test]
fn one_sample_is_every_percentile() {
    // Catches an index out of range at the smallest run, and a rank of zero.
    let report = BenchReport::from_latencies(vec![Duration::from_micros(1_234)]);

    let expected = BenchReport {
        messages: 1,
        p50_ms: 1.234,
        p90_ms: 1.234,
        p95_ms: 1.234,
        p99_ms: 1.234,
        max_ms: 1.234,
    };
    assert_eq!(report, expected);
}
