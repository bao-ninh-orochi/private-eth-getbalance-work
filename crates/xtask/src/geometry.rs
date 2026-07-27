//! `xtask geometry` — sweep [`risepir_proto::geometry::Geometry`] across
//! `arity x bucket_size` and, opt-in, measure real cuckoo-store fill
//! behaviour at those points (ADR-0030).
//!
//! # Why this exists
//!
//! The live deployment runs `arity 3, bucket_size 4`
//! (`crates/risepir-rpc/src/mainnet.rs`) and lands at load factor 0.498
//! against the 0.75 target `Geometry::for_accounts` sizes towards, because
//! `arity 3` forces `num_buckets` to `3 * 2^t`. It is tempting to conclude
//! that a different arity would recover the wasted headroom — it would
//! not: **the database size is a function of the achieved load factor
//! alone; arity does not enter the `server_db` formula at all** (see
//! [`compute_row`] and `risepir_proto::geometry::Sizes::server_db`).
//! `bucket_size` reaches the same load factors without touching arity.
//! Arity *does* move the hint total, proportional to `sqrt(arity)` — the
//! wrong direction for a browser client already fighting an 831 MB first
//! load. ADR-0030 records the full argument; this module is what makes it
//! reproducible instead of a one-off calculation.
//!
//! # Two deliverables, one module
//!
//! 1. **The arithmetic sweep** ([`sweep`], [`render_sweep_table`]): pure,
//!    fast, closed-form — every number comes from
//!    `risepir_proto::geometry::{Geometry, Sizes}` and
//!    [`risepir_proto::value::ValueCodec`], never hardcoded (the repo's own
//!    binding rule: never hardcode `plaintext_bits` or geometry). This is
//!    what `cargo test` exercises.
//! 2. **The fill-check** ([`fill_check`], [`render_fill_check`]): opt-in
//!    only (`--fill-check`), because it builds a real
//!    `segmented_cuckoo::CuckooKVStore` and inserts millions of synthetic
//!    keys — arithmetic can say a geometry's load factor *should* reach
//!    0.75, but only an actual cuckoo-eviction run can say it *does*. Never
//!    runs under `cargo test` (too slow) — see that function's docs.
//!
//! # A finding this module's own [`GeometryRow::buildable`] flag surfaces
//!
//! `Geometry::sizes` is pure arithmetic and has no opinion on whether a
//! `bucket_size` is realizable — but the real
//! `segmented_cuckoo::CuckooKVStore` does:
//! `segmented_cuckoo::SUPPORTED_BUCKET_SIZES` is `1..=4`, hard-enforced by
//! every arity's constructor. So half of this module's own default sweep
//! (`bucket_size` 5..=16) is arithmetic-only — informative for
//! understanding the shape of the design space, but not constructible with
//! the pinned IKPIR rev without an upstream change. [`GeometryRow::buildable`]
//! flags exactly this, and [`render_sweep_table`] annotates it, so the
//! sweep table cannot be misread as a menu of buildable options.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use risepir_feed::{MockConfig, MockFeed};
use risepir_proto::{Backend, GeomError, Geometry, ValueCodec};
use segmented_cuckoo::{
    CuckooKVStore, IndexScheme, Segmented2aryCuckooKVStore, Segmented3aryCuckooKVStore,
    Segmented4aryCuckooKVStore, MAX_LOAD_FACTOR, SUPPORTED_BUCKET_SIZES,
};

// ─── Fixed knobs ──────────────────────────────────────────────────────────

/// The live complete-set account count — every nonzero-balance mainnet
/// account as of the 2026-07-26 bootstrap. Source of truth:
/// `docs/deploy.md` §5.3 ("every one of mainnet's 200,503,969 funded
/// accounts") and its recorded geometry line ("100663296 buckets, server
/// DB 35.43 GB, load 0.498"). Named, not repeated as a bare literal, so a
/// future re-export (or a re-run of the §2.1 gate query) is a one-line
/// change.
pub const LIVE_COMPLETE_SET_ACCOUNTS: u64 = 200_503_969;

/// Default `--fingerprint-bits`: unchanged across every deployment and
/// every ADR in this repo (32-bit SCF positioning fingerprint).
const DEFAULT_FINGERPRINT_BITS: u32 = 32;

/// Arity of the live deployment (`crates/risepir-rpc/src/mainnet.rs`'s
/// `const ARITY`). Mirrored, not imported: `risepir-rpc` does not export
/// it (it is a `const`, private to that module), and depending on the
/// whole `risepir-rpc` binary crate for two `u32` literals would be
/// disproportionate. Used only to flag [`GeometryRow::deployed`].
const DEPLOYED_ARITY: u32 = 3;
/// Bucket size of the live deployment (`crates/risepir-rpc/src/mainnet.rs`'s
/// `const BUCKET_SIZE`) — see [`DEPLOYED_ARITY`]'s docs for why this is a
/// mirror, not an import.
const DEPLOYED_BUCKET_SIZE: u32 = 4;

