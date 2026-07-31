//! Integration tests for the periodic state autosave (ADR-0025).
//!
//! The load-bearing one is `concurrent_saves_reload_consistently`: it
//! hammers saves against a concurrently-applying writer and proves every
//! file that lands on disk is a clean single-height cut of the server —
//! `D` and `H` from the *same* block — which is the one property whose
//! violation would reload as a silently inconsistent server (the failure
//! class this repo's first rule forbids).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ikpir_common::backend::simple::SimpleParams;
use ikpir_common::pir_params::simple_max_plaintext_bits;
use ikpir_common::SimpleConfig;
use risepir_http::{wire, NodeState};
use risepir_proto::{keccak256, AddressHash, BlockUpdate, ValueCodec};
use risepir_rpc::autosave::{SaveOutcome, StateSaver};
use risepir_rpc::state::{self, LoadedState, Server};
use risepir_server::DeltaRing;
use segmented_cuckoo::Segmented2aryCuckooKVStore;

fn codec() -> ValueCodec {
    ValueCodec {
        key_tag_bits: 32,
        balance_bits: 96,
        checksum_bits: 16,
    }
}

/// The plaintext width `small_server`'s geometry uses — also what these
/// tests pass to `StateSaver::new` for journal encoding. None of these
/// pre-existing autosave tests inspect journal bytes (that is
/// `tests/journal.rs`'s job), so only *a* valid width matters here, not
/// this exact one — kept consistent with `small_server` regardless.
fn plaintext_bits() -> u32 {
    simple_max_plaintext_bits(
        2,
        64,
        4,
        32,
        codec().value_bits(),
        SimpleParams::DEFAULT_SIGMA,
    )
}

fn small_server() -> Server {
    let codec = codec();
    let num_buckets = 2 * 64;
    let pb = simple_max_plaintext_bits(
        2,
        num_buckets / 2,
        4,
        32,
        codec.value_bits(),
        SimpleParams::DEFAULT_SIGMA,
    );
    let store =
        Segmented2aryCuckooKVStore::new(num_buckets, 4, 32, codec.value_bits(), pb).unwrap();
    Server::new(store, SimpleConfig::with_lwe_dim(256), codec, 0)
}

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("risepir-autosave-{}-{name}", std::process::id()))
}

/// Deterministic per-block update over a 40-address universe: six
/// absolute changes plus one withdrawal credit, all nonzero, so the same
/// trace can be re-derived anywhere (the applier, the replay check, and
/// the plain-map simulation below).
fn update_for(b: u64) -> BlockUpdate {
    let mut changes = Vec::with_capacity(6);
    for i in 0..6u64 {
        let idx = ((b * 7 + i * 13) % 40) as u8;
        changes.push((
            keccak256(&[idx; 20]),
            1_000_000u128 + b as u128 * 1_000 + i as u128,
        ));
    }
    let credits = vec![(keccak256(&[(b % 40) as u8; 20]), 7u128)];
    BlockUpdate {
        block: b,
        changes,
        credits,
    }
}

/// The balances `update_for(1..=n)` must leave behind, computed with
/// plain arithmetic (no PIR, no store) — the independent oracle each
/// loaded file's verified reads are checked against.
fn simulate(n: u64) -> HashMap<AddressHash, u128> {
    let mut map: HashMap<AddressHash, u128> = HashMap::new();
    for b in 1..=n {
        let u = update_for(b);
        for (addr, v) in u.changes {
            map.insert(addr, v);
        }
        for (addr, amount) in u.credits {
            *map.entry(addr).or_insert(0) += amount;
        }
    }
    map
}

/// The design premise of ADR-0025, pinned as an executable fact: tokio's
/// `RwLock` is fair (write-preferring), so the moment a writer queues
/// behind a held read guard, *new* readers park behind the writer too.
/// This is exactly why a minutes-long save under the read lock must run
/// in the block-applier's own task — from any other task, the applier
/// waking mid-save would queue as a writer and turn the save into a
/// serving outage. If tokio ever changed this policy, this test failing
/// is the signal to revisit that reasoning (not a bug in tokio).
#[tokio::test]
async fn queued_writer_parks_new_readers() {
    let lock = Arc::new(tokio::sync::RwLock::new(0u32));

    // Sanity: with no writer queued, readers share freely.
    let held = lock.read().await;
    assert!(
        lock.try_read().is_ok(),
        "readers must share while no writer waits"
    );

    // Queue a writer behind the held read guard.
    let writer = tokio::spawn({
        let lock = Arc::clone(&lock);
        async move {
            *lock.write().await = 1;
        }
    });
    // Single-threaded runtime: yielding runs the spawned writer until it
    // parks on the lock — deterministic, no sleeps.
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }

    assert!(
        lock.try_read().is_err(),
        "a queued writer must park subsequent readers (fair RwLock) — \
         ADR-0025's placement argument rests on this"
    );

    drop(held);
    writer.await.unwrap();
    assert_eq!(*lock.read().await, 1);
}

