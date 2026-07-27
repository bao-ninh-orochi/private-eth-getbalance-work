//! The browser client's ABI, driven natively against a real
//! [`RisePirServer`] over the *real* wire bytes — the same
//! `encode_setup` / `encode_response_bundle` / `encode_block_delta`
//! output the HTTP transport serves.
//!
//! # Why these tests are native rather than in a browser
//!
//! The wasm build and the native build are the same source over the same
//! `risepir-client`; what differs is the target's threading and entropy,
//! not the protocol. So the *protocol* invariants — the rewind ordering,
//! the never-`0x0`-in-partial-mode rule, the wire-length checks, the
//! single-in-flight contract — are pinned here, where they run on every
//! `cargo test --workspace` with no toolchain beyond cargo. What can only
//! be checked in a real wasm host — that the entropy import is actually
//! reached, and that the module loads and answers at all — is pinned by
//! `web/test/e2e.mjs`, which drives this same ABI through
//! `WebAssembly.instantiate` against a running server.

use ikpir_common::{SimpleConfig, SimplePirBackend};
use risepir_http::wire;
use risepir_proto::{codec, keccak256, Backend, BlockUpdate, Geometry, ValueCodec};
use risepir_server::{RisePirServer, SetupBundle};
use risepir_wasm::abi::*;
use risepir_wasm::{STATUS_DECODE_FAILED, STATUS_ERROR, STATUS_FOUND, STATUS_UNTRACKED, STATUS_ZERO};
use segmented_cuckoo::{Segmented3aryCuckooKVStore, Segmented3aryScheme};

const ARITY: u32 = 3;
const BUCKET_SIZE: u32 = 4;
const FINGERPRINT_BITS: u32 = 32;
/// Small enough to keep the whole suite quick; the geometry, codecs, and
/// rewind logic under test are dimension-independent.
const LWE_DIM: u32 = 512;
const ACCOUNTS: u64 = 1_500;

fn value_codec() -> ValueCodec {
    ValueCodec {
        key_tag_bits: 32,
        balance_bits: 96,
        checksum_bits: 16,
    }
}

/// Deterministic 20-byte address number `i`, and its wei balance.
fn account(i: u64) -> ([u8; 20], u128) {
    let mut addr = [0u8; 20];
    addr[..8].copy_from_slice(&i.to_le_bytes());
    addr[19] = 0xA5;
    // Wei-scale and spread across the 96-bit balance field.
    let balance = 1_000_000_000_000_000u128 + u128::from(i) * 7_919_000_000_000u128;
    (addr, balance)
}

type Server = RisePirServer<Segmented3aryScheme, SimplePirBackend>;

/// A server holding [`ACCOUNTS`] deterministic accounts at block 0.
fn build_server() -> Server {
    let codec = value_codec();
    let geom = Geometry::for_accounts(ACCOUNTS, ARITY, BUCKET_SIZE, FINGERPRINT_BITS, &codec, Backend::Simple)
        .expect("geometry");
    let mut store = Segmented3aryCuckooKVStore::new(
        geom.num_buckets,
        geom.bucket_size,
        geom.fingerprint_bits,
        geom.value_bits,
        geom.plaintext_bits,
    )
    .expect("store");
    for i in 0..ACCOUNTS {
        let (addr, balance) = account(i);
        let key = keccak256(&addr);
        let v = codec.encode(&key, balance).expect("encode");
        store.insert(key, &v).expect("insert");
    }
    RisePirServer::new(store, SimpleConfig::with_lwe_dim(LWE_DIM), codec, 0)
}

fn setup_bytes(server: &Server) -> Vec<u8> {
    let bundle: SetupBundle<SimplePirBackend> = server.setup();
    wire::encode_setup(&bundle)
}

/// Bring the ABI up against `server`, in the deployment mode `complete`.
fn init(server: &Server, complete: bool) {
    let mode = [u8::from(complete)];
    let n = put_input(&mode);
    assert_eq!(risepir_set_mode(n), 0, "set_mode: {}", last_error());
    let setup = setup_bytes(server);
    let n = put_input(&setup);
    assert_eq!(risepir_init(n), 0, "init: {}", last_error());
}

