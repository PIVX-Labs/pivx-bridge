/// HTTP API — axum endpoints for the PIVX bridge.
///
/// All routes are prefixed with `/mainnet/` for PivxNodeController compatibility.
use std::sync::atomic::AtomicU32;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};

use crate::block_cache::BlockCache;
use crate::index::ShieldIndex;
use crate::rpc::RpcClient;

/// Shared application state.
pub struct AppState {
    pub rpc: RpcClient,
    pub index: RwLock<ShieldIndex>,
    pub cache_file: Mutex<std::fs::File>,
    /// In-memory shield buffer — the entire shield.bin held in RAM.
    /// Requests serve slices directly from this buffer (zero disk I/O).
    pub shield_buffer: RwLock<Vec<u8>>,
    pub allowed_rpcs: Vec<String>,
    /// Last chain height fully scanned (not just last shield block).
    pub last_scanned_height: RwLock<u32>,
    /// LRU cache: height → block hash. Eliminates redundant getblockhash RPCs.
    pub hash_cache: RwLock<HashCache>,
    /// Block JSON cache: `(hash, verbosity)` → stripped response.
    /// Eliminates repeated `getblock` RPCs for the same blocks.
    pub block_cache: std::sync::RwLock<BlockCache>,
    /// Current chain height — updated by ZMQ/polling, used for rehydration.
    pub chain_height: AtomicU32,
    /// True when ZMQ is connected and receiving blocks. Polling is disabled.
    pub zmq_active: std::sync::atomic::AtomicBool,
}

/// Fixed-size LRU cache for block height → hash mappings.
pub struct HashCache {
    entries: std::collections::HashMap<u32, String>,
    order: std::collections::VecDeque<u32>,
    capacity: usize,
}

impl HashCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: std::collections::HashMap::with_capacity(capacity),
            order: std::collections::VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn get(&self, height: u32) -> Option<&str> {
        self.entries.get(&height).map(|s| s.as_str())
    }

    pub fn insert(&mut self, height: u32, hash: String) {
        if self.entries.contains_key(&height) {
            return;
        }
        if self.entries.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(height, hash);
        self.order.push_back(height);
    }
}

// ---------------------------------------------------------------------------
// Stream format
// ---------------------------------------------------------------------------

/// Stream format for the shield sync protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StreamFormat {
    /// PIVX-compatible (default): 0x03 full raw tx + 0x5d block footer with time.
    /// Byte-identical to PivxNodeController. MPW parses this unchanged.
    #[default]
    #[serde(rename = "pivx")]
    PivxCompat,
    /// Compact (0x04): strips proofs/sigs, keeps out_ciphertext (724 bytes/output).
    Compact,
    /// Compact+ (0x05): strips out_ciphertext too (644 bytes/output).
    #[serde(rename = "compactplus")]
    CompactPlus,
}

// ---------------------------------------------------------------------------
// GET /mainnet/getshielddata?startBlock=N&format=...
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ShieldDataQuery {
    #[serde(rename = "startBlock")]
    pub start_block: Option<u32>,
    #[serde(default)]
    pub format: StreamFormat,
}

/// Build a binary response with X-Content-Length header (PivxNodeController compat).
fn build_binary_response(data: Vec<u8>) -> Response {
    let len = data.len();
    let mut resp = Response::new(axum::body::Body::from(data));
    resp.headers_mut().insert("Content-Type", HeaderValue::from_static("application/octet-stream"));
    resp.headers_mut().insert("X-Content-Length", HeaderValue::from(len as u64));
    resp
}

