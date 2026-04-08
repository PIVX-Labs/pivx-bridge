/// PIVX Bridge — drop-in replacement for PivxNodeController.
///
/// Connects to a PIVX full node, scans for Sapling transactions, and serves
/// shield data to light wallets (MyPIVXWallet) over HTTP.
mod api;
mod cache;
mod config;
mod index;
mod proxy;
mod rpc;
mod scanner;
mod stream;

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use clap::Parser;
use tokio::sync::{Mutex, RwLock};
use tower_http::compression::{CompressionLayer, CompressionLevel};
use tower_http::cors::CorsLayer;

/// Network initialization parameters.
struct NetworkConfig<'a> {
    label: &'a str,
    rpc_url: &'a str,
    rpc_user: &'a str,
    rpc_pass: &'a str,
    index_path: &'a str,
    cache_path: &'a str,
    sapling_height: u32,
    allowed_rpcs: Vec<String>,
}

/// Initialize a network: connect to node, scan blocks, build state.
fn init_network(cfg: &NetworkConfig) -> Option<Arc<api::AppState>> {
    let label = cfg.label;
    eprintln!("\n  [{label}] Connecting to {}...", cfg.rpc_url);

    let rpc = rpc::RpcClient::new(cfg.rpc_url, cfg.rpc_user, cfg.rpc_pass);

    let chain_height = match rpc.get_block_count() {
        Ok(h) => {
            eprintln!("  [{label}] Connected — chain height: {h}");
            h as u32
        }
        Err(e) => {
            eprintln!("  [{label}] Cannot connect: {e}");
            return None;
        }
    };

    let mut shield_index = index::load_or_create(cfg.index_path);

    if let Some(last_good) = cache::recover_cache(cfg.cache_path) {
        eprintln!("  [{label}] Cache recovered — last complete block: {last_good}");
    }

    let mut cache_file = cache::open_cache(cfg.cache_path)
        .expect("failed to open cache file");

    let scan_from = shield_index
        .last_height()
        .map(|h| h + 1)
        .unwrap_or(cfg.sapling_height);

    if scan_from <= chain_height {
        eprintln!("  [{label}] Scanning blocks {scan_from}..{chain_height}...");

        match scanner::scan_range(&rpc, scan_from, chain_height, |done, total| {
            if total > 0 && (done % 100 == 0 || done == total) {
                eprint!("\r  [{label}] Progress: {done}/{total} blocks");
            }
        }) {
            Ok(blocks) => {
                let count = blocks.len();
                match cache::append_blocks(&mut cache_file, &blocks) {
                    Ok(entries) => {
                        for (height, offset, _len) in entries {
                            shield_index.add(height, offset);
                        }
                    }
                    Err(e) => {
                        eprintln!("\n  [{label}] Warning: cache write failed: {e}");
                        for block in &blocks {
                            shield_index.add(block.height, 0);
                        }
                    }
                }
                if let Err(e) = shield_index.save(cfg.index_path) {
                    eprintln!("\n  [{label}] Warning: index save failed: {e}");
                }
                eprintln!("\n  [{label}] Found {count} shield blocks ({} total)",
                    shield_index.shield_heights.len());
            }
            Err(e) => {
                eprintln!("\n  [{label}] Scan error: {e}");
            }
        }
    } else {
        eprintln!("  [{label}] Up to date — {} shield blocks",
            shield_index.shield_heights.len());
    }

    // Load entire shield.bin into memory for zero-disk-I/O serving
    let shield_buffer = std::fs::read(cfg.cache_path).unwrap_or_default();
    let buffer_mb = shield_buffer.len() as f64 / (1024.0 * 1024.0);
    eprintln!("  [{label}] Shield buffer loaded: {buffer_mb:.1} MB in memory");

    Some(Arc::new(api::AppState {
        rpc: rpc::RpcClient::new(cfg.rpc_url, cfg.rpc_user, cfg.rpc_pass),
        index: RwLock::new(shield_index),
        cache_file: Mutex::new(cache_file),
        shield_buffer: RwLock::new(shield_buffer),
        allowed_rpcs: cfg.allowed_rpcs.clone(),
    }))
}

/// Build the standard route set for a network.
fn build_routes(state: Arc<api::AppState>) -> Router {
    Router::new()
        .route("/getshielddata", get(api::get_shield_data))
        .route("/getshielddatalength", get(api::get_shield_data_length))
        .route("/getblockcount", get(api::get_block_count))
        .route("/getshieldblocks", get(api::get_shield_blocks))
        .route("/sendrawtransaction", post(api::send_raw_transaction))
        .route("/address_index", get(api::get_address_index))
        .route("/{method}", get(proxy::rpc_proxy))
        .with_state(state)
}

