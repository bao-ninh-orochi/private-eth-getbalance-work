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
use segmented_cuckoo::{Segmented2aryCuckooKVStore, Segmented2aryScheme};

/// This deployment's fixed SCF knobs — arity-2 SCF, mirroring
/// `xtask::conformance` and every other crate's shared test geometry.
const ARITY: u32 = 2;
const BUCKET_SIZE: u32 = 4;
pub(crate) const FINGERPRINT_BITS: u32 = 32;
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

// ─── §7: the complete mainnet set — fixed historical citations ───────────
//
// `docs/numbers.md` §1–§6 (rendered from a fresh `run()` sweep, see
// `BenchReport::to_markdown`) currently stop at 9,437,184 accounts on this
// laptop. The live deployment serves `DEPLOYMENT_ACCOUNTS` — far larger.
// As of the 2026-09-03 measurement campaign (issue #4), the complete-set
// figures §1 sweeps at bench scale — full-rebuild/setup time, per-block
// patch time, answer latency — are directly MEASURED at deployment scale
// too, on the deployment host itself, and reported with full n/mean/p50/p95
// statistics in `docs/deployment-numbers.md`. This module quotes none of
// those timing figures — they belong to that report, which alone can keep
// them in sync — and instead points there. §7 (`complete_set_markdown`,
// below) also keeps, as a dated and explicitly superseded method record, a
// separate laptop run from before the campaign existed that extended this
// harness's own scales far enough to fit a defensible *extrapolation* of
// the §6 headline ratio — useful history for how this repo estimated the
// answer before it could measure it directly.
//
// Most figures below are one-off historical measurements with no other
// source of truth (Run B's scale extension, the same-machine control) —
// those are pinned as named constants, never re-derived at render time,
// exactly as before.
// `PUBLISHED_TOP_SCALE_ACCOUNTS`/`PUBLISHED_MID_SCALE_ACCOUNTS` and their
// three `PUBLISHED_*` siblings are different: they used to be hand-typed
// copies of whatever §1/§5/§6 happened to show, which only stayed correct
// as long as a human remembered to update them after every `--write` —
// they didn't, twice, which is why `complete_set_markdown` now takes the
// `BenchReport` being rendered and quotes that report's own measured
// values directly wherever the report reaches the relevant scale (see
// `resolve_top_scale_figures`/`resolve_mid_scale_latency`), falling back
// to these constants only when it does not. See each constant's own docs
// for which role it plays — self-describing anchor, or fallback-only, or
// (for the reproducibility note and the same-machine control's
// machine-state facts) a fixed citation that must never self-describe,
// because those two specifically compare one `(arity 3, bucket_size 4)`
// measurement against another on purpose.

/// The complete mainnet nonzero-balance account count the live deployment
/// serves (`GET /mode` = 1) — `risepir_store_items`, measured 2026-09-03 on
/// the deployment host (GCP `c3d-highmem-16`, `us-east4-a`) at block
/// 25,892,719, as the measurement campaign's starting count (issue #4,
/// `docs/deployment-numbers.md`).
///
/// Lineage: the previous **201,059,658** was the 2026-07-31 round
/// (`docs/deploy.md` §5.8); a 2026-08-19 re-bootstrap this repo's docs did
/// not record at the time moved it to 203,879,841 (`docs/deploy.md`
/// §5.11, loaded on the migrated `risepir-c3d` host); this campaign's own
/// count supersedes both (superseded for the deployment by
/// `docs/deployment-numbers.md`, 2026-09-03).
///
/// The geometry is unchanged by the growth: `Geometry::for_accounts` still
/// lands on the same 67,108,864 buckets and `plaintext_bits` 8, so every
/// size in §4 is identical and only the load factor moves (0.7490 → 0.7626).
const DEPLOYMENT_ACCOUNTS: u64 = 204_714_034;

/// `BenchConfig::default().scales`'s last (largest) entry — the operating
/// point §1/§6's "honest summary" wants to talk about. Doubles as: (a) the
/// key `resolve_top_scale_figures` looks up in the *current* report's own
/// `scales` — if present, §7 quotes that report's own measured rebuild
/// time and headline ratio at this exact scale, never a hand-maintained
/// copy; if absent (a tiny test config, or a future `--scales` run that
/// stops short), the "honest summary" paragraph falls back to
/// `PUBLISHED_REBUILD_SECS_AT_TOP_SCALE`/`PUBLISHED_HEADLINE_RATIO_AT_TOP_SCALE`
/// and says so explicitly; and (b) the fixed scale the reproducibility
/// note and the same-machine control's machine-state facts cite
/// unconditionally (see those paragraphs, and `PUBLISHED_REBUILD_SECS_AT_TOP_SCALE`'s
/// docs, for why those two never self-describe).
const PUBLISHED_TOP_SCALE_ACCOUNTS: u64 = 9_437_184;

/// `BenchConfig::default().mid_scale` — the sibling of
/// `PUBLISHED_TOP_SCALE_ACCOUNTS` for §5's answer latency.
/// `resolve_mid_scale_latency` compares the *current* report's own
/// `answer_latency.accounts` against this to decide whether §7 can quote
/// that report's own §5 latency directly (self-describing) or must fall
/// back to `PUBLISHED_ANSWER_LATENCY_MS_AT_MID_SCALE`.
const PUBLISHED_MID_SCALE_ACCOUNTS: u64 = 1_000_000;

/// FALLBACK ONLY for the "honest summary" paragraph, via
/// `resolve_top_scale_figures`: the last-known committed `docs/numbers.md`'s
/// full-rebuild time at `PUBLISHED_TOP_SCALE_ACCOUNTS` (§1), pinned
/// 2026-07-22, pre-ADR-0034 `(arity 3, bucket_size 4)`. Used only when the
/// report being rendered does not itself reach `PUBLISHED_TOP_SCALE_ACCOUNTS`
/// — a real `BenchConfig::default()` run always does, so it always
/// self-describes instead and this constant never enters that render at
/// all. Not "the current number" and nobody needs to remember to update
/// it for that path to stay correct.
///
/// Separately, this same constant is quoted *unconditionally* (never
/// self-describing) in the reproducibility note and in the same-machine
/// control's "machine is still slow" fact, where it is compared directly
/// against other fixed `(arity 3, bucket_size 4)` measurements (Run
/// A/Run B, `TODAY_CONTROL_ARITY3_SCALES`) to show how much this laptop's
/// raw timings drift run to run. That comparison is only mathematically
/// meaningful `(3,4)`-vs-`(3,4)`: swapping in a self-describing value
/// there would silently start comparing across geometries the moment the
/// deployed geometry moves again (as it already has, arity 3 → 2), and
/// could turn "uniformly slower" into a nonsense number rather than a
/// stale label — a worse failure than the one this constant used to have.
const PUBLISHED_REBUILD_SECS_AT_TOP_SCALE: f64 = 6.677;

/// FALLBACK ONLY / unconditional-citation constant — the `(arity 3,
/// bucket_size 4)`, 2026-07-22 headline ratio (§6) at
/// `PUBLISHED_TOP_SCALE_ACCOUNTS`. Same two roles as
/// `PUBLISHED_REBUILD_SECS_AT_TOP_SCALE` — see its docs: fallback for the
/// "honest summary" paragraph via `resolve_top_scale_figures` when the
/// report doesn't reach this scale itself; unconditional fixed citation in
/// the same-machine control's ratio-robustness fact, which exists
/// specifically to show a `(3,4)` control ratio landing close to *this*
/// `(3,4)` figure — self-describing there would compare it against
/// whatever the current report's geometry happens to be instead, breaking
/// the fact's own arithmetic the moment that geometry isn't `(3,4)`.
const PUBLISHED_HEADLINE_RATIO_AT_TOP_SCALE: u64 = 1_346;

/// FALLBACK ONLY / unconditional-citation constant — the `(arity 3,
/// bucket_size 4)`, 2026-07-22 answer latency, in milliseconds, at
/// `PUBLISHED_MID_SCALE_ACCOUNTS` (§5). Same two roles as
/// `PUBLISHED_REBUILD_SECS_AT_TOP_SCALE` — see its docs: fallback for
/// `resolve_mid_scale_latency` when the report's own `mid_scale` isn't
/// `PUBLISHED_MID_SCALE_ACCOUNTS`; unconditional fixed `(3,4)` citation in
/// the reproducibility note and the same-machine control's "machine is
/// still slow" fact, for the same cross-geometry reason.
const PUBLISHED_ANSWER_LATENCY_MS_AT_MID_SCALE: f64 = 2.6845;

/// This laptop's own re-measurement of
/// `PUBLISHED_ANSWER_LATENCY_MS_AT_MID_SCALE`, from the same uncontaminated
/// 2026-07-27 run as `RUN_B_LARGEST_SCALES` ("Run B") — 1,000,000 accounts
/// is not one of that table's three *largest* scales, so it is not part of
/// it, but is quoted alongside for the same reproducibility note. Same
/// pre-ADR-0034 `(arity 3, bucket_size 4)` lineage as `RUN_B_LARGEST_SCALES`.
const RUN_B_ANSWER_LATENCY_MS_AT_MID_SCALE: f64 = 5.5713;

