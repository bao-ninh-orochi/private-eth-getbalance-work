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

/// The wire went silent: a request neither completed nor failed within
/// [`STALL_TIMEOUT_MS`]. A `PirError` subtype because it is still "the
/// lookup did not happen", but a *named* one because the recovery is
/// different from every other failure here — the session, its hint and its
/// epoch are all still perfectly good, so the honest move is to retry the
/// query, never to re-download the hint.
export class TimeoutError extends PirError {
  constructor(message) {
    super(message);
    this.name = "TimeoutError";
  }
}

/// How long a request may make *no progress at all* before it is abandoned.
///
/// This bounds **silence, not duration** — the watchdog below re-arms on
/// every byte received — because the two are wildly different here:
/// `GET /setup` is 553.82 MB at the live complete-mainnet set and
/// legitimately runs for minutes, so any total deadline generous enough
/// for it would be no bound at all for the small endpoints. A download
/// that is slow but *moving* never trips this; a socket that has died
/// (laptop sleep, a Wi-Fi change, a network switch — the case that
/// wedged the page before this existed) trips it in 45 s.
///
/// 45 s specifically, and above the server's own 30 s handler bound
/// (`REQUEST_TIMEOUT`, crates/risepir-http/src/node.rs): a client budget
/// *under* that would abort requests the server was still about to answer
/// honestly, turning a server-labelled `408` into a client-side guess.
/// The Rust client has had exactly this (`READ_STALL_TIMEOUT`,
/// crates/risepir-http/src/client.rs) since it shipped; the browser was
/// the outlier. See ADR-0035.
export const STALL_TIMEOUT_MS = 45_000;

/// An `AbortSignal` that fires after `ms` of no progress. `progress()`
/// re-arms it; `stop()` disarms it for good. `fired` distinguishes "we
/// aborted this" from an abort or network error that came from anywhere
/// else, so a stall is never misreported as a protocol failure.
function stallWatchdog(ms) {
  const controller = new AbortController();
  const w = { signal: controller.signal, fired: false };
  let timer = null;
  w.progress = () => {
    clearTimeout(timer);
    timer = setTimeout(() => {
      w.fired = true;
      controller.abort();
    }, ms);
  };
  w.stop = () => clearTimeout(timer);
  w.progress();
  return w;
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

// ── persistent hint cache (IndexedDB, ADR-0038) ─────────────────────────
//
// `GET /setup` is the one large transfer this page ever makes — up to
// 553.82 MB at the deployed complete-mainnet geometry (docs/numbers.md
// §4c) — and until this section existed nothing on this page persisted
// it: a reload, a return visit, or a connection dropped at 99% each paid
// the full cost again. This persists the raw encoded bytes in IndexedDB,
// keyed by the hint-lineage epoch (ADR-0033), in ~16 MiB chunks.
//
// IndexedDB, not the Cache API, and deliberately so. Caching a response
// whose body can run to hundreds of MB via `caches.put(request,
// response)` needs the platform to buffer the whole body somewhere before
// the entry is committed — either `response.clone()`/`body.tee()` (an
// unbounded second in-flight copy racing the first) or reading the body
// fully into a `Blob` first. Either shape adds a second ~554 MB buffer at
// exactly the moment ADR-0032's capacity pre-flight is budgeting the wasm
// init peak (`ESTIMATED_PEAK_MULTIPLE` below) against the device's
// memory — precisely the number this project already had to fight hard to
// keep honest. A chunked IndexedDB store writes one ~16 MiB chunk at a
// time (bounded transient memory, no second whole-body buffer) and gets
// resume-across-*sessions* for free, as a side effect of being keyed by
// byte offset at all.
//
// The epoch — never the block a bundle happens to be pinned at — is the
// cache key, and the server, never the cache, stays the authority on
// whether a cached entry is still usable:
//
//   - A *complete* entry is only ever read after confirming the live
//     `x-risepir-mode` (from a cheap, fresh `GET /head`) agrees with what
//     was cached; a disagreement evicts rather than trusts the cache.
//   - Every partial-download resume sends `If-Range` naming the exact
//     `ETag` the bytes were downloaded against; the server refuses to
//     bridge a `Range` across a cache regeneration
//     (`crates/risepir-http/src/node.rs`'s `setup` handler, ADR-0038), so a
//     stale resume falls back to a full re-download rather than splicing
//     two bundles together.
//   - Every `/sync`/`/answer` `409` (`StaleSetupError`) evicts this
//     epoch's cache entry immediately, and the decoded bundle's *own*
//     `risepir_epoch()` is asserted against the epoch the cache was keyed
//     on as a hard error, never a warning — so a corrupt or mismatched
//     cache entry can only ever become a slower answer, never a wrong one.
//
// At most one epoch's data is ever retained: starting a fresh download for
// a new epoch clears the whole store first, so a browser tab can never
// accumulate an unbounded history of past bootstraps.
//
// Every function below degrades to "as if no cache existed" on any
// failure — `indexedDB` undefined (required: `web/test/e2e.mjs` runs this
// file under plain Node, which has none), an open failure, a
// `QuotaExceededError` mid-write, a missing or short chunk, a byte count
// that disagrees with what was recorded — none of these ever throw out of
// this section into the boot path; see each function's own try/catch.

const IDB_NAME = "risepir-hint-cache";
const IDB_VERSION = 1;
const IDB_META_STORE = "meta";
const IDB_CHUNK_STORE = "chunks";

/// ~16 MiB: large enough that flushing to IndexedDB is a rare event on a
/// multi-hundred-MB hint (tens of writes, not thousands), small enough
/// that a stall mid-chunk loses at most one chunk's worth of otherwise-
/// uncommitted bytes on resume.
const CHUNK_SIZE = 16 * 1024 * 1024;

/// How many times a stalled `/setup` transfer may resume itself with a
/// `Range` continuation (ADR-0038 point 7) before giving up and surfacing
/// the same `TimeoutError` a stall always has. Bounds a flapping
/// connection to a handful of retries rather than an unbounded loop.
const MAX_SETUP_RESUME_ATTEMPTS = 3;

/// Where the hint bytes now backing a session came from — surfaced via
/// `connect`'s `onSource` callback so the boot UI can say so truthfully
/// instead of showing a download rate/ETA that means nothing for a cache
/// hit.
export const HINT_SOURCE = Object.freeze({ CACHE: "cache", NETWORK: "network", RESUME: "resume" });

function idbRequest(req) {
  return new Promise((resolve, reject) => {
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error ?? new Error("IndexedDB request failed"));
  });
}

