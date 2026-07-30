//! Post-bootstrap snapshot audit (ADR-0040): during snapshot ingest,
//! reservoir-sample a handful of `(address, ingested-balance)` pairs from
//! the rows actually being ingested — a single streaming pass, never a
//! second read of a ~200M-row export — then, once PIR setup finishes,
//! verify each sampled pair against the same multi-provider quorum
//! `crate::hard_refresh` uses, at the exact block the snapshot declares
//! itself exact at. This is what stops the boundary-error finding
//! (`docs/deploy.md` §2.1, ADR-0040) from silently reappearing in a
//! *future* export undetected: every complete-set bootstrap now measures
//! and discloses its own residual error rate instead of assuming the
//! dataset is exact.
//!
//! # What this reports, and what it deliberately does not do
//!
//! [`verify`] logs one summary line, sets the one-line `GET /healthz`
//! summary (`risepir_http::NodeState::set_snapshot_audit_line`), and
//! writes a small sidecar next to `--state` so a later restart that only
//! *loads* the state file (no fresh ingest to sample from) can still
//! report the last measurement instead of the field silently going blank.
//! A high measured rate is reported loudly ([`is_alarming`]) but never
//! refuses to serve — the same reasoning ADR-0027 already established for
//! the cross-provider reconcile check: a rate-limited or occasionally
//! wrong third-party data source must not become this deployment's
//! outage, and the honest response to finding a problem in the input data
//! is disclosure with a number, not a self-inflicted denial of service.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use risepir_feed::rpc::RpcClient;
use risepir_http::NodeState;
use risepir_proto::Balance;

use crate::hard_refresh::{hex20, quorum, Quorum};

/// How many sampled addresses [`verify`] checks against the configured
/// providers concurrently — same reasoning and same order of magnitude as
/// `crate::hard_refresh::CONCURRENT_ADDRESS_CHECKS`: the default sample
/// size (512) times the provider count is enough round trips that a
/// fully serial pass would take minutes for no benefit, and this is a
/// background task that never blocks serving or following either way.
const AUDIT_CONCURRENCY: usize = 8;

// ─── Dependency-free, non-cryptographic randomness ───────────────────────

/// SplitMix64 — a tiny, fast, dependency-free PRNG; the same algorithm
/// `risepir_feed`'s mock chain already uses for its own reproducible
/// stream, kept as an independent copy here rather than adding a
/// cross-crate dependency for ~10 lines of a well-known public-domain
/// algorithm. **Not security-sensitive**: this only decides which
/// addresses get spot-checked, never anything cryptographic — the LWE
/// secret is a completely different, much more strictly sourced code path
/// (`crates/risepir-wasm/src/entropy.rs`).
#[derive(Clone)]
struct SplitMix64(u64);

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// A best-effort, non-cryptographic random `u64` to seed the reservoir
/// sampler when nothing else pins one. Mixes wall-clock time through a
/// [`std::collections::hash_map::RandomState`]-seeded hasher — the same
/// OS-entropy-backed construction `HashMap`'s own DoS-resistant hashing
/// already relies on — so this needs no new dependency for randomness
/// that is explicitly not security-sensitive (see `SplitMix64`'s docs).
/// The caller logs whatever this returns, so a given bootstrap's sample is
/// reproducible after the fact even though the seed itself is not chosen
/// deterministically.
pub fn random_seed() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = RandomState::new().build_hasher();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hasher.write_u128(nanos);
    hasher.finish()
}

// ─── Reservoir sampling ───────────────────────────────────────────────────

/// A streaming reservoir sampler (Algorithm R) over `(address,
/// ingested-balance)` pairs: a uniform random sample of up to `capacity`
/// items drawn from a stream of unknown, possibly huge length, in exactly
/// one pass — the shape a ~200M-row snapshot ingest needs, since a second
/// pass over the shards to sample from is exactly what this exists to
/// avoid. Seeded (never OS-random directly), so a run is reproducible
/// from its logged seed.
pub struct ReservoirSampler {
    capacity: usize,
    seen: u64,
    reservoir: Vec<([u8; 20], Balance)>,
    rng: SplitMix64,
}

