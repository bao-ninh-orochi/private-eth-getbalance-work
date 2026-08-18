//! Stage 0.5 — the conformance harness, "the real gate" (`docs/plan.md`
//! §6). Proves the private client's answers are byte-identical to
//! in-process ground truth across a large sample of addresses and many
//! consecutive blocks, diffing **continuously** — at many checkpoints
//! across the run, not once at the end (`docs/plan.md` §8's
//! never-return-a-wrong-answer checklist: "conformance includes
//! nonexistent addrs" / "conformance includes created-during-run
//! accounts").
//!
//! # Why in-process
//!
//! [`run`] drives [`RisePirClient`] directly against a [`RisePirServer`]
//! in the same process — no HTTP, no threads, no `Mutex`/`RwLock`
//! (`docs/plan.md` ADR-0010's concurrency story is a separate, already
//! independently tested concern). Single-threaded means the server's head
//! cannot advance between an `apply_block` and a query answered against
//! it, so every query's `at_block` is exactly the client's pending delta
//! head, which is exactly the server's head at that instant: the
//! `ResponseBlockMismatch` / malformed-query cases `RisePirClient::finish`
//! and `RisePirServer::answer` guard against cannot arise here by
//! construction. That is why `diff_sample` `.expect`s those `Result`s
//! rather than folding an `Err` into [`ConformanceReport::mismatches`]: an
//! `Err` here would mean the harness itself is wired wrong, not that the
//! private client gave a wrong answer.
//!
//! # Account categories
//!
//! Five categories ([`CATEGORIES`]), every one required to be non-empty in
//! the final sample:
//!
//! - **never-existed** — addresses in an id range strictly beyond
//!   anything this run's `MockFeed` can ever mint, so disjoint from every
//!   real account *by construction*, not merely with high probability.
//! - **contract** — a nominal label over a fixed subset of genesis keys.
//!   `MockFeed` has no EOA/contract distinction; there is nothing
//!   contract-*specific* about these accounts other than the label. Kept
//!   as its own always-present category because the brief requires
//!   contract-account coverage even though the mock cannot honestly
//!   simulate contract-specific behaviour.
//! - **high-activity** — genesis keys (excluding the ones labelled
//!   `contract`) ranked by how many times the change stream has touched
//!   them so far, restricted to keys still live at sampling time.
//! - **created-during-run** — keys whose first appearance in the change
//!   stream is after genesis, still live at sampling time. Exercises the
//!   step-4/5 ordering trap (`docs/plan.md` §3.3): the account did not
//!   exist when the client's hint was pinned at genesis.
//! - **zero-balance** — keys the change stream has deleted (balance went
//!   to `0`). `MockFeed` never reuses an address (an insert always mints a
//!   fresh id), so once deleted a key's ground truth is `0` forever;
//!   every observed delete is a permanently valid member of this
//!   category.
//!
//! The five are kept mutually exclusive by construction (contract ids are
//! excluded from the high-activity and zero-balance pools; created-during-
//! run keys are by definition disjoint from genesis) so
//! [`ConformanceReport::category_counts`] has no double-counting.
//!
//! # The diff rule
//!
//! One rule, applied uniformly to every category and every checkpoint —
//! see `diff_sample`: `Lookup::Found(b)` must equal `balance_of(key)`
//! and that must be nonzero; `Lookup::NotFound` requires
//! `balance_of(key) == 0`; `Lookup::DecodeFailed` is always a mismatch —
//! no in-process LWE corruption can occur, so a checksum failure here can
//! only mean a real bug.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::{self, Write as _};

use ikpir_common::{SimpleConfig, SimplePirBackend};
use risepir_client::{Lookup, RisePirClient};
use risepir_feed::{Feed, MockConfig, MockFeed};
use risepir_proto::{AddressHash, Backend, Balance, Geometry, ValueCodec};
use risepir_server::{DeltaRing, RisePirServer};
use segmented_cuckoo::{Segmented2aryCuckooKVStore, Segmented2aryScheme};

// ─── Fixed deployment geometry knobs ─────────────────────────────────────
//
// Mirrors risepir-feed's own integration test
// (crates/risepir-feed/tests/pipeline.rs) and risepir-server/
// risepir-client's shared test geometry: arity-2 SCF, the ADR-0009 value
// layout (32-bit key_tag / 96-bit balance / 16-bit checksum), RisePIR-S.
const ARITY: u32 = 2;
const BUCKET_SIZE: u32 = 4;
const FINGERPRINT_BITS: u32 = 32;
const KEY_TAG_BITS: u32 = 32;
const BALANCE_BITS: u32 = 96;
const CHECKSUM_BITS: u32 = 16;

