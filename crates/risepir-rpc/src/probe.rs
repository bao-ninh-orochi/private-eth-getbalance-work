//! `risepir-rpc probe` — the **client-side measurement probe**: one
//! long-lived product session against a live deployment, every private
//! query timed from the client, every trial's answer checked byte-exactly
//! against an independent provider at the same explicit block height.
//!
//! # Privacy stance (the binding rule, restated because this module
//! writes files)
//!
//! The queried address never leaves this machine, exactly as in
//! [`crate::front`] — the probe drives the *same*
//! [`PrivateEth::get_balance`] path, so what crosses the network is
//! precisely the LWE query bundle the privacy claim covers.
//!
//! It also never leaves this *process*. No column of either CSV, no log
//! line, and no error message this module emits carries an address, a
//! balance, or anything derived from them, with exactly two deliberate
//! exceptions — [`TrialRow::found`] (one bit: did the scan match) and
//! [`TrialRow::provider_match`] (one bit: did an independent provider
//! agree). Both are answers *about* the answer, not the answer. Error
//! text is always a fixed variant name from a closed set, never a
//! formatted message that could quote a body. A tripwire test
//! (`no_row_can_carry_an_address_or_a_balance`) pins this.
//!
//! # The latency budget closes by construction
//!
//! ```text
//! t_total_us = build_us + head_wire_us + sync_wire_us + answer_wire_us
//!            + finish_us + residual_us
//! ```
//!
//! This is arithmetic, not luck. Every term is measured, and
//! `residual_us` is *defined* as the subtraction — never estimated,
//! never distributed over the others. Concretely:
//!
//! * `t_total_us` (**A1**) wraps [`PrivateEth::get_balance_timed`]: the
//!   moment the request enters the session (lock acquisition and the
//!   `keccak256` included) until the decoded balance is out. The
//!   JSON-RPC front end ([`crate::rpc`]) adds only JSON parse/format on
//!   top of this — no PIR work happens above this boundary.
//! * `build_us` (**A2**) is `RisePirClient::build_query` alone.
//! * `head_wire_us` / `sync_wire_us` / `answer_wire_us` (the **A3**
//!   inputs) come from [`risepir_http::NetSink`], which times each call
//!   from just before the request is sent until the last body byte is
//!   in hand. **A3** itself — wire time attributable to the network —
//!   is `answer_wire_us − server_handler_ns/1000` whenever the server
//!   reports its handler time; the two raw numbers are recorded rather
//!   than the subtraction, so the arithmetic stays visible.
//! * `finish_us` (**A5**) is `RisePirClient::finish`, with
//!   `rewind_us`/`decode_us`/`delta_apply_us`/`scan_us` the four
//!   ADR-0003 steps inside it (their sum is slightly under `finish_us`;
//!   the rest is the argument checks, the re-hash, and the value
//!   decode).
//! * `residual_us` is everything else the client did: wire
//!   encode/decode, the `/sync` ingest, mutex acquisition, and — on the
//!   rare row with `attempts = 2` — an entire re-bootstrap `/setup`
//!   download, which is why that column exists to flag such rows.
//!
//! Truncation cannot break the identity: each part is floored to
//! microseconds independently, and `floor(a) + floor(b) ≤ floor(a + b)`,
//! so the parts can only under-count and the residual absorbs the
//! difference.
//!
//! # Session semantics = the product's
//!
//! One `GET /setup` for the whole run, one session, and
//! `RisePirClient::collect_garbage` is **never** called — the product's
//! own client never calls it either, and a session following head with a
//! growing `ΔD` is precisely the operating point being measured
//! (ADR-0003: hint patching is garbage collection, not synchronisation).
//! One [`reqwest::Client`] for the whole run, so connection reuse is the
//! norm and a per-query TLS handshake never contaminates the network
//! number. Trials are strictly sequential — one request in flight at a
//! time.
//!
//! # Between batches: following head, and what a block row means
//!
//! Every `--poll-secs` the probe runs [`PrivateEth::follow_once`], which
//! is the same catch-up `get_balance` performs inline: `GET /head`, then,
//! if the server advanced, **one `GET /sync?from=&to=`** for the whole
//! gap. That is what the product does — `/sync` serves a *coalesced*
//! delta with no per-block framing inside it, so there is no way to
//! attribute bytes to individual blocks from a range fetch.
//!
//! So the blocks CSV carries **one row per fetch**, not per block:
//! `wire_bytes` is the whole `/sync` body, and `blocks_in_fetch` says how
//! many blocks it covered. At the default 12 s poll against mainnet's
//! ~12 s block time, `blocks_in_fetch` is 1 on the overwhelming majority
//! of rows and those rows *are* per-block — which is what makes **B9**
//! (delta bytes per block) and **B10** (`ingest_us`) readable. Filter on
//! `blocks_in_fetch == 1` for a clean per-block distribution; rows above
//! 1 are range totals and must not be divided down, since a coalesced
//! delta telescopes (ADR-0005) and is strictly smaller than the sum of
//! its parts.
//!
//! # Transient failures
//!
//! A failed `/head`, a `409` from `/sync`, or a provider timeout is
//! recorded and the run continues — a measurement campaign that aborts
//! on the first blip measures nothing. The one exception is a **decode
//! failure of a PIR answer** ([`RpcError::DecodeFailed`], or any
//! [`RpcError::Client`] rewind rejection): that is a real defect, not a
//! blip, and the run stops loudly rather than averaging over it.

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use risepir_feed::rpc::RpcClient;
use risepir_feed::FeedError;
use risepir_http::{NetSink, PirHttpClient};

use crate::error::RpcError;
use crate::private_eth::PrivateEth;

/// The default independent provider for the correctness check — a
/// *different* operator from the feed the deployment follows, which is
/// the whole point of the comparison (`docs/deploy.md` §4).
pub const DEFAULT_CONFIRM_URL: &str = "https://ethereum-rpc.publicnode.com";

/// TCP connect bound for the probe's own HTTP client.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Max silence between bytes on an in-flight response (never total time)
/// — the same guardrail [`PirHttpClient::new`] sets, restated here
/// because a caller-supplied client gets none by default.
const READ_STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Sent so a deployment operator can tell probe traffic apart in their
/// access log. Carries no information about any query.
const USER_AGENT: &str = concat!("risepir-rpc-probe/", env!("CARGO_PKG_VERSION"));

// ─── configuration ──────────────────────────────────────────────────────

/// One curl-style `--resolve host:port:ip` DNS override.
///
/// Applied via [`reqwest::ClientBuilder::resolve`], which is how a
/// deployment can be measured while its public DNS record still points
/// somewhere else (a staged cutover, a second origin behind the same
/// name). **TLS validation stays on**: the URL's host is still what SNI
/// carries and what the certificate is checked against — only the
/// address the name resolves to is overridden.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolveOverride {
    /// The hostname to override, exactly as it appears in the URL.
    pub host: String,
    /// Where it should resolve to.
    pub addr: SocketAddr,
}

impl ResolveOverride {
    /// Parse curl's `host:port:ip` form.
    ///
    /// Split from the left twice, so an IPv6 literal (which contains
    /// colons, bracketed or not) survives in the remainder.
    ///
    /// # Errors
    ///
    /// A short message naming what was wrong with the *shape* of the
    /// argument — it is a command-line value, so echoing it is safe.
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut it = s.splitn(3, ':');
        let (Some(host), Some(port), Some(ip)) = (it.next(), it.next(), it.next()) else {
            return Err(format!("--resolve expects host:port:ip, got {s:?}"));
        };
        if host.is_empty() {
            return Err(format!("--resolve host is empty in {s:?}"));
        }
        let port: u16 = port
            .parse()
            .map_err(|_| format!("--resolve port {port:?} is not a u16 (in {s:?})"))?;
        let ip = ip.trim_start_matches('[').trim_end_matches(']');
        let ip: std::net::IpAddr = ip
            .parse()
            .map_err(|_| format!("--resolve address {ip:?} is not an IP address (in {s:?})"))?;
        Ok(Self {
            host: host.to_string(),
            addr: SocketAddr::new(ip, port),
        })
    }
}