/// Scan for new blocks and index them. Shared by both ZMQ and polling.
fn index_new_blocks(
    label: &'static str,
    rpc_url: &str,
    rpc_user: &str,
    rpc_pass: &str,
    state: &Arc<api::AppState>,
    index_path: &str,
    source: &str,
) {
    let rpc = rpc::RpcClient::new(rpc_url, rpc_user, rpc_pass);

    let chain_height = match rpc.get_block_count() {
        Ok(h) => h as u32,
        Err(e) => {
            eprintln!("  [{label}] {source} RPC error: {e}");
            return;
        }
    };

    let last_height = {
        let rt = tokio::runtime::Handle::current();
        rt.block_on(async { state.index.read().await.last_height() })
    };

    let scan_from = last_height.map(|h| h + 1).unwrap_or(0);
    if scan_from > chain_height {
        return;
    }

    match scanner::scan_range(&rpc, scan_from, chain_height, |_, _| {}) {
        Ok(blocks) => {
            let count = blocks.len();
            let new_bytes = stream::encode_shield_stream(
                &blocks, api::StreamFormat::PivxCompat,
            );

            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                let cache_entries = {
                    let mut file = state.cache_file.lock().await;
                    cache::append_blocks(&mut file, &blocks).ok()
                };

                {
                    let mut buffer = state.shield_buffer.write().await;
                    buffer.extend_from_slice(&new_bytes);
                }

                let mut index = state.index.write().await;
                if let Some(entries) = cache_entries {
                    for (height, offset, _len) in entries {
                        index.add(height, offset);
                    }
                } else {
                    for block in &blocks {
                        index.add(block.height, 0);
                    }
                }
                if let Err(e) = index.save(index_path) {
                    eprintln!("  [{label}] Index save failed: {e}");
                }
            });
            if count > 0 {
                eprintln!("  [{label}] {source}: indexed {count} new shield block(s) (chain: {chain_height})");
            }
        }
        Err(e) => {
            eprintln!("  [{label}] {source} scan error: {e}");
        }
    }
}