fn value_codec() -> ValueCodec {
    ValueCodec {
        key_tag_bits: KEY_TAG_BITS,
        balance_bits: BALANCE_BITS,
        checksum_bits: CHECKSUM_BITS,
    }
}

// ─── Category labels ──────────────────────────────────────────────────
//
// Named constants (rather than repeating string literals at every call
// site) so a typo in one place is a compile error, not a silent mismatch
// between what `build_sample` labels a sample entry and what `tally` /
// the final coverage check look for.
const CAT_NEVER_EXISTED: &str = "never-existed";
const CAT_CONTRACT: &str = "contract";
const CAT_HIGH_ACTIVITY: &str = "high-activity";
const CAT_CREATED: &str = "created-during-run";
const CAT_ZERO_BALANCE: &str = "zero-balance";

/// The five required account categories (see the module docs), in report
/// order. Every one must be non-empty in the final sample for
/// [`ConformanceReport::passed`] to be `true`.
pub const CATEGORIES: [&str; 5] = [
    CAT_NEVER_EXISTED,
    CAT_CONTRACT,
    CAT_HIGH_ACTIVITY,
    CAT_CREATED,
    CAT_ZERO_BALANCE,
];

/// Configuration for one conformance run.
///
/// [`Default`] is sized for the real gate (`docs/plan.md` §6 Stage 0.5): at
/// least 1000 addresses spanning every category, across at least 100
/// consecutive blocks, diffed at many checkpoints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConformanceConfig {
    /// Seed for the deterministic mock chain
    /// (`risepir_feed::MockConfig::seed`) — fixed, so every run is
    /// reproducible.
    pub seed: u64,
    /// Genesis account population.
    pub num_genesis_keys: u64,
    /// Number of consecutive blocks to run (must be >= 1).
    pub blocks: u64,
    /// Minimum total sampled addresses the final diff pass must cover.
    pub min_addresses: usize,
    /// Number of roughly-evenly-spaced in-run checkpoints, plus one
    /// dedicated final full-sample pass after the run (see [`run`]).
    pub checkpoints: usize,
    /// SimplePIR LWE dimension (`ikpir_common::SimpleConfig::with_lwe_dim`).
    pub lwe_dim: u32,
}

impl Default for ConformanceConfig {
    /// The real gate: ~20,000 genesis accounts, 120 blocks, at least 1200
    /// sampled addresses across 12 in-run checkpoints plus a final pass —
    /// comfortably above `docs/plan.md` §6 Stage 0.5's "at least 1000
    /// addrs x at least 100 blocks" floor.
    fn default() -> Self {
        Self {
            seed: 0xC0FF_EE15_5CAF_FE00,
            num_genesis_keys: 20_000,
            blocks: 120,
            min_addresses: 1_200,
            checkpoints: 12,
            lwe_dim: 512,
        }
    }
}

/// Outcome of one conformance run.
#[derive(Clone, Debug)]
pub struct ConformanceReport {
    /// `true` iff there were zero mismatches, the final sample reached
    /// `min_addresses`, and every category in [`CATEGORIES`] was
    /// non-empty in the final sample.
    pub passed: bool,
    /// Total number of `(address, checkpoint)` diffs performed across the
    /// whole run: every in-run checkpoint plus the dedicated final pass.
    pub total_checks: u64,
    /// Every mismatch found, formatted as a human-readable line: either a
    /// balance/lookup disagreement (`block N: [category] 0x.. expected=..
    /// got=..`) or a harness-structural failure (sample too small, or a
    /// required category came up empty), prefixed `HARNESS:` for the
    /// latter so the two kinds are never confused when skimming the
    /// report.
    pub mismatches: Vec<String>,
    /// The final sample's size per category. Always contains every one of
    /// [`CATEGORIES`] as a key once [`run`] returns — a category present
    /// with count `0` is exactly the "required category is empty"
    /// failure.
    pub category_counts: BTreeMap<String, usize>,
    /// Blocks actually run (`ConformanceConfig::blocks`).
    pub blocks: u64,
    /// Number of diff passes performed: in-run checkpoints plus the one
    /// dedicated final full-sample pass.
    pub checkpoints_run: usize,
    /// Size of the final sample diffed in the dedicated final pass (the
    /// "N addresses" the CLI's summary line reports).
    pub sample_size: usize,
    /// The `min_addresses` target this run was judged against
    /// (`ConformanceConfig::min_addresses`).
    pub min_addresses: usize,
}

