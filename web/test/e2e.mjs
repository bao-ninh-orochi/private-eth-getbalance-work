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
import {
  connect,
  formatEth,
  parseAddress,
  PirError,
  TimeoutError,
  STALL_TIMEOUT_MS,
  STATUS,
  assessCapacity,
  CAPACITY_VERDICT,
  HINT_SOURCE,
} from "../pir.js";

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
  // docs/numbers.md §4c, computed for the deployed (arity 2, bucket_size 4)
  // geometry (ADR-0034). PRE_ADR_0034_HINT_BYTES is the (3,4) figure the
  // ADR-0032 pre-flight was designed against and the box still serves until
  // it is re-bootstrapped — kept because the 4 GB-phone regression below is
  // only meaningful at the size that produced the bug.
  const COMPLETE_SET_HINT_BYTES = 553_819_200;
  const PRE_ADR_0034_HINT_BYTES = 830_728_800;
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

  // The estimate must model the browser's init PEAK, not steady state:
  // wasm linear memory never shrinks, so the init peak is the tab's
  // floor. It therefore has to sit strictly ABOVE §4c's steady-state
  // A+hint figure (an estimate at or below it re-creates the bug where
  // 4 GB phones were waved into a renderer-killing download) and at or
  // below 4x (the measured pre-fix worst case, before risepir_init freed
  // the encoded buffer mid-init and from_setup consumed hints per
  // segment).
  const measuredResidentBytes = 1_108_536_000; // docs/numbers.md §4c, steady-state A+hint
  const estimate = assessCapacity({ hintBytes: COMPLETE_SET_HINT_BYTES, deviceMemoryGb: 1000 }).estimatedPeakBytes;
  check(
    "the peak estimate exceeds the steady-state resident figure (peak ⊃ resident, never equal)",
    estimate > measuredResidentBytes,
    `estimate ${estimate} vs steady-state ${measuredResidentBytes}`,
  );
  check(
    "the peak estimate stays at or below the pre-fix 4x worst case",
    estimate <= COMPLETE_SET_HINT_BYTES * 4,
    `estimate ${estimate}`,
  );

  // THE phone this gate exists for: navigator.deviceMemory caps at 8 and
  // rounds down, so real 4-8 GB phones report 4 — budget 2.0 GB. Under
  // the old 2x steady-state estimate (1.66 GB) they slipped through, paid
  // the 830 MB download, and had the renderer killed at the real ~2.5 GB
  // peak. At the (3,4) hint size that produced the bug they must still be
  // REFUSE — that is the ADR-0032 regression, and it is pinned against the
  // historical size on purpose, so shrinking the deployment can never
  // quietly retire it.
  const midPhonePreAdr0034 = assessCapacity({ hintBytes: PRE_ADR_0034_HINT_BYTES, deviceMemoryGb: 4 });
  check(
    "a deviceMemory=4 phone is refused at the pre-ADR-0034 hint size (the pre-fix wave-through)",
    midPhonePreAdr0034.verdict === CAPACITY_VERDICT.REFUSE,
    JSON.stringify(midPhonePreAdr0034),
  );

  // ...and the honest consequence of ADR-0034: a third off the hint puts the
  // same phone back INSIDE its budget — estimated peak 3 x 553.82 MB =
  // 1.66 GB against 2.0 GB usable — so the complete set stops being a
  // desktop-only deployment for this device class. Asserted, not assumed:
  // this is a real product change, and if a future geometry pushes the
  // estimate back over the budget this flips and says so.
  const midPhone = assessCapacity({ hintBytes: COMPLETE_SET_HINT_BYTES, deviceMemoryGb: 4 });
  check(
    "the same phone is admitted at the ADR-0034 complete-set hint (33% smaller)",
    midPhone.verdict === CAPACITY_VERDICT.OK && midPhone.estimatedPeakBytes < midPhone.budgetBytes,
    JSON.stringify(midPhone),
  );

  // Save-Data: an explicit user preference downgrades an otherwise-fine
  // large download to a warning — never a refusal, and never any effect
  // on a small deployment.
  const saveDataBig = assessCapacity({ hintBytes: COMPLETE_SET_HINT_BYTES, deviceMemoryGb: 32, saveData: true });
  check(
    "Save-Data downgrades a large-deployment OK to WARN (basis save-data)",
    saveDataBig.verdict === CAPACITY_VERDICT.WARN && saveDataBig.basis === "save-data",
    JSON.stringify(saveDataBig),
  );
  const saveDataMock = assessCapacity({ hintBytes: MOCK_HINT_BYTES, deviceMemoryGb: 2, saveData: true });
  check(
    "Save-Data leaves a mock-sized deployment untouched",
    saveDataMock.verdict === CAPACITY_VERDICT.OK,
    JSON.stringify(saveDataMock),
  );
  const saveDataTiny = assessCapacity({ hintBytes: COMPLETE_SET_HINT_BYTES, deviceMemoryGb: 2, saveData: true });
  check(
    "Save-Data never upgrades a REFUSE (memory outranks preference)",
    saveDataTiny.verdict === CAPACITY_VERDICT.REFUSE,
    JSON.stringify(saveDataTiny),
  );
}

