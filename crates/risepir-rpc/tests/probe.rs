//! Integration test for `risepir-rpc probe`: stands up the whole mock
//! deployment in-process on ephemeral ports (the exact same
//! [`risepir_rpc::demo::spawn`] path `src/main.rs` runs), then drives the
//! real probe against it over real HTTP.
//!
//! The independent-provider check is disabled (`--no-confirm`): there is
//! no mainnet here to confirm against, and the thing under test is the
//! probe's *measurement* machinery — that rows are written, that the
//! latency budget closes on real measured numbers rather than only on a
//! hand-built record, that the found/not-found paths both work, and that
//! neither CSV can carry an address.

use std::path::Path;
use std::time::Duration;

use risepir_rpc::demo::{self, DemoConfig};
use risepir_rpc::probe::{self, ProbeConfig, BLOCK_COLUMNS, TRIAL_COLUMNS};

/// Distinctive, so a passing assertion proves this test's own config
/// plumbing rather than coincidentally matching a default.
const TEST_CHAIN_ID: u64 = 1337;

fn demo_config() -> DemoConfig {
    DemoConfig {
        rpc_port: 0,
        pir_port: 0,
        chain_id: TEST_CHAIN_ID,
        // Fast follow cadence keeps the suite quick; every other field
        // stays at the deployment shape `main.rs` runs.
        block_interval: Duration::from_millis(15),
        ..DemoConfig::default()
    }
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("risepir-probe-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Split a CSV file into `(header, data rows)`, asserting the header
/// appears exactly once.
fn read_csv(path: &Path, expected_header: &[&str]) -> (String, Vec<Vec<String>>) {
    let text = std::fs::read_to_string(path).expect("csv readable");
    let mut lines = text.lines();
    let header = lines.next().expect("a header line").to_string();
    assert_eq!(header, expected_header.join(","), "header must be exact");
    let rows: Vec<Vec<String>> = lines
        .map(|l| {
            assert_ne!(l, header, "the header must never repeat mid-file");
            l.split(',').map(str::to_string).collect()
        })
        .collect();
    for r in &rows {
        assert_eq!(
            r.len(),
            expected_header.len(),
            "every row has one field per column"
        );
    }
    (header, rows)
}

fn field<'a>(row: &'a [String], columns: &[&str], name: &str) -> &'a str {
    let i = columns
        .iter()
        .position(|c| *c == name)
        .unwrap_or_else(|| panic!("no column {name}"));
    &row[i]
}

fn num(row: &[String], columns: &[&str], name: &str) -> u64 {
    field(row, columns, name).parse().unwrap_or_else(|_| {
        panic!(
            "column {name} is not a number: {:?}",
            field(row, columns, name)
        )
    })
}