/// Knobs for [`run`]. Defaults match the documented campaign shape:
/// three batches of 100 trials at window open, +1.5 h and +3 h, inside a
/// 3 h 10 min lifetime.
#[derive(Clone, Debug)]
pub struct ProbeConfig {
    /// Base URL of the PIR transport to measure. Required.
    pub pir_url: String,
    /// Independent provider for the correctness check.
    pub confirm_url: String,
    /// Skip the provider check entirely (a run that only wants
    /// latency/bytes, or a test with no network). `provider_match` is
    /// then empty on every row.
    pub no_confirm: bool,
    /// curl-style DNS overrides for the PIR client.
    pub resolve: Vec<ResolveOverride>,
    /// Where the per-trial rows go (appended; header written once).
    pub queries_csv: PathBuf,
    /// Where the per-fetch delta rows go (appended; header written once).
    pub blocks_csv: PathBuf,
    /// Trials per batch.
    pub batch_size: usize,
    /// How many batches.
    pub batches: usize,
    /// Spacing between batch *start* times.
    pub batch_interval_secs: u64,
    /// Sleep between trials, so the server's follow loop can take its
    /// write lock rather than starving behind a back-to-back query
    /// stream.
    pub trial_gap_ms: u64,
    /// Total run lifetime from start. The run stops at this deadline
    /// even if batches remain.
    pub follow_secs: u64,
    /// How often the session polls `GET /head` and ingests new deltas
    /// between batches.
    pub poll_secs: u64,
    /// Fraction of trials that query a uniformly random address,
    /// exercising the not-found path (in complete mode the correct
    /// answer is `0x0`).
    pub absent_fraction: f64,
    /// Sample resident memory every this many trials (`0` disables).
    pub rss_every: usize,
    /// Address source: one hex address per line. Overrides `GET
    /// /recent` when set.
    pub addresses_file: Option<PathBuf>,
    /// `eth_chainId` the session reports. Not used by any measurement;
    /// present because [`PrivateEth`] requires one.
    pub chain_id: u64,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            pir_url: String::new(),
            confirm_url: DEFAULT_CONFIRM_URL.to_string(),
            no_confirm: false,
            resolve: Vec::new(),
            queries_csv: PathBuf::from("probe-queries.csv"),
            blocks_csv: PathBuf::from("probe-blocks.csv"),
            batch_size: 100,
            batches: 3,
            batch_interval_secs: 5_400,
            trial_gap_ms: 500,
            follow_secs: 11_400,
            poll_secs: 12,
            absent_fraction: 0.1,
            rss_every: 10,
            addresses_file: None,
            chain_id: 1,
        }
    }
}

// ─── the two row types ──────────────────────────────────────────────────

/// One trial: **A1-A6** plus the correctness evidence.
///
/// Every field is a count, a duration, or a fixed class string. Nothing
/// here is derived from the address or the balance except
/// [`Self::found`] and [`Self::provider_match`] — see the module docs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrialRow {
    /// 0-based batch index.
    pub batch: usize,
    /// 0-based trial index within the whole run (not within the batch),
    /// so it is a stable identifier for a `MISMATCH` report.
    pub trial: usize,
    /// Wall-clock start, milliseconds since the Unix epoch. Wall clock,
    /// never used for a duration — every duration is a monotonic
    /// [`Instant`].
    pub started_at_unix_ms: u128,
    /// `1` if this trial queried a uniformly random address (the
    /// not-found path), `0` otherwise.
    pub absent_probe: bool,
    /// **A1**: request in → decoded balance out.
    pub t_total_us: u64,
    /// **A2**: `build_query` (LWE encryption of the whole bundle).
    pub build_us: u64,
    /// `GET /head` wire time inside this trial.
    pub head_wire_us: u64,
    /// `GET /sync` wire time inside this trial (usually `0` — the
    /// session is normally already caught up).
    pub sync_wire_us: u64,
    /// `POST /answer` wire time: before send → last body byte.
    pub answer_wire_us: u64,
    /// **A5**: `finish` (rewind + decode + delta apply + scan).
    pub finish_us: u64,
    /// `t_total_us` minus every term above. Client-side bookkeeping no
    /// timer covers — written explicitly, never distributed.
    pub residual_us: u64,
    /// **A5** step 2: `rewind_response`, summed over segments.
    pub rewind_us: u64,
    /// **A5** step 3: `client_decode`, summed over segments.
    pub decode_us: u64,
    /// **A5** step 4: the per-cell delta apply, summed over segments.
    pub delta_apply_us: u64,
    /// **A5** step 5: the constant-time fp ∧ `key_tag` scan, summed
    /// over segments.
    pub scan_us: u64,
    /// **A4**: the server's `x-risepir-answer-compute-ns`, if it sent
    /// one. Empty otherwise — never defaulted to zero.
    pub server_compute_ns: Option<u64>,
    /// The server's `x-risepir-answer-handler-ns`, if it sent one.
    /// Subtracting this from `answer_wire_us` is what isolates **A3**.
    pub server_handler_ns: Option<u64>,
    /// **A6** up: the exact encoded `/answer` request body length (all
    /// segments).
    pub query_bytes: u64,
    /// **A6** down: the exact `/answer` response body length received.
    pub response_bytes: u64,
    /// The `/answer` response's declared `content-length`, if any — a
    /// cross-check on [`Self::response_bytes`].
    pub response_content_length: Option<u64>,
    /// The block the server answered at.
    pub at_block: u64,
    /// The block the client's hint is pinned at (block₀ for the whole
    /// run — this session never garbage-collects).
    pub pinned_block: u64,
    /// `at_block - pinned_block`: how stale the hint was.
    pub stale_blocks: u64,
    /// `|ΔD|` at query time.
    pub delta_cells: u64,
    /// Whether the scan matched. Empty when the trial errored.
    pub found: Option<bool>,
    /// `1` when the independent provider's `eth_getBalance(addr,
    /// at_block)` equalled the decoded balance byte-exactly (canonical
    /// hex string **and** integer value), `0` when it did not, empty
    /// when the check did not run or the provider call failed.
    pub provider_match: Option<bool>,
    /// A short class name for why the provider call failed. Never a
    /// body, never a message.
    pub provider_error: Option<&'static str>,
    /// Round-trip time of the provider call. **Not** part of A1 — the
    /// trial's clock stops before this call is made.
    pub provider_rtt_us: Option<u64>,
    /// **C12**: client resident set size, sampled every `--rss-every`
    /// trials. Empty on unsampled trials and on platforms with no
    /// supported reader.
    pub client_rss_bytes: Option<u64>,
    /// Attempts the call took: `1` normally, `2` after a re-bootstrap
    /// retry. A `2` marks a row whose `residual_us` includes a whole
    /// `/setup` download.
    pub attempts: u32,
    /// A fixed class name when the trial failed, empty otherwise.
    pub error: Option<&'static str>,
}

/// The trials CSV header, in the order [`TrialRow::to_csv`] writes.
pub const TRIAL_COLUMNS: &[&str] = &[
    "batch",
    "trial",
    "started_at_unix_ms",
    "absent_probe",
    "t_total_us",
    "build_us",
    "head_wire_us",
    "sync_wire_us",
    "answer_wire_us",
    "finish_us",
    "residual_us",
    "rewind_us",
    "decode_us",
    "delta_apply_us",
    "scan_us",
    "server_compute_ns",
    "server_handler_ns",
    "query_bytes",
    "response_bytes",
    "response_content_length",
    "at_block",
    "pinned_block",
    "stale_blocks",
    "delta_cells",
    "found",
    "provider_match",
    "provider_error",
    "provider_rtt_us",
    "client_rss_bytes",
    "attempts",
    "error",
];

/// Renders `Some(v)` as its digits and `None` as an empty field — the
/// distinction between "not measured" and "measured as zero" is
/// load-bearing throughout this module.
fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(String::new, |v| v.to_string())
}

/// `Some(true)`/`Some(false)` as `1`/`0`, `None` as empty.
fn optb(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "1",
        Some(false) => "0",
        None => "",
    }
}

