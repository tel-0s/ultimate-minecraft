//! Chunk and light streaming: view tracking, deferred chunk queue
//! helpers, chunk/light packet encoding (1.21.5+ wire format).

use std::collections::{HashSet, VecDeque};

use anyhow::Result;
use azalea_buf::AzBuf;
use azalea_core::bitset::BitSet;
use azalea_protocol::packets::game::{
    ClientboundGamePacket, ClientboundSetChunkCacheCenter,
    ClientboundForgetLevelChunk,
    ClientboundChunkBatchStart, ClientboundChunkBatchFinished,
};
use azalea_protocol::packets::Packet;
use azalea_protocol::write::write_packet;
use tokio::io::AsyncWrite;
use ultimate_engine::world::World;

use crate::event_bus::{self};
use crate::worldgen::WorldGen;

// ── Dynamic chunk loading ────────────────────────────────────────────────

/// Check if the player has crossed a chunk boundary, and if so, queue new
/// chunks for deferred loading and immediately unload old ones.
///
/// New chunks are sorted by Chebyshev distance from the player (nearest first)
/// and added to `chunk_send_queue`. The main loop drains this queue
/// progressively so the event loop stays responsive during fast movement.
pub(crate) async fn update_loaded_chunks<W: AsyncWrite + Unpin + Send>(
    write: &mut W,
    compression: Option<u32>,
    cipher: &mut Option<azalea_crypto::Aes128CfbEnc>,
    world: &World,
    worldgen: &dyn WorldGen,
    player_x: f64,
    player_z: f64,
    view_distance: i32,
    immediate_radius: i32,
    current_chunk_x: &mut i32,
    current_chunk_z: &mut i32,
    loaded_chunks: &mut HashSet<(i32, i32)>,
    sent_to_client: &mut HashSet<(i32, i32)>,
    chunk_send_queue: &mut VecDeque<(i32, i32)>,
) -> Result<()> {
    let new_cx = (player_x.floor() as i32) >> 4;
    let new_cz = (player_z.floor() as i32) >> 4;

    // No chunk boundary crossed -- nothing to do.
    if new_cx == *current_chunk_x && new_cz == *current_chunk_z {
        return Ok(());
    }

    *current_chunk_x = new_cx;
    *current_chunk_z = new_cz;

    // Compute the desired set of loaded chunks.
    let desired: HashSet<(i32, i32)> = {
        let mut s = HashSet::with_capacity(((2 * view_distance + 1) * (2 * view_distance + 1)) as usize);
        for cx in (new_cx - view_distance)..=(new_cx + view_distance) {
            for cz in (new_cz - view_distance)..=(new_cz + view_distance) {
                s.insert((cx, cz));
            }
        }
        s
    };

    // Unload chunks that are no longer in range.
    //
    // azalea-core 0.15's `ChunkPos` serialization is buggy for negative X:
    //   (pos.x as u64) | ((pos.z as u64) << 32)
    // sign-extends a negative i32 across all 64 bits, which then OR's with
    // z and loses z entirely. Concretely: ForgetLevelChunk for (-4, 5)
    // serializes the same as (-4, -1), so eight of every nine forgets at
    // cx=-4 reach the client as (-4, -1), and the other chunks stay in the
    // client's cache outside the view distance — interactable but not
    // rendered. Build the packet manually with correct bit handling.
    let to_unload: Vec<(i32, i32)> = loaded_chunks.difference(&desired).copied().collect();
    for (cx, cz) in &to_unload {
        send_forget_level_chunk(write, compression, cipher, *cx, *cz).await?;
        loaded_chunks.remove(&(*cx, *cz));
        sent_to_client.remove(&(*cx, *cz));
    }

    // Remove stale entries from the queue.
    chunk_send_queue.retain(|pos| desired.contains(pos));

    // Collect new chunks to load, sorted by distance (nearest first).
    let mut to_load: Vec<(i32, i32)> = desired
        .difference(loaded_chunks)
        .copied()
        .collect();
    to_load.sort_by_key(|(cx, cz)| {
        let dx = (*cx - new_cx).abs();
        let dz = (*cz - new_cz).abs();
        dx.max(dz) // Chebyshev distance
    });

    // Inner-ring chunks (Chebyshev ≤ `immediate_radius`) are sent
    // SYNCHRONOUSLY before the cache-center update; outer-ring chunks
    // queue and stream in over the next few main-loop iterations.
    // The radius is config-driven (`network.immediate_radius` in
    // server.yaml; null = view_distance, all immediate).
    let (immediate, deferred): (Vec<_>, Vec<_>) = to_load
        .into_iter()
        .partition(|(cx, cz)| {
            let dx = (*cx - new_cx).abs();
            let dz = (*cz - new_cz).abs();
            dx.max(dz) <= immediate_radius
        });

    // Send inner chunks NOW (before center update), wrapped in a chunk batch
    // so the client actually renders them.
    if !immediate.is_empty() {
        let batch_start: ClientboundGamePacket = ClientboundChunkBatchStart.into_variant();
        write_packet(&batch_start, write, compression, cipher).await?;

        for (cx, cz) in &immediate {
            worldgen.ensure_generated(world, *cx, *cz);
            send_chunk_from_world(write, compression, cipher, world, worldgen, *cx, *cz).await?;
            loaded_chunks.insert((*cx, *cz));
            sent_to_client.insert((*cx, *cz));
        }

        let batch_end: ClientboundGamePacket = ClientboundChunkBatchFinished {
            batch_size: immediate.len() as u32,
        }.into_variant();
        write_packet(&batch_end, write, compression, cipher).await?;
    }

    // NOW update the chunk cache center -- client already has nearby chunks.
    let center: ClientboundGamePacket = ClientboundSetChunkCacheCenter {
        x: new_cx,
        z: new_cz,
    }.into_variant();
    write_packet(&center, write, compression, cipher).await?;

    // Mark deferred chunks as "claimed" and enqueue.
    for pos in &deferred {
        loaded_chunks.insert(*pos);
    }
    chunk_send_queue.extend(deferred.iter());

    if !immediate.is_empty() || !deferred.is_empty() || !to_unload.is_empty() {
        tracing::debug!(
            "Chunk update: center ({},{}), {} unloaded, {} immediate, {} deferred, queue={}",
            new_cx, new_cz,
            to_unload.len(), immediate.len(), deferred.len(),
            chunk_send_queue.len(),
        );
    }

    Ok(())
}

