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
database); `/setup` is a ~46.51 MB response at 1M-account scale
(`--partial-capacity 1000000`, the deployed `(arity 2, bucket_size 4)`
geometry — ADR-0034; ~48.96 MB before that retune). A per-request
timeout and a small concurrency cap on `/setup` bound the trivial cases;
production-grade rate limiting, per-IP quotas, and a CDN for `/setup` (it is
immutable per epoch pin) are documented follow-ups, not present.

The same discipline runs in the *other* direction: the client transport
(`risepir-http/src/client.rs`) caps every response body per endpoint before
buffering and bounds connect/stall time, so a hostile or wedged **server**
cannot OOM or hang a front end. The state-file loader applies it to disk
input too (`setup_len` bounded by the file's own size before allocation,
`RPST2` whole-file checksum) — "validate every length before allocating"
holds on every input path, not just the listener.

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

**As of 2026-07-26 the public deployment has TLS** (Caddy + Let's Encrypt in
front of a loopback-only `:8645`, deploy.md §3.7), which closes the on-path
gap above — and, in closing it, names the parties. *Who* is now trusted is
worth being explicit about, because it is more than "the operator":

- **Whoever controls the DNS record.** Anyone who can repoint the hostname at
  another machine can obtain a valid certificate for it, since DNS control is
  the whole of what Let's Encrypt verifies. They would then serve a wasm
  client of their choosing under this name.
- **The CA.** A mis-issued certificate has the same effect.
- Both are the *same category* as ADR-0019's disclosed code-delivery trust —
  the page's crypto is delivered by the party serving the page — just widened
  from one party to three. TLS does not remove that trust; it makes it
  attributable to named parties rather than to anyone on the path.

**As of 2026-08-17 the name is `demo.risepir.org`, a registered domain**
(deploy.md §3.7), which is the "buys a domain it controls" mitigation this
section used to only recommend. It does not remove the trust above — DNS
control still implies certificate control, inherently — but it shrinks and
hardens the set of parties who hold it:

- **Out:** DuckDNS, a free service authenticated by a bearer token with no
  second factor, plus a subdomain the operator never owned.
- **In:** Cloudflare, as registrar and DNS for a domain held on an account
  with 2FA and a registrar lock (`clientTransferProhibited`).
- **New, and not previously available on a free subdomain:** DNSSEC on the
  zone, and CAA records — which narrow "any of ~50 public CAs may issue for
  this name" to five.

  Five, not one, and the gap is worth recording because it is invisible in the
  dashboard. The zone's own CAA record is `0 issue "letsencrypt.org"`, but
  Cloudflare **injects additional CAA records authorising its own CAs** the
  moment any CAA record exists in a zone it serves, so that Universal SSL can
  still issue. Those injected records do not appear in the DNS UI — `dig CAA
  risepir.org` returns eleven records where the dashboard lists two. The
  effective issuer set is Let's Encrypt, Google Trust Services, SSL.com,
  Sectigo and DigiCert. They cannot be removed without disabling Universal SSL
  for the whole zone.

  For the same reason the zone's `0 issuewild ";"` is **inert**: CAA is a union
  of permissions rather than a veto, so an injected `issuewild "letsencrypt.org"`
  authorises wildcards regardless of what `;` says. Read the policy with `dig`,
  never from the dashboard — this is a case where the UI and the DNS disagree.

One deliberate non-change belongs here, because it is the obvious future
"optimization" that would silently undo the above: the `demo.` record is
**DNS-only (unproxied)** at Cloudflare. Enabling the proxy would terminate
TLS at Cloudflare and re-add exactly one party to the code-delivery chain
this section exists to enumerate. PIR queries are LWE ciphertexts and stay
private under a proxy; the wasm client is what must not gain a party. Any
future CDN for `/setup` (roadmap C5) must therefore serve *the bundle*
without becoming the origin for *the page*.

Reducing this further means serving the page from a different party than the
PIR server (ADR-0019's stronger arrangement, which needs CORS the PIR routes
do not currently emit), or delivering the client through a channel the server
operator cannot vary per-visitor.

**A second name, and a second party, since 2026-08-17 (ADR-0043).** The zone
apex `risepir.org` — the URL a paper cites — is an always-on static page on
**Cloudflare Pages**, so that a cited link resolves while the demo VM is
stopped. Three things about it belong in this section:

- **It is not the mitigation named in the paragraph above.** That mitigation
  is *the demo's own page* served by a party other than the PIR server. This
  is a *different page* answering a different question. The demo's page and
  its PIR transport remain on one hostname with `connect-src 'self'`. Do not
  read ADR-0043 as having reduced the code-delivery trust; it has not.
- **It delivers no client**, makes no PIR query, and holds no key, so it sits
  outside the code-delivery boundary this section enumerates. What it *can* do
  is send a reader somewhere other than the real demo, or show screenshots
  that lie. That is a genuine capability, and it is **undefended** — but it is
  strictly more visible than a modified wasm client: a wrong destination is
  legible in the address bar, where tampered client bytes are legible to
  nobody who does not audit them.
- **It adds no party who was not already trusted.** Cloudflare is already this
  zone's registrar and DNS, so the account that could serve a bad apex page
  could already repoint `demo.` itself. The marginal exposure is a cheaper and
  more visible route to a class of harm this section already attributes to the
  Cloudflare account — not a new class.