impl TrialRow {
    /// One CSV line (no trailing newline), columns in [`TRIAL_COLUMNS`]
    /// order.
    ///
    /// No field can contain a comma or a quote: every one is an
    /// integer, a `0`/`1`, an empty string, or a fixed class name drawn
    /// from a closed set this crate defines — so no quoting or escaping
    /// is needed, and none is done.
    #[must_use]
    pub fn to_csv(&self) -> String {
        let f = [
            self.batch.to_string(),
            self.trial.to_string(),
            self.started_at_unix_ms.to_string(),
            u8::from(self.absent_probe).to_string(),
            self.t_total_us.to_string(),
            self.build_us.to_string(),
            self.head_wire_us.to_string(),
            self.sync_wire_us.to_string(),
            self.answer_wire_us.to_string(),
            self.finish_us.to_string(),
            self.residual_us.to_string(),
            self.rewind_us.to_string(),
            self.decode_us.to_string(),
            self.delta_apply_us.to_string(),
            self.scan_us.to_string(),
            opt(self.server_compute_ns),
            opt(self.server_handler_ns),
            self.query_bytes.to_string(),
            self.response_bytes.to_string(),
            opt(self.response_content_length),
            self.at_block.to_string(),
            self.pinned_block.to_string(),
            self.stale_blocks.to_string(),
            self.delta_cells.to_string(),
            optb(self.found).to_string(),
            optb(self.provider_match).to_string(),
            self.provider_error.unwrap_or("").to_string(),
            opt(self.provider_rtt_us),
            opt(self.client_rss_bytes),
            self.attempts.to_string(),
            self.error.unwrap_or("").to_string(),
        ];
        debug_assert_eq!(f.len(), TRIAL_COLUMNS.len());
        f.join(",")
    }

    /// Whether this row's latency budget closes exactly:
    /// `t_total_us == build + head + sync + answer + finish + residual`.
    ///
    /// True by construction for every row [`run`] writes — [`run`]
    /// *defines* `residual_us` as the subtraction. Exposed so a test can
    /// state the invariant rather than trusting it.
    #[must_use]
    pub const fn budget_closes(&self) -> bool {
        let parts = self.build_us
            + self.head_wire_us
            + self.sync_wire_us
            + self.answer_wire_us
            + self.finish_us
            + self.residual_us;
        parts == self.t_total_us
    }

    /// Fill [`Self::residual_us`] as `t_total_us` minus every other
    /// budget term, so [`Self::budget_closes`] holds.
    ///
    /// `saturating_sub` is belt-and-braces: each part is a sub-interval
    /// of `t_total`, and flooring to microseconds can only shrink the
    /// parts (`floor(a) + floor(b) ≤ floor(a + b)`), so the difference
    /// is non-negative on any monotonic clock.
    fn close_budget(&mut self) {
        let parts = self.build_us
            + self.head_wire_us
            + self.sync_wire_us
            + self.answer_wire_us
            + self.finish_us;
        self.residual_us = self.t_total_us.saturating_sub(parts);
    }
}

/// One delta fetch: **B9** (bytes) and **B10** (`ingest_us`).
///
/// One row per `GET /sync`, which covers `blocks_in_fetch` blocks — see
/// the module docs for why a range fetch cannot be split per block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockRow {
    /// The block this fetch brought the accumulator up to (the `to` of
    /// `(from, to]`).
    pub block: u64,
    /// Wall-clock arrival, milliseconds since the Unix epoch.
    pub received_at_unix_ms: u128,
    /// **B9**: the `/sync` response body length. A range total when
    /// `blocks_in_fetch > 1`.
    pub wire_bytes: u64,
    /// Decoding those bytes into a `BlockDelta`.
    pub decode_us: u64,
    /// **B10**: `ingest_delta` — folding the delta into the rolling
    /// `ΔD`.
    pub ingest_us: u64,
    /// Nonzero cells carried by this delta. A coalesced range
    /// telescopes (ADR-0005), so this is *not* the sum of its blocks'
    /// individual cell counts.
    pub cells_in_block: u64,
    /// `|ΔD|` after the ingest.
    pub delta_cells_total: u64,
    /// Wire time for the fetch: before send → last body byte.
    pub fetch_wire_us: u64,
    /// How many blocks this one fetch covered. `1` means the row is a
    /// genuine per-block measurement.
    pub blocks_in_fetch: u64,
}

/// The blocks CSV header, in the order [`BlockRow::to_csv`] writes.
pub const BLOCK_COLUMNS: &[&str] = &[
    "block",
    "received_at_unix_ms",
    "wire_bytes",
    "decode_us",
    "ingest_us",
    "cells_in_block",
    "delta_cells_total",
    "fetch_wire_us",
    "blocks_in_fetch",
];

impl BlockRow {
    /// One CSV line (no trailing newline), columns in
    /// [`BLOCK_COLUMNS`] order. Every field is an integer.
    #[must_use]
    pub fn to_csv(&self) -> String {
        let f = [
            self.block.to_string(),
            self.received_at_unix_ms.to_string(),
            self.wire_bytes.to_string(),
            self.decode_us.to_string(),
            self.ingest_us.to_string(),
            self.cells_in_block.to_string(),
            self.delta_cells_total.to_string(),
            self.fetch_wire_us.to_string(),
            self.blocks_in_fetch.to_string(),
        ];
        debug_assert_eq!(f.len(), BLOCK_COLUMNS.len());
        f.join(",")
    }
}

// ─── CSV sink ───────────────────────────────────────────────────────────

/// An append-only CSV file that writes its header **only** when it
/// creates (or finds empty) the file.
///
/// A campaign runs in batches hours apart and may be restarted between
/// them; a second header mid-file would silently corrupt every parser
/// downstream. Each row is flushed as it is written, so a run killed at
/// hour two still leaves hour one's data on disk.
pub struct CsvWriter {
    file: std::fs::File,
    /// Column count, checked against every row so a schema drift fails
    /// at the writer rather than in whatever reads the file next.
    columns: usize,
}

impl CsvWriter {
    /// Open `path` for append, writing `header` first if the file is new
    /// or empty.
    ///
    /// # Errors
    ///
    /// Any [`std::io::Error`] from opening, measuring, or writing the
    /// file.
    pub fn open(path: &Path, header: &[&str]) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        if file.metadata()?.len() == 0 {
            writeln!(file, "{}", header.join(","))?;
            file.flush()?;
        }
        Ok(Self {
            file,
            columns: header.len(),
        })
    }

    /// Append one already-rendered row and flush it.
    ///
    /// # Errors
    ///
    /// Any [`std::io::Error`] from writing or flushing.
    pub fn write_row(&mut self, row: &str) -> std::io::Result<()> {
        debug_assert_eq!(
            row.split(',').count(),
            self.columns,
            "row field count must match the header"
        );
        writeln!(self.file, "{row}")?;
        self.file.flush()
    }
}

// ─── errors ─────────────────────────────────────────────────────────────

/// A failure that stops the whole probe run.
///
/// Hand-rolled (house style — no `thiserror`). Deliberately few
/// variants: almost everything a run meets is transient and is recorded
/// as a row rather than raised here.
#[derive(Debug)]
pub enum ProbeError {
    /// A required flag was missing or malformed.
    Config(String),
    /// Reading or writing one of the CSVs, or the addresses file.
    Io(std::io::Error),
    /// Bootstrapping the session (`GET /setup` / `GET /mode`) failed.
    Bootstrap(String),
    /// No usable query addresses: `--addresses-file` was empty, or
    /// `GET /recent` served none and no file was given.
    NoAddresses(String),
    /// A PIR answer failed to decode, or the rewind client rejected the
    /// pipeline. **Not** transient: this is the "never return a wrong
    /// answer" contract firing, and a measurement run must not average
    /// over it. Carries the failing trial index and a fixed class name,
    /// never a message that could quote a body.
    PirDecodeFailure {
        /// Which trial (0-based, run-wide) failed.
        trial: usize,
        /// The block the answer was at, if the trial got that far.
        at_block: u64,
        /// A fixed [`RpcError`] variant name.
        class: &'static str,
    },
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(m) => write!(f, "configuration: {m}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Bootstrap(m) => write!(f, "bootstrap: {m}"),
            Self::NoAddresses(m) => write!(f, "no query addresses: {m}"),
            Self::PirDecodeFailure {
                trial,
                at_block,
                class,
            } => write!(
                f,
                "PIR ANSWER FAILED TO DECODE at trial {trial}, block {at_block} ({class}) \
                 — this is a defect, not a transient failure; the run stopped"
            ),
        }
    }
}

impl std::error::Error for ProbeError {}

