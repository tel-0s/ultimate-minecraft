//! Drift guard for the GENERATED `src/block_ids.rs`.
//!
//! Every named constant and fluid table is re-derived here through the
//! compiled azalea version and compared against the checked-in generated
//! values. After an azalea (= MC protocol) bump this fails loudly; the fix
//! is `cargo run --example gen_block_ids`, never a hand edit.

use ultimate_server::{block_ids, registry};

#[test]
fn generated_constants_match_live_derivation() {
    assert_eq!(
        block_ids::GENERATED_TABLE.len(),
        registry::GENERATED_DEFAULTS.len(),
        "manifest changed — rerun: cargo run --example gen_block_ids"
    );
    for ((gen_name, gen_id), (name, block)) in block_ids::GENERATED_TABLE
        .iter()
        .zip(registry::GENERATED_DEFAULTS)
    {
        assert_eq!(gen_name, name, "manifest order changed — regenerate");
        let live = registry::block_id_from_name(block)
            .unwrap_or_else(|| panic!("{block} must resolve in this azalea version"));
        assert_eq!(
            *gen_id, live.0,
            "{name} ({block}) drifted: generated {gen_id}, azalea says {} — \
             rerun: cargo run --example gen_block_ids",
            live.0
        );
    }
}

#[test]
fn generated_fluid_tables_match_live_derivation() {
    assert_eq!(
        block_ids::WATER_LEVEL_IDS,
        registry::fluid_level_ids("water"),
        "water levels drifted — rerun: cargo run --example gen_block_ids"
    );
    assert_eq!(
        block_ids::LAVA_LEVEL_IDS,
        registry::fluid_level_ids("lava"),
        "lava levels drifted — rerun: cargo run --example gen_block_ids"
    );
}

#[test]
fn constants_are_the_states_they_claim() {
    use ultimate_server::block;
    assert_eq!(registry::block_name(block::SAND), "sand");
    assert_eq!(registry::block_name(block::WATER), "water");
    assert_eq!(registry::block_prop(block::WATER, "level"), Some("0"));
    assert_eq!(registry::block_name(block::OAK_LOG), "oak_log");
    assert_eq!(registry::block_prop(block::OAK_LOG, "axis"), Some("y"));
    assert_eq!(registry::block_name(block::LEAVES), "oak_leaves");
    // True default state (the old hand-written constant was
    // oak_leaves[distance=2], a subtly wrong non-default state).
    assert_eq!(registry::block_prop(block::LEAVES, "distance"), Some("7"));
}

#[test]
fn gravity_covers_the_vanilla_set() {
    use ultimate_server::block::has_gravity;
    for name in ["sand", "red_sand", "gravel", "white_concrete_powder", "anvil"] {
        let id = registry::block_id_from_name(name).unwrap();
        assert!(has_gravity(id), "{name} must have gravity");
    }
    for name in ["stone", "dirt", "oak_log", "water"] {
        let id = registry::block_id_from_name(name).unwrap();
        assert!(!has_gravity(id), "{name} must not have gravity");
    }
}