// ── Chunk data ──────────────────────────────────────────────────────────

/// Send a `ForgetLevelChunk` packet with correct bit handling, working around
/// the `azalea-core` `ChunkPos` serialization bug for negative coordinates.
///
/// The wire format is (Z, X) each as a big-endian i32, packed into a u64.
/// We build the u64 manually using `u32` casts so a negative i32 zero-extends
/// to its lower 32 bits without polluting the upper 32 bits.
pub(crate) async fn send_forget_level_chunk<W: AsyncWrite + Unpin + Send>(
    write: &mut W,
    compression: Option<u32>,
    cipher: &mut Option<azalea_crypto::Aes128CfbEnc>,
    cx: i32,
    cz: i32,
) -> Result<()> {
    use azalea_buf::AzBufVar;
    use azalea_protocol::packets::ProtocolPacket;

    let mut raw = Vec::new();

    // Packet ID
    let dummy = ClientboundForgetLevelChunk {
        pos: azalea_core::position::ChunkPos::new(0, 0),
    };
    let packet_id = ClientboundGamePacket::ForgetLevelChunk(dummy).id();
    (packet_id as u32).azalea_write_var(&mut raw)?;

    // ChunkPos as u64: (cx as u32 as u64) | ((cz as u32 as u64) << 32).
    // The double-cast is critical: `cx as u64` directly would sign-extend.
    let packed: u64 = ((cx as u32) as u64) | (((cz as u32) as u64) << 32);
    packed.azalea_write(&mut raw)?;

    azalea_protocol::write::write_raw_packet(&raw, write, compression, cipher).await?;
    Ok(())
}