impl fmt::Display for ConformanceReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Conformance report")?;
        writeln!(f, "  blocks:           {}", self.blocks)?;
        writeln!(f, "  checkpoints run:  {}", self.checkpoints_run)?;
        writeln!(
            f,
            "  sample size:      {} (target >= {})",
            self.sample_size, self.min_addresses
        )?;
        writeln!(f, "  total checks:     {}", self.total_checks)?;
        writeln!(f, "  mismatches:       {}", self.mismatches.len())?;
        writeln!(f, "  category counts (final sample):")?;
        for (k, v) in &self.category_counts {
            writeln!(f, "    {k:<20} {v}")?;
        }
        if !self.mismatches.is_empty() {
            writeln!(f, "  first mismatches:")?;
            for m in self.mismatches.iter().take(10) {
                writeln!(f, "    {m}")?;
            }
            if self.mismatches.len() > 10 {
                writeln!(f, "    ... and {} more", self.mismatches.len() - 10)?;
            }
        }
        write!(f, "  result: {}", if self.passed { "PASS" } else { "FAIL" })
    }
}

/// Run one conformance pass end to end. See the module docs for the
/// category definitions and the diff rule.
///
/// # Panics
///
/// Every `.expect(..)` inside this function guards a condition that a
/// correctly-wired harness can never violate (e.g. `MockFeed::next_block`
/// erroring, `apply_block` hitting `TableFull` under the fixed
/// live-set-invariant churn this function derives, or `RisePirClient`
/// reporting a block/context mismatch when the harness always ingests
/// every delta before querying — see the module docs). A panic here means
/// the harness itself is broken, not that the private client answered
/// wrongly; that distinction is exactly why these are `.expect`s rather
/// than entries folded into [`ConformanceReport::mismatches`].
pub fn run(cfg: &ConformanceConfig) -> ConformanceReport {
    let codec = value_codec();
    let (changes_per_block, inserts_per_block, deletes_per_block) = per_block_rates(cfg);

    // ── Genesis + deployment geometry ───────────────────────────────────
    let mut feed = MockFeed::new(MockConfig {
        seed: cfg.seed,
        num_genesis_keys: cfg.num_genesis_keys,
        changes_per_block,
        inserts_per_block,
        deletes_per_block,
    });
    let geom = Geometry::for_accounts(
        cfg.num_genesis_keys,
        ARITY,
        BUCKET_SIZE,
        FINGERPRINT_BITS,
        &codec,
        Backend::Simple,
    )
    .expect("geometry: harness-configured account count/codec must be a valid geometry");

    let mut store = Segmented2aryCuckooKVStore::new(
        geom.num_buckets,
        geom.bucket_size,
        geom.fingerprint_bits,
        geom.value_bits,
        geom.plaintext_bits,
    )
    .expect("store: geometry-derived params must construct a valid store");

    let genesis_snapshot = feed.snapshot();
    for (addr, bal) in &genesis_snapshot {
        let v = codec.encode(addr, *bal).expect("encode genesis balance");
        store.insert(*addr, &v).expect("insert genesis account");
    }

    let mut server: RisePirServer<Segmented2aryScheme, SimplePirBackend> =
        RisePirServer::new(store, SimpleConfig::with_lwe_dim(cfg.lwe_dim), codec, 0);
    let mut ring = DeltaRing::new(cfg.blocks as usize + 8); // retain the whole run: /sync-equivalent never ages out
    let mut client: RisePirClient<SimplePirBackend> =
        RisePirClient::from_setup(server.setup(), codec);

    // ── Pre-selected category pools ─────────────────────────────────────
    let genesis_set: HashSet<AddressHash> = genesis_snapshot.iter().map(|(a, _)| *a).collect();

    let never_existed_count = (cfg.min_addresses / 15).clamp(8, 300);
    let contract_count = (cfg.min_addresses / 15).clamp(8, 300);
    assert!(
        contract_count as u64 <= cfg.num_genesis_keys,
        "harness bug: contract_count ({contract_count}) exceeds genesis population ({})",
        cfg.num_genesis_keys
    );

    // CONTRACT: a nominal label over genesis ids 0..contract_count — see
    // the module docs (MockFeed has no EOA/contract distinction).
    let contract: Vec<AddressHash> = (0..contract_count as u64)
        .map(MockFeed::address_for)
        .collect();
    let contract_set: HashSet<AddressHash> = contract.iter().copied().collect();
    debug_assert!(
        contract.iter().all(|a| genesis_set.contains(a)),
        "harness bug: contract ids must be a subset of genesis ids"
    );

    // NEVER-EXISTED: ids strictly beyond anything this run's inserts can
    // ever mint (`inserts_per_block * blocks` upper-bounds every id
    // MockFeed will ever assign), plus a large margin — disjoint from
    // every mock-generated address BY CONSTRUCTION, not merely with high
    // probability.
    let max_minted_id = cfg.num_genesis_keys + inserts_per_block as u64 * cfg.blocks;
    let never_existed_base = max_minted_id + 1_000_000;
    let never_existed: Vec<AddressHash> = (0..never_existed_count as u64)
        .map(|i| MockFeed::address_for(never_existed_base + i))
        .collect();
    for a in &never_existed {
        assert_eq!(
            feed.balance_of(a),
            0,
            "harness bug: a 'never-existed' address collided with a real mock account"
        );
    }

    // The three ground-truth-driven categories split whatever's left of
    // `min_addresses` roughly evenly.
    let per_bucket = cfg
        .min_addresses
        .saturating_sub(never_existed_count + contract_count)
        .div_ceil(3)
        .max(1);

    // ── Run the chain, observing the change stream, diffing continuously ─
    let mut activity: HashMap<AddressHash, u64> = HashMap::new();
    let mut created: Vec<AddressHash> = Vec::new();
    let mut deleted: Vec<AddressHash> = Vec::new();
    let mut known: HashSet<AddressHash> = genesis_set.clone();

    let checkpoint_set: HashSet<u64> = checkpoint_blocks(cfg.blocks, cfg.checkpoints)
        .into_iter()
        .collect();

    let mut total_checks: u64 = 0;
    let mut mismatches: Vec<String> = Vec::new();
    let mut checkpoints_run: usize = 0;

    for block_num in 1..=cfg.blocks {
        let upd = feed
            .next_block()
            .expect("MockFeed::next_block never errors")
            .expect("MockFeed always has a next block");

        for (addr, bal) in &upd.changes {
            *activity.entry(*addr).or_insert(0) += 1;
            if *bal == 0 {
                deleted.push(*addr);
            } else if known.insert(*addr) {
                created.push(*addr);
            }
        }

        let delta = server
            .apply_block(&upd)
            .expect("apply_block: harness geometry must never hit TableFull");
        client
            .ingest_delta(&delta)
            .expect("ingest_delta: contiguous by construction (single producer)");
        ring.push(delta);

        if checkpoint_set.contains(&block_num) {
            let full = full_candidates(
                &feed,
                &genesis_set,
                &contract_set,
                &activity,
                &created,
                &deleted,
            );
            let sample = build_sample(&never_existed, &contract, &full, per_bucket, None);
            total_checks += diff_sample(
                &mut client,
                &server,
                &feed,
                &sample,
                block_num,
                &mut mismatches,
            );
            checkpoints_run += 1;
        }
    }

    assert_eq!(server.block(), cfg.blocks);
    assert_eq!(
        client.pinned_block(),
        0,
        "harness never GCs; hint stays pinned at genesis throughout"
    );
    assert_eq!(ring.head(), Some(cfg.blocks));

    // ── Final full-sample diff: pad to >= min_addresses ─────────────────
    let full = full_candidates(
        &feed,
        &genesis_set,
        &contract_set,
        &activity,
        &created,
        &deleted,
    );
    let final_sample = build_sample(
        &never_existed,
        &contract,
        &full,
        per_bucket,
        Some(cfg.min_addresses),
    );
    total_checks += diff_sample(
        &mut client,
        &server,
        &feed,
        &final_sample,
        cfg.blocks,
        &mut mismatches,
    );
    checkpoints_run += 1;

    let category_counts = tally(&final_sample);
    let sample_size = final_sample.len();

    if sample_size < cfg.min_addresses {
        mismatches.push(format!(
            "HARNESS: final sample size {sample_size} < min_addresses {} — the run was too short \
             for these category pools to fill; widen blocks/num_genesis_keys or the per-block \
             churn rates",
            cfg.min_addresses
        ));
    }
    for cat in CATEGORIES {
        if category_counts.get(cat).copied().unwrap_or(0) == 0 {
            mismatches.push(format!(
                "HARNESS: required category '{cat}' is empty in the final sample — the run was \
                 too short to populate it"
            ));
        }
    }

    let passed = mismatches.is_empty()
        && sample_size >= cfg.min_addresses
        && CATEGORIES
            .iter()
            .all(|c| category_counts.get(*c).copied().unwrap_or(0) > 0);

    ConformanceReport {
        passed,
        total_checks,
        mismatches,
        category_counts,
        blocks: cfg.blocks,
        checkpoints_run,
        sample_size,
        min_addresses: cfg.min_addresses,
    }
}

