//! Geometry calculator: the single source of truth for sizing a RisePIR
//! deployment from an account count.
//!
//! # Purpose
//!
//! Everything downstream — the SCF store, the per-segment PIR matrices, the
//! wire bundles — is sized from a [`Geometry`]. In particular
//! `plaintext_bits` is *always* derived here from `ikpir_common::pir_params`
//! (the noise-bound analysis), never hardcoded or guessed at a call site.
//!
//! # Formulas
//!
//! Verified against the upstream `results/*.csv` bench output — see
//! `docs/verification.md` ("closed forms reproduce the measured CSVs
//! exactly") — and pinned by the tests in this module. Note the brief this
//! project derives from had `R`/`C` swapped for SimplePIR; the formulas
//! below are the corrected ones.
//!
//! ```text
//! cells_per_slot = ceil((fingerprint_bits + value_bits) / plaintext_bits)
//! row_width      = bucket_size * cells_per_slot
//! segment_rows   = num_buckets / arity
//! slots          = num_buckets * bucket_size
//! server_db      = num_buckets * bucket_size * cells_per_slot * 4
//!
//! FrodoPIR (no reshape):
//!   hint/segment     = lwe_dim * row_width * 4
//!   query/segment    = segment_rows * 4
//!   response/segment = row_width * 4
//!   A/segment        = segment_rows * lwe_dim * 4
//!
//! SimplePIR (near-square reshape):
//!   k = max(1, round(sqrt(segment_rows / row_width)))
//!   R = ceil(segment_rows / k)
//!   C = k * row_width
//!   hint/segment     = lwe_dim * C * 4
//!   query/segment    = R * 4
//!   response/segment = C * 4
//!   A/segment        = R * lwe_dim * 4
//! ```
//!
//! `lwe_dim` is read from `ikpir_common`'s own defaults
//! (`FrodoParams::DEFAULT_LWE_DIM` = 1566, `SimpleParams::DEFAULT_LWE_DIM` =
//! 1275) rather than repeated as a literal here, so this module cannot drift
//! from the backend it is sizing for.

use ikpir_common::backend::frodo::FrodoParams;
use ikpir_common::backend::simple::SimpleParams;
use ikpir_common::pir_params::{frodo_max_plaintext_bits, simple_max_plaintext_bits};
use segmented_cuckoo::MAX_LOAD_FACTOR;

use crate::value::ValueCodec;

/// Which LWE backend a [`Sizes`] computation targets.
///
/// `Frodo` is RisePIR-F (tall matrix, no reshape); `Simple` is RisePIR-S
/// (near-square reshape). `docs/verification.md` Correction 5 closes the
/// choice for this project (`RisePIR-S`); `Frodo` is kept for the
/// comparison row in the numbers table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// RisePIR-S: SimplePIR backend, reshaped to a near-square matrix.
    Simple,
    /// RisePIR-F: FrodoPIR backend, no reshape.
    Frodo,
}

/// Fully-derived SCF geometry for one deployment.
///
/// # Constraints
///
/// `arity` is always 2, 3, or 4. `num_buckets` is a power of two for
/// `arity` 2/4, or `3 * 2^t` for `arity` 3 (mirrors
/// `segmented_cuckoo::CuckooKVStore`'s own constructors, which reject
/// anything else). `plaintext_bits` is derived, never hand-set — construct
/// via [`Geometry::for_accounts`], or, for pinning a specific known-good
/// configuration (as the tests below do), build the struct literal
/// directly with a `plaintext_bits` obtained from
/// `ikpir_common::pir_params`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// Candidate buckets per key: 2, 3, or 4.
    pub arity: u32,
    /// Total buckets across all segments.
    pub num_buckets: u32,
    /// Slots per bucket.
    pub bucket_size: u32,
    /// Fingerprint width in bits.
    pub fingerprint_bits: u32,
    /// Value width in bits (the `risepir-proto` value codec's `value_bits`).
    pub value_bits: u32,
    /// PIR plaintext cell width in bits. Derived — see the struct-level
    /// constraints note.
    pub plaintext_bits: u32,
}

