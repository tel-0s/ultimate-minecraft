//! Shared player registry for multiplayer visibility.
//!
//! Tracks all connected players and broadcasts join/leave events so that
//! every connection can send the appropriate tab-list and entity packets.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::RwLock;

use tokio::sync::broadcast;
use uuid::Uuid;

/// Information about a connected player, stored in the registry.
#[derive(Clone, Debug)]
pub struct PlayerInfo {
    pub conn_id: u64,
    pub entity_id: i32,
    pub uuid: Uuid,
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub y_rot: f32,
    pub x_rot: f32,
    pub on_ground: bool,
}

/// Lifecycle events broadcast to all connections.
#[derive(Clone, Debug)]
pub enum PlayerEvent {
    Joined {
        conn_id: u64,
        entity_id: i32,
        uuid: Uuid,
        name: String,
        x: f64,
        y: f64,
        z: f64,
        y_rot: f32,
        x_rot: f32,
    },
    Left {
        conn_id: u64,
        entity_id: i32,
        uuid: Uuid,
    },
    /// A player moved or rotated. Sent at high frequency (~20 Hz per player).
    Moved {
        conn_id: u64,
        entity_id: i32,
        x: f64,
        y: f64,
        z: f64,
        y_rot: f32,
        x_rot: f32,
        on_ground: bool,
    },
}

/// Thread-safe registry of all connected players.
///
/// Uses `std::sync::RwLock` because every operation is brief (no awaits while
/// the lock is held) and the access pattern is read-heavy.
pub struct PlayerRegistry {
    players: RwLock<HashMap<u64, PlayerInfo>>,
    next_entity_id: AtomicI32,
    event_tx: broadcast::Sender<PlayerEvent>,
}

impl PlayerRegistry {
    /// Create a new empty registry. Entity IDs start at 2 (1 is conventionally
    /// the "self" entity on vanilla clients, but we use our own IDs now).
    pub fn new() -> Self {
        // Capacity must accommodate high-frequency movement events from all
        // players. 512 gives ~25 ticks of buffer at 20 players × 1 event/tick.
        let (event_tx, _) = broadcast::channel(512);
        Self {
            players: RwLock::new(HashMap::new()),
            next_entity_id: AtomicI32::new(1),
            event_tx,
        }
    }

    /// Allocate a unique entity ID for a new player.
    pub fn allocate_entity_id(&self) -> i32 {
        self.next_entity_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Register a player and broadcast `PlayerEvent::Joined`.
    ///
    /// Call this *after* you have already sent existing-player info to the
    /// newcomer, so the newcomer doesn't receive its own join event.
    pub fn register(&self, info: PlayerInfo) {
        let event = PlayerEvent::Joined {
            conn_id: info.conn_id,
            entity_id: info.entity_id,
            uuid: info.uuid,
            name: info.name.clone(),
            x: info.x,
            y: info.y,
            z: info.z,
            y_rot: info.y_rot,
            x_rot: info.x_rot,
        };
        self.players
            .write()
            .expect("player registry poisoned")
            .insert(info.conn_id, info);
        // Best-effort: if no subscribers yet, the send fails silently.
        let _ = self.event_tx.send(event);
    }

    /// Update a player's position and rotation, broadcasting `PlayerEvent::Moved`.
    pub fn update_position(
        &self,
        conn_id: u64,
        x: f64,
        y: f64,
        z: f64,
        y_rot: f32,
        x_rot: f32,
        on_ground: bool,
    ) {
        let entity_id = {
            let mut players = self.players.write().expect("player registry poisoned");
            let Some(info) = players.get_mut(&conn_id) else {
                return;
            };
            info.x = x;
            info.y = y;
            info.z = z;
            info.y_rot = y_rot;
            info.x_rot = x_rot;
            info.on_ground = on_ground;
            info.entity_id
        };
        let _ = self.event_tx.send(PlayerEvent::Moved {
            conn_id,
            entity_id,
            x,
            y,
            z,
            y_rot,
            x_rot,
            on_ground,
        });
    }

    /// Remove a player and broadcast `PlayerEvent::Left`.
    pub fn deregister(&self, conn_id: u64) {
        let info = self
            .players
            .write()
            .expect("player registry poisoned")
            .remove(&conn_id);
        if let Some(info) = info {
            let _ = self.event_tx.send(PlayerEvent::Left {
                conn_id: info.conn_id,
                entity_id: info.entity_id,
                uuid: info.uuid,
            });
        }
    }

    /// Snapshot of all currently registered players.
    pub fn snapshot(&self) -> Vec<PlayerInfo> {
        self.players
            .read()
            .expect("player registry poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Number of currently connected players.
    pub fn player_count(&self) -> usize {
        self.players
            .read()
            .expect("player registry poisoned")
            .len()
    }

    /// Subscribe to player lifecycle events.
    pub fn subscribe(&self) -> broadcast::Receiver<PlayerEvent> {
        self.event_tx.subscribe()
    }
}
