use std::sync::Arc;
use tokio::net::TcpListener;
use ultimate_engine::world::World;

use crate::config::ServerConfig;
use crate::dashboard::DashboardState;
use crate::event_bus::SpatialBus;
use crate::player_registry::PlayerRegistry;
use crate::worldgen::WorldGen;

/// Start the TCP listener and accept Minecraft client connections.
pub async fn run(
    world: Arc<World>,
    dashboard: Arc<DashboardState>,
    spatial: Arc<SpatialBus>,
    registry: Arc<PlayerRegistry>,
    worldgen: Arc<dyn WorldGen>,
    config: Arc<ServerConfig>,
    physics: crate::physics::PhysicsHandle,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&config.network.bind).await?;
    tracing::info!("Listening on {}", config.network.bind);

    // Telemetry heartbeat: total socket bytes written, to correlate with
    // process RSS during load tests.
    tokio::spawn(async {
        let mut last: u64 = 0;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let now = super::connection::BYTES_WRITTEN.load(std::sync::atomic::Ordering::Relaxed);
            if now != last {
                tracing::info!("net: {:.2} GB written total ({:.1} MB/s)",
                    now as f64 / 1e9, (now - last) as f64 / 10.0 / 1e6);
                last = now;
            }
        }
    });

    loop {
        let (stream, addr) = listener.accept().await?;
        tracing::info!("Connection from {}", addr);

        // Disable Nagle's algorithm. Without this, the kernel batches small
        // writes with up to a 200 ms delay, which serializes chunk streams
        // into a 1-chunk-per-second drip when paired with delayed ACKs.
        if let Err(e) = stream.set_nodelay(true) {
            tracing::warn!("Failed to set TCP_NODELAY on {}: {}", addr, e);
        }

        let world = Arc::clone(&world);
        let dashboard = Arc::clone(&dashboard);
        let spatial = Arc::clone(&spatial);
        let registry = Arc::clone(&registry);
        let worldgen = Arc::clone(&worldgen);
        let config = Arc::clone(&config);
        let physics = physics.clone();
        let fut = super::connection::handle(stream, world, dashboard, spatial, registry, worldgen, config, physics);
        {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::info!("connection task future size: {} KB", std::mem::size_of_val(&fut) / 1024);
            });
        }
        tokio::spawn(async move {
            if let Err(e) = fut.await {
                tracing::warn!("Connection from {} closed: {}", addr, e);
            }
        });
    }
}
