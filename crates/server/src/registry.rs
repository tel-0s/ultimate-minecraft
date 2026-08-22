//! The version boundary: every name ↔ numeric-ID translation for the
//! current Minecraft protocol lives here (or in the generated
//! `block_ids.rs` this module's tests guard).
//!
//! Numeric block-state IDs and biome wire IDs are renumbered by Mojang in
//! essentially every Minecraft version. The rule that keeps upgrades sane:
//! **names are forever, numbers are per-version** — anything persistent
//! (saves), cross-version (cluster peers), or hand-maintained (constants)
//! must be name-based or generated, and the translation happens in exactly
//! one place: this module, backed by azalea's state tables for whatever MC
//! version the workspace is pinned to.
//!
//! Tables are built once (LazyLock) by walking azalea's full block-state
//! space — the same one-time-cost pattern as the light LUTs in `block.rs`.

use std::collections::HashMap;
use std::sync::LazyLock;

use azalea_block::{BlockState, BlockTrait};
use ultimate_engine::world::block::BlockId;

// ── Version facts (single home) ──────────────────────────────────────────

/// Vanilla DataVersion stamped into Anvil saves. The one version fact
/// azalea doesn't export: comes from the `version.json` inside the client
/// jar (or the minecraft.wiki page for the release). MC 26.2 = 4903
/// (26.1 was 4786, 1.21.11 was 4189).
pub const MC_DATA_VERSION: i32 = 4903;

/// Wire-format identity for cluster peers: two nodes may only link when
/// they agree on this. Combines the MC protocol version (block/biome ID
/// spaces) with our own payload-codec revision (bump when
/// `cluster::encode_payload` changes shape).
pub const CLUSTER_CODEC_VERSION: u32 = 1;
pub fn cluster_wire_version() -> u64 {
    ((azalea_protocol::packets::PROTOCOL_VERSION as u64) << 32) | CLUSTER_CODEC_VERSION as u64
}

// ── Block-state tables ───────────────────────────────────────────────────

/// Sorted-property list, the canonical property representation everywhere
/// in this module.
pub type Props = Vec<(String, String)>;

/// Forward table: state id → (bare name, sorted properties). Precomputed
/// so hot paths (redstone property queries, palette building) never touch
/// azalea's `Box<dyn BlockTrait>`-per-call API.
static BLOCK_PARTS: LazyLock<Vec<(&'static str, Props)>> = LazyLock::new(|| {
    (0..=BlockState::MAX_STATE)
        .map(|raw| {
            let state = BlockState::try_from(raw).expect("id in range");
            let block: &dyn BlockTrait = state.to_trait();
            // azalea's names are 'static in disguise (string literals);
            // leaking the Box would also work, but `id()` returns &str
            // borrowed from the trait object — intern via leak once.
            let name: &'static str = Box::leak(block.id().to_string().into_boxed_str());
            let mut props: Props = block
                .property_map()
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            props.sort();
            (name, props)
        })
        .collect()
});

/// Reverse table: (bare name, sorted properties) → state id.
static BLOCK_LOOKUP: LazyLock<HashMap<(String, Props), BlockId>> = LazyLock::new(|| {
    BLOCK_PARTS
        .iter()
        .enumerate()
        .map(|(id, (name, props))| ((name.to_string(), props.clone()), BlockId(id as u16)))
        .collect()
});

/// Force both block tables to build now (bulk callers — e.g. save
/// loading — pay the one-time cost up front instead of mid-work).
pub fn warm_block_tables() {
    let _ = &*BLOCK_PARTS;
    let _ = &*BLOCK_LOOKUP;
}

/// Bare name (no `minecraft:` prefix) and sorted properties of a state.
/// Borrowed from the one-time table — no per-call allocation.
pub fn block_parts(id: BlockId) -> Option<(&'static str, &'static Props)> {
    BLOCK_PARTS.get(id.0 as usize).map(|(n, p)| (*n, p))
}

/// Bare block name of a state (`""` for out-of-range ids).
pub fn block_name(id: BlockId) -> &'static str {
    block_parts(id).map(|(n, _)| n).unwrap_or("")
}

/// One property's value on a state.
pub fn block_prop(id: BlockId, key: &str) -> Option<&'static str> {
    block_parts(id)?
        .1
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Look up a state id by bare name + **sorted** property list.
pub fn lookup_block_state(name: &str, props: &[(String, String)]) -> Option<BlockId> {
    BLOCK_LOOKUP
        .get(&(name.to_string(), props.to_vec()))
        .copied()
}

/// The same block with some properties changed (None if that combination
/// doesn't exist in the state table).
pub fn with_props(id: BlockId, changes: &[(&str, &str)]) -> Option<BlockId> {
    let (name, props) = block_parts(id)?;
    let mut props = props.clone();
    for (key, value) in changes {
        match props.iter_mut().find(|(k, _)| k == key) {
            Some((_, v)) => *v = value.to_string(),
            None => return None,
        }
    }
    props.sort();
    lookup_block_state(name, &props)
}

/// Default-state id for a block name (with or without `minecraft:`).
/// This is the workhorse for every name-driven config surface (worldgen
/// presets, generated constants, delta-save palettes).
pub fn block_id_from_name(name: &str) -> Option<BlockId> {
    use azalea_registry::builtin::BlockKind;
    use std::str::FromStr;

    let bare = name.strip_prefix("minecraft:").unwrap_or(name);
    let kind = BlockKind::from_str(bare).ok()?;
    let state: u32 = BlockState::from(kind).into();
    Some(BlockId(state as u16))
}

