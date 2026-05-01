/// Persistent binary cache — shield.bin management.
///
/// The scan loop writes pre-encoded PIVX-compat binary data to disk.
/// HTTP requests serve byte ranges from the file instead of re-fetching
/// from the node on every request.
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use crate::scanner::ShieldBlock;
use crate::stream;
use crate::api::StreamFormat;

/// Append multiple shield blocks to the cache file with a single flush.
///
/// Returns a vec of (height, byte_offset, byte_length) for each block.
///
/// On a successful return the bytes have been written AND `sync_data`
/// has been called — the kernel has been instructed to flush them to
/// the storage layer. Without sync, a power loss between
/// `cache::append_blocks` and the index save could leave the index
/// referencing offsets that the file system never durably wrote.
pub fn append_blocks(file: &mut File, blocks: &[ShieldBlock]) -> std::io::Result<Vec<(u32, u64, u64)>> {
    let mut current_offset = file.seek(SeekFrom::End(0))?;
    let mut entries = Vec::with_capacity(blocks.len());

    for block in blocks {
        let encoded = stream::encode_shield_stream(std::slice::from_ref(block), StreamFormat::PivxCompat);
        file.write_all(&encoded)?;
        entries.push((block.height, current_offset, encoded.len() as u64));
        current_offset += encoded.len() as u64;
    }

    // sync_data is the cheap variant of fsync — flushes the file's
    // contents but not all metadata. For our case (append + truncate)
    // that's exactly what we need; the file size *is* metadata and
    // sync_data covers it on Linux.
    file.sync_data()?;
    Ok(entries)
}

