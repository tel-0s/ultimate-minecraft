//! Mobile causal actors (Phase 5).
//!
//! Blocks are stationary causality: events happen *at* positions. Entities
//! are mobile causality: the position itself is state. An entity's state is
//! a **parametric trajectory** — position and velocity at a `stamp` — so
//! `position(t)` is a pure function and nothing is computed, stored, or
//! scheduled between causally-relevant events. A resting entity costs zero.
//!
//! Deliberately mirrors the block design: the engine knows entities exist,
//! have identity, kinematic state, and spatial location. It does not know
//! what an item or a zombie is — that's the game's concern (`EntityKind` is
//! opaque, like `BlockId`; `aux` is an opaque game payload).
//!
//! Design: docs/phase5-entities.md.

use crate::world::position::{BlockPos, ChunkPos};
use dashmap::DashMap;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};

/// Engine time in nanoseconds (see [`crate::causal::clock`]). Causality
/// provides ORDER; the clock provides PACE.
pub type Nanos = u64;

/// Stable entity identity. NOT a slotmap key: ids must survive
/// serialization (cluster frames, replicas) and be mintable independently
/// on every node.
///
/// Namespace layout (by convention, enforced nowhere but respected
/// everywhere):
/// - bit 63: reserved for game-level namespaces (the server derives player
///   entity ids as `0x8000_0000_0000_0000 | registry_eid`),
/// - bits 48..63: node salt ([`EntityStore::set_id_salt`]) so cluster nodes
///   never mint colliding ids,
/// - bits 0..48: per-node allocation counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntityId(pub u64);

/// Opaque entity kind — the entity analog of `BlockId`. The engine never
/// interprets it; the game registers kinds and dispatches rules on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntityKind(pub u16);

/// A 3D vector in block-space (f64: positions are continuous, unlike the
/// lattice).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Vec3 {
    pub const ZERO: Vec3 = Vec3 { x: 0.0, y: 0.0, z: 0.0 };

    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    /// The lattice cell containing this point (floor on every axis).
    pub fn block_pos(&self) -> BlockPos {
        BlockPos::new(
            self.x.floor() as i64,
            self.y.floor() as i64,
            self.z.floor() as i64,
        )
    }
}

/// An entity's complete engine-visible state: a parametric trajectory
/// segment plus opaque game data.
///
/// `pos`/`vel` are the state at engine time `stamp`; between events the
/// game extrapolates (`pos + vel·Δt + ½gΔt²` — the g is the game's,
/// not ours). `Copy + PartialEq` because guarded transitions compare whole
/// states: an `EntitySet { old, .. }` applies only while the store still
/// holds exactly `old` (first-write-wins, like `BlockSet`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EntityState {
    pub kind: EntityKind,
    /// Position at `stamp`.
    pub pos: Vec3,
    /// Velocity at `stamp`.
    pub vel: Vec3,
    /// Engine-clock time this state was true.
    pub stamp: Nanos,
    /// Opaque game data (e.g. the block id a FallingBlock carries, a
    /// dropped item's despawn deadline, a player's view rotations).
    pub aux: u64,
}

impl EntityState {
    /// The chunk holding this state's position — the routing anchor for
    /// entity events (ownership follows the entity).
    pub fn chunk(&self) -> ChunkPos {
        self.pos.block_pos().chunk()
    }
}

/// Bits available to the per-node allocation counter (the salt sits above).
const ID_COUNTER_BITS: u32 = 48;
const ID_COUNTER_MASK: u64 = (1 << ID_COUNTER_BITS) - 1;

/// The mobile half of the world: entity states plus the spatial indexes
/// rules use to find them. Lives on [`crate::world::World`] beside the
/// chunks, which is why `RuleFn(&World, &EventPayload)` needed no new
/// parameter for Phase 5.
///
/// ## Write discipline
///
/// [`set_entity`](Self::set_entity) is the guarded transition every
/// `EntitySet` applies through: compare-current-vs-`old`, first write wins.
/// The DashMap **entry lock is the process-wide commit point** — the atomic
/// `EntityMaterialize` conversion relies on winning it (see
/// `causal::scheduler::apply_event`), which is what makes entity→block
/// conversion correct under contention and region-handoff dual ownership.
///
/// ## Index consistency
///
/// `by_chunk` / `by_column` are maintained *after* the entity write commits,
/// so a concurrent spatial query can transiently miss (or over-report) an
/// entity mid-transition. That is the same tolerated race class as
/// cross-partition block reads: wakes are idempotent and re-derive from the
/// store, so a spurious hit is a cheap no-op and a miss is healed by the
/// next causally-related wake. Queries return owned `Vec`s — never hold an
/// index guard while reading entity states (lock order is always
/// entities → index).
pub struct EntityStore {
    entities: DashMap<EntityId, EntityState>,
    /// Spatial index: which entities are in a chunk (AOI queries, client
    /// view backfill).
    by_chunk: DashMap<ChunkPos, BTreeSet<EntityId>>,
    /// Spatial index: which entities are in a 1×1 block column
    /// (wake-on-block-change — per-chunk scans dominated dense cascades).
    /// Key is the (x, z) of the containing block cell. `BTreeSet` for
    /// deterministic iteration order.
    by_column: DashMap<(i64, i64), BTreeSet<EntityId>>,
    /// Per-node allocation counter (low 48 bits of minted ids).
    next_id: AtomicU64,
    /// Pre-shifted node salt OR'd into minted ids (bits 48..63).
    id_salt: AtomicU64,
}

