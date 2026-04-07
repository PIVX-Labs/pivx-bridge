<p align="center">
  <strong>PIVX Bridge</strong><br>
  <em>High-performance shield sync server for the PIVX network.</em><br>
  <em>Drop-in replacement for PivxNodeController, written in Rust.</em>
</p>

---

A shield sync bridge that connects to a PIVX full node, scans for Sapling shielded transactions, and serves compact shield data to light wallets over HTTP. Fully compatible with [MyPIVXWallet](https://github.com/PIVX-Labs/MyPIVXWallet) — zero client changes required.

## Why?

PivxNodeController is written in Node.js. PIVX Bridge replaces it with a single Rust binary that:

- **Starts faster** — scans the full chain in a fraction of the time
- **Uses less memory** — no V8 heap, no garbage collector
- **Responds instantly** — serves from a pre-built binary cache on disk
- **Stays current** — ZMQ block notifications instead of 60-second polling
- **Saves bandwidth** — opt-in compact formats cut sync data by 24-42%

## Quick Start

```bash
# Build
cargo build --release

# Run (with a PIVX node on localhost)
./target/release/pivx-bridge \
  --rpc-url http://127.0.0.1:51473 \
  --rpc-user rpc \
  --rpc-pass rpc
```

Or use a `.env` file (copy `.env.sample`):

```bash
cp .env.sample .env
# Edit .env with your node credentials
./target/release/pivx-bridge
```

The bridge scans from Sapling activation (block 2,700,501) on first run, then indexes new blocks in real-time via ZMQ.

## PIVX Node Configuration

Add to `pivx.conf`:

```
server=1
rpcuser=rpc
rpcpassword=rpc
zmqpubhashblock=tcp://127.0.0.1:28332
```

## API

All endpoints are served at both `/mainnet/...` and `/...` (without prefix).

### Shield Sync

| Route | Method | Description |
|-------|--------|-------------|
| `/mainnet/getshielddata?startBlock=N` | GET | Binary shield stream |
| `/mainnet/getshielddatalength?startBlock=N&endBlock=M` | GET | Byte count (for progress bars) |
| `/mainnet/getshieldblocks` | GET | JSON array of shield block heights |
| `/mainnet/sendrawtransaction` | POST | Broadcast raw transaction hex |

### RPC Proxy

| Route | Method | Description |
|-------|--------|-------------|
| `/mainnet/:method?params=a,b,c&filter=<jq>` | GET | Proxied RPC call with optional jq filtering |

The proxy validates methods against a configurable whitelist. Parameters are automatically type-coerced (numbers, booleans, strings).

### Stream Formats

The `getshielddata` endpoint accepts an optional `format` query parameter:

| Format | Packet | Per output | Use case |
|--------|--------|-----------|----------|
| `pivx` (default) | 0x03 full raw tx | 948 bytes | MPW compatibility |
| `compact` | 0x04 | 724 bytes (-24%) | Wallets with sender recovery |
| `compactplus` | 0x05 | 644 bytes (-32%) | New wallet bootstrap |

The default PIVX format is byte-identical to PivxNodeController output.

## Binary Protocol

The shield stream is a sequence of length-prefixed packets:

```
PivxNodeController-compatible (default):

  Transaction:   [4-byte LE length][raw PIVX v3 tx bytes]
  Block footer:  [4-byte LE length=9][0x5d][height:4LE][time:4LE]

  Transactions come first, block footer comes last (per block).

Compact (opt-in):

  Block header:  [4-byte LE length=5][0x5d][height:4LE]
  Compact tx:    [4-byte LE length][0x04][nSpends:1][nOutputs:1]
                   per spend: nullifier(32)
                   per output: cmu(32) + epk(32) + enc(580) + out_ct(80)

CompactPlus (opt-in):

  Same as compact, but type=0x05 and out_ciphertext omitted per output.
```

## Architecture

```
src/
  main.rs       Startup, initial scan, ZMQ subscriber, axum server
  config.rs     CLI args + .env configuration
  rpc.rs        JSON-RPC 1.0 client with auth + connection pooling
  scanner.rs    Block scanner + PIVX type 10 Sapling tx parser
  stream.rs     Binary stream encoder (PivxCompat / Compact / CompactPlus)
  cache.rs      Persistent shield.bin binary cache
  index.rs      Shield block index with byte offsets (shield.json compat)
  api.rs        HTTP endpoints (axum handlers)
  proxy.rs      RPC proxy with jq filtering
```

## Configuration

All options support both CLI flags and environment variables:

| Flag | Env | Default | Description |
|------|-----|---------|-------------|
| `--rpc-url` | `RPC_URL` | `http://127.0.0.1:51473` | PIVX node RPC endpoint |
| `--rpc-user` | `RPC_USER` | `rpc` | RPC username |
| `--rpc-pass` | `RPC_PASS` | `rpc` | RPC password |
| `--zmq-url` | `ZMQ_URL` | `tcp://127.0.0.1:28332` | ZMQ hashblock endpoint |
| `--port` | `PORT` | `3000` | HTTP server port |
| `--sapling-height` | `SAPLING_HEIGHT` | `2700501` | Sapling activation block |
| `--allowed-rpcs` | `ALLOWED_RPCS` | (see below) | Comma-separated RPC whitelist |

Default allowed RPCs: `getblockcount`, `getblockhash`, `getblock`, `getrawtransaction`, `sendrawtransaction`, `getmasternodecount`, `listmasternodes`, `getbudgetprojection`, `getbudgetinfo`, `getbudgetvotes`

## Persistence

The bridge writes two files in the working directory:

- **`shield.bin`** — Pre-encoded binary shield stream (append-only, survives restarts)
- **`shield_index.json`** — Block height to byte offset index (PivxNodeController `shield.json` compatible)

## Testing

```bash
cargo test    # 36 tests
cargo clippy  # Zero warnings
```

## Migrating from PivxNodeController

1. Build the bridge: `cargo build --release`
2. Copy your `.env` credentials (same format)
3. Point your reverse proxy at the bridge instead of the Node.js server
4. MPW connects unchanged — the binary protocol and API are identical

The bridge will re-scan from Sapling activation on first run and build its own `shield.bin`.

## License

MIT
