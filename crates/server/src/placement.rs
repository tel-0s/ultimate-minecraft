//! Block orientation logic for player-placed blocks.
//!
//! When a player places a block, vanilla Minecraft sets directional properties
//! (facing, axis, half, etc.) based on the player's orientation and the face
//! they clicked.  This module replicates that logic so blocks are placed with
//! the correct orientation instead of always defaulting to the first state.

use azalea_block::{BlockState, BlockTrait};
use azalea_core::direction::Direction;

use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::position::BlockPos as EngineBlockPos;
use ultimate_engine::world::World;

use crate::persistence::lookup_block_state;

// ── Public API ──────────────────────────────────────────────────────────────

/// Compute the correctly-oriented block state for a placed block.
///
/// * `default_state` – the default `BlockState` for this `BlockKind`
/// * `player_y_rot` – player yaw in degrees (MC convention: 0=south, 90=west,
///   180=north, 270=east)
/// * `player_x_rot` – player pitch in degrees (positive = looking down)
/// * `hit_direction` – face of the existing block that was clicked
/// * `cursor_y`      – Y position of the click *within* the clicked block face
///   (0.0 = bottom edge, 1.0 = top edge)
pub fn orient_block(
    default_state: BlockState,
    player_y_rot: f32,
    player_x_rot: f32,
    hit_direction: Direction,
    cursor_y: f32,
) -> BlockState {
    let block: Box<dyn BlockTrait> = Box::<dyn BlockTrait>::from(default_state);
    let prop_map = block.property_map();

    // Fast path: blocks with no properties need no orientation.
    if prop_map.is_empty() {
        return default_state;
    }

    let name = block.id().to_string();

    // Build a mutable copy of the property map we can tweak.
    let mut props: Vec<(String, String)> = prop_map
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    let mut changed = false;

    // ── axis (logs, pillars, hay bale, bone block, basalt, etc.) ────────
    if let Some(val) = prop_mut(&mut props, "axis") {
        let new = axis_from_hit_face(hit_direction);
        if *val != new {
            *val = new.to_string();
            changed = true;
        }
    }

    // ── facing ──────────────────────────────────────────────────────────
    // Check cubic support *before* taking a mutable borrow on props.
    let facing_is_cubic = props
        .iter()
        .find(|(k, _)| k == "facing")
        .map(|(_, v)| {
            matches!(v.as_str(), "up" | "down")
                || block_supports_vertical_facing(&name, &props)
        })
        .unwrap_or(false);

    if let Some(val) = prop_mut(&mut props, "facing") {
        if facing_is_cubic {
            // Six-directional blocks (pistons, dispensers, observers, etc.)
            // Face toward the direction the player is looking at.
            let new = cubic_facing_from_look(player_y_rot, player_x_rot);
            if *val != new {
                *val = new.to_string();
                changed = true;
            }
        } else if uses_same_direction_facing(&name) {
            // Stairs, repeaters, comparators, etc.: the `facing` property
            // points the same way the player is looking (so the step/input
            // side faces toward the player).
            let new = cardinal_same_as_yaw(player_y_rot);
            if *val != new {
                *val = new.to_string();
                changed = true;
            }
        } else {
            // Furnaces, chests, carved pumpkins, etc.: the `facing` property
            // is *opposite* to the player's look direction (so the "front"
            // faces toward the player).
            let new = cardinal_opposite_of_yaw(player_y_rot);
            if *val != new {
                *val = new.to_string();
                changed = true;
            }
        }
    }

    // ── half (stairs, trapdoors) ────────────────────────────────────────
    if let Some(val) = prop_mut(&mut props, "half") {
        if matches!(val.as_str(), "top" | "bottom") {
            let new = half_from_placement(hit_direction, cursor_y);
            if *val != new {
                *val = new.to_string();
                changed = true;
            }
        }
    }

    // ── type (slabs: top / bottom / double) ─────────────────────────────
    if let Some(val) = prop_mut(&mut props, "type") {
        if matches!(val.as_str(), "top" | "bottom" | "double") {
            let new = slab_type_from_placement(hit_direction, cursor_y);
            if *val != new {
                *val = new.to_string();
                changed = true;
            }
        }
    }

    // ── rotation (standing signs, banners, heads: 0-15) ─────────────────
    if let Some(val) = prop_mut(&mut props, "rotation") {
        let new = rotation_from_yaw(player_y_rot);
        let new_str = new.to_string();
        if *val != new_str {
            *val = new_str;
            changed = true;
        }
    }

    if !changed {
        return default_state;
    }

    // Sort and look up the oriented state.
    props.sort();
    lookup_block_state(&name, &props)
        .and_then(|id| BlockState::try_from(id as u32).ok())
        .unwrap_or(default_state)
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Get a mutable reference to a property value by key, if it exists.
fn prop_mut<'a>(props: &'a mut [(String, String)], key: &str) -> Option<&'a mut String> {
    props.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Determine whether a block supports up/down facing (cubic) by trying a