/// Derived byte/count sizes for one [`Geometry`] under one [`Backend`].
///
/// All `u64` fields are byte counts; the `u32` fields are counts of cells
/// or rows (never bytes) as named. `k`, `reshape_rows`, and
/// `reshape_row_width` apply only to [`Backend::Simple`] (SimplePIR's
/// reshape); they are `0` for [`Backend::Frodo`], which does not reshape.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sizes {
    /// Plaintext cells per slot: `ceil((fingerprint_bits + value_bits) / plaintext_bits)`.
    pub cells_per_slot: u32,
    /// Cells per row: `bucket_size * cells_per_slot`. Not bytes.
    pub row_width: u32,
    /// Buckets (rows of the per-segment matrix) per segment: `num_buckets / arity`.
    pub segment_rows: u32,
    /// SimplePIR reshape factor. `0` for [`Backend::Frodo`].
    pub k: u32,
    /// SimplePIR reshape row count `R`. `0` for [`Backend::Frodo`].
    pub reshape_rows: u32,
    /// SimplePIR reshape row width `C`, in cells. `0` for [`Backend::Frodo`].
    pub reshape_row_width: u32,
    /// Server hint size per segment, in bytes.
    pub hint_per_segment: u64,
    /// Client query size per segment, in bytes.
    pub query_per_segment: u64,
    /// Server response size per segment, in bytes.
    pub response_per_segment: u64,
    /// Public matrix `A` size per segment, in bytes.
    pub a_per_segment: u64,
    /// Whole-store flat cell array size, in bytes: `num_buckets * bucket_size * cells_per_slot * 4`.
    pub server_db: u64,
    /// Whole-store slot count: `num_buckets * bucket_size`. Not bytes.
    pub slots: u64,
    /// `accounts / slots`. Not meaningful (and not bounded to `[0, 1]`) if
    /// `accounts` was not the count `slots` was sized for.
    pub load_factor: f64,
}

/// Errors constructing a [`Geometry`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeomError {
    /// `arity` was not 2, 3, or 4.
    InvalidArity(u32),
    /// `bucket_size` was 0.
    InvalidBucketSize,
    /// `fingerprint_bits` or `value_bits` was 0.
    InvalidFieldWidth,
    /// The account count needs a `num_buckets` that does not fit in `u32`.
    TooManyAccounts,
}

impl std::fmt::Display for GeomError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidArity(a) => write!(f, "arity must be 2, 3, or 4, got {a}"),
            Self::InvalidBucketSize => write!(f, "bucket_size must be >= 1"),
            Self::InvalidFieldWidth => write!(f, "fingerprint_bits and value_bits must both be >= 1"),
            Self::TooManyAccounts => {
                write!(f, "account count requires a num_buckets that does not fit in u32")
            }
        }
    }
}

impl std::error::Error for GeomError {}

/// Flat ceiling `for_accounts` never sizes above, regardless of arity or
/// bucket_size, expressed as a numerator over [`TARGET_DEN`]:
/// `GLOBAL_TARGET_NUM / TARGET_DEN` = `7_500 / 10_000` = `0.75`.
///
/// `docs/plan.md` §9: "run at ~75% load for headroom." This used to be the
/// *only* term `for_accounts` sized against; it is now one of two terms
/// [`effective_target_load`] takes the `min` of — see that function and ADR-0031
/// (`docs/adr/README.md`) for why a flat cap survives alongside a
/// per-configuration one: this store mutates continuously inside a
/// ~12 s block budget while holding the write lock `/answer` also needs,
/// it cannot grow in place once built (`RisePirServer::full_rebuild` only
/// ever re-derives hints for the *existing* geometry), and a `TableFull`
/// mid-block is therefore a full re-bootstrap outage rather than a
/// slowdown — so the flat 0.75 headroom is kept regardless of what any
/// single `(arity, bucket_size)` could theoretically reach.
const GLOBAL_TARGET_NUM: u128 = 7_500;

/// Common denominator for every target-load fraction in this module:
/// hundredths of a percent. Large enough to represent both
/// [`GLOBAL_TARGET_NUM`] and the margin-adjusted per-configuration ceiling
/// (`SAFETY_MARGIN_NUM` × a `MAX_LOAD_FACTOR` hundredth, itself out of
/// `100 * 100`) exactly, so [`effective_target_load`] never has to reduce
/// either side before comparing them.
const TARGET_DEN: u128 = 10_000;

/// Safety margin applied to `segmented_cuckoo::MAX_LOAD_FACTOR` before its
/// per-configuration ceiling is allowed to tighten [`GLOBAL_TARGET_NUM`] —
/// out of 100, so `SAFETY_MARGIN_NUM * (a MAX_LOAD_FACTOR hundredth)` lands
/// directly on the [`TARGET_DEN`] scale.
///
/// See [`effective_target_load`] and ADR-0031 for the measured blast
/// radius at this margin: exactly three `(arity, bucket_size)`
/// configurations bind — `(2,1)`, `(2,2)`, `(3,1)` — and none of them is
/// used anywhere in this repo (deployed and benched configurations are all
/// `(3,4)`), so every configuration this repo actually deploys or benches
/// is bit-identical to before this safety margin existed.
const SAFETY_MARGIN_NUM: u128 = 85;

