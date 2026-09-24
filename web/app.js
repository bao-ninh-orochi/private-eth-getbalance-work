// The UI. All protocol logic lives in pir.js (which has no DOM reference,
// so the e2e test drives exactly the same code under Node).
//
// The one rule this file exists to honour: every degraded or failed state
// must be visible. A spinner that swallows "this account is not in the
// tracked set", or a `0` rendered for an answer the system does not
// actually have, would be a wrong answer with a nice font.

import {
  connect,
  formatEth,
  PirError,
  StaleSetupError,
  TimeoutError,
  STATUS,
  assessCapacity,
  CAPACITY_VERDICT,
  HINT_SOURCE,
} from "./pir.js";

const $ = (id) => document.getElementById(id);

let session = null;
let booted = false;
/// Server head observed over time, so a stalled deployment is detected
/// locally — without asking any third party what the real chain head is,
/// which would leak this page's existence to someone new.
let headWatch = { block: null, since: Date.now() };

// ── boot: a staged, honest loading experience ───────────────────────
//
// Three stages, each advanced by something real rather than by a timer:
// the wasm fetch, the /setup download (with byte-level progress), and the
// local expansion + pin. `bootFetch` wraps fetch only to *observe* which
// request is in flight — every byte still moves through pir.js.

const STAGES = ["engine", "hint", "pin"];

function setStage(active) {
  const idx = active === "done" ? STAGES.length : STAGES.indexOf(active);
  STAGES.forEach((name, i) => {
    const el = $(`step-${name}`);
    el.classList.toggle("is-done", i < idx);
    el.classList.toggle("is-active", i === idx);
  });
}

function bootFetch(input, init) {
  if (!booted && String(input).includes("/setup")) {
    setStage("hint");
    $("boot-status-text").textContent =
      "Downloading the hint — the one large transfer this page ever makes.";
  }
  return fetch(input, init);
}

/// A distinct boot stage for "this browser already has the hint"
/// (ADR-0038): fired once, as soon as `pir.js` has decided where the hint
/// is coming from — before any of it has necessarily arrived, since a
/// cache read is not a network transfer `bootFetch` above would ever see.
/// A rate/ETA display that meant nothing for a cache hit (or a resumed
/// download, which starts partway through a nonexistent "0%") would be a
/// wrong answer with a nice font, exactly the failure mode this file
/// exists to avoid.
function onHintSource(source) {
  if (source === HINT_SOURCE.CACHE) {
    setStage("hint");
    $("boot-status-text").textContent =
      "Loading the hint from this browser's cache — no network transfer needed this time.";
  } else if (source === HINT_SOURCE.RESUME) {
    setStage("hint");
    $("boot-status-text").textContent =
      "Resuming the hint download from where an earlier visit left off.";
  }
  // HINT_SOURCE.NETWORK: bootFetch above already sets the "Downloading
  // the hint…" text the moment the real request goes out; nothing to add.
}

// Rolling-window transfer rate, so the ETA reflects the link as it is
// right now rather than the whole download's history.
const samples = [];
let lastPaintAt = 0;

const fmtMB = (n) => (n / 1e6).toFixed(1);

function fmtEta(seconds) {
  if (!Number.isFinite(seconds) || seconds < 0) return "";
  if (seconds < 90) return `about ${Math.max(1, Math.round(seconds))} s left`;
  return `about ${Math.round(seconds / 60)} min left`;
}

function onProgress(done, total) {
  const now = performance.now();
  samples.push({ t: now, b: done });
  while (samples.length > 2 && samples[0].t < now - 3000) samples.shift();

  const finished = total > 0 && done >= total;
  if (!finished && now - lastPaintAt < 80) return;
  lastPaintAt = now;

  if (total > 0) {
    const pct = (done / total) * 100;
    $("boot-bar").style.width = `${pct}%`;
    $("boot-progress").setAttribute("aria-valuenow", String(Math.floor(pct)));
    $("boot-pct").textContent = `${Math.floor(pct)}%`;
    $("boot-mb").textContent = `${fmtMB(done)} MB of ${fmtMB(total)} MB`;
  } else {
    $("boot-mb").textContent = `${fmtMB(done)} MB`;
  }

  const span = samples[samples.length - 1].t - samples[0].t;
  const bytes = samples[samples.length - 1].b - samples[0].b;
  if (span > 300 && bytes > 0) {
    const rate = bytes / (span / 1000);
    $("boot-rate").textContent = `${(rate / 1e6).toFixed(1)} MB/s`;
    $("boot-eta").textContent = finished ? "" : fmtEta((total - done) / rate);
  }

  if (finished) {
    $("boot-eta").textContent = "";
    setStage("pin");
    // The expansion is synchronous wasm, so the page may not repaint again
    // until it is over — say so *before* it starts, not after.
    $("boot-status-text").textContent =
      "Expanding the hint into memory and pinning it to a finalized block — " +
      "this runs on your CPU and may pause the page for a moment.";
  }
}