// The target load this sweep's headroom column measures against is *not*
// mirrored here. It used to be — a flat `3/4` pair copied from
// `risepir_proto::geometry`'s then-private constants — which was correct
// only for as long as the target actually was flat for every
// configuration. ADR-0031 made it per-`(arity, bucket_size)`
// (`min(0.75, 0.85 × segmented_cuckoo::MAX_LOAD_FACTOR)`), at which point a
// copy here would have quietly overstated the headroom of exactly the three
// combinations that fix tightens — `(2,1)`, `(2,2)`, `(3,1)` — while every
// row this repo deploys stayed right, which is the worst shape a stale
// mirror can take. `risepir_proto::geometry::effective_target_load` is now
// public for this caller, so the column is derived from the same rule
// `for_accounts` applies rather than from a comment asking the next person
// to keep two numbers in sync. Still exact integers, never `f64`: the ratio
// is compared as `slots * num / den` in `u128`, so it cannot drift from
// `for_accounts`'s own rounding at multi-billion-slot scale.

// ─── The arithmetic sweep ─────────────────────────────────────────────────

/// One fully-derived `(arity, bucket_size)` configuration at a fixed
/// account count. Every field is computed from
/// `risepir_proto::geometry::{Geometry, Sizes}` — see [`compute_row`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeometryRow {
    /// Candidate buckets per key: 2, 3, or 4.
    pub arity: u32,
    /// Slots per bucket, as swept.
    pub bucket_size: u32,
    /// Total buckets `Geometry::for_accounts` picked for this configuration.
    pub num_buckets: u32,
    /// Derived PIR plaintext cell width, in bits (`Geometry::plaintext_bits`
    /// — never hardcoded, see the module docs).
    pub plaintext_bits: u32,
    /// Plaintext cells per slot (`Sizes::cells_per_slot`).
    pub cells_per_slot: u32,
    /// `accounts / slots` at this configuration.
    pub load_factor: f64,
    /// Whole-store flat cell array size, in bytes (`Sizes::server_db`) —
    /// depends only on `slots` and `cells_per_slot`, never on `arity`
    /// directly (see the module docs and
    /// `tests::db_size_depends_on_slots_not_arity`).
    pub server_db: u64,
    /// Server hint size, in bytes, **across every segment**
    /// (`Sizes::hint_per_segment * arity`) — the browser client's first
    /// download. Grows with `sqrt(arity)` at equal `server_db` (see
    /// `tests::sqrt_arity_hint_law`).
    pub hint_total: u64,
    /// Client query size, in bytes, across every segment.
    pub query_total: u64,
    /// Server response size, in bytes, across every segment.
    pub response_total: u64,
    /// Client resident memory, in bytes, across every segment: `(A +
    /// hint) * arity` — what a long-lived rewind client holds.
    pub client_mem_total: u64,
    /// The largest account count this exact `num_buckets` still satisfies
    /// this configuration's own target load for (ADR-0031 — 0.75 for every
    /// row this repo deploys or benches) — i.e. its capacity before the
    /// next doubling.
    pub max_accounts_at_target: u64,
    /// `(max_accounts_at_target / accounts - 1) * 100` — percentage growth
    /// still free before this geometry must re-bootstrap into a bigger one.
    pub headroom_pct: f64,
    /// `server_db` of the configuration one account past
    /// `max_accounts_at_target` — i.e. what today's account count grows
    /// into once it crosses the cliff.
    pub next_db: u64,
    /// Whether `segmented_cuckoo`'s real `CuckooKVStore` constructors
    /// accept this `bucket_size` today (`segmented_cuckoo::SUPPORTED_BUCKET_SIZES`,
    /// currently `1..=4`). `false` means this row is arithmetic-only — see
    /// the module docs.
    pub buildable: bool,
    /// The highest load factor a real cuckoo table at this `(arity,
    /// bucket_size)` is documented to reach
    /// (`segmented_cuckoo::MAX_LOAD_FACTOR`), or `None` for a
    /// `bucket_size` outside the published table (which is also outside
    /// `SUPPORTED_BUCKET_SIZES`, so such a row is arithmetic-only anyway).
    ///
    /// This is *not* the same quantity as `load_factor`: that is what this
    /// sizing lands on, this is what the structure can actually hold. A row
    /// whose `load_factor` exceeds this cannot be filled — see
    /// [`GeometryRow::fillable`].
    pub load_ceiling: Option<f64>,
    /// Whether this is the exact `(arity, bucket_size)` the live
    /// deployment runs.
    pub deployed: bool,
}

impl GeometryRow {
    /// Whether a real store at this geometry can actually hold `accounts`.
    ///
    /// `false` means the sizing target overshot what the structure
    /// supports, so building this configuration and filling it would end in
    /// `CuckooError::TableFull` partway through — measured, not theorised:
    /// `arity 2, bucket_size 1` sizes to load 0.7153 against a 0.48 ceiling
    /// and dies after 70.1% of its inserts. Rows with no published ceiling
    /// (`load_ceiling == None`) are reported fillable because there is
    /// nothing to contradict; they are already flagged unbuildable.
    pub fn fillable(&self) -> bool {
        self.load_ceiling.is_none_or(|c| self.load_factor <= c)
    }
}