/// Lazily compute sky light for a chunk the first time it is sent.
///
/// Scans each column top-down: sky=15 for air/transparent blocks, dropping
/// to 0 at the first fully opaque block. Only non-zero values are written
/// since `LightSection` defaults to all zeros. Idempotent via `World::sky_lit`.
///
/// Holds the chunk's `RefMut` for the duration of the scan so we do one
/// DashMap acquisition instead of ~100K (one per `set_sky_light`/`get_block`
/// call). This is the difference between ~30 ms and <1 ms per chunk.
pub(crate) fn ensure_sky_light(world: &World, cx: i32, cz: i32) {
    use ultimate_engine::world::position::{ChunkPos, LocalBlockPos};

    let cp = ChunkPos::new(cx, cz);
    if world.is_sky_lit(&cp) {
        return;
    }

    let max_y = 319i64;
    let min_y = -64i64;

    // Single write-lock acquisition for the whole chunk.
    if let Some(mut chunk) = world.get_chunk_mut(&cp) {
        // Re-check under the lock: with many players joining at once,
        // hundreds of tasks pass the lock-free `is_sky_lit` check above
        // and queue here — without this, each would redo the full-chunk
        // scan (a thundering herd measured at 1,000 concurrent joins).
        if world.is_sky_lit(&cp) {
            return;
        }
        for lx in 0..16u8 {
            for lz in 0..16u8 {
                let mut sky_level: u8 = 15;
                for y in (min_y..=max_y).rev() {
                    let pos = LocalBlockPos { x: lx, y, z: lz };
                    let block = chunk.get_block(pos);
                    let opacity = crate::block::light_opacity(block);
                    if sky_level > 0 {
                        chunk.set_sky_light(pos, sky_level);
                    }
                    if opacity >= 15 {
                        break;
                    } else if opacity > 0 {
                        sky_level = sky_level.saturating_sub(opacity);
                    }
                }
            }
        }
        // Mark while still holding the chunk guard so the under-lock
        // re-check above is exact (no scan/mark race window).
        world.mark_sky_lit(cp);
        return;
    }

    // Chunk absent: nothing to scan, but mark so we don't retry forever.
    world.mark_sky_lit(cp);
}

