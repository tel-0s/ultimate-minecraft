//! Phases 6b/6d: the partitioned, adaptive physics service.
//!
//! N worker threads each own a disjoint set of **regions** (4×4-chunk
//! squares): a region's events execute only on its owner's thread, in its
//! owner's private causal graph — no locks, no shared mutable graph
//! state. Every event source (player connections, ambient simulation)
//! submits root events through a [`PhysicsHandle`], which routes them to
//! owners by chunk.
//!
//! ## The partition boundary is a message
//!
//! When a cascade's consequent targets a chunk owned by another worker,
//! it is **forwarded** over that worker's channel instead of being
//! inserted locally ([`Scheduler::step_routed`]). Consequents are
//! generated after their cause executed, and the cause's world write is
//! published by the channel send (release/acquire), so the happens-before
//! edge rides the transport. This same protocol — serialize an event,
//! send it to the owner of its position — is what 6f later runs over a
//! network socket between machines.
//!
//! ## Priority (Phase 6d)
//!
//! Player actions enter at priority 1; background physics at 0. The
//! graph's two-lane ready queue drains priority work first among
//! spacelike-separated events, forwarded events keep their lane, and
//! workers **publish changes after every step**, so a player's block
//! break reaches clients while a million-event background flood is still
//! cascading.
//!
//! ## Adaptive load balancing (Phase 6d)
//!
//! Region→worker assignment starts as a deterministic hash but is a
//! *table*, not a function: a rebalancer thread meters per-region write
//! throughput and (with hysteresis) **moves** hot regions from the
//! busiest worker to the idlest, or **splits** a region that dominates
//! total load into per-chunk ownership across all workers. Workers
//! refresh their assignment snapshot every loop iteration; during a
//! handoff both the old and new owner may briefly execute events for the
//! same region. That transient dual-ownership is the same class of race
//! as a partition boundary and is tolerated by the same machinery: the
//! stale-precondition guard plus confluent, self-stabilizing rules.
//!
//! ## Quiescence
//!
//! `pending` counts in-flight messages: +1 on every send (submission or
//! forward), −k when the batch that consumed k messages reaches local
//! quiescence. Forwards are counted before their consuming batch
//! decrements, so `pending() == 0` implies global quiescence.
//!
//! ## Known ownership exceptions (deliberate, documented)
//!
//! - The light rule's BFS writes light values directly across chunk
//!   borders (races converge; partition-aware light is future work).
//! - Stair-shape rewrites touch radius-1 neighbours directly.
//! - Rebalancing handoffs (above).

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, RwLock, Weak};
use std::time::{Duration, Instant};

use dashmap::DashMap;

use ultimate_engine::causal::event::{Event, EventPayload};
use ultimate_engine::causal::graph::CausalGraph;
use ultimate_engine::causal::scheduler::Scheduler;
use ultimate_engine::rules::RuleSet;
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::position::{BlockPos, ChunkPos};
use ultimate_engine::world::World;

use crate::dashboard::DashboardState;
use crate::event_bus::{self, ChangeSource, SpatialBus};

/// Regions are 2^REGION_BITS × 2^REGION_BITS chunks.
const REGION_BITS: i32 = 2;

/// Priority lane for player-initiated actions.
const PRIO_PLAYER: u8 = 1;

// ── Rebalancer tuning ───────────────────────────────────────────────────────

/// How often the rebalancer examines per-region load.
const REBALANCE_INTERVAL: Duration = Duration::from_millis(50);
/// Ignore ticks with less total work than this (events/tick) — idle noise.
const REBALANCE_MIN_EVENTS: u64 = 4_000;
/// Move a region when its worker carries > this multiple of the idlest.
const MOVE_IMBALANCE_RATIO: f64 = 2.0;
/// Split a region into per-chunk ownership when it alone carries more
/// than this share of total load.
const SPLIT_SHARE: f64 = 0.5;
/// Revert a split when the region's share drops below this.
const UNSPLIT_SHARE: f64 = 0.05;
/// Leave a region alone for this long after changing its assignment.
const REGION_COOLDOWN: Duration = Duration::from_millis(500);

