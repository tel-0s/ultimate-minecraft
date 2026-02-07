use std::sync::Arc;
use tokio::net::TcpListener;
use ultimate_engine::world::World;

/// Start the TCP listener and accept Minecraft client connections.
pub async fn run(world: Arc<World>, bind_addr: &str) -> anyhow::Result<()> {
    let listener = TcpListener::bind(bind_addr).await?;
    tracing::info!("Listening on {}", bind_addr);

    loop {
        let (stream, addr) = listener.accept().await?;
        tracing::info!("Connection from {}", addr);

        let world = Arc::clone(&world);
        tokio::spawn(async move {
            if let Err(e) = super::connection::handle(stream, world).await {
                tracing::warn!("Connection from {} closed: {}", addr, e);
            }
        });
    }
}
