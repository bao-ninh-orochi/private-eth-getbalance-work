//! Integration tests for the sidecar delta journal (ADR-0026). The
//! load-bearing one is `journal_replay_matches_live_apply`: replay through
//! the real restore code path (`risepir_rpc::state::load_with_journal_restore`)
//! must reproduce live `apply_block` bit-exactly — cells, encoded setup
//! (hints + params + block), item count, and individual balances,
//! including a deleted account and a credited one. The rest pin the two
//! failure classes ADR-0026 requires: pre-apply validation failures
//! (torn tail, mid-file corruption, base mismatch, height gap, oversized
//! length) must all *stop and use the good prefix*, never error the whole
//! load or skip past the bad point.

use std::path::PathBuf;

use ikpir_common::backend::simple::SimpleParams;
use ikpir_common::pir_params::simple_max_plaintext_bits;
use ikpir_common::SimpleConfig;
use risepir_http::{wire, NodeState};
use risepir_proto::{keccak256, BlockDelta, BlockUpdate, ValueCodec};
use risepir_rpc::journal::{journal_path_for, JournalError, JournalWriter, ScanStop};
use risepir_rpc::state::{self, LoadedState, RestoreError, Server};
use risepir_server::DeltaRing;
use segmented_cuckoo::Segmented3aryCuckooKVStore;

fn codec() -> ValueCodec {
    ValueCodec {
        key_tag_bits: 32,
        balance_bits: 96,
        checksum_bits: 16,
    }
}

fn small_server() -> Server {
    let codec = codec();
    let num_buckets = 3 * 64;
    let pb = simple_max_plaintext_bits(num_buckets / 3, 4, 32, codec.value_bits(), SimpleParams::DEFAULT_SIGMA);
    let store = Segmented3aryCuckooKVStore::new(num_buckets, 4, 32, codec.value_bits(), pb).unwrap();
    Server::new(store, SimpleConfig::with_lwe_dim(256), codec, 0)
}

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("risepir-journal-test-{}-{name}", std::process::id()))
}

/// A distinct, out-of-band address the regular per-block changes/credits
/// below can never touch (they only ever address `[idx; 20]` for
/// `idx < 40`; `0xEE` = 238 is well outside that range), so the
/// created-then-deleted test below never depends on modular-arithmetic
/// luck to avoid being accidentally re-created by later blocks.
const DELETE_TARGET: [u8; 20] = [0xEE; 20];

/// Deterministic per-block update over a 40-address universe (mirrors
/// `tests/autosave.rs`'s `update_for`): six absolute changes plus one
/// withdrawal credit, all nonzero. Block 1 additionally creates
/// [`DELETE_TARGET`]; block 7 deletes it — a real `(addr, 0)` delete, so
/// `num_items` moves both directions, not just up.
fn update_for(b: u64) -> BlockUpdate {
    let mut changes = Vec::with_capacity(7);
    for i in 0..6u64 {
        let idx = ((b * 7 + i * 13) % 40) as u8;
        changes.push((keccak256(&[idx; 20]), 1_000_000u128 + b as u128 * 1_000 + i as u128));
    }
    if b == 1 {
        changes.push((keccak256(&DELETE_TARGET), 42_000_000_000u128));
    } else if b == 7 {
        changes.push((keccak256(&DELETE_TARGET), 0u128));
    }
    let credits = vec![(keccak256(&[(b % 40) as u8; 20]), 7u128)];
    BlockUpdate { block: b, changes, credits }
}

