//! Minecraft block type definitions and property lookups.
//!
//! BlockId values are MC block state IDs (from azalea-block), so they can be
//! used directly in protocol chunk data without any mapping layer.

use ultimate_engine::world::block::BlockId;

// -- MC block state IDs (from azalea-block for MC 1.21.11) --
// These match the vanilla protocol, so BlockId can be used directly in chunks.

pub const AIR: BlockId = BlockId(0);
pub const STONE: BlockId = BlockId(1);
pub const GRASS_BLOCK: BlockId = BlockId(9);  // snowy=false
pub const DIRT: BlockId = BlockId(10);
pub const BEDROCK: BlockId = BlockId(85);
pub const SAND: BlockId = BlockId(118);
pub const OAK_LOG: BlockId = BlockId(137);    // axis=y

// Legacy aliases for engine tests (which use small sequential IDs)
pub const GRASS: BlockId = GRASS_BLOCK;
pub const LOG: BlockId = OAK_LOG;
pub const LEAVES: BlockId = BlockId(259);     // oak_leaves default

/// Source water block: `water[level=0]` (block state 86).
/// Levels 0-15 are sequential: `water[level=N]` = 86 + N.
///   0     = source block (placed by player / infinite sources)
///   1-7   = flowing water (increasing level = thinner, further from source)
///   8-15  = falling variants (unused for now)
pub const WATER: BlockId = BlockId(86);       // water[level=0]
const WATER_BASE: u16 = 86;
const WATER_MAX_LEVEL: u8 = 7;

/// Does this block fall under gravity (like sand/gravel)?
pub fn has_gravity(id: BlockId) -> bool {
    id == SAND
}

/// Is this any kind of water (source or flowing)?
pub fn is_fluid(id: BlockId) -> bool {
    water_level(id).is_some()
}

/// Get the water level (0-15) if this is a water block, None otherwise.
pub fn water_level(id: BlockId) -> Option<u8> {
    if id.0 >= WATER_BASE && id.0 <= WATER_BASE + 15 {
        Some((id.0 - WATER_BASE) as u8)
    } else {
        None
    }
}

/// Create a water block at the given level (0-15).
pub fn water_at_level(level: u8) -> BlockId {
    BlockId(WATER_BASE + level.min(15) as u16)
}

/// The maximum horizontal spread level. Water at this level doesn't spread further.
pub fn water_max_spread() -> u8 {
    WATER_MAX_LEVEL
}

/// Can another block be placed in this space?
pub fn is_replaceable(id: BlockId) -> bool {
    id == AIR || is_fluid(id)
}

/// Is this block fully solid?
pub fn is_solid(id: BlockId) -> bool {
    !is_replaceable(id)
}

/// Human-readable name for dashboard display.
pub fn name(id: BlockId) -> String {
    match id {
        AIR => "air".into(),
        STONE => "stone".into(),
        GRASS_BLOCK => "grass_block".into(),
        DIRT => "dirt".into(),
        BEDROCK => "bedrock".into(),
        SAND => "sand".into(),
        OAK_LOG => "oak_log".into(),
        LEAVES => "oak_leaves".into(),
        _ => {
            if let Some(level) = water_level(id) {
                if level == 0 {
                    "water(source)".into()
                } else {
                    format!("water(lvl {})", level)
                }
            } else {
                format!("block#{}", id.0)
            }
        }
    }
}
