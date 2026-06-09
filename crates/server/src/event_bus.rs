//! World-change event bus for cross-player and simulation-to-player distribution.
//!
//! Every action that modifies the world (player block break/place, ambient simulation)
//! publishes a [`WorldChangeBatch`] to a shared `tokio::sync::broadcast` channel.
//! Each connection subscribes and forwards changes to its client -- except changes
//! it originated itself.

use std::sync::Arc;

use ultimate_engine::causal::event::{EventPayload, LightType};
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::position::BlockPos;

/// Recommended capacity for the broadcast channel.
///
/// Batches are `Arc`-backed (a slot is ~100 bytes), so a deep buffer is
/// nearly free. 256 lagged visibly at 100 wandering+digging players once
/// physics moved to per-step publishing (many small batches): 1,909
/// dropped-batch warnings in a 30 s load test. 8192 absorbs that burst
/// profile with megabytes, not gigabytes, of worst-case buffer.
pub const BUS_CAPACITY: usize = 8192;

/// Identifies where a batch of world changes originated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeSource {
    /// A specific player connection (identified by connection ID).
    Player(u64),
    /// An ambient simulation layer.
    Simulation(&'static str),
    /// The shared physics service (Phase 6b-1). Physics batches go to
    /// every client — including the player whose action caused them; the
    /// connection no longer sends block updates directly.
    Physics,
}

/// A single light change: position, light type, new value.
#[derive(Clone, Debug)]
pub struct LightChange {
    pub pos: BlockPos,
    pub light_type: LightType,
    pub new: u8,
}

/// A batch of block changes from a single cascade.
///
/// Uses `Arc<[...]>` so cloning per broadcast subscriber is just a refcount bump.
#[derive(Clone, Debug)]
pub struct WorldChangeBatch {
    pub source: ChangeSource,
    pub changes: Arc<[(BlockPos, BlockId)]>,
    pub light_changes: Arc<[LightChange]>,
}

// ── Spatial pub/sub (Phase 6f: the 10k-player delivery plane) ───────────────

/// A region key: 4×4 chunks, consistent with physics partitioning and
/// cluster ownership (`block >> 6` == `chunk >> 2`).
pub type Region = (i32, i32);

/// Region of a block position.
pub fn region_of_block(x: i64, z: i64) -> Region {
    ((x >> 6) as i32, (z >> 6) as i32)
}

/// A spatially-routed message.
#[derive(Debug)]
pub enum SpatialMsg {
    /// World changes whose positions all fall in the bucket's region.
    World(WorldChangeBatch),
    /// A player movement (always `PlayerEvent::Moved`).
    Move(crate::player_registry::PlayerEvent),
}

/// Region-bucketed pub/sub: publishers deliver to the subscribers of the
/// event's region only, making delivery O(nearby connections) instead of
/// O(all connections). This is what the 6e/6f load tests showed the
/// broadcast firehose can't do: at 10k players, a move must not touch
/// 10k queues.
///
/// Join/leave/chat remain on the global broadcast channel — the tab list
/// is global and those events are rare.
pub struct SpatialBus {
    buckets: dashmap::DashMap<Region, std::collections::HashMap<u64, Tx>>,
    next_sub: std::sync::atomic::AtomicU64,
}

type Tx = tokio::sync::mpsc::UnboundedSender<Arc<SpatialMsg>>;