/// Derives the mock's per-block churn rates from `cfg`, sized so the
/// created/deleted/high-activity category pools comfortably clear
/// `per_bucket` candidates (roughly `min_addresses / 3`) well before the
/// run ends, with margin for the CLI's `--blocks`/`--addresses`
/// overrides. `inserts_per_block == deletes_per_block` keeps the live-set
/// size invariant across the whole run (mirrors
/// `crates/risepir-feed/tests/pipeline.rs`), so a single genesis-sized
/// geometry never risks `TableFull`.
fn per_block_rates(cfg: &ConformanceConfig) -> (usize, usize, usize) {
    let per_block = ((cfg.min_addresses as u64 / cfg.blocks.max(1)) + 5).max(8) as usize;
    let inserts_per_block = per_block;
    let deletes_per_block = per_block;
    // Updates fill the rest of `changes_per_block`; `+ 40` keeps a
    // healthy update volume (for HIGH-ACTIVITY ranking) even when
    // `per_block` itself is small.
    let changes_per_block = per_block * 2 + 40;
    (changes_per_block, inserts_per_block, deletes_per_block)
}

/// Roughly-evenly-spaced block numbers to checkpoint at, always including
/// `blocks` itself (the run's last block) even if `checkpoints` does not
/// evenly divide it, is `0`, or exceeds `blocks`.
fn checkpoint_blocks(blocks: u64, checkpoints: usize) -> Vec<u64> {
    let checkpoints = checkpoints.max(1) as u64;
    let mut out: Vec<u64> = (1..=checkpoints)
        .map(|i| blocks.saturating_mul(i) / checkpoints)
        .filter(|&b| b >= 1)
        .collect();
    out.dedup();
    if blocks >= 1 && out.last() != Some(&blocks) {
        out.push(blocks);
    }
    out
}

