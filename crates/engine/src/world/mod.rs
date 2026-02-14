pub mod block;
pub mod chunk;
pub mod position;

use block::BlockId;
use chunk::Chunk;
use dashmap::DashMap;
use position::{BlockPos, ChunkPos};

/// The entire block world. Thread-safe, lock-sharded by chunk.
///
/// This is the spatial substrate -- the fixed 3D lattice. Time and causality
/// live in `causal::Graph`, not here.
pub struct World {
    chunks: DashMap<ChunkPos, Chunk>,
}

impl World {
    pub fn new() -> Self {
        Self {
            chunks: DashMap::new(),
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
    ///
    /// Takes `&self` (not `&mut self`) because `DashMap` provides interior
    /// mutability via per-shard locking.
    pub fn set_block(&self, pos: BlockPos, block: BlockId) {
        self.chunks
            .entry(pos.chunk())
            .or_default()
            .set_block(pos.local(), block);
    }

    pub fn has_chunk(&self, pos: ChunkPos) -> bool {
        self.chunks.contains_key(&pos)
    }

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
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}
