//! Optional wire-level instrumentation for [`crate::PirHttpClient`] —
//! per-endpoint call counts, wall time, and byte counts.
//!
//! # Why this lives at the transport, not above it
//!
//! A measurement campaign that wants to say "this much of the end-to-end
//! latency was the network" has to time each call *before the request is
//! sent* and *until the last body byte is in hand* — which is a fact only
//! the code holding the [`reqwest::Response`] knows. Timing from above
//! (around `PirHttpClient::answer`) would fold the wire codec's own
//! encode/decode into "network", and timing from below (reqwest metrics)
//! cannot see the streamed, capped body accumulation this crate does
//! itself. So the client records it, and a caller opts in by attaching a
//! [`NetSink`].
//!
//! # What is measured, exactly
//!
//! For every call: `wire_ns` spans from immediately before
//! `RequestBuilder::send()` to immediately after the last body byte has
//! been accumulated (or, on a non-`200`, after the error body has been
//! read). `decode_ns` spans the subsequent wire/codec decode of that
//! body, and is therefore *disjoint* from `wire_ns` — a caller building
//! a latency budget adds them rather than worrying about overlap.
//! Failed calls are recorded too: a request that never got a response
//! still consumed wall time, and silently dropping it would make a
//! budget close only by luck.
//!
//! # Privacy
//!
//! Nothing here is per-address. A [`NetStats`] holds counts, durations,
//! and byte totals — never a query, a response body, or anything derived
//! from the key being looked up. `docs/threat-model.md` §5's caution
//! about publishing aggregates applies to the *server's* `/metrics`;
//! this is a client-local counter that never leaves the process that
//! made the calls.

use std::sync::Mutex;
use std::time::Duration;

/// Which endpoint a measured call hit.
///
/// A closed set, so a [`NetStats`] has one fixed field per endpoint and
/// a caller never has to string-match a route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NetCall {
    /// `GET /setup`.
    Setup,
    /// `GET /head`.
    Head,
    /// `GET /mode`.
    Mode,
    /// `GET /sync?from=&to=&epoch=`.
    Sync,
    /// `POST /answer?epoch=`.
    Answer,
    /// `GET /recent`.
    Recent,
}

/// Accumulated measurements for one endpoint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallStats {
    /// How many calls were made (successful *and* failed).
    pub calls: u64,
    /// Total time from just before `send()` to the last body byte,
    /// summed over calls. Excludes decoding (see [`Self::decode_ns`]).
    pub wire_ns: u64,
    /// Total decode time — the wire/codec decode of the received body,
    /// summed over calls. Disjoint from [`Self::wire_ns`].
    pub decode_ns: u64,
    /// Total request-body bytes sent (0 for `GET`s).
    pub request_bytes: u64,
    /// Total response-body bytes received.
    pub response_bytes: u64,
    /// The `content-length` header of the **last** response, if the
    /// server declared one. Kept as the last value rather than a sum:
    /// it exists to be cross-checked against that call's own
    /// [`Self::response_bytes`], which a total would destroy.
    pub last_content_length: Option<u64>,
}

impl CallStats {
    /// [`Self::wire_ns`] as a [`Duration`].
    #[must_use]
    pub const fn wire(&self) -> Duration {
        Duration::from_nanos(self.wire_ns)
    }

    /// [`Self::decode_ns`] as a [`Duration`].
    #[must_use]
    pub const fn decode(&self) -> Duration {
        Duration::from_nanos(self.decode_ns)
    }
}

/// One snapshot of an instrumented client's per-endpoint counters, plus
/// the server-reported answer timings from the most recent `POST
/// /answer`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetStats {
    /// `GET /setup`.
    pub setup: CallStats,
    /// `GET /head`.
    pub head: CallStats,
    /// `GET /mode`.
    pub mode: CallStats,
    /// `GET /sync`.
    pub sync: CallStats,
    /// `POST /answer`.
    pub answer: CallStats,
    /// `GET /recent`.
    pub recent: CallStats,
    /// The last `POST /answer` response's
    /// `x-risepir-answer-compute-ns` header, if the server sent one.
    ///
    /// Optional by construction: a server that does not publish
    /// per-answer timings simply omits the header, and this stays
    /// `None`. Never defaulted to zero — "the server did not say" and
    /// "the server said zero" are different facts.
    pub server_compute_ns: Option<u64>,
    /// The last `POST /answer` response's
    /// `x-risepir-answer-handler-ns` header, if the server sent one.
    /// Same optionality as [`Self::server_compute_ns`].
    pub server_handler_ns: Option<u64>,
}

impl NetStats {
    /// Per-endpoint stats for `call`.
    #[must_use]
    pub const fn get(&self, call: NetCall) -> CallStats {
        match call {
            NetCall::Setup => self.setup,
            NetCall::Head => self.head,
            NetCall::Mode => self.mode,
            NetCall::Sync => self.sync,
            NetCall::Answer => self.answer,
            NetCall::Recent => self.recent,
        }
    }

    /// Mutable per-endpoint stats for `call`.
    fn get_mut(&mut self, call: NetCall) -> &mut CallStats {
        match call {
            NetCall::Setup => &mut self.setup,
            NetCall::Head => &mut self.head,
            NetCall::Mode => &mut self.mode,
            NetCall::Sync => &mut self.sync,
            NetCall::Answer => &mut self.answer,
            NetCall::Recent => &mut self.recent,
        }
    }

