//! Per-client connection handler implementing the MC 1.21.11 protocol state machine.
//!
//! Handshake -> Status | Login -> Configuration -> Play

use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use azalea_auth::game_profile::GameProfile;
use azalea_buf::AzaleaWrite;
use azalea_chat::FormattedText;
use azalea_core::bitset::BitSet;
use azalea_protocol::common::movements::{PositionMoveRotation, RelativeMovements};
use azalea_protocol::packets::ClientIntention;
use azalea_protocol::packets::config::{
    ClientboundConfigPacket, ClientboundFinishConfiguration, ClientboundRegistryData,
    ClientboundSelectKnownPacks, ClientboundUpdateTags, ServerboundConfigPacket,
};
use azalea_protocol::common::tags::{TagMap, Tags};
use azalea_protocol::packets::game::{
    ClientboundGamePacket, ClientboundGameEvent, ClientboundLogin,
    ClientboundPlayerPosition, ClientboundSetChunkCacheCenter,
    ServerboundGamePacket,
};
use azalea_protocol::packets::game::c_game_event::EventType;
use azalea_protocol::packets::handshake::ServerboundHandshakePacket;
use azalea_protocol::packets::login::{
    ClientboundLoginFinished, ClientboundLoginPacket, ServerboundLoginPacket,
};
use azalea_protocol::packets::status::{
    ClientboundPongResponse, ClientboundStatusPacket, ClientboundStatusResponse,
    ServerboundStatusPacket,
};
use azalea_protocol::packets::status::c_status_response::{Version, Players};
use azalea_protocol::packets::Packet;
use azalea_protocol::packets::common::CommonPlayerSpawnInfo;
use azalea_protocol::packets::config::s_select_known_packs::KnownPack;
use azalea_protocol::read::read_packet;
use azalea_protocol::write::write_packet;
use azalea_core::game_type::{GameMode, OptionalGameType};
use azalea_core::position::Vec3;
use azalea_entity::LookDirection;
use azalea_registry::DataRegistry;
use azalea_registry::data::DimensionKind;
use azalea_registry::identifier::Identifier;
use azalea_world::MinecraftEntityId;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use ultimate_engine::world::World;
use uuid::Uuid;

use crate::dashboard::{self, DashboardState};

/// Handle a single client connection through all protocol phases.
pub async fn handle(stream: TcpStream, world: Arc<World>, dashboard: Arc<DashboardState>) -> Result<()> {
    let (read, write) = stream.into_split();
    let mut read = read;
    let mut write = write;
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
            handle_status(&mut read, &mut write, &mut buf, compression, &mut cipher_enc, &mut cipher_dec).await?;
        }
        ClientIntention::Login => {
            let name = handle_login(&mut read, &mut write, &mut buf, compression, &mut cipher_enc, &mut cipher_dec).await?;
            handle_configuration(&mut read, &mut write, &mut buf, compression, &mut cipher_enc, &mut cipher_dec).await?;
            dashboard.metrics.player_joined();
            let result = handle_play(&mut read, &mut write, &mut buf, compression, &mut cipher_enc, &mut cipher_dec, &world, &name, &dashboard).await;
            dashboard.metrics.player_left();
            result?;
        }
        _ => {
            tracing::warn!("Unsupported intention: {:?}", intention.intention);
        }
    }

    Ok(())
}

// ── Status ──────────────────────────────────────────────────────────────

