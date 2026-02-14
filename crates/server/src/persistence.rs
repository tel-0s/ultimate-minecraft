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

/// Save only dirty (modified) chunks to Anvil region files under `<dir>/region/`.
///
/// Existing region files are opened and updated in-place; new region files are
/// created as needed. Returns the number of chunks written.
pub fn save_world(world: &World, dir: &Path) -> Result<usize> {
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
        let nbt = chunk_to_nbt(*pos, &*chunk_ref);
        drop(chunk_ref); // Release DashMap ref before serialization.
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

/// Convert an engine `Chunk` to the Anvil NBT representation.
fn chunk_to_nbt(pos: ChunkPos, chunk: &Chunk) -> ChunkNbt {
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
    }
}

/// Convert a single engine `ChunkSection` to the Anvil NBT section format.
fn section_to_nbt(section_idx: i32, section: &ChunkSection) -> SectionNbt {
    let blocks = section.blocks();

    // Build palette: map each unique BlockId to a palette index.
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

/// Load a world from Anvil region files under `<dir>/region/`.
///
/// Returns `None` if the region directory does not exist or contains no `.mca` files.
pub fn load_world(dir: &Path) -> Result<Option<World>> {
    let region_dir = dir.join("region");
    if !region_dir.is_dir() {
        return Ok(None);
    }

    let start = Instant::now();

    // Force the reverse lookup table to initialize before we start loading.
    let _ = &*BLOCK_LOOKUP;

    let world = World::new();
    let mut total_chunks = 0usize;
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
                let chunk = nbt_to_chunk(&chunk_nbt);
                world.insert_chunk(chunk_pos, chunk);
                total_chunks += 1;
            }
        }
        region_count += 1;
    }

    if total_chunks == 0 {
        return Ok(None);
    }

    let elapsed = start.elapsed();
    tracing::info!(
        "World loaded: {} chunks from {} regions ({:.2?})",
        total_chunks,
        region_count,
        elapsed,
    );
    Ok(Some(world))
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
        let saved = save_world(&world, &tmp).unwrap();
        assert_eq!(saved, 1); // only the one dirty chunk

        // Verify region file exists.
        assert!(tmp.join("region/r.0.0.mca").exists());

        // Load back.
        let loaded = load_world(&tmp).unwrap().expect("should load world");
        assert_eq!(loaded.chunk_count(), 1);

        // Verify blocks match.
        for x in 0..16i64 {
            for z in 0..16i64 {
                assert_eq!(
                    loaded.get_block(BlockPos::new(x, 60, z)),
                    BlockId(crate::block::BEDROCK.0),
                    "bedrock mismatch at ({}, 60, {})",
                    x, z,
                );
                for y in 61..=63i64 {
                    assert_eq!(
                        loaded.get_block(BlockPos::new(x, y, z)),
                        BlockId(crate::block::STONE.0),
                        "stone mismatch at ({}, {}, {})",
                        x, y, z,
                    );
                }
                assert_eq!(
                    loaded.get_block(BlockPos::new(x, 64, z)),
                    BlockId(crate::block::DIRT.0),
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
        let saved_again = save_world(&loaded, &tmp).unwrap();
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
        let saved = save_world(&world, &tmp).unwrap();
        assert_eq!(saved, 2);
        assert_eq!(world.dirty_count(), 0);

        // Modify only chunk (0,0).
        world.set_block(BlockPos::new(1, 60, 0), crate::block::BEDROCK);
        assert_eq!(world.dirty_count(), 1);

        // Second save: only 1 chunk.
        let saved = save_world(&world, &tmp).unwrap();
        assert_eq!(saved, 1);

        // Load and verify both chunks are present (the untouched one persisted from first save).
        let loaded = load_world(&tmp).unwrap().expect("should load");
        assert_eq!(loaded.chunk_count(), 2);
        assert_eq!(loaded.get_block(BlockPos::new(0, 60, 0)), crate::block::STONE);
        assert_eq!(loaded.get_block(BlockPos::new(1, 60, 0)), crate::block::BEDROCK);
        assert_eq!(loaded.get_block(BlockPos::new(16, 60, 0)), crate::block::DIRT);

        let _ = fs::remove_dir_all(&tmp);
    }
}
