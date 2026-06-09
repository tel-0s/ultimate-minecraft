//! Phase 6c: chunk memory measurement.
//!
//! Generates a realistic area with the built-in `noise` preset and
//! reports block-storage bytes under paletted sections vs the previous
//! raw `[BlockId; 4096]` (8 KB/section) representation.
//!
//! Run with: `cargo run --release --example bench_memory`

use ultimate_engine::world::World;
use ultimate_server::worldgen::preset;

const R: i32 = 12; // 24x24 chunks

fn main() {
    let wg = preset::load("noise", 0xC0FFEE).expect("builtin noise preset");
    let world = World::new();

    let mut sections = 0usize;
    let mut paletted_bytes = 0usize;
    let mut uniform_sections = 0usize;
    let mut bits4_or_less = 0usize;
    let mut max_palette = 0usize;

    for cx in -R..R {
        for cz in -R..R {
            let chunk = wg.generate_chunk(cx, cz, &world);
            for (_, section) in chunk.sections() {
                sections += 1;
                paletted_bytes += section.memory_bytes();
                let p = section.palette().len();
                max_palette = max_palette.max(p);
                if p == 1 {
                    uniform_sections += 1;
                } else if p <= 16 {
                    bits4_or_less += 1;
                }
            }
            world.insert_chunk(
                ultimate_engine::world::position::ChunkPos::new(cx, cz),
                chunk,
            );
        }
    }

    let raw_bytes = sections * 8192;
    let chunks = (2 * R) * (2 * R);
    println!("=== Ultimate Minecraft: Chunk Memory (Phase 6c paletted sections) ===");
    println!("area: {chunks} chunks ({sections} non-empty sections, noise preset)");
    println!();
    println!("  raw [BlockId; 4096] storage: {:>10} KB ({} B/section)", raw_bytes / 1024, 8192);
    println!(
        "  paletted storage:            {:>10} KB ({:.0} B/section avg)",
        paletted_bytes / 1024,
        paletted_bytes as f64 / sections as f64,
    );
    println!(
        "  reduction: {:.1}x  |  sections: {} uniform, {} at ≤4-bit, {} wider  |  max palette {}",
        raw_bytes as f64 / paletted_bytes as f64,
        uniform_sections,
        bits4_or_less,
        sections - uniform_sections - bits4_or_less,
        max_palette,
    );
    println!(
        "  per loaded chunk (blocks only): {:.1} KB → {:.1} KB",
        raw_bytes as f64 / chunks as f64 / 1024.0,
        paletted_bytes as f64 / chunks as f64 / 1024.0,
    );
}
