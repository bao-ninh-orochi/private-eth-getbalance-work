#![warn(missing_docs)]
//! A `#[global_allocator]`-installable instrument: wraps
//! [`std::alloc::System`], recording the largest single allocation
//! request size seen while [`start_recording`] is in effect.
//!
//! # Why this is its own crate
//!
//! Every other crate in this workspace sets `unsafe_code = "forbid"` (or
//! `"deny"` for `risepir-wasm`) — a deliberate, workspace-wide policy. A
//! [`GlobalAlloc`] implementation is `unsafe fn` by the standard library's
//! own trait definition; there is no safe way to write one. Cargo's
//! per-package `[lints]` table applies to *every* target in a package —
//! library, tests, everything — by emitting the equivalent of `-F
//! unsafe-code` for each rustc invocation, and a command-line `forbid`
//! cannot be downgraded by any in-source `#[allow(unsafe_code)]`
//! (confirmed directly: adding the impl straight to
//! `crates/risepir-server/tests/full_rebuild_alloc.rs` fails to compile
//! with "implementation of an `unsafe` trait ... requested on the command
//! line with `-F unsafe-code`"). So the one `unsafe impl` this instrument
//! needs cannot live inside `risepir-server` at all, no matter which file
//! it's in.
//!
//! Rather than weaken `risepir-server`'s forbid — a project-wide
//! invariant, not a per-file accident — this crate exists to quarantine
//! that one unavoidable `unsafe impl` by itself. Nothing here is a
//! runtime dependency of anything; it is `[dev-dependencies]` only, used
//! exactly once: `crates/risepir-server/tests/full_rebuild_alloc.rs`
//! installs [`TrackingAllocator`] as its `#[global_allocator]`, which is
//! itself a safe `static` declaration, so that test file stays under
//! `risepir-server`'s own forbid without contradiction.
//!
//! # Design
//!
//! No locking anywhere — a `Mutex` (or anything else heap-backed) guarding
//! the recorded maximum would either deadlock (the allocator being called
//! reentrantly while it holds its own lock) or itself allocate, corrupting
//! the very measurement it exists to take. A compare-exchange loop over a
//! plain [`AtomicUsize`] needs neither, and every access here is a bare
//! atomic op — this module never allocates.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Recording toggle: allocation sizes are only measured while this is
/// `true` (set by [`start_recording`] / [`stop_recording`]).
static RECORDING: AtomicBool = AtomicBool::new(false);

/// Largest single `alloc`/`alloc_zeroed`/`realloc` request size, in bytes,
/// observed since the most recent [`start_recording`].
static MAX_SINGLE_ALLOC: AtomicUsize = AtomicUsize::new(0);

/// Reset the recorded maximum to 0 and start recording allocation sizes.
///
/// Not scoped/RAII — paired explicitly with [`stop_recording`] by the
/// caller — because the property under test brackets one specific call
/// (e.g. `RisePirServer::full_rebuild`), not a lexical scope, and the
/// caller already controls exactly that window.
pub fn start_recording() {
    MAX_SINGLE_ALLOC.store(0, Ordering::SeqCst);
    RECORDING.store(true, Ordering::SeqCst);
}

/// Stop recording. [`max_single_alloc`] keeps reporting the last recorded
/// value until the next [`start_recording`] resets it.
pub fn stop_recording() {
    RECORDING.store(false, Ordering::SeqCst);
}

/// The largest single allocation/reallocation request size seen during the
/// most recent `start_recording`/`stop_recording` window, in bytes.
pub fn max_single_alloc() -> usize {
    MAX_SINGLE_ALLOC.load(Ordering::SeqCst)
}

/// Compare-exchange the running maximum up to `size` if it's larger — the
/// only thing on the hot allocation path, and it never allocates.
#[inline]
fn record(size: usize) {
    if !RECORDING.load(Ordering::Relaxed) {
        return;
    }
    let mut observed = MAX_SINGLE_ALLOC.load(Ordering::Relaxed);
    while size > observed {
        match MAX_SINGLE_ALLOC.compare_exchange_weak(
            observed,
            size,
            Ordering::SeqCst,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(current) => observed = current,
        }
    }
}

/// Wraps [`System`], recording the largest single allocation request size
/// passed to `alloc`/`alloc_zeroed`/`realloc` while [`start_recording`] is
/// in effect. Install it with `#[global_allocator]` in a test binary that
/// needs to pin "no single allocation exceeds N bytes" as a regression
/// test.
///
/// `dealloc` needs no accounting — freeing memory can never be the
/// over-large allocation this instrument exists to catch.
///
/// # Why a single process-wide instance is fine
///
/// All state lives in this module's statics rather than on the struct
/// itself (`TrackingAllocator` carries no fields), which matches its one
/// intended use: exactly one instance is ever the process's
/// `#[global_allocator]` at a time.
#[derive(Debug, Default, Clone, Copy)]
pub struct TrackingAllocator;

// SAFETY: every method here forwards straight to `System`'s own
// implementation of the identical method, after recording the requested
// size via `record` — which touches only plain atomics and performs no
// allocation of its own. So this impl upholds exactly the safety
// obligations `GlobalAlloc` documents and no more, identically to using
// `System` directly, for every method including the ones not overridden
// here (their default implementations delegate back to `alloc`/`dealloc`
// above, so nothing bypasses the recording).
unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        System.alloc(layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        System.alloc_zeroed(layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(new_size);
        System.realloc(ptr, layout, new_size)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
}