impl ReservoirSampler {
    /// A sampler that keeps at most `capacity` items, seeded with `seed`.
    /// `capacity == 0` makes every [`Self::observe`] call a permanent,
    /// cheap no-op — the `--snapshot-audit-samples 0` disable path.
    pub fn new(capacity: usize, seed: u64) -> Self {
        Self {
            capacity,
            seen: 0,
            reservoir: Vec::with_capacity(capacity),
            rng: SplitMix64::new(seed),
        }
    }

    /// Offers one more `(address, balance)` pair from the stream. The
    /// first `capacity` offers are always kept; every offer after that
    /// replaces a uniformly-random existing slot with probability
    /// `capacity / seen`, the textbook Algorithm R invariant that makes
    /// the final reservoir a uniform sample of everything ever offered,
    /// without needing to know the stream's length in advance.
    pub fn observe(&mut self, addr: [u8; 20], balance: Balance) {
        // `seen` counts the stream, independent of `capacity` — see this
        // method's and `Self::seen`'s docs — so it increments even when
        // `capacity == 0` makes everything after it a no-op.
        self.seen += 1;
        if self.capacity == 0 {
            return;
        }
        if self.reservoir.len() < self.capacity {
            self.reservoir.push((addr, balance));
        } else {
            let j = self.rng.next_u64() % self.seen;
            if (j as usize) < self.capacity {
                self.reservoir[j as usize] = (addr, balance);
            }
        }
    }

    /// How many items have been [`Self::observe`]d so far — the stream
    /// length seen, not the (capped) reservoir size.
    pub fn seen(&self) -> u64 {
        self.seen
    }

    /// Consumes the sampler and returns its current reservoir: exactly
    /// `min(capacity, seen)` items.
    pub fn into_sample(self) -> Vec<([u8; 20], Balance)> {
        self.reservoir
    }
}

// ─── Wilson score interval ────────────────────────────────────────────────

/// The 95% two-sided z-score, `Φ⁻¹(0.975)`.
const Z_95: f64 = 1.959_963_984_540_054;

/// 95% Wilson score confidence interval for a binomial proportion `x / n`
/// (`x` disagreements out of `n` checked). Chosen over the naive normal
/// ("Wald") approximation because it stays inside `[0, 1]` and remains
/// sane at small `n` or extreme `p̂` — the audit's default of 512 samples
/// routinely sees `x = 0` or `x = 1`, where a Wald interval degenerates
/// toward a point or goes negative.
///
/// Returns `(lo, hi)` as fractions in `[0, 1]`. `n == 0` returns
/// `(0.0, 1.0)` — a totally uninformative interval, the honest answer to
/// "confidence interval for zero data", rather than a division by zero.
pub fn wilson_interval(x: u64, n: u64) -> (f64, f64) {
    if n == 0 {
        return (0.0, 1.0);
    }
    let n = n as f64;
    let x = x as f64;
    let p = x / n;
    let z2 = Z_95 * Z_95;
    let denom = 1.0 + z2 / n;
    let center = (p + z2 / (2.0 * n)) / denom;
    let half = (Z_95 / denom) * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt();
    ((center - half).max(0.0), (center + half).min(1.0))
}

// ─── The audit record and its sidecar ────────────────────────────────────

/// One completed audit's counts — everything the sidecar persists, and
/// everything [`wilson_interval`]/reporting needs. Rates and confidence
/// intervals are deliberately *not* stored: they are cheap to recompute
/// from `checked`/`disagreed` on every read, so there is no way for a
/// persisted rate to drift from what the same formula would compute
/// afresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuditRecord {
    /// How many sampled addresses got a usable (quorum-agreed) chain
    /// value at `block`.
    pub checked: u64,
    /// Of those, how many disagreed with the value ingested from the
    /// snapshot.
    pub disagreed: u64,
    /// The block the check ran at (`--snapshot-block`).
    pub block: u64,
    /// The reservoir sampler's seed, for a human to reconstruct which
    /// addresses were sampled after the fact.
    pub seed: u64,
}