/// lookup with facing=up.  This is cached implicitly by the LazyLock in
/// persistence, so the lookup itself is O(1).
fn block_supports_vertical_facing(name: &str, props: &[(String, String)]) -> bool {
    let mut test_props: Vec<(String, String)> = props
        .iter()
        .map(|(k, v)| {
            if k == "facing" {
                (k.clone(), "up".to_string())
            } else {
                (k.clone(), v.clone())
            }
        })
        .collect();
    test_props.sort();
    lookup_block_state(name, &test_props).is_some()
}

/// Does this block use the *same* direction as the player's look for its
/// `facing` property?  Most blocks face *opposite* (so the "front" faces the
/// player), but stairs, repeaters, and comparators face the *same* way (so
/// the step / input side faces toward the player).
fn uses_same_direction_facing(name: &str) -> bool {
    name.ends_with("_stairs")
        || name == "repeater"
        || name == "comparator"
}

/// Axis from the face that was clicked:
/// - Top/Bottom → Y (vertical)
/// - North/South → Z
/// - East/West  → X
fn axis_from_hit_face(dir: Direction) -> &'static str {
    match dir {
        Direction::Up | Direction::Down => "y",
        Direction::North | Direction::South => "z",
        Direction::East | Direction::West => "x",
    }
}

/// Cardinal direction opposite to the player's yaw.
///
/// MC yaw: 0°=south, 90°=west, 180°=north, 270°=east.
/// Players place blocks facing *toward* themselves, so the block should face
/// *away from* the player (= opposite of their look direction).
fn cardinal_opposite_of_yaw(y_rot: f32) -> &'static str {
    let yaw = ((y_rot % 360.0) + 360.0) % 360.0;
    if yaw >= 315.0 || yaw < 45.0 {
        "north" // player faces south → block faces north
    } else if (45.0..135.0).contains(&yaw) {
        "east" // player faces west → block faces east
    } else if (135.0..225.0).contains(&yaw) {
        "south" // player faces north → block faces south
    } else {
        "west" // player faces east → block faces west
    }
}

/// Cardinal direction matching the player's yaw (same direction they look).
///
/// Used for stairs, repeaters, comparators — blocks where `facing` encodes
/// the direction the "back" of the block points, so the step / input side
/// ends up facing the player.
fn cardinal_same_as_yaw(y_rot: f32) -> &'static str {
    let yaw = ((y_rot % 360.0) + 360.0) % 360.0;
    if yaw >= 315.0 || yaw < 45.0 {
        "south" // player faces south → block faces south
    } else if (45.0..135.0).contains(&yaw) {
        "west" // player faces west → block faces west
    } else if (135.0..225.0).contains(&yaw) {
        "north" // player faces north → block faces north
    } else {
        "east" // player faces east → block faces east
    }
}

/// Six-directional (cubic) facing from the player's full look direction.
/// Used for pistons, dispensers, observers, end rods, etc.
fn cubic_facing_from_look(y_rot: f32, x_rot: f32) -> &'static str {
    if x_rot > 45.0 {
        "down" // looking steeply down
    } else if x_rot < -45.0 {
        "up" // looking steeply up
    } else {
        // Horizontal: same direction player is looking (not opposite!)
        let yaw = ((y_rot % 360.0) + 360.0) % 360.0;
        if yaw >= 315.0 || yaw < 45.0 {
            "south"
        } else if (45.0..135.0).contains(&yaw) {
            "west"
        } else if (135.0..225.0).contains(&yaw) {
            "north"
        } else {
            "east"
        }
    }
}

