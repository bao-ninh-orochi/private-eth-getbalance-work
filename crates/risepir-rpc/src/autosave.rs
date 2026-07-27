//! Periodic state autosave (ADR-0025): bound how far the on-disk state
//! file can fall behind the running server.
//!
//! Before this module the state file was written exactly twice — once at
//! bootstrap and once on Ctrl-C — so an ungraceful kill cost a replay of
//! everything applied since startup (measured on the live deployment,
//! 2026-07-26: 3,647 blocks behind after under two hours, growing ~300
//! blocks/hour without bound). [`StateSaver`] caps that at an
//! operator-chosen interval.
//!
//! # Why the save must run in the block-applier's own task
//!
//! A full save streams the whole database under [`NodeState::with_server`]'s
//! **read** lock — minutes at the complete mainnet set. `tokio::sync::RwLock`
//! is fair (write-preferring): the moment a writer queues behind that read
//! guard, every *later* reader parks behind the writer too. So a periodic
//! save from its own task would, each time `apply_block` woke up mid-save,
//! turn "the follow loop waits" into "every `/answer` request waits" — a
//! minutes-long serving outage per save (pinned by the
//! `queued_writer_parks_new_readers` test in `tests/autosave.rs`).
//!
//! `NodeState`'s only writer is the follow loop (`inner.write().await`
//! appears exactly once, in `NodeState::apply_block`). Running the save
//! *from that same task, between blocks* therefore guarantees no writer
//! can queue during a save — readers keep flowing for the whole save, and
//! the only cost is the follow loop pausing (it falls a handful of blocks
//! behind and catches up, exactly as it does after any restart).
//!
//! # Why the saved file is consistent
//!
//! `D` (cells) and `H` (hints) must be captured at one block height — a
//! file mixing heights would reload as a silently inconsistent server, the
//! one outcome this project treats as total failure. The read guard held
//! across the entire streamed [`state::save`] is what guarantees that:
//! `apply_block` needs the write lock, so no mutation can interleave with
//! the save no matter which task runs it. (Running in the applier's task
//! is about *liveness*; the read guard alone carries *consistency* — see
//! `concurrent_saves_reload_consistently` in `tests/autosave.rs`, which
//! hammers saves against a concurrent applier and byte-compares every
//! saved file to a sequential reference at the same height.)
//!
//! # Memory
//!
//! This module adds no path that copies the cells: it calls the same
//! streaming [`state::save`] the Ctrl-C path uses (borrowed cells,
//! 64 Ki-cell chunks — the PR #6 constraint). Peak RSS during an autosave
//! is identical to peak RSS during the existing shutdown save.
//!
//! # The delta journal (`docs/adr/README.md` ADR-0026)
//!
//! Beside the periodic full save, this module also owns the current
//! [`JournalWriter`] — a sidecar `<state>.journal` recording one
//! [`BlockDelta`] per applied block, so a crash between full saves costs
//! only journal replay, not a full re-follow. [`StateSaver::append_delta`]
//! is called once per applied block (`mainnet.rs`'s follow loop); rotation
//! — starting a fresh journal bound to the base a save just produced —
//! happens here, in `StateSaver::save_with` (private), strictly *after*
//! `state::save`'s rename completes. That ordering is load-bearing: a
//! crash between the two leaves (new base, old journal) whose digests
//! mismatch, which a loader detects and ignores loudly (`mainnet.rs`)
//! rather than trusting a journal that might predate the base it sits
//! beside.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use risepir_http::NodeState;
use risepir_proto::{BlockDelta, ValueCodec};

use crate::journal::{self, JournalWriter};
use crate::state::{self, StateError};

/// Serializes every state save for one `--state` path and decides when a
/// periodic one is due. Shared (via `Arc`) between the follow loop (which
/// calls [`Self::maybe_save`] between blocks) and the shutdown path
/// (which calls [`Self::save_now`]); the internal mutex is what stops
/// those two from ever writing `<path>.tmp` concurrently — unserialized,
/// their interleaved writes would produce garbage that the rename then
/// installs *over the previous good file* (the checksum would catch it at
/// load, loudly, but the fast-restart file would be lost).
pub struct StateSaver {
    path: PathBuf,
    /// `<path>`'s journal sidecar (`crate::journal::journal_path_for`) —
    /// derived once at construction, not re-derived on every rotation.
    journal_path: PathBuf,
    codec: ValueCodec,
    complete: bool,
    interval: Duration,
    /// SCF plaintext width — needed to encode every journal record
    /// (`risepir_proto::codec::encode_block_delta`); fixed for the
    /// deployment's lifetime (this design never changes geometry without
    /// a fresh base), so it lives here rather than being re-threaded
    /// through every [`Self::append_delta`] / rotation call.
    plaintext_bits: u32,
    inner: tokio::sync::Mutex<SaverInner>,
}