/// State files written *while* blocks are being applied from another task
/// must each reload as a consistent single-height server. Two independent
/// checks per saved file:
///
/// 1. **`D` is exactly height `b`**: every verified read agrees with a
///    plain-arithmetic simulation of blocks `1..=b`.
/// 2. **`H` matches `D` at that same height**: replaying `b+1..=N` onto
///    the loaded server must land byte-identical (cells *and* encoded
///    setup, which covers the hints) to the live server at `N`. Hint
///    patches are additive, so a file whose hints were captured at any
///    height other than its cells' would end with wrong hints — the
///    credit in every block also makes any *partial-block* cell capture
///    non-idempotent, so a mid-block tear cannot cancel out.
///
/// In production the saver runs in the applier's own task (liveness —
/// see ADR-0025); this test deliberately runs it from a *different* task
/// because consistency must hold even in that adversarial arrangement —
/// it is carried by the read guard alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_saves_reload_consistently() {
    const LAST_BLOCK: u64 = 40;
    let path = tmp("concurrent.bin");
    let node = Arc::new(NodeState::new(small_server(), DeltaRing::new(64), true));
    let saver = Arc::new(StateSaver::new(
        path.clone(),
        codec(),
        true,
        Duration::from_millis(1),
        None,
        plaintext_bits(),
        None,
    ));

    let done = Arc::new(AtomicBool::new(false));

    let applier = tokio::spawn({
        let node = Arc::clone(&node);
        let done = Arc::clone(&done);
        async move {
            for b in 1..=LAST_BLOCK {
                node.apply_block(&update_for(b)).await.unwrap();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            done.store(true, Ordering::SeqCst);
        }
    });

    // Save as aggressively as the 1 ms interval allows, copying each
    // completed file aside so every intermediate cut gets validated (the
    // next save overwrites `path`; only this task writes the copies).
    let saver_task = tokio::spawn({
        let node = Arc::clone(&node);
        let saver = Arc::clone(&saver);
        let done = Arc::clone(&done);
        let path = path.clone();
        async move {
            let mut copies: Vec<(u64, PathBuf)> = Vec::new();
            loop {
                let finished = done.load(Ordering::SeqCst);
                let outcome = if finished {
                    // One forced save after the applier stops, so the
                    // final height is always among the validated files.
                    saver.save_now(&node, "test-final").await.unwrap()
                } else {
                    saver.maybe_save(&node).await.unwrap()
                };
                if let SaveOutcome::Saved { block, .. } = outcome {
                    if copies.last().map(|(b, _)| *b) != Some(block) {
                        let copy = path.with_extension(format!("at{block}"));
                        std::fs::copy(&path, &copy).unwrap();
                        copies.push((block, copy));
                    }
                }
                if finished {
                    return copies;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    });

    applier.await.unwrap();
    let copies = saver_task.await.unwrap();

    // The live server at LAST_BLOCK is the replay target for check 2.
    let (final_cells, final_setup) = node
        .with_server(|s| (s.cells().to_vec(), wire::encode_setup(&s.setup())))
        .await;

    assert!(
        copies.len() >= 3,
        "expected several interleaved saves to validate, got {} — the test lost its subject",
        copies.len()
    );
    assert!(
        copies.iter().any(|(b, _)| *b < LAST_BLOCK),
        "every save landed at the final height; nothing was captured mid-run"
    );
    assert_eq!(
        copies.last().unwrap().0,
        LAST_BLOCK,
        "the forced final save must capture the last block"
    );
    assert!(
        copies.windows(2).all(|w| w[0].0 < w[1].0),
        "saved heights must be strictly increasing"
    );

    for (saved_block, copy) in &copies {
        let LoadedState {
            server: mut loaded,
            complete,
            ..
        } = state::load(copy, SimpleConfig::with_lwe_dim(256), &codec()).unwrap();
        assert!(complete);
        assert_eq!(
            loaded.block(),
            *saved_block,
            "file must carry the height it was captured at"
        );

        // Check 1: D is exactly the simulated state at `saved_block`.
        let expected = simulate(*saved_block);
        for idx in 0..40u8 {
            let addr = keccak256(&[idx; 20]);
            assert_eq!(
                loaded.balance_of(&addr).unwrap(),
                expected.get(&addr).copied(),
                "balance mismatch at height {saved_block} for address index {idx}"
            );
        }

        // Check 2: replaying the remaining blocks lands byte-identical to
        // the live server — cells and hints both.
        for b in (*saved_block + 1)..=LAST_BLOCK {
            loaded.apply_block(&update_for(b)).unwrap();
        }
        assert_eq!(
            loaded.cells(),
            &final_cells[..],
            "cells diverge after replay from height {saved_block}"
        );
        assert_eq!(
            wire::encode_setup(&loaded.setup()),
            final_setup,
            "hints/params diverge after replay from height {saved_block} — \
             the file's D and H were not captured at one height"
        );

        std::fs::remove_file(copy).unwrap();
    }
    eprintln!(
        "validated {} interleaved saves at heights {:?}",
        copies.len(),
        copies.iter().map(|(b, _)| *b).collect::<Vec<_>>()
    );
    std::fs::remove_file(&path).unwrap();
}

/// The periodic trigger's economics: it must skip the (at scale, tens of
/// GB) rewrite when nothing changed, fire again once something did, obey
/// the interval, and stay inert when disabled — while `save_now` (the
/// shutdown path) always writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn autosave_skips_unchanged_obeys_interval_and_disable() {
    let path = tmp("skip.bin");
    let node = NodeState::new(small_server(), DeltaRing::new(16), true);
    // Wide enough that one save — including its post-rename parent-dir
    // fsync, which on macOS alone can cost tens of ms — always finishes
    // well inside it; the NotDue assertion below is about the *clock*,
    // and a save that outlives the interval would make it racy.
    let interval = Duration::from_millis(250);
    let saver = StateSaver::new(
        path.clone(),
        codec(),
        true,
        interval,
        None,
        plaintext_bits(),
        None,
    );

    node.apply_block(&update_for(1)).await.unwrap();
    tokio::time::sleep(interval * 2).await;
    assert!(matches!(
        saver.maybe_save(&node).await.unwrap(),
        SaveOutcome::Saved { block: 1, .. }
    ));

    // Immediately after a save the interval has not elapsed.
    assert_eq!(saver.maybe_save(&node).await.unwrap(), SaveOutcome::NotDue);

    // Interval elapsed but nothing applied: no rewrite.
    tokio::time::sleep(interval * 2).await;
    assert_eq!(
        saver.maybe_save(&node).await.unwrap(),
        SaveOutcome::Unchanged { block: 1 }
    );

    // A new block makes the next due save fire again.
    node.apply_block(&update_for(2)).await.unwrap();
    tokio::time::sleep(interval * 2).await;
    assert!(matches!(
        saver.maybe_save(&node).await.unwrap(),
        SaveOutcome::Saved { block: 2, .. }
    ));

    // save_now writes even with an unchanged height (the operator asked).
    assert!(matches!(
        saver.save_now(&node, "test-shutdown").await.unwrap(),
        SaveOutcome::Saved { block: 2, .. }
    ));

    // Disabled saver: never due, no matter what.
    let disabled = StateSaver::new(
        tmp("disabled.bin"),
        codec(),
        true,
        Duration::ZERO,
        None,
        plaintext_bits(),
        None,
    );
    tokio::time::sleep(interval).await;
    assert_eq!(
        disabled.maybe_save(&node).await.unwrap(),
        SaveOutcome::NotDue
    );
    assert!(!tmp("disabled.bin").exists());

    let loaded = state::load(&path, SimpleConfig::with_lwe_dim(256), &codec()).unwrap();
    assert_eq!(loaded.server.block(), 2);
    std::fs::remove_file(&path).unwrap();
}

/// Concurrent `save_now` calls (the shutdown-vs-autosave race) serialize
/// on the saver's mutex: every write completes, and the file that ends up
/// at the path passes the whole-file checksum — i.e. no two writers ever
/// interleaved on `<path>.tmp`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_save_now_calls_serialize() {
    let path = tmp("serialize.bin");
    let node = Arc::new(NodeState::new(small_server(), DeltaRing::new(16), true));
    let saver = Arc::new(StateSaver::new(
        path.clone(),
        codec(),
        true,
        Duration::ZERO,
        None,
        plaintext_bits(),
        None,
    ));
    node.apply_block(&update_for(1)).await.unwrap();

    let tasks: Vec<_> = (0..2)
        .map(|_| {
            let node = Arc::clone(&node);
            let saver = Arc::clone(&saver);
            tokio::spawn(async move {
                for _ in 0..5 {
                    assert!(matches!(
                        saver.save_now(&node, "race").await.unwrap(),
                        SaveOutcome::Saved { block: 1, .. }
                    ));
                }
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }

    // load() re-verifies the whole-file xxh3 — interleaved writes would
    // fail it here.
    let loaded = state::load(&path, SimpleConfig::with_lwe_dim(256), &codec()).unwrap();
    assert_eq!(loaded.server.block(), 1);
    std::fs::remove_file(&path).unwrap();
}

/// The shutdown/append race artifact (review follow-up to ADR-0025/0026):
/// the follow loop commits block N in memory and is on its way to journal
/// it when a SIGINT-triggered `save_now` wins the saver mutex — the save
/// captures height N (the in-memory state includes it) and rotates the
/// journal to a fresh one based at N, so the parked append's delta is
/// already inside the new journal's base. That append must be a silent
/// no-op — journaling stays enabled, no "disabling journaling" WARNING
/// during a perfectly healthy shutdown — while a *forward* gap (a block
/// genuinely skipped) must still disable it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_backward_gap_append_is_skipped_not_a_journal_failure() {
    use risepir_proto::BlockDelta;
    use risepir_rpc::journal::{journal_path_for, JournalReader};
    use std::io::BufReader;

    let path = tmp("benign-gap.bin");
    let node = NodeState::new(small_server(), DeltaRing::new(16), true);
    let saver = StateSaver::new(
        path.clone(),
        codec(),
        true,
        Duration::from_secs(3600),
        None,
        plaintext_bits(),
        None,
    );

    node.apply_block(&update_for(1)).await.unwrap();
    node.apply_block(&update_for(2)).await.unwrap();
    // The "shutdown" save at height 2: rotates in a fresh journal based at 2.
    assert!(matches!(
        saver.save_now(&node, "test-shutdown-race").await.unwrap(),
        SaveOutcome::Saved { block: 2, .. }
    ));

    // The parked append that lost the race: block 2 is already inside the
    // base the fresh journal hangs off.
    let raced = BlockDelta {
        block: 2,
        per_segment: vec![vec![(0, vec![(0, 1)])], vec![]],
    };
    saver.append_delta(&raced, 2).await;

    // Journaling must still be alive: the next block appends normally.
    let next = BlockDelta {
        block: 3,
        per_segment: vec![vec![(0, vec![(0, 1)])], vec![]],
    };
    saver.append_delta(&next, 3).await;

    let journal_path = journal_path_for(&path);
    let file = std::fs::File::open(&journal_path).unwrap();
    let len = file.metadata().unwrap().len();
    // 2 = small_server()'s real arity — a mismatch here would make every
    // record's payload fail to decode (ArityMismatch), not just look wrong.
    let (header, reader) =
        JournalReader::open(BufReader::new(file), len, plaintext_bits(), 2).unwrap();
    assert_eq!(
        header.base_block, 2,
        "the rotation moved the base to the save height"
    );
    let blocks: Vec<u64> = reader.map(|rec| rec.delta.block).collect();
    assert_eq!(
        blocks,
        vec![3],
        "the raced block-2 append must be absent (subsumed by the base), block 3 present (journaling alive)"
    );

    // A *forward* gap is still a real failure: block 5 after 3 leaves 4
    // missing, and journaling shuts down rather than recording a hole.
    let hole = BlockDelta {
        block: 5,
        per_segment: vec![vec![(0, vec![(0, 1)])], vec![]],
    };
    saver.append_delta(&hole, 5).await;
    let after = BlockDelta {
        block: 6,
        per_segment: vec![vec![(0, vec![(0, 1)])], vec![]],
    };
    saver.append_delta(&after, 6).await;
    let file = std::fs::File::open(&journal_path).unwrap();
    let len = file.metadata().unwrap().len();
    let (_, reader) = JournalReader::open(BufReader::new(file), len, plaintext_bits(), 2).unwrap();
    let blocks: Vec<u64> = reader.map(|rec| rec.delta.block).collect();
    assert_eq!(
        blocks,
        vec![3],
        "after a forward gap nothing further may be recorded"
    );

    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(&journal_path).unwrap();
}
