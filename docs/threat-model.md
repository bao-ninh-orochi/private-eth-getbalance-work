# Threat model

*What this system defends against, what it detects, and what it plainly does
not — the adversary definitions every other security decision in this repo
hangs on. Written 2026-07-21 (ADR-0020). If a change alters any boundary
below, update this document in the same commit.*

The system: a server follows Ethereum mainnet and answers `eth_getBalance`
via LWE keyword-PIR (RisePIR over a Segmented Cuckoo Filter) without learning
which account was asked. Two listeners: the binary PIR transport on `:8645`
(`/setup`, `/answer`, `/delta/{block}`, `/sync`, `/head`, `/mode`) and a
JSON-RPC front end on `:8545`. In the remote deployment shape, the front end
and PIR client run on the *user's* machine (`risepir-rpc client`) and only
PIR messages cross the network.

## 1. Assets

| asset | held by | worth protecting because |
|---|---|---|
| **Which address was queried** | the user | the entire point of the system; linking address ↔ requester deanonymizes holdings |
| **Answer integrity** | both | the repo's first binding rule: *never return a wrong answer* — a silently wrong balance is total failure |
| **The stored balance set** | operator | corrupting it converts into wrong answers downstream |
| **Availability** | operator | a PoC concern only; documented, mostly undefended |

## 2. Actors and what each is trusted with

| actor | trust assumption |
|---|---|
| PIR operator | **honest-but-curious** (runs the protocol as written, may inspect everything it sees) — the load-bearing assumption; see §4 |
| Feed provider (dRPC) | untrusted-but-audited: errors are *detected* with bounded lag, not prevented |
| Reconcile provider (publicnode) | trusted to be independent of the feed provider; only makes the feed audit meaningful |
| Network observer | untrusted for content; **conceded the metadata of §5** |
| Anyone who can send bytes to the listeners | fully untrusted (§3) |
| The Rust dependency graph | pinned and audited in CI (`Cargo.lock`, pinned git rev for IKPIR, `cargo-deny`) |

## 3. Adversary: anyone who can reach the listeners

**Capability:** send arbitrary bytes to `:8645`/`:8545`; open many
connections.

**Guaranteed:** malformed, truncated, or adversarial input produces a clean
error — never a panic, never an attempted oversized allocation, and never a
value that decodes "successfully" into something a PIR call indexes
out-of-bounds. Mechanisms: request bodies are size-capped before buffering
(`MAX_ANSWER_BODY_BYTES`, `risepir-http/src/node.rs`); every wire length is
validated against bytes-actually-present before allocation, and query/response
/hint segment lengths are checked **exactly** against the deployment geometry,
not merely bounded (`risepir-http/src/wire.rs` module docs — the exactness is
load-bearing: upstream slices on that assumption with only a
`debug_assert`). Enforced by unit tests, in-tree byte-fuzz tests, and
coverage-guided fuzz targets (`fuzz/`).

**Not guaranteed:** resistance to volumetric denial of service. `/answer` has
a server-side compute cost by design (a PIR answer touches the whole
database); `/setup` is a ~47 MB response at 1M-account scale. A per-request
timeout and a small concurrency cap on `/setup` bound the trivial cases;
production-grade rate limiting, per-IP quotas, and a CDN for `/setup` (it is
immutable per epoch pin) are documented follow-ups, not present.

## 4. Adversary: the PIR operator

### 4.1 Honest-but-curious (assumed)

**Guaranteed:** the operator does not learn which account a query targets.
The query is an LWE encryption of a unit selector; distinguishing targets
reduces to decision-LWE at the deployed parameters. This holds
per-query with no bound on query count, and the single-server setting means
there is no non-collusion assumption to violate.

**The operator still sees:** that a query happened, from which IP, when, how
often; the client's pinned epoch (a `/sync?from=…` or `/delta/{block}` fetch
names the blocks the client lacks, dating its bootstrap). None of this names
the account, but see §5 for what correlation can do with it.

### 4.2 Malicious (NOT defended — the honest gap)

