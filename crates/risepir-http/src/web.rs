//! The browser front end's static assets, served from the *same origin*
//! as the PIR transport (ADR-0019).
//!
//! # Why the same origin, and why that is not just convenience
//!
//! The page has to reach `/setup`, `/answer` and `/sync`. Serving it from
//! anywhere else means either CORS plus a TLS certificate on the PIR port,
//! or a browser refusing the whole thing as mixed content. Same-origin
//! removes both problems — and, more usefully, lets the response carry a
//! `connect-src 'self'` Content-Security-Policy, so the delivered page is
//! *structurally* unable to POST the user's address to a third party.
//!
//! Be clear about what that does and does not buy. CSP constrains a page
//! that has been tampered with in transit or via an injected script; it
//! cannot constrain a dishonest origin, because the origin is what sends
//! the CSP. The residual trust — you trust whoever serves this page not to
//! serve you a malicious client — is stated in ADR-0019 and on the page
//! itself, not papered over here.
//!
//! # No path joining, ever
//!
//! This module maps a fixed set of routes to a fixed set of filenames
//! loaded once at startup. There is no request-path-to-filesystem-path
//! translation anywhere, so directory traversal is not defended against —
//! it is absent. Anything not on the list is a plain `404`.

use std::path::Path;
use std::sync::Arc;

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

/// One servable file: its route, its content type, and its bytes.
struct Asset {
    route: &'static str,
    content_type: &'static str,
    body: Arc<Vec<u8>>,
}

/// The front end's files, read into memory at startup.
///
/// Loaded eagerly (rather than read per request) for three reasons: a
/// missing file is a startup error instead of a 404 a visitor discovers;
/// the bytes served cannot change under a running deployment; and no
/// request ever reaches the filesystem.
pub struct WebAssets {
    assets: Vec<Asset>,
}

/// Every file this server will ever serve, with the route it answers on.
/// `client.wasm` is the build output of `crates/risepir-wasm`; the rest
/// are checked in under `web/`.
const MANIFEST: &[(&str, &str, &str)] = &[
    ("/", "index.html", "text/html; charset=utf-8"),
    ("/app.js", "app.js", "text/javascript; charset=utf-8"),
    ("/pir.js", "pir.js", "text/javascript; charset=utf-8"),
    ("/style.css", "style.css", "text/css; charset=utf-8"),
    ("/client.wasm", "client.wasm", "application/wasm"),
];

/// The policy sent with every asset. `connect-src 'self'` is the
/// load-bearing directive — see the module docs.
/// `'wasm-unsafe-eval'` is not optional and not a loosening of the policy
/// in any way that matters: Chromium classes WebAssembly compilation as
/// script evaluation, so without it `WebAssembly.instantiateStreaming`
/// throws and the private client never starts. It permits *wasm*
/// compilation only — plain `eval` and `new Function` stay blocked, which
/// is the pair that actually matters for injected script. (Caught by
/// `web/test/browser.mjs`, which is exactly why that gate exists: nothing
/// under Node can see a CSP.)
const CSP: &str = "default-src 'none'; \
     script-src 'self' 'wasm-unsafe-eval'; \
     style-src 'self'; \
     img-src 'self' data:; \
     font-src 'self'; \
     connect-src 'self'; \
     form-action 'none'; \
     frame-ancestors 'none'; \
     base-uri 'none'";

impl WebAssets {
    /// Read every file in [`MANIFEST`] from `dir`.
    ///
    /// # Errors
    ///
    /// The first file that cannot be read, naming it. `client.wasm` in
    /// particular is a build artifact, not a checked-in file — if it is
    /// missing, the operator needs to be told to build it (`cargo xtask
    /// web`), not left serving a page whose client never loads.
    pub fn load(dir: &Path) -> std::io::Result<Self> {
        let mut assets = Vec::with_capacity(MANIFEST.len());
        for (route, filename, content_type) in MANIFEST {
            let path = dir.join(filename);
            let body = std::fs::read(&path).map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!(
                        "{}: {e}{}",
                        path.display(),
                        if *filename == "client.wasm" {
                            " (build it with `cargo run -p xtask --release -- web`)"
                        } else {
                            ""
                        }
                    ),
                )
            })?;
            assets.push(Asset {
                route,
                content_type,
                body: Arc::new(body),
            });
        }
        Ok(Self { assets })
    }

    /// Total bytes held, for the startup banner.
    pub fn total_bytes(&self) -> usize {
        self.assets.iter().map(|a| a.body.len()).sum()
    }

    /// Add a route per asset to `router`.
    pub(crate) fn attach(self, router: Router) -> Router {
        let mut router = router;
        for asset in self.assets {
            let Asset {
                route,
                content_type,
                body,
            } = asset;
            router = router.route(
                route,
                get(move || {
                    let body = body.clone();
                    async move { asset_response(&body, content_type) }
                }),
            );
        }
        router
    }
}

/// One asset, with its content type and the hardening headers.
///
/// Deliberately **not** cached: the whole point of `client.wasm` is that
/// the user is running the code they think they are, and a stale cached
/// copy of a client that has since been fixed is the wrong trade for a
/// ~150 KB file. The ~50 MB thing worth caching is `/setup`, and the page
/// caches that itself, keyed by the block its hint is pinned at.
fn asset_response(body: &Arc<Vec<u8>>, content_type: &'static str) -> Response {
    let mut resp = (StatusCode::OK, body.as_ref().clone()).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    resp
}
