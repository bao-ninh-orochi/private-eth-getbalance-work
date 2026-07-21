//! `xtask bench` — Stage 3, the measured numbers table (`docs/plan.md` §7
//! "The headline, measured"; `docs/verification.md` §7 Correction 4
//! ("per-block patch is not constant in wall-clock") and the "coalesced
//! deltas are magnitude-bounded" finding).
//!
//! # Purpose
//!
//! Every number this module reports is *measured*, with `std::time::Instant`,
//! against the real, built [`RisePirServer`] / [`ValueCodec`] / [`Geometry`]
//! at the real `lwe_dim = 1275` (`SimpleConfig::default()`) and realistic
//! wei-scale balances ([`risepir_feed::MockFeed`]) — never guessed, never
//! hardcoded, never extrapolated. The one exception, by design: the byte
//! *sizes* (hint/query/response/A/server-DB), which are a deterministic,
//! closed-form function of the geometry ([`Geometry::sizes`]) — reported as
//! **computed**, never as **measured**, and never conflated with the timed
//! numbers around them.
//!
//! # What gets measured (see [`run`] and [`BenchReport`])
//!
//! 1. **Full-rebuild time** — the §7 headline denominator — at several
//!    account scales, up to the largest that safely fits this machine's
//!    16 GB and completes in a reasonable time (see
//!    `unsafe_to_attempt_top_scale` for the fallback logic; any scale
//!    that could not be reached is labelled, never silently dropped).
//! 2. **Per-block patch time** as a curve over mutations/block (`K`), at a
//!    fixed mid scale — the "N-independent in op count, plateaus once the
//!    hint exceeds cache" curve of `docs/verification.md` Correction 4.
//! 3. **Per-block delta bytes**, compact ([`risepir_proto::BlockDelta::encoded_len`])
//!    vs. the naive 10-B/cell upstream baseline, on realistic balances —
//!    `docs/plan.md` ADR-0005: small-int balances would understate the win.
//! 4. **Hint / query / response / `A` / server-DB sizes**, and client
//!    memory, at every scale — computed, not timed.
//! 5. **Answer latency** at the mid scale.
//! 6. **The headline**: full-rebuild ÷ per-block-patch(K≈300) ratio, and
//!    the per-block-patch duty cycle against a 12 s block, per scale —
//!    framed as `docs/plan.md` §7 does: the honest measured ratio, not the
//!    brief's 10^5–10^6.
//!
//! # Why insert/delete-free updates drive every timed block
//!
//! Every block this module applies is drawn from a [`MockFeed`] configured
//! with `inserts_per_block = 0, deletes_per_block = 0` — pure balance
//! updates to already-live accounts. This matches the brief's "K realistic
//! updates to **existing** accounts" for the patch curve, and, just as
//! importantly, keeps every account the mock ever generates permanently
//! live (the live set can only ever grow at genesis, never shrink or gain
//! members later) — so a key drawn from [`MockFeed::live_keys`] at any
//! point during a run is *guaranteed* to already be present in the
//! server's store, without this module having to re-derive or track that
//! itself.
//!
//! # Why block slices, not fresh `MockFeed` configs, produce each `K`
//!
//! [`MockFeed::next_block`] always emits exactly `changes_per_block`
//! changes. Rather than construct a fresh mock per `K` value (which would
//! also mean a fresh, differently-seeded genesis — no longer the "fixed
//! mid scale" the brief asks for), this module configures one mock with
//! `changes_per_block` equal to the *largest* `K` the run needs, and takes
//! the first `K` entries of each emitted block for a smaller `K`. Every
//! entry is still a genuine `MockFeed`-generated `(address, realistic
//! balance)` pair — see `sliced_block`.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use ikpir_common::backend::simple::SimpleParams;
use ikpir_common::{SimpleConfig, SimplePirBackend};
use risepir_client::RisePirClient;
use risepir_feed::{Feed, MockConfig, MockFeed};
use risepir_proto::{Backend, BlockDelta, BlockUpdate, Geometry, Sizes, ValueCodec};
use risepir_server::RisePirServer;
use segmented_cuckoo::{Segmented3aryCuckooKVStore, Segmented3aryScheme};

/// This deployment's fixed SCF knobs — arity-3 SCF, mirroring
/// `xtask::conformance` and every other crate's shared test geometry.
const ARITY: u32 = 3;
const BUCKET_SIZE: u32 = 4;
const FINGERPRINT_BITS: u32 = 32;
/// This deployment's fixed value-codec knobs — the ADR-0009 layout:
/// 32-bit `key_tag` / 96-bit balance / 16-bit checksum = 144 bits.
const KEY_TAG_BITS: u32 = 32;
const BALANCE_BITS: u32 = 96;
const CHECKSUM_BITS: u32 = 16;

