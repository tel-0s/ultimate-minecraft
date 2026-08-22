//! Pre-play protocol phases: status ping, offline login,
//! configuration (registries, tags, known packs).

use std::io::Cursor;

use anyhow::{anyhow, Result};
use azalea_auth::game_profile::GameProfile;
use azalea_chat::FormattedText;
use azalea_protocol::packets::config::{
    ClientboundConfigPacket, ClientboundFinishConfiguration, ClientboundRegistryData,
    ClientboundSelectKnownPacks, ClientboundUpdateTags, ServerboundConfigPacket,
};
use azalea_protocol::common::tags::{TagMap, Tags};
use azalea_protocol::packets::status::c_status_response::SamplePlayer;
use azalea_protocol::packets::login::{
    ClientboundLoginFinished, ClientboundLoginPacket,
    ServerboundLoginPacket,
};
use azalea_protocol::packets::status::{
    ClientboundPongResponse, ClientboundStatusPacket, ClientboundStatusResponse,
    ServerboundStatusPacket,
};
use azalea_protocol::packets::status::c_status_response::{Version, Players};
use azalea_protocol::packets::Packet;
use azalea_protocol::packets::config::s_select_known_packs::KnownPack;
use azalea_protocol::read::read_packet;
use azalea_protocol::write::write_packet;
use azalea_registry::identifier::Identifier;
use tokio::io::{AsyncRead, AsyncWrite};
use uuid::Uuid;

use crate::player_registry::PlayerRegistry;


// ── Status ──────────────────────────────────────────────────────────────

pub(crate) async fn handle_status<R, W>(
    read: &mut R, write: &mut W, buf: &mut Cursor<Vec<u8>>,
    compression: Option<u32>,
    cipher_enc: &mut Option<azalea_crypto::Aes128CfbEnc>,
    cipher_dec: &mut Option<azalea_crypto::Aes128CfbDec>,
    registry: &PlayerRegistry,
    network: &crate::config::NetworkConfig,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + Sync,
    W: AsyncWrite + Unpin + Send,
{
    // Client sends status request
    let packet = read_packet::<ServerboundStatusPacket, _>(read, buf, compression, cipher_dec).await?;
    tracing::debug!("Status request: {:?}", packet);

    // Build player sample from registry.
    let online_players = registry.snapshot();
    let sample: Vec<SamplePlayer> = online_players
        .iter()
        .take(12) // MC shows at most ~12 in the hover tooltip
        .map(|p| SamplePlayer {
            id: p.uuid.to_string(),
            name: p.name.clone(),
        })
        .collect();

    // Respond with server status
    let response: ClientboundStatusPacket = ClientboundStatusResponse {
        description: FormattedText::from("Ultimate Minecraft - Causal Graph Engine"),
        favicon: None,
        players: Players {
            max: network.max_players as i32,
            online: online_players.len() as i32,
            sample,
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

pub(crate) async fn handle_login<R, W>(
    read: &mut R, write: &mut W, buf: &mut Cursor<Vec<u8>>,
    compression: Option<u32>,
    cipher_enc: &mut Option<azalea_crypto::Aes128CfbEnc>,
    cipher_dec: &mut Option<azalea_crypto::Aes128CfbDec>,
) -> Result<(String, Uuid)>
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
        // 26.2: server-assigned session identity (used by the client for
        // report/chat session plumbing; any stable UUID works offline).
        session_id: uuid,
    }.into_variant();
    write_packet(&response, write, compression, cipher_enc).await?;

    // Wait for Login Acknowledged
    let ack = read_packet::<ServerboundLoginPacket, _>(read, buf, compression, cipher_dec).await?;
    tracing::debug!("Login ack: {:?}", ack);

    Ok((name, uuid))
}

// ── Configuration ───────────────────────────────────────────────────────

pub(crate) async fn handle_configuration<R, W>(
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
pub(crate) async fn send_registries<W: AsyncWrite + Unpin + Send>(
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
pub(crate) async fn send_tags<W: AsyncWrite + Unpin + Send>(
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
pub(crate) fn registry_entries() -> Vec<(String, Vec<String>)> {
    vec![
        ("minecraft:dimension_type".into(), vec![
            "minecraft:overworld".into(),
            "minecraft:overworld_caves".into(),
            "minecraft:the_nether".into(),
            "minecraft:the_end".into(),
        ]),
        // Derived from azalea's biome table (single source with
        // `worldgen::Biome::registry_id`): the order of this list DEFINES
        // the numeric biome ids in every chunk packet.
        (
            "minecraft:worldgen/biome".into(),
            crate::registry::BIOME_REGISTRY.clone(),
        ),
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


/// Generate an offline-mode UUID from a player name.
pub(crate) fn offline_uuid(name: &str) -> Uuid {
    Uuid::new_v3(&Uuid::NAMESPACE_URL, format!("OfflinePlayer:{}", name).as_bytes())
}