/// **THE test.** Build a base server, save it (capturing the digest), and
/// run two independent evolutions from that same base:
///
/// - **L** (live): `NodeState::apply_block` for blocks `1..=N`, appending
///   each returned delta + the post-block item count through a real
///   [`JournalWriter`].
/// - **R** (restored): `state::load_with_journal_restore` — the real
///   restore code path — replaying that same journal onto the same base.
///
/// R must match L byte-exact: cells, encoded setup (hints + params +
/// block), `num_items()`, and individual balances — including the
/// deleted [`DELETE_TARGET`] (must read back `None`) and the last
/// credited address.
#[tokio::test]
async fn journal_replay_matches_live_apply() {
    const N: u64 = 30;
    let base_path = tmp("replay-base.bin");
    let server = small_server();
    let report = state::save(&server, &codec(), true, &base_path).unwrap();

    let node = NodeState::new(server, DeltaRing::new(64), true);
    let plaintext_bits = node.with_server(|s| s.params().plaintext_bits).await;
    let journal_path = journal_path_for(&base_path);
    let mut writer = JournalWriter::create(&journal_path, report.digest, 0, plaintext_bits).unwrap();
    for b in 1..=N {
        let (delta, _) = node.apply_block(&update_for(b)).await.unwrap();
        let n_items = node.with_server(|s| s.num_items()).await;
        writer.append(&delta, n_items).unwrap();
    }
    drop(writer);

    let (live_cells, live_setup, live_block, live_num_items) = node
        .with_server(|s| (s.cells().to_vec(), wire::encode_setup(&s.setup()), s.block(), s.num_items()))
        .await;

    let restored = state::load_with_journal_restore(&base_path, SimpleConfig::with_lwe_dim(256), &codec(), 64).unwrap();
    assert_eq!(restored.replayed, N);
    assert_eq!(restored.base_block, 0);
    assert_eq!(restored.tail_deltas.len(), N as usize, "ring capacity 64 >= N, so every delta survives as tail");
    assert!(matches!(restored.scan_stop, Some(ScanStop::Eof)));

    let LoadedState { server: restored_server, complete, .. } = restored.loaded;
    assert!(complete);
    assert_eq!(restored_server.block(), live_block);
    assert_eq!(restored_server.block(), N);
    assert_eq!(restored_server.num_items(), live_num_items);
    assert_eq!(restored_server.cells(), &live_cells[..], "cells must be byte-exact");
    assert_eq!(wire::encode_setup(&restored_server.setup()), live_setup, "hints/params/block must be byte-exact");

    let deleted_addr = keccak256(&DELETE_TARGET);
    assert_eq!(restored_server.balance_of(&deleted_addr).unwrap(), None, "deleted account must read back None");
    assert_eq!(node.balance_of(&deleted_addr).await.unwrap(), None, "sanity: L must also show it deleted");

    let credited_addr = keccak256(&[(N % 40) as u8; 20]);
    let expected = node.balance_of(&credited_addr).await.unwrap();
    assert!(expected.is_some(), "sanity: the credited address must actually be tracked");
    assert_eq!(restored_server.balance_of(&credited_addr).unwrap(), expected, "credited account must agree");

    std::fs::remove_file(&base_path).unwrap();
    std::fs::remove_file(&journal_path).unwrap();
}

