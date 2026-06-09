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
use super::carver::CarverSchema;
use super::climate::BiomeSourceSchema;
use super::decorator::DecoratorSchema;
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
    /// Carvers run in declared order after the heightmap stratification.
    /// Empty/omitted → solid terrain (no caves). See `worldgen::carver`.
    #[serde(default)]
    pub carvers: Vec<CarverSchema>,
    /// Decorators run after carvers, scattering features per chunk.
    /// Empty/omitted → no ores / plants / trees. See `worldgen::decorator`.
    #[serde(default)]
    pub decorators: Vec<DecoratorSchema>,
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

/// Resolve a preset spec to `(source_label, json_text)` — either a
/// built-in name (`"noise"`, `"superflat"`) or a path to a JSON file.
fn resolve(spec: &str) -> Result<(String, String)> {
    Ok(match spec {
        "noise" => ("builtin:noise".to_string(), BUILTIN_NOISE.to_string()),
        "superflat" => ("builtin:superflat".to_string(), BUILTIN_SUPERFLAT.to_string()),
        path => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| anyhow!("reading worldgen preset {}: {}", path, e))?;
            (format!("file:{}", path), text)
        }
    })
}

/// Load a preset by spec — either a built-in name (`"noise"`,
/// `"superflat"`) or a path to a JSON file.
pub fn load(spec: &str, seed: u32) -> Result<Arc<dyn WorldGen>> {
    let (source, json) = resolve(spec)?;
    let schema: PresetSchema = serde_json::from_str(&json)
        .map_err(|e| anyhow!("parsing worldgen preset {}: {}", source, e))?;
    schema.build(seed)
}

