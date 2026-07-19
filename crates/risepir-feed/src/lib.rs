#![forbid(unsafe_code)]
#![warn(missing_docs)]
//! Chain-follow feed for the private `eth_getBalance` service: the source of
//! the [`BlockUpdate`] stream that drives `risepir_server::RisePirServer::apply_block`
//! (see `docs/plan.md` §3.1–3.2, `docs/sync.md`).
//!
//! # The interface
//!
//! [`Feed`] is the one seam every chain producer sits behind: the Stage-0
//! [`MockFeed`] here, and — later — an `rpc` feed (dRPC `prestateTracer ⊕
//! block.withdrawals[]`) and an `exex` feed (Reth `BundleState`), per
//! `docs/sync.md`. Everything downstream of a `BlockUpdate` is generic PIR
//! plumbing; everything upstream is Ethereum.
//!
//! # The mock (Stage 0.2)
//!
//! [`MockFeed`] is a **deterministic, seeded** generator: the same
//! [`MockConfig`] always produces the same genesis population and the same
//! per-block change stream, so conformance runs are reproducible. It models
//! a live account set — realistic **wei-scale** balances (ADR-0005: the
//! delta codec's compaction win depends on realistic balances, not small
//! integers), a mix of **inserts** (new accounts), **updates**
//! (transaction-sized perturbations), and **deletes** (a balance going to
//! `0`, which per ADR-0015 is a *removal*, not a stored zero).
//!
//! # The invariant this crate serves
//!
//! The mock's [`MockFeed::balance_of`] is the **ground truth** the
//! conformance harness (Stage 0.5) diffs the private client's answers
//! against. It must be *exact*: it reflects precisely the change stream the
//! mock emitted, in order, so that "the client's answer differs from
//! `balance_of`" is always a real bug, never an artefact of the oracle.

use std::collections::HashMap;

use risepir_proto::{AddressHash, Balance, BlockUpdate};

// ─── The Feed interface ──────────────────────────────────────────────────

/// A source of per-block [`BlockUpdate`]s.
///
/// Implementations normalise whatever they read (synthetic generator, RPC
/// trace, ExEx `BundleState`) into the one shape the server consumes. A
/// change of `(address_hash, 0)` means the account's balance went to zero
/// and must be **deleted** (ADR-0015); any other balance is an insert (first
/// appearance) or an update.
pub trait Feed {
    /// The next block's update, or `Ok(None)` if no new block is available
    /// yet. [`MockFeed`] always returns `Some` (it generates on demand); a
    /// future `rpc` feed returns `None` when it has caught up to `finalized`.
    fn next_block(&mut self) -> Result<Option<BlockUpdate>, FeedError>;

    /// Highest block number produced so far (`0` before the first block).
    fn head(&self) -> u64;
}

/// Errors a [`Feed`] can report.
///
/// [`MockFeed`] never errors — it is a pure in-memory generator — but the
/// type exists so the future `rpc`/`exex` feeds (network failures, trace
/// gaps, reorg detection) share one error surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedError {
    /// A feed-internal failure (network, trace, or decode) with a message.
    Internal(String),
}

impl std::fmt::Display for FeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Internal(msg) => write!(f, "feed error: {msg}"),
        }
    }
}

impl std::error::Error for FeedError {}

// ─── Deterministic PRNG (dependency-free) ────────────────────────────────

/// SplitMix64 — a tiny, fast, dependency-free PRNG. Used both to seed the
/// mock's stream and (via [`MockFeed::address_for`]) to derive hash-keyed
/// addresses, so the whole mock is reproducible from one `u64` seed.
#[derive(Clone)]
struct SplitMix64(u64);

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Domain-separation constant for [`MockFeed::address_for`] so a mock
/// address for id `i` is not trivially `i` in the low bytes (real addresses
/// are `keccak256` outputs — uniform 32 bytes — and the SCF hashes the key
/// anyway, but a hash-shaped address keeps the mock honest).
const ADDR_DOMAIN: u64 = 0x6D6F_636B_5F61_6464; // "mock_add"

