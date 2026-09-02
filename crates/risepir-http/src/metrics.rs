//! `GET /metrics` rendering (ADR-0039): a hand-rolled Prometheus text
//! exposition writer. **No new dependency** — the format is a handful of
//! `# HELP`/`# TYPE`/`name{labels} value` lines, and `deny.toml` denies
//! unknown registries and git sources anyway, so a `prometheus`/`metrics`
//! crate is a new supply-chain surface for something this small. See
//! `docs/adr/README.md` ADR-0039 for the alternatives that were rejected.
//!
//! This module is deliberately pure: every function here takes plain data
//! and returns a `String` (or a bool/escaped copy), with no lock, no
//! `NodeState`, no I/O. [`crate::node::NodeState::render_metrics`] is the
//! only caller, and it does all the (briefly-held, never-across-`.await`)
//! locking before handing a [`Snapshot`] in — which is what makes the
//! rendering logic here directly unit-testable with hand-built fixtures
//! (see the tests below), independent of a running server.
//!
//! # Privacy — read this before adding a field
//!
//! Every value rendered by this module is an **aggregate**: a counter, a
//! gauge, or a histogram bucket over *all* requests this process has ever
//! served — never one query's own data. Nothing here may carry an address,
//! a bucket/segment index, a query or response ciphertext, or anything
//! computed from them (`docs/threat-model.md` §5; ADR-0039's own privacy
//! audit). In particular:
//!
//! - [`Counters::requests`] / [`Counters::request_errors`] are keyed by
//!   `(&'static str, &'static str)` pairs *only* — a route name and an
//!   outcome/class, both chosen by this crate's own code
//!   ([`crate::node`]'s route-name match and the `WireError`/`ServerError`
//!   `metric_class()` methods). Never a `String` built from request data,
//!   which would let an attacker inflate this map's key count or (worse)
//!   smuggle request content into a label.
//! - The `class` label is always an error **variant name**, never a
//!   formatted `Display` message — a message can carry attacker-supplied
//!   lengths, offsets, or segment indices (see `WireError`'s own module
//!   docs); a variant name is a small, fixed, closed set.
//! - [`Histogram::observe`] takes only a [`std::time::Duration`] — never
//!   anything about *which* query was answered.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use crate::node::ReconcileHealth;

/// Answer-latency histogram bucket upper bounds, in seconds (Prometheus
/// `le`, "less than or equal"), strictly ascending — the implicit `+Inf`
/// bucket is always appended by [`Histogram`] and is not listed here.
///
/// Chosen to span both a modest partial deployment (`docs/numbers.md` §5:
/// 3.3489 ms average `server.answer(&queries)` at 1,000,000 accounts) and
/// the live complete-mainnet deployment two orders of magnitude larger
/// (200,503,969 accounts) — where the same per-segment scan is expected to
/// land somewhere in the hundreds-of-ms to low-seconds range, by the same
/// `O(n_rows × row_width × lwe_dim)` scaling `RisePirServer::answer`'s own
/// docs state. Retuning this list is a code change, not a config knob —
/// deliberately: a wrong bucket boundary only makes the histogram coarser
/// in one region, never wrong, so it is not worth a flag.
const ANSWER_DURATION_BUCKETS_SECONDS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// A cumulative histogram over [`ANSWER_DURATION_BUCKETS_SECONDS`] plus an
/// implicit `+Inf` bucket — the standard Prometheus histogram shape: each
/// bucket's count is the number of observations `<=` its `le`, so bucket
/// counts are monotonically non-decreasing and the last (`+Inf`) bucket
/// always equals the total observation count ([`Self::count`]).
#[derive(Debug, Clone)]
pub(crate) struct Histogram {
    /// Per-bucket cumulative counts, same length and order as
    /// [`ANSWER_DURATION_BUCKETS_SECONDS`] (the `+Inf` bucket is
    /// [`Self::count`] itself, not stored separately here).
    bucket_counts: Vec<u64>,
    count: u64,
    sum_seconds: f64,
}

impl Histogram {
    /// An empty histogram — every bucket, the sum, and the count start at
    /// zero. Not a `Default` impl on purpose: `bucket_counts` must always
    /// have exactly [`ANSWER_DURATION_BUCKETS_SECONDS`]`.len()` entries,
    /// an invariant a derived `Default` (which would zero-length the
    /// `Vec`) cannot express.
    pub(crate) fn new() -> Self {
        Self {
            bucket_counts: vec![0; ANSWER_DURATION_BUCKETS_SECONDS.len()],
            count: 0,
            sum_seconds: 0.0,
        }
    }

    /// Folds in one observation. `O(buckets)` — a dozen comparisons, once
    /// per `/answer` call; not worth a binary search at this size.
    pub(crate) fn observe(&mut self, d: Duration) {
        let secs = d.as_secs_f64();
        self.count += 1;
        self.sum_seconds += secs;
        for (i, &le) in ANSWER_DURATION_BUCKETS_SECONDS.iter().enumerate() {
            if secs <= le {
                self.bucket_counts[i] += 1;
            }
        }
    }

