//! Player presence on one client: tab list + player entity spawns, with
//! the O(N²)-taming caps and batch coalescing from the 10k load test.
//!
//! Tracks WHO this client has been told about so removals stay
//! consistent under the caps (`network.tab_list_cap` /
//! `network.entity_spawn_cap` — uncapped presence is O(N²) bytes across
//! all clients during a join storm). Also delivers chat, which rides the
//! same global lifecycle bus.

use anyhow::Result;
use azalea_auth::game_profile::GameProfile;
use azalea_chat::FormattedText;
use azalea_core::delta::LpVec3;
use azalea_core::entity_id::MinecraftEntityId;
use azalea_core::game_type::GameMode;
use azalea_core::position::Vec3;
use azalea_protocol::packets::Packet;
use azalea_protocol::packets::game::c_player_info_update::{ActionEnumSet, PlayerInfoEntry};
use azalea_protocol::packets::game::{
    ClientboundAddEntity, ClientboundGamePacket, ClientboundPlayerInfoRemove,
    ClientboundPlayerInfoUpdate, ClientboundRemoveEntities, ClientboundSystemChat,
};
use azalea_protocol::write::write_packet;
use azalea_registry::builtin::EntityKind;
use std::collections::HashSet;
use tokio::io::AsyncWrite;
use uuid::Uuid;

use super::entity_view::degrees_to_byte_angle;
use crate::player_registry::{PlayerEvent, PlayerInfo};

/// One tab-list entry (creative, no chat session — the only shape this
/// server sends today).
fn tab_entry(uuid: Uuid, name: String) -> PlayerInfoEntry {
    PlayerInfoEntry {
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
    }
}

/// The action set for an "add players" tab-list update.
fn add_players_actions() -> ActionEnumSet {
    ActionEnumSet {
        add_player: true,
        initialize_chat: false,
        update_game_mode: true,
        update_listed: true,
        update_latency: true,
        update_display_name: false,
        update_hat: false,
        update_list_order: false,
    }
}

fn player_spawn_packet(
    eid: i32,
    uuid: Uuid,
    x: f64,
    y: f64,
    z: f64,
    y_rot: f32,
    x_rot: f32,
) -> ClientboundGamePacket {
    ClientboundAddEntity {
        id: MinecraftEntityId(eid),
        uuid,
        entity_type: EntityKind::Player,
        position: Vec3 { x, y, z },
        movement: LpVec3::Zero,
        x_rot: degrees_to_byte_angle(x_rot),
        y_rot: degrees_to_byte_angle(y_rot),
        y_head_rot: degrees_to_byte_angle(y_rot),
        data: 0,
    }
    .into_variant()
}

pub(crate) struct Presence {
    tab_cap: usize,
    spawn_cap: usize,
    /// Players this client has in its tab list.
    tab_listed: HashSet<Uuid>,
    /// Player entities this client has been sent.
    spawned: HashSet<i32>,
}

impl Presence {
    pub fn new(net: &crate::config::NetworkConfig) -> Self {
        let cap = |n: usize| if n == 0 { usize::MAX } else { n };
        Self {
            tab_cap: cap(net.tab_list_cap),
            spawn_cap: cap(net.entity_spawn_cap),
            tab_listed: HashSet::new(),
            spawned: HashSet::new(),
        }
    }

    /// Tell a joining client about every player already online (plus
    /// itself) in ONE multi-entry tab-list packet — a packet per player
    /// made joining O(N) packets and a join storm O(N²) server-wide —
    /// then spawn each existing player's entity.
    pub async fn send_initial<W: AsyncWrite + Unpin + Send>(
        &mut self,
        write: &mut W,
        compression: Option<u32>,
        cipher: &mut Option<azalea_crypto::Aes128CfbEnc>,
        existing_players: &[PlayerInfo],
        self_uuid: Uuid,
        self_name: &str,
    ) -> Result<()> {
        let mut tab_entries: Vec<PlayerInfoEntry> = Vec::new();
        for p in existing_players.iter().take(self.tab_cap) {
            self.tab_listed.insert(p.uuid);
            tab_entries.push(tab_entry(p.uuid, p.name.clone()));
        }
        tab_entries.push(tab_entry(self_uuid, self_name.to_owned()));
        let info_packet: ClientboundGamePacket = ClientboundPlayerInfoUpdate {
            actions: add_players_actions(),
            entries: tab_entries,
        }
        .into_variant();
        write_packet(&info_packet, write, compression, cipher).await?;

        for p in existing_players.iter().take(self.spawn_cap) {
            self.spawned.insert(p.entity_id);
            let pkt = player_spawn_packet(p.entity_id, p.uuid, p.x, p.y, p.z, p.y_rot, p.x_rot);
            write_packet(&pkt, write, compression, cipher).await?;
        }
        Ok(())
    }

