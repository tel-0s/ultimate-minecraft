use super::block::BlockId;
use super::position::LocalBlockPos;
use std::collections::HashMap;

/// Number of blocks along each axis of a chunk section.
pub const SECTION_SIZE: usize = 16;
/// Total block count in one section.
const SECTION_VOLUME: usize = SECTION_SIZE * SECTION_SIZE * SECTION_SIZE;
/// Bytes needed for a nibble array (4 bits per block, 4096 blocks).
const NIBBLE_LEN: usize = SECTION_VOLUME / 2;

/// A 16x16x16 cube of blocks.
///
/// Stored as a flat array in XZY order for cache-friendly vertical scans
/// (gravity, lighting). A section that is entirely air is never allocated
/// (see `Chunk`).
#[derive(Clone)]
pub struct ChunkSection {
    blocks: Box<[BlockId; SECTION_VOLUME]>,
}

/// Per-section lighting: sky light + block light as packed nibble arrays.
///
/// Each array is 2048 bytes storing 4096 4-bit values (one per block).
/// Two block indices share a byte: even index in the low nibble, odd in the
/// high nibble (matching the vanilla MC wire format).
#[derive(Clone)]
pub struct LightSection {
    pub sky: Box<[u8; NIBBLE_LEN]>,
    pub block: Box<[u8; NIBBLE_LEN]>,
}

impl LightSection {
    pub fn new() -> Self {
        Self {
            sky: Box::new([0u8; NIBBLE_LEN]),
            block: Box::new([0u8; NIBBLE_LEN]),
        }
    }

    pub fn new_full_sky() -> Self {
        Self {
            sky: Box::new([0xFF; NIBBLE_LEN]),
            block: Box::new([0u8; NIBBLE_LEN]),
        }
    }

    #[inline]
    fn nibble_index(x: u8, y: u8, z: u8) -> (usize, bool) {
        let idx = (y as usize) * SECTION_SIZE * SECTION_SIZE
            + (z as usize) * SECTION_SIZE
            + (x as usize);
        (idx >> 1, idx & 1 != 0)
    }

    #[inline]
    pub fn get_sky(&self, x: u8, y: u8, z: u8) -> u8 {
        let (byte_idx, high) = Self::nibble_index(x, y, z);
        if high { self.sky[byte_idx] >> 4 } else { self.sky[byte_idx] & 0x0F }
    }

    #[inline]
    pub fn set_sky(&mut self, x: u8, y: u8, z: u8, val: u8) {
        let (byte_idx, high) = Self::nibble_index(x, y, z);
        if high {
            self.sky[byte_idx] = (self.sky[byte_idx] & 0x0F) | ((val & 0x0F) << 4);
        } else {
            self.sky[byte_idx] = (self.sky[byte_idx] & 0xF0) | (val & 0x0F);
        }
    }

    #[inline]
    pub fn get_block_light(&self, x: u8, y: u8, z: u8) -> u8 {
        let (byte_idx, high) = Self::nibble_index(x, y, z);
        if high { self.block[byte_idx] >> 4 } else { self.block[byte_idx] & 0x0F }
    }

    #[inline]
    pub fn set_block_light(&mut self, x: u8, y: u8, z: u8, val: u8) {
        let (byte_idx, high) = Self::nibble_index(x, y, z);
        if high {
            self.block[byte_idx] = (self.block[byte_idx] & 0x0F) | ((val & 0x0F) << 4);
        } else {
            self.block[byte_idx] = (self.block[byte_idx] & 0xF0) | (val & 0x0F);
        }
    }

    pub fn is_sky_empty(&self) -> bool {
        self.sky.iter().all(|&b| b == 0)
    }

    pub fn is_block_empty(&self) -> bool {
        self.block.iter().all(|&b| b == 0)
    }
}

impl Default for LightSection {
    fn default() -> Self {
        Self::new()
    }
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

    /// Direct access to the underlying block array (4096 entries, XZY order).
    pub fn blocks(&self) -> &[BlockId; SECTION_VOLUME] {
        &self.blocks
    }
}

/// A column of chunk sections, keyed by section index (y >> 4).
///
/// Only non-empty sections are stored (sparse).
pub struct Chunk {
    sections: HashMap<i32, ChunkSection>,
    light: HashMap<i32, LightSection>,
}

impl Chunk {
    pub fn new() -> Self {
        Self {
            sections: HashMap::new(),
            light: HashMap::new(),
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

    // ── Light accessors ──────────────────────────────────────────────────

    pub fn get_sky_light(&self, pos: LocalBlockPos) -> u8 {
        let si = pos.section_index();
        match self.light.get(&si) {
            Some(ls) => ls.get_sky(pos.x, pos.section_local_y(), pos.z),
            None => 0,
        }
    }

    pub fn set_sky_light(&mut self, pos: LocalBlockPos, val: u8) {
        let si = pos.section_index();
        self.light
            .entry(si)
            .or_default()
            .set_sky(pos.x, pos.section_local_y(), pos.z, val);
    }

    pub fn get_block_light(&self, pos: LocalBlockPos) -> u8 {
        let si = pos.section_index();
        match self.light.get(&si) {
            Some(ls) => ls.get_block_light(pos.x, pos.section_local_y(), pos.z),
            None => 0,
        }
    }

    pub fn set_block_light(&mut self, pos: LocalBlockPos, val: u8) {
        let si = pos.section_index();
        self.light
            .entry(si)
            .or_default()
            .set_block_light(pos.x, pos.section_local_y(), pos.z, val);
    }

    /// Get the light section for a given section index, if it exists.
    pub fn light_section(&self, section_idx: i32) -> Option<&LightSection> {
        self.light.get(&section_idx)
    }

    /// Get or create a mutable light section for a given section index.
    pub fn light_section_mut(&mut self, section_idx: i32) -> &mut LightSection {
        self.light.entry(section_idx).or_default()
    }

    pub fn section_count(&self) -> usize {
        self.sections.len()
    }

    /// Iterate over all non-empty sections as (section_index, section).
    pub fn sections(&self) -> impl Iterator<Item = (&i32, &ChunkSection)> {
        self.sections.iter()
    }

    /// Iterate over all light sections as (section_index, light_section).
    pub fn light_sections(&self) -> impl Iterator<Item = (&i32, &LightSection)> {
        self.light.iter()
    }
}

impl Default for Chunk {
    fn default() -> Self {
        Self::new()
    }
}