async function boot() {
  const t0 = performance.now();
  try {
    session = await connect(location.origin, {
      wasmUrl: "client.wasm",
      fetchImpl: bootFetch,
      onProgress,
      onSource: onHintSource,
    });
  } catch (e) {
    $("boot").classList.add("is-failed");
    $("boot-status-text").textContent = "Could not start the private client.";
    const err = $("boot-error");
    err.textContent = `${String(e.message ?? e)} — reload the page to try again.`;
    err.classList.remove("hidden");
    return;
  }

  booted = true;
  setStage("done");
  $("boot-bar").style.width = "100%";
  $("boot-progress").setAttribute("aria-valuenow", "100");

  const secs = ((performance.now() - t0) / 1000).toFixed(1);
  // Where the hint came from — a cache hit or a resumed download are both
  // materially different experiences from a plain cold download, and
  // saying so in one clause is cheaper than leaving it to be inferred
  // from how fast "Ready" appeared (ADR-0038).
  const source = session.traffic.setupFromCache
    ? "loaded from this browser's cache"
    : session.traffic.setupResumed
      ? "downloaded, resumed"
      : "downloaded";
  $("ready-note").textContent =
    `Ready in ${secs} s — ${fmtMB(session.traffic.setupBytes)} MB of hint held locally (${source}), ` +
    `pinned at finalized block ${session.pinnedBlock.toLocaleString("en-US")}.`;

  // Populate before revealing: the deployment's mode, head, and freshness
  // are the context an answer has to be read in, so they must never be
  // absent (or, worse, briefly blank) at the moment the query box appears.
  if (!session.complete) $("limit-partial").classList.remove("hidden");
  await refreshState();
  await loadSuggestions();

  $("boot").classList.add("hidden");
  $("query").classList.remove("hidden");
  $("state").classList.remove("hidden");
  // Layout only: the setup/wire/state arrangement while booting gives way
  // to the ready arrangement (query+state left, wire right) — a class
  // toggle `style.css` reads, nothing about fetches or timing.
  $("grid-main").classList.remove("is-boot");
  setInterval(refreshState, 12_000);
  $("address").focus();
}

// ── capacity pre-flight (ADR-0032) ───────────────────────────────────
//
// Runs once, before boot() ever touches the network for real work. The
// download that buys this page's privacy (see the "Why the download?"
// aside) is also the one thing that can crash a phone's tab before it
// gets the chance to explain itself: at the deployed `(arity 2,
// bucket_size 4)` geometry (ADR-0034), the live complete-mainnet set
// computes to 553.82 MB downloaded and ~1.11 GB resident (a computed
// estimate; the CLI client measured 1.16 GB resident on 2026-09-03,
// docs/deployment-numbers.md) once the public matrix A is expanded
// (docs/numbers.md §4c) — was 830.73 MB / ~1.66 GB at the previous
// `(arity 3, bucket_size 4)` geometry the deployment ran until
// 2026-07-27, ADR-0034.
// assessCapacity (pir.js) makes the actual call; everything here just
// gathers its inputs from a cheap HEAD probe and whatever the device is
// willing to say about itself, and renders whichever of its three
// verdicts comes back.

const GB = 1_000_000_000;
const fmtGB = (n) => (n / GB).toFixed(2);

/// `HEAD /setup`'s `Content-Length` — this deployment's exact hint size,
/// read without paying for the download it describes. `null` on anything
/// short of a clean positive number: a failed or ambiguous probe falls
/// straight through to today's unconditional boot(), never manufactures a
/// refusal (ADR-0032, point 1).
async function probeHintBytes() {
  try {
    const resp = await fetch(`${location.origin}/setup`, { method: "HEAD" });
    if (!resp.ok) return null;
    const len = Number(resp.headers.get("content-length"));
    return Number.isFinite(len) && len > 0 ? len : null;
  } catch {
    return null;
  }
}

