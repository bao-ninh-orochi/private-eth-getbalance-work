//! Server state persistence: one file holding everything
//! [`RisePirServer::from_parts`] needs to reassemble a mainnet deployment
//! after a restart — cells, per-segment `A` params + hints, block, item
//! count, codec widths, and whether the account set is *complete*
//! (snapshot-bootstrapped) or *partial* (`docs/deploy.md`).
//!
//! # Format (all integers little-endian)
//!
//! ```text
//! magic  b"RPST1"
//! u8     complete            (1 = complete nonzero set; 0 = partial)
//! u32×3  key_tag_bits, balance_bits, checksum_bits
//! u64    num_items
//! u64    setup_len,  then setup_len bytes   (risepir_http::wire::encode_setup)
//! u64    cells_len,  then cells_len × u32   (the store's flat cell array)
//! ```
//!
//! The setup section reuses the HTTP transport's own `GET /setup` codec —
//! one codec, one set of length-validation rules, no second serializer to
//! drift. Writes go to `<path>.tmp` then rename, so a crash mid-save
//! never truncates the previous good state.
//!
//! # Scale note
//!
//! At mainnet-complete scale the cells section is ~10 GB; save/load are
//! streamed through fixed 64 Ki-cell chunks, never materialising a second
//! byte-copy of the array.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use ikpir_common::SimplePirBackend;
use risepir_http::wire;
use risepir_proto::ValueCodec;
use risepir_server::{RisePirServer, SetupBundle};
use segmented_cuckoo::{Segmented3aryCuckooKVStore, Segmented3aryScheme};

const MAGIC: &[u8; 5] = b"RPST1";
/// Cells per streamed chunk (256 KiB of bytes at 4 B/cell).
const CHUNK_CELLS: usize = 64 * 1024;

/// The concrete server type this deployment persists.
pub type Server = RisePirServer<Segmented3aryScheme, SimplePirBackend>;

/// Everything [`load`] returns: the reassembled server plus the
/// completeness marker the JSON-RPC front end's `NotFound` policy is
/// keyed on (complete ⇒ `0x0`; partial ⇒ error — `docs/plan.md`
/// ADR-0015 only licenses `0x0`-by-absence for a *complete* set).
pub struct LoadedState {
    /// The reassembled server, bit-identical `A`/hints to the run that
    /// saved it.
    pub server: Server,
    /// Whether the persisted set was complete (snapshot-bootstrapped).
    pub complete: bool,
}

/// Errors from [`save`] / [`load`]. Strings carry enough context to
/// diagnose a bad file without a debugger; a state file that fails *any*
/// check is rejected outright — restarting from a corrupt state and
/// serving from it would be a wrong answer waiting to happen.
#[derive(Debug)]
pub enum StateError {
    /// File open/read/write/rename failure.
    Io(String),
    /// The file is not a well-formed `RPST1` state file, or its contents
    /// failed a consistency check (codec mismatch, cell-count mismatch,
    /// store reconstruction rejection).
    Corrupt(String),
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(m) => write!(f, "state file I/O: {m}"),
            Self::Corrupt(m) => write!(f, "state file rejected: {m}"),
        }
    }
}

impl std::error::Error for StateError {}

fn io_err(e: impl std::fmt::Display) -> StateError {
    StateError::Io(e.to_string())
}

/// Serialize `server` (+ the completeness marker) to `path`, atomically
/// (`<path>.tmp` + rename).
pub fn save(server: &Server, codec: &ValueCodec, complete: bool, path: &Path) -> Result<(), StateError> {
    let tmp = path.with_extension("tmp");
    {
        let file = File::create(&tmp).map_err(io_err)?;
        let mut w = BufWriter::new(file);

        w.write_all(MAGIC).map_err(io_err)?;
        w.write_all(&[u8::from(complete)]).map_err(io_err)?;
        for width in [codec.key_tag_bits, codec.balance_bits, codec.checksum_bits] {
            w.write_all(&width.to_le_bytes()).map_err(io_err)?;
        }
        w.write_all(&server.num_items().to_le_bytes()).map_err(io_err)?;

        let setup_bytes = wire::encode_setup(&server.setup());
        w.write_all(&(setup_bytes.len() as u64).to_le_bytes()).map_err(io_err)?;
        w.write_all(&setup_bytes).map_err(io_err)?;

        let cells = server.snapshot_cells();
        w.write_all(&(cells.len() as u64).to_le_bytes()).map_err(io_err)?;
        let mut chunk = Vec::with_capacity(CHUNK_CELLS * 4);
        for block in cells.chunks(CHUNK_CELLS) {
            chunk.clear();
            for c in block {
                chunk.extend_from_slice(&c.to_le_bytes());
            }
            w.write_all(&chunk).map_err(io_err)?;
        }
        w.flush().map_err(io_err)?;
    }
    std::fs::rename(&tmp, path).map_err(io_err)
}

