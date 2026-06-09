//! World persistence using Minecraft's Anvil region file format (.mca).
//!
//! Saves and loads `World` data to/from `world/region/r.X.Z.mca` files,
//! producing files compatible with vanilla Minecraft tools.

use std::collections::HashMap;
use std::fs;
use std::io::{Cursor, Seek};
use std::path::Path;
use std::sync::LazyLock;
use std::time::Instant;

use anyhow::{Context, Result};
use azalea_block::{BlockState, BlockTrait};
use serde::{Deserialize, Serialize};

use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::chunk::{Chunk, ChunkSection};
use ultimate_engine::world::position::{ChunkPos, LocalBlockPos};
use ultimate_engine::world::World;

// ── MC 1.21.11 data version ─────────────────────────────────────────────────

/// DataVersion tag written into every saved chunk. MC 1.21.11 = 4189.
const DATA_VERSION: i32 = 4189;

// ── Reverse lookup table: (name, properties) → BlockState ID ─────────────────

/// Key for the reverse block lookup: `("stone", {})` or `("oak_stairs", {"facing": "north", ...})`.
type BlockLookupKey = (String, Vec<(String, String)>);

/// Lazily-built reverse lookup table: `(name, sorted_properties) → state_id`.
static BLOCK_LOOKUP: LazyLock<HashMap<BlockLookupKey, u16>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    for id in 0..=BlockState::MAX_STATE {
        let state = BlockState::try_from(id as u32).unwrap();
        let block: Box<dyn BlockTrait> = Box::<dyn BlockTrait>::from(state);
        let name = block.id().to_string(); // "stone", "oak_stairs", etc.
        let mut props: Vec<(String, String)> = block
            .property_map()
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        props.sort();
        map.insert((name, props), id);
    }
    map
});

/// Look up a block state ID by name and sorted property list.
///
/// Used by the placement system to resolve oriented block states.
pub(crate) fn lookup_block_state(name: &str, props: &[(String, String)]) -> Option<u16> {
    BLOCK_LOOKUP
        .get(&(name.to_string(), props.to_vec()))
        .copied()
}

/// Convert a palette entry (name + optional properties) back to a BlockId.
fn palette_entry_to_block_id(entry: &PaletteEntry) -> BlockId {
    let name = entry
        .name
        .strip_prefix("minecraft:")
        .unwrap_or(&entry.name);
    let mut props: Vec<(String, String)> = entry
        .properties
        .as_ref()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    props.sort();

    if let Some(&id) = BLOCK_LOOKUP.get(&(name.to_string(), props)) {
        BlockId(id)
    } else {
        tracing::warn!("Unknown block in save file: {}, defaulting to air", entry.name);
        BlockId::AIR
    }
}