/// Every currently-eligible candidate for the three ground-truth-driven
/// categories, uncapped and freshly filtered against `feed`'s CURRENT
/// state — so calling this mid-run reflects exactly what is known so far
/// (the module docs' "diffing continuously" requirement), while calling
/// it after the last block reflects the run's final truth.
struct FullCandidates {
    zero_balance: Vec<AddressHash>,
    created: Vec<AddressHash>,
    high_activity: Vec<AddressHash>,
}

fn full_candidates(
    feed: &MockFeed,
    genesis_set: &HashSet<AddressHash>,
    contract_set: &HashSet<AddressHash>,
    activity: &HashMap<AddressHash, u64>,
    created: &[AddressHash],
    deleted: &[AddressHash],
) -> FullCandidates {
    // ZERO-BALANCE: every observed delete, excluding contract-labelled ids
    // (kept exclusive of the `contract` category — see the module docs).
    let zero_balance: Vec<AddressHash> = deleted
        .iter()
        .copied()
        .filter(|a| !contract_set.contains(a))
        .collect();

    // CREATED-DURING-RUN: first-appearance keys still live right now.
    let created_live: Vec<AddressHash> = created
        .iter()
        .copied()
        .filter(|a| feed.balance_of(a) != 0)
        .collect();

    // HIGH-ACTIVITY: genesis keys (excluding `contract`), still live,
    // ranked by touch count descending (ties broken by address bytes for
    // determinism).
    let mut ranked: Vec<(AddressHash, u64)> = activity
        .iter()
        .map(|(a, c)| (*a, *c))
        .filter(|(a, _)| {
            genesis_set.contains(a) && !contract_set.contains(a) && feed.balance_of(a) != 0
        })
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let high_activity: Vec<AddressHash> = ranked.into_iter().map(|(a, _)| a).collect();

    FullCandidates {
        zero_balance,
        created: created_live,
        high_activity,
    }
}