/// Top or bottom half for stairs & trapdoors.
///
/// - Clicking a block's **top face** → bottom half (stair sits on top of that
///   block).
/// - Clicking a block's **bottom face** → top half (stair hangs from it).
/// - Clicking a **side face** → depends on the cursor Y within the face.
fn half_from_placement(hit_dir: Direction, cursor_y: f32) -> &'static str {
    match hit_dir {
        Direction::Up => "bottom",
        Direction::Down => "top",
        _ => {
            if cursor_y > 0.5 {
                "top"
            } else {
                "bottom"
            }
        }
    }
}

/// Slab type from placement context (same logic as half, but using slab names).
fn slab_type_from_placement(hit_dir: Direction, cursor_y: f32) -> &'static str {
    match hit_dir {
        Direction::Up => "bottom",
        Direction::Down => "top",
        _ => {
            if cursor_y > 0.5 {
                "top"
            } else {
                "bottom"
            }
        }
    }
}

/// Standing sign / banner rotation (0-15) from player yaw.
///
/// Rotation 0 = south, increments clockwise in 22.5° steps.
/// The sign faces *toward* the player, so we add 180° to the yaw (the sign
/// text faces the player, meaning the block's rotation points away and then
/// we compensate).
fn rotation_from_yaw(y_rot: f32) -> u8 {
    // Sign faces the player (toward them), so the "rotation" value encodes
    // the direction the front of the sign points.  That's *opposite* to the
    // player's look direction → add 180°.
    let yaw = ((y_rot + 180.0) % 360.0 + 360.0) % 360.0;
    // Each step is 22.5°.  Round to nearest.
    ((yaw / 22.5 + 0.5) as u32 % 16) as u8
}

// ── Stair corner shape logic ────────────────────────────────────────────────
//
// Vanilla Minecraft recomputes the `shape` property of stairs whenever a
// neighboring block changes.  The shape depends on adjacent stairs' `facing`
// and `half` properties:
//
// - **Front neighbor** (in the stair's facing direction) with a perpendicular
//   facing → **outer** corner (convex turn).
// - **Back neighbor** (opposite of facing) with a perpendicular facing →
//   **inner** corner (concave turn).
// - The `canTakeShape` check prevents double-cornering when three stairs form
//   a Z-shape.

/// Minimal stair info extracted from a block in the world.
struct StairInfo {
    facing: String,
    half: String,
}

/// Try to extract stair properties from a `BlockId`.
fn stair_info_from_id(id: BlockId) -> Option<StairInfo> {
    if id == BlockId::AIR {
        return None;
    }
    let state = BlockState::try_from(id.0 as u32).ok()?;
    let block: Box<dyn BlockTrait> = Box::<dyn BlockTrait>::from(state);
    if !block.id().ends_with("_stairs") {
        return None;
    }
    let props = block.property_map();
    Some(StairInfo {
        facing: props.get("facing")?.to_string(),
        half: props.get("half")?.to_string(),
    })
}

/// Read stair info from the world at `pos`.
fn stair_info_at(world: &World, pos: EngineBlockPos) -> Option<StairInfo> {
    stair_info_from_id(world.get_block(pos))
}

/// Offset for a cardinal direction name.
fn cardinal_offset(dir: &str) -> (i64, i64) {
    match dir {
        "north" => (0, -1),
        "south" => (0, 1),
        "east" => (1, 0),
        "west" => (-1, 0),
        _ => (0, 0),
    }
}

fn opposite_cardinal(dir: &str) -> &'static str {
    match dir {
        "north" => "south",
        "south" => "north",
        "east" => "west",
        "west" => "east",
        _ => "north",
    }
}

/// Counter-clockwise rotation (viewed from above).
fn ccw(dir: &str) -> &'static str {
    match dir {
        "north" => "west",
        "west" => "south",
        "south" => "east",
        "east" => "north",
        _ => "north",
    }
}

/// Two directions are perpendicular if they lie on different axes.
fn perpendicular(a: &str, b: &str) -> bool {
    let z_axis = |d: &str| matches!(d, "north" | "south");
    z_axis(a) != z_axis(b)
}