// ── Generated-constant manifest ──────────────────────────────────────────
//
// `src/block_ids.rs` is GENERATED from this manifest by
// `cargo run --example gen_block_ids` and guarded by `tests/block_ids.rs`,
// which re-derives every value through azalea and fails loudly when an
// azalea (= MC protocol) bump renumbers the state space. The fix is never
// hand-editing: rerun the generator.

/// `(CONST_NAME, block_name)` — each becomes a `pub const` holding the
/// block's default-state id.
pub const GENERATED_DEFAULTS: &[(&str, &str)] = &[
    ("AIR", "air"),
    ("STONE", "stone"),
    ("GRASS_BLOCK", "grass_block"), // snowy=false is the default state
    ("DIRT", "dirt"),
    ("BEDROCK", "bedrock"),
    ("SAND", "sand"),
    ("OAK_LOG", "oak_log"), // axis=y is the default state
    ("LEAVES", "oak_leaves"),
    ("WATER", "water"), // level=0 (source) is the default state
    ("LAVA", "lava"),
];

/// State ids of a fluid at level 0..=15, derived per level — no
/// assumption that the 16 states are contiguous (they are today; a future
/// version needn't keep them so).
pub fn fluid_level_ids(name: &str) -> [u16; 16] {
    std::array::from_fn(|level| {
        lookup_block_state(name, &[("level".into(), level.to_string())])
            .unwrap_or_else(|| panic!("{name}[level={level}] must exist"))
            .0
    })
}

// ── Biome wire IDs ───────────────────────────────────────────────────────

/// Namespaced names of a derived data registry, in azalea's (= vanilla
/// data-pack) order.
pub fn registry_names<K: Clone>(all: &[K]) -> Vec<String>
where
    azalea_registry::identifier::Identifier: From<K>,
{
    all.iter()
        .map(|k| azalea_registry::identifier::Identifier::from(k.clone()).to_string())
        .collect()
}

/// The `minecraft:worldgen/biome` registry the server declares during
/// configuration, in azalea's (= vanilla data-pack) order. **This list
/// defines the numeric biome IDs in every chunk packet** — the client
/// indexes into the registry exactly as sent. Derived, not hand-written,
/// so an azalea version bump renumbers both sides together.
pub static BIOME_REGISTRY: LazyLock<Vec<String>> =
    LazyLock::new(|| registry_names(azalea_registry::data::BiomeKey::ALL));

/// The `minecraft:world_clock` registry (new in MC 26.x): the client's
/// dimension data references per-dimension clocks, so configuration
/// FAILS on a vanilla client unless the server declares these
/// ("Unbound values in registry minecraft:world_clock"). Found the hard
/// way: azalea-based smoke clients don't run vanilla's registry
/// validation, so only a real client caught it.
pub static WORLD_CLOCK_REGISTRY: LazyLock<Vec<String>> =
    LazyLock::new(|| registry_names(azalea_registry::data::WorldClockKey::ALL));

static BIOME_WIRE_IDS: LazyLock<HashMap<&'static str, u32>> = LazyLock::new(|| {
    BIOME_REGISTRY
        .iter()
        .enumerate()
        .map(|(i, name)| (name.as_str(), i as u32))
        .collect()
});

/// Wire id of a namespaced biome name (`"minecraft:plains"`), as declared
/// by [`BIOME_REGISTRY`].
pub fn biome_wire_id(name: &str) -> Option<u32> {
    BIOME_WIRE_IDS.get(name).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_reverse_roundtrip() {
        for raw in [0u16, 1, 86, 118, 259] {
            let id = BlockId(raw);
            let (name, props) = block_parts(id).expect("in range");
            assert_eq!(
                lookup_block_state(name, props),
                Some(id),
                "roundtrip failed for {name}"
            );
        }
    }

    #[test]
    fn default_states_resolve() {
        assert_eq!(block_id_from_name("minecraft:air"), Some(BlockId(0)));
        assert_eq!(block_id_from_name("stone"), block_id_from_name("minecraft:stone"));
        assert!(block_id_from_name("not_a_block").is_none());
    }

    #[test]
    fn with_props_changes_one_axis() {
        let log = block_id_from_name("oak_log").unwrap();
        let x_axis = with_props(log, &[("axis", "x")]).unwrap();
        assert_ne!(log, x_axis);
        assert_eq!(block_prop(x_axis, "axis"), Some("x"));
        assert_eq!(block_name(x_axis), "oak_log");
        assert!(with_props(log, &[("no_such_prop", "1")]).is_none());
    }

    #[test]
    fn world_clock_registry_has_the_referenced_clocks() {
        // The client's overworld/end dimension data references exactly
        // these; missing either reproduces the connect-refusal.
        assert_eq!(
            *WORLD_CLOCK_REGISTRY,
            vec!["minecraft:overworld".to_string(), "minecraft:the_end".to_string()],
        );
    }

    #[test]
    fn biome_registry_is_derived_and_consistent() {
        assert!(BIOME_REGISTRY.len() >= 60, "vanilla ships ~65 biomes");
        assert!(BIOME_REGISTRY.iter().all(|n| n.starts_with("minecraft:")));
        for (i, name) in BIOME_REGISTRY.iter().enumerate() {
            assert_eq!(biome_wire_id(name), Some(i as u32));
        }
        assert!(biome_wire_id("minecraft:plains").is_some());
    }
}