/// Convert a BlockId to a palette entry (name + optional properties).
fn block_id_to_palette_entry(id: BlockId) -> PaletteEntry {
    if id == BlockId::AIR {
        return PaletteEntry {
            name: "minecraft:air".into(),
            properties: None,
        };
    }
    let state = BlockState::try_from(id.0 as u32).unwrap_or(BlockState::AIR);
    let block: Box<dyn BlockTrait> = Box::<dyn BlockTrait>::from(state);
    let name = format!("minecraft:{}", block.id());
    let prop_map = block.property_map();
    let properties = if prop_map.is_empty() {
        None
    } else {
        Some(
            prop_map
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    };
    PaletteEntry { name, properties }
}

// ── Chunk NBT structs (serde) ────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug)]
struct ChunkNbt {
    #[serde(rename = "DataVersion")]
    data_version: i32,
    #[serde(rename = "xPos")]
    x_pos: i32,
    #[serde(rename = "zPos")]
    z_pos: i32,
    #[serde(rename = "yPos")]
    y_pos: i32,
    sections: Vec<SectionNbt>,
    #[serde(rename = "Status")]
    status: String,
    /// Generator fingerprint (preset + seed; see
    /// `worldgen::preset::fingerprint`) stamped at save time.
    ///
    /// For **delta** chunks a mismatch is fine — the delta re-applies onto
    /// freshly regenerated terrain, so edits survive generator upgrades.
    /// For legacy **full-section** chunks a mismatch (or missing stamp)
    /// skips the chunk: stale-generator terrain stitched against new
    /// terrain produces hard seams. Custom tag; vanilla tools ignore it.
    #[serde(rename = "UmcGenFp", default, skip_serializing_if = "Option::is_none")]
    gen_fp: Option<i64>,
    /// Phase 6c delta encoding: block modifications relative to the
    /// procedurally regenerated baseline, packed one per i64 as
    /// `(section_y << 32) | (cell_index << 16) | block_id` with
    /// `cell_index = local_y*256 + z*16 + x`. When present, `sections` is
    /// empty and loading regenerates the chunk then applies these cells.
    /// 10-100× smaller than full chunks for lightly-edited terrain, and
    /// robust to worldgen changes.
    #[serde(rename = "UmcDelta", default, skip_serializing_if = "Option::is_none")]
    delta: Option<Vec<i64>>,
}

// ── Delta store + overlay generator (Phase 6c eviction) ─────────────────────

/// Live in-RAM index of every known chunk delta, shared between
/// persistence (which populates it on load and save) and the worldgen
/// overlay (which re-applies deltas whenever a chunk is regenerated —
/// lazily, after eviction, or at startup). This is what makes eviction
/// safe: a non-dirty chunk is always `generate(baseline) + delta`.
pub type DeltaStore = std::sync::Arc<dashmap::DashMap<ChunkPos, std::sync::Arc<[i64]>>>;

pub fn new_delta_store() -> DeltaStore {
    std::sync::Arc::new(dashmap::DashMap::new())
}

/// Worldgen wrapper that re-applies stored deltas on every generation.
/// Installed as THE server worldgen so every `generate_chunk` /
/// `ensure_generated` path — chunk streaming, eviction re-materialization,
/// startup pregeneration — produces baseline-plus-edits.
pub struct DeltaOverlayGen {
    inner: std::sync::Arc<dyn crate::worldgen::WorldGen>,
    deltas: DeltaStore,
}

impl DeltaOverlayGen {
    pub fn new(inner: std::sync::Arc<dyn crate::worldgen::WorldGen>, deltas: DeltaStore) -> Self {
        Self { inner, deltas }
    }
}

impl crate::worldgen::WorldGen for DeltaOverlayGen {
    fn generate_chunk(&self, cx: i32, cz: i32, world: &World) -> Chunk {
        let mut chunk = self.inner.generate_chunk(cx, cz, world);
        if let Some(delta) = self.deltas.get(&ChunkPos::new(cx, cz)) {
            for &packed in delta.iter() {
                let (sy, cell, block) = unpack_delta(packed);
                chunk.set_block(delta_local_pos(sy, cell), block);
            }
        }
        chunk
    }

    fn spawn_y(&self, x: i64, z: i64) -> f64 {
        self.inner.spawn_y(x, z)
    }

    fn biome_at(&self, cx: i32, cz: i32) -> u32 {
        self.inner.biome_at(cx, cz)
    }

    fn biome_at_cell(&self, x: i64, y: i64, z: i64) -> u32 {
        self.inner.biome_at_cell(x, y, z)
    }
}

/// Chunk-local position of a packed delta cell.
fn delta_local_pos(section_y: i32, cell: usize) -> LocalBlockPos {
    LocalBlockPos {
        x: (cell & 15) as u8,
        y: section_y as i64 * 16 + (cell >> 8) as i64,
        z: ((cell >> 4) & 15) as u8,
    }
}

/// Pack one delta cell. `cell` is the in-section flat index (YZX order).
fn pack_delta(section_y: i32, cell: usize, block: BlockId) -> i64 {
    ((section_y as i64) << 32) | ((cell as i64) << 16) | (block.0 as i64)
}

/// Unpack a delta cell to `(section_y, cell, block)`.
fn unpack_delta(v: i64) -> (i32, usize, BlockId) {
    let section_y = (v >> 32) as i32;
    let cell = ((v >> 16) & 0xFFFF) as usize;
    let block = BlockId((v & 0xFFFF) as u16);
    (section_y, cell, block)
}

#[derive(Serialize, Deserialize, Debug)]
struct SectionNbt {
    #[serde(rename = "Y")]
    y: i8,
    block_states: BlockStatesNbt,
}

#[derive(Serialize, Deserialize, Debug)]
struct BlockStatesNbt {
    palette: Vec<PaletteEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Vec<i64>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct PaletteEntry {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Properties")]
    #[serde(skip_serializing_if = "Option::is_none")]
    properties: Option<HashMap<String, String>>,
}

// ── Bit-packing helpers ──────────────────────────────────────────────────────

/// Pack 4096 palette indices into a `Vec<i64>` using MC's bit-packing format.
///
/// `bits_per_entry` = max(4, ceil(log2(palette_len))).
/// Entries are packed sequentially into i64s with no entry spanning two longs.
fn pack_indices(indices: &[u16; 4096], palette_len: usize) -> Option<Vec<i64>> {
    if palette_len <= 1 {
        return None; // single-block section, no data array needed
    }

    let bits = bits_per_entry(palette_len);
    let entries_per_long = 64 / bits;
    let num_longs = (4096 + entries_per_long - 1) / entries_per_long;
    let mask = (1u64 << bits) - 1;

    let mut longs = vec![0i64; num_longs];
    for (i, &idx) in indices.iter().enumerate() {
        let long_idx = i / entries_per_long;
        let bit_offset = (i % entries_per_long) * bits;
        longs[long_idx] |= ((idx as u64 & mask) << bit_offset) as i64;
    }
    Some(longs)
}

/// Unpack palette indices from a `Vec<i64>` back into 4096 entries.
fn unpack_indices(data: &[i64], palette_len: usize) -> [u16; 4096] {
    let bits = bits_per_entry(palette_len);
    let entries_per_long = 64 / bits;
    let mask = (1u64 << bits) - 1;

    let mut indices = [0u16; 4096];
    for (i, idx) in indices.iter_mut().enumerate() {
        let long_idx = i / entries_per_long;
        let bit_offset = (i % entries_per_long) * bits;
        if long_idx < data.len() {
            *idx = ((data[long_idx] as u64 >> bit_offset) & mask) as u16;
        }
    }
    indices
}

/// Calculate bits per palette entry (minimum 4 per MC spec).
fn bits_per_entry(palette_len: usize) -> usize {
    let raw = if palette_len <= 1 {
        0
    } else {
        (usize::BITS - (palette_len - 1).leading_zeros()) as usize
    };
    raw.max(4) // MC minimum is 4 bits
}

// ── Section index order ──────────────────────────────────────────────────────
//
// Our engine stores sections by their section index (y >> 4) so section 3 has
// y_base = 48. The engine uses signed i64 y-coordinates (no offset).
//
// MC Anvil uses yPos = lowest_section_index and section Y = signed i8 section index.
// For our flat world the y range is roughly 0..=384, but we only save sections
// that have data. We compute min/max from the actual chunk data.

// ── Save ─────────────────────────────────────────────────────────────────────

/// Save only dirty (modified) chunks to Anvil region files under
/// `<dir>/region/`, **delta-encoded** (Phase 6c): each chunk stores only
/// the cells that differ from the procedurally regenerated baseline.
///
/// Existing region files are opened and updated in-place; new region files are
/// created as needed. Every chunk is stamped with `gen_fp` (the current
/// generator fingerprint). Returns the number of chunks written.
///
/// `worldgen` MUST be the **base** generator, never a [`DeltaOverlayGen`]:
/// the diff has to be computed against the pristine procedural baseline.
/// Diffing against an overlay would yield edits-since-last-delta, which
/// would then REPLACE the stored full delta and silently lose history.
pub fn save_world(
    world: &World,
    dir: &Path,
    gen_fp: u64,
    worldgen: &dyn crate::worldgen::WorldGen,
    deltas: Option<&DeltaStore>,
) -> Result<usize> {
    let dirty = world.take_dirty_chunks();
    if dirty.is_empty() {
        tracing::info!("World save: nothing to save (no dirty chunks)");
        return Ok(0);
    }

    let start = Instant::now();
    let region_dir = dir.join("region");
    fs::create_dir_all(&region_dir)?;

    // Serialize dirty chunks and group by region.
    let mut region_chunks: HashMap<(i32, i32), Vec<(ChunkPos, Vec<u8>)>> = HashMap::new();

    for pos in &dirty {
        let Some(chunk_ref) = world.get_chunk(pos) else {
            continue; // Chunk was removed between dirty-mark and save.
        };
        let nbt = chunk_to_delta_nbt(*pos, &chunk_ref, gen_fp, worldgen);
        drop(chunk_ref); // Release DashMap ref before region I/O.

        // Refresh the live delta store: after this save the chunk is
        // clean AND its regeneration recipe is current → evictable.
        if let Some(store) = deltas {
            if let Some(delta) = &nbt.delta {
                store.insert(*pos, std::sync::Arc::from(delta.as_slice()));
            }
        }
        let nbt_bytes = fastnbt::to_bytes(&nbt)
            .with_context(|| format!("serializing chunk ({}, {})", pos.x, pos.z))?;

        let rx = pos.x.div_euclid(32);
        let rz = pos.z.div_euclid(32);
        region_chunks
            .entry((rx, rz))
            .or_default()
            .push((*pos, nbt_bytes));
    }

    let mut total_chunks = 0usize;

    for ((rx, rz), chunks) in &region_chunks {
        let path = region_dir.join(format!("r.{}.{}.mca", rx, rz));

        // Open existing region file or create a new one.
        let mut region = if path.exists() {
            let file_bytes = fs::read(&path)
                .with_context(|| format!("reading region r.{}.{}", rx, rz))?;
            fastanvil::Region::from_stream(Cursor::new(file_bytes))
                .with_context(|| format!("parsing region r.{}.{}", rx, rz))?
        } else {
            fastanvil::Region::new(Cursor::new(Vec::new()))
                .with_context(|| format!("creating region r.{}.{}", rx, rz))?
        };

        for (pos, nbt_bytes) in chunks {
            let local_x = pos.x.rem_euclid(32) as usize;
            let local_z = pos.z.rem_euclid(32) as usize;
            region
                .write_chunk(local_x, local_z, nbt_bytes)
                .with_context(|| format!("writing chunk ({}, {})", pos.x, pos.z))?;
            total_chunks += 1;
        }

        // Flush: recover the cursor and write to disk.
        let mut cursor = region.into_inner()?;
        let len = cursor.stream_position()?;
        let data = cursor.into_inner();
        fs::write(&path, &data[..len as usize])?;
    }

    let elapsed = start.elapsed();
    tracing::info!(
        "World saved: {} dirty chunks across {} regions ({:.2?})",
        total_chunks,
        region_chunks.len(),
        elapsed,
    );
    Ok(total_chunks)
}

/// Build the delta NBT for a chunk: regenerate the baseline from the
/// worldgen pipeline and record only the differing cells.
///
/// The baseline is generated standalone (fresh empty world), so spill-in
/// blocks from neighbouring chunks' features (tree canopies crossing the
/// border) appear in the delta. That's correct: they re-apply on load
/// regardless of which neighbours have generated yet.
fn chunk_to_delta_nbt(
    pos: ChunkPos,
    chunk: &Chunk,
    gen_fp: u64,
    worldgen: &dyn crate::worldgen::WorldGen,
) -> ChunkNbt {
    let baseline = worldgen.generate_chunk(pos.x, pos.z, &World::new());

    // Union of section indices present on either side: a section missing
    // entirely on one side still diffs cell-by-cell against air.
    let mut section_indices: Vec<i32> = chunk
        .sections()
        .map(|(&i, _)| i)
        .chain(baseline.sections().map(|(&i, _)| i))
        .collect();
    section_indices.sort_unstable();
    section_indices.dedup();

    let mut delta = Vec::new();
    for si in section_indices {
        let live = chunk.section(si);
        let base = baseline.section(si);
        for cell in 0..4096usize {
            let live_block = live.map_or(BlockId::AIR, |s| s.get_by_index(cell));
            let base_block = base.map_or(BlockId::AIR, |s| s.get_by_index(cell));
            if live_block != base_block {
                delta.push(pack_delta(si, cell, live_block));
            }
        }
    }

    ChunkNbt {
        data_version: DATA_VERSION,
        x_pos: pos.x,
        z_pos: pos.z,
        y_pos: 0,
        sections: Vec::new(),
        status: "minecraft:full".into(),
        gen_fp: Some(gen_fp as i64),
        delta: Some(delta),
    }
}

/// Convert an engine `Chunk` to the full-section Anvil NBT representation.
/// Legacy format — current saves are delta-encoded; this is kept for
/// vanilla-tool export and for tests exercising the legacy load path.
#[cfg_attr(not(test), allow(dead_code))]
fn chunk_to_nbt(pos: ChunkPos, chunk: &Chunk, gen_fp: u64) -> ChunkNbt {
    let mut sections = Vec::new();

    for (&section_idx, section) in chunk.sections() {
        let nbt_section = section_to_nbt(section_idx, section);
        sections.push(nbt_section);
    }

    // Sort sections by Y for tidiness.
    sections.sort_by_key(|s| s.y);

    // yPos = lowest section index in this chunk.
    let y_pos = sections.first().map(|s| s.y as i32).unwrap_or(0);

    ChunkNbt {
        data_version: DATA_VERSION,
        x_pos: pos.x,
        z_pos: pos.z,
        y_pos,
        sections,
        status: "minecraft:full".into(),
        gen_fp: Some(gen_fp as i64),
        delta: None,
    }
}

/// Convert a single engine `ChunkSection` to the Anvil NBT section format.
fn section_to_nbt(section_idx: i32, section: &ChunkSection) -> SectionNbt {
    // Materialize the paletted section once (cheap index reads).
    let mut blocks = [BlockId::AIR; 4096];
    for (i, b) in blocks.iter_mut().enumerate() {
        *b = section.get_by_index(i);
    }

    // Build palette: map each unique BlockId to a palette index. (Built
    // fresh rather than reusing the section's own palette, which may
    // contain stale entries for since-overwritten blocks.)
    let mut palette_map: HashMap<BlockId, u16> = HashMap::new();
    let mut palette_entries: Vec<PaletteEntry> = Vec::new();

    // MC section block order is YZX (y varies fastest? Actually it's:
    // index = y*16*16 + z*16 + x). Our engine uses XZY order:
    // index = y*16*16 + z*16 + x ... wait let me re-check.
    //
    // Engine: index(x,y,z) = y * 256 + z * 16 + x  (XZY means x varies fastest)
    //   Actually: y * SECTION_SIZE * SECTION_SIZE + z * SECTION_SIZE + x
    //   = y*256 + z*16 + x. This IS YZX order (y outermost, x innermost).
    //
    // MC Anvil: index = ((y*16 + z)*16 + x) = y*256 + z*16 + x.
    //   This is the SAME order! Great, no remapping needed.

    let mut indices = [0u16; 4096];

    for (i, &block_id) in blocks.iter().enumerate() {
        let palette_idx = if let Some(&idx) = palette_map.get(&block_id) {
            idx
        } else {
            let idx = palette_entries.len() as u16;
            palette_entries.push(block_id_to_palette_entry(block_id));
            palette_map.insert(block_id, idx);
            idx
        };
        indices[i] = palette_idx;
    }

    let data = pack_indices(&indices, palette_entries.len());

    SectionNbt {
        y: section_idx as i8,
        block_states: BlockStatesNbt {
            palette: palette_entries,
            data,
        },
    }
}

// ── Load ─────────────────────────────────────────────────────────────────────

/// Load saved chunks from Anvil region files into an existing world.
///
/// **Delta chunks** (Phase 6c, the current format): the chunk is
/// generated from the current worldgen if not already present, then the
/// saved cell diffs are applied on top. A generator-fingerprint mismatch
/// is *fine* — the edits re-apply onto the new terrain, so block
/// modifications survive preset/seed changes (logged for visibility).
///
/// **Legacy full-section chunks**: loaded verbatim only when the
/// fingerprint matches; otherwise skipped (stale-generator terrain
/// stitched against new terrain produces hard seams — the bug this
/// pipeline originally shipped with).
///
/// Chunks loaded either way are **not** marked dirty.
/// Returns the number of chunks loaded (0 if no save directory exists).
///
/// When a `deltas` store is supplied, every loaded delta is also recorded
/// there so later regenerations (lazy loads, post-eviction) re-apply it.
pub fn load_into(
    world: &World,
    dir: &Path,
    gen_fp: u64,
    worldgen: &dyn crate::worldgen::WorldGen,
    deltas: Option<&DeltaStore>,
) -> Result<usize> {
    let region_dir = dir.join("region");
    if !region_dir.is_dir() {
        return Ok(0);
    }

    let start = Instant::now();

    // Force the reverse lookup table to initialize before we start loading.
    let _ = &*BLOCK_LOOKUP;

    let mut total_chunks = 0usize;
    let mut stale_chunks = 0usize;
    let mut migrated_chunks = 0usize;
    let mut region_count = 0usize;

    for entry in fs::read_dir(&region_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".mca") {
            continue;
        }

        // Parse region coordinates from filename: r.X.Z.mca
        let parts: Vec<&str> = name.trim_end_matches(".mca").split('.').collect();
        if parts.len() != 3 || parts[0] != "r" {
            tracing::warn!("Skipping unexpected file in region dir: {}", name);
            continue;
        }
        let rx: i32 = parts[1].parse().unwrap_or(0);
        let rz: i32 = parts[2].parse().unwrap_or(0);

        let file = fs::File::open(&path)
            .with_context(|| format!("opening region file {}", path.display()))?;
        let mut region = fastanvil::Region::from_stream(file)
            .with_context(|| format!("parsing region file {}", path.display()))?;

        for x in 0..32usize {
            for z in 0..32usize {
                let Some(nbt_bytes) = region
                    .read_chunk(x, z)
                    .with_context(|| format!("reading chunk ({}, {}) from r.{}.{}", x, z, rx, rz))?
                else {
                    continue;
                };

                let chunk_nbt: ChunkNbt = fastnbt::from_bytes(&nbt_bytes)
                    .with_context(|| {
                        format!(
                            "deserializing chunk ({}, {}) from r.{}.{}",
                            x, z, rx, rz
                        )
                    })?;

                let chunk_pos = ChunkPos::new(chunk_nbt.x_pos, chunk_nbt.z_pos);

                if let Some(delta) = &chunk_nbt.delta {
                    // Delta chunk: regenerate baseline (if needed), apply.
                    if chunk_nbt.gen_fp != Some(gen_fp as i64) {
                        migrated_chunks += 1;
                    }
                    // Record in the live store FIRST so an overlay-backed
                    // `ensure_generated` already applies it; the manual
                    // apply below covers already-present chunks and is
                    // idempotent when both run.
                    if let Some(store) = deltas {
                        store.insert(chunk_pos, std::sync::Arc::from(delta.as_slice()));
                    }
                    worldgen.ensure_generated(world, chunk_pos.x, chunk_pos.z);
                    if let Some(mut chunk) = world.get_chunk_mut(&chunk_pos) {
                        for &packed in delta {
                            let (sy, cell, block) = unpack_delta(packed);
                            chunk.set_block(delta_local_pos(sy, cell), block);
                        }
                    }
                    total_chunks += 1;
                    continue;
                }

                // Legacy full-section chunk: verbatim load only under the
                // exact generator that produced it.
                if chunk_nbt.gen_fp != Some(gen_fp as i64) {
                    stale_chunks += 1;
                    continue;
                }
                let chunk = nbt_to_chunk(&chunk_nbt);
                world.insert_chunk(chunk_pos, chunk);
                total_chunks += 1;
            }
        }
        region_count += 1;
    }

    if migrated_chunks > 0 {
        tracing::info!(
            "Re-applied {} delta chunks saved under an older generator version — \
             block edits preserved on top of the regenerated terrain",
            migrated_chunks,
        );
    }
    if stale_chunks > 0 {
        tracing::warn!(
            "Skipped {} legacy full-section chunks from an older generator version; \
             their terrain will regenerate and any block modifications they \
             contained are discarded (re-save under the current format to migrate)",
            stale_chunks,
        );
    }
    if total_chunks > 0 {
        let elapsed = start.elapsed();
        tracing::info!(
            "Loaded {} saved chunks from {} regions ({:.2?})",
            total_chunks,
            region_count,
            elapsed,
        );
    }
    Ok(total_chunks)
}

/// Convert Anvil NBT chunk data back into an engine `Chunk`.
fn nbt_to_chunk(nbt: &ChunkNbt) -> Chunk {
    let mut chunk = Chunk::new();

    for section_nbt in &nbt.sections {
        let section_idx = section_nbt.y as i32;
        let palette = &section_nbt.block_states.palette;

        if palette.is_empty() {
            continue;
        }

        // Resolve palette to BlockIds.
        let resolved_palette: Vec<BlockId> =
            palette.iter().map(palette_entry_to_block_id).collect();

        // If single-block section (palette length 1, no data array), fill uniformly.
        let block_ids: [BlockId; 4096] = if palette.len() == 1 || section_nbt.block_states.data.is_none() {
            [resolved_palette[0]; 4096]
        } else {
            let data = section_nbt.block_states.data.as_ref().unwrap();
            let indices = unpack_indices(data, palette.len());
            let mut ids = [BlockId::AIR; 4096];
            for (i, &idx) in indices.iter().enumerate() {
                ids[i] = resolved_palette
                    .get(idx as usize)
                    .copied()
                    .unwrap_or(BlockId::AIR);
            }
            ids
        };

        // Skip all-air sections.
        if block_ids.iter().all(|&b| b == BlockId::AIR) {
            continue;
        }

        // Write blocks into the chunk using set_block.
        let y_base = (section_idx as i64) * 16;
        for y in 0..16u8 {
            for z in 0..16u8 {
                for x in 0..16u8 {
                    let idx = (y as usize) * 256 + (z as usize) * 16 + (x as usize);
                    let block = block_ids[idx];
                    if block != BlockId::AIR {
                        chunk.set_block(
                            LocalBlockPos {
                                x,
                                y: y_base + y as i64,
                                z,
                            },
                            block,
                        );
                    }
                }
            }
        }
    }

    chunk
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worldgen::WorldGen;

    /// Generates empty chunks: the delta baseline is "nothing", so every
    /// non-air cell of a saved chunk lands in the delta — making delta
    /// round-trips behave exactly like the old full-chunk format.
    struct EmptyGen;
    impl WorldGen for EmptyGen {
        fn generate_chunk(&self, _cx: i32, _cz: i32, _world: &World) -> Chunk {
            Chunk::new()
        }
        fn spawn_y(&self, _x: i64, _z: i64) -> f64 {
            0.0
        }
    }

    /// Fills y=0..4 of every chunk with one block — a stand-in for "a
    /// generator version" so tests can simulate preset changes.
    struct FillGen(BlockId);
    impl WorldGen for FillGen {
        fn generate_chunk(&self, _cx: i32, _cz: i32, _world: &World) -> Chunk {
            let mut chunk = Chunk::new();
            for x in 0..16u8 {
                for z in 0..16u8 {
                    for y in 0..4i64 {
                        chunk.set_block(LocalBlockPos { x, y, z }, self.0);
                    }
                }
            }
            chunk
        }
        fn spawn_y(&self, _x: i64, _z: i64) -> f64 {
            5.0
        }
    }

    #[test]
    fn test_delta_pack_roundtrip() {
        for (sy, cell, block) in [
            (-4i32, 0usize, BlockId::AIR),
            (0, 4095, BlockId::new(1)),
            (19, 2048, BlockId::new(0xFFFF)),
            (-1, 17, BlockId::new(118)),
        ] {
            assert_eq!(unpack_delta(pack_delta(sy, cell, block)), (sy, cell, block));
        }
    }

    #[test]
    fn test_bits_per_entry() {
        assert_eq!(bits_per_entry(1), 4); // min 4
        assert_eq!(bits_per_entry(2), 4); // ceil(log2(2)) = 1, clamped to 4
        assert_eq!(bits_per_entry(16), 4); // ceil(log2(16)) = 4
        assert_eq!(bits_per_entry(17), 5); // ceil(log2(17)) = 5
        assert_eq!(bits_per_entry(256), 8);
    }

    #[test]
    fn test_pack_unpack_roundtrip() {
        let mut indices = [0u16; 4096];
        for (i, idx) in indices.iter_mut().enumerate() {
            *idx = (i % 7) as u16; // 7-entry palette
        }
        let packed = pack_indices(&indices, 7).unwrap();
        let unpacked = unpack_indices(&packed, 7);
        assert_eq!(indices, unpacked);
    }

    #[test]
    fn test_single_block_section_no_data() {
        let result = pack_indices(&[0u16; 4096], 1);
        assert!(result.is_none());
    }

    #[test]
    fn test_palette_entry_roundtrip() {
        // Test a simple block.
        let id = BlockId(1); // stone
        let entry = block_id_to_palette_entry(id);
        assert_eq!(entry.name, "minecraft:stone");
        assert!(entry.properties.is_none());
        let back = palette_entry_to_block_id(&entry);
        assert_eq!(back, id);
    }

    #[test]
    fn test_palette_entry_air() {
        let entry = block_id_to_palette_entry(BlockId::AIR);
        assert_eq!(entry.name, "minecraft:air");
        let back = palette_entry_to_block_id(&entry);
        assert_eq!(back, BlockId::AIR);
    }

    #[test]
    fn test_save_load_roundtrip() {
        use ultimate_engine::world::position::BlockPos;

        let world = World::new();

        // Build a small test chunk via set_block (which marks dirty).
        for x in 0..16i64 {
            for z in 0..16i64 {
                world.set_block(BlockPos::new(x, 60, z), crate::block::BEDROCK);
                for y in 61..=63i64 {
                    world.set_block(BlockPos::new(x, y, z), crate::block::STONE);
                }
                world.set_block(BlockPos::new(x, 64, z), crate::block::DIRT);
            }
        }
        assert_eq!(world.dirty_count(), 1); // one chunk dirty

        // Save to a temp directory.
        let tmp = std::env::temp_dir().join("ultimate_mc_test_persistence");
        let _ = fs::remove_dir_all(&tmp);
        let saved = save_world(&world, &tmp, 0xFEED, &EmptyGen, None).unwrap();
        assert_eq!(saved, 1); // only the one dirty chunk

        // Verify region file exists.
        assert!(tmp.join("region/r.0.0.mca").exists());

        // Load back into a fresh world (simulating: generate base, then overlay).
        let loaded = World::new();
        let n = load_into(&loaded, &tmp, 0xFEED, &EmptyGen, None).unwrap();
        assert_eq!(n, 1);
        assert_eq!(loaded.chunk_count(), 1);

        // Verify blocks match.
        for x in 0..16i64 {
            for z in 0..16i64 {
                assert_eq!(
                    loaded.get_block(BlockPos::new(x, 60, z)),
                    crate::block::BEDROCK,
                    "bedrock mismatch at ({}, 60, {})",
                    x, z,
                );
                for y in 61..=63i64 {
                    assert_eq!(
                        loaded.get_block(BlockPos::new(x, y, z)),
                        crate::block::STONE,
                        "stone mismatch at ({}, {}, {})",
                        x, y, z,
                    );
                }
                assert_eq!(
                    loaded.get_block(BlockPos::new(x, 64, z)),
                    crate::block::DIRT,
                    "dirt mismatch at ({}, 64, {})",
                    x, z,
                );
                // Air above should be air.
                assert_eq!(
                    loaded.get_block(BlockPos::new(x, 65, z)),
                    BlockId::AIR,
                );
            }
        }

        // After loading, dirty set should be empty (load doesn't dirty).
        assert_eq!(loaded.dirty_count(), 0);

        // Saving again should write 0 chunks (nothing dirty).
        let saved_again = save_world(&loaded, &tmp, 0xFEED, &EmptyGen, None).unwrap();
        assert_eq!(saved_again, 0);

        // Cleanup.
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_incremental_save() {
        use ultimate_engine::world::position::BlockPos;

        let world = World::new();

        // Two chunks: (0,0) and (1,0).
        world.set_block(BlockPos::new(0, 60, 0), crate::block::STONE);
        world.set_block(BlockPos::new(16, 60, 0), crate::block::DIRT);
        assert_eq!(world.dirty_count(), 2);

        let tmp = std::env::temp_dir().join("ultimate_mc_test_incremental");
        let _ = fs::remove_dir_all(&tmp);

        // First save: both chunks written.
        let saved = save_world(&world, &tmp, 0xFEED, &EmptyGen, None).unwrap();
        assert_eq!(saved, 2);
        assert_eq!(world.dirty_count(), 0);

        // Modify only chunk (0,0).
        world.set_block(BlockPos::new(1, 60, 0), crate::block::BEDROCK);
        assert_eq!(world.dirty_count(), 1);

        // Second save: only 1 chunk.
        let saved = save_world(&world, &tmp, 0xFEED, &EmptyGen, None).unwrap();
        assert_eq!(saved, 1);

        // Load into a fresh world and verify both chunks persisted.
        let loaded = World::new();
        let n = load_into(&loaded, &tmp, 0xFEED, &EmptyGen, None).unwrap();
        assert_eq!(n, 2);
        assert_eq!(loaded.chunk_count(), 2);
        assert_eq!(loaded.get_block(BlockPos::new(0, 60, 0)), crate::block::STONE);
        assert_eq!(loaded.get_block(BlockPos::new(1, 60, 0)), crate::block::BEDROCK);
        assert_eq!(loaded.get_block(BlockPos::new(16, 60, 0)), crate::block::DIRT);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_overlay_on_generated_world() {
        use ultimate_engine::world::position::BlockPos;

        // Simulate the real startup flow: generate base, modify, save, restart.
        let world = World::new();

        // "Generate" a base: fill chunk (0,0) with stone at y=60.
        for x in 0..16i64 {
            for z in 0..16i64 {
                // Use insert_chunk path (not dirty).
                world.set_block(BlockPos::new(x, 60, z), crate::block::STONE);
            }
        }
        // Clear dirty flags to simulate insert_chunk-based generation.
        world.take_dirty_chunks();

        // Player places a diamond block at (5, 61, 5).
        let diamond = BlockId(azalea_block::BlockState::from(
            azalea_registry::builtin::BlockKind::DiamondBlock,
        ).id());
        world.set_block(BlockPos::new(5, 61, 5), diamond);
        assert_eq!(world.dirty_count(), 1);

        let tmp = std::env::temp_dir().join("ultimate_mc_test_overlay");
        let _ = fs::remove_dir_all(&tmp);
        save_world(&world, &tmp, 0xFEED, &EmptyGen, None).unwrap();

        // "Restart": generate base world again, then overlay saved chunks.
        let world2 = World::new();
        for x in 0..16i64 {
            for z in 0..16i64 {
                world2.set_block(BlockPos::new(x, 60, z), crate::block::STONE);
            }
        }
        world2.take_dirty_chunks(); // clear generation dirt

        load_into(&world2, &tmp, 0xFEED, &EmptyGen, None).unwrap();

        // The saved chunk overwrites the generated one -- diamond block is there.
        assert_eq!(world2.get_block(BlockPos::new(5, 61, 5)), diamond);
        // The base stone at y=60 is also present (from the saved chunk, which
        // had stone + diamond).
        assert_eq!(world2.get_block(BlockPos::new(0, 60, 0)), crate::block::STONE);
        // Nothing dirty after load.
        assert_eq!(world2.dirty_count(), 0);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_delta_chunks_survive_generator_change() {
        use ultimate_engine::world::position::BlockPos;

        // THE delta-persistence acceptance test: a block edit saved under
        // generator A must survive a load under generator B — re-applied
        // onto B's freshly generated terrain instead of being discarded
        // (the fingerprint-skip behaviour full chunks had).
        let gen_a = FillGen(crate::block::STONE);
        let gen_b = FillGen(crate::block::DIRT);

        // World generated by A, plus one player edit.
        let world = World::new();
        gen_a.ensure_generated(&world, 0, 0);
        let diamond = BlockId(azalea_block::BlockState::from(
            azalea_registry::builtin::BlockKind::DiamondBlock,
        ).id());
        world.set_block(BlockPos::new(5, 10, 5), diamond);
        assert_eq!(world.dirty_count(), 1);

        let tmp = std::env::temp_dir().join("ultimate_mc_test_delta_migrate");
        let _ = fs::remove_dir_all(&tmp);
        save_world(&world, &tmp, 0xAAAA, &gen_a, None).unwrap();

        // "Upgrade the generator": load under B with a different fingerprint.
        let world2 = World::new();
        let n = load_into(&world2, &tmp, 0xBBBB, &gen_b, None).unwrap();
        assert_eq!(n, 1, "delta chunk must load despite the fingerprint change");

        // The edit survived...
        assert_eq!(world2.get_block(BlockPos::new(5, 10, 5)), diamond);
        // ...on top of generator B's terrain, not A's.
        assert_eq!(world2.get_block(BlockPos::new(0, 0, 0)), crate::block::DIRT);
        assert_eq!(world2.get_block(BlockPos::new(8, 3, 8)), crate::block::DIRT);
        assert_eq!(world2.dirty_count(), 0, "loads must not dirty");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_delta_is_minimal_against_matching_baseline() {
        use ultimate_engine::world::position::BlockPos;

        // When the live chunk equals its baseline except for one edit,
        // the delta must contain exactly that one cell.
        let generator = FillGen(crate::block::STONE);
        let world = World::new();
        generator.ensure_generated(&world, 0, 0);
        world.set_block(BlockPos::new(7, 2, 7), crate::block::SAND);

        let chunk_ref = world.get_chunk(&ChunkPos::new(0, 0)).unwrap();
        let nbt = chunk_to_delta_nbt(ChunkPos::new(0, 0), &chunk_ref, 1, &generator);
        let delta = nbt.delta.expect("delta format");
        assert_eq!(delta.len(), 1, "one edit → one delta cell, got {}", delta.len());
        let (sy, cell, block) = unpack_delta(delta[0]);
        assert_eq!((sy, block), (0, crate::block::SAND));
        assert_eq!(cell, 2 * 256 + 7 * 16 + 7);
    }

    #[test]
    fn test_eviction_roundtrip_through_overlay() {
        use ultimate_engine::world::position::BlockPos;
        use crate::worldgen::WorldGen as _;

        // THE eviction acceptance test: save an edited chunk (populating
        // the delta store), evict it, regenerate through the overlay —
        // the chunk must come back bit-for-bit, edit included.
        let base: std::sync::Arc<dyn crate::worldgen::WorldGen> =
            std::sync::Arc::new(FillGen(crate::block::STONE));
        let store = new_delta_store();
        let overlay = DeltaOverlayGen::new(std::sync::Arc::clone(&base), std::sync::Arc::clone(&store));

        let world = World::new();
        overlay.ensure_generated(&world, 0, 0);
        let edit_pos = BlockPos::new(9, 12, 9);
        world.set_block(edit_pos, crate::block::SAND);

        let tmp = std::env::temp_dir().join("ultimate_mc_test_evict_rt");
        let _ = fs::remove_dir_all(&tmp);
        // Save diffs against the BASE generator, refreshing the store.
        save_world(&world, &tmp, 7, &*base, Some(&store)).unwrap();
        assert!(store.contains_key(&ChunkPos::new(0, 0)), "save must populate the store");
        assert!(!world.is_dirty(ChunkPos::new(0, 0)), "saved chunk is clean");

        // Evict, then regenerate through the overlay (the lazy-load path).
        assert!(world.remove_chunk(ChunkPos::new(0, 0)));
        assert!(!world.has_chunk(ChunkPos::new(0, 0)));
        overlay.ensure_generated(&world, 0, 0);

        assert_eq!(world.get_block(edit_pos), crate::block::SAND, "edit survives eviction");
        assert_eq!(world.get_block(BlockPos::new(0, 0, 0)), crate::block::STONE, "terrain intact");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_legacy_full_chunks_skip_on_fingerprint_mismatch() {
        use ultimate_engine::world::position::BlockPos;
        use std::io::Seek;

        // Hand-craft a legacy full-section chunk (the pre-6c format) and
        // verify the loader still applies the old rule: verbatim load on
        // fingerprint match, skip on mismatch.
        let world = World::new();
        world.set_block(BlockPos::new(3, 70, 3), crate::block::STONE);
        let chunk_ref = world.get_chunk(&ChunkPos::new(0, 0)).unwrap();
        let legacy = chunk_to_nbt(ChunkPos::new(0, 0), &chunk_ref, 0xAAAA);
        drop(chunk_ref);
        assert!(legacy.delta.is_none());

        let tmp = std::env::temp_dir().join("ultimate_mc_test_legacy_fp");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("region")).unwrap();
        let bytes = fastnbt::to_bytes(&legacy).unwrap();
        let mut region = fastanvil::Region::new(Cursor::new(Vec::new())).unwrap();
        region.write_chunk(0, 0, &bytes).unwrap();
        let mut cursor = region.into_inner().unwrap();
        let len = cursor.stream_position().unwrap();
        fs::write(tmp.join("region/r.0.0.mca"), &cursor.into_inner()[..len as usize]).unwrap();

        // Mismatch: skipped entirely.
        let loaded = World::new();
        let n = load_into(&loaded, &tmp, 0xBBBB, &EmptyGen, None).unwrap();
        assert_eq!(n, 0, "legacy chunk with stale fingerprint must be skipped");
        assert_eq!(loaded.chunk_count(), 0);

        // Match: verbatim load.
        let loaded = World::new();
        let n = load_into(&loaded, &tmp, 0xAAAA, &EmptyGen, None).unwrap();
        assert_eq!(n, 1);
        assert_eq!(loaded.get_block(BlockPos::new(3, 70, 3)), crate::block::STONE);

        let _ = fs::remove_dir_all(&tmp);
    }
}
