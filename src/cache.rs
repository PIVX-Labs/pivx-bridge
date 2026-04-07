/// Persistent binary cache — shield.bin management.
///
/// The scan loop writes pre-encoded PIVX-compat binary data to disk.
/// HTTP requests serve byte ranges from the file instead of re-fetching
/// from the node on every request.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::scanner::ShieldBlock;
use crate::stream;
use crate::api::StreamFormat;

/// Append multiple shield blocks to the cache file with a single flush.
///
/// Returns a vec of (height, byte_offset, byte_length) for each block.
pub fn append_blocks(file: &mut File, blocks: &[ShieldBlock]) -> std::io::Result<Vec<(u32, u64, u64)>> {
    let mut current_offset = file.seek(SeekFrom::End(0))?;
    let mut entries = Vec::with_capacity(blocks.len());

    for block in blocks {
        let encoded = stream::encode_shield_stream(std::slice::from_ref(block), StreamFormat::PivxCompat);
        file.write_all(&encoded)?;
        entries.push((block.height, current_offset, encoded.len() as u64));
        current_offset += encoded.len() as u64;
    }

    file.flush()?; // single flush for entire batch
    Ok(entries)
}

/// Read a byte range from the cache file.
pub fn read_range(file: &mut File, offset: u64, length: u64) -> std::io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; length as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

/// Read from a byte offset to the end of the file.
pub fn read_from(file: &mut File, offset: u64) -> std::io::Result<Vec<u8>> {
    let end = file.seek(SeekFrom::End(0))?;
    if offset >= end {
        return Ok(Vec::new());
    }
    read_range(file, offset, end - offset)
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

        let data = read_from(&mut file, 0).unwrap();
        assert_eq!(data.len(), entries[0].2 as usize);

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

