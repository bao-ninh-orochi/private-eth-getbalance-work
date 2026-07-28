//! `--snapshot-rewind <N>` (ADR-0040): bootstrap mitigation for
//! `docs/deploy.md` §2.1's finding that the BigQuery balances export is
//! measurably **not** exact at its own declared block — treat the
//! snapshot as exact `N` blocks earlier than `--snapshot-block` instead
//! of exactly at it, so the ordinary catch-up replay re-derives, from the
//! prestate tracer's *absolute* post-state, every account the extra `N`
//! blocks touch — overwriting wrong rows and inserting missing ones,
//! before the replay ever reaches the true declared block.
//!
//! # This is arithmetically just a lower `--snapshot-block`
//!
//! Nothing here is a new mechanism: `RisePirServer::new`'s genesis
//! argument already accepts any block number, and the existing catch-up
//! replay (`crate::mainnet`'s follow loop, run unmodified from
//! `effective_genesis + 1` up to the real finalized head) does the entire
//! job. What is new is that this is **on by default**, with a *measured*
//! value (see below) and a stated rationale, rather than being a hint an
//! operator may or may not take (`docs/deploy.md` §2.1 already gestured at
//! "getting `snapshot_block` slightly too low is safe").
//!
//! # What this narrows, and what it does not fix
//!
//! The measurement behind the default (`docs/adr/README.md` ADR-0040)
//! found the export's error rate is **highest for accounts last touched
//! closest to the boundary** and decays — but does not vanish — with
//! distance: `depth <= 1` blocks measured 27.99% wrong, `depth
//! (1000,2000]` measured 5.47% wrong, and a population-wide random sample
//! still measured 0.33% wrong (Wilson 95% CI [0.09%, 1.21%]) *outside* any
//! bounded recent window at all. So `--snapshot-rewind` removes the
//! **densest** part of the error — it is not a fix for the whole
//! population, and the ADR is explicit that it must not be sold as one.
//!
//! It also does not fix one entire class by construction: EIP-4895
//! withdrawal credits are **relative** amounts
//! (`risepir_proto::BlockUpdate::credits`), resolved against the store's
//! *prior* value at apply time. Inside the rewind window, that prior is
//! the possibly-wrong snapshot value — a relative credit applied on top of
//! a wrong base is still wrong, and unlike an absolute `changes` entry
//! (which simply *replaces* whatever was there), nothing about replaying
//! more blocks self-heals that. `--hard-refresh` (`crate::hard_refresh`)
//! is the remedy for those addresses: an absolute, quorum-verified
//! re-write that does not care what the store held before.

/// Computes the effective genesis block [`crate::mainnet::spawn`] passes
/// to `RisePirServer::new` for a `--snapshot-rewind <N>` bootstrap:
/// `snapshot_block - N`, so the replay treats the snapshot as exact that
/// much earlier and re-derives the rewind window from the chain itself.
///
/// `rewind == 0` is the documented disable switch and is handled before
/// any arithmetic: it always returns `snapshot_block` unchanged,
/// regardless of `snapshot_block`'s own value (in particular, `rewind ==
/// snapshot_block == 0` is not an error — "disabled" never is).
///
/// # Errors
///
/// A message naming the problem if `rewind >= snapshot_block` (and
/// `rewind != 0`) — rewinding to block `0` or past it is refused rather
/// than silently saturating, because that would silently discard the
/// operator's intent (a bounded safety margin) in favor of replaying the
/// entire chain from genesis, which is almost certainly not what was
/// asked for and would make a "quick default" bootstrap take days.
pub fn rewound_genesis(snapshot_block: u64, rewind: u64) -> Result<u64, String> {
    if rewind == 0 {
        return Ok(snapshot_block);
    }
    if rewind >= snapshot_block {
        return Err(format!(
            "--snapshot-rewind {rewind} >= --snapshot-block {snapshot_block} (would rewind to block 0 \
             or below, i.e. past genesis) — pick a smaller --snapshot-rewind, or 0 to disable it"
        ));
    }
    Ok(snapshot_block.saturating_sub(rewind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_disables_regardless_of_snapshot_block() {
        assert_eq!(rewound_genesis(25_613_233, 0), Ok(25_613_233));
        // The documented disable switch must win even in the degenerate
        // snapshot_block == 0 case — "disabled" is never an error.
        assert_eq!(rewound_genesis(0, 0), Ok(0));
    }

    #[test]
    fn default_2000_subtracts_plainly() {
        assert_eq!(rewound_genesis(25_613_233, 2_000), Ok(25_611_233));
    }

    #[test]
    fn rewind_equal_to_snapshot_block_hard_fails() {
        let err = rewound_genesis(2_000, 2_000).unwrap_err();
        assert!(err.contains("2000"), "{err}");
        assert!(err.contains("past genesis"), "{err}");
    }

    #[test]
    fn rewind_exceeding_snapshot_block_hard_fails() {
        assert!(rewound_genesis(2_000, 2_001).is_err());
        assert!(rewound_genesis(100, u64::MAX).is_err());
    }

    #[test]
    fn rewind_one_less_than_snapshot_block_is_the_boundary_ok_case() {
        // snapshot_block - rewind == 1: valid (block 0 itself is refused
        // only when rewind >= snapshot_block, i.e. result <= 0).
        assert_eq!(rewound_genesis(2_000, 1_999), Ok(1));
    }

    #[test]
    fn never_panics_or_underflows_across_a_wide_sweep() {
        // saturating_sub as a defensive belt: even if the `>=` guard above
        // were ever loosened by mistake, this must not panic.
        for snapshot_block in [0u64, 1, 2, 100, u64::MAX] {
            for rewind in [0u64, 1, 100, u64::MAX] {
                let _ = rewound_genesis(snapshot_block, rewind);
            }
        }
    }
}
