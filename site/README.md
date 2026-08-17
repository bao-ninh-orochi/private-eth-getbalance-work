# `site/` — the always-on apex page at <https://risepir.org>

The static page a paper cites. It is deliberately **not** served by the demo
VM: the VM costs $8.60/day and is stopped most of the time, so a URL pointing
at it fails hard on most days. This page is on Cloudflare Pages, is always up,
and carries the result — numbers, an architecture diagram, and screenshots of
a real lookup — so a reader gets the substance even when the server is off.

The decision, and the trust it does and does not add, is **ADR-0043**. The
short version: this page delivers **no cryptographic client** and makes no PIR
query, so it is outside ADR-0019's code-delivery boundary. It is *not* the
"serve the page from a different party" mitigation threat model §4.2
describes — that one is about the *demo's* page, which stays same-origin with
its PIR transport under `connect-src 'self'`. Do not conflate the two.

## Layout

```
index.html      the page; CSS is a separate file, everything else is inline SVG
style.css       tokens on :root, dark via prefers-color-scheme
404.html        matches the design
favicon.svg
assets/         screenshots captured from the live demo (deploy.md §5.9)
```

No build step, no dependencies, no external requests. Every asset is
same-origin; the only outbound link is to `https://demo.risepir.org`.

## Deploying

Cloudflare Pages project `risepir-org`, account `86fb23a2e1be18581ea3ac9f205f4aad`.
Needs an API token with **Account → Cloudflare Pages → Edit** (and
**Zone → DNS → Edit** only if the custom domains are being re-attached):

```bash
export CLOUDFLARE_API_TOKEN=...            # not stored in this repo
export CLOUDFLARE_ACCOUNT_ID=86fb23a2e1be18581ea3ac9f205f4aad
npx wrangler pages deploy site --project-name risepir-org --branch main
```

`risepir.org` and `www.risepir.org` are already attached as custom domains, so
a deploy to `--branch main` goes live at both. The certificate is issued by
Google Trust Services via Cloudflare Universal SSL, permitted by the `pki.goog`
CAA record Cloudflare injects into any zone it serves that has CAA at all
(deploy.md §3.7).

## Two things not to break

- **`demo.risepir.org` must stay DNS-only (grey cloud).** This page being
  proxied is fine — it ships no client. The demo being proxied would put
  Cloudflare in the path that delivers the wasm client, which is exactly the
  trust ADR-0019 discloses. The reason is written into `ops/caddy/Caddyfile`,
  deploy.md §3.7 and threat model §4.2 so it does not get "optimized" later.
- **Every number here must match `docs/numbers.md`.** They were checked against
  it when the page was written; a figure that drifts silently is the failure
  mode this project cares most about. The account count is the value the
  deployment was last bootstrapped at, and the page says so — the live set
  grows above it as the chain advances.

## Known wart

Cloudflare injects its own analytics beacon
(`static.cloudflareinsights.com/beacon.min.js`) into this page at the edge. It
is not in this source and appears only for requests carrying a browser
`User-Agent`. It is **zone-level Web Analytics**, not a Pages project setting
(`web_analytics_tag` on the project is `null`), so it is switched off in the
dashboard under Analytics → Web Analytics, not from here. Until it is off, the
page discloses it in its own trust section — a page arguing that a reader
should care who serves it cannot quietly ship a tracker it never mentions. If
it is switched off, drop that clause from `index.html` and redeploy.

## Still to fill in

The paper's DOI and a link to the PDF, once CANS 2026 publishes. The citation
itself is on the page; only those two are pending, marked with a `TODO`
comment in `index.html`.