function idbTxDone(tx) {
  return new Promise((resolve, reject) => {
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error ?? new Error("IndexedDB transaction failed"));
    tx.onabort = () => reject(tx.error ?? new Error("IndexedDB transaction aborted"));
  });
}

/// Opens (and, on first use, creates) the hint cache database. `null` on
/// anything short of success — no `indexedDB` global, a blocked or
/// refused open, or any other failure — which every caller treats
/// identically to "no cache is available", never an exception that could
/// reach the boot path.
///
/// Two stores: `meta` keyed by `epoch` (one record:
/// `{epoch, mode, pinnedBlock, etag, totalBytes, bytesWritten, complete,
/// updatedAt}`), and `chunks` keyed by the compound `[epoch, index]` (one
/// record per ~[`CHUNK_SIZE`] slice: `{epoch, index, bytes}`).
async function idbOpen() {
  if (typeof indexedDB === "undefined") return null;
  try {
    const req = indexedDB.open(IDB_NAME, IDB_VERSION);
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains(IDB_META_STORE)) {
        db.createObjectStore(IDB_META_STORE, { keyPath: "epoch" });
      }
      if (!db.objectStoreNames.contains(IDB_CHUNK_STORE)) {
        db.createObjectStore(IDB_CHUNK_STORE, { keyPath: ["epoch", "index"] });
      }
    };
    return await idbRequest(req);
  } catch {
    return null;
  }
}

async function idbGetMeta(db, epoch) {
  if (!db) return null;
  try {
    const tx = db.transaction(IDB_META_STORE, "readonly");
    const rec = await idbRequest(tx.objectStore(IDB_META_STORE).get(epoch));
    return rec ?? null;
  } catch {
    return null;
  }
}

/// `false` on any failure — including a mid-write `QuotaExceededError`,
/// which is expected rather than exceptional: a browser is always free to
/// refuse or evict a disposable cache under storage pressure, and the
/// caller's only obligation is to keep working without it, never to retry
/// or let the failure surface.
async function idbPutMeta(db, meta) {
  if (!db) return false;
  try {
    const tx = db.transaction(IDB_META_STORE, "readwrite");
    tx.objectStore(IDB_META_STORE).put(meta);
    await idbTxDone(tx);
    return true;
  } catch {
    return false;
  }
}

async function idbGetChunk(db, epoch, index) {
  if (!db) return null;
  try {
    const tx = db.transaction(IDB_CHUNK_STORE, "readonly");
    const rec = await idbRequest(tx.objectStore(IDB_CHUNK_STORE).get([epoch, index]));
    return rec ? rec.bytes : null;
  } catch {
    return null;
  }
}