/// The server side of one `POST /answer`, over the real wire codec.
fn answer_over_wire(server: &Server, query_body: &[u8]) -> Vec<u8> {
    let params = server.params();
    let backend_params = server.setup().backend_params;
    let expected: Vec<u32> = backend_params.iter().map(|sp| sp.reshape_rows).collect();
    let queries = wire::decode_query_bundle(query_body, &params, &expected).expect("server decodes our query");
    let (responses, head) = server.answer(&queries).expect("server answers");
    wire::encode_response_bundle(&responses, head)
}

/// One full lookup through the ABI, syncing exactly as a host must.
/// Returns `(status, balance_bytes)`.
fn lookup(server: &Server, addr: &[u8; 20]) -> (i32, Vec<u8>) {
    let n = put_input(addr);
    let qlen = risepir_query(n);
    assert!(qlen > 0, "query: {}", last_error());
    let query_body = output();
    assert_eq!(query_body.len(), qlen as usize);

    let response_body = answer_over_wire(server, &query_body);
    let n = put_input(&response_body);
    let at_block = risepir_answer(n);
    assert!(at_block >= 0, "answer: {}", last_error());

    let status = risepir_finish();
    (status, output())
}

fn balance_of(bytes: &[u8]) -> u128 {
    u128::from_le_bytes(bytes.try_into().expect("16-byte balance"))
}

// ─── the happy path ────────────────────────────────────────────────────

#[test]
fn lookup_returns_the_exact_balance() {
    let server = build_server();
    init(&server, true);

    for i in [0u64, 1, 7, 999, ACCOUNTS - 1] {
        let (addr, expected) = account(i);
        let (status, out) = lookup(&server, &addr);
        assert_eq!(status, STATUS_FOUND, "account {i}: {}", last_error());
        assert_eq!(balance_of(&out), expected, "account {i} balance");
    }
}

#[test]
fn absent_account_is_zero_for_a_complete_set() {
    let server = build_server();
    init(&server, true);

    let mut addr = [0u8; 20];
    addr[0] = 0xDE;
    addr[1] = 0xAD;
    let (status, out) = lookup(&server, &addr);
    assert_eq!(status, STATUS_ZERO, "{}", last_error());
    assert!(out.is_empty(), "a zero answer carries no balance bytes");
}

/// The rule that outranks everything: in a partial deployment, absence is
/// *unknown*, not zero. The flag comes from the server's `GET /mode` body
/// and there is no client-side default (ADR-0015/0017).
#[test]
fn absent_account_is_untracked_for_a_partial_set() {
    let server = build_server();
    init(&server, false);

    let mut addr = [0u8; 20];
    addr[0] = 0xDE;
    addr[1] = 0xAD;
    let (status, _) = lookup(&server, &addr);
    assert_eq!(status, STATUS_UNTRACKED, "{}", last_error());
    assert_ne!(status, STATUS_ZERO, "partial mode must never answer 0x0 for an untracked account");
}

// ─── the rewind ────────────────────────────────────────────────────────

/// `docs/plan.md` §3.3: a client pinned at block₀ answering against a
/// server at a later head must fold the public delta in first. The ABI
/// refuses to finish before that — and the refusal is recoverable, so the
/// host syncs and completes the *same* lookup rather than starting over.
#[test]
fn finish_refuses_until_the_delta_is_synced_then_succeeds() {
    let mut server = build_server();
    init(&server, true);

    // Move the server forward one block, changing an account we then read.
    let (addr, _) = account(42);
    let key = keccak256(&addr);
    let new_balance = 424_242_424_242_424_242u128;
    let delta = server
        .apply_block(&BlockUpdate {
            block: 1,
            changes: vec![(key, new_balance)],
            credits: Vec::new(),
        })
        .expect("apply_block");

    // Query without syncing: the answer arrives stamped at block 1 while
    // the client's delta still reaches only block 0.
    let n = put_input(&addr);
    assert!(risepir_query(n) > 0, "query: {}", last_error());
    let query_body = output();
    let response_body = answer_over_wire(&server, &query_body);
    let n = put_input(&response_body);
    assert_eq!(risepir_answer(n), 1, "answer should be stamped at block 1");

    assert_eq!(risepir_finish(), STATUS_ERROR, "finish must refuse to guess the span");
    let err = last_error();
    assert!(err.contains("answered at block 1"), "error should name the block: {err}");
    assert!(err.contains("sync"), "error should say what to do: {err}");

    // Sync, then finish the same in-flight lookup.
    let delta_bytes = codec::encode_block_delta(&delta, server.params().plaintext_bits);
    let n = put_input(&delta_bytes);
    assert_eq!(risepir_ingest(n), 1, "ingest: {}", last_error());

    let status = risepir_finish();
    assert_eq!(status, STATUS_FOUND, "{}", last_error());
    assert_eq!(balance_of(&output()), new_balance, "must be the post-block balance");
}

