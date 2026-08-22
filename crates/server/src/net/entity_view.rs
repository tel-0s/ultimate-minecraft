//! Entity projection to clients: wire ids, spawn/backfill packets, the
//! player mirror into the EntityStore, and item pickup.
//!
//! (Pickup radius/delay are gameplay policy that currently lives here;
//! it moves to the gameplay layer with the rest of the packet-arm logic.)

use std::collections::HashSet;

use anyhow::Result;
use azalea_protocol::packets::game::{
    ClientboundGamePacket,
    ClientboundAddEntity,
};
use azalea_core::delta::LpVec3;
use azalea_registry::builtin::EntityKind;
use azalea_protocol::packets::Packet;
use azalea_protocol::write::write_packet;
use azalea_core::position::Vec3;
use azalea_core::entity_id::MinecraftEntityId;
use tokio::io::AsyncWrite;
use ultimate_engine::world::World;

use crate::event_bus::{self};

/// Convert degrees (f32) to a Minecraft protocol byte angle (i8).
/// MC encodes angles as 256 = 360 degrees.
pub(crate) fn degrees_to_byte_angle(degrees: f32) -> i8 {
    (degrees / 360.0 * 256.0) as i8
}

/// Try to convert an ItemKind to its corresponding BlockKind.
/// Uses string name matching: ItemKind::OakPlanks displays as "minecraft:oak_planks",
/// and BlockKind::from_str("oak_planks") parses it back.
/// Special-cases items whose name doesn't match a block (e.g. water_bucket → water).
pub(crate) fn item_to_block_kind(item: azalea_registry::builtin::ItemKind) -> Option<azalea_registry::builtin::BlockKind> {
    use azalea_registry::builtin::{BlockKind, ItemKind};

    // Items whose name doesn't map to a block name directly.
    match item {
        ItemKind::WaterBucket => return Some(BlockKind::Water),
        ItemKind::LavaBucket => return Some(BlockKind::Lava),
        ItemKind::Redstone => return Some(BlockKind::RedstoneWire),
        _ => {}
    }

    // Display gives "minecraft:oak_planks", strip prefix for FromStr which expects "oak_planks"
    let full = format!("{}", item);
    let name = full.strip_prefix("minecraft:").unwrap_or(&full);
    name.parse::<BlockKind>().ok()
}

/// Map engine BlockId to MC BlockState for protocol.
pub(crate) fn engine_block_to_mc(id: ultimate_engine::world::block::BlockId) -> azalea_block::BlockState {
    // For now, treat BlockId as a direct MC block state ID.
    // BlockId(0) = air, others map through azalea.
    azalea_block::BlockState::try_from(id.0 as u32).unwrap_or(azalea_block::BlockState::AIR)
}

// ── Item entities on the wire (Phase 5) ─────────────────────────────────

/// Wire entity id for an engine entity — offset into a high range so it
/// can never collide with player entity ids from the registry.
pub(crate) fn item_wire_id(id: ultimate_engine::world::entity::EntityId) -> MinecraftEntityId {
    MinecraftEntityId(0x4000_0000 | (id.0 as i32 & 0x3FFF_FFFF))
}

pub(crate) fn item_uuid(id: ultimate_engine::world::entity::EntityId) -> uuid::Uuid {
    // Stable, deterministic, and disjoint from player uuids.
    uuid::Uuid::from_u64_pair(0x554D_435F_4954_454D, id.0) // "UMC_ITEM"
}

/// The MC item rendered for a dropped block. Resolves the real block
/// name for ANY state via azalea's block trait (crate::block::name only
/// covers a handful of constants and returns "block#N" otherwise, which
/// silently fell back to stone for most drops). Blocks without a
/// same-named item still fall back to stone.
pub(crate) fn dropped_item_kind(block: ultimate_engine::world::block::BlockId) -> azalea_registry::builtin::ItemKind {
    use azalea_block::{BlockState, BlockTrait};
    let Ok(state) = BlockState::try_from(block.0 as u32) else {
        return azalea_registry::builtin::ItemKind::Stone;
    };
    let b: &dyn BlockTrait = state.to_trait();
    b.id().parse().unwrap_or(azalea_registry::builtin::ItemKind::Stone)
}

/// Is this an engine entity kind we project to clients?
pub(crate) fn is_client_entity(kind: ultimate_engine::world::entity::EntityKind) -> bool {
    kind == crate::rules::entity::KIND_ITEM || kind == crate::rules::entity::KIND_FALLING_BLOCK
}

