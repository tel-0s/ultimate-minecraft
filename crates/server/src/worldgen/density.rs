//! Density functions: composable scalar fields over `(x, y, z)`.
//!
//! Mirrors vanilla 1.18+'s "noise router" approach. A density function is a
//! tree of atoms (`constant`, `y_index`, `noise2d`, `noise3d`) combined by
//! operators (`add`, `sub`, `mul`, `min`, `max`, `clamp`). The tree is
//! described in JSON ([`DensityFnSchema`]) and *compiled* at startup into a
//! tree of `Arc<dyn DensityFunction>` trait objects via
//! [`DensityFnSchema::build`].
//!
//! Convention (matches vanilla): positive output = solid, negative = air.
//! The surface at column `(x, z)` is the highest `y` where density crosses
//! from negative to positive going downward.

use std::sync::Arc;

use noise::{Fbm, MultiFractal, NoiseFn, Perlin};
use serde::{Deserialize, Serialize};

/// A composable scalar field over `(x, y, z)`. Implementations must be
/// deterministic from their construction parameters.
pub trait DensityFunction: Send + Sync {
    fn sample(&self, x: i64, y: i64, z: i64) -> f64;
}

// ── Schema (serializable) ───────────────────────────────────────────────────

/// JSON schema for a density function. Compiles to a tree of
/// `Arc<dyn DensityFunction>` via [`build`].
///
/// `seed_offset` on noise atoms is XOR'd into the worldgen seed so each
/// noise field in a preset has a unique, but seed-derived, stream.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DensityFnSchema {
    /// Returns a fixed value everywhere.
    Constant { value: f64 },

    /// Returns the current `y` coordinate. Combined with `sub` against a
    /// height field gives a 2D heightmap density: `(height - y_index)`.
    YIndex,

    /// Fractional Brownian motion (octaved Perlin) sampled at `(x, z)`.
    Noise2d {
        seed_offset: u32,
        frequency: f64,
        #[serde(default = "default_octaves")]
        octaves: usize,
        #[serde(default = "default_persistence")]
        persistence: f64,
        #[serde(default = "default_lacunarity")]
        lacunarity: f64,
    },

    /// Fractional Brownian motion sampled at `(x, y, z)`.
    Noise3d {
        seed_offset: u32,
        frequency: f64,
        #[serde(default = "default_octaves")]
        octaves: usize,
        #[serde(default = "default_persistence")]
        persistence: f64,
        #[serde(default = "default_lacunarity")]
        lacunarity: f64,
    },

    Add { argument1: Box<DensityFnSchema>, argument2: Box<DensityFnSchema> },
    Sub { argument1: Box<DensityFnSchema>, argument2: Box<DensityFnSchema> },
    Mul { argument1: Box<DensityFnSchema>, argument2: Box<DensityFnSchema> },
    Min { argument1: Box<DensityFnSchema>, argument2: Box<DensityFnSchema> },
    Max { argument1: Box<DensityFnSchema>, argument2: Box<DensityFnSchema> },

    Clamp { input: Box<DensityFnSchema>, min: f64, max: f64 },

    /// Piecewise-linear spline: sample `input` and linearly interpolate
    /// between adjacent points in `points`. Inputs outside the range
    /// clamp to the endpoint outputs. Used to map a low-frequency
    /// climate noise field (continentalness, erosion, …) to a non-linear
    /// terrain contribution: e.g., most of the input range maps to
    /// "land", a small tail to "deep ocean", another to "mountain peak".
    ///
    /// Vanilla uses cubic splines; linear is enough for Stage 4c and
    /// keeps the schema readable.
    Spline { input: Box<DensityFnSchema>, points: Vec<SplinePoint> },
}

/// One (input, output) pair on a [`DensityFnSchema::Spline`].
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SplinePoint {
    pub input: f64,
    pub output: f64,
}

fn default_octaves() -> usize { 4 }
fn default_persistence() -> f64 { 0.5 }
fn default_lacunarity() -> f64 { 2.0 }

