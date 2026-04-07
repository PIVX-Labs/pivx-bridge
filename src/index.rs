/// Shield block index — tracks which blocks contain Sapling transactions.
///
/// Persisted as JSON for compatibility with PivxNodeController's shield.json.
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// An entry in the shield index (compatible with PivxNodeController format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    /// Block height.
    pub block: u32,
    /// Byte offset into the binary stream file.
    pub i: u64,
}

/// In-memory shield block index.
#[derive(Debug, Clone)]
pub struct ShieldIndex {
    /// Ordered list of shield block entries.
    pub entries: Vec<IndexEntry>,
    /// Just the heights (for fast lookup).
    pub shield_heights: Vec<u32>,
}

impl ShieldIndex {
    /// Create an empty index.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            shield_heights: Vec::new(),
        }
    }

    /// Load from a JSON file (PivxNodeController shield.json format).
    pub fn load(path: &str) -> Result<Self, String> {
        let data = fs::read_to_string(path)
            .map_err(|e| format!("read index: {e}"))?;
        let entries: Vec<IndexEntry> = serde_json::from_str(&data)
            .map_err(|e| format!("parse index: {e}"))?;
        let shield_heights = entries.iter().map(|e| e.block).collect();
        Ok(Self { entries, shield_heights })
    }

    /// Save to a JSON file.
    pub fn save(&self, path: &str) -> Result<(), String> {
        let json = serde_json::to_string(&self.entries)
            .map_err(|e| format!("serialize index: {e}"))?;
        fs::write(path, json)
            .map_err(|e| format!("write index: {e}"))
    }

    /// Add a shield block to the index.
    pub fn add(&mut self, height: u32, byte_offset: u64) {
        if !self.shield_heights.contains(&height) {
            self.entries.push(IndexEntry { block: height, i: byte_offset });
            self.shield_heights.push(height);
        }
    }

    /// Get all shield block heights from a starting height (binary search).
    pub fn heights_from(&self, start: u32) -> Vec<u32> {
        let idx = self.shield_heights.partition_point(|&h| h < start);
        self.shield_heights[idx..].to_vec()
    }

    /// Last indexed height, if any.
    pub fn last_height(&self) -> Option<u32> {
        self.shield_heights.last().copied()
    }

    /// Get the byte offset for a given start block.
    ///
    /// Returns the offset of the first entry with height >= start_block,
    /// or the end of the file if no entries match.
    pub fn offset_for_height(&self, height: u32) -> Option<u64> {
        let idx = self.entries.partition_point(|e| e.block < height);
        self.entries.get(idx).map(|e| e.i)
    }

    /// Compute the total byte length of shield data between two block heights.
    ///
    /// Used by `getshielddatalength` to report progress without materializing the stream.
    pub fn byte_length_between(&self, start: u32, end: u32, total_cache_size: u64) -> u64 {
        let start_offset = self.offset_for_height(start).unwrap_or(total_cache_size);
        // Find the first entry AFTER end block
        let end_offset = self.entries.iter()
            .find(|e| e.block > end)
            .map(|e| e.i)
            .unwrap_or(total_cache_size);
        end_offset.saturating_sub(start_offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_index_is_empty() {
        let idx = ShieldIndex::new();
        assert!(idx.entries.is_empty());
        assert!(idx.shield_heights.is_empty());
        assert_eq!(idx.last_height(), None);
    }

    #[test]
    fn add_block_tracks_height_and_offset() {
        let mut idx = ShieldIndex::new();
        idx.add(2700501, 0);
        idx.add(2700510, 1234);
        assert_eq!(idx.shield_heights, vec![2700501, 2700510]);
        assert_eq!(idx.entries[0].block, 2700501);
        assert_eq!(idx.entries[0].i, 0);
        assert_eq!(idx.entries[1].i, 1234);
    }

    #[test]
    fn add_duplicate_height_ignored() {
        let mut idx = ShieldIndex::new();
        idx.add(100, 0);
        idx.add(100, 999);
        assert_eq!(idx.shield_heights.len(), 1);
        assert_eq!(idx.entries[0].i, 0); // first one wins
    }

    #[test]
    fn heights_from_filters_correctly() {
        let mut idx = ShieldIndex::new();
        for h in [100, 200, 300, 400, 500] {
            idx.add(h, 0);
        }
        assert_eq!(idx.heights_from(250), vec![300, 400, 500]);
        assert_eq!(idx.heights_from(100), vec![100, 200, 300, 400, 500]);
        assert_eq!(idx.heights_from(600), Vec::<u32>::new());
    }

    #[test]
    fn offset_for_height() {
        let mut idx = ShieldIndex::new();
        idx.add(100, 0);
        idx.add(200, 500);
        idx.add(300, 1200);

        assert_eq!(idx.offset_for_height(100), Some(0));
        assert_eq!(idx.offset_for_height(200), Some(500));
        assert_eq!(idx.offset_for_height(150), Some(500)); // next >= 150
        assert_eq!(idx.offset_for_height(400), None);
    }

    #[test]
    fn byte_length_between() {
        let mut idx = ShieldIndex::new();
        idx.add(100, 0);
        idx.add(200, 500);
        idx.add(300, 1200);

        let total = 2000u64;
        // From block 100 to 200: offset 0 to first block > 200 (300 at offset 1200)
        assert_eq!(idx.byte_length_between(100, 200, total), 1200);
        // From block 200 to 300: offset 500 to end (no block > 300)
        assert_eq!(idx.byte_length_between(200, 300, total), 1500);
        // From block 100 to 300: full file
        assert_eq!(idx.byte_length_between(100, 300, total), 2000);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let mut idx = ShieldIndex::new();
        idx.add(2700501, 0);
        idx.add(2700510, 5000);

        let path = "/tmp/pivx_bridge_test_index.json";
        idx.save(path).unwrap();
        let loaded = ShieldIndex::load(path).unwrap();

        assert_eq!(loaded.shield_heights, vec![2700501, 2700510]);
        assert_eq!(loaded.entries[0].block, 2700501);
        assert_eq!(loaded.entries[1].i, 5000);

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn json_format_matches_pivx_node_controller() {
        let mut idx = ShieldIndex::new();
        idx.add(2700501, 0);
        idx.add(2700510, 1234);

        let json = serde_json::to_string(&idx.entries).unwrap();
        // PivxNodeController format: [{block: N, i: M}, ...]
        assert!(json.contains("\"block\":2700501"));
        assert!(json.contains("\"i\":1234"));

        // Verify it parses back
        let parsed: Vec<IndexEntry> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 2);
    }
}

/// Load index from file, or create empty if it doesn't exist.
pub fn load_or_create(path: &str) -> ShieldIndex {
    if Path::new(path).exists() {
        match ShieldIndex::load(path) {
            Ok(idx) => {
                eprintln!("  Loaded shield index: {} blocks", idx.shield_heights.len());
                idx
            }
            Err(e) => {
                eprintln!("  Warning: failed to load index: {e} — starting fresh");
                ShieldIndex::new()
            }
        }
    } else {
        eprintln!("  No existing index — starting fresh");
        ShieldIndex::new()
    }
}