/// The two `navigator`-level touch signals `assessCapacity` accepts as one
/// merged boolean; the viewport is passed separately so the pure function
/// can apply its own threshold to it.
function coarsePointerSignal() {
  const touch = typeof navigator !== "undefined" && navigator.maxTouchPoints > 0;
  const coarse = typeof matchMedia === "function" && matchMedia("(pointer: coarse)").matches;
  return touch || coarse;
}

/// Swap the "01 One-time setup" panel for an explanation of what it would
/// cost, instead of spending the visitor's data to find that out the hard
/// way. Never a dead end: "Download anyway" leads to the exact same
/// boot() a capable device runs unconditionally.
function renderCapacityGate(v) {
  // Presentation only: which tint (warn amber vs err red) the panel wears.
  // "Download anyway" stays present either way — this never becomes a
  // harder lock-out than the text itself already is.
  const panel = $("capacity-gate-panel");
  panel.classList.remove("is-warn", "is-err");
  panel.classList.add(v.verdict === CAPACITY_VERDICT.REFUSE ? "is-err" : "is-warn");

  $("capacity-gate-lede").textContent =
    v.verdict === CAPACITY_VERDICT.REFUSE
      ? "This deployment's hint is bigger than this device says it can comfortably hold in one " +
        "browser tab. Downloading it anyway may work — or may run out of memory partway through, " +
        "after the download is already spent."
      : v.basis === "save-data"
        ? "This browser is set to reduce data usage, and this deployment's hint is a large " +
          "one-time download. Memory looks fine — this pause is only about the data."
        : "This device did not report how much memory it has, but it looks like a small-screen or " +
          "touch device, and this deployment's hint is a large download. This is a guess, not a " +
          "measurement — it may be entirely fine.";

  const rows = $("capacity-gate-rows");
  rows.replaceChildren(
    row("Hint to download", `${fmtMB(v.hintBytes)} MB`),
    row("Estimated peak memory", `${fmtGB(v.estimatedPeakBytes)} GB — the hint plus the expanded public matrix`),
    row(
      "Your device reports",
      v.deviceMemoryGb != null
        ? `${v.deviceMemoryGb} GB of memory`
        : "no memory figure — Safari and Firefox do not expose one",
    ),
  );

  $("boot").classList.add("hidden");
  $("capacity-gate").classList.remove("hidden");
}

/// Gate boot() behind a cheap cost check. Anything that leaves the true
/// cost unknown (the HEAD probe failing) or clearly affordable (a verdict
/// of "ok") falls straight through to boot() with no DOM touched first —
/// which is what makes this a no-op on `mock`, and on every deployment
/// small enough not to matter.
async function preflight() {
  const hintBytes = await probeHintBytes();
  if (hintBytes === null) return boot();

  const verdict = assessCapacity({
    hintBytes,
    deviceMemoryGb: typeof navigator !== "undefined" ? navigator.deviceMemory : undefined,
    coarsePointer: coarsePointerSignal(),
    viewportWidth: typeof window !== "undefined" ? window.innerWidth : undefined,
    saveData: typeof navigator !== "undefined" && navigator.connection?.saveData === true,
  });

  if (verdict.verdict === CAPACITY_VERDICT.OK) return boot();
  renderCapacityGate(verdict);
}

// ── deployment state ────────────────────────────────────────────────

function row(label, value, cls = "") {
  const r = document.createElement("div");
  r.className = cls ? `rrow is-${cls}` : "rrow";
  const k = document.createElement("span");
  k.className = "rk";
  k.textContent = label;
  // The dot leader between label and value — presentational only, so it
  // contributes nothing to the row's text (the tests read innerText).
  const leader = document.createElement("span");
  leader.className = "leader";
  leader.setAttribute("aria-hidden", "true");
  const v = document.createElement("span");
  v.className = "rv";
  if (value instanceof Node) v.appendChild(value);
  else v.textContent = value;
  r.append(k, leader, v);
  return r;
}

function tag(text, cls) {
  const span = document.createElement("span");
  span.className = `tag ${cls}`;
  span.textContent = text;
  return span;
}

/// A value cell that leads with a badge and follows with plain words —
/// the words carry the meaning, the badge only underlines it.
function tagged(node, note) {
  const frag = document.createDocumentFragment();
  frag.appendChild(node);
  const span = document.createElement("span");
  span.className = "note";
  span.textContent = note;
  frag.appendChild(span);
  const wrap = document.createElement("span");
  wrap.appendChild(frag);
  return wrap;
}

