pub mod block_updates;
pub mod entity;
pub mod helpers;
pub mod light;

use ultimate_engine::rules::RuleSet;

/// The standard Minecraft rule set: gravity + water + lava + light +
/// entity kinematics (Phase 5).
pub fn standard() -> RuleSet {
    let mut rules = RuleSet::new();
    rules.add(block_updates::gravity);
    rules.add(block_updates::water_spread);
    rules.add(block_updates::lava_spread);
    rules.add(light::light_propagation);
    rules.add(entity::item_kinematics);
    rules.add(entity::entity_block_wake);
    rules
}
