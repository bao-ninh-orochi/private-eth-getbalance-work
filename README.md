# private-eth-getbalance

A proof-of-concept **private Ethereum RPC**: answer `eth_getBalance` without the
server learning which account was asked about, using **RisePIR** (the LWE
instantiation of Incremental Keyword PIR) over a Segmented Cuckoo Filter.

The point is not speed or hint size — those are inherited from SimplePIR. The point is
that classical PIR-with-preprocessing must re-run a minutes-long setup on every state
change, which at Ethereum scale is impossible every 12 s; RisePIR folds a block's ~300
balance changes into a **~5 ms** incremental hint patch with no dependence on database
size. See [`docs/plan.md`](docs/plan.md) §7.

## Status: runs against real mainnet

```bash
cargo build --release -p risepir-rpc
./target/release/risepir-rpc mainnet --partial     # zero-prerequisite live demo
./target/release/risepir-rpc mock                  # synthetic demo, no network

# ...or in a browser (ADR-0019): the same client, compiled to wasm, in the page
rustup target add wasm32-unknown-unknown
cargo run -p xtask --release -- web                # builds web/client.wasm
./target/release/risepir-rpc mainnet --partial --partial-capacity 1000000 --web web
open http://127.0.0.1:8645/

# ...or measure a running deployment from the client side
./target/release/risepir-rpc probe --pir-url https://demo.risepir.org \
  --queries-csv queries.csv --blocks-csv blocks.csv
```

`mainnet` follows finalized blocks over keyless public RPC (dRPC traces ⊕
block withdrawals), answers `eth_getBalance` privately on `:8545`
(`cast balance <addr> --rpc-url http://127.0.0.1:8545`), reconciles sampled
accounts against an independent provider every few blocks, and persists/reloads
its full PIR state. A third subcommand, `client --pir-url http://<server>:8645`,
runs the JSON-RPC front end + rewind client on *your* machine against a remote
PIR server (`--bind 0.0.0.0`) — the queried address never leaves your machine.
A fourth, `probe`, runs one long-lived session of that same client as a
**measurement campaign**: per-query latency split into build / wire / the
server's own reported answer time / decode's four rewind steps / an explicit
residual (the budget closes by construction, never by distributing a
remainder), bytes each way, per-block delta cost, hint size, and client RSS —
with every decoded balance checked byte-exactly against an independent
provider at the *same explicit block height*. Measuring changes nothing about
the privacy claim: the address never leaves the machine, and it never reaches
a CSV, a log line, or an error message either.
A **browser front end** (`--web web`) runs that same rewind client as
WebAssembly *in the page*, so the address never leaves the browser either: a
real mainnet balance was fetched this way and confirmed byte-exact against an
independent provider ([`docs/deploy.md`](docs/deploy.md) §1.5, §5.1). What it
does not protect — you trust whoever serves you the page, and the network still
sees who is asking — is stated on the page itself, and argued in ADR-0019.
Recorded live evidence — 8/8 private queries byte-exact
against publicnode on real blocks — in [`docs/deploy.md`](docs/deploy.md) §5,
which is also the complete runbook (including the BigQuery snapshot export that
upgrades `--partial` to the complete ~100 M-account set).

Eight crates, **191 tests**, plus two heavier gates: `cargo run -p xtask
--release -- conformance` (≥1200 addrs × 120 blocks, byte-identical, all account
categories) and the live test `cargo test -p risepir-feed --release --
--ignored` (trace-derived balances vs an independent provider on a real
finalized block). CI enforces clippy + tests on every push, conformance on
PRs, and runs the live gate plus coverage-guided fuzzing of every
attacker-facing decoder ([`fuzz/`](fuzz/)) nightly.

| crate | role |
|---|---|
| `risepir-proto` | geometry, `BlockUpdate`/`BlockDelta`, value + delta codecs, keccak |
| `risepir-server` | batched per-block server; verified (fp ∧ key_tag) store ops; delta ring |
| `risepir-client` | the response-rewind client |
| `risepir-feed` | chain ingest: seeded mock, BigQuery snapshot loader, mainnet rpc feed |
| `risepir-http` | axum PIR transport + binary wire codec + HTTP client |
| `risepir-rpc` | JSON-RPC `:8545` front end + `mock`/`mainnet` binary + state files |
| `risepir-wasm` | the same rewind client as a browser (wasm32) module — pure compute, one import |
| `xtask` | conformance harness + measured numbers table |

## Docs

| doc | what |
|---|---|
| [`docs/plan.md`](docs/plan.md) | **authoritative current spec** — read first |
| [`docs/threat-model.md`](docs/threat-model.md) | what is defended, what is detected, what is *knowingly* not |
| [`docs/deploy.md`](docs/deploy.md) | **the runbook**: demo in 5 min, complete mainnet, costs, live evidence |
| [`docs/adr/README.md`](docs/adr/README.md) | every decision, with rationale + rejected alternatives |
| [`docs/sync.md`](docs/sync.md) | keeping the DB current from the chain |
| [`docs/data-acquisition.md`](docs/data-acquisition.md) | snapshot source analysis (BigQuery / snap / Xatu) |
| [`docs/numbers.md`](docs/numbers.md) | the measured Stage-3 numbers table (`xtask bench`) |
| [`docs/verification.md`](docs/verification.md) | evidence log — what was measured/checked (historical) |
| [`docs/HANDOFF.md`](docs/HANDOFF.md) | the next session's task list |

## Build

Depends on the IKPIR primitive, pinned as a git dependency
(`bao-ninh-orochi/IKPIR` @ `v0.2.0-perf`, the `perf/optimized` tip) — the fork
is public, so building needs no credentials. `.cargo/config.toml` sets
`git-fetch-with-cli` and `target-cpu=native` (git deps do **not** inherit the
upstream perf config).

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

## License

Licensed under [Apache-2.0](LICENSE) — chosen for its express patent
grant and retaliation clause, which matter for a lattice-PIR implementation
(ADR-0044). See
[`SECURITY.md`](SECURITY.md) for reporting vulnerabilities and
[`CONTRIBUTING.md`](CONTRIBUTING.md) for the gates a change must pass.
