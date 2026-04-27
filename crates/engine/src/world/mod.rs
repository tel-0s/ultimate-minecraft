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
    /// Chunks whose sky light has already been initialized.
    sky_lit: DashSet<ChunkPos>,
}

impl World {
    pub fn new() -> Self {
        Self {
            chunks: DashMap::new(),
            dirty: DashSet::new(),
            sky_lit: DashSet::new(),
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

    /// Get a mutable reference to a single chunk by position, if present.
    pub fn get_chunk_mut(&self, pos: &ChunkPos) -> Option<dashmap::mapref::one::RefMut<'_, ChunkPos, Chunk>> {
        self.chunks.get_mut(pos)
    }

    // ── Light accessors ──────────────────────────────────────────────────

    pub fn get_sky_light(&self, pos: BlockPos) -> u8 {
        match self.chunks.get(&pos.chunk()) {
            Some(chunk) => chunk.get_sky_light(pos.local()),
            None => 15, // unloaded chunks default to full sky light
        }
    }

    pub fn set_sky_light(&self, pos: BlockPos, val: u8) {
        self.chunks
            .entry(pos.chunk())
            .or_default()
            .set_sky_light(pos.local(), val);
    }

    /// Set sky light only if the chunk already exists. Returns `true` if the
    /// write was performed. This avoids creating phantom empty chunks when
    /// light propagation BFS reaches beyond the generated world.
    pub fn set_sky_light_if_loaded(&self, pos: BlockPos, val: u8) -> bool {
        if let Some(mut chunk) = self.chunks.get_mut(&pos.chunk()) {
            chunk.set_sky_light(pos.local(), val);
            true
        } else {
            false
        }
    }

    pub fn get_block_light(&self, pos: BlockPos) -> u8 {
        match self.chunks.get(&pos.chunk()) {
            Some(chunk) => chunk.get_block_light(pos.local()),
            None => 0,
        }
    }

    pub fn set_block_light(&self, pos: BlockPos, val: u8) {
        self.chunks
            .entry(pos.chunk())
            .or_default()
            .set_block_light(pos.local(), val);
    }

    /// Set block light only if the chunk already exists. Returns `true` if
    /// the write was performed. This avoids creating phantom empty chunks
    /// when light propagation BFS reaches beyond the generated world.
    pub fn set_block_light_if_loaded(&self, pos: BlockPos, val: u8) -> bool {
        if let Some(mut chunk) = self.chunks.get_mut(&pos.chunk()) {
            chunk.set_block_light(pos.local(), val);
            true
        } else {
            false
        }
    }

    /// Returns `true` if this chunk already has sky light initialized.
    pub fn is_sky_lit(&self, pos: &ChunkPos) -> bool {
        self.sky_lit.contains(pos)
    }

    /// Mark a chunk as having its sky light initialized.
    pub fn mark_sky_lit(&self, pos: ChunkPos) {
        self.sky_lit.insert(pos);
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}
