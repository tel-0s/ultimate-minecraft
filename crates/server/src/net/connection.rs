//! Per-client connection handler implementing the MC 1.21.11 protocol state machine.
//!
//! Handshake -> Status | Login -> Configuration -> Play

use std::collections::{HashSet, VecDeque};
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use azalea_auth::game_profile::GameProfile;
use azalea_chat::FormattedText;
use azalea_protocol::common::movements::{PositionMoveRotation, RelativeMovements};
use azalea_protocol::packets::ClientIntention;
use azalea_protocol::packets::game::{
    ClientboundGamePacket, ClientboundGameEvent, ClientboundLogin,
    ClientboundPlayerPosition, ClientboundSetChunkCacheCenter,
    ClientboundPlayerInfoUpdate, ClientboundPlayerInfoRemove,
    ClientboundAddEntity, ClientboundRemoveEntities,
    ClientboundTeleportEntity, ClientboundRotateHead,
    ClientboundChunkBatchStart, ClientboundChunkBatchFinished,
    ClientboundSystemChat,
    ServerboundGamePacket,
};
use azalea_protocol::packets::game::c_game_event::EventType;
use azalea_protocol::packets::game::c_player_info_update::{ActionEnumSet, PlayerInfoEntry};
use azalea_core::delta::LpVec3;
use azalea_registry::builtin::EntityKind;
use azalea_protocol::packets::handshake::ServerboundHandshakePacket;
use azalea_protocol::packets::login::{
    ClientboundLoginDisconnect, ClientboundLoginPacket,
};
use azalea_protocol::packets::Packet;
use azalea_protocol::packets::common::CommonPlayerSpawnInfo;
use azalea_protocol::read::read_packet;
use azalea_protocol::write::write_packet;
use azalea_core::game_type::{GameMode, OptionalGameType};
use azalea_core::position::Vec3;
use azalea_entity::LookDirection;
use azalea_registry::DataRegistry;
use azalea_registry::data::DimensionKind;
use azalea_registry::identifier::Identifier;
use azalea_core::entity_id::MinecraftEntityId;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use ultimate_engine::world::World;
use uuid::Uuid;

use crate::config::ServerConfig;
use crate::dashboard::DashboardState;
use crate::event_bus::{self};
use crate::player_registry::{PlayerEvent, PlayerInfo, PlayerRegistry};
use crate::worldgen::WorldGen;

#[allow(unused_imports)]
use super::chunk_stream::*;
#[allow(unused_imports)]
use super::entity_view::*;
#[allow(unused_imports)]
use super::handshake::*;

/// Monotonic connection ID counter for identifying change sources.
static NEXT_CONN_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Admission control for bulk chunk streaming (`network.stream_permits`):
/// at most N connections drain their deferred chunk queues at once, so a
/// join storm streams in fast waves instead of 10k simultaneous trickles
/// (where one chunk packet can exceed the client's 30s read timeout).
/// Waiters idle in the main loop with keep-alives flowing.
static STREAM_PERMITS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> =
    std::sync::OnceLock::new();

/// Install the streaming-admission semaphore from server config. Called
/// once by the listener at startup so the permit count comes from THE
/// config, not whichever connection happened to arrive first.
pub fn init_stream_permits(stream_permits: usize) {
    let _ = STREAM_PERMITS.get_or_init(|| {
        let n = match stream_permits {
            0 => tokio::sync::Semaphore::MAX_PERMITS,
            n => n,
        };
        Arc::new(tokio::sync::Semaphore::new(n))
    });
}

/// Total bytes successfully handed to client sockets (all connections).
/// Load-test diagnostic: correlate with process RSS to distinguish heap
/// retention from socket-layer accumulation.
pub static BYTES_WRITTEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// AsyncWrite wrapper counting bytes accepted by the socket.
pub struct CountingWriter<W> {
    inner: W,
}

impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for CountingWriter<W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let poll = std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
        if let std::task::Poll::Ready(Ok(n)) = &poll {
            BYTES_WRITTEN.fetch_add(*n as u64, std::sync::atomic::Ordering::Relaxed);
        }
        poll
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Handle a single client connection through all protocol phases.
pub async fn handle(
    stream: TcpStream,
    world: Arc<World>,
    dashboard: Arc<DashboardState>,
    spatial: Arc<crate::event_bus::SpatialBus>,
    registry: Arc<PlayerRegistry>,
    worldgen: Arc<dyn WorldGen>,
    config: Arc<ServerConfig>,
    physics: crate::physics::PhysicsHandle,
) -> Result<()> {
    let (read, write) = stream.into_split();
    let mut read = read;
    let mut write = CountingWriter { inner: write };
    let mut buf = Cursor::new(Vec::new());

    // No encryption or compression in offline mode.
    let mut cipher_enc: Option<azalea_crypto::Aes128CfbEnc> = None;
    let mut cipher_dec: Option<azalea_crypto::Aes128CfbDec> = None;
    let compression: Option<u32> = None;

    // ── Phase 1: Handshake ──────────────────────────────────────────────
    let handshake = read_packet::<ServerboundHandshakePacket, _>(
        &mut read, &mut buf, compression, &mut cipher_dec,
    ).await?;

    let intention = match handshake {
        ServerboundHandshakePacket::Intention(p) => p,
    };

    tracing::info!(
        "Handshake: protocol={}, host={}:{}, intention={:?}",
        intention.protocol_version,
        intention.hostname,
        intention.port,
        intention.intention,
    );

    match intention.intention {
        ClientIntention::Status => {
            handle_status(&mut read, &mut write, &mut buf, compression, &mut cipher_enc, &mut cipher_dec, &registry, &config.network).await?;
        }
        ClientIntention::Login => {
            // Admission: the advertised max is enforced, not decorative.
            // (Small TOCTOU window under concurrent joins is fine — the
            // cap is capacity protection, not a hard invariant.)
            let cap = config.network.max_players as usize;
            if cap > 0 && registry.player_count() >= cap {
                let reject: ClientboundLoginPacket = ClientboundLoginDisconnect {
                    reason: azalea_chat::FormattedText::from(format!(
                        "Server is full ({cap} players)"
                    )),
                }
                .into_variant();
                write_packet(&reject, &mut write, compression, &mut cipher_enc).await?;
                return Ok(());
            }
            let (name, uuid) = handle_login(&mut read, &mut write, &mut buf, compression, &mut cipher_enc, &mut cipher_dec).await?;
            handle_configuration(&mut read, &mut write, &mut buf, compression, &mut cipher_enc, &mut cipher_dec).await?;
            dashboard.metrics.player_joined();
            // handle_play registers/deregisters with the player registry internally.
            let result = handle_play(&mut read, &mut write, &mut buf, compression, &mut cipher_enc, &mut cipher_dec, &world, &name, uuid, &dashboard, &spatial, &registry, &*worldgen, &config, &physics).await;
            dashboard.metrics.player_left();
            result?;
        }
        _ => {
            tracing::warn!("Unsupported intention: {:?}", intention.intention);
        }
    }

    Ok(())
}

