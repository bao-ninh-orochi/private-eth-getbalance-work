# `site/` — the always-on apex page at <https://risepir.org>

The static page a paper cites. It is deliberately **not** served by the demo
VM: the VM is billed by the hour while it runs and is stopped most of the
time, so a URL pointing at it fails hard on most days. This page is on
Cloudflare Pages, is always up, and carries the result — numbers, an
architecture diagram, and an illustration of a lookup — so a reader gets the
substance even when the server is off.

The decision, and the trust it does and does not add, is **ADR-0043**. The
short version: this page delivers **no cryptographic client** and makes no PIR
query, so it is outside ADR-0019's code-delivery boundary. It is *not* the
"serve the page from a different party" mitigation threat model §4.2
describes — that one is about the *demo's* page, which stays same-origin with
its PIR transport under `connect-src 'self'`. Do not conflate the two.

## Layout

```
index.html      the page; CSS is a separate file, everything else is inline SVG or same-origin <img>
style.css       design tokens on :root (Orochi Network / zkDatabase brand lineage), light theme only
404.html        matches the design; root-relative asset paths (Cloudflare serves it for any path depth)
favicon.svg     the RisePIR mark (patched-ribbon), replacing the old favicon
fonts/          self-hosted Raleway + JetBrains Mono, subset to woff2, OFL-1.1 (license text alongside
                each family). No font CDN: a third-party font request would be exactly the kind of
                request a page about caring who sees your requests should not make. Recorded as
                ADR-0049, which lands with the demo redesign (#14, PR #20).
assets/         brand marks (RisePIR mark, Orochi Network symbol), illustrations (patched ribbon,
                segment grid, icons), real captures of the redesigned demo.risepir.org (#15,
                captured 2026-09-24): demo-result-1440.webp (a completed lookup), demo-boot-1440.webp
                (first-visit hint download in progress), demo-result-390.webp (the same lookup at
                mobile width) — WebP, ~187 KB total, every browser this page targets renders it —
                and social-card.png, the og:image/twitter:card link-preview image (#21; see "Social
                card" below).
```

No build step, no dependencies, no external requests. Every asset — including
every font and every illustration — is same-origin; outbound links only (the
demo, GitHub, Orochi Network's site and X — `https://demo.risepir.org`,
`https://github.com/orochi-network/...`, `https://orochi.network`,
`https://x.com/OrochiNetwork`), no outbound requests.

## Deploying

Cloudflare Pages project `risepir-org`, account `86fb23a2e1be18581ea3ac9f205f4aad`.
Needs an API token with **Account → Cloudflare Pages → Edit** (and
**Zone → DNS → Edit** only if the custom domains are being re-attached):

```bash
export CLOUDFLARE_API_TOKEN=...            # not stored in this repo
export CLOUDFLARE_ACCOUNT_ID=86fb23a2e1be18581ea3ac9f205f4aad
npx wrangler pages deploy site --project-name risepir-org --branch main
```

**Before every deploy that changes `style.css` or `favicon.svg`, bump its
`?v=` in both `index.html` and `404.html`.** The value is the first 10 hex
characters of the file's own sha256
(`shasum -a 256 site/style.css | cut -c1-10`). The zone serves static files
with `cache-control: public, max-age=14400`, so a browser that fetched the
old file in the last 4 hours keeps using it without asking again. HTML is
`max-age=0`, so that visitor gets the new page with the old stylesheet.
This happened on the redesign's first deploy (#28). A new query string is a
new URL for both the browser cache and the edge cache key, so it cannot be
masked. There is still no build step: the hash is a hand-maintained literal,
checked against the file before each deploy.

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
- **Every number here must match its source.** Figures on this page trace to
  `docs/deployment-numbers.md` (the 2026-09-03 measurement campaign) and the
  paper (§8, §9) — they were checked against those sources when the page was
  written. A figure that drifts silently is the failure mode this project
  cares most about. The account count is the 2026-09-03 measurement-campaign
  figure, and the page dates it — the live set grows above it as the chain
  advances.

## Social card

`assets/social-card.png` is the `og:image`/`twitter:card` (`summary_large_image`)
shown in link previews (Slack, X, LinkedIn, iMessage, …) — 1200×630, under
200 KB, built from the same tokens and fonts as the page: the RisePIR
lockup, the "Private eth_getBalance · RisePIR" eyebrow, the hero headline
("The chain is public. Only your RPC query needs hiding."), `risepir.org`,
and a crop of `assets/illustrations/hero-patched-ribbon.svg` bleeding off
the right edge. It states no measured figures, so it never drifts out of
sync with `docs/deployment-numbers.md` the way a numbers-bearing image
could.

**Regenerate it if the hero headline changes.** It is a static PNG, not
generated at deploy time — there is no build step for this page (above).
The generator is an HTML page rendered with Playwright at exactly
1200×630, `deviceScaleFactor: 1`, referencing this repo's own
`fonts/*.woff2` and `assets/*.svg` by `file://` path; its source is kept in
the PR that introduced the card (#21) rather than shipped in `site/`, since
nothing here executes it. Re-render, re-save as `assets/social-card.png`,
and confirm it is still under 200 KB before committing.

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

The proceedings DOI, once CANS 2026 publishes. The full version is already
linked from the Paper card (IACR ePrint 2026/2082); only the DOI is pending,
marked with a `TODO` comment in `index.html`.