/// The step-4-before-step-5 trap, through the ABI: an account that did not
/// exist when the hint was pinned must still be found, not reported as
/// `0x0`.
#[test]
fn account_created_after_the_pin_is_found() {
    let mut server = build_server();
    init(&server, true);

    let mut addr = [0u8; 20];
    addr[0] = 0x5E;
    addr[1] = 0x77;
    let key = keccak256(&addr);
    let balance = 987_654_321_000_000_000u128;

    let delta = server
        .apply_block(&BlockUpdate {
            block: 1,
            changes: vec![(key, balance)],
            credits: Vec::new(),
        })
        .expect("apply_block");
    let delta_bytes = codec::encode_block_delta(&delta, server.params().plaintext_bits);
    let n = put_input(&delta_bytes);
    assert_eq!(risepir_ingest(n), 1, "ingest: {}", last_error());

    let (status, out) = lookup(&server, &addr);
    assert_eq!(status, STATUS_FOUND, "{}", last_error());
    assert_eq!(balance_of(&out), balance);
}

// ─── the entropy property ──────────────────────────────────────────────

/// Two queries for the *same* address must not produce the same bytes.
/// Equal bytes would mean the LWE secret was reused (or fixed), which is
/// exactly how a server would learn which bucket was asked for — the one
/// failure that still returns correct balances while destroying the
/// privacy claim, so it is asserted rather than assumed.
#[test]
fn repeated_queries_for_one_address_differ() {
    let server = build_server();
    init(&server, true);
    let (addr, _) = account(11);

    let (_, _) = lookup(&server, &addr);
    let n = put_input(&addr);
    assert!(risepir_query(n) > 0, "{}", last_error());
    let first = output();

    // Finish it so the in-flight slot is free, then ask again.
    let response_body = answer_over_wire(&server, &first);
    let n = put_input(&response_body);
    assert!(risepir_answer(n) >= 0);
    assert_eq!(risepir_finish(), STATUS_FOUND);

    let n = put_input(&addr);
    assert!(risepir_query(n) > 0, "{}", last_error());
    let second = output();

    assert_eq!(first.len(), second.len(), "same geometry, so same length");
    assert_ne!(first, second, "a repeated query must carry fresh randomness");
}

// ─── refusing bad input ────────────────────────────────────────────────

#[test]
fn mode_byte_must_be_exactly_zero_or_one() {
    for body in [vec![], vec![2u8], vec![0u8, 1], vec![255]] {
        let n = put_input(&body);
        assert_eq!(risepir_set_mode(n), STATUS_ERROR, "accepted mode body {body:?}");
        assert!(last_error().contains("/mode"), "{}", last_error());
    }
}

#[test]
fn setup_must_precede_use_and_mode_must_precede_setup() {
    // Nothing loaded at all.
    let n = put_input(&[0u8; 20]);
    assert_eq!(risepir_query(n), i64::from(STATUS_ERROR));
    assert!(last_error().contains("not initialised"), "{}", last_error());

    // Setup without mode: refused rather than defaulted.
    let server = build_server();
    let setup = setup_bytes(&server);
    let n = put_input(&setup);
    assert_eq!(risepir_init(n), STATUS_ERROR);
    assert!(last_error().contains("/mode"), "{}", last_error());
}

/// Truncated, corrupt, and empty setup bodies are the realistic failure
/// for a ~50 MB download. Every one must be a clean error.
#[test]
fn malformed_setup_is_rejected_without_panicking() {
    let server = build_server();
    let good = setup_bytes(&server);

    let cases: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"not a setup bundle at all".to_vec(),
        good[..good.len() / 2].to_vec(),
        good[..4].to_vec(),
        {
            let mut flipped = good.clone();
            flipped[0] ^= 0xFF;
            flipped
        },
        {
            let mut extra = good.clone();
            extra.push(0);
            extra
        },
    ];

    for (i, body) in cases.iter().enumerate() {
        let n = put_input(&[1u8]);
        assert_eq!(risepir_set_mode(n), 0);
        let n = put_input(body);
        assert_eq!(risepir_init(n), STATUS_ERROR, "case {i} was accepted");
        assert!(!last_error().is_empty(), "case {i} gave no reason");
    }
}