/// `segmented_cuckoo::MAX_LOAD_FACTOR[arity - 2][bucket_size - 1]`,
/// converted once to an exact numerator over a denominator of 100 (e.g.
/// `0.48` -> `48`) so [`effective_target_load`] never touches `f64`.
/// Round-trips exactly for all 12 published entries — see
/// `max_load_factor_hundredths_round_trips_exactly` in the tests below.
///
/// Returns `None` when `MAX_LOAD_FACTOR` has no entry for `(arity,
/// bucket_size)`. `for_accounts` calls this *before* its own arity
/// validation runs (the `match` further down in its body), so `arity`
/// here is not yet guaranteed to be 2/3/4 — this function guards both
/// indices defensively with `checked_sub`/`get` rather than trusting that,
/// so it can never panic regardless. The real, reachable case is
/// `bucket_size` outside `1..=4`: those configurations are not
/// constructible by `segmented_cuckoo` (`SUPPORTED_BUCKET_SIZES` is
/// `1..=4`), but `for_accounts` has always accepted any `bucket_size >=
/// 1`, and a sweep tool on another branch deliberately sizes
/// `bucket_size` 5..16 for arithmetic-only exploration — so this falls
/// back to "no per-configuration ceiling known" rather than rejecting or
/// panicking.
fn max_load_factor_hundredths(arity: u32, bucket_size: u32) -> Option<u128> {
    let row = arity.checked_sub(2)? as usize;
    let col = bucket_size.checked_sub(1)? as usize;
    let ceiling = *MAX_LOAD_FACTOR.get(row)?.get(col)?;
    Some((ceiling * 100.0).round() as u128)
}

/// The load factor `for_accounts` actually sizes towards for one `(arity,
/// bucket_size)`, as an exact `(numerator, denominator)` pair over
/// `TARGET_DEN`:
///
/// ```text
/// target = min( GLOBAL_TARGET , SAFETY_MARGIN × MAX_LOAD_FACTOR[arity-2][bucket_size-1] )
/// ```
///
/// i.e. the smaller of the flat `GLOBAL_TARGET_NUM` cap and
/// `SAFETY_MARGIN_NUM` applied to `segmented_cuckoo`'s own published
/// achievable-load ceiling for this configuration — never the
/// per-configuration term alone, because upstream's own ceiling is
/// calibrated for a fill-once benchmark, not this store's continuous,
/// lock-held, no-in-place-growth mutation pattern (see
/// `GLOBAL_TARGET_NUM`'s docs). When `max_load_factor_hundredths` has
/// no ceiling to offer, this returns the flat cap alone — bit-identical to
/// every `(arity, bucket_size)` before this module considered a
/// per-configuration ceiling at all.
///
/// # Why this is `pub`
///
/// Because the alternative is a copy of it somewhere else, and this repo's
/// standing rule is that geometry is derived here, never hardcoded. The
/// `xtask geometry` sweep (ADR-0030) needs this exact ratio to compute its
/// headroom column — "how many more accounts fit before `num_buckets`
/// doubles" — and it previously mirrored a flat `3/4` in its own source,
/// which was correct only while the target *was* flat. Publishing the real
/// rule means that column tracks `for_accounts` by construction instead of
/// by a comment asking the next person to remember.
pub fn effective_target_load(arity: u32, bucket_size: u32) -> (u128, u128) {
    match max_load_factor_hundredths(arity, bucket_size) {
        Some(ceiling_hundredths) => (
            GLOBAL_TARGET_NUM.min(SAFETY_MARGIN_NUM * ceiling_hundredths),
            TARGET_DEN,
        ),
        None => (GLOBAL_TARGET_NUM, TARGET_DEN),
    }
}

