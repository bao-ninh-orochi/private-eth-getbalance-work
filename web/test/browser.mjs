// Real-browser gate for the front end (ADR-0019).
//
//     ./target/release/risepir-rpc mock --web web &
//     node web/test/browser.mjs http://127.0.0.1:8645
//
// `e2e.mjs` covers the protocol under Node; this covers what only a
// browser can: that the page boots under its own Content-Security-Policy,
// that WebAssembly instantiates and finds real entropy there, that the DOM
// wiring actually performs a lookup, and — the point of the whole
// exercise — that the balance it displays is the true one.
//
// Drives headless Chromium (Brave/Chrome/Chromium, whichever is present)
// over the DevTools protocol using Node's built-in WebSocket client, so it
// adds no dependency to the repo.

import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const base = process.argv[2] ?? "http://127.0.0.1:8645";
const PORT = 9333;

const CANDIDATES = [
  "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  "/Applications/Chromium.app/Contents/MacOS/Chromium",
  "/usr/bin/chromium",
  "/usr/bin/google-chrome",
];

const browser = CANDIDATES.find((p) => existsSync(p));

if (!browser) {
  console.log("no Chromium-family browser found; skipping the browser gate");
  process.exit(0);
}

let failures = 0;
function check(name, cond, detail = "") {
  if (cond) console.log(`  ok    ${name}`);
  else {
    failures += 1;
    console.log(`  FAIL  ${name}${detail ? ` — ${detail}` : ""}`);
  }
}

const profile = mkdtempSync(join(tmpdir(), "risepir-browser-"));
const proc = spawn(
  browser,
  [
    "--headless=new",
    "--disable-gpu",
    "--no-first-run",
    "--no-default-browser-check",
    `--remote-debugging-port=${PORT}`,
    `--user-data-dir=${profile}`,
    base,
  ],
  { stdio: "ignore" },
);

const cleanup = () => {
  try {
    proc.kill("SIGKILL");
  } catch {}
  try {
    rmSync(profile, { recursive: true, force: true });
  } catch {}
};
process.on("exit", cleanup);

async function devtoolsTarget() {
  for (let i = 0; i < 60; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
      const page = list.find((t) => t.type === "page" && t.url.startsWith(base));
      if (page?.webSocketDebuggerUrl) return page.webSocketDebuggerUrl;
    } catch {}
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error("browser never exposed a debugging target");
}

const ws = new WebSocket(await devtoolsTarget());
await new Promise((resolve, reject) => {
  ws.addEventListener("open", resolve, { once: true });
  ws.addEventListener("error", reject, { once: true });
});

let nextId = 1;
const pending = new Map();
const consoleErrors = [];

ws.addEventListener("message", (event) => {
  const msg = JSON.parse(event.data);
  if (msg.id && pending.has(msg.id)) {
    pending.get(msg.id)(msg);
    pending.delete(msg.id);
  }
  if (msg.method === "Log.entryAdded" && msg.params?.entry?.level === "error") {
    consoleErrors.push(msg.params.entry.text);
  }
  if (msg.method === "Runtime.exceptionThrown") {
    consoleErrors.push(msg.params?.exceptionDetails?.text ?? "exception");
  }
});

function send(method, params = {}) {
  const id = nextId++;
  return new Promise((resolve) => {
    pending.set(id, resolve);
    ws.send(JSON.stringify({ id, method, params }));
  });
}

/// Evaluate an async expression in the page and return its resolved value.
async function evaluate(expression) {
  const res = await send("Runtime.evaluate", {
    expression: `(async () => { ${expression} })()`,
    awaitPromise: true,
    returnByValue: true,
  });
  if (res.result?.exceptionDetails) {
    throw new Error(res.result.exceptionDetails.exception?.description ?? "page threw");
  }
  return res.result?.result?.value;
}

await send("Runtime.enable");
await send("Log.enable");
await send("Page.enable");

// Which deployment is this? The page adapts to `GET /mode` and so must
// the gate: in a complete set an absent address is exactly 0, in a partial
// one it must be an error. Asserting the wrong one would be asserting the
// bug this project most wants to catch.
const modeByte = new Uint8Array(await (await fetch(`${base}/mode`)).arrayBuffer());
const complete = modeByte[0] === 1;

console.log(
  `\ndriving ${browser.split("/").pop()} against ${base} (${complete ? "COMPLETE" : "PARTIAL"} set)\n`,
);

// ── 1. the page boots: wasm instantiated, hint loaded ─────────────────

const booted = await evaluate(`
  const deadline = Date.now() + 120000;
  while (Date.now() < deadline) {
    const q = document.getElementById("query");
    const err = document.getElementById("boot-error");
    if (err && !err.classList.contains("hidden")) return { ok: false, error: err.textContent };
    if (q && !q.classList.contains("hidden")) {
      return { ok: true, state: document.getElementById("state-rows").innerText };
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  return { ok: false, error: "timed out waiting for the client to boot" };
`);

check("the page boots and the private client initialises", booted?.ok === true, booted?.error ?? "");
if (!booted?.ok) {
  console.log(`\nFAIL: ${failures} failing checks\n`);
  process.exit(1);
}
check(
  "deployment state is rendered rather than implied",
  /Data set|Server head/.test(booted.state ?? ""),
  JSON.stringify(booted.state ?? "").slice(0, 120),
);

// ── 2. a real lookup through the real DOM ────────────────────────────
//
// The address is typed into the input and the form submitted, exactly as
// a person would. The expected value comes from the server's own startup
// banner (the mock's seeded demo accounts).