impl SpatialBus {
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            buckets: dashmap::DashMap::new(),
            next_sub: std::sync::atomic::AtomicU64::new(1),
        })
    }

    /// Create a subscriber. It starts with no regions; call
    /// [`SpatialSubscriber::set_view`] to subscribe an area.
    pub fn subscribe(
        self: &Arc<Self>,
    ) -> (SpatialSubscriber, tokio::sync::mpsc::UnboundedReceiver<Arc<SpatialMsg>>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let id = self
            .next_sub
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        (
            SpatialSubscriber {
                id,
                bus: Arc::clone(self),
                regions: std::collections::HashSet::new(),
                tx,
            },
            rx,
        )
    }

    fn deliver(&self, region: Region, msg: &Arc<SpatialMsg>) {
        let Some(mut bucket) = self.buckets.get_mut(&region) else {
            return;
        };
        // Lazily reap subscribers whose receiver died without Drop
        // (aborted task).
        bucket.retain(|_, tx| tx.send(Arc::clone(msg)).is_ok());
    }

    /// Publish a set of world changes, split per region so each bucket's
    /// subscribers receive only what's near them.
    pub fn publish_world(
        &self,
        source: ChangeSource,
        changes: Vec<(BlockPos, BlockId)>,
        light_changes: Vec<LightChange>,
    ) {
        if changes.is_empty() && light_changes.is_empty() {
            return;
        }
        let mut per_region: std::collections::HashMap<
            Region,
            (Vec<(BlockPos, BlockId)>, Vec<LightChange>),
        > = std::collections::HashMap::new();
        for (pos, block) in changes {
            per_region
                .entry(region_of_block(pos.x, pos.z))
                .or_default()
                .0
                .push((pos, block));
        }
        for lc in light_changes {
            per_region
                .entry(region_of_block(lc.pos.x, lc.pos.z))
                .or_default()
                .1
                .push(lc);
        }
        for (region, (changes, light_changes)) in per_region {
            let msg = Arc::new(SpatialMsg::World(WorldChangeBatch {
                source: source.clone(),
                changes: changes.into(),
                light_changes: light_changes.into(),
            }));
            self.deliver(region, &msg);
        }
    }

    /// Publish a player movement to its region's subscribers.
    pub fn publish_move(&self, event: crate::player_registry::PlayerEvent) {
        let crate::player_registry::PlayerEvent::Moved { x, z, .. } = &event else {
            debug_assert!(false, "publish_move expects PlayerEvent::Moved");
            return;
        };
        let region = region_of_block(*x as i64, *z as i64);
        let msg = Arc::new(SpatialMsg::Move(event));
        self.deliver(region, &msg);
    }
}

/// A connection's spatial subscription. Re-point it with
/// [`set_view`](Self::set_view) when the player crosses chunk borders;
/// dropping it unsubscribes everywhere.
pub struct SpatialSubscriber {
    id: u64,
    bus: Arc<SpatialBus>,
    regions: std::collections::HashSet<Region>,
    tx: Tx,
}

impl SpatialSubscriber {
    /// Subscribe to every region intersecting the view box around the
    /// given center chunk (`view_distance` + 2 chunks of margin), and
    /// unsubscribe from regions that left it. Cheap: region sets are
    /// small (~dozens) and only diffs touch the bucket maps.
    pub fn set_view(&mut self, center_cx: i32, center_cz: i32, view_distance: i32) {
        let margin = view_distance + 2;
        let (rx0, rx1) = ((center_cx - margin) >> 2, (center_cx + margin) >> 2);
        let (rz0, rz1) = ((center_cz - margin) >> 2, (center_cz + margin) >> 2);
        let mut wanted = std::collections::HashSet::new();
        for rx in rx0..=rx1 {
            for rz in rz0..=rz1 {
                wanted.insert((rx, rz));
            }
        }

        for region in self.regions.difference(&wanted) {
            if let Some(mut bucket) = self.bus.buckets.get_mut(region) {
                bucket.remove(&self.id);
            }
        }
        for region in wanted.difference(&self.regions) {
            self.bus
                .buckets
                .entry(*region)
                .or_default()
                .insert(self.id, self.tx.clone());
        }
        self.regions = wanted;
    }
}

impl Drop for SpatialSubscriber {
    fn drop(&mut self) {
        for region in &self.regions {
            if let Some(mut bucket) = self.bus.buckets.get_mut(region) {
                bucket.remove(&self.id);
            }
        }
    }
}