impl Geometry {
    /// Picks the smallest `num_buckets` (respecting the arity shape
    /// constraint) with `accounts / (num_buckets * bucket_size) <=` the
    /// *effective* target load for this `(arity, bucket_size)` — see
    /// `effective_target_load` (private to this module): the smaller of a flat 0.75 cap and a
    /// safety margin on `segmented_cuckoo::MAX_LOAD_FACTOR`'s own
    /// published achievable-load ceiling for that configuration
    /// (ADR-0031). For every `(arity, bucket_size)` this repo actually
    /// deploys or benches, the effective target is still exactly 0.75 —
    /// the per-configuration term only ever tightens three combinations
    /// this repo does not use. Then derives `plaintext_bits` from
    /// `ikpir_common::pir_params` for `backend` at that geometry.
    ///
    /// Before ADR-0031, `for_accounts` sized every configuration against a
    /// single flat 0.75 with no regard for arity/bucket_size, which is
    /// unsound at the edges of the space: `segmented_cuckoo`'s own
    /// achievable load for `(arity=2, bucket_size=1)` is only ~0.48, so a
    /// flat 0.75 target could return a geometry too small for the store to
    /// actually be filled to (a measured failure at 1,051,458 of
    /// 1,500,000 inserts, 70.1%, before this fix — see ADR-0031).
    ///
    /// The whole target-load search — including the per-configuration
    /// ceiling, converted from `segmented_cuckoo::MAX_LOAD_FACTOR`'s `f64`
    /// table to exact integer hundredths once by
    /// `max_load_factor_hundredths` — is done in exact `u128` integer
    /// arithmetic, not `f64`, so it cannot drift from either bound by
    /// floating-point rounding at large account counts.
    ///
    /// # Errors
    ///
    /// [`GeomError::InvalidArity`] if `arity` is not 2, 3, or 4;
    /// [`GeomError::InvalidBucketSize`] if `bucket_size == 0`;
    /// [`GeomError::InvalidFieldWidth`] if `fingerprint_bits == 0` or
    /// `value_codec.value_bits() == 0`; [`GeomError::TooManyAccounts`] if
    /// the resulting `num_buckets` would not fit in `u32`.
    pub fn for_accounts(
        accounts: u64,
        arity: u32,
        bucket_size: u32,
        fingerprint_bits: u32,
        value_codec: &ValueCodec,
        backend: Backend,
    ) -> Result<Self, GeomError> {
        if bucket_size == 0 {
            return Err(GeomError::InvalidBucketSize);
        }
        let value_bits = value_codec.value_bits();
        if fingerprint_bits == 0 || value_bits == 0 {
            return Err(GeomError::InvalidFieldWidth);
        }

        // buckets_needed = ceil(accounts / (bucket_size * target))
        //                = ceil(accounts * target_den / (bucket_size * target_num))
        // where (target_num, target_den) is the *effective* target load for
        // this (arity, bucket_size) — see `effective_target_load`. `arity`
        // is not yet validated at this point (that happens in the `match`
        // below), but `effective_target_load` never panics on a bad one —
        // it just falls back to the flat cap — and an invalid arity's
        // `buckets_needed` here is discarded when that `match` returns
        // `GeomError::InvalidArity` anyway.
        let (target_num, target_den) = effective_target_load(arity, bucket_size);
        let numerator = u128::from(accounts) * target_den;
        let denominator = u128::from(bucket_size) * target_num;
        let buckets_needed = numerator.div_ceil(denominator);

        let num_buckets_u128 = match arity {
            2 | 4 => buckets_needed.max(u128::from(arity)).next_power_of_two(),
            3 => {
                let segment = buckets_needed.div_ceil(3).max(1).next_power_of_two();
                segment * 3
            }
            other => return Err(GeomError::InvalidArity(other)),
        };
        let num_buckets = u32::try_from(num_buckets_u128).map_err(|_| GeomError::TooManyAccounts)?;
        let segment_rows = num_buckets / arity;

        let plaintext_bits = match backend {
            Backend::Frodo => frodo_max_plaintext_bits(segment_rows),
            Backend::Simple => simple_max_plaintext_bits(
                segment_rows,
                bucket_size,
                fingerprint_bits,
                value_bits,
                SimpleParams::DEFAULT_SIGMA,
            ),
        };

        Ok(Self {
            arity,
            num_buckets,
            bucket_size,
            fingerprint_bits,
            value_bits,
            plaintext_bits,
        })
    }

    /// Derives per-segment and whole-store sizes for `backend` at this
    /// geometry. Uses `self.plaintext_bits` as given — it is not
    /// re-derived, so a `Geometry` built for one backend can (deliberately)
    /// be sized under the other for side-by-side comparison; see
    /// [`Backend`].
    ///
    /// `accounts` feeds only `Sizes::load_factor`; pass the same count used
    /// to build `self` via [`Geometry::for_accounts`] to get a meaningful
    /// ratio.
    ///
    /// # Panics
    ///
    /// If any intermediate size overflows its integer type. This is a
    /// deployer-configuration bug (e.g. an absurd `value_bits`), not
    /// attacker input — `Geometry` is never built from untrusted bytes —
    /// so a loud panic is preferable to silent wraparound.
    pub fn sizes(&self, backend: Backend, accounts: u64) -> Sizes {
        let cells_per_slot = self
            .fingerprint_bits
            .checked_add(self.value_bits)
            .expect("fingerprint_bits + value_bits overflowed u32")
            .div_ceil(self.plaintext_bits);
        let row_width = self
            .bucket_size
            .checked_mul(cells_per_slot)
            .expect("row_width = bucket_size * cells_per_slot overflowed u32");
        let segment_rows = self.num_buckets / self.arity;

        let slots = u64::from(self.num_buckets) * u64::from(self.bucket_size);
        let server_db = u64::from(self.num_buckets)
            .checked_mul(u64::from(self.bucket_size))
            .and_then(|v| v.checked_mul(u64::from(cells_per_slot)))
            .and_then(|v| v.checked_mul(4))
            .expect("server_db overflowed u64");

        let (k, reshape_rows, reshape_row_width, hint_per_segment, query_per_segment, response_per_segment, a_per_segment) =
            match backend {
                Backend::Frodo => {
                    let lwe_dim = u64::from(FrodoParams::DEFAULT_LWE_DIM);
                    let hint = lwe_dim * u64::from(row_width) * 4;
                    let query = u64::from(segment_rows) * 4;
                    let response = u64::from(row_width) * 4;
                    let a = u64::from(segment_rows) * lwe_dim * 4;
                    (0, 0, 0, hint, query, response, a)
                }
                Backend::Simple => {
                    let (k, r, c) = reshape_dims(segment_rows, row_width);
                    let lwe_dim = u64::from(SimpleParams::DEFAULT_LWE_DIM);
                    let hint = lwe_dim * u64::from(c) * 4;
                    let query = u64::from(r) * 4;
                    let response = u64::from(c) * 4;
                    let a = u64::from(r) * lwe_dim * 4;
                    (k, r, c, hint, query, response, a)
                }
            };

        Sizes {
            cells_per_slot,
            row_width,
            segment_rows,
            k,
            reshape_rows,
            reshape_row_width,
            hint_per_segment,
            query_per_segment,
            response_per_segment,
            a_per_segment,
            server_db,
            slots,
            load_factor: accounts as f64 / slots as f64,
        }
    }
}

