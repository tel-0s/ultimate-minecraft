use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use ultimate_engine::world::World;
use ultimate_server::dashboard::{self, DashboardState};
use ultimate_server::event_bus::{self, WorldChangeBatch};
use ultimate_server::persistence;
use ultimate_server::player_registry::PlayerRegistry;

/// Default autosave interval (5 minutes).
const AUTOSAVE_INTERVAL: Duration = Duration::from_secs(300);

#[tokio::main]
async fn main() {
    let demo_mode = std::env::args().any(|a| a == "--demo");
    let bind_addr = std::env::args()
        .skip_while(|a| a != "--bind")
        .nth(1)
        .unwrap_or_else(|| "0.0.0.0:25565".into());
    let dashboard_port: u16 = std::env::args()
        .skip_while(|a| a != "--dashboard-port")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(8000);
    let world_dir: PathBuf = std::env::args()
        .skip_while(|a| a != "--world")
        .nth(1)
        .unwrap_or_else(|| "world".into())
        .into();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".parse().unwrap()),
        )
        .init();

    if demo_mode {
        run_demo();
        return;
    }

    tracing::info!("Ultimate Minecraft -- causal voxel engine server");

    // ── Generate base world, then overlay saved modifications ──────────
    let world = Arc::new(World::new());
    tracing::info!("Generating flat world...");
    generate_flat_world_mc(&world, 32);
    tracing::info!("Base world ready: {} chunks", world.chunk_count());

    // Load saved (player-modified) chunks on top of the generated base.
    match persistence::load_into(&world, &world_dir) {
        Ok(0) => tracing::info!("No saved modifications found"),
        Ok(n) => tracing::info!("Loaded {} modified chunks from {}", n, world_dir.display()),
        Err(e) => tracing::error!("Failed to load saved chunks: {:#}", e),
    }

    // Start live dashboard (non-blocking — runs on its own tasks).
    let dashboard = Arc::new(DashboardState::new(Arc::clone(&world)));
    let dash = Arc::clone(&dashboard);
    tokio::spawn(async move {
        dashboard::server::start(dash, dashboard_port).await;
    });

    // World-change event bus: player actions and simulation layers publish here,
    // all connections subscribe to receive cross-player updates.
    let (bus_tx, _) = broadcast::channel::<WorldChangeBatch>(event_bus::BUS_CAPACITY);

    // Start ambient simulation layers (empty for now -- add layers here).
    let sim_layers: Vec<Box<dyn ultimate_server::simulation::SimulationLayer>> = vec![];
    ultimate_server::simulation::start(Arc::clone(&world), sim_layers, bus_tx.clone());

    // Shared player registry for multiplayer visibility.
    let registry = Arc::new(PlayerRegistry::new());

    // ── Periodic autosave ────────────────────────────────────────────────
    let save_world_ref = Arc::clone(&world);
    let save_dir = world_dir.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(AUTOSAVE_INTERVAL);
        interval.tick().await; // first tick is immediate, skip it
        loop {
            interval.tick().await;
            tracing::info!("Autosaving...");
            match persistence::save_world(&save_world_ref, &save_dir) {
                Ok(n) => tracing::info!("Autosave complete: {} chunks", n),
                Err(e) => tracing::error!("Autosave failed: {:#}", e),
            }
        }
    });

    // ── Start listener with graceful shutdown ────────────────────────────
    tracing::info!("Starting Minecraft 1.21.11 server on {}", bind_addr);

    tokio::select! {
        result = ultimate_server::net::listener::run(
            Arc::clone(&world), dashboard, bus_tx, registry, &bind_addr,
        ) => {
            if let Err(e) = result {
                tracing::error!("Server error: {}", e);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Ctrl+C received, shutting down...");
        }
    }

    // ── Save on shutdown ─────────────────────────────────────────────────
    tracing::info!("Saving world before exit...");
    match persistence::save_world(&world, &world_dir) {
        Ok(n) => tracing::info!("Shutdown save complete: {} chunks written", n),
        Err(e) => tracing::error!("Shutdown save failed: {:#}", e),
    }
}