/// Derives one [`GeometryRow`]: sizes `accounts` at `(arity, bucket_size,
/// fingerprint_bits)` under [`Backend::Simple`] (this repo's chosen
/// backend, ADR-0002), then derives this configuration's capacity before
/// the next doubling by asking `Geometry::for_accounts` directly, and its
/// target load by asking
/// `risepir_proto::geometry::effective_target_load` — never by re-deriving
/// either from a constant kept here (see the note above that constant's
/// former home).
///
/// # Errors
///
/// Propagates [`GeomError`] from `Geometry::for_accounts` (invalid arity,
/// zero `bucket_size`, zero field width, or a `num_buckets` that would not
/// fit `u32`).
pub fn compute_row(
    accounts: u64,
    arity: u32,
    bucket_size: u32,
    fingerprint_bits: u32,
    codec: &ValueCodec,
) -> Result<GeometryRow, GeomError> {
    let g = Geometry::for_accounts(accounts, arity, bucket_size, fingerprint_bits, codec, Backend::Simple)?;
    let s = g.sizes(Backend::Simple, accounts);
    let arity64 = u64::from(arity);

    // Largest accounts' with this exact num_buckets: the floor-division
    // form of the same `accounts/slots <= target` test `for_accounts`
    // applies internally, asking that module for this configuration's own
    // target rather than assuming a flat one (see the note above).
    let (target_num, target_den) = risepir_proto::geometry::effective_target_load(arity, bucket_size);
    let max_accounts_at_target = u64::try_from((u128::from(s.slots) * target_num) / target_den)
        .expect("slots * target fits u64: slots itself is u64 and the ratio only shrinks it");

    let g_next = Geometry::for_accounts(
        max_accounts_at_target + 1,
        arity,
        bucket_size,
        fingerprint_bits,
        codec,
        Backend::Simple,
    )?;
    let s_next = g_next.sizes(Backend::Simple, max_accounts_at_target + 1);

    Ok(GeometryRow {
        arity,
        bucket_size,
        num_buckets: g.num_buckets,
        plaintext_bits: g.plaintext_bits,
        cells_per_slot: s.cells_per_slot,
        load_factor: s.load_factor,
        server_db: s.server_db,
        hint_total: s.hint_per_segment * arity64,
        query_total: s.query_per_segment * arity64,
        response_total: s.response_per_segment * arity64,
        client_mem_total: (s.a_per_segment + s.hint_per_segment) * arity64,
        max_accounts_at_target,
        headroom_pct: (max_accounts_at_target as f64 / accounts as f64 - 1.0) * 100.0,
        next_db: s_next.server_db,
        buildable: SUPPORTED_BUCKET_SIZES.contains(&bucket_size),
        // Indexed defensively rather than by arithmetic: the table is
        // `[[f64; 4]; 3]` (arity 2..=4 x bucket_size 1..=4) and this
        // function accepts a wider `bucket_size` on purpose, so a bare
        // `MAX_LOAD_FACTOR[arity - 2][bucket_size - 1]` would panic on
        // exactly the arithmetic-only rows the sweep exists to show.
        load_ceiling: MAX_LOAD_FACTOR
            .get((arity as usize).wrapping_sub(2))
            .and_then(|row| row.get((bucket_size as usize).wrapping_sub(1)))
            .copied(),
        deployed: arity == DEPLOYED_ARITY && bucket_size == DEPLOYED_BUCKET_SIZE,
    })
}

/// Configuration for one [`sweep`] run. [`Default`] is the real gate:
/// today's live account count across every supported arity and
/// `bucket_size` 1..=16 (ADR-0030).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SweepConfig {
    /// Account count every row in the sweep is sized for.
    pub accounts: u64,
    /// Arities to sweep. `Geometry::for_accounts` rejects anything outside
    /// `{2, 3, 4}` — see [`GeomError::InvalidArity`].
    pub arities: Vec<u32>,
    /// `bucket_size` values to sweep. Values outside
    /// `segmented_cuckoo::SUPPORTED_BUCKET_SIZES` are still computed (this
    /// module is pure arithmetic) but flagged
    /// [`GeometryRow::buildable`]` == false`.
    pub bucket_sizes: Vec<u32>,
    /// SCF positioning fingerprint width, in bits.
    pub fingerprint_bits: u32,
}

impl Default for SweepConfig {
    fn default() -> Self {
        Self {
            accounts: LIVE_COMPLETE_SET_ACCOUNTS,
            arities: vec![2, 3, 4],
            bucket_sizes: (1..=16).collect(),
            fingerprint_bits: DEFAULT_FINGERPRINT_BITS,
        }
    }
}

