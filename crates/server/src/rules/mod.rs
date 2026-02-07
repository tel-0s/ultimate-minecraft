pub mod block_updates;

use ultimate_engine::rules::RuleSet;

/// The standard Minecraft rule set: gravity + fluid spread.
pub fn standard() -> RuleSet {
    let mut rules = RuleSet::new();
    rules.add(block_updates::gravity);
    rules.add(block_updates::fluid_spread);
    rules
}
