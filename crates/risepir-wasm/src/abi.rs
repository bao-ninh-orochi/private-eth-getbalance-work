#![allow(unsafe_code)]
//! The host ABI: the C-ABI exports a JavaScript host calls, and the two
//! byte buffers they move data through.
//!
//! # Why this module carries the crate's `unsafe_code` exception
//!
//! `#[unsafe(no_mangle)]` is an unsafe *declaration* — it overrides symbol
//! naming, and the linker cannot be told to guarantee uniqueness. That is
//! the only reason the crate-level `deny(unsafe_code)` is relaxed here;
//! there is not one `unsafe` *block* in this file. In particular no
//! function below dereferences a pointer the host supplied: the host
//! writes into a buffer this module owns ([`risepir_in_reserve`] then
//! [`risepir_in_ptr`]) and reads results out of another one
//! ([`risepir_out_ptr`]/[`risepir_out_len`]), so the whole ABI is
//! expressible in safe Rust over `Vec<u8>`. Buffer overruns, use-after-free
//! and double-free are absent by construction rather than by review.
//!
//! The single real `unsafe` block in the crate is the entropy shim
//! ([`crate::entropy`]), which is irreducible FFI.

use core::cell::RefCell;

use crate::session::{Outcome, Session, SessionError};

// ─── status codes ──────────────────────────────────────────────────────

/// [`risepir_finish`]: found; the balance is in the output buffer.
pub const STATUS_FOUND: i32 = 0;
/// [`risepir_finish`]: absent from a **complete** set ⇒ exactly `0x0`.
pub const STATUS_ZERO: i32 = 1;
/// [`risepir_finish`]: absent from a **partial** set ⇒ *unknown*. The host
/// must surface this as an error; answering `0x0` here would be a wrong
/// answer (ADR-0015/0017).
pub const STATUS_UNTRACKED: i32 = 2;
/// [`risepir_finish`]: the value's checksum did not verify. An error,
/// never a number.
pub const STATUS_DECODE_FAILED: i32 = 3;
/// Any entry point: failed; see the error buffer.
pub const STATUS_ERROR: i32 = -1;

// ─── module state ──────────────────────────────────────────────────────
//
// `thread_local` rather than a `static mut`: wasm32-unknown-unknown is
// single-threaded, so this costs nothing, and it keeps the crate free of
// the `unsafe` a mutable static would require.

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    /// Server-declared completeness, held from `set_mode` until `init`
    /// consumes it. Stored as the raw body so `Session::new` does the
    /// validating — the flag is never defaulted (ADR-0015/0017).
    static MODE: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
    static IN: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static OUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static ERR: RefCell<String> = const { RefCell::new(String::new()) };
}

fn set_err(e: impl core::fmt::Display) -> i32 {
    ERR.with(|s| *s.borrow_mut() = e.to_string());
    STATUS_ERROR
}

fn clear_err() {
    ERR.with(|s| s.borrow_mut().clear());
}

fn put_out(bytes: Vec<u8>) {
    OUT.with(|o| *o.borrow_mut() = bytes);
}

/// Run `f` over the first `len` bytes of the input buffer.
fn with_input<R>(len: usize, f: impl FnOnce(&[u8]) -> R) -> R {
    IN.with(|i| {
        let buf = i.borrow();
        let n = len.min(buf.len());
        f(&buf[..n])
    })
}

/// Run `f` over the live session.
fn with_session<R>(
    f: impl FnOnce(&mut Session) -> Result<R, SessionError>,
) -> Result<R, SessionError> {
    SESSION.with(|s| {
        let mut s = s.borrow_mut();
        let session = s.as_mut().ok_or(SessionError::NotInitialised)?;
        f(session)
    })
}

// ─── buffers ───────────────────────────────────────────────────────────

/// Grow the input buffer to at least `len` bytes and clear the error slot.
///
/// The host must call [`risepir_in_ptr`] *after* this — growing the buffer
/// can both move it and grow wasm memory, which detaches any
/// `ArrayBuffer` view JavaScript is holding.
#[unsafe(no_mangle)]
pub extern "C" fn risepir_in_reserve(len: usize) {
    IN.with(|i| {
        let mut buf = i.borrow_mut();
        buf.clear();
        buf.resize(len, 0);
    });
}

/// Where to write the input bytes. Valid until the next
/// [`risepir_in_reserve`].
#[unsafe(no_mangle)]
pub extern "C" fn risepir_in_ptr() -> *mut u8 {
    IN.with(|i| i.borrow_mut().as_mut_ptr())
}

/// Current input-buffer length.
#[unsafe(no_mangle)]
pub extern "C" fn risepir_in_len() -> usize {
    IN.with(|i| i.borrow().len())
}

/// Start of the last result produced.
#[unsafe(no_mangle)]
pub extern "C" fn risepir_out_ptr() -> *const u8 {
    OUT.with(|o| o.borrow().as_ptr())
}

/// Length of the last result produced.
#[unsafe(no_mangle)]
pub extern "C" fn risepir_out_len() -> usize {
    OUT.with(|o| o.borrow().len())
}

