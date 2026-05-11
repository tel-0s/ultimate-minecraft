//! Worldgen presets. A preset is either:
//!
//! - **Built-in:** referenced by name (`"noise"`, `"superflat"`), embedded
//!   in the binary via `include_str!`.
//! - **Operator-supplied:** a path to a JSON file on disk.
//!
//! Two preset kinds exist:
//!
//! ## `density`
//! Compositional density-function pipeline with biomes + surface rules.
//!
//! ```json
//! { "kind": "density",
//!   "sea_level": 63, "min_y": -64, "max_y": 319, "bedrock_y": 0,
//!   "density":     { ... density function tree ... },
//!   "biome_source": { "type": "multi_noise", "temperature": {...}, "humidity": {...} },
//!   "surface_rule": { "type": "sequence", "rules": [ ... ] } }
//! ```
//!
//! ## `flat`
//! Superflat: a bedrock floor + a stack of fixed layers per column.
//!
//! ```json
//! { "kind": "flat", "min_y": 0, "biome": "plains",
//!   "layers": [ { "block": "minecraft:bedrock", "height": 1 }, ... ] }
//! ```

use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use crate::block;
use super::WorldGen;
use super::biome::Biome;
use super::climate::BiomeSourceSchema;
use super::density::DensityFnSchema;
use super::pipeline::{DensityPipeline, FlatPipeline};
use super::surface::SurfaceRuleSchema;

// ── Built-in preset bodies (embedded JSON) ──────────────────────────────────

pub const BUILTIN_NOISE: &str = include_str!("presets/noise.json");
pub const BUILTIN_SUPERFLAT: &str = include_str!("presets/superflat.json");

// ── Schema ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PresetSchema {
    Density(DensityPresetSchema),
    Flat(FlatPresetSchema),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DensityPresetSchema {
    pub sea_level: i64,
    pub min_y: i64,
    pub max_y: i64,
    pub bedrock_y: i64,
    #[serde(default = "default_skin_depth")]
    pub skin_depth: i64,
    pub density: DensityFnSchema,
    pub biome_source: BiomeSourceSchema,
    pub surface_rule: SurfaceRuleSchema,
}

fn default_skin_depth() -> i64 { 4 }

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FlatPresetSchema {
    pub min_y: i64,
    pub layers: Vec<FlatLayer>,
    #[serde(default = "default_flat_biome")]
    pub biome: Biome,
}

fn default_flat_biome() -> Biome { Biome::Plains }

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FlatLayer {
    pub block: String,
    pub height: i64,
}

// ── Loading ─────────────────────────────────────────────────────────────────

/// Load a preset by spec — either a built-in name (`"noise"`,
/// `"superflat"`) or a path to a JSON file.
pub fn load(spec: &str, seed: u32) -> Result<Arc<dyn WorldGen>> {
    let (source, json) = match spec {
        "noise" => ("builtin:noise".to_string(), BUILTIN_NOISE.to_string()),
        "superflat" => ("builtin:superflat".to_string(), BUILTIN_SUPERFLAT.to_string()),
        path => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| anyhow!("reading worldgen preset {}: {}", path, e))?;
            (format!("file:{}", path), text)
        }
    };
    let schema: PresetSchema = serde_json::from_str(&json)
        .map_err(|e| anyhow!("parsing worldgen preset {}: {}", source, e))?;
    schema.build(seed)
}

impl PresetSchema {
    pub fn build(self, seed: u32) -> Result<Arc<dyn WorldGen>> {
        match self {
            Self::Density(d) => {
                let heightmap_shortcut = d.density.as_heightmap().map(|h| h.build(seed));
                let density = d.density.build(seed);
                let biome_source = d.biome_source.build(seed);
                let surface_rule = d.surface_rule.build()?;
                Ok(Arc::new(DensityPipeline {
                    density,
                    heightmap_shortcut,
                    biome_source,
                    surface_rule,
                    sea_level: d.sea_level,
                    min_y: d.min_y,
                    max_y: d.max_y,
                    bedrock_y: d.bedrock_y,
                    skin_depth: d.skin_depth,
                }))
            }
            Self::Flat(f) => {
                let mut layers = Vec::with_capacity(f.layers.len());
                for layer in &f.layers {
                    let block_id = block::block_id_from_name(&layer.block)
                        .ok_or_else(|| anyhow!("unknown block {:?} in flat preset", layer.block))?;
                    if layer.height < 0 {
                        return Err(anyhow!("flat preset layer height must be non-negative"));
                    }
                    layers.push((block_id, layer.height));
                }
                Ok(Arc::new(FlatPipeline {
                    min_y: f.min_y,
                    layers,
                    biome: f.biome,
                }))
            }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ultimate_engine::world::position::LocalBlockPos;

    #[test]
    fn builtin_noise_parses_and_builds() {
        let schema: PresetSchema = serde_json::from_str(BUILTIN_NOISE).unwrap();
        let w = schema.build(42).unwrap();
        let chunk = w.generate_chunk(0, 0);
        assert!(chunk.section_count() > 0, "noise preset should produce non-empty chunks");
    }

    #[test]
    fn builtin_superflat_parses_and_builds() {
        let schema: PresetSchema = serde_json::from_str(BUILTIN_SUPERFLAT).unwrap();
        let w = schema.build(0).unwrap();
        let chunk = w.generate_chunk(0, 0);
        assert_eq!(
            chunk.get_block(LocalBlockPos { x: 0, y: 0, z: 0 }),
            block::BEDROCK,
        );
    }

    #[test]
    fn deterministic_from_seed() {
        let w1 = load("noise", 42).unwrap();
        let w2 = load("noise", 42).unwrap();
        let c_a = w1.generate_chunk(3, 7);
        let c_b = w2.generate_chunk(3, 7);
        for lx in 0..16u8 {
            for lz in 0..16u8 {
                for y in 0..=70i64 {
                    let pos = LocalBlockPos { x: lx, y, z: lz };
                    assert_eq!(c_a.get_block(pos), c_b.get_block(pos),
                        "mismatch at ({},{},{})", lx, y, lz);
                }
            }
        }
    }

    #[test]
    fn unknown_kind_rejected() {
        let bad = r#"{"kind": "vortex"}"#;
        assert!(serde_json::from_str::<PresetSchema>(bad).is_err());
    }

    #[test]
    fn unknown_block_in_flat_errors() {
        let json = r#"{"kind": "flat", "min_y": 0, "biome": "plains",
            "layers": [{"block": "minecraft:not_a_real_block", "height": 1}]}"#;
        let schema: PresetSchema = serde_json::from_str(json).unwrap();
        assert!(schema.build(0).is_err());
    }

    #[test]
    fn noise_preset_assigns_varied_biomes() {
        // Across a wide patch, the multi-noise biome source should
        // assign more than one biome (otherwise the noise is collapsed).
        let w = load("noise", 0xC0FFEE).unwrap();
        let mut biomes = std::collections::HashSet::new();
        for cx in -8..8i32 {
            for cz in -8..8i32 {
                biomes.insert(w.biome_at(cx, cz));
            }
        }
        assert!(biomes.len() >= 2,
            "noise preset should produce >1 biome in a 16x16 chunk patch, got {:?}",
            biomes);
    }
}
