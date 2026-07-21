// The UI. All protocol logic lives in pir.js (which has no DOM reference,
// so the e2e test drives exactly the same code under Node).
//
// The one rule this file exists to honour: every degraded or failed state
// must be visible. A spinner that swallows "this account is not in the
// tracked set", or a `0` rendered for an answer the system does not
// actually have, would be a wrong answer with a nice font.

import { connect, formatEth, PirError, StaleSetupError, STATUS } from "./pir.js";

const $ = (id) => document.getElementById(id);
const bootStatus = $("boot-status");
const bootBar = $("boot-bar");
const bootError = $("boot-error");

let session = null;
/// Server head observed over time, so a stalled deployment is detected
/// locally — without asking any third party what the real chain head is,
/// which would leak this page's existence to someone new.
let headWatch = { block: null, since: Date.now() };

// ── boot ────────────────────────────────────────────────────────────

async function boot() {
  const t0 = performance.now();
  try {
    session = await connect(location.origin, {
      wasmUrl: "client.wasm",
      onProgress: (done, total) => {
        const pct = total ? (done / total) * 100 : 0;
        bootBar.style.width = `${pct}%`;
        bootStatus.textContent =
          `Downloading the PIR hint — ${(done / 1e6).toFixed(1)} MB of ${(total / 1e6).toFixed(1)} MB`;
      },
    });
  } catch (e) {
    bootBar.style.width = "0";
    bootStatus.textContent = "Could not start the private client.";
    bootError.textContent = String(e.message ?? e);
    bootError.classList.remove("hidden");
    return;
  }

  bootBar.style.width = "100%";
  bootStatus.textContent =
    `Ready in ${((performance.now() - t0) / 1000).toFixed(1)} s — ` +
    `${(session.traffic.setupBytes / 1e6).toFixed(1)} MB of hint, pinned at block ${session.pinnedBlock}.`;
  // Populate before revealing: the deployment's mode, head, and freshness
  // are the context an answer has to be read in, so they must never be
  // absent (or, worse, briefly blank) at the moment the query box appears.
  if (!session.complete) $("limit-partial").classList.remove("hidden");
  await refreshState();
  await loadSuggestions();

  $("boot").classList.add("hidden");
  $("query").classList.remove("hidden");
  $("state").classList.remove("hidden");
  setInterval(refreshState, 12_000);
  $("address").focus();
}

// ── deployment state ────────────────────────────────────────────────

function row(label, value, cls = "") {
  const tr = document.createElement("tr");
  const k = document.createElement("td");
  k.textContent = label;
  const v = document.createElement("td");
  if (value instanceof Node) v.appendChild(value);
  else v.textContent = value;
  if (cls) v.className = cls;
  tr.append(k, v);
  return tr;
}

function tag(text, cls) {
  const span = document.createElement("span");
  span.className = `tag ${cls}`;
  span.textContent = text;
  return span;
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

  const rows = $("state-rows");
  rows.replaceChildren();
  rows.append(
    row(
      "Data set",
      session.complete
        ? tag("complete — absence means exactly 0", "tag-ok")
        : tag("partial — absence means unknown", "tag-warn"),
    ),
    row("Server head", head === null ? "unreachable" : `block ${head}`, reachable ? "" : "error"),
    row("Hint pinned at", `block ${session.pinnedBlock}`),
    row(
      "Client caught up to",
      `block ${session.pendingHead} (${session.deltaCells.toLocaleString()} pending delta cells)`,
    ),
    row("Block meaning", "latest finalized — about 13 minutes behind a block explorer"),
  );

  if (!reachable) {
    rows.append(row("Status", "the PIR server is not responding; answers below may be stale", "error"));
  } else if (stalled) {
    rows.append(
      row(
        "Status",
        `the server has not advanced for ${Math.round(stalledFor / 60)} minutes — it may have stopped ` +
          `following the chain. Answers are as of block ${head} and are labelled, not wrong.`,
        "error",
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

  $("suggestions-note").textContent = session.complete
    ? "Seeded demo accounts — or type any address; this set is complete, so anything absent is exactly 0."
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

function errorBlock(title, detail) {
  const p = document.createElement("p");
  p.className = "error";
  const strong = document.createElement("strong");
  strong.textContent = title;
  p.append(strong, document.createTextNode(detail));
  return p;
}

async function lookup(event) {
  event.preventDefault();
  if (!session) return;

  const address = $("address").value.trim();
  const button = $("go");
  button.disabled = true;
  showResult([Object.assign(document.createElement("p"), { className: "asof", textContent: "Querying privately…" })]);

  const t0 = performance.now();
  let result;
  try {
    result = await session.getBalance(address);
  } catch (e) {
    button.disabled = false;
    if (e instanceof StaleSetupError) {
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
  }
  const elapsed = performance.now() - t0;
  button.disabled = false;

  const nodes = [];
  switch (result.status) {
    case STATUS.FOUND:
    case STATUS.ZERO: {
      const balance = document.createElement("div");
      balance.className = "balance";
      balance.textContent = formatEth(result.balanceWei);
      const unit = document.createElement("span");
      unit.className = "balance-unit";
      unit.textContent = "ETH";
      balance.appendChild(unit);

      const wei = document.createElement("div");
      wei.className = "wei";
      wei.textContent = `${result.balanceWei.toString()} wei`;

      const asof = document.createElement("p");
      asof.className = "asof";
      asof.textContent =
        `As of finalized block ${result.atBlock}` +
        (result.status === STATUS.ZERO
          ? " — this account is absent from the complete nonzero-balance set, which is exactly a zero balance."
          : ".");
      nodes.push(balance, wei, asof);
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
  refreshState();
}

// ── the "what the server saw" panel ─────────────────────────────────

function updateWirePanel(elapsedMs, atBlock) {
  const rows = $("wire-rows");
  rows.replaceChildren();
  const t = session.traffic;
  rows.append(
    row("Sent", `POST /answer — ${t.queryBytes.toLocaleString()} bytes of LWE ciphertext`),
    row("…its last 32 bytes", `${session.lastQueryTail} (fresh every query)`),
    row("Received", `${t.responseBytes.toLocaleString()} bytes, answered at block ${atBlock}`),
    row("Public delta pulled", `${t.deltaBytes.toLocaleString()} bytes (identical for every client)`),
    row("Addresses transmitted", tag("none", "tag-ok")),
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
boot();