impl DensityFnSchema {
    /// Whether this subtree's output is independent of `y`. Used to detect
    /// the canonical heightmap pattern `f(x,z) - y_index` where `f` can
    /// be sampled once per column, avoiding the full top-down column scan.
    pub fn is_y_independent(&self) -> bool {
        match self {
            Self::YIndex | Self::Noise3d { .. } => false,
            Self::Constant { .. } | Self::Noise2d { .. } => true,
            Self::Add { argument1, argument2 }
            | Self::Sub { argument1, argument2 }
            | Self::Mul { argument1, argument2 }
            | Self::Min { argument1, argument2 }
            | Self::Max { argument1, argument2 } => {
                argument1.is_y_independent() && argument2.is_y_independent()
            }
            Self::Clamp { input, .. } | Self::Spline { input, .. } => input.is_y_independent(),
        }
    }

    /// If this schema is exactly `height_field - y_index` with `height_field`
    /// y-independent, return a reference to the height-field subtree. The
    /// pipeline can then compute the surface Y by sampling `height_field`
    /// once per column (the heightmap fast path).
    pub fn as_heightmap(&self) -> Option<&DensityFnSchema> {
        if let Self::Sub { argument1, argument2 } = self {
            if matches!(**argument2, Self::YIndex) && argument1.is_y_independent() {
                return Some(argument1);
            }
        }
        None
    }

    /// Compile the schema tree into a tree of trait objects, allocating
    /// noise state once. Call this at startup, then `sample(x, y, z)` is
    /// pure read-through.
    pub fn build(&self, seed: u32) -> Arc<dyn DensityFunction> {
        match self {
            Self::Constant { value } => Arc::new(Constant(*value)),
            Self::YIndex => Arc::new(YIndex),

            Self::Noise2d { seed_offset, frequency, octaves, persistence, lacunarity } => {
                Arc::new(Noise2d::new(
                    seed.wrapping_add(*seed_offset),
                    *frequency, *octaves, *persistence, *lacunarity,
                ))
            }
            Self::Noise3d { seed_offset, frequency, octaves, persistence, lacunarity } => {
                Arc::new(Noise3d::new(
                    seed.wrapping_add(*seed_offset),
                    *frequency, *octaves, *persistence, *lacunarity,
                ))
            }

            Self::Add { argument1, argument2 } =>
                Arc::new(BinOp::Add(argument1.build(seed), argument2.build(seed))),
            Self::Sub { argument1, argument2 } =>
                Arc::new(BinOp::Sub(argument1.build(seed), argument2.build(seed))),
            Self::Mul { argument1, argument2 } =>
                Arc::new(BinOp::Mul(argument1.build(seed), argument2.build(seed))),
            Self::Min { argument1, argument2 } =>
                Arc::new(BinOp::Min(argument1.build(seed), argument2.build(seed))),
            Self::Max { argument1, argument2 } =>
                Arc::new(BinOp::Max(argument1.build(seed), argument2.build(seed))),

            Self::Clamp { input, min, max } =>
                Arc::new(Clamp { input: input.build(seed), min: *min, max: *max }),

            Self::Spline { input, points } => {
                // Sort defensively so authors don't have to keep their JSON
                // strictly monotonic — and so an out-of-order list still
                // produces sensible output rather than panicking at sample
                // time. Empty list collapses to constant 0 (no contribution).
                let mut sorted: Vec<(f64, f64)> = points.iter()
                    .map(|p| (p.input, p.output))
                    .collect();
                sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
                Arc::new(Spline {
                    input: input.build(seed),
                    points: sorted,
                })
            }
        }
    }
}

// ── Atoms ───────────────────────────────────────────────────────────────────

struct Constant(f64);
impl DensityFunction for Constant {
    fn sample(&self, _x: i64, _y: i64, _z: i64) -> f64 { self.0 }
}

struct YIndex;
impl DensityFunction for YIndex {
    fn sample(&self, _x: i64, y: i64, _z: i64) -> f64 { y as f64 }
}

struct Noise2d { fbm: Fbm<Perlin> }
impl Noise2d {
    fn new(seed: u32, frequency: f64, octaves: usize, persistence: f64, lacunarity: f64) -> Self {
        let fbm = Fbm::<Perlin>::new(seed)
            .set_frequency(frequency)
            .set_octaves(octaves)
            .set_persistence(persistence)
            .set_lacunarity(lacunarity);
        Self { fbm }
    }
}
impl DensityFunction for Noise2d {
    fn sample(&self, x: i64, _y: i64, z: i64) -> f64 {
        self.fbm.get([x as f64, z as f64])
    }
}