/// Start of the UTF-8 message describing the last failure.
#[unsafe(no_mangle)]
pub extern "C" fn risepir_err_ptr() -> *const u8 {
    ERR.with(|e| e.borrow().as_ptr())
}

/// Length of that message; `0` when the last call succeeded.
#[unsafe(no_mangle)]
pub extern "C" fn risepir_err_len() -> usize {
    ERR.with(|e| e.borrow().len())
}

// ─── the same buffers, for native callers ──────────────────────────────
//
// A wasm host writes into the input buffer through `in_reserve`/`in_ptr`,
// which is meaningless in-process. These three give native callers — this
// crate's own ABI tests — the identical path in safe Rust, so the state
// machine the browser runs is the one `cargo test --workspace` covers,
// with no separate harness to drift and no `unsafe` in the test.

/// Copy `bytes` into the input buffer; returns the length to pass to
/// whichever operation consumes it.
pub fn put_input(bytes: &[u8]) -> usize {
    IN.with(|i| {
        let mut buf = i.borrow_mut();
        buf.clear();
        buf.extend_from_slice(bytes);
        buf.len()
    })
}

/// A copy of the output buffer — the result of the last successful call.
pub fn output() -> Vec<u8> {
    OUT.with(|o| o.borrow().clone())
}

/// The message left by the last failure; empty if it succeeded.
pub fn last_error() -> String {
    ERR.with(|e| e.borrow().clone())
}

/// Drop the input buffer's capacity. [`risepir_init`] now does this
/// itself at the moment it matters most (between decoding the setup
/// bundle and building the client — see its docs); this export stays for
/// hosts that want to drop the buffer after any *other* large input, and
/// calling it after `risepir_init` anyway is a harmless no-op.
#[unsafe(no_mangle)]
pub extern "C" fn risepir_in_release() {
    IN.with(|i| {
        let mut buf = i.borrow_mut();
        buf.clear();
        buf.shrink_to_fit();
    });
}

// ─── lifecycle ─────────────────────────────────────────────────────────

/// Ingest `GET /mode`'s body (exactly one byte). Must precede
/// [`risepir_init`]. Returns `0`, or [`STATUS_ERROR`].
#[unsafe(no_mangle)]
pub extern "C" fn risepir_set_mode(len: usize) -> i32 {
    clear_err();
    let body = with_input(len, <[u8]>::to_vec);
    if !matches!(body.as_slice(), [0] | [1]) {
        return set_err(SessionError::BadModeByte(body.len()));
    }
    MODE.with(|m| *m.borrow_mut() = Some(body));
    0
}

/// [`risepir_set_mode`] without the input-buffer round trip: sets the
/// completeness flag directly from `value` (`0` partial / `1` complete;
/// anything else is [`STATUS_ERROR`]). Exists for the `x-risepir-mode`
/// header path (ADR-0033), where the host learns the mode from the
/// `/setup` response's *headers* while that same response's *body* is
/// being streamed into the input buffer — going through
/// [`risepir_set_mode`] there would clobber the body bytes mid-stream.
#[unsafe(no_mangle)]
pub extern "C" fn risepir_set_mode_byte(value: u32) -> i32 {
    clear_err();
    if value > 1 {
        return set_err(format!(
            "mode must be 0 (partial) or 1 (complete), got {value} — the completeness flag is never guessed"
        ));
    }
    MODE.with(|m| *m.borrow_mut() = Some(vec![value as u8]));
    0
}

/// Ingest `GET /setup`'s body and build the client. Returns `0`, or
/// [`STATUS_ERROR`] (including when [`risepir_set_mode`] has not run —
/// the completeness flag is required, never assumed).
///
/// Decode and build are two steps with the input buffer **freed in
/// between**: at the complete mainnet set the encoded bundle is
/// 553.82 MB (was 830.73 MB before ADR-0034), the decoded bundle
/// another 553.82 MB, and the built client (per-segment hint + expanded
/// `A`) ~2x that again (~1.11 GB) — and wasm linear
/// memory never shrinks, so whatever peak this function reaches is the
/// tab's floor forever after. Releasing the encoded bytes before
/// `Session::from_bundle` allocates the client cuts that permanent
/// floor by one full hint set; `risepir-client`'s own `from_setup`
/// consumes the decoded hints per segment for the same reason. This is
/// the sequence ADR-0032's `ESTIMATED_PEAK_MULTIPLE` is derived from —
/// change one, revisit the other.
#[unsafe(no_mangle)]
pub extern "C" fn risepir_init(len: usize) -> i32 {
    clear_err();
    let Some(mode) = MODE.with(|m| m.borrow().clone()) else {
        return set_err(
            "GET /mode must be loaded before GET /setup (the completeness flag is never assumed)",
        );
    };
    let complete = match crate::session::parse_mode(&mode) {
        Ok(c) => c,
        Err(e) => return set_err(e),
    };
    let bundle = match with_input(len, Session::decode) {
        Ok(b) => b,
        Err(e) => return set_err(e),
    };
    risepir_in_release();
    let session = Session::from_bundle(bundle, complete);
    SESSION.with(|slot| *slot.borrow_mut() = Some(session));
    0
}