/// Core stair shape algorithm (mirrors vanilla `StairBlock.getStairsShape`).
///
/// `neighbor_at(dx, dz)` returns the `StairInfo` at `(base + dx, base_y, base + dz)`.
fn compute_shape(
    facing: &str,
    half: &str,
    neighbor_at: &impl Fn(i64, i64) -> Option<StairInfo>,
) -> &'static str {
    // ── Front neighbor (in the facing direction) → outer corner ──────────
    let (fx, fz) = cardinal_offset(facing);
    if let Some(front) = neighbor_at(fx, fz) {
        if front.half == half && perpendicular(&front.facing, facing) {
            // canTakeShape: the block at pos + opposite(front.facing) must NOT
            // be a stair with the same facing AND half as ours.
            let opp_front = opposite_cardinal(&front.facing);
            let (cx, cz) = cardinal_offset(opp_front);
            let can_take = neighbor_at(cx, cz)
                .map(|s| s.facing != facing || s.half != half)
                .unwrap_or(true);
            if can_take {
                if front.facing == ccw(facing) {
                    return "outer_left";
                }
                return "outer_right";
            }
        }
    }

    // ── Back neighbor (opposite of facing) → inner corner ────────────────
    let back = opposite_cardinal(facing);
    let (bx, bz) = cardinal_offset(back);
    if let Some(rear) = neighbor_at(bx, bz) {
        if rear.half == half && perpendicular(&rear.facing, facing) {
            // canTakeShape: block at pos + rear.facing must NOT match.
            let (cx, cz) = cardinal_offset(&rear.facing);
            let can_take = neighbor_at(cx, cz)
                .map(|s| s.facing != facing || s.half != half)
                .unwrap_or(true);
            if can_take {
                if rear.facing == ccw(facing) {
                    return "inner_left";
                }
                return "inner_right";
            }
        }
    }

    "straight"
}

/// Compute the correct stair shape for a stair **about to be placed** at `pos`.
///
/// The stair is not yet in the world, so we inspect only the existing
/// neighbors.  Returns the `BlockState` with the `shape` property updated.
pub fn compute_stair_shape_for_placement(
    state: BlockState,
    world: &World,
    pos: EngineBlockPos,
) -> BlockState {
    let block: Box<dyn BlockTrait> = Box::<dyn BlockTrait>::from(state);
    let name = block.id().to_string();
    if !name.ends_with("_stairs") {
        return state;
    }

    let prop_map = block.property_map();
    let facing = match prop_map.get("facing") {
        Some(f) => f.to_string(),
        None => return state,
    };
    let half = match prop_map.get("half") {
        Some(h) => h.to_string(),
        None => return state,
    };
    let cur_shape = prop_map
        .get("shape")
        .unwrap_or(&"straight")
        .to_string();

    let shape = compute_shape(&facing, &half, &|dx, dz| {
        stair_info_at(world, EngineBlockPos::new(pos.x + dx, pos.y, pos.z + dz))
    });

    if shape == cur_shape {
        return state;
    }

    // Build the updated property list.
    let mut props: Vec<(String, String)> = prop_map
        .into_iter()
        .map(|(k, v)| {
            if k == "shape" {
                (k.to_string(), shape.to_string())
            } else {
                (k.to_string(), v.to_string())
            }
        })
        .collect();
    props.sort();

    lookup_block_state(&name, &props)
        .and_then(|id| BlockState::try_from(id as u32).ok())
        .unwrap_or(state)
}

