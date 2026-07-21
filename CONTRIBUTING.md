# Contributing

This is a research PoC with production-grade discipline. The architecture
and its rationale live in [`docs/plan.md`](docs/plan.md) and the ADR log
([`docs/adr/README.md`](docs/adr/README.md)); the operating manual is
[`CLAUDE.md`](CLAUDE.md). Read the binding rules there first — above all:
**never return a wrong answer**, and **validate every length before
allocating**.

## Toolchain

The workspace pins Rust via [`rust-toolchain.toml`](rust-toolchain.toml)
(rustup selects it automatically). Bump the pin deliberately and in its own
commit — `docs/numbers.md` measurements are toolchain-sensitive.

Building needs read access to the private pinned dependency
`bao-ninh-orochi/IKPIR` (`.cargo/config.toml` sets `git-fetch-with-cli`, so
your system git credentials are used). CI authenticates with the
`IKPIR_TOKEN` secret — a fine-grained PAT scoped to that repo, Contents
read-only (see the provisioning note in `.github/workflows/ci.yml`).

## Gates, in escalating strength

```bash
cargo clippy --workspace --all-targets -- -D warnings   # always; CI-enforced
cargo test --workspace                                  # always; CI-enforced
cargo run -p xtask --release -- conformance             # CI on PRs; byte-exact vs ground truth
cargo test -p risepir-feed --release -- --ignored       # live feed gate; CI nightly
```

Run the live gate and a `mainnet --partial` smoke run (deploy.md §1) after
touching the feed or the apply path. Report real output — a gate that was
not run is a gate that failed.

`cargo fmt` is **not** yet CI-enforced: the tree predates `rustfmt.toml` and
a mechanical reformat is deferred until the in-flight branches land (one
formatting-only commit, then the CI gate turns on). Until then, match the
surrounding code's formatting by hand and do not mass-reformat.

## Fuzzing

Coverage-guided fuzz targets for every attacker-facing decoder live in
[`fuzz/`](fuzz/) (`cargo-fuzz`, needs nightly):

```bash
cargo +nightly fuzz run wire_setup -- -max_total_time=300
```

New decoders for untrusted bytes get a fuzz target in the same PR.

## Decisions

New decisions get a new ADR in `docs/adr/README.md` — one paragraph: chosen,
rejected, why. **Reasoned deviation is welcome; silent deviation is the
failure mode.** If a change alters a security boundary, update
[`docs/threat-model.md`](docs/threat-model.md) in the same commit.

## Conventions

- Commit style: `type: summary` or `type(scope): summary`, lowercase summary
  (see `git log`). Sign commits.
- `unsafe_code = "forbid"` stays on every crate (the single documented FFI
  `allow` in `risepir-wasm` stays single).
- Never hardcode `plaintext_bits` or geometry — derive via
  `risepir_proto::Geometry`.
- Store writes go through the verified fp ∧ `key_tag` scan
  (`risepir-server/src/verified.rs`, ADR-0017) — never the store's raw
  key-addressed `update`/`delete`.
- Hand-rolled error types are house style (no `thiserror`/`anyhow`); keep
  the external dependency set lean.

## Licensing

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as MIT OR Apache-2.0, without any additional
terms or conditions.