/// Options for [`start`]. `..Default::default()` gives production
/// behaviour: auto worker count, no pinning, rebalancing on, single node.
#[derive(Clone)]
pub struct PhysicsOptions {
    /// Partition worker threads. 0 = auto (logical cores, capped at 8).
    pub workers: usize,
    /// Pin worker threads to distinct CPU cores (NUMA prep).
    pub pin_workers: bool,
    /// Enable the adaptive rebalancer.
    pub rebalance: bool,
    /// Phase 6f: multi-node clustering. Regions whose
    /// [`cluster::owner_node`](crate::cluster::owner_node) isn't this
    /// node route over the peer link instead of to local workers.
    pub cluster: Option<ClusterCtx>,
}

/// Cluster membership for this physics service: the full N-node mesh.
#[derive(Clone)]
pub struct ClusterCtx {
    pub mesh: Arc<crate::cluster::ClusterMesh>,
}

impl Default for PhysicsOptions {
    fn default() -> Self {
        Self { workers: 0, pin_workers: false, rebalance: true, cluster: None }
    }
}

/// A player block action: one `BlockSet` root plus the 6-neighbour notify
/// fan-out, kept as children of the set in the owner's graph so they
/// re-evaluate the world strictly after the set applies.
#[derive(Debug, Clone, Copy)]
pub struct BlockAction {
    pub pos: BlockPos,
    /// World state the actor observed; the scheduler's stale-precondition
    /// guard skips the write if the cell changed since.
    pub old: BlockId,
    pub new: BlockId,
    /// Recompute adjacent stair shapes after the cascade settles.
    pub update_stairs: bool,
}

enum WorkerMsg {
    Action(BlockAction),
    /// Externally-submitted root events (simulation layers, priority 0).
    Events(Vec<Event>),
    /// Cross-partition consequents with their inherited priorities.
    /// Inserted as roots: the causal parents live (executed) in the
    /// sender's graph; the channel carries the happens-before edge.
    Forward(Vec<(Event, u8)>),
}

// ── Region assignment ───────────────────────────────────────────────────────