function setLivePill(state, text) {
  const pill = $("live-pill");
  pill.classList.remove("hidden", "is-ok", "is-warn", "is-err");
  pill.classList.add(`is-${state}`);
  $("live-text").textContent = text;
}

async function refreshState() {
  if (!session) return;
  let head = null;
  let reachable = true;
  try {
    head = await session.head();
  } catch {
    reachable = false;
  }

  if (head !== null) {
    if (headWatch.block === null || head > headWatch.block) {
      headWatch = { block: head, since: Date.now() };
    }
  }

  const stalledFor = (Date.now() - headWatch.since) / 1000;
  // Mainnet blocks are ~12 s; finalization advances in epochs, so allow a
  // generous margin before calling it stalled rather than crying wolf.
  const stalled = reachable && stalledFor > 15 * 60;

  if (!reachable) setLivePill("err", "server unreachable");
  else if (stalled) setLivePill("warn", `stalled at block ${head.toLocaleString("en-US")}`);
  else setLivePill("ok", `following · block ${head.toLocaleString("en-US")}`);

  const rows = $("state-rows");
  rows.replaceChildren();
  rows.append(
    row(
      "Data set",
      session.complete
        ? tagged(tag("complete", "tag-ok"), " — absence means exactly 0")
        : tagged(tag("partial", "tag-warn"), " — absence means unknown"),
    ),
    row(
      "Server head",
      head === null ? "unreachable" : `block ${head.toLocaleString("en-US")}`,
      reachable ? "" : "error",
    ),
    row("Hint pinned at", `block ${session.pinnedBlock.toLocaleString("en-US")}`),
    row(
      "Client caught up to",
      `block ${session.pendingHead.toLocaleString("en-US")} (${session.deltaCells.toLocaleString()} pending delta cells)`,
    ),
    row("Block meaning", "latest finalized — about 13 minutes behind a block explorer"),
  );

  if (!reachable) {
    rows.append(row("Status", "the PIR server is not responding; answers below may be stale", "error"));
  } else if (stalled) {
    // Stalled is a caution, not a failure — the server may simply be
    // behind, and the answers it gives are still labelled, not wrong.
    // Amber (`warn`), never the same red as an actual unreachable server.
    rows.append(
      row(
        "Status",
        `the server has not advanced for ${Math.round(stalledFor / 60)} minutes — it may have stopped ` +
          `following the chain. Answers are as of block ${head.toLocaleString("en-US")} and are labelled, not wrong.`,
        "warn",
      ),
    );
  }
}

async function loadSuggestions() {
  let addrs = [];
  try {
    addrs = await session.recent();
  } catch {
    return;
  }
  if (addrs.length === 0) return;

  // Wording must hold for every complete deployment — the live mainnet
  // set serves recently-touched real accounts here, not a demo seed list
  // (the old "Seeded demo accounts" label was written when complete ⇒
  // mock was true, and had been wrong on the live set since 2026-07-26).
  $("suggestions-note").textContent = session.complete
    ? "Recently updated accounts this server tracks (public chain data) — or type any address; " +
      "this set is complete, so anything absent is exactly 0."
    : "Accounts this server has seen change recently (public chain data). It cannot tell which of " +
      "these — if any — you go on to query.";

  const list = $("suggestion-list");
  list.replaceChildren();
  for (const addr of addrs.slice(0, 8)) {
    const chip = document.createElement("button");
    chip.type = "button";
    chip.className = "chip";
    chip.textContent = `${addr.slice(0, 10)}…${addr.slice(-6)}`;
    chip.title = addr;
    chip.addEventListener("click", () => {
      $("address").value = addr;
      $("address").dispatchEvent(new Event("input"));
      $("form").requestSubmit();
    });
    list.appendChild(chip);
  }
  $("suggestions").classList.remove("hidden");
}

// ── lookup ──────────────────────────────────────────────────────────

function showResult(nodes) {
  const box = $("result");
  box.replaceChildren(...nodes);
  box.classList.remove("hidden");
}

const SVG_NS = "http://www.w3.org/2000/svg";