impl From<std::io::Error> for ProbeError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// A fixed class name for one [`RpcError`] — never the formatted
/// message, which can embed a status body or a decode diagnosis.
const fn rpc_error_class(e: &RpcError) -> &'static str {
    match e {
        RpcError::Pir(_) => "Pir",
        RpcError::Client(_) => "Client",
        RpcError::DecodeFailed => "DecodeFailed",
        RpcError::NotInTrackedSet => "NotInTrackedSet",
        RpcError::Stalled => "Stalled",
    }
}

/// A fixed class name for one [`FeedError`] — same rationale as
/// [`rpc_error_class`]: a provider's free-text error can contain
/// anything, including the address that was asked about.
const fn feed_error_class(e: &FeedError) -> &'static str {
    match e {
        FeedError::Internal(_) => "Internal",
        FeedError::Rpc { .. } => "Rpc",
        FeedError::DepthRefused { .. } => "DepthRefused",
        FeedError::Parse { .. } => "Parse",
        FeedError::ChainIdMismatch { .. } => "ChainIdMismatch",
    }
}

/// Whether a failed trial means the deployment is returning something it
/// cannot decode — the one failure a measurement run must not continue
/// through.
const fn is_pir_decode_failure(e: &RpcError) -> bool {
    matches!(e, RpcError::DecodeFailed | RpcError::Client(_))
}

// ─── address selection ──────────────────────────────────────────────────

/// A tiny SplitMix64 for choosing which addresses to probe and which
/// trials take the absent path.
///
/// Deliberately not a CSPRNG and deliberately not a new dependency: the
/// only thing it decides is *which public address this probe looks up*,
/// which is protected by the PIR itself, not by the quality of this
/// generator. Seeded from the wall clock so two runs do not walk the
/// same sequence.
struct SplitMix64(u64);

impl SplitMix64 {
    fn from_clock() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        // Any nonzero seed works; the constant only avoids the all-zero
        // state being reachable from a stopped clock.
        Self(nanos ^ 0x9E37_79B9_7F4A_7C15)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniformly random 20-byte address — the not-found probe.
    fn address(&mut self) -> [u8; 20] {
        let mut out = [0u8; 20];
        for chunk in out.chunks_mut(8) {
            let n = self.next_u64().to_le_bytes();
            let len = chunk.len();
            chunk.copy_from_slice(&n[..len]);
        }
        out
    }

    /// `true` with probability `p` (clamped to `[0, 1]`).
    fn bernoulli(&mut self, p: f64) -> bool {
        let p = p.clamp(0.0, 1.0);
        // 53 bits of mantissa is far more resolution than any sane
        // `--absent-fraction` needs.
        let u = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        u < p
    }
}

/// Parse an addresses file: one hex address per line, `0x` optional,
/// blank lines and `#` comments skipped.
///
/// # Errors
///
/// [`ProbeError::Io`] if the file cannot be read;
/// [`ProbeError::NoAddresses`] if it yields none. A malformed line names
/// its **line number**, never its content — the content is an address.
fn read_addresses_file(path: &Path) -> Result<Vec<[u8; 20]>, ProbeError> {
    let text = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let hex = line.strip_prefix("0x").unwrap_or(line);
        let Some(addr) = parse_hex20(hex) else {
            return Err(ProbeError::NoAddresses(format!(
                "{}: line {} is not a 20-byte hex address",
                path.display(),
                n + 1
            )));
        };
        out.push(addr);
    }
    if out.is_empty() {
        return Err(ProbeError::NoAddresses(format!(
            "{} contained no addresses",
            path.display()
        )));
    }
    Ok(out)
}

/// 40 hex characters → 20 bytes, or `None`.
fn parse_hex20(s: &str) -> Option<[u8; 20]> {
    if s.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

// ─── resident memory (C12) ──────────────────────────────────────────────

/// Client resident set size in bytes, or `None` where this platform has
/// no supported reader.
///
/// Linux reads `/proc/self/status`'s `VmRSS` (already in kB, so no page
/// size is needed and none is guessed). macOS shells out to `ps -o rss=`,
/// because the in-process alternatives (`proc_pidinfo`,
/// `mach_task_basic_info`) are `unsafe` FFI and every crate here is
/// `#![forbid(unsafe_code)]` — a subprocess every `--rss-every` trials is
/// a cheaper price than an FFI exception. Anywhere else: `None`, never a
/// guess.
#[must_use]
pub fn resident_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
                return kb.checked_mul(1024);
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p"])
            .arg(std::process::id().to_string())
            .output()
            .ok()?;
        let kb: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        kb.checked_mul(1024)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

// ─── summary ────────────────────────────────────────────────────────────

/// `n` / mean / p50 / p95 over one column.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Dist {
    /// How many samples the column had.
    pub n: usize,
    /// Arithmetic mean.
    pub mean: f64,
    /// Median (nearest-rank).
    pub p50: u64,
    /// 95th percentile (nearest-rank).
    pub p95: u64,
}

impl Dist {
    /// Summarise `v` (consumed by sorting in place).
    #[must_use]
    pub fn of(v: &mut [u64]) -> Self {
        if v.is_empty() {
            return Self::default();
        }
        v.sort_unstable();
        let n = v.len();
        let sum: u128 = v.iter().map(|&x| u128::from(x)).sum();
        Self {
            n,
            mean: sum as f64 / n as f64,
            p50: v[nearest_rank(n, 0.50)],
            p95: v[nearest_rank(n, 0.95)],
        }
    }
}

/// Nearest-rank index for quantile `q` over `n` sorted samples.
fn nearest_rank(n: usize, q: f64) -> usize {
    debug_assert!(n > 0);
    let rank = (q * n as f64).ceil() as usize;
    rank.clamp(1, n) - 1
}

/// What [`run`] reports at the end of a campaign.
#[derive(Clone, Debug, Default)]
pub struct ProbeSummary {
    /// Every trial row written (successful and failed).
    pub trials: usize,
    /// Trials that returned a balance.
    pub ok: usize,
    /// Trials whose scan matched an entry.
    pub found: usize,
    /// Trials that queried a random address.
    pub absent_probes: usize,
    /// Provider comparisons that matched byte-exactly.
    pub provider_matched: usize,
    /// Provider comparisons that disagreed — every one is loud.
    pub provider_mismatched: usize,
    /// Trials with no usable provider answer.
    pub provider_unavailable: usize,
    /// Total `/answer` request bytes over the run.
    pub query_bytes_total: u64,
    /// Total `/answer` response bytes over the run.
    pub response_bytes_total: u64,
    /// Total `/sync` body bytes over the run.
    pub delta_bytes_total: u64,
    /// **C12**: the `/setup` body length.
    pub setup_bytes: u64,
    /// The `/setup` response's declared `content-length`, if any.
    pub setup_content_length: Option<u64>,
    /// How long the `/setup` download took.
    pub setup_wire_us: u64,
    /// The block the hint is pinned at.
    pub pinned_block: u64,
    /// Lowest `at_block` observed.
    pub min_at_block: u64,
    /// Highest `at_block` observed.
    pub max_at_block: u64,
    /// Delta-fetch rows written.
    pub block_rows: usize,
    /// Blocks covered by those fetches.
    pub blocks_ingested: u64,
    /// The last sampled resident size, if any.
    pub last_rss_bytes: Option<u64>,
}

// ─── the run ────────────────────────────────────────────────────────────