impl EntityStore {
    pub fn new() -> Self {
        Self {
            entities: DashMap::new(),
            by_chunk: DashMap::new(),
            by_column: DashMap::new(),
            next_id: AtomicU64::new(0),
            id_salt: AtomicU64::new(0),
        }
    }

    /// Install this node's id salt (cluster `node_id`) so ids minted on
    /// different nodes never collide. Call once at startup, before any
    /// allocation.
    pub fn set_id_salt(&self, salt: u16) {
        self.id_salt
            .store((salt as u64) << ID_COUNTER_BITS, Ordering::Relaxed);
    }

    /// Mint a fresh entity id: node salt in the high bits, a per-node
    /// counter below. Counter starts at 1 so id 0 never exists.
    pub fn allocate_id(&self) -> EntityId {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        EntityId(self.id_salt.load(Ordering::Relaxed) | (n & ID_COUNTER_MASK))
    }

    /// Current state of an entity (owned copy; states are small and `Copy`).
    pub fn get(&self, id: EntityId) -> Option<EntityState> {
        self.entities.get(&id).map(|r| *r)
    }

    /// The guarded entity transition — the entity analog of the `BlockSet`
    /// stale guard. Applies `new` (or removes, for `None`) only while the
    /// store's current state still equals `old` exactly
    /// (`None` = must not exist). Returns whether the guard passed and the
    /// transition applied; a superseded in-flight segment fails here and
    /// dies without consequents (never forks the entity).
    ///
    /// The entry lock held across the compare-and-write is the commit
    /// point that `EntityMaterialize` builds its cross-store atomicity on.
    pub fn set_entity(
        &self,
        id: EntityId,
        old: Option<&EntityState>,
        new: Option<&EntityState>,
    ) -> bool {
        use dashmap::mapref::entry::Entry;

        let prev: Option<EntityState>;
        match (self.entities.entry(id), old) {
            // Spawn: must not exist yet.
            (Entry::Vacant(v), None) => {
                prev = None;
                match new {
                    Some(n) => {
                        v.insert(*n);
                    }
                    // None → None: guard passes, nothing to do or index.
                    None => return true,
                }
            }
            // Move / despawn: must exist and match the observed state.
            (Entry::Occupied(mut occ), Some(expect)) if occ.get() == expect => {
                prev = Some(*occ.get());
                match new {
                    Some(n) => {
                        occ.insert(*n);
                    }
                    None => {
                        occ.remove();
                    }
                }
            }
            // Guard failed: another transition won this entity.
            _ => return false,
        }

        self.reindex(id, prev.as_ref(), new.copied().as_ref());
        true
    }

    /// Unguarded verbatim write — for replicas applying an owner's
    /// authoritative `WriteSync` outcome, where the guard already ran on
    /// the owner and re-checking against replica state would wrongly
    /// reject (replicas may be stale). Maintains the spatial indexes.
    pub fn set_entity_unchecked(&self, id: EntityId, new: Option<&EntityState>) {
        let prev = match new {
            Some(n) => self.entities.insert(id, *n),
            None => self.entities.remove(&id).map(|(_, s)| s),
        };
        self.reindex(id, prev.as_ref(), new);
    }