/// Send a chunk read from the World in MC 1.21.5+ wire format.
/// Reads actual block state from the engine World, so edits persist.
///
/// `worldgen` supplies the biome registry ID for the chunk (Stage 4b ships
/// one biome per chunk, encoded as a single-valued biome paletted container
/// in every section).
pub(crate) async fn send_chunk_from_world<W: AsyncWrite + Unpin + Send>(
    write: &mut W,
    compression: Option<u32>,
    cipher: &mut Option<azalea_crypto::Aes128CfbEnc>,
    world: &World,
    worldgen: &dyn WorldGen,
    cx: i32,
    cz: i32,
) -> Result<()> {
    use ultimate_engine::world::block::BlockId;
    use ultimate_engine::world::position::ChunkPos;

    let total_sections = 24;
    let min_y: i64 = -64;
    let base_x = cx as i64 * 16;
    let base_z = cz as i64 * 16;
    let mut section_data = Vec::new();

    // Track the highest non-air Y for each column (for MOTION_BLOCKING heightmap).
    // Initialised to min_y - 1 meaning "no solid block found yet".
    let mut highest_y = [min_y - 1i64; 256];

    // Acquire the DashMap chunk reference ONCE. The previous code did
    // ~98K `world.get_block` calls per chunk, each going through DashMap;
    // this collapses that to a single lock acquisition.
    let chunk_ref = world.get_chunk(&ChunkPos::new(cx, cz));

    for section_i in 0..total_sections {
        let engine_section_idx = section_i as i32 + (min_y as i32 >> 4);
        let section_base_y = min_y + (section_i as i64) * 16;

        // Per-section 4×4×4 biome cells: 64 entries, indexed
        // y*16 + z*4 + x (matches azalea-world's PalletedContainerKind<Biome>).
        // Sample at the centre of each cell. Our current biome sources are
        // y-independent, but we plumb y anyway so 3D biomes (e.g.
        // dripstone_caves vs surface) can override later.
        let mut biomes = [0u32; 64];
        for by in 0..4usize {
            for bz in 0..4usize {
                for bx in 0..4usize {
                    let wx = base_x + (bx as i64) * 4 + 2;
                    let wy = section_base_y + (by as i64) * 4 + 2;
                    let wz = base_z + (bz as i64) * 4 + 2;
                    biomes[by * 16 + bz * 4 + bx] = worldgen.biome_at_cell(wx, wy, wz);
                }
            }
        }

        // Sparse fast path: a section that doesn't exist in the chunk's
        // HashMap is by definition all-air and can be sent without scanning.
        let section_opt = chunk_ref.as_ref().and_then(|c| c.section(engine_section_idx));
        let Some(section) = section_opt else {
            write_empty_section(&mut section_data, &biomes)?;
            continue;
        };

        // Uniform fast path: a single-entry palette means every cell is
        // that block — no per-cell scan needed at all (Phase 6c paletted
        // sections make this O(1)).
        if section.palette().len() == 1 {
            let only = section.palette()[0];
            if only == BlockId::AIR {
                write_empty_section(&mut section_data, &biomes)?;
            } else {
                let top = section_base_y + 15;
                for h in highest_y.iter_mut() {
                    if top > *h {
                        *h = top;
                    }
                }
                write_single_section(&mut section_data, only.0 as u32, &biomes)?;
            }
            continue;
        }

        // General path: materialize the section once (cheap palette-index
        // reads) and scan in XZY order (y * 256 + z * 16 + x).
        let mut blocks = [BlockId::AIR; 4096];
        for (idx, b) in blocks.iter_mut().enumerate() {
            *b = section.get_by_index(idx);
        }
        let first = blocks[0];
        let mut all_same = true;
        let mut non_air: u16 = 0;

        for ly in 0..16usize {
            for lz in 0..16usize {
                for lx in 0..16usize {
                    let idx = ly * 256 + lz * 16 + lx;
                    let b = blocks[idx];
                    if b != first { all_same = false; }
                    if b != BlockId::AIR {
                        non_air = non_air.saturating_add(1);
                        let col = lz * 16 + lx;
                        let y = section_base_y + ly as i64;
                        if y > highest_y[col] {
                            highest_y[col] = y;
                        }
                    }
                }
            }
        }

        if all_same {
            if first == BlockId::AIR {
                write_empty_section(&mut section_data, &biomes)?;
            } else {
                write_single_section(&mut section_data, first.0 as u32, &biomes)?;
            }
        } else {
            write_section_from_blocks(&mut section_data, &blocks, non_air, &biomes)?;
        }
    }
    drop(chunk_ref);

    // Encode MOTION_BLOCKING heightmap (bit-packed u64 array).
    let heightmap_data = encode_heightmap(&highest_y, min_y);

    // Build the chunk packet manually because azalea's AzBuf derive
    // serializes heightmaps as a VarInt-prefixed Vec, but the MC protocol
    // expects them as an NBT compound. azalea is a client lib (reads only).
    use azalea_buf::AzBufVar;
    use azalea_protocol::packets::ProtocolPacket;

    let mut raw_packet = Vec::new();

    // Packet ID for ClientboundLevelChunkWithLight
    let dummy = azalea_protocol::packets::game::ClientboundLevelChunkWithLight {
        x: 0, z: 0,
        chunk_data: azalea_protocol::packets::game::c_level_chunk_with_light::ClientboundLevelChunkPacketData {
            heightmaps: vec![], data: vec![].into_boxed_slice().into(), block_entities: vec![],
        },
        light_data: azalea_protocol::packets::game::c_light_update::ClientboundLightUpdatePacketData {
            sky_y_mask: BitSet::new(0), block_y_mask: BitSet::new(0),
            empty_sky_y_mask: BitSet::new(0), empty_block_y_mask: BitSet::new(0),
            sky_updates: Default::default(), block_updates: Default::default(),
        },
    };
    let packet_id = ClientboundGamePacket::LevelChunkWithLight(dummy).id();
    (packet_id as u32).azalea_write_var(&mut raw_packet)?;

    // x, z (Int, Int)
    cx.azalea_write(&mut raw_packet)?;
    cz.azalea_write(&mut raw_packet)?;

    // Heightmaps as Prefixed Array (1.21.5+ format, NOT NBT).
    // Format: VarInt(count) + for each: VarInt(type_enum) + VarInt(long_count) + i64[]
    //
    // We send MOTION_BLOCKING (enum 4) + WORLD_SURFACE (enum 1).
    // Both use the same data (highest non-air block) which is sufficient for
    // the client's renderer and sky-light calculations.
    2u32.azalea_write_var(&mut raw_packet)?; // count = 2

    // MOTION_BLOCKING (ordinal 4)
    4u32.azalea_write_var(&mut raw_packet)?;
    (heightmap_data.len() as u32).azalea_write_var(&mut raw_packet)?;
    for &val in heightmap_data.iter() {
        (val as i64).azalea_write(&mut raw_packet)?;
    }

    // WORLD_SURFACE (ordinal 1) — same data
    1u32.azalea_write_var(&mut raw_packet)?;
    (heightmap_data.len() as u32).azalea_write_var(&mut raw_packet)?;
    for &val in heightmap_data.iter() {
        (val as i64).azalea_write(&mut raw_packet)?;
    }

    // Data: VarInt(length) + raw section bytes
    (section_data.len() as u32).azalea_write_var(&mut raw_packet)?;
    raw_packet.extend_from_slice(&section_data);

    // Block entities: VarInt(0)
    0u32.azalea_write_var(&mut raw_packet)?;

    // Ensure sky light is computed for this chunk (lazy, on first send).
    ensure_sky_light(world, cx, cz);

    // Light data — read real light from the world's LightSections.
    // BitSet indices: 0 = extra section below world, 1..24 = actual sections, 25 = extra above.
    let num_light_sections = total_sections + 2; // 26
    let mut sky_y_mask = BitSet::new(num_light_sections);
    let mut block_y_mask = BitSet::new(num_light_sections);
    let mut empty_sky_y_mask = BitSet::new(num_light_sections);
    let mut empty_block_y_mask = BitSet::new(num_light_sections);
    let mut sky_updates: Vec<Vec<u8>> = Vec::new();
    let mut block_updates: Vec<Vec<u8>> = Vec::new();

    let chunk_pos = ultimate_engine::world::position::ChunkPos::new(cx, cz);
    let chunk_ref = world.get_chunk(&chunk_pos);

    // Extra section below (bit 0): empty
    empty_sky_y_mask.set(0);
    empty_block_y_mask.set(0);

    for section_i in 0..total_sections {
        let bit_idx = section_i + 1;
        let engine_section_idx = section_i as i32 + (min_y as i32 >> 4); // e.g. section_i=0 → -4

        let light_sec = chunk_ref.as_ref().and_then(|c| c.light_section(engine_section_idx));

        match light_sec {
            Some(ls) => {
                if ls.is_sky_empty() {
                    empty_sky_y_mask.set(bit_idx);
                } else {
                    sky_y_mask.set(bit_idx);
                    sky_updates.push(ls.sky.to_vec());
                }
                if ls.is_block_empty() {
                    empty_block_y_mask.set(bit_idx);
                } else {
                    block_y_mask.set(bit_idx);
                    block_updates.push(ls.block.to_vec());
                }
            }
            None => {
                empty_sky_y_mask.set(bit_idx);
                empty_block_y_mask.set(bit_idx);
            }
        }
    }

    // Extra section above (bit 25): empty
    empty_sky_y_mask.set(num_light_sections - 1);
    empty_block_y_mask.set(num_light_sections - 1);

    // CRITICAL: release the DashMap read guard BEFORE the awaits below.
    // A guard held across an await parks with its task; under hundreds of
    // concurrent joins all reading the same spawn chunks, the write-side
    // (`ensure_sky_light`'s `get_chunk_mut`) then blocks tokio worker
    // threads on locks whose holders can never be polled — wedging the
    // whole runtime (found by the 1,000-player load test: 0 joins, ~0 CPU).
    drop(chunk_ref);

    sky_y_mask.azalea_write(&mut raw_packet)?;
    block_y_mask.azalea_write(&mut raw_packet)?;
    empty_sky_y_mask.azalea_write(&mut raw_packet)?;
    empty_block_y_mask.azalea_write(&mut raw_packet)?;

    (sky_updates.len() as u32).azalea_write_var(&mut raw_packet)?;
    for arr in &sky_updates {
        (arr.len() as u32).azalea_write_var(&mut raw_packet)?;
        raw_packet.extend_from_slice(arr);
    }

    (block_updates.len() as u32).azalea_write_var(&mut raw_packet)?;
    for arr in &block_updates {
        (arr.len() as u32).azalea_write_var(&mut raw_packet)?;
        raw_packet.extend_from_slice(arr);
    }

    // Write the raw packet with framing
    azalea_protocol::write::write_raw_packet(&raw_packet, write, compression, cipher).await?;

    Ok(())
}