    /// Total observations folded in — what the `+Inf` bucket (and the
    /// exposition format's trailing `_count` line) reports. `pub(crate)`:
    /// also read by `crate::node::NodeState::answer_compute_totals`, which
    /// hands the cumulative `/answer` compute count to the `risepir-rpc`
    /// follow loop for its per-block CSV (an interference indicator).
    pub(crate) fn count(&self) -> u64 {
        self.count
    }

    /// Cumulative seconds across every observation folded in — the other
    /// half of what [`Self::count`] is read for outside this module.
    pub(crate) fn sum_seconds(&self) -> f64 {
        self.sum_seconds
    }
}

/// Most recent state-save / delta-journal outcome the follow loop has
/// observed (ADR-0039). `risepir-rpc`'s `StateSaver`/`JournalWriter` live
/// in a crate that depends on *this* one, never the reverse, so they
/// cannot be named here directly — the follow loop translates its own
/// `SaveOutcome`/journal state into these plain fields and pushes them in
/// via `NodeState::record_save_outcome` and friends, the same
/// "only the follow loop knows this, so it publishes it" shape
/// `NodeState::set_finalized` uses for `finalized`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SaveState {
    /// Whether this deployment persists state at all (`--state` was
    /// given). Explicit rather than inferred from the zero-valued fields
    /// below — the same reasoning as [`ReconcileHealth::configured`]: a
    /// deployment that never saves must read as "not configured", never
    /// silently as "configured, and simply has not saved yet".
    pub configured: bool,
    /// Unix timestamp (seconds) of the most recent *completed*
    /// (`SaveOutcome::Saved`) save — `0` if none has completed yet this
    /// process.
    pub last_save_unix: u64,
    /// Wall-clock duration of that save, in seconds.
    pub last_save_duration_secs: f64,
    /// Its file size, in bytes.
    pub last_save_bytes: u64,
    /// Count of save attempts that returned `Err` this process (disk
    /// full, permission denied, ...) — see `StateSaver::save_with`.
    pub save_failures_total: u64,
    /// Records appended to the *current* journal since its last rotation —
    /// this process's own running count since it started or last rotated,
    /// not necessarily the on-disk journal's full depth at a resumed
    /// restart (the startup log's one-time `report_journal_savings` line
    /// already reports that number once; see `StateSaver::journal_status`'s
    /// docs for why the two are allowed to differ).
    pub journal_records_since_save: u64,
    /// The `journal_broken` latch — once set, journaling is disabled for
    /// the rest of this run (see `StateSaver`'s own docs).
    pub journal_broken: bool,
}

impl SaveState {
    /// Not configured, nothing saved yet — the state before any
    /// `--state`-aware setter has run (mock/demo, or a mainnet deployment
    /// before its first save).
    pub(crate) const fn new() -> Self {
        Self {
            configured: false,
            last_save_unix: 0,
            last_save_duration_secs: 0.0,
            last_save_bytes: 0,
            save_failures_total: 0,
            journal_records_since_save: 0,
            journal_broken: false,
        }
    }
}

/// Aggregate request/error counters and the answer-latency histogram —
/// everything [`crate::node::NodeState`] updates on the request path
/// itself (as opposed to [`SaveState`]/`finalized`, which only the follow
/// loop knows). Guarded by a single `std::sync::Mutex` on `NodeState`,
/// following [`ReconcileHealth`]'s own precedent: every critical section
/// that touches this is a handful of map/counter updates with no `.await`
/// inside it, so a blocking mutex is the right tool, never the async one.
#[derive(Debug, Clone)]
pub(crate) struct Counters {
    /// `(route, outcome)` → count. Both halves of the key are always
    /// `&'static str` literals this crate itself chose (see the module
    /// docs) — never attacker-influenced text — so this map's key count is
    /// bounded by (routes × outcomes), a small closed set fixed at compile
    /// time, regardless of what any caller sends.
    pub requests: BTreeMap<(&'static str, &'static str), u64>,
    /// `(route, class)` → count, same closed-key-set reasoning as
    /// [`Self::requests`].
    pub request_errors: BTreeMap<(&'static str, &'static str), u64>,
    /// Wall-clock time inside `RisePirServer::answer(&queries)` only — see
    /// `crate::node`'s `answer` handler for exactly what is (and is not)
    /// under this clock, and why.
    pub answer_duration: Histogram,
    /// Cumulative store mutations applied across every block this process
    /// has applied, keyed by kind (`"insert"`/`"update"`/`"delete"` — a
    /// fixed, closed set this crate itself chose, same reasoning as
    /// [`Self::requests`]'s labels). Deliberately excludes no-op deletes
    /// (ADR-0017): they perform no store mutation, so they are not a
    /// "kind" of mutation. Folded in once per applied block by
    /// `NodeState::record_block_apply_metrics`, from
    /// `risepir_server::BlockApplyReport`.
    pub store_mutations: BTreeMap<&'static str, u64>,
    /// Cumulative wall-clock seconds spent inside
    /// `RisePirServer::apply_block_reporting`, summed over every applied
    /// block — the numerator a scraper divides by
    /// [`Self::block_apply_total`] to compute a mean apply time
    /// (`risepir_block_apply_seconds_total` / `risepir_block_apply_total`,
    /// ADR-0039's follow-on; the standard Prometheus sum/count-counter
    /// pair, deliberately not a second histogram — one mean is enough for
    /// this one).
    pub block_apply_seconds_total: f64,
    /// Blocks successfully applied this process — the denominator for the
    /// mean above.
    pub block_apply_total: u64,
    /// Cumulative `BlockDelta::encoded_len()` bytes across every applied
    /// block — B9's running total.
    pub block_delta_bytes_total: u64,
}