/// Serve shield data as a binary stream.
///
/// For the default PIVX-compat format, serves from the shield.bin cache.
/// For compact formats, re-fetches from the node and encodes on-the-fly.
pub async fn get_shield_data(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ShieldDataQuery>,
) -> Result<Response, (StatusCode, String)> {
    let start = query.start_block.unwrap_or(0);
    let format = query.format;
    let timer = std::time::Instant::now();

    // For PIVX-compat format, serve directly from in-memory buffer
    if format == StreamFormat::PivxCompat {
        let offset = {
            let index = state.index.read().await;
            match index.offset_for_height(start) {
                Some(o) => o as usize,
                None => return Ok(build_binary_response(Vec::new())),
            }
        };

        let buffer = state.shield_buffer.read().await;
        if offset >= buffer.len() {
            return Ok(build_binary_response(Vec::new()));
        }

        // Single allocation: copy the slice into the response
        let data = buffer[offset..].to_vec();
        drop(buffer); // release read lock before sending
        crate::proxy::log_timing(timer, "getshielddata");
        return Ok(build_binary_response(data));
    }

    // Compact/CompactPlus: derive from the PivxCompat buffer by parsing
    // raw txs and extracting compact fields. Zero RPC, pure CPU.
    let offset = {
        let index = state.index.read().await;
        match index.offset_for_height(start) {
            Some(o) => o as usize,
            None => return Ok(build_binary_response(Vec::new())),
        }
    };

    let buffer = state.shield_buffer.read().await;
    if offset >= buffer.len() {
        return Ok(build_binary_response(Vec::new()));
    }

    let data = pivxcompat_to_compact(&buffer[offset..], format);
    drop(buffer);

    let label = if format == StreamFormat::Compact {
        "getshielddata (buffer→compact)"
    } else {
        "getshielddata (buffer→compact+)"
    };
    crate::proxy::log_timing(timer, label);
    Ok(build_binary_response(data))
}

// ---------------------------------------------------------------------------
// GET /mainnet/getshielddatalength?startBlock=N&endBlock=M
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ShieldDataLengthQuery {
    #[serde(rename = "startBlock")]
    pub start_block: Option<u32>,
    #[serde(rename = "endBlock")]
    pub end_block: Option<u32>,
    #[serde(default)]
    pub format: StreamFormat,
}

/// Return the byte count of shield data between two block heights.
///
/// Accepts an optional `format` parameter to return the accurate size
/// for compact or compact+ streams (derived from the PivxCompat buffer).
/// Used by MPW for sync progress bars.
pub async fn get_shield_data_length(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ShieldDataLengthQuery>,
) -> Result<String, (StatusCode, String)> {
    let start = query.start_block.unwrap_or(0);
    let end = query.end_block.unwrap_or(u32::MAX);
    let buffer = state.shield_buffer.read().await;
    let buffer_len = buffer.len() as u64;

    let length = if query.format == StreamFormat::PivxCompat {
        let index = state.index.read().await;
        index.byte_length_between(start, end, buffer_len)
    } else {
        // Compute exact size by deriving the compact stream for the range
        let index = state.index.read().await;
        let start_off = index.offset_for_height(start).unwrap_or(buffer_len) as usize;
        let end_off = index.entries.iter()
            .find(|e| e.block > end)
            .map(|e| e.i as usize)
            .unwrap_or(buffer.len());
        drop(index);

        if start_off >= buffer.len() || start_off >= end_off {
            0
        } else {
            pivxcompat_to_compact(&buffer[start_off..end_off], query.format).len() as u64
        }
    };

    Ok(length.to_string())
}

/// Transform a PivxCompat (0x03) buffer into Compact (0x04) or CompactPlus (0x05).
///
/// Walks the raw tx packets, parses Sapling fields via `parse_sapling_tx`,
/// and re-encodes as compact packets. Block footers (9 bytes with time) become
/// compact headers (5 bytes, height only, before txs).
fn pivxcompat_to_compact(data: &[u8], format: StreamFormat) -> Vec<u8> {
    use crate::scanner;

    let mut out = Vec::with_capacity(data.len() / 2); // compact is smaller
    let mut pos = 0;
    let mut pending_txs: Vec<Vec<u8>> = Vec::new();

    while pos + 4 <= data.len() {
        let pkt_len = u32::from_le_bytes([data[pos], data[pos+1], data[pos+2], data[pos+3]]) as usize;
        let pkt_end = pos + 4 + pkt_len;
        if pkt_end > data.len() || pkt_len == 0 { break; }

        let pkt_type = data[pos + 4];

        if pkt_type == 0x5d && pkt_len == 9 {
            // Block footer → emit compact header + compact txs
            let height = u32::from_le_bytes([data[pos+5], data[pos+6], data[pos+7], data[pos+8]]);

            // Compact block header (5 bytes)
            out.extend(5u32.to_le_bytes());
            out.push(0x5d);
            out.extend(height.to_le_bytes());

            // Encode each raw tx as compact
            for raw_tx in &pending_txs {
                if let Ok(Some(compact)) = scanner::parse_sapling_tx(raw_tx) {
                    encode_compact_packet(&mut out, &compact, format);
                }
            }
            pending_txs.clear();
        } else {
            // Raw tx (type byte is first byte of the tx = 0x03 for v3)
            pending_txs.push(data[pos + 4..pkt_end].to_vec());
        }

        pos = pkt_end;
    }

    out
}