/// Encode a MOTION_BLOCKING / WORLD_SURFACE heightmap as a bit-packed `u64`
/// array matching the vanilla Minecraft format.
///
/// Each column's entry stores `(highest_non_air_y + 1 - min_y)` using 9 bits
/// (for a 384-block world height).  Entries are packed LSB-first into u64s,
/// 7 entries per u64 (63 bits used, 1 bit padding).
pub(crate) fn encode_heightmap(highest_y: &[i64; 256], min_y: i64) -> Box<[u64]> {
    const BITS: usize = 9; // ceil(log2(384 + 1))
    const PER_LONG: usize = 64 / BITS; // 7
    const NUM_LONGS: usize = (256 + PER_LONG - 1) / PER_LONG; // 37

    let mut data = vec![0u64; NUM_LONGS];
    for (i, &hy) in highest_y.iter().enumerate() {
        let value = if hy >= min_y {
            (hy + 1 - min_y) as u64
        } else {
            0 // column is entirely air
        };
        let long_idx = i / PER_LONG;
        let bit_offset = (i % PER_LONG) * BITS;
        data[long_idx] |= (value & ((1 << BITS) - 1)) << bit_offset;
    }
    data.into_boxed_slice()
}

/// Write a mixed chunk section directly from the section's flat block array.
/// Uses indirect palette encoding (1.21.5+ format: no VarInt data_length).
///
/// Replaces `write_section_from_world` (which did 4096 DashMap lookups per
/// section). Palette construction uses a 256-bucket linear-probe map keyed
/// on `(state_id mod 256)` to avoid the previous O(palette_size) scan per
/// block — typical sections have 1-8 unique blocks so the buckets are
/// effectively single-entry.
pub(crate) fn write_section_from_blocks(
    buf: &mut Vec<u8>,
    blocks_in: &[ultimate_engine::world::block::BlockId; 4096],
    non_air_count: u16,
    biomes: &[u32; 64],
) -> Result<()> {
    use azalea_buf::AzBufVar;

    // Palette: lookup keyed by state_id; cap palette length so we fall
    // back to direct encoding if a section is unusually heterogeneous.
    let mut palette: Vec<u32> = vec![0]; // air always at index 0
    let mut state_to_palette: std::collections::HashMap<u32, u8> =
        std::collections::HashMap::with_capacity(8);
    state_to_palette.insert(0, 0);

    let mut indices = [0u8; 4096];
    for i in 0..4096 {
        let state_id = blocks_in[i].0 as u32;
        let palette_idx = match state_to_palette.get(&state_id) {
            Some(&idx) => idx,
            None => {
                let idx = palette.len() as u8;
                palette.push(state_id);
                state_to_palette.insert(state_id, idx);
                idx
            }
        };
        indices[i] = palette_idx;
    }

    // Bits per entry: minimum 4 for blocks (MC indirect-palette rule).
    let bpe = (palette.len() as f64).log2().ceil().max(1.0) as u8;
    let bpe = bpe.max(4);

    (non_air_count as i16).azalea_write(buf)?;
    bpe.azalea_write(buf)?;
    (palette.len() as u32).azalea_write_var(buf)?;
    for &id in &palette {
        id.azalea_write_var(buf)?;
    }

    // Packed data (1.21.5+: NO VarInt length prefix).
    let values_per_long = 64 / bpe as usize;
    let num_longs = (4096 + values_per_long - 1) / values_per_long;
    let mask = (1u64 << bpe) - 1;
    for long_i in 0..num_longs {
        let mut long_val: u64 = 0;
        for vi in 0..values_per_long {
            let block_i = long_i * values_per_long + vi;
            if block_i < 4096 {
                long_val |= ((indices[block_i] as u64) & mask) << (vi * bpe as usize);
            }
        }
        long_val.azalea_write(buf)?;
    }

    // Biomes: per-4×4×4-cell palette (64 entries per section).
    write_biome_container(buf, biomes)?;

    Ok(())
}