/// Runs [`compute_row`] over every `(arity, bucket_size)` pair in `cfg`,
/// in the order given (arities outer, bucket sizes inner), against the
/// fixed ADR-0009 value codec (`xtask::bench::value_codec`) — the same
/// `key_tag(32) ‖ balance(96) ‖ checksum(16)` layout the live deployment
/// uses, so these numbers are directly comparable to `docs/numbers.md`
/// and `docs/deploy.md`.
///
/// # Errors
///
/// The first [`GeomError`] hit by any configuration, propagated
/// immediately — a bad sweep parameter (e.g. an arity outside `{2,3,4}`
/// from a hand-typed `--arity`) is a configuration mistake worth failing
/// loudly on, not silently dropping a row for.
pub fn sweep(cfg: &SweepConfig) -> Result<Vec<GeometryRow>, GeomError> {
    let codec = crate::bench::value_codec();
    let mut rows = Vec::with_capacity(cfg.arities.len() * cfg.bucket_sizes.len());
    for &arity in &cfg.arities {
        for &bucket_size in &cfg.bucket_sizes {
            rows.push(compute_row(cfg.accounts, arity, bucket_size, cfg.fingerprint_bits, &codec)?);
        }
    }
    Ok(rows)
}

/// Renders `rows` as a plain-text table: identity + derived sizes +
/// headroom, one line per configuration, with the deployed row and any
/// not-buildable `bucket_size` annotated inline (see [`GeometryRow::deployed`]
/// / [`GeometryRow::buildable`]).
pub fn render_sweep_table(rows: &[GeometryRow], accounts: u64) -> String {
    let mut out = String::new();
    // Not "target load 0.75": since ADR-0031 the target is per-row
    // (`min(0.75, 0.85 × segmented_cuckoo::MAX_LOAD_FACTOR)`), so a single
    // number in the header would be wrong for exactly the rows where it
    // matters. Every configuration this repo deploys or benches still
    // resolves to 0.75; the three that do not are `(2,1)`, `(2,2)`, `(3,1)`.
    writeln!(
        out,
        "accounts = {accounts} (target load per row: min(0.75, 0.85 x MAX_LOAD_FACTOR); \
         docs/plan.md §9, ADR-0030, ADR-0031)"
    )
    .unwrap();
    writeln!(
        out,
        "{:>5} {:>3} {:>12} {:>5} {:>3} {:>7} {:>7} | {:>8} {:>9} {:>8} {:>8} {:>9} | {:>12} {:>8} | {:>8}",
        "arity",
        "bs",
        "num_bkts",
        "pbits",
        "cps",
        "load",
        "maxload",
        "db_GB",
        "hint_MB",
        "qry_KB",
        "rsp_KB",
        "cmem_GB",
        "max@tgt",
        "headrm%",
        "next_GB",
    )
    .unwrap();
    for r in rows {
        writeln!(
            out,
            "{:>5} {:>3} {:>12} {:>5} {:>3} {:>7.4} {:>7} | {:>8.2} {:>9.2} {:>8.2} {:>8.2} {:>9.2} | {:>12} {:>8.1} | {:>8.2}{}",
            r.arity,
            r.bucket_size,
            r.num_buckets,
            r.plaintext_bits,
            r.cells_per_slot,
            r.load_factor,
            r.load_ceiling.map_or_else(|| "—".to_string(), |c| format!("{c:.2}")),
            r.server_db as f64 / 1e9,
            r.hint_total as f64 / 1e6,
            r.query_total as f64 / 1e3,
            r.response_total as f64 / 1e3,
            r.client_mem_total as f64 / 1e9,
            r.max_accounts_at_target,
            r.headroom_pct,
            r.next_db as f64 / 1e9,
            row_annotation(r),
        )
        .unwrap();
    }
    if rows.iter().any(|r| !r.buildable) {
        writeln!(
            out,
            "† bucket_size outside segmented_cuckoo::SUPPORTED_BUCKET_SIZES ({:?} today) — arithmetic \
             only, not constructible with the pinned IKPIR rev without an upstream change (ADR-0030).",
            SUPPORTED_BUCKET_SIZES
        )
        .unwrap();
    }
    if rows.iter().any(|r| !r.fillable()) {
        writeln!(
            out,
            "‡ this sizing lands above what a real cuckoo table at that (arity, bucket_size) holds \
             (`load` > `maxload`, segmented_cuckoo::MAX_LOAD_FACTOR) — the geometry is well-formed \
             but cannot be filled: inserts end in TableFull partway through. Sizing, not arithmetic, \
             is what is wrong; see ADR-0030 and ADR-0031. Since ADR-0031, Geometry::for_accounts \
             sizes against each configuration's own achievable load, so no row here should earn this \
             mark — one that does means a configuration outside what that rule covers."
        )
        .unwrap();
    }
    out
}

/// Trailing per-row annotation for [`render_sweep_table`]: the deployed
/// marker takes priority (it is also live today, so both buildable and
/// fillable); then a not-buildable dagger; then a not-fillable
/// double-dagger.
///
/// The last two are mutually exclusive by construction, not by luck: a row
/// can only be judged unfillable when `segmented_cuckoo::MAX_LOAD_FACTOR`
/// publishes a ceiling for it, which it does exactly for the `bucket_size`
/// values `SUPPORTED_BUCKET_SIZES` also accepts.
fn row_annotation(r: &GeometryRow) -> &'static str {
    if r.deployed {
        "  <- DEPLOYED (mainnet.rs)"
    } else if !r.buildable {
        "  †"
    } else if !r.fillable() {
        "  ‡"
    } else {
        ""
    }
}

// ─── The fill-check (opt-in, real store) ──────────────────────────────────

