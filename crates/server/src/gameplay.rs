//! The gameplay layer: game-rule decisions that sit between the protocol
//! handler and the physics service.
//!
//! The connection layer parses packets into *intents* (break here, use
//! item on that face, switch slot); this module turns intents into
//! guarded `BlockAction`s — face-offset math, block orientation, stair
//! shapes, interactive-block behavior, and the creative inventory model
//! all live here, not in the packet match arms. azalea *value* types
//! (BlockState, Direction) appear in signatures; packet types never do.

use azalea_block::BlockState;
use azalea_core::direction::Direction;
use azalea_protocol::packets::game::s_use_item_on::BlockHit;
use azalea_registry::builtin::ItemKind;
use ultimate_engine::world::World;
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::position::BlockPos;

use crate::physics::BlockAction;

/// Creative-mode inventory model: nine hotbar block slots.
/// (Survival inventories are a future feature; the protocol's item
/// stacks are collapsed to their block form on the way in.)
pub struct Inventory {
    hotbar: [BlockState; 9],
    selected: usize,
}

impl Default for Inventory {
    fn default() -> Self {
        Self { hotbar: [BlockState::AIR; 9], selected: 0 }
    }
}

impl Inventory {
    /// Handle a creative slot update. Window slots 36–44 are the hotbar.
    pub fn set_creative_slot(&mut self, slot_num: i32, kind: Option<ItemKind>) {
        let hotbar_idx = slot_num - 36;
        if (0..9).contains(&hotbar_idx) {
            self.hotbar[hotbar_idx as usize] = kind
                .and_then(item_to_block_kind)
                .map(BlockState::from)
                .unwrap_or(BlockState::AIR);
        }
    }

    pub fn select(&mut self, slot: usize) {
        self.selected = slot.min(8);
    }

    /// The block form of the held item (AIR = nothing placeable).
    pub fn held(&self) -> BlockState {
        self.hotbar[self.selected]
    }
}

/// Break a block: instant in creative. `old` is our observation — the
/// physics stale guard drops the action if another event won the cell.
pub fn break_action(world: &World, pos: BlockPos) -> BlockAction {
    BlockAction {
        pos,
        old: world.get_block(pos),
        new: crate::block::AIR,
        update_stairs: true,
        drop_item: true,
    }
}

/// Right-clicking an interactive block *uses* it instead of placing:
/// levers toggle, buttons press (they release themselves via a timed
/// event in the redstone rule), repeaters cycle their delay. Returns
/// the resulting action.
pub fn use_block_action(world: &World, clicked: BlockPos) -> Option<BlockAction> {
    let clicked_id = world.get_block(clicked);
    let new = crate::rules::redstone::toggle_lever(clicked_id)
        .or_else(|| crate::rules::redstone::press_button(clicked_id))
        .or_else(|| crate::rules::redstone::cycle_repeater_delay(clicked_id))?;
    Some(BlockAction {
        pos: clicked,
        old: clicked_id,
        new,
        update_stairs: false,
        drop_item: false,
    })
}

/// The cell a placement lands in: adjacent to the clicked face.
pub fn placement_target(hit: &BlockHit) -> BlockPos {
    let p = hit.block_pos;
    let (dx, dy, dz) = match hit.direction {
        Direction::Down => (0, -1, 0),
        Direction::Up => (0, 1, 0),
        Direction::North => (0, 0, -1),
        Direction::South => (0, 0, 1),
        Direction::West => (-1, 0, 0),
        Direction::East => (1, 0, 0),
    };
    BlockPos::new(
        (p.x + dx) as i64,
        (p.y + dy) as i64,
        (p.z + dz) as i64,
    )
}

/// Place the held block against the clicked face, oriented by the
/// player's view (facing/axis/half/stair-shape rules in `placement`).
/// `None` when there is nothing to place.
pub fn place_action(
    world: &World,
    held: BlockState,
    hit: &BlockHit,
    player_y_rot: f32,
    player_x_rot: f32,
) -> Option<BlockAction> {
    if held == BlockState::AIR {
        return None;
    }
    let target = placement_target(hit);

    let cursor_y = (hit.location.y - hit.block_pos.y as f64) as f32;
    let oriented = crate::placement::orient_block(
        held,
        player_y_rot,
        player_x_rot,
        hit.direction,
        cursor_y,
    );
    let oriented = crate::placement::compute_stair_shape_for_placement(oriented, world, target);

    Some(BlockAction {
        pos: target,
        old: world.get_block(target),
        new: BlockId::new(u32::from(oriented) as u16),
        update_stairs: true,
        drop_item: false,
    })
}

/// Which block does this item place? (Handles the bucket/redstone
/// special cases whose item names differ from their block names.)
pub fn item_to_block_kind(item: azalea_registry::builtin::ItemKind) -> Option<azalea_registry::builtin::BlockKind> {
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

/// `/summon [count]` — spawn wander-mobs near a position (the manual
/// spawn path until natural spawning lands). Returns a human-readable
/// receipt for the chat.
pub fn summon_mobs(
    world: &World,
    physics: &crate::physics::PhysicsHandle,
    x: f64,
    y: f64,
    z: f64,
    count: usize,
) -> String {
    use ultimate_engine::world::entity::Vec3;
    let n = count.clamp(1, 100);
    for i in 0..n {
        // Ring placement so a batch doesn't stack in one cell.
        let theta = i as f64 / n as f64 * std::f64::consts::TAU;
        let at = Vec3::new(x + theta.cos() * 2.0, y + 0.5, z + theta.sin() * 2.0);
        physics.submit_events(crate::rules::mob::spawn_mob_events(world, at, 0));
    }
    format!("Summoned {n} mob{}", if n == 1 { "" } else { "s" })
}