**A dishonest operator can serve a wrong balance that passes every client
check.** The client's `key_tag` and checksum (ADR-0009) are recomputed over
whatever value the store holds; an operator who *places* a forged
value under an account's key produces a perfectly consistent forgery. The
client has no independent reference — and cannot fetch one per-query without
revealing the address, which would defeat the system. Every integrity
mechanism in this repo (ADR-0005's `|Δ| < p` bound, ADR-0009,
ADR-0017's verified store scan) defends against *accident* — bugs,
corruption, collisions — not against the operator lying.

Posture (ADR-0020): this is a **documented trust assumption plus
operator-side detection**, stated here and to be stated anywhere a user can
read (`/mode`-style, in the web UI when it lands). Detection rungs, in
increasing strength, are future work: a signed per-block store digest (makes
*global* tampering attributable), public anchoring of that digest (stops
serving different histories to different clients), full verifiable PIR
(VeriSimplePIR-style — a research effort, and none of the weaker rungs stop
*targeted* lying to one user; anything describing them must say so).

**Without TLS this assumption silently widens.** Plain HTTP means anyone
on-path can *be* the operator (swap `/setup`, serve their own answers).
An active MITM tampering with individual response bytes is caught by the
checksum with overwhelming probability (it cannot steer LWE-decoded cells
without the client's secret), but wholesale session replacement is
indistinguishable from a different operator. TLS is therefore a hard
prerequisite for any deployment where "the operator" is meant to name a
specific party — until then, trust extends to the network path.

## 5. Adversary: a network / traffic observer

PIR hides *which record*. It does not hide **that** you asked, **when**,
**from where**, or **how often** — and this deployment currently does nothing
to change that:

- **Timing correlation:** a query that lands seconds after an on-chain
  transaction touching some account is a strong join between that account
  and the client IP. This is a realistic deanonymization path, not a
  theoretical one.
- **Anonymity set:** with a single small deployment, the set of possible
  requesters is tiny regardless of the cryptography. The crypto bounds what
  the *content* reveals; it cannot manufacture crowd cover.
- **Sync patterns:** delta fetches reveal the client's pin age and how
  regularly it follows the chain.

Mitigations, cheap to expensive, all future work and none implemented: state
this in the UI; a Tor onion service; fixed-rate polling / cover traffic; an
OHTTP relay so the operator never sees client IPs.

## 6. Adversary: the feed provider

The store is built from a feed the server chooses (dRPC traces ⊕ block
withdrawals). A poisoned or subtly wrong feed writes well-formed wrong
balances that every client-side check accepts — the ingest path is where
*never-wrong-answer* currently stops.

**Present defense — detection with bounded lag:** every `reconcile_every`
blocks the server diffs a sample of that block's own touched accounts
against an independent operator (publicnode) at the same explicit height; a
mismatch is CRITICAL — the server stops following and keeps serving the last
good block (`risepir-rpc/src/mainnet.rs`). The live feed gate
(`cargo test -p risepir-feed --release -- --ignored`) independently
cross-validates trace parsing. Assumes feed and reconcile providers do not
collude; sampled, so a targeted single-account poisoning can win the lottery
between checkpoints.

**Planned strengthening (A3):** an N-provider quorum on each block's effects
before apply — prevention rather than detection — extends the
never-wrong-answer contract to ingest. Blocked today on the shortage of
independent keyless *trace* providers; do not fake it with two frontends of
the same operator.

**Rule (operator-side only):** the reconcile check must **never** run
client-side per query — fetching a queried account from a public RPC reveals
the address and undoes the system. It is an operator audit tool, full stop.

## 7. Honesty invariants (self-imposed, enforced in code)

- **Partial mode never answers `0x0` for an untracked account** and never
  applies a withdrawal credit to one — absence only means zero for a
  *complete* set (ADR-0015/0017). The completeness flag is served via
  `GET /mode`, never guessed by a front end.
- **`"latest"` means our finalized head** (~13 min behind the public head,
  ADR-0007) — deletes the reorg bug class; conformance must compare at an
  explicit height, never "latest"-vs-"latest".
- **Fail loudly.** `NotFound` / `DecodeFailed` / `FingerprintAmbiguity` /
  `CorruptStoredValue` / strict-partial errors all exist so that no doubt is
  ever resolved by answering a number.

## 8. Residual risks, plainly

| risk | status |
|---|---|
| Operator forges a balance | **undefended** (§4.2) — trust assumption, stated |
| On-path attacker replaces the session (no TLS) | **undefended** until TLS lands (§4.2) |
| Traffic analysis / timing correlation | **undefended** (§5) |
| Feed poisoning between reconcile checkpoints | detected with lag, sampled (§6) |
| Feed + reconcile providers collude | undetected (§6) |
| Volumetric DoS | partially mitigated (§3) |
| Cuckoo false positive answers garbage for an absent account | ~2⁻⁶⁰ (ADR-0009), bounded by conformance sampling |
| LWE noise corrupts an SCF fingerprint cell → silent `0x0` | inherent to the filter layer, documented (ADR-0009 residual) |
| Side channels in client decode | best-effort branchless scan upstream; a constant-time audit is out of scope (upstream SECURITY.md) |