/// Read a state file back into a running server. `config` must be the
/// same backend config the deployment always uses (it only matters for a
/// later `full_rebuild`); `codec` must match the widths recorded in the
/// file — a mismatch means the operator changed the value encoding
/// between runs, which silently breaks every stored slot, so it is
/// rejected loudly instead.
pub fn load(path: &Path, config: ikpir_common::SimpleConfig, codec: &ValueCodec) -> Result<LoadedState, StateError> {
    let file = File::open(path).map_err(io_err)?;
    let mut r = BufReader::new(file);

    let mut magic = [0u8; 5];
    r.read_exact(&mut magic).map_err(io_err)?;
    if &magic != MAGIC {
        return Err(StateError::Corrupt("bad magic (not an RPST1 state file)".to_string()));
    }
    let mut flag = [0u8; 1];
    r.read_exact(&mut flag).map_err(io_err)?;
    let complete = match flag[0] {
        0 => false,
        1 => true,
        other => return Err(StateError::Corrupt(format!("bad completeness flag {other}"))),
    };

    let mut widths = [0u32; 3];
    for w in &mut widths {
        *w = read_u32(&mut r)?;
    }
    let file_codec = ValueCodec {
        key_tag_bits: widths[0],
        balance_bits: widths[1],
        checksum_bits: widths[2],
    };
    if file_codec != *codec {
        return Err(StateError::Corrupt(format!(
            "value codec mismatch: file has key_tag/balance/checksum = {}/{}/{} bits, \
             deployment configured {}/{}/{}",
            widths[0], widths[1], widths[2], codec.key_tag_bits, codec.balance_bits, codec.checksum_bits
        )));
    }

    let num_items = read_u64(&mut r)?;

    let setup_len = read_u64(&mut r)?;
    let setup_len = usize::try_from(setup_len).map_err(|_| StateError::Corrupt("setup_len overflow".to_string()))?;
    let mut setup_bytes = vec![0u8; setup_len];
    r.read_exact(&mut setup_bytes).map_err(io_err)?;
    let SetupBundle {
        params,
        backend_params,
        hints,
        block,
    } = wire::decode_setup(&setup_bytes).map_err(|e| StateError::Corrupt(format!("setup section: {e}")))?;

    let cells_len = read_u64(&mut r)?;
    let cells_len = usize::try_from(cells_len).map_err(|_| StateError::Corrupt("cells_len overflow".to_string()))?;
    let expected =
        params.num_buckets as usize * params.bucket_size as usize * params.cells_per_slot() as usize;
    if cells_len != expected {
        return Err(StateError::Corrupt(format!(
            "cells_len {cells_len} does not match the setup section's geometry ({expected} cells)"
        )));
    }
    let mut cells = vec![0u32; cells_len];
    let mut buf = vec![0u8; CHUNK_CELLS * 4];
    let mut filled = 0usize;
    while filled < cells_len {
        let take = (cells_len - filled).min(CHUNK_CELLS);
        let bytes = &mut buf[..take * 4];
        r.read_exact(bytes).map_err(io_err)?;
        for (i, four) in bytes.chunks_exact(4).enumerate() {
            cells[filled + i] = u32::from_le_bytes([four[0], four[1], four[2], four[3]]);
        }
        filled += take;
    }
    // Anything after the cells is corruption, same posture as the wire
    // codecs' TrailingBytes.
    let mut probe = [0u8; 1];
    match r.read(&mut probe) {
        Ok(0) => {}
        Ok(_) => return Err(StateError::Corrupt("trailing bytes after cells section".to_string())),
        Err(e) => return Err(io_err(e)),
    }

    let store = Segmented3aryCuckooKVStore::from_cells(cells, params, num_items)
        .map_err(|e| StateError::Corrupt(format!("store reconstruction: {e:?}")))?;
    let server = Server::from_parts(store, config, *codec, backend_params, hints, block);
    Ok(LoadedState { server, complete })
}

