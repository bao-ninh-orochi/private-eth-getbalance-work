//! `--hard-refresh <file>` (ADR-0040): a one-time, multi-provider quorum
//! re-verification of a caller-supplied address list against the chain,
//! correcting the store only where **every** configured provider agrees
//! on a value that differs from what is currently stored. The remedy for
//! accounts a snapshot export got wrong at its own boundary
//! (`docs/deploy.md` §2.1) whose error a block-touch never happens to
//! heal — chiefly relative withdrawal credits inside a
//! `--snapshot-rewind` window (`crate::snapshot_rewind`'s docs explain why
//! those do not self-heal from a replay the way absolute changes do).
//!
//! # The three invariants this module exists to hold, always
//!
//! 1. **A correction is only ever produced from unanimous agreement.**
//!    [`quorum`] treats *any* fetch error or *any* disagreement among the
//!    configured providers as "no usable answer" — never as a guess — and
//!    [`decide_correction`] (the one function that turns a quorum outcome
//!    plus the store's current value into a possible write) can reach its
//!    [`CheckOutcome::Correct`] arm only through [`Quorum::Agreed`]. There
//!    is no code path from a fetch error or a disagreement to a queued
//!    correction — [`run`] never constructs a [`Correction`] except by
//!    matching that one arm.
//! 2. **No block ever receives more than [`MAX_CORRECTIONS_PER_BLOCK`]
//!    corrections.** [`CorrectionQueue::drain_up_to`] is the only way
//!    corrections leave the queue, and its one real caller (the follow
//!    loop, in `crate::mainnet`) always passes exactly that constant — a
//!    correction set larger than the cap drains across as many
//!    subsequent blocks as it takes, never all at once
//!    (`risepir_server::RisePirServer::apply_block`'s own docs: it is not
//!    transactional, so an oversized injected batch is exactly the
//!    failure mode this caps).
//! 3. **A queued correction that has gone stale is dropped, never applied.**
//!    A large correction set can sit queued across many blocks before it
//!    finishes draining (invariant 2 guarantees that, for a set bigger than
//!    the per-block cap). If the feed legitimately changes the *same*
//!    account in the meantime — an ordinary transaction, nothing to do with
//!    this feature — that on-chain change is strictly fresher than a
//!    correction decided against an earlier snapshot of the store, and must
//!    win. [`Correction::baseline`] records what the store held at the
//!    moment the correction was decided; [`filter_stale_corrections`]
//!    re-reads the store immediately before a correction is actually
//!    injected and drops (does not apply) any whose live value no longer
//!    matches that baseline — the follow loop calls this on every drain,
//!    between [`CorrectionQueue::drain_up_to`] and
//!    [`prepend_corrections`]. Without this, a correction queued behind a
//!    large backlog could silently overwrite a newer, correct, feed-derived
//!    balance with stale data — exactly the wrong-answer class this whole
//!    mechanism exists to prevent, not cause.
//!
//! # Concurrency, and why this cannot stall the follow loop
//!
//! Checking a large address list is a background task ([`run`]), spawned
//! once at startup via `tokio::spawn` and never awaited by the follow
//! loop or any request handler. It never takes the PIR server's *write*
//! lock — only brief *read* locks via
//! [`risepir_http::NodeState::balance_of`] — and every finding is pushed
//! to a shared [`CorrectionQueue`] the follow loop drains on its own
//! schedule, [`MAX_CORRECTIONS_PER_BLOCK`] at a time, from an ordinary
//! `std::sync::Mutex` critical section with no `.await` in it. Because
//! `tokio::sync::RwLock` is fair/write-preferring (ADR-0025), a flood of
//! reads from this task can never starve the follow loop's writer once it
//! is queued — it can at most make one `balance_of` call wait briefly
//! behind whichever reader already holds the lock, the same as any other
//! caller.
//!
//! The slow part is the network, not the lock: provider fetches run with
//! bounded concurrency (`CONCURRENT_ADDRESS_CHECKS`) so a large address
//! file finishes in minutes rather than hours — see that constant's docs
//! for the measured shape of the estimate. Corrections are pushed to the
//! queue **incrementally**, address by address, as [`run`] decides them —
//! never batched until the whole file finishes — so the follow loop can
//! start draining real corrections into blocks long before a large file
//! is fully checked.
//!
//! # Rate limits are the binding constraint, not correctness (ADR-0041)
//!
//! Every fetch is retried with exponential, per-address-jittered backoff
//! (`MAX_FETCH_ATTEMPTS`, `backoff_delay`) before the address is given
//! up on. This is not defensive padding: the first real pass on the live
//! deployment skipped **88% of its window** purely to HTTP 429s from the
//! two default keyless providers, with not one disagreement among them —
//! see `MAX_FETCH_ATTEMPTS` for the measured numbers. The three
//! invariants above are untouched by it. A retry can only turn "no usable
//! answer" into "a usable answer"; it can never turn a disagreement into
//! an agreement, because [`quorum`] is re-evaluated over the final
//! per-provider results and still requires unanimity. Skipping remains
//! free — the address is simply re-verified on the next run.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use risepir_feed::rpc::RpcClient;
use risepir_http::NodeState;
use risepir_proto::{keccak256, AddressHash, Balance};

/// Never inject more than this many hard-refresh corrections into one
/// block's `changes` — `apply_block` is explicitly not transactional (see
/// its own docs): a failure partway through an oversized injected batch
/// would leave cells the hints do not reflect, for no better reason than
/// "the correction list happened to be big". A correction set larger than
/// this drains across as many following blocks as it takes; nothing is
/// lost, nothing is duplicated (see this crate's
/// `chunk_drains_a_large_queue_across_blocks_without_loss_or_duplication`
/// test).
pub const MAX_CORRECTIONS_PER_BLOCK: usize = 2_000;