/// Extract all `BlockSet` writes from an execution-ordered write log
/// (see `CausalGraph::write_log` / `take_write_log`) as
/// `(position, new_block)` pairs suitable for broadcasting.
///
/// The log order matches actual execution: a cell written twice in one
/// cascade reports its final value last, and the log survives pruning.
pub fn collect_block_changes(write_log: &[EventPayload]) -> Vec<(BlockPos, BlockId)> {
    write_log
        .iter()
        .filter_map(|payload| match payload {
            EventPayload::BlockSet { pos, new, .. } => Some((*pos, *new)),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod spatial_tests {
    use super::*;
    use ultimate_engine::world::block::BlockId;

    fn moved_at(x: f64, z: f64) -> crate::player_registry::PlayerEvent {
        crate::player_registry::PlayerEvent::Moved {
            conn_id: 1,
            entity_id: 7,
            x,
            y: 80.0,
            z,
            y_rot: 0.0,
            x_rot: 0.0,
            on_ground: true,
        }
    }

    #[test]
    fn delivery_is_region_scoped() {
        let bus = SpatialBus::new();
        let (mut sub, mut rx) = bus.subscribe();
        sub.set_view(0, 0, 4); // chunks ±6 → regions ±1 around origin

        // In view: block at (10, z=10) → region (0,0).
        bus.publish_world(
            ChangeSource::Physics,
            vec![(BlockPos::new(10, 5, 10), BlockId::new(1))],
            vec![],
        );
        assert!(matches!(*rx.try_recv().expect("in-view delivery"), SpatialMsg::World(_)));

        // Far away: region (20, 20) — not subscribed.
        bus.publish_world(
            ChangeSource::Physics,
            vec![(BlockPos::new(20 * 64 + 5, 5, 20 * 64 + 5), BlockId::new(1))],
            vec![],
        );
        assert!(rx.try_recv().is_err(), "far event must not be delivered");

        // Moves likewise.
        bus.publish_move(moved_at(12.0, 12.0));
        assert!(matches!(*rx.try_recv().expect("near move"), SpatialMsg::Move(_)));
        bus.publish_move(moved_at(5000.0, 5000.0));
        assert!(rx.try_recv().is_err(), "far move must not be delivered");
    }

    #[test]
    fn batches_split_per_region_and_views_rediff() {
        let bus = SpatialBus::new();
        let (mut sub, mut rx) = bus.subscribe();
        sub.set_view(0, 0, 4);

        // One publish spanning two regions: only the near part arrives.
        bus.publish_world(
            ChangeSource::Physics,
            vec![
                (BlockPos::new(1, 5, 1), BlockId::new(1)),
                (BlockPos::new(50 * 64, 5, 50 * 64), BlockId::new(2)),
            ],
            vec![],
        );
        let msg = rx.try_recv().expect("near slice");
        match &*msg {
            SpatialMsg::World(b) => assert_eq!(b.changes.len(), 1, "only the near change"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(rx.try_recv().is_err());

        // Move the view far away: old region stops delivering, new starts.
        sub.set_view(50 * 4, 50 * 4, 4);
        bus.publish_world(
            ChangeSource::Physics,
            vec![(BlockPos::new(1, 5, 1), BlockId::new(1))],
            vec![],
        );
        assert!(rx.try_recv().is_err(), "old area unsubscribed");
        bus.publish_world(
            ChangeSource::Physics,
            vec![(BlockPos::new(50 * 64 + 3, 5, 50 * 64 + 3), BlockId::new(1))],
            vec![],
        );
        assert!(rx.try_recv().is_ok(), "new area subscribed");
    }

    #[test]
    fn drop_unsubscribes() {
        let bus = SpatialBus::new();
        let (mut sub, rx) = bus.subscribe();
        sub.set_view(0, 0, 4);
        drop(sub);
        drop(rx);
        bus.publish_world(
            ChangeSource::Physics,
            vec![(BlockPos::new(1, 5, 1), BlockId::new(1))],
            vec![],
        );
        // No panic / no leak: bucket entries were removed on Drop.
        let total: usize = bus.buckets.iter().map(|b| b.len()).sum();
        assert_eq!(total, 0);
    }
}

/// Extract all light writes (`LightSet` and `LightBatch` cells) from an
/// execution-ordered write log.
pub fn collect_light_changes(write_log: &[EventPayload]) -> Vec<LightChange> {
    let mut out = Vec::new();
    for payload in write_log {
        match payload {
            EventPayload::LightSet { pos, light_type, new, .. } => out.push(LightChange {
                pos: *pos,
                light_type: *light_type,
                new: *new,
            }),
            EventPayload::LightBatch { changes } => {
                out.extend(changes.iter().map(|c| LightChange {
                    pos: c.pos,
                    light_type: c.light_type,
                    new: c.new,
                }));
            }
            _ => {}
        }
    }
    out
}
