/// Block scanner — extracts Sapling transactions from the PIVX node.
///
/// Uses verbosity 1 (txid list) + getrawtransaction (raw hex) to avoid
/// potential issues with JSON serialization of Sapling txs.
use crate::rpc::RpcClient;

/// PIVX Sapling transaction header: version 3 (LE u16) + type 10 (LE u16).
const SAPLING_TX_HEADER: [u8; 4] = [0x03, 0x00, 0x0a, 0x00];

/// Size of each spend description in bytes.
const SPEND_DESC_SIZE: usize = 384;

/// Size of each output description in bytes.
const OUTPUT_DESC_SIZE: usize = 948;

/// Size of enc_ciphertext in bytes.
const ENC_CIPHERTEXT_SIZE: usize = 580;

/// Size of out_ciphertext in bytes.
const OUT_CIPHERTEXT_SIZE: usize = 80;

// ---------------------------------------------------------------------------
// Scanned block types
// ---------------------------------------------------------------------------

/// A scanned shield block.
#[derive(Debug, Clone)]
pub struct ShieldBlock {
    /// Block height.
    pub height: u32,
    /// Block timestamp.
    pub time: u32,
    /// Sapling transactions in this block.
    pub txs: Vec<ShieldTx>,
}

/// A Sapling transaction with extracted compact data.
#[derive(Debug, Clone)]
pub struct ShieldTx {
    /// Full raw transaction bytes (for 0x03 legacy compat).
    pub raw: Vec<u8>,
    /// Extracted compact data (for 0x04/0x05 formats).
    pub compact: Option<CompactTx>,
}

/// Compact transaction data — proofs and signatures stripped.
#[derive(Debug, Clone)]
pub struct CompactTx {
    /// Nullifiers from Sapling spends (32 bytes each).
    pub nullifiers: Vec<[u8; 32]>,
    /// Compact outputs.
    pub outputs: Vec<CompactOutput>,
}

/// A compact Sapling output.
#[derive(Debug, Clone)]
pub struct CompactOutput {
    /// Note commitment (32 bytes).
    pub cmu: [u8; 32],
    /// Ephemeral public key (32 bytes).
    pub epk: [u8; 32],
    /// Encrypted ciphertext (580 bytes).
    pub enc_ciphertext: [u8; ENC_CIPHERTEXT_SIZE],
    /// Outgoing ciphertext (80 bytes) — for sender recovery.
    pub out_ciphertext: [u8; OUT_CIPHERTEXT_SIZE],
}

// ---------------------------------------------------------------------------
// Block scanning
// ---------------------------------------------------------------------------

/// Scan a single block by hash for Sapling transactions.
fn scan_block_inner(
    rpc: &RpcClient,
    block_json: &serde_json::Value,
    height: u32,
) -> Result<Option<ShieldBlock>, ScanError> {
    let txids = block_json
        .get("tx")
        .and_then(|t| t.as_array())
        .ok_or(ScanError::Parse("missing 'tx' array in block".into()))?;

    let time = block_json
        .get("time")
        .and_then(|t| t.as_u64())
        .unwrap_or(0) as u32;

    let mut txs = Vec::new();

    for txid_val in txids {
        let txid = txid_val
            .as_str()
            .ok_or(ScanError::Parse("txid not a string".into()))?;

        // Fetch raw transaction hex
        let raw_hex = match rpc.get_raw_transaction(txid, false) {
            Ok(val) => match val.as_str() {
                Some(s) => s.to_string(),
                None => continue,
            },
            Err(e) => {
                eprintln!("  Warning: getrawtransaction({txid}): {e} — skipping");
                continue;
            }
        };

        let tx_bytes = match hex_decode(&raw_hex) {
            Some(b) => b,
            None => continue,
        };

        // Check for Sapling tx header (version 3, type 10)
        if tx_bytes.len() >= 4 && tx_bytes[..4] == SAPLING_TX_HEADER {
            let compact = parse_sapling_tx(&tx_bytes).ok().flatten();
            txs.push(ShieldTx { raw: tx_bytes, compact });
        }
    }

    if txs.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ShieldBlock { height, time, txs }))
    }
}

/// Scan a single block by height.
pub fn scan_block(rpc: &RpcClient, height: u32) -> Result<Option<ShieldBlock>, ScanError> {
    let hash = rpc
        .get_block_hash(height)
        .map_err(|e| ScanError::Rpc(format!("getblockhash({height}): {e}")))?;

    let block_json = rpc
        .get_block(&hash, 1)
        .map_err(|e| ScanError::Rpc(format!("getblock({hash}): {e}")))?;

    let h = block_json
        .get("height")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or(height);

    scan_block_inner(rpc, &block_json, h)
}