async function idbPutChunk(db, epoch, index, bytes) {
  if (!db) return false;
  try {
    const tx = db.transaction(IDB_CHUNK_STORE, "readwrite");
    tx.objectStore(IDB_CHUNK_STORE).put({ epoch, index, bytes });
    await idbTxDone(tx);
    return true;
  } catch {
    return false;
  }
}

/// Wipes both stores unconditionally — safe because at most one epoch's
/// data is ever retained (see the section docs above), so "clear
/// everything" and "clear whichever other epoch was here" are the same
/// operation. Called before the first byte of a *fresh* download for a
/// new epoch; never called when resuming that same epoch's own partial
/// entry.
async function idbClearAll(db) {
  if (!db) return;
  try {
    const tx = db.transaction([IDB_META_STORE, IDB_CHUNK_STORE], "readwrite");
    tx.objectStore(IDB_META_STORE).clear();
    tx.objectStore(IDB_CHUNK_STORE).clear();
    await idbTxDone(tx);
  } catch {
    // Best-effort: a failure here only risks retaining a foreign epoch's
    // bytes until the next successful clear, never a correctness problem
    // — every read is re-validated against the live epoch before use.
  }
}

/// Deletes the cached entry for `epoch` specifically — used when the live
/// server has just said (a `409`, or a mode disagreement) that this
/// epoch's cached bytes can no longer be trusted. Guarded on the stored
/// meta actually naming `epoch`, so this can never clobber a *different*
/// epoch's legitimately fresher entry (e.g. another tab already moved on).
async function idbEvictEpoch(db, epoch) {
  if (!db || !epoch) return;
  const meta = await idbGetMeta(db, epoch);
  if (!meta || meta.epoch !== epoch) return;
  await idbClearAll(db);
}

/// Pulls the block number out of a `"setup-<epoch>-<block>"` ETag —
/// informational only (recorded in the cache metadata so a stored entry
/// is legible without decoding the bundle), never load-bearing: the ETag
/// *string itself*, not this parsed number, is what `If-Range` echoes back
/// to the server. `null` for anything that does not match, rather than a
/// guess.
function blockFromEtag(etag) {
  const m = /^"?setup-[0-9a-f]+-([0-9]+)"?$/.exec(etag ?? "");
  return m ? Number(m[1]) : null;
}

/// A crude sanity bound on any byte-length read back from IndexedDB before
/// it is ever used to size a wasm allocation (`risepir_in_reserve`) — this
/// project's "validate every length before allocating" discipline
/// (`CLAUDE.md`), applied here to a store that is nominally trusted
/// (written by this same code) but not immune to disk corruption or a
/// curious user editing it by hand in DevTools. 16 GiB is arbitrary but
/// generous: comfortably above any hint this project has ever measured
/// (553.82 MB at the complete mainnet set, `docs/numbers.md` §4c) with
/// headroom for years of account growth, while still catching a clearly-
/// impossible value before it reaches a wasm call that would otherwise
/// attempt to honour it literally.
const MAX_SANE_CACHE_BYTES = 16 * 1024 * 1024 * 1024;

function isSaneByteLength(n) {
  return Number.isInteger(n) && n > 0 && n <= MAX_SANE_CACHE_BYTES;
}

export class PirSession {
  #inst;
  #base;
  #fetch;
  /// The hint cache database this session opened at `load()` time, or
  /// `null` if unavailable — kept so a later `/sync`/`/answer` `409` can
  /// evict this session's own cached entry (see `#evictCacheOnStale`).
  #db = null;
  /// The epoch this session's cache entry (if any) was keyed on, so the
  /// same eviction path can name the right record.
  #cacheEpoch = null;
  /// Counts of entropy-shim invocations; surfaced so the page (and the
  /// e2e test) can show that fresh randomness really is being drawn.
  entropy = { calls: 0, bytes: 0 };
  /// Byte counters for the "what actually left your browser" panel.
  traffic = {
    setupBytes: 0,
    queryBytes: 0,
    responseBytes: 0,
    deltaBytes: 0,
    /// `true` when `setupBytes` came from this browser's IndexedDB cache
    /// rather than the network (ADR-0038).
    setupFromCache: false,
    /// `true` when the hint download resumed a previous attempt — either
    /// a partial entry left over from an earlier session, or an
    /// in-session retry after a stall (ADR-0035/ADR-0038) — rather than
    /// starting at byte 0.
    setupResumed: false,
  };
  /// Hex of the last 32 bytes of the most recent query bundle — deep in
  /// the LWE payload, past all framing. Kept so the page can show what
  /// actually went out, and so a test can assert that two queries for the
  /// *same* address differ: identical bytes would mean a reused LWE
  /// secret, which is the failure that keeps returning correct balances
  /// while handing the server the bucket you asked for.
  lastQueryTail = "";
  /// Per-request silence budget; [`STALL_TIMEOUT_MS`] unless `connect`
  /// was given an override (which only the tests do — they need a stall
  /// to be observable in seconds rather than in 45 of them).
  stallTimeoutMs = STALL_TIMEOUT_MS;