/// One `(arity, bucket_size)` combination to fill-check for real.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FillCandidate {
    /// Candidate arity (2, 3, or 4).
    pub arity: u32,
    /// Candidate bucket size.
    pub bucket_size: u32,
}

/// The minimum candidate set this deliverable asks for: today's deployed
/// point, the arity-3 alternatives ADR-0030's recommendation compares
/// (including `bucket_size = 6`, which — see [`GeometryRow::buildable`] —
/// this fill-check is exactly what catches as unconstructible today), and
/// one point each at arity 2 and 4 so the "arity does not move the
/// database size" claim is checked empirically, not just arithmetically.
pub const DEFAULT_FILL_CANDIDATES: [FillCandidate; 5] = [
    FillCandidate { arity: 3, bucket_size: 4 },
    FillCandidate { arity: 3, bucket_size: 6 },
    FillCandidate { arity: 3, bucket_size: 3 },
    FillCandidate { arity: 2, bucket_size: 4 },
    FillCandidate { arity: 4, bucket_size: 4 },
];

/// Default `--fill-accounts` scale: comfortably fits 16 GB, and matches
/// `xtask::bench::BenchConfig::default()`'s own largest scale (the
/// `segment_rows = 2^20`-at-75%-load point `docs/verification.md` already
/// reports numbers at), so this tool's fill-check sits next to numbers
/// already measured elsewhere in this repo rather than inventing a new
/// unrelated scale.
pub const DEFAULT_FILL_ACCOUNTS: u64 = 9_437_184;

/// Deterministic seed for the fill-check's synthetic genesis population —
/// the same fixed seed `xtask::bench::BenchConfig::default()` uses, purely
/// so a reader does not have to wonder whether a different seed changes
/// the picture.
const FILL_CHECK_SEED: u64 = 0xB0DA_C0DE_5CA1_E000;

/// Real, measured outcome of filling one candidate's store.
#[derive(Clone, Debug)]
pub struct FillOutcome {
    /// Accounts actually inserted successfully.
    pub accounts_inserted: u64,
    /// Insert attempts that failed. In practice this can only be
    /// `segmented_cuckoo::CuckooError::TableFull` — every key is distinct
    /// (fresh from `MockFeed`) and every value already round-trips through
    /// [`risepir_proto::value::ValueCodec::encode`] before the insert is
    /// attempted, so a well-formed insert's only failure mode left is the
    /// store being full.
    pub insert_failures: u64,
    /// `CuckooKVStore::load_factor()` after every insert attempt.
    pub achieved_load_factor: f64,
    /// Wall-clock time for the whole fill loop (construction + every
    /// insert).
    pub elapsed: Duration,
}

/// Result of fill-checking one candidate.
#[derive(Clone, Debug)]
pub struct FillCheckResult {
    /// The candidate fill-checked.
    pub candidate: FillCandidate,
    /// Accounts requested (the `--fill-accounts` scale).
    pub accounts_requested: u64,
    /// `Ok` once the store both constructed and accepted the fill loop;
    /// `Err` with a message if construction itself failed — e.g.
    /// `bucket_size` outside `segmented_cuckoo::SUPPORTED_BUCKET_SIZES`.
    /// That is a real, reportable outcome (see [`DEFAULT_FILL_CANDIDATES`]'s
    /// docs), never papered over: this module never fakes a result either
    /// way.
    pub outcome: Result<FillOutcome, String>,
}

/// Builds `store` (already constructed by the caller, so this is generic
/// over which concrete arity it is), inserts `accounts` distinct synthetic
/// `(address, balance)` genesis pairs from a deterministic [`MockFeed`],
/// and reports the achieved load factor and any insert failures.
///
/// This mirrors `xtask::bench::build_scale`'s own genesis-population loop
/// (`MockFeed::new(..).snapshot()`, `ValueCodec::encode`, `store.insert`)
/// bit-for-bit — generalised over the scheme type via [`IndexScheme`] so
/// arity 2/3/4 share this one loop instead of three near-identical copies.
/// Unlike `build_scale`, this never builds a `RisePirServer` — a
/// fill-check is a claim about cuckoo eviction alone, and running the full
/// LWE `server_setup` on top would burn minutes measuring something this
/// deliverable never asked about.
fn fill_store<S: IndexScheme>(mut store: CuckooKVStore<S>, accounts: u64, codec: &ValueCodec) -> FillOutcome {
    let feed = MockFeed::new(MockConfig {
        seed: FILL_CHECK_SEED,
        num_genesis_keys: accounts,
        changes_per_block: 0,
        inserts_per_block: 0,
        deletes_per_block: 0,
    });
    let snapshot = feed.snapshot();
    drop(feed); // free the live/balances model before the insert loop runs

    let t0 = Instant::now();
    let mut accounts_inserted = 0u64;
    let mut insert_failures = 0u64;
    for (addr, balance) in snapshot {
        let encoded = codec
            .encode(&addr, balance)
            .expect("fill-check: the ADR-0009 codec must encode a MockFeed genesis balance (bench.rs relies on the same fact)");
        match store.insert(addr, &encoded) {
            Ok(()) => accounts_inserted += 1,
            Err(_) => insert_failures += 1,
        }
    }

    FillOutcome {
        accounts_inserted,
        insert_failures,
        achieved_load_factor: store.load_factor(),
        elapsed: t0.elapsed(),
    }
}