/// Operational safety ceiling for the adaptive top-scale fallback (see
/// `unsafe_to_attempt_top_scale`) — not a reported number, just a guard
/// against a multi-minute-or-worse run on an unexpectedly slow machine.
const MAX_PROJECTED_REBUILD_SECS: f64 = 240.0;
/// Generous multiplicative pad over a naive linear extrapolation from the
/// previous scale's measured rebuild time, since GEMM/matvec efficiency
/// can vary with matrix shape.
const SAFETY_FACTOR: f64 = 5.0;
/// Soft memory ceiling for the top-scale fallback: well under this
/// machine's 16 GB, leaving headroom for the OS, cargo, and every
/// structure the bench harness itself allocates around the store.
const MAX_PROJECTED_BYTES: u64 = 10 * 1024 * 1024 * 1024;

type Server = RisePirServer<Segmented3aryScheme, SimplePirBackend>;
type Client = RisePirClient<SimplePirBackend>;

/// This deployment's [`ValueCodec`] — ADR-0009's `key_tag(32) ‖
/// balance(96) ‖ checksum(16)` = 144-bit value.
fn value_codec() -> ValueCodec {
    ValueCodec {
        key_tag_bits: KEY_TAG_BITS,
        balance_bits: BALANCE_BITS,
        checksum_bits: CHECKSUM_BITS,
    }
}

// ─── Configuration ────────────────────────────────────────────────────────

/// Configuration for one `bench` run. [`Default`] is the real Stage 3 gate
/// (`docs/plan.md` §7); tests build a small override so the harness itself
/// is exercised in `cargo test` without the full multi-minute sweep.
#[derive(Clone, Debug)]
pub struct BenchConfig {
    /// Deterministic mock seed. Fixed (never randomised) so re-runs are
    /// comparable, per the brief.
    pub seed: u64,
    /// Full-rebuild account scales (item 1). The *last* (largest) entry is
    /// subject to the adaptive safety fallback (see
    /// `unsafe_to_attempt_top_scale`); every other entry is always
    /// attempted as given.
    pub scales: Vec<u64>,
    /// The fixed mid scale the patch curve (item 2), delta-bytes (item 3),
    /// and answer-latency (item 5) sections run at. Must appear in
    /// [`Self::scales`].
    pub mid_scale: u64,
    /// Mutations/block values for the patch curve (item 2).
    pub k_values: Vec<usize>,
    /// The `K` used for the one-point-per-scale headline patch
    /// measurement (item 6) and the delta-bytes section (item 3).
    pub headline_k: usize,
    /// Warm-up blocks applied (and discarded) before each timed patch
    /// measurement.
    pub warmup_blocks: usize,
    /// Timed blocks averaged for each patch-time-at-`K` measurement.
    pub measured_blocks: usize,
    /// Timed queries averaged for the answer-latency measurement (item 5).
    pub measured_queries: usize,
    /// Simulated Ethereum block time, in seconds — the duty-cycle
    /// denominator (item 6).
    pub block_time_secs: f64,
}

impl Default for BenchConfig {
    /// The real Stage 3 gate: 100K / 1M / ~9.4M account scales — the third
    /// is `segment_rows = 2^20`'s exact 75%-load account count, the same
    /// operating point `docs/verification.md` Correction 4 and
    /// `docs/plan.md` §7 already report, so this run's numbers are
    /// directly comparable to those — the full Correction-4 `K` curve at
    /// 1M, and `lwe_dim = 1275` (`SimpleConfig::default()`, applied in
    /// `build_scale`).
    fn default() -> Self {
        Self {
            seed: 0xB0DA_C0DE_5CA1_E000,
            scales: vec![100_000, 1_000_000, 9_437_184],
            mid_scale: 1_000_000,
            k_values: vec![50, 150, 300, 600, 1200],
            headline_k: 300,
            warmup_blocks: 5,
            measured_blocks: 10,
            measured_queries: 20,
            block_time_secs: 12.0,
        }
    }
}

// ─── Report types ───────────────────────────────────────────────────────

/// Measured + computed results for one full-rebuild scale (item 1, item 4,
/// and this scale's one-point contribution to the item-6 headline).
#[derive(Clone, Debug)]
pub struct ScaleReport {
    /// Accounts actually built at this scale (equals the requested value
    /// from [`BenchConfig::scales`] unless this was the top scale and the
    /// adaptive fallback reduced it — see
    /// [`BenchReport::top_scale_fallback_reason`]).
    pub accounts: u64,
    /// Wall-clock time of `RisePirServer::new(..)` — the full per-segment
    /// `server_setup`, i.e. the full rebuild (item 1).
    pub rebuild: Duration,
    /// This scale's derived geometry.
    pub geometry: Geometry,
    /// This scale's derived sizes (item 4) — computed from `geometry`, not
    /// timed.
    pub sizes: Sizes,
    /// Average `apply_block` time at `K = `[`BenchConfig::headline_k`]
    /// mutations/block, at this scale — this scale's point in the item-6
    /// headline table.
    pub headline_patch_ms: f64,
}