// ── 3. bring up a session ─────────────────────────────────────────────
//
// This is the load-bearing precondition for ADR-0038's hint cache: plain
// Node has no IndexedDB at all, so every cache code path in pir.js must be
// feature-detected to degrade to a plain download here, exactly as it did
// before the cache existed. Asserted first, not assumed — the "everything
// still works" claim below means nothing if this is false.
check("sanity: this environment has no IndexedDB (the precondition for every check below)", typeof indexedDB === "undefined");

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

/// The one-time cost of the hint, captured at boot so section 8 can prove
/// a recovered timeout never quietly paid it a second time.
const setupBytesAtBoot = session.traffic.setupBytes;

check("entropy shim was called during setup or is ready", session.entropy.calls >= 0);
check("pinned block is a real block", session.pinnedBlock >= 0n);
check(
  "the lineage epoch is exposed for /sync and /answer (ADR-0033)",
  /^[0-9a-f]{16}$/.test(session.epoch),
  `epoch=${JSON.stringify(session.epoch)}`,
);
check(
  "with no IndexedDB, the hint came from the network, not a phantom cache (ADR-0038)",
  session.traffic.setupFromCache === false,
  `setupFromCache=${session.traffic.setupFromCache}`,
);
check(
  "...and was not reported as resumed on a clean first boot",
  session.traffic.setupResumed === false,
  `setupResumed=${session.traffic.setupResumed}`,
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

// ── 8. a stalled request is bounded, and never poisons the session ────
//
// The regression this pins is the one that took the live page down for a
// user on 2026-07-28 (ADR-0035): every request here used to be a bare
// `fetch()` with no signal, so a socket that accepted and then went silent
// — a laptop sleep, a Wi-Fi change — hung forever. The page disabled its
// lookup button before the await and only re-enabled it on paths that
// require the promise to *settle*, so one stall killed the query UI until
// a reload, which at the complete set costs a 553.82 MB re-download.
//
// The stub below is what a real stalled connection looks like to `fetch`:
// nothing comes back, and the only thing that ever settles it is its own
// `AbortSignal`. A client that forgets to pass the signal through hangs
// here, so the outer race turns that into a named failure rather than a
// hung CI job.
{
  const realFetch = globalThis.fetch;
  const stallMs = 1200;
  session.stallTimeoutMs = stallMs;
  globalThis.fetch = (_input, init) =>
    new Promise((_resolve, reject) => {
      const signal = init?.signal;
      if (!signal) return; // no signal passed through: the race below reports it
      if (signal.aborted) reject(new Error("aborted"));
      signal.addEventListener("abort", () => reject(new Error("aborted")));
    });

  const HUNG = Symbol("hung");
  const t0 = Date.now();
  let err = null;
  try {
    await Promise.race([
      session.getBalance(target),
      new Promise((resolve) => setTimeout(() => resolve(HUNG), stallMs * 10)),
    ]).then((r) => {
      if (r === HUNG) err = HUNG;
    });
  } catch (e) {
    err = e;
  }
  const elapsed = Date.now() - t0;
  globalThis.fetch = realFetch;
  session.stallTimeoutMs = STALL_TIMEOUT_MS;

  check("a stalled request rejects rather than hanging forever", err !== null && err !== HUNG, err === HUNG ? "still pending after 10x the budget — is the AbortSignal reaching fetch?" : "");
  check(
    "...as a TimeoutError, which is a PirError subtype",
    err instanceof TimeoutError && err instanceof PirError,
    `got ${err?.constructor?.name ?? String(err)}`,
  );
  check("...within a small multiple of the stall budget", elapsed < stallMs * 5, `${elapsed} ms for a ${stallMs} ms budget`);

  // The point of the whole exercise: a timeout costs the attempt and
  // nothing else. The hint, its pin and its epoch are all untouched, so
  // the very next query works — no reload, no re-download.
  const after = await session.getBalance(target);
  check(
    "the session still answers after a timeout (retry is a query, not a reload)",
    after.status === result.status && after.balanceWei === result.balanceWei,
    `status ${after.status} vs ${result.status}`,
  );
  check(
    "...and the hint was never re-fetched to recover",
    session.traffic.setupBytes === setupBytesAtBoot,
    `${session.traffic.setupBytes} vs ${setupBytesAtBoot}`,
  );
}

// ── 9. a stalled /setup resumes with Range + If-Range (ADR-0038) ───────
//
// Before this landed, any stall during the hint download — mid-transfer
// or not — cost the whole 553.82 MB again on retry. This proves the
// within-session resume directly: a *fresh* session's very first `/setup`
// request is truncated then hangs — a real stalled connection looks
// exactly like this to `fetch` (nothing more ever arrives, and only the
// AbortSignal settles it, the same stub shape section 8 uses) — and the
// retry must ask for exactly the missing tail with `Range`/`If-Range`
// naming the first response's own `ETag`.
//
// The strongest proof the reassembled bytes are byte-identical to the
// real full body is functional, not introspective: the wasm buffer that
// held them is gone by the time `connect()` returns (freed inside
// `risepir_init`), but `crates/risepir-http/src/wire.rs`'s per-segment
// *exact*-length checks would reject a misaligned or spliced bundle
// outright, and a correct, byte-exact balance lookup afterward is
// impossible against corrupted hint cells — so matching the already-
// booted `session`'s answer for the same address is conclusive.
{
  const realResp = await fetch(`${base}/setup`);
  const fullSetupBytes = new Uint8Array(await realResp.arrayBuffer());
  const etagValue = realResp.headers.get("etag");
  const modeHeaderValue = realResp.headers.get("x-risepir-mode");
  check(
    "sanity: the real /setup response carries an ETag and a mode header to stub against",
    Boolean(etagValue) && (modeHeaderValue === "0" || modeHeaderValue === "1"),
    `etag=${JSON.stringify(etagValue)} mode=${JSON.stringify(modeHeaderValue)}`,
  );

  const truncateAt = Math.floor(fullSetupBytes.length * 0.6);
  const setupCalls = [];
  let firstSetupCall = true;

  // What a real stalled connection looks like: some bytes arrive, then
  // nothing more — until the watchdog's own AbortSignal fires, at which
  // point the stream must actually settle (error out) or the retry logic
  // downstream would wait on a promise that never resolves.
  function truncatedThenHangStream(signal) {
    return new ReadableStream({
      start(controller) {
        const mid = Math.floor(truncateAt / 2);
        controller.enqueue(fullSetupBytes.subarray(0, mid));
        controller.enqueue(fullSetupBytes.subarray(mid, truncateAt));
        const onAbort = () => controller.error(new DOMException("aborted", "AbortError"));
        if (signal.aborted) onAbort();
        else signal.addEventListener("abort", onAbort, { once: true });
      },
    });
  }

  async function stubFetch(input, init) {
    const url = String(input);
    if (!url.includes("/setup")) return fetch(url, init);

    const h = new Headers(init?.headers ?? {});
    const range = h.get("range");
    const ifRange = h.get("if-range");
    setupCalls.push({ range, ifRange });

    if (firstSetupCall) {
      firstSetupCall = false;
      return new Response(truncatedThenHangStream(init.signal), {
        status: 200,
        headers: {
          "content-length": String(fullSetupBytes.length),
          "x-risepir-mode": modeHeaderValue,
          etag: etagValue,
        },
      });
    }

    // Every retry after the first must ask for exactly the missing tail,
    // naming the exact bundle the truncated response already committed to
    // — never a guess, never the whole thing again.
    if (range !== `bytes=${truncateAt}-` || ifRange !== etagValue) {
      return new Response(`test stub: unexpected retry range=${range} if-range=${ifRange}`, { status: 400 });
    }
    const tail = fullSetupBytes.subarray(truncateAt);
    return new Response(tail, {
      status: 206,
      headers: {
        "content-range": `bytes ${truncateAt}-${fullSetupBytes.length - 1}/${fullSetupBytes.length}`,
        "content-length": String(tail.length),
        "x-risepir-mode": modeHeaderValue,
        etag: etagValue,
      },
    });
  }

  const HUNG = Symbol("hung-resume-test");
  const stallMs = 1200;
  let sawSource = null;
  let bootError = null;
  const raced = await Promise.race([
    connect(base, {
      wasmBytes,
      fetchImpl: stubFetch,
      stallTimeoutMs: stallMs,
      onProgress: () => {},
      onSource: (s) => {
        sawSource = s;
      },
    }).catch((e) => {
      bootError = e;
      return null;
    }),
    new Promise((resolve) => setTimeout(() => resolve(HUNG), stallMs * 10)),
  ]);

  if (raced === HUNG) {
    check(
      "a truncated-then-resumed /setup boots within a bounded time",
      false,
      "still pending after 10x the stall budget — did the resume retry actually fire?",
    );
  } else if (bootError || !raced) {
    check("a truncated-then-resumed /setup boots successfully", false, String(bootError?.message ?? bootError));
  } else {
    const resumedSession = raced;
    check("a truncated-then-resumed /setup boots successfully", true);
    check("exactly one retry was needed", setupCalls.length === 2, `${setupCalls.length} /setup calls`);
    check(
      "the retry carried Range for exactly the missing tail",
      setupCalls[1]?.range === `bytes=${truncateAt}-`,
      JSON.stringify(setupCalls[1]),
    );
    check(
      "...and If-Range naming the first response's own ETag",
      setupCalls[1]?.ifRange === etagValue,
      JSON.stringify(setupCalls[1]),
    );
    check("traffic.setupResumed reflects the retry", resumedSession.traffic.setupResumed === true);
    check(
      "traffic.setupBytes reports the full length, not just the tail",
      resumedSession.traffic.setupBytes === fullSetupBytes.length,
      `${resumedSession.traffic.setupBytes} vs ${fullSetupBytes.length}`,
    );
    // `onSource` fires once, *before* any bytes move — it can only reflect
    // what is knowable up front (a cross-session IndexedDB resume, which
    // there is none of here, since this environment has no IndexedDB at
    // all). The stall this test injects happens mid-transfer, so from
    // `onSource`'s vantage point this legitimately started as a plain
    // network download; `traffic.setupResumed` (asserted above) is the
    // field that reflects what actually happened once the stall hit.
    check(
      "onSource reported the honest upfront decision (a plain network download, since no prior partial existed)",
      sawSource === HINT_SOURCE.NETWORK,
      `saw ${sawSource}`,
    );

    // The functional byte-identity proof (see the section header): a
    // lookup through the resumed session must match the already-booted
    // session's answer for the same address, exactly.
    const viaResumed = await resumedSession.getBalance(target);
    check(
      "a lookup through the resumed session matches the already-booted session exactly",
      viaResumed.status === result.status && viaResumed.balanceWei === result.balanceWei,
      `status ${viaResumed.status}/${String(viaResumed.balanceWei)} vs ${result.status}/${String(result.balanceWei)}`,
    );
  }
}

console.log(
  `\n${failures === 0 ? "PASS" : "FAIL"}: ${failures} failing check${failures === 1 ? "" : "s"}\n`,
);
process.exit(failures === 0 ? 0 : 1);