/// After a block change at `changed_pos`, recompute the stair shapes of the
/// four horizontal neighbors.  Returns `(position, new_block_id)` pairs for
/// any stairs whose shape actually changed.
///
/// The caller is responsible for writing the new IDs into the world and
/// sending block-update packets.
pub fn update_adjacent_stair_shapes(
    world: &World,
    changed_pos: EngineBlockPos,
) -> Vec<(EngineBlockPos, BlockId)> {
    let mut updates = Vec::new();

    for &(dx, dz) in &[(0i64, -1i64), (0, 1), (1, 0), (-1, 0)] {
        let npos = EngineBlockPos::new(
            changed_pos.x + dx,
            changed_pos.y,
            changed_pos.z + dz,
        );

        let id = world.get_block(npos);
        if id == BlockId::AIR {
            continue;
        }

        let state = match BlockState::try_from(id.0 as u32) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let block: Box<dyn BlockTrait> = Box::<dyn BlockTrait>::from(state);
        let name = block.id().to_string();
        if !name.ends_with("_stairs") {
            continue;
        }

        let prop_map = block.property_map();
        let facing = match prop_map.get("facing") {
            Some(f) => f.to_string(),
            None => continue,
        };
        let half = match prop_map.get("half") {
            Some(h) => h.to_string(),
            None => continue,
        };
        let cur_shape = prop_map
            .get("shape")
            .unwrap_or(&"straight")
            .to_string();

        let new_shape = compute_shape(&facing, &half, &|ddx, ddz| {
            stair_info_at(
                world,
                EngineBlockPos::new(npos.x + ddx, npos.y, npos.z + ddz),
            )
        });

        if new_shape == cur_shape {
            continue;
        }

        let mut props: Vec<(String, String)> = prop_map
            .into_iter()
            .map(|(k, v)| {
                if k == "shape" {
                    (k.to_string(), new_shape.to_string())
                } else {
                    (k.to_string(), v.to_string())
                }
            })
            .collect();
        props.sort();

        if let Some(new_id) = lookup_block_state(&name, &props) {
            updates.push((npos, BlockId(new_id)));
        }
    }

    updates
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cardinal_opposite() {
        assert_eq!(cardinal_opposite_of_yaw(0.0), "north"); // facing south→north
        assert_eq!(cardinal_opposite_of_yaw(90.0), "east"); // facing west→east
        assert_eq!(cardinal_opposite_of_yaw(180.0), "south"); // facing north→south
        assert_eq!(cardinal_opposite_of_yaw(270.0), "west"); // facing east→west
        assert_eq!(cardinal_opposite_of_yaw(-90.0), "west"); // -90 == 270
    }

    #[test]
    fn test_axis_from_face() {
        assert_eq!(axis_from_hit_face(Direction::Up), "y");
        assert_eq!(axis_from_hit_face(Direction::Down), "y");
        assert_eq!(axis_from_hit_face(Direction::North), "z");
        assert_eq!(axis_from_hit_face(Direction::South), "z");
        assert_eq!(axis_from_hit_face(Direction::East), "x");
        assert_eq!(axis_from_hit_face(Direction::West), "x");
    }

    #[test]
    fn test_half_from_placement() {
        assert_eq!(half_from_placement(Direction::Up, 0.5), "bottom");
        assert_eq!(half_from_placement(Direction::Down, 0.5), "top");
        assert_eq!(half_from_placement(Direction::North, 0.3), "bottom");
        assert_eq!(half_from_placement(Direction::North, 0.7), "top");
    }

    #[test]
    fn test_rotation_from_yaw() {
        // Player facing south (yaw=0) → sign faces north → rotation should
        // be 8 (north = 180° from south, 180/22.5 = 8).
        assert_eq!(rotation_from_yaw(0.0), 8);
        // Player facing north (yaw=180) → sign faces south → rotation = 0.
        assert_eq!(rotation_from_yaw(180.0), 0);
    }

    #[test]
    fn test_orient_oak_log_axis() {
        use azalea_registry::builtin::BlockKind;

        // Default oak_log has axis=y.
        let default = BlockState::from(BlockKind::OakLog);
        // Click the north face → axis should become z.
        let oriented = orient_block(default, 0.0, 0.0, Direction::North, 0.5);
        let block: Box<dyn BlockTrait> = Box::<dyn BlockTrait>::from(oriented);
        let props = block.property_map();
        let axis = props.iter().find(|(k, _)| **k == "axis").map(|(_, v)| v.to_string());
        assert_eq!(axis.as_deref(), Some("z"));
    }

    #[test]
    fn test_orient_oak_stairs_facing() {
        use azalea_registry::builtin::BlockKind;

        let default = BlockState::from(BlockKind::OakStairs);
        // Player facing north (yaw=180) → stairs face north (same direction,
        // so the step side faces toward the player).
        let oriented = orient_block(default, 180.0, 0.0, Direction::Up, 0.5);
        let block: Box<dyn BlockTrait> = Box::<dyn BlockTrait>::from(oriented);
        let props = block.property_map();
        let facing = props
            .iter()
            .find(|(k, _)| **k == "facing")
            .map(|(_, v)| v.to_string());
        assert_eq!(facing.as_deref(), Some("north"));
    }

    #[test]
    fn test_orient_slab_top() {
        use azalea_registry::builtin::BlockKind;

        let default = BlockState::from(BlockKind::OakSlab);
        // Click bottom face → top slab.
        let oriented = orient_block(default, 0.0, 0.0, Direction::Down, 0.5);
        let block: Box<dyn BlockTrait> = Box::<dyn BlockTrait>::from(oriented);
        let props = block.property_map();
        let slab_type = props
            .iter()
            .find(|(k, _)| **k == "type")
            .map(|(_, v)| v.to_string());
        assert_eq!(slab_type.as_deref(), Some("top"));
    }

    #[test]
    fn test_no_orientation_for_plain_block() {
        use azalea_registry::builtin::BlockKind;

        let default = BlockState::from(BlockKind::Stone);
        // Stone has no directional properties; should return unchanged.
        let oriented = orient_block(default, 90.0, 45.0, Direction::Up, 0.5);
        assert_eq!(u32::from(oriented), u32::from(default));
    }

    // ── Stair corner shape tests ────────────────────────────────────────

    #[test]
    fn test_straight_when_no_neighbors() {
        // No neighbors → straight.
        let shape = compute_shape("north", "bottom", &|_, _| None);
        assert_eq!(shape, "straight");
    }

    #[test]
    fn test_outer_right_corner() {
        // Stair facing north, front neighbor (north) faces east.
        // East is clockwise from north → outer_right.
        let shape = compute_shape("north", "bottom", &|dx, dz| {
            if dx == 0 && dz == -1 {
                // North neighbor: stair facing east, same half.
                Some(StairInfo {
                    facing: "east".into(),
                    half: "bottom".into(),
                })
            } else {
                None
            }
        });
        assert_eq!(shape, "outer_right");
    }

    #[test]
    fn test_outer_left_corner() {
        // Stair facing north, front neighbor (north) faces west.
        // West is counter-clockwise from north → outer_left.
        let shape = compute_shape("north", "bottom", &|dx, dz| {
            if dx == 0 && dz == -1 {
                Some(StairInfo {
                    facing: "west".into(),
                    half: "bottom".into(),
                })
            } else {
                None
            }
        });
        assert_eq!(shape, "outer_left");
    }

    #[test]
    fn test_inner_right_corner() {
        // Stair facing north, back neighbor (south) faces east.
        // East is clockwise from north → inner_right.
        let shape = compute_shape("north", "bottom", &|dx, dz| {
            if dx == 0 && dz == 1 {
                Some(StairInfo {
                    facing: "east".into(),
                    half: "bottom".into(),
                })
            } else {
                None
            }
        });
        assert_eq!(shape, "inner_right");
    }

    #[test]
    fn test_inner_left_corner() {
        // Stair facing north, back neighbor (south) faces west.
        // West is counter-clockwise from north → inner_left.
        let shape = compute_shape("north", "bottom", &|dx, dz| {
            if dx == 0 && dz == 1 {
                Some(StairInfo {
                    facing: "west".into(),
                    half: "bottom".into(),
                })
            } else {
                None
            }
        });
        assert_eq!(shape, "inner_left");
    }

    #[test]
    fn test_no_corner_when_half_differs() {
        // Front neighbor is perpendicular but has a different half → no corner.
        let shape = compute_shape("north", "bottom", &|dx, dz| {
            if dx == 0 && dz == -1 {
                Some(StairInfo {
                    facing: "east".into(),
                    half: "top".into(), // different half!
                })
            } else {
                None
            }
        });
        assert_eq!(shape, "straight");
    }

    #[test]
    fn test_no_corner_when_parallel() {
        // Front neighbor faces same axis (north) → not perpendicular → no corner.
        let shape = compute_shape("north", "bottom", &|dx, dz| {
            if dx == 0 && dz == -1 {
                Some(StairInfo {
                    facing: "south".into(),
                    half: "bottom".into(),
                })
            } else {
                None
            }
        });
        assert_eq!(shape, "straight");
    }

    #[test]
    fn test_can_take_shape_prevents_double_corner() {
        // Stair facing north; front neighbor (north) faces east.
        // But block at pos + west (opposite of east) is ALSO a stair facing
        // north with the same half → canTakeShape fails → no corner.
        let shape = compute_shape("north", "bottom", &|dx, dz| {
            if dx == 0 && dz == -1 {
                // Front (north): stair facing east
                Some(StairInfo {
                    facing: "east".into(),
                    half: "bottom".into(),
                })
            } else if dx == -1 && dz == 0 {
                // West: stair facing north, same half → blocks the corner
                Some(StairInfo {
                    facing: "north".into(),
                    half: "bottom".into(),
                })
            } else {
                None
            }
        });
        assert_eq!(shape, "straight");
    }
}