/// A wei-scale balance, log-uniform in magnitude over roughly `[2^39, 2^89)`
/// wei (~5.5e11 .. ~6e26 wei, i.e. ~5e-7 .. ~6e8 ETH — dust to whale) with
/// random low bits. `2^89 < 2^96`, so every value fits `balance_bits = 96`
/// with headroom and never overflows the codec. The random low bits are the
/// point: a realistic balance flips many plaintext cells when it changes,
/// which is exactly the regime ADR-0005 says the compact delta codec needs.
fn random_balance(rng: &mut SplitMix64) -> Balance {
    let bits = 40 + (rng.next_u64() % 50); // [40, 89]
    let lo = u128::from(rng.next_u64());
    let hi = u128::from(rng.next_u64());
    let raw = (hi << 64) | lo;
    let mask = (1u128 << bits) - 1;
    ((raw & mask) | (1u128 << (bits - 1))).max(1)
}

/// Perturb `old` by a transaction-sized amount, log-uniform in magnitude
/// over roughly `[2^42, 2^63)` wei (~4e12 .. ~9e18 wei), added or subtracted,
/// clamped to `[1, 2^96 - 1]`. Never returns `0` — a balance reaching zero
/// is modelled as a *delete*, not an update.
fn perturb(rng: &mut SplitMix64, old: Balance) -> Balance {
    let dbits = 43 + (rng.next_u64() % 21); // [43, 63]
    let lo = u128::from(rng.next_u64());
    let dmask = (1u128 << dbits) - 1;
    let amount = ((lo & dmask) | (1u128 << (dbits - 1))).max(1);
    let cap = (1u128 << 96) - 1;
    let up = (rng.next_u64() & 1) == 0;
    let new = if up {
        old.saturating_add(amount).min(cap)
    } else {
        old.saturating_sub(amount)
    };
    new.max(1)
}

// ─── Mock feed ───────────────────────────────────────────────────────────

/// Configuration for a [`MockFeed`].
///
/// `changes_per_block` must be `>= inserts_per_block + deletes_per_block`;
/// the remainder are updates. Keeping `inserts_per_block == deletes_per_block`
/// makes the live-set size **invariant** across blocks — convenient for
/// sizing a store once and never courting `TableFull`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockConfig {
    /// Seed for the deterministic stream. Any value; the same seed always
    /// reproduces the same genesis and the same block stream.
    pub seed: u64,
    /// Number of nonzero-balance accounts at genesis (block 0).
    pub num_genesis_keys: u64,
    /// Total balance changes emitted per block.
    pub changes_per_block: usize,
    /// How many of `changes_per_block` are brand-new accounts (inserts).
    pub inserts_per_block: usize,
    /// How many of `changes_per_block` are accounts going to zero (deletes).
    pub deletes_per_block: usize,
}

impl Default for MockConfig {
    /// The Stage-0.2 target: ~1M accounts, ~300 changes/block (≈240 updates,
    /// 30 inserts, 30 deletes — the mainnet-ish operating point, with a
    /// live-set-invariant insert/delete balance).
    fn default() -> Self {
        Self {
            seed: 0xB16B_00B5_5EED_1234,
            num_genesis_keys: 1_000_000,
            changes_per_block: 300,
            inserts_per_block: 30,
            deletes_per_block: 30,
        }
    }
}

/// A deterministic, seeded mock chain feed with exact ground truth.
///
/// Maintains an in-memory model of the live account set: `live` (the keys,
/// in a `Vec` so random selection is by PRNG index — never by nondeterministic
/// `HashMap` iteration) and `balances` (the authoritative `address -> balance`
/// map, queried by [`Self::balance_of`]). Every emitted change is applied to
/// this model in the same order it is emitted, so [`Self::balance_of`] is an
/// exact oracle for what `apply_block` will have done to the server.
pub struct MockFeed {
    cfg: MockConfig,
    rng: SplitMix64,
    /// Live keys, in insertion order modulo swap-removes — a deterministic
    /// index space for random selection.
    live: Vec<AddressHash>,
    /// Authoritative current balance of every live key. `live.len() ==
    /// balances.len()` always; a key is in exactly one iff in the other.
    balances: HashMap<AddressHash, Balance>,
    block: u64,
    /// Next fresh account id for an insert (genesis used `0..num_genesis_keys`).
    next_id: u64,
}

