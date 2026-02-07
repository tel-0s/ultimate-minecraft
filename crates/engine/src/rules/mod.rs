use crate::causal::event::{Event, EventPayload};
use crate::world::World;

/// A rule function: given the current world state and an event that just
/// occurred, produce zero or more consequent events.
///
/// Rules must be **local**: they only read blocks in a bounded neighborhood
/// of the event's position. This locality is what makes causal independence
/// (and therefore parallelism) possible.
pub type RuleFn = fn(&World, &EventPayload) -> Vec<Event>;

/// An ordered collection of rules. When an event is executed, every rule
/// is consulted; their outputs are merged into the causal graph as children
/// of the triggering event.
pub struct RuleSet {
    rules: Vec<RuleFn>,
}

impl RuleSet {
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    pub fn add(&mut self, rule: RuleFn) {
        self.rules.push(rule);
    }

    pub fn evaluate(&self, world: &World, payload: &EventPayload) -> Vec<Event> {
        let mut out = Vec::new();
        for rule in &self.rules {
            out.extend(rule(world, payload));
        }
        out
    }
}

impl Default for RuleSet {
    fn default() -> Self {
        Self::new()
    }
}
