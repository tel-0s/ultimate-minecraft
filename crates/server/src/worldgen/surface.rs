//! Composable surface rules.
//!
//! A [`SurfaceRule`] decides what block to place at a position in the
//! "surface band" (the top few cells of a column). Rules form a tree of
//! atoms (`block`) and combinators (`sequence`, `condition`) walked with
//! a [`SurfaceContext`] holding the position, biome, surface Y, and sea
//! level.
//!
//! ## Tree semantics
//!
//! - [`SurfaceRule::try_apply`] returns `Option<BlockId>`. `None` means
//!   "this rule didn't fire" — the next sibling in a sequence gets to try.
//! - `block` always returns `Some(block_id)`.
//! - `condition { condition, rule }` evaluates the condition; if true,
//!   delegates to `rule`; otherwise returns `None`.
//! - `sequence { rules }` returns the first `Some` from the children in
//!   order, or `None` if all decline.
//!
//! Mirrors the shape of vanilla's `surface_rule` data files.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use ultimate_engine::world::block::BlockId;

use crate::block;
use super::biome::Biome;

/// Per-cell context handed to every surface rule on the way down the tree.
pub struct SurfaceContext {
    pub biome: Biome,
    pub x: i64,
    pub y: i64,
    pub z: i64,
    pub surface_y: i64,
    pub sea_level: i64,
}

impl SurfaceContext {
    /// Blocks below the surface (0 at the top block, positive going down).
    pub fn depth(&self) -> i64 {
        self.surface_y - self.y
    }
    pub fn at_surface(&self) -> bool {
        self.y == self.surface_y
    }
    /// Whether the surface itself is above (or at) sea level for this column.
    pub fn above_water(&self) -> bool {
        self.surface_y >= self.sea_level
    }
}

/// Compiled tree node. Trait-object so the pipeline can hold an
/// `Arc<dyn SurfaceRule>` without naming a concrete shape.
pub trait SurfaceRule: Send + Sync {
    fn try_apply(&self, ctx: &SurfaceContext) -> Option<BlockId>;
}