/// Spawn block indexer: tries ZMQ for instant notifications, falls back to 10s polling.
fn spawn_block_subscriber(
    label: &'static str,
    zmq_url: String,
    rpc_url: String,
    rpc_user: String,
    rpc_pass: String,
    state: Arc<api::AppState>,
    index_path: String,
) {
    tokio::spawn(async move {
        use futures::StreamExt;

        loop {
            // Try ZMQ first
            eprintln!("  [{label}] ZMQ subscribing to {zmq_url}...");
            let zmq_result = bitcoincore_zmq::subscribe_async(&[&zmq_url]);

            match zmq_result {
                Ok(mut subscriber) => {
                    eprintln!("  [{label}] ZMQ connected — waiting for blocks...");

                    // Test if ZMQ actually delivers: wait up to 90s for first message
                    let first_msg = tokio::time::timeout(
                        std::time::Duration::from_secs(90),
                        subscriber.next(),
                    ).await;

                    match first_msg {
                        Ok(Some(Ok(msg))) => {
                            eprintln!("  [{label}] ZMQ confirmed working");
                            // Process the first message
                            if matches!(msg, bitcoincore_zmq::Message::HashBlock(_, _)) {
                                let s = state.clone();
                                let r = rpc_url.clone();
                                let u = rpc_user.clone();
                                let p = rpc_pass.clone();
                                let i = index_path.clone();
                                tokio::task::spawn_blocking(move || {
                                    index_new_blocks(label, &r, &u, &p, &s, &i, "ZMQ");
                                }).await.ok();
                            }

                            // ZMQ works — use it for subsequent blocks
                            loop {
                                match subscriber.next().await {
                                    Some(Ok(msg)) => {
                                        if !matches!(msg, bitcoincore_zmq::Message::HashBlock(_, _)) {
                                            continue;
                                        }
                                        let s = state.clone();
                                        let r = rpc_url.clone();
                                        let u = rpc_user.clone();
                                        let p = rpc_pass.clone();
                                        let i = index_path.clone();
                                        tokio::task::spawn_blocking(move || {
                                            index_new_blocks(label, &r, &u, &p, &s, &i, "ZMQ");
                                        }).await.ok();
                                    }
                                    Some(Err(e)) => {
                                        eprintln!("  [{label}] ZMQ error: {e} — restarting");
                                        break;
                                    }
                                    None => {
                                        eprintln!("  [{label}] ZMQ stream ended — restarting");
                                        break;
                                    }
                                }
                            }
                            // ZMQ broke mid-session, restart from the top
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            continue;
                        }
                        _ => {
                            eprintln!("  [{label}] ZMQ silent after 90s — falling back to polling");
                        }
                    }
                }
                Err(e) => {
                    eprintln!("  [{label}] ZMQ failed: {e} — falling back to polling");
                }
            }

            // Polling fallback: 10s interval until ZMQ is retried
            eprintln!("  [{label}] Polling active (10s interval)");
            for _ in 0..60 {
                // Poll for ~10 minutes, then retry ZMQ
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;

                let s = state.clone();
                let r = rpc_url.clone();
                let u = rpc_user.clone();
                let p = rpc_pass.clone();
                let i = index_path.clone();
                tokio::task::spawn_blocking(move || {
                    index_new_blocks(label, &r, &u, &p, &s, &i, "Poll");
                }).await.ok();
            }
            eprintln!("  [{label}] Retrying ZMQ...");
        }
    });
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    let config = config::Config::parse();

    eprintln!("╔══════════════════════════════════════════╗");
    eprintln!("║          PIVX Bridge v{}           ║", env!("CARGO_PKG_VERSION"));
    eprintln!("╠══════════════════════════════════════════╣");
    eprintln!("║  RPC:  {}  ", config.rpc_url);
    eprintln!("║  ZMQ:  {}  ", config.zmq_url);
    eprintln!("║  Port: {}                            ", config.port);
    eprintln!("╚══════════════════════════════════════════╝");

    let allowed_rpcs = config.allowed_rpc_set();

    // -- Mainnet --
    let mainnet_state = init_network(&NetworkConfig {
        label: "mainnet",
        rpc_url: &config.rpc_url,
        rpc_user: &config.rpc_user,
        rpc_pass: &config.rpc_pass,
        index_path: "shield_index.json",
        cache_path: "shield.bin",
        sapling_height: config.sapling_height,
        allowed_rpcs: allowed_rpcs.clone(),
    }).expect("mainnet initialization failed");

    let mainnet_index_path = "shield_index.json".to_string();
    spawn_block_subscriber(
        "mainnet",
        config.zmq_url.clone(),
        config.rpc_url.clone(), config.rpc_user.clone(), config.rpc_pass.clone(),
        mainnet_state.clone(),
        mainnet_index_path,
    );

    let mainnet_routes = build_routes(mainnet_state);

    // -- Router --
    let mut app = Router::new()
        .nest("/mainnet", mainnet_routes.clone())
        .merge(mainnet_routes);

    // -- Testnet (optional) --
    if let Some(testnet_url) = &config.testnet_rpc_url {
        let testnet_user = config.testnet_rpc_user.as_deref().unwrap_or(&config.rpc_user);
        let testnet_pass = config.testnet_rpc_pass.as_deref().unwrap_or(&config.rpc_pass);

        if let Some(testnet_state) = init_network(&NetworkConfig {
            label: "testnet",
            rpc_url: testnet_url,
            rpc_user: testnet_user,
            rpc_pass: testnet_pass,
            index_path: "shield_index.testnet.json",
            cache_path: "shield.testnet.bin",
            sapling_height: config.sapling_height,
            allowed_rpcs,
        }) {
            let testnet_index_path = "shield_index.testnet.json".to_string();
            spawn_block_subscriber(
                "testnet",
                config.zmq_url.clone(),
                testnet_url.clone(), testnet_user.to_string(), testnet_pass.to_string(),
                testnet_state.clone(),
                testnet_index_path,
            );

            let testnet_routes = build_routes(testnet_state);
            app = app.nest("/testnet", testnet_routes);
            eprintln!("  Testnet: enabled at /testnet/");
        }
    }

    // -- Layers --
    let app = app.layer(CorsLayer::permissive());

    let app = if config.no_compression {
        eprintln!("  Compression: OFF");
        app.into_make_service()
    } else {
        eprintln!("  Compression: ON (gzip, best quality — disable with --no-compression)");
        app.layer(CompressionLayer::new().quality(CompressionLevel::Best))
            .into_make_service()
    };

    let addr = format!("0.0.0.0:{}", config.port);
    eprintln!("\n  Listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind");

    axum::serve(listener, app).await.expect("server error");
}