/// The small three-scale brand mark on the answer card — a solid fill (no
/// gradient) so it never needs an id, unlike the static marks in the nav
/// and footer, which each carry their own uniquely-id'd gradient.
function answerScalesMark() {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("class", "answer-scales");
  svg.setAttribute("viewBox", "0 0 32 32");
  svg.setAttribute("aria-hidden", "true");
  const scales = [
    { x: 1, y: 15, fill: "#FFD9C9" },
    { x: 8, y: 8, fill: "#FFD9C9" },
    { x: 15, y: 1, fill: "#FF4608" },
  ];
  for (const { x, y, fill } of scales) {
    const rect = document.createElementNS(SVG_NS, "rect");
    rect.setAttribute("x", String(x));
    rect.setAttribute("y", String(y));
    rect.setAttribute("width", "14");
    rect.setAttribute("height", "14");
    rect.setAttribute("fill", fill);
    rect.setAttribute("stroke", "#E03C06");
    rect.setAttribute("stroke-width", "1.1");
    rect.setAttribute("stroke-linejoin", "round");
    svg.appendChild(rect);
  }
  return svg;
}

function checkIcon() {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("fill", "none");
  svg.setAttribute("aria-hidden", "true");
  const path = document.createElementNS(SVG_NS, "path");
  path.setAttribute("d", "M3 8.5 6.2 12 13 4.5");
  path.setAttribute("stroke", "currentColor");
  path.setAttribute("stroke-width", "1.8");
  path.setAttribute("stroke-linecap", "round");
  path.setAttribute("stroke-linejoin", "round");
  svg.appendChild(path);
  return svg;
}

/// A small "!" in a circle, built with presentation attributes only (no
/// `style=`, per the CSP-driven rule every inline SVG on this page follows)
/// — the same mark `states.html` uses for every error/warn tile.
function alertIcon() {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("fill", "none");
  svg.setAttribute("aria-hidden", "true");
  const circle = document.createElementNS(SVG_NS, "circle");
  circle.setAttribute("cx", "8");
  circle.setAttribute("cy", "8");
  circle.setAttribute("r", "6.5");
  circle.setAttribute("stroke", "currentColor");
  circle.setAttribute("stroke-width", "1.4");
  const bar = document.createElementNS(SVG_NS, "path");
  bar.setAttribute("d", "M8 5v3.6M8 11h.01");
  bar.setAttribute("stroke", "currentColor");
  bar.setAttribute("stroke-width", "1.4");
  bar.setAttribute("stroke-linecap", "round");
  svg.append(circle, bar);
  return svg;
}

/// `.error` is the class the browser gate and the rest of this file key
/// on (see the module docs); `state-panel is-err` is presentation only —
/// the tinted-panel-with-icon look every error/warn state shares.
function errorBlock(title, detail) {
  const wrap = document.createElement("div");
  wrap.className = "error state-panel is-err";
  const head = document.createElement("div");
  head.className = "state-panel-head";
  const strong = document.createElement("strong");
  strong.textContent = title;
  head.append(alertIcon(), strong);
  const p = document.createElement("p");
  p.textContent = detail;
  wrap.append(head, p);
  return wrap;
}

/// A timed-out lookup, with the one control that actually helps. The
/// distinction this draws is the whole point of `TimeoutError` being its
/// own type: a stall costs nothing but the attempt — the hint, its pin and
/// its epoch are all still valid — so retrying is a *query*, not the
/// 553.82 MB reload a StaleSetupError genuinely requires.
function retryBlock() {
  const wrap = document.createElement("div");
  wrap.append(
    errorBlock(
      "The request timed out. ",
      "The server stopped responding partway through — it may be briefly unreachable, or this " +
        "device's connection may have dropped (a sleep or a network change does it). Nothing is " +
        "lost: the hint you already downloaded is still valid, so this retries the query alone.",
    ),
  );
  const retry = document.createElement("button");
  retry.type = "button";
  retry.className = "btn-secondary";
  retry.textContent = "Retry the lookup";
  retry.addEventListener("click", () => $("form").requestSubmit());
  wrap.append(retry);
  return wrap;
}

function queryingNode() {
  const p = document.createElement("p");
  p.className = "querying";
  const spin = document.createElement("span");
  spin.className = "spinner";
  spin.setAttribute("aria-hidden", "true");
  p.append(
    spin,
    document.createTextNode("Querying privately — the server is computing over its entire set…"),
  );
  return p;
}

const ADDR_RE = /^0x[0-9a-fA-F]{40}$/;