/// A torn tail (file cut mid-record) must restore to the last good
/// height, not error the whole load or silently skip past the tear; then
/// `adopt` + further appends must produce a journal that restores
/// cleanly again from that point.
#[tokio::test]
async fn torn_tail_restores_to_last_good_height_then_recovers() {
    let base_path = tmp("torn-base.bin");
    let server = small_server();
    let report = state::save(&server, &codec(), true, &base_path).unwrap();
    let node = NodeState::new(server, DeltaRing::new(64), true);
    let plaintext_bits = node.with_server(|s| s.params().plaintext_bits).await;
    let journal_path = journal_path_for(&base_path);

    let mut writer = JournalWriter::create(&journal_path, report.digest, 0, plaintext_bits).unwrap();
    for b in 1..=5u64 {
        let (delta, _) = node.apply_block(&update_for(b)).await.unwrap();
        let n_items = node.with_server(|s| s.num_items()).await;
        writer.append(&delta, n_items).unwrap();
    }
    drop(writer);

    let mut bytes = std::fs::read(&journal_path).unwrap();
    bytes.truncate(bytes.len() - 4); // cut into the last record
    std::fs::write(&journal_path, &bytes).unwrap();

    let restored = state::load_with_journal_restore(&base_path, SimpleConfig::with_lwe_dim(256), &codec(), 64).unwrap();
    assert_eq!(restored.replayed, 4, "the torn 5th record must not be used");
    assert_eq!(restored.loaded.server.block(), 4);
    assert!(matches!(restored.scan_stop, Some(ScanStop::Invalid { .. })));
    let (end_offset, end_height) = restored.adopt_at.expect("a torn tail must still be adopt-eligible up to its good prefix");
    assert_eq!(end_height, 4);

    let mut writer2 = JournalWriter::adopt(&journal_path, plaintext_bits, end_offset, end_height).unwrap();
    let mut resumed_server = restored.loaded.server;
    for b in 5..=8u64 {
        let delta = resumed_server.apply_block(&update_for(b)).unwrap();
        writer2.append(&delta, resumed_server.num_items()).unwrap();
    }
    drop(writer2);

    let restored2 = state::load_with_journal_restore(&base_path, SimpleConfig::with_lwe_dim(256), &codec(), 64).unwrap();
    assert_eq!(restored2.replayed, 8, "adopt + further appends must restore cleanly again");
    assert_eq!(restored2.loaded.server.block(), 8);
    assert!(matches!(restored2.scan_stop, Some(ScanStop::Eof)));

    std::fs::remove_file(&base_path).unwrap();
    std::fs::remove_file(&journal_path).unwrap();
}

/// A bit flip inside a later record must stop replay exactly there:
/// heights after the flip are absent from the restored server, never
/// skipped-and-continued.
#[tokio::test]
async fn mid_file_corruption_stops_replay_there() {
    let base_path = tmp("corrupt-base.bin");
    let server = small_server();
    let report = state::save(&server, &codec(), true, &base_path).unwrap();
    let node = NodeState::new(server, DeltaRing::new(64), true);
    let plaintext_bits = node.with_server(|s| s.params().plaintext_bits).await;
    let journal_path = journal_path_for(&base_path);

    let mut writer = JournalWriter::create(&journal_path, report.digest, 0, plaintext_bits).unwrap();
    for b in 1..=6u64 {
        let (delta, _) = node.apply_block(&update_for(b)).await.unwrap();
        let n_items = node.with_server(|s| s.num_items()).await;
        writer.append(&delta, n_items).unwrap();
    }
    drop(writer);

    let mut bytes = std::fs::read(&journal_path).unwrap();
    let flip_at = bytes.len() - 15; // inside one of the later records
    bytes[flip_at] ^= 0x01;
    std::fs::write(&journal_path, &bytes).unwrap();

    let restored = state::load_with_journal_restore(&base_path, SimpleConfig::with_lwe_dim(256), &codec(), 64).unwrap();
    assert!(restored.replayed < 6, "the corruption must have stopped replay before all 6 records");
    assert!(restored.replayed >= 1, "records before the flip must still have been used");
    assert_eq!(restored.loaded.server.block(), restored.replayed, "base_block is 0, so block == records replayed");
    assert!(matches!(restored.scan_stop, Some(ScanStop::Invalid { .. })));

    std::fs::remove_file(&base_path).unwrap();
    std::fs::remove_file(&journal_path).unwrap();
}

/// A journal whose header names a *different* base must be refused
/// entirely: replay falls back to exactly the plain base load (zero
/// records), never partially trusting bytes that belong to someone
/// else's history.
#[tokio::test]
async fn base_mismatch_falls_back_to_plain_load() {
    let base_path = tmp("mismatch-base.bin");
    let server = small_server();
    let plaintext_bits = server.params().plaintext_bits;
    let _report = state::save(&server, &codec(), true, &base_path).unwrap();
    let journal_path = journal_path_for(&base_path);

    // A journal bound to a wholly different digest/height.
    JournalWriter::create(&journal_path, 0xDEAD_BEEF_0000_0001, 999, plaintext_bits).unwrap();

    let restored = state::load_with_journal_restore(&base_path, SimpleConfig::with_lwe_dim(256), &codec(), 64).unwrap();
    assert_eq!(restored.replayed, 0);
    assert_eq!(restored.tail_deltas.len(), 0);
    assert!(restored.adopt_at.is_none());
    assert!(restored.scan_stop.is_none(), "a mismatched journal must never even be consulted");
    assert_eq!(restored.loaded.server.block(), 0);

    std::fs::remove_file(&base_path).unwrap();
    std::fs::remove_file(&journal_path).unwrap();
}

