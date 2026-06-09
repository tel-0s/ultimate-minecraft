use super::block::BlockId;
use super::position::LocalBlockPos;
use std::collections::HashMap;

/// Number of blocks along each axis of a chunk section.
pub const SECTION_SIZE: usize = 16;
/// Total block count in one section.
const SECTION_VOLUME: usize = SECTION_SIZE * SECTION_SIZE * SECTION_SIZE;
/// Bytes needed for a nibble array (4 bits per block, 4096 blocks).
const NIBBLE_LEN: usize = SECTION_VOLUME / 2;

/// A 16x16x16 cube of blocks, stored **paletted** (Phase 6c).
///
/// Natural terrain sections contain few unique blocks, so instead of a
/// raw `[BlockId; 4096]` (8 KB) we store the unique blocks once and pack
/// per-cell palette *indices* at the smallest sufficient width:
///
/// - `bits == 0`: uniform section — every cell is `palette[0]`, no index
///   array at all (~16 bytes). Stone bulk, ocean water, etc.
/// - `bits == 4`: ≤16 unique blocks — 2 KB of indices. The common case
///   for surface terrain (4× smaller than raw).
/// - `bits == 8` / `16`: ≤256 / arbitrary unique blocks.
///
/// Indices pack little-endian into `u64` words with no index spanning two
/// words — the same convention as the MC wire format, so serialization
/// can walk indices cheaply. The palette only grows (an overwritten
/// block's entry may linger, slightly widening `bits` until a future
/// compaction pass); `get`/`set` stay O(1) plus a short palette scan on
/// novel blocks.
///
/// Cell order is XZY (`y*256 + z*16 + x`) for cache-friendly vertical
/// scans (gravity, lighting). A section that is entirely air is never
/// allocated (see `Chunk`).
#[derive(Clone)]
pub struct ChunkSection {
    /// Unique blocks; cell values are indices into this.
    palette: Vec<BlockId>,
    /// Bits per packed index: 0 (uniform), 4, 8, or 16.
    bits: u8,
    /// Packed indices (`SECTION_VOLUME` entries); empty when `bits == 0`.
    data: Vec<u64>,
    /// Count of non-air cells — makes `is_empty` O(1) (it used to be an
    /// O(4096) scan on every air-write via `Chunk::set_block`).
    non_air: u16,
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
            palette: vec![block],
            bits: 0,
            data: Vec::new(),
            non_air: if block == BlockId::AIR { 0 } else { SECTION_VOLUME as u16 },
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
    fn read_index(&self, cell: usize) -> usize {
        debug_assert!(self.bits > 0);
        let per_word = 64 / self.bits as usize;
        let word = self.data[cell / per_word];
        let shift = (cell % per_word) * self.bits as usize;
        let mask = (1u64 << self.bits) - 1;
        ((word >> shift) & mask) as usize
    }

    #[inline]
    fn write_index(&mut self, cell: usize, value: usize) {
        debug_assert!(self.bits > 0);
        let per_word = 64 / self.bits as usize;
        let shift = (cell % per_word) * self.bits as usize;
        let mask = (1u64 << self.bits) - 1;
        let word = &mut self.data[cell / per_word];
        *word = (*word & !(mask << shift)) | (((value as u64) & mask) << shift);
    }

    /// Widen packed indices to `new_bits`, re-packing every cell.
    fn repack(&mut self, new_bits: u8) {
        debug_assert!(new_bits > self.bits);
        let per_word = 64 / new_bits as usize;
        let mut new_data = vec![0u64; SECTION_VOLUME.div_ceil(per_word)];
        for cell in 0..SECTION_VOLUME {
            let value = if self.bits == 0 { 0 } else { self.read_index(cell) } as u64;
            let shift = (cell % per_word) * new_bits as usize;
            new_data[cell / per_word] |= value << shift;
        }
        self.data = new_data;
        self.bits = new_bits;
    }

    /// Palette position of `block`, adding (and widening if needed) when new.
    fn palette_index(&mut self, block: BlockId) -> usize {
        if let Some(i) = self.palette.iter().position(|b| *b == block) {
            return i;
        }
        let i = self.palette.len();
        let capacity = if self.bits == 0 { 1 } else { 1usize << self.bits };
        if i >= capacity {
            let new_bits = match self.bits {
                0 => 4,
                4 => 8,
                8 => 16,
                _ => unreachable!("palette cannot exceed 4096 entries"),
            };
            self.repack(new_bits);
        }
        self.palette.push(block);
        i
    }

    #[inline]
    pub fn get(&self, x: u8, y: u8, z: u8) -> BlockId {
        self.get_by_index(Self::index(x, y, z))
    }

    /// Read by flat cell index (`y*256 + z*16 + x`); used by serialization
    /// loops that walk the whole section.
    #[inline]
    pub fn get_by_index(&self, cell: usize) -> BlockId {
        if self.bits == 0 {
            self.palette[0]
        } else {
            self.palette[self.read_index(cell)]
        }
    }

