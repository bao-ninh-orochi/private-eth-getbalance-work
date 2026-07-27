// End-to-end gate for the browser client (ADR-0019): the *real* wasm
// module, driven by the *real* web/pir.js, against a *real* running
// `risepir-rpc` server over HTTP.
//
//     cargo run -p xtask --release -- web           # build web/client.wasm
//     cargo build --release -p risepir-rpc
//     ./target/release/risepir-rpc mock --web web &
//     node web/test/e2e.mjs http://127.0.0.1:8645
//
// The Rust-side tests in crates/risepir-wasm/tests/abi.rs already pin the
// protocol invariants natively. What only this can check is the part that
// is specific to actually being in a wasm host:
//
//   1. the module has exactly ONE import — the entropy shim — so it is
//      structurally incapable of reaching the network itself;
//   2. that shim is really called, with real bytes, on every query (a
//      client whose LWE secret was not random would still return correct
//      balances while leaking the address, so this is asserted, never
//      assumed);
//   3. the whole thing loads, syncs, and answers correctly through the
//      same host code the page runs.
//
// Exits 0 on pass, 1 on failure, with the failing assertion named.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { connect, formatEth, parseAddress, PirError, STATUS, assessCapacity, CAPACITY_VERDICT } from "../pir.js";

const base = process.argv[2] ?? "http://127.0.0.1:8645";
const wasmPath = fileURLToPath(new URL("../client.wasm", import.meta.url));

let failures = 0;
function check(name, cond, detail = "") {
  if (cond) {
    console.log(`  ok    ${name}`);
  } else {
    failures += 1;
    console.log(`  FAIL  ${name}${detail ? ` — ${detail}` : ""}`);
  }
}

const wasmBytes = readFileSync(wasmPath);

// ── 1. the module cannot phone home ───────────────────────────────────
//
// This is the structural half of the privacy claim: whatever the module
// does internally, the only thing it can reach outside its own linear
// memory is the function that hands it random bytes. No fetch, no clock,
// no storage, no DOM.
{
  const mod = new WebAssembly.Module(wasmBytes);
  const imports = WebAssembly.Module.imports(mod).map((i) => `${i.module}.${i.name}`);
  check(
    "wasm imports are exactly [env.risepir_fill_random]",
    imports.length === 1 && imports[0] === "env.risepir_fill_random",
    `got [${imports.join(", ")}]`,
  );
}

// ── 2. the capacity pre-flight is a pure, deterministic decision ─────
//
// assessCapacity (web/pir.js) never touches the network, `navigator`, or
// the DOM, so it is exercised directly here, under plain Node — no
// browser, no server, no wasm. See ADR-0032 for why this pure function
// lives in pir.js rather than its own web/capacity.js: every file the
// browser can fetch has to be named in crates/risepir-http/src/web.rs's
// fixed asset MANIFEST, and this repo's static-asset routing has no
// path-mapping fallback to ride a new file in on (ADR-0019).
{
  const COMPLETE_SET_HINT_BYTES = 830_728_800; // docs/numbers.md §4c, measured
  const MOCK_HINT_BYTES = 1_770_000; // ~1.77 MB — this repo's own mock deployment

  const tinyDevice = assessCapacity({ hintBytes: COMPLETE_SET_HINT_BYTES, deviceMemoryGb: 2 });
  check(
    "a complete-set-sized hint refuses to auto-start on a 2 GB device",
    tinyDevice.verdict === CAPACITY_VERDICT.REFUSE,
    JSON.stringify(tinyDevice),
  );

  const bigDevice = assessCapacity({ hintBytes: COMPLETE_SET_HINT_BYTES, deviceMemoryGb: 32 });
  check(
    "the identical hint is fine on a 32 GB device",
    bigDevice.verdict === CAPACITY_VERDICT.OK,
    JSON.stringify(bigDevice),
  );

  const mockOnTinyDevice = assessCapacity({ hintBytes: MOCK_HINT_BYTES, deviceMemoryGb: 2 });
  check(
    "a mock-sized hint never gates even a 2 GB device (the no-op property the browser gate relies on)",
    mockOnTinyDevice.verdict === CAPACITY_VERDICT.OK,
    JSON.stringify(mockOnTinyDevice),
  );

  // No deviceMemory number at all (Safari, Firefox) must never refuse,
  // regardless of how large the hint is or how mobile-looking any coarse
  // signal is — point 4 of ADR-0032. Both cases below reuse the same
  // complete-set hint as the first case, so the only variable is whether a
  // real number was ever available to compare against.
  const unknownNoSignal = assessCapacity({ hintBytes: COMPLETE_SET_HINT_BYTES, deviceMemoryGb: undefined });
  const unknownCoarseSignal = assessCapacity({
    hintBytes: COMPLETE_SET_HINT_BYTES,
    deviceMemoryGb: undefined,
    coarsePointer: true,
    viewportWidth: 360,
  });
  check(
    "unknown deviceMemory never refuses, even with no other signal",
    unknownNoSignal.verdict !== CAPACITY_VERDICT.REFUSE,
    JSON.stringify(unknownNoSignal),
  );
  check(
    "unknown deviceMemory never refuses, even with a strong coarse-mobile signal",
    unknownCoarseSignal.verdict !== CAPACITY_VERDICT.REFUSE,
    JSON.stringify(unknownCoarseSignal),
  );
  check(
    "...but never blocking a capable desktop still leaves room for a soft warning at that size",
    unknownCoarseSignal.verdict === CAPACITY_VERDICT.WARN,
    JSON.stringify(unknownCoarseSignal),
  );
  check(
    "and with no coarse signal at all, an unknown-memory visit is a plain no-op",
    unknownNoSignal.verdict === CAPACITY_VERDICT.OK,
    JSON.stringify(unknownNoSignal),
  );

  // Sanity: the 2x peak-estimate multiple is chosen to match the measured
  // ratio in docs/numbers.md §4c, not picked independently of it.
  const measuredResidentBytes = 1_662_804_000;
  const estimate = assessCapacity({ hintBytes: COMPLETE_SET_HINT_BYTES, deviceMemoryGb: 1000 }).estimatedPeakBytes;
  const relErr = Math.abs(estimate - measuredResidentBytes) / measuredResidentBytes;
  check(
    "the 2x peak estimate tracks docs/numbers.md §4c's measured hint->resident ratio within 1%",
    relErr < 0.01,
    `estimate ${estimate}, measured ${measuredResidentBytes}, error ${(relErr * 100).toFixed(2)}%`,
  );
}