/// Scan a range of blocks by chaining via `nextblockhash`.
pub fn scan_range(
    rpc: &RpcClient,
    start: u32,
    end: u32,
    on_progress: impl Fn(u32, u32),
) -> Result<Vec<ShieldBlock>, ScanError> {
    let mut shield_blocks = Vec::new();
    let total = end.saturating_sub(start) + 1;
    let mut errors = 0u32;

    let mut current_hash = rpc
        .get_block_hash(start)
        .map_err(|e| ScanError::Rpc(format!("getblockhash({start}): {e}")))?;

    for height in start..=end {
        on_progress(height - start, total);

        let block_json = match rpc.get_block(&current_hash, 1) {
            Ok(json) => json,
            Err(e) => {
                eprintln!("\n  Warning: block {height} ({current_hash}): {e} — skipping");
                errors += 1;
                match rpc.get_block_hash(height + 1) {
                    Ok(hash) => {
                        current_hash = hash;
                        continue;
                    }
                    Err(_) => break,
                }
            }
        };

        match scan_block_inner(rpc, &block_json, height) {
            Ok(Some(block)) => {
                let tx_count = block.txs.len();
                shield_blocks.push(block);
                if tx_count > 0 {
                    eprintln!("\n  Found shield block {height} ({tx_count} Sapling tx{})",
                        if tx_count == 1 { "" } else { "s" });
                }
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("\n  Warning: block {height} parse error: {e} — skipping");
                errors += 1;
            }
        }

        match block_json.get("nextblockhash").and_then(|v| v.as_str()) {
            Some(next) => current_hash = next.to_string(),
            None => break,
        }
    }

    on_progress(total, total);

    if errors > 0 {
        eprintln!("\n  Completed with {errors} skipped block(s)");
    }

    Ok(shield_blocks)
}

// ---------------------------------------------------------------------------
// PIVX Sapling transaction parser
// ---------------------------------------------------------------------------

/// Parse a raw PIVX type 10 transaction and extract compact Sapling data.
fn parse_sapling_tx(data: &[u8]) -> Result<Option<CompactTx>, String> {
    let mut pos = 4; // skip header

    // Skip vin
    let (vin_count, br) = read_compact_size(data, pos)?;
    pos += br;
    for _ in 0..vin_count {
        pos += 32 + 4; // prevout
        let (script_len, br) = read_compact_size(data, pos)?;
        pos += br + script_len;
        pos += 4; // sequence
        if pos > data.len() { return Err("truncated vin".into()); }
    }

    // Skip vout
    let (vout_count, br) = read_compact_size(data, pos)?;
    pos += br;
    for _ in 0..vout_count {
        pos += 8; // value
        let (script_len, br) = read_compact_size(data, pos)?;
        pos += br + script_len;
        if pos > data.len() { return Err("truncated vout".into()); }
    }

    // Skip nLockTime
    pos += 4;

    // Read extra payload
    let (payload_len, br) = read_compact_size(data, pos)?;
    pos += br;

    if pos + payload_len > data.len() {
        return Err("truncated payload".into());
    }

    parse_sapling_payload(&data[pos..pos + payload_len])
}

/// Parse the Sapling extra payload to extract compact data.
fn parse_sapling_payload(data: &[u8]) -> Result<Option<CompactTx>, String> {
    let mut pos = 2; // skip payload nVersion (u16)

    // Spend descriptions
    let (num_spends, br) = read_compact_size(data, pos)?;
    pos += br;

    let mut nullifiers = Vec::new();
    for _ in 0..num_spends {
        if pos + SPEND_DESC_SIZE > data.len() { return Err("truncated spend".into()); }
        // cv(32) + anchor(32) + nullifier(32) + rk(32) + proof(192) + sig(64)
        let mut nf = [0u8; 32];
        nf.copy_from_slice(&data[pos + 64..pos + 96]);
        nullifiers.push(nf);
        pos += SPEND_DESC_SIZE;
    }

    // Output descriptions
    let (num_outputs, br) = read_compact_size(data, pos)?;
    pos += br;

    let mut outputs = Vec::new();
    for _ in 0..num_outputs {
        if pos + OUTPUT_DESC_SIZE > data.len() { return Err("truncated output".into()); }
        // cv(32) + cmu(32) + epk(32) + enc(580) + out(80) + proof(192)
        let mut cmu = [0u8; 32];
        cmu.copy_from_slice(&data[pos + 32..pos + 64]);

        let mut epk = [0u8; 32];
        epk.copy_from_slice(&data[pos + 64..pos + 96]);

        let mut enc_ciphertext = [0u8; ENC_CIPHERTEXT_SIZE];
        enc_ciphertext.copy_from_slice(&data[pos + 96..pos + 96 + ENC_CIPHERTEXT_SIZE]);

        let mut out_ciphertext = [0u8; OUT_CIPHERTEXT_SIZE];
        out_ciphertext.copy_from_slice(&data[pos + 96 + ENC_CIPHERTEXT_SIZE..pos + 96 + ENC_CIPHERTEXT_SIZE + OUT_CIPHERTEXT_SIZE]);

        outputs.push(CompactOutput { cmu, epk, enc_ciphertext, out_ciphertext });
        pos += OUTPUT_DESC_SIZE;
    }

    if nullifiers.is_empty() && outputs.is_empty() {
        return Ok(None);
    }

    Ok(Some(CompactTx { nullifiers, outputs }))
}

