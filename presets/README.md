# Worldgen presets

Drop-in JSON presets for the `world.preset` field in `server.yaml`.
Anything else (`"noise"`, `"superflat"`) resolves to the built-in preset of
that name; any other value is treated as a path to a JSON file like the
ones in this directory.

## Files

- **`amplified.json`** — vanilla-amplified-style dramatic terrain. Higher
  base height (80), large continent amplitude (±50), strong clamped hill
  layer (up to +84), sea level 70. Expect towering mountains and deep
  valleys. Drop into `server.yaml`:
  ```yaml
  world:
    preset: "presets/amplified.json"
  ```

## Schema overview

Top level chooses a preset *kind*:

```json
{ "kind": "density" | "flat", ... }
```

### `kind: "density"`

Compositional density-function pipeline. The `density` field is a tree of
nodes; the surface at each `(x, z)` is the highest `y` at which density
crosses zero from above.

| Field           | Type   | Meaning                                                  |
|-----------------|--------|----------------------------------------------------------|
| `sea_level`     | int    | Y below which empty columns fill with water              |
| `min_y` / `max_y` | int  | World vertical bounds                                    |
| `bedrock_y`     | int    | Y of the bedrock floor layer                             |
| `dirt_depth`    | int    | Thickness of dirt skin under the surface (default 4)     |
| `beach_band`    | int    | ± blocks around `sea_level` to replace dirt with sand    |
| `density`       | tree   | Density function (see node types below)                  |

### `kind: "flat"`

Superflat: bedrock floor + a stack of fixed-height block layers, identical
across every column.

```json
{ "kind": "flat", "min_y": 0,
  "layers": [
    { "block": "minecraft:bedrock",     "height": 1  },
    { "block": "minecraft:stone",       "height": 62 },
    { "block": "minecraft:dirt",        "height": 3  },
    { "block": "minecraft:grass_block", "height": 1  }
  ] }
```

## Density-function node types

Every node has a `type` field. Leaf nodes:

| Type        | Fields                                                                | Returns                                |
|-------------|-----------------------------------------------------------------------|----------------------------------------|
| `constant`  | `value: f64`                                                          | A constant scalar                      |
| `y_index`   | —                                                                     | The current `y` coordinate             |
| `noise2d`   | `seed_offset, frequency, octaves?, persistence?, lacunarity?`         | Fractional Brownian motion over (x, z) |
| `noise3d`   | (same)                                                                | FBM over (x, y, z)                     |

Combinators (every "binary" combinator has `argument1` and `argument2`):

| Type    | Behavior                                |
|---------|-----------------------------------------|
| `add`   | `argument1 + argument2`                 |
| `sub`   | `argument1 - argument2`                 |
| `mul`   | `argument1 * argument2`                 |
| `min`   | `min(argument1, argument2)`             |
| `max`   | `max(argument1, argument2)`             |
| `clamp` | `clamp(input, min, max)` — three fields |

## Authoring tip

The canonical heightmap shape is `(height_field) - y_index`, where
`height_field` is any composition that produces a target Y value. Positive
density = solid (below the surface), negative = air. Walking the tree
top-down through a column finds the surface at the first non-negative
sample.

Noise output is roughly in `[-1, 1]`; multiply by an amplitude to scale.
Clamp before multiplying to bound the maximum elevation contribution.

`seed_offset` is XOR'd into the worldgen seed so each noise atom in a
preset has a unique stream but the whole world stays seed-deterministic.
