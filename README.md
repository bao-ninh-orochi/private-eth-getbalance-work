# private-ETH-getBalance

A proof-of-concept **private Ethereum RPC**: answer `eth_getBalance` without the
server learning which account was asked about, using **RisePIR** (the LWE
instantiation of Incremental Keyword PIR) over a Segmented Cuckoo Filter.

The point is not speed or hint size — those are inherited from SimplePIR. The point is
that classical PIR-with-preprocessing must re-run a minutes-long setup on every state
change, which at Ethereum scale is impossible every 12 s; RisePIR folds a block's ~300
balance changes into a **~5 ms** incremental hint patch with no dependence on database
size. See [`docs/plan.md`](docs/plan.md) §7.

## Status

Four crates, **81 tests passing**, all commits signed:

| crate | role | state |
|---|---|---|
| `risepir-proto` | geometry, `BlockUpdate`/`BlockDelta`, value + delta codecs | built (51 tests) |
| `risepir-server` | batched per-block server over the PIR primitives + delta ring | built (21 tests) |
| `risepir-client` | the response-rewind client | built (9 tests), interim value encoding |
| `risepir-feed` | chain ingest (mock / rpc / exex) | **todo** |

Next: the value-encoding upgrade (64-bit-effective fingerprint), the mock feed, HTTP
transport, JSON-RPC `:8545`, and the conformance harness. See
[`docs/HANDOFF.md`](docs/HANDOFF.md).

## Docs

| doc | what |
|---|---|
| [`docs/plan.md`](docs/plan.md) | **authoritative current spec** — read first |
| [`docs/adr/README.md`](docs/adr/README.md) | every decision, with rationale + rejected alternatives |
| [`docs/sync.md`](docs/sync.md) | keeping the DB current from the chain |
| [`docs/data-acquisition.md`](docs/data-acquisition.md) | getting the initial balance snapshot cheaply |
| [`docs/verification.md`](docs/verification.md) | evidence log — what was measured/checked (historical) |
| [`docs/HANDOFF.md`](docs/HANDOFF.md) | the next session's task list |

## Build

```bash
cargo test --workspace          # 81 tests
```

Depends on the IKPIR primitive (`segmented-cuckoo`, `ikpir-common`, …). Currently via
path deps to a local checkout of the `perf/optimized` branch; the pinned git dep is
`{ git = "https://github.com/bao-ninh-orochi/IKPIR", rev = "042d868" }`. `.cargo/config.toml`
sets `target-cpu=native` (git deps do **not** inherit the upstream perf config).

## The core idea in one paragraph

The client downloads a one-time "hint" pinned at some block `block₀` and never has to
update it. To read an account it sends a PIR query; the server answers at its current
head `E'`; the client subtracts `qᵀ·ΔD` (the public delta stream from `block₀` to
`E'`) to recover **bit-for-bit** the answer a `block₀` server would have given, decodes
it against its stale hint, then applies the public per-cell delta to reach the current
value — **correcting the recovered bucket cells *before* the fingerprint scan**, which
is the one ordering that must not be gotten wrong. Hint updating thus becomes optional
garbage collection, and steady-state PIR traffic is ~zero because a client queries each
account once and then follows the public delta stream. This response-rewind appears
novel (see `docs/verification.md`).