async fn handle_status<R, W>(
    read: &mut R, write: &mut W, buf: &mut Cursor<Vec<u8>>,
    compression: Option<u32>,
    cipher_enc: &mut Option<azalea_crypto::Aes128CfbEnc>,
    cipher_dec: &mut Option<azalea_crypto::Aes128CfbDec>,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + Sync,
    W: AsyncWrite + Unpin + Send,
{
    // Client sends status request
    let packet = read_packet::<ServerboundStatusPacket, _>(read, buf, compression, cipher_dec).await?;
    tracing::debug!("Status request: {:?}", packet);

    // Respond with server status
    let response: ClientboundStatusPacket = ClientboundStatusResponse {
        description: FormattedText::from("Ultimate Minecraft - Causal Graph Engine"),
        favicon: None,
        players: Players {
            max: 20,
            online: 0,
            sample: vec![],
        },
        version: Version {
            name: azalea_protocol::packets::VERSION_NAME.to_string(),
            protocol: azalea_protocol::packets::PROTOCOL_VERSION,
        },
        enforces_secure_chat: Some(false),
    }.into_variant();
    write_packet(&response, write, compression, cipher_enc).await?;

    // Client may send ping
    let packet = read_packet::<ServerboundStatusPacket, _>(read, buf, compression, cipher_dec).await?;
    if let ServerboundStatusPacket::PingRequest(ping) = packet {
        let pong: ClientboundStatusPacket = ClientboundPongResponse {
            time: ping.time,
        }.into_variant();
        write_packet(&pong, write, compression, cipher_enc).await?;
    }

    Ok(())
}

// ── Login ───────────────────────────────────────────────────────────────

async fn handle_login<R, W>(
    read: &mut R, write: &mut W, buf: &mut Cursor<Vec<u8>>,
    compression: Option<u32>,
    cipher_enc: &mut Option<azalea_crypto::Aes128CfbEnc>,
    cipher_dec: &mut Option<azalea_crypto::Aes128CfbDec>,
) -> Result<String>
where
    R: AsyncRead + Unpin + Send + Sync,
    W: AsyncWrite + Unpin + Send,
{
    // Client sends Login Start
    let packet = read_packet::<ServerboundLoginPacket, _>(read, buf, compression, cipher_dec).await?;

    let (name, _client_uuid) = match packet {
        ServerboundLoginPacket::Hello(hello) => {
            tracing::info!("Login: {} (uuid: {})", hello.name, hello.profile_id);
            (hello.name, hello.profile_id)
        }
        other => return Err(anyhow!("Expected Login Start, got: {:?}", other)),
    };

    // Offline mode: skip encryption, generate UUID from name
    let uuid = offline_uuid(&name);

    // Send Login Success
    let response: ClientboundLoginPacket = ClientboundLoginFinished {
        game_profile: GameProfile {
            uuid,
            name: name.clone(),
            properties: Default::default(),
        },
    }.into_variant();
    write_packet(&response, write, compression, cipher_enc).await?;

    // Wait for Login Acknowledged
    let ack = read_packet::<ServerboundLoginPacket, _>(read, buf, compression, cipher_dec).await?;
    tracing::debug!("Login ack: {:?}", ack);

    Ok(name)
}

// ── Configuration ───────────────────────────────────────────────────────

async fn handle_configuration<R, W>(
    read: &mut R, write: &mut W, buf: &mut Cursor<Vec<u8>>,
    compression: Option<u32>,
    cipher_enc: &mut Option<azalea_crypto::Aes128CfbEnc>,
    cipher_dec: &mut Option<azalea_crypto::Aes128CfbDec>,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + Sync,
    W: AsyncWrite + Unpin + Send,
{
    // Send Known Packs -- tell client we share the vanilla data pack
    let known_packs: ClientboundConfigPacket = ClientboundSelectKnownPacks {
        known_packs: vec![KnownPack {
            namespace: "minecraft".into(),
            id: "core".into(),
            version: azalea_protocol::packets::VERSION_NAME.into(),
        }],
    }.into_variant();
    write_packet(&known_packs, write, compression, cipher_enc).await?;

    // Client may send ClientInformation, CustomPayload (brand), etc. before
    // responding to our KnownPacks. Drain until we get SelectKnownPacks.
    loop {
        let packet = read_packet::<ServerboundConfigPacket, _>(read, buf, compression, cipher_dec).await?;
        match &packet {
            ServerboundConfigPacket::SelectKnownPacks(_) => {
                tracing::debug!("Client known packs: {:?}", packet);
                break;
            }
            other => {
                tracing::debug!("Config packet (pre-registry): {:?}", other);
            }
        }
    }

    // Send registry data -- with Known Packs, entries have None NBT (client uses local data)
    send_registries(write, compression, cipher_enc).await?;

    // Send tags -- timeline registry requires in_overworld/in_nether/in_end tags
    send_tags(write, compression, cipher_enc).await?;

    // Signal end of configuration
    let finish: ClientboundConfigPacket = ClientboundFinishConfiguration {}.into_variant();
    write_packet(&finish, write, compression, cipher_enc).await?;

    // Client may send more packets before acknowledging finish. Drain them.
    loop {
        let packet = read_packet::<ServerboundConfigPacket, _>(read, buf, compression, cipher_dec).await?;
        match &packet {
            ServerboundConfigPacket::FinishConfiguration(_) => {
                tracing::debug!("Client finished configuration");
                break;
            }
            other => {
                tracing::debug!("Config packet (post-registry): {:?}", other);
            }
        }
    }

    Ok(())
}

/// Send all required registry data packets.
async fn send_registries<W: AsyncWrite + Unpin + Send>(
    write: &mut W,
    compression: Option<u32>,
    cipher: &mut Option<azalea_crypto::Aes128CfbEnc>,
) -> Result<()> {
    // Each registry: (registry_id, list of entry identifiers)
    // With Known Packs, we send None for NBT data -- client fills from local files.
    let registries = registry_entries();

    for (registry_id, entries) in registries {
        let packet: ClientboundConfigPacket = ClientboundRegistryData {
            registry_id: Identifier::new(&registry_id),
            entries: entries
                .into_iter()
                .map(|name| (Identifier::new(&name), None))
                .collect(),
        }.into_variant();
        write_packet(&packet, write, compression, cipher).await?;
    }

    Ok(())
}

/// Send UpdateTags packet. The timeline registry needs tags to bind its entries.
async fn send_tags<W: AsyncWrite + Unpin + Send>(
    write: &mut W,
    compression: Option<u32>,
    cipher: &mut Option<azalea_crypto::Aes128CfbEnc>,
) -> Result<()> {
    use indexmap::IndexMap;

    // Timeline entries: day=0, early_game=1, moon=2, villager_schedule=3
    // Tags needed: in_overworld, in_nether, in_end (group entries by dimension)
    let mut tag_map = IndexMap::new();
    tag_map.insert(
        Identifier::new("minecraft:timeline"),
        vec![
            Tags {
                name: Identifier::new("minecraft:in_overworld"),
                elements: vec![0, 1, 2, 3], // all timeline entries apply in overworld
            },
            Tags {
                name: Identifier::new("minecraft:in_nether"),
                elements: vec![0, 2], // day and moon (basic time cycles)
            },
            Tags {
                name: Identifier::new("minecraft:in_end"),
                elements: vec![0, 2], // day and moon
            },
        ],
    );

    let tags_packet: ClientboundConfigPacket = ClientboundUpdateTags {
        tags: TagMap(tag_map),
    }.into_variant();
    write_packet(&tags_packet, write, compression, cipher).await?;

    Ok(())
}

/// Returns (registry_id, vec_of_entry_identifiers) for all required registries.
///
/// Entry ordering defines numeric IDs starting from 0. The order must match
/// the vanilla server's ordering for the Known Packs optimization to work.
fn registry_entries() -> Vec<(String, Vec<String>)> {
    vec![
        ("minecraft:dimension_type".into(), vec![
            "minecraft:overworld".into(),
            "minecraft:overworld_caves".into(),
            "minecraft:the_nether".into(),
            "minecraft:the_end".into(),
        ]),
        ("minecraft:worldgen/biome".into(), vec![
            "minecraft:badlands".into(),
            "minecraft:bamboo_jungle".into(),
            "minecraft:basalt_deltas".into(),
            "minecraft:beach".into(),
            "minecraft:birch_forest".into(),
            "minecraft:cherry_grove".into(),
            "minecraft:cold_ocean".into(),
            "minecraft:crimson_forest".into(),
            "minecraft:dark_forest".into(),
            "minecraft:deep_cold_ocean".into(),
            "minecraft:deep_dark".into(),
            "minecraft:deep_frozen_ocean".into(),
            "minecraft:deep_lukewarm_ocean".into(),
            "minecraft:deep_ocean".into(),
            "minecraft:desert".into(),
            "minecraft:dripstone_caves".into(),
            "minecraft:end_barrens".into(),
            "minecraft:end_highlands".into(),
            "minecraft:end_midlands".into(),
            "minecraft:eroded_badlands".into(),
            "minecraft:flower_forest".into(),
            "minecraft:forest".into(),
            "minecraft:frozen_ocean".into(),
            "minecraft:frozen_peaks".into(),
            "minecraft:frozen_river".into(),
            "minecraft:grove".into(),
            "minecraft:ice_spikes".into(),
            "minecraft:jagged_peaks".into(),
            "minecraft:jungle".into(),
            "minecraft:lukewarm_ocean".into(),
            "minecraft:lush_caves".into(),
            "minecraft:mangrove_swamp".into(),
            "minecraft:meadow".into(),
            "minecraft:mushroom_fields".into(),
            "minecraft:nether_wastes".into(),
            "minecraft:ocean".into(),
            "minecraft:old_growth_birch_forest".into(),
            "minecraft:old_growth_pine_taiga".into(),
            "minecraft:old_growth_spruce_taiga".into(),
            "minecraft:pale_garden".into(),
            "minecraft:plains".into(),
            "minecraft:river".into(),
            "minecraft:savanna".into(),
            "minecraft:savanna_plateau".into(),
            "minecraft:small_end_islands".into(),
            "minecraft:snowy_beach".into(),
            "minecraft:snowy_plains".into(),
            "minecraft:snowy_slopes".into(),
            "minecraft:snowy_taiga".into(),
            "minecraft:soul_sand_valley".into(),
            "minecraft:sparse_jungle".into(),
            "minecraft:stony_peaks".into(),
            "minecraft:stony_shore".into(),
            "minecraft:sunflower_plains".into(),
            "minecraft:swamp".into(),
            "minecraft:taiga".into(),
            "minecraft:the_end".into(),
            "minecraft:the_void".into(),
            "minecraft:warm_ocean".into(),
            "minecraft:warped_forest".into(),
            "minecraft:windswept_forest".into(),
            "minecraft:windswept_gravelly_hills".into(),
            "minecraft:windswept_hills".into(),
            "minecraft:windswept_savanna".into(),
            "minecraft:wooded_badlands".into(),
        ]),
        // All entries below sourced from azalea-registry 0.15.1+mc1.21.11 data.rs
        ("minecraft:damage_type".into(), vec![
            "minecraft:arrow".into(), "minecraft:bad_respawn_point".into(),
            "minecraft:cactus".into(), "minecraft:campfire".into(),
            "minecraft:cramming".into(), "minecraft:dragon_breath".into(),
            "minecraft:drown".into(), "minecraft:dry_out".into(),
            "minecraft:ender_pearl".into(), "minecraft:explosion".into(),
            "minecraft:fall".into(), "minecraft:falling_anvil".into(),
            "minecraft:falling_block".into(), "minecraft:falling_stalactite".into(),
            "minecraft:fireball".into(), "minecraft:fireworks".into(),
            "minecraft:fly_into_wall".into(), "minecraft:freeze".into(),
            "minecraft:generic".into(), "minecraft:generic_kill".into(),
            "minecraft:hot_floor".into(), "minecraft:in_fire".into(),
            "minecraft:in_wall".into(), "minecraft:indirect_magic".into(),
            "minecraft:lava".into(), "minecraft:lightning_bolt".into(),
            "minecraft:mace_smash".into(), "minecraft:magic".into(),
            "minecraft:mob_attack".into(), "minecraft:mob_attack_no_aggro".into(),
            "minecraft:mob_projectile".into(), "minecraft:on_fire".into(),
            "minecraft:out_of_world".into(), "minecraft:outside_border".into(),
            "minecraft:player_attack".into(), "minecraft:player_explosion".into(),
            "minecraft:sonic_boom".into(), "minecraft:spear".into(),
            "minecraft:spit".into(), "minecraft:stalagmite".into(),
            "minecraft:starve".into(), "minecraft:sting".into(),
            "minecraft:sweet_berry_bush".into(), "minecraft:thorns".into(),
            "minecraft:thrown".into(), "minecraft:trident".into(),
            "minecraft:unattributed_fireball".into(), "minecraft:wind_charge".into(),
            "minecraft:wither".into(), "minecraft:wither_skull".into(),
        ]),
        ("minecraft:painting_variant".into(), vec![
            "minecraft:alban".into(), "minecraft:aztec".into(), "minecraft:aztec2".into(),
            "minecraft:backyard".into(), "minecraft:baroque".into(), "minecraft:bomb".into(),
            "minecraft:bouquet".into(), "minecraft:burning_skull".into(), "minecraft:bust".into(),
            "minecraft:cavebird".into(), "minecraft:changing".into(), "minecraft:cotan".into(),
            "minecraft:courbet".into(), "minecraft:creebet".into(), "minecraft:dennis".into(),
            "minecraft:donkey_kong".into(), "minecraft:earth".into(), "minecraft:endboss".into(),
            "minecraft:fern".into(), "minecraft:fighters".into(), "minecraft:finding".into(),
            "minecraft:fire".into(), "minecraft:graham".into(), "minecraft:humble".into(),
            "minecraft:kebab".into(), "minecraft:lowmist".into(), "minecraft:match".into(),
            "minecraft:meditative".into(), "minecraft:orb".into(), "minecraft:owlemons".into(),
            "minecraft:passage".into(), "minecraft:pigscene".into(), "minecraft:plant".into(),
            "minecraft:pointer".into(), "minecraft:pond".into(), "minecraft:pool".into(),
            "minecraft:prairie_ride".into(), "minecraft:sea".into(), "minecraft:skeleton".into(),
            "minecraft:skull_and_roses".into(), "minecraft:stage".into(),
            "minecraft:sunflowers".into(), "minecraft:sunset".into(), "minecraft:tides".into(),
            "minecraft:unpacked".into(), "minecraft:void".into(), "minecraft:wanderer".into(),
            "minecraft:wasteland".into(), "minecraft:water".into(), "minecraft:wind".into(),
            "minecraft:wither".into(),
        ]),
        ("minecraft:wolf_variant".into(), vec![
            "minecraft:ashen".into(), "minecraft:black".into(), "minecraft:chestnut".into(),
            "minecraft:pale".into(), "minecraft:rusty".into(), "minecraft:snowy".into(),
            "minecraft:spotted".into(), "minecraft:striped".into(), "minecraft:woods".into(),
        ]),
        ("minecraft:cat_variant".into(), vec![
            "minecraft:all_black".into(), "minecraft:black".into(),
            "minecraft:british_shorthair".into(), "minecraft:calico".into(),
            "minecraft:jellie".into(), "minecraft:persian".into(), "minecraft:ragdoll".into(),
            "minecraft:red".into(), "minecraft:siamese".into(), "minecraft:tabby".into(),
            "minecraft:white".into(),
        ]),
        ("minecraft:chicken_variant".into(), vec![
            "minecraft:cold".into(), "minecraft:temperate".into(), "minecraft:warm".into(),
        ]),
        ("minecraft:cow_variant".into(), vec![
            "minecraft:cold".into(), "minecraft:temperate".into(), "minecraft:warm".into(),
        ]),
        ("minecraft:frog_variant".into(), vec![
            "minecraft:cold".into(), "minecraft:temperate".into(), "minecraft:warm".into(),
        ]),
        ("minecraft:pig_variant".into(), vec![
            "minecraft:cold".into(), "minecraft:temperate".into(), "minecraft:warm".into(),
        ]),
        ("minecraft:wolf_sound_variant".into(), vec![
            "minecraft:angry".into(), "minecraft:big".into(), "minecraft:classic".into(),
            "minecraft:cute".into(), "minecraft:grumpy".into(), "minecraft:puglin".into(),
            "minecraft:sad".into(),
        ]),
        ("minecraft:zombie_nautilus_variant".into(), vec![
            "minecraft:temperate".into(), "minecraft:warm".into(),
        ]),
        ("minecraft:timeline".into(), vec![
            "minecraft:day".into(), "minecraft:early_game".into(),
            "minecraft:moon".into(), "minecraft:villager_schedule".into(),
        ]),
    ]
}

// ── Play ────────────────────────────────────────────────────────────────

async fn handle_play<R, W>(
    read: &mut R, write: &mut W, buf: &mut Cursor<Vec<u8>>,
    compression: Option<u32>,
    cipher_enc: &mut Option<azalea_crypto::Aes128CfbEnc>,
    cipher_dec: &mut Option<azalea_crypto::Aes128CfbDec>,
    world: &World,
    player_name: &str,
    dashboard: &DashboardState,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + Sync,
    W: AsyncWrite + Unpin + Send,
{
    let entity_id = 1;
    let spawn_x = 8.0_f64;
    let spawn_y = 80.0_f64; // above the dirt layer (section 8 = y 64-79)
    let spawn_z = 8.0_f64;

    // Send Login (Play) -- this initializes the client's world state
    let login: ClientboundGamePacket = ClientboundLogin {
        player_id: MinecraftEntityId(entity_id),
        hardcore: false,
        levels: vec![Identifier::new("minecraft:overworld")],
        max_players: 20,
        chunk_radius: 4,
        simulation_distance: 4,
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

    // Send chunk data for a small area around the player
    let view_distance = 4i32;
    for cx in (chunk_x - view_distance)..=(chunk_x + view_distance) {
        for cz in (chunk_z - view_distance)..=(chunk_z + view_distance) {
            send_chunk_from_world(write, compression, cipher_enc, world, cx, cz).await?;
        }
    }

    tracing::info!("{} joined the game at ({}, {}, {})", player_name, spawn_x, spawn_y, spawn_z);

    // ── Causal engine for this connection ────────────────────────────────
    use azalea_block::{blocks as mc_blocks, BlockState, BlockTrait};
    use azalea_core::direction::Direction;
    use azalea_protocol::packets::game::{
        ClientboundBlockUpdate, ClientboundBlockChangedAck,
        s_player_action::Action,
    };
    use ultimate_engine::causal::event::{Event, EventPayload};
    use ultimate_engine::causal::graph::CausalGraph;
    use ultimate_engine::causal::scheduler::Scheduler;
    use ultimate_engine::world::block::BlockId;

    let rules = crate::rules::standard();
    let scheduler = Scheduler::new();

    // Track hotbar contents and selected slot for creative placement.
    use azalea_inventory::ItemStack;
    use azalea_registry::builtin::{BlockKind, ItemKind};
    let mut hotbar: [BlockState; 9] = [BlockState::AIR; 9];
    let mut selected_slot: usize = 0;

    // ── Main loop: keep-alive + handle incoming packets ─────────────────
    let mut keepalive_timer = tokio::time::interval(Duration::from_secs(15));
    let mut keepalive_id: u64 = 0;

    loop {
        tokio::select! {
            _ = keepalive_timer.tick() => {
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

                                    // Fresh causal graph per action -- the world state is the
                                    // persistent data; the graph is scratch space for the cascade.
                                    let mut graph = CausalGraph::new();
                                    let old = world.get_block(epos);
                                    let root = graph.insert_root(Event {
                                        payload: EventPayload::BlockSet {
                                            pos: epos,
                                            old,
                                            new: BlockId::AIR,
                                        },
                                    });
                                    // Notify all 6 neighbors (causal children of the break)
                                    for neighbor in epos.neighbors() {
                                        graph.insert(Event {
                                            payload: EventPayload::BlockNotify { pos: neighbor },
                                        }, vec![root]);
                                    }

                                    // Run causal engine -- gravity, fluid spread cascade
                                    let cascade_start = std::time::Instant::now();
                                    let cascade_events = scheduler.run_until_quiet(world, &mut graph, &rules, 1000);
                                    let cascade_dur = cascade_start.elapsed();

                                    // Record metrics + publish graph snapshot (non-blocking).
                                    dashboard.metrics.record_cascade(
                                        graph.len() as u64,
                                        cascade_dur,
                                    );
                                    dashboard.publish_graph(dashboard::snapshot_graph(&graph));

                                    // Broadcast all BlockSet events from this cascade to the client.
                                    for id in &graph.all_ids() {
                                        if let Some(node) = graph.get(*id) {
                                            if node.executed {
                                                if let EventPayload::BlockSet { pos: ep, new, .. } = &node.event.payload {
                                                    let mc_pos = azalea_core::position::BlockPos::new(
                                                        ep.x as i32, ep.y as i32, ep.z as i32,
                                                    );
                                                    let mc_state = engine_block_to_mc(*new);
                                                    let update: ClientboundGamePacket = ClientboundBlockUpdate {
                                                        pos: mc_pos,
                                                        block_state: mc_state,
                                                    }.into_variant();
                                                    write_packet(&update, write, compression, cipher_enc).await?;
                                                }
                                            }
                                        }
                                    }

                                    // Acknowledge the sequence
                                    let ack: ClientboundGamePacket = ClientboundBlockChangedAck {
                                        seq: action.seq,
                                    }.into_variant();
                                    write_packet(&ack, write, compression, cipher_enc).await?;

                                    if cascade_events > 0 {
                                        tracing::info!(
                                            "Block break at ({},{},{}) -> {} causal events in {:?}",
                                            pos.x, pos.y, pos.z, cascade_events, cascade_dur
                                        );
                                    }
                                }
                            }

                            // ── Block placing ───────────────────────────
                            ServerboundGamePacket::UseItemOn(place) => {
                                let hit = &place.block_hit;
                                // Calculate target position (adjacent to clicked face)
                                let target = match hit.direction {
                                    Direction::Down  => azalea_core::position::BlockPos::new(hit.block_pos.x, hit.block_pos.y - 1, hit.block_pos.z),
                                    Direction::Up    => azalea_core::position::BlockPos::new(hit.block_pos.x, hit.block_pos.y + 1, hit.block_pos.z),
                                    Direction::North => azalea_core::position::BlockPos::new(hit.block_pos.x, hit.block_pos.y, hit.block_pos.z - 1),
                                    Direction::South => azalea_core::position::BlockPos::new(hit.block_pos.x, hit.block_pos.y, hit.block_pos.z + 1),
                                    Direction::West  => azalea_core::position::BlockPos::new(hit.block_pos.x - 1, hit.block_pos.y, hit.block_pos.z),
                                    Direction::East  => azalea_core::position::BlockPos::new(hit.block_pos.x + 1, hit.block_pos.y, hit.block_pos.z),
                                };

                                let epos = ultimate_engine::world::position::BlockPos::new(
                                    target.x as i64, target.y as i64, target.z as i64,
                                );

                                // Place the held block via the causal engine so that
                                // gravity, fluid spread, etc. trigger on placement.
                                let held = hotbar[selected_slot];
                                if held == BlockState::AIR { continue; } // nothing to place
                                let old = world.get_block(epos);
                                let new_id = BlockId::new(u32::from(held) as u16);

                                // Fresh causal graph per action.
                                let mut graph = CausalGraph::new();
                                let root = graph.insert_root(Event {
                                    payload: EventPayload::BlockSet {
                                        pos: epos,
                                        old,
                                        new: new_id,
                                    },
                                });
                                // Notify all 6 neighbors (gravity, fluid rules react).
                                for neighbor in epos.neighbors() {
                                    graph.insert(Event {
                                        payload: EventPayload::BlockNotify { pos: neighbor },
                                    }, vec![root]);
                                }

                                // Run causal engine to quiescence.
                                let cascade_start = std::time::Instant::now();
                                let cascade_events = scheduler.run_until_quiet(world, &mut graph, &rules, 1000);
                                let cascade_dur = cascade_start.elapsed();

                                // Record metrics + publish graph snapshot.
                                dashboard.metrics.record_cascade(
                                    graph.len() as u64,
                                    cascade_dur,
                                );
                                dashboard.publish_graph(dashboard::snapshot_graph(&graph));

                                // Broadcast all BlockSet events from this cascade to the client.
                                for id in &graph.all_ids() {
                                    if let Some(node) = graph.get(*id) {
                                        if node.executed {
                                            if let EventPayload::BlockSet { pos: ep, new, .. } = &node.event.payload {
                                                let mc_pos = azalea_core::position::BlockPos::new(
                                                    ep.x as i32, ep.y as i32, ep.z as i32,
                                                );
                                                let mc_state = engine_block_to_mc(*new);
                                                let update: ClientboundGamePacket = ClientboundBlockUpdate {
                                                    pos: mc_pos,
                                                    block_state: mc_state,
                                                }.into_variant();
                                                write_packet(&update, write, compression, cipher_enc).await?;
                                            }
                                        }
                                    }
                                }

                                // Acknowledge
                                let ack: ClientboundGamePacket = ClientboundBlockChangedAck {
                                    seq: place.seq,
                                }.into_variant();
                                write_packet(&ack, write, compression, cipher_enc).await?;

                                if cascade_events > 0 {
                                    tracing::info!(
                                        "Block place at ({},{},{}) -> {} causal events in {:?}",
                                        target.x, target.y, target.z, cascade_events, cascade_dur
                                    );
                                }
                            }

                            // ── Creative inventory slot update ───────────
                            ServerboundGamePacket::SetCreativeModeSlot(slot) => {
                                // Hotbar slots are 36-44 in the inventory window.
                                let hotbar_idx = slot.slot_num as i32 - 36;
                                if hotbar_idx >= 0 && hotbar_idx < 9 {
                                    let bs = match &slot.item_stack {
                                        ItemStack::Present(data) => {
                                            item_to_block_kind(data.kind)
                                                .map(BlockState::from)
                                                .unwrap_or(BlockState::AIR)
                                        }
                                        ItemStack::Empty => BlockState::AIR,
                                    };
                                    hotbar[hotbar_idx as usize] = bs;
                                }
                            }

                            // ── Hotbar slot selection ────────────────────
                            ServerboundGamePacket::SetCarriedItem(carried) => {
                                selected_slot = (carried.slot as usize).min(8);
                            }

                            // ── Ignored packets ─────────────────────────
                            ServerboundGamePacket::KeepAlive(_) => {}
                            ServerboundGamePacket::MovePlayerPos(_) => {}
                            ServerboundGamePacket::MovePlayerPosRot(_) => {}
                            ServerboundGamePacket::MovePlayerRot(_) => {}
                            _ => {}
                        }
                    }
                    Err(e) => {
                        let msg = format!("{}", e);
                        if msg.contains("Leftover data") || msg.contains("unknown variant") {
                            // Non-fatal parse error (modded client, unknown packet variant).
                            // Log and continue rather than disconnecting.
                            tracing::debug!("Ignoring packet parse error: {}", msg);
                        } else {
                            tracing::info!("{} disconnected: {}", player_name, e);
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
}

/// Try to convert an ItemKind to its corresponding BlockKind.
/// Uses string name matching: ItemKind::OakPlanks displays as "minecraft:oak_planks",
/// and BlockKind::from_str("oak_planks") parses it back.
/// Special-cases items whose name doesn't match a block (e.g. water_bucket → water).
fn item_to_block_kind(item: azalea_registry::builtin::ItemKind) -> Option<azalea_registry::builtin::BlockKind> {
    use azalea_registry::builtin::{BlockKind, ItemKind};

    // Items whose name doesn't map to a block name directly.
    match item {
        ItemKind::WaterBucket => return Some(BlockKind::Water),
        ItemKind::LavaBucket => return Some(BlockKind::Lava),
        _ => {}
    }

    // Display gives "minecraft:oak_planks", strip prefix for FromStr which expects "oak_planks"
    let full = format!("{}", item);
    let name = full.strip_prefix("minecraft:").unwrap_or(&full);
    name.parse::<BlockKind>().ok()
}

/// Map engine BlockId to MC BlockState for protocol.
fn engine_block_to_mc(id: ultimate_engine::world::block::BlockId) -> azalea_block::BlockState {
    // For now, treat BlockId as a direct MC block state ID.
    // BlockId(0) = air, others map through azalea.
    azalea_block::BlockState::try_from(id.0 as u32).unwrap_or(azalea_block::BlockState::AIR)
}

// ── Chunk data ──────────────────────────────────────────────────────────

/// Send a chunk read from the World in MC 1.21.5+ wire format.
/// Reads actual block state from the engine World, so edits persist.
async fn send_chunk_from_world<W: AsyncWrite + Unpin + Send>(
    write: &mut W,
    compression: Option<u32>,
    cipher: &mut Option<azalea_crypto::Aes128CfbEnc>,
    world: &World,
    cx: i32,
    cz: i32,
) -> Result<()> {
    use ultimate_engine::world::block::BlockId;

    let total_sections = 24;
    let min_y: i64 = -64;
    let base_x = (cx as i64) * 16;
    let base_z = (cz as i64) * 16;
    let mut section_data = Vec::new();

    for section_i in 0..total_sections {
        let section_base_y = min_y + (section_i as i64) * 16;

        // Scan: is the section uniform? Count non-air blocks.
        let first = world.get_block(ultimate_engine::world::position::BlockPos::new(
            base_x, section_base_y, base_z,
        ));
        let mut all_same = true;
        let mut non_air: u16 = 0;

        for ly in 0..16i64 {
            for lz in 0..16i64 {
                for lx in 0..16i64 {
                    let b = world.get_block(ultimate_engine::world::position::BlockPos::new(
                        base_x + lx, section_base_y + ly, base_z + lz,
                    ));
                    if b != first { all_same = false; }
                    if b != BlockId::AIR { non_air = non_air.saturating_add(1); }
                }
            }
        }

        if all_same {
            if first == BlockId::AIR {
                write_empty_section(&mut section_data)?;
            } else {
                write_single_section(&mut section_data, first.0 as u32)?;
            }
        } else {
            // Mixed section: build palette + indirect encoding
            write_section_from_world(
                &mut section_data, world,
                base_x, section_base_y, base_z, non_air,
            )?;
        }
    }

    // Build the chunk packet manually because azalea's AzBuf derive
    // serializes heightmaps as a VarInt-prefixed Vec, but the MC protocol
    // expects them as an NBT compound. azalea is a client lib (reads only).
    use azalea_buf::AzaleaWriteVar;
    use azalea_protocol::packets::ProtocolPacket;
    use azalea_protocol::simdnbt;

    let mut raw_packet = Vec::new();

    // Packet ID for ClientboundLevelChunkWithLight
    let dummy = azalea_protocol::packets::game::ClientboundLevelChunkWithLight {
        x: 0, z: 0,
        chunk_data: azalea_protocol::packets::game::c_level_chunk_with_light::ClientboundLevelChunkPacketData {
            heightmaps: vec![], data: vec![].into_boxed_slice().into(), block_entities: vec![],
        },
        light_data: azalea_protocol::packets::game::c_light_update::ClientboundLightUpdatePacketData {
            sky_y_mask: BitSet::new(0), block_y_mask: BitSet::new(0),
            empty_sky_y_mask: BitSet::new(0), empty_block_y_mask: BitSet::new(0),
            sky_updates: vec![], block_updates: vec![],
        },
    };
    let packet_id = ClientboundGamePacket::LevelChunkWithLight(dummy).id();
    (packet_id as u32).azalea_write_var(&mut raw_packet)?;

    // x, z (Int, Int)
    cx.azalea_write(&mut raw_packet)?;
    cz.azalea_write(&mut raw_packet)?;

    // Heightmaps as Prefixed Array (1.21.5+ format, NOT NBT).
    // Format: VarInt(count) + for each: VarInt(type_enum) + VarInt(long_count) + i64[]
    // Empty = just VarInt(0).
    0u32.azalea_write_var(&mut raw_packet)?;

    // Data: VarInt(length) + raw section bytes
    (section_data.len() as u32).azalea_write_var(&mut raw_packet)?;
    raw_packet.extend_from_slice(&section_data);

    // Block entities: VarInt(0)
    0u32.azalea_write_var(&mut raw_packet)?;

    // Light data
    // sky_y_mask, block_y_mask, empty_sky_y_mask, empty_block_y_mask (BitSets)
    BitSet::new(0).azalea_write(&mut raw_packet)?;
    BitSet::new(0).azalea_write(&mut raw_packet)?;
    BitSet::new(0).azalea_write(&mut raw_packet)?;
    BitSet::new(0).azalea_write(&mut raw_packet)?;
    // sky_updates, block_updates (empty arrays)
    0u32.azalea_write_var(&mut raw_packet)?;
    0u32.azalea_write_var(&mut raw_packet)?;

    // Write the raw packet with framing
    azalea_protocol::write::write_raw_packet(&raw_packet, write, compression, cipher).await?;

    Ok(())
}

/// Write a mixed chunk section by reading blocks from the World.
/// Uses indirect palette encoding (1.21.5+ format: no VarInt data_length).
fn write_section_from_world(
    buf: &mut Vec<u8>,
    world: &World,
    base_x: i64,
    base_y: i64,
    base_z: i64,
    non_air_count: u16,
) -> Result<()> {
    use azalea_buf::AzaleaWriteVar;
    use ultimate_engine::world::block::BlockId;

    // Build palette and block index array
    let mut palette: Vec<u32> = vec![0]; // air always at index 0
    let mut blocks = [0u8; 4096];

    for ly in 0..16u64 {
        for lz in 0..16u64 {
            for lx in 0..16u64 {
                let b = world.get_block(ultimate_engine::world::position::BlockPos::new(
                    base_x + lx as i64, base_y + ly as i64, base_z + lz as i64,
                ));
                let state_id = b.0 as u32;
                let palette_idx = match palette.iter().position(|&v| v == state_id) {
                    Some(i) => i,
                    None => {
                        palette.push(state_id);
                        palette.len() - 1
                    }
                };
                let idx = (ly as usize) * 256 + (lz as usize) * 16 + (lx as usize);
                blocks[idx] = palette_idx as u8;
            }
        }
    }

    // Bits per entry: minimum 4 for blocks
    let bpe = (palette.len() as f64).log2().ceil().max(1.0) as u8;
    let bpe = bpe.max(4); // MC minimum for indirect block palette

    // Write block count
    (non_air_count as i16).azalea_write(buf)?;
    // Bits per entry
    bpe.azalea_write(buf)?;
    // Palette
    (palette.len() as u32).azalea_write_var(buf)?;
    for &id in &palette {
        id.azalea_write_var(buf)?;
    }
    // Packed data (1.21.5+: NO VarInt length prefix)
    let values_per_long = 64 / bpe as usize;
    let num_longs = (4096 + values_per_long - 1) / values_per_long;
    let mask = (1u64 << bpe) - 1;
    for long_i in 0..num_longs {
        let mut long_val: u64 = 0;
        for vi in 0..values_per_long {
            let block_i = long_i * values_per_long + vi;
            if block_i < 4096 {
                long_val |= ((blocks[block_i] as u64) & mask) << (vi * bpe as usize);
            }
        }
        long_val.azalea_write(buf)?;
    }

    // Biomes: single-valued (plains = 0)
    0u8.azalea_write(buf)?;
    0u32.azalea_write_var(buf)?;

    Ok(())
}

/// Write a single-valued non-air chunk section (all blocks the same).
///
/// 1.21.5+ format: no VarInt data_length for paletted containers.
fn write_single_section(buf: &mut Vec<u8>, block_state_id: u32) -> Result<()> {
    use azalea_buf::AzaleaWriteVar;

    // Block count (i16)
    4096i16.azalea_write(buf)?;
    // Block states: single-valued palette (bpe=0, value, NO data array)
    0u8.azalea_write(buf)?;                    // bits_per_entry = 0
    block_state_id.azalea_write_var(buf)?;     // palette value
    // No data array length or data for single-valued (1.21.5+)
    // Biomes: single-valued (plains = 0)
    0u8.azalea_write(buf)?;
    0u32.azalea_write_var(buf)?;
    // No data array for biomes either

    Ok(())
}

/// Write an empty (all-air) chunk section to the buffer.
///
/// 1.21.5+ format: no VarInt data_length for paletted containers.
fn write_empty_section(buf: &mut Vec<u8>) -> Result<()> {
    use azalea_buf::AzaleaWriteVar;

    // Block count: 0 (no non-air blocks)
    0i16.azalea_write(buf)?;
    // Block states: single-valued palette = air (0)
    0u8.azalea_write(buf)?;       // bits_per_entry = 0
    0u32.azalea_write_var(buf)?;   // palette value = 0 (air)
    // No data array (1.21.5+)
    // Biomes: single-valued = plains (0)
    0u8.azalea_write(buf)?;
    0u32.azalea_write_var(buf)?;
    // No data array

    Ok(())
}

/// Write a chunk section with specific block layers.
/// `layers` is a slice of (local_y, block_state_id, height_in_blocks).
fn write_mixed_section(buf: &mut Vec<u8>, layers: &[(u8, u32, u8)]) -> Result<()> {
    use azalea_buf::AzaleaWriteVar;

    // Count non-air blocks
    let non_air: u16 = layers.iter().map(|(_, _, h)| 256 * (*h as u16)).sum();

    // Build a palette: collect unique block state IDs (including air)
    let mut palette_ids: Vec<u32> = vec![0]; // air is always index 0
    for &(_, block_id, _) in layers {
        if !palette_ids.contains(&block_id) {
            palette_ids.push(block_id);
        }
    }

    // Build the 16x16x16 block array
    let mut blocks = [0u8; 4096]; // palette indices, not block state IDs
    for &(start_y, block_id, height) in layers {
        let palette_idx = palette_ids.iter().position(|&id| id == block_id).unwrap() as u8;
        for dy in 0..height {
            let y = (start_y + dy) as usize;
            for z in 0..16usize {
                for x in 0..16usize {
                    blocks[y * 256 + z * 16 + x] = palette_idx;
                }
            }
        }
    }

    // Determine bits per entry
    let bits_per_entry = if palette_ids.len() <= 1 {
        0
    } else if palette_ids.len() <= 2 {
        1 // minimum indirect bits for blocks is 4, but let's use proper calculation
    } else {
        (palette_ids.len() as f64).log2().ceil() as u8
    };

    // For blocks, minimum indirect bits is 4
    let bits_per_entry = if bits_per_entry == 0 { 0 } else { bits_per_entry.max(4) };

    // Write block count
    non_air.azalea_write(buf)?;

    if bits_per_entry == 0 {
        // Single-valued palette
        0u8.azalea_write(buf)?;
        palette_ids[0].azalea_write_var(buf)?;
        0u32.azalea_write_var(buf)?;
    } else {
        // Indirect palette
        (bits_per_entry as u8).azalea_write(buf)?;
        // Palette length
        (palette_ids.len() as u32).azalea_write_var(buf)?;
        for &id in &palette_ids {
            id.azalea_write_var(buf)?;
        }

        // Pack block indices into longs
        let values_per_long = 64 / bits_per_entry as usize;
        let num_longs = (4096 + values_per_long - 1) / values_per_long;
        (num_longs as u32).azalea_write_var(buf)?;

        let mask = (1u64 << bits_per_entry) - 1;
        for long_i in 0..num_longs {
            let mut long_val: u64 = 0;
            for vi in 0..values_per_long {
                let block_i = long_i * values_per_long + vi;
                if block_i < 4096 {
                    long_val |= ((blocks[block_i] as u64) & mask) << (vi * bits_per_entry as usize);
                }
            }
            long_val.azalea_write(buf)?;
        }
    }

    // Biomes: single-valued (plains = 0)
    0u8.azalea_write(buf)?;
    0u32.azalea_write_var(buf)?;
    0u32.azalea_write_var(buf)?;

    Ok(())
}

/// Generate an offline-mode UUID from a player name.
fn offline_uuid(name: &str) -> Uuid {
    Uuid::new_v3(&Uuid::NAMESPACE_URL, format!("OfflinePlayer:{}", name).as_bytes())
}