    /// Deliver a drained burst of lifecycle events, COALESCED: one
    /// multi-entry tab add, batched entity spawns, one batched remove —
    /// during a join storm every connection receives every join, so
    /// per-event packets made the storm O(N²) packet writes server-wide.
    /// Chat rides the same bus and is written inline.
    pub async fn apply_events<W: AsyncWrite + Unpin + Send>(
        &mut self,
        write: &mut W,
        compression: Option<u32>,
        cipher: &mut Option<azalea_crypto::Aes128CfbEnc>,
        own_conn_id: u64,
        events: Vec<PlayerEvent>,
    ) -> Result<()> {
        let mut join_entries: Vec<PlayerInfoEntry> = Vec::new();
        let mut spawn_pkts: Vec<ClientboundGamePacket> = Vec::new();
        let mut left_eids: Vec<MinecraftEntityId> = Vec::new();
        let mut left_uuids = Vec::new();
        for event in events {
            match event {
                PlayerEvent::Joined {
                    conn_id, entity_id, uuid, name, x, y, z, y_rot, x_rot,
                } => {
                    if conn_id == own_conn_id {
                        continue; // our own join
                    }
                    if self.tab_listed.len() < self.tab_cap && self.tab_listed.insert(uuid) {
                        join_entries.push(tab_entry(uuid, name));
                    }
                    if self.spawned.len() < self.spawn_cap && self.spawned.insert(entity_id) {
                        spawn_pkts.push(player_spawn_packet(entity_id, uuid, x, y, z, y_rot, x_rot));
                    }
                }
                PlayerEvent::Moved { .. } => {
                    // Movement is delivered through the spatial bus;
                    // nothing should arrive here.
                }
                PlayerEvent::Left { conn_id, entity_id, uuid } => {
                    if conn_id == own_conn_id {
                        continue;
                    }
                    // Only retract what this client was actually sent.
                    if self.spawned.remove(&entity_id) {
                        left_eids.push(MinecraftEntityId(entity_id));
                    }
                    if self.tab_listed.remove(&uuid) {
                        left_uuids.push(uuid);
                    }
                }
                PlayerEvent::Chat { name, message, .. } => {
                    let text = format!("<{}> {}", name, message);
                    let chat_pkt: ClientboundGamePacket = ClientboundSystemChat {
                        content: FormattedText::from(text),
                        overlay: false,
                    }
                    .into_variant();
                    write_packet(&chat_pkt, write, compression, cipher).await?;
                }
            }
        }

        if !join_entries.is_empty() {
            let info_pkt: ClientboundGamePacket = ClientboundPlayerInfoUpdate {
                actions: add_players_actions(),
                entries: join_entries,
            }
            .into_variant();
            write_packet(&info_pkt, write, compression, cipher).await?;
            for spawn_pkt in &spawn_pkts {
                write_packet(spawn_pkt, write, compression, cipher).await?;
            }
        }
        if !left_eids.is_empty() {
            let remove_pkt: ClientboundGamePacket = ClientboundRemoveEntities {
                entity_ids: left_eids,
            }
            .into_variant();
            write_packet(&remove_pkt, write, compression, cipher).await?;
        }
        if !left_uuids.is_empty() {
            let info_remove: ClientboundGamePacket = ClientboundPlayerInfoRemove {
                profile_ids: left_uuids,
            }
            .into_variant();
            write_packet(&info_remove, write, compression, cipher).await?;
        }
        Ok(())
    }
}