/// Live hint, not a gate: shown once the input can no longer become a
/// valid address (or is full-length and still wrong). Submission always
/// goes through parseAddress in pir.js, which is the real check.
function checkAddressInput() {
  const v = $("address").value.trim();
  const undecided = /^0(x[0-9a-fA-F]{0,40})?$/.test(v);
  const bad = v.length > 0 && !ADDR_RE.test(v) && !undecided;
  $("address").classList.toggle("is-invalid", bad);
  $("addr-hint").classList.toggle("hidden", !bad);
}

async function lookup(event) {
  event.preventDefault();
  if (!session) return;

  const address = $("address").value.trim();
  const button = $("go");
  button.disabled = true;
  showResult([queryingNode()]);

  const t0 = performance.now();
  let result;
  try {
    result = await session.getBalance(address);
  } catch (e) {
    if (e instanceof TimeoutError) {
      // Checked before PirError, which it extends.
      showResult([retryBlock()]);
    } else if (e instanceof StaleSetupError) {
      showResult([
        errorBlock(
          "This client is too far behind to answer safely. ",
          "The server no longer keeps the block deltas this hint needs, so the answer could not be " +
            "corrected to the current block. Reload the page to fetch a fresh hint — the client " +
            "refuses to guess rather than return a possibly-wrong balance.",
        ),
      ]);
    } else if (e instanceof PirError) {
      showResult([errorBlock("Could not complete the lookup. ", e.message)]);
    } else {
      showResult([errorBlock("Unexpected failure. ", String(e.message ?? e))]);
    }
    return;
  } finally {
    // Re-enabled on *every* exit, not once per outcome. The two
    // outcome-local assignments this replaces were what turned a single
    // stalled request into a permanently dead query box: the button was
    // disabled before the await and only re-enabled by paths that require
    // the promise to settle, so a request that never settled disabled the
    // UI for the life of the page (ADR-0035).
    button.disabled = false;
  }
  const elapsed = performance.now() - t0;

  const nodes = [];
  switch (result.status) {
    case STATUS.FOUND:
    case STATUS.ZERO: {
      const card = document.createElement("div");
      card.className = "answer-card";
      card.appendChild(answerScalesMark());

      const label = document.createElement("p");
      label.className = "answer-label";
      label.textContent = "Balance";

      const balance = document.createElement("div");
      balance.className = "balance num";
      const amount = document.createElement("span");
      amount.className = "balance-num";
      amount.textContent = formatEth(result.balanceWei);
      const unit = document.createElement("span");
      unit.className = "balance-unit";
      unit.textContent = "ETH";
      balance.append(amount, unit);

      const wei = document.createElement("p");
      wei.className = "wei num";
      wei.textContent = `${result.balanceWei.toString()} wei`;

      const asof = document.createElement("p");
      asof.className = "asof";
      asof.textContent =
        `As of finalized block ${result.atBlock}` +
        (result.status === STATUS.ZERO
          ? " — this account is absent from the complete nonzero-balance set, which is exactly a zero balance."
          : ".");

      const badge = document.createElement("span");
      badge.className = "badge-ok";
      badge.append(checkIcon(), document.createTextNode("Decoded in your browser"));

      card.append(label, balance, wei, asof, badge);
      nodes.push(card);
      break;
    }
    case STATUS.UNTRACKED:
      nodes.push(
        errorBlock(
          "Not in this server's tracked set. ",
          "This deployment holds only accounts touched since it started, so it does not know this " +
            "one's balance. It will not answer 0 — for a partial set, absence says nothing about the " +
            "balance, and a confident zero here would simply be wrong.",
        ),
      );
      break;
    case STATUS.DECODE_FAILED:
      nodes.push(
        errorBlock(
          "The answer did not decode cleanly. ",
          "The value's checksum failed, so the balance cells came back corrupted. Reporting the " +
            "number anyway is the one thing this system will not do.",
        ),
      );
      break;
    default:
      nodes.push(errorBlock("Unexpected status. ", `code ${result.status}`));
  }

  showResult(nodes);
  updateWirePanel(elapsed, result.atBlock);
  renderBlockStrip();
  refreshState();
}

// ── "Patched in place": blocks folded in since the hint was pinned ────
//
// Built from exactly three session values — `pinnedBlock`, `pendingHead`,
// `traffic.deltaBytes` — and nothing else: no per-block account counts or
// timings, because the session does not have them, and this file never
// fabricates a number it cannot back with something the client actually
// measured.

