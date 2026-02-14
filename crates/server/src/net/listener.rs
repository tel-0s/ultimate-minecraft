use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use ultimate_engine::world::World;

use crate::dashboard::DashboardState;
use crate::event_bus::WorldChangeBatch;
use crate::player_registry::PlayerRegistry;

/// Start the TCP listener and accept Minecraft client connections.
pub async fn run(
    world: Arc<World>,
    dashboard: Arc<DashboardState>,
    bus_tx: broadcast::Sender<WorldChangeBatch>,
    registry: Arc<PlayerRegistry>,
    bind_addr: &str,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(bind_addr).await?;
    tracing::info!("Listening on {}", bind_addr);

    loop {
        let (stream, addr) = listener.accept().await?;
        tracing::info!("Connection from {}", addr);

        let world = Arc::clone(&world);
        let dashboard = Arc::clone(&dashboard);
        let bus_tx = bus_tx.clone();
        let registry = Arc::clone(&registry);
        tokio::spawn(async move {
            if let Err(e) = super::connection::handle(stream, world, dashboard, bus_tx, registry).await {
                tracing::warn!("Connection from {} closed: {}", addr, e);
            }
        });
    }
}
