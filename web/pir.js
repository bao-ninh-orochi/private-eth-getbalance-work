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
//   2. GET  /sync?from&to         fold the public delta in
//   3. POST /answer               LWE query out, LWE response in
//   4. (sync again if the server moved during 3) then finish
//
// Step 4's second sync is not paranoia: a block can land between the /head
// read and the /answer, and the wasm module refuses to finish against a
// span it cannot prove (it returns an error naming the block, and this
// file syncs and retries once). Nothing here guesses.

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

  async #get(path, { onProgress, intoWasm } = {}) {
    const resp = await this.#fetch(`${this.#base}${path}`);
    if (resp.status === 409) {
      throw new StaleSetupError(await resp.text());
    }
    if (!resp.ok) {
      throw new PirError(`GET ${path}: ${resp.status} ${await resp.text()}`);
    }
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
    if (!resp.ok) {
      throw new PirError(`POST ${path}: ${resp.status} ${await resp.text()}`);
    }
    return new Uint8Array(await resp.arrayBuffer());
  }

  // ── lifecycle ────────────────────────────────────────────────────────

  /// GET /mode then GET /setup, in that order — the completeness flag is
  /// loaded before the client exists, and the wasm module refuses to
  /// initialise without it, so "absent means zero" can never be a
  /// browser-side default (ADR-0015/0017).
  async load({ onProgress } = {}) {
    const mode = await this.#get("/mode");
    if (this.exports.risepir_set_mode(this.#writeIn(mode)) !== 0) {
      throw new PirError(`GET /mode: ${this.#lastError()}`);
    }

    const streamed = await this.#get("/setup", { onProgress, intoWasm: true });
    let len;
    if (streamed instanceof Uint8Array) {
      len = this.#writeIn(streamed);
    } else {
      len = streamed.streamedInto;
    }
    this.traffic.setupBytes = len;
    if (this.exports.risepir_init(len) !== 0) {
      throw new PirError(`GET /setup: ${this.#lastError()}`);
    }
    // The setup bundle is the biggest thing this page ever holds twice.
    this.exports.risepir_in_release();
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

  /// Fold the public delta over (pendingHead, to] into the client.
  async #syncTo(to) {
    const from = this.pendingHead;
    if (to <= from) return;
    const bytes = await this.#get(`/sync?from=${from}&to=${to}`);
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

    const response = await this.#post("/answer", query);
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