impl Counters {
    pub(crate) fn new() -> Self {
        Self {
            requests: BTreeMap::new(),
            request_errors: BTreeMap::new(),
            answer_duration: Histogram::new(),
            store_mutations: BTreeMap::new(),
            block_apply_seconds_total: 0.0,
            block_apply_total: 0,
            block_delta_bytes_total: 0,
        }
    }
}

/// Everything [`render`] turns into Prometheus text, gathered by
/// [`crate::node::NodeState::render_metrics`] under whatever lock each
/// piece actually needs — never all the locks at once, and never the
/// `/answer` path's own `inner` read lock for longer than it takes to read
/// three plain values (see that method's docs). Because this type is a
/// plain, lock-free snapshot, [`render`] itself needs no lock, no `.await`,
/// and no `NodeState` at all, which is what makes it directly
/// unit-testable from a hand-built fixture.
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    /// This crate's own package version (`env!("CARGO_PKG_VERSION")`).
    pub version: &'static str,
    /// This deployment's hint-lineage epoch (`NodeState::epoch`, ADR-0033).
    pub epoch: String,
    /// Whether this deployment serves the complete nonzero-balance set.
    pub complete: bool,
    /// Seconds since this `NodeState` was constructed.
    pub uptime_seconds: f64,
    /// The PIR server's current applied head block.
    pub head_block: u64,
    /// The most recent `finalized` height the follow loop observed (`0`
    /// before its first successful poll — always true for mock/demo,
    /// which never calls `NodeState::set_finalized`).
    pub finalized_block: u64,
    /// Total items (accounts) currently in the store.
    pub store_items: u64,
    /// Total slot capacity (`num_buckets × bucket_size`) of the store.
    pub store_capacity: u64,
    /// The store's raw cell array length in bytes (`cells().len() * 4`) —
    /// the "server DB size" (C11). A live gauge, not accumulated: reflects
    /// the store's *current* size, unlike the cumulative counters below.
    pub store_cells_bytes: u64,
    /// Sum, over every segment, of that segment's hint size in bytes
    /// (`RisePirServer::hint_bytes`, `BackendWireSize::hint_byte_size`) —
    /// also a live gauge.
    pub hint_bytes: u64,
    /// This process's resident set size in bytes (`/proc/self/statm` on
    /// Linux; `0` elsewhere or on any read failure — see
    /// `crate::node::process_rss_bytes`'s own docs for the page-size
    /// assumption this makes).
    pub process_rss_bytes: u64,
    /// Size, in bytes, of the currently cached `GET /setup` response —
    /// `0` if nothing has been encoded yet this process. Reading this
    /// never *forces* an encode (`NodeState::cached_setup_bytes`).
    pub setup_bytes: u64,
    /// How many times `GET /setup` has actually (re)encoded a bundle this
    /// process (`NodeState::setup_generation`, ADR-0028).
    pub setup_regenerations: u64,
    /// Cross-provider reconciliation health — the same fields `GET
    /// /healthz` already reports (ADR-0027), rendered here as proper
    /// Prometheus counters/gauges instead of `key=value` text lines.
    pub reconcile: ReconcileHealth,
    /// Most recent state-save / journal outcome.
    pub save: SaveState,
    /// `(route, outcome)` request counts.
    pub requests: BTreeMap<(&'static str, &'static str), u64>,
    /// `(route, class)` error counts.
    pub request_errors: BTreeMap<(&'static str, &'static str), u64>,
    /// The answer-latency histogram.
    pub answer_duration: Histogram,
    /// Cumulative store mutations by kind — see [`Counters::store_mutations`].
    pub store_mutations: BTreeMap<&'static str, u64>,
    /// See [`Counters::block_apply_seconds_total`].
    pub block_apply_seconds_total: f64,
    /// See [`Counters::block_apply_total`].
    pub block_apply_total: u64,
    /// See [`Counters::block_delta_bytes_total`].
    pub block_delta_bytes_total: u64,
}