/// Write a single-valued non-air chunk section (all blocks the same).
///
/// 1.21.5+ format: no VarInt data_length for paletted containers.
pub(crate) fn write_single_section(buf: &mut Vec<u8>, block_state_id: u32, biomes: &[u32; 64]) -> Result<()> {
    use azalea_buf::AzBufVar;

    // Block count (i16)
    4096i16.azalea_write(buf)?;
    // Block states: single-valued palette (bpe=0, value, NO data array)
    0u8.azalea_write(buf)?;
    block_state_id.azalea_write_var(buf)?;
    // Biomes: per-cell.
    write_biome_container(buf, biomes)?;

    Ok(())
}

/// Write an empty (all-air) chunk section to the buffer.
///
/// 1.21.5+ format: no VarInt data_length for paletted containers.
pub(crate) fn write_empty_section(buf: &mut Vec<u8>, biomes: &[u32; 64]) -> Result<()> {
    use azalea_buf::AzBufVar;

    // Block count: 0 (no non-air blocks)
    0i16.azalea_write(buf)?;
    // Block states: single-valued palette = air (0)
    0u8.azalea_write(buf)?;
    0u32.azalea_write_var(buf)?;
    // Biomes: per-cell.
    write_biome_container(buf, biomes)?;

    Ok(())
}