/// Fill-checks one candidate at `accounts`: derives `num_buckets` /
/// `plaintext_bits` from the exact same `Geometry::for_accounts` the
/// deployment itself calls (never hardcoded), builds the concrete
/// `segmented_cuckoo` store type matching `candidate.arity`, and runs
/// `fill_store`. Never panics on a bad candidate — a construction
/// failure is folded into `FillCheckResult::outcome` as `Err`.
pub fn fill_check_one(candidate: FillCandidate, accounts: u64, fingerprint_bits: u32, codec: &ValueCodec) -> FillCheckResult {
    let outcome = (|| -> Result<FillOutcome, String> {
        let geom = Geometry::for_accounts(accounts, candidate.arity, candidate.bucket_size, fingerprint_bits, codec, Backend::Simple)
            .map_err(|e| format!("geometry: {e}"))?;
        match candidate.arity {
            2 => {
                let store = Segmented2aryCuckooKVStore::new(
                    geom.num_buckets,
                    geom.bucket_size,
                    geom.fingerprint_bits,
                    geom.value_bits,
                    geom.plaintext_bits,
                )
                .map_err(|e| format!("store construction: {e}"))?;
                Ok(fill_store(store, accounts, codec))
            }
            3 => {
                let store = Segmented3aryCuckooKVStore::new(
                    geom.num_buckets,
                    geom.bucket_size,
                    geom.fingerprint_bits,
                    geom.value_bits,
                    geom.plaintext_bits,
                )
                .map_err(|e| format!("store construction: {e}"))?;
                Ok(fill_store(store, accounts, codec))
            }
            4 => {
                let store = Segmented4aryCuckooKVStore::new(
                    geom.num_buckets,
                    geom.bucket_size,
                    geom.fingerprint_bits,
                    geom.value_bits,
                    geom.plaintext_bits,
                )
                .map_err(|e| format!("store construction: {e}"))?;
                Ok(fill_store(store, accounts, codec))
            }
            other => Err(format!("fill_check_one: arity must be 2, 3, or 4, got {other}")),
        }
    })();

    FillCheckResult {
        candidate,
        accounts_requested: accounts,
        outcome,
    }
}

/// Fill-checks every candidate in `candidates`, in order, against the
/// fixed ADR-0009 value codec. **Slow — never call this from `cargo
/// test`.** Each candidate builds a real store and inserts `accounts` real
/// entries; at the default scale and candidate count this is a
/// multi-minute run, which is exactly why `xtask::main` only reaches this
/// function behind an explicit opt-in `--fill-check` flag, never as part
/// of the always-on sweep.
pub fn fill_check(candidates: &[FillCandidate], accounts: u64, fingerprint_bits: u32) -> Vec<FillCheckResult> {
    let codec = crate::bench::value_codec();
    candidates.iter().map(|&c| fill_check_one(c, accounts, fingerprint_bits, &codec)).collect()
}