/// How many addresses [`run`] checks against the configured providers
/// concurrently.
///
/// Sequential would make a large address list take hours: a real
/// `--hard-refresh` file this mechanism has been asked to carry holds
/// 11,306 addresses, each needing one round trip per configured provider.
/// At a plausible ~0.3-0.8s per round trip against a free keyless
/// endpoint, two providers checked one address at a time puts a fully
/// serial pass at roughly **1.9-4.7 hours** — long enough that "does this
/// block the server" stops being a rounding-error question. It does not
/// (see the module docs: this all runs in a background task, off the
/// write-lock path), but that duration is still a bad experience for
/// however long an operator waits to *see* corrections land. Bounded
/// concurrency amortizes the same per-address latency across this many
/// in-flight checks at once, bringing the same list to roughly
/// **15-35 minutes** instead — a modest, non-abusive load on a free
/// endpoint, not a hands-off firehose. This bounds *fan-out* only, never
/// correctness: every single check is still exactly the same
/// [`quorum`]/[`decide_correction`] gate, independent of how many run
/// side by side.
///
/// Left at 8 even after ADR-0041 made every fetch retryable, rather than
/// lowered to dodge the 429s that motivated it. Lowering fan-out trades
/// throughput for a *guess* at whatever rate the endpoint happens to
/// tolerate today; [`backoff_delay`] instead lets each address find that
/// rate on its own and, because the jitter is per-address, keeps a burst
/// that was refused together from retrying together. The retries do
/// stretch the estimate above — worst case ~3.5 s of waiting per
/// fully-failing address, amortized across this many in flight.
const CONCURRENT_ADDRESS_CHECKS: usize = 8;

/// Log a progress line every this many addresses checked, so a long run
/// is visible in the log rather than silent for tens of minutes.
const PROGRESS_LOG_INTERVAL: u64 = 1_000;

/// How many times one provider fetch is attempted before the address is
/// given up on for this run (1 initial try + `MAX_FETCH_ATTEMPTS - 1`
/// retries).
///
/// # Why this exists at all (ADR-0041)
///
/// The first real `--hard-refresh` pass on the live deployment checked
/// 57,646 addresses and **skipped 50,813 of them — 88%** (server log,
/// block 25,630,606, 2026-07-28). Every one of those skips was
/// [`Quorum::Errored`], not a disagreement: the run logged zero
/// `providers disagree` warnings, and **all 67,791 fetch failures were
/// HTTP 429 rate limits** from the two default keyless providers
/// (50,596 from `eth-mainnet.public.blastapi.io`, 17,195 from
/// `gateway.tenderly.co`). A single unretried attempt against a free
/// endpoint, fired [`CONCURRENT_ADDRESS_CHECKS`]-wide with no pacing, is
/// simply not a fetch that usually lands.
///
/// Nothing about that was *unsafe* — the "never guess" invariant held
/// perfectly, which is exactly why it was invisible from the outside: a
/// rate-limited pass and a clean pass both report "no correction", and
/// only the skip count tells them apart. But a re-verification that can
/// only see 12% of its own window is not doing the job ADR-0040 built it
/// for. Retrying a failed fetch changes **only** how many addresses get a
/// usable answer; it cannot change what a usable answer means, because
/// every retry still feeds the same unanimous-[`quorum`] gate.
const MAX_FETCH_ATTEMPTS: u32 = 4;

/// Base delay for the exponential backoff between fetch attempts —
/// attempt *n* waits roughly `RETRY_BASE_DELAY * 2^(n-1)`, so a
/// four-attempt address spends at most ~0.5 + 1 + 2 = 3.5 s waiting
/// before it is given up on. Deliberately unhurried: the whole point is
/// to stop hammering an endpoint that has just said "too many requests".
const RETRY_BASE_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// Backoff before the next attempt, after `attempt` (1-based) has just
/// failed: `RETRY_BASE_DELAY * 2^(attempt-1)`, plus up to one whole base
/// delay of **per-address** jitter.
///
/// The jitter is the part that matters, and it is derived from the
/// address rather than from a clock or an RNG. [`CONCURRENT_ADDRESS_CHECKS`]
/// checks run side by side and tend to be rate-limited *together* — a
/// burst arrives, the endpoint refuses all of it at once. Backing off by
/// an identical amount would re-synchronize that whole burst onto the
/// same instant and reproduce the 429 that caused it, indefinitely.
/// Spreading each address across a different point in the window is what
/// actually breaks the lockstep. Deriving it from the address keeps this
/// function pure and its behaviour reproducible in a test, which an
/// `OsRng` or a `SystemTime` read would not.
fn backoff_delay(attempt: u32, addr: &[u8; 20]) -> std::time::Duration {
    // Saturating, so an (unreachable) large `attempt` cannot overflow into
    // a tiny or absurd delay.
    let factor = 1u32.checked_shl(attempt.saturating_sub(1)).unwrap_or(u32::MAX);
    let base = RETRY_BASE_DELAY.saturating_mul(factor);
    // SplitMix64's finalizer over the address plus the attempt number, so
    // the same address jitters differently on each of its own retries.
    let mut z = u64::from_le_bytes([addr[0], addr[1], addr[2], addr[3], addr[4], addr[5], addr[6], addr[7]])
        .wrapping_add(u64::from(attempt).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    let jitter_millis = z % (RETRY_BASE_DELAY.as_millis() as u64).max(1);
    base.saturating_add(std::time::Duration::from_millis(jitter_millis))
}

// ─── Address file parsing ────────────────────────────────────────────────

/// Errors from [`parse_address_file`] / [`parse_addresses`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressFileError {
    /// The file could not be opened or read.
    Io(String),
    /// One line failed validation. 1-based, matching the convention
    /// `risepir_feed::snapshot`'s CSV loader already uses for its own
    /// `file:line` errors.
    Malformed {
        /// 1-based line number of the first offending line.
        line: u64,
        /// What was wrong with it.
        reason: String,
    },
}

impl std::fmt::Display for AddressFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Malformed { line, reason } => write!(f, "line {line}: {reason}"),
        }
    }
}

impl std::error::Error for AddressFileError {}

/// Reads `path` and parses it as a `--hard-refresh` address file — see
/// [`parse_addresses`] for the accepted format. Kept separate from that
/// function so the actual parsing logic is unit-tested with no filesystem
/// access at all.
///
/// # Errors
///
/// [`AddressFileError::Io`] if the file cannot be read;
/// [`AddressFileError::Malformed`] on the first line that fails
/// validation — this loader never skips a bad line and continues.
pub fn parse_address_file(path: &Path) -> Result<Vec<[u8; 20]>, AddressFileError> {
    let text = std::fs::read_to_string(path).map_err(|e| AddressFileError::Io(format!("{}: {e}", path.display())))?;
    parse_addresses(&text)
}

/// Parses address-file text: one lowercase `0x`-prefixed, 40-hex-digit
/// address per line. Blank lines (after trimming whitespace) and lines
/// whose first non-whitespace character is `#` are skipped; every other
/// line is validated in full, and the first malformed line hard-fails
/// with its 1-based line number — this never guesses, the same posture
/// `risepir_feed::snapshot`'s CSV loader takes for the balance snapshot
/// itself. Uppercase (or mixed-case/checksummed) hex is rejected rather
/// than silently lowercased: it is a different, unexpected input shape
/// here, not a formatting variant worth papering over.
pub fn parse_addresses(text: &str) -> Result<Vec<[u8; 20]>, AddressFileError> {
    let mut out = Vec::new();
    for (i, raw_line) in text.lines().enumerate() {
        let line_no = i as u64 + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let addr = parse_lowercase_address(line).ok_or_else(|| AddressFileError::Malformed {
            line: line_no,
            reason: format!("expected a lowercase 0x-prefixed 40-hex-digit address, got {line:?}"),
        })?;
        out.push(addr);
    }
    Ok(out)
}

