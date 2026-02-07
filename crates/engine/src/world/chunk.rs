use super::block::BlockId;
use super::position::LocalBlockPos;
use std::collections::HashMap;

/// Number of blocks along each axis of a chunk section.
pub const SECTION_SIZE: usize = 16;
/// Total block count in one section.
const SECTION_VOLUME: usize = SECTION_SIZE * SECTION_SIZE * SECTION_SIZE;

/// A 16x16x16 cube of blocks.
///
/// Stored as a flat array in XZY order for cache-friendly vertical scans
/// (gravity, lighting). A section that is entirely air is never allocated
/// (see `Chunk`).
#[derive(Clone)]
pub struct ChunkSection {
    blocks: Box<[BlockId; SECTION_VOLUME]>,
}

impl ChunkSection {
    pub fn new_filled(block: BlockId) -> Self {
        Self {
            blocks: Box::new([block; SECTION_VOLUME]),
        }
    }

    pub fn new_empty() -> Self {
        Self::new_filled(BlockId::AIR)
    }

    #[inline]
    const fn index(x: u8, y: u8, z: u8) -> usize {
        (y as usize) * SECTION_SIZE * SECTION_SIZE + (z as usize) * SECTION_SIZE + (x as usize)
    }

    #[inline]
    pub fn get(&self, x: u8, y: u8, z: u8) -> BlockId {
        self.blocks[Self::index(x, y, z)]
    }

    #[inline]
    pub fn set(&mut self, x: u8, y: u8, z: u8, block: BlockId) {
        self.blocks[Self::index(x, y, z)] = block;
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.iter().all(|b| *b == BlockId::AIR)
    }
}

/// A column of chunk sections, keyed by section index (y >> 4).
///
/// Only non-empty sections are stored (sparse).
pub struct Chunk {
    sections: HashMap<i32, ChunkSection>,
}

impl Chunk {
    pub fn new() -> Self {
        Self {
            sections: HashMap::new(),
        }
    }

    pub fn get_block(&self, pos: LocalBlockPos) -> BlockId {
        let section_idx = pos.section_index();
        match self.sections.get(&section_idx) {
            Some(section) => section.get(pos.x, pos.section_local_y(), pos.z),
            None => BlockId::AIR,
        }
    }

    pub fn set_block(&mut self, pos: LocalBlockPos, block: BlockId) {
        let section_idx = pos.section_index();

        if block == BlockId::AIR {
            if let Some(section) = self.sections.get_mut(&section_idx) {
                section.set(pos.x, pos.section_local_y(), pos.z, block);
                if section.is_empty() {
                    self.sections.remove(&section_idx);
                }
            }
        } else {
            let section = self
                .sections
                .entry(section_idx)
                .or_insert_with(ChunkSection::new_empty);
            section.set(pos.x, pos.section_local_y(), pos.z, block);
        }
    }

    pub fn section_count(&self) -> usize {
        self.sections.len()
    }
}

impl Default for Chunk {
    fn default() -> Self {
        Self::new()
    }
}