    /// Total wire time across every endpoint — the "all network" term of
    /// a client-side latency budget.
    #[must_use]
    pub const fn total_wire_ns(&self) -> u64 {
        self.setup
            .wire_ns
            .saturating_add(self.head.wire_ns)
            .saturating_add(self.mode.wire_ns)
            .saturating_add(self.sync.wire_ns)
            .saturating_add(self.answer.wire_ns)
            .saturating_add(self.recent.wire_ns)
    }
}

/// A shared, interior-mutable [`NetStats`] a [`crate::PirHttpClient`]
/// writes into.
///
/// A plain `std::sync::Mutex`: every critical section is a handful of
/// integer adds with no `.await` inside, so it can never be held across
/// a suspension point. A poisoned lock is recovered rather than
/// propagated — the guarded value is a bag of counters, with no
/// invariant a panic mid-update could tear.
#[derive(Debug, Default)]
pub struct NetSink {
    stats: Mutex<NetStats>,
}

impl NetSink {
    /// A fresh, all-zero sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one completed (or failed) call.
    ///
    /// `content_length` is the response's declared `content-length`, if
    /// any; `server_ns` is `(compute, handler)` read from the answer
    /// timing headers, each independently optional and only ever
    /// overwritten when present — a call that carries no headers leaves
    /// the previous values alone rather than clearing them.
    pub(crate) fn record(
        &self,
        call: NetCall,
        wire_ns: u64,
        request_bytes: u64,
        response_bytes: u64,
        content_length: Option<u64>,
    ) {
        let mut g = self.lock();
        let s = g.get_mut(call);
        s.calls = s.calls.saturating_add(1);
        s.wire_ns = s.wire_ns.saturating_add(wire_ns);
        s.request_bytes = s.request_bytes.saturating_add(request_bytes);
        s.response_bytes = s.response_bytes.saturating_add(response_bytes);
        s.last_content_length = content_length;
    }

    /// Add `decode_ns` to `call`'s decode total, without counting
    /// another call.
    pub(crate) fn record_decode(&self, call: NetCall, decode_ns: u64) {
        let mut g = self.lock();
        let s = g.get_mut(call);
        s.decode_ns = s.decode_ns.saturating_add(decode_ns);
    }

    /// Record the optional server-side answer timings from a `POST
    /// /answer` response. Each is only overwritten when present.
    pub(crate) fn record_server_timing(&self, compute_ns: Option<u64>, handler_ns: Option<u64>) {
        let mut g = self.lock();
        if compute_ns.is_some() {
            g.server_compute_ns = compute_ns;
        }
        if handler_ns.is_some() {
            g.server_handler_ns = handler_ns;
        }
    }

    /// The current counters, without clearing them.
    #[must_use]
    pub fn snapshot(&self) -> NetStats {
        *self.lock()
    }

    /// The current counters, resetting the sink to zero — the shape a
    /// per-trial measurement wants (reset, run one trial, take).
    #[must_use]
    pub fn take(&self) -> NetStats {
        let mut g = self.lock();
        std::mem::take(&mut *g)
    }

    /// Zero every counter.
    pub fn reset(&self) {
        *self.lock() = NetStats::default();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, NetStats> {
        self.stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_resets_and_snapshot_does_not() {
        let sink = NetSink::new();
        sink.record(NetCall::Answer, 1_000, 42, 84, Some(84));
        sink.record_decode(NetCall::Answer, 7);
        sink.record_server_timing(Some(11), None);

        let snap = sink.snapshot();
        assert_eq!(snap.answer.calls, 1);
        assert_eq!(snap.answer.wire_ns, 1_000);
        assert_eq!(snap.answer.decode_ns, 7);
        assert_eq!(snap.answer.request_bytes, 42);
        assert_eq!(snap.answer.response_bytes, 84);
        assert_eq!(snap.answer.last_content_length, Some(84));
        assert_eq!(snap.server_compute_ns, Some(11));
        assert_eq!(snap.server_handler_ns, None);
        // snapshot must not have cleared anything
        assert_eq!(sink.snapshot(), snap);

        let taken = sink.take();
        assert_eq!(taken, snap);
        assert_eq!(sink.snapshot(), NetStats::default());
    }

    #[test]
    fn absent_server_headers_never_clear_a_previous_value() {
        let sink = NetSink::new();
        sink.record_server_timing(Some(5), Some(9));
        sink.record_server_timing(None, None);
        let s = sink.snapshot();
        assert_eq!(s.server_compute_ns, Some(5));
        assert_eq!(s.server_handler_ns, Some(9));
    }

    #[test]
    fn total_wire_sums_every_endpoint() {
        let sink = NetSink::new();
        sink.record(NetCall::Head, 1, 0, 8, None);
        sink.record(NetCall::Sync, 2, 0, 16, None);
        sink.record(NetCall::Answer, 4, 32, 64, None);
        assert_eq!(sink.snapshot().total_wire_ns(), 7);
    }

    #[test]
    fn get_reaches_every_variant() {
        let sink = NetSink::new();
        for call in [
            NetCall::Setup,
            NetCall::Head,
            NetCall::Mode,
            NetCall::Sync,
            NetCall::Answer,
            NetCall::Recent,
        ] {
            sink.record(call, 3, 0, 0, None);
            assert_eq!(sink.snapshot().get(call).wire_ns, 3, "{call:?}");
        }
    }
}