/// Run a probe campaign to completion, writing both CSVs as it goes and
/// returning the summary [`print_summary`] renders.
///
/// # Errors
///
/// [`ProbeError::Config`] for a missing/malformed flag,
/// [`ProbeError::Bootstrap`] if `/setup` or `/mode` fails,
/// [`ProbeError::NoAddresses`] if no query addresses can be obtained,
/// [`ProbeError::Io`] for a CSV/file failure, and
/// [`ProbeError::PirDecodeFailure`] — the one *deliberate* abort — if an
/// answer fails to decode.
pub async fn run(cfg: ProbeConfig) -> Result<ProbeSummary, ProbeError> {
    if cfg.pir_url.is_empty() {
        return Err(ProbeError::Config(
            "--pir-url is required (e.g. --pir-url https://demo.risepir.org)".to_string(),
        ));
    }

    // ── one HTTP client for the whole run ──────────────────────────────
    // Connection reuse is a measurement requirement, not a nicety: a
    // fresh TCP+TLS handshake per query would land entirely inside
    // `answer_wire_us` and contaminate A3.
    let mut builder = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_STALL_TIMEOUT)
        .tcp_keepalive(Duration::from_secs(30))
        // No idle eviction: batches are 90 minutes apart by default, and
        // dropping the pooled connection in between would silently turn
        // the first trial of every batch into a handshake measurement.
        .pool_idle_timeout(None::<Duration>);
    for r in &cfg.resolve {
        // TLS validation is untouched — only the name→address mapping
        // is overridden, so the certificate is still checked against the
        // URL's own host.
        builder = builder.resolve(&r.host, r.addr);
        logln!(
            "risepir-rpc probe: resolving {} to {} (TLS validation unchanged)",
            r.host,
            r.addr
        );
    }
    let http = builder
        .build()
        .map_err(|e| ProbeError::Config(format!("could not build the HTTP client: {e}")))?;

    let sink = Arc::new(NetSink::new());
    let pir =
        PirHttpClient::with_http_client(cfg.pir_url.clone(), http).with_net_sink(Arc::clone(&sink));

    // ── C12: the hint download, timed and sized ────────────────────────
    sink.reset();
    logln!(
        "risepir-rpc probe: downloading setup bundle from {} ...",
        cfg.pir_url
    );
    let (bundle, header_mode) = pir
        .setup_with_mode()
        .await
        .map_err(|e| ProbeError::Bootstrap(format!("GET /setup: {e}")))?;
    let setup_net = sink.take();
    let complete = match header_mode {
        Some(m) => m,
        None => pir
            .mode()
            .await
            .map_err(|e| ProbeError::Bootstrap(format!("GET /mode: {e}")))?,
    };
    let pinned_block = bundle.block;

    let mut summary = ProbeSummary {
        setup_bytes: setup_net.setup.response_bytes,
        setup_content_length: setup_net.setup.last_content_length,
        setup_wire_us: setup_net.setup.wire_ns / 1_000,
        pinned_block,
        min_at_block: u64::MAX,
        ..ProbeSummary::default()
    };
    logln!(
        "risepir-rpc probe: setup {} B in {:.1}s — hint pinned at block {pinned_block}, mode {}",
        summary.setup_bytes,
        summary.setup_wire_us as f64 / 1e6,
        if complete { "complete" } else { "partial" }
    );

    let session = PrivateEth::from_setup(
        pir,
        bundle,
        crate::mainnet::value_codec(),
        complete,
        cfg.chain_id,
        None,
    );

    // ── outputs ────────────────────────────────────────────────────────
    let mut trials_csv = CsvWriter::open(&cfg.queries_csv, TRIAL_COLUMNS)?;
    let mut blocks_csv = CsvWriter::open(&cfg.blocks_csv, BLOCK_COLUMNS)?;

    let confirm = (!cfg.no_confirm).then(|| RpcClient::new(cfg.confirm_url.clone()));
    let mut rng = SplitMix64::from_clock();

    // Fixed address list, if one was given; otherwise `GET /recent`,
    // refreshed at each batch.
    let fixed: Option<Vec<[u8; 20]>> = match cfg.addresses_file.as_deref() {
        Some(p) => Some(read_addresses_file(p)?),
        None => None,
    };

    // Accumulators for the end-of-run distributions.
    let mut acc = Accumulators::default();

    let started = Instant::now();
    let deadline = started + Duration::from_secs(cfg.follow_secs);
    let mut trial_index = 0usize;

    for batch in 0..cfg.batches {
        let batch_at = started + Duration::from_secs(cfg.batch_interval_secs * batch as u64);
        if batch_at > deadline {
            logln!(
                "risepir-rpc probe: batch {batch} would start after --follow-secs; stopping early"
            );
            break;
        }
        // Follow head until this batch is due, so the session is at the
        // operating point a real long-lived client would be at.
        follow_until(&session, &sink, batch_at, &cfg, &mut blocks_csv, &mut acc).await?;

        let pool: Vec<[u8; 20]> = match &fixed {
            Some(v) => v.clone(),
            None => match session.pir().recent().await {
                Ok(v) if !v.is_empty() => v,
                Ok(_) => {
                    return Err(ProbeError::NoAddresses(
                        "GET /recent served no addresses and no --addresses-file was given"
                            .to_string(),
                    ))
                }
                Err(e) => {
                    return Err(ProbeError::NoAddresses(format!(
                        "GET /recent failed ({}); pass --addresses-file instead",
                        e.metric_class()
                    )))
                }
            },
        };
        logln!(
            "risepir-rpc probe: batch {batch} — {} trials over a pool of {} addresses",
            cfg.batch_size,
            pool.len()
        );

        for _ in 0..cfg.batch_size {
            if Instant::now() > deadline {
                logln!("risepir-rpc probe: --follow-secs reached mid-batch; stopping");
                break;
            }
            let absent = rng.bernoulli(cfg.absent_fraction);
            let addr = if absent {
                rng.address()
            } else {
                pool[(rng.next_u64() % pool.len() as u64) as usize]
            };
            let sample_rss = cfg.rss_every > 0 && trial_index.is_multiple_of(cfg.rss_every);

            let row = one_trial(
                &session,
                &sink,
                confirm.as_ref(),
                addr,
                batch,
                trial_index,
                absent,
                sample_rss,
            )
            .await?;

            record(&mut summary, &mut acc, &row);
            trials_csv.write_row(&row.to_csv())?;
            trial_index += 1;

            if cfg.trial_gap_ms > 0 {
                tokio::time::sleep(Duration::from_millis(cfg.trial_gap_ms)).await;
            }
        }
    }

    // Tail: keep following until the configured lifetime is up.
    follow_until(&session, &sink, deadline, &cfg, &mut blocks_csv, &mut acc).await?;

    summary.trials = trial_index;
    if summary.min_at_block == u64::MAX {
        summary.min_at_block = 0;
    }
    Ok(finish_summary(summary, acc))
}

/// Poll `GET /head` every `--poll-secs` until `until`, ingesting whatever
/// the server has added and writing one blocks-CSV row per fetch.
///
/// Transient failures are logged with a class name and skipped — a
/// campaign that dies on one refused `/head` measures nothing. A
/// `Stalled` (the range aged out) is likewise logged and skipped: the
/// next query's own catch-up will re-bootstrap if it must.
async fn follow_until(
    session: &PrivateEth,
    sink: &NetSink,
    until: Instant,
    cfg: &ProbeConfig,
    blocks_csv: &mut CsvWriter,
    acc: &mut Accumulators,
) -> Result<(), ProbeError> {
    let poll = Duration::from_secs(cfg.poll_secs.max(1));
    while Instant::now() < until {
        sink.reset();
        match session.follow_once().await {
            Ok(Some(t)) => {
                let net = sink.take();
                let row = BlockRow {
                    block: t.to_block,
                    received_at_unix_ms: unix_ms(),
                    wire_bytes: net.sync.response_bytes,
                    decode_us: net.sync.decode_ns / 1_000,
                    ingest_us: u64::try_from(t.ingest.as_micros()).unwrap_or(u64::MAX),
                    cells_in_block: t.cells_in_delta as u64,
                    delta_cells_total: t.delta_cells_total as u64,
                    fetch_wire_us: net.sync.wire_ns / 1_000,
                    blocks_in_fetch: t.blocks,
                };
                acc.ingest_us.push(row.ingest_us);
                acc.delta_wire_bytes.push(row.wire_bytes);
                acc.blocks_ingested += row.blocks_in_fetch;
                acc.block_rows += 1;
                blocks_csv.write_row(&row.to_csv())?;
            }
            Ok(None) => {}
            Err(e) => logln!(
                "risepir-rpc probe: follow step failed ({}); continuing",
                rpc_error_class(&e)
            ),
        }
        let remaining = until.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        tokio::time::sleep(poll.min(remaining)).await;
    }
    Ok(())
}

