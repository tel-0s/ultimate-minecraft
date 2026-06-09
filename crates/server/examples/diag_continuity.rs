//! Diagnostic: measure surface-height and biome continuity across chunk
//! borders vs within chunks for the built-in `noise` preset.
//!
//! If the generator is continuous, the height-step distribution across
//! chunk borders should match the interior distribution. A spike at
//! borders = a chunk-aligned bug in the pipeline; matching distributions
//! = the terrain itself is just steep (spline/biome-table tuning).

use std::collections::HashMap;

use ultimate_engine::world::World;
use ultimate_server::worldgen::preset::{PresetSchema, BUILTIN_NOISE};

const SEED: u32 = 0xC0FFEE;
const RADIUS: i32 = 6; // 12x12 chunks = 192x192 blocks

fn main() {
    // Build the noise preset WITHOUT carvers/decorators so the highest
    // non-air block is exactly the stratified surface.
    let mut schema: PresetSchema = serde_json::from_str(BUILTIN_NOISE).unwrap();
    if let PresetSchema::Density(d) = &mut schema {
        d.carvers.clear();
        d.decorators.clear();
    }
    let wg = schema.build(SEED).unwrap();

    let world = World::new();
    let n = (RADIUS * 2) as usize * 16;
    let base = -(RADIUS as i64) * 16;

    // Surface height per world column, from generated chunks.
    let mut height = vec![0i64; n * n];
    for cx in -RADIUS..RADIUS {
        for cz in -RADIUS..RADIUS {
            let chunk = wg.generate_chunk(cx, cz, &world);
            for lx in 0..16u8 {
                for lz in 0..16u8 {
                    let mut h = -64i64;
                    for y in (-64..=200i64).rev() {
                        use ultimate_engine::world::position::LocalBlockPos;
                        if chunk.get_block(LocalBlockPos { x: lx, y, z: lz })
                            != ultimate_engine::world::block::BlockId::AIR
                        {
                            h = y;
                            break;
                        }
                    }
                    let wx = (cx as i64 * 16 + lx as i64 - base) as usize;
                    let wz = (cz as i64 * 16 + lz as i64 - base) as usize;
                    height[wz * n + wx] = h;
                }
            }
        }
    }

    // Pair statistics: |dh| across x-adjacent and z-adjacent column pairs,
    // split by whether the pair straddles a chunk border.
    let mut interior: Vec<i64> = Vec::new();
    let mut border: Vec<i64> = Vec::new();
    for wz in 0..n {
        for wx in 0..n - 1 {
            let dh = (height[wz * n + wx + 1] - height[wz * n + wx]).abs();
            let world_x = wx as i64 + base;
            // Border pair: crossing from lx=15 into lx=0 of the next chunk.
            if (world_x + 1).rem_euclid(16) == 0 {
                border.push(dh);
            } else {
                interior.push(dh);
            }
        }
    }
    for wx in 0..n {
        for wz in 0..n - 1 {
            let dh = (height[(wz + 1) * n + wx] - height[wz * n + wx]).abs();
            let world_z = wz as i64 + base;
            if (world_z + 1).rem_euclid(16) == 0 {
                border.push(dh);
            } else {
                interior.push(dh);
            }
        }
    }

    let stats = |v: &mut Vec<i64>| -> (f64, i64, i64) {
        v.sort_unstable();
        let mean = v.iter().sum::<i64>() as f64 / v.len() as f64;
        let p99 = v[(v.len() as f64 * 0.99) as usize];
        let max = *v.last().unwrap();
        (mean, p99, max)
    };
    let (im, ip99, imax) = stats(&mut interior);
    let (bm, bp99, bmax) = stats(&mut border);
    println!("height steps  interior: mean {:.3}  p99 {}  max {}", im, ip99, imax);
    println!("height steps  border:   mean {:.3}  p99 {}  max {}", bm, bp99, bmax);

    // Biome continuity via biome_at_cell on a 4-block grid at surface level.
    let mut biome_interior = 0usize;
    let mut biome_interior_diff = 0usize;
    let mut biome_border = 0usize;
    let mut biome_border_diff = 0usize;
    let mut biome_counts: HashMap<u32, usize> = HashMap::new();
    let cells = n / 4;
    for gz in 0..cells {
        for gx in 0..cells - 1 {
            let x1 = base + (gx as i64) * 4 + 2;
            let x2 = base + (gx as i64 + 1) * 4 + 2;
            let z = base + (gz as i64) * 4 + 2;
            let b1 = wg.biome_at_cell(x1, 80, z);
            let b2 = wg.biome_at_cell(x2, 80, z);
            *biome_counts.entry(b1).or_default() += 1;
            let crosses = (x2.div_euclid(16)) != (x1.div_euclid(16));
            if crosses {
                biome_border += 1;
                if b1 != b2 { biome_border_diff += 1; }
            } else {
                biome_interior += 1;
                if b1 != b2 { biome_interior_diff += 1; }
            }
        }
    }
    println!(
        "biome change rate  interior: {:.4} ({}/{})",
        biome_interior_diff as f64 / biome_interior as f64, biome_interior_diff, biome_interior
    );
    println!(
        "biome change rate  border:   {:.4} ({}/{})",
        biome_border_diff as f64 / biome_border as f64, biome_border_diff, biome_border
    );
    println!("biome histogram: {:?}", biome_counts);

    // ASCII heightmap (1 char per 2 columns) to eyeball seams.
    println!("\nheight map ({}x{} blocks, '.' low → '#' high):", n, n);
    let ramp: &[u8] = b" .:-=+*#%@";
    for wz in (0..n).step_by(3) {
        let mut line = String::new();
        for wx in (0..n).step_by(2) {
            let h = height[wz * n + wx];
            let t = ((h - 40).clamp(0, 99) as usize) / 10;
            line.push(ramp[t.min(ramp.len() - 1)] as char);
        }
        println!("{}", line);
    }
}