/// A height gap must never be written in the first place — continuity is
/// enforced at `append` time, and nothing after the gap is ever
/// persisted, so a restore from that journal only ever sees the
/// contiguous prefix.
#[tokio::test]
async fn gap_is_refused_at_append_time_and_replay_stops_at_the_prefix() {
    let base_path = tmp("gap-base.bin");
    let server = small_server();
    let report = state::save(&server, &codec(), true, &base_path).unwrap();
    let node = NodeState::new(server, DeltaRing::new(64), true);
    let plaintext_bits = node.with_server(|s| s.params().plaintext_bits).await;
    let journal_path = journal_path_for(&base_path);

    let mut writer = JournalWriter::create(&journal_path, report.digest, 0, plaintext_bits).unwrap();
    let (d1, _) = node.apply_block(&update_for(1)).await.unwrap();
    let n1 = node.with_server(|s| s.num_items()).await;
    writer.append(&d1, n1).unwrap();

    // Skip block 2 entirely; attempt to jump straight to block 3's delta.
    let (_d2, _) = node.apply_block(&update_for(2)).await.unwrap();
    let (d3, _) = node.apply_block(&update_for(3)).await.unwrap();
    let n3 = node.with_server(|s| s.num_items()).await;
    match writer.append(&d3, n3) {
        Err(JournalError::Gap { expected: 2, found: 3 }) => {}
        other => panic!("expected append to refuse the gap, got {other:?}"),
    }
    drop(writer);

    let restored = state::load_with_journal_restore(&base_path, SimpleConfig::with_lwe_dim(256), &codec(), 64).unwrap();
    assert_eq!(restored.replayed, 1, "only block 1 was ever actually written to disk");
    assert_eq!(restored.loaded.server.block(), 1);
    assert!(matches!(restored.scan_stop, Some(ScanStop::Eof)), "a short-but-clean file, not a torn one");

    std::fs::remove_file(&base_path).unwrap();
    std::fs::remove_file(&journal_path).unwrap();
}

/// The mainnet-side plumbing for seeding the ring after a restore: a
/// small `ring_capacity` caps `tail_deltas` to the most recent entries,
/// oldest first — the shape `NodeState::seed_history` expects (that
/// method's own HTTP-level coverage lives in risepir-http's test suite;
/// this confirms the tail this crate hands it is correctly bounded and
/// ordered).
#[tokio::test]
async fn restore_caps_and_orders_the_tail_deltas() {
    let base_path = tmp("tail-base.bin");
    let server = small_server();
    let report = state::save(&server, &codec(), true, &base_path).unwrap();
    let node = NodeState::new(server, DeltaRing::new(64), true);
    let plaintext_bits = node.with_server(|s| s.params().plaintext_bits).await;
    let journal_path = journal_path_for(&base_path);

    let mut writer = JournalWriter::create(&journal_path, report.digest, 0, plaintext_bits).unwrap();
    for b in 1..=10u64 {
        let (delta, _) = node.apply_block(&update_for(b)).await.unwrap();
        let n_items = node.with_server(|s| s.num_items()).await;
        writer.append(&delta, n_items).unwrap();
    }
    drop(writer);

    let ring_capacity = 4;
    let restored = state::load_with_journal_restore(&base_path, SimpleConfig::with_lwe_dim(256), &codec(), ring_capacity).unwrap();
    assert_eq!(restored.replayed, 10);
    assert_eq!(restored.tail_deltas.len(), ring_capacity);
    let heights: Vec<u64> = restored.tail_deltas.iter().map(|d| d.block).collect();
    assert_eq!(heights, vec![7, 8, 9, 10], "tail must be the most recent ring_capacity blocks, oldest first");

    std::fs::remove_file(&base_path).unwrap();
    std::fs::remove_file(&journal_path).unwrap();
}