/// Encode a single compact or compact+ tx packet.
fn encode_compact_packet(out: &mut Vec<u8>, compact: &crate::scanner::CompactTx, format: StreamFormat) {
    const ENC_CT: usize = 580;
    const OUT_CT: usize = 80;

    let (pkt_type, output_size) = match format {
        StreamFormat::Compact => (0x04u8, 32 + 32 + 32 + ENC_CT + OUT_CT), // cv+cmu+epk+enc+out
        StreamFormat::CompactPlus => (0x05u8, 32 + 32 + ENC_CT),           // cmu+epk+enc
        _ => unreachable!(),
    };

    let payload_len = 1 + 2 + compact.nullifiers.len() * 32 + compact.outputs.len() * output_size;
    out.extend((payload_len as u32).to_le_bytes());
    out.push(pkt_type);
    out.push(compact.nullifiers.len() as u8);
    out.push(compact.outputs.len() as u8);

    for nf in &compact.nullifiers {
        out.extend_from_slice(nf);
    }

    for o in &compact.outputs {
        if format == StreamFormat::Compact {
            out.extend_from_slice(&o.cv);
        }
        out.extend_from_slice(&o.cmu);
        out.extend_from_slice(&o.epk);
        out.extend_from_slice(&o.enc_ciphertext);
        if format == StreamFormat::Compact {
            out.extend_from_slice(&o.out_ciphertext);
        }
    }
}

// ---------------------------------------------------------------------------
// GET /mainnet/getblockcount
// ---------------------------------------------------------------------------

/// Return the current block height from the node.
pub async fn get_block_count(
    State(state): State<Arc<AppState>>,
) -> Result<String, (StatusCode, String)> {
    let count = state
        .rpc
        .get_block_count()
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("node error: {e}")))?;
    Ok(count.to_string())
}

// ---------------------------------------------------------------------------
// GET /mainnet/getshieldblocks
// ---------------------------------------------------------------------------

/// Return a JSON array of all shield block heights.
pub async fn get_shield_blocks(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<u32>> {
    let index = state.index.read().await;
    Json(index.shield_heights.clone())
}

// ---------------------------------------------------------------------------
// POST /mainnet/sendrawtransaction (body = hex string)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// GET /mainnet/address_index
// ---------------------------------------------------------------------------

/// Serve the address index SQLite file (used by MPW-Tauri for bootstrap).
pub async fn get_address_index() -> Result<Response, (StatusCode, String)> {
    let path = "address_index.sqlite";
    let data = tokio::fs::read(path)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, format!("address index not available: {e}")))?;

    let mut resp = Response::new(axum::body::Body::from(data));
    resp.headers_mut().insert("Content-Type", HeaderValue::from_static("application/octet-stream"));
    resp.headers_mut().insert(
        "Content-Disposition",
        HeaderValue::from_static("attachment; filename=\"address_index.sqlite\""),
    );
    Ok(resp)
}

// ---------------------------------------------------------------------------
// POST /mainnet/sendrawtransaction (body = hex string)
// ---------------------------------------------------------------------------

