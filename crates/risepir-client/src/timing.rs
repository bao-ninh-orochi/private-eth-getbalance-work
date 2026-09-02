//! Optional per-step instrumentation for [`crate::RisePirClient::finish`]
//! — the four rewind steps ADR-0003 names, timed individually.
//!
//! # Why an observer rather than timers in `finish` itself
//!
//! `finish` is the product's hot path and also the wasm client's
//! (ADR-0019, where `std::time::Instant` does not even work). Timing it
//! unconditionally would put a clock read on every segment boundary of
//! every query, in a build that never reads the numbers. So the *caller*
//! supplies the clock: [`FinishObserver::mark`] is called at each step
//! boundary, and [`NoFinishObserver`] — the observer
//! [`crate::RisePirClient::finish`] itself passes — implements it as an
//! empty body, which monomorphises away entirely.
//!
//! # What it must not change
//!
//! Step 5's fingerprint / `key_tag` scan is deliberately constant-time
//! (see [`crate::RisePirClient::finish`]'s docs). A mark is placed
//! *around* it, never inside its per-slot loop, so measuring the scan
//! cannot introduce a data-dependent branch into it. Timing it is fine;
//! changing it is not.

use std::time::{Duration, Instant};

/// A step boundary inside [`crate::RisePirClient::finish`]'s per-segment
/// loop. Each variant names the step that has just *completed*, except
/// [`Self::SegmentStart`], which is the clock origin for the segment.
///
/// The numbering matches `docs/plan.md` §3.3 / ADR-0003's rewind steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FinishPhase {
    /// Start of one segment's work — the origin for the segment's
    /// [`Self::Rewind`] measurement.
    SegmentStart,
    /// Step 2 (`B::rewind_response`: `resp -= qᵀ·ΔD`) just completed.
    Rewind,
    /// Step 3 (`B::client_decode` against the stale hint) just completed.
    Decode,
    /// Step 4 (per-cell `cells += ΔD[row]`) just completed.
    DeltaApply,
    /// Step 5 (the constant-time fp ∧ `key_tag` scan) just completed.
    Scan,
}

/// Receives [`FinishPhase`] boundaries from
/// [`crate::RisePirClient::finish_observed`].
///
/// Implementors own the clock: `mark` is called with no timing argument,
/// so an implementation that does not want to measure (the default,
/// [`NoFinishObserver`]) never reads one.
pub trait FinishObserver {
    /// Called once per step boundary, in `finish`'s own order, for every
    /// segment. An implementation must not panic and must not block —
    /// it runs inside the rewind's inner loop.
    fn mark(&mut self, phase: FinishPhase);
}

/// The default observer: every [`FinishObserver::mark`] is an empty
/// inlined body, so an uninstrumented `finish` reads no clock at all.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoFinishObserver;

impl FinishObserver for NoFinishObserver {
    #[inline]
    fn mark(&mut self, _phase: FinishPhase) {}
}

/// Accumulates ADR-0003's four rewind steps across every segment of one
/// [`crate::RisePirClient::finish_observed`] call.
///
/// The four durations sum to slightly less than the wall time of
/// `finish` — the argument checks, the `candidate_buckets` re-hash, and
/// the final `value_codec.decode` sit outside the per-segment loop. A
/// caller that reports a budget must carry that difference explicitly
/// rather than distributing it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FinishTimings {
    /// Step 2, summed over segments: `B::rewind_response`.
    pub rewind: Duration,
    /// Step 3, summed over segments: `B::client_decode`.
    pub decode: Duration,
    /// Step 4, summed over segments: the per-cell delta apply.
    pub delta_apply: Duration,
    /// Step 5, summed over segments: the constant-time fp ∧ `key_tag`
    /// scan.
    pub scan: Duration,
    /// The previous mark's instant. `None` before the first
    /// [`FinishPhase::SegmentStart`].
    last: Option<Instant>,
}

impl FinishTimings {
    /// A fresh, all-zero accumulator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Total of the four measured steps.
    #[must_use]
    pub fn total(&self) -> Duration {
        self.rewind + self.decode + self.delta_apply + self.scan
    }
}

impl FinishObserver for FinishTimings {
    fn mark(&mut self, phase: FinishPhase) {
        let now = Instant::now();
        // `saturating_duration_since` rather than `-`: `Instant` is
        // monotonic on every supported platform, but a saturating read
        // means even a pathological clock can only under-report, never
        // panic in the middle of a lookup.
        let elapsed = self
            .last
            .map_or(Duration::ZERO, |t| now.saturating_duration_since(t));
        match phase {
            FinishPhase::SegmentStart => {}
            FinishPhase::Rewind => self.rewind += elapsed,
            FinishPhase::Decode => self.decode += elapsed,
            FinishPhase::DeltaApply => self.delta_apply += elapsed,
            FinishPhase::Scan => self.scan += elapsed,
        }
        self.last = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_observer_is_zero_sized() {
        assert_eq!(std::mem::size_of::<NoFinishObserver>(), 0);
    }

    #[test]
    fn marks_accumulate_into_the_named_buckets() {
        let mut t = FinishTimings::new();
        // Two segments' worth of marks; every bucket must be touched
        // exactly twice and none of them may stay at `Duration::ZERO`'s
        // "never marked" state by accident.
        for _ in 0..2 {
            t.mark(FinishPhase::SegmentStart);
            t.mark(FinishPhase::Rewind);
            t.mark(FinishPhase::Decode);
            t.mark(FinishPhase::DeltaApply);
            t.mark(FinishPhase::Scan);
        }
        assert_eq!(
            t.total(),
            t.rewind + t.decode + t.delta_apply + t.scan,
            "total() must be exactly the four buckets"
        );
    }

    #[test]
    fn segment_start_is_not_charged_to_any_bucket() {
        let mut t = FinishTimings::new();
        t.mark(FinishPhase::SegmentStart);
        t.mark(FinishPhase::Scan);
        let after_first = t.scan;
        // A second SegmentStart resets the origin without charging the
        // gap since the previous Scan to anything.
        t.mark(FinishPhase::SegmentStart);
        assert_eq!(t.scan, after_first);
        assert_eq!(t.rewind, Duration::ZERO);
        assert_eq!(t.decode, Duration::ZERO);
        assert_eq!(t.delta_apply, Duration::ZERO);
    }
}