function blockTile(blockNum, isCurrent) {
  const wrap = document.createElement("div");
  wrap.className = `block-tile ${isCurrent ? "is-current" : "is-done"}`;

  const scaleWrap = document.createElement("span");
  scaleWrap.className = "bt-scale-wrap";
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("class", "bt-scale");
  svg.setAttribute("viewBox", "0 0 32 32");
  svg.setAttribute("aria-hidden", "true");
  const poly = document.createElementNS(SVG_NS, "polygon");
  poly.setAttribute("points", "9,3 29,3 23,29 3,29");
  // A solid fill (never a shared gradient) — safe to build many of these
  // in one document without an id ever colliding.
  poly.setAttribute("fill", isCurrent ? "#FF4608" : "#FFD9C9");
  poly.setAttribute("stroke", "#E03C06");
  poly.setAttribute("stroke-width", "1.2");
  svg.appendChild(poly);
  scaleWrap.appendChild(svg);

  const label = document.createElement("span");
  label.className = "bt-num mono";
  label.textContent = `…${(blockNum % 1000n).toString().padStart(3, "0")}`;
  label.title = blockNum.toLocaleString("en-US");

  wrap.append(scaleWrap, label);
  return wrap;
}

function renderBlockStrip() {
  if (!session) return;
  const strip = $("block-strip");
  const pinned = session.pinnedBlock;
  const head = session.pendingHead;
  const n = head - pinned;
  if (n < 0n) return; // never happens in practice; never render a negative count

  strip.replaceChildren();

  const rangeLen = n + 1n;
  const showCount = rangeLen < 8n ? Number(rangeLen) : 8;
  const startBlock = head - BigInt(showCount - 1);

  const tiles = document.createElement("div");
  tiles.className = "block-strip";
  for (let i = 0; i < showCount; i++) {
    const blockNum = startBlock + BigInt(i);
    tiles.appendChild(blockTile(blockNum, blockNum === head));
  }

  const caption = document.createElement("p");
  caption.className = "block-strip-caption";
  if (n === 0n) {
    caption.textContent =
      `Hint pinned at finalized block ${pinned.toLocaleString("en-US")} — no newer finalized block ` +
      `to fold in yet.`;
  } else {
    const label = n === 1n ? "1 finalized block" : `${n.toLocaleString("en-US")} finalized blocks`;
    const kb = (session.traffic.deltaBytes / 1000).toFixed(1);
    caption.textContent = `${label} folded in since your hint was pinned · ${kb} kB of public deltas · no re-download`;
  }

  strip.append(tiles, caption);
  strip.classList.remove("hidden");
}

// ── the "what the server saw" panel ─────────────────────────────────

function updateWirePanel(elapsedMs, atBlock) {
  // Presentation only: after the first completed lookup the panel stops
  // describing what it *will* show and starts reporting what it *did*.
  $("wire-heading").textContent = "What the server saw";
  $("wire-sub").textContent = "The receipt for your last query — everything that crossed the wire.";

  const rows = $("wire-rows");
  rows.replaceChildren();
  const t = session.traffic;

  const tail = document.createElement("code");
  tail.className = "hexdump";
  tail.textContent = session.lastQueryTail;

  rows.append(
    row("Sent", `POST /answer — ${t.queryBytes.toLocaleString()} bytes of LWE ciphertext`),
    row("Its last 32 bytes", tagged(tail, " — fresh every query")),
    row("Received", `${t.responseBytes.toLocaleString()} bytes, answered at block ${atBlock.toLocaleString("en-US")}`),
    row("Public delta pulled", `${t.deltaBytes.toLocaleString()} bytes (identical for every client)`),
    row(
      "Addresses transmitted",
      tagged(tag("none", "tag-ok"), " — not hashed, not truncated, not encrypted-for-them"),
      "hero",
    ),
    row(
      "LWE secret",
      `sampled fresh for this query, from a CSPRNG seeded with ` +
        `${session.entropy.bytes.toLocaleString()} bytes of crypto.getRandomValues`,
    ),
    row("Round trip", `${elapsedMs.toFixed(0)} ms`),
  );
  $("wire").classList.remove("hidden");
}

$("form").addEventListener("submit", lookup);
$("address").addEventListener("input", checkAddressInput);
$("capacity-continue").addEventListener("click", () => {
  $("capacity-gate").classList.add("hidden");
  $("boot").classList.remove("hidden");
  boot();
});
preflight();