/// One point of the item-2 patch-time-vs-`K` curve, at
/// [`BenchConfig::mid_scale`].
#[derive(Clone, Copy, Debug)]
pub struct PatchCurvePoint {
    /// Mutations in this block.
    pub k: usize,
    /// Average `apply_block` wall-clock time, in milliseconds.
    pub avg_ms: f64,
}

/// Item 3: per-block delta bytes, compact vs. the naive upstream baseline,
/// measured on one representative `K = `[`BenchConfig::headline_k`] block
/// at [`BenchConfig::mid_scale`] with realistic wei-scale balances.
#[derive(Clone, Copy, Debug)]
pub struct DeltaBytesReport {
    /// `K` this delta was produced at.
    pub k: usize,
    /// Scale this delta was produced at.
    pub accounts: u64,
    /// Total nonzero `(row, offset)` cells across every segment of the
    /// measured [`BlockDelta`].
    pub nonzero_cells: usize,
    /// [`BlockDelta::encoded_len`] — the compact varint/zigzag wire size.
    pub compact_bytes: usize,
    /// `nonzero_cells * 10` — the upstream `u16` offset + `i64` delta
    /// baseline (`docs/verification.md`: "Delta wire cost is 10 B/cell").
    pub naive_bytes: usize,
    /// `naive_bytes / compact_bytes`.
    pub ratio: f64,
}

/// Item 5: `RisePirServer::answer` latency at [`BenchConfig::mid_scale`],
/// averaged over [`BenchConfig::measured_queries`] real queries built by a
/// real [`RisePirClient`] from `server.setup()`.
#[derive(Clone, Copy, Debug)]
pub struct AnswerLatencyReport {
    /// Scale this was measured at.
    pub accounts: u64,
    /// Queries averaged.
    pub n_queries: usize,
    /// Average `server.answer(&queries)` wall-clock time, in milliseconds.
    pub avg_ms: f64,
}

/// Full result of one [`run`] — every number `docs/numbers.md` reports.
#[derive(Clone, Debug)]
pub struct BenchReport {
    /// The config this report was produced from.
    pub config: BenchConfig,
    /// Item 1 + item 4 + one point of item 6, per scale, ascending.
    pub scales: Vec<ScaleReport>,
    /// Item 2: the full `K` curve at [`BenchConfig::mid_scale`], ascending
    /// by `k`.
    pub patch_curve: Vec<PatchCurvePoint>,
    /// Item 3.
    pub delta_bytes: DeltaBytesReport,
    /// Item 5.
    pub answer_latency: AnswerLatencyReport,
    /// The largest scale [`BenchConfig::scales`] actually asked for.
    pub requested_top_scale: u64,
    /// The largest scale actually built and measured — equals
    /// `requested_top_scale` unless the adaptive safety fallback (see
    /// `unsafe_to_attempt_top_scale`) reduced it.
    pub reached_top_scale: u64,
    /// `Some(reason)` iff `reached_top_scale < requested_top_scale` — the
    /// brief requires any unreached scale to be labelled explicitly, never
    /// silently dropped.
    pub top_scale_fallback_reason: Option<String>,
}

// ─── The sweep ───────────────────────────────────────────────────────────