/// Stable fingerprint of the effective generator: FNV-1a over the
/// *canonical re-serialization* of the parsed preset schema plus the seed.
///
/// Persistence stamps every saved chunk with this value and skips chunks
/// whose stamp doesn't match at load time — terrain from an older
/// generator version must regenerate rather than be stitched against new
/// terrain (mismatched heights/biomes at chunk borders). Canonical
/// re-serialization makes the fingerprint insensitive to JSON whitespace
/// and key order, but sensitive to any value or schema-default change.
pub fn fingerprint(spec: &str, seed: u32) -> Result<u64> {
    let (source, json) = resolve(spec)?;
    let schema: PresetSchema = serde_json::from_str(&json)
        .map_err(|e| anyhow!("parsing worldgen preset {}: {}", source, e))?;
    let canonical = serde_json::to_string(&schema)
        .map_err(|e| anyhow!("canonicalizing worldgen preset {}: {}", source, e))?;

    // FNV-1a 64-bit: deterministic across runs and platforms (unlike the
    // std SipHash `DefaultHasher`, whose keys are per-process random).
    let mut h: u64 = 0xcbf29ce484222325;
    for b in canonical.bytes().chain(seed.to_le_bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    Ok(h)
}

impl PresetSchema {
    pub fn build(self, seed: u32) -> Result<Arc<dyn WorldGen>> {
        match self {
            Self::Density(d) => {
                let heightmap_shortcut = d.density.as_heightmap().map(|h| h.build(seed));
                let density = d.density.build(seed);
                let biome_source = d.biome_source.build(seed);
                let surface_rule = d.surface_rule.build()?;
                let carvers = d.carvers.iter()
                    .map(|c| c.build(seed))
                    .collect::<Result<Vec<_>>>()?;
                let decorators = d.decorators.iter()
                    .map(|x| x.build())
                    .collect::<Result<Vec<_>>>()?;
                Ok(Arc::new(DensityPipeline {
                    density,
                    heightmap_shortcut,
                    biome_source,
                    surface_rule,
                    carvers,
                    decorators,
                    seed,
                    pending: Arc::new(super::decorator::PendingWrites::new()),
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
    use ultimate_engine::world::World;
    use ultimate_engine::world::position::LocalBlockPos;

    #[test]
    fn builtin_noise_parses_and_builds() {
        let schema: PresetSchema = serde_json::from_str(BUILTIN_NOISE).unwrap();
        let w = schema.build(42).unwrap();
        let chunk = w.generate_chunk(0, 0, &World::new());
        assert!(chunk.section_count() > 0, "noise preset should produce non-empty chunks");
    }

    #[test]
    fn builtin_superflat_parses_and_builds() {
        let schema: PresetSchema = serde_json::from_str(BUILTIN_SUPERFLAT).unwrap();
        let w = schema.build(0).unwrap();
        let chunk = w.generate_chunk(0, 0, &World::new());
        assert_eq!(
            chunk.get_block(LocalBlockPos { x: 0, y: 0, z: 0 }),
            block::BEDROCK,
        );
    }

    #[test]
    fn deterministic_from_seed() {
        let w1 = load("noise", 42).unwrap();
        let w2 = load("noise", 42).unwrap();
        let world = World::new();
        let c_a = w1.generate_chunk(3, 7, &world);
        let world = World::new();
        let c_b = w2.generate_chunk(3, 7, &world);
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
    fn fingerprint_is_stable_and_input_sensitive() {
        // Same preset + seed → same fingerprint across calls (FNV, not the
        // per-process-random std hasher).
        let a = fingerprint("noise", 42).unwrap();
        let b = fingerprint("noise", 42).unwrap();
        assert_eq!(a, b);

        // Different seed → different fingerprint.
        assert_ne!(fingerprint("noise", 42).unwrap(), fingerprint("noise", 43).unwrap());

        // Different preset → different fingerprint.
        assert_ne!(fingerprint("noise", 42).unwrap(), fingerprint("superflat", 42).unwrap());
    }

    #[test]
    fn fingerprint_ignores_whitespace_formatting() {
        // Canonical re-serialization: reformatting the JSON (whitespace
        // only) must not change the fingerprint. Compare a re-pretty-printed
        // copy of the built-in noise preset against the built-in itself.
        let schema: PresetSchema = serde_json::from_str(BUILTIN_NOISE).unwrap();
        let pretty = serde_json::to_string_pretty(&schema).unwrap();
        let tmp = std::env::temp_dir().join("umc_fingerprint_ws_test.json");
        std::fs::write(&tmp, &pretty).unwrap();

        let from_file = fingerprint(tmp.to_str().unwrap(), 7).unwrap();
        let builtin = fingerprint("noise", 7).unwrap();
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(from_file, builtin);
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
    fn noise_preset_carvers_actually_carve() {
        // Regression: the previous default threshold of 0.55 was higher
        // than the noise crate's FBM ever produced for our parameters
        // (peaks around ±0.73), so the carver fired on <0.3% of cells —
        // effectively no visible caves in-game. Assert the default produces
        // a meaningful number of air cells inside the carver range across
        // a small chunk patch.
        use ultimate_engine::world::block::BlockId;
        use ultimate_engine::world::position::LocalBlockPos;
        let w = load("noise", 0xC0FFEE).unwrap();
        let mut air_in_range = 0usize;
        // Probe a 4x4 chunk patch (64 chunks worth of cells if we sampled
        // every block; we sample sparsely to keep the test fast).
        for cx in -2..2i32 {
            for cz in -2..2i32 {
                let chunk = w.generate_chunk(cx, cz, &World::new());
                // y=-30..40 is inside the carver range (-56..55) AND
                // typically below the surface (~70-90), so any AIR we
                // find here is from carving, not from "above-surface".
                for lx in 0..16u8 {
                    for lz in 0..16u8 {
                        for y in -30..=40i64 {
                            if chunk.get_block(LocalBlockPos { x: lx, y, z: lz }) == BlockId::AIR {
                                air_in_range += 1;
                            }
                        }
                    }
                }
            }
        }
        // 4*4*16*16 columns × 71 y values = 1,163,264 cells inspected.
        // At ~3% carved we expect ~35K air cells; sanity-check at least 5K.
        assert!(
            air_in_range > 5000,
            "default carver only produced {} air cells in 4x4 chunk patch — caves probably aren't generating",
            air_in_range,
        );
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
