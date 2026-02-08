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
pub const WATER: BlockId = BlockId(80);       // level=0

/// Does this block fall under gravity (like sand/gravel)?
pub fn has_gravity(id: BlockId) -> bool {
    id == SAND
}

/// Is this a fluid that spreads?
pub fn is_fluid(id: BlockId) -> bool {
    id == WATER
}

/// Can another block be placed in this space?
pub fn is_replaceable(id: BlockId) -> bool {
    id == AIR || id == WATER
}

/// Is this block fully solid?
pub fn is_solid(id: BlockId) -> bool {
    !is_replaceable(id)
}
