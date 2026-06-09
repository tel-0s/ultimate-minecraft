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
        tokio::spawn(async move {
            if let Err(e) = super::connection::handle(stream, world, dashboard, spatial, registry, worldgen, config, physics).await {
                tracing::warn!("Connection from {} closed: {}", addr, e);
            }
        });
    }
}