struct Noise3d { fbm: Fbm<Perlin> }
impl Noise3d {
    fn new(seed: u32, frequency: f64, octaves: usize, persistence: f64, lacunarity: f64) -> Self {
        let fbm = Fbm::<Perlin>::new(seed)
            .set_frequency(frequency)
            .set_octaves(octaves)
            .set_persistence(persistence)
            .set_lacunarity(lacunarity);
        Self { fbm }
    }
}
impl DensityFunction for Noise3d {
    fn sample(&self, x: i64, y: i64, z: i64) -> f64 {
        self.fbm.get([x as f64, y as f64, z as f64])
    }
}

// ── Combinators ─────────────────────────────────────────────────────────────

enum BinOp {
    Add(Arc<dyn DensityFunction>, Arc<dyn DensityFunction>),
    Sub(Arc<dyn DensityFunction>, Arc<dyn DensityFunction>),
    Mul(Arc<dyn DensityFunction>, Arc<dyn DensityFunction>),
    Min(Arc<dyn DensityFunction>, Arc<dyn DensityFunction>),
    Max(Arc<dyn DensityFunction>, Arc<dyn DensityFunction>),
}
impl DensityFunction for BinOp {
    fn sample(&self, x: i64, y: i64, z: i64) -> f64 {
        match self {
            Self::Add(a, b) => a.sample(x, y, z) + b.sample(x, y, z),
            Self::Sub(a, b) => a.sample(x, y, z) - b.sample(x, y, z),
            Self::Mul(a, b) => a.sample(x, y, z) * b.sample(x, y, z),
            Self::Min(a, b) => a.sample(x, y, z).min(b.sample(x, y, z)),
            Self::Max(a, b) => a.sample(x, y, z).max(b.sample(x, y, z)),
        }
    }
}

struct Clamp {
    input: Arc<dyn DensityFunction>,
    min: f64,
    max: f64,
}
impl DensityFunction for Clamp {
    fn sample(&self, x: i64, y: i64, z: i64) -> f64 {
        self.input.sample(x, y, z).clamp(self.min, self.max)
    }
}