/// Run the full Stage 3 measurement sweep described in this module's docs.
/// Every timed number comes from `std::time::Instant` around a real call
/// into the built [`RisePirServer`] / [`ValueCodec`] / [`Geometry`]; sizes
/// are computed via [`Geometry::sizes`], never timed.
///
/// # Panics
///
/// If `cfg.scales` is empty, if `cfg.mid_scale` is not one of
/// `cfg.scales`, or if any internal step that this harness's own fixed
/// geometry/config guarantees to succeed (store construction at the
/// target 75% load, genesis insertion, `apply_block` under the
/// insert/delete-free `K`-update workload) fails regardless — a failure
/// here means the harness itself is misconfigured, not that it hit a
/// real, expected error condition.
pub fn run(cfg: &BenchConfig) -> BenchReport {
    assert!(!cfg.scales.is_empty(), "bench: cfg.scales must not be empty");
    assert!(
        cfg.scales.contains(&cfg.mid_scale),
        "bench: cfg.mid_scale ({}) must be one of cfg.scales ({:?})",
        cfg.mid_scale,
        cfg.scales
    );

    let codec = value_codec();
    let max_changes_per_block = cfg
        .k_values
        .iter()
        .copied()
        .chain(std::iter::once(cfg.headline_k))
        .max()
        .expect("bench: at least headline_k is always present");

    let mut scales = cfg.scales.clone();
    scales.sort_unstable();
    let requested_top_scale = *scales.last().expect("checked non-empty above");

    let mut scale_reports: Vec<ScaleReport> = Vec::with_capacity(scales.len());
    let mut top_scale_fallback_reason: Option<String> = None;
    let mut reached_top_scale = requested_top_scale;

    let mut patch_curve: Vec<PatchCurvePoint> = Vec::new();
    let mut delta_bytes: Option<DeltaBytesReport> = None;
    let mut answer_latency: Option<AnswerLatencyReport> = None;

    for (i, &requested_accounts) in scales.iter().enumerate() {
        let is_last = i + 1 == scales.len();
        let mut accounts = requested_accounts;

        // Adaptive fallback: only the largest scale, and only once there is
        // a previous measurement to project from — see the module docs
        // ("largest additional scale ... that both fits in memory ... and
        // completes ... in a reasonable time").
        if is_last && i > 0 {
            if let Some(reason) = unsafe_to_attempt_top_scale(&scale_reports[i - 1], accounts, &codec) {
                let prev_accounts = scale_reports[i - 1].accounts;
                let mut candidate = accounts;
                loop {
                    let next = (candidate / 2).max(prev_accounts);
                    if next == candidate {
                        break;
                    }
                    candidate = next;
                    if candidate <= prev_accounts
                        || unsafe_to_attempt_top_scale(&scale_reports[i - 1], candidate, &codec).is_none()
                    {
                        break;
                    }
                }
                top_scale_fallback_reason = Some(format!(
                    "{reason} — falling back to the largest scale that passed the safety check: \
                     {candidate} accounts (requested {requested_accounts})"
                ));
                accounts = candidate;
            }
        }
        reached_top_scale = accounts;

        let mut build = build_scale(accounts, cfg.seed, max_changes_per_block, codec);

        // Every scale gets one headline K≈headline_k patch measurement
        // (item 6's per-scale ratio/duty-cycle point).
        let (headline_ms, headline_delta) = measure_patch_at_k(
            &mut build.server,
            &mut build.feed,
            cfg.headline_k,
            cfg.warmup_blocks,
            cfg.measured_blocks,
        );

        // The fixed mid scale additionally gets the full K curve (item 2),
        // the delta-bytes section (item 3), and answer latency (item 5).
        let is_mid_scale = accounts == cfg.mid_scale && patch_curve.is_empty();
        if is_mid_scale {
            for &k in &cfg.k_values {
                let avg_ms = if k == cfg.headline_k {
                    headline_ms
                } else {
                    measure_patch_at_k(&mut build.server, &mut build.feed, k, cfg.warmup_blocks, cfg.measured_blocks).0
                };
                patch_curve.push(PatchCurvePoint { k, avg_ms });
            }
            patch_curve.sort_by_key(|p| p.k);

            let nonzero_cells: usize = headline_delta
                .per_segment
                .iter()
                .flat_map(|seg| seg.iter())
                .map(|(_, cells)| cells.len())
                .sum();
            let compact_bytes = headline_delta.encoded_len();
            let naive_bytes = nonzero_cells * 10;
            delta_bytes = Some(DeltaBytesReport {
                k: cfg.headline_k,
                accounts,
                nonzero_cells,
                compact_bytes,
                naive_bytes,
                ratio: naive_bytes as f64 / compact_bytes.max(1) as f64,
            });

            let avg_ms = measure_answer_latency(&build.server, &build.feed, &codec, cfg.measured_queries);
            answer_latency = Some(AnswerLatencyReport {
                accounts,
                n_queries: cfg.measured_queries,
                avg_ms,
            });
        }

        scale_reports.push(ScaleReport {
            accounts,
            rebuild: build.rebuild,
            geometry: build.geometry,
            sizes: build.sizes,
            headline_patch_ms: headline_ms,
        });
        // `build` (and the multi-hundred-MB-to-multi-GB server/feed it
        // owns) is dropped here, before the next scale is built — peak
        // memory is one scale's worth, not the sum of every scale.
    }

    BenchReport {
        config: cfg.clone(),
        scales: scale_reports,
        patch_curve,
        delta_bytes: delta_bytes.expect("mid_scale is always in cfg.scales (asserted above), so it always runs"),
        answer_latency: answer_latency.expect("mid_scale is always in cfg.scales (asserted above), so it always runs"),
        requested_top_scale,
        reached_top_scale,
        top_scale_fallback_reason,
    }
}

// ─── Measurement primitives ───────────────────────────────────────────────

/// Owns one scale's fully-built server + feed + derived geometry/sizes,
/// plus the measured rebuild time — the item-1 number.
struct ScaleBuild {
    server: Server,
    feed: MockFeed,
    geometry: Geometry,
    sizes: Sizes,
    rebuild: Duration,
}