/// Escapes a label value per the Prometheus text exposition format:
/// backslash → `\\`, double-quote → `\"`, newline → `\n`. Every label
/// value this module actually emits is a fixed `&'static str` this crate
/// chose (a route name, an outcome, an error variant name, a version
/// string, a hex epoch, ...) and none of them need escaping in practice —
/// this function exists, and every label value is routed through it
/// anyway, so that remains true *by construction* rather than by
/// discipline, and so it has a defined, tested answer if a future label
/// value ever does need it.
fn escape_label_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

/// Formats a label list as `{k1="v1",k2="v2"}`, or the empty string for no
/// labels — every value passed through [`escape_label_value`].
fn format_labels(pairs: &[(&str, &str)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let mut out = String::from("{");
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{k}=\"{}\"", escape_label_value(v));
    }
    out.push('}');
    out
}

/// Appends one gauge or counter metric: `# HELP`, `# TYPE`, then exactly
/// one `name{labels} value` line. `ty` is `"gauge"` or `"counter"` (a
/// plain `&str`, not an enum — this module has exactly two call sites'
/// worth of variance and a two-arm enum would not earn its keep).
fn write_metric(
    out: &mut String,
    name: &str,
    help: &str,
    ty: &str,
    labels: &[(&str, &str)],
    value: impl std::fmt::Display,
) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {ty}");
    let _ = writeln!(out, "{name}{} {value}", format_labels(labels));
}

