//! `xtask geometry` — sweep [`risepir_proto::geometry::Geometry`] across
//! `arity x bucket_size` and, opt-in, measure real cuckoo-store fill
//! behaviour at those points (ADR-0030, retuned by ADR-0034).
//!
//! # Why this exists
//!
//! The live deployment runs `arity 2, bucket_size 4`
//! (`crates/risepir-rpc/src/mainnet.rs`) and lands at load factor 0.7469 —
//! comfortably under its own per-configuration target of 0.8645
//! (`risepir_proto::geometry::effective_target_load(2, 4)`), itself
//! tighter than the flat 0.90 cap every configuration is also bounded by
//! (ADR-0034). Before ADR-0034 the deployment ran `arity 3, bucket_size 4`
//! at load factor 0.498 against a flat 0.75 cap — a configuration
//! [`compute_row`] still reproduces exactly
//! (`tests::pre_adr_0034_configuration_pins_historical_geometry`), and one
//! this module's default fill-check candidates still include for
//! comparison.
//!
//! It is tempting to conclude that picking a different arity, by itself,
//! recovers wasted database headroom — it still does not: **the database
//! size is a function of the achieved load factor alone (equivalently, of
//! `slots = num_buckets * bucket_size`); arity does not enter the
//! `server_db` formula at all** (see [`compute_row`] and
//! `risepir_proto::geometry::Sizes::server_db`, pinned by
//! `tests::db_size_depends_on_slots_not_arity`, which finds `(2,4)` and
//! `(4,4)` bit-identical in `server_db` at the live account count).
//! `bucket_size` reaches the same load factors without touching arity
//! either. This is ADR-0030's original argument, and it still stands.
//!
//! What ADR-0034 adds is the other half: arity fixes which `num_buckets`
//! values are reachable at all, and therefore which `slots` "rungs" a
//! configuration can land on. `Geometry::for_accounts` shapes `num_buckets`
//! as a power of two for arity 2/4, or as `3 * 2^t` for arity 3. Combined
//! with `bucket_size`'s own factor into `slots = num_buckets *
//! bucket_size`, the buildable configurations (`bucket_size` 1..=4,
//! `segmented_cuckoo::SUPPORTED_BUCKET_SIZES`) can only ever reach `slots`
//! in one of exactly three families: `2^u` (arity 2 or 4 with `bucket_size`
//! 1, 2, or 4 — themselves powers of two), `3 * 2^u` (arity 3 with
//! `bucket_size` 1, 2, or 4, or arity 2/4 with `bucket_size = 3`), or
//! `9 * 2^u` (arity 3 with `bucket_size = 3`). A configuration can only
//! land on a rung in its own family — so no arity-3 configuration, at any
//! buildable `bucket_size`, can ever land exactly on the 268,435,456-slot
//! rung (`2^28`, not divisible by 3) that `(2,4)` and `(4,4)` both reach at
//! the live account count; arity 3's nearest rung in its own `3 * 2^u`
//! family is 402,653,184 slots, half again as large. That is *why* the
//! pre-ADR-0034 `(3,4)` deployment had a bigger database than `(2,4)`: not
//! because arity itself moves `server_db` at equal `slots` (it provably
//! does not — see the previous paragraph), but because arity 3 cannot
//! reach the smaller rung that arity 2 (and 4) can. Once two configurations
//! *do* share a rung, arity stops mattering for database size and starts
//! mattering only for the hint: it moves the hint total, proportional to
//! `sqrt(arity)` — the wrong direction for a browser client already
//! fighting a first load in the hundreds of MB (554 MB at the deployed
//! `(2,4)`; `(4,4)` reaches the identical database on the same rung for
//! ~231 MB more hint — see
//! `tests::arity_on_a_shared_rung_only_moves_the_hint`). ADR-0030 and
//! ADR-0034 record the full argument; this module is what makes it
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
//!    its target, but only an actual cuckoo-eviction run can say it
//!    *does*. Never runs under `cargo test` (too slow) — see that
//!    function's docs.
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
/// account as of the 2026-07-26 bootstrap. Source of truth: `docs/deploy.md`
/// §5.3 ("every one of mainnet's 200,503,969 funded accounts") — that count
/// is pinned-and-dated, not still current: it is the 2026-07-26 bootstrap
/// count, and the served set was 204,714,034 on 2026-09-03
/// (`docs/deployment-numbers.md` §4.1); kept fixed here so the geometry
/// arithmetic tests stay byte-stable. §5.3's *geometry* line quoted
/// alongside it ("100663296 buckets, server DB 35.43 GB, load 0.498") is
/// stale for a separate reason: it recorded the pre-ADR-0034 `(arity 3,
/// bucket_size 4)` deployment, kept here as an explicit historical pin
/// (`tests::pre_adr_0034_configuration_pins_historical_geometry`). The live
/// geometry since ADR-0034 is `(arity 2, bucket_size 4)` — 67,108,864
/// buckets, 23.62 GB, load 0.7469 — see
/// `tests::deployed_configuration_pins_live_geometry`. Named, not repeated
/// as a bare literal, so a future re-export (or a re-run of the §2.1 gate
/// query) is a one-line change.
pub const LIVE_COMPLETE_SET_ACCOUNTS: u64 = 200_503_969;

