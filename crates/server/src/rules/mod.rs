pub mod block_updates;
pub mod helpers;
pub mod light;

use ultimate_engine::rules::RuleSet;

/// The standard Minecraft rule set: gravity + water + lava + light.
pub fn standard() -> RuleSet {
    let mut rules = RuleSet::new();
    rules.add(block_updates::gravity);
    rules.add(block_updates::water_spread);
    rules.add(block_updates::lava_spread);
    rules.add(light::light_propagation);
    rules
}