impl MockFeed {
    /// Build a mock at block 0 with `cfg.num_genesis_keys` nonzero-balance
    /// accounts.
    ///
    /// # Panics
    ///
    /// If `cfg.changes_per_block < cfg.inserts_per_block + cfg.deletes_per_block`
    /// — a configuration bug (there would be a negative number of updates),
    /// not a runtime condition.
    pub fn new(cfg: MockConfig) -> Self {
        assert!(
            cfg.changes_per_block >= cfg.inserts_per_block + cfg.deletes_per_block,
            "changes_per_block ({}) must be >= inserts_per_block ({}) + deletes_per_block ({})",
            cfg.changes_per_block,
            cfg.inserts_per_block,
            cfg.deletes_per_block
        );

        let mut rng = SplitMix64::new(cfg.seed);
        let n = cfg.num_genesis_keys;
        let mut live = Vec::with_capacity(n as usize);
        let mut balances = HashMap::with_capacity(n as usize);
        for id in 0..n {
            let addr = Self::address_for(id);
            let bal = random_balance(&mut rng);
            live.push(addr);
            balances.insert(addr, bal);
        }

        Self {
            cfg,
            rng,
            live,
            balances,
            block: 0,
            next_id: n,
        }
    }

    /// Deterministic, distinct 32-byte address for account `id`.
    ///
    /// Filled from four SplitMix64 draws seeded by `id`, so distinct ids give
    /// distinct, uniform-looking (hash-keyed, like real `keccak256(address)`)
    /// addresses. Exposed so a conformance harness can construct addresses in
    /// — or deliberately *outside* — the mock's generated id space (a
    /// "never-existed" account is any address the mock never generates, e.g.
    /// `address_for(id)` for an `id` beyond the run, or an unrelated random
    /// 32 bytes).
    pub fn address_for(id: u64) -> AddressHash {
        let mut sm = SplitMix64::new(id ^ ADDR_DOMAIN);
        let mut a = [0u8; 32];
        for chunk in a.chunks_mut(8) {
            chunk.copy_from_slice(&sm.next_u64().to_le_bytes());
        }
        a
    }

    /// The current `(address, balance)` set, in deterministic order.
    ///
    /// At block 0 this is exactly the genesis population — use it to build
    /// the initial store. Called later, it is the current live snapshot (all
    /// balances nonzero), which is equally valid as "rebuild a store from the
    /// current truth".
    pub fn snapshot(&self) -> Vec<(AddressHash, Balance)> {
        self.live.iter().map(|a| (*a, self.balances[a])).collect()
    }

    /// Ground-truth balance of `addr`: its current balance, or `0` if the
    /// account is absent (never existed, or deleted). This is the exact
    /// oracle the conformance harness diffs private answers against.
    pub fn balance_of(&self, addr: &AddressHash) -> Balance {
        self.balances.get(addr).copied().unwrap_or(0)
    }

    /// The current live key set (all with nonzero balance), for sampling.
    pub fn live_keys(&self) -> &[AddressHash] {
        &self.live
    }
}