/// Broadcast a raw transaction to the network.
pub async fn send_raw_transaction(
    State(state): State<Arc<AppState>>,
    body: String,
) -> Result<String, (StatusCode, String)> {
    let hex = body.trim();
    if hex.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "empty transaction hex".into()));
    }
    if hex.len() > 10_000_000 {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, "transaction too large".into()));
    }
    eprintln!("  [broadcast] Received tx hex ({} bytes)", hex.len());

    match state.rpc.send_raw_transaction(hex) {
        Ok(txid) => {
            eprintln!("  [broadcast] Success: {txid}");
            Ok(txid)
        }
        Err(e) => {
            let msg = e.to_string();
            eprintln!("  [broadcast] FAILED: {msg}");
            Err((StatusCode::BAD_REQUEST, msg))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden vector: first 3 blocks of real PIVX shield.bin (mainnet).
    const SHIELD_SLICE: &[u8] = include_bytes!("../tests/shield_slice.bin");

    fn parse_block_heights(data: &[u8], footer_len: usize) -> Vec<u32> {
        let mut heights = Vec::new();
        let mut pos = 0;
        while pos + 4 <= data.len() {
            let pkt_len = u32::from_le_bytes([data[pos], data[pos+1], data[pos+2], data[pos+3]]) as usize;
            let pkt_end = pos + 4 + pkt_len;
            if pkt_end > data.len() || pkt_len == 0 { break; }
            if data[pos + 4] == 0x5d && pkt_len == footer_len {
                heights.push(u32::from_le_bytes([data[pos+5], data[pos+6], data[pos+7], data[pos+8]]));
            }
            pos = pkt_end;
        }
        heights
    }

    fn count_tx_packets(data: &[u8], pkt_type: u8) -> usize {
        let mut count = 0;
        let mut pos = 0;
        while pos + 4 <= data.len() {
            let pkt_len = u32::from_le_bytes([data[pos], data[pos+1], data[pos+2], data[pos+3]]) as usize;
            let pkt_end = pos + 4 + pkt_len;
            if pkt_end > data.len() || pkt_len == 0 { break; }
            if data[pos + 4] == pkt_type { count += 1; }
            pos = pkt_end;
        }
        count
    }

    #[test]
    fn pivxcompat_to_compact_golden_vector() {
        let compact = pivxcompat_to_compact(SHIELD_SLICE, StreamFormat::Compact);

        // Source has 3 blocks with PivxCompat footers (9 bytes)
        let src_heights = parse_block_heights(SHIELD_SLICE, 9);
        assert_eq!(src_heights.len(), 3, "golden vector should have 3 blocks");

        // Compact output should have 3 blocks with compact headers (5 bytes)
        let dst_heights = parse_block_heights(&compact, 5);
        assert_eq!(dst_heights, src_heights, "compact must preserve block heights");

        // Compact should have 0x04 tx packets, not 0x03
        assert_eq!(count_tx_packets(SHIELD_SLICE, 0x03), count_tx_packets(&compact, 0x04),
            "compact must have same number of txs as source");
        assert_eq!(count_tx_packets(&compact, 0x03), 0,
            "compact must not contain raw tx packets");

        // Compact should be smaller
        assert!(compact.len() < SHIELD_SLICE.len(),
            "compact ({}) should be smaller than pivxcompat ({})", compact.len(), SHIELD_SLICE.len());
    }

    #[test]
    fn pivxcompat_to_compactplus_golden_vector() {
        let compact = pivxcompat_to_compact(SHIELD_SLICE, StreamFormat::Compact);
        let compact_plus = pivxcompat_to_compact(SHIELD_SLICE, StreamFormat::CompactPlus);

        // CompactPlus should be even smaller (strips cv + out_ciphertext)
        assert!(compact_plus.len() < compact.len(),
            "compact+ ({}) should be smaller than compact ({})", compact_plus.len(), compact.len());

        // Same block heights
        let cp_heights = parse_block_heights(&compact_plus, 5);
        let src_heights = parse_block_heights(SHIELD_SLICE, 9);
        assert_eq!(cp_heights, src_heights);

        // 0x05 packets
        assert_eq!(count_tx_packets(&compact_plus, 0x05), count_tx_packets(SHIELD_SLICE, 0x03));
        assert_eq!(count_tx_packets(&compact_plus, 0x04), 0);
    }
}
