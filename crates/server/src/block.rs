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

// ── Light property queries ──────────────────────────────────────────────

/// How much light this block emits (0-15).
pub fn light_emission(id: BlockId) -> u8 {
    use azalea_block::{BlockState, BlockTrait};

    // Fast path: air and common solid blocks never emit light.
    if id == AIR || id == STONE || id == DIRT || id == BEDROCK || id == GRASS_BLOCK {
        return 0;
    }

    let state = match BlockState::try_from(id.0 as u32) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let block: Box<dyn BlockTrait> = Box::<dyn BlockTrait>::from(state);
    let name = block.id();

    // azalea's BlockTrait::id() returns the bare name (e.g. "torch"),
    // NOT the namespaced form ("minecraft:torch").
    match name {
        "glowstone"
        | "jack_o_lantern"
        | "lantern"
        | "sea_lantern"
        | "shroomlight"
        | "beacon"
        | "conduit"
        | "end_gateway"
        | "end_portal"
        | "fire"
        | "soul_fire"
        | "redstone_lamp" => 15,

        "lava" => 15,

        "torch" | "wall_torch" => 14,
        "soul_torch" | "soul_wall_torch" => 10,
        "soul_lantern" => 10,

        "crying_obsidian" | "end_rod" => 14,

        "blast_furnace" | "furnace" | "smoker" => {
            let props = block.property_map();
            let lit = props
                .iter()
                .find(|(k, _)| **k == "lit")
                .map(|(_, v)| *v == "true")
                .unwrap_or(false);
            if lit { 13 } else { 0 }
        }

        "campfire" => {
            let props = block.property_map();
            let lit = props
                .iter()
                .find(|(k, _)| **k == "lit")
                .map(|(_, v)| *v == "true")
                .unwrap_or(false);
            if lit { 15 } else { 0 }
        }
        "soul_campfire" => {
            let props = block.property_map();
            let lit = props
                .iter()
                .find(|(k, _)| **k == "lit")
                .map(|(_, v)| *v == "true")
                .unwrap_or(false);
            if lit { 10 } else { 0 }
        }

        "redstone_torch" | "redstone_wall_torch" => 7,

        "enchanting_table" | "ender_chest" => 7,
        "magma_block" => 3,
        "brewing_stand" => 1,
        "brown_mushroom" => 1,
        "dragon_egg" => 1,

        _ => 0,
    }
}

/// How much light this block absorbs when light passes through (0-15).
/// 0 = fully transparent (air, glass, flowers, etc.)
/// 15 = fully opaque (stone, dirt, etc.)
/// 1 = slightly attenuating (water, ice, leaves)
pub fn light_opacity(id: BlockId) -> u8 {
    use azalea_block::{BlockState, BlockTrait};

    // Fast path: the vast majority of blocks hit during light propagation
    // are air (transparent) or common solid blocks (fully opaque).
    if id == AIR { return 0; }
    if id == STONE || id == DIRT || id == BEDROCK || id == GRASS_BLOCK {
        return 15;
    }

    let state = match BlockState::try_from(id.0 as u32) {
        Ok(s) => s,
        Err(_) => return 15,
    };
    let block: Box<dyn BlockTrait> = Box::<dyn BlockTrait>::from(state);
    let name = block.id();

    // azalea's BlockTrait::id() returns the bare name (e.g. "torch"),
    // NOT the namespaced form ("minecraft:torch").
    match name {
        "air" | "cave_air" | "void_air" => 0,

        n if n.ends_with("_stained_glass")
            || n.ends_with("_stained_glass_pane")
            || n == "glass"
            || n == "glass_pane"
            || n == "tinted_glass" => 0,

        // Torches
        "torch" | "wall_torch"
        | "soul_torch" | "soul_wall_torch"
        | "redstone_torch" | "redstone_wall_torch"
        | "end_rod" => 0,

        // Water / lava
        "water" | "lava" => 1,

        // Leaves
        n if n.ends_with("_leaves") => 1,

        // Ice
        "ice" | "frosted_ice"
        | "packed_ice" | "blue_ice" => 1,

        "slime_block" | "honey_block" => 1,

        // Non-solid / partial blocks: use name-based heuristics
        n if n.ends_with("_sapling")
            || n.ends_with("_button")
            || n.ends_with("_pressure_plate")
            || n.ends_with("_sign")
            || n.ends_with("_wall_sign")
            || n.ends_with("_hanging_sign")
            || n.ends_with("_wall_hanging_sign")
            || n.ends_with("_fence")
            || n.ends_with("_fence_gate")
            || n.ends_with("_slab")
            || n.ends_with("_stairs")
            || n.ends_with("_wall")
            || n.ends_with("_carpet")
            || n.ends_with("_trapdoor")
            || n.ends_with("_door")
            || n.ends_with("_bed")
            || n.ends_with("_candle")
            || n.ends_with("_banner")
            || n.ends_with("_wall_banner") => 0,

        // Flowers / grass / plants
        "dandelion" | "poppy" | "blue_orchid"
        | "allium" | "azure_bluet"
        | "red_tulip" | "orange_tulip"
        | "white_tulip" | "pink_tulip"
        | "oxeye_daisy" | "cornflower"
        | "lily_of_the_valley" | "wither_rose"
        | "sunflower" | "lilac"
        | "rose_bush" | "peony"
        | "short_grass" | "tall_grass"
        | "fern" | "large_fern"
        | "dead_bush" | "sugar_cane"
        | "vine" | "kelp" | "kelp_plant"
        | "bamboo" | "bamboo_sapling"
        | "sweet_berry_bush" => 0,

        // Rails
        "rail" | "powered_rail"
        | "detector_rail" | "activator_rail" => 0,

        // Redstone
        "redstone_wire" | "lever"
        | "repeater" | "comparator" => 0,

        // Misc transparent / partial
        "ladder" | "snow" | "cobweb"
        | "barrier" | "chest" | "trapped_chest"
        | "ender_chest" | "enchanting_table"
        | "brewing_stand" | "anvil"
        | "chipped_anvil" | "damaged_anvil"
        | "hopper" | "cauldron"
        | "grindstone" | "lectern"
        | "bell" | "lantern" | "soul_lantern"
        | "chain" | "conduit" | "beacon" => 0,

        // Crops
        "wheat" | "carrots" | "potatoes"
        | "beetroots" | "melon_stem"
        | "pumpkin_stem" => 0,

        // Fire
        "fire" | "soul_fire"
        | "campfire" | "soul_campfire" => 0,

        _ => {
            if is_replaceable(id) { 0 } else { 15 }
        }
    }
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