/// Spawn an engine entity on the client. Items get AddEntity + the
/// item-stack metadata that makes them render; falling blocks render from
/// AddEntity alone (`data` = the block state id).
pub(crate) async fn send_entity_spawn<W: AsyncWrite + Unpin + Send>(
    write: &mut W,
    compression: Option<u32>,
    cipher: &mut Option<azalea_crypto::Aes128CfbEnc>,
    id: ultimate_engine::world::entity::EntityId,
    state: &ultimate_engine::world::entity::EntityState,
) -> Result<()> {
    use azalea_entity::{EntityDataItem, EntityDataValue, EntityMetadataItems};
    use azalea_inventory::ItemStack;

    let falling = state.kind == crate::rules::entity::KIND_FALLING_BLOCK;
    let wire = item_wire_id(id);
    let add: ClientboundGamePacket = ClientboundAddEntity {
        id: wire,
        uuid: item_uuid(id),
        entity_type: if falling { EntityKind::FallingBlock } else { EntityKind::Item },
        position: Vec3 { x: state.pos.x, y: state.pos.y, z: state.pos.z },
        // Initial velocity (blocks/tick on the wire): the client animates
        // the pop arc / fall locally between our segment corrections.
        movement: LpVec3::from_vec3(Vec3 {
            x: state.vel.x / 20.0,
            y: state.vel.y / 20.0,
            z: state.vel.z / 20.0,
        }),
        x_rot: 0,
        y_rot: 0,
        y_head_rot: 0,
        data: if falling {
            u32::from(engine_block_to_mc(crate::rules::entity::aux_block(state.aux))) as i32
        } else {
            0
        },
    }.into_variant();
    write_packet(&add, write, compression, cipher).await?;

    if !falling {
        let stack = ItemStack::Present(azalea_inventory::ItemStackData {
            kind: dropped_item_kind(crate::rules::entity::aux_block(state.aux)),
            count: 1,
            component_patch: Default::default(),
        });
        let meta: ClientboundGamePacket = azalea_protocol::packets::game::ClientboundSetEntityData {
            id: wire,
            packed_items: EntityMetadataItems(vec![EntityDataItem {
                index: 8, // Item entity: the displayed stack
                value: EntityDataValue::ItemStack(stack),
            }]),
        }.into_variant();
        write_packet(&meta, write, compression, cipher).await?;
    }
    Ok(())
}

/// Send spawn packets for every item entity resting in the given regions
/// that this client hasn't been sent yet. Called when the spatial view
/// gains regions (join, chunk-border crossing): resting entities emit no
/// events, so subscription alone would never reveal them.
pub(crate) async fn backfill_region_entities<W: AsyncWrite + Unpin + Send>(
    write: &mut W,
    compression: Option<u32>,
    cipher: &mut Option<azalea_crypto::Aes128CfbEnc>,
    world: &World,
    regions: &[event_bus::Region],
    spawned_items: &mut HashSet<u64>,
) -> Result<()> {
    for region in regions {
        for chunk in event_bus::SpatialSubscriber::chunks_of_region(*region) {
            for id in world.entities().in_chunk(chunk) {
                let Some(state) = world.entities().get(id) else { continue };
                if is_client_entity(state.kind) && spawned_items.insert(id.0) {
                    send_entity_spawn(write, compression, cipher, id, &state).await?;
                }
            }
        }
    }
    Ok(())
}

/// Mirror a player's position/rotation into the EntityStore (Phase 5).
/// Guarded on the CURRENT store state read here — each update is
/// independent, so a guard-dropped update (e.g. racing a cross-worker
/// spawn) self-heals at the next move packet instead of desyncing a
/// local cache chain. No-op until the spawn mirror has applied.
pub(crate) fn mirror_player_entity(
    world: &World,
    physics: &crate::physics::PhysicsHandle,
    pid: ultimate_engine::world::entity::EntityId,
    x: f64,
    y: f64,
    z: f64,
    y_rot: f32,
    x_rot: f32,
) {
    use ultimate_engine::causal::event::{Event, EventPayload};
    let Some(cur) = world.entities().get(pid) else { return };
    let new_state = crate::rules::entity::player_state(
        ultimate_engine::world::entity::Vec3::new(x, y, z),
        y_rot,
        x_rot,
        world.now(),
    );
    physics.submit_events(vec![Event {
        payload: EventPayload::EntitySet { id: pid, old: Some(cur), new: Some(new_state) },
    }]);
}

/// Pick up nearby items: submit a guarded despawn for each item within
/// reach (first player wins at the entity store's stale guard — no dupes)
/// and play the collect animation optimistically. The authoritative
/// despawn comes back through the spatial bus for everyone.
pub(crate) async fn try_item_pickup<W: AsyncWrite + Unpin + Send>(
    write: &mut W,
    compression: Option<u32>,
    cipher: &mut Option<azalea_crypto::Aes128CfbEnc>,
    world: &World,
    physics: &crate::physics::PhysicsHandle,
    player_eid: i32,
    px: f64,
    py: f64,
    pz: f64,
) -> Result<()> {
    use ultimate_engine::causal::event::{Event, EventPayload};
    use ultimate_engine::world::position::{BlockPos as EnginePos, ChunkPos};

    let pc = EnginePos::new(px as i64, py as i64, pz as i64).chunk();
    let now = world.now();
    for dx in -1..=1 {
        for dz in -1..=1 {
            for id in world.entities().in_chunk(ChunkPos::new(pc.x + dx, pc.z + dz)) {
                let Some(s) = world.entities().get(id) else { continue };
                if s.kind != crate::rules::entity::KIND_ITEM {
                    continue;
                }
                // Vanilla-style pickup delay after spawning.
                let spawn_at = crate::rules::entity::aux_despawn_at(s.aux)
                    .saturating_sub(crate::rules::entity::DESPAWN_AFTER);
                if now < spawn_at + 500_000_000 {
                    continue;
                }
                if (s.pos.x - px).abs() <= 1.5
                    && (s.pos.z - pz).abs() <= 1.5
                    && (s.pos.y - py).abs() <= 1.75
                {
                    physics.submit_events(vec![Event {
                        payload: EventPayload::EntitySet { id, old: Some(s), new: None },
                    }]);
                    let take: ClientboundGamePacket =
                        azalea_protocol::packets::game::ClientboundTakeItemEntity {
                            item_id: item_wire_id(id).0 as u32,
                            player_id: MinecraftEntityId(player_eid),
                            amount: 1,
                        }.into_variant();
                    write_packet(&take, write, compression, cipher).await?;
                }
            }
        }
    }
    Ok(())
}
