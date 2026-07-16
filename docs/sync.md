# Continuous sync — keeping the database current

After the initial snapshot ([`data-acquisition.md`](data-acquisition.md)), the server
tracks the chain block-by-block. This is the part that runs forever and must never
drift into a wrong answer.

## The one design decision that matters

> **Get the change *set* from a block trace (or ExEx); treat `eth_getBalance` as the
> authoritative *value* source.** Never derive a balance by summing transaction
> `value` fields — that silently misses gas, internal transfers, `SELFDESTRUCT`,
> coinbase, and **withdrawals** (verified: `prestateTracer` alone misses 100% of
> withdrawals — they are block-level, after all transactions, EIP-4895).

Everything downstream is the `BlockUpdate → apply_block` interface already built and
tested; the sync loop only produces one `BlockUpdate` per block.

## The loop

```
every few seconds:
    f = eth_getBlockByNumber("finalized").number
    for N in (last_applied + 1 ..= f):
        changes = feed.changes(N)                       # Vec<(keccak(addr), new_balance)>
        delta   = server.apply_block({block: N, changes})   # 1 epoch, 1 hint patch, 1 delta
        delta_ring.push(delta)                          # clients stream this; CDN-cacheable
    last_applied = f
```

**Cadence — follow `finalized` (ADR-0007).** `finalized` advances in ~32-block bursts
roughly every 6.4 min (an epoch finalizing), ~13 min behind head. So updates arrive in
bursts, not smoothly every 12 s, and clients see a burst of deltas together. This
deletes the entire reorg bug class (finalized is final) for ~13 min of labelled
staleness — the user confirmed near-real-time is enough. (The alternative, follow
`latest` for a true 12-s cadence at the head, is possible because our deltas are
additive — a reorg applies the *negated* delta of orphaned blocks — but it is more
failure surface and deferred.)

## Producing `changes(N)` — the feed

For block `N`, from a public RPC (dRPC is keyless and serves both `debug_trace…` and
archive `eth_getBalance`):

1. **Change set** — who moved:
   `keys(prestateTracer(N).pre)` (every account execution touched, incl. the fee
   recipient/coinbase) **∪** `block(N).withdrawals[].address` (withdrawals are *not* in
   any trace).
2. **New values**, computed without a per-account RPC on the hot path:
   - tx-touched account → the `post.balance` of the **last** transaction that changed
     it. `post` is **sparse**: an absent field means *unchanged, not zero*
     (`new = post[a].balance ?? pre[a].balance`).
   - withdrawal recipient → `store[addr] + Σ amount` (gwei ×10⁹). Our store *is* the
     authoritative prior value (ADR-0016), read by key at the 64-bit-effective
     fingerprint. No RPC needed.
3. `apply_block(N, changes)`.

Map each change to a store op (ADR-0015): `0→nonzero` = **insert**, `nonzero→nonzero`
= **update**, `nonzero→0` = **delete**.

**If a node is ever self-hosted**, Reth **ExEx** replaces all of the above:
`ChainCommitted` hands you `BundleState` with `original_info.balance → info.balance`
per changed account, **withdrawals included natively**, in-process, zero RPC. Make
`risepir-feed` an interface with `mock`, `rpc`, and `exex` implementations; develop
against `rpc`, drop in `exex` if a node exists.

## Never returning a wrong answer

- **Continuous reconciliation.** Every K blocks, sample R tracked accounts and assert
  `store[addr] == eth_getBalance(addr, our_head)` against the reference RPC. Any
  mismatch ⇒ the feed drifted ⇒ stop serving, re-bootstrap. Hard-refresh the ~32k
  withdrawal addresses on a slow cadence (they are most exposed to accumulated error).
- **Stall handling.** RPC hiccup or chain stall ⇒ keep serving the last good block,
  **labelled with its number**, report stalled, never guess a newer state.
- **Catch-up after downtime.** On restart, replay `last_applied+1 .. finalized` in
  order — each block is one epoch. Clients past the delta-ring window (300 blocks ≈
  1 hr) are told to resync from a fresh setup bundle, never served a wrong answer.

## Bootstrap seam

The snapshot is at some `B_snap` (possibly a day behind). Bootstrap = snapshot at
`B_snap` → replay `B_snap+1 .. finalized` through the feed → steady-state follow. For
the one-time gap, the Xatu balance-diffs parquet is ideal (bulk, cheap over a known
range); switch to the live RPC feed for the tail, with a `getBalance` reconciliation at
the join to prove the handoff is exact.

## Stage 0 exercises all of this without a chain

The mock feed (`risepir-feed` `mock`) emits ~300 synthetic changes every 12 s
(realistic wei-scale balances), driving the same `apply_block`, delete-on-zero, delta
ring, catch-up, and reconciliation paths — so `cast balance` against the mock is the
gate before any real chain data.