/// Renders fill-check results as a plain-text table: requested vs.
/// inserted accounts, failures, achieved load factor, and wall-clock —
/// or, for a candidate whose store did not even construct, the error in
/// place of a row of numbers.
pub fn render_fill_check(results: &[FillCheckResult]) -> String {
    let mut out = String::new();
    writeln!(out, "fill-check: real segmented_cuckoo::CuckooKVStore, real inserts (opt-in — not part of `cargo test`)").unwrap();
    writeln!(
        out,
        "{:>5} {:>3} {:>12} {:>12} {:>10} {:>8} {:>10}",
        "arity", "bs", "requested", "inserted", "failed", "load", "elapsed"
    )
    .unwrap();
    for r in results {
        match &r.outcome {
            Ok(o) => writeln!(
                out,
                "{:>5} {:>3} {:>12} {:>12} {:>10} {:>8.4} {:>9.1}s",
                r.candidate.arity,
                r.candidate.bucket_size,
                r.accounts_requested,
                o.accounts_inserted,
                o.insert_failures,
                o.achieved_load_factor,
                o.elapsed.as_secs_f64(),
            )
            .unwrap(),
            Err(msg) => writeln!(
                out,
                "{:>5} {:>3} {:>12} CONSTRUCTION FAILED: {msg}",
                r.candidate.arity, r.candidate.bucket_size, r.accounts_requested,
            )
            .unwrap(),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codec() -> ValueCodec {
        crate::bench::value_codec()
    }

    /// Pins the deployed configuration (`arity 3, bucket_size 4`) at the
    /// live complete-set account count against the exact figures recorded
    /// in `docs/deploy.md` §5.3 and re-derived for ADR-0030: 100,663,296
    /// buckets, load 0.4980, a 35,433,480,192 B server DB, and an
    /// 830,728,800 B total hint.
    #[test]
    fn deployed_configuration_pins_live_geometry() {
        let row = compute_row(LIVE_COMPLETE_SET_ACCOUNTS, 3, 4, 32, &codec()).expect("deployed configuration must size");
        assert_eq!(row.num_buckets, 100_663_296);
        assert!((row.load_factor - 0.4980).abs() < 1e-4, "load_factor = {}", row.load_factor);
        assert_eq!(row.server_db, 35_433_480_192);
        assert_eq!(row.hint_total, 830_728_800);
        assert!(row.deployed);
        assert!(row.buildable);
    }

    /// Pins the brief's own arithmetic: switching to `arity 4, bucket_size
    /// 4` at the live account count gives a 23,622,320,128 B server DB and
    /// a 784,502,400 B total hint — the brief's computed 23.62 GB / 784.5
    /// MB, reproduced exactly. (Its *conclusion* — that this is a good
    /// trade — is what the other tests below refute.)
    #[test]
    fn arity4_bucket4_pins_the_briefs_arithmetic() {
        let row = compute_row(LIVE_COMPLETE_SET_ACCOUNTS, 4, 4, 32, &codec()).expect("arity 4 / bucket_size 4 must size");
        assert_eq!(row.server_db, 23_622_320_128);
        assert_eq!(row.hint_total, 784_502_400);
        assert!(!row.deployed);
        assert!(row.buildable, "bucket_size 4 is within segmented_cuckoo::SUPPORTED_BUCKET_SIZES");
    }

    /// The central correction: `server_db` depends on the achieved load
    /// factor (equivalently, on `slots = num_buckets * bucket_size` and
    /// `cells_per_slot`), never on `arity` directly. At the live account
    /// count and `bucket_size = 4`, `for_accounts`'s own `num_buckets`
    /// formula for arity 2 and arity 4 is bit-identical (`buckets_needed.max(arity).next_power_of_two()`
    /// — the `.max(arity)` floor only bites for tiny inputs, not 200M
    /// accounts), so this is checked against the real sizing path, not a
    /// hand-constructed coincidence. The hint total, in contrast, *does*
    /// move — that is `sqrt_arity_hint_law` below.
    #[test]
    fn db_size_depends_on_slots_not_arity() {
        let c = codec();
        let row2 = compute_row(LIVE_COMPLETE_SET_ACCOUNTS, 2, 4, 32, &c).unwrap();
        let row4 = compute_row(LIVE_COMPLETE_SET_ACCOUNTS, 4, 4, 32, &c).unwrap();
        assert_eq!(row2.num_buckets, row4.num_buckets, "arity 2 and 4 must reach the same num_buckets at this scale");
        assert_eq!(
            row2.server_db, row4.server_db,
            "server_db must depend only on slots/cells_per_slot, not arity"
        );
        assert_ne!(row2.hint_total, row4.hint_total, "but the hint DOES move with arity (sqrt_arity_hint_law)");
    }

    /// The `sqrt(arity)` hint law, checked at *equal* database size:
    /// `(arity=3, bucket_size=6)` and `(arity=4, bucket_size=9)` both reach
    /// `num_buckets * bucket_size = 301,989,888` slots at the live account
    /// count, so — per `db_size_depends_on_slots_not_arity` — their
    /// `server_db` must match; their hint totals must not, and the ratio
    /// must land within ~1% of `sqrt(4/3)` (the brief's own closed form:
    /// `hint_total ≈ 4 * lwe_dim * sqrt(arity * db_cells)`).
    #[test]
    fn sqrt_arity_hint_law() {
        let c = codec();
        let row3 = compute_row(LIVE_COMPLETE_SET_ACCOUNTS, 3, 6, 32, &c).unwrap();
        let row4 = compute_row(LIVE_COMPLETE_SET_ACCOUNTS, 4, 9, 32, &c).unwrap();
        assert_eq!(row3.server_db, row4.server_db, "both must land at the same 301,989,888-slot database size");

        let ratio = row4.hint_total as f64 / row3.hint_total as f64;
        let expected = (4.0_f64 / 3.0).sqrt();
        assert!(
            (ratio - expected).abs() / expected < 0.01,
            "hint_total(4)/hint_total(3) = {ratio:.4}, expected sqrt(4/3) = {expected:.4} within 1%"
        );
    }

    /// The cliff: `arity 4, bucket_size 4` (the brief's proposed swap) has
    /// under 1% growth headroom before the next doubling, and that next
    /// doubling (47.24 GB) is worse than today's deployed 35.43 GB — i.e.
    /// the "saving" evaporates within weeks of mainnet's account growth.
    /// This also cross-checks the headroom column's target-load source
    /// (`risepir_proto::geometry::effective_target_load`) against
    /// `for_accounts`'s real behaviour at the exact boundary: capacity
    /// must satisfy this `num_buckets`, and one more account must not.
    #[test]
    fn arity4_bucket4_sits_on_the_cliff() {
        let c = codec();
        let row = compute_row(LIVE_COMPLETE_SET_ACCOUNTS, 4, 4, 32, &c).unwrap();
        assert!(row.headroom_pct < 1.0, "headroom must be under 1%, got {:.2}%", row.headroom_pct);

        let deployed = compute_row(LIVE_COMPLETE_SET_ACCOUNTS, 3, 4, 32, &c).unwrap();
        assert!(
            row.next_db > deployed.server_db,
            "post-doubling DB ({} B) must exceed today's deployed DB ({} B)",
            row.next_db,
            deployed.server_db
        );

        // Target-load self-check: capacity fits this num_buckets, one
        // more account does not.
        let at_capacity =
            Geometry::for_accounts(row.max_accounts_at_target, row.arity, row.bucket_size, 32, &c, Backend::Simple).unwrap();
        assert_eq!(at_capacity.num_buckets, row.num_buckets, "capacity must still fit today's num_buckets");
        let past_capacity =
            Geometry::for_accounts(row.max_accounts_at_target + 1, row.arity, row.bucket_size, 32, &c, Backend::Simple).unwrap();
        assert!(
            past_capacity.num_buckets > row.num_buckets,
            "one account past capacity must force the next doubling"
        );
    }

    /// `bucket_size` beyond `segmented_cuckoo::SUPPORTED_BUCKET_SIZES`
    /// (currently `1..=4`) is still computed (this module is pure
    /// arithmetic) but must be flagged not buildable — the fill-check
    /// (run for real in `xtask::main`, not here — see that function's
    /// docs) is what turns this flag from a suspicion into a demonstrated
    /// `CuckooError::InvalidParams`.
    #[test]
    fn bucket_size_above_four_is_flagged_not_buildable() {
        let row = compute_row(LIVE_COMPLETE_SET_ACCOUNTS, 3, 6, 32, &codec()).unwrap();
        assert!(!row.buildable);
        let deployed = compute_row(LIVE_COMPLETE_SET_ACCOUNTS, 3, 4, 32, &codec()).unwrap();
        assert!(deployed.buildable);
    }

    /// `arity 2, bucket_size 1` is the configuration that motivated
    /// ADR-0031, and it is now sized *safely*: a flat 0.75 target used to
    /// land it at load 0.7469 against a published ceiling of 0.48 — a real
    /// store died with `TableFull` at 70.1% of its inserts — while
    /// `effective_target_load` now caps it at `0.85 × 0.48 = 0.408`, so the
    /// sweep reports a row that can actually be filled.
    ///
    /// This is the sweep-level regression test for that fix: it asserts the
    /// property (sized at or under the ceiling), not the exact figure, so
    /// it keeps its meaning if the margin is ever retuned.
    #[test]
    fn the_motivating_configuration_is_now_sized_below_its_ceiling() {
        let row = compute_row(LIVE_COMPLETE_SET_ACCOUNTS, 2, 1, 32, &codec()).unwrap();
        assert_eq!(row.load_ceiling, Some(0.48));
        assert!(row.buildable, "bucket_size 1 is inside SUPPORTED_BUCKET_SIZES");
        assert!(
            row.load_factor <= 0.48,
            "ADR-0031 must size (2,1) at or under its published ceiling, got {}",
            row.load_factor
        );
        assert!(row.fillable(), "nothing left to flag once it is sized correctly");
        assert_eq!(row_annotation(&row), "");
    }

    /// ...but the flag itself must still work. `for_accounts` can no longer
    /// *produce* a row above its own ceiling (that is exactly what ADR-0031
    /// removed), so this drives [`GeometryRow::fillable`] and
    /// [`row_annotation`] directly with a row placed over the line. Without
    /// this, the sweep's unfillable-reporting path would be left with no
    /// test at all the moment the sizing fix landed — the machinery would
    /// still be there, and nothing would notice if it broke.
    #[test]
    fn a_row_above_its_ceiling_is_still_flagged_unfillable() {
        let mut row = compute_row(LIVE_COMPLETE_SET_ACCOUNTS, 2, 1, 32, &codec()).unwrap();
        assert_eq!(row.load_ceiling, Some(0.48));
        row.load_factor = 0.7469; // what a flat-0.75 target used to produce here
        assert!(!row.fillable(), "must be flagged: sized above what the table holds");
        assert_eq!(row_annotation(&row), "  ‡");
    }

    /// ...and must not cry wolf on the ones that are fine, including the
    /// deployed geometry and the arithmetic-only rows that have no
    /// published ceiling to be measured against.
    #[test]
    fn fillable_rows_are_not_flagged() {
        for (arity, bucket_size) in [(2u32, 2u32), (2, 3), (2, 4), (3, 1), (3, 3), (3, 4), (4, 4)] {
            let row = compute_row(LIVE_COMPLETE_SET_ACCOUNTS, arity, bucket_size, 32, &codec()).unwrap();
            assert!(
                row.fillable(),
                "({arity},{bucket_size}) sized at {} against ceiling {:?}",
                row.load_factor,
                row.load_ceiling
            );
        }
        // No published ceiling => nothing to contradict; flagged unbuildable
        // by the dagger instead, never by the double-dagger.
        let wide = compute_row(LIVE_COMPLETE_SET_ACCOUNTS, 3, 6, 32, &codec()).unwrap();
        assert_eq!(wide.load_ceiling, None);
        assert!(wide.fillable());
        assert_eq!(row_annotation(&wide), "  †");
    }
}
