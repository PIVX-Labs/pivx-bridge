/// HTTP API — axum endpoints for the PIVX bridge.
///
/// All routes are prefixed with `/mainnet/` for PivxNodeController compatibility.
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::index::ShieldIndex;
use crate::rpc::RpcClient;
use crate::scanner;
use crate::stream;

/// Shared application state.
pub struct AppState {
    pub rpc: RpcClient,
    pub index: RwLock<ShieldIndex>,
    pub index_path: String,
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

/// Serve shield data as a binary stream.
pub async fn get_shield_data(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ShieldDataQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let start = query.start_block.unwrap_or(0);
    let format = query.format;
    eprintln!("  [shielddata] Request startBlock={start} format={format:?}");

    // Get shield block heights from the index
    let heights = {
        let index = state.index.read().await;
        index.heights_from(start)
    };
    eprintln!("  [shielddata] {} shield blocks to serve", heights.len());

    if heights.is_empty() {
        return Ok((
            StatusCode::OK,
            [("Content-Type", "application/octet-stream")],
            Vec::new(),
        ));
    }

    // Fetch and encode blocks (blocking RPC calls in spawn_blocking)
    let rpc_url = state.rpc.url().to_string();
    let rpc_user = state.rpc.user().to_string();
    let rpc_pass = state.rpc.pass().to_string();

    let data = tokio::task::spawn_blocking(move || {
        let rpc = RpcClient::new(&rpc_url, &rpc_user, &rpc_pass);
        let mut blocks = Vec::new();

        for height in heights {
            match scanner::scan_block(&rpc, height) {
                Ok(Some(block)) => blocks.push(block),
                Ok(None) => {}
                Err(e) => return Err(format!("scan error at height {height}: {e}")),
            }
        }

        Ok(stream::encode_shield_stream(&blocks, format))
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task error: {e}")))?
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok((
        StatusCode::OK,
        [("Content-Type", "application/octet-stream")],
        data,
    ))
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

/// Broadcast a raw transaction to the network.
pub async fn send_raw_transaction(
    State(state): State<Arc<AppState>>,
    body: String,
) -> Result<String, (StatusCode, String)> {
    let hex = body.trim();
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
