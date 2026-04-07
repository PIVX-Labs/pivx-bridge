/// PIVX Bridge — drop-in replacement for PivxNodeController.
///
/// Connects to a PIVX full node, scans for Sapling transactions, and serves
/// shield data to light wallets (MyPIVXWallet) over HTTP.
mod api;
mod config;
mod index;
mod rpc;
mod scanner;
mod stream;

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use clap::Parser;
use tokio::sync::RwLock;

#[tokio::main]
async fn main() {
    // Load .env if present
    let _ = dotenvy::dotenv();

    let config = config::Config::parse();

    eprintln!("╔══════════════════════════════════════════╗");
    eprintln!("║          PIVX Bridge v{}           ║", env!("CARGO_PKG_VERSION"));
    eprintln!("╠══════════════════════════════════════════╣");
    eprintln!("║  RPC:  {}  ", config.rpc_url);
    eprintln!("║  ZMQ:  {}  ", config.zmq_url);
    eprintln!("║  Port: {}                            ", config.port);
    eprintln!("╚══════════════════════════════════════════╝");

    // Connect to PIVX node
    let rpc = rpc::RpcClient::new(&config.rpc_url, &config.rpc_user, &config.rpc_pass);

    let chain_height = match rpc.get_block_count() {
        Ok(h) => {
            eprintln!("  Connected to PIVX node — chain height: {h}");
            h as u32
        }
        Err(e) => {
            eprintln!("  ERROR: Cannot connect to PIVX node: {e}");
            eprintln!("  Make sure the node is running with server=1 in pivx.conf");
            std::process::exit(1);
        }
    };

    // Load shield index
    let index_path = "shield_index.json".to_string();
    let mut shield_index = index::load_or_create(&index_path);

    // Initial scan
    let scan_from = shield_index
        .last_height()
        .map(|h| h + 1)
        .unwrap_or(config.sapling_height);

    if scan_from <= chain_height {
        eprintln!("  Scanning blocks {scan_from}..{chain_height} for Sapling transactions...");

        match scanner::scan_range(&rpc, scan_from, chain_height, |done, total| {
            if total > 0 && (done % 100 == 0 || done == total) {
                eprint!("\r  Progress: {done}/{total} blocks");
            }
        }) {
            Ok(blocks) => {
                let count = blocks.len();
                for block in &blocks {
                    shield_index.add(block.height, 0);
                }
                if let Err(e) = shield_index.save(&index_path) {
                    eprintln!("\n  Warning: failed to save index: {e}");
                }
                eprintln!("\n  Found {count} shield blocks ({} total indexed)",
                    shield_index.shield_heights.len());
            }
            Err(e) => {
                eprintln!("\n  Scan error: {e} — continuing with partial index");
            }
        }
    } else {
        eprintln!("  Index up to date — {} shield blocks indexed",
            shield_index.shield_heights.len());
    }

    // Build shared state
    let state = Arc::new(api::AppState {
        rpc: rpc::RpcClient::new(&config.rpc_url, &config.rpc_user, &config.rpc_pass),
        index: RwLock::new(shield_index),
        index_path: index_path.clone(),
    });

    // ZMQ background subscriber
    let zmq_state = state.clone();
    let zmq_url = config.zmq_url.clone();
    let zmq_rpc_url = config.rpc_url.clone();
    let zmq_rpc_user = config.rpc_user.clone();
    let zmq_rpc_pass = config.rpc_pass.clone();

    tokio::spawn(async move {
        eprintln!("  [zmq] Subscribing to {zmq_url}...");

        let mut subscriber = match bitcoincore_zmq::subscribe_async(&[&zmq_url]) {
            Ok(s) => {
                eprintln!("  [zmq] Connected — listening for new blocks");
                s
            }
            Err(e) => {
                eprintln!("  [zmq] Failed to subscribe: {e} — no live updates");
                return;
            }
        };

        use futures::StreamExt;
        while let Some(msg) = subscriber.next().await {
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("  [zmq] Stream error: {e}");
                    continue;
                }
            };

            if !matches!(msg, bitcoincore_zmq::Message::HashBlock(_, _)) {
                continue;
            }

            eprintln!("  [zmq] New block notification");

            let bg_rpc_url = zmq_rpc_url.clone();
            let bg_rpc_user = zmq_rpc_user.clone();
            let bg_rpc_pass = zmq_rpc_pass.clone();
            let bg_state = zmq_state.clone();
            let bg_index_path = index_path.clone();

            tokio::task::spawn_blocking(move || {
                let rpc = rpc::RpcClient::new(&bg_rpc_url, &bg_rpc_user, &bg_rpc_pass);

                let chain_height = match rpc.get_block_count() {
                    Ok(h) => h as u32,
                    Err(e) => {
                        eprintln!("  [zmq] RPC error: {e}");
                        return;
                    }
                };

                let last_height = {
                    let rt = tokio::runtime::Handle::current();
                    rt.block_on(async { bg_state.index.read().await.last_height() })
                };

                let scan_from = last_height.map(|h| h + 1).unwrap_or(0);
                if scan_from > chain_height {
                    return;
                }

                match scanner::scan_range(&rpc, scan_from, chain_height, |_, _| {}) {
                    Ok(blocks) => {
                        let count = blocks.len();
                        let rt = tokio::runtime::Handle::current();
                        rt.block_on(async {
                            let mut index = bg_state.index.write().await;
                            for block in &blocks {
                                index.add(block.height, 0);
                            }
                            if let Err(e) = index.save(&bg_index_path) {
                                eprintln!("  [zmq] Failed to save index: {e}");
                            }
                        });
                        if count > 0 {
                            eprintln!("  [zmq] Indexed {count} new shield block(s)");
                        }
                    }
                    Err(e) => {
                        eprintln!("  [zmq] Scan error: {e}");
                    }
                }
            }).await.ok();
        }
    });

    // Build router with /mainnet/ prefix (PivxNodeController compat)
    let mainnet_routes = Router::new()
        .route("/getshielddata", get(api::get_shield_data))
        .route("/getblockcount", get(api::get_block_count))
        .route("/getshieldblocks", get(api::get_shield_blocks))
        .route("/sendrawtransaction", post(api::send_raw_transaction));

    // Also serve without prefix for direct access
    let app = Router::new()
        .nest("/mainnet", mainnet_routes.clone())
        .merge(mainnet_routes)
        .with_state(state);

    let addr = format!("0.0.0.0:{}", config.port);
    eprintln!("\n  Listening on http://{addr}");
    eprintln!("  Routes: /mainnet/getshielddata, /mainnet/getblockcount, /mainnet/getshieldblocks");
    eprintln!("          /mainnet/sendrawtransaction");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind");

    axum::serve(listener, app).await.expect("server error");
}