/// Default `--fingerprint-bits`: unchanged across every deployment and
/// every ADR in this repo (32-bit SCF positioning fingerprint).
const DEFAULT_FINGERPRINT_BITS: u32 = 32;

/// Arity of the live deployment (`crates/risepir-rpc/src/mainnet.rs`'s
/// `const ARITY`). Mirrored, not imported: `risepir-rpc` does not export
/// it (it is a `const`, private to that module), and depending on the
/// whole `risepir-rpc` binary crate for two `u32` literals would be
/// disproportionate. Used only to flag [`GeometryRow::deployed`].
const DEPLOYED_ARITY: u32 = 2;
/// Bucket size of the live deployment (`crates/risepir-rpc/src/mainnet.rs`'s
/// `const BUCKET_SIZE`) — see [`DEPLOYED_ARITY`]'s docs for why this is a
/// mirror, not an import.
const DEPLOYED_BUCKET_SIZE: u32 = 4;

// The target load this sweep's headroom column measures against is *not*
// mirrored here. It used to be — a flat `3/4` pair copied from
// `risepir_proto::geometry`'s then-private constants — which was correct
// only for as long as the target actually was flat for every
// configuration. ADR-0031 made it per-`(arity, bucket_size)`
// (`min(0.75, 0.85 × segmented_cuckoo::MAX_LOAD_FACTOR)` at the time), at
// which point a copy here would have quietly overstated the headroom of
// exactly the three combinations that fix tightens — `(2,1)`, `(2,2)`,
// `(3,1)` — while every row this repo deployed or benched stayed right,
// which is the worst shape a stale mirror can take. ADR-0034 then retuned
// both numbers to `0.90`/`0.95` for the `(arity 2, bucket_size 4)`
// deployment, which inverts that split: ten of the twelve combinations now
// resolve below the flat cap, including the deployed row itself (see
// `risepir_proto::geometry::SAFETY_MARGIN_NUM`'s docs for the full list) —
// a mirror kept here would by now have silently overstated the headroom of
// the one row this whole module exists to report on correctly.
// `risepir_proto::geometry::effective_target_load` is now
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
    /// this configuration's own target load for
    /// (`risepir_proto::geometry::effective_target_load` — ADR-0031's
    /// per-row mechanism at ADR-0034's retuned `0.90`/`0.95`; `0.8645` for
    /// the deployed `(2,4)`, not a flat number for every row) — i.e. its
    /// capacity before the next doubling.
    pub max_accounts_at_target: u64,
    /// `(max_accounts_at_target / accounts - 1) * 100` — percentage growth
    /// still free before this geometry must re-bootstrap into a bigger one.
    pub headroom_pct: f64,
    /// `server_db` of the configuration one account past
    /// `max_accounts_at_target` — i.e. what today's account count grows
    /// into once `max_accounts_at_target` is exceeded and `num_buckets`
    /// must double.
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
    let g = Geometry::for_accounts(
        accounts,
        arity,
        bucket_size,
        fingerprint_bits,
        codec,
        Backend::Simple,
    )?;
    let s = g.sizes(Backend::Simple, accounts);
    let arity64 = u64::from(arity);

    // Largest accounts' with this exact num_buckets: the floor-division
    // form of the same `accounts/slots <= target` test `for_accounts`
    // applies internally, asking that module for this configuration's own
    // target rather than assuming a flat one (see the note above).
    let (target_num, target_den) =
        risepir_proto::geometry::effective_target_load(arity, bucket_size);
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
            rows.push(compute_row(
                cfg.accounts,
                arity,
                bucket_size,
                cfg.fingerprint_bits,
                &codec,
            )?);
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
    // Not "target load 0.90" either: since ADR-0031 the target is per-row
    // (`min(GLOBAL_TARGET, SAFETY_MARGIN × segmented_cuckoo::MAX_LOAD_FACTOR)`
    // — `min(0.90, 0.95 × MAX_LOAD_FACTOR)` since ADR-0034's retune — so a
    // single number in the header would be wrong for exactly the rows where
    // it matters. ADR-0034 inverted which rows those are: ten of the twelve
    // `(arity, bucket_size)` combinations now resolve *below* 0.90,
    // including the deployed `(2,4)` (0.8645) and every configuration this
    // repo benches; only `(4,3)` and `(4,4)` still resolve to the flat 0.90
    // (see `risepir_proto::geometry::SAFETY_MARGIN_NUM`'s docs for the full
    // split — before ADR-0034 it was the other way around: only `(2,1)`,
    // `(2,2)`, `(3,1)` did not resolve to the flat 0.75, and none of those
    // three was used anywhere in this repo).
    writeln!(
        out,
        "accounts = {accounts} (target load per row: min(0.90, 0.95 x MAX_LOAD_FACTOR); \
         ADR-0030, ADR-0031, ADR-0034)"
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
/// point (`(2,4)`, ADR-0034) first, then the previously-deployed `(3,4)`
/// and the arity-3 alternatives ADR-0030's recommendation compares against
/// it (including `bucket_size = 6`, which — see [`GeometryRow::buildable`]
/// — this fill-check is exactly what catches as unconstructible today),
/// and one point at arity 4 so the "arity does not move the database size
/// at a shared rung" claim is checked empirically against the deployed
/// point, not just arithmetically.
pub const DEFAULT_FILL_CANDIDATES: [FillCandidate; 5] = [
    FillCandidate {
        arity: 2,
        bucket_size: 4,
    },
    FillCandidate {
        arity: 3,
        bucket_size: 4,
    },
    FillCandidate {
        arity: 3,
        bucket_size: 6,
    },
    FillCandidate {
        arity: 3,
        bucket_size: 3,
    },
    FillCandidate {
        arity: 4,
        bucket_size: 4,
    },
];

/// Default `--fill-accounts` scale: comfortably fits 16 GB, and matches
/// `xtask::bench::BenchConfig::default()`'s own largest scale, so this
/// tool's fill-check sits next to numbers already measured elsewhere in
/// this repo rather than inventing a new unrelated scale. That scale was
/// originally chosen (pre-ADR-0034) as the arity-3 `segment_rows =
/// 2^20`-at-75%-load point `docs/verification.md` already reports numbers
/// at; kept unchanged for comparability even though it lands differently
/// under the deployed `(arity 2, bucket_size 4)` geometry (`num_buckets =
/// 2^22`, `segment_rows = 2^21`, load 0.5625 — see
/// `xtask::bench::BenchConfig::default`'s doc for the full accounting).
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
fn fill_store<S: IndexScheme>(
    mut store: CuckooKVStore<S>,
    accounts: u64,
    codec: &ValueCodec,
) -> FillOutcome {
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
pub fn fill_check_one(
    candidate: FillCandidate,
    accounts: u64,
    fingerprint_bits: u32,
    codec: &ValueCodec,
) -> FillCheckResult {
    let outcome = (|| -> Result<FillOutcome, String> {
        let geom = Geometry::for_accounts(
            accounts,
            candidate.arity,
            candidate.bucket_size,
            fingerprint_bits,
            codec,
            Backend::Simple,
        )
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
            other => Err(format!(
                "fill_check_one: arity must be 2, 3, or 4, got {other}"
            )),
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
pub fn fill_check(
    candidates: &[FillCandidate],
    accounts: u64,
    fingerprint_bits: u32,
) -> Vec<FillCheckResult> {
    let codec = crate::bench::value_codec();
    candidates
        .iter()
        .map(|&c| fill_check_one(c, accounts, fingerprint_bits, &codec))
        .collect()
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

    /// Pins the deployed configuration (`arity 2, bucket_size 4`, ADR-0034)
    /// at the live complete-set account count: 67,108,864 buckets, load
    /// 0.7469, a 23,622,320,128 B server DB, and a 553,819,200 B total
    /// hint — every figure taken from this tool's own `compute_row`, not
    /// copied from the ADR draft (whose independent hand-arithmetic landed
    /// on a slightly different hint figure, 553,821,600 B; the value
    /// pinned here is the one the code actually produces).
    #[test]
    fn deployed_configuration_pins_live_geometry() {
        let row = compute_row(LIVE_COMPLETE_SET_ACCOUNTS, 2, 4, 32, &codec())
            .expect("deployed configuration must size");
        assert_eq!(row.num_buckets, 67_108_864);
        assert!(
            (row.load_factor - 0.7469).abs() < 1e-4,
            "load_factor = {}",
            row.load_factor
        );
        assert_eq!(row.server_db, 23_622_320_128);
        assert_eq!(row.hint_total, 553_819_200);
        assert!(row.deployed);
        assert!(row.buildable);
    }

    /// The pre-ADR-0034 deployment (`arity 3, bucket_size 4`), kept as an
    /// explicit historical pin: the exact figures recorded in
    /// `docs/deploy.md` §5.3 and re-derived for ADR-0030 — 100,663,296
    /// buckets, load 0.4980, a 35,433,480,192 B server DB, and an
    /// 830,728,800 B total hint — still reproduce bit-for-bit from this
    /// tool, but this configuration is no longer the one [`GeometryRow`]
    /// flags [`GeometryRow::deployed`].
    #[test]
    fn pre_adr_0034_configuration_pins_historical_geometry() {
        let row = compute_row(LIVE_COMPLETE_SET_ACCOUNTS, 3, 4, 32, &codec())
            .expect("historical configuration must size");
        assert_eq!(row.num_buckets, 100_663_296);
        assert!(
            (row.load_factor - 0.4980).abs() < 1e-4,
            "load_factor = {}",
            row.load_factor
        );
        assert_eq!(row.server_db, 35_433_480_192);
        assert_eq!(row.hint_total, 830_728_800);
        assert!(
            !row.deployed,
            "ADR-0034 moved the deployed configuration to (2,4)"
        );
        assert!(row.buildable);
    }

    /// Pins the brief's own arithmetic: switching to `arity 4, bucket_size
    /// 4` at the live account count gives a 23,622,320,128 B server DB and
    /// a 784,502,400 B total hint — the brief's computed 23.62 GB / 784.5
    /// MB, reproduced exactly. (Its *conclusion* — that this is a good
    /// trade — is what the other tests below refute.)
    #[test]
    fn arity4_bucket4_pins_the_briefs_arithmetic() {
        let row = compute_row(LIVE_COMPLETE_SET_ACCOUNTS, 4, 4, 32, &codec())
            .expect("arity 4 / bucket_size 4 must size");
        assert_eq!(row.server_db, 23_622_320_128);
        assert_eq!(row.hint_total, 784_502_400);
        assert!(!row.deployed);
        assert!(
            row.buildable,
            "bucket_size 4 is within segmented_cuckoo::SUPPORTED_BUCKET_SIZES"
        );
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
        assert_eq!(
            row2.num_buckets, row4.num_buckets,
            "arity 2 and 4 must reach the same num_buckets at this scale"
        );
        assert_eq!(
            row2.server_db, row4.server_db,
            "server_db must depend only on slots/cells_per_slot, not arity"
        );
        assert_ne!(
            row2.hint_total, row4.hint_total,
            "but the hint DOES move with arity (sqrt_arity_hint_law)"
        );
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
        assert_eq!(
            row3.server_db, row4.server_db,
            "both must land at the same 301,989,888-slot database size"
        );

        let ratio = row4.hint_total as f64 / row3.hint_total as f64;
        let expected = (4.0_f64 / 3.0).sqrt();
        assert!(
            (ratio - expected).abs() / expected < 0.01,
            "hint_total(4)/hint_total(3) = {ratio:.4}, expected sqrt(4/3) = {expected:.4} within 1%"
        );
    }

    /// Replaces `arity4_bucket4_sits_on_the_cliff`: that test's premise
    /// (`headroom_pct < 1.0` for `arity 4, bucket_size 4`) was an artefact
    /// of the flat 0.75 target, which ADR-0034's retune to 0.90/0.95 has
    /// erased — `(4,4)` now carries ≈20.5% headroom
    /// (`max_accounts_at_target` 241,591,910 against the pinned
    /// `LIVE_COMPLETE_SET_ACCOUNTS` of 200,503,969 — the 2026-07-26
    /// bootstrap count kept fixed for this test, not today's 204,714,034),
    /// not a cliff. What is still true, and load-bearing for ADR-0034:
    /// `(2,4)` (the deployed configuration) and `(4,4)` (the brief's
    /// original swap target) sit on the exact same 67,108,864-bucket /
    /// 268,435,456-slot rung at the live account count
    /// (`db_size_depends_on_slots_not_arity` already proves this in
    /// general — this test pins the deployment-specific instance of it),
    /// so their `server_db` is bit-identical, and the only real cost of
    /// choosing `(4,4)` over `(2,4)` on that shared rung is 230,683,200 B
    /// (~231 MB) more hint. Once a rung is chosen by slots, the arity *on*
    /// that rung is a hint-size decision, not a database-size or headroom
    /// decision.
    #[test]
    fn arity_on_a_shared_rung_only_moves_the_hint() {
        let c = codec();
        let deployed = compute_row(LIVE_COMPLETE_SET_ACCOUNTS, 2, 4, 32, &c).unwrap();
        let alt = compute_row(LIVE_COMPLETE_SET_ACCOUNTS, 4, 4, 32, &c).unwrap();

        assert!(
            deployed.deployed,
            "(2,4) must be the flagged deployed configuration"
        );
        assert!(!alt.deployed);
        assert_eq!(
            deployed.num_buckets, alt.num_buckets,
            "(2,4) and (4,4) must land on the same rung"
        );
        assert_eq!(
            deployed.server_db, alt.server_db,
            "a shared rung means identical server_db"
        );

        let hint_delta_bytes = alt.hint_total - deployed.hint_total;
        assert_eq!(
            hint_delta_bytes, 230_683_200,
            "(4,4) must cost exactly this much more hint than (2,4)"
        );

        // The cliff is gone: both configurations now carry comparable,
        // healthy headroom under the retuned target.
        assert!(
            alt.headroom_pct > 15.0,
            "headroom must no longer be a cliff, got {:.2}%",
            alt.headroom_pct
        );

        // Target-load self-check (kept from the test this replaces):
        // capacity fits this num_buckets, one more account does not.
        let at_capacity = Geometry::for_accounts(
            alt.max_accounts_at_target,
            alt.arity,
            alt.bucket_size,
            32,
            &c,
            Backend::Simple,
        )
        .unwrap();
        assert_eq!(
            at_capacity.num_buckets, alt.num_buckets,
            "capacity must still fit this num_buckets"
        );
        let past_capacity = Geometry::for_accounts(
            alt.max_accounts_at_target + 1,
            alt.arity,
            alt.bucket_size,
            32,
            &c,
            Backend::Simple,
        )
        .unwrap();
        assert!(
            past_capacity.num_buckets > alt.num_buckets,
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
    /// `effective_target_load` now caps it at `0.95 × 0.48 = 0.456`
    /// (ADR-0034's retuned margin; ADR-0031's original margin gave
    /// `0.85 × 0.48 = 0.408`), so the sweep reports a row that can actually
    /// be filled.
    ///
    /// This is the sweep-level regression test for that fix: it asserts the
    /// property (sized at or under the ceiling), not the exact figure, so
    /// it keeps its meaning if the margin is ever retuned.
    #[test]
    fn the_motivating_configuration_is_now_sized_below_its_ceiling() {
        let row = compute_row(LIVE_COMPLETE_SET_ACCOUNTS, 2, 1, 32, &codec()).unwrap();
        assert_eq!(row.load_ceiling, Some(0.48));
        assert!(
            row.buildable,
            "bucket_size 1 is inside SUPPORTED_BUCKET_SIZES"
        );
        assert!(
            row.load_factor <= 0.48,
            "ADR-0031 must size (2,1) at or under its published ceiling, got {}",
            row.load_factor
        );
        assert!(
            row.fillable(),
            "nothing left to flag once it is sized correctly"
        );
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
        assert!(
            !row.fillable(),
            "must be flagged: sized above what the table holds"
        );
        assert_eq!(row_annotation(&row), "  ‡");
    }

    /// ...and must not cry wolf on the ones that are fine, including the
    /// deployed geometry and the arithmetic-only rows that have no
    /// published ceiling to be measured against.
    #[test]
    fn fillable_rows_are_not_flagged() {
        for (arity, bucket_size) in [(2u32, 2u32), (2, 3), (2, 4), (3, 1), (3, 3), (3, 4), (4, 4)] {
            let row =
                compute_row(LIVE_COMPLETE_SET_ACCOUNTS, arity, bucket_size, 32, &codec()).unwrap();
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