#[test]
fn malformed_answer_and_delta_bodies_are_rejected() {
    let server = build_server();
    init(&server, true);
    let (addr, _) = account(3);

    let n = put_input(&addr);
    assert!(risepir_query(n) > 0);
    let good = answer_over_wire(&server, &output());

    for body in [Vec::new(), b"garbage".to_vec(), good[..good.len() - 1].to_vec()] {
        let n = put_input(&body);
        assert_eq!(risepir_answer(n), i64::from(STATUS_ERROR), "accepted {} bytes", body.len());
        assert!(!last_error().is_empty());
    }

    for body in [Vec::new(), b"garbage".to_vec(), vec![0u8; 3]] {
        let n = put_input(&body);
        assert_eq!(risepir_ingest(n), i64::from(STATUS_ERROR));
        assert!(!last_error().is_empty());
    }
}

#[test]
fn address_must_be_twenty_bytes() {
    let server = build_server();
    init(&server, true);

    for len in [0usize, 19, 21, 32] {
        let n = put_input(&vec![7u8; len]);
        assert_eq!(risepir_query(n), i64::from(STATUS_ERROR), "accepted a {len}-byte address");
        assert!(last_error().contains("20 bytes"), "{}", last_error());
    }
}

/// The backend keeps one in-flight slot per segment; a second query before
/// the first is finished would discard the first's secret and decode it
/// into garbage. Refused loudly instead.
#[test]
fn a_second_query_before_finishing_is_refused() {
    let server = build_server();
    init(&server, true);
    let (addr, expected) = account(5);

    let n = put_input(&addr);
    assert!(risepir_query(n) > 0);
    let first_query = output();

    let (other, _) = account(6);
    let n = put_input(&other);
    assert_eq!(risepir_query(n), i64::from(STATUS_ERROR));
    assert!(last_error().contains("already in flight"), "{}", last_error());

    // The first lookup is untouched and still completes correctly.
    let response_body = answer_over_wire(&server, &first_query);
    let n = put_input(&response_body);
    assert!(risepir_answer(n) >= 0);
    assert_eq!(risepir_finish(), STATUS_FOUND, "{}", last_error());
    assert_eq!(balance_of(&output()), expected);
}

#[test]
fn finish_without_a_query_is_refused() {
    let server = build_server();
    init(&server, true);
    assert_eq!(risepir_finish(), STATUS_ERROR);
    assert!(last_error().contains("no query in flight"), "{}", last_error());
}