type Region = (i32, i32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionAssign {
    /// All chunks of the region execute on this worker.
    Worker(usize),
    /// Hot region: chunks distribute individually across all workers.
    SplitByChunk,
}

/// The region→worker table. The default (no entry) is the deterministic
/// hash; overrides are installed by the rebalancer. Readers grab an `Arc`
/// snapshot once per loop iteration, so routing is lock-free on the hot
/// path and assignment changes propagate within one iteration.
struct Assignment {
    overrides: RwLock<Arc<HashMap<Region, RegionAssign>>>,
}

impl Assignment {
    fn new() -> Self {
        Self { overrides: RwLock::new(Arc::new(HashMap::new())) }
    }

    fn snapshot(&self) -> Arc<HashMap<Region, RegionAssign>> {
        self.overrides.read().expect("assignment lock").clone()
    }

    fn install(&self, table: HashMap<Region, RegionAssign>) {
        *self.overrides.write().expect("assignment lock") = Arc::new(table);
    }
}

fn region_of(chunk: ChunkPos) -> Region {
    (chunk.x >> REGION_BITS, chunk.z >> REGION_BITS)
}

/// SplitMix64-style mix used for both region- and chunk-granular hashing.
fn mix(a: i64, b: i64) -> u64 {
    let mut h = ((a as u64) << 32) ^ ((b as u64) & 0xFFFF_FFFF);
    h = h.wrapping_add(0x9E3779B97F4A7C15);
    h = (h ^ (h >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94D049BB133111EB);
    h ^ (h >> 31)
}

/// Default (hash) owner of a region.
fn default_owner(region: Region, workers: usize) -> usize {
    (mix(region.0 as i64, region.1 as i64) % workers as u64) as usize
}

/// Owner of a chunk under an assignment snapshot.
fn owner_of(chunk: ChunkPos, table: &HashMap<Region, RegionAssign>, workers: usize) -> usize {
    let region = region_of(chunk);
    match table.get(&region) {
        Some(RegionAssign::Worker(w)) => *w % workers,
        Some(RegionAssign::SplitByChunk) => {
            (mix(chunk.x as i64, chunk.z as i64) % workers as u64) as usize
        }
        None => default_owner(region, workers),
    }
}

// ── Handle ──────────────────────────────────────────────────────────────────

/// Cloneable submission handle. Sends never block (unbounded channels).
#[derive(Clone)]
pub struct PhysicsHandle {
    txs: Vec<mpsc::Sender<WorkerMsg>>,
    assignment: Arc<Assignment>,
    pending: Arc<AtomicI64>,
    executed: Arc<AtomicU64>,
    cluster: Option<ClusterCtx>,
}

impl PhysicsHandle {
    fn send(&self, worker: usize, msg: WorkerMsg) {
        self.pending.fetch_add(1, Ordering::SeqCst);
        if self.txs[worker].send(msg).is_err() {
            self.pending.fetch_sub(1, Ordering::SeqCst);
            tracing::error!("physics worker {} is down; dropping submission", worker);
        }
    }

    /// Which cluster node owns this chunk, if it isn't us.
    fn foreign_node(&self, chunk: ChunkPos) -> Option<u32> {
        let c = self.cluster.as_ref()?;
        let owner = c.mesh.owner(chunk);
        (owner != c.mesh.node_id).then_some(owner)
    }

    pub fn submit_action(&self, action: BlockAction) {
        if let Some(node) = self.foreign_node(action.pos.chunk()) {
            // The owning node ingests it (fan-out, priority, stair hook)
            // and mirrors the results back via WriteSync.
            self.cluster.as_ref().unwrap().mesh.send_action(node, action);
            return;
        }
        self.submit_action_local(action);
    }

    /// Submit an action to LOCAL workers without the node-ownership check
    /// (entry point for actions arriving over the cluster link).
    pub fn submit_action_local(&self, action: BlockAction) {
        let table = self.assignment.snapshot();
        let worker = owner_of(action.pos.chunk(), &table, self.txs.len());
        self.send(worker, WorkerMsg::Action(action));
    }

    /// Submit raw root events; each is routed to the owner of its chunk —
    /// a local worker, or the peer node over the cluster link.
    pub fn submit_events(&self, events: Vec<Event>) {
        if events.is_empty() {
            return;
        }
        let table = self.assignment.snapshot();
        let workers = self.txs.len();
        let mut per_worker: Vec<Vec<Event>> = vec![Vec::new(); workers];
        let mut per_node: HashMap<u32, Vec<(Event, u8)>> = HashMap::new();
        for event in events {
            if let Some(node) = self.foreign_node(event.chunk()) {
                per_node.entry(node).or_default().push((event, 0));
            } else {
                per_worker[owner_of(event.chunk(), &table, workers)].push(event);
            }
        }
        for (worker, batch) in per_worker.into_iter().enumerate() {
            if !batch.is_empty() {
                self.send(worker, WorkerMsg::Events(batch));
            }
        }
        for (node, batch) in per_node {
            self.cluster.as_ref().unwrap().mesh.send_forward(node, batch);
        }
    }

    /// Inject forwarded events from the peer node, preserving their
    /// priorities. Routed by LOCAL ownership only (the sender already
    /// decided this node owns them).
    pub fn submit_forwards(&self, items: Vec<(Event, u8)>) {
        if items.is_empty() {
            return;
        }
        let table = self.assignment.snapshot();
        let workers = self.txs.len();
        let mut per_worker: Vec<Vec<(Event, u8)>> = vec![Vec::new(); workers];
        for (event, prio) in items {
            per_worker[owner_of(event.chunk(), &table, workers)].push((event, prio));
        }
        for (worker, batch) in per_worker.into_iter().enumerate() {
            if !batch.is_empty() {
                self.send(worker, WorkerMsg::Forward(batch));
            }
        }
    }

    /// In-flight message count; 0 means globally quiescent.
    pub fn pending(&self) -> i64 {
        self.pending.load(Ordering::SeqCst)
    }

    /// Total events executed across all workers since startup.
    pub fn executed_total(&self) -> u64 {
        self.executed.load(Ordering::Relaxed)
    }

    pub fn workers(&self) -> usize {
        self.txs.len()
    }
}

// ── Service startup ─────────────────────────────────────────────────────────

/// Start the physics service. Workers exit when every handle is dropped;
/// the rebalancer exits with them.
pub fn start(
    world: Arc<World>,
    rules_factory: fn() -> RuleSet,
    bus: Arc<SpatialBus>,
    dashboard: Option<Arc<DashboardState>>,
    opts: PhysicsOptions,
) -> PhysicsHandle {
    let workers = if opts.workers == 0 {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8)
    } else {
        opts.workers
    };

    let assignment = Arc::new(Assignment::new());
    let region_loads: Arc<DashMap<Region, u64>> = Arc::new(DashMap::new());
    let pending = Arc::new(AtomicI64::new(0));
    let executed = Arc::new(AtomicU64::new(0));

    let mut txs = Vec::with_capacity(workers);
    let mut rxs = Vec::with_capacity(workers);
    for _ in 0..workers {
        let (tx, rx) = mpsc::channel::<WorkerMsg>();
        txs.push(tx);
        rxs.push(rx);
    }

    let core_ids = if opts.pin_workers {
        core_affinity::get_core_ids().unwrap_or_default()
    } else {
        Vec::new()
    };

    for (id, rx) in rxs.into_iter().enumerate() {
        let ctx = WorkerCtx {
            id,
            world: Arc::clone(&world),
            rules: rules_factory(),
            peers: txs.clone(),
            assignment: Arc::clone(&assignment),
            region_loads: Arc::clone(&region_loads),
            bus: Arc::clone(&bus),
            dashboard: dashboard.clone(),
            pending: Arc::clone(&pending),
            executed: Arc::clone(&executed),
            cluster: opts.cluster.clone(),
        };
        let pin = if core_ids.is_empty() { None } else { Some(core_ids[id % core_ids.len()]) };
        std::thread::Builder::new()
            .name(format!("physics-{id}"))
            .spawn(move || {
                if let Some(core) = pin {
                    if core_affinity::set_for_current(core) {
                        tracing::debug!("physics-{id} pinned to core {:?}", core.id);
                    }
                }
                worker_loop(ctx, rx)
            })
            .expect("spawning physics worker");
    }

    if opts.rebalance && workers > 1 {
        let weak_assignment = Arc::downgrade(&assignment);
        let weak_loads = Arc::downgrade(&region_loads);
        std::thread::Builder::new()
            .name("physics-rebalancer".into())
            .spawn(move || rebalancer_loop(weak_assignment, weak_loads, workers))
            .expect("spawning rebalancer");
    }

    tracing::info!(
        "Physics service started: {} partition workers, {}x{}-chunk regions, \
         rebalance={}, pinned={}, node={}",
        workers, 1 << REGION_BITS, 1 << REGION_BITS, opts.rebalance, opts.pin_workers,
        opts.cluster.as_ref().map(|c| format!("{}/{}", c.mesh.node_id, c.mesh.total_nodes))
            .unwrap_or_else(|| "single".into()),
    );
    PhysicsHandle { txs, assignment, pending, executed, cluster: opts.cluster }
}

// ── Worker ──────────────────────────────────────────────────────────────────

struct WorkerCtx {
    id: usize,
    world: Arc<World>,
    rules: RuleSet,
    peers: Vec<mpsc::Sender<WorkerMsg>>,
    assignment: Arc<Assignment>,
    region_loads: Arc<DashMap<Region, u64>>,
    bus: Arc<SpatialBus>,
    dashboard: Option<Arc<DashboardState>>,
    pending: Arc<AtomicI64>,
    executed: Arc<AtomicU64>,
    cluster: Option<ClusterCtx>,
}

fn worker_loop(ctx: WorkerCtx, rx: mpsc::Receiver<WorkerMsg>) {
    let mut graph = CausalGraph::with_pruning();
    let scheduler = Scheduler::new();
    let workers = ctx.peers.len();
    let mut outbox: Vec<(usize, Event, u8)> = Vec::new();
    // Consequents owned by peer NODES (6f): shipped over the mesh after
    // each step, same post-execution ordering as local forwards —
    // happens-before rides the socket.
    let mut remote_outbox: Vec<(u32, Event, u8)> = Vec::new();

    while let Ok(first) = rx.recv() {
        let mut consumed: i64 = 0;
        let mut stair_hooks: Vec<BlockPos> = Vec::new();
        let executed_before = graph.executed_total();
        let started = Instant::now();

        ingest(&mut graph, first, &mut stair_hooks);
        consumed += 1;

        // Run to local quiescence: drain the inbox between steps, refresh
        // the assignment snapshot, and PUBLISH AFTER EVERY STEP so player
        // cascades reach clients while long background cascades continue.
        loop {
            while let Ok(msg) = rx.try_recv() {
                ingest(&mut graph, msg, &mut stair_hooks);
                consumed += 1;
            }

            let table = ctx.assignment.snapshot();
            let cluster = &ctx.cluster;
            let n = scheduler.step_routed(&ctx.world, &mut graph, &ctx.rules, &mut |event, prio| {
                if let Some(c) = cluster {
                    let node = c.mesh.owner(event.chunk());
                    if node != c.mesh.node_id {
                        remote_outbox.push((node, event.clone(), prio));
                        return false;
                    }
                }
                let target = owner_of(event.chunk(), &table, workers);
                if target == ctx.id {
                    true
                } else {
                    outbox.push((target, event.clone(), prio));
                    false
                }
            });

            // Flush routed consequents, grouped per target worker. The +1
            // happens before our batch's decrement, so the pending counter
            // can't reach zero while these are in flight.
            if !outbox.is_empty() {
                let mut per_worker: Vec<Vec<(Event, u8)>> = vec![Vec::new(); workers];
                for (target, event, prio) in outbox.drain(..) {
                    per_worker[target].push((event, prio));
                }
                for (target, batch) in per_worker.into_iter().enumerate() {
                    if !batch.is_empty() {
                        ctx.pending.fetch_add(1, Ordering::SeqCst);
                        if ctx.peers[target].send(WorkerMsg::Forward(batch)).is_err() {
                            ctx.pending.fetch_sub(1, Ordering::SeqCst);
                        }
                    }
                }
            }

            // Per-step publish FIRST: it also mirrors this step's writes
            // to the peer (WriteSync). Socket FIFO then guarantees the
            // peer's replica contains a forwarded event's causal
            // prerequisites before the Forward below arrives — the
            // cross-node analogue of "consequents ship after their cause's
            // write is visible".
            publish_writes(&ctx, &mut graph, &mut Vec::new());

            // Ship cross-NODE consequents (their causes executed above),
            // grouped per destination node.
            if !remote_outbox.is_empty() {
                if let Some(c) = &ctx.cluster {
                    let mut per_node: HashMap<u32, Vec<(Event, u8)>> = HashMap::new();
                    for (node, event, prio) in remote_outbox.drain(..) {
                        per_node.entry(node).or_default().push((event, prio));
                    }
                    for (node, batch) in per_node {
                        c.mesh.send_forward(node, batch);
                    }
                }
            }

            if n == 0 {
                match rx.try_recv() {
                    Ok(msg) => {
                        ingest(&mut graph, msg, &mut stair_hooks);
                        consumed += 1;
                    }
                    Err(_) => break,
                }
            }
        }

        // Post-batch: stair rewrites (read the settled world), final publish.
        let mut extra_changes: Vec<(BlockPos, BlockId)> = Vec::new();
        for pos in stair_hooks {
            for (npos, new_id) in crate::placement::update_adjacent_stair_shapes(&ctx.world, pos) {
                ctx.world.set_block(npos, new_id);
                extra_changes.push((npos, new_id));
            }
        }
        publish_writes(&ctx, &mut graph, &mut extra_changes);

        let executed_delta = graph.executed_total() - executed_before;
        let elapsed = started.elapsed();
        ctx.executed.fetch_add(executed_delta, Ordering::Relaxed);

        if let Some(dash) = &ctx.dashboard {
            dash.metrics.record_cascade(executed_delta, elapsed);
            dash.publish_graph(crate::dashboard::snapshot_graph(&graph));
        }
        if executed_delta > 0 {
            tracing::debug!(
                "physics-{}: {} events in {:?}",
                ctx.id, executed_delta, elapsed,
            );
        }

        ctx.pending.fetch_sub(consumed, Ordering::SeqCst);
    }

    tracing::info!("physics-{} stopped (all handles dropped)", ctx.id);
}

/// Drain the graph's write log; publish it (plus any `extra` block
/// changes) to the bus and attribute the writes to their regions for the
/// rebalancer's load metering.
fn publish_writes(ctx: &WorkerCtx, graph: &mut CausalGraph, extra: &mut Vec<(BlockPos, BlockId)>) {
    let log = graph.take_write_log();
    if log.is_empty() && extra.is_empty() {
        return;
    }

    // Region load attribution (writes are a good proxy for work).
    let mut local_counts: HashMap<Region, u64> = HashMap::new();
    for payload in &log {
        match payload {
            EventPayload::BlockSet { pos, .. } | EventPayload::LightSet { pos, .. } => {
                *local_counts.entry(region_of(pos.chunk())).or_default() += 1;
            }
            EventPayload::LightBatch { changes } => {
                for c in changes.iter() {
                    *local_counts.entry(region_of(c.pos.chunk())).or_default() += 1;
                }
            }
            _ => {}
        }
    }
    for (region, count) in local_counts {
        *ctx.region_loads.entry(region).or_default() += count;
    }

    let mut changes = event_bus::collect_block_changes(&log);
    let light_changes = event_bus::collect_light_changes(&log);
    let extra_payloads: Vec<EventPayload> = extra
        .iter()
        .map(|&(pos, new)| EventPayload::BlockSet { pos, old: new, new })
        .collect();
    changes.append(extra);

    // Spatial delivery (6f): each change reaches only the connections
    // subscribed near it — O(nearby players), not O(all players).
    ctx.bus.publish_world(ChangeSource::Physics, changes, light_changes);

    // 6f: mirror this node's executed writes to every peer so their
    // replica worlds (and their connected clients) see physics computed
    // here.
    if let Some(c) = &ctx.cluster {
        let mut sync = log;
        sync.extend(extra_payloads);
        c.mesh.broadcast_write_sync(sync);
    }
}

fn ingest(graph: &mut CausalGraph, msg: WorkerMsg, stair_hooks: &mut Vec<BlockPos>) {
    match msg {
        WorkerMsg::Action(a) => {
            // Player actions ride the priority lane; the notify fan-out
            // and the whole cascade inherit it.
            let root = graph.insert_root_with_priority(
                Event { payload: EventPayload::BlockSet { pos: a.pos, old: a.old, new: a.new } },
                PRIO_PLAYER,
            );
            for neighbor in a.pos.neighbors() {
                graph.insert(
                    Event { payload: EventPayload::BlockNotify { pos: neighbor } },
                    vec![root],
                );
            }
            if a.update_stairs {
                stair_hooks.push(a.pos);
            }
        }
        WorkerMsg::Events(events) => {
            for event in events {
                graph.insert_root(event);
            }
        }
        WorkerMsg::Forward(events) => {
            for (event, prio) in events {
                graph.insert_root_with_priority(event, prio);
            }
        }
    }
}

// ── Rebalancer ──────────────────────────────────────────────────────────────

/// Periodically: read per-region load since the last tick, then apply at
/// most one assignment change (split a dominating region, revert a cooled
/// split, or move a hot region from the busiest worker to the idlest).
/// One change per tick + per-region cooldown = hysteresis against thrash.
fn rebalancer_loop(
    assignment: Weak<Assignment>,
    region_loads: Weak<DashMap<Region, u64>>,
    workers: usize,
) {
    let mut last_change: HashMap<Region, Instant> = HashMap::new();

    loop {
        std::thread::sleep(REBALANCE_INTERVAL);
        let (Some(assignment), Some(loads_map)) = (assignment.upgrade(), region_loads.upgrade())
        else {
            return; // service shut down
        };

        // Drain this tick's load counters.
        let mut loads: Vec<(Region, u64)> = Vec::new();
        for entry in loads_map.iter() {
            loads.push((*entry.key(), *entry.value()));
        }
        loads_map.clear();

        let total: u64 = loads.iter().map(|(_, c)| c).sum();
        if total < REBALANCE_MIN_EVENTS {
            continue;
        }
        loads.sort_by_key(|&(_, c)| std::cmp::Reverse(c));

        let table = assignment.snapshot();
        let now = Instant::now();
        let cooled = |r: &Region| {
            last_change.get(r).is_none_or(|t| now.duration_since(*t) >= REGION_COOLDOWN)
        };

        // 1. Split a region that dominates total load.
        let (hottest, hottest_load) = loads[0];
        let already_split = table.get(&hottest) == Some(&RegionAssign::SplitByChunk);
        if !already_split
            && hottest_load as f64 > total as f64 * SPLIT_SHARE
            && cooled(&hottest)
        {
            let mut next = (*table).clone();
            next.insert(hottest, RegionAssign::SplitByChunk);
            assignment.install(next);
            last_change.insert(hottest, now);
            tracing::info!(
                "rebalancer: split region {:?} ({}/{} writes this tick) across all workers",
                hottest, hottest_load, total,
            );
            continue;
        }

        // 2. Revert splits that have cooled off.
        if let Some((&region, _)) = table
            .iter()
            .find(|(r, a)| {
                **a == RegionAssign::SplitByChunk
                    && cooled(r)
                    && (loads.iter().find(|(lr, _)| lr == *r).map_or(0, |(_, c)| *c) as f64)
                        < total as f64 * UNSPLIT_SHARE
            })
        {
            let mut next = (*table).clone();
            next.remove(&region);
            assignment.install(next);
            last_change.insert(region, now);
            tracing::info!("rebalancer: un-split quiet region {:?}", region);
            continue;
        }

        // 3. Move a region from the busiest worker to the idlest.
        let mut worker_load = vec![0u64; workers];
        for &(region, count) in &loads {
            worker_load[owner_of(
                ChunkPos::new(region.0 << REGION_BITS, region.1 << REGION_BITS),
                &table,
                workers,
            )] += count;
        }
        let busiest = (0..workers).max_by_key(|&w| worker_load[w]).unwrap_or(0);
        let idlest = (0..workers).min_by_key(|&w| worker_load[w]).unwrap_or(0);
        if worker_load[busiest] as f64 > worker_load[idlest] as f64 * MOVE_IMBALANCE_RATIO
            && worker_load[busiest] > 0
        {
            // Move the busiest worker's SECOND-hottest region (moving the
            // hottest just relocates the problem; moving the second evens
            // the load), or the hottest if it owns only one loaded region.
            let mut candidates: Vec<(Region, u64)> = loads
                .iter()
                .filter(|(r, _)| {
                    owner_of(
                        ChunkPos::new(r.0 << REGION_BITS, r.1 << REGION_BITS),
                        &table,
                        workers,
                    ) == busiest
                        && table.get(r) != Some(&RegionAssign::SplitByChunk)
                        && cooled(r)
                })
                .copied()
                .collect();
            if !candidates.is_empty() {
                candidates.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
                let (region, count) = candidates.get(1).copied().unwrap_or(candidates[0]);
                let mut next = (*table).clone();
                next.insert(region, RegionAssign::Worker(idlest));
                assignment.install(next);
                last_change.insert(region, now);
                tracing::info!(
                    "rebalancer: moved region {:?} ({} writes) worker {} → {} \
                     (loads {} vs {})",
                    region, count, busiest, idlest, worker_load[busiest], worker_load[idlest],
                );
            }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn no_overrides() -> HashMap<Region, RegionAssign> {
        HashMap::new()
    }

    #[test]
    fn owner_assignment_is_deterministic_and_region_grained() {
        let t = no_overrides();
        assert_eq!(
            owner_of(ChunkPos::new(5, -3), &t, 8),
            owner_of(ChunkPos::new(5, -3), &t, 8)
        );
        let base = owner_of(ChunkPos::new(0, 0), &t, 8);
        for cx in 0..4 {
            for cz in 0..4 {
                assert_eq!(owner_of(ChunkPos::new(cx, cz), &t, 8), base);
            }
        }
        for cx in -20..20 {
            assert_eq!(owner_of(ChunkPos::new(cx, cx * 3), &t, 1), 0);
        }
    }

    #[test]
    fn owner_assignment_balances_regions() {
        let t = no_overrides();
        let workers = 8;
        let mut counts = vec![0usize; workers];
        for rx in -16..16 {
            for rz in -16..16 {
                counts[owner_of(ChunkPos::new(rx * 4, rz * 4), &t, workers)] += 1;
            }
        }
        let total: usize = counts.iter().sum();
        let ideal = total / workers;
        for (w, &c) in counts.iter().enumerate() {
            assert!(
                c > ideal / 2 && c < ideal * 2,
                "worker {w} owns {c} regions (ideal {ideal}) — assignment badly skewed",
            );
        }
    }

    #[test]
    fn overrides_redirect_and_split() {
        let mut t = no_overrides();
        // Region (0,0) covers chunks 0..4 × 0..4.
        t.insert((0, 0), RegionAssign::Worker(3));
        assert_eq!(owner_of(ChunkPos::new(1, 1), &t, 8), 3);
        assert_eq!(owner_of(ChunkPos::new(3, 0), &t, 8), 3);

        t.insert((0, 0), RegionAssign::SplitByChunk);
        // Split distributes the region's 16 chunks across many workers.
        let mut seen = std::collections::HashSet::new();
        for cx in 0..4 {
            for cz in 0..4 {
                seen.insert(owner_of(ChunkPos::new(cx, cz), &t, 8));
            }
        }
        assert!(seen.len() >= 4, "split region should spread chunks, got {seen:?}");
        // Chunks outside the region keep their default owner.
        let t2 = no_overrides();
        assert_eq!(owner_of(ChunkPos::new(9, 9), &t, 8), owner_of(ChunkPos::new(9, 9), &t2, 8));
    }
}