/// `0x` (lowercase only) + exactly 40 lowercase-hex digits.
fn parse_lowercase_address(s: &str) -> Option<[u8; 20]> {
    let hex = s.strip_prefix("0x")?;
    if hex.len() != 40 || !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

// ─── Quorum ───────────────────────────────────────────────────────────────

/// Outcome of comparing every configured provider's balance read for one
/// address at one height. The *only* gate a hard-refresh (or the snapshot
/// audit, `crate::snapshot_audit`, which reuses this) correction can come
/// through — see the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quorum {
    /// Every configured provider answered, and every answer agreed.
    Agreed(Balance),
    /// Every provider answered, but at least one disagreed with another.
    Disagreed,
    /// At least one provider failed to answer at all (or none were
    /// configured to ask in the first place).
    Errored,
}

/// Reduces one address's per-provider reads (`Ok(balance)` or `Err(_)`,
/// one per configured `--refresh-url`) to a [`Quorum`]. Requires **every**
/// configured provider to agree, not just a majority: a single silent
/// liar (or a single unreachable provider) among N is exactly the failure
/// this mechanism exists to catch, and "skip" costs nothing — the address
/// is simply re-verified next run — while "guess" can serve a wrong
/// balance. An empty `results` slice is `Errored`, never `Agreed`: there
/// is nothing to agree on (real callers always supply at least the two
/// required providers; `validate_refresh_urls` is what enforces that
/// before any of this runs).
pub fn quorum<E>(results: &[Result<Balance, E>]) -> Quorum {
    if results.is_empty() || results.iter().any(Result::is_err) {
        return Quorum::Errored;
    }
    // `.ok()`/`filter_map` rather than `.expect`/`.unwrap`: those require
    // `E: Debug` for their panic message, which would needlessly infect
    // every caller's error type with a trait bound this function does not
    // otherwise need — the `is_err` check above already guarantees every
    // element here is `Ok`.
    let mut values = results.iter().filter_map(|r| r.as_ref().ok());
    let Some(&first) = values.next() else {
        // Unreachable given the checks above (a non-empty, all-`Ok` slice
        // always yields a first value) — handled rather than panicking.
        return Quorum::Errored;
    };
    if values.all(|&v| v == first) {
        Quorum::Agreed(first)
    } else {
        Quorum::Disagreed
    }
}

/// Validates `--refresh-url` configuration: at least 2 *distinct* URLs
/// are required whenever `--hard-refresh` or the snapshot audit
/// (`--snapshot-audit-samples > 0`) will actually run — agreement between
/// fewer than 2 independent providers proves nothing. Pure, so the exit-2
/// CLI behaviour this backs (`crate::mainnet::spawn`) is testable without
/// a real process exit.
///
/// # Errors
///
/// A message naming how many distinct URLs were found, if fewer than 2.
pub fn validate_refresh_urls(urls: &[String]) -> Result<(), String> {
    let mut distinct: Vec<&String> = urls.iter().collect();
    distinct.sort();
    distinct.dedup();
    if distinct.len() < 2 {
        return Err(format!(
            "need at least 2 distinct --refresh-url values (agreement between fewer than 2 independent \
             providers proves nothing); got {} distinct out of {:?}",
            distinct.len(),
            urls
        ));
    }
    Ok(())
}

// ─── Deciding a correction ──────────────────────────────────────────────

/// What one address's check amounts to, decided purely from its [`Quorum`]
/// outcome and the store's current verified balance — no I/O, and the
/// *only* place a hard-refresh correction is decided. [`Self::Correct`] is
/// reachable only from [`Quorum::Agreed`] with a value that differs from
/// `current`; every other combination — an error, a disagreement, or an
/// agreement that just confirms the store is already right — produces
/// something other than a write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    /// At least one provider fetch failed.
    Skipped,
    /// Every provider answered but did not all agree with each other.
    SkippedDisagreement,
    /// Every provider agreed, and the agreed value is exactly what the
    /// store already holds — nothing to write.
    AlreadyCorrect,
    /// Every provider agreed on a value that differs from the store's
    /// current balance — the one outcome that produces a [`Correction`].
    Correct {
        /// The value every provider agreed on — what gets written.
        new_balance: Balance,
        /// What the store held immediately before the correction.
        old_balance: Balance,
    },
}

/// The one function that turns a [`Quorum`] outcome into a decision. See
/// [`CheckOutcome`]'s docs for the invariant this holds.
pub fn decide_correction(q: Quorum, current: Balance) -> CheckOutcome {
    match q {
        Quorum::Errored => CheckOutcome::Skipped,
        Quorum::Disagreed => CheckOutcome::SkippedDisagreement,
        Quorum::Agreed(value) if value == current => CheckOutcome::AlreadyCorrect,
        Quorum::Agreed(value) => CheckOutcome::Correct {
            new_balance: value,
            old_balance: current,
        },
    }
}

// ─── Corrections: the shared queue and how they ride a block ────────────

/// One pending write: a store key and the balance every configured
/// provider agreed it should hold. Produced only by
/// [`decide_correction`]'s [`CheckOutcome::Correct`] arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Correction {
    /// The store key (`keccak256(address)`).
    pub key: AddressHash,
    /// The balance to write.
    pub balance: Balance,
    /// The store's own verified balance at the moment this correction was
    /// decided (`decide_correction`'s `current` argument, i.e.
    /// [`CheckOutcome::Correct`]'s `old_balance`). [`filter_stale_corrections`]
    /// re-checks this immediately before the correction is actually
    /// applied and drops it if the live value has since moved on — see the
    /// module docs' third invariant for why this matters: a correction can
    /// sit queued across several blocks, and a real transaction touching
    /// the same account in the meantime must win over stale corrected data.
    pub baseline: Balance,
}

/// FIFO queue of corrections waiting to ride a block's `changes`, shared
/// between [`run`] (the producer) and the follow loop (the consumer,
/// [`Self::drain_up_to`]).
///
/// A plain [`std::sync::Mutex`], not an async one: every critical section
/// here is a handful of `VecDeque` operations with no `.await` inside it —
/// the same reasoning `risepir_http::NodeState`'s own `reconcile` field
/// gives for using `std`'s mutex over tokio's in exactly this shape.
#[derive(Default)]
pub struct CorrectionQueue {
    inner: Mutex<VecDeque<Correction>>,
}