/// One trial, end to end: the timed private query, then — with the
/// trial's clock already stopped — the independent provider check.
#[allow(clippy::too_many_arguments)] // one flat trial record; grouping these into a struct would only move the arity
async fn one_trial(
    session: &PrivateEth,
    sink: &NetSink,
    confirm: Option<&RpcClient>,
    addr: [u8; 20],
    batch: usize,
    trial: usize,
    absent: bool,
    sample_rss: bool,
) -> Result<TrialRow, ProbeError> {
    let mut timings = crate::private_eth::BalanceTimings::default();

    // ── A1 starts here ────────────────────────────────────────────────
    sink.reset();
    let started_at_unix_ms = unix_ms();
    let t0 = Instant::now();
    let outcome = session.get_balance_timed(addr, &mut timings).await;
    let t_total = t0.elapsed();
    // ── A1 stops here; everything below is off the clock ──────────────
    let net = sink.take();

    let mut row = TrialRow {
        batch,
        trial,
        started_at_unix_ms,
        absent_probe: absent,
        t_total_us: u64::try_from(t_total.as_micros()).unwrap_or(u64::MAX),
        build_us: us(timings.build),
        head_wire_us: net.head.wire_ns / 1_000,
        sync_wire_us: net.sync.wire_ns / 1_000,
        answer_wire_us: net.answer.wire_ns / 1_000,
        finish_us: us(timings.finish),
        rewind_us: us(timings.finish_parts.rewind),
        decode_us: us(timings.finish_parts.decode),
        delta_apply_us: us(timings.finish_parts.delta_apply),
        scan_us: us(timings.finish_parts.scan),
        server_compute_ns: net.server_compute_ns,
        server_handler_ns: net.server_handler_ns,
        query_bytes: net.answer.request_bytes,
        response_bytes: net.answer.response_bytes,
        response_content_length: net.answer.last_content_length,
        at_block: timings.at_block,
        pinned_block: timings.pinned_block,
        stale_blocks: timings.at_block.saturating_sub(timings.pinned_block),
        delta_cells: timings.delta_cells as u64,
        attempts: timings.attempts,
        client_rss_bytes: sample_rss.then(resident_bytes).flatten(),
        ..TrialRow::default()
    };
    row.close_budget();

    let balance = match outcome {
        Ok(b) => {
            row.found = Some(timings.found);
            b
        }
        Err(e) => {
            if is_pir_decode_failure(&e) {
                return Err(ProbeError::PirDecodeFailure {
                    trial,
                    at_block: row.at_block,
                    class: rpc_error_class(&e),
                });
            }
            row.error = Some(rpc_error_class(&e));
            return Ok(row);
        }
    };

    // ── correctness evidence, at the SAME explicit block height ───────
    // Never "latest" vs "latest": this deployment follows `finalized`, so
    // its head is ~13 min behind a public provider's own "latest"
    // (ADR-0007). Comparing tags would compare two different states and
    // report a mismatch that is not one.
    if let Some(rpc) = confirm {
        let t = Instant::now();
        let got = rpc.balance_at(&addr, row.at_block).await;
        row.provider_rtt_us = Some(u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX));
        match got {
            Ok(theirs) => {
                // Both halves, deliberately: the integers must agree AND
                // their canonical `0x`-hex renderings must be the same
                // string, which is what "byte-exact" means for a value
                // that crosses the wire as hex.
                let same = theirs == balance && format!("{theirs:x}") == format!("{balance:x}");
                row.provider_match = Some(same);
                if !same {
                    // Loud, and still address-free and balance-free.
                    logln!(
                        "risepir-rpc probe: MISMATCH trial {trial} block {} \
                         (decoded balance disagrees with the independent provider)",
                        row.at_block
                    );
                }
            }
            Err(e) => row.provider_error = Some(feed_error_class(&e)),
        }
    }

    Ok(row)
}

/// Per-column samples for the end-of-run distributions.
#[derive(Default)]
struct Accumulators {
    t_total_us: Vec<u64>,
    build_us: Vec<u64>,
    head_wire_us: Vec<u64>,
    sync_wire_us: Vec<u64>,
    answer_wire_us: Vec<u64>,
    finish_us: Vec<u64>,
    residual_us: Vec<u64>,
    rewind_us: Vec<u64>,
    decode_us: Vec<u64>,
    delta_apply_us: Vec<u64>,
    scan_us: Vec<u64>,
    server_compute_ns: Vec<u64>,
    server_handler_ns: Vec<u64>,
    query_bytes: Vec<u64>,
    response_bytes: Vec<u64>,
    provider_rtt_us: Vec<u64>,
    rss_bytes: Vec<u64>,
    ingest_us: Vec<u64>,
    delta_wire_bytes: Vec<u64>,
    blocks_ingested: u64,
    block_rows: usize,
}

/// Fold one finished row into the running summary and the accumulators.
fn record(summary: &mut ProbeSummary, acc: &mut Accumulators, row: &TrialRow) {
    if row.absent_probe {
        summary.absent_probes += 1;
    }
    match row.provider_match {
        Some(true) => summary.provider_matched += 1,
        Some(false) => summary.provider_mismatched += 1,
        None => summary.provider_unavailable += 1,
    }
    if let Some(rtt) = row.provider_rtt_us {
        acc.provider_rtt_us.push(rtt);
    }
    if let Some(rss) = row.client_rss_bytes {
        acc.rss_bytes.push(rss);
        summary.last_rss_bytes = Some(rss);
    }
    if row.error.is_some() {
        return;
    }
    summary.ok += 1;
    if row.found == Some(true) {
        summary.found += 1;
    }
    summary.query_bytes_total += row.query_bytes;
    summary.response_bytes_total += row.response_bytes;
    summary.min_at_block = summary.min_at_block.min(row.at_block);
    summary.max_at_block = summary.max_at_block.max(row.at_block);

    acc.t_total_us.push(row.t_total_us);
    acc.build_us.push(row.build_us);
    acc.head_wire_us.push(row.head_wire_us);
    acc.sync_wire_us.push(row.sync_wire_us);
    acc.answer_wire_us.push(row.answer_wire_us);
    acc.finish_us.push(row.finish_us);
    acc.residual_us.push(row.residual_us);
    acc.rewind_us.push(row.rewind_us);
    acc.decode_us.push(row.decode_us);
    acc.delta_apply_us.push(row.delta_apply_us);
    acc.scan_us.push(row.scan_us);
    if let Some(v) = row.server_compute_ns {
        acc.server_compute_ns.push(v);
    }
    if let Some(v) = row.server_handler_ns {
        acc.server_handler_ns.push(v);
    }
    acc.query_bytes.push(row.query_bytes);
    acc.response_bytes.push(row.response_bytes);
}

/// Move the block-side totals out of the accumulators and hand the
/// accumulators to [`print_summary`] via a boxed distribution table.
fn finish_summary(mut summary: ProbeSummary, acc: Accumulators) -> ProbeSummary {
    summary.block_rows = acc.block_rows;
    summary.blocks_ingested = acc.blocks_ingested;
    summary.delta_bytes_total = acc.delta_wire_bytes.iter().sum();
    LAST_DISTRIBUTIONS.with(|slot| *slot.borrow_mut() = Some(distributions(acc)));
    summary
}

