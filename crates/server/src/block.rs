//! Minecraft block type definitions and property lookups.
//!
//! BlockId values are MC block state IDs (from azalea-block), so they can be
//! used directly in protocol chunk data without any mapping layer.

use ultimate_engine::world::block::BlockId;

// ── MC block state IDs (from azalea-block for MC 1.21.11) ────────────────
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
pub const WATER: BlockId = BlockId(86);

/// Source lava block: `lava[level=0]` (block state 102, verified via azalea).
pub const LAVA: BlockId = BlockId(102);

// ── Fluid abstraction ────────────────────────────────────────────────────

/// Which kind of fluid a block ID belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FluidKind {
    Water,
    Lava,
}

impl FluidKind {
    /// Base block-state ID for this fluid (level 0 = source).
    const fn base_id(self) -> u16 {
        match self {
            FluidKind::Water => 86,
            FluidKind::Lava => 102,
        }
    }

    /// Maximum horizontal spread distance.
    /// Water: 7 blocks.  Lava: 3 blocks (overworld).
    pub const fn max_spread(self) -> u8 {
        match self {
            FluidKind::Water => 7,
            FluidKind::Lava => 3,
        }
    }

    /// Source block for this fluid (level 0).
    pub const fn source(self) -> BlockId {
        BlockId(self.base_id())
    }

    /// Block ID for this fluid at a given level (0-15, clamped).
    pub const fn at_level(self, level: u8) -> BlockId {
        let l = if level > 15 { 15 } else { level };
        BlockId(self.base_id() + l as u16)
    }

    /// If `id` is this fluid, return its level (0-15). Otherwise `None`.
    pub const fn level(self, id: BlockId) -> Option<u8> {
        let base = self.base_id();
        if id.0 >= base && id.0 <= base + 15 {
            Some((id.0 - base) as u8)
        } else {
            None
        }
    }

    /// Does `id` belong to this fluid at any level?
    pub const fn is_match(self, id: BlockId) -> bool {
        let base = self.base_id();
        id.0 >= base && id.0 <= base + 15
    }
}

/// If `id` is any fluid, return which kind and its level.
pub fn fluid_kind(id: BlockId) -> Option<(FluidKind, u8)> {
    if let Some(l) = FluidKind::Water.level(id) {
        Some((FluidKind::Water, l))
    } else if let Some(l) = FluidKind::Lava.level(id) {
        Some((FluidKind::Lava, l))
    } else {
        None
    }
}

// ── Convenience wrappers (backward-compatible) ──────────────────────────

/// Is this any kind of fluid (water or lava)?
pub fn is_fluid(id: BlockId) -> bool {
    fluid_kind(id).is_some()
}

/// Get the water level (0-15) if this is a water block, `None` otherwise.
pub fn water_level(id: BlockId) -> Option<u8> {
    FluidKind::Water.level(id)
}

/// Create a water block at the given level (0-15).
pub fn water_at_level(level: u8) -> BlockId {
    FluidKind::Water.at_level(level)
}

/// Maximum horizontal spread for water.
pub fn water_max_spread() -> u8 {
    FluidKind::Water.max_spread()
}

/// Get the lava level (0-15) if this is a lava block, `None` otherwise.
pub fn lava_level(id: BlockId) -> Option<u8> {
    FluidKind::Lava.level(id)
}

/// Create a lava block at the given level (0-15).
pub fn lava_at_level(level: u8) -> BlockId {
    FluidKind::Lava.at_level(level)
}

/// Maximum horizontal spread for lava.
pub fn lava_max_spread() -> u8 {
    FluidKind::Lava.max_spread()
}

// ── Block property queries ──────────────────────────────────────────────

/// Does this block fall under gravity (like sand/gravel)?
pub fn has_gravity(id: BlockId) -> bool {
    id == SAND
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
            if let Some((kind, level)) = fluid_kind(id) {
                let fluid_name = match kind {
                    FluidKind::Water => "water",
                    FluidKind::Lava => "lava",
                };
                if level == 0 {
                    format!("{}(source)", fluid_name)
                } else {
                    format!("{}(lvl {})", fluid_name, level)
                }
            } else {
                format!("block#{}", id.0)
            }
        }
    }
}