    /// Entities currently indexed in `chunk`. Owned snapshot — see the
    /// index-consistency note on [`EntityStore`].
    pub fn in_chunk(&self, chunk: ChunkPos) -> Vec<EntityId> {
        self.by_chunk
            .get(&chunk)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Entities currently indexed in the 1×1 block column at `(x, z)`.
    /// Owned snapshot — see the index-consistency note on [`EntityStore`].
    pub fn in_column(&self, x: i64, z: i64) -> Vec<EntityId> {
        self.by_column
            .get(&(x, z))
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    /// All entities, sorted by id (deterministic for tests/diagnostics).
    pub fn snapshot(&self) -> Vec<(EntityId, EntityState)> {
        let mut all: Vec<(EntityId, EntityState)> = self
            .entities
            .iter()
            .map(|r| (*r.key(), *r.value()))
            .collect();
        all.sort_by_key(|(id, _)| *id);
        all
    }

    pub fn len(&self) -> usize {
        self.entities.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// Move `id` between spatial-index buckets to reflect a committed
    /// transition from `prev` to `new`. Buckets whose key is unchanged are
    /// left untouched; emptied buckets are dropped so the indexes stay
    /// bounded by live entities, not visited space.
    fn reindex(&self, id: EntityId, prev: Option<&EntityState>, new: Option<&EntityState>) {
        let prev_cell = prev.map(|s| s.pos.block_pos());
        let new_cell = new.map(|s| s.pos.block_pos());

        let prev_chunk = prev_cell.map(|c| c.chunk());
        let new_chunk = new_cell.map(|c| c.chunk());
        if prev_chunk != new_chunk {
            if let Some(c) = prev_chunk {
                if let Some(mut set) = self.by_chunk.get_mut(&c) {
                    set.remove(&id);
                    if set.is_empty() {
                        drop(set);
                        self.by_chunk.remove_if(&c, |_, s| s.is_empty());
                    }
                }
            }
            if let Some(c) = new_chunk {
                self.by_chunk.entry(c).or_default().insert(id);
            }
        }

        let prev_col = prev_cell.map(|c| (c.x, c.z));
        let new_col = new_cell.map(|c| (c.x, c.z));
        if prev_col != new_col {
            if let Some(k) = prev_col {
                if let Some(mut set) = self.by_column.get_mut(&k) {
                    set.remove(&id);
                    if set.is_empty() {
                        drop(set);
                        self.by_column.remove_if(&k, |_, s| s.is_empty());
                    }
                }
            }
            if let Some(k) = new_col {
                self.by_column.entry(k).or_default().insert(id);
            }
        }
    }
}

impl Default for EntityStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(x: f64, y: f64, z: f64) -> EntityState {
        EntityState {
            kind: EntityKind(1),
            pos: Vec3::new(x, y, z),
            vel: Vec3::ZERO,
            stamp: 0,
            aux: 0,
        }
    }

    #[test]
    fn guarded_transitions_first_write_wins() {
        let store = EntityStore::new();
        let id = store.allocate_id();
        let s0 = state(8.5, 5.0, 8.5);

        // Spawn requires non-existence.
        assert!(store.set_entity(id, None, Some(&s0)));
        assert!(!store.set_entity(id, None, Some(&s0)), "double spawn must fail");

        // Move guards on the exact current state.
        let s1 = state(9.5, 5.0, 8.5);
        assert!(store.set_entity(id, Some(&s0), Some(&s1)));
        assert!(
            !store.set_entity(id, Some(&s0), None),
            "despawn guarded on a stale state must fail"
        );

        // Contested despawn: exactly one winner.
        assert!(store.set_entity(id, Some(&s1), None));
        assert!(!store.set_entity(id, Some(&s1), None));
        assert!(store.is_empty());
    }

    #[test]
    fn spatial_indexes_track_transitions() {
        let store = EntityStore::new();
        let id = store.allocate_id();
        let s0 = state(8.5, 5.0, 8.5);
        assert!(store.set_entity(id, None, Some(&s0)));
        assert_eq!(store.in_column(8, 8), vec![id]);
        assert_eq!(store.in_chunk(ChunkPos::new(0, 0)), vec![id]);

        // Cross a chunk border: both indexes follow.
        let s1 = state(20.5, 5.0, 20.5);
        assert!(store.set_entity(id, Some(&s0), Some(&s1)));
        assert!(store.in_column(8, 8).is_empty());
        assert_eq!(store.in_column(20, 20), vec![id]);
        assert!(store.in_chunk(ChunkPos::new(0, 0)).is_empty());
        assert_eq!(store.in_chunk(ChunkPos::new(1, 1)), vec![id]);

        // Despawn empties the indexes.
        assert!(store.set_entity(id, Some(&s1), None));
        assert!(store.in_column(20, 20).is_empty());
        assert!(store.in_chunk(ChunkPos::new(1, 1)).is_empty());
    }

    #[test]
    fn id_salt_namespaces_nodes() {
        let a = EntityStore::new();
        let b = EntityStore::new();
        a.set_id_salt(0);
        b.set_id_salt(1);
        let ia = a.allocate_id();
        let ib = b.allocate_id();
        assert_ne!(ia, ib, "same counter, different node salt");
        assert_eq!(ia.0 & ID_COUNTER_MASK, ib.0 & ID_COUNTER_MASK);
    }

    #[test]
    fn negative_coordinates_floor_correctly() {
        // -0.5 is in cell -1, not cell 0 (truncation would be wrong).
        assert_eq!(
            Vec3::new(-0.5, 5.0, -16.5).block_pos(),
            BlockPos::new(-1, 5, -17)
        );
    }

    #[test]
    fn unchecked_replica_writes_maintain_indexes() {
        let store = EntityStore::new();
        let id = EntityId(42);
        store.set_entity_unchecked(id, Some(&state(8.5, 5.0, 8.5)));
        assert_eq!(store.in_column(8, 8), vec![id]);
        // Replica apply overwrites regardless of current state.
        store.set_entity_unchecked(id, Some(&state(20.5, 5.0, 20.5)));
        assert_eq!(store.in_column(20, 20), vec![id]);
        store.set_entity_unchecked(id, None);
        assert!(store.is_empty());
        assert!(store.in_column(20, 20).is_empty());
    }
}
