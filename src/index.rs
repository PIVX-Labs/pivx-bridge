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

    /// Get all shield block heights from a starting height.
    pub fn heights_from(&self, start: u32) -> Vec<u32> {
        self.shield_heights.iter().copied().filter(|&h| h >= start).collect()
    }

    /// Last indexed height, if any.
    pub fn last_height(&self) -> Option<u32> {
        self.shield_heights.last().copied()
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
