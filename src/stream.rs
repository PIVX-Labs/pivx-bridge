/// Binary stream encoder — produces the shield sync wire format.
///
/// Supports three output formats:
/// - **PivxCompat** (default): 0x03 full raw tx + 0x5d block footer (9 bytes, with time).
///   Byte-identical to PivxNodeController output. MPW parses this unchanged.
/// - **Compact**: 0x04 packets with out_ciphertext (724 bytes/output) + 0x5d block header.
/// - **CompactPlus**: 0x05 packets without out_ciphertext (644 bytes/output) + 0x5d block header.
use crate::api::StreamFormat;
use crate::scanner::ShieldBlock;

// ---------------------------------------------------------------------------
// Wire format constants
// ---------------------------------------------------------------------------

/// Packet type: block marker.
const PACKET_TYPE_BLOCK: u8 = 0x5d;

/// Packet type: compact transaction (with out_ciphertext, 724 bytes/output).
const PACKET_TYPE_COMPACT_TX: u8 = 0x04;

/// Packet type: compact+ transaction (without out_ciphertext, 644 bytes/output).
const PACKET_TYPE_COMPACT_PLUS_TX: u8 = 0x05;

/// Size of enc_ciphertext.
const ENC_CT_SIZE: usize = 580;

/// Size of out_ciphertext.
const OUT_CT_SIZE: usize = 80;

/// Write a length as a Bitcoin CompactSize varint — the inverse of
/// `scanner::read_compact_size`. Values < 253 encode as a single byte, so a
/// stream of normal-sized transactions is byte-identical to the previous `u8`
/// encoding; only transactions with >= 253 spends or outputs differ (those
/// previously truncated the count to a single byte and silently corrupted the
/// stream, e.g. an 821-spend tx wrote count 53).
pub(crate) fn write_compact_size(buf: &mut Vec<u8>, n: usize) {
    if n < 253 {
        buf.push(n as u8);
    } else if n <= u16::MAX as usize {
        buf.push(253);
        buf.extend_from_slice(&(n as u16).to_le_bytes());
    } else if n <= u32::MAX as usize {
        buf.push(254);
        buf.extend_from_slice(&(n as u32).to_le_bytes());
    } else {
        buf.push(255);
        buf.extend_from_slice(&(n as u64).to_le_bytes());
    }
}

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