impl CorrectionQueue {
    /// An empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one correction to the back of the queue.
    pub fn push_one(&self, correction: Correction) {
        self.inner
            .lock()
            .expect("CorrectionQueue mutex poisoned")
            .push_back(correction);
    }

    /// Appends every correction in `corrections`, in order, to the back of
    /// the queue.
    pub fn push_many(&self, corrections: Vec<Correction>) {
        self.inner
            .lock()
            .expect("CorrectionQueue mutex poisoned")
            .extend(corrections);
    }

    /// Removes and returns up to `max` corrections from the front of the
    /// queue, FIFO order preserved. Returns fewer than `max` (down to
    /// none) when the queue holds fewer — this is the mechanism by which a
    /// correction set larger than [`MAX_CORRECTIONS_PER_BLOCK`] drains
    /// across many blocks instead of landing in one.
    pub fn drain_up_to(&self, max: usize) -> Vec<Correction> {
        let mut inner = self.inner.lock().expect("CorrectionQueue mutex poisoned");
        let n = max.min(inner.len());
        inner.drain(..n).collect()
    }

    /// How many corrections are currently queued — used to log progress as
    /// a large set drains across blocks.
    pub fn len(&self) -> usize {
        self.inner.lock().expect("CorrectionQueue mutex poisoned").len()
    }