impl AuditRecord {
    /// Point-estimate disagreement rate `disagreed / checked`, or `0.0` if
    /// nothing was checked.
    pub fn rate(&self) -> f64 {
        if self.checked == 0 {
            0.0
        } else {
            self.disagreed as f64 / self.checked as f64
        }
    }

    /// 95% Wilson CI for [`Self::rate`], freshly computed from the stored
    /// counts every time.
    pub fn wilson(&self) -> (f64, f64) {
        wilson_interval(self.disagreed, self.checked)
    }
}

/// The point-estimate rate above which [`is_alarming`] considers a
/// finding loud enough to escalate. Deliberately **not** zero — see that
/// function's docs for why comparing against literal zero would fire on
/// almost every real run — and deliberately well above the ~0.33%
/// population-wide rate ADR-0040's own measurement disclosed for the live
/// deployment's snapshot generation: that number is the *expected*
/// baseline for this specific export, already disclosed in the ADR and in
/// `docs/deploy.md`, not itself a fresh anomaly worth re-alarming on at
/// every single bootstrap forever. Set to roughly 3x that baseline —
/// loose enough that reproducing the known baseline stays quiet, tight
/// enough to catch a real regression (a materially different, and worse,
/// export).
const ALARM_THRESHOLD: f64 = 0.01; // 1%

/// Whether `record`'s finding is loud enough to warrant a startup
/// `WARNING` rather than a plain informational report line: 95%-confident
/// evidence (the Wilson interval's own lower bound) that the true
/// disagreement rate exceeds `ALARM_THRESHOLD`.
///
/// This is **not** simply "the lower bound is above zero" — that would
/// almost always be true. The Wilson score interval is derived by
/// inverting a hypothesis test against a candidate rate `p0`; at `p0 = 0`
/// that test is degenerate (any observed disagreement at all is
/// "infinitely" inconsistent with a true rate of exactly zero), so the
/// lower bound is exactly `0.0` when `disagreed == 0` and **strictly
/// positive** for **any** `disagreed >= 1`, however large `checked` is —
/// comparing against literal zero would therefore flag nearly every audit
/// that ever finds a single disagreement, including ones that merely
/// reproduce this deployment's own already-disclosed baseline, which
/// would train an operator to ignore the line. Comparing the same lower
/// bound against a small positive `p0` instead asks the question that
/// actually matters: "are we 95% confident the true rate exceeds this
/// threshold" — which degrades gracefully back toward "any evidence at
/// all" only as `ALARM_THRESHOLD` itself shrinks toward zero.
///
/// Never used to refuse service (see the module docs) — only to decide
/// how loud the one report line is.
pub fn is_alarming(record: &AuditRecord) -> bool {
    record.wilson().0 > ALARM_THRESHOLD
}

fn format_pct(fraction: f64) -> String {
    format!("{:.2}%", fraction * 100.0)
}

/// The compact one-line value `GET /healthz` reports for this audit
/// (`risepir_http::NodeState::set_snapshot_audit_line`) — kept to one line
/// per the endpoint's plain-text, `key=value`-per-line convention
/// (`docs/adr/README.md` ADR-0027), with this feature's several numbers
/// folded into the single `snapshot_audit` line's value rather than each
/// getting its own line.
pub fn healthz_value(record: &AuditRecord) -> String {
    let (lo, hi) = record.wilson();
    format!(
        "checked={} disagreed={} block={} rate={} ci=[{},{}]",
        record.checked,
        record.disagreed,
        record.block,
        format_pct(record.rate()),
        format_pct(lo),
        format_pct(hi),
    )
}

/// The full startup report line:
/// `snapshot audit: N checked, W disagreed with the chain at block B
/// (rate R%, Wilson 95% CI [lo%, hi%]) — implies ~X of the Y ingested
/// accounts`. `total_ingested` is the snapshot's own ingested row count
/// (`risepir_feed::snapshot::SnapshotStats::nonzero`), passed in rather
/// than stored on [`AuditRecord`] because it is already known at ingest
/// time and does not need to survive a restart for this line to be
/// reprintable (a restart that only loads the sidecar reports
/// [`healthz_value`] instead, which does not need it).
pub fn report_line(record: &AuditRecord, total_ingested: u64) -> String {
    let (lo, hi) = record.wilson();
    let rate = record.rate();
    let implied = (rate * total_ingested as f64).round() as u64;
    format!(
        "snapshot audit: {} checked, {} disagreed with the chain at block {} (rate {}, Wilson 95% CI [{}, {}]) \
         — implies ~{implied} of the {total_ingested} ingested accounts",
        record.checked,
        record.disagreed,
        record.block,
        format_pct(rate),
        format_pct(lo),
        format_pct(hi),
    )
}