/// Assembles a sample from the fixed pools (`never_existed`, `contract`)
/// plus per-bucket-capped slices of the three ground-truth-driven
/// candidate lists in `full`.
///
/// `min_total`: `None` for an in-run checkpoint — take exactly
/// `per_bucket` from each growing pool, whatever is available so far (the
/// organic "full sample known so far"). `Some(target)` for the dedicated
/// final pass: if the `per_bucket` caps alone would leave the total under
/// `target`, grow whichever pool(s) have spare candidates beyond their
/// cap — high-activity first, then created, then zero-balance (an
/// arbitrary but fixed priority; at the default geometry all three clear
/// `per_bucket` comfortably on their own) — until `target` is met or every
/// pool is exhausted (in which case [`run`] reports the shortfall as a
/// `HARNESS:` mismatch rather than silently under-filling).
fn build_sample(
    never_existed: &[AddressHash],
    contract: &[AddressHash],
    full: &FullCandidates,
    per_bucket: usize,
    min_total: Option<usize>,
) -> Vec<(AddressHash, &'static str)> {
    let lens = [
        full.zero_balance.len(),
        full.created.len(),
        full.high_activity.len(),
    ];
    let mut caps = [per_bucket, per_bucket, per_bucket]; // [zero_balance, created, high_activity]

    if let Some(target) = min_total {
        let fixed = never_existed.len() + contract.len();
        let capped_total: usize = fixed
            + caps
                .iter()
                .zip(&lens)
                .map(|(c, l)| (*c).min(*l))
                .sum::<usize>();
        let mut deficit = target.saturating_sub(capped_total);
        for idx in [2usize, 1, 0] {
            // priority: high-activity, created, zero-balance
            if deficit == 0 {
                break;
            }
            let room = lens[idx].saturating_sub(caps[idx]);
            let take = room.min(deficit);
            caps[idx] += take;
            deficit -= take;
        }
    }

    let mut sample: Vec<(AddressHash, &'static str)> = Vec::with_capacity(
        never_existed.len()
            + contract.len()
            + caps
                .iter()
                .zip(&lens)
                .map(|(c, l)| (*c).min(*l))
                .sum::<usize>(),
    );
    sample.extend(never_existed.iter().map(|a| (*a, CAT_NEVER_EXISTED)));
    sample.extend(contract.iter().map(|a| (*a, CAT_CONTRACT)));
    sample.extend(
        full.zero_balance
            .iter()
            .take(caps[0])
            .map(|a| (*a, CAT_ZERO_BALANCE)),
    );
    sample.extend(full.created.iter().take(caps[1]).map(|a| (*a, CAT_CREATED)));
    sample.extend(
        full.high_activity
            .iter()
            .take(caps[2])
            .map(|a| (*a, CAT_HIGH_ACTIVITY)),
    );
    sample
}

/// Per-category counts of `sample`. Always contains every one of
/// [`CATEGORIES`] as a key (defaulted to `0`) even if a category has no
/// members, so callers can detect "empty category" via a plain lookup
/// rather than an absent key.
fn tally(sample: &[(AddressHash, &'static str)]) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> =
        CATEGORIES.iter().map(|c| ((*c).to_string(), 0)).collect();
    for (_, cat) in sample {
        *counts.entry((*cat).to_string()).or_insert(0) += 1;
    }
    counts
}

/// Diffs every `(address, category)` pair in `sample`, at `block`, against
/// `feed.balance_of` — the module docs' diff rule — and appends any
/// mismatch to `mismatches`. Returns the number of checks performed.
fn diff_sample(
    client: &mut RisePirClient<SimplePirBackend>,
    server: &RisePirServer<Segmented2aryScheme, SimplePirBackend>,
    feed: &MockFeed,
    sample: &[(AddressHash, &'static str)],
    block: u64,
    mismatches: &mut Vec<String>,
) -> u64 {
    for &(addr, category) in sample {
        let (queries, ctx) = client.build_query(&addr);
        let (resp, at_block) = server
            .answer(&queries)
            .expect("answer: malformed query is a harness bug");
        let lookup = client.finish(&addr, &ctx, resp, at_block).expect(
            "finish: block/context mismatch is a harness wiring bug, not a conformance failure",
        );

        let expected: Balance = feed.balance_of(&addr);
        let ok = match lookup {
            Lookup::Found(b) => b == expected && expected != 0,
            Lookup::NotFound => expected == 0,
            Lookup::DecodeFailed => false,
        };
        if !ok {
            let addr_hex = hex_addr(&addr);
            mismatches.push(format!(
                "block {block}: [{category}] {addr_hex} expected={expected} got={lookup:?}"
            ));
        }
    }
    sample.len() as u64
}

fn hex_addr(a: &AddressHash) -> String {
    let mut s = String::with_capacity(2 + a.len() * 2);
    s.push_str("0x");
    for b in a {
        let _ = write!(s, "{b:02x}");
    }
    s
}