struct SaverInner {
    /// Block height of the last completed save to `path`, if one is known
    /// to exist — lets [`StateSaver::maybe_save`] skip rewriting tens of
    /// GB of identical bytes when nothing was applied since (e.g. while
    /// the feed is unreachable). Sound because `RisePirServer::block()`
    /// is strictly monotone across successful `apply_block` calls, and
    /// `apply_block` is the sole mutator: equal height ⇔ identical state.
    last_saved_block: Option<u64>,
    /// When the last save *attempt* finished (successfully or not) — the
    /// interval is measured from completion, so a save that takes minutes
    /// can never be scheduled back-to-back, and a failing save (disk
    /// full) retries once per interval instead of once per loop tick.
    last_finished: Instant,
    /// The current journal appender, if journaling is currently active.
    /// `None` means "not writing right now" — either nothing has rotated
    /// yet this run, a rotation I/O failure left it unset (retried at the
    /// next successful save), or [`Self::journal_broken`] permanently
    /// disabled it.
    journal: Option<JournalWriter>,
    /// Set once [`StateSaver::append_delta`] gets any error from
    /// [`JournalWriter::append`] — a continuity gap, or an I/O failure
    /// that may have torn the file's tail — permanently disabling
    /// journaling for the rest of this process's run, including future
    /// rotations. Deliberately stronger than a rotation I/O failure
    /// (which only disables journaling *until the next save*): a gap
    /// means this run's own bookkeeping violated the one invariant the
    /// journal exists to guarantee, and a mid-append I/O failure leaves
    /// the current file untrustworthy, so a fresh file at the next
    /// rotation is not trusted to be free of whatever caused it either —
    /// never a silently wrong answer.
    journal_broken: bool,
}

/// What a [`StateSaver`] call did — returned so callers (and tests) can
/// assert on behavior; all logging happens inside the saver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveOutcome {
    /// A new state file was written and renamed into place.
    Saved {
        /// Block height the file captures.
        block: u64,
        /// File size in bytes.
        bytes: u64,
    },
    /// Nothing was applied since the last save; the write was skipped.
    Unchanged {
        /// The (already persisted) block height.
        block: u64,
    },
    /// The interval has not elapsed yet (or autosave is disabled).
    NotDue,
    /// Another save is currently running ([`StateSaver::maybe_save`]
    /// never waits — the next loop iteration will try again).
    Busy,
}

impl StateSaver {
    /// A saver for `path`. `interval` of zero disables the periodic
    /// trigger ([`Self::maybe_save`] becomes a no-op; [`Self::save_now`]
    /// still works, so shutdown saves are unaffected). `last_saved_block`
    /// is the height already on disk at `path`, when the caller knows it
    /// (just loaded the file, or just wrote it after bootstrap) — `None`
    /// makes the first due save unconditional.
    ///
    /// `plaintext_bits` is this deployment's fixed SCF plaintext width
    /// (needed to encode every journal record). `initial_journal` is the
    /// appender `mainnet.rs` decided to adopt or create at startup, if
    /// any (`None` is entirely normal — e.g. a fresh partial bootstrap has
    /// no base to bind a journal to yet; the first successful save's
    /// rotation starts one).
    pub fn new(
        path: PathBuf,
        codec: ValueCodec,
        complete: bool,
        interval: Duration,
        last_saved_block: Option<u64>,
        plaintext_bits: u32,
        initial_journal: Option<JournalWriter>,
    ) -> Self {
        let journal_path = journal::journal_path_for(&path);
        Self {
            path,
            journal_path,
            codec,
            complete,
            interval,
            plaintext_bits,
            inner: tokio::sync::Mutex::new(SaverInner {
                last_saved_block,
                // First periodic save is due one full interval after
                // startup: the process just bootstrapped from (or wrote)
                // a current file, so there is nothing urgent to persist.
                last_finished: Instant::now(),
                journal: initial_journal,
                journal_broken: false,
            }),
        }
    }