/// Encode a 64-entry biome paletted container (4×4×4 cells per section).
///
/// - All 64 cells share one biome → bits_per_entry=0, single VarInt palette.
/// - Otherwise → indirect palette with ceil(log2(palette_len)) bits per
///   entry (min 1), no length prefix on the data array (1.21.5+ format).
///
/// Cell index layout matches azalea-world's `PalletedContainerKind<Biome>`:
/// `index = y * 16 + z * 4 + x` where each axis is in `0..4`.
pub(crate) fn write_biome_container(buf: &mut Vec<u8>, biomes: &[u32; 64]) -> Result<()> {
    use azalea_buf::AzBufVar;

    let first = biomes[0];
    if biomes.iter().all(|&b| b == first) {
        // Single-valued fast path — exactly what the per-chunk biome
        // implementation used to write.
        0u8.azalea_write(buf)?;
        first.azalea_write_var(buf)?;
        return Ok(());
    }

    // Indirect palette. Build it preserving insertion order so cell
    // indices stay deterministic across runs.
    let mut palette: Vec<u32> = Vec::with_capacity(8);
    let mut indices = [0u8; 64];
    for (i, &b) in biomes.iter().enumerate() {
        let idx = match palette.iter().position(|&v| v == b) {
            Some(p) => p,
            None => {
                palette.push(b);
                palette.len() - 1
            }
        };
        indices[i] = idx as u8;
    }

    let bpe = (palette.len() as f64).log2().ceil().max(1.0) as u8;

    bpe.azalea_write(buf)?;
    (palette.len() as u32).azalea_write_var(buf)?;
    for &id in &palette {
        id.azalea_write_var(buf)?;
    }

    // Packed data — no VarInt length prefix (1.21.5+).
    let values_per_long = 64 / bpe as usize;
    let num_longs = (64 + values_per_long - 1) / values_per_long;
    let mask = (1u64 << bpe) - 1;
    for long_i in 0..num_longs {
        let mut long_val: u64 = 0;
        for vi in 0..values_per_long {
            let cell_i = long_i * values_per_long + vi;
            if cell_i < 64 {
                long_val |= ((indices[cell_i] as u64) & mask) << (vi * bpe as usize);
            }
        }
        long_val.azalea_write(buf)?;
    }

    Ok(())
}