/// Read a Bitcoin compact size (varint).
fn read_compact_size(data: &[u8], pos: usize) -> Result<(usize, usize), String> {
    if pos >= data.len() { return Err("read past end".into()); }
    match data[pos] {
        n if n < 253 => Ok((n as usize, 1)),
        253 => {
            if pos + 3 > data.len() { return Err("truncated varint".into()); }
            Ok((u16::from_le_bytes([data[pos+1], data[pos+2]]) as usize, 3))
        }
        254 => {
            if pos + 5 > data.len() { return Err("truncated varint".into()); }
            Ok((u32::from_le_bytes([data[pos+1], data[pos+2], data[pos+3], data[pos+4]]) as usize, 5))
        }
        255 => {
            if pos + 9 > data.len() { return Err("truncated varint".into()); }
            Ok((u64::from_le_bytes([
                data[pos+1], data[pos+2], data[pos+3], data[pos+4],
                data[pos+5], data[pos+6], data[pos+7], data[pos+8],
            ]) as usize, 9))
        }
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// Hex utilities
// ---------------------------------------------------------------------------

fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) { return None; }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ScanError {
    Rpc(String),
    Parse(String),
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rpc(e) => write!(f, "scanner RPC error: {e}"),
            Self::Parse(e) => write!(f, "scanner parse error: {e}"),
        }
    }
}

impl std::error::Error for ScanError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_decode_valid() {
        assert_eq!(hex_decode("deadbeef"), Some(vec![0xDE, 0xAD, 0xBE, 0xEF]));
        assert_eq!(hex_decode("0300"), Some(vec![0x03, 0x00]));
        assert_eq!(hex_decode(""), Some(vec![]));
    }

    #[test]
    fn hex_decode_odd_length() {
        assert_eq!(hex_decode("abc"), None);
    }

    #[test]
    fn hex_decode_invalid_chars() {
        assert_eq!(hex_decode("gg"), None);
    }

    #[test]
    fn read_compact_size_single_byte() {
        let data = [42u8];
        let (val, consumed) = read_compact_size(&data, 0).unwrap();
        assert_eq!(val, 42);
        assert_eq!(consumed, 1);
    }

    #[test]
    fn read_compact_size_two_byte() {
        let data = [253u8, 0x00, 0x01]; // 256
        let (val, consumed) = read_compact_size(&data, 0).unwrap();
        assert_eq!(val, 256);
        assert_eq!(consumed, 3);
    }

    #[test]
    fn read_compact_size_four_byte() {
        let data = [254u8, 0x01, 0x00, 0x01, 0x00]; // 65537
        let (val, consumed) = read_compact_size(&data, 0).unwrap();
        assert_eq!(val, 65537);
        assert_eq!(consumed, 5);
    }

    #[test]
    fn read_compact_size_past_end() {
        let data = [];
        assert!(read_compact_size(&data, 0).is_err());
    }

    #[test]
    fn sapling_tx_header_correct() {
        // Version 3, type 10 in little-endian
        assert_eq!(SAPLING_TX_HEADER, [0x03, 0x00, 0x0a, 0x00]);
    }

    #[test]
    fn spend_output_sizes_correct() {
        // cv(32) + anchor(32) + nullifier(32) + rk(32) + proof(192) + sig(64)
        assert_eq!(SPEND_DESC_SIZE, 384);
        // cv(32) + cmu(32) + epk(32) + enc(580) + out(80) + proof(192)
        assert_eq!(OUTPUT_DESC_SIZE, 948);
    }

    #[test]
    fn parse_sapling_tx_no_shielded_data() {
        let mut tx = Vec::new();
        tx.extend_from_slice(&SAPLING_TX_HEADER);
        tx.push(0); // vin = 0
        tx.push(0); // vout = 0
        tx.extend([0u8; 4]); // nLockTime
        tx.push(4); // payload length = 4
        tx.extend([0x01, 0x00]); // payload nVersion
        tx.push(0); // num_spends = 0
        tx.push(0); // num_outputs = 0

        let result = parse_sapling_tx(&tx).unwrap();
        assert!(result.is_none());
    }
}
