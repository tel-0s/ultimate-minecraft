use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use ultimate_engine::world::World;
use ultimate_server::config::{self, ServerConfig};
use ultimate_server::dashboard::{self, DashboardState};
use ultimate_server::event_bus::{self};
use ultimate_server::persistence;
use ultimate_server::player_registry::PlayerRegistry;
use ultimate_server::worldgen::{self, WorldGen};

/// Pull a `--key value` flag out of the CLI args.
fn cli_arg(key: &str) -> Option<String> {
    std::env::args()
        .skip_while(|a| a != key)
        .nth(1)
}

#[tokio::main]
async fn main() {
    let demo_mode = std::env::args().any(|a| a == "--demo");
    let config_path: PathBuf = cli_arg("--config")
        .unwrap_or_else(|| "server.yaml".into())
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

    // ── Load config (auto-create on first run) ──────────────────────────
    let mut cfg: ServerConfig = match config::load_or_create(&config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Config load failed: {:#}", e);
            return;
        }
    };

    // CLI flags override file values for one-off operator overrides.
    if let Some(v) = cli_arg("--bind") { cfg.network.bind = v; }
    if let Some(v) = cli_arg("--dashboard-port").and_then(|s| s.parse().ok()) {
        cfg.dashboard.port = v;
    }
    if let Some(v) = cli_arg("--world") { cfg.world.dir = v.into(); }
    if let Some(v) = cli_arg("--seed").and_then(|s| s.parse().ok()) {
        cfg.world.seed = v;
    }

    let cfg = Arc::new(cfg);
    tracing::info!(
        "Config loaded from {}: view_distance={}, max_players={}, seed={:#x}",
        config_path.display(),
        cfg.network.view_distance,
        cfg.network.max_players,
        cfg.world.seed,
    );

    // ── Generate base world, then overlay saved modifications ──────────
    let world = Arc::new(World::new());
    // Base generator: pristine procedural pipeline. Persistence diffs
    // against THIS (never the overlay — see persistence::save_world).
    let base_worldgen: Arc<dyn WorldGen> = match worldgen::preset::load(&cfg.world.preset, cfg.world.seed) {
        Ok(g) => g,
        Err(e) => {
            tracing::error!("Worldgen preset {:?} failed to load: {:#}", cfg.world.preset, e);
            return;
        }
    };
    // Live delta store + overlay: every chunk generation re-applies saved
    // edits, which is what makes eviction / lazy regeneration faithful.
    let delta_store = persistence::new_delta_store();
    let worldgen: Arc<dyn WorldGen> = Arc::new(persistence::DeltaOverlayGen::new(
        Arc::clone(&base_worldgen),
        Arc::clone(&delta_store),
    ));
    // Fingerprint of (preset content, seed): stamped into saved chunks so
    // stale-generator terrain is detected and regenerated at load.
    let gen_fp = match worldgen::preset::fingerprint(&cfg.world.preset, cfg.world.seed) {
        Ok(fp) => fp,
        Err(e) => {
            tracing::error!("Worldgen preset {:?} failed to fingerprint: {:#}", cfg.world.preset, e);
            return;
        }
    };
    tracing::info!(
        "Generating world from preset {:?} (seed {:#x})...",
        cfg.world.preset, cfg.world.seed,
    );
    worldgen.pregenerate_radius(&world, cfg.world.pregenerate_radius);
    tracing::info!(
        "Base world ready: {} chunks pre-generated; further chunks generated on demand",
        world.chunk_count(),
    );

    // Load saved (player-modified) chunks on top of the generated base,
    // populating the delta store for future regenerations.
    match persistence::load_into(&world, &cfg.world.dir, gen_fp, &*worldgen, Some(&delta_store)) {
        Ok(0) => tracing::info!("No saved modifications found"),
        Ok(n) => tracing::info!("Loaded {} modified chunks from {}", n, cfg.world.dir.display()),
        Err(e) => tracing::error!("Failed to load saved chunks: {:#}", e),
    }

    // Start live dashboard (non-blocking — runs on its own tasks).
    let dashboard = Arc::new(DashboardState::new(Arc::clone(&world)));
    let dash = Arc::clone(&dashboard);
    let dashboard_port = cfg.dashboard.port;
    tokio::spawn(async move {
        dashboard::server::start(dash, dashboard_port).await;
    });

    // Spatial event bus (Phase 6f): world changes and entity moves are
    // delivered per-region to nearby subscribers only.
    let spatial = event_bus::SpatialBus::new();

    // ── Cluster membership (Phase 6f, optional) ──────────────────────────
    // Join the mesh BEFORE physics starts so region routing is node-aware
    // from the first event. A gateway (node_id >= physics_nodes) owns no
    // regions: it serves players from its replica and submits all physics.
    let mesh = if cfg.cluster.enabled {
        let listener = match std::net::TcpListener::bind(&cfg.cluster.listen) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("cluster listen {} failed: {e}", cfg.cluster.listen);
                return;
            }
        };
        let physics_nodes = if cfg.cluster.physics_nodes == 0 {
            cfg.cluster.total_nodes
        } else {
            cfg.cluster.physics_nodes
        };
        tracing::info!(
            "Joining cluster as node {}/{} ({} physics nodes{})...",
            cfg.cluster.node_id, cfg.cluster.total_nodes, physics_nodes,
            if cfg.cluster.node_id >= physics_nodes { ", GATEWAY" } else { "" },
        );
        match ultimate_server::cluster::ClusterMesh::form_with_physics(
            cfg.cluster.node_id,
            cfg.cluster.total_nodes,
            physics_nodes,
            &listener,
            &cfg.cluster.peers,
        ) {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::error!("cluster mesh formation failed: {e:#}");
                return;
            }
        }
    } else {
        None
    };

    // ── Physics service ──────────────────────────────────────────────────
    // Partition workers own the shared causal graphs; connections and
    // simulation layers submit root events and the spatial bus carries
    // results to interested connections.
    let physics = ultimate_server::physics::start(
        Arc::clone(&world),
        ultimate_server::rules::standard,
        Arc::clone(&spatial),
        Some(Arc::clone(&dashboard)),
        ultimate_server::physics::PhysicsOptions {
            workers: cfg.physics.workers,
            pin_workers: cfg.physics.pin_workers,
            rebalance: cfg.physics.rebalance,
            cluster: mesh.as_ref().map(|m| ultimate_server::physics::ClusterCtx {
                mesh: Arc::clone(m),
            }),
        },
    );
    if let Some(m) = &mesh {
        m.attach(Arc::clone(&world), Arc::clone(&spatial), physics.clone());
    }

    // Ambient simulation layers (empty for now).
    let sim_layers: Vec<Box<dyn ultimate_server::simulation::SimulationLayer>> = vec![];
    ultimate_server::simulation::start(Arc::clone(&world), sim_layers, physics.clone());

    // Shared player registry for multiplayer visibility.
    let registry = Arc::new(PlayerRegistry::new(Arc::clone(&spatial)));

    // ── Periodic autosave ────────────────────────────────────────────────
    let save_world_ref = Arc::clone(&world);
    let save_dir = cfg.world.dir.clone();
    let save_worldgen = Arc::clone(&base_worldgen); // diff against the BASE
    let save_deltas = Arc::clone(&delta_store);
    let autosave = Duration::from_secs(cfg.world.autosave_interval_secs);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(autosave);
        interval.tick().await; // first tick is immediate, skip it
        loop {
            interval.tick().await;
            tracing::info!("Autosaving...");
            match persistence::save_world(
                &save_world_ref, &save_dir, gen_fp, &*save_worldgen, Some(&save_deltas),
            ) {
                Ok(n) => tracing::info!("Autosave complete: {} chunks", n),
                Err(e) => tracing::error!("Autosave failed: {:#}", e),
            }
        }
    });

    // ── Chunk eviction (Phase 6c): memory bounded by active area ────────
    let keep_radius = if cfg.world.keep_radius == 0 {
        cfg.network.view_distance + 8
    } else {
        cfg.world.keep_radius
    };
    ultimate_server::eviction::start(
        Arc::clone(&world),
        Arc::clone(&registry),
        keep_radius,
        cfg.world.pregenerate_radius,
        cfg.world.eviction_interval_secs,
    );

    // ── Start listener with graceful shutdown ────────────────────────────
    tracing::info!("Starting Minecraft 1.21.11 server on {}", cfg.network.bind);

    tokio::select! {
        result = ultimate_server::net::listener::run(
            Arc::clone(&world), dashboard, spatial, registry,
            Arc::clone(&worldgen),
            Arc::clone(&cfg),
            physics,
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
    match persistence::save_world(&world, &cfg.world.dir, gen_fp, &*base_worldgen, None) {
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