/// Renders `snapshot` as a complete Prometheus text exposition document
/// (`crate::node`'s `/metrics` handler serves this verbatim, with
/// `Content-Type: text/plain; version=0.0.4; charset=utf-8`). Pure: no
/// I/O, no lock, no allocation beyond the one `String` returned and the
/// small formatting scratch space above.
pub(crate) fn render(s: &Snapshot) -> String {
    let mut out = String::new();

    // ── build info + uptime ────────────────────────────────────────────
    let mode = if s.complete { "complete" } else { "partial" };
    write_metric(
        &mut out,
        "risepir_build_info",
        "Static build/deployment identity (value is always 1; the identity is in the labels).",
        "gauge",
        &[("version", s.version), ("epoch", &s.epoch), ("mode", mode)],
        1,
    );
    write_metric(
        &mut out,
        "risepir_process_uptime_seconds",
        "Seconds since this process's NodeState was constructed.",
        "gauge",
        &[],
        s.uptime_seconds,
    );

    // ── block height / lag ──────────────────────────────────────────────
    write_metric(
        &mut out,
        "risepir_head_block",
        "The PIR server's current applied head block.",
        "gauge",
        &[],
        s.head_block,
    );
    write_metric(
        &mut out,
        "risepir_finalized_block",
        "Most recent `finalized` height the follow loop observed (0 before its first poll; always 0 for a deployment with no follow loop, e.g. mock/demo).",
        "gauge",
        &[],
        s.finalized_block,
    );
    write_metric(
        &mut out,
        "risepir_block_lag",
        "finalized_block - head_block, saturating at 0. Reads 0 both when caught up and when finalized_block has never been observed yet — see risepir_finalized_block.",
        "gauge",
        &[],
        s.finalized_block.saturating_sub(s.head_block),
    );

    // ── store occupancy ──────────────────────────────────────────────────
    write_metric(
        &mut out,
        "risepir_store_items",
        "Accounts currently held in the store.",
        "gauge",
        &[],
        s.store_items,
    );
    write_metric(
        &mut out,
        "risepir_store_capacity",
        "Total slot capacity of the store (num_buckets * bucket_size).",
        "gauge",
        &[],
        s.store_capacity,
    );
    let load_factor = if s.store_capacity == 0 {
        0.0
    } else {
        s.store_items as f64 / s.store_capacity as f64
    };
    write_metric(
        &mut out,
        "risepir_store_load_factor",
        "risepir_store_items / risepir_store_capacity.",
        "gauge",
        &[],
        load_factor,
    );
    write_metric(
        &mut out,
        "risepir_store_cells_bytes",
        "The store's raw cell array length in bytes (the server DB size).",
        "gauge",
        &[],
        s.store_cells_bytes,
    );
    write_metric(
        &mut out,
        "risepir_hint_bytes",
        "Sum, over every segment, of that segment's hint size in bytes.",
        "gauge",
        &[],
        s.hint_bytes,
    );
    write_metric(
        &mut out,
        "risepir_process_rss_bytes",
        "This process's resident set size in bytes (Linux only; 0 elsewhere or on read failure).",
        "gauge",
        &[],
        s.process_rss_bytes,
    );

    // ── setup cache ───────────────────────────────────────────────────────
    write_metric(
        &mut out,
        "risepir_setup_bytes",
        "Size of the currently cached GET /setup response, in bytes (0 if nothing has been encoded yet).",
        "gauge",
        &[],
        s.setup_bytes,
    );
    write_metric(
        &mut out,
        "risepir_setup_regenerations_total",
        "How many times GET /setup has actually (re)encoded a bundle, as opposed to serving a cache hit (ADR-0028).",
        "counter",
        &[],
        s.setup_regenerations,
    );

    // ── answer-latency histogram ─────────────────────────────────────────
    // Timed around RisePirServer::answer(&queries) only — never the lock
    // wait, never the wire decode/encode (crate::node's `answer` handler
    // docs explain why, mirroring NodeState::apply_block's own precedent).
    let name = "risepir_answer_duration_seconds";
    let _ = writeln!(
        out,
        "# HELP {name} Wall-clock time inside RisePirServer::answer(&queries) only — excludes lock wait and wire decode/encode."
    );
    let _ = writeln!(out, "# TYPE {name} histogram");
    for (le, count) in ANSWER_DURATION_BUCKETS_SECONDS
        .iter()
        .zip(s.answer_duration.bucket_counts.iter())
    {
        let _ = writeln!(out, "{name}_bucket{{le=\"{le}\"}} {count}");
    }
    let _ = writeln!(
        out,
        "{name}_bucket{{le=\"+Inf\"}} {}",
        s.answer_duration.count()
    );
    let _ = writeln!(out, "{name}_sum {}", s.answer_duration.sum_seconds);
    let _ = writeln!(out, "{name}_count {}", s.answer_duration.count());

    // ── store mutations / block apply / delta bytes ──────────────────────
    let _ = writeln!(
        out,
        "# HELP risepir_store_mutations_total Cumulative store mutations applied, by kind. Excludes no-op deletes (ADR-0017), which perform no store mutation."
    );
    let _ = writeln!(out, "# TYPE risepir_store_mutations_total counter");
    for (kind, count) in &s.store_mutations {
        let _ = writeln!(
            out,
            "risepir_store_mutations_total{} {count}",
            format_labels(&[("kind", kind)])
        );
    }
    write_metric(
        &mut out,
        "risepir_block_apply_seconds_total",
        "Cumulative wall-clock seconds spent applying blocks (RisePirServer::apply_block_reporting). Divide by risepir_block_apply_total for the mean.",
        "counter",
        &[],
        s.block_apply_seconds_total,
    );
    write_metric(
        &mut out,
        "risepir_block_apply_total",
        "Blocks successfully applied this process.",
        "counter",
        &[],
        s.block_apply_total,
    );
    write_metric(
        &mut out,
        "risepir_block_delta_bytes_total",
        "Cumulative BlockDelta::encoded_len() bytes across every applied block.",
        "counter",
        &[],
        s.block_delta_bytes_total,
    );

    // ── requests / errors ─────────────────────────────────────────────────
    let _ = writeln!(
        out,
        "# HELP risepir_requests_total Total requests served, by route and outcome."
    );
    let _ = writeln!(out, "# TYPE risepir_requests_total counter");
    for ((route, outcome), count) in &s.requests {
        let _ = writeln!(
            out,
            "risepir_requests_total{} {count}",
            format_labels(&[("route", route), ("outcome", outcome)])
        );
    }
    let _ = writeln!(
        out,
        "# HELP risepir_request_errors_total Error responses, by route and error class. class is always a fixed error-variant name (WireError/ServerError), never a formatted message."
    );
    let _ = writeln!(out, "# TYPE risepir_request_errors_total counter");
    for ((route, class), count) in &s.request_errors {
        let _ = writeln!(
            out,
            "risepir_request_errors_total{} {count}",
            format_labels(&[("route", route), ("class", class)])
        );
    }

    // ── state save / journal ────────────────────────────────────────────
    write_metric(
        &mut out,
        "risepir_state_save_configured",
        "Whether this deployment persists state (--state was given).",
        "gauge",
        &[],
        u8::from(s.save.configured),
    );
    write_metric(
        &mut out,
        "risepir_state_save_last_success_timestamp_seconds",
        "Unix time of the most recent completed state save (0 if none yet).",
        "gauge",
        &[],
        s.save.last_save_unix,
    );
    write_metric(
        &mut out,
        "risepir_state_save_last_duration_seconds",
        "Wall-clock duration of that save.",
        "gauge",
        &[],
        s.save.last_save_duration_secs,
    );
    write_metric(
        &mut out,
        "risepir_state_save_last_bytes",
        "That save's file size, in bytes.",
        "gauge",
        &[],
        s.save.last_save_bytes,
    );
    write_metric(
        &mut out,
        "risepir_state_save_failures_total",
        "Save attempts that returned an error.",
        "counter",
        &[],
        s.save.save_failures_total,
    );
    write_metric(
        &mut out,
        "risepir_journal_records_since_save",
        "Records appended to the current delta journal since its last rotation (this process's own count since start/last rotation).",
        "gauge",
        &[],
        s.save.journal_records_since_save,
    );
    write_metric(&mut out, "risepir_journal_broken", "Whether journaling has been permanently disabled this run (a continuity gap or I/O failure).", "gauge", &[], u8::from(s.save.journal_broken));

    // ── reconcile (same fields as GET /healthz, ADR-0027) ────────────────
    write_metric(
        &mut out,
        "risepir_reconcile_configured",
        "Whether cross-provider reconciliation runs at all.",
        "gauge",
        &[],
        u8::from(s.reconcile.configured),
    );
    write_metric(
        &mut out,
        "risepir_reconcile_last_checkpoint_block",
        "Block of the most recent reconcile checkpoint attempted.",
        "gauge",
        &[],
        s.reconcile.last_checkpoint_block,
    );
    write_metric(
        &mut out,
        "risepir_reconcile_last_checkpoint_timestamp_seconds",
        "Unix time of the most recent reconcile checkpoint attempted.",
        "gauge",
        &[],
        s.reconcile.last_checkpoint_unix,
    );
    write_metric(
        &mut out,
        "risepir_reconcile_last_success_block",
        "Block of the most recent checkpoint with >=1 completed comparison.",
        "gauge",
        &[],
        s.reconcile.last_success_block,
    );
    write_metric(
        &mut out,
        "risepir_reconcile_last_success_timestamp_seconds",
        "Unix time of the most recent checkpoint with >=1 completed comparison.",
        "gauge",
        &[],
        s.reconcile.last_success_unix,
    );
    write_metric(
        &mut out,
        "risepir_reconcile_comparisons_total",
        "Total individual account comparisons completed.",
        "counter",
        &[],
        s.reconcile.comparisons_total,
    );
    write_metric(
        &mut out,
        "risepir_reconcile_checkpoints_total",
        "Total checkpoints attempted (empty, successful, or dark).",
        "counter",
        &[],
        s.reconcile.checkpoints_total,
    );
    write_metric(
        &mut out,
        "risepir_reconcile_consecutive_dark",
        "Consecutive checkpoints that attempted >=1 comparison and had every attempt fail.",
        "gauge",
        &[],
        s.reconcile.consecutive_dark,
    );
    write_metric(
        &mut out,
        "risepir_reconcile_halted",
        "Whether a value mismatch has permanently halted the follow loop.",
        "gauge",
        &[],
        u8::from(s.reconcile.halted),
    );

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_reconcile() -> ReconcileHealth {
        ReconcileHealth::default()
    }

    fn empty_save() -> SaveState {
        SaveState::new()
    }

    fn base_snapshot() -> Snapshot {
        Snapshot {
            version: "0.1.0",
            epoch: "00ff00ff00ff00ff".to_string(),
            complete: true,
            uptime_seconds: 12.5,
            head_block: 100,
            finalized_block: 90,
            store_items: 42,
            store_capacity: 128,
            store_cells_bytes: 512,
            hint_bytes: 256,
            process_rss_bytes: 0,
            setup_bytes: 4096,
            setup_regenerations: 2,
            reconcile: empty_reconcile(),
            save: empty_save(),
            requests: BTreeMap::new(),
            request_errors: BTreeMap::new(),
            answer_duration: Histogram::new(),
            store_mutations: BTreeMap::new(),
            block_apply_seconds_total: 0.0,
            block_apply_total: 0,
            block_delta_bytes_total: 0,
        }
    }

    // ── escaping ─────────────────────────────────────────────────────────

    #[test]
    fn escapes_backslash_quote_and_newline() {
        assert_eq!(escape_label_value("a\\b\"c\nd"), "a\\\\b\\\"c\\nd");
    }

    #[test]
    fn leaves_ordinary_text_untouched() {
        assert_eq!(escape_label_value("BadMagic"), "BadMagic");
        assert_eq!(escape_label_value(""), "");
    }

    #[test]
    fn format_labels_escapes_every_value() {
        let out = format_labels(&[("class", "weird\"value"), ("route", "answer")]);
        assert_eq!(out, "{class=\"weird\\\"value\",route=\"answer\"}");
    }

    #[test]
    fn format_labels_empty_is_empty_string() {
        assert_eq!(format_labels(&[]), "");
    }

    // ── histogram shape ──────────────────────────────────────────────────

    #[test]
    fn histogram_starts_at_all_zero() {
        let h = Histogram::new();
        assert_eq!(h.count(), 0);
        assert_eq!(h.sum_seconds, 0.0);
        assert!(h.bucket_counts.iter().all(|&c| c == 0));
        assert_eq!(h.bucket_counts.len(), ANSWER_DURATION_BUCKETS_SECONDS.len());
    }

    #[test]
    fn histogram_buckets_are_cumulative_and_monotonic() {
        let mut h = Histogram::new();
        h.observe(Duration::from_millis(2)); // <= 0.005 and every larger bucket
        h.observe(Duration::from_millis(2));
        h.observe(Duration::from_secs(3)); // <= 5.0 and 10.0 only

        // Monotonically non-decreasing.
        for w in h.bucket_counts.windows(2) {
            assert!(
                w[0] <= w[1],
                "buckets must be cumulative: {:?}",
                h.bucket_counts
            );
        }
        // The 0.001s bucket sees neither observation (both > 1ms).
        assert_eq!(h.bucket_counts[0], 0);
        // The 0.005s..2.5s buckets see exactly the two 2ms observations.
        let idx_5ms = ANSWER_DURATION_BUCKETS_SECONDS
            .iter()
            .position(|&b| b == 0.005)
            .unwrap();
        let idx_1s = ANSWER_DURATION_BUCKETS_SECONDS
            .iter()
            .position(|&b| b == 1.0)
            .unwrap();
        assert_eq!(h.bucket_counts[idx_5ms], 2);
        assert_eq!(h.bucket_counts[idx_1s], 2);
        // The 5s bucket picks up the 3s observation too.
        let idx_5s = ANSWER_DURATION_BUCKETS_SECONDS
            .iter()
            .position(|&b| b == 5.0)
            .unwrap();
        assert_eq!(h.bucket_counts[idx_5s], 3);
        // The last finite bucket must equal the total count (+Inf's count).
        assert_eq!(*h.bucket_counts.last().unwrap(), h.count());
        assert_eq!(h.count(), 3);
    }

    // ── full render: shape ───────────────────────────────────────────────

    #[test]
    fn render_produces_well_formed_histogram_lines() {
        let mut snap = base_snapshot();
        snap.answer_duration.observe(Duration::from_millis(3));
        snap.answer_duration.observe(Duration::from_millis(300));
        let text = render(&snap);

        assert!(text.contains("# HELP risepir_answer_duration_seconds"));
        assert!(text.contains("# TYPE risepir_answer_duration_seconds histogram"));
        // Every finite bucket line present, in ascending `le` order, plus `+Inf`.
        let bucket_lines: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with("risepir_answer_duration_seconds_bucket{"))
            .collect();
        assert_eq!(
            bucket_lines.len(),
            ANSWER_DURATION_BUCKETS_SECONDS.len() + 1,
            "every finite bucket plus +Inf"
        );
        assert!(
            bucket_lines.last().unwrap().contains("le=\"+Inf\""),
            "the last bucket line must be +Inf: {:?}",
            bucket_lines.last()
        );

        // Bucket counts, read back off the rendered text, must be monotonic.
        let counts: Vec<u64> = bucket_lines
            .iter()
            .map(|line| line.rsplit(' ').next().unwrap().parse::<u64>().unwrap())
            .collect();
        for w in counts.windows(2) {
            assert!(
                w[0] <= w[1],
                "rendered bucket counts must be monotonic: {counts:?}"
            );
        }
        assert_eq!(
            *counts.last().unwrap(),
            2,
            "the +Inf bucket must equal the total observation count"
        );

        assert!(text.contains("risepir_answer_duration_seconds_sum "));
        assert!(text.contains("risepir_answer_duration_seconds_count 2"));
    }

    #[test]
    fn render_includes_request_and_error_counters_with_labels() {
        let mut snap = base_snapshot();
        snap.requests.insert(("answer", "ok"), 10);
        snap.requests.insert(("answer", "error"), 2);
        snap.request_errors
            .insert(("answer", "SegmentLengthMismatch"), 2);
        let text = render(&snap);

        assert!(text.contains("risepir_requests_total{route=\"answer\",outcome=\"ok\"} 10"));
        assert!(text.contains("risepir_requests_total{route=\"answer\",outcome=\"error\"} 2"));
        assert!(text.contains(
            "risepir_request_errors_total{route=\"answer\",class=\"SegmentLengthMismatch\"} 2"
        ));
    }

    /// The per-block apply instrumentation (ADR-0039's follow-on):
    /// mutation-kind counters, the apply-time sum/count pair, the
    /// cumulative delta-byte counter, and the two live store-size gauges.
    #[test]
    fn render_includes_block_apply_and_store_size_metrics() {
        let mut snap = base_snapshot();
        snap.store_mutations.insert("insert", 5);
        snap.store_mutations.insert("update", 3);
        snap.store_mutations.insert("delete", 1);
        snap.block_apply_seconds_total = 0.042;
        snap.block_apply_total = 4;
        snap.block_delta_bytes_total = 12_345;
        snap.store_cells_bytes = 1_048_576;
        snap.hint_bytes = 65_536;
        let text = render(&snap);

        assert!(text.contains("risepir_store_mutations_total{kind=\"insert\"} 5"));
        assert!(text.contains("risepir_store_mutations_total{kind=\"update\"} 3"));
        assert!(text.contains("risepir_store_mutations_total{kind=\"delete\"} 1"));
        assert!(text.contains("risepir_block_apply_seconds_total 0.042"));
        assert!(text.contains("risepir_block_apply_total 4"));
        assert!(text.contains("risepir_block_delta_bytes_total 12345"));
        assert!(text.contains("risepir_store_cells_bytes 1048576"));
        assert!(text.contains("risepir_hint_bytes 65536"));
        // process_rss_bytes is always rendered even when 0 (non-Linux/test
        // default) — an absent field would read as "healthy"/unmonitored
        // rather than honestly "unavailable here".
        assert!(text.contains("risepir_process_rss_bytes 0"));
    }

    #[test]
    fn render_includes_build_info_and_gauges() {
        let text = render(&base_snapshot());
        assert!(text.contains(
            "risepir_build_info{version=\"0.1.0\",epoch=\"00ff00ff00ff00ff\",mode=\"complete\"} 1"
        ));
        assert!(text.contains("risepir_head_block 100"));
        assert!(text.contains("risepir_finalized_block 90"));
        // finalized (90) < head (100): lag saturates at 0, never underflows/goes negative.
        assert!(text.contains("risepir_block_lag 0"));
    }

    #[test]
    fn block_lag_saturates_rather_than_underflowing() {
        let mut snap = base_snapshot();
        snap.head_block = 100;
        snap.finalized_block = 150;
        let text = render(&snap);
        assert!(text.contains("risepir_block_lag 50"));

        snap.finalized_block = 0; // never polled
        let text = render(&snap);
        assert!(
            text.contains("risepir_block_lag 0"),
            "must saturate, never panic or print a negative number"
        );
    }

    #[test]
    fn store_load_factor_guards_against_division_by_zero() {
        let mut snap = base_snapshot();
        snap.store_capacity = 0;
        snap.store_items = 0;
        let text = render(&snap);
        assert!(text.contains("risepir_store_load_factor 0"));
    }

    /// Every metric name that appears must be preceded by its own `# HELP`
    /// and `# TYPE` lines — the minimal well-formedness the exposition
    /// format requires, checked directly rather than assumed.
    #[test]
    fn every_metric_has_help_and_type_before_its_first_sample() {
        let mut snap = base_snapshot();
        snap.requests.insert(("answer", "ok"), 1);
        snap.request_errors.insert(("answer", "BadMagic"), 1);
        snap.answer_duration.observe(Duration::from_millis(1));
        let text = render(&snap);

        let mut declared_help: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut declared_type: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# HELP ") {
                declared_help.insert(rest.split_whitespace().next().unwrap());
            } else if let Some(rest) = line.strip_prefix("# TYPE ") {
                declared_type.insert(rest.split_whitespace().next().unwrap());
            } else if !line.is_empty() {
                // A sample line: `name{...} value` or `name value`.
                let name = line.split(['{', ' ']).next().unwrap();
                // Histogram sub-series (`_bucket`/`_sum`/`_count`) are
                // declared once under the base name, not individually.
                let base = name
                    .strip_suffix("_bucket")
                    .or_else(|| name.strip_suffix("_sum"))
                    .or_else(|| name.strip_suffix("_count"))
                    .unwrap_or(name);
                assert!(
                    declared_help.contains(base),
                    "{name} (base {base}) sampled with no # HELP"
                );
                assert!(
                    declared_type.contains(base),
                    "{name} (base {base}) sampled with no # TYPE"
                );
            }
        }
    }

    /// A cheap, blunt tripwire: nothing this module ever renders should
    /// look like a 20-byte hex address or a `0x`-prefixed hex blob — the
    /// same shape a leaked address or a leaked query/response ciphertext
    /// segment would take. This does not *prove* privacy (that is the
    /// ADR's argument, backed by what `NodeState`'s new fields are and are
    /// not derived from) but it is a cheap regression tripwire against a
    /// future field that accidentally renders one.
    #[test]
    fn nothing_rendered_looks_address_or_hex_blob_shaped() {
        let mut snap = base_snapshot();
        snap.requests.insert(("answer", "ok"), 1);
        snap.request_errors.insert(("sync", "OutOfWindow"), 1);
        let text = render(&snap);

        for token in text.split(|c: char| !c.is_ascii_hexdigit()) {
            // A 20-byte address is 40 hex chars; `0x`-prefixed variants are
            // handled by the split above (the `x`/`X` boundary itself
            // isn't a hex digit). The one legitimate 16-hex-char token this
            // module emits is the epoch, which is 16 chars — well short of
            // 40, so the length bound alone does not need to special-case it.
            assert!(
                token.len() < 40,
                "found a 40+ hex-digit run, address-shaped: {token:?} in:\n{text}"
            );
        }
    }
}