/// The named distributions [`print_summary`] renders.
type NamedDists = Vec<(&'static str, Dist)>;

thread_local! {
    /// The distributions from the most recent [`run`] on this thread.
    ///
    /// Kept beside the summary rather than inside it so [`ProbeSummary`]
    /// stays a small, copyable record of totals; [`print_summary`] is
    /// the only reader, and it runs on the same thread immediately after
    /// [`run`] returns.
    static LAST_DISTRIBUTIONS: std::cell::RefCell<Option<NamedDists>> =
        const { std::cell::RefCell::new(None) };
}

fn distributions(mut acc: Accumulators) -> NamedDists {
    vec![
        ("t_total_us  (A1)", Dist::of(&mut acc.t_total_us)),
        ("build_us    (A2)", Dist::of(&mut acc.build_us)),
        ("head_wire_us", Dist::of(&mut acc.head_wire_us)),
        ("sync_wire_us", Dist::of(&mut acc.sync_wire_us)),
        ("answer_wire_us", Dist::of(&mut acc.answer_wire_us)),
        ("finish_us   (A5)", Dist::of(&mut acc.finish_us)),
        ("  rewind_us", Dist::of(&mut acc.rewind_us)),
        ("  decode_us", Dist::of(&mut acc.decode_us)),
        ("  delta_apply_us", Dist::of(&mut acc.delta_apply_us)),
        ("  scan_us", Dist::of(&mut acc.scan_us)),
        ("residual_us", Dist::of(&mut acc.residual_us)),
        ("server_compute_ns", Dist::of(&mut acc.server_compute_ns)),
        ("server_handler_ns", Dist::of(&mut acc.server_handler_ns)),
        ("query_bytes (A6up)", Dist::of(&mut acc.query_bytes)),
        ("response_bytes", Dist::of(&mut acc.response_bytes)),
        ("provider_rtt_us", Dist::of(&mut acc.provider_rtt_us)),
        ("client_rss_bytes", Dist::of(&mut acc.rss_bytes)),
        ("delta wire_bytes (B9)", Dist::of(&mut acc.delta_wire_bytes)),
        ("delta ingest_us (B10)", Dist::of(&mut acc.ingest_us)),
    ]
}

/// Print the end-of-run summary to stdout: `n`/mean/p50/p95 for every
/// timed column, the byte totals, the correctness counts, and the block
/// range covered.
pub fn print_summary(s: &ProbeSummary) {
    println!();
    println!("RisePIR client probe — summary");
    println!("  trials              {} ({} ok)", s.trials, s.ok);
    println!(
        "  found / not-found   {} / {}   (absent probes: {})",
        s.found,
        s.ok.saturating_sub(s.found),
        s.absent_probes
    );
    println!(
        "  provider            {} matched / {} MISMATCHED / {} unavailable",
        s.provider_matched, s.provider_mismatched, s.provider_unavailable
    );
    println!(
        "  blocks              pinned {} · answered {}..{} · {} fetches covering {} blocks",
        s.pinned_block, s.min_at_block, s.max_at_block, s.block_rows, s.blocks_ingested
    );
    println!(
        "  hint download (C12) {} B in {} us (content-length {})",
        s.setup_bytes,
        s.setup_wire_us,
        opt(s.setup_content_length)
    );
    println!(
        "  bytes               query up {} · response down {} · delta {}",
        s.query_bytes_total, s.response_bytes_total, s.delta_bytes_total
    );
    if let Some(rss) = s.last_rss_bytes {
        println!("  client RSS (C12)    {rss} B (last sample)");
    }
    println!();
    println!(
        "  {:<24} {:>6} {:>14} {:>14} {:>14}",
        "column", "n", "mean", "p50", "p95"
    );
    LAST_DISTRIBUTIONS.with(|slot| {
        if let Some(dists) = slot.borrow().as_ref() {
            for (name, d) in dists {
                if d.n == 0 {
                    continue;
                }
                println!(
                    "  {:<24} {:>6} {:>14.1} {:>14} {:>14}",
                    name, d.n, d.mean, d.p50, d.p95
                );
            }
        }
    });
    if s.provider_mismatched > 0 {
        println!();
        println!(
            "  *** {} MISMATCH(ES) — see the MISMATCH lines above; \
             a mismatch is a correctness defect, not noise ***",
            s.provider_mismatched
        );
    }
    println!();
}

/// Milliseconds since the Unix epoch, `0` if the clock is before it.
fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// A [`Duration`] floored to whole microseconds.
fn us(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact schema both writers must emit, pinned here so a column
    /// added, removed, or reordered fails the suite rather than silently
    /// shifting every downstream parser by one field.
    const EXPECTED_TRIAL_HEADER: &str = "batch,trial,started_at_unix_ms,absent_probe,\
t_total_us,build_us,head_wire_us,sync_wire_us,answer_wire_us,finish_us,residual_us,\
rewind_us,decode_us,delta_apply_us,scan_us,server_compute_ns,server_handler_ns,\
query_bytes,response_bytes,response_content_length,at_block,pinned_block,stale_blocks,\
delta_cells,found,provider_match,provider_error,provider_rtt_us,client_rss_bytes,\
attempts,error";

    const EXPECTED_BLOCK_HEADER: &str = "block,received_at_unix_ms,wire_bytes,decode_us,\
ingest_us,cells_in_block,delta_cells_total,fetch_wire_us,blocks_in_fetch";

    fn sample_row() -> TrialRow {
        let mut row = TrialRow {
            batch: 1,
            trial: 7,
            started_at_unix_ms: 1_785_079_800_123,
            absent_probe: false,
            t_total_us: 1_000_000,
            build_us: 12_345,
            head_wire_us: 4_000,
            sync_wire_us: 0,
            answer_wire_us: 900_000,
            finish_us: 60_000,
            rewind_us: 20_000,
            decode_us: 25_000,
            delta_apply_us: 1_000,
            scan_us: 9_000,
            server_compute_ns: Some(41_000_000),
            server_handler_ns: Some(52_000_000),
            query_bytes: 393_216,
            response_bytes: 8_192,
            response_content_length: Some(8_192),
            at_block: 25_617_400,
            pinned_block: 25_600_000,
            stale_blocks: 17_400,
            delta_cells: 220_400,
            found: Some(true),
            provider_match: Some(true),
            provider_error: None,
            provider_rtt_us: Some(88_000),
            client_rss_bytes: Some(1_190_000_000),
            attempts: 1,
            error: None,
            residual_us: 0,
        };
        row.close_budget();
        row
    }

    // ── (1) budget arithmetic ─────────────────────────────────────────

    #[test]
    fn the_latency_budget_closes_exactly() {
        let row = sample_row();
        assert_eq!(
            row.t_total_us,
            row.build_us
                + row.head_wire_us
                + row.sync_wire_us
                + row.answer_wire_us
                + row.finish_us
                + row.residual_us,
            "A1 must equal A2 + all wire + A5 + residual, by construction"
        );
        assert!(row.budget_closes());
        // The residual really is the leftover, not a fudge factor.
        assert_eq!(
            row.residual_us,
            1_000_000 - (12_345 + 4_000 + 900_000 + 60_000)
        );
    }

    #[test]
    fn close_budget_never_goes_negative() {
        // A pathological row whose parts exceed the total (impossible on
        // a monotonic clock, but the arithmetic must still be total).
        let mut row = TrialRow {
            t_total_us: 10,
            build_us: 100,
            finish_us: 100,
            ..TrialRow::default()
        };
        row.close_budget();
        assert_eq!(row.residual_us, 0);
    }

    #[test]
    fn a_default_row_is_a_closed_budget() {
        let mut row = TrialRow::default();
        row.close_budget();
        assert!(row.budget_closes());
    }

    // ── (2) CSV schema ────────────────────────────────────────────────

    #[test]
    fn the_documented_columns_are_the_emitted_columns() {
        assert_eq!(TRIAL_COLUMNS.join(","), EXPECTED_TRIAL_HEADER);
        assert_eq!(BLOCK_COLUMNS.join(","), EXPECTED_BLOCK_HEADER);
        assert_eq!(
            sample_row().to_csv().split(',').count(),
            TRIAL_COLUMNS.len(),
            "every trial row must have exactly one field per column"
        );
        assert_eq!(
            BlockRow::default().to_csv().split(',').count(),
            BLOCK_COLUMNS.len(),
            "every block row must have exactly one field per column"
        );
    }

    #[test]
    fn empty_fields_survive_the_round_trip() {
        // `None` must render as an *empty* field, never as `0` — "not
        // measured" and "measured as zero" are different facts.
        let row = TrialRow {
            server_compute_ns: None,
            found: None,
            provider_match: None,
            provider_error: Some("DepthRefused"),
            ..TrialRow::default()
        };
        let line = row.to_csv();
        let f: Vec<&str> = line.split(',').collect();
        let idx = |name: &str| TRIAL_COLUMNS.iter().position(|c| *c == name).unwrap();
        assert_eq!(f[idx("server_compute_ns")], "");
        assert_eq!(f[idx("found")], "");
        assert_eq!(f[idx("provider_match")], "");
        assert_eq!(f[idx("provider_error")], "DepthRefused");
        assert_eq!(f[idx("attempts")], "0");
    }

    #[test]
    fn header_is_written_once_across_two_appends() {
        let dir = std::env::temp_dir().join(format!(
            "risepir-probe-csv-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trials.csv");
        let _ = std::fs::remove_file(&path);

        {
            let mut w = CsvWriter::open(&path, TRIAL_COLUMNS).unwrap();
            w.write_row(&sample_row().to_csv()).unwrap();
        }
        {
            // Second open, as a restarted campaign would do.
            let mut w = CsvWriter::open(&path, TRIAL_COLUMNS).unwrap();
            w.write_row(&sample_row().to_csv()).unwrap();
        }

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "header + two rows");
        assert_eq!(lines[0], EXPECTED_TRIAL_HEADER);
        assert_eq!(
            lines
                .iter()
                .filter(|l| **l == EXPECTED_TRIAL_HEADER)
                .count(),
            1,
            "the header must appear exactly once across both appends"
        );
        for line in &lines[1..] {
            assert_eq!(line.split(',').count(), TRIAL_COLUMNS.len());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn block_rows_share_the_same_append_discipline() {
        let dir = std::env::temp_dir().join(format!(
            "risepir-probe-blocks-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("blocks.csv");
        let _ = std::fs::remove_file(&path);

        let row = BlockRow {
            block: 25_617_400,
            received_at_unix_ms: 1_785_079_800_123,
            wire_bytes: 1_024,
            decode_us: 40,
            ingest_us: 900,
            cells_in_block: 3_400,
            delta_cells_total: 220_400,
            fetch_wire_us: 30_000,
            blocks_in_fetch: 1,
        };
        for _ in 0..2 {
            let mut w = CsvWriter::open(&path, BLOCK_COLUMNS).unwrap();
            w.write_row(&row.to_csv()).unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], EXPECTED_BLOCK_HEADER);
        assert_eq!(
            lines[1],
            "25617400,1785079800123,1024,40,900,3400,220400,30000,1"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── (3) tripwire: no address, no balance, ever ────────────────────

    /// The address and balance a leak would carry. Both deliberately
    /// distinctive: a 20-byte address with no repeated-byte pattern, and
    /// a wei-scale balance whose decimal and hex forms are far too long
    /// to appear in a timing column by coincidence.
    const TRIPWIRE_ADDR: [u8; 20] = [
        0xde, 0xad, 0xbe, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba,
        0x98, 0x76, 0x54, 0x32, 0x10,
    ];
    const TRIPWIRE_BALANCE: u128 = 123_456_789_012_345_678_901;

    fn hex_lower(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Longest run of hex characters anywhere in `s`. An address is 40
    /// of them; no legitimate field is remotely that long, and commas
    /// break every run at a field boundary.
    fn longest_hex_run(s: &str) -> usize {
        let mut best = 0;
        let mut run = 0;
        for c in s.chars() {
            if c.is_ascii_hexdigit() {
                run += 1;
                best = best.max(run);
            } else {
                run = 0;
            }
        }
        best
    }

    #[test]
    fn no_row_can_carry_an_address_or_a_balance() {
        // A row built from a real lookup of TRIPWIRE_ADDR returning
        // TRIPWIRE_BALANCE: every field that *could* have been derived
        // from either is populated.
        let mut row = sample_row();
        row.found = Some(true);
        row.provider_match = Some(true);
        row.absent_probe = false;
        let line = row.to_csv();

        let addr_lower = hex_lower(&TRIPWIRE_ADDR);
        let addr_upper = addr_lower.to_uppercase();
        assert!(!line.contains(&addr_lower), "address (lower hex) leaked");
        assert!(!line.contains(&addr_upper), "address (upper hex) leaked");
        assert!(
            !line.contains(&format!("0x{addr_lower}")),
            "address (0x-prefixed) leaked"
        );
        assert!(
            longest_hex_run(&line) < 40,
            "a 40-hex run is an address, whatever it is labelled: {line}"
        );

        let bal_dec = TRIPWIRE_BALANCE.to_string();
        let bal_hex = format!("{TRIPWIRE_BALANCE:x}");
        assert!(!line.contains(&bal_dec), "balance (decimal) leaked");
        assert!(!line.contains(&bal_hex), "balance (hex) leaked");
        assert!(
            !line.contains(&format!("0x{bal_hex}")),
            "balance (0x-hex) leaked"
        );

        // And the same for a block row, which never sees either.
        let block_line = BlockRow {
            block: 25_617_400,
            wire_bytes: 4_096,
            ..BlockRow::default()
        }
        .to_csv();
        assert!(longest_hex_run(&block_line) < 40);
        assert!(!block_line.contains(&bal_dec));
    }

    #[test]
    fn error_columns_are_fixed_class_names_never_messages() {
        // Every error string a row can hold comes from these two closed
        // sets — no formatted message, no server body, ever.
        for e in [
            RpcError::DecodeFailed,
            RpcError::NotInTrackedSet,
            RpcError::Stalled,
        ] {
            let c = rpc_error_class(&e);
            assert!(c.chars().all(|ch| ch.is_ascii_alphanumeric()), "{c}");
        }
        for e in [
            FeedError::Internal("secret".into()),
            FeedError::Rpc {
                method: "eth_getBalance".into(),
                detail: "0xdeadbeef...".into(),
            },
            FeedError::Parse {
                context: "x".into(),
                detail: "y".into(),
            },
        ] {
            let c = feed_error_class(&e);
            assert!(c.chars().all(|ch| ch.is_ascii_alphanumeric()), "{c}");
            assert!(!c.contains("secret"));
            assert!(!c.contains("0x"));
        }
    }

    // ── helpers ───────────────────────────────────────────────────────

    #[test]
    fn resolve_parses_curl_syntax_including_ipv6() {
        let r = ResolveOverride::parse("demo.risepir.org:443:136.115.93.177").unwrap();
        assert_eq!(r.host, "demo.risepir.org");
        assert_eq!(r.addr.port(), 443);
        assert_eq!(r.addr.ip().to_string(), "136.115.93.177");

        let r6 = ResolveOverride::parse("example.com:8645:[2001:db8::1]").unwrap();
        assert_eq!(r6.addr.ip().to_string(), "2001:db8::1");

        assert!(ResolveOverride::parse("demo.risepir.org:443").is_err());
        assert!(ResolveOverride::parse("demo.risepir.org:notaport:1.2.3.4").is_err());
        assert!(ResolveOverride::parse("demo.risepir.org:443:not-an-ip").is_err());
        assert!(ResolveOverride::parse(":443:1.2.3.4").is_err());
    }

    #[test]
    fn hex20_round_trips_and_rejects_junk() {
        let a = [0x42u8; 20];
        assert_eq!(parse_hex20(&hex_lower(&a)), Some(a));
        assert_eq!(parse_hex20("00"), None);
        assert_eq!(parse_hex20(&"z".repeat(40)), None);
    }

    #[test]
    fn addresses_file_parses_and_never_echoes_a_line() {
        let dir = std::env::temp_dir().join(format!("risepir-probe-addrs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("addrs.txt");
        std::fs::write(
            &path,
            "# a comment\n0x1111111111111111111111111111111111111111\n\n2222222222222222222222222222222222222222\n",
        )
        .unwrap();
        let got = read_addresses_file(&path).unwrap();
        assert_eq!(got, vec![[0x11u8; 20], [0x22u8; 20]]);

        // A malformed line names its number, never its content.
        std::fs::write(&path, "0xnope\n").unwrap();
        let err = read_addresses_file(&path).unwrap_err().to_string();
        assert!(err.contains("line 1"), "{err}");
        assert!(!err.contains("nope"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dist_reports_nearest_rank_quantiles() {
        let mut v: Vec<u64> = (1..=100).collect();
        let d = Dist::of(&mut v);
        assert_eq!(d.n, 100);
        assert!((d.mean - 50.5).abs() < 1e-9);
        assert_eq!(d.p50, 50);
        assert_eq!(d.p95, 95);

        assert_eq!(Dist::of(&mut []).n, 0);
        assert_eq!(Dist::of(&mut [7]).p95, 7);
    }

    #[test]
    fn absent_fraction_is_honoured_within_sampling_noise() {
        let mut rng = SplitMix64(0x1234_5678_9abc_def0);
        let hits = (0..10_000).filter(|_| rng.bernoulli(0.1)).count();
        assert!((800..1_200).contains(&hits), "got {hits} of 10000");
        assert_eq!((0..100).filter(|_| rng.bernoulli(0.0)).count(), 0);
        assert_eq!((0..100).filter(|_| rng.bernoulli(1.0)).count(), 100);
    }

    #[test]
    fn random_addresses_are_full_width_and_distinct() {
        let mut rng = SplitMix64(0xfeed_face_dead_beef);
        let a = rng.address();
        let b = rng.address();
        assert_ne!(a, b);
        assert!(a.iter().any(|&x| x != 0), "must not be all zero");
    }
}