/// Longest run of hex characters — an address is 40 of them.
fn longest_hex_run(s: &str) -> usize {
    let (mut best, mut run) = (0, 0);
    for c in s.chars() {
        if c.is_ascii_hexdigit() {
            run += 1;
            best = best.max(run);
        } else {
            run = 0;
        }
    }
    best
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_measures_the_mock_deployment_end_to_end() {
    let handle = demo::spawn(demo_config()).await;
    let pir_url = format!("http://{}", handle.pir_addr);

    // Let the background follow loop apply a few blocks, so the session
    // has a nonempty ΔD to rewind against and `/sync` has something to
    // serve — i.e. so this exercises the real operating point, not a
    // degenerate at-genesis one.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let dir = scratch("run");
    let queries_csv = dir.join("queries.csv");
    let blocks_csv = dir.join("blocks.csv");
    let addrs = dir.join("addrs.txt");

    // The demo's seeded accounts: every one is in the store, so these
    // trials must all report `found = 1`.
    let list: String = handle
        .demo_accounts
        .iter()
        .map(|(a, _)| {
            let hex: String = a.iter().map(|b| format!("{b:02x}")).collect();
            format!("0x{hex}\n")
        })
        .collect();
    std::fs::write(&addrs, &list).expect("write addresses file");

    let base = ProbeConfig {
        pir_url: pir_url.clone(),
        no_confirm: true,
        queries_csv: queries_csv.clone(),
        blocks_csv: blocks_csv.clone(),
        addresses_file: Some(addrs.clone()),
        batch_size: 3,
        batches: 1,
        batch_interval_secs: 0,
        trial_gap_ms: 0,
        // A short tail so `follow_until` really runs and the blocks CSV
        // gets rows; the mock advances a block every 15 ms.
        follow_secs: 2,
        poll_secs: 1,
        absent_fraction: 0.0,
        rss_every: 1,
        chain_id: TEST_CHAIN_ID,
        ..ProbeConfig::default()
    };

    // ── pass 1: three known-present addresses ─────────────────────────
    let s1 = probe::run(base.clone()).await.expect("probe run 1");
    assert_eq!(s1.trials, 3, "three trials were configured");
    assert_eq!(s1.ok, 3, "every trial must have returned a balance");
    assert_eq!(s1.found, 3, "every seeded demo account must be found");
    assert_eq!(
        s1.absent_probes, 0,
        "--absent-fraction 0 means no random addresses"
    );
    assert_eq!(s1.provider_matched, 0);
    assert_eq!(s1.provider_mismatched, 0, "a mismatch is never acceptable");
    assert_eq!(
        s1.provider_unavailable, 3,
        "the check was disabled, so every trial is 'unavailable'"
    );
    assert!(s1.setup_bytes > 0, "the hint download must have been sized");
    assert!(s1.query_bytes_total > 0 && s1.response_bytes_total > 0);
    assert!(s1.max_at_block >= s1.pinned_block);

    // ── pass 2: only random addresses, exercising the not-found path ──
    // Appended to the same files, which is also the second half of the
    // "header written once" contract in a real setting. `follow_secs: 0`
    // also pins the deadline rule: a batch that started runs to
    // completion, so "do not follow" is not "do nothing".
    let s2 = probe::run(ProbeConfig {
        absent_fraction: 1.0,
        follow_secs: 0,
        ..base
    })
    .await
    .expect("probe run 2");
    assert_eq!(
        s2.trials, 3,
        "--follow-secs 0 must still run the batch to completion"
    );
    assert_eq!(
        s2.absent_probes, 3,
        "every trial took the random-address path"
    );
    assert_eq!(
        s2.found, 0,
        "a uniformly random address is not in the mock's account universe"
    );
    assert_eq!(
        s2.ok, 3,
        "the mock deployment is COMPLETE, so not-found is a legitimate 0x0, not an error"
    );

    // ── the files ─────────────────────────────────────────────────────
    let (_, rows) = read_csv(&queries_csv, TRIAL_COLUMNS);
    assert_eq!(rows.len(), 6, "three rows per pass, header written once");

    for (i, r) in rows.iter().enumerate() {
        let c = TRIAL_COLUMNS;
        assert_eq!(field(r, c, "error"), "", "row {i} must not have errored");

        // (a) The latency budget closes exactly, on measured numbers.
        let total = num(r, c, "t_total_us");
        let parts = num(r, c, "build_us")
            + num(r, c, "head_wire_us")
            + num(r, c, "sync_wire_us")
            + num(r, c, "answer_wire_us")
            + num(r, c, "finish_us")
            + num(r, c, "residual_us");
        assert_eq!(total, parts, "row {i}: A1 must equal its parts + residual");
        assert!(total > 0, "row {i}: a real query takes nonzero time");

        // (b) The finish sub-timers are inside finish, not beside it.
        let finish = num(r, c, "finish_us");
        let sub = num(r, c, "rewind_us")
            + num(r, c, "decode_us")
            + num(r, c, "delta_apply_us")
            + num(r, c, "scan_us");
        assert!(
            sub <= finish,
            "row {i}: the four rewind steps ({sub}us) must fit inside finish ({finish}us)"
        );

        // (c) Bytes and blocks are real.
        assert!(num(r, c, "query_bytes") > 0, "row {i}: a query was sent");
        assert!(
            num(r, c, "response_bytes") > 0,
            "row {i}: a response arrived"
        );
        assert_eq!(
            num(r, c, "at_block") - num(r, c, "pinned_block"),
            num(r, c, "stale_blocks"),
            "row {i}: stale_blocks is the definition, not an estimate"
        );
        assert_eq!(
            field(r, c, "attempts"),
            "1",
            "row {i}: no re-bootstrap expected"
        );

        // (d) found/absent agree with which pass this row came from.
        let absent = field(r, c, "absent_probe");
        let found = field(r, c, "found");
        if i < 3 {
            assert_eq!((absent, found), ("0", "1"), "row {i}: seeded account");
        } else {
            assert_eq!((absent, found), ("1", "0"), "row {i}: random account");
        }

        // (e) The provider check was off, so both its columns are empty
        //     — never a defaulted 0, which would read as "mismatch".
        assert_eq!(field(r, c, "provider_match"), "");
        assert_eq!(field(r, c, "provider_error"), "");

        // (f) Nothing address- or balance-shaped anywhere in the row.
        let line = r.join(",");
        assert!(
            longest_hex_run(&line) < 40,
            "row {i} contains a 40-hex run: {line}"
        );
        for (addr, balance) in &handle.demo_accounts {
            let hex: String = addr.iter().map(|b| format!("{b:02x}")).collect();
            assert!(!line.contains(&hex), "row {i} leaked an address");
            // Only wei-scale balances are checked by substring. One demo
            // account holds 100 wei, and "100" is indistinguishable by
            // inspection from a microsecond count — which is an honest
            // limit of a substring tripwire, not a gap in the guarantee:
            // the balance never enters a row by construction (nothing
            // downstream of `Lookup` is written), and the unit-test
            // tripwire pins that against a deliberately distinctive
            // 21-digit balance where no coincidence is possible.
            if *balance >= 1_000_000 {
                assert!(
                    !line.contains(&balance.to_string()),
                    "row {i} leaked a balance"
                );
                assert!(
                    !line.contains(&format!("{balance:x}")),
                    "row {i} leaked a balance (hex)"
                );
            }
        }
    }

    // ── the blocks CSV ────────────────────────────────────────────────
    let (_, brows) = read_csv(&blocks_csv, BLOCK_COLUMNS);
    assert!(
        !brows.is_empty(),
        "a 2 s tail against a 15 ms mock chain must have ingested something"
    );
    for (i, r) in brows.iter().enumerate() {
        let c = BLOCK_COLUMNS;
        assert!(num(r, c, "block") > 0, "block row {i}");
        assert!(
            num(r, c, "blocks_in_fetch") >= 1,
            "block row {i}: a fetch covers at least one block"
        );
        assert!(
            num(r, c, "wire_bytes") > 0,
            "block row {i}: B9 must be a real byte count"
        );
        assert!(
            num(r, c, "delta_cells_total") >= num(r, c, "cells_in_block"),
            "block row {i}: |ΔD| after ingest is at least this fetch's own cells"
        );
        assert!(longest_hex_run(&r.join(",")) < 40);
    }
    assert_eq!(
        s2.block_rows, 0,
        "--follow-secs 0 means no follow, so pass 2 adds no block rows"
    );
    assert!(s1.block_rows > 0 && s1.blocks_ingested > 0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_refuses_to_start_without_a_pir_url() {
    let dir = scratch("cfg");
    let err = probe::run(ProbeConfig {
        queries_csv: dir.join("q.csv"),
        blocks_csv: dir.join("b.csv"),
        no_confirm: true,
        ..ProbeConfig::default()
    })
    .await
    .expect_err("an empty --pir-url must be refused, not silently defaulted");
    assert!(err.to_string().contains("--pir-url"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
