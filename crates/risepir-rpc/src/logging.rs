//! One timestamped stderr line per event — the [`crate::logln!`] macro every
//! long-running server path logs through, and the dependency-free UTC
//! formatter behind it.
//!
//! # Why this exists
//!
//! Until 2026-07-30 every log line this binary emitted looked like:
//!
//! ```text
//! risepir-rpc mainnet: reconcile at block 25638690: 8 account(s) exact vs independent provider
//! ```
//!
//! — no timestamp, anywhere. For a service whose whole job is following a
//! chain that is itself a clock, that is defensible right up to the moment
//! anyone asks a question spanning the process boundary, at which point it
//! is not: *when* did the dark-reconcile window end, how long did that
//! `--hard-refresh` pass actually take, did this stall line up with the
//! provider outage the RPC dashboard shows. Reading the live deployment's
//! 70 MB log on 2026-07-29 meant answering all three by correlating block
//! numbers against file mtimes, which works exactly until the file is
//! rotated or the block rate is not what you assumed.
//!
//! Block height is a fine clock for anything *inside* the chain. It is not
//! a clock you can join against anything outside it.
//!
//! # Why not a logging crate
//!
//! `tracing`/`log` + a subscriber is the obvious answer and is the wrong
//! trade here, for the reason ADR-0039 gives for hand-rolling the
//! Prometheus exposition rather than pulling in a metrics crate: this
//! binary ingests attacker-controlled blobs, every dependency is audited
//! (`deny.toml`, ADR-0022), and the requirement is *one line, one
//! timestamp, one format*. That is thirty lines of arithmetic, below,
//! entirely testable, with no allocator behaviour or filtering machinery
//! to reason about. If structured/levelled logging is ever actually
//! needed, that is a real decision deserving a real ADR — not something
//! to acquire as a side effect of wanting a timestamp.
//!
//! # The atomicity property, which is load-bearing
//!
//! [`crate::logln!`] formats the timestamp *and* the message into a **single**
//! `eprintln!` call, never two. `eprintln!` locks stderr for the duration
//! of one call, so one call is one atomic write. Emitting the timestamp
//! and the message separately would let `hard_refresh`'s
//! `CONCURRENT_ADDRESS_CHECKS` concurrent tasks interleave a timestamp
//! from one task with a message from another — producing lines that are
//! individually well-formed and collectively lies. This is the reason the
//! macro body is one expression and must stay that way.

use std::time::{SystemTime, UNIX_EPOCH};

/// Emit one timestamped line to stderr: an RFC 3339 UTC instant, a space,
/// then the caller's message formatted exactly as [`eprintln!`] would.
///
/// The message text is deliberately unchanged from what these call sites
/// already passed to `eprintln!` — including their own
/// `risepir-rpc mainnet: ` prefixes — so every `grep` in `docs/deploy.md`,
/// every runbook habit, and every recorded log transcript keeps matching.
/// The timestamp is added in front, not woven in:
///
/// ```text
/// 2026-07-30T04:12:33Z risepir-rpc mainnet: state saved (autosave): block 25637839, 24.18 GB in 175.6s
/// ```
///
/// Note that this shifts lines off column 1, so a `^`-anchored pattern
/// against a log line needs to lose its anchor. (Checked when this landed:
/// the repo's own docs and workflows had none.)
///
/// `#[macro_export]` rather than `pub(crate) use`: this package's binary
/// target (`src/main.rs`) is a *separate crate* that depends on this lib,
/// and its shutdown path — the `state saved; exiting` line an operator
/// waits for — needs a timestamp as much as anything in the follow loop
/// does. A crate-private macro could not reach it.
#[macro_export]
macro_rules! logln {
    ($($arg:tt)*) => {
        // ONE eprintln!, never two — see the module docs' atomicity note.
        eprintln!("{} {}", $crate::logging::utc_now_rfc3339(), format_args!($($arg)*))
    };
}

/// The current instant as `YYYY-MM-DDTHH:MM:SSZ` (RFC 3339, UTC, second
/// resolution).
///
/// A clock before the epoch — only reachable from a grossly misconfigured
/// host — formats as the epoch itself rather than panicking or returning
/// an error nobody could act on. A log call is never the right place to
/// fail a running server.
pub fn utc_now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_unix_secs(secs)
}

/// [`utc_now_rfc3339`]'s formatting half, split out so it is testable
/// against known instants with no clock involved.
fn format_unix_secs(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3_600,
        (tod % 3_600) / 60,
        tod % 60
    )
}