/// The apply-time failure class, end to end (ADR-0026 rule 3): a record
/// that passes **every** pre-apply check — real writer, valid framing and
/// checksum, contiguous height, decodable payload with `|Δ| < p` — but
/// whose delta drives a cell outside `[0, 2^plaintext_bits)` when applied
/// to *this* base must surface as [`RestoreError::ApplyFailure`], never
/// as a stop-and-use-the-prefix fallback: by the time the violation is
/// detectable the in-memory cells are torn mid-block, and the caller
/// (`mainnet.rs`) refuses to serve at all (`die()`). Distinct from every
/// pre-apply test above, which all *recover*; this one must *refuse*.
#[tokio::test]
async fn semantically_wrong_record_is_an_apply_failure_not_a_fallback() {
    let base_path = tmp("applyfail-base.bin");
    let server = small_server();
    let plaintext_bits = server.params().plaintext_bits;
    assert_eq!(server.cells()[0], 0, "precondition: a fresh store's first cell is empty (zero)");
    let report = state::save(&server, &codec(), true, &base_path).unwrap();
    let journal_path = journal_path_for(&base_path);

    // Largest negative delta the wire codec itself accepts (|Δ| < p):
    // applied to a zero cell it lands at -(p-1) < 0 — decode-clean,
    // apply-impossible against this base.
    let poison = -((1i64 << plaintext_bits) - 1);
    let mut writer = JournalWriter::create(&journal_path, report.digest, 0, plaintext_bits).unwrap();
    writer
        .append(
            &BlockDelta {
                block: 1,
                per_segment: vec![vec![(0, vec![(0, poison)])], vec![], vec![]],
            },
            1,
        )
        .unwrap();
    drop(writer);

    match state::load_with_journal_restore(&base_path, SimpleConfig::with_lwe_dim(256), &codec(), 64) {
        Err(RestoreError::ApplyFailure(msg)) => {
            assert!(msg.contains("violates"), "the message must name the bound violation, got: {msg}");
        }
        Err(other) => panic!("expected ApplyFailure, got a different error: {other}"),
        Ok(restored) => panic!(
            "a semantically wrong record must never load; got a server at block {}",
            restored.loaded.server.block()
        ),
    }

    std::fs::remove_file(&base_path).unwrap();
    std::fs::remove_file(&journal_path).unwrap();
}

/// A record declaring `len = u32::MAX` must be a clean stop through the
/// full restore path, never an OOM or panic (the fuzz target exercises
/// this at the reader level directly across arbitrary bytes; this
/// confirms the integration path degrades the same way).
#[tokio::test]
async fn oversized_record_length_is_a_clean_stop_not_an_oom() {
    let base_path = tmp("oversize-base.bin");
    let server = small_server();
    let plaintext_bits = server.params().plaintext_bits;
    let report = state::save(&server, &codec(), true, &base_path).unwrap();
    let journal_path = journal_path_for(&base_path);

    JournalWriter::create(&journal_path, report.digest, 0, plaintext_bits).unwrap();
    let mut bytes = std::fs::read(&journal_path).unwrap();
    bytes.extend_from_slice(&u32::MAX.to_le_bytes());
    std::fs::write(&journal_path, &bytes).unwrap();

    let restored = state::load_with_journal_restore(&base_path, SimpleConfig::with_lwe_dim(256), &codec(), 64).unwrap();
    assert_eq!(restored.replayed, 0);
    assert!(matches!(restored.scan_stop, Some(ScanStop::Invalid { .. })));
    assert_eq!(restored.loaded.server.block(), 0);

    std::fs::remove_file(&base_path).unwrap();
    std::fs::remove_file(&journal_path).unwrap();
}