  constructor(instance, base, fetchImpl, stallTimeoutMs = STALL_TIMEOUT_MS) {
    this.#inst = instance;
    this.#base = base.replace(/\/$/, "");
    this.#fetch = fetchImpl;
    this.stallTimeoutMs = stallTimeoutMs;
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

  /// Reserves the wasm input buffer for exactly `len` bytes and returns a
  /// fresh `Uint8Array` view over it, ready for the caller to `.set()`
  /// into at arbitrary offsets — the pattern the `/setup` streaming and
  /// cache-replay paths both need, factored out of what used to be
  /// `#getWatched`'s own inline reserve/view pair. Must be called at most
  /// once per logical write (any further wasm call can move memory and
  /// detach the view — see the class docs above `#writeIn`).
  #reserveInput(len) {
    const e = this.exports;
    e.risepir_in_reserve(len);
    const ptr = e.risepir_in_ptr();
    return new Uint8Array(e.memory.buffer, ptr, len);
  }

  /// A `/sync` or `/answer` `409` (`StaleSetupError`) means this session's
  /// hint is no longer bridgeable — the only sound recovery is a fresh
  /// `/setup`, so whatever this session cached under its own epoch must go
  /// with it: keeping it around would just make the *next* boot replay the
  /// same now-useless bytes before falling back to the network anyway.
  /// Best-effort and never thrown from: a cache that fails to evict is not
  /// a new failure, only a slightly slower future boot.
  async #evictCacheOnStale() {
    if (this.#db && this.epoch) {
      await idbEvictEpoch(this.#db, this.epoch).catch(() => {});
    }
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

  /// Every plain (small-body) `GET` this class makes — `/head`, `/mode`,
  /// `/recent`, `/sync`. `/setup` is the one exception: its own streaming,
  /// resumable, cache-aware fetch lives entirely in
  /// `#attemptSetupDownload`/`#tryReadCacheIntoWasm` (ADR-0038), since none
  /// of those bodies are small enough — or safe enough to retry naively —
  /// to share this path.
  async #get(path, { onResponse } = {}) {
    const watch = stallWatchdog(this.stallTimeoutMs);
    try {
      return await this.#getWatched(path, watch, { onResponse });
    } catch (e) {
      throw watch.fired ? new TimeoutError(`GET ${path}: no response for ${this.stallTimeoutMs / 1000}s`) : e;
    } finally {
      watch.stop();
    }
  }