/// Original sand-drop demo for testing the causal engine.
fn run_demo() {
    use ultimate_engine::causal::event::{Event, EventPayload};
    use ultimate_engine::causal::graph::CausalGraph;
    use ultimate_engine::causal::scheduler::Scheduler;
    use ultimate_engine::world::chunk::{Chunk, SECTION_SIZE};
    use ultimate_engine::world::position::{BlockPos, ChunkPos, LocalBlockPos};
    use ultimate_server::block;

    let dump_dot = std::env::args().any(|a| a == "--dot");
    let use_parallel = std::env::args().any(|a| a == "--parallel");

    tracing::info!("Ultimate Minecraft -- causal engine demo");
    tracing::info!("Generating flat world...");

    let world = World::new();
    for cx in -4..4 {
        for cz in -4..4 {
            let mut chunk = Chunk::new();
            for x in 0..SECTION_SIZE as u8 {
                for z in 0..SECTION_SIZE as u8 {
                    chunk.set_block(LocalBlockPos { x, y: 0, z }, block::BEDROCK);
                    for y in 1..=3i64 {
                        chunk.set_block(LocalBlockPos { x, y, z }, block::STONE);
                    }
                    chunk.set_block(LocalBlockPos { x, y: 4, z }, block::DIRT);
                }
            }
            world.insert_chunk(ChunkPos::new(cx, cz), chunk);
        }
    }

    tracing::info!("World ready: {} chunks loaded", world.chunk_count());

    let mut graph = CausalGraph::new();
    let rules = ultimate_server::rules::standard();
    let scheduler = Scheduler::new();

    let sand_pos = BlockPos::new(8, 10, 8);
    graph.insert_root(Event {
        payload: EventPayload::BlockSet {
            pos: sand_pos,
            old: block::AIR,
            new: block::SAND,
        },
    });

    tracing::info!("Injected sand at {:?}", sand_pos);

    let total = if use_parallel {
        tracing::info!("Running PARALLEL scheduler...");
        scheduler.run_until_quiet_parallel(&world, &mut graph, &rules, 100)
    } else {
        tracing::info!("Running sequential scheduler...");
        scheduler.run_until_quiet(&world, &mut graph, &rules, 100)
    };

    tracing::info!("Quiescence after {} events ({} in graph)", total, graph.len());

    let landed = world.get_block(BlockPos::new(8, 5, 8));
    tracing::info!("Block at (8, 5, 8): {:?}", landed);

    if landed == block::SAND {
        tracing::info!("Sand landed correctly on the surface.");
    } else {
        tracing::warn!("Unexpected block -- something is off.");
    }

    if dump_dot {
        print!("{}", graph.to_dot());
    }
}

/// Generate a flat world using MC block state IDs (for the real server).
/// Bedrock at y=60, stone y=61-63, dirt at y=64-79. Player spawns at y=80.
fn generate_flat_world_mc(world: &World, chunk_radius: i32) {
    use ultimate_engine::world::position::{ChunkPos, LocalBlockPos};
    use ultimate_engine::world::chunk::Chunk;
    use ultimate_server::block;

    for cx in -chunk_radius..chunk_radius {
        for cz in -chunk_radius..chunk_radius {
            let mut chunk = Chunk::new();

            // Section 7 (y=48..63): bedrock at y=60, stone at y=61-63
            for x in 0..16u8 {
                for z in 0..16u8 {
                    chunk.set_block(LocalBlockPos { x, y: 60, z }, block::BEDROCK);
                    for y in 61..=63i64 {
                        chunk.set_block(LocalBlockPos { x, y, z }, block::STONE);
                    }
                }
            }

            // Section 8 (y=64..79): dirt at y=64
            for x in 0..16u8 {
                for z in 0..16u8 {
                    chunk.set_block(LocalBlockPos { x, y: 64, z }, block::DIRT);
                }
            }

            world.insert_chunk(ChunkPos::new(cx, cz), chunk);
        }
    }
}
