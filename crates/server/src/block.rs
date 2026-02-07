//! Minecraft block type definitions and property lookups.
//!
//! The engine stores opaque `BlockId` values. This module gives them meaning
//! for Minecraft: sand has gravity, water is fluid, bedrock is indestructible, etc.

use ultimate_engine::world::block::BlockId;

// -- Named constants for the block types we support so far. --
// These IDs are arbitrary within our server; they don't need to match
// Minecraft's internal state IDs (that mapping happens at the protocol layer).

pub const AIR: BlockId = BlockId(0);
pub const STONE: BlockId = BlockId(1);
pub const DIRT: BlockId = BlockId(2);
pub const GRASS: BlockId = BlockId(3);
pub const SAND: BlockId = BlockId(4);
pub const WATER: BlockId = BlockId(5);
pub const BEDROCK: BlockId = BlockId(6);
pub const LOG: BlockId = BlockId(7);
pub const LEAVES: BlockId = BlockId(8);

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