/// Run B — this laptop, 2026-07-27, nothing of this project's own
/// competing for CPU (contrast Run A, contaminated by competing cargo
/// builds and not quoted here) — full-rebuild time, headline-`K`(≈300)
/// patch time, and their ratio, at the three largest scales this
/// worktree's own `--scales`/`--mid-scale` `xtask bench` flags reached.
/// Pre-ADR-0034 `(arity 3, bucket_size 4)` lineage: this run predates the
/// arity retune, back when this module's `ARITY` constant was still 3 —
/// not the now-deployed `(arity 2, bucket_size 4)` geometry §1–§6 measure.
/// `(accounts, full_rebuild_secs, headline_patch_ms, ratio)`. A one-off
/// historical measurement, not reproduced by `BenchConfig::default()`.
const RUN_B_LARGEST_SCALES: [(u64, f64, f64, u64); 3] = [
    (9_437_184, 10.559, 6.7172, 1_572),
    (18_874_368, 23.503, 7.2875, 3_225),
    (37_748_736, 72.410, 14.6611, 4_939),
];

/// EXTRAPOLATION, not a measurement: the empirical exponent of Run B's
/// ratio-vs-`N` growth between `RUN_B_LARGEST_SCALES`'s smallest and
/// largest entries — ratio grows `1572 -> 4939` as `N` grows `9,437,184 ->
/// 37,748,736` (exactly 4x), i.e. `log(4939 / 1572) / log(4) ~ 0.83`:
/// ratio grows roughly as `N^0.83`. Inherits `RUN_B_LARGEST_SCALES`'s
/// pre-ADR-0034 `(arity 3, bucket_size 4)` lineage — it is a property of
/// that run, not re-derived here. Used only to extend the *ratio* (§6's
/// headline) to deployment scale — never applied to either raw time
/// alone, since only the ratio is (to first order) machine-independent;
/// see `complete_set_markdown`.
const RUN_B_RATIO_GROWTH_EXPONENT: f64 = 0.83;

/// EXTRAPOLATION, not a measurement: `RUN_B_RATIO_GROWTH_EXPONENT`
/// extended from Run B's largest scale (37,748,736) to `DEPLOYMENT_ACCOUNTS`
/// (a further 5.42x in `N`) — `4939 * 5.42^0.83 ~ 2.0e4`. On the order of
/// 10^4; not a precise figure, and itself superseded now that
/// `docs/deployment-numbers.md` (2026-09-03) measures the deployment's
/// actual per-block apply time directly — see `complete_set_markdown`'s
/// honest summary text.
const EXTRAPOLATED_COMPLETE_SET_RATIO: f64 = 2.0e4;

// ─── §7: same-machine (2,4) vs (3,4) control — ADR-0034 follow-up ───────
//
// Everything above (`RUN_B_*`, `PUBLISHED_*`) predates the arity retune
// from `(3,4)` to `(2,4)`
// (ADR-0034) and answers "how does this laptop's measured ratio extend to
// deployment scale". The four constants below answer a different
// question, made necessary *by* that retune: now that §1–§6 are freshly
// committed at a new geometry *and* a new day, how much of the change from
// the old 2026-07-22 `(3,4)` table is the geometry, and how much is just
// this laptop being slower today than it was then? They come from two
// runs on 2026-07-27, back to back, otherwise idle, at identical scales
// (`BenchConfig::default()`'s own 100,000 / 1,000,000 / 9,437,184) —
// `TODAY_CONTROL_ARITY2_SCALES` from this committed `bench.rs`,
// `TODAY_CONTROL_ARITY3_SCALES` immediately afterward from the pre-change
// `bench.rs` (this module's `ARITY` constant still 3) — so the *only*
// thing that differs between the two runs is the geometry, not the
// machine. See `complete_set_markdown`'s reproducibility note for how
// they're used.

/// Same-machine control — `(arity 2, bucket_size 4)`, 2026-07-27: this
/// committed, post-ADR-0034 `bench.rs`, run at `BenchConfig::default()`'s
/// three scales on an otherwise-idle machine, immediately before
/// `TODAY_CONTROL_ARITY3_SCALES` below. A one-off historical measurement,
/// not reproduced by `BenchConfig::default()` (which now *is* this
/// geometry, so a future run's own §1/§6 should land close to this row,
/// machine state permitting — a prediction this module does not itself
/// check). `(accounts, full_rebuild_secs, headline_patch_ms, ratio)`.
const TODAY_CONTROL_ARITY2_SCALES: [(u64, f64, f64, u64); 3] = [
    (100_000, 0.037, 2.4493, 15),
    (1_000_000, 0.973, 4.6408, 210),
    (9_437_184, 12.797, 5.2321, 2_446),
];

/// This control's own answer latency at `BenchConfig::default().mid_scale`
/// (1,000,000 accounts, §5) — quoted alongside `TODAY_CONTROL_ARITY2_SCALES`
/// for the same reproducibility note; kept separate for the same reason
/// `RUN_B_ANSWER_LATENCY_MS_AT_MID_SCALE` is: 1,000,000 is this run's
/// *middle*, not *largest*, scale.
const TODAY_CONTROL_ARITY2_ANSWER_LATENCY_MS_AT_MID_SCALE: f64 = 5.5062;

/// Same-machine control — `(arity 3, bucket_size 4)`, 2026-07-27: the
/// pre-ADR-0034 `bench.rs` (this module's `ARITY` constant still 3), run
/// immediately after `TODAY_CONTROL_ARITY2_SCALES` at the identical three
/// scales, same otherwise-idle machine, same sitting. Comparing this to
/// `TODAY_CONTROL_ARITY2_SCALES` holds the machine fixed and varies only
/// `(arity, bucket_size)`; comparing it instead to the *published*
/// (2026-07-22) `(3,4)` figures holds the geometry fixed and varies only
/// machine state — the two comparisons this control exists to make
/// possible. Same pre-ADR-0034 `(arity 3, bucket_size 4)` lineage as
/// `RUN_B_LARGEST_SCALES`, but a distinct run — three scales matching
/// `BenchConfig::default()`, not `RUN_B`'s largest-three-past-published.
/// `(accounts, full_rebuild_secs, headline_patch_ms, ratio)`.
const TODAY_CONTROL_ARITY3_SCALES: [(u64, f64, f64, u64); 3] = [
    (100_000, 0.066, 2.8398, 23),
    (1_000_000, 0.569, 4.8994, 116),
    (9_437_184, 8.984, 6.8111, 1_319),
];

/// This control's own answer latency at 1,000,000 accounts — the `(3,4)`
/// sibling of `TODAY_CONTROL_ARITY2_ANSWER_LATENCY_MS_AT_MID_SCALE`, same
/// run as `TODAY_CONTROL_ARITY3_SCALES`.
const TODAY_CONTROL_ARITY3_ANSWER_LATENCY_MS_AT_MID_SCALE: f64 = 4.3058;

type Server = RisePirServer<Segmented2aryScheme, SimplePirBackend>;
type Client = RisePirClient<SimplePirBackend>;

