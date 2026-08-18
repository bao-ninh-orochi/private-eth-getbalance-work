//! Stage 0.5 conformance harness — fast smoke test at a SMALL scale
//! (`docs/plan.md` §6). Proves the harness itself is wired correctly
//! (every category populated, zero mismatches) well under the real
//! gate's >= 1000 x >= 100 target, so the normal `cargo test` run stays
//! fast. The real gate (`ConformanceConfig::default()`) is driven via
//! `cargo run -p xtask --release -- conformance`, not here.

use xtask::conformance::{run, ConformanceConfig, CATEGORIES};

#[test]
fn small_conformance_run_passes_with_full_category_coverage() {
    let cfg = ConformanceConfig {
        seed: 0x5CA1_AB1E_C0FF_EE42,
        num_genesis_keys: 3_000,
        blocks: 30,
        min_addresses: 300,
        checkpoints: 5,
        lwe_dim: 512,
    };

    let report = run(&cfg);

    assert!(report.passed, "conformance run must pass:\n{report}");
    assert_eq!(
        report.mismatches.len(),
        0,
        "must be zero mismatches:\n{report}"
    );
    assert!(
        report.sample_size >= cfg.min_addresses,
        "final sample must reach min_addresses:\n{report}"
    );
    assert_eq!(report.blocks, cfg.blocks);
    assert_eq!(
        report.checkpoints_run,
        cfg.checkpoints + 1,
        "checkpoints_run must be the in-run checkpoints plus the dedicated final pass"
    );

    for cat in CATEGORIES {
        assert!(
            report.category_counts.get(cat).copied().unwrap_or(0) > 0,
            "category '{cat}' must be non-empty in the final sample:\n{report}"
        );
    }
}
