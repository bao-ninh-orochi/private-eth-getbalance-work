// The RisePIR private-eth_getBalance client, host side (ADR-0019).
//
// This file does every byte of I/O; the wasm module does every byte of
// crypto and never touches the network (its *only* import is the entropy
// shim below — `web/test/e2e.mjs` asserts that, because "the client cannot
// phone home" is a property worth pinning rather than asserting).
//
// Deliberately free of any DOM reference, so `web/test/e2e.mjs` can drive
// the identical code under Node against a real server. The UI lives in
// app.js.
//
// ── The conversation, and the one ordering that matters ────────────────
//
// A lookup is four steps (docs/plan.md §3.3). The client's hint is pinned
// at some block₀ and the server answers at its own head E'; the response
// must be corrected by the public delta over (block₀ → E'] *before* it is
// decoded and scanned. So:
//
//   1. GET  /head                 where is the server now?
//   2. GET  /sync?from&to&epoch   fold the public delta in
//   3. POST /answer?epoch         LWE query out, LWE response in
//   4. (sync again if the server moved during 3) then finish
//
// Step 4's second sync is not paranoia: a block can land between the /head
// read and the /answer, and the wasm module refuses to finish against a
// span it cannot prove (it returns an error naming the block, and this
// file syncs and retries once). Nothing here guesses.
//
// `epoch` is the hint-lineage token (ADR-0033), read off the /setup
// response at load() and echoed on every /sync and /answer: a server that
// was re-bootstrapped onto a different hint since this page's /setup
// answers 409 (surfaced as StaleSetupError → "reload the page") instead
// of feeding this client deltas that would decode to garbage against its
// hint — garbage a complete-mode client could surface as a wrong 0x0.

/// Status codes returned by `risepir_finish` — see crates/risepir-wasm.
export const STATUS = Object.freeze({
  FOUND: 0,
  ZERO: 1,
  UNTRACKED: 2,
  DECODE_FAILED: 3,
  ERROR: -1,
});

/// The server no longer retains the deltas this client's hint needs — its
/// delta ring has moved past our pinned block, or it re-bootstrapped onto
/// a different hint entirely. Either way the only sound move is a fresh
/// /setup; reusing the hint would be decoding against the wrong lineage.
export class StaleSetupError extends Error {
  constructor(message) {
    super(message);
    this.name = "StaleSetupError";
  }
}

/// A server-side or protocol failure. Never silently turned into a balance.
export class PirError extends Error {
  constructor(message) {
    super(message);
    this.name = "PirError";
  }
}

function readU64LE(bytes) {
  if (bytes.length !== 8) throw new PirError(`expected 8 bytes, got ${bytes.length}`);
  return new DataView(bytes.buffer, bytes.byteOffset, 8).getBigUint64(0, true);
}

/// The one host capability the wasm module has: cryptographically secure
/// randomness. Everything about the privacy claim rests on this being a
/// real CSPRNG — see crates/risepir-wasm/src/entropy.rs.
function entropyImport(getInstance, counter) {
  return (ptr, len) => {
    const inst = getInstance();
    const view = new Uint8Array(inst.exports.memory.buffer, ptr, len);
    if (typeof crypto === "undefined" || !crypto.getRandomValues) {
      // Refuse rather than degrade. A predictable LWE secret lets the
      // server read the queried bucket straight out of the query.
      throw new Error("no Web Crypto available: refusing to build a PIR query without a CSPRNG");
    }
    // getRandomValues is capped at 65536 bytes per call.
    for (let off = 0; off < len; off += 65536) {
      crypto.getRandomValues(view.subarray(off, Math.min(off + 65536, len)));
    }
    counter.calls += 1;
    counter.bytes += len;
  };
}

export class PirSession {
  #inst;
  #base;
  #fetch;
  /// Counts of entropy-shim invocations; surfaced so the page (and the
  /// e2e test) can show that fresh randomness really is being drawn.
  entropy = { calls: 0, bytes: 0 };
  /// Byte counters for the "what actually left your browser" panel.
  traffic = { setupBytes: 0, queryBytes: 0, responseBytes: 0, deltaBytes: 0 };
  /// Hex of the last 32 bytes of the most recent query bundle — deep in
  /// the LWE payload, past all framing. Kept so the page can show what
  /// actually went out, and so a test can assert that two queries for the
  /// *same* address differ: identical bytes would mean a reused LWE
  /// secret, which is the failure that keeps returning correct balances
  /// while handing the server the bucket you asked for.
  lastQueryTail = "";