    pub fn set(&mut self, x: u8, y: u8, z: u8, block: BlockId) {
        let cell = Self::index(x, y, z);
        let old = self.get_by_index(cell);
        if old == block {
            return;
        }

        let pi = self.palette_index(block);
        if self.bits == 0 {
            // Uniform section gaining a second block: promote to 4-bit
            // indices (all currently 0 = the old uniform value).
            self.repack(4);
        }
        self.write_index(cell, pi);

        match (old == BlockId::AIR, block == BlockId::AIR) {
            (true, false) => self.non_air += 1,
            (false, true) => self.non_air -= 1,
            _ => {}
        }
    }

    /// O(1): the section is all air.
    pub fn is_empty(&self) -> bool {
        self.non_air == 0
    }

    /// Number of non-air cells (the chunk wire format sends this).
    pub fn non_air_count(&self) -> u16 {
        self.non_air
    }

    /// The unique blocks present (may include stale entries for blocks
    /// since overwritten). Cell values index into this via `read_index`.
    pub fn palette(&self) -> &[BlockId] {
        &self.palette
    }

    /// Heap bytes used by this section's block storage (palette + packed
    /// indices). A raw array would be 8192 bytes; uniform sections are ~2,
    /// 4-bit sections ~2050.
    pub fn memory_bytes(&self) -> usize {
        self.palette.len() * size_of::<BlockId>() + self.data.len() * 8
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

    /// Get a non-empty section by section index, if present. Returns `None`
    /// for sections that are entirely air (those are never allocated).
    pub fn section(&self, section_idx: i32) -> Option<&ChunkSection> {
        self.sections.get(&section_idx)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_section_is_tiny_and_reads_correctly() {
        let s = ChunkSection::new_filled(BlockId::new(7));
        assert_eq!(s.get(0, 0, 0), BlockId::new(7));
        assert_eq!(s.get(15, 15, 15), BlockId::new(7));
        assert!(!s.is_empty());
        assert_eq!(s.non_air_count(), 4096);
        assert!(s.memory_bytes() < 16, "uniform section should store no index data");

        let air = ChunkSection::new_empty();
        assert!(air.is_empty());
        assert_eq!(air.non_air_count(), 0);
    }

    #[test]
    fn promotion_keeps_existing_cells() {
        let mut s = ChunkSection::new_filled(BlockId::new(1));
        s.set(3, 4, 5, BlockId::new(2)); // promotes 0 → 4 bits
        assert_eq!(s.get(3, 4, 5), BlockId::new(2));
        assert_eq!(s.get(0, 0, 0), BlockId::new(1));
        assert_eq!(s.get(15, 0, 9), BlockId::new(1));
        assert_eq!(s.non_air_count(), 4096);
    }

    #[test]
    fn repack_4_to_8_to_16_bits() {
        let mut s = ChunkSection::new_empty();
        // Force >16 unique blocks (4→8 bit repack), then >256 (8→16).
        for i in 0..300u16 {
            let x = (i % 16) as u8;
            let z = ((i / 16) % 16) as u8;
            let y = (i / 256) as u8;
            s.set(x, y, z, BlockId::new(i + 1));
        }
        for i in 0..300u16 {
            let x = (i % 16) as u8;
            let z = ((i / 16) % 16) as u8;
            let y = (i / 256) as u8;
            assert_eq!(s.get(x, y, z), BlockId::new(i + 1), "cell {i} after repacks");
        }
        assert_eq!(s.non_air_count(), 300);
    }

    #[test]
    fn randomized_ops_match_reference_array() {
        // SplitMix64-driven fuzz: identical results to a plain array.
        let mut s = ChunkSection::new_empty();
        let mut reference = vec![BlockId::AIR; SECTION_VOLUME];
        let mut state = 0x5EEDu64;
        let mut next = move || {
            state = state.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            z ^ (z >> 31)
        };

        for _ in 0..50_000 {
            let x = (next() % 16) as u8;
            let y = (next() % 16) as u8;
            let z = (next() % 16) as u8;
            // ~40 distinct block values, AIR included → exercises 8-bit width.
            let block = BlockId::new((next() % 40) as u16);
            s.set(x, y, z, block);
            reference[(y as usize) * 256 + (z as usize) * 16 + (x as usize)] = block;
        }

        let mut ref_non_air = 0u16;
        for (cell, &expect) in reference.iter().enumerate() {
            assert_eq!(s.get_by_index(cell), expect, "cell {cell}");
            if expect != BlockId::AIR {
                ref_non_air += 1;
            }
        }
        assert_eq!(s.non_air_count(), ref_non_air);
        assert!(
            s.memory_bytes() < 8192,
            "paletted storage ({} B) should beat the raw 8 KB array",
            s.memory_bytes(),
        );
    }
}
