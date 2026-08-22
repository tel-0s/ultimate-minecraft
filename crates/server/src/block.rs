//! Minecraft block type definitions and property lookups.
//!
//! BlockId values are MC block state IDs (from azalea-block), so they can be
//! used directly in protocol chunk data without any mapping layer. The
//! named constants are GENERATED (`src/block_ids.rs`, regenerated with
//! `cargo run --example gen_block_ids`) so an azalea version bump can
//! never leave a stale hand-written number behind; name↔id translation
//! itself lives in `crate::registry`.

use ultimate_engine::world::block::BlockId;

pub use crate::block_ids::*;

// ── Fluid abstraction ────────────────────────────────────────────────────

/// Which kind of fluid a block ID belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FluidKind {
    Water,
    Lava,
}

impl FluidKind {
    /// Per-level state ids (generated; no contiguity assumption).
    const fn level_ids(self) -> &'static [u16; 16] {
        match self {
            FluidKind::Water => &WATER_LEVEL_IDS,
            FluidKind::Lava => &LAVA_LEVEL_IDS,
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
        BlockId(self.level_ids()[0])
    }

    /// Block ID for this fluid at a given level (0-15, clamped).
    pub const fn at_level(self, level: u8) -> BlockId {
        let l = if level > 15 { 15 } else { level };
        BlockId(self.level_ids()[l as usize])
    }

    /// If `id` is this fluid, return its level (0-15). Otherwise `None`.
    pub const fn level(self, id: BlockId) -> Option<u8> {
        let ids = self.level_ids();
        let mut l = 0;
        while l < 16 {
            if ids[l] == id.0 {
                return Some(l as u8);
            }
            l += 1;
        }
        None
    }

    /// Does `id` belong to this fluid at any level?
    pub const fn is_match(self, id: BlockId) -> bool {
        self.level(id).is_some()
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

/// Does this block fall under gravity? Name-derived LUT over the whole
/// state space (sand, gravel, concrete powders, anvils, ...), same
/// pattern as the light LUTs below.
pub fn has_gravity(id: BlockId) -> bool {
    static GRAVITY_LUT: std::sync::LazyLock<Box<[bool]>> = std::sync::LazyLock::new(|| {
        (0..=azalea_block::BlockState::MAX_STATE)
            .map(|raw| {
                let name = crate::registry::block_name(BlockId(raw as u16));
                matches!(
                    name,
                    "sand" | "red_sand" | "gravel" | "suspicious_sand" | "suspicious_gravel"
                        | "anvil" | "chipped_anvil" | "damaged_anvil"
                        | "dragon_egg" | "scaffolding"
                ) || name.ends_with("_concrete_powder")
            })
            .collect()
    });
    GRAVITY_LUT.get(id.0 as usize).copied().unwrap_or(false)
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
//
// The `*_uncached` functions resolve properties through azalea's
// `Box<dyn BlockTrait>` — a heap allocation plus string matching PER
// CALL, which dominated the light BFS inner loop (~84K property queries
// per torch placement). The public functions read one-time lookup tables
// built over the whole block-state space (~2 × 27 KB) at first use.

static LIGHT_EMISSION_LUT: std::sync::LazyLock<Box<[u8]>> = std::sync::LazyLock::new(|| {
    (0..=azalea_block::BlockState::MAX_STATE)
        .map(|raw| light_emission_uncached(BlockId(raw as u16)))
        .collect()
});

static LIGHT_OPACITY_LUT: std::sync::LazyLock<Box<[u8]>> = std::sync::LazyLock::new(|| {
    (0..=azalea_block::BlockState::MAX_STATE)
        .map(|raw| light_opacity_uncached(BlockId(raw as u16)))
        .collect()
});

/// How much light this block emits (0-15). LUT-backed; O(1).
#[inline]
pub fn light_emission(id: BlockId) -> u8 {
    LIGHT_EMISSION_LUT.get(id.0 as usize).copied().unwrap_or(0)
}

/// How much light this block absorbs (0-15). LUT-backed; O(1).
#[inline]
pub fn light_opacity(id: BlockId) -> u8 {
    LIGHT_OPACITY_LUT.get(id.0 as usize).copied().unwrap_or(15)
}

/// How much light this block emits (0-15).
fn light_emission_uncached(id: BlockId) -> u8 {
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
fn light_opacity_uncached(id: BlockId) -> u8 {
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

/// Look up the *default-state* `BlockId` by Minecraft name (re-exported
/// from the registry; used by worldgen presets that name blocks via JSON).
pub use crate::registry::block_id_from_name;

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