// In a complete (mock) deployment we know an exact expected balance. In a
// partial (live mainnet) one we take an address the server says it
// tracks, and check the *shape* of the answer plus its block label; the
// value itself is verified byte-exactly against independent providers by
// `web/test/e2e.mjs`'s reported result and docs/deploy.md's recorded runs.
const probe = complete
  ? "0x5555555555555555555555555555555555555555"
  : await (async () => {
      const bytes = new Uint8Array(await (await fetch(`${base}/recent`)).arrayBuffer());
      const count = new DataView(bytes.buffer, 0, 4).getUint32(0, true);
      if (count === 0) throw new Error("server reports no tracked accounts yet; let it follow a few blocks");
      let hex = "0x";
      for (const b of bytes.subarray(4, 24)) hex += b.toString(16).padStart(2, "0");
      return hex;
    })();

const lookup = await evaluate(`
  const input = document.getElementById("address");
  input.value = "${probe}";
  document.getElementById("form").requestSubmit();
  const deadline = Date.now() + 60000;
  while (Date.now() < deadline) {
    const box = document.getElementById("result");
    const wei = box.querySelector(".wei");
    const err = box.querySelector(".error");
    if (wei) return { ok: true, wei: wei.textContent, eth: box.querySelector(".balance").textContent,
                      asof: box.querySelector(".asof").textContent,
                      wire: document.getElementById("wire-rows").innerText };
    if (err) return { ok: false, error: err.textContent };
    await new Promise((r) => setTimeout(r, 100));
  }
  return { ok: false, error: "timed out waiting for a result" };
`);

check("a lookup completes in the browser", lookup?.ok === true, lookup?.error ?? "");
if (complete) {
  check(
    "the displayed balance is byte-exact (100 wei, the mock's dust account)",
    lookup?.wei === "100 wei",
    `got ${JSON.stringify(lookup?.wei)}`,
  );
} else {
  check(
    "a tracked mainnet account shows an exact wei balance",
    /^\d+ wei$/.test(lookup?.wei ?? ""),
    `got ${JSON.stringify(lookup?.wei)} for ${probe}`,
  );
  console.log(`        ${probe} -> ${lookup?.wei}`);
}
check("the answer is labelled with the block it is as of", /finalized block \d+/.test(lookup?.asof ?? ""));
check(
  "the wire panel reports LWE ciphertext and no addresses",
  /LWE ciphertext/.test(lookup?.wire ?? "") && /none/i.test(lookup?.wire ?? ""),
);
check(
  "real entropy was drawn in the browser",
  /crypto\.getRandomValues/.test(lookup?.wire ?? "") && !/ 0 bytes of crypto/.test(lookup?.wire ?? ""),
);

// The same address queried twice must not produce the same ciphertext —
// checked here in the browser, where the entropy comes from Web Crypto
// rather than from the OS backend the native tests use.
const fresh = await evaluate(`
  const tails = [];
  for (let i = 0; i < 2; i++) {
    document.getElementById("address").value = "0x5555555555555555555555555555555555555555";
    document.getElementById("form").requestSubmit();
    const deadline = Date.now() + 60000;
    while (Date.now() < deadline) {
      const wire = document.getElementById("wire-rows").innerText;
      const m = wire.match(/([0-9a-f]{64})/);
      if (m && (tails.length === 0 || m[1] !== tails[0])) { tails.push(m[1]); break; }
      await new Promise((r) => setTimeout(r, 50));
    }
  }
  return tails;
`);
check(
  "two browser queries for the same address send different ciphertext",
  Array.isArray(fresh) && fresh.length === 2 && fresh[0] !== fresh[1],
  JSON.stringify(fresh),
);

// ── 3. an absent address is handled honestly ─────────────────────────

const absent = await evaluate(`
  const input = document.getElementById("address");
  input.value = "0x" + "de".repeat(20);
  document.getElementById("form").requestSubmit();
  const deadline = Date.now() + 60000;
  const before = document.getElementById("result").textContent;
  while (Date.now() < deadline) {
    const box = document.getElementById("result");
    const text = box.textContent;
    if (text !== before && !/Querying privately/.test(text)) {
      return { text, isZero: !!box.querySelector(".balance"), isError: !!box.querySelector(".error") };
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  return { text: "timeout" };
`);

if (complete) {
  // Absence is exactly zero — and the UI must say *why* rather than just
  // printing 0.
  check(
    "an absent address in a complete set shows 0 and explains that absence means zero",
    absent?.isZero === true && /absent from the complete/.test(absent?.text ?? ""),
    JSON.stringify(absent?.text ?? "").slice(0, 160),
  );
} else {
  // The rule that outranks everything, at the last place it can go wrong:
  // the pixels. An untracked account must render as an error, and must
  // not render a zero anywhere.
  check(
    "an untracked address in a partial set is an error, never a 0 balance",
    absent?.isError === true && absent?.isZero === false,
    JSON.stringify(absent?.text ?? "").slice(0, 160),
  );
  check(
    "and the page explains why it will not answer 0",
    /will not answer 0|absence says nothing/.test(absent?.text ?? ""),
    JSON.stringify(absent?.text ?? "").slice(0, 200),
  );
}

// ── 4. nothing broke quietly ─────────────────────────────────────────

const csp = consoleErrors.filter((e) => /Content Security Policy|Refused to/i.test(e));
check("no Content-Security-Policy violations", csp.length === 0, csp.join(" | "));
check(
  "no uncaught page errors",
  consoleErrors.length === 0,
  consoleErrors.slice(0, 3).join(" | "),
);

console.log(`\n${failures === 0 ? "PASS" : "FAIL"}: ${failures} failing check${failures === 1 ? "" : "s"}\n`);
process.exit(failures === 0 ? 0 : 1);