// ── Schema (JSON-driven) ────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SurfaceRuleSchema {
    /// Always places the named block.
    Block { block: String },
    /// Try each child in order; first `Some` wins.
    Sequence { rules: Vec<SurfaceRuleSchema> },
    /// Apply `rule` only when `condition` matches.
    Condition {
        condition: ConditionSchema,
        rule: Box<SurfaceRuleSchema>,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConditionSchema {
    /// `y == surface_y` — the topmost cell of the column.
    AtSurface,
    /// Depth below surface ≤ N (0 = the top block; 1 = one below; …).
    DepthAtMost { depth: i64 },
    /// Strictly above the given Y (`y > value`).
    AboveY { value: i64 },
    /// Strictly below the given Y (`y < value`).
    BelowY { value: i64 },
    /// Biome ∈ list.
    InBiome { biomes: Vec<Biome> },
    /// The surface itself sits at or above sea level (column is land).
    AboveWater,
    /// The surface itself sits below sea level (column is submerged).
    BelowWater,
}

// ── Compiled forms ──────────────────────────────────────────────────────────

struct BlockRule(BlockId);
impl SurfaceRule for BlockRule {
    fn try_apply(&self, _ctx: &SurfaceContext) -> Option<BlockId> {
        Some(self.0)
    }
}

struct SequenceRule(Vec<Arc<dyn SurfaceRule>>);
impl SurfaceRule for SequenceRule {
    fn try_apply(&self, ctx: &SurfaceContext) -> Option<BlockId> {
        for r in &self.0 {
            if let Some(b) = r.try_apply(ctx) {
                return Some(b);
            }
        }
        None
    }
}

struct ConditionRule {
    condition: Condition,
    rule: Arc<dyn SurfaceRule>,
}
impl SurfaceRule for ConditionRule {
    fn try_apply(&self, ctx: &SurfaceContext) -> Option<BlockId> {
        if self.condition.matches(ctx) {
            self.rule.try_apply(ctx)
        } else {
            None
        }
    }
}

enum Condition {
    AtSurface,
    DepthAtMost(i64),
    AboveY(i64),
    BelowY(i64),
    InBiome(Vec<Biome>),
    AboveWater,
    BelowWater,
}

impl Condition {
    fn matches(&self, ctx: &SurfaceContext) -> bool {
        match self {
            Self::AtSurface => ctx.at_surface(),
            Self::DepthAtMost(d) => ctx.depth() <= *d,
            Self::AboveY(v) => ctx.y > *v,
            Self::BelowY(v) => ctx.y < *v,
            Self::InBiome(bs) => bs.contains(&ctx.biome),
            Self::AboveWater => ctx.above_water(),
            Self::BelowWater => !ctx.above_water(),
        }
    }
}

// ── Build ───────────────────────────────────────────────────────────────────

impl SurfaceRuleSchema {
    pub fn build(&self) -> Result<Arc<dyn SurfaceRule>> {
        match self {
            Self::Block { block } => {
                let id = block::block_id_from_name(block)
                    .ok_or_else(|| anyhow!("unknown block {:?} in surface rule", block))?;
                Ok(Arc::new(BlockRule(id)))
            }
            Self::Sequence { rules } => {
                let compiled = rules.iter()
                    .map(|r| r.build())
                    .collect::<Result<Vec<_>>>()?;
                Ok(Arc::new(SequenceRule(compiled)))
            }
            Self::Condition { condition, rule } => {
                Ok(Arc::new(ConditionRule {
                    condition: condition.build(),
                    rule: rule.build()?,
                }))
            }
        }
    }
}

impl ConditionSchema {
    fn build(&self) -> Condition {
        match self {
            Self::AtSurface => Condition::AtSurface,
            Self::DepthAtMost { depth } => Condition::DepthAtMost(*depth),
            Self::AboveY { value } => Condition::AboveY(*value),
            Self::BelowY { value } => Condition::BelowY(*value),
            Self::InBiome { biomes } => Condition::InBiome(biomes.clone()),
            Self::AboveWater => Condition::AboveWater,
            Self::BelowWater => Condition::BelowWater,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(biome: Biome, y: i64, surface_y: i64, sea_level: i64) -> SurfaceContext {
        SurfaceContext { biome, x: 0, y, z: 0, surface_y, sea_level }
    }

    #[test]
    fn block_rule_always_fires() {
        let r = SurfaceRuleSchema::Block { block: "minecraft:stone".into() }.build().unwrap();
        let id = r.try_apply(&ctx(Biome::Plains, 70, 70, 63)).unwrap();
        assert_eq!(id, block::STONE);
    }

    #[test]
    fn condition_gates_rule() {
        let r = SurfaceRuleSchema::Condition {
            condition: ConditionSchema::AtSurface,
            rule: Box::new(SurfaceRuleSchema::Block { block: "minecraft:grass_block".into() }),
        }.build().unwrap();
        assert!(r.try_apply(&ctx(Biome::Plains, 70, 70, 63)).is_some()); // at surface
        assert!(r.try_apply(&ctx(Biome::Plains, 69, 70, 63)).is_none()); // below surface
    }

    #[test]
    fn sequence_picks_first_matching() {
        let r = SurfaceRuleSchema::Sequence {
            rules: vec![
                SurfaceRuleSchema::Condition {
                    condition: ConditionSchema::InBiome { biomes: vec![Biome::Desert] },
                    rule: Box::new(SurfaceRuleSchema::Block { block: "minecraft:sand".into() }),
                },
                SurfaceRuleSchema::Block { block: "minecraft:grass_block".into() },
            ],
        }.build().unwrap();
        assert_eq!(r.try_apply(&ctx(Biome::Desert, 70, 70, 63)).unwrap(), block::SAND);
        assert_eq!(r.try_apply(&ctx(Biome::Plains, 70, 70, 63)).unwrap(), block::GRASS_BLOCK);
    }

    #[test]
    fn depth_at_most_distinguishes_skin() {
        let r = SurfaceRuleSchema::Condition {
            condition: ConditionSchema::DepthAtMost { depth: 3 },
            rule: Box::new(SurfaceRuleSchema::Block { block: "minecraft:dirt".into() }),
        }.build().unwrap();
        // depth 0..3 → dirt; depth 4 → none.
        for depth in 0..=3i64 {
            assert!(r.try_apply(&ctx(Biome::Plains, 70 - depth, 70, 63)).is_some(),
                "depth {} should fire", depth);
        }
        assert!(r.try_apply(&ctx(Biome::Plains, 66, 70, 63)).is_none(), "depth 4 should not fire");
    }

    #[test]
    fn above_water_distinguishes_columns() {
        let r = SurfaceRuleSchema::Condition {
            condition: ConditionSchema::AboveWater,
            rule: Box::new(SurfaceRuleSchema::Block { block: "minecraft:grass_block".into() }),
        }.build().unwrap();
        assert!(r.try_apply(&ctx(Biome::Plains, 70, 70, 63)).is_some());  // land
        assert!(r.try_apply(&ctx(Biome::Ocean, 50, 50, 63)).is_none());   // submerged
    }

    #[test]
    fn unknown_block_in_rule_errors_at_build() {
        let bad = SurfaceRuleSchema::Block { block: "minecraft:nonexistent_block_xyz".into() };
        assert!(bad.build().is_err());
    }

    #[test]
    fn schema_round_trips_through_json() {
        let r = SurfaceRuleSchema::Sequence {
            rules: vec![
                SurfaceRuleSchema::Condition {
                    condition: ConditionSchema::InBiome { biomes: vec![Biome::Desert] },
                    rule: Box::new(SurfaceRuleSchema::Block { block: "minecraft:sand".into() }),
                },
                SurfaceRuleSchema::Block { block: "minecraft:grass_block".into() },
            ],
        };
        let json = serde_json::to_string(&r).unwrap();
        let parsed: SurfaceRuleSchema = serde_json::from_str(&json).unwrap();
        let built = parsed.build().unwrap();
        assert_eq!(built.try_apply(&ctx(Biome::Desert, 70, 70, 63)).unwrap(), block::SAND);
    }
}