/// `<state>.audit` — the sidecar path for a given `--state` path. Reserved
/// in `crate::state::acquire_state_path`'s sibling-suffix check, the same
/// way `.journal`/`.tmp`/`.lock` already are.
pub fn sidecar_path(state_path: &Path) -> PathBuf {
    state_path.with_extension("audit")
}

/// What reading a sidecar produces: either a valid [`AuditRecord`], or
/// simply "unknown" — collapsing *both* "no sidecar has ever been
/// written" (the ordinary case for any deployment that has not yet run a
/// snapshot bootstrap under this feature) and "a sidecar exists but failed
/// validation" into one variant, per the brief's own instruction that
/// absent/corrupt sidecars are simply unknown, not distinguished failure
/// modes a caller needs to branch on. [`read_sidecar`] still logs a
/// `WARNING` for the "present but corrupt" sub-case — silently discarding
/// a file an operator might be relying on is how soak evidence gets lost,
/// the same reasoning `crate::journal`'s own corrupt-sidecar handling
/// gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditSidecar {
    /// No usable audit result — absent file, or present but corrupt.
    Unknown,
    /// A validated audit record.
    Known(AuditRecord),
}

/// Serializes `record` to `path` as plain `key=value` text (`checked`,
/// `disagreed`, `block`, `seed`, one per line, `#`-comments and blank
/// lines ignored on read), atomically (`<path>.tmp` + rename — a
/// *distinct* `.tmp` name from the state file's own staging file, since
/// `path` is already `<state>.audit`, not `<state>` itself).
pub fn write_sidecar(path: &Path, record: &AuditRecord) -> std::io::Result<()> {
    let contents = format!(
        "# risepir snapshot audit sidecar (ADR-0040) — plain key=value, regenerated at every snapshot bootstrap\n\
         checked={}\n\
         disagreed={}\n\
         block={}\n\
         seed={}\n",
        record.checked, record.disagreed, record.block, record.seed,
    );
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Reads and validates the sidecar at `path`. Any problem at all — the
/// file does not exist, cannot be read, or fails to validate —
/// collapses to [`AuditSidecar::Unknown`]; only the "present but corrupt"
/// sub-case is worth a log line (an absent file is the ordinary case and
/// would be pure noise to warn about every restart).
pub fn read_sidecar(path: &Path) -> AuditSidecar {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return AuditSidecar::Unknown,
    };
    match parse_sidecar(&text) {
        Ok(record) => AuditSidecar::Known(record),
        Err(e) => {
            logln!(
                "risepir-rpc: WARNING: snapshot audit sidecar {} is present but corrupt ({e}); reporting unknown",
                path.display()
            );
            AuditSidecar::Unknown
        }
    }
}

/// Parses sidecar text into an [`AuditRecord`], validating every line —
/// pure (no I/O), so every malformed shape is testable directly on a
/// string. Requires all four keys, each exactly once, `disagreed <=
/// checked`; anything else is `Err`.
fn parse_sidecar(text: &str) -> Result<AuditRecord, String> {
    let mut checked = None;
    let mut disagreed = None;
    let mut block = None;
    let mut seed = None;
    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            return Err(format!("line {line_no}: expected key=value, got {line:?}"));
        };
        let slot = match k {
            "checked" => &mut checked,
            "disagreed" => &mut disagreed,
            "block" => &mut block,
            "seed" => &mut seed,
            other => return Err(format!("line {line_no}: unknown key {other:?}")),
        };
        if slot.is_some() {
            return Err(format!("line {line_no}: duplicate key {k:?}"));
        }
        *slot = Some(
            v.parse::<u64>()
                .map_err(|_| format!("line {line_no}: {k}={v:?} is not a u64"))?,
        );
    }
    let (checked, disagreed, block, seed) = match (checked, disagreed, block, seed) {
        (Some(c), Some(d), Some(b), Some(s)) => (c, d, b, s),
        _ => return Err("missing one or more required keys (checked, disagreed, block, seed)".to_string()),
    };
    if disagreed > checked {
        return Err(format!("disagreed ({disagreed}) exceeds checked ({checked})"));
    }
    Ok(AuditRecord { checked, disagreed, block, seed })
}