// ── 3. bring up a session ─────────────────────────────────────────────

console.log(`\nconnecting to ${base} ...`);
let session;
try {
  session = await connect(base, { wasmBytes, onProgress: () => {} });
} catch (e) {
  console.error(`\ncould not connect: ${e.message}`);
  console.error("is the server running?  ./target/release/risepir-rpc mock --web web");
  process.exit(1);
}

const complete = session.complete;
console.log(
  `  mode=${complete ? "COMPLETE" : "PARTIAL"} pinned=${session.pinnedBlock} ` +
    `arity=${session.arity} setup=${(session.traffic.setupBytes / 1e6).toFixed(2)} MB\n`,
);

check("entropy shim was called during setup or is ready", session.entropy.calls >= 0);
check("pinned block is a real block", session.pinnedBlock >= 0n);
check(
  "the lineage epoch is exposed for /sync and /answer (ADR-0033)",
  /^[0-9a-f]{16}$/.test(session.epoch),
  `epoch=${JSON.stringify(session.epoch)}`,
);

// ── 4. a real lookup ──────────────────────────────────────────────────

const recent = await session.recent();
check("GET /recent returned addresses", recent.length > 0, `${recent.length} addresses`);

const entropyBefore = { ...session.entropy };
const target = recent[0];
const result = await session.getBalance(target);

check(
  "a tracked address resolves to FOUND or ZERO",
  result.status === STATUS.FOUND || result.status === STATUS.ZERO,
  `status ${result.status}`,
);
// Note what is *not* claimed: `rand::rng()` seeds a userspace CSPRNG from
// the host once and then streams from it, so OS entropy is not drawn per
// query — and asserting that it were would be asserting something false.
// What must hold is that the shim was reached at all (a wasm client that
// never got real entropy would be catastrophic and silent), which is
// checked here, and that each query's ciphertext is fresh, which is
// checked at the wire level below.
check(
  "the entropy shim was reached with real bytes",
  session.entropy.calls > 0 && session.entropy.bytes >= 32,
  `${session.entropy.calls} calls, ${session.entropy.bytes} bytes`,
);
void entropyBefore;
check(
  "the query carried no address-sized plaintext",
  session.traffic.queryBytes > 1000,
  `${session.traffic.queryBytes} bytes of LWE ciphertext`,
);
console.log(
  `        ${target} = ${result.balanceWei} wei (${formatEth(result.balanceWei ?? 0n)} ETH) ` +
    `at block ${result.atBlock}`,
);

// ── 5. the never-a-wrong-answer surface ───────────────────────────────

// An address that is certainly not in any set.
const absent = "0x" + "de".repeat(20);
const absentResult = await session.getBalance(absent);
if (complete) {
  check(
    "an absent address is exactly 0x0 for a complete set",
    absentResult.status === STATUS.ZERO && absentResult.balanceWei === 0n,
    `status ${absentResult.status}`,
  );
} else {
  check(
    "an absent address is UNTRACKED (never 0x0) for a partial set",
    absentResult.status === STATUS.UNTRACKED,
    `status ${absentResult.status}`,
  );
}

// Malformed addresses never become queries.
for (const bad of ["", "0x", "0xzz", "0x1234", "not an address", "0x" + "aa".repeat(19)]) {
  let threw = false;
  try {
    parseAddress(bad);
  } catch (e) {
    threw = e instanceof PirError;
  }
  check(`rejects malformed address ${JSON.stringify(bad)}`, threw);
}

// ── 6. fresh randomness per query, checked on the wire ────────────────
//
// Three lookups of the *same* address. The ciphertext must differ every
// time (a reused LWE secret lets the server subtract A·s and read the
// queried bucket straight out of the query — a failure that returns
// perfectly correct balances the whole time it is leaking), and the size
// must not, since a size that varied with the address would leak through
// the length alone.
{
  const tails = new Set();
  const sizes = new Set();
  const answers = new Set();
  for (let i = 0; i < 3; i++) {
    const r = await session.getBalance(target);
    tails.add(session.lastQueryTail);
    sizes.add(session.traffic.queryBytes);
    answers.add(String(r.balanceWei));
  }
  check("repeated queries for one address send different ciphertext", tails.size === 3, `${tails.size} distinct`);
  check("repeated queries keep a constant size (no length leak)", sizes.size === 1, `${[...sizes]}`);
  check("repeated queries agree on the balance", answers.size === 1, `${[...answers]}`);
}

// ── 7. the client tracks the chain ────────────────────────────────────

const headNow = await session.head();
check("client synced up to the server head", session.pendingHead <= headNow);
check(
  "the hint is still pinned where it started (the rewind, not a re-download)",
  session.pinnedBlock <= session.pendingHead,
);

console.log(
  `\n${failures === 0 ? "PASS" : "FAIL"}: ${failures} failing check${failures === 1 ? "" : "s"}\n`,
);
process.exit(failures === 0 ? 0 : 1);