struct Spline {
    input: Arc<dyn DensityFunction>,
    /// Sorted by `input` ascending. Empty list returns 0.
    points: Vec<(f64, f64)>,
}
impl DensityFunction for Spline {
    fn sample(&self, x: i64, y: i64, z: i64) -> f64 {
        if self.points.is_empty() { return 0.0; }
        let v = self.input.sample(x, y, z);
        // Clamp at endpoints.
        if v <= self.points[0].0 { return self.points[0].1; }
        let last = *self.points.last().unwrap();
        if v >= last.0 { return last.1; }
        // Find the segment whose right edge is just above v.
        let i = self.points.partition_point(|p| p.0 < v);
        // i is in 1..points.len() since we ruled out the endpoints above.
        let (x1, y1) = self.points[i - 1];
        let (x2, y2) = self.points[i];
        let t = (v - x1) / (x2 - x1);
        y1 + (y2 - y1) * t
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_is_constant() {
        let f = Constant(5.0);
        assert_eq!(f.sample(0, 0, 0), 5.0);
        assert_eq!(f.sample(100, -50, 100), 5.0);
    }

    #[test]
    fn y_index_returns_y() {
        assert_eq!(YIndex.sample(0, 42, 0), 42.0);
        assert_eq!(YIndex.sample(100, -10, 200), -10.0);
    }

    #[test]
    fn schema_round_trips_through_json() {
        let schema = DensityFnSchema::Add {
            argument1: Box::new(DensityFnSchema::Constant { value: 5.0 }),
            argument2: Box::new(DensityFnSchema::YIndex),
        };
        let json = serde_json::to_string(&schema).unwrap();
        let parsed: DensityFnSchema = serde_json::from_str(&json).unwrap();
        let built = parsed.build(0);
        assert_eq!(built.sample(0, 10, 0), 15.0);
    }

    #[test]
    fn clamp_clamps_both_directions() {
        let f = DensityFnSchema::Clamp {
            input: Box::new(DensityFnSchema::Constant { value: 100.0 }),
            min: -5.0, max: 5.0,
        }.build(0);
        assert_eq!(f.sample(0, 0, 0), 5.0);

        let g = DensityFnSchema::Clamp {
            input: Box::new(DensityFnSchema::Constant { value: -100.0 }),
            min: -5.0, max: 5.0,
        }.build(0);
        assert_eq!(g.sample(0, 0, 0), -5.0);
    }

    #[test]
    fn min_max_pick_correctly() {
        let min_f = DensityFnSchema::Min {
            argument1: Box::new(DensityFnSchema::Constant { value: 3.0 }),
            argument2: Box::new(DensityFnSchema::Constant { value: 7.0 }),
        }.build(0);
        assert_eq!(min_f.sample(0, 0, 0), 3.0);

        let max_f = DensityFnSchema::Max {
            argument1: Box::new(DensityFnSchema::Constant { value: 3.0 }),
            argument2: Box::new(DensityFnSchema::Constant { value: 7.0 }),
        }.build(0);
        assert_eq!(max_f.sample(0, 0, 0), 7.0);
    }

    #[test]
    fn same_seed_same_noise() {
        let n1 = Noise2d::new(42, 0.01, 3, 0.5, 2.0);
        let n2 = Noise2d::new(42, 0.01, 3, 0.5, 2.0);
        for x in -10..10i64 {
            for z in -10..10i64 {
                assert!((n1.sample(x, 0, z) - n2.sample(x, 0, z)).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn seed_offset_decorrelates_noise() {
        // Two noise atoms with different seed offsets should produce
        // mostly-different values under the same base seed.
        let base_seed = 100;
        let n1 = DensityFnSchema::Noise2d {
            seed_offset: 0, frequency: 0.01,
            octaves: 3, persistence: 0.5, lacunarity: 2.0,
        }.build(base_seed);
        let n2 = DensityFnSchema::Noise2d {
            seed_offset: 1, frequency: 0.01,
            octaves: 3, persistence: 0.5, lacunarity: 2.0,
        }.build(base_seed);
        let mut differences = 0;
        for x in 0..50i64 {
            for z in 0..50i64 {
                if (n1.sample(x, 0, z) - n2.sample(x, 0, z)).abs() > 0.01 {
                    differences += 1;
                }
            }
        }
        assert!(differences > 2000, "seed offsets should decorrelate noise");
    }

    #[test]
    fn detects_heightmap_pattern() {
        let h = DensityFnSchema::Sub {
            argument1: Box::new(DensityFnSchema::Constant { value: 80.0 }),
            argument2: Box::new(DensityFnSchema::YIndex),
        };
        assert!(h.as_heightmap().is_some());

        // Constant - constant: y-independent both sides but not the heightmap shape.
        let not_h = DensityFnSchema::Sub {
            argument1: Box::new(DensityFnSchema::Constant { value: 80.0 }),
            argument2: Box::new(DensityFnSchema::Constant { value: 5.0 }),
        };
        assert!(not_h.as_heightmap().is_none());

        // Heightmap with noise2d in the height field: still valid.
        let h_with_noise = DensityFnSchema::Sub {
            argument1: Box::new(DensityFnSchema::Add {
                argument1: Box::new(DensityFnSchema::Constant { value: 80.0 }),
                argument2: Box::new(DensityFnSchema::Noise2d {
                    seed_offset: 0, frequency: 0.01,
                    octaves: 3, persistence: 0.5, lacunarity: 2.0,
                }),
            }),
            argument2: Box::new(DensityFnSchema::YIndex),
        };
        assert!(h_with_noise.as_heightmap().is_some());

        // Heightmap broken by noise3d (y-dependent) in the height field.
        let broken = DensityFnSchema::Sub {
            argument1: Box::new(DensityFnSchema::Noise3d {
                seed_offset: 0, frequency: 0.01,
                octaves: 3, persistence: 0.5, lacunarity: 2.0,
            }),
            argument2: Box::new(DensityFnSchema::YIndex),
        };
        assert!(broken.as_heightmap().is_none());

        // Reversed (y_index - height): not the heightmap shape.
        let reversed = DensityFnSchema::Sub {
            argument1: Box::new(DensityFnSchema::YIndex),
            argument2: Box::new(DensityFnSchema::Constant { value: 80.0 }),
        };
        assert!(reversed.as_heightmap().is_none());
    }

    #[test]
    fn spline_interpolates_linearly_between_points() {
        let schema = DensityFnSchema::Spline {
            input: Box::new(DensityFnSchema::Constant { value: 0.0 }),
            points: vec![
                SplinePoint { input: -1.0, output: -10.0 },
                SplinePoint { input:  0.0, output:   0.0 },
                SplinePoint { input:  1.0, output:  20.0 },
            ],
        };
        let f = schema.build(0);
        // Endpoints exact.
        assert_eq!(f.sample(0, 0, 0), 0.0); // input=0 → output=0
        // Halfway between (0, 0) and (1, 20): input=0.5 → output=10.
        let schema = DensityFnSchema::Spline {
            input: Box::new(DensityFnSchema::Constant { value: 0.5 }),
            points: vec![
                SplinePoint { input: -1.0, output: -10.0 },
                SplinePoint { input:  0.0, output:   0.0 },
                SplinePoint { input:  1.0, output:  20.0 },
            ],
        };
        assert!((schema.build(0).sample(0, 0, 0) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn spline_clamps_outside_range() {
        let schema = DensityFnSchema::Spline {
            input: Box::new(DensityFnSchema::Constant { value: 5.0 }),
            points: vec![
                SplinePoint { input: -1.0, output: -10.0 },
                SplinePoint { input:  1.0, output:  20.0 },
            ],
        };
        assert_eq!(schema.build(0).sample(0, 0, 0), 20.0); // way above range → clamp top

        let schema = DensityFnSchema::Spline {
            input: Box::new(DensityFnSchema::Constant { value: -5.0 }),
            points: vec![
                SplinePoint { input: -1.0, output: -10.0 },
                SplinePoint { input:  1.0, output:  20.0 },
            ],
        };
        assert_eq!(schema.build(0).sample(0, 0, 0), -10.0); // way below → clamp bottom
    }

    #[test]
    fn spline_handles_unsorted_input() {
        // Author writes points out of order; build sorts them and
        // sampling still works.
        let schema = DensityFnSchema::Spline {
            input: Box::new(DensityFnSchema::Constant { value: 0.0 }),
            points: vec![
                SplinePoint { input:  1.0, output:  20.0 },
                SplinePoint { input: -1.0, output: -10.0 },
                SplinePoint { input:  0.0, output:   0.0 },
            ],
        };
        assert_eq!(schema.build(0).sample(0, 0, 0), 0.0);
    }

    #[test]
    fn spline_empty_points_returns_zero() {
        let schema = DensityFnSchema::Spline {
            input: Box::new(DensityFnSchema::Constant { value: 42.0 }),
            points: vec![],
        };
        assert_eq!(schema.build(0).sample(0, 0, 0), 0.0);
    }

    #[test]
    fn spline_is_y_independent_when_input_is() {
        let s = DensityFnSchema::Spline {
            input: Box::new(DensityFnSchema::Noise2d {
                seed_offset: 0, frequency: 0.001,
                octaves: 3, persistence: 0.5, lacunarity: 2.0,
            }),
            points: vec![SplinePoint { input: 0.0, output: 0.0 }],
        };
        assert!(s.is_y_independent());

        let s = DensityFnSchema::Spline {
            input: Box::new(DensityFnSchema::YIndex),
            points: vec![SplinePoint { input: 0.0, output: 0.0 }],
        };
        assert!(!s.is_y_independent());
    }

    #[test]
    fn heightmap_pattern_sub_y() {
        // The canonical heightmap density: `f(x,z) - y_index`.
        // Surface should be at y = f(x,z).
        let schema = DensityFnSchema::Sub {
            argument1: Box::new(DensityFnSchema::Constant { value: 64.0 }),
            argument2: Box::new(DensityFnSchema::YIndex),
        };
        let f = schema.build(0);
        // At y=0 (well below surface): density = 64 - 0 = 64 > 0 (solid).
        assert!(f.sample(0, 0, 0) > 0.0);
        // At y=64 (exactly surface): density = 64 - 64 = 0.
        assert_eq!(f.sample(0, 64, 0), 0.0);
        // At y=100 (well above): density = 64 - 100 = -36 < 0 (air).
        assert!(f.sample(0, 100, 0) < 0.0);
    }
}