// ─── The background verification task ────────────────────────────────────

/// Runs the whole post-bootstrap audit as a background task: checks every
/// `(address, ingested_balance)` pair in `sample` against `refresh_urls`'
/// quorum (`crate::hard_refresh::quorum`) at `snapshot_block`, with
/// `AUDIT_CONCURRENCY`-way fan-out so a few hundred samples take
/// seconds to a couple of minutes rather than serially chaining every
/// round trip. Logs the report line, updates `node`'s `GET /healthz`
/// summary, and — when `state_path` is `Some` — writes the sidecar next
/// to it.
///
/// A no-op if `sample` is empty (`--snapshot-audit-samples 0`, or a
/// snapshot smaller than the requested sample size never observed
/// anything — impossible in practice at mainnet scale, but handled
/// rather than assumed away).
///
/// Never touches the PIR server's write lock and is spawned via
/// `tokio::spawn` by its one caller (`crate::mainnet::spawn`) — like
/// `crate::hard_refresh::run`, it runs fully concurrently with the follow
/// loop and every request handler.
///
/// Assumes `refresh_urls` already passed
/// [`crate::hard_refresh::validate_refresh_urls`] — the caller checks that
/// before spawning this at all.
#[allow(clippy::too_many_arguments)]
pub async fn verify(
    node: Arc<NodeState>,
    sample: Vec<([u8; 20], Balance)>,
    snapshot_block: u64,
    refresh_urls: Vec<String>,
    total_ingested: u64,
    seed: u64,
    state_path: Option<PathBuf>,
) {
    let total_sampled = sample.len();
    if total_sampled == 0 {
        return;
    }
    logln!(
        "risepir-rpc mainnet: snapshot audit: verifying {total_sampled} sampled address(es) at block \
         {snapshot_block} against {} provider(s) (reservoir seed={seed})",
        refresh_urls.len()
    );

    let clients: Vec<Arc<RpcClient>> = refresh_urls.into_iter().map(|u| Arc::new(RpcClient::new(u))).collect();

    let mut checked = 0u64;
    let mut disagreed = 0u64;

    let mut join_set: tokio::task::JoinSet<(Balance, Quorum)> = tokio::task::JoinSet::new();
    let mut remaining = sample.into_iter();
    for (addr, ingested) in remaining.by_ref().take(AUDIT_CONCURRENCY) {
        spawn_audit_check(&mut join_set, &clients, addr, ingested, snapshot_block);
    }

    while let Some(res) = join_set.join_next().await {
        let (ingested, q) = res.expect("snapshot-audit check task panicked");
        if let Some((next_addr, next_ingested)) = remaining.next() {
            spawn_audit_check(&mut join_set, &clients, next_addr, next_ingested, snapshot_block);
        }
        if let Quorum::Agreed(chain_value) = q {
            checked += 1;
            if chain_value != ingested {
                disagreed += 1;
            }
        }
        // A disagreement or a fetch error simply is not counted — see the
        // module docs: only a quorum-agreed answer is "checked" at all.
    }

    let record = AuditRecord {
        checked,
        disagreed,
        block: snapshot_block,
        seed,
    };
    logln!("risepir-rpc mainnet: {}", report_line(&record, total_ingested));
    if is_alarming(&record) {
        logln!(
            "risepir-rpc mainnet: WARNING: the snapshot audit is 95% confident the true disagreement rate \
             exceeds {} — see the report line above; this deployment continues to serve regardless (a \
             sampled third-party dataset's imperfection must not become this deployment's outage, the same \
             reasoning ADR-0027 applies to the reconcile check), but the rate is now disclosed rather than \
             assumed away",
            format_pct(ALARM_THRESHOLD),
        );
    }

    node.set_snapshot_audit_line(healthz_value(&record));

    if let Some(path) = state_path {
        let sidecar = sidecar_path(&path);
        if let Err(e) = write_sidecar(&sidecar, &record) {
            logln!(
                "risepir-rpc mainnet: WARNING: could not write snapshot audit sidecar {}: {e}",
                sidecar.display()
            );
        }
    }
}