The apex is a **proxied** Pages record, unavoidably, since that is how Pages
serves at all; its certificate comes from Google Trust Services under the
CAA records Cloudflare injects. That is acceptable precisely because no
cryptographic client is delivered there. It changes nothing about `demo.`,
which stays a **DNS-only, unproxied `A` record** for the reason above.

**One thing the apex does that is worth naming, because it was not asked
for.** Cloudflare injects its own analytics beacon
(`static.cloudflareinsights.com/beacon.min.js`) into the apex page at the
edge. It is absent from the source and absent from a plain `curl` — it
appears only for requests carrying a browser `User-Agent`, which is why it
is easy to miss. It is zone-level Web Analytics, not a Pages project setting
(`web_analytics_tag` on the project is `null`), so turning it off is a
dashboard action on the zone. Until it is off, the apex page **discloses it
in its own trust section**: a page that asks a reader to care who serves it
cannot quietly ship a third-party script it never mentions. This is a
privacy wart on a marketing page, not a break in any PIR property — no
cryptographic client, no query, and no key is delivered there — but a
project that documents its trust this carefully should not have to be told
about its own beacon by an auditor.

None of this reaches `demo.`, which is unproxied end to end.

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

**`GET /metrics`/`GET /status` (ADR-0039) do not change this status, but
they do change who can observe it.** Both are public wherever Caddy
proxies the whole PIR port (as it does today, deploy.md §3.7), and both
expose only aggregates — request rate and outcome by route, error counts
by class, block lag, store occupancy, an answer-latency histogram — never
which account any single request named. The answer-latency histogram in
particular cannot leak the queried bucket: `RisePirServer::answer` folds
over its *entire* segment for every query regardless of content (ADR-0039
has the full argument, and what would falsify it). What genuinely changes
is reach: the query-rate/timing metadata this section already treats as
conceded previously required a position on the network path to observe;
after this change, anyone can obtain the same order of metadata — request
volume, timing, error rates — by just asking the server.

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

**Honest throughput:** at the defaults (`reconcile_every 30`,
`reconcile_samples 8`) that is one checkpoint per ~6 min of chain time and up
to 8 comparisons each — **~1,920 account comparisons per day** against a
complete set of **204.7 M** accounts (2026-09-03; was 200.5 M). Say plainly
what that is: not coverage, but
a well-targeted smoke test, because the accounts it samples are exactly the
ones the block just changed — precisely where a feed error would actually
show up, not a uniform sample of the whole universe.

**Observability of the check itself (ADR-0027):** the check can go dark —
every fetch to the independent provider failing, e.g. a keyless tier refusing
archive-depth reads during a catch-up — and the old code made that
indistinguishable from "checked and exact": both logged nothing and both
reported success. `GET /healthz` now reports the reconcile check's own health
(configured or not, last checkpoint, last *successful* checkpoint, running
totals, consecutive dark checkpoints), a dark checkpoint always logs a
warning, and a prolonged dark streak escalates to `CRITICAL` without halting
— halting the follow loop because a third party is unreachable would convert
that party's outage into this deployment's, for zero additional wrong-answer
prevention. A value mismatch still halts, unconditionally, exactly as above.

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
| On-path attacker replaces the session | closed for the public deployment by TLS (§4.2); still open for any plaintext-HTTP or SSH-tunnel-less use |
| DNS/CA holder serves a modified client under the deployment's name | **undefended, narrowed** (§4.2) — inherent: DNS control implies certificate control. Since 2026-08-17 the chain is a registered domain on a 2FA'd, transfer-locked registrar account rather than a free subdomain and its bearer token, with DNSSEC on the zone and CAA narrowing the issuer set from ~50 CAs to five (not one — the DNS provider injects records for its own CAs; §4.2). The `demo.` record stays **unproxied** so no TLS-terminating CDN rejoins the chain |
| Apex-page host (`risepir.org`, Cloudflare Pages) sends a reader to the wrong demo, or shows false screenshots | **undefended** (§4.2, ADR-0043) — but it delivers no client and holds no key, and a wrong destination is visible in the address bar where a tampered wasm client is visible to nobody. Adds no party: Cloudflare is already this zone's registrar and DNS, so the same account could already repoint `demo.` |
| Traffic analysis / timing correlation | **undefended** (§5) |
| `GET /metrics`/`GET /status` publish aggregate request-rate, error-rate, block-lag, and answer-latency metadata publicly | **same status as traffic analysis above, now ask-able without a network position** (§5, ADR-0039) — every field is an aggregate, never per-query, and the latency histogram is timing-side-channel-safe by construction |
| Feed poisoning between reconcile checkpoints | detected with lag, sampled (§6) |
| Feed + reconcile providers collude | undetected (§6) |
| Volumetric DoS | partially mitigated (§3) |
| Hostile server OOMs/hangs a client | bounded: per-endpoint body caps + stall timeouts (§3) |
| Disk corruption of the state file silently loads | detected at load: `RPST2` whole-file checksum |
| Cuckoo false positive answers garbage for an absent account | ~2⁻⁶⁰ (ADR-0009), bounded by conformance sampling |
| LWE noise corrupts an SCF fingerprint cell → silent `0x0` | inherent to the filter layer, documented (ADR-0009 residual) |
| Side channels in client decode | best-effort branchless scan upstream; a constant-time audit is out of scope (upstream SECURITY.md) |