impl Feed for MockFeed {
    fn next_block(&mut self) -> Result<Option<BlockUpdate>, FeedError> {
        self.block += 1;
        let mut changes: Vec<(AddressHash, Balance)> = Vec::with_capacity(self.cfg.changes_per_block);

        // Deletes first: swap-remove random live keys (a deleted key leaves
        // `live`, so nothing later this block can re-touch it). Emit
        // `(addr, 0)` — the delete signal (ADR-0015).
        let mut deletes = 0usize;
        while deletes < self.cfg.deletes_per_block && !self.live.is_empty() {
            let i = (self.rng.next_u64() % self.live.len() as u64) as usize;
            let a = self.live.swap_remove(i);
            self.balances.remove(&a);
            changes.push((a, 0));
            deletes += 1;
        }

        // Inserts: fresh ids, nonzero wei-scale balances.
        for _ in 0..self.cfg.inserts_per_block {
            let id = self.next_id;
            self.next_id += 1;
            let a = Self::address_for(id);
            let bal = random_balance(&mut self.rng);
            self.live.push(a);
            self.balances.insert(a, bal);
            changes.push((a, bal));
        }

        // Updates fill the rest: perturb random live keys. (Selection is with
        // replacement — a key touched twice in one block is fine; `apply_block`
        // and this model both take the last write, so ground truth stays exact.)
        let updates = self
            .cfg
            .changes_per_block
            .saturating_sub(deletes)
            .saturating_sub(self.cfg.inserts_per_block);
        let mut done = 0usize;
        while done < updates && !self.live.is_empty() {
            let i = (self.rng.next_u64() % self.live.len() as u64) as usize;
            let a = self.live[i];
            let old = self.balances[&a];
            let new = perturb(&mut self.rng, old);
            self.balances.insert(a, new);
            changes.push((a, new));
            done += 1;
        }

        Ok(Some(BlockUpdate {
            block: self.block,
            changes,
            credits: vec![],
        }))
    }