/// Spawns one sampled address's quorum check at `height`, carrying its
/// `ingested` balance through to the result so the consumer can compare
/// without a side table. Mirrors
/// `crate::hard_refresh::spawn_check`'s network/quorum logic exactly
/// (kept as a separate function rather than a shared generic one because
/// the two return different payloads — `crate::hard_refresh`'s consumer
/// needs the address back to do a *live* verified store read, this one
/// needs the *ingested* balance back to compare against a *fixed* sample
/// value; unifying the two would need more abstraction than the ~15 lines
/// it would save).
fn spawn_audit_check(
    join_set: &mut tokio::task::JoinSet<(Balance, Quorum)>,
    clients: &[Arc<RpcClient>],
    addr: [u8; 20],
    ingested: Balance,
    height: u64,
) {
    let clients = clients.to_vec();
    join_set.spawn(async move {
        let mut results = Vec::with_capacity(clients.len());
        for c in &clients {
            match c.balance_at(&addr, height).await {
                Ok(b) => results.push(Ok(b)),
                Err(e) => {
                    logln!(
                        "risepir-rpc mainnet: snapshot audit: fetch for 0x{} at {height} from {} failed ({e})",
                        hex20(&addr),
                        c.url()
                    );
                    results.push(Err(()));
                }
            }
        }
        (ingested, quorum(&results))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ReservoirSampler ─────────────────────────────────────────────────

    #[test]
    fn capacity_zero_is_a_permanent_no_op() {
        let mut s = ReservoirSampler::new(0, 42);
        for i in 0..1000u64 {
            s.observe([i as u8; 20], u128::from(i));
        }
        assert_eq!(s.seen(), 1000);
        assert!(s.into_sample().is_empty());
    }

    #[test]
    fn reservoir_size_is_min_of_capacity_and_seen() {
        for (capacity, n) in [(10usize, 3u32), (10, 10), (10, 1000)] {
            let mut s = ReservoirSampler::new(capacity, 7);
            for i in 0..n {
                s.observe([i as u8; 20], u128::from(i));
            }
            assert_eq!(s.into_sample().len(), capacity.min(n as usize));
        }
    }

    #[test]
    fn same_seed_same_stream_reproduces_the_identical_sample() {
        let stream: Vec<([u8; 20], Balance)> = (0u32..5000).map(|i| ([(i % 256) as u8; 20], u128::from(i))).collect();

        let mut a = ReservoirSampler::new(50, 0xC0FFEE);
        let mut b = ReservoirSampler::new(50, 0xC0FFEE);
        for &(addr, bal) in &stream {
            a.observe(addr, bal);
            b.observe(addr, bal);
        }
        assert_eq!(a.into_sample(), b.into_sample(), "identical seed + identical stream must reproduce identically");
    }

    #[test]
    fn every_sampled_item_actually_came_from_the_stream() {
        let stream: Vec<([u8; 20], Balance)> = (0u32..300).map(|i| ([(i % 256) as u8; 20], u128::from(i) * 7)).collect();
        let mut s = ReservoirSampler::new(20, 999);
        for &(addr, bal) in &stream {
            s.observe(addr, bal);
        }
        let sample = s.into_sample();
        assert_eq!(sample.len(), 20);
        for item in &sample {
            assert!(stream.contains(item), "{item:?} was never offered to the sampler");
        }
    }

    // ── wilson_interval ──────────────────────────────────────────────────

    /// Pinned against the brief's own measured population figure: 600
    /// samples, 2 disagreements, Wilson 95% CI [0.09%, 1.21%].
    #[test]
    fn known_value_2_of_600() {
        let (lo, hi) = wilson_interval(2, 600);
        assert!((lo * 100.0 - 0.09).abs() < 0.02, "lo = {}%, want ~0.09%", lo * 100.0);
        assert!((hi * 100.0 - 1.21).abs() < 0.02, "hi = {}%, want ~1.21%", hi * 100.0);
    }

    /// A textbook known value: 0 successes in 10 trials gives a Wilson
    /// interval of approximately [0, 0.278].
    #[test]
    fn known_value_0_of_10() {
        let (lo, hi) = wilson_interval(0, 10);
        assert!(lo.abs() < 1e-9, "lo = {lo}, want ~0");
        assert!((hi - 0.2775).abs() < 0.001, "hi = {hi}, want ~0.2775");
    }

    #[test]
    fn zero_trials_is_totally_uninformative() {
        assert_eq!(wilson_interval(0, 0), (0.0, 1.0));
    }

    #[test]
    fn always_bounded_and_ordered_across_a_sweep() {
        for n in [1u64, 2, 10, 100, 512, 10_000] {
            for x in [0u64, 1, n / 2, n] {
                let (lo, hi) = wilson_interval(x, n);
                assert!((0.0..=1.0).contains(&lo), "lo out of bounds at x={x} n={n}: {lo}");
                assert!((0.0..=1.0).contains(&hi), "hi out of bounds at x={x} n={n}: {hi}");
                assert!(lo <= hi, "lo > hi at x={x} n={n}: {lo} > {hi}");
            }
        }
    }

    // ── AuditRecord / is_alarming ────────────────────────────────────────

    #[test]
    fn a_single_disagreement_in_hundreds_of_samples_is_not_alarming() {
        let r = AuditRecord { checked: 512, disagreed: 1, block: 1, seed: 0 };
        // Wilson lower bound for 1/512 is ~0.034% — comfortably below
        // ALARM_THRESHOLD (1%), even though it is (as the Wilson score
        // interval always is for any disagreed >= 1) strictly above zero.
        assert!(!is_alarming(&r));
    }

    #[test]
    fn zero_disagreements_is_never_alarming() {
        let r = AuditRecord { checked: 512, disagreed: 0, block: 1, seed: 0 };
        assert!(!is_alarming(&r));
    }

    /// The exact rate this ADR's own population measurement disclosed
    /// (2/600, ~0.33%, Wilson CI [0.09%, 1.21%]) must not itself register
    /// as alarming — it is the *expected*, already-disclosed baseline for
    /// this snapshot generation, not a fresh anomaly.
    #[test]
    fn the_disclosed_population_baseline_rate_is_not_itself_alarming() {
        let r = AuditRecord { checked: 600, disagreed: 2, block: 1, seed: 0 };
        assert!(!is_alarming(&r));
    }

    #[test]
    fn a_clear_majority_disagreeing_is_alarming() {
        let r = AuditRecord { checked: 100, disagreed: 40, block: 1, seed: 0 };
        assert!(is_alarming(&r));
    }

    #[test]
    fn report_line_and_healthz_value_contain_the_expected_numbers() {
        let r = AuditRecord { checked: 512, disagreed: 3, block: 25_613_233, seed: 123 };
        let report = report_line(&r, 200_503_969);
        assert!(report.contains("512 checked"), "{report}");
        assert!(report.contains("3 disagreed"), "{report}");
        assert!(report.contains("25613233"), "{report}");
        assert!(report.contains("200503969 ingested"), "{report}");

        let hv = healthz_value(&r);
        assert!(hv.contains("checked=512"), "{hv}");
        assert!(hv.contains("disagreed=3"), "{hv}");
        assert!(hv.contains("block=25613233"), "{hv}");
    }

    // ── sidecar: parse_sidecar (pure) ────────────────────────────────────

    #[test]
    fn parse_sidecar_accepts_a_well_formed_record() {
        let text = "checked=512\ndisagreed=3\nblock=25613233\nseed=123456789\n";
        assert_eq!(
            parse_sidecar(text).unwrap(),
            AuditRecord { checked: 512, disagreed: 3, block: 25_613_233, seed: 123_456_789 }
        );
    }

    #[test]
    fn parse_sidecar_ignores_comments_and_blank_lines() {
        let text = "# a comment\n\nchecked=1\n\ndisagreed=0\nblock=5\nseed=9\n# trailing\n";
        assert_eq!(parse_sidecar(text).unwrap(), AuditRecord { checked: 1, disagreed: 0, block: 5, seed: 9 });
    }

    #[test]
    fn parse_sidecar_rejects_missing_keys() {
        assert!(parse_sidecar("checked=1\ndisagreed=0\nblock=5\n").is_err(), "missing seed");
        assert!(parse_sidecar("").is_err(), "nothing at all");
    }

    #[test]
    fn parse_sidecar_rejects_duplicate_keys() {
        let text = "checked=1\nchecked=2\ndisagreed=0\nblock=5\nseed=9\n";
        assert!(parse_sidecar(text).is_err());
    }

    #[test]
    fn parse_sidecar_rejects_unknown_keys() {
        let text = "checked=1\ndisagreed=0\nblock=5\nseed=9\nbogus=1\n";
        assert!(parse_sidecar(text).is_err());
    }

    #[test]
    fn parse_sidecar_rejects_non_numeric_values() {
        let text = "checked=oops\ndisagreed=0\nblock=5\nseed=9\n";
        assert!(parse_sidecar(text).is_err());
    }

    #[test]
    fn parse_sidecar_rejects_disagreed_exceeding_checked() {
        let text = "checked=1\ndisagreed=5\nblock=5\nseed=9\n";
        assert!(parse_sidecar(text).is_err());
    }

    #[test]
    fn parse_sidecar_rejects_lines_without_equals() {
        let text = "checked=1\nnotakeyvalue\ndisagreed=0\nblock=5\nseed=9\n";
        assert!(parse_sidecar(text).is_err());
    }

    // ── sidecar: read/write round trip, corrupt/absent handling ─────────

    fn tmp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("risepir-snapshot-audit-{}-{name}", std::process::id()))
    }

    #[test]
    fn write_then_read_round_trips_exactly() {
        let path = tmp_path("roundtrip.audit");
        let record = AuditRecord { checked: 512, disagreed: 3, block: 25_613_233, seed: 42 };
        write_sidecar(&path, &record).unwrap();

        match read_sidecar(&path) {
            AuditSidecar::Known(got) => assert_eq!(got, record),
            AuditSidecar::Unknown => panic!("a freshly written sidecar must read back as Known"),
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn absent_sidecar_reads_as_unknown() {
        let path = tmp_path("does-not-exist.audit");
        let _ = std::fs::remove_file(&path); // make sure
        assert_eq!(read_sidecar(&path), AuditSidecar::Unknown);
    }

    #[test]
    fn corrupt_sidecar_reads_as_unknown_not_an_error() {
        let path = tmp_path("corrupt.audit");
        std::fs::write(&path, "checked=oops\ndisagreed=0\nblock=5\nseed=9\n").unwrap();
        assert_eq!(read_sidecar(&path), AuditSidecar::Unknown);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn write_sidecar_does_not_collide_with_the_state_files_own_tmp_file() {
        // path is itself already "<state>.audit" — write_sidecar's tmp
        // staging file must be "<state>.audit.tmp", never "<state>.tmp"
        // (which is what `crate::state::save` stages into).
        let path = tmp_path("collision.audit");
        write_sidecar(&path, &AuditRecord { checked: 1, disagreed: 0, block: 1, seed: 1 }).unwrap();
        let mut expected_tmp = path.as_os_str().to_os_string();
        expected_tmp.push(".tmp");
        assert!(!std::path::Path::new(&expected_tmp).exists(), "the tmp file must be renamed away, not left behind");
        // sibling `<state>.tmp` (one extension shorter) must never have been touched.
        let state_tmp = path.with_extension("tmp");
        assert!(!state_tmp.exists());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn sidecar_path_replaces_the_state_extension() {
        let state = PathBuf::from("/tmp/risepir-state.bin");
        assert_eq!(sidecar_path(&state), PathBuf::from("/tmp/risepir-state.audit"));
    }
}