    /// The configured interval (zero = periodic saves disabled).
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Appends `delta` (with the item count immediately after applying
    /// it) to the current journal, if journaling is active. Locks the
    /// same mutex [`Self::maybe_save`] / [`Self::save_now`] use —
    /// `lock().await`, **not** `try_lock`: an append must never be
    /// silently dropped just because a save happens to be in flight. The
    /// only long holder of this mutex is a save itself, and appends are
    /// driven from the same task as autosaves (`mainnet.rs`'s follow
    /// loop), so in practice this only ever waits during a shutdown save
    /// racing the final applied block.
    ///
    /// On an append error — a continuity gap, or an I/O failure that may
    /// have left a torn record at the file's tail (see
    /// [`JournalWriter::append`]): logs one loud `WARNING`, permanently
    /// disables journaling for the rest of this run, and returns —
    /// serving is entirely unaffected; durability degrades to whatever
    /// `--save-interval` the operator configured, never to a wrong
    /// answer.
    pub async fn append_delta(&self, delta: &BlockDelta, num_items_after: u64) {
        let mut guard = self.inner.lock().await;
        if guard.journal_broken {
            return;
        }
        let Some(writer) = guard.journal.as_mut() else {
            return; // no journal active right now (nothing has rotated yet, or a rotation failed)
        };
        if let Err(e) = writer.append(delta, num_items_after) {
            eprintln!(
                "risepir-rpc mainnet: WARNING: journal append failed ({e}) — disabling journaling for \
                 the rest of this run; state saves continue on schedule (--save-interval), so durability \
                 degrades to that cadence, never to a wrong answer"
            );
            guard.journal = None;
            guard.journal_broken = true;
        }
    }

    /// Periodic trigger — called by the follow loop between blocks. Saves
    /// iff the interval has elapsed since the last attempt finished *and*
    /// at least one block was applied since the last completed save.
    /// Never waits on a running save and never blocks the caller beyond
    /// the save itself. Errors are logged and returned; the caller is
    /// expected to keep following regardless (a failed save costs
    /// restart speed, never correctness — the next interval retries).
    pub async fn maybe_save(&self, node: &NodeState) -> Result<SaveOutcome, StateError> {
        if self.interval.is_zero() {
            return Ok(SaveOutcome::NotDue);
        }
        // try_lock, not lock: if a save is already running (a shutdown
        // save racing us), queueing a second one behind it is never
        // useful — the follow loop should get back to applying blocks.
        let Ok(mut guard) = self.inner.try_lock() else {
            return Ok(SaveOutcome::Busy);
        };
        if guard.last_finished.elapsed() < self.interval {
            return Ok(SaveOutcome::NotDue);
        }
        self.save_with(&mut guard, node, false, "autosave").await
    }

    /// Unconditional save — the shutdown path. Waits for any in-flight
    /// save to finish first (serialization, see the type docs), then
    /// writes the current state even if the height is unchanged (the
    /// operator asked for a save; honoring it is cheaper than arguing).
    pub async fn save_now(&self, node: &NodeState, reason: &str) -> Result<SaveOutcome, StateError> {
        let mut guard = self.inner.lock().await;
        self.save_with(&mut guard, node, true, reason).await
    }

    async fn save_with(
        &self,
        guard: &mut SaverInner,
        node: &NodeState,
        force: bool,
        reason: &str,
    ) -> Result<SaveOutcome, StateError> {
        let last_saved = guard.last_saved_block;
        let started = Instant::now();
        // The closure only ever *reads* `self`/`last_saved`/`force`/
        // `reason` — rotation and every `guard` mutation happen below,
        // after the read lock (held for the closure's whole body — the
        // consistency guarantee, module docs) has been released, so
        // there is no question of a `guard` borrow needing to cross it.
        let result: Result<(SaveOutcome, Option<u64>), StateError> = node
            .with_server(|server| {
                let block = server.block();
                if !force && last_saved == Some(block) {
                    return Ok((SaveOutcome::Unchanged { block }, None));
                }
                eprintln!(
                    "risepir-rpc mainnet: saving state ({reason}) at block {block} to {} ...",
                    self.path.display()
                );
                // block_in_place keeps the minutes-long file write from
                // starving the runtime worker this task occupies.
                let report = run_blocking(|| state::save(server, &self.codec, self.complete, &self.path))?;
                Ok((SaveOutcome::Saved { block, bytes: report.bytes }, Some(report.digest)))
            })
            .await;

        // The interval restarts after every *attempt* (see the field
        // docs); only a completed write updates the saved height.
        guard.last_finished = Instant::now();
        match &result {
            Ok((SaveOutcome::Saved { block, bytes }, Some(digest))) => {
                guard.last_saved_block = Some(*block);

                // Rotation strictly *after* the base save's rename above
                // has already completed (module docs: a crash between
                // here and a completed rotation leaves (new base, old
                // journal) whose digests mismatch, which load-time
                // detects and ignores loudly rather than trusting a
                // journal that might predate this base). A journal
                // permanently disabled by a continuity gap is never
                // resurrected here — see `journal_broken`'s docs.
                if !guard.journal_broken {
                    match JournalWriter::create(&self.journal_path, *digest, *block, self.plaintext_bits) {
                        Ok(w) => guard.journal = Some(w),
                        Err(e) => {
                            guard.journal = None;
                            eprintln!(
                                "risepir-rpc mainnet: WARNING: journal rotation failed ({e}) — journaling \
                                 disabled until the next successful save"
                            );
                        }
                    }
                }

                let secs = started.elapsed().as_secs_f64();
                eprintln!(
                    "risepir-rpc mainnet: state saved ({reason}): block {block}, {:.2} GB in {:.1}s ({:.0} MB/s)",
                    *bytes as f64 / 1e9,
                    secs,
                    *bytes as f64 / 1e6 / secs.max(f64::EPSILON),
                );
            }
            Ok((SaveOutcome::Saved { .. }, None)) => {
                unreachable!("SaveOutcome::Saved is only ever produced paired with Some(digest)")
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!(
                    "risepir-rpc mainnet: WARNING: state save ({reason}) failed: {e} — \
                     serving is unaffected; the previous state file (if any) is intact"
                );
            }
        }
        result.map(|(outcome, _digest)| outcome)
    }
}

/// Run a blocking closure without starving the async runtime: on the
/// multi-thread runtime this is `block_in_place` (the worker hands its
/// queue to a sibling); anywhere else (`current_thread` tests) it just
/// runs inline, where blocking the only thread is the caller's informed
/// choice.
fn run_blocking<R>(f: impl FnOnce() -> R) -> R {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}
