/// Persistent binary cache — shield.bin management.
///
/// The scan loop writes pre-encoded PIVX-compat binary data to disk.
/// HTTP requests serve byte ranges from the file instead of re-fetching
/// from the node on every request.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

use crate::scanner::ShieldBlock;
use crate::stream;
use crate::api::StreamFormat;

/// Append a shield block to the binary cache file in PIVX-compat format.
///
/// Returns the byte offset where this block's data starts and its length.
pub fn append_block(file: &mut File, block: &ShieldBlock) -> std::io::Result<(u64, u64)> {
    let offset = file.seek(SeekFrom::End(0))?;
    let encoded = stream::encode_shield_stream(std::slice::from_ref(block), StreamFormat::PivxCompat);
    file.write_all(&encoded)?;
    file.flush()?;
    Ok((offset, encoded.len() as u64))
}

/// Append multiple shield blocks to the cache file.
///
/// Returns a vec of (height, byte_offset, byte_length) for each block.
pub fn append_blocks(file: &mut File, blocks: &[ShieldBlock]) -> std::io::Result<Vec<(u32, u64, u64)>> {
    let mut entries = Vec::new();
    for block in blocks {
        let (offset, len) = append_block(file, block)?;
        entries.push((block.height, offset, len));
    }
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

