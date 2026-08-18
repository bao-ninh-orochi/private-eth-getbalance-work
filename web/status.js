// GET /status (ADR-0039): polls GET /metrics and renders it. Dependency-
// free (this origin's CSP is `script-src 'self'`, so this file exists
// because inline <script> is blocked exactly like inline <style>). This
// page only ever reads aggregate counters — it never queries the PIR
// server and never touches an address in any form.

const POLL_MS = 5000;

// ── Prometheus text exposition parsing — hand-rolled, matching
//    risepir-http::metrics' own hand-rolled writer (no dependency either
//    side). Tolerant of anything *this server* emits; not a general-
//    purpose Prometheus client. ──

function splitLabels(s) {
  const parts = [];
  let cur = "";
  let inQuotes = false;
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    if (c === '"' && s[i - 1] !== "\\") inQuotes = !inQuotes;
    if (c === "," && !inQuotes) {
      parts.push(cur);
      cur = "";
    } else cur += c;
  }
  if (cur) parts.push(cur);
  return parts;
}

function parseMetrics(text) {
  const samples = [];
  for (const line of text.split("\n")) {
    if (!line || line.startsWith("#")) continue;
    const spaceIdx = line.lastIndexOf(" ");
    if (spaceIdx < 0) continue;
    const head = line.slice(0, spaceIdx);
    const value = Number(line.slice(spaceIdx + 1));
    const braceIdx = head.indexOf("{");
    let name = head;
    const labels = {};
    if (braceIdx >= 0) {
      name = head.slice(0, braceIdx);
      for (const part of splitLabels(head.slice(braceIdx + 1, head.lastIndexOf("}")))) {
        const eq = part.indexOf("=");
        if (eq < 0) continue;
        let v = part.slice(eq + 1);
        if (v.startsWith('"') && v.endsWith('"')) v = v.slice(1, -1);
        labels[part.slice(0, eq)] = v.replace(/\\n/g, "\n").replace(/\\"/g, '"').replace(/\\\\/g, "\\");
      }
    }
    samples.push({ name, labels, value });
  }
  return samples;
}

const all = (s, name) => s.filter((x) => x.name === name);
const first = (s, name, lf) => all(s, name).find((x) => !lf || Object.entries(lf).every(([k, v]) => x.labels[k] === v));
const num = (s, name, lf, fb = 0) => {
  const x = first(s, name, lf);
  return x ? x.value : fb;
};

// Standard Prometheus-side quantile approximation: linear interpolation
// within the bucket containing the target rank.
function histogramBuckets(s) {
  return all(s, "risepir_answer_duration_seconds_bucket")
    .map((x) => ({ le: x.labels.le === "+Inf" ? Infinity : Number(x.labels.le), count: x.value }))
    .sort((a, b) => a.le - b.le);
}
function quantile(buckets, q, total) {
  if (total <= 0) return null;
  const target = q * total;
  let prevLe = 0;
  let prevCount = 0;
  for (const b of buckets) {
    if (b.count >= target) {
      if (b.le === Infinity) return prevLe;
      const frac = b.count === prevCount ? 0 : (target - prevCount) / (b.count - prevCount);
      return prevLe + frac * (b.le - prevLe);
    }
    prevLe = b.le;
    prevCount = b.count;
  }
  return prevLe;
}

// ── formatting ──────────────────────────────────────────────────────────

const fmtSeconds = (s) => (s === null || Number.isNaN(s) ? "—" : s < 1 ? `${(s * 1000).toFixed(1)} ms` : `${s.toFixed(2)} s`);
function fmtAgo(unixSecs) {
  if (!unixSecs) return "never";
  const d = Date.now() / 1000 - unixSecs;
  if (d < 0) return "just now";
  if (d < 90) return `${Math.round(d)}s ago`;
  if (d < 5400) return `${Math.round(d / 60)}m ago`;
  return `${(d / 3600).toFixed(1)}h ago`;
}
function fmtUptime(s) {
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return h > 0 ? `${h}h ${m}m` : `${m}m ${Math.floor(s % 60)}s`;
}
function fmtBytes(b) {
  if (b < 1024) return `${b} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = b;
  let i = -1;
  do {
    v /= 1024;
    i++;
  } while (v >= 1024 && i < units.length - 1);
  return `${v.toFixed(2)} ${units[i]}`;
}
const tag = (text, cls) => `<span class="tag tag-${cls}">${text}</span>`;
const setText = (id, text) => (document.getElementById(id).textContent = text);
const setHtml = (id, html) => (document.getElementById(id).innerHTML = html);

function renderRequestsTable(s) {
  const routes = new Map();
  const get = (r) => routes.get(r) || routes.set(r, { ok: 0, error: 0, classes: new Map() }).get(r);
  for (const x of all(s, "risepir_requests_total")) {
    const r = get(x.labels.route);
    if (x.labels.outcome === "ok") r.ok += x.value;
    else r.error += x.value;
  }
  for (const x of all(s, "risepir_request_errors_total")) {
    const r = get(x.labels.route);
    r.classes.set(x.labels.class, (r.classes.get(x.labels.class) || 0) + x.value);
  }
  const body = document.getElementById("requests-body");
  body.innerHTML = "";
  for (const route of [...routes.keys()].sort()) {
    const r = routes.get(route);
    const classes = [...r.classes.entries()].map(([c, n]) => `${c}: ${n}`).join(", ") || "—";
    const tr = document.createElement("tr");
    tr.innerHTML = `<th>${route}</th><td>${r.ok}</td><td>${r.error}</td><td class="errclasses">${classes}</td>`;
    body.appendChild(tr);
  }
}

// ── main render ─────────────────────────────────────────────────────────

function render(text) {
  const s = parseMetrics(text);
  const g = (name) => num(s, name); // shorthand: unlabeled gauge/counter

  setText("head-block", g("risepir_head_block").toLocaleString());
  setText("finalized-block", g("risepir_finalized_block").toLocaleString());
  const lag = g("risepir_block_lag");
  setText("block-lag", lag.toLocaleString());
  setText("uptime", fmtUptime(g("risepir_process_uptime_seconds")));

  // Obvious visual state as the lag grows: a handful of blocks is routine
  // (finality itself lags the public head by design, ADR-0007); dozens
  // suggest the follow loop has stalled.
  const lagTile = document.getElementById("lag-tile");
  lagTile.classList.remove("is-warn", "is-err");
  if (lag > 60) lagTile.classList.add("is-err");
  else if (lag > 5) lagTile.classList.add("is-warn");

  const info = first(s, "risepir_build_info");
  setText("mode", info ? info.labels.mode : "—");
  setText("version", info ? info.labels.version : "—");
  setText("epoch", info ? info.labels.epoch : "—");

  setText("store-items", g("risepir_store_items").toLocaleString());
  setText("store-capacity", g("risepir_store_capacity").toLocaleString());
  setText("store-load", g("risepir_store_load_factor").toFixed(3));
  setText("setup-bytes", fmtBytes(g("risepir_setup_bytes")));
  setText("setup-regens", g("risepir_setup_regenerations_total").toLocaleString());

  const buckets = histogramBuckets(s);
  const total = g("risepir_answer_duration_seconds_count");
  setText("lat-count", total.toLocaleString());
  setText("lat-p50", fmtSeconds(quantile(buckets, 0.5, total)));
  setText("lat-p90", fmtSeconds(quantile(buckets, 0.9, total)));
  setText("lat-p99", fmtSeconds(quantile(buckets, 0.99, total)));

  renderRequestsTable(s);

  setText("rec-configured", g("risepir_reconcile_configured") === 1 ? "yes" : "no");
  setHtml("rec-halted", g("risepir_reconcile_halted") === 1 ? tag("HALTED", "err") : tag("no", "ok"));
  const dark = g("risepir_reconcile_consecutive_dark");
  setHtml("rec-dark", dark > 0 ? tag(String(dark), "warn") : "0");
  setText("rec-totals", `${g("risepir_reconcile_checkpoints_total").toLocaleString()} checkpoints / ${g("risepir_reconcile_comparisons_total").toLocaleString()} comparisons`);
  setText("rec-last-checkpoint", `block ${g("risepir_reconcile_last_checkpoint_block")} (${fmtAgo(g("risepir_reconcile_last_checkpoint_timestamp_seconds"))})`);
  setText("rec-last-success", `block ${g("risepir_reconcile_last_success_block")} (${fmtAgo(g("risepir_reconcile_last_success_timestamp_seconds"))})`);

  setText("save-configured", g("risepir_state_save_configured") === 1 ? "yes" : "no");
  setText("save-last", fmtAgo(g("risepir_state_save_last_success_timestamp_seconds")));
  setText("save-duration", fmtSeconds(g("risepir_state_save_last_duration_seconds")));
  setText("save-bytes", fmtBytes(g("risepir_state_save_last_bytes")));
  const failures = g("risepir_state_save_failures_total");
  setHtml("save-failures", failures > 0 ? tag(String(failures), "err") : "0");
  setText("journal-records", g("risepir_journal_records_since_save").toLocaleString());
  setHtml("journal-broken", g("risepir_journal_broken") === 1 ? tag("BROKEN", "err") : tag("no", "ok"));
}

async function poll() {
  const pollState = document.getElementById("poll-state");
  const errBox = document.getElementById("fetch-error");
  try {
    const resp = await fetch("metrics", { cache: "no-store" });
    if (!resp.ok) throw new Error(`GET /metrics: HTTP ${resp.status}`);
    render(await resp.text());
    errBox.classList.add("hidden");
    pollState.classList.remove("is-err");
    pollState.textContent = `live · updated ${new Date().toLocaleTimeString()}`;
  } catch (e) {
    pollState.classList.add("is-err");
    pollState.textContent = "unreachable";
    errBox.textContent = `Could not reach /metrics: ${e.message}`;
    errBox.classList.remove("hidden");
  }
}

poll();
setInterval(poll, POLL_MS);