/// `1` if the deployment declared a complete nonzero-balance set, `0` if
/// partial, [`STATUS_ERROR`] if uninitialised.
#[unsafe(no_mangle)]
pub extern "C" fn risepir_complete() -> i32 {
    match with_session(|s| Ok(s.complete())) {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(e) => set_err(e),
    }
}

/// The block the hint is pinned at, or `u64::MAX` if uninitialised.
#[unsafe(no_mangle)]
pub extern "C" fn risepir_pinned_block() -> u64 {
    with_session(|s| Ok(s.pinned_block())).unwrap_or(u64::MAX)
}

/// Write the session's lineage token (ADR-0033; 16 lowercase-hex ASCII
/// bytes) to the output buffer, returning its length — the host attaches
/// it as `?epoch=` to every `/sync` and `/answer` request. On an
/// uninitialised session, [`STATUS_ERROR`] as an `i64`.
#[unsafe(no_mangle)]
pub extern "C" fn risepir_epoch() -> i64 {
    clear_err();
    match with_session(|s| Ok(s.epoch().as_bytes().to_vec())) {
        Ok(bytes) => {
            let n = bytes.len() as i64;
            put_out(bytes);
            n
        }
        Err(e) => i64::from(set_err(e)),
    }
}

/// The block the accumulated delta reaches — the `from` for the next
/// `GET /sync`. `u64::MAX` if uninitialised.
#[unsafe(no_mangle)]
pub extern "C" fn risepir_pending_head() -> u64 {
    with_session(|s| Ok(s.pending_head())).unwrap_or(u64::MAX)
}

/// Segment count (SCF arity), or `0` if uninitialised.
#[unsafe(no_mangle)]
pub extern "C" fn risepir_arity() -> u32 {
    with_session(|s| Ok(s.arity() as u32)).unwrap_or(0)
}

/// Nonzero cells pending in `ΔD` — a "how far has this client drifted"
/// readout for the UI. `0` if uninitialised.
#[unsafe(no_mangle)]
pub extern "C" fn risepir_delta_cells() -> usize {
    with_session(|s| Ok(s.delta_cells())).unwrap_or(0)
}

// ─── the query conversation ────────────────────────────────────────────

/// Ingest a `GET /sync` body. Returns the new pending head, or `-1` cast
/// to `i64` on failure.
#[unsafe(no_mangle)]
pub extern "C" fn risepir_ingest(len: usize) -> i64 {
    clear_err();
    let result = IN.with(|i| {
        let buf = i.borrow();
        let n = len.min(buf.len());
        let bytes = buf[..n].to_vec();
        drop(buf);
        with_session(|s| s.ingest(&bytes))
    });
    match result {
        Ok(head) => head as i64,
        Err(e) => i64::from(set_err(e)),
    }
}

/// Build the PIR query for the 20-byte address in the input buffer. On
/// success the `POST /answer` body is in the output buffer and the return
/// value is its length; on failure, [`STATUS_ERROR`] as an `i64`.
#[unsafe(no_mangle)]
pub extern "C" fn risepir_query(len: usize) -> i64 {
    clear_err();
    let result = IN.with(|i| {
        let buf = i.borrow();
        let n = len.min(buf.len());
        let addr = buf[..n].to_vec();
        drop(buf);
        with_session(|s| s.build_query(&addr))
    });
    match result {
        Ok(body) => {
            let n = body.len() as i64;
            put_out(body);
            n
        }
        Err(e) => i64::from(set_err(e)),
    }
}

/// Ingest `POST /answer`'s response body. Returns the block the server
/// answered at — which the host must sync the delta up to before
/// [`risepir_finish`] — or `-1` on failure.
#[unsafe(no_mangle)]
pub extern "C" fn risepir_answer(len: usize) -> i64 {
    clear_err();
    let result = IN.with(|i| {
        let buf = i.borrow();
        let n = len.min(buf.len());
        let bytes = buf[..n].to_vec();
        drop(buf);
        with_session(|s| s.accept_answer(&bytes))
    });
    match result {
        Ok(at) => at as i64,
        Err(e) => i64::from(set_err(e)),
    }
}

/// Complete the lookup. Returns one of [`STATUS_FOUND`] (balance in the
/// output buffer as 16 little-endian bytes), [`STATUS_ZERO`],
/// [`STATUS_UNTRACKED`], [`STATUS_DECODE_FAILED`], or [`STATUS_ERROR`].
///
/// A [`SessionError::SyncRequired`] failure is recoverable: sync the
/// delta to the block [`risepir_answer`] reported and call this again.
#[unsafe(no_mangle)]
pub extern "C" fn risepir_finish() -> i32 {
    clear_err();
    put_out(Vec::new());
    match with_session(Session::finish) {
        Ok(Outcome::Found(balance)) => {
            put_out(balance.to_le_bytes().to_vec());
            STATUS_FOUND
        }
        Ok(Outcome::Zero) => STATUS_ZERO,
        Ok(Outcome::Untracked) => STATUS_UNTRACKED,
        Ok(Outcome::DecodeFailed) => STATUS_DECODE_FAILED,
        Err(e) => set_err(e),
    }
}