fn read_u32(r: &mut impl Read) -> Result<u32, StateError> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).map_err(io_err)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut impl Read) -> Result<u64, StateError> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b).map_err(io_err)?;
    Ok(u64::from_le_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ikpir_common::pir_params::simple_max_plaintext_bits;
    use ikpir_common::backend::simple::SimpleParams;
    use ikpir_common::{IndexPirBackend, SimpleConfig};
    use risepir_proto::keccak256;
    use risepir_proto::BlockUpdate;

    fn codec() -> ValueCodec {
        ValueCodec {
            key_tag_bits: 32,
            balance_bits: 96,
            checksum_bits: 16,
        }
    }

    fn small_server() -> Server {
        let codec = codec();
        let num_buckets = 3 * 64;
        let pb = simple_max_plaintext_bits(num_buckets / 3, 4, 32, codec.value_bits(), SimpleParams::DEFAULT_SIGMA);
        let store = Segmented3aryCuckooKVStore::new(num_buckets, 4, 32, codec.value_bits(), pb).unwrap();
        Server::new(store, SimpleConfig::with_lwe_dim(256), codec, 0)
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("risepir-state-{}-{name}", std::process::id()))
    }

    /// Round trip: save a populated, block-advanced server; load it; the
    /// hints, A-params, block, item count, completeness, and answers to a
    /// verified read must all survive bit-exactly — and the reloaded
    /// server must keep applying blocks from where it left off.
    #[test]
    fn round_trip_preserves_everything_and_keeps_running() {
        let mut server = small_server();
        let addrs: Vec<_> = (0u8..50).map(|i| keccak256(&[i; 20])).collect();
        server
            .apply_block(&BlockUpdate {
                block: 7,
                changes: addrs.iter().enumerate().map(|(i, a)| (*a, 1_000u128 + i as u128)).collect(),
                credits: vec![(addrs[0], 5u128)],
            })
            .unwrap();

        let before = server.setup();
        let path = tmp("roundtrip.bin");
        save(&server, &codec(), true, &path).unwrap();
        let LoadedState { server: mut reloaded, complete } = load(&path, SimpleConfig::with_lwe_dim(256), &codec()).unwrap();
        std::fs::remove_file(&path).unwrap();

        assert!(complete);
        assert_eq!(reloaded.block(), 7);
        assert_eq!(reloaded.num_items(), 50);
        let after = reloaded.setup();
        for (j, (b, a)) in before.hints.iter().zip(&after.hints).enumerate() {
            assert_eq!(b.data, a.data, "segment {j}: hint bytes must survive the round trip");
        }
        for (j, (b, a)) in before.backend_params.iter().zip(&after.backend_params).enumerate() {
            // A is expanded deterministically from ServerParams; equal
            // expanded material ⇒ equal A. (SimpleServerParams has no
            // PartialEq; the expanded hint material is the observable.)
            let mb = SimplePirBackend::expand_hint_material(b);
            let ma = SimplePirBackend::expand_hint_material(a);
            assert_eq!(mb.a, ma.a, "segment {j}: A must survive the round trip");
        }
        assert_eq!(reloaded.balance_of(&addrs[0]).unwrap(), Some(1_005));
        assert_eq!(reloaded.balance_of(&addrs[49]).unwrap(), Some(1_049));

        // Still a working server: apply another block on top.
        reloaded
            .apply_block(&BlockUpdate {
                block: 8,
                changes: vec![(addrs[1], 0u128)],
                credits: vec![],
            })
            .unwrap();
        assert_eq!(reloaded.balance_of(&addrs[1]).unwrap(), None);
    }

    #[test]
    fn partial_flag_survives() {
        let server = small_server();
        let path = tmp("partial.bin");
        save(&server, &codec(), false, &path).unwrap();
        let loaded = load(&path, SimpleConfig::with_lwe_dim(256), &codec()).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(!loaded.complete);
    }

    #[test]
    fn codec_mismatch_and_corruption_rejected() {
        let server = small_server();
        let path = tmp("reject.bin");
        save(&server, &codec(), true, &path).unwrap();

        let wrong = ValueCodec {
            key_tag_bits: 16,
            ..codec()
        };
        assert!(matches!(
            load(&path, SimpleConfig::with_lwe_dim(256), &wrong),
            Err(StateError::Corrupt(_))
        ));

        // Truncated file: cut the last 100 bytes.
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 100]).unwrap();
        assert!(load(&path, SimpleConfig::with_lwe_dim(256), &codec()).is_err());

        // Bad magic.
        let mut bad = bytes.clone();
        bad[0] ^= 0xFF;
        std::fs::write(&path, &bad).unwrap();
        assert!(matches!(
            load(&path, SimpleConfig::with_lwe_dim(256), &codec()),
            Err(StateError::Corrupt(_))
        ));

        // Trailing garbage.
        let mut long = bytes;
        long.push(0);
        std::fs::write(&path, &long).unwrap();
        assert!(matches!(
            load(&path, SimpleConfig::with_lwe_dim(256), &codec()),
            Err(StateError::Corrupt(_))
        ));

        std::fs::remove_file(&path).unwrap();
    }
}