/// `(k, R, C)` for SimplePIR's near-square reshape of an `n_rows x
/// row_width` segment: `k = max(1, round(sqrt(n_rows / row_width)))`, `R =
/// ceil(n_rows / k)`, `C = k * row_width`.
///
/// Mirrors `ikpir_common::backend::simple`'s private `reshape_dims`
/// bit-for-bit (same `f64` sqrt-then-round, same argument order) — that
/// function is `pub(crate)` in the upstream crate and not reachable from
/// here, so this is a from-scratch re-implementation, not a wrapper.
/// Verified identical by the pinning tests below, which reproduce
/// `ikpir_common`'s own bench CSVs (`docs/verification.md`).
fn reshape_dims(n_rows: u32, row_width: u32) -> (u32, u32, u32) {
    debug_assert!(n_rows > 0 && row_width > 0);
    let ratio = f64::from(n_rows) / f64::from(row_width);
    let k = ratio.sqrt().round().max(1.0) as u32;
    let reshape_rows = n_rows.div_ceil(k);
    let reshape_row_width = k
        .checked_mul(row_width)
        .expect("reshape_row_width = k * row_width overflowed u32");
    (k, reshape_rows, reshape_row_width)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `ValueCodec` decomposition summing to a 96-bit `value_bits` —
    /// `for_accounts`' tests below only exercise *sizing*, so the specific
    /// `(key_tag_bits, balance_bits, checksum_bits)` split never matters,
    /// only the sum.
    fn codec_96() -> ValueCodec {
        ValueCodec {
            key_tag_bits: 32,
            balance_bits: 64,
            checksum_bits: 0,
        }
    }

    /// Same, summing to 256 bits (matches [`pinned_arity4_65536`]'s
    /// `value_bits`).
    fn codec_256() -> ValueCodec {
        ValueCodec {
            key_tag_bits: 32,
            balance_bits: 208,
            checksum_bits: 16,
        }
    }

    /// Pinning test (arity 4, `num_buckets` 65536, `bucket_size` 4, `fp`
    /// 32, `value_bits` 256): reproduces the upstream bench CSV exactly for
    /// both backends. This is the regression that proves the formulas.
    #[test]
    fn pinned_arity4_65536() {
        let segment_rows = 65536 / 4;
        assert_eq!(segment_rows, 16384);

        let frodo_pb = frodo_max_plaintext_bits(segment_rows);
        assert_eq!(frodo_pb, 11);
        let frodo_geom = Geometry {
            arity: 4,
            num_buckets: 65536,
            bucket_size: 4,
            fingerprint_bits: 32,
            value_bits: 256,
            plaintext_bits: frodo_pb,
        };
        let frodo_sizes = frodo_geom.sizes(Backend::Frodo, 0);
        assert_eq!(frodo_sizes.cells_per_slot, 27);
        assert_eq!(frodo_sizes.row_width, 108);
        assert_eq!(frodo_sizes.segment_rows, 16384);
        assert_eq!(frodo_sizes.hint_per_segment, 676_512);

        let simple_pb =
            simple_max_plaintext_bits(segment_rows, 4, 32, 256, SimpleParams::DEFAULT_SIGMA);
        assert_eq!(simple_pb, 10);
        let simple_geom = Geometry {
            plaintext_bits: simple_pb,
            ..frodo_geom
        };
        let simple_sizes = simple_geom.sizes(Backend::Simple, 0);
        assert_eq!(simple_sizes.cells_per_slot, 29);
        assert_eq!(simple_sizes.row_width, 116);
        assert_eq!(simple_sizes.k, 12);
        assert_eq!(simple_sizes.reshape_rows, 1366);
        assert_eq!(simple_sizes.reshape_row_width, 1392);
        assert_eq!(simple_sizes.hint_per_segment, 7_099_200);
    }

    /// Pinning test at the mainnet extrapolation point: `arity=3,
    /// num_buckets=3*2^25, bucket_size=4, fp=32, value_bits=96`. Both
    /// backends land on `plaintext_bits==8` here (unlike the arity-4 point
    /// above, where they differ) — a useful second data point precisely
    /// because it does *not* exercise the same coincidence.
    #[test]
    fn pinned_mainnet_point() {
        let num_buckets = 3 * (1u32 << 25);
        let segment_rows = num_buckets / 3;
        assert_eq!(segment_rows, 1 << 25);

        let frodo_pb = frodo_max_plaintext_bits(segment_rows);
        let simple_pb =
            simple_max_plaintext_bits(segment_rows, 4, 32, 96, SimpleParams::DEFAULT_SIGMA);
        assert_eq!(frodo_pb, 8);
        assert_eq!(simple_pb, 8);

        let geom = Geometry {
            arity: 3,
            num_buckets,
            bucket_size: 4,
            fingerprint_bits: 32,
            value_bits: 96,
            plaintext_bits: simple_pb,
        };
        let simple_sizes = geom.sizes(Backend::Simple, 0);
        assert_eq!(simple_sizes.row_width, 64);
        assert_eq!(simple_sizes.k, 724);
        assert_eq!(simple_sizes.reshape_rows, 46346);
        assert_eq!(simple_sizes.reshape_row_width, 46336);
        assert_eq!(simple_sizes.server_db, 25_769_803_776);

        let frodo_geom = Geometry {
            plaintext_bits: frodo_pb,
            ..geom
        };
        let frodo_sizes = frodo_geom.sizes(Backend::Frodo, 0);
        assert_eq!(frodo_sizes.row_width, 64);
        assert_eq!(frodo_sizes.server_db, 25_769_803_776);
    }

    #[test]
    fn for_accounts_rejects_bad_arity() {
        for bad in [0u32, 1, 5, 6, 100] {
            assert_eq!(
                Geometry::for_accounts(1_000, bad, 4, 32, &codec_96(), Backend::Simple),
                Err(GeomError::InvalidArity(bad))
            );
        }
    }

    #[test]
    fn for_accounts_rejects_zero_bucket_size() {
        assert_eq!(
            Geometry::for_accounts(1_000, 4, 0, 32, &codec_96(), Backend::Simple),
            Err(GeomError::InvalidBucketSize)
        );
    }

    #[test]
    fn for_accounts_rejects_zero_field_widths() {
        assert_eq!(
            Geometry::for_accounts(1_000, 4, 4, 0, &codec_96(), Backend::Simple),
            Err(GeomError::InvalidFieldWidth)
        );
        let zero_codec = ValueCodec {
            key_tag_bits: 0,
            balance_bits: 0,
            checksum_bits: 0,
        };
        assert_eq!(
            Geometry::for_accounts(1_000, 4, 4, 32, &zero_codec, Backend::Simple),
            Err(GeomError::InvalidFieldWidth)
        );
    }

    #[test]
    fn for_accounts_enforces_arity_shape() {
        for arity in [2u32, 4] {
            let g = Geometry::for_accounts(1_234_567, arity, 4, 32, &codec_96(), Backend::Simple).unwrap();
            assert!(g.num_buckets.is_power_of_two());
            assert!(g.num_buckets >= arity);
        }
        let g = Geometry::for_accounts(1_234_567, 3, 4, 32, &codec_96(), Backend::Simple).unwrap();
        assert_eq!(g.num_buckets % 3, 0);
        assert!((g.num_buckets / 3).is_power_of_two());
    }

    #[test]
    fn for_accounts_never_exceeds_target_load() {
        for accounts in [1u64, 100, 98_304, 98_305, 196_608, 10_000_000] {
            for arity in [2u32, 3, 4] {
                for bucket_size in [1u32, 2, 4] {
                    let g = Geometry::for_accounts(accounts, arity, bucket_size, 32, &codec_96(), Backend::Simple)
                        .unwrap();
                    let sizes = g.sizes(Backend::Simple, accounts);
                    assert!(
                        sizes.load_factor <= 0.75 + 1e-9,
                        "accounts={accounts} arity={arity} bucket_size={bucket_size}: load_factor={} > 0.75",
                        sizes.load_factor
                    );
                }
            }
        }
    }

    /// The regression this whole module exists to prevent: picking
    /// `num_buckets = 65536` via `accounts` lands exactly on the pinned
    /// arity-4 configuration above.
    #[test]
    fn for_accounts_reproduces_pinned_num_buckets() {
        // num_buckets=65536 is the smallest power of two >= accounts/3 for
        // any accounts in (98304, 196608]; pick the upper boundary.
        let g = Geometry::for_accounts(196_608, 4, 4, 32, &codec_256(), Backend::Frodo).unwrap();
        assert_eq!(g.num_buckets, 65536);
        assert_eq!(g.plaintext_bits, 11);
    }

    proptest::proptest! {
        /// `sizes()` never panics across a wide sweep of geometries,
        /// including wide `value_bits` (up to 512). This no longer routes
        /// through `for_accounts` for the swept `value_bits` itself —
        /// `for_accounts` now takes a `ValueCodec`, not a raw `value_bits`,
        /// and the property under test is about `sizes()`'s robustness to
        /// any `value_bits`, not about any particular `(key_tag_bits,
        /// balance_bits, checksum_bits)` decomposition of it. `for_accounts`
        /// still derives a realistic `(num_buckets, plaintext_bits)` pair
        /// (num_buckets depends only on accounts/arity/bucket_size, never on
        /// value_bits) via a fixed small codec, and the swept `value_bits`
        /// is substituted in afterward — `Sizes` docs: "`plaintext_bits`
        /// ... is not re-derived", so building a `Geometry` whose
        /// `value_bits` disagrees with the `value_bits` that produced its
        /// `plaintext_bits` is an already-supported, deliberately-tested
        /// combination (see the pinned tests above, which do the same
        /// swap in the other direction).
        #[test]
        fn sizes_never_panics_on_for_accounts_output(
            accounts in 1u64..50_000_000,
            arity_idx in 0usize..3,
            bucket_size in 1u32..=4,
            fingerprint_bits in 8u32..=32,
            value_bits in 8u32..=512,
        ) {
            let arity = [2u32, 3, 4][arity_idx];
            let backend = if arity_idx % 2 == 0 { Backend::Simple } else { Backend::Frodo };
            let seed = Geometry::for_accounts(accounts, arity, bucket_size, fingerprint_bits, &codec_96(), backend).unwrap();
            let g = Geometry { value_bits, ..seed };
            let _ = g.sizes(backend, accounts);
        }
    }

    /// `max_load_factor_hundredths`'s `f64 -> integer hundredths`
    /// conversion round-trips exactly for all 12 published
    /// `MAX_LOAD_FACTOR` entries — the precondition for treating the
    /// result as exact rather than merely "close enough" (this module's
    /// whole point, per `for_accounts`'s docs, is doing this search in
    /// exact integer arithmetic end to end).
    #[test]
    fn max_load_factor_hundredths_round_trips_exactly() {
        for arity in [2u32, 3, 4] {
            for bucket_size in 1u32..=4 {
                let row = (arity - 2) as usize;
                let col = (bucket_size - 1) as usize;
                let original = MAX_LOAD_FACTOR[row][col];
                let hundredths = max_load_factor_hundredths(arity, bucket_size).unwrap_or_else(|| {
                    panic!("({arity},{bucket_size}) must have a MAX_LOAD_FACTOR entry")
                });
                assert_eq!(
                    hundredths as f64 / 100.0,
                    original,
                    "({arity},{bucket_size}): round-trip mismatch, hundredths={hundredths}, original={original}"
                );
            }
        }
    }

    /// `bucket_size` outside `1..=4` has no `MAX_LOAD_FACTOR` entry — a
    /// sweep tool on another branch deliberately sizes `bucket_size`
    /// 5..16 for arithmetic-only exploration, so `for_accounts` must keep
    /// accepting it rather than start rejecting what it always has. An
    /// out-of-range `arity` is reachable too, because `for_accounts` calls
    /// this before its own arity validation runs. Neither panics; both
    /// fall back to `None` ("no per-configuration ceiling known").
    #[test]
    fn max_load_factor_hundredths_falls_back_gracefully_out_of_range() {
        for arity in [2u32, 3, 4] {
            for bucket_size in [5u32, 6, 16, 1_000] {
                assert_eq!(max_load_factor_hundredths(arity, bucket_size), None);
            }
        }
        for arity in [0u32, 1, 5, 6, 100] {
            assert_eq!(max_load_factor_hundredths(arity, 4), None);
        }
    }

    /// The whole blast radius of ADR-0031's 0.85 safety margin, asserted
    /// rather than trusted: at margin 0.85, exactly three `(arity,
    /// bucket_size)` configurations bind the per-configuration ceiling
    /// below the flat 0.75 cap, and every other configuration in `2..=4 x
    /// 1..=4` stays at *exactly* 0.75 — bit-identical to before this
    /// module considered a per-configuration ceiling at all.
    #[test]
    fn effective_target_load_matches_measured_blast_radius() {
        // (arity, bucket_size) -> numerator over TARGET_DEN, i.e.
        // SAFETY_MARGIN_NUM * MAX_LOAD_FACTOR-as-hundredths:
        //   (2,1): 85 * 48 = 4_080  (0.408)
        //   (2,2): 85 * 83 = 7_055  (0.7055)
        //   (3,1): 85 * 85 = 7_225  (0.7225)
        let tight: [((u32, u32), u128); 3] = [((2, 1), 4_080), ((2, 2), 7_055), ((3, 1), 7_225)];
        for ((arity, bucket_size), expected_num) in tight {
            assert_eq!(
                effective_target_load(arity, bucket_size),
                (expected_num, TARGET_DEN),
                "({arity},{bucket_size}): expected the margined ceiling to bind"
            );
            assert!(
                expected_num < GLOBAL_TARGET_NUM,
                "({arity},{bucket_size}): must be strictly below the flat 0.75 cap"
            );
        }

        let flat: [(u32, u32); 9] = [
            (2, 3),
            (2, 4),
            (3, 2),
            (3, 3),
            (3, 4),
            (4, 1),
            (4, 2),
            (4, 3),
            (4, 4),
        ];
        for (arity, bucket_size) in flat {
            assert_eq!(
                effective_target_load(arity, bucket_size),
                (GLOBAL_TARGET_NUM, TARGET_DEN),
                "({arity},{bucket_size}): expected the flat 0.75 cap to stay exact"
            );
        }
    }

    /// "Nothing else moved": every configuration this repo actually
    /// deploys or benches — all `(arity=3, bucket_size=4)`, none of the
    /// three ADR-0031 tightens — must land on exactly the same
    /// `num_buckets` as before this fix (`docs/numbers.md` §4a,
    /// `docs/deploy.md` §5.3 / ADR-0023).
    #[test]
    fn for_accounts_deployed_and_bench_num_buckets_unchanged() {
        let cases: [(u64, u32); 4] = [
            (100_000, 49_152),
            (1_000_000, 393_216),
            (9_437_184, 3_145_728),
            (200_503_969, 100_663_296), // the live complete mainnet set
        ];
        for (accounts, expected_num_buckets) in cases {
            let g = Geometry::for_accounts(accounts, 3, 4, 32, &codec_96(), Backend::Simple).unwrap();
            assert_eq!(
                g.num_buckets, expected_num_buckets,
                "accounts={accounts}: num_buckets must be bit-identical to pre-ADR-0031"
            );
        }
    }

    /// The bug this module exists to fix, end to end. Before ADR-0031,
    /// `(arity=2, bucket_size=1)` sized against the flat 0.75 cap alone,
    /// even though `segmented_cuckoo`'s own achievable load there
    /// (`MAX_LOAD_FACTOR[0][0]`) is only ~0.48 — measured (outside this
    /// suite, ADR-0031) to fail a real fill at 1,051,458 of 1,500,000
    /// inserts (70.1%). After the fix, both the sized load and an actual
    /// fill of a real store must respect the achievable ceiling.
    #[test]
    fn geometry_for_accounts_2_1_is_actually_fillable() {
        use segmented_cuckoo::Segmented2aryCuckooKVStore;

        // 300,000, not the 1,500,000 the ADR measured: both size above the
        // 0.48 ceiling under the old flat-0.75 rule (0.5722 and 0.7153
        // respectively), so both are genuine regression cases, and the
        // smaller one fills a fifth as many slots in a debug build — this
        // is the `cargo test --workspace` gate, which CLAUDE.md wants fast.
        // Note 500,000 would NOT do: the old rule happens to land it at
        // 0.4768, just under the ceiling, so it would silently pass either
        // way.
        let accounts = 300_000u64;
        let g = Geometry::for_accounts(accounts, 2, 1, 32, &codec_96(), Backend::Simple).unwrap();

        let sizes = g.sizes(Backend::Simple, accounts);
        assert!(
            sizes.load_factor <= MAX_LOAD_FACTOR[0][0] + 1e-9,
            "sized load {} exceeds the (arity=2, bucket_size=1) achievable ceiling {}",
            sizes.load_factor,
            MAX_LOAD_FACTOR[0][0]
        );

        let mut store = Segmented2aryCuckooKVStore::new(
            g.num_buckets,
            g.bucket_size,
            g.fingerprint_bits,
            g.value_bits,
            g.plaintext_bits,
        )
        .expect("Geometry::for_accounts must always produce a constructible store");

        let value = vec![0u8; store.value_size_in_bytes()];
        for i in 0..accounts {
            let mut addr = [0u8; 20];
            addr[12..].copy_from_slice(&i.to_be_bytes());
            let key = crate::keccak256(&addr);
            store
                .insert(key, &value)
                .unwrap_or_else(|e| panic!("insert #{i} of {accounts} failed: {e}"));
        }
        assert_eq!(store.num_items(), accounts, "not every insert landed");
    }
}
