pub mod block_updates;
pub mod helpers;

use ultimate_engine::rules::RuleSet;

/// The standard Minecraft rule set: gravity + water + lava.
pub fn standard() -> RuleSet {
    let mut rules = RuleSet::new();
    rules.add(block_updates::gravity);
    rules.add(block_updates::water_spread);
    rules.add(block_updates::lava_spread);
    rules
}
