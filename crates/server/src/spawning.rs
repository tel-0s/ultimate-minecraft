//! Natural mob spawning — the first real ambient simulation layer.
//!
//! A [`SimulationLayer`] task ticks every couple of seconds and, per
//! player (read from the EntityStore's player mirrors, so it works on
//! gateways and physics nodes alike), rolls a few spawn attempts in a
//! ring around them: find standable ground (a colliding block below, two
//! pass-through cells above), respect the per-player and global caps,
//! and submit the ordinary guarded mob-spawn events. The same sweep
//! despawns mobs that no player is near — vanilla's 128-block rule.
//!
//! The layer is a pure event SOURCE: everything it decides lands in the
//! causal graph as root events with the usual guards, so a racing
//! player action can never be trampled and replicas learn spawns through
//! WriteSync like any other entity transition.

use std::time::Duration;

use ultimate_engine::causal::event::{Event, EventPayload};
use ultimate_engine::world::World;
use ultimate_engine::world::entity::Vec3;
use ultimate_engine::world::position::BlockPos;

use crate::rules::entity::KIND_PLAYER;
use crate::rules::mob::{KIND_MOB, spawn_mob_events};
use crate::simulation::SimulationLayer;

/// Ring around each player where spawns land (vanilla-ish).
const SPAWN_MIN_DIST: f64 = 24.0;
const SPAWN_MAX_DIST: f64 = 64.0;
/// Mobs farther than this from EVERY player despawn.
const DESPAWN_DIST: f64 = 128.0;
/// Ground probe range around the player's altitude.
const PROBE_UP: i64 = 12;
const PROBE_DOWN: i64 = 24;

pub struct MobSpawner {
    pub per_player_cap: usize,
    pub global_cap: usize,
    /// Non-cryptographic tick-local randomness (spawning is a root cause,
    /// like a player action — it doesn't need cross-run determinism).
    state: std::sync::atomic::AtomicU64,
}

impl MobSpawner {
    pub fn new(per_player_cap: usize, global_cap: usize) -> Self {
        Self {
            per_player_cap,
            global_cap,
            state: std::sync::atomic::AtomicU64::new(0x9E3779B97F4A7C15),
        }
    }

    fn roll(&self) -> u64 {
        let mut x = self
            .state
            .fetch_add(0x9E3779B97F4A7C15, std::sync::atomic::Ordering::Relaxed);
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
        x ^ (x >> 31)
    }

    /// First standable cell near `(x, z)` around altitude `y_hint`: a
    /// colliding block below, two pass-through cells at and above. Scans
    /// downward from above so mobs land ON terrain, not inside caves.
    fn find_ground(world: &World, x: i64, z: i64, y_hint: i64) -> Option<i64> {
        let passes = |y: i64| {
            !crate::block::blocks_entity_movement(world.get_block(BlockPos::new(x, y, z)))
        };
        let floor = |y: i64| {
            crate::block::blocks_entity_movement(world.get_block(BlockPos::new(x, y - 1, z)))
        };
        for y in ((y_hint - PROBE_DOWN)..=(y_hint + PROBE_UP)).rev() {
            if passes(y) && passes(y + 1) && floor(y) {
                return Some(y);
            }
        }
        None
    }
}

impl SimulationLayer for MobSpawner {
    fn name(&self) -> &'static str {
        "mob_spawning"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(2)
    }

    fn generate_events(&self, world: &World) -> Vec<Event> {
        let mut players: Vec<Vec3> = Vec::new();
        let mut mobs: Vec<(ultimate_engine::world::entity::EntityId, ultimate_engine::world::entity::EntityState)> =
            Vec::new();
        for (id, s) in world.entities().snapshot() {
            if s.kind == KIND_PLAYER {
                players.push(s.pos);
            } else if s.kind == KIND_MOB {
                mobs.push((id, s));
            }
        }

        let mut events = Vec::new();

        // Despawn sweep: mobs beyond DESPAWN_DIST of every player
        // (guarded — a concurrent transition wins and the despawn no-ops).
        let near = |m: &Vec3, limit: f64| {
            players
                .iter()
                .any(|p| (p.x - m.x).abs() <= limit && (p.z - m.z).abs() <= limit)
        };
        if !players.is_empty() {
            for (id, s) in &mobs {
                if !near(&s.pos, DESPAWN_DIST) {
                    events.push(Event {
                        payload: EventPayload::EntitySet { id: *id, old: Some(*s), new: None },
                    });
                }
            }
        }

        // Spawn attempts.
        if mobs.len() >= self.global_cap {
            return events;
        }
        let mut projected_total = mobs.len();
        for p in &players {
            let nearby = mobs
                .iter()
                .filter(|(_, m)| near(&m.pos, 0.0_f64.max(SPAWN_MAX_DIST + 16.0)) && {
                    (m.pos.x - p.x).abs() <= SPAWN_MAX_DIST + 16.0
                        && (m.pos.z - p.z).abs() <= SPAWN_MAX_DIST + 16.0
                })
                .count();
            if nearby >= self.per_player_cap {
                continue;
            }
            for _ in 0..3 {
                if projected_total >= self.global_cap {
                    break;
                }
                let r = self.roll();
                let dist = SPAWN_MIN_DIST
                    + (r & 0xFFFF) as f64 / 65535.0 * (SPAWN_MAX_DIST - SPAWN_MIN_DIST);
                let theta = ((r >> 16) & 0xFFFF) as f64 / 65535.0 * std::f64::consts::TAU;
                let x = (p.x + theta.cos() * dist).floor() as i64;
                let z = (p.z + theta.sin() * dist).floor() as i64;
                let Some(y) = Self::find_ground(world, x, z, p.y.floor() as i64) else {
                    continue;
                };
                events.extend(spawn_mob_events(
                    world,
                    Vec3::new(x as f64 + 0.5, y as f64 + 0.1, z as f64 + 0.5),
                    0,
                ));
                projected_total += 1;
                break; // one spawn per player per tick keeps the ramp gentle
            }
        }
        events
    }
}
