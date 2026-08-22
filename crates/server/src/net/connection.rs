//! Per-client connection handler implementing the MC 1.21.11 protocol state machine.
//!
//! Handshake -> Status | Login -> Configuration -> Play

use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use azalea_protocol::common::movements::{PositionMoveRotation, RelativeMovements};
use azalea_protocol::packets::ClientIntention;
use azalea_protocol::packets::game::{
    ClientboundGamePacket, ClientboundGameEvent, ClientboundLogin,
    ClientboundPlayerPosition, ClientboundSetChunkCacheCenter,
    ClientboundTeleportEntity, ClientboundRotateHead,
    ServerboundGamePacket,
};
use azalea_protocol::packets::game::c_game_event::EventType;
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

/// The player's connection-local identity and last-known pose. The
/// EntityStore mirror and the registry are updated FROM this via
/// [`apply_player_move`]; this copy exists so packet handling never
/// reads back through a lock.
struct Avatar {
    conn_id: u64,
    entity_id: i32,
    /// EntityStore id (high-bit player namespace).
    pid: ultimate_engine::world::entity::EntityId,
    x: f64,
    y: f64,
    z: f64,
    y_rot: f32,
    x_rot: f32,
}

/// One player-movement update, shared by all three movement packet
/// shapes (pos / pos+rot / rot — previously three copy-pasted arms):
/// update the avatar, the registry render path, and the EntityStore
/// mirror; when the position changed, also roll the chunk view, spatial
/// subscription, entity backfill, and item pickup.
#[allow(clippy::too_many_arguments)]
async fn apply_player_move<W: AsyncWrite + Unpin + Send>(
    write: &mut W,
    compression: Option<u32>,
    cipher_enc: &mut Option<azalea_crypto::Aes128CfbEnc>,
    world: &World,
    worldgen: &dyn WorldGen,
    registry: &PlayerRegistry,
    physics: &crate::physics::PhysicsHandle,
    avatar: &mut Avatar,
    streamer: &mut ChunkStreamer,
    tracker: &mut EntityTracker,
    spatial_sub: &mut event_bus::SpatialSubscriber,
    pos: Option<(f64, f64, f64)>,
    rot: Option<(f32, f32)>,
    on_ground: bool,
) -> Result<()> {
    if let Some((x, y, z)) = pos {
        avatar.x = x;
        avatar.y = y;
        avatar.z = z;
    }
    if let Some((y_rot, x_rot)) = rot {
        avatar.y_rot = y_rot;
        avatar.x_rot = x_rot;
    }
    registry.update_position(
        avatar.conn_id, avatar.x, avatar.y, avatar.z, avatar.y_rot, avatar.x_rot, on_ground,
    );
    mirror_player_entity(
        world, physics, avatar.pid, avatar.x, avatar.y, avatar.z, avatar.y_rot, avatar.x_rot,
    );
    if pos.is_some() {
        streamer
            .on_player_move(write, compression, cipher_enc, world, worldgen, avatar.x, avatar.z)
            .await?;
        let (ccx, ccz) = streamer.center();
        let added = spatial_sub.set_view(ccx, ccz, streamer.view_distance());
        tracker.backfill(write, compression, cipher_enc, world, &added).await?;
        try_item_pickup(
            write, compression, cipher_enc, world, physics,
            avatar.entity_id, avatar.x, avatar.y, avatar.z,
        )
        .await?;
    }
    Ok(())
}


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

    // Chunk streaming (view set, deferred queue, bulk-streaming
    // admission) lives in the ChunkStreamer. MC 1.20+ requires chunks to
    // be wrapped in ChunkBatchStart/Finished markers — without these,
    // the client receives the data but won't render the chunks.
    let view_distance = config.network.view_distance;
    init_stream_permits(config.network.stream_permits);
    let mut streamer = ChunkStreamer::new(
        &config.network,
        STREAM_PERMITS.get().expect("init_stream_permits ran").clone(),
        (chunk_x, chunk_z),
    );
    streamer.send_initial(write, compression, cipher_enc, world, &*worldgen).await?;

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
    // border crossings. The EntityTracker owns what this client has been
    // sent, and backfills entities already at rest in view — they emit
    // no events, so a newcomer would otherwise never see them.
    let mut tracker = EntityTracker::new();
    let (mut spatial_sub, mut spatial_rx) = spatial.subscribe();
    let initial_regions = spatial_sub.set_view(chunk_x, chunk_z, view_distance);
    tracker.backfill(write, compression, cipher_enc, world, &initial_regions).await?;
    // Subscribe to player lifecycle events (join/leave/chat — global).
    let mut player_rx = registry.subscribe();

    // ── Multiplayer: send existing players to newcomer, then register ───
    let mut presence = super::presence::Presence::new(&config.network);
    {
        let existing_players = registry.snapshot();
        presence
            .send_initial(write, compression, cipher_enc, &existing_players, player_uuid, player_name)
            .await?;
        // Scope-drop: the snapshot (up to one PlayerInfo per online
        // player) must not live in this stack frame for the connection's
        // whole lifetime — ~0.5 MB × 10k connections was gigabytes in
        // the 10k load test.
    }

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

    // The player's avatar: ids + last-known position/rotation (the
    // connection-local copy; the EntityStore mirror and registry are
    // updated through `apply_player_move`).
    let mut avatar = Avatar {
        conn_id,
        entity_id,
        pid: player_pid,
        x: spawn_x,
        y: spawn_y,
        z: spawn_z,
        y_rot: 0.0,
        x_rot: 0.0,
    };
    // Creative inventory model lives in the gameplay layer.
    let mut inventory = crate::gameplay::Inventory::default();

    // ── Main loop: keep-alive + handle incoming packets + bus ────────────
    let mut keepalive_timer = tokio::time::interval(Duration::from_secs(15));
    let mut keepalive_id: u64 = 0;
    // Diagnostics: a keep-alive gap above 25s means this client was one
    // missed packet from a vanilla 30s timeout — log who and how long.
    let mut last_keepalive_sent: Option<std::time::Instant> = None;

    loop {
        // Eagerly pump the chunk queue before waiting for events (batch
        // send + self-heal + permit handoff — see ChunkStreamer::pump).
        streamer.pump(write, compression, cipher_enc, world, &*worldgen).await?;

        tokio::select! {
            // When chunks are queued and we hold the streaming permit, yield
            // immediately so we cycle back to the pump at the top of the
            // loop. This keeps chunk loading progressing rapidly without
            // starving event processing.
            _ = std::future::ready(()), if streamer.streaming_ready() => {}
            // Chunks queued but no permit yet: wait for admission. Other
            // arms (keep-alive, reads, lifecycle) stay live while we wait.
            permit = streamer.semaphore().acquire_owned(), if streamer.awaiting_admission() => {
                if let Ok(permit) = permit {
                    streamer.admit(permit, player_name);
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
                                    avatar.y_rot,
                                    avatar.x_rot,
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

                            // ── Player movement (one path for all three
                            // packet shapes; previously triplicated) ────
                            ServerboundGamePacket::MovePlayerPos(pkt) => {
                                apply_player_move(
                                    write, compression, cipher_enc, world, &*worldgen,
                                    registry, physics,
                                    &mut avatar, &mut streamer, &mut tracker, &mut spatial_sub,
                                    Some((pkt.pos.x, pkt.pos.y, pkt.pos.z)),
                                    None,
                                    pkt.flags.on_ground,
                                ).await?;
                            }
                            ServerboundGamePacket::MovePlayerPosRot(pkt) => {
                                apply_player_move(
                                    write, compression, cipher_enc, world, &*worldgen,
                                    registry, physics,
                                    &mut avatar, &mut streamer, &mut tracker, &mut spatial_sub,
                                    Some((pkt.pos.x, pkt.pos.y, pkt.pos.z)),
                                    Some((pkt.look_direction.y_rot(), pkt.look_direction.x_rot())),
                                    pkt.flags.on_ground,
                                ).await?;
                            }
                            ServerboundGamePacket::MovePlayerRot(pkt) => {
                                apply_player_move(
                                    write, compression, cipher_enc, world, &*worldgen,
                                    registry, physics,
                                    &mut avatar, &mut streamer, &mut tracker, &mut spatial_sub,
                                    None,
                                    Some((pkt.look_direction.y_rot(), pkt.look_direction.x_rot())),
                                    pkt.flags.on_ground,
                                ).await?;
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
                        // ── Spatial entities (Phase 5) ───────────────
                        event_bus::SpatialMsg::Entities(changes) => {
                            tracker.apply_changes(write, compression, cipher_enc, changes).await?;
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
                    if (x - avatar.x).abs() > aoi || (z - avatar.z).abs() > aoi {
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

                presence
                    .apply_events(write, compression, cipher_enc, conn_id, events)
                    .await?;
            }
        }
    }

    // Deregister now happens via DeregisterGuard's Drop impl (so it runs
    // on every exit path, including `?` early returns from network errors).
    tracing::info!("{} disconnected cleanly", player_name);
    Ok(())
}