/// Corruption of a response must never produce a *wrong number*.
///
/// Note carefully what is and is not claimed here, because an earlier
/// version of this test claimed the wrong thing. SimplePIR decoding is
/// noise-tolerant by construction: a small perturbation is absorbed by the
/// rounding and the balance comes back **correct**. That is a feature, not
/// a failure — and most single-byte flips do not even land on the queried
/// account's own slot among the bucket's cells. What must never happen is
/// `STATUS_FOUND` carrying a balance that is not the true one. Past the
/// noise threshold, the value checksum (`docs/plan.md` §3.5) or the
/// fp ∧ `key_tag` mask has to catch it.
///
/// So the assertion is exactly the project's rule — erroring is fine,
/// `0x0` from a complete set is fine, a silently wrong balance is total
/// failure — checked over both regimes: single-byte flips (which should
/// mostly be absorbed, and correct when they are) and gross corruption of
/// the whole payload (which must be caught).
#[test]
fn a_corrupted_response_never_becomes_a_wrong_balance() {
    let server = build_server();
    init(&server, true);
    let (addr, expected) = account(23);

    /// Header bytes before the first segment's payload: magic(4) +
    /// arity(1) + head(8). Corrupting past this hits payload framing and
    /// data rather than the magic.
    const HEADER: usize = 13;

    // Runs one lookup whose response bytes are mangled by `mangle`, and
    // returns `Some(balance)` if the ABI reported FOUND.
    let run = |mangle: &dyn Fn(&mut Vec<u8>)| -> Option<u128> {
        let n = put_input(&addr);
        assert!(risepir_query(n) > 0, "{}", last_error());
        let clean = answer_over_wire(&server, &output());
        let mut body = clean.clone();
        mangle(&mut body);

        let n = put_input(&body);
        if risepir_answer(n) < 0 {
            // Rejected at the wire boundary (mangled length prefix). The
            // in-flight query is still open — close it out cleanly so the
            // next iteration starts from a known state.
            let n = put_input(&clean);
            assert!(risepir_answer(n) >= 0, "{}", last_error());
            assert_eq!(risepir_finish(), STATUS_FOUND, "{}", last_error());
            return None;
        }
        match risepir_finish() {
            STATUS_FOUND => Some(balance_of(&output())),
            STATUS_DECODE_FAILED | STATUS_ZERO | STATUS_ERROR => None,
            other => panic!("unexpected status {other}"),
        }
    };

    // Regime 1: single-byte flips. Whenever one still decodes, it must
    // decode to the *right* balance.
    let mut absorbed = 0usize;
    for (num, den) in [(1u64, 4u64), (1, 3), (1, 2), (2, 3), (3, 4)] {
        for mask in [0x01u8, 0x40, 0x80, 0xFF] {
            let got = run(&move |body: &mut Vec<u8>| {
                let idx = (body.len() as u64 * num / den) as usize;
                body[idx] ^= mask;
            });
            if let Some(balance) = got {
                assert_eq!(
                    balance, expected,
                    "a single-byte flip (mask {mask:#04x}) produced a WRONG balance"
                );
                absorbed += 1;
            }
        }
    }
    assert!(absorbed > 0, "every single-byte flip was rejected; the noise-tolerance regime went unexercised");

    // Regime 2: gross corruption, well past any noise budget. This must
    // never come back as a number — and if it somehow does, that number
    // must still be the true balance.
    let mut caught = 0usize;
    for mask in [0x80u8, 0xC0, 0xFF] {
        let got = run(&move |body: &mut Vec<u8>| {
            for b in body.iter_mut().skip(HEADER) {
                *b ^= mask;
            }
        });
        match got {
            None => caught += 1,
            Some(balance) => assert_eq!(
                balance, expected,
                "wholesale corruption (mask {mask:#04x}) produced a WRONG balance"
            ),
        }
    }
    assert!(caught > 0, "wholesale corruption was never caught; the integrity checks went unexercised");
}

// ── lineage epoch (ADR-0033) ───────────────────────────────────────────

/// `risepir_epoch` must hand the host exactly the token
/// `wire::lineage_epoch` derives from the served bundle — the value the
/// server's `/sync`/`/answer` gate compares against, so any disagreement
/// here would wedge every browser lookup behind spurious 409s.
#[test]
fn epoch_matches_the_wire_derivation_and_needs_a_session() {
    // Before init: an error, not a made-up token.
    assert_eq!(risepir_epoch(), i64::from(STATUS_ERROR), "no session ⇒ no epoch");

    let server = build_server();
    init(&server, true);

    let n = risepir_epoch();
    assert!(n > 0, "epoch: {}", last_error());
    let epoch = String::from_utf8(output()).expect("epoch is ASCII hex");
    assert_eq!(epoch.len(), 16, "16 lowercase-hex chars");
    assert!(epoch.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_eq!(
        epoch,
        wire::lineage_epoch(&server.setup().backend_params),
        "host-visible epoch must equal the derivation from the same bundle"
    );
}

/// `risepir_set_mode_byte` (the buffer-free header path, ADR-0033):
/// accepts exactly 0/1 — never defaults a garbled flag — and satisfies
/// `risepir_init`'s mode requirement just like `risepir_set_mode`.
#[test]
fn set_mode_byte_validates_and_initialises() {
    assert_eq!(risepir_set_mode_byte(2), STATUS_ERROR, "2 is not a mode");
    assert_eq!(risepir_set_mode_byte(u32::MAX), STATUS_ERROR);

    let server = build_server();
    assert_eq!(risepir_set_mode_byte(0), 0, "set_mode_byte(0): {}", last_error());
    let setup = setup_bytes(&server);
    let n = put_input(&setup);
    assert_eq!(risepir_init(n), 0, "init after set_mode_byte: {}", last_error());
    assert_eq!(risepir_complete(), 0, "mode 0 = partial must stick");
}