/// Encode a compact transaction (0x04) with cv + out_ciphertext for sender recovery.
fn encode_compact_tx(stream: &mut Vec<u8>, compact: &crate::scanner::CompactTx, _raw: &[u8]) {
    let payload_len = 1 + 2
        + compact.nullifiers.len() * 32
        + compact.outputs.len() * (32 + 32 + 32 + ENC_CT_SIZE + OUT_CT_SIZE);

    let mut payload = Vec::with_capacity(payload_len);
    payload.push(PACKET_TYPE_COMPACT_TX);
    write_compact_size(&mut payload, compact.nullifiers.len());
    write_compact_size(&mut payload, compact.outputs.len());

    for nf in &compact.nullifiers {
        payload.extend_from_slice(nf);
    }

    for out in &compact.outputs {
        payload.extend_from_slice(&out.cv);
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
    write_compact_size(&mut payload, compact.nullifiers.len());
    write_compact_size(&mut payload, compact.outputs.len());

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::{CompactOutput, CompactTx, ShieldBlock, ShieldTx};

    fn make_fake_tx(raw: Vec<u8>) -> ShieldTx {
        ShieldTx { raw, compact: None }
    }

    fn make_fake_tx_with_compact(raw: Vec<u8>, compact: CompactTx) -> ShieldTx {
        ShieldTx { raw, compact: Some(compact) }
    }

    fn make_block(height: u32, time: u32, txs: Vec<ShieldTx>) -> ShieldBlock {
        ShieldBlock { height, time, txs }
    }

    // -- PIVX-compat binary protocol tests (critical for MPW) --

    #[test]
    fn pivx_compat_no_type_prefix_on_raw_tx() {
        // The raw tx bytes should NOT have an extra 0x03 prefix
        let raw = vec![0x03, 0x00, 0x00, 0x00, 0xDE, 0xAD];
        let block = make_block(100, 1000, vec![make_fake_tx(raw.clone())]);
        let stream = encode_shield_stream(&[block], StreamFormat::PivxCompat);

        // First 4 bytes = length prefix = raw.len() (no +1 for type byte)
        let len = u32::from_le_bytes([stream[0], stream[1], stream[2], stream[3]]);
        assert_eq!(len as usize, raw.len(), "length prefix should equal raw tx length, no extra type byte");

        // Next bytes = raw tx (starting with natural 0x03 version)
        assert_eq!(&stream[4..4 + raw.len()], &raw[..]);
    }

    #[test]
    fn pivx_compat_footer_after_txs() {
        let raw = vec![0x03, 0x00, 0x00, 0x00];
        let block = make_block(2700501, 1700000000, vec![make_fake_tx(raw.clone())]);
        let stream = encode_shield_stream(&[block], StreamFormat::PivxCompat);

        // Skip the tx packet: 4 (length) + raw.len()
        let footer_start = 4 + raw.len();

        // Footer: length prefix = 9
        let footer_len = u32::from_le_bytes([
            stream[footer_start], stream[footer_start+1],
            stream[footer_start+2], stream[footer_start+3],
        ]);
        assert_eq!(footer_len, 9);

        // Footer type = 0x5d
        assert_eq!(stream[footer_start + 4], 0x5d);

        // Footer height
        let height = u32::from_le_bytes([
            stream[footer_start+5], stream[footer_start+6],
            stream[footer_start+7], stream[footer_start+8],
        ]);
        assert_eq!(height, 2700501);

        // Footer time
        let time = u32::from_le_bytes([
            stream[footer_start+9], stream[footer_start+10],
            stream[footer_start+11], stream[footer_start+12],
        ]);
        assert_eq!(time, 1700000000);
    }

    #[test]
    fn pivx_compat_multi_tx_then_footer() {
        let tx1 = vec![0x03, 0x00, 0x00, 0x00, 0x01];
        let tx2 = vec![0x03, 0x00, 0x00, 0x00, 0x02, 0x03];
        let block = make_block(500, 999, vec![
            make_fake_tx(tx1.clone()),
            make_fake_tx(tx2.clone()),
        ]);
        let stream = encode_shield_stream(&[block], StreamFormat::PivxCompat);

        let mut pos = 0;

        // Packet 1: tx1
        let len1 = u32::from_le_bytes(stream[pos..pos+4].try_into().unwrap()) as usize;
        assert_eq!(len1, tx1.len());
        assert_eq!(&stream[pos+4..pos+4+len1], &tx1[..]);
        pos += 4 + len1;

        // Packet 2: tx2
        let len2 = u32::from_le_bytes(stream[pos..pos+4].try_into().unwrap()) as usize;
        assert_eq!(len2, tx2.len());
        assert_eq!(&stream[pos+4..pos+4+len2], &tx2[..]);
        pos += 4 + len2;

        // Packet 3: footer
        let footer_len = u32::from_le_bytes(stream[pos..pos+4].try_into().unwrap());
        assert_eq!(footer_len, 9);
        assert_eq!(stream[pos+4], 0x5d);
    }

    #[test]
    fn pivx_compat_multi_block_ordering() {
        let block1 = make_block(100, 1000, vec![make_fake_tx(vec![0x03, 0x00, 0x00, 0x00])]);
        let block2 = make_block(200, 2000, vec![make_fake_tx(vec![0x03, 0x00, 0x00, 0x00])]);
        let stream = encode_shield_stream(&[block1, block2], StreamFormat::PivxCompat);

        // Find both footers
        let mut footers = Vec::new();
        let mut pos = 0;
        while pos < stream.len() {
            let len = u32::from_le_bytes(stream[pos..pos+4].try_into().unwrap()) as usize;
            if stream[pos+4] == 0x5d {
                let height = u32::from_le_bytes(stream[pos+5..pos+9].try_into().unwrap());
                let time = u32::from_le_bytes(stream[pos+9..pos+13].try_into().unwrap());
                footers.push((height, time));
            }
            pos += 4 + len;
        }

        assert_eq!(footers, vec![(100, 1000), (200, 2000)]);
    }

    #[test]
    fn pivx_compat_empty_block() {
        let block = make_block(100, 1000, vec![]);
        let stream = encode_shield_stream(&[block], StreamFormat::PivxCompat);
        // Just the footer: 4 (length prefix) + 9 (payload) = 13 bytes
        assert_eq!(stream.len(), 13);
        assert_eq!(stream[4], 0x5d);
    }

    // -- Compact format tests --

    #[test]
    fn compact_header_before_txs() {
        let compact = CompactTx {
            nullifiers: vec![[0xAA; 32]],
            outputs: vec![CompactOutput {
                cv: [0u8; 32],
                cmu: [1u8; 32],
                epk: [2u8; 32],
                enc_ciphertext: [3u8; 580],
                out_ciphertext: [4u8; 80],
            }],
        };
        let block = make_block(500, 999, vec![
            make_fake_tx_with_compact(vec![0x03, 0x00, 0x00, 0x00], compact),
        ]);
        let stream = encode_shield_stream(&[block], StreamFormat::Compact);

        // First packet: block header (5 bytes: type + height)
        let len = u32::from_le_bytes(stream[0..4].try_into().unwrap());
        assert_eq!(len, 5);
        assert_eq!(stream[4], 0x5d); // block marker
        let height = u32::from_le_bytes(stream[5..9].try_into().unwrap());
        assert_eq!(height, 500);

        // Second packet: compact tx (type 0x04)
        let pos = 9;
        let tx_len = u32::from_le_bytes(stream[pos..pos+4].try_into().unwrap()) as usize;
        assert_eq!(stream[pos+4], 0x04); // compact type
        assert_eq!(stream[pos+5], 1);    // 1 spend
        assert_eq!(stream[pos+6], 1);    // 1 output
        // Total: 1 (type) + 2 (counts) + 32 (nullifier) + 32+32+32+580+80 (output) = 791
        assert_eq!(tx_len, 791);
    }

    #[test]
    fn compact_plus_no_out_ciphertext() {
        let compact = CompactTx {
            nullifiers: vec![],
            outputs: vec![CompactOutput {
                cv: [0u8; 32],
                cmu: [1u8; 32],
                epk: [2u8; 32],
                enc_ciphertext: [3u8; 580],
                out_ciphertext: [4u8; 80],
            }],
        };
        let block = make_block(500, 999, vec![
            make_fake_tx_with_compact(vec![0x03, 0x00, 0x00, 0x00], compact),
        ]);

        let compact_stream = encode_shield_stream(&[block.clone()], StreamFormat::Compact);
        let plus_stream = encode_shield_stream(&[block], StreamFormat::CompactPlus);

        // CompactPlus should be smaller (no cv or out_ciphertext: -112 bytes per output)
        assert!(plus_stream.len() < compact_stream.len());
        let diff = compact_stream.len() - plus_stream.len();
        assert_eq!(diff, 112); // cv(32) + out_ciphertext(80) stripped

        // CompactPlus type byte should be 0x05
        // Skip header (9 bytes), then length prefix (4), then type byte
        assert_eq!(plus_stream[9 + 4], 0x05);
    }

    #[test]
    fn compact_output_size_math() {
        // Verify the documented size savings
        let full_output = 32 + 32 + 32 + 580 + 80 + 192; // 948
        let compact_output = 32 + 32 + 32 + 580 + 80; // 756 (cv + cmu + epk + enc + out_ct)
        let compact_plus_output = 32 + 32 + 580; // 644 (cmu + epk + enc only)
        assert_eq!(full_output, 948);
        assert_eq!(compact_output, 756);
        assert_eq!(compact_plus_output, 644);
    }

    #[test]
    fn write_compact_size_matches_reader() {
        // Round-trip against the canonical decoder for the boundary values.
        for n in [0usize, 1, 252, 253, 1000, 65535, 65536, 821] {
            let mut buf = Vec::new();
            write_compact_size(&mut buf, n);
            let (decoded, _) = crate::scanner::read_compact_size(&buf, 0).unwrap();
            assert_eq!(decoded, n, "compact size round-trip for {n}");
        }
        // <253 must stay a single byte (back-compat with the old u8 encoding).
        let mut one = Vec::new();
        write_compact_size(&mut one, 200);
        assert_eq!(one, vec![200u8]);
    }

    #[test]
    fn compact_tx_encodes_large_counts_as_varint() {
        // A tx with >255 spends must NOT truncate the count (the 821-spend bug):
        // the count is a CompactSize and decodes back to the true value.
        let compact = CompactTx {
            nullifiers: vec![[7u8; 32]; 300],
            outputs: vec![],
        };
        let mut stream = Vec::new();
        encode_compact_tx(&mut stream, &compact, &[]);

        // [len:4][type:1][nSpends:CompactSize][nOutputs:CompactSize][nullifiers..]
        let len = u32::from_le_bytes([stream[0], stream[1], stream[2], stream[3]]) as usize;
        assert_eq!(stream.len(), 4 + len, "length prefix must be exact");
        assert_eq!(stream[4], PACKET_TYPE_COMPACT_TX);
        let (n_spends, c1) = crate::scanner::read_compact_size(&stream, 5).unwrap();
        let (n_outputs, _) = crate::scanner::read_compact_size(&stream, 5 + c1).unwrap();
        assert_eq!(n_spends, 300, "300 spends must round-trip (not 300 % 256 = 44)");
        assert_eq!(n_outputs, 0);
        assert_eq!(len, 1 + 3 + 1 + 300 * 32, "type + CompactSize(300)=3 + CompactSize(0)=1 + 300 nullifiers");
    }
}
