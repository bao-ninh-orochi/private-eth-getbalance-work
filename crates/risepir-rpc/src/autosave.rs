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

use std::path::PathBuf;
use std::time::{Duration, Instant};

use risepir_http::NodeState;
use risepir_proto::ValueCodec;

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
    codec: ValueCodec,
    complete: bool,
    interval: Duration,
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
    pub fn new(
        path: PathBuf,
        codec: ValueCodec,
        complete: bool,
        interval: Duration,
        last_saved_block: Option<u64>,
    ) -> Self {
        Self {
            path,
            codec,
            complete,
            interval,
            inner: tokio::sync::Mutex::new(SaverInner {
                last_saved_block,
                // First periodic save is due one full interval after
                // startup: the process just bootstrapped from (or wrote)
                // a current file, so there is nothing urgent to persist.
                last_finished: Instant::now(),
            }),
        }
    }

    /// The configured interval (zero = periodic saves disabled).
    pub fn interval(&self) -> Duration {
        self.interval
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
        let result = node
            .with_server(|server| {
                let block = server.block();
                if !force && last_saved == Some(block) {
                    return Ok(SaveOutcome::Unchanged { block });
                }
                eprintln!(
                    "risepir-rpc mainnet: saving state ({reason}) at block {block} to {} ...",
                    self.path.display()
                );
                // The read guard is held for this entire call — that is
                // the consistency guarantee (module docs). block_in_place
                // keeps the minutes-long file write from starving the
                // runtime worker this task occupies.
                let bytes = run_blocking(|| state::save(server, &self.codec, self.complete, &self.path))?;
                Ok(SaveOutcome::Saved { block, bytes })
            })
            .await;

        // The interval restarts after every *attempt* (see the field
        // docs); only a completed write updates the saved height.
        guard.last_finished = Instant::now();
        match &result {
            Ok(SaveOutcome::Saved { block, bytes }) => {
                guard.last_saved_block = Some(*block);
                let secs = started.elapsed().as_secs_f64();
                eprintln!(
                    "risepir-rpc mainnet: state saved ({reason}): block {block}, {:.2} GB in {:.1}s ({:.0} MB/s)",
                    *bytes as f64 / 1e9,
                    secs,
                    *bytes as f64 / 1e6 / secs.max(f64::EPSILON),
                );
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!(
                    "risepir-rpc mainnet: WARNING: state save ({reason}) failed: {e} — \
                     serving is unaffected; the previous state file (if any) is intact"
                );
            }
        }
        result
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