/// Builds a [`MockFeed`] genesis of `accounts` nonzero-balance accounts, a
/// [`Segmented3aryCuckooKVStore`] from its `snapshot()`, and TIMES
/// `RisePirServer::new(..)` — the full rebuild (item 1). The feed is
/// configured for insert/delete-free churn (`inserts_per_block =
/// deletes_per_block = 0`) so every subsequent `next_block()` call is pure
/// updates to already-live accounts (see the module docs).
fn build_scale(accounts: u64, seed: u64, max_changes_per_block: usize, codec: ValueCodec) -> ScaleBuild {
    let geometry = Geometry::for_accounts(accounts, ARITY, BUCKET_SIZE, FINGERPRINT_BITS, &codec, Backend::Simple)
        .unwrap_or_else(|e| panic!("bench: geometry for {accounts} accounts: {e}"));

    let feed = MockFeed::new(MockConfig {
        seed,
        num_genesis_keys: accounts,
        changes_per_block: max_changes_per_block,
        inserts_per_block: 0,
        deletes_per_block: 0,
    });

    let mut store = Segmented3aryCuckooKVStore::new(
        geometry.num_buckets,
        geometry.bucket_size,
        geometry.fingerprint_bits,
        geometry.value_bits,
        geometry.plaintext_bits,
    )
    .unwrap_or_else(|e| panic!("bench: store construction for {accounts} accounts: {e}"));

    for (addr, balance) in feed.snapshot() {
        let encoded = codec
            .encode(&addr, balance)
            .expect("bench: genesis balance must encode under the fixed ADR-0009 codec");
        store.insert(addr, &encoded).unwrap_or_else(|e| {
            panic!(
                "bench: genesis insert at {accounts} accounts hit {e:?} (geometry is sized for \
                 75% target load; this should not happen)"
            )
        });
    }

    let t0 = Instant::now();
    let server = RisePirServer::new(store, SimpleConfig::default(), codec, 0);
    let rebuild = t0.elapsed();

    let sizes = geometry.sizes(Backend::Simple, accounts);
    ScaleBuild {
        server,
        feed,
        geometry,
        sizes,
        rebuild,
    }
}

/// The first `k` changes of one fresh `feed.next_block()` — see the module
/// docs ("Why block slices, not fresh `MockFeed` configs, produce each
/// `K`") for why this is a genuine `MockFeed`-sourced, realistic-balance
/// `K`-sized update block to already-live accounts.
fn sliced_block(feed: &mut MockFeed, k: usize) -> BlockUpdate {
    let upd = feed
        .next_block()
        .expect("MockFeed::next_block never errors")
        .expect("MockFeed always has a next block");
    let block = upd.block;
    let changes = upd.changes.into_iter().take(k).collect();
    BlockUpdate { block, changes, credits: vec![] }
}

/// Applies `warmup_blocks` (discarded) then `measured_blocks` (timed)
/// blocks of exactly `k` realistic updates to existing accounts, and
/// returns the average `apply_block` wall-clock time in milliseconds plus
/// the last measured block's [`BlockDelta`] (consumed for the item-3
/// delta-bytes section when `k == headline_k`).
fn measure_patch_at_k(
    server: &mut Server,
    feed: &mut MockFeed,
    k: usize,
    warmup_blocks: usize,
    measured_blocks: usize,
) -> (f64, BlockDelta) {
    for _ in 0..warmup_blocks {
        let update = sliced_block(feed, k);
        server
            .apply_block(&update)
            .expect("bench: apply_block under the insert/delete-free K-update workload must not fail");
    }

    let mut total = Duration::ZERO;
    let mut last_delta = None;
    for _ in 0..measured_blocks {
        let update = sliced_block(feed, k);
        let t0 = Instant::now();
        let delta = server
            .apply_block(&update)
            .expect("bench: apply_block under the insert/delete-free K-update workload must not fail");
        total += t0.elapsed();
        last_delta = Some(delta);
    }

    let avg_ms = total.as_secs_f64() * 1000.0 / measured_blocks as f64;
    (avg_ms, last_delta.expect("measured_blocks is always >= 1 in this module's configs"))
}

/// Builds a real [`RisePirClient`] from `server.setup()` and times
/// `server.answer(&queries)` (only — never `client.finish`, per the
/// brief) over `n_queries` real, live keys, returning the average in
/// milliseconds.
fn measure_answer_latency(server: &Server, feed: &MockFeed, codec: &ValueCodec, n_queries: usize) -> f64 {
    let mut client: Client = RisePirClient::from_setup(server.setup(), *codec);
    let live = feed.live_keys();
    assert!(!live.is_empty(), "bench: a scale's live set must be non-empty");

    let mut rng = Xorshift64(0xA5A5_5A5A_1234_5678);
    let mut total = Duration::ZERO;
    for _ in 0..n_queries {
        let idx = (rng.next() as usize) % live.len();
        let (queries, _ctx) = client.build_query(&live[idx]);
        let t0 = Instant::now();
        let _ = server.answer(&queries).expect("bench: answer must succeed for a well-formed query");
        total += t0.elapsed();
    }
    total.as_secs_f64() * 1000.0 / n_queries as f64
}