/// Truncate the cache file to a specific byte offset. Used by the
/// reorg handler to lop off shield blocks that no longer exist on
/// the canonical chain. After this call the file's logical EOF is
/// at `offset`; any later `append_blocks` will write starting there.
///
/// **Critical**: must be called BEFORE the index is trimmed, so the
/// caller can compute `offset` from the about-to-be-removed first
/// orphan entry. After both the file truncate and index trim, the
/// in-memory `shield_buffer` mirror MUST also be truncated to the
/// same offset under the same lock — otherwise readers see torn
/// state where the index points one place and the buffer holds
/// stale (orphan) bytes.
pub fn truncate_to(file: &mut File, offset: u64) -> std::io::Result<()> {
    file.set_len(offset)?;
    // Reset the file cursor — set_len doesn't move it, and the next
    // append's `seek(End(0))` would be correct without this, but
    // being explicit avoids surprises if seek-position is ever read
    // before the next append.
    file.seek(SeekFrom::Start(offset))?;
    file.sync_data()?;
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::{ShieldBlock, ShieldTx};

    fn make_block(height: u32, time: u32) -> ShieldBlock {
        ShieldBlock {
            height,
            time,
            txs: vec![ShieldTx {
                raw: vec![0x03, 0x00, 0x0a, 0x00, 0xDE, 0xAD],
                compact: None,
            }],
        }
    }

    #[test]
    fn find_last_footer_in_valid_stream() {
        let block = make_block(100, 1000);
        let stream = crate::stream::encode_shield_stream(
            &[block], crate::api::StreamFormat::PivxCompat,
        );
        let end = find_last_footer(&stream).unwrap();
        assert_eq!(end, stream.len());
    }

    #[test]
    fn find_last_footer_truncated_stream() {
        let block = make_block(100, 1000);
        let mut stream = crate::stream::encode_shield_stream(
            &[block], crate::api::StreamFormat::PivxCompat,
        );
        let good_len = stream.len();

        // Append garbage (simulates crash mid-write of next block)
        stream.extend_from_slice(&[0x06, 0x00, 0x00, 0x00, 0x03, 0x00]);

        let end = find_last_footer(&stream).unwrap();
        assert_eq!(end, good_len, "should find footer before the garbage");
    }

    #[test]
    fn find_last_footer_multi_block() {
        let blocks = vec![make_block(100, 1000), make_block(200, 2000)];
        let stream = crate::stream::encode_shield_stream(
            &blocks, crate::api::StreamFormat::PivxCompat,
        );
        let end = find_last_footer(&stream).unwrap();
        assert_eq!(end, stream.len());
    }

    #[test]
    fn find_last_footer_empty() {
        assert!(find_last_footer(&[]).is_none());
    }

    #[test]
    fn recover_cache_truncates_garbage() {
        let path = "/tmp/pivx_bridge_test_recover.bin";
        let block = make_block(500, 9999);
        let stream = crate::stream::encode_shield_stream(
            &[block], crate::api::StreamFormat::PivxCompat,
        );
        let good_len = stream.len();

        // Write good data + garbage
        let mut data = stream;
        data.extend_from_slice(&[0xFF; 50]);
        fs::write(path, &data).unwrap();

        let height = recover_cache(path).unwrap();
        assert_eq!(height, 500);
        assert_eq!(fs::metadata(path).unwrap().len(), good_len as u64);

        fs::remove_file(path).ok();
    }

    #[test]
    fn append_and_read_roundtrip() {
        let path = "/tmp/pivx_bridge_test_append.bin";
        let _ = fs::remove_file(path);

        let mut file = open_cache(path).unwrap();
        let block = make_block(100, 1000);
        let entries = append_blocks(&mut file, &[block]).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, 100); // height
        assert_eq!(entries[0].1, 0);   // offset
        assert!(entries[0].2 > 0);     // length

        // Verify by reading the file back directly
        let data = fs::read(path).unwrap();
        assert_eq!(data.len(), entries[0].2 as usize);

        fs::remove_file(path).ok();
    }

    /// Regression for the prod incident on bridge.pivxla.bz: 132 reorgs
    /// over 9 days corrupted shield.bin because the reorg handler trimmed
    /// the index but left orphan bytes in the file. Every subsequent
    /// streaming request for a height after the fork point read the
    /// orphan bytes too, producing duplicate-block / monotonic-order
    /// errors at clients (MPW, agent-kit).
    ///
    /// This test exercises the fix end-to-end: append → truncate →
    /// re-append, and asserts that the final file size matches exactly
    /// what the index reports — i.e. no orphan tail.
    #[test]
    fn reorg_truncates_cache_no_orphan_bytes() {
        use crate::index::ShieldIndex;
        let path = "/tmp/pivx_bridge_test_reorg.bin";
        let _ = fs::remove_file(path);

        let mut file = open_cache(path).unwrap();
        let mut index = ShieldIndex::new();

        // Phase 1: append 3 blocks on the original chain
        let blocks_a = vec![
            make_block(100, 1000),
            make_block(200, 2000),
            make_block(300, 3000),
        ];
        let entries_a = append_blocks(&mut file, &blocks_a).unwrap();
        for (h, off, _len) in &entries_a {
            index.add(*h, *off);
        }
        let pre_reorg_size = fs::metadata(path).unwrap().len();
        assert_eq!(pre_reorg_size, entries_a.iter().map(|e| e.2).sum::<u64>());

        // Phase 2: simulate a reorg at height 250. Fork point = 250
        // means heights >= 250 are orphaned. Index says block 300 lives
        // at offset_for_height(300). Truncate the file there and trim
        // the index to match.
        let fork_height = 250u32;
        let truncate_offset = index.offset_for_height(fork_height).unwrap();
        truncate_to(&mut file, truncate_offset).unwrap();
        index.remove_from(fork_height);

        // Cache + index now agree: only blocks 100 and 200 remain.
        assert_eq!(fs::metadata(path).unwrap().len(), truncate_offset);
        assert_eq!(index.shield_heights, vec![100, 200]);

        // Phase 3: replay forward on the new chain. New block at
        // height 300 (different time → different bytes) must land at
        // the truncated offset, NOT after the orphaned tail.
        let blocks_b = vec![make_block(300, 9999)];
        let entries_b = append_blocks(&mut file, &blocks_b).unwrap();
        assert_eq!(entries_b.len(), 1);
        assert_eq!(entries_b[0].0, 300);
        assert_eq!(
            entries_b[0].1, truncate_offset,
            "new block must start at truncate point, not orphan tail",
        );
        index.add(entries_b[0].0, entries_b[0].1);

        // Final invariant: file size = sum of entry lengths in the
        // index. The pre-fix code had file_size > sum_of_entry_lengths
        // because the orphan bytes between truncate_offset and the
        // pre-reorg EOF stayed on disk. If that ever regresses, this
        // assertion blows up.
        let final_size = fs::metadata(path).unwrap().len();
        let total_block_a_kept: u64 = entries_a.iter()
            .filter(|e| e.0 < fork_height)
            .map(|e| e.2)
            .sum();
        let total_block_b: u64 = entries_b.iter().map(|e| e.2).sum();
        assert_eq!(
            final_size,
            total_block_a_kept + total_block_b,
            "shield.bin contains orphan bytes — reorg handler regressed",
        );

        // And the new block 300's bytes are the new chain's bytes,
        // not the original — read them back and check the height
        // field in the footer.
        let data = fs::read(path).unwrap();
        let last = find_last_footer(&data).unwrap();
        // Footer payload: [0x5d][height:4LE][time:4LE], at last - 9
        let footer_start = last - 9;
        let height = u32::from_le_bytes([
            data[footer_start + 1], data[footer_start + 2],
            data[footer_start + 3], data[footer_start + 4],
        ]);
        let time = u32::from_le_bytes([
            data[footer_start + 5], data[footer_start + 6],
            data[footer_start + 7], data[footer_start + 8],
        ]);
        assert_eq!(height, 300);
        assert_eq!(time, 9999, "wrong block 300 — orphan bytes leaked through");

        fs::remove_file(path).ok();
    }
}