/// Days since the Unix epoch → `(year, month, day)` in the proleptic
/// Gregorian calendar.
///
/// Howard Hinnant's `civil_from_days`, the standard branch-free algorithm
/// (public domain, and what `chrono`/`time` compute in essence too). It
/// shifts the epoch to 0000-03-01 so that leap day lands at the *end* of
/// the year, which is what removes the month-length special-casing: the
/// `(5*doy + 2)/153` step is an exact integer fit over the March-February
/// month-length cycle. Correct for every date this program can encounter;
/// the tests below pin the cases that actually catch an off-by-one —
/// epoch, both sides of a leap day, and a century non-leap year.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_itself() {
        assert_eq!(format_unix_secs(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn time_of_day_is_split_correctly() {
        assert_eq!(format_unix_secs(1), "1970-01-01T00:00:01Z");
        assert_eq!(format_unix_secs(59), "1970-01-01T00:00:59Z");
        assert_eq!(format_unix_secs(60), "1970-01-01T00:01:00Z");
        assert_eq!(format_unix_secs(3_599), "1970-01-01T00:59:59Z");
        assert_eq!(format_unix_secs(3_600), "1970-01-01T01:00:00Z");
        assert_eq!(format_unix_secs(86_399), "1970-01-01T23:59:59Z");
        assert_eq!(format_unix_secs(86_400), "1970-01-02T00:00:00Z");
    }

    /// Real instants this deployment actually produced, so the test is
    /// pinned to something checkable rather than to the algorithm's own
    /// output. Both come from the live `/metrics` exposition read on
    /// 2026-07-29 (`risepir_state_save_last_success_timestamp_seconds` and
    /// `risepir_reconcile_last_success_timestamp_seconds`), and every
    /// expectation in this module was cross-checked against Python's
    /// `datetime` rather than computed by hand — which is what caught two
    /// wrong expectations here on the first run.
    #[test]
    fn instants_from_the_live_deployment() {
        assert_eq!(format_unix_secs(1_785_321_529), "2026-07-29T10:38:49Z");
        assert_eq!(format_unix_secs(1_785_331_434), "2026-07-29T13:23:54Z");
    }

    /// Leap-day arithmetic is where a hand-rolled civil-date conversion
    /// goes wrong, so pin both sides of one.
    #[test]
    fn leap_day_and_its_neighbours() {
        // 2024 is a leap year: Feb 29 exists.
        assert_eq!(format_unix_secs(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(format_unix_secs(1_709_164_800 - 86_400), "2024-02-28T00:00:00Z");
        assert_eq!(format_unix_secs(1_709_164_800 + 86_400), "2024-03-01T00:00:00Z");
    }

    /// 2000 was a leap year (divisible by 400) but 1900 and 2100 are not.
    /// The `doe/36524` and `doe/146096` terms are what get this right, and
    /// dropping either still passes every non-century test above.
    #[test]
    fn century_leap_year_rules() {
        assert_eq!(format_unix_secs(951_782_400), "2000-02-29T00:00:00Z");
        // 2100-02-28 + 1 day must be March, not a nonexistent Feb 29.
        assert_eq!(format_unix_secs(4_107_456_000), "2100-02-28T00:00:00Z");
        assert_eq!(format_unix_secs(4_107_456_000 + 86_400), "2100-03-01T00:00:00Z");
    }

    /// Every rendered line must be exactly 20 characters, so the messages
    /// after it stay column-aligned in a terminal and in the log file.
    #[test]
    fn the_stamp_is_fixed_width() {
        for secs in [0u64, 1, 1_785_321_529, 4_107_456_000] {
            assert_eq!(format_unix_secs(secs).len(), 20, "{secs}");
        }
    }

    /// The macro must put the stamp first and leave the caller's text —
    /// including its own `risepir-rpc mainnet: ` prefix — byte-identical,
    /// because the docs and runbooks grep for that text.
    #[test]
    fn the_macro_preserves_the_message_verbatim() {
        let stamp = utc_now_rfc3339();
        let msg = format!("{} {}", stamp, format_args!("risepir-rpc mainnet: block {} applied", 25_638_894u64));
        assert!(msg.starts_with(&stamp));
        assert!(msg.ends_with("risepir-rpc mainnet: block 25638894 applied"));
        assert_eq!(msg.as_bytes()[stamp.len()], b' ');
    }

    #[test]
    fn now_is_plausible_and_well_formed() {
        let s = utc_now_rfc3339();
        assert_eq!(s.len(), 20, "{s}");
        assert!(s.ends_with('Z'), "{s}");
        // Anything running this is well past 2020 and well before 2200.
        let year: i64 = s[..4].parse().expect("year parses");
        assert!((2020..2200).contains(&year), "{s}");
    }
}
