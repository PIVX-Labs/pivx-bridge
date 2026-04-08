/// HTTP API — axum endpoints for the PIVX bridge.
///
/// All routes are prefixed with `/mainnet/` for PivxNodeController compatibility.
use std::sync::atomic::{AtomicU32, Ordering};
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
use crate::scanner;
use crate::stream;

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

    // For compact formats, fetch from node and encode on-the-fly
    let heights = {
        let index = state.index.read().await;
        index.heights_from(start)
    };

    if heights.is_empty() {
        return Ok(build_binary_response(Vec::new()));
    }

    let rpc_url = state.rpc.url().to_string();
    let rpc_user = state.rpc.user().to_string();
    let rpc_pass = state.rpc.pass().to_string();
    let state_ref = state.clone();

    let data = tokio::task::spawn_blocking(move || {
        let rpc = RpcClient::new(&rpc_url, &rpc_user, &rpc_pass);
        let chain_h = state_ref.chain_height.load(Ordering::Relaxed);
        let mut blocks = Vec::new();
        for height in heights {
            match scanner::scan_block(&rpc, height, &state_ref.block_cache, chain_h) {
                Ok(Some(block)) => blocks.push(block),
                Ok(None) => {}
                Err(e) => return Err(format!("scan error at height {height}: {e}")),
            }
        }
        Ok(stream::encode_shield_stream(&blocks, format))
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")))?
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

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
}

/// Return the byte count of shield data between two block heights.
///
/// Used by MPW for sync progress bars.
pub async fn get_shield_data_length(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ShieldDataLengthQuery>,
) -> Result<String, (StatusCode, String)> {
    let start = query.start_block.unwrap_or(0);
    let end = query.end_block.unwrap_or(u32::MAX);
    let buffer_len = state.shield_buffer.read().await.len() as u64;

    let length = {
        let index = state.index.read().await;
        index.byte_length_between(start, end, buffer_len)
    };

    Ok(length.to_string())
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