/// Send ClientboundLightUpdate packets for a batch of light changes.
///
/// Groups changes by chunk so we send at most one packet per affected chunk.
/// Each packet carries the full nibble array for every section that was touched
/// in that chunk, read directly from the world (which has already been updated
/// by the causal engine).
pub(crate) async fn send_light_updates<W: AsyncWrite + Unpin + Send>(
    write: &mut W,
    compression: Option<u32>,
    cipher: &mut Option<azalea_crypto::Aes128CfbEnc>,
    world: &World,
    light_changes: &[event_bus::LightChange],
) -> Result<()> {
    use std::collections::{HashMap, HashSet};
    use ultimate_engine::world::position::ChunkPos;

    if light_changes.is_empty() {
        return Ok(());
    }

    // Group changed sections by chunk.
    // Key: (cx, cz), Value: set of section indices that were touched.
    let mut chunk_sections: HashMap<(i32, i32), HashSet<i32>> = HashMap::new();
    for lc in light_changes {
        let cp = lc.pos.chunk();
        let section_idx = if lc.pos.y >= 0 {
            (lc.pos.y >> 4) as i32
        } else {
            ((lc.pos.y + 1) >> 4) as i32 - 1
        };
        chunk_sections
            .entry((cp.x, cp.z))
            .or_default()
            .insert(section_idx);
    }

    let min_y: i64 = -64;
    let total_sections = 24usize;
    let num_light_sections = total_sections + 2; // 26

    for ((cx, cz), touched_sections) in chunk_sections {
        let chunk_pos = ChunkPos::new(cx, cz);
        let chunk_ref = world.get_chunk(&chunk_pos);

        let mut sky_y_mask = BitSet::new(num_light_sections);
        let mut block_y_mask = BitSet::new(num_light_sections);
        let mut empty_sky_y_mask = BitSet::new(num_light_sections);
        let mut empty_block_y_mask = BitSet::new(num_light_sections);
        let mut sky_updates: Vec<Vec<u8>> = Vec::new();
        let mut block_updates: Vec<Vec<u8>> = Vec::new();

        for section_i in 0..total_sections {
            let engine_section_idx = section_i as i32 + (min_y as i32 >> 4);
            if !touched_sections.contains(&engine_section_idx) {
                continue;
            }

            let bit_idx = section_i + 1;
            let light_sec = chunk_ref.as_ref().and_then(|c| c.light_section(engine_section_idx));

            match light_sec {
                Some(ls) => {
                    if ls.is_sky_empty() {
                        empty_sky_y_mask.set(bit_idx);
                    } else {
                        sky_y_mask.set(bit_idx);
                        sky_updates.push(ls.sky.to_vec());
                    }
                    if ls.is_block_empty() {
                        empty_block_y_mask.set(bit_idx);
                    } else {
                        block_y_mask.set(bit_idx);
                        block_updates.push(ls.block.to_vec());
                    }
                }
                None => {
                    empty_sky_y_mask.set(bit_idx);
                    empty_block_y_mask.set(bit_idx);
                }
            }
        }

        // Release the read guard BEFORE the packet write awaits below —
        // guards held across awaits wedge the runtime under load (see
        // the matching comment in send_chunk_from_world).
        drop(chunk_ref);

        // Build the LightUpdate packet manually (azalea's Write impls
        // don't always match the server-side wire format).
        use azalea_buf::AzBufVar;
        use azalea_protocol::packets::ProtocolPacket;

        let mut raw = Vec::new();

        // Packet ID
        let dummy = azalea_protocol::packets::game::ClientboundLightUpdate {
            x: 0, z: 0,
            light_data: azalea_protocol::packets::game::c_light_update::ClientboundLightUpdatePacketData {
                sky_y_mask: BitSet::new(0), block_y_mask: BitSet::new(0),
                empty_sky_y_mask: BitSet::new(0), empty_block_y_mask: BitSet::new(0),
                sky_updates: Default::default(), block_updates: Default::default(),
            },
        };
        let packet_id = ClientboundGamePacket::LightUpdate(dummy).id();
        (packet_id as u32).azalea_write_var(&mut raw)?;

        // Chunk X, Chunk Z (VarInt)
        (cx as u32).azalea_write_var(&mut raw)?;
        (cz as u32).azalea_write_var(&mut raw)?;

        // Light data — same format as the tail of LevelChunkWithLight
        sky_y_mask.azalea_write(&mut raw)?;
        block_y_mask.azalea_write(&mut raw)?;
        empty_sky_y_mask.azalea_write(&mut raw)?;
        empty_block_y_mask.azalea_write(&mut raw)?;

        (sky_updates.len() as u32).azalea_write_var(&mut raw)?;
        for arr in &sky_updates {
            (arr.len() as u32).azalea_write_var(&mut raw)?;
            raw.extend_from_slice(arr);
        }

        (block_updates.len() as u32).azalea_write_var(&mut raw)?;
        for arr in &block_updates {
            (arr.len() as u32).azalea_write_var(&mut raw)?;
            raw.extend_from_slice(arr);
        }

        azalea_protocol::write::write_raw_packet(&raw, write, compression, cipher).await?;
    }

    Ok(())
}
