/// Binary stream encoder — produces the shield sync wire format.
///
/// Supports three output formats:
/// - **PivxCompat** (default): 0x03 full raw tx + 0x5d block footer (9 bytes, with time).
///   Byte-identical to PivxNodeController output. MPW parses this unchanged.
/// - **Compact**: 0x04 packets with out_ciphertext (724 bytes/output) + 0x5d block header.
/// - **CompactPlus**: 0x05 packets without out_ciphertext (644 bytes/output) + 0x5d block header.
use crate::api::StreamFormat;
use crate::scanner::{CompactOutput, ShieldBlock};

// ---------------------------------------------------------------------------
// Wire format constants
// ---------------------------------------------------------------------------

/// Packet type: block marker.
const PACKET_TYPE_BLOCK: u8 = 0x5d;

/// Packet type: full raw transaction (PIVX-compatible).
const PACKET_TYPE_FULL_TX: u8 = 0x03;

/// Packet type: compact transaction (with out_ciphertext, 724 bytes/output).
const PACKET_TYPE_COMPACT_TX: u8 = 0x04;

/// Packet type: compact+ transaction (without out_ciphertext, 644 bytes/output).
const PACKET_TYPE_COMPACT_PLUS_TX: u8 = 0x05;

/// Size of enc_ciphertext.
const ENC_CT_SIZE: usize = 580;

/// Size of out_ciphertext.
const OUT_CT_SIZE: usize = 80;

// ---------------------------------------------------------------------------
// Stream encoder
// ---------------------------------------------------------------------------

/// Encode a slice of shield blocks into a binary stream.
pub fn encode_shield_stream(blocks: &[ShieldBlock], format: StreamFormat) -> Vec<u8> {
    let mut stream = Vec::new();

    for block in blocks {
        match format {
            StreamFormat::PivxCompat => encode_pivx_compat(&mut stream, block),
            StreamFormat::Compact => encode_compact(&mut stream, block),
            StreamFormat::CompactPlus => encode_compact_plus(&mut stream, block),
        }
    }

    stream
}

/// PIVX-compatible encoding: raw tx bytes (naturally start with 0x03), then 0x5d block footer.
///
/// PivxNodeController writes raw tx bytes directly — the version byte (0x03) IS the
/// implicit type marker. There is NO separate type prefix byte.
fn encode_pivx_compat(stream: &mut Vec<u8>, block: &ShieldBlock) {
    // Transactions first (raw bytes, no type prefix — 0x03 is the tx version byte)
    for tx in &block.txs {
        stream.extend((tx.raw.len() as u32).to_le_bytes());
        stream.extend_from_slice(&tx.raw);
    }

    // Block footer AFTER txs: [0x5d][height:4LE][time:4LE]
    stream.extend(9u32.to_le_bytes()); // length prefix = 9
    stream.push(PACKET_TYPE_BLOCK);
    stream.extend(block.height.to_le_bytes());
    stream.extend(block.time.to_le_bytes());
}

/// Compact encoding (0x04): block header, then compact tx packets with out_ciphertext.
fn encode_compact(stream: &mut Vec<u8>, block: &ShieldBlock) {
    // Block header BEFORE txs (Kerrigan-style, 5 bytes)
    encode_block_header(stream, block.height);

    for tx in &block.txs {
        if let Some(compact) = &tx.compact {
            encode_compact_tx(stream, compact, &tx.raw);
        }
    }
}

/// CompactPlus encoding (0x05): block header, then ultra-compact tx packets.
fn encode_compact_plus(stream: &mut Vec<u8>, block: &ShieldBlock) {
    encode_block_header(stream, block.height);

    for tx in &block.txs {
        if let Some(compact) = &tx.compact {
            encode_compact_plus_tx(stream, compact);
        }
    }
}

// ---------------------------------------------------------------------------
// Packet encoders
// ---------------------------------------------------------------------------

/// Encode a block header (Kerrigan-style, 5 bytes: type + height).
fn encode_block_header(stream: &mut Vec<u8>, height: u32) {
    stream.extend(5u32.to_le_bytes());
    stream.push(PACKET_TYPE_BLOCK);
    stream.extend(height.to_le_bytes());
}

/// Encode a compact transaction (0x04) with out_ciphertext.
fn encode_compact_tx(stream: &mut Vec<u8>, compact: &crate::scanner::CompactTx, _raw: &[u8]) {
    let payload_len = 1 + 2
        + compact.nullifiers.len() * 32
        + compact.outputs.len() * (32 + 32 + ENC_CT_SIZE + OUT_CT_SIZE);

    let mut payload = Vec::with_capacity(payload_len);
    payload.push(PACKET_TYPE_COMPACT_TX);
    payload.push(compact.nullifiers.len() as u8);
    payload.push(compact.outputs.len() as u8);

    for nf in &compact.nullifiers {
        payload.extend_from_slice(nf);
    }

    for out in &compact.outputs {
        payload.extend_from_slice(&out.cmu);
        payload.extend_from_slice(&out.epk);
        payload.extend_from_slice(&out.enc_ciphertext);
        payload.extend_from_slice(&out.out_ciphertext);
    }

    stream.extend((payload.len() as u32).to_le_bytes());
    stream.extend_from_slice(&payload);
}

/// Encode a compact+ transaction (0x05) without out_ciphertext.
fn encode_compact_plus_tx(stream: &mut Vec<u8>, compact: &crate::scanner::CompactTx) {
    let payload_len = 1 + 2
        + compact.nullifiers.len() * 32
        + compact.outputs.len() * (32 + 32 + ENC_CT_SIZE);

    let mut payload = Vec::with_capacity(payload_len);
    payload.push(PACKET_TYPE_COMPACT_PLUS_TX);
    payload.push(compact.nullifiers.len() as u8);
    payload.push(compact.outputs.len() as u8);

    for nf in &compact.nullifiers {
        payload.extend_from_slice(nf);
    }

    for out in &compact.outputs {
        payload.extend_from_slice(&out.cmu);
        payload.extend_from_slice(&out.epk);
        payload.extend_from_slice(&out.enc_ciphertext);
        // out_ciphertext intentionally omitted
    }

    stream.extend((payload.len() as u32).to_le_bytes());
    stream.extend_from_slice(&payload);
}

/// Compute the byte size of a PIVX-compat encoded stream for given blocks.
///
/// Used by the `getshielddatalength` endpoint without materializing the stream.
pub fn pivx_compat_stream_size(blocks: &[ShieldBlock]) -> u64 {
    let mut size: u64 = 0;
    for block in blocks {
        for tx in &block.txs {
            // Length prefix (4) + raw tx bytes (no type prefix)
            size += 4 + tx.raw.len() as u64;
        }
        // Block footer: length prefix (4) + 9-byte payload
        size += 4 + 9;
    }
    size
}