/// `Some(reason)` iff attempting `candidate` accounts is judged unsafe on
/// this machine, projected from `prev`'s own measured rebuild time at
/// `prev.accounts` — a generously-padded linear extrapolation
/// ([`SAFETY_FACTOR`]) against a wall-clock ceiling
/// ([`MAX_PROJECTED_REBUILD_SECS`]), plus a memory projection from the
/// scale's own closed-form [`Geometry::sizes`] against a soft ceiling
/// ([`MAX_PROJECTED_BYTES`]). This projection is used *only* to decide
/// whether to attempt a scale — it is never itself reported as a measured
/// number (the brief: report actual measured times, never extrapolated
/// ones).
fn unsafe_to_attempt_top_scale(prev: &ScaleReport, candidate: u64, codec: &ValueCodec) -> Option<String> {
    let prev_secs = prev.rebuild.as_secs_f64().max(1e-6);
    let projected_secs = prev_secs * (candidate as f64 / prev.accounts as f64) * SAFETY_FACTOR;
    if projected_secs > MAX_PROJECTED_REBUILD_SECS {
        return Some(format!(
            "projected rebuild time for {candidate} accounts (~{projected_secs:.0} s, a \
             {SAFETY_FACTOR}x-padded linear extrapolation from the measured {prev_secs:.3} s at \
             {} accounts) exceeds the {MAX_PROJECTED_REBUILD_SECS:.0} s safety ceiling",
            prev.accounts
        ));
    }
    let projected_bytes = projected_memory_bytes(candidate, codec);
    if projected_bytes > MAX_PROJECTED_BYTES {
        return Some(format!(
            "projected memory for {candidate} accounts (~{}) exceeds the {} safety ceiling",
            fmt_bytes(projected_bytes),
            fmt_bytes(MAX_PROJECTED_BYTES)
        ));
    }
    None
}

/// A generous over-estimate of this harness's own peak memory at
/// `accounts` accounts: the store's flat cell array ([`Sizes::server_db`],
/// exact) plus a padded allowance for the three per-segment backend
/// structures ([`Sizes::hint_per_segment`] and twice
/// [`Sizes::a_per_segment`], to cover both the kept `Hint`/`ServerParams`
/// and the transient `HintMaterial` `server_setup` also builds) plus a
/// flat per-account allowance for the [`MockFeed`] genesis model (its
/// `live` vec, `balances` map, and the transient `snapshot()` vec this
/// module consumes) — this crate does not expose an exact formula for the
/// latter, so the allowance is deliberately generous.
fn projected_memory_bytes(accounts: u64, codec: &ValueCodec) -> u64 {
    let geometry = Geometry::for_accounts(accounts, ARITY, BUCKET_SIZE, FINGERPRINT_BITS, codec, Backend::Simple)
        .expect("bench: geometry must be constructible for the projected scale");
    let sizes = geometry.sizes(Backend::Simple, accounts);
    let backend_bytes = 3 * (sizes.hint_per_segment + 2 * sizes.a_per_segment);
    let mock_feed_overhead = accounts * 120;
    sizes.server_db + backend_bytes + mock_feed_overhead
}

/// Small dependency-free deterministic PRNG (xorshift64*) — mirrors the
/// identical helper `risepir-server`/`risepir-client`'s own test modules
/// use, for the same "no `rand` dev-dependency just for a bench harness"
/// reason.
struct Xorshift64(u64);
impl Xorshift64 {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

// ─── Formatting ────────────────────────────────────────────────────────────

/// Thousands-separated `u64`, e.g. `9437184` -> `"9,437,184"`.
fn fmt_num(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Human-readable byte size with the exact count alongside, e.g.
/// `"13.90 MB (14,572,800 B)"`.
fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut unit = 0usize;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} B", fmt_num(bytes))
    } else {
        format!("{v:.2} {} ({} B)", UNITS[unit], fmt_num(bytes))
    }
}