/// This deployment's [`ValueCodec`] — ADR-0009's `key_tag(32) ‖
/// balance(96) ‖ checksum(16)` = 144-bit value.
///
/// `pub(crate)` so `crate::geometry` (ADR-0030) shares this exact codec
/// rather than redefining a second copy of the same three literals.
pub(crate) fn value_codec() -> ValueCodec {
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
    /// The real Stage 3 gate: 100K / 1M / ~9.4M account scales. The third
    /// (9,437,184) predates the deployed geometry: it was originally chosen
    /// (pre-ADR-0034) as the arity-3 `segment_rows = 2^20`'s exact
    /// 75%-load account count, the same operating point
    /// `docs/verification.md` Correction 4 and `docs/plan.md` §7 already
    /// report numbers at. Kept unchanged — not re-picked for the deployed
    /// geometry — so this run's numbers stay directly comparable to those,
    /// and to the prior run this repo already measured. Under the
    /// now-deployed `(arity 2, bucket_size 4)` geometry (ADR-0034) the same
    /// account count instead lands at `num_buckets = 2^22` (`segment_rows =
    /// 2^21`), load 0.5625 — verified via `Geometry::for_accounts`, not a
    /// property of `segment_rows = 2^20` any more. The full Correction-4 `K`
    /// curve runs at 1M, and `lwe_dim = 1275` (`SimpleConfig::default()`,
    /// applied in `build_scale`).
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
/// geometry/config guarantees to succeed (store construction at this
/// configuration's own sized target load —
/// `risepir_proto::geometry::effective_target_load`, no longer a flat 75%
/// since ADR-0034 — genesis insertion, `apply_block` under the
/// insert/delete-free `K`-update workload) fails regardless — a failure
/// here means the harness itself is misconfigured, not that it hit a
/// real, expected error condition.
pub fn run(cfg: &BenchConfig) -> BenchReport {
    assert!(
        !cfg.scales.is_empty(),
        "bench: cfg.scales must not be empty"
    );
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
            if let Some(reason) =
                unsafe_to_attempt_top_scale(&scale_reports[i - 1], accounts, &codec)
            {
                let prev_accounts = scale_reports[i - 1].accounts;
                let mut candidate = accounts;
                loop {
                    let next = (candidate / 2).max(prev_accounts);
                    if next == candidate {
                        break;
                    }
                    candidate = next;
                    if candidate <= prev_accounts
                        || unsafe_to_attempt_top_scale(&scale_reports[i - 1], candidate, &codec)
                            .is_none()
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
                    measure_patch_at_k(
                        &mut build.server,
                        &mut build.feed,
                        k,
                        cfg.warmup_blocks,
                        cfg.measured_blocks,
                    )
                    .0
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

            let avg_ms =
                measure_answer_latency(&build.server, &build.feed, &codec, cfg.measured_queries);
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
        delta_bytes: delta_bytes
            .expect("mid_scale is always in cfg.scales (asserted above), so it always runs"),
        answer_latency: answer_latency
            .expect("mid_scale is always in cfg.scales (asserted above), so it always runs"),
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
/// [`Segmented2aryCuckooKVStore`] from its `snapshot()`, and TIMES
/// `RisePirServer::new(..)` — the full rebuild (item 1). The feed is
/// configured for insert/delete-free churn (`inserts_per_block =
/// deletes_per_block = 0`) so every subsequent `next_block()` call is pure
/// updates to already-live accounts (see the module docs).
fn build_scale(
    accounts: u64,
    seed: u64,
    max_changes_per_block: usize,
    codec: ValueCodec,
) -> ScaleBuild {
    let geometry = Geometry::for_accounts(
        accounts,
        ARITY,
        BUCKET_SIZE,
        FINGERPRINT_BITS,
        &codec,
        Backend::Simple,
    )
    .unwrap_or_else(|e| panic!("bench: geometry for {accounts} accounts: {e}"));

    let feed = MockFeed::new(MockConfig {
        seed,
        num_genesis_keys: accounts,
        changes_per_block: max_changes_per_block,
        inserts_per_block: 0,
        deletes_per_block: 0,
    });

    let mut store = Segmented2aryCuckooKVStore::new(
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
                "bench: genesis insert at {accounts} accounts hit {e:?} (geometry is sized to stay \
                 within this configuration's own target load; this should not happen)"
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
    BlockUpdate {
        block,
        changes,
        credits: vec![],
    }
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
        server.apply_block(&update).expect(
            "bench: apply_block under the insert/delete-free K-update workload must not fail",
        );
    }

    let mut total = Duration::ZERO;
    let mut last_delta = None;
    for _ in 0..measured_blocks {
        let update = sliced_block(feed, k);
        let t0 = Instant::now();
        let delta = server.apply_block(&update).expect(
            "bench: apply_block under the insert/delete-free K-update workload must not fail",
        );
        total += t0.elapsed();
        last_delta = Some(delta);
    }

    let avg_ms = total.as_secs_f64() * 1000.0 / measured_blocks as f64;
    (
        avg_ms,
        last_delta.expect("measured_blocks is always >= 1 in this module's configs"),
    )
}

/// Builds a real [`RisePirClient`] from `server.setup()` and times
/// `server.answer(&queries)` (only — never `client.finish`, per the
/// brief) over `n_queries` real, live keys, returning the average in
/// milliseconds.
fn measure_answer_latency(
    server: &Server,
    feed: &MockFeed,
    codec: &ValueCodec,
    n_queries: usize,
) -> f64 {
    let mut client: Client = RisePirClient::from_setup(server.setup(), *codec);
    let live = feed.live_keys();
    assert!(
        !live.is_empty(),
        "bench: a scale's live set must be non-empty"
    );

    let mut rng = Xorshift64(0xA5A5_5A5A_1234_5678);
    let mut total = Duration::ZERO;
    for _ in 0..n_queries {
        let idx = (rng.next() as usize) % live.len();
        let (queries, _ctx) = client.build_query(&live[idx]);
        let t0 = Instant::now();
        let _ = server
            .answer(&queries)
            .expect("bench: answer must succeed for a well-formed query");
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
fn unsafe_to_attempt_top_scale(
    prev: &ScaleReport,
    candidate: u64,
    codec: &ValueCodec,
) -> Option<String> {
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
    let geometry = Geometry::for_accounts(
        accounts,
        ARITY,
        BUCKET_SIZE,
        FINGERPRINT_BITS,
        codec,
        Backend::Simple,
    )
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
pub(crate) fn fmt_num(n: u64) -> String {
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
/// `"14.57 MB (14,572,800 B)"`. **Decimal (SI)** units — `KB`/`MB`/`GB`/`TB`
/// mean exactly `1e3`/`1e6`/`1e9`/`1e12` bytes — matching `xtask geometry`'s
/// `db_GB`/`hint_MB`/`cmem_GB` columns (`server_db as f64 / 1e9` etc.),
/// `docs/deploy.md`, `CLAUDE.md`, and every ADR that cites a size in this
/// repo. This function used to divide by `1024` (binary units under a
/// decimal label), which silently disagreed with all of those — e.g. it
/// would have printed `22.00 GB` for the same 23,622,320,128 B that
/// `xtask geometry`'s own doc comment already calls `23.62 GB`. Never
/// binary again: two different numbers for one byte count, in one repo,
/// is exactly the failure mode `CLAUDE.md`'s "never return a wrong answer"
/// discipline exists to prevent, even when (as here) both numbers were
/// individually correct arithmetic under their own unlabelled convention.
/// The exact byte count in parentheses is unaffected either way — only the
/// leading human-readable figure moves.
///
/// `pub(crate)`, alongside [`fmt_num`] and [`value_codec`], so `xtask
/// report` (`crate::report`) can reuse the exact same size formatting and
/// fixed value-codec layout rather than risking a second, silently
/// drifting copy of either.
pub(crate) fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut unit = 0usize;
    while v >= 1000.0 && unit + 1 < UNITS.len() {
        v /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} B", fmt_num(bytes))
    } else {
        format!("{v:.2} {} ({} B)", UNITS[unit], fmt_num(bytes))
    }
}

// ─── §7: the complete mainnet set ─────────────────────────────────────────

/// The "honest summary" paragraph's top-scale figures: `resolve_top_scale_figures`
/// prefers the *report being rendered*'s own measurement at
/// `PUBLISHED_TOP_SCALE_ACCOUNTS`, so that paragraph can never drift from
/// what this same file's own §1/§6 show. `self_describing` is `false`
/// only in the fallback case (`rebuild_secs`/`ratio` are then the frozen
/// `PUBLISHED_*` constants instead) — the "honest summary" paragraph reads
/// it to say so explicitly rather than silently.
struct TopScaleFigures {
    rebuild_secs: f64,
    ratio: u64,
    self_describing: bool,
}

/// Looks up `PUBLISHED_TOP_SCALE_ACCOUNTS` in `report.scales`. If present
/// (every `BenchConfig::default()` run — the common case), computes the
/// rebuild-time-÷-patch-time ratio the exact same way §6's own table does,
/// so the "honest summary" paragraph's number is bit-for-bit what a reader
/// checking §6 would compute themselves. If absent (a tiny test config, or
/// a future `--scales` run that stops short of this scale), falls back to
/// the frozen constants and flags `self_describing: false`.
fn resolve_top_scale_figures(report: &BenchReport) -> TopScaleFigures {
    match report
        .scales
        .iter()
        .find(|s| s.accounts == PUBLISHED_TOP_SCALE_ACCOUNTS)
    {
        Some(s) => {
            let rebuild_secs = s.rebuild.as_secs_f64();
            let patch_secs = s.headline_patch_ms / 1000.0;
            let ratio = (rebuild_secs / patch_secs.max(1e-9)).round() as u64;
            TopScaleFigures {
                rebuild_secs,
                ratio,
                self_describing: true,
            }
        }
        None => TopScaleFigures {
            rebuild_secs: PUBLISHED_REBUILD_SECS_AT_TOP_SCALE,
            ratio: PUBLISHED_HEADLINE_RATIO_AT_TOP_SCALE,
            self_describing: false,
        },
    }
}

/// The mid-scale sibling of [`TopScaleFigures`] — see
/// `resolve_mid_scale_latency`.
struct MidScaleLatency {
    ms: f64,
    self_describing: bool,
}

/// Prefers `report.answer_latency` (always measured at `report.config.mid_scale`)
/// when that scale is `PUBLISHED_MID_SCALE_ACCOUNTS` (every
/// `BenchConfig::default()` run); otherwise falls back to
/// `PUBLISHED_ANSWER_LATENCY_MS_AT_MID_SCALE` and flags
/// `self_describing: false`.
fn resolve_mid_scale_latency(report: &BenchReport) -> MidScaleLatency {
    if report.answer_latency.accounts == PUBLISHED_MID_SCALE_ACCOUNTS {
        MidScaleLatency {
            ms: report.answer_latency.avg_ms,
            self_describing: true,
        }
    } else {
        MidScaleLatency {
            ms: PUBLISHED_ANSWER_LATENCY_MS_AT_MID_SCALE,
            self_describing: false,
        }
    }
}

/// Renders `docs/numbers.md` §7, "The complete mainnet set", for the
/// report actually being written. Most figures here are still either a
/// fixed historical measurement (this laptop's Run B sweep past the
/// published top scale), a plain computed geometry size (`Geometry::sizes`
/// at `DEPLOYMENT_ACCOUNTS` under this module's own fixed `ARITY`/
/// `BUCKET_SIZE` — deterministic, not timed, exactly like §4a/4b/4c above),
/// or an explicitly labelled extrapolation from those — never a fresh
/// *timing* this function takes itself. Every complete-set timing — setup
/// time, per-block apply time, answer compute, each with full
/// n/mean/p50/p95 statistics — is measured by the campaign directly and
/// reported in `docs/deployment-numbers.md`; this function quotes none of
/// them and points there instead. The exception, deliberately: the
/// "honest summary" paragraph's top-scale rebuild time and headline ratio,
/// which come from `report` itself whenever it reaches
/// `PUBLISHED_TOP_SCALE_ACCOUNTS` (see `resolve_top_scale_figures`) — so
/// that paragraph is guaranteed to agree with whatever §1/§6 actually show
/// for *this* render, rather than needing a human to keep two copies in
/// sync (the bug this function used to have, twice). The reproducibility
/// note and the same-machine control's machine-state facts, below,
/// deliberately do *not* follow that pattern — see
/// `PUBLISHED_REBUILD_SECS_AT_TOP_SCALE`'s docs for why those two must
/// keep citing the fixed `(arity 3, bucket_size 4)` constants
/// unconditionally instead.
fn complete_set_markdown(report: &BenchReport) -> String {
    let mut out = String::new();
    let codec = value_codec();
    let top_figs = resolve_top_scale_figures(report);
    let mid_lat = resolve_mid_scale_latency(report);

    writeln!(
        out,
        "## 7. The complete mainnet set ({} accounts)",
        fmt_num(DEPLOYMENT_ACCOUNTS)
    )
    .unwrap();
    writeln!(out).unwrap();
    // "This file's largest bench scale" is always `report.reached_top_scale`
    // — whatever this exact render actually reached — never
    // `PUBLISHED_TOP_SCALE_ACCOUNTS`, so this sentence cannot itself go
    // stale the way the "honest summary" paragraph used to.
    writeln!(
        out,
        "The live deployment serves the complete nonzero-balance mainnet set — {:.0}x larger than \
         this file's largest bench scale ({} accounts, §1–§6), which this laptop cannot build a \
         server at (§4b). As of the 2026-09-03 measurement campaign (issue #4), the complete set's \
         own numbers — per-block apply time, answer compute, setup time, and sizes, each with \
         n/mean/p50/p95 — are MEASURED directly on the deployment host and reported in \
         `docs/deployment-numbers.md`; that file is the one to cite for the deployment's actual \
         behaviour — this section quotes none of those figures itself. It instead keeps — dated and \
         explicitly labelled superseded — the extrapolation method this repo used to estimate the \
         answer before it could measure it directly. §1–§6 above are \
         this run's own fresh measurements, never a value anyone hand-maintains to match them — see \
         the reproducibility note below for how much run-to-run machine variance to expect before \
         comparing them against any other figure this section cites.",
        DEPLOYMENT_ACCOUNTS as f64 / report.reached_top_scale as f64,
        fmt_num(report.reached_top_scale),
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(
        out,
        "**What is measured, and where.** The complete set's one-time full PIR-setup rebuild — \
         exactly §1's quantity, at deployment scale rather than inferred — is measured on the \
         deployment host (GCP `c3d-highmem-16`, `us-east4-a`), at the *same* `(arity 2, bucket_size \
         4)` geometry §1–§6 above measure, via `risepir-rpc time-setup` (C13) run against the served \
         store — the same `server_setup` the bootstrap itself runs, not a different code path. So is \
         every other complete-set figure this file used to estimate — per-block apply time above \
         all — measured the same rigorous way, with full n/mean/p50/p95 statistics. None of those \
         figures are quoted here: see `docs/deployment-numbers.md` for the actual numbers."
    )
    .unwrap();
    writeln!(out).unwrap();

    let deployed_db_bytes = Geometry::for_accounts(
        DEPLOYMENT_ACCOUNTS,
        ARITY,
        BUCKET_SIZE,
        FINGERPRINT_BITS,
        &codec,
        Backend::Simple,
    )
    .expect(
        "complete_set_markdown: the deployed (arity, bucket_size) must size the live account count",
    )
    .sizes(Backend::Simple, DEPLOYMENT_ACCOUNTS)
    .server_db;

    writeln!(
        out,
        "**Why §6 still has no {accounts}-account row.** This bench harness cannot build a server at \
         deployment scale on this laptop — the deployed `(arity 2, bucket_size 4)` geometry alone is \
         a {db_gb:.2} GB server DB (§4b) — and the deployment box is a production server, not a \
         benchmark rig, so it has never run this harness's warm-up/measured-block protocol either. \
         §6 above therefore still has no {accounts}-account row, and never will from this machine. \
         That used to mean per-block patch time at the complete set was unmeasured anywhere; it no \
         longer does — `docs/deployment-numbers.md` carries the campaign's own measured per-block \
         apply time, taken on the deployment host itself rather than extrapolated from this laptop.",
        db_gb = deployed_db_bytes as f64 / 1e9,
        accounts = fmt_num(DEPLOYMENT_ACCOUNTS),
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(
        out,
        "**EXTRAPOLATION (2026-07-27 Run B, arity-3 lineage) — superseded by the measured deployment \
         figures.** The remainder of this section through \"Honest summary\" below is a dated method \
         record: how this repo estimated the deployment's headline ratio *before* the 2026-09-03 \
         measurement campaign made a direct measurement possible. It is retained for provenance, not \
         as a current citation — see `docs/deployment-numbers.md` for the measured figures."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(
        out,
        "**What the trend shows.** A separate run on 2026-07-27 — \"Run B\" below, uncontaminated by \
         competing builds, *not* the run behind §1–§6 — extended this harness past {top} accounts with \
         this worktree's own `xtask bench --scales <n,n,...> --mid-scale <n>` flags, under the \
         pre-ADR-0034 `(arity 3, bucket_size 4)` lineage this harness ran at the time (its `ARITY` \
         constant has since moved to 2) — not the now-deployed `(arity 2, bucket_size 4)` geometry \
         §1–§6 above measure. Its three largest points:",
        top = fmt_num(PUBLISHED_TOP_SCALE_ACCOUNTS),
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "| accounts | full rebuild | per-block patch (K≈300) | ratio (rebuild ÷ patch) |"
    )
    .unwrap();
    writeln!(out, "|---:|---:|---:|---:|").unwrap();
    for &(accounts, rebuild_secs, patch_ms, ratio) in &RUN_B_LARGEST_SCALES {
        writeln!(
            out,
            "| {} | {rebuild_secs:.3} s | {patch_ms:.4} ms | {ratio}× |",
            fmt_num(accounts)
        )
        .unwrap();
    }
    writeln!(out).unwrap();
    writeln!(
        out,
        "Per-block patch time is *not* holding flat here — it grows from single-digit to ~15 ms across \
         this range, the cache-plateau effect `docs/verification.md` Correction 4 already names, still \
         climbing at these scales. §6's implicit hope that patch time stays near ~5 ms all the way to \
         {} accounts is not supported by this trend.",
        fmt_num(DEPLOYMENT_ACCOUNTS),
    )
    .unwrap();
    writeln!(out).unwrap();

    let (n0, _, _, r0) = RUN_B_LARGEST_SCALES[0];
    let (n2, _, _, r2) = RUN_B_LARGEST_SCALES[2];
    let n_growth = n2 as f64 / n0 as f64;
    let ratio_growth = r2 as f64 / r0 as f64;
    let n_extension = DEPLOYMENT_ACCOUNTS as f64 / n2 as f64;

    writeln!(
        out,
        "**The extrapolated ratio — EXTRAPOLATION, not a measurement.** The ratio itself, unlike \
         either time alone, is to first order machine-independent: numerator and denominator both \
         scale with this machine's own CPU speed, so a uniform machine slowdown (see the \
         reproducibility note below) largely cancels out of their quotient. That is what makes \
         extrapolating the *ratio* from Run B defensible where extrapolating either raw time alone \
         would not be:"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "- Run B's ratio grows {r0}× → {r2}× from {} to {} accounts — a {n_growth:.0}× increase in N \
         producing a {ratio_growth:.2}× increase in ratio, i.e. ratio ∝ N^{RUN_B_RATIO_GROWTH_EXPONENT}.",
        fmt_num(n0),
        fmt_num(n2),
    )
    .unwrap();
    writeln!(
        out,
        "- Extending that exponent from {} to the deployment's {} (a {n_extension:.2}× further \
         increase in N) gives ratio ≈ {r2} × {n_extension:.2}^{RUN_B_RATIO_GROWTH_EXPONENT} ≈ \
         EXTRAPOLATION **2 × 10^4** (on the order of 10^4).",
        fmt_num(n2),
        fmt_num(DEPLOYMENT_ACCOUNTS),
    )
    .unwrap();
    writeln!(
        out,
        "- Run A (the contaminated run, not tabulated above) fits the same way to a smaller \
         extrapolated ratio — consistent with treating this as an order-of-magnitude statement, not a \
         precise one."
    )
    .unwrap();
    writeln!(
        out,
        "- A cross-check against a real deployment figure used to sit here, built from the \
         pre-ADR-0034 host's rebuild time — removed now that a direct measurement exists: see \
         `docs/deployment-numbers.md` for the campaign's own measured per-block apply time at {} \
         accounts, not a figure implied by this extrapolation.",
        fmt_num(DEPLOYMENT_ACCOUNTS),
    )
    .unwrap();
    writeln!(out).unwrap();

    // Self-describing when `top_figs.self_describing` (the common case: a
    // real `BenchConfig::default()` run) — this is the exact paragraph
    // that used to go stale against a freshly-regenerated §6, twice. The
    // gap is computed live rather than restated as a fixed "more than an
    // order of magnitude" because that qualitative claim is only true for
    // some ratios (the pre-ADR-0034 1,346× fallback) and not others (a
    // self-describing `(arity 2, bucket_size 4)` ratio can land under 10×
    // of `EXTRAPOLATED_COMPLETE_SET_RATIO`) — see `resolve_top_scale_figures`.
    let ratio_gap = EXTRAPOLATED_COMPLETE_SET_RATIO / top_figs.ratio as f64;
    let honest_summary_verb = if top_figs.self_describing {
        "this file publishes"
    } else {
        "the previously committed file published"
    };
    writeln!(
        out,
        "**Honest summary.** The {ratio}× {verb} for {top} accounts (§6) understates the argument \
         at deployment scale by roughly {ratio_gap:.0}×; a 10^5 claim (the original brief's \
         assumption) would overstate it. The defensible statement **at the time** was: on the order \
         of 10^4, and rising with N — an estimate. The 2026-09-03 measurement campaign has since \
         measured the real complete-set per-block apply time directly, so `docs/deployment-numbers.md` \
         carries the actual ratio now, not this extrapolation.",
        ratio = top_figs.ratio,
        verb = honest_summary_verb,
        top = fmt_num(PUBLISHED_TOP_SCALE_ACCOUNTS),
    )
    .unwrap();
    writeln!(out).unwrap();

    // Deliberately compares Run A/Run B (fixed, `(arity 3, bucket_size 4)`,
    // 2026-07-27) against `PUBLISHED_ANSWER_LATENCY_MS_AT_MID_SCALE`/
    // `PUBLISHED_REBUILD_SECS_AT_TOP_SCALE` *unconditionally* — never
    // `mid_lat`/`top_figs`'s self-describing values — because both fixed
    // constants are the same `(arity 3, bucket_size 4)` lineage as Run
    // A/Run B; a self-describing value could silently be a different
    // geometry (as it already is: `ARITY` is now 2), which would turn
    // "uniformly slower" into a meaningless cross-geometry comparison
    // rather than a measurement of machine state. See
    // `PUBLISHED_REBUILD_SECS_AT_TOP_SCALE`'s docs. What *is*
    // self-describing here is the closing aside: when this run also
    // reaches both scales, it quotes this run's own figures too, as a
    // third, independent illustration of the same variance.
    let self_describing_aside = if top_figs.self_describing && mid_lat.self_describing {
        format!(
            " This run's own figures at the same two scales — {:.3} s (§1) and {:.4} ms (§5) — are \
             a third, independent data point in that same run-to-run variance.",
            top_figs.rebuild_secs, mid_lat.ms,
        )
    } else {
        String::new()
    };
    writeln!(
        out,
        "**Reproducibility note.** Two historical runs on 2026-07-27 — Run A (contaminated by \
         competing cargo builds) and Run B (quoted above), both this module's pre-ADR-0034 `(arity \
         3, bucket_size 4)` — measured answer latency at {mid} accounts ({run_b_lat:.4} ms) and \
         full-rebuild time at {top} accounts ({run_b_rebuild:.3} s), uniformly slower than the \
         2026-07-22 `(3,4)` baseline this file used to publish ({pub_lat:.4} ms and {pub_rebuild:.3} \
         s respectively — fixed historical citations, not this file's own current §1/§5, which may \
         since be a different geometry and a different day).{aside} That gap is machine-state \
         variance on this laptop, not a code change — which is exactly why §1–§6 above are always \
         this run's own fresh measurements rather than a value anyone hand-maintains to match them: \
         comparing any two runs of this file is meaningful only in *shape* (the trend, the ratio), \
         never in absolute terms. See the same-machine control below for a same-day, same-machine \
         comparison that isolates the geometry's own effect from exactly this kind of variance.",
        mid = fmt_num(PUBLISHED_MID_SCALE_ACCOUNTS),
        run_b_lat = RUN_B_ANSWER_LATENCY_MS_AT_MID_SCALE,
        pub_lat = PUBLISHED_ANSWER_LATENCY_MS_AT_MID_SCALE,
        top = fmt_num(PUBLISHED_TOP_SCALE_ACCOUNTS),
        run_b_rebuild = RUN_B_LARGEST_SCALES[0].1,
        pub_rebuild = PUBLISHED_REBUILD_SECS_AT_TOP_SCALE,
        aside = self_describing_aside,
    )
    .unwrap();
    writeln!(out).unwrap();

    // Same-machine control (ADR-0034 follow-up): see
    // `TODAY_CONTROL_ARITY2_SCALES`/`TODAY_CONTROL_ARITY3_SCALES` for what
    // this is and why it exists. Every number below is either one of
    // those pinned constants or a fresh `Geometry` computation — never a
    // hardcoded literal — exactly like `deployed_db_bytes` above.
    writeln!(
        out,
        "**Same-machine control (2026-07-27).** §1–§6 above are this run's own fresh measurements at \
         the now-deployed `(arity 2, bucket_size 4)` geometry (ADR-0034) — a different geometry, and \
         very possibly a different day, from the 2026-07-22 `(arity 3, bucket_size 4)` table this \
         file used to publish before ADR-0034, so comparing the two naively conflates both changes \
         at once. To separate them, this laptop ran both configurations back to back, otherwise \
         idle, at `BenchConfig::default()`'s three scales — measured *before* the run that produced \
         §1–§6 above, so this control's own `(2,4)` column differs from whatever §1/§5/§6 actually \
         show by however much the machine's load changed between the two runs, not by a code or \
         geometry change: exactly the run-to-run variance this control exists to expose, not a \
         discrepancy to reconcile."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "| accounts | (2,4) rebuild | (2,4) patch (K≈300) | (2,4) ratio | (3,4) rebuild | (3,4) \
         patch (K≈300) | (3,4) ratio |"
    )
    .unwrap();
    writeln!(out, "|---:|---:|---:|---:|---:|---:|---:|").unwrap();
    for (&(a2, r2, p2, ratio2), &(a3, r3, p3, ratio3)) in TODAY_CONTROL_ARITY2_SCALES
        .iter()
        .zip(TODAY_CONTROL_ARITY3_SCALES.iter())
    {
        debug_assert_eq!(
            a2, a3,
            "the two control tables must list the same scales in the same order"
        );
        writeln!(
            out,
            "| {} | {r2:.3} s | {p2:.4} ms | {ratio2}× | {r3:.3} s | {p3:.4} ms | {ratio3}× |",
            fmt_num(a2)
        )
        .unwrap();
    }
    writeln!(out).unwrap();
    writeln!(
        out,
        "(§5's exact measurement, answer latency @ {mid} accounts: {lat2:.4} ms at `(2,4)`, \
         {lat3:.4} ms at `(3,4)`.)",
        mid = fmt_num(PUBLISHED_MID_SCALE_ACCOUNTS),
        lat2 = TODAY_CONTROL_ARITY2_ANSWER_LATENCY_MS_AT_MID_SCALE,
        lat3 = TODAY_CONTROL_ARITY3_ANSWER_LATENCY_MS_AT_MID_SCALE,
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(
        out,
        "Three things this control establishes, holding the machine fixed:"
    )
    .unwrap();
    writeln!(out).unwrap();

    // Facts 1 and 2 deliberately compare the `(3,4)` control against the
    // fixed 2026-07-22 `(3,4)` baseline, unconditionally — never
    // `mid_lat`/`top_figs`'s self-describing values — for the same
    // cross-geometry reason as the reproducibility note above: this
    // control's own `(3,4)` column and the 2026-07-22 baseline share a
    // geometry; a self-describing figure might not (it is `(2,4)` right
    // now). See `PUBLISHED_HEADLINE_RATIO_AT_TOP_SCALE`'s docs.
    let ctrl3_top_lat = TODAY_CONTROL_ARITY3_ANSWER_LATENCY_MS_AT_MID_SCALE;
    writeln!(
        out,
        "- The machine is still in a slow state: today's `(3,4)` control latency ({ctrl3_top_lat:.4} \
         ms) and the 2026-07-27 Run B figure ({run_b_lat:.4} ms, above) are both well above the \
         2026-07-22 `(3,4)` baseline this file used to publish ({pub_lat:.4} ms) — clear evidence of \
         how much this laptop's run-to-run timing varies on its own, independent of any code or \
         geometry change.",
        run_b_lat = RUN_B_ANSWER_LATENCY_MS_AT_MID_SCALE,
        pub_lat = PUBLISHED_ANSWER_LATENCY_MS_AT_MID_SCALE,
    )
    .unwrap();

    let (_, _, _, ctrl3_top_ratio) = TODAY_CONTROL_ARITY3_SCALES[2];
    let ratio_pct_diff = (PUBLISHED_HEADLINE_RATIO_AT_TOP_SCALE as f64 - ctrl3_top_ratio as f64)
        .abs()
        / PUBLISHED_HEADLINE_RATIO_AT_TOP_SCALE as f64
        * 100.0;
    writeln!(
        out,
        "- The ratio is machine-state-robust — direct evidence for the reproducibility note's \"the \
         ratio largely cancels a uniform machine slowdown\" argument above: every absolute time in \
         the control table runs roughly 1.5× slower than that same 2026-07-22 baseline, yet the \
         `(3,4)` control's {ctrl3_top_ratio}× ratio at {top} accounts lands within \
         {ratio_pct_diff:.1}% of the baseline's own {pub_ratio}× — the ratio held even though \
         nothing else did.",
        top = fmt_num(PUBLISHED_TOP_SCALE_ACCOUNTS),
        pub_ratio = PUBLISHED_HEADLINE_RATIO_AT_TOP_SCALE,
    )
    .unwrap();

    let bench_top_arity3_geom =
        Geometry::for_accounts(PUBLISHED_TOP_SCALE_ACCOUNTS, 3, BUCKET_SIZE, FINGERPRINT_BITS, &codec, Backend::Simple)
            .expect("complete_set_markdown: pre-ADR-0034 (arity 3, bucket_size 4) must size the published top scale");
    let bench_top_arity3_sizes =
        bench_top_arity3_geom.sizes(Backend::Simple, PUBLISHED_TOP_SCALE_ACCOUNTS);
    let bench_top_arity2_geom = Geometry::for_accounts(
        PUBLISHED_TOP_SCALE_ACCOUNTS,
        ARITY,
        BUCKET_SIZE,
        FINGERPRINT_BITS,
        &codec,
        Backend::Simple,
    )
    .expect("complete_set_markdown: the deployed (arity 2, bucket_size 4) must size the published top scale");
    let bench_top_arity2_sizes =
        bench_top_arity2_geom.sizes(Backend::Simple, PUBLISHED_TOP_SCALE_ACCOUNTS);
    let bench_top_arity3_cells =
        bench_top_arity3_sizes.slots * u64::from(bench_top_arity3_sizes.cells_per_slot);
    let bench_top_arity2_cells =
        bench_top_arity2_sizes.slots * u64::from(bench_top_arity2_sizes.cells_per_slot);
    let bench_top_data_ratio = bench_top_arity2_cells as f64 / bench_top_arity3_cells as f64;
    let (_, ctrl2_top_rebuild, ctrl2_top_patch, _) = TODAY_CONTROL_ARITY2_SCALES[2];
    let (_, ctrl3_top_rebuild, ctrl3_top_patch, _) = TODAY_CONTROL_ARITY3_SCALES[2];
    let bench_top_rebuild_ratio = ctrl2_top_rebuild / ctrl3_top_rebuild;
    let arity3_deploy_bytes =
        Geometry::for_accounts(DEPLOYMENT_ACCOUNTS, 3, BUCKET_SIZE, FINGERPRINT_BITS, &codec, Backend::Simple)
            .expect("complete_set_markdown: pre-ADR-0034 (arity 3, bucket_size 4) must size the deployment account count")
            .sizes(Backend::Simple, DEPLOYMENT_ACCOUNTS)
            .server_db;
    let deploy_fewer_cells_ratio = arity3_deploy_bytes as f64 / deployed_db_bytes as f64;

    writeln!(
        out,
        "- At these three bench scales `(2,4)` is the *unfavourable* case, and the committed \
         numbers therefore understate the deployed geometry — a reader must not conclude the arity \
         change itself made this system {bench_top_rebuild_ratio:.2}× slower. Arity 2's power-of-two \
         quantization lands badly at exactly {top} accounts: `(3,4)` needs {n3_buckets} buckets \
         (load {n3_load:.4}, {n3_cells} cells) where `(2,4)` needs {n2_buckets} ({n2_load:.4} load, \
         {n2_cells} cells — {bench_top_data_ratio:.2}× more data), which is why the `(2,4)` control \
         rebuilds {bench_top_rebuild_ratio:.2}× slower here ({ctrl2_top_rebuild:.3} s vs. \
         {ctrl3_top_rebuild:.3} s) — a consequence of the account count landing awkwardly for this \
         arity's quantization at this particular scale, not of the arity change in general. At the \
         complete set the relationship inverts: `(2,4)`'s server DB is {deploy2_bytes} against \
         `(3,4)`'s {deploy3_bytes} — {deploy_fewer_cells_ratio:.2}× *fewer* cells at deployment \
         scale. The one genuine arity effect visible in the control table runs the other way from \
         rebuild time: per-block patch time is *lower* at `(2,4)` ({ctrl2_top_patch:.4} ms vs. \
         {ctrl3_top_patch:.4} ms at {top} accounts) even though that scale holds more data under \
         `(2,4)` — fewer segments (2, not 3) to patch.",
        top = fmt_num(PUBLISHED_TOP_SCALE_ACCOUNTS),
        n3_buckets = fmt_num(u64::from(bench_top_arity3_geom.num_buckets)),
        n3_load = bench_top_arity3_sizes.load_factor,
        n3_cells = fmt_num(bench_top_arity3_cells),
        n2_buckets = fmt_num(u64::from(bench_top_arity2_geom.num_buckets)),
        n2_load = bench_top_arity2_sizes.load_factor,
        n2_cells = fmt_num(bench_top_arity2_cells),
        deploy2_bytes = fmt_bytes(deployed_db_bytes),
        deploy3_bytes = fmt_bytes(arity3_deploy_bytes),
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(
        out,
        "**Instrumentation superseded.** `NodeState::apply_block` \
         (`crates/risepir-http/src/node.rs`) has, since 2026-07-31, returned its own measured \
         hint-patch duration, and the mainnet follow loop (`crates/risepir-rpc/src/mainnet.rs`) \
         aggregates and periodically logs it — the source of an earlier, ad-hoc log-scraped estimate \
         of the complete set's per-block apply time. That estimate is superseded: the 2026-09-03 \
         measurement campaign (issue #4) reports the same quantity properly, with a controlled \
         protocol and full n/mean/p50/p95 statistics, in `docs/deployment-numbers.md` — cite that \
         file, not this paragraph, for the complete set's per-block apply time."
    )
    .unwrap();

    out
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
            "**IKPIR build (read before reproducing).** The full-rebuild and answer-latency \
             numbers here are measured against the workspace's pinned IKPIR `perf/optimized` tag \
             (`v0.2.0-perf` — see the root `Cargo.toml`), with the default-on `parallel` feature \
             (rayon matvec/GEMM kernels). `v0.2.0-perf` differs from `v0.1.0-perf` (`0f3b99b`) \
             only in the SimplePIR error sampler — a true discrete Gaussian `D_σ` in place of a \
             rounded continuous one (ADR-0046) — plus a version bump; no kernel, hash lineage, or \
             geometry moved. A `--no-default-features` build \
             reports substantially slower, single-threaded rebuild/answer times (the sizes and \
             delta-byte figures are unaffected). `xtask bench` prints to stdout by default; pass \
             `--write` to overwrite this file, and only do so from a build against the pinned tag \
             — bump the pin and these numbers together, never separately."
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
            let flag = if s.accounts == self.reached_top_scale
                && self.top_scale_fallback_reason.is_some()
            {
                " *(fallback scale — see note above)*"
            } else {
                ""
            };
            writeln!(
                out,
                "| {} | {:.3} s{flag} |",
                fmt_num(s.accounts),
                s.rebuild.as_secs_f64()
            )
            .unwrap();
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
        writeln!(
            out,
            "| K (mutations/block) | patch time (ms/block, measured) |"
        )
        .unwrap();
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
        writeln!(
            out,
            "| nonzero cells in delta | {} |",
            fmt_num(self.delta_bytes.nonzero_cells as u64)
        )
        .unwrap();
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
        writeln!(
            out,
            "## 4. Hint / query / response / A / server-DB sizes, and client memory"
        )
        .unwrap();
        writeln!(out).unwrap();

        // The deployment row every one of 4a/4b/4c appends after its
        // `self.scales` loop: `DEPLOYMENT_ACCOUNTS` (the live complete
        // mainnet set, §7) is not, and cannot be, one of `self.scales` — no
        // server at that size can be built on this machine — but its sizes
        // are exactly as computable as any other row's, from the same
        // `Geometry::for_accounts`/`Geometry::sizes` this module already
        // calls in `build_scale`. Computed once here, not per subsection,
        // so 4a/4b/4c cannot drift from each other.
        let deployment_geometry =
            Geometry::for_accounts(DEPLOYMENT_ACCOUNTS, ARITY, BUCKET_SIZE, FINGERPRINT_BITS, &codec, Backend::Simple)
                .expect("to_markdown: the deployed (arity, bucket_size) must size the live complete-mainnet account count");
        let deployment_sizes = deployment_geometry.sizes(Backend::Simple, DEPLOYMENT_ACCOUNTS);
        // Distinct from the committed file's old hand-typed label (which
        // this row replaces) precisely so it cannot be mistaken for a
        // measured scale — see the prose below for the full explanation.
        let deployment_label = format!(
            "{} (complete mainnet — computed, no server built at this scale)",
            fmt_num(DEPLOYMENT_ACCOUNTS)
        );

        writeln!(
            out,
            "Every row below is computed from a [`Geometry`] — deterministic, never timed — the \
             same as every other scale in `self.scales` (§1/§2/§3/§5/§6 additionally report real \
             measurements at those scales, since a server was actually built there). The final row \
             in each of 4a/4b/4c is different in kind, not just in size: it is \
             `DEPLOYMENT_ACCOUNTS`, the live complete mainnet set (§7), and no server has ever been \
             built at that scale on this machine — hence no corresponding row in §1/§2/§3/§5/§6, \
             which report only what this run actually measured. Its geometry and sizes are derived \
             exactly like every other row's, via `Geometry::for_accounts`/`Geometry::sizes` at this \
             module's own `ARITY`/`BUCKET_SIZE` — pure arithmetic, not a measurement in disguise, \
             and labelled below so it cannot be mistaken for one."
        )
        .unwrap();
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
        writeln!(
            out,
            "| {} | {} | {} | {:.4} | {} | {} | {} | {} | {} |",
            deployment_label,
            fmt_num(u64::from(deployment_geometry.num_buckets)),
            deployment_geometry.plaintext_bits,
            deployment_sizes.load_factor,
            deployment_sizes.cells_per_slot,
            deployment_sizes.row_width,
            deployment_sizes.k,
            fmt_num(u64::from(deployment_sizes.reshape_rows)),
            fmt_num(u64::from(deployment_sizes.reshape_row_width)),
        )
        .unwrap();
        writeln!(out).unwrap();

        writeln!(
            out,
            "### 4b. Per-segment sizes, per scale (computed, not timed)"
        )
        .unwrap();
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
        writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} |",
            deployment_label,
            fmt_bytes(deployment_sizes.hint_per_segment),
            fmt_bytes(deployment_sizes.query_per_segment),
            fmt_bytes(deployment_sizes.response_per_segment),
            fmt_bytes(deployment_sizes.a_per_segment),
            fmt_bytes(deployment_sizes.server_db),
        )
        .unwrap();
        writeln!(out).unwrap();

        writeln!(
            out,
            "### 4c. Deployment totals (×{ARITY} segments) and client memory (computed)"
        )
        .unwrap();
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
        // Kept as locals (not a nested block) rather than recomputed inside
        // the interpretation paragraph below — that paragraph is the whole
        // point of Job 2's Fix 2: it must quote these exact figures, not
        // hardcode its own copies of them.
        let deployment_arity = u64::from(ARITY);
        let deployment_hint_total = deployment_sizes.hint_per_segment * deployment_arity;
        let deployment_query_total = deployment_sizes.query_per_segment * deployment_arity;
        let deployment_response_total = deployment_sizes.response_per_segment * deployment_arity;
        let deployment_a_total = deployment_sizes.a_per_segment * deployment_arity;
        let deployment_client_mem =
            (deployment_sizes.a_per_segment + deployment_sizes.hint_per_segment) * deployment_arity;
        writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} |",
            deployment_label,
            fmt_bytes(deployment_hint_total),
            fmt_bytes(deployment_query_total),
            fmt_bytes(deployment_response_total),
            fmt_bytes(deployment_a_total),
            fmt_bytes(deployment_client_mem),
        )
        .unwrap();
        writeln!(out).unwrap();

        // The client-cost interpretation of the row just rendered — this
        // used to be hand-typed prose in the committed `docs/numbers.md`
        // (quoting the pre-ADR-0034 `(3,4)` figures) and was silently
        // dropped by every `--write` that didn't also hand-restore it,
        // exactly the failure mode the deployment row itself existed to
        // fix. Every figure below comes from `deployment_hint_total`/
        // `deployment_client_mem` above (this row's own `Sizes`), never a
        // hardcoded literal, so it tracks the geometry automatically.
        writeln!(
            out,
            "The last row is the honest cost of the complete set to a *client*: **{hint} \
             downloaded once** from `/setup`, and **{client_mem} resident** thereafter (the hint, \
             plus `A` re-expanded locally from its seed rather than transferred). That is the \
             inherent SimplePIR-class client footprint at {accounts} accounts, and it is what \
             `docs/adr/0019` means when it says the browser front end gives way to the CLI client \
             at the complete set.",
            hint = fmt_bytes(deployment_hint_total),
            client_mem = fmt_bytes(deployment_client_mem),
            accounts = fmt_num(DEPLOYMENT_ACCOUNTS),
        )
        .unwrap();
        writeln!(out).unwrap();

        // 2.4 and 3 are not this module's to derive — they are empirical/
        // architectural facts about the browser wasm init sequence (`2.4x`:
        // `web/pir.js`'s own comment "peaking near 2.4x mid-build", and
        // ADR-0032; `3x`: `web/pir.js`'s `ESTIMATED_PEAK_MULTIPLE` constant,
        // ADR-0032's pre-flight safety margin over the observed 2.4x) —
        // cited by name exactly as the original hand-typed prose cited
        // them. Only the resulting *size* is this module's to compute, from
        // `deployment_hint_total` (this row's own `Sizes`), never hardcoded.
        let peak_bytes = (deployment_hint_total as f64 * 2.4) as u64;
        writeln!(
            out,
            "One caveat for the *browser* specifically: this table is steady state, and a tab's \
             real ceiling is the **init peak** — encoded bundle, decoded bundle, and the built \
             client transiently coexist, and wasm linear memory never shrinks, so the peak is also \
             the tab's floor from then on. After the init-sequence fixes (free the encoded buffer \
             between decode and build; consume decoded hints per segment) that peak is ~2.4× the \
             hint (~{peak} here), and the front end's pre-flight budgets **3× the hint** for it \
             (`ESTIMATED_PEAK_MULTIPLE`, `web/pir.js` — derivation there; ADR-0032 revision). The \
             CLI client's peak is the same sequence minus the wasm no-shrink property.",
            peak = fmt_bytes(peak_bytes),
        )
        .unwrap();
        writeln!(out).unwrap();

        // ── 5. Answer latency ────────────────────────────────────────
        writeln!(
            out,
            "## 5. Answer latency, at {} accounts",
            fmt_num(self.answer_latency.accounts)
        )
        .unwrap();
        writeln!(out).unwrap();
        writeln!(out, "| metric | value |").unwrap();
        writeln!(out, "|---|---:|").unwrap();
        writeln!(
            out,
            "| queries measured | {} |",
            self.answer_latency.n_queries
        )
        .unwrap();
        writeln!(
            out,
            "| avg `server.answer(&queries)` latency | {:.4} ms |",
            self.answer_latency.avg_ms
        )
        .unwrap();
        writeln!(out).unwrap();

        // ── 6. Headline ───────────────────────────────────────────────
        writeln!(
            out,
            "## 6. The headline: full rebuild ÷ per-block patch (K≈{})",
            self.config.headline_k
        )
        .unwrap();
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
        writeln!(out).unwrap();

        // ── 7. The complete mainnet set — fixed historical citations,
        // independent of this run's own `self` — see `complete_set_markdown`.
        out.push_str(&complete_set_markdown(self));

        out
    }
}

#[cfg(test)]
mod complete_set_section_tests {
    use super::*;

    /// The tiny single-scale config every test in this module that
    /// doesn't need a specific scale reuses — deliberately far from both
    /// `PUBLISHED_TOP_SCALE_ACCOUNTS` (9,437,184) and
    /// `PUBLISHED_MID_SCALE_ACCOUNTS` (1,000,000), so every report built
    /// from it exercises the fallback path in `resolve_top_scale_figures`/
    /// `resolve_mid_scale_latency` — the tiny-config case those functions'
    /// own docs describe.
    fn tiny_cfg() -> BenchConfig {
        BenchConfig {
            seed: 0x5EC7_10CE_5CA1_E000,
            scales: vec![10_000],
            mid_scale: 10_000,
            k_values: vec![50],
            headline_k: 50,
            warmup_blocks: 1,
            measured_blocks: 1,
            measured_queries: 1,
            block_time_secs: 12.0,
        }
    }

    /// `docs/numbers.md` §7 must be rendered by this module, not
    /// hand-typed — this locks in the figures a human reading the
    /// committed file should be able to find, so an edit that silently
    /// drops or unlabels one is caught here rather than only by
    /// inspection. Uses the (fallback-path) tiny report: none of the
    /// figures asserted below depend on self-describing vs. fallback, so
    /// this test's job is unchanged by that split.
    #[test]
    fn complete_set_markdown_contains_key_figures() {
        let report = run(&tiny_cfg());
        let section = complete_set_markdown(&report);

        assert!(
            section.contains("docs/deployment-numbers.md"),
            "must point readers at the measurement campaign's own report for the current \
             complete-set figures, not restate them here"
        );
        assert!(
            !section.contains("1236.5"),
            "the old pre-campaign rebuild figure must not appear — the campaign's own measured \
             numbers live in docs/deployment-numbers.md, never restated or implied here"
        );
        assert!(
            section.contains("time-setup"),
            "must still describe how the deployment's own setup time is measured (`risepir-rpc \
             time-setup`), even while quoting no number for it"
        );
        assert!(
            section.contains(&fmt_num(DEPLOYMENT_ACCOUNTS)),
            "must cite the live deployment's account count, thousands-separated to match this \
             module's `fmt_num` convention"
        );
        assert!(
            section.contains("EXTRAPOLATION"),
            "the extrapolated ratio (and its cross-check) must be labelled, never presented as if \
             it were measured"
        );
        assert!(
            section.starts_with("## 7."),
            "must render as its own numbered markdown section"
        );
        assert!(
            section.contains("Same-machine control"),
            "must render the (2,4) vs (3,4) same-machine control table and its reproducibility \
             discussion"
        );
    }

    /// `to_markdown` must append §7 unconditionally, even for a tiny
    /// single-scale report — §7's content does not depend on `self`, so
    /// this also guards against a future refactor accidentally gating it
    /// on `self.scales` containing some particular scale. This tiny
    /// report's scale/mid_scale never reach `PUBLISHED_TOP_SCALE_ACCOUNTS`/
    /// `PUBLISHED_MID_SCALE_ACCOUNTS`, so it also exercises (and locks in)
    /// the fallback path's own explicit labelling — the other half of the
    /// "cover both paths" requirement alongside
    /// `complete_set_markdown_self_describes_when_report_reaches_the_published_scale`
    /// below.
    #[test]
    fn to_markdown_always_appends_the_complete_set_section() {
        let report = run(&tiny_cfg());
        let markdown = report.to_markdown("test-machine", "2026-01-01");
        assert!(markdown.contains("## 7. The complete mainnet set"));
        assert!(markdown.contains(&fmt_num(DEPLOYMENT_ACCOUNTS)));

        let honest_summary_start = markdown
            .find("**Honest summary.**")
            .expect("Honest summary must be present");
        let repro_note_start = markdown
            .find("**Reproducibility note.**")
            .expect("Reproducibility note must be present");
        let honest_summary = &markdown[honest_summary_start..repro_note_start];
        assert!(
            honest_summary.contains("the previously committed file published"),
            "a report that never reaches PUBLISHED_TOP_SCALE_ACCOUNTS must fall back and label \
             itself as such, not silently say \"this file publishes\": {honest_summary}"
        );
        assert!(
            honest_summary.contains(&PUBLISHED_HEADLINE_RATIO_AT_TOP_SCALE.to_string()),
            "the fallback path must quote PUBLISHED_HEADLINE_RATIO_AT_TOP_SCALE, not some other \
             number: {honest_summary}"
        );
    }

    /// §4a/4b/4c must each append a computed `DEPLOYMENT_ACCOUNTS` row —
    /// even for a report whose own `scales` never comes close to that
    /// count — because no run can ever include a 200M-account scale (no
    /// server at that size can be built on this machine), so this row is
    /// the *only* way `docs/numbers.md` §4 can state the live deployment's
    /// sizes without hand-editing them back in after every `--write`
    /// (which is exactly what used to happen, and what the next `--write`
    /// would otherwise silently delete). The label must appear exactly
    /// three times — once per subsection — and must be unmistakable as
    /// "computed, not measured" on its own, without relying on a reader
    /// having also read the surrounding prose.
    #[test]
    fn to_markdown_includes_computed_deployment_row_in_section_4() {
        let report = run(&tiny_cfg());
        let markdown = report.to_markdown("test-machine", "2026-01-01");

        let label = format!(
            "{} (complete mainnet — computed, no server built at this scale)",
            fmt_num(DEPLOYMENT_ACCOUNTS)
        );
        let label = label.as_str();
        let occurrences = markdown.matches(label).count();
        assert_eq!(
            occurrences, 3,
            "expected exactly one deployment row in each of §4a/§4b/§4c (3 total), got {occurrences} \
             in:\n{markdown}"
        );

        let section_4_start = markdown.find("## 4.").expect("§4 must be present");
        let section_5_start = markdown.find("## 5.").expect("§5 must be present");
        let section_4 = &markdown[section_4_start..section_5_start];
        assert!(
            section_4.contains("no server has ever been built at that scale"),
            "§4's own prose must state plainly that the deployment row is computed, not measured"
        );

        // Fix 2: the client-cost interpretation paragraphs (download-once +
        // resident cost, and the browser init-peak caveat) used to be
        // hand-typed in the committed file and were silently dropped by
        // every `--write` that didn't also hand-restore them. Check both
        // that the substance survived and that its cross-references did,
        // and — the part a hand-typed paragraph can never guarantee — that
        // its figures are exactly the deployment row's own `Sizes`, not a
        // stale or independently hand-typed number.
        assert!(
            section_4.contains("downloaded once") && section_4.contains("resident"),
            "§4 must interpret the deployment row's client cost (download-once + resident), not \
             just table it"
        );
        assert!(
            section_4.contains("init peak")
                && section_4.contains("ESTIMATED_PEAK_MULTIPLE")
                && section_4.contains("docs/adr/0019"),
            "the client-cost interpretation must preserve its substance (the browser init-peak \
             caveat) and its cross-references (ADR-0019, `web/pir.js`'s `ESTIMATED_PEAK_MULTIPLE`)"
        );

        let codec = value_codec();
        let deployment_sizes = Geometry::for_accounts(
            DEPLOYMENT_ACCOUNTS,
            ARITY,
            BUCKET_SIZE,
            FINGERPRINT_BITS,
            &codec,
            Backend::Simple,
        )
        .unwrap()
        .sizes(Backend::Simple, DEPLOYMENT_ACCOUNTS);
        let arity = u64::from(ARITY);
        let hint_total = deployment_sizes.hint_per_segment * arity;
        let client_mem_total =
            (deployment_sizes.a_per_segment + deployment_sizes.hint_per_segment) * arity;
        assert!(
            section_4.contains(&fmt_bytes(hint_total)),
            "the interpretation paragraph's \"downloaded once\" figure must equal the deployment \
             row's own hint total ({}), not a stale or hand-typed number",
            fmt_bytes(hint_total)
        );
        assert!(
            section_4.contains(&fmt_bytes(client_mem_total)),
            "the interpretation paragraph's \"resident\" figure must equal the deployment row's own \
             client memory total ({}), not a stale or hand-typed number",
            fmt_bytes(client_mem_total)
        );
    }

    /// `resolve_top_scale_figures`/`resolve_mid_scale_latency`: the
    /// fallback path, exercised directly (not just via the rendered
    /// markdown — `to_markdown_always_appends_the_complete_set_section`
    /// already covers that). `tiny_cfg`'s scale/mid_scale never reach the
    /// published operating point, so both must report the frozen
    /// constants and flag themselves as not self-describing.
    #[test]
    fn resolve_functions_fall_back_when_the_report_does_not_reach_the_published_scale() {
        let report = run(&tiny_cfg());

        let top_figs = resolve_top_scale_figures(&report);
        assert!(!top_figs.self_describing);
        assert_eq!(top_figs.rebuild_secs, PUBLISHED_REBUILD_SECS_AT_TOP_SCALE);
        assert_eq!(top_figs.ratio, PUBLISHED_HEADLINE_RATIO_AT_TOP_SCALE);

        let mid_lat = resolve_mid_scale_latency(&report);
        assert!(!mid_lat.self_describing);
        assert_eq!(mid_lat.ms, PUBLISHED_ANSWER_LATENCY_MS_AT_MID_SCALE);
    }

    /// The other half of "cover both paths": a report whose `scales`
    /// actually contains `PUBLISHED_TOP_SCALE_ACCOUNTS` and whose
    /// `mid_scale` is `PUBLISHED_MID_SCALE_ACCOUNTS` — every real
    /// `BenchConfig::default()` run — must make §7's "honest summary"
    /// quote *this report's own* measured rebuild time and headline
    /// ratio, not the frozen `PUBLISHED_*` constants, and must say so
    /// ("this file publishes"). This is the exact bug that shipped twice:
    /// §7 citing a number that does not match what this same file's own
    /// §6 shows.
    ///
    /// Deliberately does not run a real `BenchConfig::default()` sweep
    /// (multi-second at 9,437,184 accounts) just to exercise this branch
    /// — `Geometry`/`Sizes` are pure arithmetic regardless of account
    /// count (see this module's own §4 doc comment), so a hand-built
    /// `ScaleReport` with a *real* geometry/sizes but *synthetic* timing
    /// figures exercises the exact same code path instantly. The
    /// synthetic figures are chosen far from every real `PUBLISHED_*`
    /// value so the test fails loudly if `complete_set_markdown` ever
    /// silently falls back instead of self-describing.
    #[test]
    fn complete_set_markdown_self_describes_when_report_reaches_the_published_scale() {
        let codec = value_codec();
        let geometry = Geometry::for_accounts(
            PUBLISHED_TOP_SCALE_ACCOUNTS,
            ARITY,
            BUCKET_SIZE,
            FINGERPRINT_BITS,
            &codec,
            Backend::Simple,
        )
        .unwrap();
        let sizes = geometry.sizes(Backend::Simple, PUBLISHED_TOP_SCALE_ACCOUNTS);

        const FAKE_REBUILD_SECS: f64 = 3.0;
        const FAKE_HEADLINE_PATCH_MS: f64 = 6.0; // ratio = 3.0 s / 0.006 s = 500
        const FAKE_RATIO: u64 = 500;
        const FAKE_MID_LATENCY_MS: f64 = 9.999;

        let scale = ScaleReport {
            accounts: PUBLISHED_TOP_SCALE_ACCOUNTS,
            rebuild: Duration::from_secs_f64(FAKE_REBUILD_SECS),
            geometry,
            sizes,
            headline_patch_ms: FAKE_HEADLINE_PATCH_MS,
        };
        let report = BenchReport {
            config: BenchConfig {
                seed: 0,
                scales: vec![PUBLISHED_TOP_SCALE_ACCOUNTS],
                mid_scale: PUBLISHED_MID_SCALE_ACCOUNTS,
                k_values: vec![300],
                headline_k: 300,
                warmup_blocks: 0,
                measured_blocks: 0,
                measured_queries: 0,
                block_time_secs: 12.0,
            },
            scales: vec![scale],
            patch_curve: vec![],
            delta_bytes: DeltaBytesReport {
                k: 300,
                accounts: PUBLISHED_TOP_SCALE_ACCOUNTS,
                nonzero_cells: 1,
                compact_bytes: 1,
                naive_bytes: 10,
                ratio: 10.0,
            },
            answer_latency: AnswerLatencyReport {
                accounts: PUBLISHED_MID_SCALE_ACCOUNTS,
                n_queries: 1,
                avg_ms: FAKE_MID_LATENCY_MS,
            },
            requested_top_scale: PUBLISHED_TOP_SCALE_ACCOUNTS,
            reached_top_scale: PUBLISHED_TOP_SCALE_ACCOUNTS,
            top_scale_fallback_reason: None,
        };

        let top_figs = resolve_top_scale_figures(&report);
        assert!(top_figs.self_describing);
        assert_eq!(top_figs.rebuild_secs, FAKE_REBUILD_SECS);
        assert_eq!(top_figs.ratio, FAKE_RATIO);

        let mid_lat = resolve_mid_scale_latency(&report);
        assert!(mid_lat.self_describing);
        assert_eq!(mid_lat.ms, FAKE_MID_LATENCY_MS);

        let section = complete_set_markdown(&report);
        let honest_summary_start = section
            .find("**Honest summary.**")
            .expect("Honest summary must be present");
        let repro_note_start = section
            .find("**Reproducibility note.**")
            .expect("Reproducibility note must be present");
        let honest_summary = &section[honest_summary_start..repro_note_start];
        assert!(
            honest_summary.contains(&format!("{FAKE_RATIO}× this file publishes")),
            "the honest summary must quote this report's own ratio ({FAKE_RATIO}×) and say \"this \
             file publishes\", not fall back: {honest_summary}"
        );
        assert!(
            !honest_summary.contains(&PUBLISHED_HEADLINE_RATIO_AT_TOP_SCALE.to_string()),
            "the frozen fallback ratio ({}) must not appear in the honest summary once the report \
             self-describes: {honest_summary}",
            PUBLISHED_HEADLINE_RATIO_AT_TOP_SCALE
        );
    }
}