// ── Play ────────────────────────────────────────────────────────────────

async fn handle_play<R, W>(
    read: &mut R, write: &mut W, buf: &mut Cursor<Vec<u8>>,
    compression: Option<u32>,
    cipher_enc: &mut Option<azalea_crypto::Aes128CfbEnc>,
    cipher_dec: &mut Option<azalea_crypto::Aes128CfbDec>,
    world: &World,
    player_name: &str,
    player_uuid: Uuid,
    // Cascade metrics moved to the physics service in 6b-1; the slot stays
    // for future per-connection dashboards (latency, packet rates).
    _dashboard: &DashboardState,
    spatial: &Arc<crate::event_bus::SpatialBus>,
    registry: &PlayerRegistry,
    worldgen: &dyn WorldGen,
    config: &ServerConfig,
    physics: &crate::physics::PhysicsHandle,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + Sync,
    W: AsyncWrite + Unpin + Send,
{
    let entity_id = registry.allocate_entity_id();
    let spawn_x = 8.0_f64;
    let spawn_z = 8.0_f64;
    // Pre-generate the spawn column so the surface is sampled from the
    // committed world, not just the noise function — this matters once
    // persistence layers modifications on top of the generator.
    worldgen.ensure_generated(&world, (spawn_x as i32) >> 4, (spawn_z as i32) >> 4);
    let spawn_y = worldgen.spawn_y(spawn_x as i64, spawn_z as i64);

    // Send Login (Play) -- this initializes the client's world state
    let login: ClientboundGamePacket = ClientboundLogin {
        player_id: MinecraftEntityId(entity_id),
        hardcore: false,
        // 26.2: tells the client whether the server verifies profiles.
        online_mode: false,
        levels: vec![Identifier::new("minecraft:overworld")],
        max_players: config.network.max_players as i32,
        chunk_radius: config.network.view_distance.max(0) as u32,
        simulation_distance: config.network.simulation_distance.max(0) as u32,
        reduced_debug_info: false,
        show_death_screen: true,
        do_limited_crafting: false,
        common: CommonPlayerSpawnInfo {
            dimension_type: DimensionKind::new_raw(0), // overworld = 0
            dimension: Identifier::new("minecraft:overworld"),
            seed: 0,
            game_type: GameMode::Creative,
            previous_game_type: OptionalGameType(None),
            is_debug: false,
            is_flat: true,
            last_death_location: None,
            portal_cooldown: 0,
            sea_level: 63,
        },
        enforces_secure_chat: false,
    }.into_variant();
    write_packet(&login, write, compression, cipher_enc).await?;

    // Send player position (teleport)
    let position: ClientboundGamePacket = ClientboundPlayerPosition {
        id: 1,
        change: PositionMoveRotation {
            pos: Vec3 {
                x: spawn_x,
                y: spawn_y,
                z: spawn_z,
            },
            delta: Vec3 {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            look_direction: LookDirection::new(0.0, 0.0),
        },
        relative: RelativeMovements::default(),
    }.into_variant();
    write_packet(&position, write, compression, cipher_enc).await?;

    // Wait for client to confirm teleport
    let tp_ack = read_packet::<ServerboundGamePacket, _>(read, buf, compression, cipher_dec).await?;
    tracing::debug!("Teleport ack: {:?}", tp_ack);

    // Send Game Event: "start waiting for level chunks" (event 13)
    let game_event: ClientboundGamePacket = ClientboundGameEvent {
        event: EventType::WaitForLevelChunks,
        param: 0.0,
    }.into_variant();
    write_packet(&game_event, write, compression, cipher_enc).await?;

    // Set center chunk
    let chunk_x = (spawn_x as i32) >> 4;
    let chunk_z = (spawn_z as i32) >> 4;
    let center: ClientboundGamePacket = ClientboundSetChunkCacheCenter {
        x: chunk_x,
        z: chunk_z,
    }.into_variant();
    write_packet(&center, write, compression, cipher_enc).await?;

    // Send chunk data for a small area around the player.
    // MC 1.20+ requires chunks to be wrapped in ChunkBatchStart/Finished
    // markers — without these, the client receives the data but won't
    // render the chunks (blocks remain interactable but invisible).
    let view_distance = config.network.view_distance;
    // null in config → a small inner ring is sent synchronously; everything
    // else streams through the deferred queue from the main loop, where
    // keep-alives interleave between chunk batches. Sending the full view
    // synchronously here meant a client could sit >30s without a single
    // packet during a join storm and time itself out (10k load test).
    let immediate_radius = config.network.immediate_radius.unwrap_or(2).min(view_distance);
    let mut loaded_chunks: HashSet<(i32, i32)> = HashSet::new();
    // Queue for deferred chunk loading -- chunks are sent progressively to
    // avoid blocking the event loop during the initial load and fast movement.
    let mut chunk_send_queue: VecDeque<(i32, i32)> = VecDeque::new();

    // Bulk-streaming admission (see STREAM_PERMITS). Uncontended, the
    // permit is granted instantly and joining behaves as before; in a
    // join storm, permit-less connections defer EVERYTHING to the queue
    // and stream when their wave comes (keep-alives flowing meanwhile).
    init_stream_permits(config.network.stream_permits);
    let stream_sem = STREAM_PERMITS.get().expect("init_stream_permits ran").clone();
    let mut stream_permit = Arc::clone(&stream_sem).try_acquire_owned().ok();

    let mut immediate: Vec<(i32, i32)> = Vec::new();
    let mut deferred: Vec<(i32, i32)> = Vec::new();
    for cx in (chunk_x - view_distance)..=(chunk_x + view_distance) {
        for cz in (chunk_z - view_distance)..=(chunk_z + view_distance) {
            let inner = (cx - chunk_x).abs().max((cz - chunk_z).abs()) <= immediate_radius;
            if inner && stream_permit.is_some() {
                immediate.push((cx, cz));
            } else {
                deferred.push((cx, cz));
            }
            loaded_chunks.insert((cx, cz));
        }
    }

    if !immediate.is_empty() {
        let batch_start: ClientboundGamePacket = ClientboundChunkBatchStart.into_variant();
        write_packet(&batch_start, write, compression, cipher_enc).await?;
        for &(cx, cz) in &immediate {
            worldgen.ensure_generated(world, cx, cz);
            send_chunk_from_world(write, compression, cipher_enc, world, &*worldgen, cx, cz).await?;
        }
        let batch_end: ClientboundGamePacket = ClientboundChunkBatchFinished {
            batch_size: immediate.len() as u32,
        }.into_variant();
        write_packet(&batch_end, write, compression, cipher_enc).await?;
    }

    // Outer ring (everything, when admission deferred us) streams from the
    // main loop, nearest first.
    deferred.sort_by_key(|(cx, cz)| (cx - chunk_x).abs().max((cz - chunk_z).abs()));
    chunk_send_queue.extend(deferred.iter());

    let mut current_chunk_x = chunk_x;
    let mut current_chunk_z = chunk_z;

    tracing::info!("{} joined the game at ({}, {}, {})", player_name, spawn_x, spawn_y, spawn_z);

    // ── Physics submission (Phase 6b-1) ──────────────────────────────────
    // This connection is a pure event SOURCE: block actions are submitted
    // to the shared physics service and acknowledged immediately. All
    // resulting world changes — including our own — come back through the
    // event bus as `ChangeSource::Physics` batches.
    
use azalea_inventory::ItemStack;
    
    use azalea_protocol::packets::game::{
        ClientboundBlockUpdate, ClientboundBlockChangedAck,
        s_player_action::Action,
    };
    

    

    // Unique ID for this connection (used to filter self-originated bus messages).
    let conn_id = NEXT_CONN_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // RAII guard so deregister always runs, even if a `?` early-exits the
    // function (e.g. client TCP drop). Without this the player stays in
    // `registry.snapshot()` forever, showing as "online" in the multiplayer
    // ping screen until the server restarts. Also despawns the player's
    // EntityStore mirror (safe to read-then-guard: this connection was its
    // only writer, and it is gone).
    struct DeregisterGuard<'a> {
        registry: &'a PlayerRegistry,
        conn_id: u64,
        world: &'a World,
        physics: crate::physics::PhysicsHandle,
        player_eid: i32,
    }
    impl Drop for DeregisterGuard<'_> {
        fn drop(&mut self) {
            self.registry.deregister(self.conn_id);
            let pid = crate::rules::entity::player_entity_id(self.player_eid);
            if let Some(cur) = self.world.entities().get(pid) {
                self.physics.submit_events(vec![ultimate_engine::causal::event::Event {
                    payload: ultimate_engine::causal::event::EventPayload::EntitySet {
                        id: pid,
                        old: Some(cur),
                        new: None,
                    },
                }]);
            }
        }
    }
    let _deregister_guard = DeregisterGuard {
        registry,
        conn_id,
        world,
        physics: physics.clone(),
        player_eid: entity_id,
    };

    // Spatial subscription (Phase 6f): world changes and entity moves are
    // delivered only for regions near this player; re-pointed on chunk
    // border crossings.
    // Item entities this client has been sent (Phase 5) — the client's
    // ground truth for spawn/teleport/remove packet correctness.
    let mut spawned_items: HashSet<u64> = HashSet::new();

    let (mut spatial_sub, mut spatial_rx) = spatial.subscribe();
    let initial_regions = spatial_sub.set_view(chunk_x, chunk_z, config.network.view_distance);
    // Backfill entities already at rest in view — they emit no events, so
    // a newcomer would otherwise never see them.
    backfill_region_entities(
        write, compression, cipher_enc, world, &initial_regions, &mut spawned_items,
    ).await?;
    // Subscribe to player lifecycle events (join/leave/chat — global).
    let mut player_rx = registry.subscribe();

    // ── Multiplayer: send existing players to newcomer, then register ───
    // Presence caps (`network.tab_list_cap` / `network.entity_spawn_cap`):
    // uncapped, presence is O(N²) bytes across clients — at 10k players
    // the join-storm tab/spawn flood alone is ~12 GB and chokes the write
    // plane. Track WHO this client knows so removals stay consistent.
    let tab_cap = match config.network.tab_list_cap {
        0 => usize::MAX,
        n => n,
    };
    let spawn_cap = match config.network.entity_spawn_cap {
        0 => usize::MAX,
        n => n,
    };
    let mut tab_listed: HashSet<uuid::Uuid> = HashSet::new();
    let mut spawned_entities: HashSet<i32> = HashSet::new();

    // Step 1: Tell this client about every player already online (plus
    // ourselves) in ONE multi-entry tab-list packet — a packet per player
    // made joining O(N) packets and a join storm O(N²) server-wide.
    let existing_players = registry.snapshot();
    let mut tab_entries: Vec<PlayerInfoEntry> = Vec::new();
    for p in existing_players.iter().take(tab_cap) {
        tab_listed.insert(p.uuid);
        tab_entries.push(PlayerInfoEntry {
            profile: GameProfile {
                uuid: p.uuid,
                name: p.name.clone(),
                properties: Default::default(),
            },
            listed: true,
            latency: 0,
            game_mode: GameMode::Creative,
            display_name: None,
            list_order: 0,
            update_hat: false,
            chat_session: None,
        });
    }
    tab_entries.push(PlayerInfoEntry {
        profile: GameProfile {
            uuid: player_uuid,
            name: player_name.to_owned(),
            properties: Default::default(),
        },
        listed: true,
        latency: 0,
        game_mode: GameMode::Creative,
        display_name: None,
        list_order: 0,
        update_hat: false,
        chat_session: None,
    });
    let info_packet: ClientboundGamePacket = ClientboundPlayerInfoUpdate {
        actions: ActionEnumSet {
            add_player: true,
            initialize_chat: false,
            update_game_mode: true,
            update_listed: true,
            update_latency: true,
            update_display_name: false,
            update_hat: false,
            update_list_order: false,
        },
        entries: tab_entries,
    }.into_variant();
    write_packet(&info_packet, write, compression, cipher_enc).await?;

    // Spawn each existing player's entity at their current position.
    for p in existing_players.iter().take(spawn_cap) {
        spawned_entities.insert(p.entity_id);
        let spawn_packet: ClientboundGamePacket = ClientboundAddEntity {
            id: MinecraftEntityId(p.entity_id),
            uuid: p.uuid,
            entity_type: EntityKind::Player,
            position: Vec3 { x: p.x, y: p.y, z: p.z },
            movement: LpVec3::Zero,
            x_rot: degrees_to_byte_angle(p.x_rot),
            y_rot: degrees_to_byte_angle(p.y_rot),
            y_head_rot: degrees_to_byte_angle(p.y_rot),
            data: 0,
        }.into_variant();
        write_packet(&spawn_packet, write, compression, cipher_enc).await?;
    }
    // Without this, the snapshot (up to one PlayerInfo per online player)
    // lives in this stack frame for the connection's whole lifetime —
    // ~0.5 MB × 10k connections was gigabytes in the 10k load test.
    drop(existing_players);

    // Player mirror in the EntityStore (Phase 5 unification): position
    // authority for RULES and cluster replicas. The registry keeps
    // identity and the movement render path. Guarded spawn through
    // physics so it write-logs (replicas learn it via WriteSync).
    let player_pid = crate::rules::entity::player_entity_id(entity_id);
    physics.submit_events(vec![ultimate_engine::causal::event::Event {
        payload: ultimate_engine::causal::event::EventPayload::EntitySet {
            id: player_pid,
            old: None,
            new: Some(crate::rules::entity::player_state(
                ultimate_engine::world::entity::Vec3::new(spawn_x, spawn_y, spawn_z),
                0.0,
                0.0,
                world.now(),
            )),
        },
    }]);

    // Step 3: Register in the shared registry -- this broadcasts PlayerEvent::Joined
    // to all other connections so they can send the tab-list + entity spawn packets.
    registry.register(PlayerInfo {
        conn_id,
        entity_id,
        uuid: player_uuid,
        name: player_name.to_owned(),
        x: spawn_x,
        y: spawn_y,
        z: spawn_z,
        y_rot: 0.0,
        x_rot: 0.0,
        on_ground: false,
    });

    // Track player position and rotation for movement relaying.
    let mut player_x = spawn_x;
    let mut player_y = spawn_y;
    let mut player_z = spawn_z;
    let mut player_y_rot: f32 = 0.0;
    let mut player_x_rot: f32 = 0.0;
    // Creative inventory model lives in the gameplay layer.
    let mut inventory = crate::gameplay::Inventory::default();

    // ── Main loop: keep-alive + handle incoming packets + bus ────────────
    let mut keepalive_timer = tokio::time::interval(Duration::from_secs(15));
    let mut keepalive_id: u64 = 0;
    // Diagnostics: a keep-alive gap above 25s means this client was one
    // missed packet from a vanilla 30s timeout — log who and how long.
    let mut last_keepalive_sent: Option<std::time::Instant> = None;
    let mut stream_wait_started: Option<std::time::Instant> = None;

    // Max chunks to send per loop iteration. Keeps the loop responsive while
    // still making rapid progress on the queue.
    let chunks_per_iter: usize = config.network.chunks_per_iter;

    // Track chunks physically sent to the client. Deferred chunks are added to
    // `loaded_chunks` optimistically before being sent, so this set lets us
    // detect and re-queue any that slip through the cracks. The initial load
    // also defers its outer ring, so anything still queued is not yet sent.
    let mut sent_to_client: HashSet<(i32, i32)> = loaded_chunks
        .iter()
        .copied()
        .filter(|pos| !chunk_send_queue.contains(pos))
        .collect();

    loop {
        // ── Eagerly drain chunk queue before waiting for events ──────────
        // Only while holding a bulk-streaming permit (admission control —
        // without it we wait for the permit arm in the select below).
        // Wrap each drain pass in a ChunkBatchStart/Finished pair so the
        // client renders the chunks (1.20+ requirement).
        if stream_permit.is_some() {
            let mut to_send: Vec<(i32, i32)> = Vec::new();
            while to_send.len() < chunks_per_iter {
                let Some((cx, cz)) = chunk_send_queue.pop_front() else { break };
                if !loaded_chunks.contains(&(cx, cz)) {
                    sent_to_client.remove(&(cx, cz));
                    continue; // Player moved away before this chunk was sent.
                }
                to_send.push((cx, cz));
            }

            if !to_send.is_empty() {
                let batch_start: ClientboundGamePacket = ClientboundChunkBatchStart.into_variant();
                write_packet(&batch_start, write, compression, cipher_enc).await?;

                for &(cx, cz) in &to_send {
                    worldgen.ensure_generated(world, cx, cz);
                    send_chunk_from_world(write, compression, cipher_enc, world, &*worldgen, cx, cz).await?;
                    sent_to_client.insert((cx, cz));
                }

                let batch_end: ClientboundGamePacket = ClientboundChunkBatchFinished {
                    batch_size: to_send.len() as u32,
                }.into_variant();
                write_packet(&batch_end, write, compression, cipher_enc).await?;
            }
        }

        // ── Self-heal: when queue is empty, re-queue any claimed-but-unsent chunks ──
        if chunk_send_queue.is_empty() {
            sent_to_client.retain(|pos| loaded_chunks.contains(pos));
            for pos in loaded_chunks.iter() {
                if !sent_to_client.contains(pos) {
                    chunk_send_queue.push_back(*pos);
                }
            }
        }

        // Done streaming: hand the permit to the next waiting connection.
        if chunk_send_queue.is_empty() {
            stream_permit = None;
        } else if stream_permit.is_none() && stream_wait_started.is_none() {
            stream_wait_started = Some(std::time::Instant::now());
        }

        tokio::select! {
            // When chunks are queued and we hold the streaming permit, yield
            // immediately so we cycle back to the drain at the top of the
            // loop. This keeps chunk loading progressing rapidly without
            // starving event processing.
            _ = std::future::ready(()), if stream_permit.is_some() && !chunk_send_queue.is_empty() => {}
            // Chunks queued but no permit yet: wait for admission. Other
            // arms (keep-alive, reads, lifecycle) stay live while we wait.
            permit = Arc::clone(&stream_sem).acquire_owned(), if stream_permit.is_none() && !chunk_send_queue.is_empty() => {
                stream_permit = permit.ok();
                if let Some(t0) = stream_wait_started.take() {
                    let waited = t0.elapsed();
                    if waited > Duration::from_secs(30) {
                        tracing::info!("{} admitted to stream after {:.1}s wait ({} chunks queued)",
                            player_name, waited.as_secs_f64(), chunk_send_queue.len());
                    }
                }
            }
            _ = keepalive_timer.tick() => {
                let now = std::time::Instant::now();
                if let Some(prev) = last_keepalive_sent {
                    let gap = now.duration_since(prev);
                    if gap > Duration::from_secs(25) {
                        tracing::warn!("{} keep-alive gap {:.1}s (client times out at 30s)",
                            player_name, gap.as_secs_f64());
                    }
                }
                last_keepalive_sent = Some(now);
                keepalive_id += 1;
                let ka: ClientboundGamePacket = azalea_protocol::packets::game::ClientboundKeepAlive {
                    id: keepalive_id,
                }.into_variant();
                write_packet(&ka, write, compression, cipher_enc).await?;
            }
            result = read_packet::<ServerboundGamePacket, _>(read, buf, compression, cipher_dec) => {
                match result {
                    Ok(packet) => {
                        match packet {
                            // ── Block breaking (creative = instant) ──────
                            ServerboundGamePacket::PlayerAction(action) => {
                                if action.action == Action::StartDestroyBlock {
                                    let pos = action.pos;
                                    let epos = ultimate_engine::world::position::BlockPos::new(
                                        pos.x as i64, pos.y as i64, pos.z as i64,
                                    );

                                    // Gameplay decides the action; physics'
                                    // stale-precondition guard drops it if
                                    // another event got to the cell first.
                                    physics.submit_action(crate::gameplay::break_action(&world, epos));

                                    // Acknowledge the sequence immediately; the
                                    // authoritative block updates arrive via the
                                    // event bus once the cascade settles.
                                    let ack: ClientboundGamePacket = ClientboundBlockChangedAck {
                                        seq: action.seq,
                                    }.into_variant();
                                    write_packet(&ack, write, compression, cipher_enc).await?;
                                }
                            }

                            // ── Block placing / interaction ─────────────
                            ServerboundGamePacket::UseItemOn(place) => {
                                let hit = &place.block_hit;

                                // Right-clicking an interactive block uses
                                // it instead of placing (gameplay decides).
                                let clicked = ultimate_engine::world::position::BlockPos::new(
                                    hit.block_pos.x as i64,
                                    hit.block_pos.y as i64,
                                    hit.block_pos.z as i64,
                                );
                                if let Some(action) = crate::gameplay::use_block_action(&world, clicked) {
                                    physics.submit_action(action);
                                    let ack: ClientboundGamePacket = ClientboundBlockChangedAck {
                                        seq: place.seq,
                                    }.into_variant();
                                    write_packet(&ack, write, compression, cipher_enc).await?;
                                    continue;
                                }
                                // Placement (face offset, orientation, stair
                                // shape) is a gameplay decision; the cascade
                                // runs in physics and comes back via the bus.
                                let Some(action) = crate::gameplay::place_action(
                                    &world,
                                    inventory.held(),
                                    hit,
                                    player_y_rot,
                                    player_x_rot,
                                ) else {
                                    continue; // nothing to place
                                };
                                physics.submit_action(action);

                                // Acknowledge immediately; authoritative updates
                                // arrive via the event bus once the cascade settles.
                                let ack: ClientboundGamePacket = ClientboundBlockChangedAck {
                                    seq: place.seq,
                                }.into_variant();
                                write_packet(&ack, write, compression, cipher_enc).await?;
                            }

                            // ── Creative inventory slot update ───────────
                            ServerboundGamePacket::SetCreativeModeSlot(slot) => {
                                let kind = match &slot.item_stack {
                                    ItemStack::Present(data) => Some(data.kind),
                                    ItemStack::Empty => None,
                                };
                                inventory.set_creative_slot(slot.slot_num as i32, kind);
                            }

                            // ── Hotbar slot selection ────────────────────
                            ServerboundGamePacket::SetCarriedItem(carried) => {
                                inventory.select(carried.slot as usize);
                            }

                            // ── Player movement ───────────────────────
                            ServerboundGamePacket::MovePlayerPos(pkt) => {
                                player_x = pkt.pos.x;
                                player_y = pkt.pos.y;
                                player_z = pkt.pos.z;
                                registry.update_position(
                                    conn_id, player_x, player_y, player_z,
                                    player_y_rot, player_x_rot, pkt.flags.on_ground,
                                );
                                mirror_player_entity(
                                    world, physics, player_pid,
                                    player_x, player_y, player_z, player_y_rot, player_x_rot,
                                );
                                update_loaded_chunks(
                                    write, compression, cipher_enc, world,
                                    &*worldgen,
                                    player_x, player_z, view_distance, immediate_radius,
                                    &mut current_chunk_x, &mut current_chunk_z,
                                    &mut loaded_chunks, &mut sent_to_client,
                                    &mut chunk_send_queue,
                                ).await?;
                                let added = spatial_sub.set_view(current_chunk_x, current_chunk_z, view_distance);
                                backfill_region_entities(
                                    write, compression, cipher_enc, world, &added, &mut spawned_items,
                                ).await?;
                                try_item_pickup(
                                    write, compression, cipher_enc, world, physics,
                                    entity_id, player_x, player_y, player_z,
                                ).await?;
                            }
                            ServerboundGamePacket::MovePlayerPosRot(pkt) => {
                                player_x = pkt.pos.x;
                                player_y = pkt.pos.y;
                                player_z = pkt.pos.z;
                                player_y_rot = pkt.look_direction.y_rot();
                                player_x_rot = pkt.look_direction.x_rot();
                                registry.update_position(
                                    conn_id, player_x, player_y, player_z,
                                    player_y_rot, player_x_rot, pkt.flags.on_ground,
                                );
                                mirror_player_entity(
                                    world, physics, player_pid,
                                    player_x, player_y, player_z, player_y_rot, player_x_rot,
                                );
                                update_loaded_chunks(
                                    write, compression, cipher_enc, world,
                                    &*worldgen,
                                    player_x, player_z, view_distance, immediate_radius,
                                    &mut current_chunk_x, &mut current_chunk_z,
                                    &mut loaded_chunks, &mut sent_to_client,
                                    &mut chunk_send_queue,
                                ).await?;
                                let added = spatial_sub.set_view(current_chunk_x, current_chunk_z, view_distance);
                                backfill_region_entities(
                                    write, compression, cipher_enc, world, &added, &mut spawned_items,
                                ).await?;
                                try_item_pickup(
                                    write, compression, cipher_enc, world, physics,
                                    entity_id, player_x, player_y, player_z,
                                ).await?;
                            }
                            ServerboundGamePacket::MovePlayerRot(pkt) => {
                                player_y_rot = pkt.look_direction.y_rot();
                                player_x_rot = pkt.look_direction.x_rot();
                                registry.update_position(
                                    conn_id, player_x, player_y, player_z,
                                    player_y_rot, player_x_rot, pkt.flags.on_ground,
                                );
                                mirror_player_entity(
                                    world, physics, player_pid,
                                    player_x, player_y, player_z, player_y_rot, player_x_rot,
                                );
                            }

                            // ── Chat ────────────────────────────────────
                            ServerboundGamePacket::Chat(chat) => {
                                tracing::info!("<{}> {}", player_name, chat.message);
                                registry.broadcast_chat(conn_id, &player_name, &chat.message);
                            }
                            ServerboundGamePacket::ChatCommand(cmd) => {
                                // Ignore slash-commands for now; just swallow the packet.
                                tracing::debug!("{} sent command: /{}", player_name, cmd.command);
                            }

                            // ── Ignored packets ─────────────────────────
                            ServerboundGamePacket::KeepAlive(_) => {}
                            _ => {}
                        }
                    }
                    Err(e) => {
                        // Structural classification (the old string-match on
                        // the error message broke whenever azalea reworded
                        // it). Recoverable: the frame splitter consumed the
                        // whole frame, so a packet we couldn't make sense of
                        // (modded client, unknown id, trailing data) can be
                        // skipped without desyncing the stream. Everything
                        // else is a transport-level failure — disconnect.
                        use azalea_protocol::read::ReadPacketError as RPE;
                        match &*e {
                            RPE::Parse { .. }
                            | RPE::UnknownPacketId { .. }
                            | RPE::LeftoverData { .. } => {
                                tracing::debug!("Ignoring packet parse error: {e}");
                            }
                            _ => {
                                tracing::info!("{} disconnected: {}", player_name, e);
                                break;
                            }
                        }
                    }
                }
            }

            // ── Spatial bus: world changes + entity moves near this player ──
            // Region-scoped (Phase 6f): we only receive events for regions
            // inside our subscribed view, so a busy far-away area costs us
            // nothing. World batches apply in arrival order; movement
            // bursts coalesce to the newest absolute position per entity
            // (entity-tracker pattern).
            spatial_msg = spatial_rx.recv() => {
                let Some(first) = spatial_msg else {
                    tracing::info!("{}: spatial bus closed", player_name);
                    break;
                };
                let mut burst = vec![first];
                while burst.len() < 8192 {
                    match spatial_rx.try_recv() {
                        Ok(m) => burst.push(m),
                        Err(_) => break,
                    }
                }
                let mut latest_move: std::collections::HashMap<i32, PlayerEvent> =
                    std::collections::HashMap::new();

                for msg in &burst {
                    match &**msg {
                        event_bus::SpatialMsg::World(batch) => {
                            // Light updates before block updates so the
                            // client re-renders with fresh light data.
                            if !batch.light_changes.is_empty() {
                                send_light_updates(write, compression, cipher_enc, world, &batch.light_changes).await?;
                            }
                            for &(pos, new_block) in batch.changes.iter() {
                                let mc_pos = azalea_core::position::BlockPos::new(
                                    pos.x as i32, pos.y as i32, pos.z as i32,
                                );
                                let mc_state = engine_block_to_mc(new_block);
                                let update: ClientboundGamePacket = ClientboundBlockUpdate {
                                    pos: mc_pos,
                                    block_state: mc_state,
                                }.into_variant();
                                write_packet(&update, write, compression, cipher_enc).await?;
                            }
                        }
                        event_bus::SpatialMsg::Move(ev) => {
                            if let PlayerEvent::Moved { entity_id, .. } = ev {
                                latest_move.insert(*entity_id, ev.clone());
                            }
                        }
                        // ── Item entities (Phase 5) ──────────────────────
                        event_bus::SpatialMsg::Entities(changes) => {
                            for c in changes {
                                match c {
                                    event_bus::EntityChange::Spawn { id, state } => {
                                        if is_client_entity(state.kind)
                                            && spawned_items.insert(id.0)
                                        {
                                            send_entity_spawn(write, compression, cipher_enc, *id, state).await?;
                                        }
                                    }
                                    event_bus::EntityChange::Move { id, state } => {
                                        if !spawned_items.contains(&id.0) {
                                            // Entered our view mid-flight (or
                                            // crossed in from another region):
                                            // late-spawn it.
                                            if is_client_entity(state.kind)
                                                && spawned_items.insert(id.0)
                                            {
                                                send_entity_spawn(write, compression, cipher_enc, *id, state).await?;
                                            }
                                            continue;
                                        }
                                        let tp: ClientboundGamePacket = ClientboundTeleportEntity {
                                            id: item_wire_id(*id),
                                            change: PositionMoveRotation {
                                                pos: Vec3 { x: state.pos.x, y: state.pos.y, z: state.pos.z },
                                                // Velocity: clients extrapolate
                                                // between segment endpoints
                                                // (blocks/tick on the wire).
                                                delta: Vec3 {
                                                    x: state.vel.x / 20.0,
                                                    y: state.vel.y / 20.0,
                                                    z: state.vel.z / 20.0,
                                                },
                                                look_direction: LookDirection::new(0.0, 0.0),
                                            },
                                            relative: RelativeMovements::default(),
                                            on_ground: state.vel.y == 0.0,
                                        }.into_variant();
                                        write_packet(&tp, write, compression, cipher_enc).await?;
                                    }
                                    event_bus::EntityChange::Despawn { id, .. } => {
                                        if spawned_items.remove(&id.0) {
                                            let rm: ClientboundGamePacket = ClientboundRemoveEntities {
                                                entity_ids: vec![item_wire_id(*id)],
                                            }.into_variant();
                                            write_packet(&rm, write, compression, cipher_enc).await?;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                for ev in latest_move.into_values() {
                    let PlayerEvent::Moved { conn_id: moved_id, entity_id: eid, x, y, z, y_rot, x_rot, on_ground } = ev else {
                        continue;
                    };
                    if moved_id == conn_id { continue; }
                    // Fine AOI filter on top of region-granular delivery.
                    let aoi = ((config.network.view_distance as f64) + 2.0) * 16.0;
                    if (x - player_x).abs() > aoi || (z - player_z).abs() > aoi {
                        continue;
                    }

                    let tp: ClientboundGamePacket = ClientboundTeleportEntity {
                        id: MinecraftEntityId(eid),
                        change: PositionMoveRotation {
                            pos: Vec3 { x, y, z },
                            delta: Vec3 { x: 0.0, y: 0.0, z: 0.0 },
                            look_direction: LookDirection::new(y_rot, x_rot),
                        },
                        relative: RelativeMovements::default(),
                        on_ground,
                    }.into_variant();
                    write_packet(&tp, write, compression, cipher_enc).await?;

                    let head: ClientboundGamePacket = ClientboundRotateHead {
                        entity_id: MinecraftEntityId(eid),
                        y_head_rot: degrees_to_byte_angle(y_rot),
                    }.into_variant();
                    write_packet(&head, write, compression, cipher_enc).await?;
                }
            }

            // ── Player lifecycle: join/leave/chat (movement is spatial now) ──
            // Bursts are drained and COALESCED: during a join storm every
            // connection receives every join, so per-event packets made the
            // storm O(N²) packet writes server-wide. One drain pass emits one
            // multi-entry tab-list add and one batched remove (same pattern
            // as entity-move coalescing in the spatial arm).
            result = player_rx.recv() => {
                let mut events: Vec<PlayerEvent> = Vec::new();
                match result {
                    Ok(event) => events.push(event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("{} player event bus lagged, skipped {} events", player_name, n);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
                loop {
                    use tokio::sync::broadcast::error::TryRecvError;
                    match player_rx.try_recv() {
                        Ok(event) => {
                            events.push(event);
                            if events.len() >= 8192 { break; }
                        }
                        Err(TryRecvError::Lagged(n)) => {
                            tracing::warn!("{} player event bus lagged, skipped {} events", player_name, n);
                        }
                        Err(_) => break, // Empty (or Closed — next recv handles it)
                    }
                }

                let mut join_entries: Vec<PlayerInfoEntry> = Vec::new();
                let mut spawn_pkts: Vec<ClientboundGamePacket> = Vec::new();
                let mut left_eids: Vec<MinecraftEntityId> = Vec::new();
                let mut left_uuids = Vec::new();
                for event in events {
                    match event {
                        PlayerEvent::Joined { conn_id: joined_id, entity_id: eid, uuid, name, x, y, z, y_rot, x_rot } => {
                            // Skip our own join event.
                            if joined_id == conn_id { continue; }
                            if tab_listed.len() < tab_cap && tab_listed.insert(uuid) {
                                join_entries.push(PlayerInfoEntry {
                                    profile: GameProfile {
                                        uuid,
                                        name,
                                        properties: Default::default(),
                                    },
                                    listed: true,
                                    latency: 0,
                                    game_mode: GameMode::Creative,
                                    display_name: None,
                                    list_order: 0,
                                    update_hat: false,
                                    chat_session: None,
                                });
                            }
                            if spawned_entities.len() < spawn_cap && spawned_entities.insert(eid) {
                                spawn_pkts.push(ClientboundAddEntity {
                                    id: MinecraftEntityId(eid),
                                    uuid,
                                    entity_type: EntityKind::Player,
                                    position: Vec3 { x, y, z },
                                    movement: LpVec3::Zero,
                                    x_rot: degrees_to_byte_angle(x_rot),
                                    y_rot: degrees_to_byte_angle(y_rot),
                                    y_head_rot: degrees_to_byte_angle(y_rot),
                                    data: 0,
                                }.into_variant());
                            }
                        }
                        PlayerEvent::Moved { .. } => {
                            // Movement is delivered through the spatial
                            // bus; nothing should arrive here.
                        }
                        PlayerEvent::Left { conn_id: left_id, entity_id: eid, uuid } => {
                            if left_id == conn_id { continue; }
                            // Only retract what this client was actually sent.
                            if spawned_entities.remove(&eid) {
                                left_eids.push(MinecraftEntityId(eid));
                            }
                            if tab_listed.remove(&uuid) {
                                left_uuids.push(uuid);
                            }
                        }
                        PlayerEvent::Chat { name, message, .. } => {
                            // Send as system chat to all clients (including sender).
                            let text = format!("<{}> {}", name, message);
                            let chat_pkt: ClientboundGamePacket = ClientboundSystemChat {
                                content: FormattedText::from(text),
                                overlay: false,
                            }.into_variant();
                            write_packet(&chat_pkt, write, compression, cipher_enc).await?;
                        }
                    }
                }

                if !join_entries.is_empty() {
                    let info_pkt: ClientboundGamePacket = ClientboundPlayerInfoUpdate {
                        actions: ActionEnumSet {
                            add_player: true,
                            initialize_chat: false,
                            update_game_mode: true,
                            update_listed: true,
                            update_latency: true,
                            update_display_name: false,
                            update_hat: false,
                            update_list_order: false,
                        },
                        entries: join_entries,
                    }.into_variant();
                    write_packet(&info_pkt, write, compression, cipher_enc).await?;
                    for spawn_pkt in &spawn_pkts {
                        write_packet(spawn_pkt, write, compression, cipher_enc).await?;
                    }
                }
                if !left_eids.is_empty() {
                    let remove_pkt: ClientboundGamePacket = ClientboundRemoveEntities {
                        entity_ids: left_eids,
                    }.into_variant();
                    write_packet(&remove_pkt, write, compression, cipher_enc).await?;
                }
                if !left_uuids.is_empty() {
                    let info_remove: ClientboundGamePacket = ClientboundPlayerInfoRemove {
                        profile_ids: left_uuids,
                    }.into_variant();
                    write_packet(&info_remove, write, compression, cipher_enc).await?;
                }
            }
        }
    }

    // Deregister now happens via DeregisterGuard's Drop impl (so it runs
    // on every exit path, including `?` early returns from network errors).
    tracing::info!("{} disconnected cleanly", player_name);
    Ok(())
}
