//! axum web server for the live dashboard.
//!
//! Serves a single-page HTML dashboard at `/` and pushes live metrics +
//! graph snapshots to connected browsers via WebSocket at `/ws`.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;

use super::DashboardState;

/// Start the dashboard web server. Runs forever on its own tasks.
pub async fn start(state: Arc<DashboardState>, port: u16) {
    let app = Router::new()
        .route("/", get(index))
        .route("/ws", get(ws_upgrade))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("Dashboard failed to bind to {}: {}", addr, e);
            return;
        }
    };
    tracing::info!("Dashboard listening on http://{}", addr);

    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!("Dashboard server error: {}", e);
    }
}

/// Serve the embedded single-page dashboard.
async fn index() -> Html<&'static str> {
    Html(include_str!("index.html"))
}

/// Upgrade an HTTP request to a WebSocket connection.
async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<Arc<DashboardState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// Push metrics and graph snapshots to a connected browser.
async fn handle_socket(mut socket: WebSocket, state: Arc<DashboardState>) {
    let mut graph_rx = state.subscribe_graph();
    let mut ticker = tokio::time::interval(Duration::from_millis(200));

    loop {
        tokio::select! {
            // Push metrics every 200 ms.
            _ = ticker.tick() => {
                let snap = state.metrics.snapshot(state.world.chunk_count() as u64);
                let msg = serde_json::json!({
                    "type": "metrics",
                    "data": snap,
                });
                if send_json(&mut socket, &msg).await.is_err() {
                    break;
                }
            }

            // Push graph whenever a new snapshot arrives.
            result = graph_rx.changed() => {
                if result.is_err() {
                    break; // sender dropped
                }
                let graph = graph_rx.borrow_and_update().clone();
                let msg = serde_json::json!({
                    "type": "graph",
                    "data": graph,
                });
                if send_json(&mut socket, &msg).await.is_err() {
                    break;
                }
            }

            // Drain any incoming messages (ping/pong, close).
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {} // ignore pings, text, etc.
                }
            }
        }
    }
}

async fn send_json(socket: &mut WebSocket, value: &serde_json::Value) -> Result<(), ()> {
    let text = value.to_string();
    socket.send(Message::Text(text.into())).await.map_err(|_| ())
}
