pub mod block;
pub mod chunk;
pub mod position;

use block::BlockId;
use chunk::Chunk;
use dashmap::{DashMap, DashSet};
use position::{BlockPos, ChunkPos};

/// The entire block world. Thread-safe, lock-sharded by chunk.
///
/// This is the spatial substrate -- the fixed 3D lattice. Time and causality
/// live in `causal::Graph`, not here.
pub struct World {
    chunks: DashMap<ChunkPos, Chunk>,
    /// Chunks that have been modified since the last save.
    dirty: DashSet<ChunkPos>,
}

impl World {
    pub fn new() -> Self {
        Self {
            chunks: DashMap::new(),
            dirty: DashSet::new(),
        }
    }

    /// Read a block at an absolute position. Returns AIR for unloaded chunks.
    pub fn get_block(&self, pos: BlockPos) -> BlockId {
        match self.chunks.get(&pos.chunk()) {
            Some(chunk) => chunk.get_block(pos.local()),
            None => BlockId::AIR,
        }
    }

    /// Write a block at an absolute position. Creates the chunk if needed.
    /// Marks the containing chunk as dirty for persistence.
    ///
    /// Takes `&self` (not `&mut self`) because `DashMap` provides interior
    /// mutability via per-shard locking.
    pub fn set_block(&self, pos: BlockPos, block: BlockId) {
        let chunk_pos = pos.chunk();
        self.chunks
            .entry(chunk_pos)
            .or_default()
            .set_block(pos.local(), block);
        self.dirty.insert(chunk_pos);
    }

    pub fn has_chunk(&self, pos: ChunkPos) -> bool {
        self.chunks.contains_key(&pos)
    }

    /// Insert a chunk without marking it dirty (used for generation/loading).
    pub fn insert_chunk(&self, pos: ChunkPos, chunk: Chunk) {
        self.chunks.insert(pos, chunk);
    }

    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Iterate over all chunks. Each entry is a DashMap ref that derefs to
    /// `(ChunkPos, Chunk)`. Use `*entry.key()` and `&*entry` (value).
    pub fn iter_chunks(&self) -> dashmap::iter::Iter<'_, ChunkPos, Chunk> {
        self.chunks.iter()
    }

    /// Drain and return all chunk positions that have been modified since the
    /// last call. After this returns, the dirty set is empty.
    pub fn take_dirty_chunks(&self) -> Vec<ChunkPos> {
        let mut dirty = Vec::new();
        // Collect then remove; a tiny race (chunk dirtied between collect and
        // remove) just means it'll be re-saved next time -- always safe.
        for entry in self.dirty.iter() {
            dirty.push(*entry);
        }
        for pos in &dirty {
            self.dirty.remove(pos);
        }
        dirty
    }

    /// Number of chunks currently marked dirty.
    pub fn dirty_count(&self) -> usize {
        self.dirty.len()
    }

    /// Get a reference to a single chunk by position, if present.
    pub fn get_chunk(&self, pos: &ChunkPos) -> Option<dashmap::mapref::one::Ref<'_, ChunkPos, Chunk>> {
        self.chunks.get(pos)
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}
