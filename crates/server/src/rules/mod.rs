pub mod attachment;
pub mod block_updates;
pub mod entity;
pub mod helpers;
pub mod light;
pub mod mob;
pub mod piston;
pub mod redstone;

use ultimate_engine::rules::RuleSet;

/// The standard Minecraft rule set: gravity + water + lava + light +
/// entity kinematics (Phase 5). Gravity here is the INSTANT rule (cell
/// teleport) — the benchmark workhorse and the causal-invariance test
/// substrate.
pub fn standard() -> RuleSet {
    let mut rules = RuleSet::new();
    rules.add(block_updates::gravity);
    rules.add(block_updates::water_spread);
    rules.add(block_updates::lava_spread);
    rules.add(light::light_propagation);
    rules.add(entity::item_kinematics);
    rules.add(mob::mob_ai);
    rules.add(entity::entity_block_wake);
    rules.add(redstone::redstone);
    rules.add(piston::piston);
    rules.add(attachment::attachment_support);
    rules
}

/// `standard()` with vanilla-parity falling-block ENTITIES instead of
/// instant gravity: unsupported sand detaches, falls as a visible
/// trajectory, and re-lands as a block. Final block state is identical to
/// the instant rule; the pacing is real. Selected by
/// `physics.falling_block_entities` in server.yaml.
pub fn standard_with_falling_blocks() -> RuleSet {
    let mut rules = RuleSet::new();
    rules.add(entity::falling_block_gravity);
    rules.add(block_updates::water_spread);
    rules.add(block_updates::lava_spread);
    rules.add(light::light_propagation);
    rules.add(entity::item_kinematics);
    rules.add(entity::falling_block_kinematics);
    rules.add(mob::mob_ai);
    rules.add(entity::entity_block_wake);
    rules.add(redstone::redstone);
    rules.add(piston::piston);
    rules.add(attachment::attachment_support);
    rules
}