impl BenchReport {
    /// Renders every measured/computed number into the markdown this
    /// module's `xtask bench` subcommand prints to stdout and writes to
    /// `docs/numbers.md` — always the identical bytes to both
    /// destinations. `machine_note` and `date` are the only inputs not
    /// already captured by `self` (both environmental, not measurements;
    /// this function does no I/O itself, so it stays pure and testable).
    pub fn to_markdown(&self, machine_note: &str, date: &str) -> String {
        let mut out = String::new();
        let codec = value_codec();

        writeln!(out, "# RisePIR numbers table — Stage 3 (measured)").unwrap();
        writeln!(out).unwrap();
        writeln!(out, "Machine: {machine_note}").unwrap();
        writeln!(out, "Date: {date}").unwrap();
        writeln!(
            out,
            "Config: arity {ARITY}, bucket_size {BUCKET_SIZE}, fingerprint_bits {FINGERPRINT_BITS}, \
             value = key_tag({key_tag_bits}) ‖ balance({balance_bits}) ‖ checksum({checksum_bits}) = \
             {value_bits} bits (ADR-0009), lwe_dim {lwe_dim} / sigma {sigma} (`SimpleConfig::default()`), \
             mock seed 0x{seed:016X}",
            key_tag_bits = KEY_TAG_BITS,
            balance_bits = BALANCE_BITS,
            checksum_bits = CHECKSUM_BITS,
            value_bits = codec.value_bits(),
            lwe_dim = SimpleParams::DEFAULT_LWE_DIM,
            sigma = SimpleParams::DEFAULT_SIGMA,
            seed = self.config.seed,
        )
        .unwrap();
        writeln!(
            out,
            "Every number below is measured with `std::time::Instant` against a real, built \
             `RisePirServer` — except the byte sizes in §4, which are computed from \
             `Geometry::sizes` (deterministic, not timed)."
        )
        .unwrap();
        writeln!(out).unwrap();
        writeln!(
            out,
            "**IKPIR build (read before reproducing).** The full-rebuild and answer-latency numbers \
             here are measured against the workspace's pinned IKPIR `perf/optimized` rev \
             (`3d60fa7`, 2026-07-21 — see the root `Cargo.toml`), with the default-on `parallel` \
             feature (rayon matvec/GEMM kernels). A `--no-default-features` build reports \
             substantially slower, single-threaded rebuild/answer times (the sizes and delta-byte \
             figures are unaffected). `xtask bench` prints to stdout by default; pass `--write` to \
             overwrite this file, and only do so from a build against the pinned rev — bump the rev \
             and these numbers together, never separately."
        )
        .unwrap();
        if let Some(reason) = &self.top_scale_fallback_reason {
            writeln!(out).unwrap();
            writeln!(
                out,
                "**Note:** the requested top scale ({} accounts) was not reached: {reason}.",
                fmt_num(self.requested_top_scale)
            )
            .unwrap();
        }
        writeln!(out).unwrap();

        // ── 1. Full-rebuild time ─────────────────────────────────────
        writeln!(out, "## 1. Full-rebuild time (the headline denominator)").unwrap();
        writeln!(out).unwrap();
        writeln!(out, "| accounts | full rebuild (measured) |").unwrap();
        writeln!(out, "|---:|---:|").unwrap();
        for s in &self.scales {
            let flag = if s.accounts == self.reached_top_scale && self.top_scale_fallback_reason.is_some() {
                " *(fallback scale — see note above)*"
            } else {
                ""
            };
            writeln!(out, "| {} | {:.3} s{flag} |", fmt_num(s.accounts), s.rebuild.as_secs_f64()).unwrap();
        }
        writeln!(out).unwrap();

        // ── 2. Patch curve ───────────────────────────────────────────
        writeln!(
            out,
            "## 2. Per-block patch time vs. mutations/block (K), at {} accounts",
            fmt_num(self.config.mid_scale)
        )
        .unwrap();
        writeln!(out).unwrap();
        writeln!(
            out,
            "Each point: {} warm-up blocks discarded, then {} measured blocks averaged \
             (`docs/verification.md` Correction 4: N-independent in op count, plateaus once the \
             hint exceeds cache — report what is actually seen).",
            self.config.warmup_blocks, self.config.measured_blocks
        )
        .unwrap();
        writeln!(out).unwrap();
        writeln!(out, "| K (mutations/block) | patch time (ms/block, measured) |").unwrap();
        writeln!(out, "|---:|---:|").unwrap();
        for p in &self.patch_curve {
            writeln!(out, "| {} | {:.4} |", p.k, p.avg_ms).unwrap();
        }
        writeln!(out).unwrap();

        // ── 3. Delta bytes ───────────────────────────────────────────
        writeln!(
            out,
            "## 3. Per-block delta bytes: compact vs. naive (K≈{}, {} accounts, realistic \
             wei-scale balances)",
            self.delta_bytes.k,
            fmt_num(self.delta_bytes.accounts)
        )
        .unwrap();
        writeln!(out).unwrap();
        writeln!(out, "| metric | value |").unwrap();
        writeln!(out, "|---|---:|").unwrap();
        writeln!(out, "| nonzero cells in delta | {} |", fmt_num(self.delta_bytes.nonzero_cells as u64)).unwrap();
        writeln!(
            out,
            "| naive (10 B/cell, upstream `u16`+`i64`) | {} |",
            fmt_bytes(self.delta_bytes.naive_bytes as u64)
        )
        .unwrap();
        writeln!(
            out,
            "| compact (`BlockDelta::encoded_len`, varint/zigzag) | {} |",
            fmt_bytes(self.delta_bytes.compact_bytes as u64)
        )
        .unwrap();
        writeln!(out, "| compaction ratio | {:.2}× |", self.delta_bytes.ratio).unwrap();
        writeln!(out).unwrap();

        // ── 4. Sizes ──────────────────────────────────────────────────
        writeln!(out, "## 4. Hint / query / response / A / server-DB sizes, and client memory").unwrap();
        writeln!(out).unwrap();
        writeln!(out, "### 4a. Geometry, per scale (computed)").unwrap();
        writeln!(out).unwrap();
        writeln!(
            out,
            "| accounts | num_buckets | plaintext_bits | load factor | cells/slot | row_width | k | \
             R (reshape_rows) | C (reshape_row_width) |"
        )
        .unwrap();
        writeln!(out, "|---:|---:|---:|---:|---:|---:|---:|---:|---:|").unwrap();
        for s in &self.scales {
            writeln!(
                out,
                "| {} | {} | {} | {:.4} | {} | {} | {} | {} | {} |",
                fmt_num(s.accounts),
                fmt_num(u64::from(s.geometry.num_buckets)),
                s.geometry.plaintext_bits,
                s.sizes.load_factor,
                s.sizes.cells_per_slot,
                s.sizes.row_width,
                s.sizes.k,
                fmt_num(u64::from(s.sizes.reshape_rows)),
                fmt_num(u64::from(s.sizes.reshape_row_width)),
            )
            .unwrap();
        }
        writeln!(out).unwrap();

        writeln!(out, "### 4b. Per-segment sizes, per scale (computed, not timed)").unwrap();
        writeln!(out).unwrap();
        writeln!(out, "| accounts | hint/segment | query/segment | response/segment | A/segment | server DB |").unwrap();
        writeln!(out, "|---:|---:|---:|---:|---:|---:|").unwrap();
        for s in &self.scales {
            writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} |",
                fmt_num(s.accounts),
                fmt_bytes(s.sizes.hint_per_segment),
                fmt_bytes(s.sizes.query_per_segment),
                fmt_bytes(s.sizes.response_per_segment),
                fmt_bytes(s.sizes.a_per_segment),
                fmt_bytes(s.sizes.server_db),
            )
            .unwrap();
        }
        writeln!(out).unwrap();

        writeln!(out, "### 4c. Deployment totals (×{ARITY} segments) and client memory (computed)").unwrap();
        writeln!(out).unwrap();
        writeln!(
            out,
            "A client holds `A` + hint for every segment; server DB / hint / query / response / A \
             above are already per-segment, so deployment totals and client memory both multiply by \
             arity ({ARITY})."
        )
        .unwrap();
        writeln!(out).unwrap();
        writeln!(
            out,
            "| accounts | hint total | query total | response total | A total | client memory (A+hint) total |"
        )
        .unwrap();
        writeln!(out, "|---:|---:|---:|---:|---:|---:|").unwrap();
        for s in &self.scales {
            let arity = u64::from(ARITY);
            let hint_total = s.sizes.hint_per_segment * arity;
            let query_total = s.sizes.query_per_segment * arity;
            let response_total = s.sizes.response_per_segment * arity;
            let a_total = s.sizes.a_per_segment * arity;
            let client_mem = (s.sizes.a_per_segment + s.sizes.hint_per_segment) * arity;
            writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} |",
                fmt_num(s.accounts),
                fmt_bytes(hint_total),
                fmt_bytes(query_total),
                fmt_bytes(response_total),
                fmt_bytes(a_total),
                fmt_bytes(client_mem),
            )
            .unwrap();
        }
        writeln!(out).unwrap();

        // ── 5. Answer latency ────────────────────────────────────────
        writeln!(out, "## 5. Answer latency, at {} accounts", fmt_num(self.answer_latency.accounts)).unwrap();
        writeln!(out).unwrap();
        writeln!(out, "| metric | value |").unwrap();
        writeln!(out, "|---|---:|").unwrap();
        writeln!(out, "| queries measured | {} |", self.answer_latency.n_queries).unwrap();
        writeln!(out, "| avg `server.answer(&queries)` latency | {:.4} ms |", self.answer_latency.avg_ms).unwrap();
        writeln!(out).unwrap();

        // ── 6. Headline ───────────────────────────────────────────────
        writeln!(out, "## 6. The headline: full rebuild ÷ per-block patch (K≈{})", self.config.headline_k).unwrap();
        writeln!(out).unwrap();
        writeln!(
            out,
            "Duty cycle assumes a {:.0} s block (`docs/plan.md` §7's framing — the honest measured \
             ratio, not the brief's 10^5–10^6).",
            self.config.block_time_secs
        )
        .unwrap();
        writeln!(out).unwrap();
        writeln!(
            out,
            "| accounts | full rebuild | per-block patch (K≈{}) | ratio (rebuild ÷ patch) | duty cycle @ {:.0}s block |",
            self.config.headline_k, self.config.block_time_secs
        )
        .unwrap();
        writeln!(out, "|---:|---:|---:|---:|---:|").unwrap();
        for s in &self.scales {
            let rebuild_secs = s.rebuild.as_secs_f64();
            let patch_secs = s.headline_patch_ms / 1000.0;
            let ratio = rebuild_secs / patch_secs.max(1e-9);
            let duty_pct = s.headline_patch_ms / (self.config.block_time_secs * 1000.0) * 100.0;
            writeln!(
                out,
                "| {} | {:.3} s | {:.4} ms | {:.0}× | {:.4}% |",
                fmt_num(s.accounts),
                rebuild_secs,
                s.headline_patch_ms,
                ratio,
                duty_pct,
            )
            .unwrap();
        }

        out
    }
}