  async #getWatched(path, watch, { onResponse } = {}) {
    const resp = await this.#fetch(`${this.#base}${path}`, { signal: watch.signal });
    watch.progress();
    if (resp.status === 409) {
      const text = await resp.text();
      await this.#evictCacheOnStale();
      throw new StaleSetupError(text);
    }
    if (!resp.ok) {
      throw new PirError(`GET ${path}: ${resp.status} ${await resp.text()}`);
    }
    // After the status checks, before any body byte: the one caller that
    // uses this (`#probeHead`) reads response *headers* whose meaning is
    // defined only for a 200.
    if (onResponse) onResponse(resp);
    return new Uint8Array(await resp.arrayBuffer());
  }

  async #post(path, body) {
    const watch = stallWatchdog(this.stallTimeoutMs);
    try {
      return await this.#postWatched(path, body, watch);
    } catch (e) {
      throw watch.fired ? new TimeoutError(`POST ${path}: no response for ${this.stallTimeoutMs / 1000}s`) : e;
    } finally {
      watch.stop();
    }
  }

  async #postWatched(path, body, watch) {
    const resp = await this.#fetch(`${this.#base}${path}`, {
      method: "POST",
      headers: { "content-type": "application/octet-stream" },
      body,
      signal: watch.signal,
    });
    watch.progress();
    if (resp.status === 409) {
      // /answer's lineage gate (ADR-0033) — same recovery as a stale
      // /sync: only a fresh /setup is sound, so this epoch's cache entry
      // (if any) is evicted right alongside the error (ADR-0038).
      const text = await resp.text();
      await this.#evictCacheOnStale();
      throw new StaleSetupError(text);
    }
    if (!resp.ok) {
      throw new PirError(`POST ${path}: ${resp.status} ${await resp.text()}`);
    }
    return new Uint8Array(await resp.arrayBuffer());
  }

  // ── lifecycle ────────────────────────────────────────────────────────

  /// A cheap `GET /head` solely to learn this deployment's current
  /// `(epoch, mode)` pair (ADR-0038) before deciding whether a cached
  /// `/setup` entry even applies — far cheaper than asking `/setup`
  /// itself, which can pay `NodeState::setup_bytes`'s cache-regeneration
  /// cost (~10 s CPU at the complete set). Either field can come back
  /// `null` against a server predating ADR-0033/ADR-0038's headers; every
  /// caller treats that exactly like "no cache decision is possible from
  /// this alone" rather than guessing.
  async #probeHead() {
    let epoch = null;
    let mode = null;
    const bytes = await this.#get("/head", {
      onResponse: (resp) => {
        epoch = resp.headers.get("x-risepir-epoch");
        const m = resp.headers.get("x-risepir-mode");
        mode = m === "0" ? 0 : m === "1" ? 1 : null;
      },
    });
    return { block: readU64LE(bytes), epoch, mode };
  }

  /// Replays a *complete* cached entry straight into the wasm input
  /// buffer — no network at all. Returns `{ mode }` on success, or `null`
  /// on absolutely any failure (a mode disagreement with the live `/head`
  /// header, a missing/short chunk, a byte count that does not match what
  /// was recorded): every failure here just means "not a cache hit after
  /// all", falling through to the ordinary network path in
  /// `#loadSetupBytes`, never an exception that reaches the boot path.
  ///
  /// `head.mode` — the *live* header, not the cache record — is the
  /// authority: a disagreement evicts the entry rather than trusting
  /// stale metadata (the entry may be old enough that this deployment's
  /// completeness has since changed under a fresh re-bootstrap sharing an
  /// improbable epoch collision, or, more mundanely, may simply be
  /// corrupt — either way, absence-means-zero is never a browser-side
  /// guess, ADR-0015/0017).
  async #tryReadCacheIntoWasm(db, head, meta, onProgress) {
    try {
      if (head.mode !== null && head.mode !== meta.mode) {
        await idbEvictEpoch(db, meta.epoch);
        return null;
      }
      if (!isSaneByteLength(meta.totalBytes)) {
        // A corrupt or hand-edited record — never pass this straight to a
        // wasm allocation. Falls through like any other cache failure.
        await idbEvictEpoch(db, meta.epoch);
        return null;
      }
      const mode = head.mode !== null ? head.mode : meta.mode;
      const dest = this.#reserveInput(meta.totalBytes);
      const chunkCount = Math.ceil(meta.totalBytes / CHUNK_SIZE);
      let off = 0;
      for (let i = 0; i < chunkCount; i++) {
        const chunk = await idbGetChunk(db, meta.epoch, i);
        if (!chunk || chunk.length === 0) throw new Error(`missing cached chunk ${i} of ${chunkCount}`);
        if (off + chunk.length > meta.totalBytes) throw new Error("cached bytes exceed the recorded total");
        dest.set(chunk, off);
        off += chunk.length;
        if (onProgress) onProgress(off, meta.totalBytes);
      }
      if (off !== meta.totalBytes) throw new Error(`assembled ${off} of ${meta.totalBytes} cached bytes`);
      return { mode };
    } catch {
      return null;
    }
  }

  /// Drives `/setup` to completion into the wasm input buffer, retrying up
  /// to [`MAX_SETUP_RESUME_ATTEMPTS`] times if the connection stalls
  /// partway through (ADR-0035's watchdog; ADR-0038 point 7) — a stall
  /// with genuine bytes already in hand resumes with `Range`/`If-Range`
  /// rather than paying for the whole transfer again. `resumeFrom`, when
  /// given, is a *previous session's* cached partial entry
  /// (`{epoch, etag, totalBytes, bytesWritten}`); its already-cached
  /// prefix is replayed from IndexedDB before the network is asked for
  /// anything.
  ///
  /// Flushes completed ~[`CHUNK_SIZE`] chunks to IndexedDB as it goes (when
  /// `db`/`epoch` are available) — never one write per network chunk, see
  /// the module docs above `HINT_SOURCE`. If a `Range` continuation ever
  /// comes back `200` instead of `206`, the bundle was regenerated since
  /// the offset this method holds was valid (ADR-0028): rather than splice
  /// bytes from two different bundles, this discards whatever it had (in
  /// IndexedDB and in the wasm buffer) and restarts the whole transfer at
  /// byte 0 using that very `200`'s own body.
  ///
  /// Returns `{ total, mode, resumed }` on success. `mode` is `null` if
  /// this deployment never sent `x-risepir-mode` on `/setup` (a legacy
  /// server) — the caller (`#resolveMode`) falls back to `GET /mode`.
  async #attemptSetupDownload({ db, epoch, onProgress, resumeFrom }) {
    let off = 0;
    let total = null;
    let etag = null;
    let mode = null;
    let dest = null;
    let nextChunk = 0;
    let resumed = false;

    const resumeFromIsSane =
      resumeFrom &&
      isSaneByteLength(resumeFrom.totalBytes) &&
      Number.isInteger(resumeFrom.bytesWritten) &&
      resumeFrom.bytesWritten > 0 &&
      resumeFrom.bytesWritten <= resumeFrom.totalBytes;

    if (resumeFrom && !resumeFromIsSane) {
      // A corrupt or hand-edited record — never pass these straight to a
      // wasm allocation or an arithmetic offset. Evict and fall straight
      // through to an ordinary fresh download below.
      await idbEvictEpoch(db, epoch);
    } else if (resumeFromIsSane) {
      total = resumeFrom.totalBytes;
      etag = resumeFrom.etag;
      dest = this.#reserveInput(total);
      const cachedChunks = Math.floor(resumeFrom.bytesWritten / CHUNK_SIZE);
      let replayOk = true;
      for (let i = 0; i < cachedChunks; i++) {
        const chunk = await idbGetChunk(db, epoch, i);
        if (!chunk || chunk.length !== CHUNK_SIZE) {
          replayOk = false;
          break;
        }
        dest.set(chunk, i * CHUNK_SIZE);
      }
      if (replayOk) {
        off = cachedChunks * CHUNK_SIZE;
        nextChunk = cachedChunks;
        resumed = true;
      } else {
        // The cached prefix cannot be trusted — never guess at the
        // missing bytes. Wipe it and fall back to an ordinary fresh
        // download instead.
        await idbEvictEpoch(db, epoch);
        total = null;
        etag = null;
        dest = null;
      }
    }

    for (let attempt = 0; ; attempt++) {
      const watch = stallWatchdog(this.stallTimeoutMs);
      try {
        const headers = {};
        if (off > 0) {
          headers["range"] = `bytes=${off}-`;
          headers["if-range"] = etag ?? "";
        }
        const resp = await this.#fetch(`${this.#base}/setup`, { headers, signal: watch.signal });
        watch.progress();
        // `/setup` itself carries no epoch gate (it is the *source* of the
        // epoch, so it cannot be gated on one) and so does not emit this
        // today — handled anyway, defensively, the same way every other
        // endpoint here is.
        if (resp.status === 409) {
          throw new StaleSetupError(await resp.text());
        }
        if (resp.status !== 200 && resp.status !== 206) {
          throw new PirError(`GET /setup: ${resp.status} ${await resp.text()}`);
        }

        if (off > 0 && resp.status === 200) {
          // Regenerated meanwhile (ADR-0028): the bytes already held are
          // from a different bundle than this response. Discard
          // everything for this epoch and restart against this response.
          if (epoch) await idbEvictEpoch(db, epoch);
          off = 0;
          nextChunk = 0;
          dest = null;
          total = null; // this 200's own Content-Length is the new total
          resumed = false;
        }

        const m = resp.headers.get("x-risepir-mode");
        if (m === "0" || m === "1") {
          mode = m === "1" ? 1 : 0;
        } else if (m !== null) {
          throw new PirError(`x-risepir-mode header was ${JSON.stringify(m)} (expected "0" or "1")`);
        }
        const respEtag = resp.headers.get("etag");
        if (respEtag) etag = respEtag;

        if (dest === null) {
          const contentLength = Number(resp.headers.get("content-length") ?? 0);
          if (contentLength > 0) {
            total = contentLength;
            dest = this.#reserveInput(total);
            if (db && epoch) await idbClearAll(db);
          } else {
            // No declared length anywhere: read the whole body up front.
            // Rare in practice (this deployment's own server always sets
            // Content-Length on its fully-materialized body); caching is
            // simply skipped for this one response rather than adding a
            // third bookkeeping path for a case that should not occur
            // against this project's own server.
            const buf = new Uint8Array(await resp.arrayBuffer());
            total = buf.length;
            dest = this.#reserveInput(total);
            dest.set(buf, 0);
            off = buf.length;
            if (onProgress) onProgress(off, total);
            return { total, mode, resumed };
          }
        }

        if (resp.body) {
          const reader = resp.body.getReader();
          for (;;) {
            const { done, value } = await reader.read();
            if (done) break;
            watch.progress();
            if (off + value.length > total) {
              throw new PirError(`GET /setup: body longer than its Content-Length (${total})`);
            }
            dest.set(value, off);
            off += value.length;
            if (onProgress) onProgress(off, total);
            // Flush every full ~16 MiB chunk as it completes — never one
            // IDB write per network chunk (module docs above).
            if (db && epoch) {
              while ((nextChunk + 1) * CHUNK_SIZE <= off) {
                const start = nextChunk * CHUNK_SIZE;
                const ok = await idbPutChunk(db, epoch, nextChunk, dest.slice(start, start + CHUNK_SIZE));
                if (!ok) break; // a write failed; keep downloading, stop trying to cache this session
                nextChunk += 1;
                await idbPutMeta(db, {
                  epoch,
                  mode,
                  pinnedBlock: blockFromEtag(etag),
                  etag,
                  totalBytes: total,
                  bytesWritten: nextChunk * CHUNK_SIZE,
                  complete: false,
                  updatedAt: Date.now(),
                });
              }
            }
          }
        } else {
          const buf = new Uint8Array(await resp.arrayBuffer());
          if (off + buf.length > total) {
            throw new PirError(`GET /setup: body longer than its Content-Length (${total})`);
          }
          dest.set(buf, off);
          off += buf.length;
          if (onProgress) onProgress(off, total);
        }

        if (off !== total) {
          throw new PirError(`GET /setup: truncated (${off} of ${total} bytes)`);
        }

        // Done: flush the final (possibly short) tail chunk and mark the
        // entry complete.
        if (db && epoch) {
          if (nextChunk * CHUNK_SIZE < total) {
            await idbPutChunk(db, epoch, nextChunk, dest.slice(nextChunk * CHUNK_SIZE, total));
            nextChunk += 1;
          }
          await idbPutMeta(db, {
            epoch,
            mode,
            pinnedBlock: blockFromEtag(etag),
            etag,
            totalBytes: total,
            bytesWritten: total,
            complete: true,
            updatedAt: Date.now(),
          });
        }

        return { total, mode, resumed };
      } catch (e) {
        const isStall = watch.fired;
        if (isStall && off > 0 && attempt < MAX_SETUP_RESUME_ATTEMPTS) {
          // The partially-filled wasm input buffer (and whatever is
          // already flushed to IndexedDB) is still valid after a stall —
          // resume with Range/If-Range rather than paying for the whole
          // transfer again (ADR-0038 point 7).
          resumed = true;
          continue;
        }
        throw isStall ? new TimeoutError(`GET /setup: no response for ${this.stallTimeoutMs / 1000}s`) : e;
      } finally {
        watch.stop();
      }
    }
  }

  /// Resolves the `{ len, mode, fromCache, resumed }` tuple `load()` needs
  /// from whichever path `#loadSetupBytes` took. Falls back to the legacy
  /// `GET /mode` request only when nothing along the way ever supplied a
  /// mode (a server predating ADR-0033's `x-risepir-mode` header) —
  /// fetched without touching the wasm input buffer, so whatever is
  /// already staged there (from cache or from the network) stays intact.
  async #resolveMode({ len, mode, fromCache, resumed }) {
    if (mode === null) {
      const body = await this.#get("/mode");
      if (!(body.length === 1 && (body[0] === 0 || body[0] === 1))) {
        throw new PirError(`GET /mode returned ${body.length} bytes; expected exactly one byte, 0 or 1`);
      }
      mode = body[0];
    }
    return { len, mode, fromCache, resumed };
  }

  /// Gets the `/setup` bytes into the wasm input buffer by whichever route
  /// is cheapest and still safe (ADR-0038): a complete IndexedDB cache
  /// hit (no network at all), a resumed partial download (a `Range`
  /// continuation of a previous session's attempt), or a plain fresh
  /// download — falling back one step further at the first sign of
  /// trouble in each. `indexedDB` being unavailable (plain Node, some
  /// locked-down browser contexts) collapses this straight to "plain
  /// fresh download, exactly as before ADR-0038" with no special-casing
  /// needed at any call site, since every `idb*` helper already treats a
  /// `null` db as a no-op.
  async #loadSetupBytes({ onProgress, onSource } = {}) {
    const head = await this.#probeHead();
    const db = await idbOpen();
    this.#db = db;
    this.#cacheEpoch = null;

    if (db && head.epoch) {
      const meta = await idbGetMeta(db, head.epoch);
      if (meta && meta.complete && meta.totalBytes > 0) {
        const hit = await this.#tryReadCacheIntoWasm(db, head, meta, onProgress);
        if (hit) {
          if (onSource) onSource(HINT_SOURCE.CACHE);
          this.#cacheEpoch = head.epoch;
          return this.#resolveMode({ len: meta.totalBytes, mode: hit.mode, fromCache: true, resumed: false });
        }
      }
    }

    const partial = db && head.epoch ? await idbGetMeta(db, head.epoch) : null;
    const resumeFrom = partial && !partial.complete && partial.etag && partial.bytesWritten > 0 ? partial : null;
    if (onSource) onSource(resumeFrom ? HINT_SOURCE.RESUME : HINT_SOURCE.NETWORK);

    const dl = await this.#attemptSetupDownload({ db, epoch: head.epoch, onProgress, resumeFrom });
    return this.#resolveMode({ len: dl.total, mode: dl.mode, fromCache: false, resumed: dl.resumed });
  }

  /// One `/setup`, from whichever source `#loadSetupBytes` finds cheapest
  /// (ADR-0038). The wasm module still refuses to initialise without a
  /// validated completeness flag, so "absent means zero" can never be a
  /// browser-side default (ADR-0015/0017) regardless of which path
  /// supplied the bytes — cache, resume, or network alike.
  async load({ onProgress, onSource } = {}) {
    const { len, mode, fromCache, resumed } = await this.#loadSetupBytes({ onProgress, onSource });
    this.traffic.setupBytes = len;
    this.traffic.setupFromCache = fromCache;
    this.traffic.setupResumed = resumed;

    if (this.exports.risepir_set_mode_byte(mode) !== 0) {
      throw new PirError(`mode: ${this.#lastError()}`);
    }
    // risepir_init frees the encoded input buffer itself, between
    // decoding and building — the one point where releasing it actually
    // lowers the tab's permanent footprint (wasm memory never shrinks;
    // see ESTIMATED_PEAK_MULTIPLE's derivation below).
    if (this.exports.risepir_init(len) !== 0) {
      throw new PirError(`GET /setup: ${this.#lastError()}`);
    }

    // The lineage token every /sync and /answer must echo (ADR-0033) —
    // derived by the wasm from the bundle's own seeds, so it cannot
    // disagree with what was just initialised, *unless* the bytes it was
    // initialised from came from a corrupted cache entry — exactly what
    // the check below exists to catch.
    const elen = this.exports.risepir_epoch();
    if (elen < 0n) {
      throw new PirError(`epoch: ${this.#lastError()}`);
    }
    this.epoch = new TextDecoder().decode(this.#readOut());

    if (fromCache && this.epoch !== this.#cacheEpoch) {
      // A cache entry that decodes to a *different* epoch than the one it
      // was keyed on is corrupt, not merely stale — a hard error, never a
      // warning, because trusting it would mean serving a hint under the
      // wrong lineage token. Evict immediately so the next boot does not
      // trip over the same entry.
      await idbEvictEpoch(this.#db, this.#cacheEpoch).catch(() => {});
      throw new PirError(
        `cached hint decoded to epoch ${this.epoch}, but the cache was keyed on ${this.#cacheEpoch} — ` +
          `evicted the corrupt entry; reload to fetch a fresh hint`,
      );
    }
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
///
/// `onSource`, when given, is called once with a [`HINT_SOURCE`] value as
/// soon as `load()` has decided where the hint bytes are coming from
/// (ADR-0038) — before any of them have necessarily arrived — so a caller
/// can label a cache hit or a resumed download honestly instead of
/// showing a download rate/ETA that would mean nothing for either.
export async function connect(
  base,
  { wasmUrl = "client.wasm", wasmBytes, fetchImpl, onProgress, onSource, stallTimeoutMs = STALL_TIMEOUT_MS } = {},
) {
  const doFetch = fetchImpl ?? ((...args) => fetch(...args));
  let instance = null;
  const counter = { calls: 0, bytes: 0 };
  const imports = { env: { risepir_fill_random: entropyImport(() => instance, counter) } };

  if (wasmBytes) {
    ({ instance } = await WebAssembly.instantiate(wasmBytes, imports));
  } else {
    // Bounded like every other request: this is the *first* thing the page
    // fetches, so a stall here used to hang boot before there was any UI
    // to report it.
    const watch = stallWatchdog(stallTimeoutMs);
    try {
      ({ instance } = await WebAssembly.instantiateStreaming(doFetch(wasmUrl, { signal: watch.signal }), imports));
    } catch (e) {
      throw watch.fired ? new TimeoutError(`GET ${wasmUrl}: no response for ${stallTimeoutMs / 1000}s`) : e;
    } finally {
      watch.stop();
    }
  }

  const session = new PirSession(instance, base, doFetch, stallTimeoutMs);
  session.entropy = counter;
  await session.load({ onProgress, onSource });
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
/// (46.51 MB at `--partial-capacity 1000000` per ADR-0034's deployed
/// geometry, ~1.77 MB for `mock`) and comfortably below the real
/// complete-set hint (553.82 MB, ADR-0034), so only a deployment actually
/// built at that scale can ever reach it.
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