  constructor(instance, base, fetchImpl) {
    this.#inst = instance;
    this.#base = base.replace(/\/$/, "");
    this.#fetch = fetchImpl;
  }

  get exports() {
    return this.#inst.exports;
  }

  // ── wasm buffer plumbing ─────────────────────────────────────────────
  //
  // The module owns both buffers; we ask for space, write into it, and
  // call the operation. Every view is created fresh from
  // `memory.buffer` because any wasm call can grow memory and detach the
  // previous ArrayBuffer.

  #writeIn(bytes) {
    const e = this.exports;
    e.risepir_in_reserve(bytes.length);
    const ptr = e.risepir_in_ptr();
    new Uint8Array(e.memory.buffer, ptr, bytes.length).set(bytes);
    return bytes.length;
  }

  #readOut() {
    const e = this.exports;
    const ptr = e.risepir_out_ptr();
    const len = e.risepir_out_len();
    return new Uint8Array(e.memory.buffer, ptr, len).slice();
  }

  #lastError() {
    const e = this.exports;
    const ptr = e.risepir_err_ptr();
    const len = e.risepir_err_len();
    if (len === 0) return "";
    return new TextDecoder().decode(new Uint8Array(e.memory.buffer, ptr, len));
  }

  // ── HTTP ─────────────────────────────────────────────────────────────

  async #get(path, { onProgress, intoWasm, onResponse } = {}) {
    const resp = await this.#fetch(`${this.#base}${path}`);
    if (resp.status === 409) {
      throw new StaleSetupError(await resp.text());
    }
    if (!resp.ok) {
      throw new PirError(`GET ${path}: ${resp.status} ${await resp.text()}`);
    }
    // After the status checks, before any body byte: the one caller that
    // uses this (load()) reads response *headers* whose meaning is
    // defined only for a 200.
    if (onResponse) onResponse(resp);
    if (!intoWasm) {
      return new Uint8Array(await resp.arrayBuffer());
    }

    // The ~50 MB case: stream straight into the wasm input buffer rather
    // than building a second copy in JS. No wasm call happens during the
    // loop, so the pointer and the ArrayBuffer both stay valid.
    const total = Number(resp.headers.get("content-length") ?? 0);
    if (!total || !resp.body) {
      const bytes = new Uint8Array(await resp.arrayBuffer());
      if (onProgress) onProgress(bytes.length, bytes.length);
      return bytes;
    }
    const e = this.exports;
    e.risepir_in_reserve(total);
    const ptr = e.risepir_in_ptr();
    const dest = new Uint8Array(e.memory.buffer, ptr, total);
    const reader = resp.body.getReader();
    let off = 0;
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      if (off + value.length > total) {
        throw new PirError(`GET ${path}: body longer than its Content-Length (${total})`);
      }
      dest.set(value, off);
      off += value.length;
      if (onProgress) onProgress(off, total);
    }
    if (off !== total) {
      throw new PirError(`GET ${path}: truncated (${off} of ${total} bytes)`);
    }
    return { streamedInto: off };
  }

  async #post(path, body) {
    const resp = await this.#fetch(`${this.#base}${path}`, {
      method: "POST",
      headers: { "content-type": "application/octet-stream" },
      body,
    });
    if (resp.status === 409) {
      // /answer's lineage gate (ADR-0033) — same recovery as a stale
      // /sync: only a fresh /setup is sound.
      throw new StaleSetupError(await resp.text());
    }
    if (!resp.ok) {
      throw new PirError(`POST ${path}: ${resp.status} ${await resp.text()}`);
    }
    return new Uint8Array(await resp.arrayBuffer());
  }

  // ── lifecycle ────────────────────────────────────────────────────────

  /// One GET /setup: the completeness flag is read from that response's
  /// own `x-risepir-mode` header (ADR-0033) — mode and bundle from one
  /// atomic response, so the pair cannot straddle a server restart — and
  /// the wasm module still refuses to initialise without the flag, so
  /// "absent means zero" can never be a browser-side default
  /// (ADR-0015/0017). A server predating the header gets the old
  /// two-request sequence as a fallback (`GET /mode`), which reopens
  /// exactly the pre-ADR-0033 race and nothing more.
  async load({ onProgress } = {}) {
    let modeSet = false;
    const streamed = await this.#get("/setup", {
      onProgress,
      intoWasm: true,
      onResponse: (resp) => {
        const m = resp.headers.get("x-risepir-mode");
        if (m === "0" || m === "1") {
          // Buffer-free setter on purpose: this fires while the same
          // response's body is about to stream into the wasm input
          // buffer, which risepir_set_mode would clobber.
          if (this.exports.risepir_set_mode_byte(m === "1" ? 1 : 0) !== 0) {
            throw new PirError(`x-risepir-mode: ${this.#lastError()}`);
          }
          modeSet = true;
        } else if (m !== null) {
          // A garbled flag is fatal, never defaulted: it decides whether
          // absence means 0x0.
          throw new PirError(`x-risepir-mode header was ${JSON.stringify(m)} (expected "0" or "1")`);
        }
      },
    });
    if (!modeSet) {
      // Legacy server. Deliberately after the /setup download: /mode is
      // fetched without touching the wasm input buffer (plain
      // arrayBuffer path) and set via the buffer-free setter, so the
      // setup bytes already streamed in stay intact.
      const mode = await this.#get("/mode");
      if (!(mode.length === 1 && (mode[0] === 0 || mode[0] === 1))) {
        throw new PirError(`GET /mode returned ${mode.length} bytes; expected exactly one byte, 0 or 1`);
      }
      if (this.exports.risepir_set_mode_byte(mode[0]) !== 0) {
        throw new PirError(`GET /mode: ${this.#lastError()}`);
      }
    }

    let len;
    if (streamed instanceof Uint8Array) {
      len = this.#writeIn(streamed);
    } else {
      len = streamed.streamedInto;
    }
    this.traffic.setupBytes = len;
    // risepir_init frees the encoded input buffer itself, between
    // decoding and building — the one point where releasing it actually
    // lowers the tab's permanent footprint (wasm memory never shrinks;
    // see ESTIMATED_PEAK_MULTIPLE's derivation below).
    if (this.exports.risepir_init(len) !== 0) {
      throw new PirError(`GET /setup: ${this.#lastError()}`);
    }

    // The lineage token every /sync and /answer must echo (ADR-0033) —
    // derived by the wasm from the bundle's own seeds, so it cannot
    // disagree with what was just initialised.
    const elen = this.exports.risepir_epoch();
    if (elen < 0n) {
      throw new PirError(`epoch: ${this.#lastError()}`);
    }
    this.epoch = new TextDecoder().decode(this.#readOut());
    return this;
  }

  get complete() {
    return this.exports.risepir_complete() === 1;
  }
  get pinnedBlock() {
    return this.exports.risepir_pinned_block();
  }
  get pendingHead() {
    return this.exports.risepir_pending_head();
  }
  get arity() {
    return this.exports.risepir_arity();
  }
  get deltaCells() {
    return this.exports.risepir_delta_cells();
  }

  /// The server's current block. Public, address-free.
  async head() {
    return readU64LE(await this.#get("/head"));
  }

  /// Recently-touched addresses the deployment tracks (public chain data;
  /// see NodeState's `recent` field). `[]` if the server serves none.
  async recent() {
    const bytes = await this.#get("/recent");
    if (bytes.length < 4) return [];
    const count = new DataView(bytes.buffer, bytes.byteOffset, 4).getUint32(0, true);
    const out = [];
    for (let i = 0; i < count && 4 + i * 20 + 20 <= bytes.length; i++) {
      out.push(toHexAddress(bytes.subarray(4 + i * 20, 4 + i * 20 + 20)));
    }
    return out;
  }

  /// Fold the public delta over (pendingHead, to] into the client. The
  /// epoch param is the lineage gate (ADR-0033): the server 409s rather
  /// than feed this client another bootstrap's deltas.
  async #syncTo(to) {
    const from = this.pendingHead;
    if (to <= from) return;
    const bytes = await this.#get(`/sync?from=${from}&to=${to}&epoch=${this.epoch}`);
    this.traffic.deltaBytes += bytes.length;
    const head = this.exports.risepir_ingest(this.#writeIn(bytes));
    if (head < 0n) {
      throw new PirError(`sync (${from}, ${to}]: ${this.#lastError()}`);
    }
  }

  /// One private balance lookup.
  ///
  /// Returns `{ status, balanceWei, atBlock }`. The caller must branch on
  /// `status`: UNTRACKED and DECODE_FAILED are *errors*, not zero — the
  /// module returns them as distinct codes precisely so a UI cannot
  /// collapse them into "0 ETH" by accident.
  async getBalance(address) {
    const addr = parseAddress(address);
    const e = this.exports;

    await this.#syncTo(await this.head());

    const qlen = e.risepir_query(this.#writeIn(addr));
    if (qlen < 0n) throw new PirError(this.#lastError());
    const query = this.#readOut();
    this.traffic.queryBytes = query.length;
    this.lastQueryTail = Array.from(query.subarray(Math.max(0, query.length - 32)))
      .map((b) => b.toString(16).padStart(2, "0"))
      .join("");

    const response = await this.#post(`/answer?epoch=${this.epoch}`, query);
    this.traffic.responseBytes = response.length;
    const atBlock = e.risepir_answer(this.#writeIn(response));
    if (atBlock < 0n) throw new PirError(this.#lastError());

    // The server may have advanced between /head and /answer. Catch up to
    // exactly the block it answered at, then finish. One retry, not a
    // loop: the module names the block it needs, and after syncing to
    // that exact block the requirement is met by construction.
    let status = e.risepir_finish();
    if (status === STATUS.ERROR) {
      const why = this.#lastError();
      if (!why.includes("sync")) throw new PirError(why);
      await this.#syncTo(atBlock);
      status = e.risepir_finish();
      if (status === STATUS.ERROR) throw new PirError(this.#lastError());
    }

    let balanceWei = null;
    if (status === STATUS.FOUND) {
      const out = this.#readOut();
      if (out.length !== 16) throw new PirError(`balance was ${out.length} bytes, expected 16`);
      let v = 0n;
      for (let i = 15; i >= 0; i--) v = (v << 8n) | BigInt(out[i]);
      balanceWei = v;
    } else if (status === STATUS.ZERO) {
      balanceWei = 0n;
    }
    return { status, balanceWei, atBlock };
  }
}

/// Instantiate the wasm client and bootstrap it against `base`.
///
/// `wasmBytes` may be supplied directly (Node); otherwise `wasmUrl` is
/// fetched. Streaming instantiation is used when the server sends the
/// right content type, which our own does.
export async function connect(base, { wasmUrl = "client.wasm", wasmBytes, fetchImpl, onProgress } = {}) {
  const doFetch = fetchImpl ?? ((...args) => fetch(...args));
  let instance = null;
  const counter = { calls: 0, bytes: 0 };
  const imports = { env: { risepir_fill_random: entropyImport(() => instance, counter) } };

  if (wasmBytes) {
    ({ instance } = await WebAssembly.instantiate(wasmBytes, imports));
  } else {
    ({ instance } = await WebAssembly.instantiateStreaming(doFetch(wasmUrl), imports));
  }

  const session = new PirSession(instance, base, doFetch);
  session.entropy = counter;
  await session.load({ onProgress });
  return session;
}

// ── small helpers ──────────────────────────────────────────────────────

/// Parse `0x`-prefixed 40-hex-character address into 20 bytes. Rejects
/// anything else — a malformed address must not become a query.
export function parseAddress(address) {
  const s = String(address).trim();
  if (!/^0x[0-9a-fA-F]{40}$/.test(s)) {
    throw new PirError("not an Ethereum address: expected 0x followed by 40 hex characters");
  }
  const out = new Uint8Array(20);
  for (let i = 0; i < 20; i++) out[i] = parseInt(s.slice(2 + i * 2, 4 + i * 2), 16);
  return out;
}

export function toHexAddress(bytes) {
  let s = "0x";
  for (const b of bytes) s += b.toString(16).padStart(2, "0");
  return s;
}

/// Exact wei → ETH, as a decimal string. BigInt throughout: a balance can
/// exceed what a double represents exactly, and this project does not
/// round balances.
export function formatEth(wei) {
  const neg = wei < 0n;
  const v = neg ? -wei : wei;
  const whole = v / 1_000_000_000_000_000_000n;
  const frac = (v % 1_000_000_000_000_000_000n).toString().padStart(18, "0").replace(/0+$/, "");
  return `${neg ? "-" : ""}${whole.toLocaleString("en-US")}${frac ? `.${frac}` : ""}`;
}

// ── capacity pre-flight (ADR-0032) ──────────────────────────────────────
//
// A pure decision, deliberately free of DOM/`navigator`/`fetch` — exactly
// the discipline the rest of this file already keeps, and for the same
// reason: `web/test/e2e.mjs` needs to drive it directly under plain Node,
// no browser and no server involved.
//
// This would ordinarily be its own file (`web/capacity.js`, mirroring the
// app.js/pir.js split) — a small pure function like this is exactly what
// that separation is for. It lives here instead because every file the
// browser can fetch has to be named in `crates/risepir-http/src/web.rs`'s
// fixed asset `MANIFEST`: that module maps a *fixed* set of routes to a
// fixed set of filenames read once at startup, on purpose ("no request-
// path-to-filesystem-path translation, ever" — ADR-0019), with no
// directory-listing fallback to ride a new file in on. Adding one more
// route is a one-line, non-behavioral change, but it is a change to a Rust
// crate, and this task's brief rules that out categorically. Hosting the
// decision here instead — already served, already DOM/navigator/fetch-free,
// already imported by both `app.js` and `web/test/e2e.mjs` — gets every
// property the separate-module design was actually for (pure, testable
// under plain Node, one source of truth) without a new server route.
// `app.js` gathers `HEAD /setup`, `navigator`, and the viewport; this
// function never touches any of them directly.

/// What `assessCapacity` can decide. `warn` and `refuse` both leave the
/// download exactly one click away (`app.js`'s "Download anyway") — this
/// is advice, never a lock-out.
export const CAPACITY_VERDICT = Object.freeze({ OK: "ok", WARN: "warn", REFUSE: "refuse" });

/// The tab's estimated peak memory, as a multiple of the hint download
/// size — and in wasm, whose linear memory never shrinks, the peak IS the
/// tab's resident floor from then on.
///
/// Derived from `risepir_init`'s actual allocation sequence (see that
/// function in crates/risepir-wasm — the two must move together), not
/// from steady state: (1) the encoded bundle streams into the wasm input
/// buffer (1x); (2) decoding builds the owned bundle beside it (peak ~2x);
/// (3) the input buffer is freed *before* the client is built, and the
/// decoded hints are consumed per segment while the client's own hint
/// copy and the expanded `A` (each ~1x — docs/numbers.md §4c measures
/// `A`+hint at ~2.00-2.03x hint) accumulate — peaking near 2.4x mid-build.
/// 3 is that worst phase rounded up for allocator fragmentation, which
/// wasm never gives back.
///
/// This replaces an earlier value of 2, which was calibrated against
/// §4c's *steady-state* figure and therefore waved 4 GB phones
/// (`deviceMemory` 4 → 2.0 GB budget) into a download whose true peak —
/// then ~4x, before the two init-sequence fixes above — killed the
/// renderer after the data was already spent: the exact pre-ADR-0032
/// failure the gate exists to prevent. This constant is a *ratio*; the
/// hint size itself is never hardcoded here, or anywhere in this file —
/// it is read fresh, per deployment, from `HEAD /setup`'s
/// `Content-Length` (ADR-0032, point 1).
export const ESTIMATED_PEAK_MULTIPLE = 3;

/// The share of the device's *total* memory one browser tab may reasonably
/// claim. A tab shares the device with the OS, the browser's own overhead,
/// and — on a phone — every other app running, so this can never be close
/// to 1. Chosen so the number a comfortably-capable desktop actually
/// reports still clears the complete set's cost: `navigator.deviceMemory`
/// is capped at 8 regardless of real installed RAM (rounded down for
/// privacy — a 32 GB machine reports the same 8 a device with exactly 8
/// GB does), and 8 * 0.5 = 4 GB clears the ~2.5 GB complete-set *peak*
/// estimate (3x hint — see `ESTIMATED_PEAK_MULTIPLE`) — so this fraction
/// is never *itself* the reason a real desktop gets turned away. At the
/// other end, a phone reporting 4 has a 2.0 GB budget, genuinely and
/// correctly below that same ~2.5 GB peak — that phone is exactly who
/// this gate exists for.
export const USABLE_MEMORY_FRACTION = 0.5;

/// Below this estimated peak, a `deviceMemory`-less visitor (Safari,
/// Firefox — see below) is never even offered the softer coarse-signal
/// warning — comfortably above every demo-scale deployment this repo ships
/// (49 MB at `--partial-capacity 1000000`, ~1.77 MB for `mock`) and
/// comfortably below the real complete-set hint, so only a deployment
/// actually built at that scale can ever reach it.
export const COARSE_SIGNAL_WARN_PEAK_BYTES = 200_000_000;

/// A viewport this narrow reads as a phone/small-tablet layout — one of the
/// coarse fallback signals consulted only when `deviceMemory` is
/// unavailable.
export const SMALL_VIEWPORT_WIDTH_PX = 600;

const BYTES_PER_GB = 1_000_000_000;

/// Decide whether to auto-start the hint download, given this deployment's
/// true cost and whatever the device is willing to say about itself. Pure:
/// every input is a plain value `app.js` already gathered from `fetch` /
/// `navigator` / the viewport, and this function touches none of them
/// directly — which is what lets `web/test/e2e.mjs` call it straight under
/// plain Node, no browser and no server.
///
///  - `hintBytes` — the exact `Content-Length` of `GET /setup` for *this*
///    deployment (a `HEAD` request reads it without paying for the
///    transfer itself). The caller is expected to skip calling this
///    function at all when the probe failed or omitted the header (point 1
///    of ADR-0032); passing a missing/zero value through anyway still
///    degrades to `ok` rather than manufacturing a refusal.
///  - `deviceMemoryGb` — `navigator.deviceMemory`, or `null`/`undefined`
///    exactly where the browser does not expose it (Safari, Firefox) —
///    never guessed at, never defaulted to a number.
///  - `coarsePointer` — `true` if the device looks touch-primary
///    (`navigator.maxTouchPoints > 0`, or a `(pointer: coarse)` media
///    match); only ever consulted when `deviceMemoryGb` is unavailable.
///  - `viewportWidth` — `window.innerWidth`, or `null`/`undefined`; same
///    caveat.
///  - `saveData` — `navigator.connection.saveData === true`: the user
///    explicitly asked user agents to spend less data. Memory can be
///    plentiful and the download still unwanted, so this can downgrade an
///    otherwise-`ok` verdict on a large deployment to `warn` (never to
///    `refuse` — it is a preference, and the gate is advice either way).
///
/// Returns `{ verdict, hintBytes, estimatedPeakBytes, deviceMemoryGb,
/// budgetBytes, coarseSignal, saveData, basis }` — the verdict plus every
/// number that went into it, so `app.js` can render the real figures
/// instead of re-deriving them.
export function assessCapacity({ hintBytes, deviceMemoryGb, coarsePointer, viewportWidth, saveData } = {}) {
  const hint = Number(hintBytes);
  const estimatedPeakBytes = (Number.isFinite(hint) && hint > 0 ? hint : 0) * ESTIMATED_PEAK_MULTIPLE;
  const wantsDataSavings = Boolean(saveData);

  // The user's own stated preference outranks a machine-guessed "this is
  // probably fine" — but only on a deployment big enough to matter (the
  // same threshold as the coarse-signal warning), and never as a refusal.
  const saveDataDowngrade = (base) =>
    base.verdict === CAPACITY_VERDICT.OK && wantsDataSavings && base.hintBytes > COARSE_SIGNAL_WARN_PEAK_BYTES
      ? { ...base, verdict: CAPACITY_VERDICT.WARN, basis: "save-data" }
      : base;

  const haveDeviceMemory =
    typeof deviceMemoryGb === "number" && Number.isFinite(deviceMemoryGb) && deviceMemoryGb > 0;

  if (haveDeviceMemory) {
    const budgetBytes = deviceMemoryGb * BYTES_PER_GB * USABLE_MEMORY_FRACTION;
    return saveDataDowngrade({
      verdict: estimatedPeakBytes > budgetBytes ? CAPACITY_VERDICT.REFUSE : CAPACITY_VERDICT.OK,
      hintBytes: hint,
      estimatedPeakBytes,
      deviceMemoryGb,
      budgetBytes,
      coarseSignal: false,
      saveData: wantsDataSavings,
      basis: "device-memory",
    });
  }

  // No real number to compare against: a missing deviceMemory must never
  // become a refusal (point 4 of ADR-0032) — REFUSE simply does not appear
  // as a possible outcome of this branch. Only a strong-enough coarse
  // signal, on a deployment large enough to matter, downgrades "ok" to a
  // soft "warn"; everything else — including every desktop browser that
  // simply does not implement deviceMemory at all (Firefox, at any window
  // size) — is indistinguishable from today's unconditional connect.
  const coarseSignal =
    Boolean(coarsePointer) ||
    (typeof viewportWidth === "number" && viewportWidth > 0 && viewportWidth < SMALL_VIEWPORT_WIDTH_PX);
  const warrantsWarning = coarseSignal && estimatedPeakBytes > COARSE_SIGNAL_WARN_PEAK_BYTES;
  return saveDataDowngrade({
    verdict: warrantsWarning ? CAPACITY_VERDICT.WARN : CAPACITY_VERDICT.OK,
    hintBytes: hint,
    estimatedPeakBytes,
    deviceMemoryGb: null,
    budgetBytes: null,
    coarseSignal,
    saveData: wantsDataSavings,
    basis: coarseSignal ? "coarse-signal" : "no-signal",
  });
}
