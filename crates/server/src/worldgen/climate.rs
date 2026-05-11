//! Climate sampling and biome assignment.
//!
//! A [`BiomeSource`] maps a world column `(x, z)` and its surface Y to a
//! [`Biome`]. The Stage 4b implementation [`MultiNoiseBiomeSource`] samples
//! two climate noise fields (temperature, humidity), considers elevation
//! relative to sea level, and walks a fixed decision table.
//!
//! The decision table is hand-coded in Stage 4b because exposing the full
//! vanilla 6D multi-noise table through JSON is its own rabbit hole. The
//! climate noise fields themselves are fully data-driven via
//! [`DensityFnSchema`], so an operator can already pick which "climate"
//! they want without recompiling.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::biome::Biome;
use super::density::{DensityFnSchema, DensityFunction};

/// Maps `(x, z)` and surface elevation to a biome.
pub trait BiomeSource: Send + Sync {
    fn sample(&self, x: i64, z: i64, surface_y: i64, sea_level: i64) -> Biome;
}

// ── Multi-noise biome source ────────────────────────────────────────────────

/// Two climate noise fields (temperature, humidity) + elevation drive a
/// fixed decision table. Vanilla uses six fields and an explicit table of
/// biome boxes; we'll port that in a later stage. This still produces
/// recognisably-varied terrain because the noise fields decorrelate.
pub struct MultiNoiseBiomeSource {
    pub temperature: Arc<dyn DensityFunction>,
    pub humidity: Arc<dyn DensityFunction>,
}

impl BiomeSource for MultiNoiseBiomeSource {
    fn sample(&self, x: i64, z: i64, surface_y: i64, sea_level: i64) -> Biome {
        // Elevation-driven biomes win first — these are visually obvious
        // categories (under water, at the waterline, high above the
        // tree line) that we want to override climate noise.
        let elevation = surface_y - sea_level;
        if elevation < -4 {
            return Biome::Ocean;
        }
        if elevation > 36 {
            return Biome::StonyPeaks;
        }
        if elevation.abs() <= 2 {
            return Biome::Beach;
        }

        // Climate-driven (the rest is land between the waterline and the
        // alpine band). Noise output is roughly in [-1, 1].
        let t = self.temperature.sample(x, 0, z);
        let h = self.humidity.sample(x, 0, z);
        if t < -0.3 {
            return Biome::SnowyPlains;
        }
        if t > 0.4 && h < -0.1 {
            return Biome::Desert;
        }
        if h > 0.15 {
            return Biome::Forest;
        }
        Biome::Plains
    }
}

// ── Fixed biome source (for superflat / tests) ──────────────────────────────

pub struct FixedBiomeSource(pub Biome);

impl BiomeSource for FixedBiomeSource {
    fn sample(&self, _x: i64, _z: i64, _surface_y: i64, _sea_level: i64) -> Biome {
        self.0
    }
}

// ── JSON schema ─────────────────────────────────────────────────────────────

/// Serializable biome source. Compiles to an `Arc<dyn BiomeSource>` via
/// [`build`].
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum BiomeSourceSchema {
    /// Single biome everywhere — useful for superflat presets and tests.
    Fixed { biome: Biome },

    /// Multi-noise: sample temperature + humidity noise fields per column
    /// and walk a fixed decision table that also considers elevation.
    MultiNoise {
        temperature: DensityFnSchema,
        humidity: DensityFnSchema,
    },
}

impl BiomeSourceSchema {
    pub fn build(&self, seed: u32) -> Arc<dyn BiomeSource> {
        match self {
            Self::Fixed { biome } => Arc::new(FixedBiomeSource(*biome)),
            Self::MultiNoise { temperature, humidity } => Arc::new(MultiNoiseBiomeSource {
                temperature: temperature.build(seed),
                humidity: humidity.build(seed),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn constant_field(value: f64) -> Arc<dyn DensityFunction> {
        DensityFnSchema::Constant { value }.build(0)
    }

    #[test]
    fn elevation_overrides_climate() {
        // Underwater + extreme climate noise → Ocean regardless of climate.
        let src = MultiNoiseBiomeSource {
            temperature: constant_field(1.0),
            humidity: constant_field(-1.0),
        };
        assert_eq!(src.sample(0, 0, 30, 63), Biome::Ocean);
    }

    #[test]
    fn waterline_is_beach() {
        let src = MultiNoiseBiomeSource {
            temperature: constant_field(0.0),
            humidity: constant_field(0.0),
        };
        // Just above sea level → beach band.
        assert_eq!(src.sample(0, 0, 64, 63), Biome::Beach);
        assert_eq!(src.sample(0, 0, 62, 63), Biome::Beach);
    }

    #[test]
    fn alpine_band_is_stony_peaks() {
        let src = MultiNoiseBiomeSource {
            temperature: constant_field(0.0),
            humidity: constant_field(0.0),
        };
        assert_eq!(src.sample(0, 0, 120, 63), Biome::StonyPeaks);
    }

    #[test]
    fn climate_picks_temperate_biomes() {
        let cold = MultiNoiseBiomeSource {
            temperature: constant_field(-0.5),
            humidity: constant_field(0.0),
        };
        assert_eq!(cold.sample(0, 0, 75, 63), Biome::SnowyPlains);

        let hot_dry = MultiNoiseBiomeSource {
            temperature: constant_field(0.6),
            humidity: constant_field(-0.3),
        };
        assert_eq!(hot_dry.sample(0, 0, 75, 63), Biome::Desert);

        let humid = MultiNoiseBiomeSource {
            temperature: constant_field(0.1),
            humidity: constant_field(0.5),
        };
        assert_eq!(humid.sample(0, 0, 75, 63), Biome::Forest);

        let neutral = MultiNoiseBiomeSource {
            temperature: constant_field(0.0),
            humidity: constant_field(0.0),
        };
        assert_eq!(neutral.sample(0, 0, 75, 63), Biome::Plains);
    }

    #[test]
    fn fixed_biome_source_returns_constant() {
        let src = FixedBiomeSource(Biome::Desert);
        assert_eq!(src.sample(0, 0, 64, 63), Biome::Desert);
        assert_eq!(src.sample(100, 200, 30, 63), Biome::Desert);
    }

    #[test]
    fn schema_round_trips_through_json() {
        let schema = BiomeSourceSchema::Fixed { biome: Biome::Plains };
        let json = serde_json::to_string(&schema).unwrap();
        let parsed: BiomeSourceSchema = serde_json::from_str(&json).unwrap();
        let built = parsed.build(0);
        assert_eq!(built.sample(0, 0, 75, 63), Biome::Plains);
    }
}
