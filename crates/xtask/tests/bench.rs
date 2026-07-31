//! Stage 3 bench harness — fast smoke test at a tiny scale. Proves the
//! harness itself measures self-consistent numbers (every timed quantity
//! is positive, the compact delta codec beats the naive baseline) well
//! under the real gate's multi-minute sweep, so the normal `cargo test`
//! run stays fast. The real gate (`BenchConfig::default()`) is driven via
//! `cargo run -p xtask --release -- bench`, not here.

use xtask::bench::{run, BenchConfig};

#[test]
fn tiny_bench_run_is_self_consistent() {
    let cfg = BenchConfig {
        seed: 0x7EA5_7000_5CA1_E000,
        scales: vec![20_000],
        mid_scale: 20_000,
        k_values: vec![50, 300],
        headline_k: 300,
        warmup_blocks: 2,
        measured_blocks: 3,
        measured_queries: 3,
        block_time_secs: 12.0,
    };

    let report = run(&cfg);

    // Exactly one scale, and it must be the requested (tiny) one — no
    // adaptive fallback should ever trigger for a single small scale.
    assert_eq!(report.scales.len(), 1);
    let scale = &report.scales[0];
    assert_eq!(scale.accounts, 20_000);
    assert_eq!(report.requested_top_scale, 20_000);
    assert_eq!(report.reached_top_scale, 20_000);
    assert!(
        report.top_scale_fallback_reason.is_none(),
        "a single small scale must never trigger the fallback"
    );

    // 1. Full-rebuild time must be a real, positive measurement.
    assert!(
        scale.rebuild.as_nanos() > 0,
        "rebuild time must be measured as > 0"
    );

    // 2. Per-block patch time: the full K curve, every point positive.
    assert_eq!(report.patch_curve.len(), cfg.k_values.len());
    for point in &report.patch_curve {
        assert!(
            point.avg_ms > 0.0,
            "K={} patch time must be measured as > 0",
            point.k
        );
    }
    assert!(
        scale.headline_patch_ms > 0.0,
        "headline patch time must be measured as > 0"
    );

    // 3. Compact delta bytes must beat the naive 10 B/cell baseline.
    assert!(
        report.delta_bytes.nonzero_cells > 0,
        "sanity: the measured delta must be non-empty, or the comparison below is vacuous"
    );
    assert!(
        report.delta_bytes.compact_bytes < report.delta_bytes.naive_bytes,
        "compact delta codec must beat the naive 10 B/cell baseline: compact={} naive={}",
        report.delta_bytes.compact_bytes,
        report.delta_bytes.naive_bytes
    );
    assert!(report.delta_bytes.ratio > 1.0);

    // 5. Answer latency must be a real, positive measurement.
    assert!(
        report.answer_latency.avg_ms > 0.0,
        "answer latency must be measured as > 0"
    );
    assert_eq!(report.answer_latency.n_queries, cfg.measured_queries);

    // Sizes (item 4) are computed, not timed, but must still be internally
    // sane at this tiny scale.
    assert!(scale.sizes.server_db > 0);
    assert!(scale.sizes.load_factor > 0.0 && scale.sizes.load_factor <= 0.75 + 1e-9);

    // The report must render to non-empty markdown without panicking.
    let markdown = report.to_markdown("test-machine", "2026-01-01");
    assert!(markdown.contains("# RisePIR numbers table"));
    assert!(markdown.contains("20,000"));
}