/// Open (or create) the shield.bin cache file.
pub fn open_cache(path: &str) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Get the current size of the cache file.
pub fn cache_size(path: &str) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Recover shield.bin by truncating to the last complete block footer.
///
/// Mirrors PivxNodeController's `recoverShieldbin()` — scans backward through
/// the file to find the last valid 0x5d footer packet, then truncates everything
/// after it. Prevents a half-written block from corrupting the stream.
///
/// Returns the height of the last good block, or None if the file is empty/corrupt.
pub fn recover_cache(path: &str) -> Option<u32> {
    if !Path::new(path).exists() || cache_size(path) == 0 {
        return None;
    }

    let data = fs::read(path).ok()?;
    let last_footer_end = find_last_footer(&data)?;

    // Defensive: a malformed find_last_footer return less than 9
    // would underflow `last_footer_end - 9` below in release mode,
    // and read random bytes for the height. Bail out instead — the
    // file is too small to contain a real footer.
    if last_footer_end < 9 {
        return None;
    }

    if last_footer_end < data.len() {
        eprintln!("  [recovery] Truncating shield.bin from {} to {} bytes (removing incomplete block)",
            data.len(), last_footer_end);
        let file = OpenOptions::new().write(true).open(path).ok()?;
        file.set_len(last_footer_end as u64).ok()?;
    }

    // Extract the height from the last footer
    // Footer: [4-byte LE len=9][0x5d][height:4LE][time:4LE]
    let footer_start = last_footer_end - 9; // 9-byte payload
    let height = u32::from_le_bytes([
        data[footer_start + 1], data[footer_start + 2],
        data[footer_start + 3], data[footer_start + 4],
    ]);

    Some(height)
}

/// Scan the binary stream to find the byte offset just past the last complete 0x5d footer.
fn find_last_footer(data: &[u8]) -> Option<usize> {
    let mut pos = 0;
    let mut last_footer_end = None;

    while pos + 4 <= data.len() {
        let len = u32::from_le_bytes([
            data[pos], data[pos + 1], data[pos + 2], data[pos + 3],
        ]) as usize;

        let packet_end = pos + 4 + len;
        if packet_end > data.len() {
            break; // Incomplete packet — truncate here
        }

        // Check if this is a footer (first payload byte is 0x5d, length is 9)
        if len == 9 && data[pos + 4] == 0x5d {
            last_footer_end = Some(packet_end);
        }

        pos = packet_end;
    }

    last_footer_end
}