    fn head(&self) -> u64 {
        self.block
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use risepir_proto::ValueCodec;
    use std::collections::HashSet;

    fn small_cfg(seed: u64) -> MockConfig {
        MockConfig {
            seed,
            num_genesis_keys: 2_000,
            changes_per_block: 60,
            inserts_per_block: 10,
            deletes_per_block: 10,
        }
    }

    #[test]
    fn genesis_population_is_nonzero_and_sized() {
        let cfg = small_cfg(1);
        let feed = MockFeed::new(cfg.clone());
        assert_eq!(feed.live_keys().len(), cfg.num_genesis_keys as usize);
        assert_eq!(feed.snapshot().len(), cfg.num_genesis_keys as usize);
        for (addr, bal) in feed.snapshot() {
            assert_ne!(bal, 0, "genesis balances must be nonzero (ADR-0015)");
            assert_eq!(feed.balance_of(&addr), bal);
        }
    }

    /// Same seed ⇒ byte-identical genesis and byte-identical block stream;
    /// different seed ⇒ a different stream.
    #[test]
    fn deterministic_and_seed_sensitive() {
        let mut a = MockFeed::new(small_cfg(42));
        let mut b = MockFeed::new(small_cfg(42));
        assert_eq!(a.snapshot(), b.snapshot(), "same seed ⇒ same genesis");
        for _ in 0..5 {
            assert_eq!(
                a.next_block().unwrap(),
                b.next_block().unwrap(),
                "same seed ⇒ same block stream"
            );
        }

        let c = MockFeed::new(small_cfg(43));
        assert_ne!(a.snapshot(), c.snapshot(), "different seed ⇒ different genesis");
    }

    /// Balances are genuinely wei-scale and span a wide magnitude range, and
    /// an update flips many plaintext-value bytes — the ADR-0005 regime the
    /// compact delta codec depends on (a small-int ±1 stream would fail this).
    #[test]
    fn balances_are_realistic_wei_scale() {
        // Pure-update config so we can measure old→new transitions directly.
        let cfg = MockConfig {
            seed: 7,
            num_genesis_keys: 300,
            changes_per_block: 100,
            inserts_per_block: 0,
            deletes_per_block: 0,
        };
        let mut feed = MockFeed::new(cfg);

        let genesis = feed.snapshot();
        let min = genesis.iter().map(|(_, b)| *b).min().unwrap();
        let max = genesis.iter().map(|(_, b)| *b).max().unwrap();
        assert!(min >= (1u128 << 37), "balances must be wei-scale, got min {min}");
        assert!(max / min >= (1u128 << 15), "balances must span a wide magnitude range");

        // Snapshot truth BEFORE the block, then measure each update's byte
        // delta on the encoded value.
        let before: HashMap<AddressHash, Balance> = genesis.into_iter().collect();
        let codec = ValueCodec {
            key_tag_bits: 32,
            balance_bits: 96,
            checksum_bits: 16,
        };
        let upd = feed.next_block().unwrap().unwrap();
        let mut total_changed = 0usize;
        let mut n = 0usize;
        for (addr, new) in &upd.changes {
            let old = before[addr]; // pure-update config: every change is an update of a genesis key
            let eo = codec.encode(addr, old).unwrap();
            let en = codec.encode(addr, *new).unwrap();
            total_changed += eo.iter().zip(&en).filter(|(x, y)| x != y).count();
            n += 1;
        }
        let mean = total_changed as f64 / n as f64;
        assert!(
            mean >= 5.0,
            "updates must flip many value bytes (ADR-0005 realistic regime), mean changed bytes = {mean}"
        );
    }

    /// Inserts, updates, and deletes all occur; a deleted key reads back as
    /// `0` and leaves the live set; and `balance_of` matches an independent
    /// replay of the emitted change stream (the exact-oracle invariant).
    #[test]
    fn mix_occurs_and_ground_truth_is_exact() {
        let cfg = small_cfg(99);
        let mut feed = MockFeed::new(cfg.clone());

        // Independent replica of the ground truth, built only from the
        // emitted stream — must agree with `balance_of` at the end.
        let mut replica: HashMap<AddressHash, Balance> = feed.snapshot().into_iter().collect();
        let genesis_keys: HashSet<AddressHash> = replica.keys().copied().collect();

        let start_live = feed.live_keys().len();
        let mut known: HashSet<AddressHash> = genesis_keys.clone();
        let (mut inserts, mut updates, mut deletes) = (0u64, 0u64, 0u64);
        let mut a_deleted: Option<AddressHash> = None;

        for _ in 0..20 {
            let upd = feed.next_block().unwrap().unwrap();
            for (addr, bal) in &upd.changes {
                if *bal == 0 {
                    deletes += 1;
                    a_deleted = Some(*addr);
                    replica.remove(addr);
                } else if known.contains(addr) {
                    updates += 1;
                    replica.insert(*addr, *bal);
                } else {
                    inserts += 1;
                    known.insert(*addr);
                    replica.insert(*addr, *bal);
                }
            }
        }

        assert!(inserts > 0 && updates > 0 && deletes > 0, "all three op kinds must occur: i={inserts} u={updates} d={deletes}");
        assert_eq!(
            feed.live_keys().len(),
            start_live,
            "inserts == deletes ⇒ live-set size is invariant"
        );

        // A deleted key is gone: balance 0 and absent from the live set.
        let d = a_deleted.unwrap();
        assert_eq!(feed.balance_of(&d), 0, "a deleted key must read back as 0");
        assert!(!feed.live_keys().contains(&d), "a deleted key must leave the live set");

        // The exact-oracle invariant: the independently-replayed truth equals
        // `balance_of` for every key either side knows about.
        for addr in replica.keys().chain(genesis_keys.iter()) {
            let expected = replica.get(addr).copied().unwrap_or(0);
            assert_eq!(feed.balance_of(addr), expected, "balance_of must exactly reflect the emitted stream");
        }
    }

    #[test]
    #[should_panic(expected = "changes_per_block")]
    fn rejects_inconsistent_config() {
        let _ = MockFeed::new(MockConfig {
            seed: 0,
            num_genesis_keys: 10,
            changes_per_block: 5,
            inserts_per_block: 4,
            deletes_per_block: 4, // 4 + 4 > 5
        });
    }
}