    /// Whether the queue currently holds no corrections.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Prepends `corrections` (as `(key, balance)` pairs) to `changes`, FIFO
/// order preserved — placed **first** so the feed's own changes for this
/// same block (appended after) always win for a shared key, per
/// `risepir_proto::BlockUpdate::changes`'s own documented
/// last-entry-for-a-key-wins contract. Pure rearrangement, no I/O — the
/// follow loop (`crate::mainnet`) is the one real caller, wired to a live
/// [`CorrectionQueue`]; keeping this as a standalone function is what
/// makes "the feed always wins" a fact pinned by a plain-`Vec` test
/// instead of something only checkable by running the whole follow loop.
pub fn prepend_corrections(corrections: Vec<Correction>, mut changes: Vec<(AddressHash, Balance)>) -> Vec<(AddressHash, Balance)> {
    let mut out: Vec<(AddressHash, Balance)> = Vec::with_capacity(corrections.len() + changes.len());
    out.extend(corrections.into_iter().map(|c| (c.key, c.balance)));
    out.append(&mut changes);
    out
}

/// Re-checks every drained correction's [`Correction::baseline`] against
/// the store's **current** verified balance and drops (never applies) any
/// that no longer match — the module docs' third invariant. Returns the
/// still-valid corrections (in their original order, ready for
/// [`prepend_corrections`]) and how many were dropped as stale.
///
/// # Why this exists
///
/// A correction is decided once, against the store's balance *at check
/// time*. If the correction set is larger than [`MAX_CORRECTIONS_PER_BLOCK`]
/// it can sit queued across many subsequent blocks before it is this
/// correction's turn to drain. If the feed legitimately changes that same
/// account in the interim — an ordinary transaction, unrelated to this
/// feature — that on-chain change is strictly fresher than the queued
/// correction and must win; applying the correction anyway would silently
/// overwrite correct, current data with stale data, which is exactly the
/// wrong-answer class this whole mechanism exists to prevent. Comparing
/// against `baseline` (what the store held when the correction was
/// decided) rather than against the correction's own `balance` (the new,
/// corrected value) is the part that actually closes the race: comparing
/// against `balance` would still let a *third*, unrelated value that
/// happens to differ from both get overwritten.
///
/// # Errors
///
/// Whatever [`risepir_http::NodeState::balance_of`] returns — the caller
/// (the follow loop) treats this exactly like any other verified-read
/// failure elsewhere in that loop (`CRITICAL`, stop following): it
/// indicates store corruption unrelated to this feature, not anything
/// safe to paper over.
pub async fn filter_stale_corrections(
    node: &NodeState,
    drained: Vec<Correction>,
) -> Result<(Vec<Correction>, usize), risepir_server::ServerError> {
    let mut still_valid = Vec::with_capacity(drained.len());
    let mut stale = 0usize;
    for c in drained {
        let current = node.balance_of(&c.key).await?.unwrap_or(0);
        if current == c.baseline {
            still_valid.push(c);
        } else {
            stale += 1;
        }
    }
    Ok((still_valid, stale))
}

// ─── The background task ─────────────────────────────────────────────────

/// Runs the whole `--hard-refresh` check as a background task: parses
/// `path` (hard-fails the *process* on a malformed file — a bad address
/// file is a deployment-configuration mistake caught at startup, not a
/// runtime condition anything should try to recover from), reads the
/// applied head at the moment it starts (the one explicit height `H`
/// every read in this run uses), checks every address against
/// `refresh_urls` with `CONCURRENT_ADDRESS_CHECKS`-way concurrency, and
/// pushes each resulting correction to `queue` **incrementally** — never
/// batched until the whole file finishes — so the follow loop can start
/// draining real corrections well before a large file is fully checked.
///
/// Never touches the PIR server's write lock; never awaited inline by
/// anything that needs to keep serving (call this via `tokio::spawn`) —
/// see the module docs for why this and the follow loop cannot stall each
/// other. Idempotent by construction: every write goes through
/// [`decide_correction`], which only ever fires on a real disagreement
/// with the *current* store value, so re-running with the same file after
/// the corrections already landed finds nothing left to do.
///
/// Assumes `refresh_urls` has already passed [`validate_refresh_urls`] —
/// the one caller (`crate::mainnet::spawn`) checks that before spawning
/// this at all, so it is not re-checked here.
pub async fn run(node: Arc<NodeState>, queue: Arc<CorrectionQueue>, path: PathBuf, refresh_urls: Vec<String>) {
    let addresses = match parse_address_file(&path) {
        Ok(a) => a,
        Err(e) => {
            logln!("risepir-rpc mainnet: fatal: --hard-refresh {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    let total = addresses.len();
    if total == 0 {
        logln!(
            "risepir-rpc mainnet: hard-refresh: {} contains no addresses; nothing to check",
            path.display()
        );
        return;
    }

    let height = node.with_server(|s| s.block()).await;
    logln!(
        "risepir-rpc mainnet: hard-refresh: checking {total} address(es) from {} at block {height} against \
         {} provider(s) ({CONCURRENT_ADDRESS_CHECKS} at a time): {}",
        path.display(),
        refresh_urls.len(),
        refresh_urls.join(", "),
    );

    let clients: Vec<Arc<RpcClient>> = refresh_urls.into_iter().map(|u| Arc::new(RpcClient::new(u))).collect();

    let mut checked = 0u64;
    let mut agreed = 0u64;
    let mut skipped = 0u64;
    let mut already_correct = 0u64;
    let mut corrected = 0u64;
    let mut total_wei_corrected: u128 = 0;

    let mut join_set: tokio::task::JoinSet<([u8; 20], Quorum)> = tokio::task::JoinSet::new();
    let mut remaining = addresses.into_iter();
    for addr in remaining.by_ref().take(CONCURRENT_ADDRESS_CHECKS) {
        spawn_check(&mut join_set, &clients, addr, height);
    }

    while let Some(res) = join_set.join_next().await {
        let (addr, q) = res.expect("hard-refresh address-check task panicked");
        checked += 1;
        // Keep exactly CONCURRENT_ADDRESS_CHECKS in flight: refill one slot
        // per completion until the address list is exhausted.
        if let Some(next_addr) = remaining.next() {
            spawn_check(&mut join_set, &clients, next_addr, height);
        }

        if matches!(q, Quorum::Agreed(_)) {
            agreed += 1;
        }

        let key = keccak256(&addr);
        let current = match node.balance_of(&key).await {
            Ok(v) => v.unwrap_or(0),
            Err(e) => {
                logln!(
                    "risepir-rpc mainnet: hard-refresh: CRITICAL: verified read for 0x{} failed ({e}) — stopping \
                     this hard-refresh run; this indicates store corruption independent of this feature (the \
                     same class `reconcile`'s own verified read guards against), not anything this mechanism can \
                     safely paper over",
                    hex20(&addr)
                );
                break;
            }
        };

        match decide_correction(q, current) {
            CheckOutcome::Skipped => skipped += 1,
            CheckOutcome::SkippedDisagreement => {
                skipped += 1;
                logln!(
                    "risepir-rpc mainnet: hard-refresh: WARNING: providers disagree for 0x{} at block {height}; \
                     skipping (never guessing)",
                    hex20(&addr)
                );
            }
            CheckOutcome::AlreadyCorrect => already_correct += 1,
            CheckOutcome::Correct { new_balance, old_balance } => {
                corrected += 1;
                total_wei_corrected = total_wei_corrected.saturating_add(old_balance.abs_diff(new_balance));
                // Pushed immediately, not accumulated — see the module docs.
                // baseline: old_balance — what `filter_stale_corrections`
                // re-checks against right before this is actually applied.
                queue.push_one(Correction { key, balance: new_balance, baseline: old_balance });
            }
        }

        if checked.is_multiple_of(PROGRESS_LOG_INTERVAL) {
            logln!(
                "risepir-rpc mainnet: hard-refresh: {checked}/{total} checked so far ({corrected} correction(s) \
                 queued, {} still pending in the block queue)",
                queue.len()
            );
        }
    }

    logln!(
        "risepir-rpc mainnet: hard-refresh: done at block {height} — {checked} checked, {agreed} agreed, \
         {skipped} skipped (disagreement or fetch error), {already_correct} already correct, {corrected} \
         corrected, {total_wei_corrected} wei total absolute correction. Corrections drain into blocks \
         {MAX_CORRECTIONS_PER_BLOCK} at a time; a restart with the same file is a no-op re-verification."
    );
}

/// Spawns one address's quorum check (all configured providers, in order)
/// onto `join_set`. Owns everything it captures (`clients` is cloned as a
/// `Vec<Arc<RpcClient>>` — cheap, refcount bumps only) so the task is
/// `'static`, as `tokio::spawn` (which `JoinSet::spawn` uses internally)
/// requires.
fn spawn_check(join_set: &mut tokio::task::JoinSet<([u8; 20], Quorum)>, clients: &[Arc<RpcClient>], addr: [u8; 20], height: u64) {
    let clients = clients.to_vec();
    join_set.spawn(async move {
        let mut results = Vec::with_capacity(clients.len());
        for c in &clients {
            results.push(fetch_with_retry(c, &addr, height).await);
        }
        (addr, quorum(&results))
    });
}

/// One provider's balance read for one address, retried with the
/// [`backoff_delay`] schedule until it succeeds or [`MAX_FETCH_ATTEMPTS`]
/// is exhausted (ADR-0041).
///
/// A depth refusal ([`risepir_feed::FeedError::is_depth_refusal`],
/// ADR-0036) is **not** retried: it is the endpoint saying it does not
/// hold state at this height at all, which no amount of waiting changes —
/// retrying it would only spend requests that the rate-limited fetches
/// this function exists for actually need.
///
/// Only the final give-up is logged, not each attempt. The unretried
/// version logged every failure and produced **67,791 lines in a single
/// pass** — the dominant contributor to a 70 MB server log — which buried
/// the run's actual outcome. One line per genuinely-abandoned address,
/// naming the attempt count, says strictly more.
///
/// `Err(())` rather than the real error: [`quorum`] only distinguishes
/// "answered" from "did not", and discarding the error type here is what
/// keeps that function free of a `Debug` bound on every caller's error
/// (see its own docs).
async fn fetch_with_retry(client: &RpcClient, addr: &[u8; 20], height: u64) -> Result<Balance, ()> {
    for attempt in 1..=MAX_FETCH_ATTEMPTS {
        let err = match client.balance_at(addr, height).await {
            Ok(b) => return Ok(b),
            Err(e) => e,
        };
        let unretryable = err.is_depth_refusal();
        if unretryable || attempt == MAX_FETCH_ATTEMPTS {
            logln!(
                "risepir-rpc mainnet: hard-refresh: fetch for 0x{} at {height} from {} failed after {attempt} \
                 attempt(s){}: {err}",
                hex20(addr),
                client.url(),
                if unretryable { " (depth refusal — not retryable)" } else { "" },
            );
            return Err(());
        }
        tokio::time::sleep(backoff_delay(attempt, addr)).await;
    }
    // Unreachable: the loop above always returns on its last iteration.
    Err(())
}

/// Lowercase hex of a 20-byte address, for log lines. `pub(crate)` — also
/// used by `crate::snapshot_audit`, which shares this module's quorum
/// machinery.
pub(crate) fn hex20(bytes: &[u8; 20]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_addresses: every malformed case, plus the happy path ─────

    #[test]
    fn parses_a_well_formed_file_in_order() {
        let text = "0x1111111111111111111111111111111111111111\n\
                     0x2222222222222222222222222222222222222222\n";
        let out = parse_addresses(text).unwrap();
        assert_eq!(out, vec![[0x11u8; 20], [0x22u8; 20]]);
    }

    #[test]
    fn blank_lines_and_comments_are_skipped() {
        let text = "\n\
                     # a comment\n\
                     0x1111111111111111111111111111111111111111\n\
                     \n\
                        # indented comment\n\
                     0x2222222222222222222222222222222222222222\n\
                     \n";
        let out = parse_addresses(text).unwrap();
        assert_eq!(out, vec![[0x11u8; 20], [0x22u8; 20]]);
    }

    #[test]
    fn empty_text_yields_no_addresses() {
        assert_eq!(parse_addresses("").unwrap(), Vec::<[u8; 20]>::new());
        assert_eq!(parse_addresses("\n\n# only comments\n").unwrap(), Vec::<[u8; 20]>::new());
    }

    #[test]
    fn uppercase_hex_is_rejected() {
        let err = parse_addresses("0x1111111111111111111111111111111111111111\n0xABCDEF1111111111111111111111111111111111\n")
            .unwrap_err();
        assert_eq!(
            err,
            AddressFileError::Malformed {
                line: 2,
                reason: "expected a lowercase 0x-prefixed 40-hex-digit address, got \"0xABCDEF1111111111111111111111111111111111\"".to_string(),
            }
        );
    }

    #[test]
    fn missing_0x_prefix_is_rejected() {
        let err = parse_addresses("1111111111111111111111111111111111111111\n").unwrap_err();
        assert!(matches!(err, AddressFileError::Malformed { line: 1, .. }), "{err:?}");
    }

    #[test]
    fn wrong_length_is_rejected_both_directions() {
        for bad in ["0x1234", "0x11111111111111111111111111111111111111111111"] {
            let err = parse_addresses(&format!("{bad}\n")).unwrap_err();
            assert!(matches!(err, AddressFileError::Malformed { line: 1, .. }), "{bad}: {err:?}");
        }
    }

    #[test]
    fn non_hex_character_is_rejected() {
        let err = parse_addresses("0x111111111111111111111111111111111111111g\n").unwrap_err();
        assert!(matches!(err, AddressFileError::Malformed { line: 1, .. }), "{err:?}");
    }

    #[test]
    fn first_bad_line_is_reported_not_the_last() {
        let text = "0x1111111111111111111111111111111111111111\n\
                     not-an-address\n\
                     0x2222222222222222222222222222222222222222\n\
                     also-not-one\n";
        let err = parse_addresses(text).unwrap_err();
        assert_eq!(err, AddressFileError::Malformed { line: 2, reason: "expected a lowercase 0x-prefixed 40-hex-digit address, got \"not-an-address\"".to_string() });
    }

    #[test]
    fn parse_address_file_reports_io_error_for_a_missing_file() {
        let path = std::path::Path::new("/nonexistent/definitely-not-here/addresses.txt");
        let err = parse_address_file(path).unwrap_err();
        assert!(matches!(err, AddressFileError::Io(_)), "{err:?}");
    }

    // ── backoff_delay (ADR-0041) ────────────────────────────────────────

    #[test]
    fn backoff_grows_exponentially_with_the_attempt_number() {
        let addr = [0x11u8; 20];
        let d1 = backoff_delay(1, &addr);
        let d2 = backoff_delay(2, &addr);
        let d3 = backoff_delay(3, &addr);
        // Each attempt's floor is the previous one's, doubled. Jitter is
        // bounded by one base delay, so compare against the floors rather
        // than against each other.
        assert!(d1 >= RETRY_BASE_DELAY && d1 < RETRY_BASE_DELAY * 2, "{d1:?}");
        assert!(d2 >= RETRY_BASE_DELAY * 2 && d2 < RETRY_BASE_DELAY * 3, "{d2:?}");
        assert!(d3 >= RETRY_BASE_DELAY * 4 && d3 < RETRY_BASE_DELAY * 5, "{d3:?}");
    }

    #[test]
    fn jitter_desynchronizes_addresses_that_were_rate_limited_together() {
        // The property that matters: a burst of concurrent checks refused
        // at the same instant must not all wake at the same instant. Over
        // a realistic fan-out, the delays must actually differ.
        let delays: std::collections::BTreeSet<_> = (0u8..64)
            .map(|i| backoff_delay(1, &[i; 20]))
            .collect();
        assert!(
            delays.len() > 32,
            "64 distinct addresses collapsed to {} distinct delays — the burst would re-synchronize",
            delays.len()
        );
    }

    #[test]
    fn the_same_address_jitters_differently_on_each_of_its_own_retries() {
        let addr = [0x42u8; 20];
        // Strip the exponential floor to compare the jitter component alone.
        let j1 = backoff_delay(1, &addr) - RETRY_BASE_DELAY;
        let j2 = backoff_delay(2, &addr) - RETRY_BASE_DELAY * 2;
        assert_ne!(j1, j2, "attempt number must feed the jitter, not just the floor");
    }

    #[test]
    fn backoff_is_deterministic_for_a_given_address_and_attempt() {
        let addr = [0x7fu8; 20];
        assert_eq!(backoff_delay(2, &addr), backoff_delay(2, &addr));
    }

    #[test]
    fn a_whole_address_gives_up_in_a_bounded_time() {
        // Every retry an address can make, summed: this is the worst case
        // one address adds to a pass, and it must stay small enough that
        // MAX_FETCH_ATTEMPTS cannot quietly turn a 35-minute run into an
        // overnight one.
        let addr = [0xffu8; 20];
        let total: std::time::Duration = (1..MAX_FETCH_ATTEMPTS).map(|a| backoff_delay(a, &addr)).sum();
        assert!(
            total < std::time::Duration::from_secs(10),
            "worst-case wait per address is {total:?}"
        );
    }

    // ── quorum: agree / disagree / error ────────────────────────────────

    #[test]
    fn two_providers_agree() {
        let results: Vec<Result<Balance, ()>> = vec![Ok(100), Ok(100)];
        assert_eq!(quorum(&results), Quorum::Agreed(100));
    }

    #[test]
    fn two_providers_disagree() {
        let results: Vec<Result<Balance, ()>> = vec![Ok(100), Ok(101)];
        assert_eq!(quorum(&results), Quorum::Disagreed);
    }

    #[test]
    fn one_provider_erroring_is_errored_even_if_the_other_would_have_agreed() {
        let results: Vec<Result<Balance, ()>> = vec![Ok(100), Err(())];
        assert_eq!(quorum(&results), Quorum::Errored);
        let results: Vec<Result<Balance, ()>> = vec![Err(()), Ok(100)];
        assert_eq!(quorum(&results), Quorum::Errored);
    }

    #[test]
    fn both_providers_erroring_is_errored() {
        let results: Vec<Result<Balance, ()>> = vec![Err(()), Err(())];
        assert_eq!(quorum(&results), Quorum::Errored);
    }

    #[test]
    fn empty_results_is_errored_never_agreed() {
        let results: Vec<Result<Balance, ()>> = vec![];
        assert_eq!(quorum(&results), Quorum::Errored);
    }

    #[test]
    fn generalizes_to_more_than_two_providers() {
        let all_agree: Vec<Result<Balance, ()>> = vec![Ok(7), Ok(7), Ok(7)];
        assert_eq!(quorum(&all_agree), Quorum::Agreed(7));
        let one_off: Vec<Result<Balance, ()>> = vec![Ok(7), Ok(7), Ok(8)];
        assert_eq!(quorum(&one_off), Quorum::Disagreed);
        let one_errs: Vec<Result<Balance, ()>> = vec![Ok(7), Ok(7), Err(())];
        assert_eq!(quorum(&one_errs), Quorum::Errored);
    }

    // ── validate_refresh_urls ───────────────────────────────────────────

    #[test]
    fn two_distinct_urls_pass() {
        assert!(validate_refresh_urls(&["https://a.test".to_string(), "https://b.test".to_string()]).is_ok());
    }

    #[test]
    fn one_url_fails() {
        assert!(validate_refresh_urls(&["https://a.test".to_string()]).is_err());
    }

    #[test]
    fn zero_urls_fail() {
        assert!(validate_refresh_urls(&[]).is_err());
    }

    #[test]
    fn duplicate_urls_do_not_count_twice() {
        let urls = vec!["https://a.test".to_string(), "https://a.test".to_string()];
        assert!(validate_refresh_urls(&urls).is_err(), "two copies of one URL is only 1 distinct provider");
        let urls = vec!["https://a.test".to_string(), "https://a.test".to_string(), "https://b.test".to_string()];
        assert!(validate_refresh_urls(&urls).is_ok(), "a duplicate plus a genuinely different URL is 2 distinct");
    }

    // ── decide_correction: the one gate on ever producing a Correction ──

    #[test]
    fn errored_is_always_skipped_regardless_of_current_value() {
        assert_eq!(decide_correction(Quorum::Errored, 0), CheckOutcome::Skipped);
        assert_eq!(decide_correction(Quorum::Errored, 999), CheckOutcome::Skipped);
    }

    #[test]
    fn disagreed_is_always_skipped_regardless_of_current_value() {
        assert_eq!(decide_correction(Quorum::Disagreed, 0), CheckOutcome::SkippedDisagreement);
        assert_eq!(decide_correction(Quorum::Disagreed, 999), CheckOutcome::SkippedDisagreement);
    }

    #[test]
    fn agreed_matching_current_is_already_correct_never_a_write() {
        assert_eq!(decide_correction(Quorum::Agreed(500), 500), CheckOutcome::AlreadyCorrect);
    }

    #[test]
    fn agreed_differing_from_current_is_the_only_correct_producing_case() {
        assert_eq!(
            decide_correction(Quorum::Agreed(500), 400),
            CheckOutcome::Correct { new_balance: 500, old_balance: 400 }
        );
    }

    #[test]
    fn correction_is_unreachable_without_agreed() {
        // Exhaustive over every non-Agreed Quorum value at every plausible
        // current balance: never Correct.
        for current in [0u128, 1, 500, u128::MAX] {
            assert!(!matches!(decide_correction(Quorum::Errored, current), CheckOutcome::Correct { .. }));
            assert!(!matches!(decide_correction(Quorum::Disagreed, current), CheckOutcome::Correct { .. }));
        }
    }

    // ── CorrectionQueue: chunking across blocks ─────────────────────────

    fn correction(i: u32) -> Correction {
        let mut key = [0u8; 32];
        key[..4].copy_from_slice(&i.to_le_bytes());
        Correction { key, balance: u128::from(i), baseline: 0 }
    }

    #[test]
    fn chunk_drains_a_large_queue_across_blocks_without_loss_or_duplication() {
        let queue = CorrectionQueue::new();
        let total = 5_000u32;
        queue.push_many((0..total).map(correction).collect());
        assert_eq!(queue.len(), total as usize);

        let mut drained: Vec<Correction> = Vec::new();
        loop {
            let batch = queue.drain_up_to(MAX_CORRECTIONS_PER_BLOCK);
            if batch.is_empty() {
                break;
            }
            assert!(batch.len() <= MAX_CORRECTIONS_PER_BLOCK, "one drain must never exceed the per-block cap");
            drained.extend(batch);
        }

        assert!(queue.is_empty());
        assert_eq!(drained.len(), total as usize, "none lost, none duplicated");
        assert_eq!(drained, (0..total).map(correction).collect::<Vec<_>>(), "FIFO order preserved across chunk boundaries");
        // 5000 / 2000 = 3 drains (2000, 2000, 1000) — pin the actual chunking, not just the totals.
    }

    #[test]
    fn drain_up_to_returns_fewer_than_max_when_the_queue_is_smaller() {
        let queue = CorrectionQueue::new();
        queue.push_many(vec![correction(1), correction(2)]);
        let batch = queue.drain_up_to(MAX_CORRECTIONS_PER_BLOCK);
        assert_eq!(batch, vec![correction(1), correction(2)]);
        assert!(queue.is_empty());
        assert_eq!(queue.drain_up_to(MAX_CORRECTIONS_PER_BLOCK), Vec::new());
    }

    #[test]
    fn push_one_and_push_many_are_both_fifo() {
        let queue = CorrectionQueue::new();
        queue.push_one(correction(1));
        queue.push_many(vec![correction(2), correction(3)]);
        queue.push_one(correction(4));
        assert_eq!(queue.drain_up_to(10), vec![correction(1), correction(2), correction(3), correction(4)]);
    }

    // ── prepend_corrections: ordering, and a real apply_block proof ─────

    #[test]
    fn corrections_come_before_changes_in_the_output_vec() {
        let corrections = vec![correction(1), correction(2)];
        let changes = vec![([9u8; 32], 900u128)];
        let out = prepend_corrections(corrections, changes.clone());
        assert_eq!(out[0], (correction(1).key, correction(1).balance));
        assert_eq!(out[1], (correction(2).key, correction(2).balance));
        assert_eq!(out[2], changes[0]);
    }

    #[test]
    fn empty_corrections_leaves_changes_untouched() {
        let changes = vec![([1u8; 32], 1u128), ([2u8; 32], 2u128)];
        assert_eq!(prepend_corrections(Vec::new(), changes.clone()), changes);
    }

    /// The end-to-end proof, through a real `apply_block`: when a stale
    /// hard-refresh correction and the feed's own change for the block
    /// name the *same* key with *different* balances, the stored result
    /// must be the feed's value — never the correction's — because
    /// `BlockUpdate::changes` applies in order and the last entry for a
    /// key wins, and `prepend_corrections` places corrections first.
    #[test]
    fn feed_change_wins_over_a_stale_correction_for_the_same_key_through_a_real_apply_block() {
        use ikpir_common::backend::simple::SimpleParams;
        use ikpir_common::pir_params::simple_max_plaintext_bits;
        use ikpir_common::{SimpleConfig, SimplePirBackend};
        use risepir_proto::{BlockUpdate, ValueCodec};
        use risepir_server::RisePirServer;
        use segmented_cuckoo::{Segmented2aryCuckooKVStore, Segmented2aryScheme};

        let codec = ValueCodec { key_tag_bits: 32, balance_bits: 96, checksum_bits: 16 };
        let num_buckets = 2 * 64;
        let pb = simple_max_plaintext_bits(2, num_buckets / 2, 4, 32, codec.value_bits(), SimpleParams::DEFAULT_SIGMA);
        let store = Segmented2aryCuckooKVStore::new(num_buckets, 4, 32, codec.value_bits(), pb).unwrap();
        let mut server: RisePirServer<Segmented2aryScheme, SimplePirBackend> =
            RisePirServer::new(store, SimpleConfig::with_lwe_dim(256), codec, 0);

        let key = keccak256(&[0x42u8; 20]);
        // Seed the store with an initial balance so there is a "before" to
        // overwrite either way.
        server
            .apply_block(&BlockUpdate { block: 1, changes: vec![(key, 111)], credits: vec![] })
            .unwrap();
        assert_eq!(server.balance_of(&key).unwrap(), Some(111));

        // A stale correction says 222 (what the chain looked like when the
        // hard-refresh check ran, against a baseline of 111 — the store's
        // balance at that same moment); the feed's own change for this same
        // block says the account is actually 333 now.
        let stale_correction = vec![Correction { key, balance: 222, baseline: 111 }];
        let feed_changes = vec![(key, 333u128)];
        let composed = prepend_corrections(stale_correction, feed_changes);

        server
            .apply_block(&BlockUpdate { block: 2, changes: composed, credits: vec![] })
            .unwrap();

        assert_eq!(
            server.balance_of(&key).unwrap(),
            Some(333),
            "the feed's own change must win over a stale correction for the same key"
        );
    }

    /// The race `filter_stale_corrections` exists to close: a correction
    /// queued behind a large backlog is decided against the store's
    /// balance *at check time* (`baseline`), but by the time it is this
    /// correction's turn to drain, an ordinary feed-applied block may have
    /// legitimately touched the same account in a *different* block than
    /// the one the correction happens to land in — so a naive drain would
    /// silently overwrite that fresher, correct value with the stale
    /// corrected one. This must never happen: a correction whose live
    /// value no longer matches its recorded baseline must be dropped, not
    /// applied — proved here with a real `NodeState`/`apply_block`, not a
    /// mock.
    #[tokio::test]
    async fn filter_stale_corrections_drops_a_correction_whose_account_moved_on_since_the_check() {
        use ikpir_common::backend::simple::SimpleParams;
        use ikpir_common::pir_params::simple_max_plaintext_bits;
        use ikpir_common::{SimpleConfig, SimplePirBackend};
        use risepir_http::NodeState;
        use risepir_proto::{BlockUpdate, ValueCodec};
        use risepir_server::{DeltaRing, RisePirServer};
        use segmented_cuckoo::{Segmented2aryCuckooKVStore, Segmented2aryScheme};

        let codec = ValueCodec { key_tag_bits: 32, balance_bits: 96, checksum_bits: 16 };
        let num_buckets = 2 * 64;
        let pb = simple_max_plaintext_bits(2, num_buckets / 2, 4, 32, codec.value_bits(), SimpleParams::DEFAULT_SIGMA);
        let store = Segmented2aryCuckooKVStore::new(num_buckets, 4, 32, codec.value_bits(), pb).unwrap();
        let server: RisePirServer<Segmented2aryScheme, SimplePirBackend> =
            RisePirServer::new(store, SimpleConfig::with_lwe_dim(256), codec, 0);
        let node = NodeState::new(server, DeltaRing::new(64), true);

        let key_untouched = keccak256(&[0xAAu8; 20]);
        let key_moved_on = keccak256(&[0xBBu8; 20]);
        // Both start at 100 — this is the moment the hard-refresh check ran
        // and decided both need correcting to 150.
        node.apply_block(&BlockUpdate {
            block: 1,
            changes: vec![(key_untouched, 100), (key_moved_on, 100)],
            credits: vec![],
        })
        .await
        .unwrap();

        // Before the corrections get a chance to drain (they are queued
        // behind a large backlog, say), an ordinary block legitimately
        // moves key_moved_on to 999 — a real transaction, nothing to do
        // with hard-refresh. key_untouched is not touched by anything.
        node.apply_block(&BlockUpdate { block: 2, changes: vec![(key_moved_on, 999)], credits: vec![] })
            .await
            .unwrap();

        let drained = vec![
            Correction { key: key_untouched, balance: 150, baseline: 100 },
            Correction { key: key_moved_on, balance: 150, baseline: 100 },
        ];
        let (still_valid, stale) = filter_stale_corrections(&node, drained).await.unwrap();

        assert_eq!(stale, 1, "exactly the moved-on account's correction must be dropped as stale");
        assert_eq!(
            still_valid,
            vec![Correction { key: key_untouched, balance: 150, baseline: 100 }],
            "the untouched account's correction must survive unchanged"
        );

        // And, composed through a real apply_block: applying only the
        // surviving correction must never disturb key_moved_on's fresher,
        // correct value.
        node.apply_block(&BlockUpdate {
            block: 3,
            changes: prepend_corrections(still_valid, Vec::new()),
            credits: vec![],
        })
        .await
        .unwrap();
        assert_eq!(node.balance_of(&key_untouched).await.unwrap(), Some(150));
        assert_eq!(
            node.balance_of(&key_moved_on).await.unwrap(),
            Some(999),
            "the stale correction must never have been allowed to overwrite the fresher on-chain value"
        );
    }
}
